//! Registry of live `tollgate start` sessions.
//!
//! One file per session at `~/.tollgate/live_sessions/<sid>.json`, mirroring the
//! `~/.tollgate/jobs/` convention ([`crate::mcp::jobs`]): a row is keyed by a
//! session id nobody else writes, so the file needs no ownership arbitration of
//! its own. Rows are filed by SESSION, never under the profile they launched on
//! — a session that swaps member would be misfiled the moment it moved.
//!
//! Two writers share a row, and both constraints they need are structural rather
//! than written down:
//!
//! - **The read is inside the lock, not just the write.** [`update`] is the only
//!   mutation path and it loads a FRESH row under [`with_state_lock`], hands the
//!   caller a borrow that cannot outlive the hold, and stores before releasing.
//!   There is deliberately no public load/store pair: a row read before a swap
//!   and written after would silently revert whatever the other writer put there
//!   in between.
//! - **Field ownership is a type, not a comment.** The daemon reaches its two
//!   fields through [`DaemonFields`] and the session its three through
//!   [`SessionFields`]; neither view can name the other's. Each still stores the
//!   whole freshly-loaded row, so writing one side preserves the other's.
//!
//! Liveness is the session's flock, exactly as for its runtime tree: a row is
//! dead once the marker named by its own `start_profile` + `isolated` +
//! `session_id` is no longer held, and `runtime::gc_stale_runtimes` reaps it
//! there (that module owns the marker layout; this one never rebuilds it).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lock::with_state_lock;
use crate::profile::{AppConfig, atomic_write_600, mkdir_700, tollgate_dir};
use crate::runtime::{SessionId, is_session_id};

/// One live session's row. Every field is written by exactly one of the two
/// writers; which one is enforced by the mutator views, not by this listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveSession {
    pub(crate) session_id: String,
    pub(crate) start_profile: String,
    /// Which harness's session this row describes. Liveness gating
    /// (delete/disable/rotation) never reads a row — it reads the flock-held
    /// markers, which a codex session stamps identically — and the tally keys
    /// on the name, which one namespace keeps unambiguous; both stay
    /// harness-blind with no help from this field. The tag exists for the
    /// consumers that must tell rows APART: the swap executor and the
    /// daemon's per-session decision leg skip codex rows when those sessions
    /// exist (codex reads `auth.json` once at start, so a mid-session member
    /// change is a no-op the executor would publish as a success). A Hermes
    /// row (`tollgate start <hermes-profile>`) is `follows_chain = false` with
    /// `launch_store: None`: the home is the account, switched by relaunch,
    /// and `sessions switch` refuses it with `sessions_cli::NON_CLAUDE_SWITCH`.
    /// `serde(default)` (= claude) is the upgrade gate: a row written by a
    /// tollgate that predates the axis is a claude row, which is what it was.
    #[serde(default)]
    pub(crate) harness: crate::harness::Harness,
    pub(crate) pid: u32,
    pub(crate) started_at: u64,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) isolated: bool,
    /// Whether this session follows the shared fallback chain. Set once at
    /// registration and never mutated, so it needs no view of its own — which is
    /// also why it is safe for the daemon's decision leg to READ it while the
    /// session owns it. `serde(default)` is the upgrade gate: a row written by a
    /// tollgate that predates the field must read as opted OUT, or the decision leg
    /// would move every already-running session off its launch account.
    #[serde(default)]
    pub(crate) follows_chain: bool,
    /// Daemon-owned: the member the decision leg wants this session on.
    #[serde(default)]
    pub(crate) intended_member: Option<String>,
    /// Daemon-owned: this session's position in the shared `fallback_chain`.
    #[serde(default)]
    pub(crate) chain_cursor: Option<usize>,
    /// Session-owned: the member this session's credential link resolves to.
    #[serde(default)]
    pub(crate) current_member: Option<String>,
    /// Session-owned: when this session last executed a swap.
    #[serde(default)]
    pub(crate) last_swap_at: Option<u64>,
    /// The credential store this session's rotation verdict reads, as an
    /// absolute path. Seeded at registration from the same value the runtime
    /// tree was built from, then repointed by every swap onto the member the
    /// session then reads (on macOS, only once the swap's keychain legs have
    /// landed): the verdict must answer for the member the session HOLDS, or a
    /// session swapped onto a refreshless member keeps refusing the launch
    /// member's rotations.
    ///
    /// A path rather than a decoded verdict, deliberately. What the rotation
    /// gate needs to know is whether this session is holding something
    /// rotatable, and the CONTENT at this path can change under a running
    /// session — `claude::heal_misfilled_sidecar` exists precisely because a
    /// rotating pair can land in a `session-token.json`. Freezing a boolean
    /// here would keep answering "refresh-less" while the file the session
    /// actually reads holds a live chain, so the test is made at rotation time
    /// against this path (`runtime::live_session_holds_rotatable`).
    ///
    /// `serde(default)` is the upgrade gate and the fail-closed direction at
    /// once: a row written by a tollgate that predates the field reads `None`,
    /// which every consumer must treat as "assume rotatable", so the macOS
    /// rotation refusal keeps applying to it exactly as it does today.
    #[serde(default)]
    pub(crate) launch_store: Option<PathBuf>,
    /// Session-written at registration: how this session changes account
    /// without a restart. Absent on a row that predates the field; read it
    /// through [`LiveSession::executor`], which derives it from the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) executor: Option<crate::hot_swap::Executor>,
    /// Session-written at registration: the transport class of an API-key
    /// shaped launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) launch_class: Option<crate::hot_swap::LaunchClass>,
    /// Session-owned (executor B): 0 at registration, +1 per commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key_generation: Option<u64>,
    /// Session-owned (executor B): when the last commit landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) committed_at: Option<u64>,
    /// Session-owned (executor B): the last refusal, cleared on commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) swap_refusal: Option<SwapRefusal>,
    /// Session-written at registration: the supervisor polls `.relaunch`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) relaunch_capable: bool,
    /// Session-written at registration: the sid this session was relaunched
    /// from, nonce-verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) relaunched_from: Option<String>,
    /// Daemon-written with an operator's request (`switch <sid>`): when the
    /// standing intent was last asked for. A refusal older than this is
    /// re-recorded even when it is the same member and code, so asking again
    /// never reads an older refusal as no answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) intended_at: Option<u64>,
}

/// Executor B's last refusal of an intended member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SwapRefusal {
    pub(crate) member: String,
    pub(crate) code: String,
    pub(crate) text: String,
    pub(crate) at_ms: u64,
}

impl LiveSession {
    /// A row for a session starting now. Pid, start time, and cwd are read here
    /// rather than passed in, so every registration reports them the same way.
    pub(crate) fn starting(
        session_id: &SessionId,
        start_profile: &str,
        harness: crate::harness::Harness,
        isolated: bool,
        follows_chain: bool,
        launch_store: Option<PathBuf>,
    ) -> Self {
        Self {
            session_id: session_id.as_str().to_string(),
            start_profile: start_profile.to_string(),
            harness,
            pid: std::process::id(),
            started_at: crate::usage::now_ms(),
            cwd: std::env::current_dir().ok(),
            isolated,
            follows_chain,
            intended_member: None,
            chain_cursor: None,
            current_member: None,
            last_swap_at: None,
            launch_store,
            executor: None,
            launch_class: None,
            key_generation: None,
            committed_at: None,
            swap_refusal: None,
            relaunch_capable: false,
            relaunched_from: None,
            intended_at: None,
        }
    }

    /// The executor this row runs under. A row without the field predates it:
    /// a claude row is executor A (what every such session was), a codex or
    /// Hermes row has no in-session executor.
    pub(crate) fn executor(&self) -> crate::hot_swap::Executor {
        self.executor.clone().unwrap_or(match self.harness {
            crate::harness::Harness::Claude => crate::hot_swap::Executor::Oauth,
            crate::harness::Harness::Codex | crate::harness::Harness::Hermes => {
                crate::hot_swap::Executor::None
            }
        })
    }

    /// Record the spawn-time executor choice. Executor B also starts at key
    /// generation 0 on its launch member, so the helper and every view have a
    /// committed point from the first request.
    pub(crate) fn with_executor(
        mut self,
        executor: crate::hot_swap::Executor,
        launch_class: Option<crate::hot_swap::LaunchClass>,
    ) -> Self {
        if executor == crate::hot_swap::Executor::ApiKey {
            self.key_generation = Some(0);
            self.current_member = Some(self.start_profile.clone());
        }
        self.executor = Some(executor);
        self.launch_class = launch_class;
        self
    }

    /// The spawn cwd (a resume's workspace) instead of the process cwd.
    pub(crate) fn with_cwd(mut self, cwd: Option<PathBuf>) -> Self {
        if cwd.is_some() {
            self.cwd = cwd;
        }
        self
    }

    /// Mark a `tollgate start` supervisor that polls `.relaunch`, and the
    /// session it was relaunched from.
    pub(crate) fn with_relaunch(mut self, capable: bool, from: Option<String>) -> Self {
        self.relaunch_capable = capable;
        self.relaunched_from = from;
        self
    }
}

/// The daemon's view of a row under [`update_as_daemon`]: the decision fields
/// and nothing else.
pub(crate) struct DaemonFields<'a>(&'a mut LiveSession);

impl DaemonFields<'_> {
    pub(crate) fn set_intended_member(&mut self, member: impl Into<String>) {
        self.0.intended_member = Some(member.into());
    }

    /// An operator's request: the intent plus when it was asked for
    /// ([`LiveSession::intended_at`]).
    pub(crate) fn request_member(&mut self, member: impl Into<String>) {
        self.0.intended_member = Some(member.into());
        self.0.intended_at = Some(crate::usage::now_ms());
    }

    pub(crate) fn set_chain_cursor(&mut self, cursor: usize) {
        self.0.chain_cursor = Some(cursor);
    }
}

/// The session's view of a row under [`update_as_session`]: the execution fields
/// and nothing else.
pub(crate) struct SessionFields<'a>(&'a mut LiveSession);

impl SessionFields<'_> {
    pub(crate) fn set_current_member(&mut self, member: impl Into<String>) {
        self.0.current_member = Some(member.into());
    }

    pub(crate) fn set_last_swap_at(&mut self, at: u64) {
        self.0.last_swap_at = Some(at);
    }

    /// Point the row's rotation verdict at the store the session reads now.
    /// The swap executor calls this once that is settled — inside its state-flock
    /// row update off macOS, after the keychain legs on macOS (the session's
    /// Claude Code resolves the item first, so the store it reads moves only
    /// when those legs land).
    pub(crate) fn set_launch_store(&mut self, store: PathBuf) {
        self.0.launch_store = Some(store);
    }

    /// Re-key the row onto the process that IS the session. A delegate's row is
    /// registered by the `tollgate mcp` that spawns it — `std::process::id()` at
    /// register time reads the mcp, not the delegate child — and the herdr
    /// pane-tag walk joins rows to processes by pid, so a row keyed on the mcp
    /// names a delegate's account for the pane hosting its parent session.
    pub(crate) fn set_pid(&mut self, pid: u32) {
        self.0.pid = pid;
    }

    /// Executor B's next key generation, stored and returned. Monotonic
    /// across loads: it counts from what the FRESH row holds.
    pub(crate) fn bump_key_generation(&mut self) -> u64 {
        let next = self.0.key_generation.unwrap_or(0) + 1;
        self.0.key_generation = Some(next);
        next
    }

    pub(crate) fn set_committed_at(&mut self, at: u64) {
        self.0.committed_at = Some(at);
    }

    pub(crate) fn set_swap_refusal(&mut self, refusal: SwapRefusal) {
        self.0.swap_refusal = Some(refusal);
    }

    pub(crate) fn clear_swap_refusal(&mut self) {
        self.0.swap_refusal = None;
    }
}

/// Live sessions tallied by the account each one is CURRENTLY running as.
///
/// Built from the registry rather than from per-profile marker counts: a session
/// that swapped A→B holds B's marker AND keeps A's (nothing can observe the live
/// child dropping A's tokens, so A must not rotate), which makes a marker sum
/// report one child as two sessions on two accounts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LiveTally(std::collections::BTreeMap<String, MemberSessions>);

/// One account's slice of a [`LiveTally`]. All-zero for an account hosting none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MemberSessions {
    pub(crate) sessions: usize,
    /// How many of `sessions` the fallback chain is allowed to move.
    pub(crate) following: usize,
    /// The newest swap ONTO this account. `None` when no session here has ever
    /// swapped, which is also what says no `current_member` pickup lag applies.
    pub(crate) last_swap_at: Option<u64>,
    /// How many of `sessions` are committed to another member and not served
    /// yet (executor B's `swapping`/`stalled`). Counted on the SERVED member,
    /// where the session's requests still authenticate.
    pub(crate) swapping: usize,
}

impl LiveTally {
    /// Read the registry and drop rows whose session is gone. Row GC runs from
    /// `runtime::gc_stale_runtimes` at daemon STARTUP, not per tick, so a
    /// SIGKILLed session's row outlives it for the whole daemon run. Gates on
    /// the same predicate the decision leg does, so a row cannot be live for one
    /// and dead for the other.
    pub(crate) fn collect(config: &AppConfig) -> Self {
        let mut tally = Self::from_live_rows(list().into_iter().filter(|row| {
            let probe = crate::profile::ProfileName::from(
                row.current_member.as_deref().unwrap_or(&row.start_profile),
            );
            crate::runtime::session_row_is_live(&probe, row.isolated, &row.session_id)
        }));
        tally.add_bare_sessions(config);
        tally
    }

    /// Fold in the BARE `claude` sessions — started without `tollgate start`, so
    /// they read the `~/.claude/.credentials.json` link tollgate owns and burn the
    /// account it resolves to. They hold no registry row on purpose: a row reaches
    /// the daemon's swap-decision leg, which reads [`list`] directly, and that leg
    /// may only move sessions tollgate supervises. Counting them HERE is what keeps
    /// them a display fact. What the count is actually taken from, and how loosely
    /// it stands in for a `claude` count, is
    /// [`crate::runtime::live_bare_sessions`].
    ///
    /// Attribution is resolved at READ time and never stored: `tollgate switch` and
    /// the fallback chain both repoint that one shared link mid-session. It also
    /// reads the link rather than `active_profile`, which under a divergence names
    /// an account the bare session does not authenticate as.
    ///
    /// They count as `following` as well, because the chain genuinely moves them:
    /// a global auto-switch repoints the link and Claude Code re-reads it.
    ///
    /// An unreadable marker dir counts as ZERO — the OPPOSITE direction to
    /// [`crate::runtime::has_live_session`], which gates delete, disable,
    /// rename and rotation and so must read an unknown as live. This tally
    /// only renders, so
    /// folding an unknown in as live would put a session on screen that nothing
    /// produced.
    fn add_bare_sessions(&mut self, config: &AppConfig) {
        let bare = crate::runtime::live_bare_sessions().unwrap_or(0);
        if bare == 0 {
            return;
        }
        let Some((member, _)) = crate::which::resolve_global(config) else {
            return;
        };
        let slot = self.0.entry(member).or_default();
        slot.sessions += bare;
        slot.following += bare;
    }

    /// Tally rows already known to be live. Attribution is `current_member`,
    /// which the executor writes only on a session's FIRST swap — so a session
    /// that never moved (every pinned one, and every follower before it swaps)
    /// is still running as the account it launched on.
    fn from_live_rows(rows: impl IntoIterator<Item = LiveSession>) -> Self {
        let mut per_member: std::collections::BTreeMap<String, MemberSessions> =
            std::collections::BTreeMap::new();
        for row in rows {
            // The SERVED member: a B session committed elsewhere still sends
            // the previous member's key until its helper serves the commit.
            // Only a B row has an ack worth reading.
            let ack = (row.executor() == crate::hot_swap::Executor::ApiKey)
                .then(|| read_helper_ack(&row.session_id))
                .flatten();
            let view = crate::hot_swap::SwapView::of(&row, ack.as_ref());
            let member = view.served_member().map_or_else(
                || {
                    row.current_member
                        .clone()
                        .unwrap_or_else(|| row.start_profile.clone())
                },
                str::to_string,
            );
            let slot = per_member.entry(member).or_default();
            slot.sessions += 1;
            slot.following += usize::from(row.follows_chain);
            slot.last_swap_at = slot.last_swap_at.max(row.last_swap_at);
            slot.swapping += usize::from(matches!(
                view.state,
                crate::hot_swap::SwapState::Swapping | crate::hot_swap::SwapState::Stalled
            ));
        }
        Self(per_member)
    }

    /// A tally straight from rows, for tests in other modules that need a fleet
    /// without laying registry files down. Skips the liveness filter, which is
    /// the half [`LiveTally::collect`] adds and this module's own tests pin.
    #[cfg(test)]
    pub(crate) fn of(rows: impl IntoIterator<Item = LiveSession>) -> Self {
        Self::from_live_rows(rows)
    }

    /// One account's sessions.
    pub(crate) fn member(&self, name: &crate::profile::ProfileName) -> MemberSessions {
        self.0.get(name.as_str()).copied().unwrap_or_default()
    }
}

fn registry_dir() -> Result<PathBuf> {
    Ok(tollgate_dir()?.join("live_sessions"))
}

/// Path of one session's row. The id shape is validated first: `list` reads ids
/// back off disk and the daemon's decision leg passes them around, so this join
/// must never take a `..` or a separator from a file someone else wrote.
fn row_path(session_id: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        is_session_id(session_id),
        "not a session id: {session_id:?}"
    );
    Ok(registry_dir()?.join(format!("{session_id}.json")))
}

/// Owner-only like every `~/.tollgate` write: a row carries the session's cwd and
/// which account it is running as.
fn write_row(row: &LiveSession) -> Result<()> {
    let path = row_path(&row.session_id)?;
    if let Some(parent) = path.parent() {
        mkdir_700(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec(row)?;
    atomic_write_600(&path, &bytes).with_context(|| format!("failed to write {}", path.display()))
}

/// File a starting session's row. Called once the session's liveness marker is
/// flock-held, so a row never exists without something for GC to test it by.
pub(crate) fn register(row: &LiveSession) -> Result<()> {
    with_state_lock(|_held| write_row(row))
}

/// Drop a session's row. Idempotent — a row already reaped by GC is not an
/// error. Takes the state lock so it cannot land between an [`update`]'s load and
/// its store and leave the row resurrected.
pub(crate) fn unregister(session_id: &str) -> Result<()> {
    let path = row_path(session_id)?;
    with_state_lock(|_held| match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    })
}

/// Snapshot of every registered row. Read-only: the returned rows are owned
/// copies, so nothing a caller does to one reaches disk. An unreadable or
/// unparseable file is skipped rather than failing the sweep.
pub(crate) fn list() -> Vec<LiveSession> {
    let Ok(dir) = registry_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    // By NAME, not by parse failure: the registry dir also holds each
    // session's `.helper`, `.helper.lock` and `.relaunch*` sidecars.
    entries
        .flatten()
        .filter(|entry| is_row_file_name(&entry.file_name()))
        .filter_map(|entry| read_row(&entry.path()))
        .collect()
}

/// `<sid>.json` with a valid session id stem, and nothing else.
fn is_row_file_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|n| n.strip_suffix(".json"))
        .is_some_and(is_session_id)
}

/// A sidecar of one session's row: `<sid>.<suffix>` beside `<sid>.json`.
fn sidecar_path(session_id: &str, suffix: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        is_session_id(session_id),
        "not a session id: {session_id:?}"
    );
    Ok(registry_dir()?.join(format!("{session_id}.{suffix}")))
}

/// A sidecar's path for a fixture, its registry dir created.
#[cfg(test)]
pub(crate) fn sidecar_path_for_test(session_id: &str, suffix: &str) -> PathBuf {
    let path = sidecar_path(session_id, suffix).expect("sidecar path");
    if let Some(dir) = path.parent() {
        mkdir_700(dir).expect("registry dir");
    }
    path
}

/// `<sid>.helper`: the session helper's ack.
pub(crate) fn helper_ack_path(session_id: &str) -> Result<PathBuf> {
    sidecar_path(session_id, "helper")
}

/// `<sid>.helper.lock`: the ack's writer lock. Never renamed; removed only by
/// teardown or GC.
pub(crate) fn helper_lock_path(session_id: &str) -> Result<PathBuf> {
    sidecar_path(session_id, "helper.lock")
}

/// `<sid>.relaunch` (empty suffix) or `<sid>.relaunch.<suffix>`.
pub(crate) fn relaunch_path(session_id: &str, suffix: &str) -> Result<PathBuf> {
    if suffix.is_empty() {
        sidecar_path(session_id, "relaunch")
    } else {
        sidecar_path(session_id, &format!("relaunch.{suffix}"))
    }
}

/// One session's helper ack, lock-free (the ack is replaced by rename).
pub(crate) fn read_helper_ack(session_id: &str) -> Option<crate::hot_swap::HelperAck> {
    crate::hot_swap::read_helper_ack_at(&helper_ack_path(session_id).ok()?)
}

/// Which of a session's sidecars [`remove_sidecars`] leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeepSidecars {
    Nothing,
    /// The relaunch exit path: the new process verifies its nonce against
    /// `.relaunch.taken`.
    RelaunchTaken,
}

/// The sidecar suffixes tollgate writes.
const SIDECAR_SUFFIXES: &[&str] = &[
    "helper",
    "helper.lock",
    "relaunch",
    "relaunch.taken",
    "relaunch.cancel",
    "relaunch.result",
];

/// Whether `session_id` is already taken in the registry: its row or any of
/// its sidecars exists. The sid re-mint loops check this beside the marker,
/// so a sid live under another profile or harness (a `~/.tollgate` shared
/// across pid namespaces) — or a dead namesake's leftovers, whose stale
/// helper ack would poison a new session's view — is never reused.
pub(crate) fn sid_in_use(session_id: &str) -> bool {
    let exists = |p: Result<PathBuf>| p.is_ok_and(|p| std::fs::symlink_metadata(p).is_ok());
    exists(row_path(session_id))
        || SIDECAR_SUFFIXES
            .iter()
            .any(|suffix| exists(sidecar_path(session_id, suffix)))
}

/// Remove a session's sidecars, NotFound ignored.
pub(crate) fn remove_sidecars(session_id: &str, keep: KeepSidecars) {
    for suffix in SIDECAR_SUFFIXES {
        // The relaunch exit path keeps `.relaunch.taken` (the new process
        // verifies its nonce) and `.relaunch.result` (the CLI's answer, which
        // a slow poller has not read yet); GC ages both out.
        if keep == KeepSidecars::RelaunchTaken
            && (*suffix == "relaunch.taken" || *suffix == "relaunch.result")
        {
            continue;
        }
        let Ok(path) = sidecar_path(session_id, suffix) else {
            return;
        };
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            crate::logline::logline!("tollgate: removing {} failed: {e}", path.display());
        }
    }
}

/// One sidecar file found in the registry dir.
pub(crate) struct Sidecar {
    pub(crate) session_id: String,
    pub(crate) suffix: String,
    pub(crate) path: PathBuf,
}

/// Every sidecar in the registry: each `<sid>.<suffix>` whose suffix is one
/// tollgate writes (a helper ack's staging file included).
pub(crate) fn list_sidecars() -> Vec<Sidecar> {
    let Ok(dir) = registry_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let hidden_stage = name.starts_with('.');
            let trimmed = name.trim_start_matches('.');
            let (sid, suffix) = trimmed.split_once('.')?;
            let relaunch_stage = hidden_stage
                && suffix.strip_prefix("relaunch.").is_some_and(|rest| {
                    rest.split_once('.').is_some_and(|(pid, id)| {
                        !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && !id.is_empty()
                    })
                });
            let known = SIDECAR_SUFFIXES.contains(&suffix)
                || suffix.starts_with("helper.tmp.")
                || relaunch_stage;
            (known && is_session_id(sid)).then(|| Sidecar {
                session_id: sid.to_string(),
                suffix: suffix.to_string(),
                path: entry.path(),
            })
        })
        .collect()
}

fn read_row(path: &Path) -> Option<LiveSession> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// One session's own row, or `None` when it is absent or unparseable. Read
/// WITHOUT the state lock, which is sound because [`write_row`] renames over the
/// path: a reader sees the whole old row or the whole new one. A caller that
/// intends to WRITE what it read must go through [`update`] instead — a
/// load-here/store-later pair reverts whatever the other writer landed in
/// between.
pub(crate) fn get(session_id: &str) -> Option<LiveSession> {
    read_row(&row_path(session_id).ok()?)
}

/// The one mutation path: load a FRESH row inside the state lock, edit it
/// through a borrow that cannot escape the hold, store it before releasing. A
/// missing row is an error naming the id, never a silent no-op.
fn update(session_id: &str, edit: impl FnOnce(&mut LiveSession)) -> Result<()> {
    let path = row_path(session_id)?;
    with_state_lock(|_held| {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("no live-session row for {session_id}"))?;
        let mut row: LiveSession = serde_json::from_slice(&bytes)
            .with_context(|| format!("unreadable live-session row for {session_id}"))?;
        edit(&mut row);
        write_row(&row)
    })
}

/// Edit the daemon-owned fields of one row. The session's own fields are carried
/// through untouched by construction: the row is reloaded here, not supplied.
pub(crate) fn update_as_daemon(
    session_id: &str,
    edit: impl FnOnce(&mut DaemonFields<'_>),
) -> Result<()> {
    update(session_id, |row| edit(&mut DaemonFields(row)))
}

/// Edit the session-owned fields of one row. Mirror of [`update_as_daemon`].
pub(crate) fn update_as_session(
    session_id: &str,
    edit: impl FnOnce(&mut SessionFields<'_>),
) -> Result<()> {
    update(session_id, |row| edit(&mut SessionFields(row)))
}

#[cfg(test)]
#[path = "../tests/inline/live_sessions.rs"]
mod tests;
