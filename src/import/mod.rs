//! `tollgate import clauth`: move upstream clauth 0.16.0's accounts into
//! `~/.tollgate` so tollgate becomes the only writer of every refresh chain
//! and guest mode ends (spec `docs/specs/import-clauth.md`, plan §4.0).
//!
//! Part 1 of the spec's §8 lands here: the read-only inventory and its
//! dry-run report, the write-ahead journal, the lock fence, the process and
//! marker scan, the move engine with its crash replay, the live-slot rules,
//! the roster merges, and the reverse replay. The CLI exposes only
//! `import clauth --dry-run` and `import status`; the real run and
//! `import rollback` answer "only --dry-run is available in this build" until
//! part 2 adds the global edits (G1–G4) they must not run without.
//!
//! | module | spec |
//! |--------|------|
//! | [`inventory`] | §3.4 classification, M2 static refusals |
//! | [`procs`] | §4.3 process and marker scan (M1) |
//! | [`slots`] | §4.6 live slots |
//! | [`roster`] | §4.7 names, collisions, merges; F2 |
//! | [`fence`] | §4.2 lock order (M3) |
//! | [`journal`] | §3.2 journal and its durable writes |
//! | [`fsops`] | §4.4 move engine primitives |
//! | [`txn`] | §4.1 the precheck, the plan and M3–M8, resume |
//! | [`rollback`] | §4.10 reverse replay |
//!
//! Nothing here reads a credential value into a report, a journal entry or
//! a backup: tokens are compared in memory and dropped.

// The engine (M3–M8, resume, rollback) is complete in part 1 and driven by
// the hermetic tests, but the part-1 CLI reaches only the dry-run and the
// status read (spec §8), so a non-test build sees parts of it as unused.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "engine reached by tests only until part 2 wires the real run (spec §8)"
    )
)]
pub(crate) mod fence;
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "engine reached by tests only until part 2 wires the real run (spec §8)"
    )
)]
pub(crate) mod fsops;
pub(crate) mod inventory;
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "engine reached by tests only until part 2 wires the real run (spec §8)"
    )
)]
pub(crate) mod journal;
pub(crate) mod procs;
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "engine reached by tests only until part 2 wires the real run (spec §8)"
    )
)]
pub(crate) mod rollback;
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "engine reached by tests only until part 2 wires the real run (spec §8)"
    )
)]
pub(crate) mod roster;
pub(crate) mod slots;
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "engine reached by tests only until part 2 wires the real run (spec §8)"
    )
)]
pub(crate) mod txn;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::identity::{DATA_DIR_NAME, IMPORT_JOURNAL_FILE, UPSTREAM_DATA_DIR_NAME};

/// The directory names the import works between, resolved once from the home.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) home: PathBuf,
    /// `~/.clauth`.
    pub(crate) source: PathBuf,
    /// `~/.tollgate`.
    pub(crate) target: PathBuf,
    /// `~/.claude`.
    pub(crate) claude: PathBuf,
    /// `~/.codex`.
    pub(crate) codex: PathBuf,
}

impl Paths {
    pub(crate) fn resolve() -> Result<Self> {
        Ok(Self::at(&crate::profile::home_dir()?))
    }

    pub(crate) fn at(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            source: home.join(UPSTREAM_DATA_DIR_NAME),
            target: home.join(DATA_DIR_NAME),
            claude: home.join(".claude"),
            codex: home.join(".codex"),
        }
    }

    pub(crate) fn journal(&self) -> PathBuf {
        self.target.join(IMPORT_JOURNAL_FILE)
    }

    pub(crate) fn backup_dir(&self) -> PathBuf {
        self.target.join("import-backup")
    }

    /// `~/.claude/.credentials.json`.
    pub(crate) fn claude_slot(&self) -> PathBuf {
        self.claude.join(".credentials.json")
    }

    /// `~/.codex/auth.json`.
    pub(crate) fn codex_slot(&self) -> PathBuf {
        self.codex.join("auth.json")
    }

    pub(crate) fn upstream_profile(&self, name: &str) -> PathBuf {
        self.source.join("profiles").join(name)
    }

    pub(crate) fn tollgate_profile(&self, name: &str) -> PathBuf {
        self.target.join("profiles").join(name)
    }

    /// The tombstone M8 writes (`~/.clauth/MIGRATED`).
    pub(crate) fn tombstone(&self) -> PathBuf {
        self.source.join("MIGRATED")
    }

    /// `path` spelled for a person: `~/…` under the home, else as given.
    pub(crate) fn tilde(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }

    /// `path` relative to `base` when under it, else absolute (the report's
    /// path rule, spec §2.4).
    pub(crate) fn relative(base: &Path, path: &Path) -> String {
        match path.strip_prefix(base) {
            Ok(rest) => rest.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

/// What the operator asked for on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(crate) struct Options {
    /// `--rename OLD=NEW`: upstream name → tollgate name.
    #[serde(default)]
    pub(crate) renames: BTreeMap<String, String>,
    /// `--adopt-live`: capture a diverged regular-file live slot.
    #[serde(default)]
    pub(crate) adopt_live: bool,
}

impl Options {
    /// The tollgate name an upstream profile lands under (spec §3.3).
    pub(crate) fn dst_name(&self, upstream: &str) -> String {
        self.renames
            .get(upstream)
            .cloned()
            .unwrap_or_else(|| upstream.to_string())
    }

    /// Parse repeated `--rename OLD=NEW` values. A malformed value or an OLD
    /// named twice is a usage error.
    pub(crate) fn parse_renames(values: &[String]) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for value in values {
            let Some((old, new)) = value.split_once('=') else {
                return Err(crate::usage_error(format!(
                    "--rename takes OLD=NEW, got '{value}'"
                )));
            };
            let (old, new) = (old.trim(), new.trim());
            if old.is_empty() || new.is_empty() {
                return Err(crate::usage_error(format!(
                    "--rename takes OLD=NEW, got '{value}'"
                )));
            }
            if out.insert(old.to_string(), new.to_string()).is_some() {
                return Err(crate::usage_error(format!("--rename names '{old}' twice")));
            }
        }
        Ok(out)
    }
}

/// One blocker or warning: a stable `code`, the sentence a person reads, and
/// the pid or path it is about. Never carries a credential value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Finding {
    pub(crate) code: String,
    pub(crate) pid: Option<u32>,
    pub(crate) path: Option<String>,
    pub(crate) message: String,
}

impl Finding {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            pid: None,
            path: None,
            message: message.into(),
        }
    }

    pub(crate) fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }

    pub(crate) fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// A refusal before any store moved (M-1, the post-confirmation recheck, M3,
/// M4): exit 3, nothing changed. `printed` says the command already showed
/// the blockers, so `main::exit_code` adds no second copy.
#[derive(Debug)]
pub(crate) struct ImportBlocked {
    pub(crate) blockers: Vec<Finding>,
    pub(crate) printed: bool,
}

impl std::fmt::Display for ImportBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for b in &self.blockers {
            if !first {
                f.write_str("\n")?;
            }
            first = false;
            write!(f, "  blocked  {}: {}", b.code, b.message)?;
        }
        if first {
            f.write_str("tollgate import clauth: blocked")?;
        }
        Ok(())
    }
}

impl std::error::Error for ImportBlocked {}

/// The journal needs a person: an interrupted import, a journal that
/// disagrees with disk, or a reversal that stopped part-way. Exit 4.
#[derive(Debug)]
pub(crate) struct ImportNeedsAttention {
    pub(crate) state: String,
    pub(crate) step: Option<u64>,
    pub(crate) reason: String,
}

impl std::fmt::Display for ImportNeedsAttention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.step {
            Some(step) => write!(
                f,
                "tollgate import: {} (journal state {}, step {step})",
                self.reason, self.state
            ),
            None => write!(
                f,
                "tollgate import: {} (journal state {})",
                self.reason, self.state
            ),
        }
    }
}

impl std::error::Error for ImportNeedsAttention {}

/// The one sentence the part-1 CLI answers a real run or rollback with.
pub(crate) const DRY_RUN_ONLY: &str =
    "tollgate import clauth: only --dry-run is available in this build";

/// The seams the hermetic tests drive the engine through. Outside `cfg(test)`
/// every function is the real thing; under it the defaults are the SAFE
/// ones — no `PATH` directory at all (so no test can ever find, let alone
/// retire, the operator's real `clauth`), an empty process table (so the
/// operator's live Claude Code sessions never enter a unit test), and Linux.
pub(crate) mod seams {
    use std::path::{Path, PathBuf};

    use super::procs::FakeProc;

    /// Where [`crash`] fires relative to an op.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) enum CrashPoint {
        BeforeOp,
        AfterOp,
    }

    #[cfg(test)]
    pub(crate) use test_state::*;

    /// The `PATH` directories, in order.
    pub(crate) fn path_dirs() -> Vec<PathBuf> {
        #[cfg(test)]
        {
            with(|s| s.path_dirs.clone())
        }
        #[cfg(not(test))]
        {
            std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default()
        }
    }

    /// This process's executable, as installed. `None` when unknown (and, in
    /// tests, when no test pinned one: the check it feeds is then skipped).
    pub(crate) fn current_exe() -> Option<PathBuf> {
        #[cfg(test)]
        {
            with(|s| s.current_exe.clone())
        }
        #[cfg(not(test))]
        {
            std::env::current_exe()
                .ok()
                .map(|p| crate::platform::installed_exe_path(&p))
        }
    }

    pub(crate) fn is_linux() -> bool {
        #[cfg(test)]
        if with(|s| s.not_linux) {
            return false;
        }
        cfg!(target_os = "linux")
    }

    /// The process table a test posed, or `None` to scan `/proc`.
    pub(crate) fn fake_procs() -> Option<Vec<FakeProc>> {
        #[cfg(test)]
        {
            Some(with(|s| s.procs.clone()))
        }
        #[cfg(not(test))]
        {
            None
        }
    }

    /// Runs a test's hook before step `seq` (pausing, flipping the process
    /// table). The hook is taken out while it runs, so it may edit the state.
    pub(crate) fn before_step(seq: u64) {
        #[cfg(test)]
        {
            let hook = with(|s| s.before_step.take());
            if let Some(mut hook) = hook {
                hook(seq);
                with(|s| {
                    if s.before_step.is_none() {
                        s.before_step = Some(hook);
                    }
                });
            }
        }
        #[cfg(not(test))]
        let _ = seq;
    }

    /// Runs a test's hook between the confirmation and its recheck.
    pub(crate) fn after_confirm() {
        #[cfg(test)]
        {
            let hook = with(|s| s.after_confirm.take());
            if let Some(mut hook) = hook {
                hook();
            }
        }
    }

    /// Whether a simulated crash fires at `seq`/`point`.
    pub(crate) fn crash(seq: u64, point: CrashPoint) -> bool {
        #[cfg(test)]
        {
            with(|s| s.crash == Some((seq, point)))
        }
        #[cfg(not(test))]
        {
            let _ = (seq, point);
            false
        }
    }

    /// Whether a rename of `src` must fail with `EXDEV`.
    pub(crate) fn exdev(src: &Path) -> bool {
        #[cfg(test)]
        {
            with(|s| s.exdev.iter().any(|p| p == src))
        }
        #[cfg(not(test))]
        {
            let _ = src;
            false
        }
    }

    /// Whether `path` must read as living on another filesystem.
    pub(crate) fn foreign_dev(path: &Path) -> bool {
        #[cfg(test)]
        {
            with(|s| s.foreign_dev.iter().any(|p| p == path))
        }
        #[cfg(not(test))]
        {
            let _ = path;
            false
        }
    }

    /// Record one engine event when a test armed the op log.
    pub(crate) fn log(event: impl FnOnce() -> String) {
        #[cfg(test)]
        with(|s| {
            if let Some(log) = s.op_log.as_mut() {
                log.push(event());
            }
        });
        #[cfg(not(test))]
        let _ = event;
    }

    /// The pause between rescans while only exempt read-only tollgate runs
    /// are alive (spec §4.3: up to 3 × 200 ms).
    pub(crate) fn exempt_retry_delay() -> std::time::Duration {
        if cfg!(test) {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_millis(200)
        }
    }

    #[cfg(test)]
    mod test_state {
        use super::*;
        use std::sync::Mutex;

        type Hook = Box<dyn FnMut(u64) + Send>;
        type Plain = Box<dyn FnMut() + Send>;

        /// Everything a test may pin. `Default` is the safe baseline.
        #[derive(Default)]
        pub(crate) struct State {
            pub(crate) path_dirs: Vec<PathBuf>,
            pub(crate) current_exe: Option<PathBuf>,
            pub(crate) not_linux: bool,
            pub(crate) procs: Vec<FakeProc>,
            pub(crate) before_step: Option<Hook>,
            pub(crate) after_confirm: Option<Plain>,
            pub(crate) crash: Option<(u64, CrashPoint)>,
            pub(crate) exdev: Vec<PathBuf>,
            pub(crate) foreign_dev: Vec<PathBuf>,
            pub(crate) op_log: Option<Vec<String>>,
        }

        pub(crate) static STATE: Mutex<Option<State>> = Mutex::new(None);

        /// Installs a fresh [`State`] and clears it on drop.
        #[must_use]
        pub(crate) struct Guard(());

        impl Guard {
            /// Edit the pinned state.
            pub(crate) fn set(&self, f: impl FnOnce(&mut State)) {
                with(|s| f(s));
            }

            /// The op log recorded since [`State::op_log`] was armed.
            pub(crate) fn op_log(&self) -> Vec<String> {
                with(|s| s.op_log.clone().unwrap_or_default())
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                *STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }

        pub(crate) fn install() -> Guard {
            *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(State::default());
            Guard(())
        }

        pub(crate) fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
            let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
            let state = guard.get_or_insert_with(State::default);
            f(state)
        }
    }
}

/// Seconds since the epoch as RFC 3339 UTC.
pub(crate) fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    crate::usage::epoch_secs_to_iso(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// `tollgate import clauth --dry-run`: print the report, change nothing.
pub(crate) fn cmd_dry_run(opts: &Options, json: bool) -> Result<()> {
    let survey = txn::survey(opts, txn::Mode::DryRun)?;
    let report = txn::report(&survey, "dry_run");
    print_report(&report, json, true)?;
    txn::blocked_outcome(&survey, true)
}

/// `tollgate import status`: the journal's state, never a lock.
pub(crate) fn cmd_status(json: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    let status = journal::status(&paths);
    if json {
        crate::out::outln!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        crate::out::outln!("{}", journal::render_status(&status));
    }
    Ok(())
}

/// Print a report as text or as one JSON document on stdout.
pub(crate) fn print_report(report: &txn::Report, json: bool, dry_run: bool) -> Result<()> {
    if json {
        crate::out::outln!("{}", serde_json::to_string_pretty(report)?);
    } else {
        crate::out::outln!("{}", txn::render_text(report, dry_run));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/import_common.rs"]
mod test_common;

#[cfg(test)]
#[path = "../../tests/inline/import_inventory.rs"]
mod inventory_tests;

#[cfg(test)]
#[path = "../../tests/inline/import_fence.rs"]
mod fence_tests;

#[cfg(test)]
#[path = "../../tests/inline/import_txn.rs"]
mod txn_tests;

#[cfg(test)]
#[path = "../../tests/inline/import_slots.rs"]
mod slots_tests;

#[cfg(test)]
#[path = "../../tests/inline/import_rollback.rs"]
mod rollback_tests;
