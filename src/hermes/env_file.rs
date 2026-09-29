//! The home `.env`: tollgate manages exactly one line, the bound key, and
//! every other line stays the user's (D-H13, spec §4.2).
//!
//! The key line is written unquoted, so the key must be plain printable ASCII
//! with no quote, backslash or `#`: that way python-dotenv, which is what
//! Hermes loads the file with, parses it to the same bytes tollgate wrote.
//! The writer rewrites only a file it can prove it understands. Its key-name
//! scan must agree with the projector's dotenv parse, or it refuses: a
//! disagreement means a multiline value, where a line-wise rewrite could cut
//! a value in half.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use sha2::{Digest as _, Sha256};

use super::home::HermesPaths;

/// Files larger than this are refused, never read whole.
const MAX_ENV_BYTES: u64 = 1024 * 1024;

/// The temp-file stem of the writer; stale ones are removed before a write.
const TEMP_STEM: &str = ".env.tollgate-";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The header line the writer puts first. Recognised on the next rewrite and
/// dropped, so the header never stacks up.
pub(crate) fn header_line(var: &str, name: &str) -> String {
    format!("# tollgate: {var} is managed by 'tollgate hermes key {name}'; other lines are yours")
}

fn is_header_line(line: &str) -> bool {
    line.starts_with("# tollgate: ") && line.contains(" is managed by 'tollgate hermes key ")
}

/// §4.1 step 3: trim, then refuse an empty key or one with a byte outside
/// `0x21..=0x7e`, or `'`, `"`, `\` or `#`.
pub(crate) fn validate_key(raw: &str) -> Result<String> {
    let key = raw.trim();
    if key.is_empty() {
        bail!("the key is empty");
    }
    if let Some(bad) = key
        .bytes()
        .find(|b| !(0x21..=0x7e).contains(b) || matches!(b, b'\'' | b'"' | b'\\' | b'#'))
    {
        let shown = if (0x21..=0x7e).contains(&bad) {
            format!("'{}'", bad as char)
        } else {
            format!("byte 0x{bad:02x}")
        };
        bail!(
            "the key holds {shown}; tollgate writes the .env line unquoted, so a key must be \
             printable ASCII without quotes, backslashes or '#'"
        );
    }
    Ok(key.to_string())
}

/// Hermes' own fingerprint format: `"sha256:" + hex(sha256(key))[:16]`
/// (`agent/credential_persistence.py:123-131`).
pub(crate) fn fingerprint(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("sha256:{}", &hex::encode(digest)[..16])
}

/// The key name a line sets, by `^\s*(export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=`.
pub(crate) fn line_key(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    let rest = match rest.strip_prefix("export") {
        Some(after) if after.starts_with(char::is_whitespace) => after.trim_start(),
        _ => rest,
    };
    let end = rest
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_alphanumeric() || *c == '_') || (*i == 0 && c.is_ascii_digit()))
        .map_or(rest.len(), |(i, _)| i);
    if end == 0 {
        return None;
    }
    let (key, after) = rest.split_at(end);
    after.trim_start().starts_with('=').then_some(key)
}

/// The raw value of a `KEY=value` line: after the `=`, trimmed, with one pair
/// of matching outer quotes removed. Used only for the fingerprint audit, and
/// only on the bound key's line.
fn line_value(line: &str) -> &str {
    let v = line.split_once('=').map_or("", |(_, v)| v).trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

/// The file's text, refusing a symlink, a non-regular file, a foreign owner or
/// an oversized file (§4.2 step 1). `Ok(None)` when absent.
fn read_env(path: &Path) -> Result<Option<(String, std::fs::Metadata)>> {
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to stat {}", path.display())),
    };
    if meta.file_type().is_symlink() || !meta.is_file() || !super::home::owned_by_me(&meta) {
        bail!(".env is not a regular file tollgate can rewrite");
    }
    if meta.len() > MAX_ENV_BYTES {
        bail!(".env is larger than 1 MiB; edit it by hand");
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if bytes.contains(&0) {
        bail!(".env holds a NUL byte; edit it by hand");
    }
    let text = String::from_utf8(bytes).context(".env is not UTF-8; edit it by hand")?;
    Ok(Some((text, meta)))
}

/// Split into lines, dropping a leading BOM and each line's trailing `\r`.
fn lines(text: &str) -> Vec<&str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    if out.last() == Some(&"") {
        out.pop();
    }
    out
}

/// A value that opens a quote it does not close on the same line: the
/// line-wise view cannot see where it ends. The cross-check against the
/// projector catches the rest; this catches it even without one.
fn opens_multiline(line: &str) -> bool {
    if line_key(line).is_none() {
        return false;
    }
    let v = line.split_once('=').map_or("", |(_, v)| v).trim_start();
    for q in ['"', '\''] {
        if let Some(rest) = v.strip_prefix(q) {
            return !rest.contains(q);
        }
    }
    false
}

/// §4.2 step 2: the key-name scan, cross-checked against the projector's
/// dotenv key names when a projection is at hand.
fn scan(text: &str, projector_keys: Option<&BTreeSet<String>>, env_path: &Path) -> Result<()> {
    let refuse = || {
        anyhow::anyhow!(
            "cannot safely rewrite {} (multiline values); edit it by hand",
            env_path.display()
        )
    };
    let ls = lines(text);
    if ls.iter().any(|l| opens_multiline(l)) {
        return Err(refuse());
    }
    if let Some(theirs) = projector_keys {
        let ours: BTreeSet<String> = ls
            .iter()
            .filter_map(|l| line_key(l))
            .map(String::from)
            .collect();
        if &ours != theirs {
            return Err(refuse());
        }
    }
    Ok(())
}

/// Whether the rewrite changed the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    Written,
    Unchanged,
}

/// The `.env` writer, §4.2 steps 1–6. Runs only under `name`'s RotationGuard
/// (the witness) and refuses while the profile has a live session, unknown
/// counting as live. Step 7, the roster fingerprint, is the caller's, under
/// `HermesState::update` nested inside the same guard.
pub(crate) fn write_key(
    paths: &HermesPaths,
    name: &str,
    var: &str,
    key: &str,
    projector_keys: Option<&BTreeSet<String>>,
    _rotation: &crate::runtime::RotationGuard,
) -> Result<Written> {
    if crate::runtime::has_live_session(&crate::profile::ProfileName::from(name)) {
        bail!(
            "tollgate: hermes '{name}': the home has a live session; the .env is rewritten only \
             while the home is idle"
        );
    }
    let path = paths.env_file();
    let existing = read_env(&path)?;
    let old_text = existing.as_ref().map_or("", |(t, _)| t.as_str());
    scan(old_text, projector_keys, &path)?;

    let mut new = String::new();
    new.push_str(&header_line(var, name));
    new.push('\n');
    new.push_str(&format!("{var}={key}\n"));
    for line in lines(old_text) {
        if line_key(line) == Some(var) || is_header_line(line) {
            continue;
        }
        new.push_str(line);
        new.push('\n');
    }

    if let Some((text, meta)) = &existing
        && text == &new
        && mode_is_600(meta)
    {
        return Ok(Written::Unchanged);
    }
    remove_stale_temps(&paths.home)?;
    write_atomic_600(&path, new.as_bytes())?;
    Ok(Written::Written)
}

#[cfg(unix)]
fn mode_is_600(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777 == 0o600
}

#[cfg(not(unix))]
fn mode_is_600(_meta: &std::fs::Metadata) -> bool {
    true
}

/// §4.2 step 5: any `.env.tollgate-*` a crashed writer left behind. No live
/// writer can own one: the caller holds the profile's RotationGuard.
fn remove_stale_temps(home: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(TEMP_STEM) {
            std::fs::remove_file(entry.path())
                .with_context(|| format!("failed to remove {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// A same-directory temp created `O_CREAT|O_EXCL` at 0600, then `rename(2)`.
fn write_atomic_600(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path.parent().context(".env has no parent dir")?;
    let tmp = dir.join(format!(
        "{TEMP_STEM}{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = opts.open(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("failed to write {}", path.display()))
}

/// What the launch audit (§4.2, the last paragraph) found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyAudit {
    pub(crate) fingerprint: String,
}

/// The launch-time audit: the home `.env` must hold a non-blank `var`
/// (M-NOKEY otherwise). The fingerprint returned is compared with the
/// roster's by the caller, which re-attributes on a difference. The file is
/// never rewritten here.
pub(crate) fn audit_key(
    paths: &HermesPaths,
    name: &str,
    var: &str,
    projector_keys: Option<&BTreeSet<String>>,
) -> Result<KeyAudit> {
    let path = paths.env_file();
    let nokey = || {
        anyhow::anyhow!(
            "tollgate: hermes '{name}': no {var} in {}/.env; run 'tollgate hermes key {name}'",
            paths.home.display()
        )
    };
    let Some((text, _)) = read_env(&path)? else {
        return Err(nokey());
    };
    scan(&text, projector_keys, &path)?;
    // python-dotenv's rule: the last assignment wins.
    let value = lines(&text)
        .into_iter()
        .filter(|l| line_key(l) == Some(var))
        .map(line_value)
        .next_back()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(nokey)?;
    Ok(KeyAudit {
        fingerprint: fingerprint(value),
    })
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_env_file.rs"]
mod tests;
