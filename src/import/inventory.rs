//! The upstream inventory (spec §3.4): every entry under `~/.clauth`
//! classified `move` / `copy` / `copy-0600` / `merge` / `skip` / `never` /
//! `refuse`, exhaustively — anything the table does not name is refused as
//! `unknown_entry`, because an unrecognised file may carry a credential.
//! Also the static refusals on each entry the import touches: a foreign
//! owner, a symlink where a file or dir is expected, a hard-linked carrier,
//! and a source on another filesystem than its destination.
//!
//! Metadata only: no credential file is read here.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::fsops::{self, Meta};
use super::roster::Upstream;
use super::{Finding, Options, Paths};
use crate::harness::Harness;

/// What the import does with one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    /// A chain carrier, moved by rename.
    Move,
    /// A non-secret file, copied 0600.
    Copy,
    /// A secret-bearing file (an api key), copied 0600, size journaled only.
    CopySecret,
    /// A directory copied file by file, the destination winning.
    CopyTree,
    /// A secret-bearing directory copied the same way.
    CopyTreeSecret,
    /// A roster merged into tollgate's.
    MergeRoster,
    /// `session_profiles.json`, merged.
    MergeJson,
    /// Left behind (regenerated, logs, standing state, per-session trees).
    Skip,
    /// A lock file the fence holds.
    Never,
    /// Blocks the import.
    Refuse,
}

impl Action {
    /// The report's spelling (spec §2.4).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Action::Move => "move",
            Action::Copy | Action::CopyTree => "copy",
            Action::CopySecret | Action::CopyTreeSecret => "copy-0600",
            Action::MergeRoster | Action::MergeJson => "merge",
            Action::Skip => "skip",
            Action::Never => "never",
            Action::Refuse => "refuse",
        }
    }
}

/// One classified entry.
#[derive(Debug, Clone)]
pub(crate) struct Item {
    /// Relative to `~/.clauth`.
    pub(crate) rel: String,
    pub(crate) src: PathBuf,
    pub(crate) dst: Option<PathBuf>,
    pub(crate) action: Action,
    pub(crate) kind: &'static str,
    pub(crate) secret: bool,
    pub(crate) carrier: bool,
    pub(crate) reason: String,
    /// The upstream profile it belongs to.
    pub(crate) profile: Option<String>,
    pub(crate) meta: Option<Meta>,
}

/// An upstream profile the import brings over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) dst: String,
    pub(crate) harness: Harness,
    pub(crate) has_dir: bool,
    /// For a claude profile: upstream's install source file name
    /// (`session-token.json` or `credentials.json`).
    pub(crate) install_source: Option<String>,
}

/// The classified tree.
#[derive(Debug, Clone, Default)]
pub(crate) struct Inventory {
    pub(crate) items: Vec<Item>,
    pub(crate) profiles: Vec<Profile>,
    pub(crate) blockers: Vec<Finding>,
    pub(crate) warnings: Vec<Finding>,
}

const CLAUDE_CARRIERS: [&str; 4] = [
    "credentials.json",
    "session-token.json",
    "session-token.static.json",
    "mcp-logins.json",
];
const CODEX_CARRIERS: [&str; 3] = ["auth.json", "auth.lkg.json", "auth.quarantine.json"];
const PROFILE_COPIES: [&str; 9] = [
    "account_id.json",
    "profile_fetched.json",
    "usage_cache.json",
    "third_party_cache.json",
    "third_party_auth.json",
    "throughput_cache.json",
    "touch-receipt.json",
    "usage_history.jsonl",
    "wallet_history.jsonl",
];
const PROFILE_SKIPS: [&str; 3] = ["adopt_refusal.json", "kick_block.json", "auth.attempt"];
const TOP_SKIPS: [&str; 20] = [
    "status.json",
    "status_cache.json",
    "throughput_cache.json",
    "clauth.log",
    "daemon.log",
    "clauthd.pid",
    "gateway-child.json",
    "completions",
    ".completions_installed",
    "live_bare",
    "mcp_live",
    "live_sessions",
    "jobs",
    "devices.json",
    "pairing.json",
    "auth_token.json",
    "tls.json",
    "gateway-admin-token",
    "keychain-item-owners.json",
    "keychain-deletes-in-flight.json",
];
const NEVER: [&str; 5] = [
    "rotation-locks",
    ".lock",
    "clauthd.lock",
    "clauthd-standby.lock",
    "usage-fetch.lock",
];
const GATEWAY_FILES: [&str; 4] = ["gateway.toml", "shunt.toml", "shunt.yaml", "shunt.yml"];

/// Whether a top-level name is fence-held (the inventory hash skips these:
/// M3 creates them).
pub(crate) fn is_never(name: &str) -> bool {
    NEVER.contains(&name)
}

/// Whether a top-level name is a file upstream regenerates on any run (its
/// log, status feeds and caches): skipped by the import, and left out of
/// the inventory hash, since G2 runs upstream's binary between M-1 and M4.
pub(crate) fn is_regenerated(name: &str) -> bool {
    matches!(
        name,
        "status.json"
            | "status_cache.json"
            | "throughput_cache.json"
            | "clauth.log"
            | "daemon.log"
            | "clauthd.pid"
    ) || (name.contains("price_cache") && name.ends_with(".json"))
}

fn kind_of(meta: Option<&Meta>) -> &'static str {
    match meta {
        Some(m) if m.is_symlink => "symlink",
        Some(m) if m.is_dir => "dir",
        Some(m) if m.is_file => "file",
        Some(_) => "other",
        None => "missing",
    }
}

struct Walker<'a> {
    paths: &'a Paths,
    opts: &'a Options,
    uid: u32,
    allow_migrated: bool,
    inv: Inventory,
}

/// Classify `~/.clauth`. `allow_migrated` is set on a resume, whose own
/// tombstone may already exist.
pub(crate) fn classify(
    paths: &Paths,
    upstream: &Upstream,
    opts: &Options,
    allow_migrated: bool,
) -> Inventory {
    let mut w = Walker {
        paths,
        opts,
        uid: fsops::current_uid(&paths.home),
        allow_migrated,
        inv: Inventory::default(),
    };
    w.profiles(upstream);
    w.top_level();
    w.inv
}

fn sorted_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

impl Walker<'_> {
    fn refuse(&mut self, code: &str, message: String, rel: &str) {
        self.inv
            .blockers
            .push(Finding::new(code, message).with_path(format!("~/.clauth/{rel}")));
    }

    fn unknown(&mut self, rel: &str) {
        self.refuse(
            "unknown_entry",
            format!(
                "~/.clauth/{rel} is not in the import inventory; it may carry a credential, so nothing is imported"
            ),
            rel,
        );
    }

    /// Push an entry, running the static checks on every one the import
    /// touches.
    #[allow(
        clippy::too_many_arguments,
        reason = "one call site per table row; a struct literal would be longer"
    )]
    fn push(
        &mut self,
        rel: String,
        src: PathBuf,
        dst: Option<PathBuf>,
        action: Action,
        carrier: bool,
        reason: &str,
        profile: Option<String>,
    ) {
        let meta = fsops::lmeta(&src);
        let secret = matches!(action, Action::CopySecret | Action::CopyTreeSecret) || carrier;
        let touches = !matches!(action, Action::Skip | Action::Never | Action::Refuse);
        let mut action = action;
        if touches && let Some(m) = &meta {
            if m.is_symlink {
                self.refuse(
                    "symlinked_source",
                    format!("~/.clauth/{rel} is a symlink where a file or directory is expected; nothing is imported"),
                    &rel,
                );
                action = Action::Refuse;
            } else if m.uid != self.uid {
                self.refuse(
                    "foreign_owner",
                    format!("~/.clauth/{rel} is not owned by this user; nothing is imported"),
                    &rel,
                );
                action = Action::Refuse;
            } else if carrier && m.is_file && m.nlink > 1 {
                self.refuse(
                    "hardlinked_carrier",
                    format!(
                        "~/.clauth/{rel} has {} hard links; a chain must have exactly one inode, so nothing is imported",
                        m.nlink
                    ),
                    &rel,
                );
                action = Action::Refuse;
            } else if let Some(dst) = &dst
                && !fsops::same_device(fsops::parent(&src), fsops::parent(dst))
            {
                let msg = format!(
                    "{} and {} are on different filesystems; a credential is never copied",
                    self.paths.tilde(&src),
                    self.paths.tilde(dst)
                );
                self.refuse("cross_device", msg, &rel);
                action = Action::Refuse;
            }
        }
        self.inv.items.push(Item {
            kind: kind_of(meta.as_ref()),
            rel,
            src,
            dst,
            action,
            secret,
            carrier,
            reason: reason.to_string(),
            profile,
            meta,
        });
    }

    fn profiles(&mut self, upstream: &Upstream) {
        let root = self.paths.source.join("profiles");
        let dir_names = sorted_names(&root);
        let mut names: Vec<String> = upstream.claude.clone();
        for n in upstream.codex.iter().chain(dir_names.iter()) {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        names.sort();
        for name in names {
            let dir = root.join(&name);
            let rel_dir = format!("profiles/{name}");
            let meta = fsops::lmeta(&dir);
            match &meta {
                Some(m) if m.is_symlink => {
                    self.refuse(
                        "symlinked_source",
                        format!("~/.clauth/{rel_dir} is a symlink where a directory is expected; nothing is imported"),
                        &rel_dir,
                    );
                    continue;
                }
                Some(m) if !m.is_dir => {
                    self.unknown(&rel_dir);
                    continue;
                }
                Some(m) if m.uid != self.uid => {
                    self.refuse(
                        "foreign_owner",
                        format!(
                            "~/.clauth/{rel_dir} is not owned by this user; nothing is imported"
                        ),
                        &rel_dir,
                    );
                    continue;
                }
                _ => {}
            }
            let has_dir = meta.is_some();
            let harness = if upstream.codex.contains(&name) {
                Harness::Codex
            } else if upstream.claude.contains(&name) || !dir.join("auth.json").exists() {
                Harness::Claude
            } else {
                Harness::Codex
            };
            let dst = self.opts.dst_name(&name);
            let install_source = (harness == Harness::Claude && has_dir).then(|| {
                crate::claude::install_source_in(&dir)
                    .file_name()
                    .map_or_else(String::new, |s| s.to_string_lossy().into_owned())
            });
            self.inv.profiles.push(Profile {
                name: name.clone(),
                dst: dst.clone(),
                harness,
                has_dir,
                install_source,
            });
            if has_dir {
                self.profile_entries(&name, &dir, &dst);
            }
        }
    }

    fn profile_entries(&mut self, name: &str, dir: &Path, dst_name: &str) {
        let dst_dir = self.paths.tollgate_profile(dst_name);
        for entry in sorted_names(dir) {
            let rel = format!("profiles/{name}/{entry}");
            let src = dir.join(&entry);
            let dst = Some(dst_dir.join(&entry));
            let p = Some(name.to_string());
            let e = entry.as_str();
            if CLAUDE_CARRIERS.contains(&e) {
                self.push(rel, src, dst, Action::Move, true, "claude store", p);
            } else if e == "quarantine" {
                self.push(
                    rel,
                    src,
                    dst,
                    Action::Move,
                    true,
                    "claude quarantined chain",
                    p,
                );
            } else if CODEX_CARRIERS.contains(&e) {
                self.push(rel, src, dst, Action::Move, true, "codex store", p);
            } else if e.ends_with(".pending") {
                let msg = format!(
                    "~/.clauth/profiles/{name}/{e} is a crashed rotation's staged chain; run 'clauth list' once so upstream adopts it, then retry"
                );
                self.refuse("pending_rotation", msg, &rel);
                self.push(rel, src, None, Action::Refuse, true, "staged chain", p);
            } else if fsops::is_stray_temp(e) {
                self.stray_temp(&rel);
                self.push(rel, src, None, Action::Refuse, true, "crashed write", p);
            } else if e == "config.toml" {
                self.push(
                    rel,
                    src,
                    dst,
                    Action::CopySecret,
                    false,
                    "profile config (api key)",
                    p,
                );
            } else if PROFILE_COPIES.contains(&e) {
                self.push(rel, src, dst, Action::Copy, false, "profile cache", p);
            } else if PROFILE_SKIPS.contains(&e) {
                self.push(
                    rel,
                    src,
                    None,
                    Action::Skip,
                    false,
                    "standing state, re-derived",
                    p,
                );
            } else if e == crate::runtime::CODEX_HOME_STEM {
                if let Some(found) = fsops::find_regular_named(&src, &["auth.json"], 64) {
                    let shown = self.paths.tilde(&found);
                    self.refuse(
                        "codex_home_carrier",
                        format!("{shown} is a second copy of a codex chain inside codex-home; remove it with clauth first"),
                        &rel,
                    );
                    self.push(
                        rel,
                        src,
                        None,
                        Action::Refuse,
                        false,
                        "codex home holds a chain",
                        p,
                    );
                } else {
                    self.push(rel, src, dst, Action::CopyTree, false, "codex home", p);
                }
            } else if e.starts_with("runtime")
                || e.starts_with("sessions")
                || e.starts_with("codex-home-")
            {
                if let Some(found) =
                    fsops::find_regular_named(&src, &[".credentials.json", "auth.json"], 8)
                {
                    let shown = self.paths.tilde(&found);
                    self.refuse(
                        "stale_runtime_carrier",
                        format!("{shown} is a regular-file copy of a chain in a stale session tree; remove the tree with clauth first"),
                        &rel,
                    );
                    self.push(
                        rel,
                        src,
                        None,
                        Action::Refuse,
                        false,
                        "stale runtime carrier",
                        p,
                    );
                } else {
                    self.push(rel, src, None, Action::Skip, false, "per-session tree", p);
                }
            } else {
                self.unknown(&rel);
                self.push(rel, src, None, Action::Refuse, false, "unknown entry", p);
            }
        }
    }

    fn stray_temp(&mut self, rel: &str) {
        self.refuse(
            "stray_temp",
            format!("~/.clauth/{rel} is a crashed write's staging file and may hold the newest chain; run 'clauth list' once, then retry"),
            rel,
        );
    }

    fn top_level(&mut self) {
        let source = self.paths.source.clone();
        let target = self.paths.target.clone();
        for name in sorted_names(&source) {
            let src = source.join(&name);
            let dst = Some(target.join(&name));
            let rel = name.clone();
            let n = name.as_str();
            match n {
                "profiles" => {
                    let meta = fsops::lmeta(&src);
                    if meta.is_some_and(|m| m.is_symlink || !m.is_dir) {
                        self.refuse(
                            "symlinked_source",
                            "~/.clauth/profiles is not a directory; nothing is imported"
                                .to_string(),
                            &rel,
                        );
                    }
                }
                "profiles.toml" | "codex-profiles.toml" => {
                    self.push(rel, src, dst, Action::MergeRoster, false, "roster", None);
                }
                "conversations" => {
                    self.push(
                        rel,
                        src,
                        dst,
                        Action::CopyTree,
                        false,
                        "destination wins on a name",
                        None,
                    );
                }
                "session_profiles.json" => {
                    self.push(
                        rel,
                        src,
                        dst,
                        Action::MergeJson,
                        false,
                        "session owners",
                        None,
                    );
                }
                "token_ledger.json" | "presets" => {
                    let exists = dst.as_ref().is_some_and(|d| d.exists());
                    if exists {
                        self.inv.warnings.push(
                            Finding::new(
                                "destination_kept",
                                format!("~/.tollgate/{n} exists; upstream's copy is not imported"),
                            )
                            .with_path(format!("~/.clauth/{n}")),
                        );
                        self.push(
                            rel,
                            src,
                            None,
                            Action::Skip,
                            false,
                            "tollgate's copy kept",
                            None,
                        );
                    } else {
                        let action = if n == "presets" {
                            Action::CopyTree
                        } else {
                            Action::Copy
                        };
                        self.push(rel, src, dst, action, false, "shared state", None);
                    }
                }
                _ if GATEWAY_FILES.contains(&n) || n == "shunt" => {
                    if dst.as_ref().is_some_and(|d| d.exists()) {
                        self.refuse(
                            "destination_exists",
                            format!("~/.tollgate/{n} already exists; move one aside, then retry"),
                            &rel,
                        );
                        self.push(
                            rel,
                            src,
                            None,
                            Action::Refuse,
                            false,
                            "destination exists",
                            None,
                        );
                    } else {
                        let action = if n == "shunt" {
                            Action::CopyTreeSecret
                        } else {
                            Action::CopySecret
                        };
                        self.push(rel, src, dst, action, false, "gateway config (keys)", None);
                    }
                }
                _ if TOP_SKIPS.contains(&n)
                    || (n.contains("price_cache") && n.ends_with(".json")) =>
                {
                    self.push(
                        rel,
                        src,
                        None,
                        Action::Skip,
                        false,
                        "regenerated or log",
                        None,
                    );
                }
                _ if NEVER.contains(&n) => {
                    self.push(
                        rel,
                        src,
                        None,
                        Action::Never,
                        false,
                        "held by the fence",
                        None,
                    );
                }
                "MIGRATED" if self.allow_migrated => {
                    self.push(
                        rel,
                        src,
                        None,
                        Action::Skip,
                        false,
                        "this import's tombstone",
                        None,
                    );
                }
                "MIGRATED" => {
                    self.refuse(
                        "already_migrated",
                        "~/.clauth/MIGRATED exists: this upstream tree was already imported; run 'tollgate import status'".to_string(),
                        &rel,
                    );
                    self.push(rel, src, None, Action::Refuse, false, "tombstone", None);
                }
                _ if fsops::is_stray_temp(n) => {
                    self.stray_temp(&rel);
                    self.push(rel, src, None, Action::Refuse, true, "crashed write", None);
                }
                _ => {
                    self.unknown(&rel);
                    self.push(rel, src, None, Action::Refuse, false, "unknown entry", None);
                }
            }
        }
    }
}
