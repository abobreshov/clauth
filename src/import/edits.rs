//! The global edits (spec §4.8): every change the import makes to a file
//! upstream clauth and tollgate share, each journaled with its prior value
//! and reversible, all behind the one confirmation.
//!
//! - **G1** (M7, inside the fence) `~/.claude/settings.json`
//!   `enabledPlugins["clauth@clauth"]` `true` → `false`.
//! - **G2** (M0, before any lock) upstream's own `clauth herdr uninstall
//!   --yes`, with herdr's `config.toml` backed up and upstream's plugin record
//!   journaled; its undo restores the config bytes inside the fence and
//!   reinstalls the recorded commit only after the fence is released.
//! - **G3** (M7) upstream's `apiKeyHelper` rebuilt as tollgate's, and each
//!   `permissions.allow` entry naming upstream's MCP tools renamed to
//!   tollgate's.
//! - **G4** (M7) `installed_plugins.json` installPaths under
//!   `~/.clauth/profiles/` re-pointed.
//!
//! Every write here is a raw read-modify-write (`guest_write::read_object` +
//! `guest_write::atomic_replace`), never a guest-gated wrapper: those take
//! upstream's `~/.clauth/.lock`, which the fence already holds. No value
//! journaled here is secret: a boolean, a helper command of one exact shape,
//! MCP tool names, install paths. `settings.json`'s `env` is never read into
//! a journal entry, a backup or a report (spec I13).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Map, Value};

use super::inventory::{Action, Inventory};
use super::journal::{self, BinRecord, Entry, Facts, HerdrRecord, Journal, Op, Repoint, Status};
use super::{Finding, Options, Paths, fsops, seams};

/// Upstream's Claude Code plugin key.
pub(crate) const UPSTREAM_PLUGIN: &str = crate::identity::UPSTREAM_CC_PLUGIN;
/// The prefix upstream's MCP tools carry in `permissions.allow`.
pub(crate) const UPSTREAM_ALLOW_PREFIX: &str = "mcp__plugin_clauth_clauth__";
/// Upstream's `apiKeyHelper` subcommand (`clauth __api-key <profile>`).
const UPSTREAM_HELPER_SUBCMD: &str = "__api-key";
/// G2's subprocess budget (spec §4.8).
pub(crate) const G2_DEADLINE: Duration = Duration::from_secs(20);
/// The after-fence reinstall's budget: it fetches from GitHub.
const REINSTALL_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// Upstream's marker over its blocks in herdr's `config.toml`.
pub(crate) const UPSTREAM_HERDR_MARKER: &str = "# clauth herdr plugin";
/// The plugin subdirectory of upstream's repo (`mommy:src/herdr.rs:32`).
const UPSTREAM_HERDR_SUBDIR: &str = "herdr-plugin";
/// The env vars G2's child runs with: the minimum a CLI needs, upstream's
/// three opt-outs, and the herdr it must drive. Everything else is cleared,
/// so no monitoring key, API key or session variable reaches it.
const G2_PASS_ENV: [&str; 11] = [
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "HERDR_CONFIG_PATH",
];

/// The tollgate spelling of an MCP tool prefix (`mcp__plugin_tollgate_tollgate__`).
pub(crate) fn tollgate_allow_prefix() -> String {
    format!(
        "mcp__plugin_{0}_{0}__",
        crate::identity::NAME.replace('-', "_")
    )
}

/// One row of the report's `global_edits` (spec §2.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GlobalEdit {
    pub(crate) id: String,
    pub(crate) file: String,
    pub(crate) change: String,
}

/// A planned M7 edit: its journal entry's op and facts, and its report row.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    pub(crate) op: Op,
    pub(crate) dst: PathBuf,
    pub(crate) prior: Facts,
    pub(crate) after: Facts,
    pub(crate) row: GlobalEdit,
}

// ── JSON pointers and files ────────────────────────────────────────────────

/// An RFC 6901 pointer from its segments.
pub(crate) fn pointer(segments: &[&str]) -> String {
    segments
        .iter()
        .map(|s| format!("/{}", s.replace('~', "~0").replace('/', "~1")))
        .collect()
}

fn segments(ptr: &str) -> Vec<String> {
    ptr.split('/')
        .skip(1)
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect()
}

/// The value at `ptr`, objects and arrays alike.
pub(crate) fn lookup<'a>(root: &'a Map<String, Value>, ptr: &str) -> Option<&'a Value> {
    let segs = segments(ptr);
    let (first, rest) = segs.split_first()?;
    let mut cur = root.get(first)?;
    for s in rest {
        cur = match cur {
            Value::Object(m) => m.get(s)?,
            Value::Array(a) => a.get(s.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The position of the key `ptr` names inside its parent object.
fn index_of(root: &Map<String, Value>, ptr: &str) -> Option<usize> {
    let segs = segments(ptr);
    let (last, parents) = segs.split_last()?;
    let parent = if parents.is_empty() {
        root
    } else {
        match lookup(
            root,
            &pointer(&parents.iter().map(String::as_str).collect::<Vec<_>>()),
        )? {
            Value::Object(m) => m,
            _ => return None,
        }
    };
    parent.keys().position(|k| k == last)
}

/// Set (`Some`) or remove (`None`) the value at `ptr`. A set on a missing
/// object key re-adds it at `index` when given (an undo puts a removed key
/// back where it was); a missing parent object is created for a set and
/// makes a removal a no-op. An emptied parent stays, so an undo finds it.
fn assign(
    root: &mut Map<String, Value>,
    ptr: &str,
    value: Option<Value>,
    index: Option<usize>,
) -> Result<()> {
    let segs = segments(ptr);
    let Some((last, parents)) = segs.split_last() else {
        bail!("an empty JSON pointer");
    };
    let mut map = root;
    for (depth, s) in parents.iter().enumerate() {
        if !map.contains_key(s) {
            if value.is_none() {
                return Ok(());
            }
            map.insert(s.clone(), Value::Object(Map::new()));
        }
        let child = map
            .get_mut(s)
            .ok_or_else(|| anyhow!("{ptr}: {s} vanished"))?;
        match child {
            Value::Object(m) => map = m,
            Value::Array(items) if depth + 1 == parents.len() => {
                return set_array_item(items, ptr, last, value);
            }
            _ => bail!("{ptr}: {s} is not an object"),
        }
    }
    match value {
        None => {
            map.shift_remove(last);
        }
        Some(v) => {
            if map.contains_key(last) {
                map.insert(last.clone(), v);
            } else if let Some(i) = index.filter(|i| *i <= map.len()) {
                map.shift_insert(i, last.clone(), v);
            } else {
                map.insert(last.clone(), v);
            }
        }
    }
    Ok(())
}

/// [`assign`] into an array: only an existing item is ever set
/// (`/permissions/allow/<n>`), never removed.
fn set_array_item(items: &mut [Value], ptr: &str, idx: &str, value: Option<Value>) -> Result<()> {
    let i: usize = idx
        .parse()
        .map_err(|_| anyhow!("{ptr}: {idx} is not an index"))?;
    let slot = items
        .get_mut(i)
        .ok_or_else(|| anyhow!("{ptr}: index {i} is out of range"))?;
    *slot = value.ok_or_else(|| anyhow!("{ptr}: an array item is never removed"))?;
    Ok(())
}

/// A shared JSON document as read: its object, and whether it ended in a
/// newline (kept, so a round trip is byte-identical for a pretty-printed
/// file).
#[derive(Debug, Clone)]
pub(crate) struct JsonDoc {
    pub(crate) map: Map<String, Value>,
    newline: bool,
}

/// Read `path` as a JSON object; absent reads as empty.
pub(crate) fn read_doc(path: &Path) -> Result<JsonDoc> {
    let newline = std::fs::read(path).ok().is_some_and(|b| b.ends_with(b"\n"));
    let map = crate::guest_write::read_object(path)?.unwrap_or_default();
    Ok(JsonDoc { map, newline })
}

/// Replace `path` with `doc`, pretty-printed, atomically, its mode kept
/// (a symlink is written through).
pub(crate) fn write_doc(path: &Path, doc: &JsonDoc) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(doc.map.clone()))?;
    if doc.newline {
        bytes.push(b'\n');
    }
    crate::guest_write::atomic_replace(path, &bytes)?;
    if let Some(dir) = path.parent() {
        crate::profile::sync_dir(dir).ok();
    }
    Ok(())
}

/// A value as the compact JSON text a journal entry records.
fn text_of(v: Option<&Value>) -> Option<String> {
    v.map(|v| serde_json::to_string(v).unwrap_or_default())
}

fn value_of(text: Option<&String>) -> Option<Value> {
    text.and_then(|t| serde_json::from_str(t).ok())
}

/// Whether a JSON value may hold a secret: an `env` or `headers` block, a
/// key naming a token, key, secret or password, or a string of a known
/// credential shape. Such a value is never journaled (spec §3.2: no entry
/// ever holds a credential byte). A commit digest or an install path passes.
pub(crate) fn looks_secret(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.iter().any(|(k, v)| {
            let k = k.to_ascii_lowercase();
            ["env", "headers"].contains(&k.as_str())
                || ["token", "key", "secret", "password", "auth"]
                    .iter()
                    .any(|w| k.contains(w))
                || looks_secret(v)
        }),
        Value::Array(a) => a.iter().any(looks_secret),
        Value::String(s) => {
            let t = s.trim_start();
            t.starts_with("sk-")
                || t.starts_with("eyJ")
                || t.starts_with("ghp_")
                || t.starts_with("github_pat_")
                || t.to_ascii_lowercase().contains("bearer ")
                || s.contains("sk-ant-")
        }
        _ => false,
    }
}

/// A `rewrite_json` entry's facts: `ptr` in `doc` from its current value to
/// `new` (`None` removes the key).
pub(crate) fn rewrite_facts(
    doc: &JsonDoc,
    ptr: &str,
    new: Option<&Value>,
    step: &str,
) -> (Facts, Facts) {
    let cur = lookup(&doc.map, ptr);
    (
        Facts {
            pointer: Some(ptr.to_string()),
            prior_value: text_of(cur),
            exists: Some(cur.is_some()),
            index: index_of(&doc.map, ptr),
            ..Facts::default()
        },
        Facts {
            new_value: text_of(new),
            step: Some(step.to_string()),
            ..Facts::default()
        },
    )
}

// ── G1 / G3: settings.json ─────────────────────────────────────────────────

/// Upstream's helper shape: `<exe> __api-key <profile>` whose exe file name
/// is `clauth`. Returns the profile name; anything else is not upstream's.
pub(crate) fn upstream_helper_profile(helper: &str) -> Option<String> {
    let tokens: Vec<&str> = helper.split_whitespace().collect();
    let at = tokens.iter().position(|t| *t == UPSTREAM_HELPER_SUBCMD)?;
    if at + 2 != tokens.len() || at == 0 {
        return None;
    }
    let exe = tokens[..at].join(" ");
    let exe = exe.trim_matches(|c| c == '\'' || c == '"');
    let file = exe.rsplit(['/', '\\']).next().unwrap_or_default();
    if file != crate::identity::UPSTREAM_NAME {
        return None;
    }
    let name = tokens[at + 1];
    let valid = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'+' | b'-'));
    valid.then(|| name.to_string())
}

/// G1 and G3 over `~/.claude/settings.json`. `imported` is every upstream
/// claude profile the import brings over; `exe` is the installed tollgate.
pub(crate) fn plan_settings(
    paths: &Paths,
    opts: &Options,
    imported: &[String],
    exe: Option<&Path>,
    warnings: &mut Vec<Finding>,
) -> Result<Vec<Planned>> {
    let file = paths.claude.join("settings.json");
    if fsops::lmeta(&file).is_none() {
        return Ok(Vec::new());
    }
    let doc = read_doc(&file)?;
    let shown = paths.tilde(&file);
    let mut out = Vec::new();
    // G1.
    let g1 = pointer(&["enabledPlugins", UPSTREAM_PLUGIN]);
    if lookup(&doc.map, &g1) == Some(&Value::Bool(true)) {
        let (prior, after) = rewrite_facts(&doc, &g1, Some(&Value::Bool(false)), "G1");
        out.push(Planned {
            op: Op::RewriteJson,
            dst: file.clone(),
            prior,
            after,
            row: GlobalEdit {
                id: "G1".to_string(),
                file: shown.clone(),
                change: format!("enabledPlugins.{UPSTREAM_PLUGIN} true -> false"),
            },
        });
    }
    // G3: the helper.
    if let Some(Value::String(helper)) = doc.map.get("apiKeyHelper")
        && let Some(p) = upstream_helper_profile(helper)
    {
        if !imported.contains(&p) {
            warnings.push(Finding::new(
                "helper_profile_unknown",
                format!(
                    "{shown} apiKeyHelper names upstream profile '{p}', which is not imported; it is left as is"
                ),
            ));
        } else if let Some(exe) = exe {
            let new = crate::claude::build_api_key_helper_command(
                exe,
                &crate::profile::ProfileName::from(opts.dst_name(&p)),
            );
            let ptr = pointer(&["apiKeyHelper"]);
            let (prior, after) = rewrite_facts(&doc, &ptr, Some(&Value::String(new.clone())), "G3");
            out.push(Planned {
                op: Op::RewriteJson,
                dst: file.clone(),
                prior,
                after,
                row: GlobalEdit {
                    id: "G3".to_string(),
                    file: shown.clone(),
                    change: format!("apiKeyHelper '{helper}' -> '{new}'"),
                },
            });
        } else {
            warnings.push(Finding::new(
                "helper_kept",
                format!(
                    "{shown} apiKeyHelper is upstream's, but this binary's installed path is unknown; it is left as is"
                ),
            ));
        }
    }
    // G3: `permissions.allow`, one entry per renamed item (never the whole
    // array: another rule there may quote a secret).
    if let Some(Value::Array(items)) = lookup(&doc.map, &pointer(&["permissions", "allow"])) {
        let ours = tollgate_allow_prefix();
        for (i, item) in items.iter().enumerate() {
            let Some(rest) = item
                .as_str()
                .and_then(|s| s.strip_prefix(UPSTREAM_ALLOW_PREFIX))
            else {
                continue;
            };
            let new = Value::String(format!("{ours}{rest}"));
            let ptr = pointer(&["permissions", "allow", &i.to_string()]);
            let (prior, after) = rewrite_facts(&doc, &ptr, Some(&new), "G3");
            out.push(Planned {
                op: Op::RewriteJson,
                dst: file.clone(),
                prior,
                after,
                row: GlobalEdit {
                    id: "G3".to_string(),
                    file: shown.clone(),
                    change: format!(
                        "permissions.allow {UPSTREAM_ALLOW_PREFIX}{rest} -> {ours}{rest}"
                    ),
                },
            });
        }
    }
    Ok(out)
}

/// Where disk stands for a `rewrite_json` entry.
pub(crate) fn probe_json(e: &Entry) -> Result<super::txn::Disk> {
    use super::txn::Disk;
    let dst = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    let ptr = e
        .prior
        .pointer
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no pointer"))?;
    let doc = read_doc(dst)?;
    let cur = lookup(&doc.map, ptr).cloned();
    let prior = value_of(e.prior.prior_value.as_ref());
    let new = value_of(e.after.new_value.as_ref());
    Ok(if cur == new {
        Disk::After
    } else if cur == prior {
        Disk::Prior
    } else {
        Disk::Neither
    })
}

/// Drive a `rewrite_json` entry to its `after` value.
pub(crate) fn apply_json(e: &Entry) -> Result<()> {
    let dst = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    let ptr = e
        .prior
        .pointer
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no pointer"))?;
    let mut doc = read_doc(dst)?;
    let new = value_of(e.after.new_value.as_ref());
    if lookup(&doc.map, ptr).cloned() == new {
        return Ok(());
    }
    assign(&mut doc.map, ptr, new, None)?;
    write_doc(dst, &doc)
}

/// Drive a `rewrite_json` entry back to its `prior` value. A value changed
/// since the import is left alone with a warning, except an allow-list item
/// that moved (another rule added before it): the renamed item is found by
/// value and named back.
pub(crate) fn revert_json(e: &Entry, paths: &Paths, warnings: &mut Vec<Finding>) -> Result<()> {
    let dst = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    let ptr = e
        .prior
        .pointer
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no pointer"))?;
    let mut doc = read_doc(dst)?;
    let cur = lookup(&doc.map, ptr).cloned();
    let prior = value_of(e.prior.prior_value.as_ref());
    let new = value_of(e.after.new_value.as_ref());
    if cur == prior {
        return Ok(());
    }
    if cur == new {
        assign(&mut doc.map, ptr, prior, e.prior.index)?;
        return write_doc(dst, &doc);
    }
    // An allow-list item that moved: find the renamed value and name it back.
    let segs = segments(ptr);
    if segs.len() == 3
        && segs[0] == "permissions"
        && segs[1] == "allow"
        && let (Some(Value::String(new_s)), Some(prior_v)) = (&new, &prior)
        && let Some(Value::Array(items)) = doc
            .map
            .get_mut("permissions")
            .and_then(|p| p.get_mut("allow"))
        && let Some(slot) = items.iter_mut().find(|v| v.as_str() == Some(new_s))
    {
        *slot = prior_v.clone();
        return write_doc(dst, &doc);
    }
    warnings.push(
        Finding::new(
            "edit_kept",
            format!(
                "{} {ptr} changed after the import; it is left as it is now",
                paths.tilde(dst)
            ),
        )
        .with_path(paths.tilde(dst)),
    );
    Ok(())
}

// ── G4: installed_plugins.json ─────────────────────────────────────────────

/// Every quoted value in the registry that `keep` accepts, in file order,
/// read through agentgear's byte scanner with a remap that never rewrites
/// (so the pass writes nothing).
fn registry_values(registry: &Path, keep: impl Fn(&str) -> bool) -> Result<Vec<String>> {
    let mut seen: Vec<String> = Vec::new();
    agentgear::repoint_install_paths(registry, |v: &str| {
        if keep(v) && !seen.iter().any(|s| s == v) {
            seen.push(v.to_string());
        }
        agentgear::Remap::Keep
    })
    .with_context(|| format!("failed to read {}", registry.display()))?;
    Ok(seen)
}

/// G4's plan: each installPath under `~/.clauth/profiles/` goes to (a) the
/// same suffix under `~/.tollgate/profiles/<dst>` when the import copies or
/// moves that content there, else (b) its `~/.claude/plugins/<suffix>` twin
/// when that exists, else (c) stays, with a warning (spec I7).
pub(crate) fn plan_registry(
    paths: &Paths,
    opts: &Options,
    inv: &Inventory,
    warnings: &mut Vec<Finding>,
) -> Result<Option<Planned>> {
    let registry = paths.claude.join("plugins").join("installed_plugins.json");
    if fsops::lmeta(&registry).is_none() {
        return Ok(None);
    }
    let root = paths.source.join("profiles");
    let prefix = format!("{}/", root.display());
    let all = registry_values(&registry, |_| true)?;
    let mut pairs = Vec::new();
    for from in all.iter().filter(|v| v.starts_with(&prefix)) {
        let rest = &from[prefix.len()..];
        let Some((profile, tail)) = rest.split_once('/') else {
            continue;
        };
        let src = PathBuf::from(from);
        let carried = inv.items.iter().any(|i| {
            matches!(
                i.action,
                Action::Move
                    | Action::Copy
                    | Action::CopySecret
                    | Action::CopyTree
                    | Action::CopyTreeSecret
            ) && i.profile.as_deref() == Some(profile)
                && src.starts_with(&i.src)
        });
        let tollgate = paths.tollgate_profile(&opts.dst_name(profile)).join(tail);
        let to = if carried && fsops::lmeta(&src).is_some() {
            Some(tollgate)
        } else {
            from.split_once("/plugins/")
                .map(|(_, suffix)| paths.claude.join("plugins").join(suffix))
                .filter(|twin| twin.exists())
        };
        match to {
            Some(to) => {
                let to = to.display().to_string();
                if all.contains(&to) || to.contains(['"', '\\']) {
                    warnings.push(Finding::new(
                        "registry_path_kept",
                        format!("installPath {from} is kept: {to} is already recorded"),
                    ));
                    continue;
                }
                pairs.push(Repoint {
                    from: from.clone(),
                    to,
                });
            }
            None => warnings.push(Finding::new(
                "registry_path_kept",
                format!(
                    "installPath {from} is kept: neither a tollgate copy nor a ~/.claude/plugins twin exists"
                ),
            )),
        }
    }
    if pairs.is_empty() {
        return Ok(None);
    }
    let n = pairs.len();
    Ok(Some(Planned {
        op: Op::RepointRegistry,
        dst: registry.clone(),
        prior: Facts::default(),
        after: Facts {
            pairs: Some(pairs),
            step: Some("G4".to_string()),
            ..Facts::default()
        },
        row: GlobalEdit {
            id: "G4".to_string(),
            file: paths.tilde(&registry),
            change: format!("{n} installPath value(s) under ~/.clauth/profiles re-pointed"),
        },
    }))
}

/// Where disk stands for a `repoint_registry` entry.
pub(crate) fn probe_registry(e: &Entry) -> Result<super::txn::Disk> {
    use super::txn::Disk;
    let dst = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    let pairs = e.after.pairs.as_deref().unwrap_or(&[]);
    let values = registry_values(dst, |_| true)?;
    let from = pairs.iter().filter(|p| values.contains(&p.from)).count();
    let to = pairs.iter().filter(|p| values.contains(&p.to)).count();
    Ok(match (from, to) {
        (0, 0) => Disk::Neither,
        (0, _) => Disk::After,
        (_, 0) => Disk::Prior,
        _ => Disk::Partial,
    })
}

fn repoint(dst: &Path, pairs: &[Repoint], back: bool) -> Result<()> {
    agentgear::repoint_install_paths(dst, |v: &str| {
        let hit = pairs
            .iter()
            .find(|p| if back { p.to == v } else { p.from == v });
        match hit {
            Some(p) => agentgear::Remap::Rewrite(if back { p.from.clone() } else { p.to.clone() }),
            None => agentgear::Remap::Keep,
        }
    })
    .with_context(|| format!("failed to re-point {}", dst.display()))?;
    Ok(())
}

pub(crate) fn apply_registry(e: &Entry) -> Result<()> {
    let dst = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    repoint(dst, e.after.pairs.as_deref().unwrap_or(&[]), false)
}

pub(crate) fn revert_registry(e: &Entry) -> Result<()> {
    let dst = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    if fsops::lmeta(dst).is_none() {
        return Ok(());
    }
    repoint(dst, e.after.pairs.as_deref().unwrap_or(&[]), true)
}

// ── G2: upstream's herdr uninstall ─────────────────────────────────────────

/// What G2 would run, planned at M-1 (never under the fence: finding it
/// spawns herdr).
#[derive(Debug, Clone)]
pub(crate) struct G2Plan {
    pub(crate) bin: PathBuf,
    pub(crate) herdr: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) record: Option<HerdrRecord>,
}

/// Upstream's plugin record from `herdr plugin list --json`, or `None`
/// when herdr lists no `clauth` plugin (or cannot be read).
pub(crate) fn upstream_record(herdr: &Path) -> Option<HerdrRecord> {
    let out =
        crate::herdr::bounded_output(&herdr.to_string_lossy(), &["plugin", "list", "--json"], &[])?;
    if !out.status.success() {
        return None;
    }
    let root: Value = serde_json::from_slice(&out.stdout).ok()?;
    let entry = root
        .get("result")?
        .get("plugins")?
        .as_array()?
        .iter()
        .find(|e| {
            e.get("plugin_id").and_then(Value::as_str)
                == Some(crate::identity::UPSTREAM_HERDR_PLUGIN_ID)
        })?;
    let source = entry.get("source");
    let field = |k: &str| {
        source
            .and_then(|s| s.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    Some(HerdrRecord {
        kind: field("kind"),
        owner: field("owner"),
        repo: field("repo"),
        resolved_commit: field("resolved_commit"),
        managed_path: field("managed_path"),
        enabled: entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

/// Plan G2 (spec §4.8): herdr and an upstream binary must both exist, and
/// herdr must hold something of upstream's (its plugin, or its marked
/// config blocks). No herdr or no binary is a skip with a warning.
pub(crate) fn plan_g2(
    paths: &Paths,
    bins: &[BinRecord],
    warnings: &mut Vec<Finding>,
) -> Option<(G2Plan, GlobalEdit)> {
    let Some(herdr) = seams::herdr_bin() else {
        warnings.push(Finding::new(
            "herdr_absent",
            "herdr is not installed, so upstream's herdr plugin needs no uninstall (G2 skipped)",
        ));
        return None;
    };
    let config = match crate::herdr::config_path(&herdr.to_string_lossy()) {
        Ok(path) => path,
        Err(e) => {
            warnings.push(Finding::new(
                "herdr_config_unknown",
                format!("herdr's config path could not be found ({e:#}); G2 skipped"),
            ));
            return None;
        }
    };
    let record = upstream_record(&herdr);
    let marked = std::fs::read_to_string(&config)
        .map(|t| t.contains(UPSTREAM_HERDR_MARKER))
        .unwrap_or(false);
    if record.is_none() && !marked {
        return None;
    }
    let Some(bin) = bins.first() else {
        warnings.push(Finding::new(
            "g2_no_upstream_binary",
            "upstream's herdr plugin is installed but no upstream clauth binary is on PATH to uninstall it; remove it by hand (G2 skipped)",
        ));
        return None;
    };
    let commit = record
        .as_ref()
        .and_then(|r| r.resolved_commit.clone())
        .map_or_else(String::new, |c| format!(" at {c}"));
    let row = GlobalEdit {
        id: "G2".to_string(),
        file: paths.tilde(&config),
        change: format!(
            "clauth herdr uninstall --yes (herdr plugin {}{commit}; config backed up)",
            crate::identity::UPSTREAM_HERDR_PLUGIN_ID
        ),
    };
    Some((
        G2Plan {
            bin: bin.path.clone(),
            herdr,
            config,
            record,
        },
        row,
    ))
}

/// G2's `pre` entry (planned; nothing run yet), with its config backup.
pub(crate) fn g2_entry(paths: &Paths, plan: &G2Plan) -> Result<(Entry, Option<Vec<u8>>)> {
    let seq = 0;
    let bytes = match std::fs::read(&plan.config) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", plan.config.display()));
        }
    };
    let backup = journal::backup_path(paths, seq, "herdr-config.toml");
    let env_keys: Vec<String> = g2_env(plan).into_iter().map(|(k, _)| k).collect();
    let entry = Entry {
        seq,
        op: Op::Exec,
        src: Some(plan.bin.clone()),
        dst: Some(plan.config.clone()),
        secret: false,
        prior: Facts {
            exists: Some(bytes.is_some()),
            sha256: bytes.as_deref().map(fsops::sha256_hex),
            mode: fsops::lmeta(&plan.config).map(|m| m.mode),
            backup: bytes.is_some().then(|| backup.clone()),
            plugin: plan.record.clone(),
            ..Facts::default()
        },
        after: Facts {
            argv: Some(vec![
                plan.bin.display().to_string(),
                "herdr".to_string(),
                "uninstall".to_string(),
                "--yes".to_string(),
            ]),
            env_keys: Some(env_keys),
            link: Some(plan.herdr.clone()),
            step: Some("G2".to_string()),
            ..Facts::default()
        },
        status: Status::Planned,
    };
    Ok((entry, bytes))
}

/// The env G2's child runs with: the pass-through allowlist (names only,
/// values from this process), upstream's opt-outs, and the herdr to drive.
fn g2_env(plan: &G2Plan) -> Vec<(String, std::ffi::OsString)> {
    let mut env: Vec<(String, std::ffi::OsString)> = G2_PASS_ENV
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| ((*k).to_string(), v)))
        .collect();
    for k in ["CLAUTH_NO_UPDATE", "CLAUTH_NO_COMPLETIONS", "CLAUTH_NO_API"] {
        env.push((k.to_string(), "1".into()));
    }
    env.push((
        "HERDR_BIN_PATH".to_string(),
        plan.herdr.clone().into_os_string(),
    ));
    env
}

/// Run G2's subprocess for `e` (spec §4.8): upstream's own `herdr uninstall
/// --yes`, env cleared down to [`g2_env`], stdin null, [`G2_DEADLINE`]. Records
/// the config digest after. A spawn failure, a timeout or a non-zero exit
/// is a refusal.
pub(crate) fn run_g2(e: &mut Entry) -> Result<(), Finding> {
    let bin = e.src.clone().unwrap_or_default();
    let config = e.dst.clone().unwrap_or_default();
    let herdr = e.after.link.clone().unwrap_or_default();
    let plan = G2Plan {
        bin: bin.clone(),
        herdr,
        config: config.clone(),
        record: None,
    };
    seams::log(|| "g2 exec".to_string());
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["herdr", "uninstall", "--yes"])
        .env_clear()
        .envs(g2_env(&plan))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let outcome = match cmd.spawn() {
        Ok(child) => crate::herdr::run_bounded(child, seams::g2_deadline())
            .ok_or_else(|| format!("did not finish within {}s", seams::g2_deadline().as_secs()))
            .and_then(|out| {
                if out.status.success() {
                    Ok(())
                } else {
                    let err = String::from_utf8_lossy(&out.stderr);
                    let last = err.lines().last().unwrap_or_default().trim();
                    let last: String = last.chars().take(200).collect();
                    Err(format!("exited {:?}: {last}", out.status.code()))
                }
            }),
        Err(err) => Err(format!("could not run: {err}")),
    };
    e.after.sha256 = fsops::sha256_file(&config);
    outcome.map_err(|why| {
        Finding::new(
            "g2_failed",
            format!(
                "'{} herdr uninstall --yes' {why}; herdr's config is restored and nothing else changed",
                bin.display()
            ),
        )
    })
}

/// Where disk stands for G2: herdr's config as before the run, as the run
/// left it, or (a crash mid-run) anything else, which both directions treat
/// as in between.
pub(crate) fn probe_g2(e: &Entry) -> super::txn::Disk {
    use super::txn::Disk;
    let cur = e.dst.as_deref().and_then(fsops::sha256_file);
    if cur == e.prior.sha256 {
        Disk::Prior
    } else if e.after.sha256.is_some() && cur == e.after.sha256 {
        Disk::After
    } else {
        Disk::Partial
    }
}

/// Undo G2's file half inside the fence: herdr's config comes back byte for
/// byte when it still holds what the uninstall left (or the run crashed
/// before its digest was recorded). Nothing is spawned here.
pub(crate) fn revert_g2(e: &Entry, paths: &Paths, warnings: &mut Vec<Finding>) -> Result<()> {
    let config = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    let cur = fsops::sha256_file(config);
    if cur == e.prior.sha256 {
        return Ok(());
    }
    let ours = e.after.sha256.is_none() || cur == e.after.sha256;
    if !ours {
        warnings.push(
            Finding::new(
                "herdr_config_kept",
                format!(
                    "{} changed after the import's herdr uninstall; it is left as it is now",
                    paths.tilde(config)
                ),
            )
            .with_path(paths.tilde(config)),
        );
        return Ok(());
    }
    match (&e.prior.backup, e.prior.exists) {
        (Some(backup), _) => {
            let bytes = std::fs::read(backup)
                .with_context(|| format!("failed to read {}", backup.display()))?;
            crate::guest_write::atomic_replace(config, &bytes)?;
            if let Some(mode) = e.prior.mode {
                fsops::chmod(config, mode)?;
            }
        }
        (None, Some(false)) => fsops::remove_if_present(config)?,
        _ => {}
    }
    Ok(())
}

/// The command a person runs to put upstream's herdr plugin back at its
/// recorded commit, or `None` when the record names no GitHub install.
fn reinstall_argv(rec: &HerdrRecord) -> Option<Vec<String>> {
    if rec.kind.as_deref() != Some("github") {
        return None;
    }
    let (owner, repo, commit) = (
        rec.owner.as_ref()?,
        rec.repo.as_ref()?,
        rec.resolved_commit.as_ref()?,
    );
    Some(vec![
        "plugin".to_string(),
        "install".to_string(),
        format!("{owner}/{repo}/{UPSTREAM_HERDR_SUBDIR}"),
        "--ref".to_string(),
        commit.clone(),
        "--yes".to_string(),
    ])
}

/// G2's network half, AFTER the fence is released (spec §4.10 step 6):
/// for every undone G2 whose plugin herdr no longer lists, reinstall it at
/// the recorded commit, best effort. Returns the lines to print: on any
/// failure, the command to run by hand and upstream's own fallback.
pub(crate) fn reinstall_after_fence(j: &Journal) -> Vec<String> {
    let mut lines = Vec::new();
    for e in j
        .pre
        .iter()
        .filter(|e| e.op == Op::Exec && e.status == Status::Undone)
    {
        let Some(rec) = &e.prior.plugin else {
            continue;
        };
        let herdr = seams::herdr_bin();
        if let Some(herdr) = &herdr
            && upstream_record(herdr).is_some()
        {
            continue;
        }
        let manual = |why: &str| {
            let cmd = reinstall_argv(rec).map_or_else(
                || "herdr plugin link <upstream's herdr-plugin dir>".to_string(),
                |argv| format!("herdr {}", argv.join(" ")),
            );
            vec![
                format!(
                    "tollgate: upstream's herdr plugin was not reinstalled ({why}); run: {cmd}"
                ),
                format!(
                    "tollgate: or, with upstream restored: {} herdr install --yes --no-config",
                    crate::identity::UPSTREAM_NAME
                ),
            ]
        };
        let (Some(herdr), Some(argv)) = (herdr, reinstall_argv(rec)) else {
            lines.extend(manual("no herdr or no recorded GitHub commit"));
            continue;
        };
        seams::log(|| format!("herdr reinstall {}", argv.join(" ")));
        let mut cmd = std::process::Command::new(&herdr);
        cmd.args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::providers::billing_key::scrub_helper_env(&mut cmd);
        let ok = cmd
            .spawn()
            .ok()
            .and_then(|child| crate::herdr::run_bounded(child, REINSTALL_DEADLINE))
            .is_some_and(|out| out.status.success());
        if ok {
            lines.push(format!(
                "tollgate: reinstalled upstream's herdr plugin at {}",
                rec.resolved_commit.as_deref().unwrap_or_default()
            ));
        } else {
            lines.extend(manual("herdr refused or is offline"));
        }
    }
    for line in &lines {
        let line = line.clone();
        seams::log(|| format!("print {line}"));
    }
    lines
}
