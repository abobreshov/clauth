//! Guest mode's additive writes (plan §4.0, "guest mode (pre-import)").
//!
//! In guest mode upstream clauth owns every global file the two tools share,
//! and tollgate refuses each write that would change upstream's state. A few of
//! those files also carry entries that are tollgate's alone, keyed by its own
//! names, and upstream never reads or writes them:
//!
//! | file | tollgate's keys |
//! |------|-----------------|
//! | `~/.claude.json` | `mcpServers.tollgate` |
//! | `~/.claude/settings.json` | `enabledPlugins["tollgate@tollgate"]` |
//! | `~/.claude/plugins/installed_plugins.json` | `plugins["tollgate@tollgate"]` |
//! | `~/.claude/plugins/known_marketplaces.json` | `tollgate` |
//!
//! (herdr's `config.toml` is the fifth shared file; its owned part is the
//! blocks under [`crate::identity::HERDR_CONFIG_MARKER`], checked in
//! `herdr::write_validated` through the same lock and replace helpers.)
//!
//! Adding, updating or removing those keys changes nothing upstream owns, so
//! guest mode allows it under three rules this module enforces:
//!
//! 1. **Only the owned keys move.** [`guest_additive_write`] parses the file,
//!    runs the edit, and refuses to write when anything outside the owned keys
//!    differs between the before and after documents. [`guest_guarded`] wraps
//!    a `claude plugin` child, whose writes tollgate does not control: it
//!    snapshots each file first and, afterwards, puts back any upstream key the
//!    child changed or dropped, keeping only the owned keys from the child's
//!    version.
//! 2. **Atomic, mode-preserving writes.** [`atomic_replace`] writes a temp
//!    sibling and renames it over the file, keeping the original's permission
//!    bits and writing through a symlink to its target.
//! 3. **Upstream's writers are serialized.** Each read-modify-write holds
//!    upstream's own state flock, `~/.clauth/.lock` ([`upstream_lock`]): the
//!    existing file is opened read-only and locked exclusively with a short
//!    bounded wait. It is never created; a machine where upstream never took it
//!    has no writer to wait for.
//!
//! Outside guest mode every helper here is a pass-through, so the non-guest
//! write paths are exactly what they were.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::identity::{CC_PLUGIN, NAME, UPSTREAM_DATA_DIR_NAME};

/// One key tollgate owns inside a shared JSON file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnedKey {
    /// A top-level key (`known_marketplaces.json`'s `tollgate`).
    Top(&'static str),
    /// One entry of a top-level object (`mcpServers.tollgate`). The parent
    /// object itself is shared: only the named entry is tollgate's.
    Entry(&'static str, &'static str),
}

/// `~/.claude.json`: the manual MCP wiring.
pub(crate) const CLAUDE_JSON_KEYS: &[OwnedKey] = &[OwnedKey::Entry("mcpServers", NAME)];
/// `settings.json`: the plugin's enable flag, and the marketplace declaration
/// `claude plugin marketplace add --scope user` writes beside upstream's own
/// `extraKnownMarketplaces.clauth` (Claude Code 2.1.283).
pub(crate) const SETTINGS_KEYS: &[OwnedKey] = &[
    OwnedKey::Entry("enabledPlugins", CC_PLUGIN),
    OwnedKey::Entry("extraKnownMarketplaces", NAME),
];
/// `plugins/installed_plugins.json`: the plugin's install rows.
pub(crate) const INSTALLED_PLUGINS_KEYS: &[OwnedKey] = &[OwnedKey::Entry("plugins", CC_PLUGIN)];
/// `plugins/known_marketplaces.json`: the marketplace registration.
pub(crate) const KNOWN_MARKETPLACES_KEYS: &[OwnedKey] = &[OwnedKey::Top(NAME)];

/// Upstream's state lock file name inside `~/.clauth` (upstream's
/// `lock::LOCK_FILENAME`, the same `.lock` tollgate's own lock uses).
pub(crate) const UPSTREAM_LOCK_FILE: &str = ".lock";

/// How long [`upstream_lock`] waits for upstream to release its flock.
/// Upstream holds it across sub-millisecond disk writes (and, on macOS, a
/// Keychain shell-out), so a wait this long means a wedge, not contention.
const UPSTREAM_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the wait re-polls the flock.
const UPSTREAM_LOCK_POLL: Duration = Duration::from_millis(25);

#[cfg(test)]
thread_local! {
    static UPSTREAM_LOCK_TIMEOUT_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Shrink [`UPSTREAM_LOCK_TIMEOUT`] on the calling thread until the guard
/// drops, so a test poses a held lock without waiting 5 s.
#[cfg(test)]
pub(crate) fn upstream_lock_timeout_for_test(timeout: Duration) -> impl Drop {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            UPSTREAM_LOCK_TIMEOUT_OVERRIDE.with(|c| c.set(None));
        }
    }
    UPSTREAM_LOCK_TIMEOUT_OVERRIDE.with(|c| c.set(Some(timeout)));
    Reset
}

fn upstream_lock_timeout() -> Duration {
    #[cfg(test)]
    if let Some(t) = UPSTREAM_LOCK_TIMEOUT_OVERRIDE.with(|c| c.get()) {
        return t;
    }
    UPSTREAM_LOCK_TIMEOUT
}

/// Upstream held `~/.clauth/.lock` past the bounded wait; nothing was written.
#[derive(Debug)]
pub(crate) struct UpstreamLockTimeout {
    waited: Duration,
}

impl std::fmt::Display for UpstreamLockTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "upstream clauth is holding ~/.clauth/.lock (waited {:.1}s), so nothing was written; retry once it finishes",
            self.waited.as_secs_f64()
        )
    }
}

impl std::error::Error for UpstreamLockTimeout {}

/// A held upstream state flock, released on drop. Empty outside guest mode,
/// and when upstream's lock file does not exist.
#[must_use = "the upstream flock is released as soon as the guard drops"]
pub(crate) struct UpstreamLock {
    _file: Option<File>,
}

/// Take upstream's state flock for one read-modify-write of a shared file.
/// A no-op outside guest mode. The file is opened read-only and never created:
/// an absent lock means upstream has never serialized a write through it.
pub(crate) fn upstream_lock() -> Result<UpstreamLock> {
    if !crate::identity::upstream_active() {
        return Ok(UpstreamLock { _file: None });
    }
    let path = crate::profile::home_dir()?
        .join(UPSTREAM_DATA_DIR_NAME)
        .join(UPSTREAM_LOCK_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UpstreamLock { _file: None });
        }
        Err(e) => return Err(e).with_context(|| format!("failed to open {}", path.display())),
    };
    let timeout = upstream_lock_timeout();
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(UpstreamLock { _file: Some(file) }),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(UpstreamLockTimeout { waited: timeout }.into());
                }
                std::thread::sleep(UPSTREAM_LOCK_POLL.min(deadline - now));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    }
}

/// Read `path` as a JSON object: `None` when it does not exist. Any other read
/// failure, a file that does not parse, and a non-object document are errors:
/// the owned-keys check cannot vouch for a file it could not read.
pub(crate) fn read_object(path: &Path) -> Result<Option<Map<String, Value>>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(_) => bail!("{} is not a JSON object; left it alone", path.display()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "{} does not parse (Claude Code may be writing it); left it alone, retry",
                path.display()
            )
        }),
    }
}

fn owned_parents(owned: &[OwnedKey]) -> impl Iterator<Item = &'static str> + '_ {
    owned.iter().filter_map(|key| match key {
        OwnedKey::Entry(parent, _) => Some(*parent),
        OwnedKey::Top(_) => None,
    })
}

fn owns_top(owned: &[OwnedKey], key: &str) -> bool {
    owned
        .iter()
        .any(|k| matches!(k, OwnedKey::Top(t) if *t == key))
}

fn owns_entry(owned: &[OwnedKey], parent: &str, key: &str) -> bool {
    owned
        .iter()
        .any(|k| matches!(k, OwnedKey::Entry(p, e) if *p == parent && *e == key))
}

/// `doc` with every owned key taken out: the part of the file tollgate must
/// never change. An owned entry's parent that is left empty goes too, so
/// creating `mcpServers` just to hold `tollgate` reads as no foreign change.
pub(crate) fn foreign_view(doc: &Map<String, Value>, owned: &[OwnedKey]) -> Map<String, Value> {
    let mut view = doc.clone();
    for key in owned {
        match *key {
            OwnedKey::Top(k) => {
                view.shift_remove(k);
            }
            OwnedKey::Entry(parent, k) => {
                if let Some(Value::Object(entries)) = view.get_mut(parent) {
                    entries.shift_remove(k);
                    if entries.is_empty() {
                        view.shift_remove(parent);
                    }
                }
            }
        }
    }
    view
}

/// The foreign keys whose value differs between `before` and `after` (added,
/// changed or removed), spelled `key` or `parent.entry`. Empty when the edit
/// touched only owned keys.
pub(crate) fn foreign_changes(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    owned: &[OwnedKey],
) -> Vec<String> {
    let before = foreign_view(before, owned);
    let after = foreign_view(after, owned);
    let mut keys: Vec<&String> = before.keys().collect();
    keys.extend(after.keys().filter(|k| !before.contains_key(*k)));
    let mut changed = Vec::new();
    for key in keys {
        let (b, a) = (before.get(key), after.get(key));
        if b == a {
            continue;
        }
        match (b, a) {
            (Some(Value::Object(b)), Some(Value::Object(a)))
                if owned_parents(owned).any(|p| p == key) =>
            {
                let mut entries: Vec<&String> = b.keys().collect();
                entries.extend(a.keys().filter(|k| !b.contains_key(*k)));
                for entry in entries {
                    if b.get(entry) != a.get(entry) {
                        changed.push(format!("{key}.{entry}"));
                    }
                }
            }
            _ => changed.push(key.clone()),
        }
    }
    changed
}

/// The foreign keys of `before` that `after` changed or dropped. Unlike
/// [`foreign_changes`] an ADDED foreign key does not count: rule 1 forbids
/// modifying or removing another tool's key, and a `claude plugin` child
/// scaffolding a fresh file (`"version": 2`) modifies nothing.
fn lost_foreign_keys(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    owned: &[OwnedKey],
) -> Vec<String> {
    let mut lost = Vec::new();
    for (key, value) in before {
        if owns_top(owned, key) {
            continue;
        }
        let is_parent = owned_parents(owned).any(|p| p == key);
        match (value, after.get(key)) {
            (Value::Object(b), Some(Value::Object(a))) if is_parent => {
                for (entry, v) in b {
                    if !owns_entry(owned, key, entry) && a.get(entry) != Some(v) {
                        lost.push(format!("{key}.{entry}"));
                    }
                }
            }
            (Value::Object(b), None) if is_parent => {
                lost.extend(
                    b.keys()
                        .filter(|entry| !owns_entry(owned, key, entry))
                        .map(|entry| format!("{key}.{entry}")),
                );
            }
            (v, a) if a != Some(v) => lost.push(key.clone()),
            _ => {}
        }
    }
    lost
}

/// `before` with the owned keys taken from `after`, plus any foreign key
/// `after` ADDED: what the file should hold once a `claude plugin` child's
/// changes to other tools' keys are undone. Built on `before` so the foreign
/// keys keep their original order.
fn rebase(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    owned: &[OwnedKey],
) -> Map<String, Value> {
    let mut out = before.clone();
    for (key, value) in after {
        if !out.contains_key(key) && !owns_top(owned, key) {
            out.insert(key.clone(), value.clone());
        }
    }
    for parent in owned_parents(owned) {
        if let (Some(Value::Object(added)), Some(Value::Object(entries))) =
            (after.get(parent), out.get_mut(parent))
        {
            for (entry, value) in added {
                if !entries.contains_key(entry) {
                    entries.insert(entry.clone(), value.clone());
                }
            }
        }
    }
    for key in owned {
        match *key {
            OwnedKey::Top(k) => match after.get(k) {
                Some(value) => {
                    out.insert(k.to_string(), value.clone());
                }
                None => {
                    out.shift_remove(k);
                }
            },
            OwnedKey::Entry(parent, k) => {
                let want = after.get(parent).and_then(|p| p.get(k)).cloned();
                match (want, out.get_mut(parent)) {
                    (Some(value), Some(Value::Object(entries))) => {
                        entries.insert(k.to_string(), value);
                    }
                    (Some(value), None) => {
                        out.insert(
                            parent.to_string(),
                            Value::Object(Map::from_iter([(k.to_string(), value)])),
                        );
                    }
                    // The parent is a non-object upstream set: its value is
                    // foreign and wins; the owned entry has nowhere to live.
                    (Some(_), Some(_)) => {}
                    (None, Some(Value::Object(entries))) => {
                        entries.shift_remove(k);
                    }
                    (None, _) => {}
                }
            }
        }
    }
    out
}

/// Replace `path` with `bytes` atomically: a temp sibling, then a rename. The
/// original's permission bits carry over (a new file keeps the temp's 0600),
/// and a symlink is written through to its target rather than replaced by a
/// plain file. A dangling symlink is refused.
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::canonicalize(path).with_context(|| {
                format!(
                    "{} is a symlink whose target does not resolve; left it alone",
                    path.display()
                )
            })?
        }
        _ => path.to_path_buf(),
    };
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let perms = std::fs::metadata(&target).ok().map(|m| m.permissions());
    let mut tmp = tempfile::Builder::new()
        .prefix(".tollgate-")
        .suffix(".tmp")
        .tempfile_in(&dir)
        .with_context(|| format!("failed to stage a temp file in {}", dir.display()))?;
    tmp.write_all(bytes)
        .with_context(|| format!("failed to write a temp file for {}", target.display()))?;
    tmp.as_file().sync_all()?;
    if let Some(perms) = perms {
        tmp.as_file()
            .set_permissions(perms)
            .with_context(|| format!("failed to carry {}'s mode over", target.display()))?;
    }
    tmp.persist(&target)
        .map_err(|e| e.error)
        .with_context(|| format!("failed to replace {}", target.display()))?;
    Ok(())
}

fn to_pretty(doc: Map<String, Value>) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(&Value::Object(doc))?)
}

/// Guest mode's writer for tollgate's own keys in a shared JSON file: under
/// [`upstream_lock`], read `path` (absent reads as `{}`), run `edit`, refuse
/// when anything outside `owned` changed, and replace the file atomically.
/// Returns whether it wrote: an edit that changes nothing leaves the file (and
/// its mtime) alone, and never creates an empty one.
pub(crate) fn guest_additive_write(
    path: &Path,
    owned: &[OwnedKey],
    edit: impl FnOnce(&mut Map<String, Value>) -> Result<()>,
) -> Result<bool> {
    let _lock = upstream_lock()?;
    let before = read_object(path)?;
    let base = before.clone().unwrap_or_default();
    let mut after = base.clone();
    edit(&mut after)?;
    let changed = foreign_changes(&base, &after, owned);
    if !changed.is_empty() {
        bail!(
            "refusing to write {}: the edit would change keys tollgate does not own ({})",
            path.display(),
            changed.join(", ")
        );
    }
    let unchanged = match &before {
        Some(before) => *before == after,
        None => after.is_empty(),
    };
    if unchanged {
        return Ok(false);
    }
    atomic_replace(path, &to_pretty(after)?)?;
    Ok(true)
}

/// Run `run` — a child tollgate does not control, `claude plugin …` — so it
/// can only add or change tollgate's own keys in `files`. Outside guest mode
/// this is `run()` and nothing else.
///
/// In guest mode, under [`upstream_lock`] for the whole call: every file is
/// snapshotted first (one that does not parse refuses the run, since nothing
/// could be restored), `run` runs, and then each file that existed before is
/// re-read. A foreign key the child changed or dropped is put back from the
/// snapshot, keeping the child's owned keys and any key it added, and the
/// restore is logged. A file the child created from nothing holds no foreign
/// key to protect. `run`'s own error wins over a restore error.
pub(crate) fn guest_guarded<T>(
    files: &[(PathBuf, &'static [OwnedKey])],
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if !crate::identity::upstream_active() {
        return run();
    }
    owned_keys_guarded(files, run)
}

/// [`guest_guarded`]'s snapshot-and-restore in EVERY mode, for a child whose
/// whole job is to touch only tollgate's own keys (`tollgate plugin
/// uninstall`): a foreign key it changed or dropped is put back whether or
/// not upstream is installed. Upstream's lock is taken only in guest mode
/// ([`upstream_lock`]).
pub(crate) fn owned_keys_guarded<T>(
    files: &[(PathBuf, &'static [OwnedKey])],
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _lock = upstream_lock()?;
    let snapshots = files
        .iter()
        .map(|(path, _)| read_object(path))
        .collect::<Result<Vec<_>>>()?;
    let out = run();
    let mut restore_error = None;
    for ((path, owned), before) in files.iter().zip(snapshots) {
        let Some(before) = before else {
            continue;
        };
        if let Err(e) = restore_foreign(path, &before, owned)
            && restore_error.is_none()
        {
            restore_error = Some(e);
        }
    }
    match (out, restore_error) {
        (Err(e), _) => Err(e),
        (Ok(_), Some(e)) => Err(e),
        (Ok(value), None) => Ok(value),
    }
}

/// Put back the foreign keys of `before` that `path` no longer holds. Returns
/// what it restored, empty when the child left every foreign key alone.
fn restore_foreign(
    path: &Path,
    before: &Map<String, Value>,
    owned: &[OwnedKey],
) -> Result<Vec<String>> {
    let after = read_object(path)
        .with_context(|| {
            format!(
                "could not check that upstream's entries in {} survived",
                path.display()
            )
        })?
        .unwrap_or_default();
    let lost = lost_foreign_keys(before, &after, owned);
    if lost.is_empty() {
        return Ok(lost);
    }
    atomic_replace(path, &to_pretty(rebase(before, &after, owned))?)?;
    crate::logline::logline!(
        "tollgate: guest mode: restored {} in {} after `claude plugin` changed it",
        lost.join(", "),
        path.display()
    );
    Ok(lost)
}

#[cfg(test)]
#[path = "../tests/inline/guest_write.rs"]
mod tests;
