//! The live slots (spec §4.6): `~/.claude/.credentials.json` and
//! `~/.codex/auth.json`, classified at M-1 and again at M4.
//!
//! The rule the whole module serves: after the import every chain has
//! exactly one inode in the home, and a live slot is either a symlink to it
//! or an independent login. So a slot linked into an upstream store is
//! repointed in the same step that moves the store (`move_relink`); a
//! regular-file slot holding the upstream active profile's own login is
//! renamed onto the moved store (`capture`, I4) — or, on a long-lived
//! session-token profile, relinked and its copy discarded (I22); a diverged
//! one needs `--adopt-live` (I3). Tokens are compared in memory only.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::inventory::{Action, Inventory};
use super::roster::Upstream;
use super::{Finding, Options, Paths, fsops};
use crate::harness::Harness;
use crate::profile::ClaudeCredentials;

/// What the import does with the claude slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudePlan {
    Untouched,
    /// The slot links into `profile`'s moved carrier `file`: `move_relink`.
    Relink {
        profile: String,
        file: String,
    },
    /// A regular slot becomes `profile`'s store, then a link: `capture`.
    Capture {
        profile: String,
        live_ino: u64,
    },
    /// A regular slot on a session-token profile: the store moves, the slot
    /// is relinked to it and the live copy discarded (`move_relink` with
    /// `live_regular`).
    RelinkDiscard {
        profile: String,
        file: String,
        live_ino: u64,
        live_sha: String,
    },
}

/// What the import does with the codex slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexPlan {
    Untouched,
    /// Case (i): the slot links into `profile`'s `auth.json`.
    Relink {
        profile: String,
    },
}

/// A classified slot, with its report row.
#[derive(Debug, Clone)]
pub(crate) struct Slot<P> {
    pub(crate) state: &'static str,
    pub(crate) profile: Option<String>,
    pub(crate) verdict: &'static str,
    pub(crate) plan: P,
}

/// The report row (`live_slots.claude` / `.codex`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SlotRow {
    pub(crate) state: &'static str,
    pub(crate) profile: Option<String>,
    pub(crate) verdict: &'static str,
}

impl<P> Slot<P> {
    pub(crate) fn row(&self) -> SlotRow {
        SlotRow {
            state: self.state,
            profile: self.profile.clone(),
            verdict: self.verdict,
        }
    }
}

/// The slot's shape on disk.
fn state_of(path: &Path) -> &'static str {
    match fsops::lmeta(path) {
        None => "missing",
        Some(m) if m.is_symlink => "symlink",
        Some(m) if m.is_file => "regular",
        Some(_) => "other",
    }
}

/// The `(profile, file)` a link target names under `~/.clauth/profiles`,
/// compared lexically and then canonically.
fn upstream_store_of(paths: &Paths, target: &Path) -> Option<(String, String)> {
    let root = paths.source.join("profiles");
    let rel = target
        .strip_prefix(&root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let (Ok(t), Ok(r)) = (target.canonicalize(), root.canonicalize()) else {
                return None;
            };
            t.strip_prefix(r).ok().map(Path::to_path_buf)
        })?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match parts.as_slice() {
        [p, f] => Some((p.clone(), f.clone())),
        _ => None,
    }
}

fn is_moved(inv: &Inventory, profile: &str, file: &str) -> bool {
    inv.items.iter().any(|i| {
        i.action == Action::Move
            && i.profile.as_deref() == Some(profile)
            && i.rel == format!("profiles/{profile}/{file}")
    })
}

fn read_creds(path: &Path) -> Option<ClaudeCredentials> {
    crate::profile::read_json_file::<ClaudeCredentials>(path).ok()
}

fn refuse<P>(state: &'static str, profile: Option<String>, plan: P) -> Slot<P> {
    Slot {
        state,
        profile,
        verdict: "refuse",
        plan,
    }
}

/// Classify `~/.claude/.credentials.json`.
pub(crate) fn classify_claude(
    paths: &Paths,
    inv: &Inventory,
    upstream: &Upstream,
    opts: &Options,
) -> (Slot<ClaudePlan>, Vec<Finding>, Vec<Finding>) {
    let slot = paths.claude_slot();
    let state = state_of(&slot);
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    let untouched = |state| Slot {
        state,
        profile: None,
        verdict: "untouched",
        plan: ClaudePlan::Untouched,
    };
    let out = match state {
        "missing" => untouched(state),
        "symlink" => {
            let target = fsops::link_target(&slot).unwrap_or_default();
            match upstream_store_of(paths, &target) {
                Some((p, f)) if is_moved(inv, &p, &f) => {
                    if upstream.claude_active.as_deref().is_some_and(|a| a != p) {
                        warnings.push(Finding::new(
                            "live_profile_differs",
                            format!(
                                "~/.claude/.credentials.json links to profile '{p}', not upstream's active profile; '{p}' becomes tollgate's active profile"
                            ),
                        ));
                    }
                    Slot {
                        state,
                        profile: Some(p.clone()),
                        verdict: "relink",
                        plan: ClaudePlan::Relink {
                            profile: p,
                            file: f,
                        },
                    }
                }
                _ if target.starts_with(&paths.target) => untouched(state),
                _ => {
                    blockers.push(
                        Finding::new(
                            "live_link_foreign",
                            format!(
                                "~/.claude/.credentials.json links to {}, which the import does not move; relink it with clauth first",
                                paths.tilde(&target)
                            ),
                        )
                        .with_path("~/.claude/.credentials.json"),
                    );
                    refuse(state, None, ClaudePlan::Untouched)
                }
            }
        }
        "regular" => classify_regular(paths, inv, upstream, opts, &mut blockers),
        _ => {
            blockers.push(Finding::new(
                "live_unclassifiable",
                "~/.claude/.credentials.json is neither a file nor a link; nothing is imported",
            ));
            refuse(state, None, ClaudePlan::Untouched)
        }
    };
    if out.plan != ClaudePlan::Untouched && !fsops::same_device(&paths.claude, &paths.target) {
        blockers.push(Finding::new(
            "cross_device",
            "~/.claude and ~/.tollgate are on different filesystems; a credential is never copied",
        ));
    }
    (out, blockers, warnings)
}

fn classify_regular(
    paths: &Paths,
    inv: &Inventory,
    upstream: &Upstream,
    opts: &Options,
    blockers: &mut Vec<Finding>,
) -> Slot<ClaudePlan> {
    let slot = paths.claude_slot();
    let state = "regular";
    let Some(live) = read_creds(&slot) else {
        blockers.push(Finding::new(
            "live_unclassifiable",
            "~/.claude/.credentials.json does not parse as a Claude Code login; nothing is imported",
        ));
        return refuse(state, None, ClaudePlan::Untouched);
    };
    let live_ino = fsops::lmeta(&slot).map_or(0, |m| m.ino);
    let claude_profiles: Vec<&str> = inv
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Claude && p.has_dir)
        .map(|p| p.name.as_str())
        .collect();
    // Which profile the slot belongs to, and whether it holds that profile's
    // own login (Same) or another (Diverged).
    let (profile, same) = match &upstream.claude_active {
        Some(a) => {
            let dir = paths.upstream_profile(a);
            let source = crate::claude::install_source_in(&dir);
            let same = read_creds(&source).is_some_and(|stored| {
                live.access_token().is_some_and(|t| !t.is_empty())
                    && live.access_token() == stored.access_token()
            });
            (a.clone(), same)
        }
        None => {
            let matched = live
                .refresh_token()
                .filter(|t| !t.is_empty())
                .and_then(|rt| {
                    claude_profiles.iter().find(|p| {
                        read_creds(&paths.upstream_profile(p).join("credentials.json"))
                            .is_some_and(|s| s.refresh_token() == Some(rt))
                    })
                });
            match matched {
                Some(p) => ((*p).to_string(), true),
                None => {
                    return Slot {
                        state,
                        profile: None,
                        verdict: "untouched",
                        plan: ClaudePlan::Untouched,
                    };
                }
            }
        }
    };
    let dir = paths.upstream_profile(&profile);
    let source = crate::claude::install_source_in(&dir);
    let on_static = source
        .file_name()
        .is_some_and(|n| n == "session-token.json");
    match (same, on_static) {
        (true, false) => capture(state, profile, live_ino),
        (true, true) => {
            let live_sha = fsops::sha256_file(&slot).unwrap_or_default();
            Slot {
                state,
                profile: Some(profile.clone()),
                verdict: "relink",
                plan: ClaudePlan::RelinkDiscard {
                    profile,
                    file: "session-token.json".to_string(),
                    live_ino,
                    live_sha,
                },
            }
        }
        (false, false) if opts.adopt_live => capture(state, profile, live_ino),
        (false, false) => {
            blockers.push(Finding::new(
                "claude_live_diverged",
                format!(
                    "~/.claude/.credentials.json holds a login that differs from profile '{profile}'; pass --adopt-live to import it as '{profile}' (the stored chain is discarded)"
                ),
            ));
            refuse(state, Some(profile), ClaudePlan::Untouched)
        }
        (false, true) => {
            blockers.push(Finding::new(
                "live_diverged_on_static_token",
                format!(
                    "~/.claude/.credentials.json holds a login that differs from profile '{profile}''s long-lived token; tollgate cannot tell which one to keep, so relink it with clauth first"
                ),
            ));
            refuse(state, Some(profile), ClaudePlan::Untouched)
        }
    }
}

fn capture(state: &'static str, profile: String, live_ino: u64) -> Slot<ClaudePlan> {
    Slot {
        state,
        profile: Some(profile.clone()),
        verdict: "capture",
        plan: ClaudePlan::Capture { profile, live_ino },
    }
}

fn codex_refresh(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    crate::codex_auth::CodexAuth::parse(&bytes)
        .ok()?
        .refresh_token()
        .map(str::to_string)
}

/// Classify `~/.codex/auth.json`: (i) a link into an upstream codex store is
/// relinked with the move, (ii) a regular copy of a store's chain refuses,
/// (iii) anything else is an independent login and untouched.
pub(crate) fn classify_codex(paths: &Paths, inv: &Inventory) -> (Slot<CodexPlan>, Vec<Finding>) {
    let slot = paths.codex_slot();
    let state = state_of(&slot);
    let mut blockers = Vec::new();
    let untouched = Slot {
        state,
        profile: None,
        verdict: "untouched",
        plan: CodexPlan::Untouched,
    };
    let out = match state {
        "symlink" => {
            let target = fsops::link_target(&slot).unwrap_or_default();
            match upstream_store_of(paths, &target) {
                Some((p, f)) if f == "auth.json" && is_moved(inv, &p, &f) => Slot {
                    state,
                    profile: Some(p.clone()),
                    verdict: "relink",
                    plan: CodexPlan::Relink { profile: p },
                },
                _ => untouched,
            }
        }
        "regular" => {
            let live = codex_refresh(&slot);
            let copy_of = live.as_deref().and_then(|rt| {
                inv.profiles
                    .iter()
                    .filter(|p| p.harness == Harness::Codex)
                    .find(|p| {
                        codex_refresh(&paths.upstream_profile(&p.name).join("auth.json")).as_deref()
                            == Some(rt)
                    })
            });
            match copy_of {
                Some(p) => {
                    blockers.push(Finding::new(
                        "codex_second_carrier",
                        format!(
                            "~/.codex/auth.json is a copy of profile '{}''s chain; relink it with 'clauth' first",
                            p.name
                        ),
                    ));
                    refuse(state, Some(p.name.clone()), CodexPlan::Untouched)
                }
                None => untouched,
            }
        }
        _ => untouched,
    };
    if out.plan != CodexPlan::Untouched && !fsops::same_device(&paths.codex, &paths.target) {
        blockers.push(Finding::new(
            "cross_device",
            "~/.codex and ~/.tollgate are on different filesystems; a credential is never copied",
        ));
    }
    (out, blockers)
}

/// The temp link a relink renames over `slot` (`<slot>.tollgate-import.<pid>`).
pub(crate) fn temp_for(slot: &Path) -> PathBuf {
    let name = slot
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    slot.with_file_name(format!("{name}.tollgate-import.{}", std::process::id()))
}
