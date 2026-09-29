//! Rosters, names and merges (spec §4.7), and F2's upstream roster edit
//! (§4.9).
//!
//! Uniqueness goes through `actions::validate_profile_name` and
//! `profile_dir().exists()` alone: that function owns the one namespace
//! across every roster, case-insensitively, so a roster a later lane adds is
//! covered here with no code of its own. Every roster write is raw
//! `toml_edit` over the file's bytes — never a `Config` or `ProfileTtl`
//! ranked helper — because it runs under the fence's state flock (§4.2).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{Array, DocumentMut, Item, value};

use super::{Finding, Options, Paths};
use crate::harness::Harness;

/// Keys the merge manages itself; every other top-level key is "tollgate's
/// when it sets it, else upstream's".
const MANAGED: [&str; 4] = [
    "profiles",
    "fallback_chain",
    "auth_broken",
    "active_profile",
];

/// Tables never imported: tollgate's own ports, update channel and API
/// (spec I8).
const NEVER_IMPORTED: [&str; 3] = ["serve", "update", "local_api"];

/// Upstream's two rosters as read at M-1.
#[derive(Debug, Clone, Default)]
pub(crate) struct Upstream {
    pub(crate) claude: Vec<String>,
    pub(crate) codex: Vec<String>,
    pub(crate) claude_active: Option<String>,
    pub(crate) codex_active: Option<String>,
    /// `profiles.toml`'s bytes, when present.
    pub(crate) claude_doc: Option<String>,
    /// `codex-profiles.toml`'s bytes, when present.
    pub(crate) codex_doc: Option<String>,
}

fn parse(text: &str, path: &Path) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .with_context(|| format!("{} does not parse", path.display()))
}

fn strings(doc: &DocumentMut, key: &str) -> Vec<String> {
    doc.get(key)
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn active(doc: &DocumentMut) -> Option<String> {
    doc.get("active_profile")
        .and_then(Item::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn read_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Read upstream's rosters. An unreadable one is a blocker: the import
/// cannot know which stores it names.
pub(crate) fn load_upstream(paths: &Paths) -> (Upstream, Vec<Finding>) {
    let mut up = Upstream::default();
    let mut blockers = Vec::new();
    for (file, is_codex) in [("profiles.toml", false), ("codex-profiles.toml", true)] {
        let path = paths.source.join(file);
        let text = match read_text(&path) {
            Ok(t) => t,
            Err(e) => {
                blockers.push(roster_unreadable(paths, &path, &e));
                continue;
            }
        };
        let Some(text) = text else { continue };
        match parse(&text, &path) {
            Ok(doc) => {
                if is_codex {
                    up.codex = strings(&doc, "profiles");
                    up.codex_active = active(&doc);
                    up.codex_doc = Some(text);
                } else {
                    up.claude = strings(&doc, "profiles");
                    up.claude_active = active(&doc);
                    up.claude_doc = Some(text);
                }
            }
            Err(e) => blockers.push(roster_unreadable(paths, &path, &e)),
        }
    }
    (up, blockers)
}

fn roster_unreadable(paths: &Paths, path: &Path, e: &anyhow::Error) -> Finding {
    let shown = paths.tilde(path);
    Finding::new(
        "roster_unreadable",
        format!("{shown} cannot be read ({e}); nothing is imported"),
    )
    .with_path(shown)
}

fn other(h: Harness) -> Harness {
    match h {
        Harness::Claude => Harness::Codex,
        Harness::Codex => Harness::Claude,
    }
}

fn collision(harness: Harness, name: &str, src: &str) -> Finding {
    Finding::new(
        "name_collision",
        format!(
            "a tollgate {harness} profile named '{name}' already exists; pass --rename {src}=<new>"
        ),
    )
}

/// The M2 name checks for every imported profile (`(upstream name,
/// harness)`), with `--rename` applied. Read-only, lock-free.
pub(crate) fn check_names(
    paths: &Paths,
    profiles: &[(String, Harness)],
    opts: &Options,
) -> Vec<Finding> {
    let mut blockers = Vec::new();
    // Both of tollgate's rosters must read, or no name can be checked
    // against them (and a collision would be misreported).
    if let Err(e) = crate::profile::claude_roster_names() {
        blockers.push(Finding::new(
            "tollgate_roster_unreadable",
            format!("~/.tollgate/profiles.toml cannot be read ({e:#}); fix it first"),
        ));
    }
    if let Err(e) = crate::codex_profiles::CodexState::load() {
        blockers.push(Finding::new(
            "tollgate_roster_unreadable",
            format!("~/.tollgate/codex-profiles.toml cannot be read ({e:#}); fix it first"),
        ));
    }
    if !blockers.is_empty() {
        return blockers;
    }
    for old in opts.renames.keys() {
        if !profiles.iter().any(|(n, _)| n == old) {
            blockers.push(Finding::new(
                "rename_unknown",
                format!("--rename {old}=…: upstream clauth has no profile named '{old}'"),
            ));
        }
    }
    let existing_dirs: Vec<String> = std::fs::read_dir(paths.target.join("profiles"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut seen: Vec<String> = Vec::new();
    for (name, harness) in profiles {
        let dst = opts.dst_name(name);
        if seen.iter().any(|s| s.eq_ignore_ascii_case(&dst)) {
            blockers.push(Finding::new(
                "rename_collision",
                format!("two imported profiles would both be named '{dst}'; rename one"),
            ));
            continue;
        }
        seen.push(dst.clone());
        if let Err(e) = crate::actions::validate_profile_name(&dst, *harness, None) {
            if crate::actions::validate_name_chars(&dst).is_err() {
                blockers.push(Finding::new(
                    "invalid_name",
                    format!("'{dst}' is not a valid tollgate profile name ({e}); pass --rename {name}=<new>"),
                ));
            } else {
                let holder =
                    if crate::actions::validate_foreign_harness_free(&dst, *harness).is_err() {
                        other(*harness)
                    } else {
                        *harness
                    };
                blockers.push(collision(holder, &dst, name));
            }
            continue;
        }
        if existing_dirs.iter().any(|d| d.eq_ignore_ascii_case(&dst)) {
            let mut f = collision(Harness::Claude, &dst, name);
            f.code = "destination_exists".to_string();
            blockers.push(f.with_path(format!("~/.tollgate/profiles/{dst}")));
        }
    }
    blockers
}

fn set_strings(doc: &mut DocumentMut, key: &str, names: &[String]) {
    let mut arr = Array::new();
    for n in names {
        arr.push(n.as_str());
    }
    doc[key] = value(arr);
}

/// Merge upstream's roster (`upstream`, names mapped through `opts`) into
/// tollgate's (`tollgate`, absent reads as empty): `profiles` and
/// `fallback_chain` append upstream's missing names, `auth_broken` unions,
/// `active_profile` is `slot_active` (the claude slot's profile) or
/// upstream's mapped active unless tollgate already set one, and every other
/// key is tollgate's when it sets it, else upstream's (`[serve]`, `[update]`,
/// `[local_api]` never). Returns the new text, the imported tollgate names,
/// and warnings.
pub(crate) fn merge(
    tollgate: Option<&str>,
    upstream: &str,
    opts: &Options,
    slot_active: Option<&str>,
) -> Result<(String, Vec<String>, Vec<Finding>)> {
    let path = Path::new("profiles.toml");
    let mut t = parse(tollgate.unwrap_or(""), path)?;
    let u = parse(upstream, path)?;
    let mut warnings = Vec::new();
    let map = |names: Vec<String>| -> Vec<String> {
        names.into_iter().map(|n| opts.dst_name(&n)).collect()
    };
    let up_profiles = map(strings(&u, "profiles"));
    let mut profiles = strings(&t, "profiles");
    let mut added = Vec::new();
    for n in &up_profiles {
        if !profiles.contains(n) {
            profiles.push(n.clone());
            added.push(n.clone());
        }
    }
    set_strings(&mut t, "profiles", &profiles);
    for key in ["fallback_chain", "auth_broken"] {
        let up = map(strings(&u, key));
        let had = t.contains_key(key);
        let mut mine = strings(&t, key);
        for n in up {
            if !mine.contains(&n) {
                mine.push(n);
            }
        }
        if had || !mine.is_empty() {
            set_strings(&mut t, key, &mine);
        }
    }
    let wanted = slot_active
        .map(str::to_string)
        .or_else(|| active(&u).map(|a| opts.dst_name(&a)));
    match (active(&t), wanted) {
        (Some(kept), Some(w)) if kept != w => warnings.push(Finding::new(
            "tollgate_active_kept",
            format!(
                "tollgate's active profile '{kept}' is kept; upstream's '{w}' is imported inactive"
            ),
        )),
        (Some(_), _) => {}
        (None, Some(w)) => t["active_profile"] = value(w),
        (None, None) => {}
    }
    for (key, item) in u.iter() {
        if MANAGED.contains(&key) || NEVER_IMPORTED.contains(&key) || t.contains_key(key) {
            continue;
        }
        let mut item = item.clone();
        if key == "home_tab"
            && let Some(raw) = item.as_str()
        {
            item = value(crate::profile::home_tab_alias(raw));
        }
        t.insert(key, item);
    }
    Ok((t.to_string(), added, warnings))
}

/// The semantic undo of [`merge`] when tollgate has written the roster since:
/// the imported names leave `profiles`, `fallback_chain` and `auth_broken`,
/// and an imported `active_profile` goes back to `prior_active` (or away).
/// Profiles created after the import stay.
pub(crate) fn unmerge(
    current: &str,
    imported: &[String],
    prior_active: Option<&str>,
) -> Result<String> {
    let mut doc = parse(current, Path::new("profiles.toml"))?;
    let gone: BTreeSet<&str> = imported.iter().map(String::as_str).collect();
    for key in ["profiles", "fallback_chain", "auth_broken"] {
        if doc.contains_key(key) {
            let kept: Vec<String> = strings(&doc, key)
                .into_iter()
                .filter(|n| !gone.contains(n.as_str()))
                .collect();
            set_strings(&mut doc, key, &kept);
        }
    }
    if let Some(a) = active(&doc)
        && gone.contains(a.as_str())
    {
        match prior_active {
            Some(p) => doc["active_profile"] = value(p),
            None => {
                doc.remove("active_profile");
            }
        }
    }
    Ok(doc.to_string())
}

/// Tollgate's active profile in `text`, if any.
pub(crate) fn active_of(text: &str) -> Option<String> {
    parse(text, Path::new("profiles.toml"))
        .ok()
        .and_then(|d| active(&d))
}

/// F2: `text` without `active_profile`, or `None` when it has none.
pub(crate) fn without_active(text: &str) -> Result<Option<String>> {
    let mut doc = parse(text, Path::new("profiles.toml"))?;
    if doc.remove("active_profile").is_none() {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// F2's semantic undo: `active_profile` set back to `prior` in `text`.
pub(crate) fn with_active(text: &str, prior: &str) -> Result<String> {
    let mut doc = parse(text, Path::new("profiles.toml"))?;
    doc["active_profile"] = value(prior);
    Ok(doc.to_string())
}

/// Merge upstream's `session_profiles.json` into tollgate's: the union of
/// both `sessions` maps, upstream owners mapped through `opts`; one id owned
/// by two different profiles becomes `contested`. Returns the new bytes and
/// the ids it added.
pub(crate) fn merge_sessions(
    tollgate: Option<&[u8]>,
    upstream: &[u8],
    opts: &Options,
) -> Result<(Vec<u8>, Vec<String>)> {
    use serde_json::{Map, Value};
    let parse_map = |bytes: &[u8]| -> Result<Map<String, Value>> {
        let v: Value =
            serde_json::from_slice(bytes).context("session_profiles.json does not parse")?;
        Ok(v.get("sessions")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    };
    let mut doc: Map<String, Value> = match tollgate {
        Some(bytes) => {
            match serde_json::from_slice(bytes).context("session_profiles.json does not parse")? {
                Value::Object(m) => m,
                _ => Map::new(),
            }
        }
        None => Map::new(),
    };
    let mut mine = tollgate.map(parse_map).transpose()?.unwrap_or_default();
    let theirs = parse_map(upstream)?;
    let mut added = Vec::new();
    for (id, owner) in theirs {
        let owner = match owner.get("known").and_then(Value::as_str) {
            Some(p) => serde_json::json!({ "known": opts.dst_name(p) }),
            None => owner,
        };
        match mine.get(&id) {
            None => {
                mine.insert(id.clone(), owner);
                added.push(id);
            }
            Some(existing) if *existing == owner => {}
            Some(_) => {
                mine.insert(id, Value::String("contested".to_string()));
            }
        }
    }
    doc.insert("sessions".to_string(), Value::Object(mine));
    Ok((serde_json::to_vec(&Value::Object(doc))?, added))
}

/// The semantic undo of [`merge_sessions`]: the added ids leave.
pub(crate) fn unmerge_sessions(current: &[u8], added: &[String]) -> Result<Vec<u8>> {
    use serde_json::Value;
    let mut v: Value =
        serde_json::from_slice(current).context("session_profiles.json does not parse")?;
    if let Some(map) = v.get_mut("sessions").and_then(Value::as_object_mut) {
        for id in added {
            map.shift_remove(id);
        }
    }
    Ok(serde_json::to_vec(&v)?)
}
