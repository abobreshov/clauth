//! Relaunch in place (P6b): `tollgate switch <sid> <p> --relaunch` stops a live
//! `tollgate start` session gracefully and resumes the same conversation under
//! `<p>`, in the same terminal.
//!
//! Two processes cooperate through three sidecars of the session's registry
//! row (`~/.tollgate/live_sessions/`):
//!
//! - the CLI writes `<sid>.relaunch` (a request: target, conversation, cwd; no
//!   argv, so a request file can never inject arguments), then waits for
//!   `<sid>.relaunch.result`;
//! - the session's own supervisor (`start::run`) claims the request by renaming
//!   it to `<sid>.relaunch.taken`, re-validates it, stamps a fresh nonce into
//!   it, answers `relaunching`, stops Claude Code (SIGTERM, then SIGKILL after
//!   20 s), tears the session down, restores the terminal, and `exec`s
//!   `tollgate start <p> -- <its own claude args> --resume <conv>` with the
//!   nonce in its env. The new process honours the hand-off only when the nonce
//!   matches `.relaunch.taken`, so a forged env does nothing.
//!
//! A request nobody claims within 30 s is cancelled by renaming it to
//! `<sid>.relaunch.cancel`; the rename is the arbiter, so a supervisor and the
//! CLI can never both act on one request.
//!
//! The supervisor half runs on unix only (it `exec`s and restores termios);
//! elsewhere a row is never `relaunch_capable`, so the CLI refuses with the
//! manual resume line.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        reason = "the supervisor half polls for relaunch requests on unix only"
    )
)]

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::live_sessions::LiveSession;
use crate::logline::logline;
use crate::out::{errln, outln};
use crate::profile::ProfileName;

/// The sid the new process was relaunched from.
pub(crate) const RELAUNCHED_FROM_ENV: &str = "TOLLGATE_RELAUNCHED_FROM";
/// The profile to restart on if the new start fails before its child spawns.
pub(crate) const FALLBACK_ENV: &str = "TOLLGATE_RELAUNCH_FALLBACK";
/// The nonce the new process verifies against `<sid>.relaunch.taken`.
pub(crate) const NONCE_ENV: &str = "TOLLGATE_RELAUNCH_NONCE";

/// Every relaunch hand-off variable: read once at the top of `start::run` and
/// scrubbed from every child command.
pub(crate) const RELAUNCH_ENV_KEYS: &[&str] = &[RELAUNCHED_FROM_ENV, FALLBACK_ENV, NONCE_ENV];

/// SIGTERM grace before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(20);
/// The transcript is flushed once two reads this far apart agree.
const FLUSH_POLL: Duration = Duration::from_millis(250);
const FLUSH_MAX: Duration = Duration::from_secs(2);
/// Every Nth 50 ms wait iteration the supervisor tries a claim.
pub(crate) const CLAIM_EVERY: u32 = 10;

/// `<sid>.relaunch` / `<sid>.relaunch.taken`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RelaunchRequest {
    pub(crate) version: u32,
    pub(crate) target: String,
    pub(crate) conversation: String,
    pub(crate) cwd: PathBuf,
    #[serde(default)]
    pub(crate) follows_chain: bool,
    #[serde(default)]
    pub(crate) requested_at_ms: u64,
    #[serde(default)]
    pub(crate) requester_pid: u32,
    /// Added by the supervisor when it rewrites `.relaunch.taken`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) nonce: Option<String>,
    /// The requester's random id, echoed into the result so a CLI accepts
    /// only its own answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request_id: Option<String>,
}

/// `<sid>.relaunch.result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RelaunchResult {
    pub(crate) version: u32,
    pub(crate) outcome: String,
    pub(crate) reason: Option<String>,
    /// The answered request's `request_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request_id: Option<String>,
}

/// How long the CLI waits for each step.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadlines {
    pub(crate) claim: Duration,
    pub(crate) register: Duration,
    pub(crate) poll: Duration,
}

impl Deadlines {
    const PRODUCTION: Self = Self {
        claim: Duration::from_secs(30),
        register: Duration::from_secs(60),
        poll: Duration::from_millis(100),
    };
}

#[cfg(test)]
thread_local! {
    static DEADLINES: std::cell::Cell<Option<Deadlines>> = const { std::cell::Cell::new(None) };
}

/// Test clock seam: shrink the CLI's waits.
#[cfg(test)]
pub(crate) fn set_deadlines(deadlines: Option<Deadlines>) {
    DEADLINES.with(|d| d.set(deadlines));
}

fn deadlines() -> Deadlines {
    #[cfg(test)]
    if let Some(d) = DEADLINES.with(std::cell::Cell::get) {
        return d;
    }
    Deadlines::PRODUCTION
}

/// A refusal already printed, carrying its exit code.
#[derive(Debug)]
pub(crate) struct Reported(pub(crate) i32);

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}", self.0)
    }
}

impl std::error::Error for Reported {}

/// A relaunch the CLI refuses; [`run_cli`] prints it and exits 1.
#[derive(Debug)]
pub(crate) struct RelaunchRefused {
    pub(crate) sid: String,
    pub(crate) reason: String,
}

impl std::fmt::Display for RelaunchRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tollgate: cannot relaunch session '{}': {}",
            self.sid, self.reason
        )
    }
}

impl std::error::Error for RelaunchRefused {}

fn refuse(sid: &str, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::Error::new(RelaunchRefused {
        sid: sid.to_string(),
        reason: reason.to_string(),
    })
}

// ── the transcript store ─────────────────────────────────────────────────────

/// The `projects/` store a shared session's transcripts land in: the guest
/// store in guest mode, else `~/.claude/projects`.
pub(crate) fn projects_store() -> Result<PathBuf> {
    if crate::identity::upstream_active() {
        crate::runtime::guest_projects_store()
    } else {
        Ok(crate::profile::claude_dir()?.join("projects"))
    }
}

/// Claude Code's project dir name for a cwd: every non-alphanumeric byte
/// becomes `-`.
pub(crate) fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// A conversation's top-level transcript in `store`, any project dir.
pub(crate) fn transcript_in(store: &Path, conversation: &str) -> Option<PathBuf> {
    let file = format!("{conversation}.jsonl");
    std::fs::read_dir(store)
        .ok()?
        .flatten()
        .map(|dir| dir.path().join(&file))
        .find(|p| p.is_file())
}

/// Which conversation a relaunch resumes: the `--conversation` flag (it must
/// exist in the store), else the hook records naming this runtime, else the
/// transcripts under the session's cwd written since it started. Exactly one
/// or a refusal.
pub(crate) fn resolve_conversation(
    row: &LiveSession,
    flag: Option<&str>,
    store: &Path,
) -> Result<String, String> {
    if let Some(conv) = flag {
        return if transcript_in(store, conv).is_some() {
            Ok(conv.to_string())
        } else {
            Err(format!(
                "conversation '{conv}' is not in the session's store"
            ))
        };
    }
    let mut found = crate::hook_note::conversations_for_runtime(&row.session_id);
    if found.is_empty()
        && let Some(cwd) = &row.cwd
    {
        // The scan cannot tell this session's transcript from a sibling's in
        // the same project dir: resuming a sibling's conversation would put
        // two Claude Code processes on one transcript.
        let sibling = crate::live_sessions::list().into_iter().any(|other| {
            other.session_id != row.session_id
                && other.cwd.as_deref() == Some(cwd.as_path())
                && crate::runtime::namespaced_keychain_ledger::pid_alive(other.pid)
        });
        if sibling {
            return Err(
                "another live session shares its working directory; pass --conversation <id>"
                    .to_string(),
            );
        }
        let dir = store.join(encode_cwd(cwd));
        let since = std::time::UNIX_EPOCH + Duration::from_millis(row.started_at);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let fresh = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m >= since);
                if fresh
                    && path.extension().is_some_and(|e| e == "jsonl")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && !ran_elsewhere(stem, &row.session_id)
                {
                    found.push(stem.to_string());
                }
            }
        }
    }
    found.sort();
    found.dedup();
    match found.len() {
        0 => Err("no conversation found for the session".to_string()),
        1 => Ok(found.remove(0)),
        n => Err(format!("{n} conversations match; pass --conversation <id>")),
    }
}

/// Whether `conversation`'s hook record names another tollgate session.
fn ran_elsewhere(conversation: &str, sid: &str) -> bool {
    crate::hook_note::record_path(conversation, None)
        .ok()
        .and_then(|p| crate::hook_note::load_record(&p))
        .and_then(|r| r.runtime_sid)
        .is_some_and(|other| other != sid)
}

// ── CLI side ─────────────────────────────────────────────────────────────────

/// Everything the CLI resolved before touching the session.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub(crate) sid: String,
    pub(crate) target: ProfileName,
    pub(crate) conversation: String,
    pub(crate) request: RelaunchRequest,
}

/// Steps 1-4: row, admission, conversation, cwd. Writes nothing and signals
/// nothing.
pub(crate) fn prepare(sid: &str, profile: &str, conversation: Option<&str>) -> Result<Prepared> {
    let row = crate::sessions_cli::live_claude_row(sid)?;
    if row.isolated {
        return Err(refuse(
            sid,
            "isolated sessions relaunch empty; resume it with 'tollgate resume'",
        ));
    }
    let config = crate::profile::load_config()?;
    let target = crate::sessions_cli::resolve_profile_name(&config, profile)?;
    if !row.relaunch_capable {
        let conv = conversation.unwrap_or("<conv>");
        return Err(refuse(
            sid,
            format!(
                "the session predates relaunch; exit and 'tollgate start {target} -- --resume {conv}'"
            ),
        ));
    }
    if let Err(e) = crate::start::admit(
        &config,
        &target,
        crate::runtime::Isolation::Shared,
        row.follows_chain,
    ) {
        return Err(refuse(sid, format!("{e:#}")));
    }
    let store = projects_store()?;
    let conversation =
        resolve_conversation(&row, conversation, &store).map_err(|e| refuse(sid, e))?;
    let Some(cwd) = row.cwd.clone().filter(|c| c.is_dir()) else {
        return Err(refuse(sid, "its working directory no longer exists"));
    };
    Ok(Prepared {
        sid: sid.to_string(),
        request: RelaunchRequest {
            version: 1,
            target: target.as_str().to_string(),
            conversation: conversation.clone(),
            cwd,
            follows_chain: row.follows_chain,
            requested_at_ms: crate::usage::now_ms(),
            requester_pid: std::process::id(),
            nonce: None,
            request_id: Some(nonce()?),
        },
        target,
        conversation,
    })
}

/// `tollgate switch <sid> <p> --relaunch [--yes] [--conversation <id>]`.
pub(crate) fn run_cli(
    sid: &str,
    profile: &str,
    yes: bool,
    conversation: Option<&str>,
) -> Result<()> {
    let tty = std::io::stdin().is_terminal();
    if !yes && !tty {
        errln!("tollgate: --relaunch stops a live session; confirm on a terminal or pass --yes");
        return Err(anyhow::Error::new(Reported(2)));
    }
    let run = || {
        let prepared = prepare(sid, profile, conversation)?;
        if !yes && !confirm(&prepared)? {
            outln!("tollgate: relaunch cancelled; session '{sid}' is unchanged");
            return Err(anyhow::Error::new(Reported(1)));
        }
        submit(&prepared)
    };
    match run() {
        Err(e) if e.downcast_ref::<RelaunchRefused>().is_some() => {
            errln!("{e}");
            Err(anyhow::Error::new(Reported(1)))
        }
        other => other,
    }
}

fn confirm(prepared: &Prepared) -> Result<bool> {
    let _ = crate::out::write_chunk(
        &mut std::io::stderr().lock(),
        format_args!(
            "Relaunch session '{}' (conversation {}) as '{}'? Claude Code exits and resumes \
             the conversation. [y/N] ",
            prepared.sid, prepared.conversation, prepared.target
        ),
        false,
        "stderr",
    );
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

/// Steps 5-7: write the request, wait for the answer, wait for the new row.
///
/// Two concurrent requests for one session are arbitrated: the request is
/// published exclusively, refused while
/// another is pending or claimed, and each CLI accepts — and removes — only
/// the result carrying its own `request_id`.
pub(crate) fn submit(prepared: &Prepared) -> Result<()> {
    submit_with_link(prepared, &mut |staging, request| {
        std::fs::hard_link(staging, request)
    })
}

fn submit_with_link(
    prepared: &Prepared,
    link: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let sid = prepared.sid.as_str();
    let request = crate::live_sessions::relaunch_path(sid, "")?;
    let result = crate::live_sessions::relaunch_path(sid, "result")?;
    let taken = crate::live_sessions::relaunch_path(sid, "taken")?;
    let busy = "another relaunch of this session is already in progress; it is unchanged";
    if taken.exists() {
        return Err(refuse(sid, busy));
    }
    let id = prepared.request.request_id.clone();
    let bytes = serde_json::to_vec(&prepared.request)?;
    let staging = request.with_file_name(format!(
        ".{sid}.relaunch.{}.{}",
        std::process::id(),
        id.as_deref().unwrap_or("0")
    ));
    crate::profile::write_durable_600(&staging, bytes)
        .with_context(|| format!("failed to write {}", staging.display()))?;
    let published = match link(&staging, &request) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            publish_no_replace(&staging, &request)
        }
        other => other,
    };
    let _ = std::fs::remove_file(&staging);
    match published {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(refuse(sid, busy));
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to write {}", request.display()));
        }
    }
    // A supervisor may have claimed an earlier request after our first
    // `.taken` check. Withdraw only our own newly published request.
    if taken.exists() && request_id_of(&taken) != id {
        if request_id_of(&request) == id {
            let _ = std::fs::remove_file(&request);
        }
        return Err(refuse(sid, busy));
    }
    let ours = |path: &Path| read_result(path).filter(|r| r.request_id == id);
    let limits = deadlines();
    let answer = match wait_for(limits.claim, limits.poll, || ours(&result)) {
        Some(answer) => answer,
        None => {
            let cancel = crate::live_sessions::relaunch_path(sid, "cancel")?;
            match std::fs::rename(&request, &cancel) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&cancel);
                    return Err(refuse(sid, "the session did not answer; it is unchanged"));
                }
                // Claimed while we gave up: wait once more for its answer.
                Err(_) => match wait_for(limits.claim, limits.poll, || ours(&result)) {
                    Some(answer) => answer,
                    None => {
                        return Err(refuse(
                            sid,
                            "the session claimed the request but never answered; check 'tollgate sessions'",
                        ));
                    }
                },
            }
        }
    };
    let _ = std::fs::remove_file(&result);
    if answer.outcome != "relaunching" {
        return Err(refuse(
            sid,
            answer.reason.as_deref().unwrap_or("the session refused"),
        ));
    }
    let new_row = wait_for(limits.register, limits.poll, || {
        crate::live_sessions::list()
            .into_iter()
            .find(|row| row.relaunched_from.as_deref() == Some(sid))
    });
    match new_row {
        // What registered is what is printed: a start that fell back to the
        // original profile is not reported as the target.
        Some(row) if row.start_profile != prepared.target.as_str() => Err(refuse(
            sid,
            format!(
                "the session relaunched as '{}' on '{}', not '{}'; check 'tollgate sessions'",
                row.session_id, row.start_profile, prepared.target
            ),
        )),
        Some(row) => {
            outln!(
                "tollgate: relaunched session '{sid}' as '{}' on '{}' (conversation {})",
                row.session_id,
                row.start_profile,
                prepared.conversation
            );
            Ok(())
        }
        None => Err(refuse(
            sid,
            "the session stopped but its relaunch has not registered yet; check 'tollgate sessions'",
        )),
    }
}

/// Atomic no-replace rename where Linux supports it. The exclusive-create
/// fallback covers filesystems that reject renameat2 as well as hard links.
fn publish_no_replace(staging: &Path, request: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let from = std::ffi::CString::new(staging.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let to = std::ffi::CString::new(request.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        #[allow(unsafe_code)]
        // SAFETY: both C strings are live for the duration of this syscall.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut final_file = opts.open(request)?;
    let copied = (|| {
        let mut source = std::fs::File::open(staging)?;
        std::io::copy(&mut source, &mut final_file)?;
        final_file.flush()?;
        final_file.sync_all()
    })();
    if copied.is_err() {
        let _ = std::fs::remove_file(request);
    }
    copied
}

fn read_result(path: &Path) -> Option<RelaunchResult> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn wait_for<T>(limit: Duration, poll: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(v) = probe() {
            return Some(v);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(poll);
    }
}

// ── supervisor side ──────────────────────────────────────────────────────────

/// The relaunch hand-off a starting process read from its env, honoured only
/// once its nonce matched `<from>.relaunch.taken`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Handoff {
    /// The sid this process replaces, when verified.
    pub(crate) from: Option<String>,
    /// The profile to restart on if this start fails before its child
    /// spawns, when verified.
    pub(crate) fallback: Option<String>,
}

impl Handoff {
    /// Read the three variables and verify them. A missing or mismatched
    /// `.relaunch.taken` ignores all three (logged). The accepted hand-off is
    /// consumed after a child spawns, leaving it available for one fallback.
    pub(crate) fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self::verify(get(RELAUNCHED_FROM_ENV), get(FALLBACK_ENV), get(NONCE_ENV))
    }

    pub(crate) fn verify(
        from: Option<String>,
        fallback: Option<String>,
        nonce: Option<String>,
    ) -> Self {
        let Some(from) = from else {
            return Self::default();
        };
        let verified = crate::runtime::is_session_id(&from)
            && nonce.as_deref().is_some_and(|nonce| {
                crate::live_sessions::relaunch_path(&from, "taken")
                    .ok()
                    .and_then(|p| std::fs::read(p).ok())
                    .and_then(|b| serde_json::from_slice::<RelaunchRequest>(&b).ok())
                    .and_then(|r| r.nonce)
                    .is_some_and(|stored| stored == nonce)
            });
        if !verified {
            logline!("tollgate: ignoring a relaunch hand-off whose nonce does not verify");
            return Self::default();
        }
        Self {
            from: Some(from),
            fallback: fallback.filter(|f| crate::claude::is_profile_name_token(f)),
        }
    }

    pub(crate) fn consume(&self) {
        if let Some(from) = &self.from
            && let Ok(taken) = crate::live_sessions::relaunch_path(from, "taken")
        {
            let _ = std::fs::remove_file(taken);
        }
    }
}

/// A claimed, validated request, ready to act on.
#[derive(Debug, Clone)]
pub(crate) struct Accepted {
    pub(crate) request: RelaunchRequest,
    pub(crate) nonce: String,
    pub(crate) transcript: PathBuf,
}

fn nonce() -> Result<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn write_result(
    sid: &str,
    request_id: Option<String>,
    outcome: &str,
    reason: Option<String>,
) {
    let Ok(path) = crate::live_sessions::relaunch_path(sid, "result") else {
        return;
    };
    let result = RelaunchResult {
        version: 1,
        outcome: outcome.to_string(),
        reason,
        request_id,
    };
    if let Ok(bytes) = serde_json::to_vec(&result)
        && let Err(e) = crate::profile::atomic_write_600(&path, bytes)
    {
        logline!("tollgate: writing {} failed: {e}", path.display());
    }
}

/// Try to claim `<sid>.relaunch` by rename. A claimed request is re-validated;
/// a refusal is answered and the session keeps running (`None`). An accepted
/// one gets its nonce and a `relaunching` answer.
pub(crate) fn poll_claim(sid: &str) -> Option<Accepted> {
    poll_claim_with(sid, &mut nonce, &mut |path, bytes| {
        crate::profile::atomic_write_600(path, bytes)
    })
}

fn poll_claim_with(
    sid: &str,
    make_nonce: &mut dyn FnMut() -> Result<String>,
    stamp: &mut dyn FnMut(&Path, Vec<u8>) -> std::io::Result<()>,
) -> Option<Accepted> {
    let request = crate::live_sessions::relaunch_path(sid, "").ok()?;
    let taken = crate::live_sessions::relaunch_path(sid, "taken").ok()?;
    std::fs::rename(&request, &taken).ok()?;
    let id = request_id_of(&taken);
    match validate(&taken) {
        Err(reason) => {
            let _ = std::fs::remove_file(&taken);
            write_result(sid, id, "refused", Some(reason));
            None
        }
        Ok((mut req, transcript)) => {
            let nonce = match make_nonce() {
                Ok(n) => n,
                Err(e) => {
                    let _ = std::fs::remove_file(&taken);
                    write_result(sid, id, "refused", Some(format!("{e:#}")));
                    return None;
                }
            };
            req.nonce = Some(nonce.clone());
            let stamped = serde_json::to_vec(&req)
                .map_err(anyhow::Error::from)
                .and_then(|b| stamp(&taken, b).map_err(Into::into));
            if let Err(e) = stamped {
                let _ = std::fs::remove_file(&taken);
                write_result(sid, id, "refused", Some(format!("{e:#}")));
                return None;
            }
            write_result(sid, id, "relaunching", None);
            Some(Accepted {
                request: req,
                nonce,
                transcript,
            })
        }
    }
}

/// The `request_id` of a request file, best-effort (even one that fails
/// validation is answered under its own id).
pub(crate) fn request_id_of(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct IdOnly {
        #[serde(default)]
        request_id: Option<String>,
    }
    serde_json::from_slice::<IdOnly>(&std::fs::read(path).ok()?)
        .ok()?
        .request_id
}

fn validate(taken: &Path) -> Result<(RelaunchRequest, PathBuf), String> {
    let bytes = std::fs::read(taken).map_err(|e| format!("the request could not be read: {e}"))?;
    let req: RelaunchRequest =
        serde_json::from_slice(&bytes).map_err(|_| "the request does not parse".to_string())?;
    if req.version != 1 {
        return Err(format!("request version {} is not supported", req.version));
    }
    let config = crate::profile::load_config().map_err(|e| format!("{e:#}"))?;
    let target = ProfileName::from(req.target.as_str());
    if config.find(&target).is_none() {
        return Err(format!("'{}' is not configured", req.target));
    }
    crate::start::admit(
        &config,
        &target,
        crate::runtime::Isolation::Shared,
        req.follows_chain,
    )
    .map_err(|e| format!("{e:#}"))?;
    if !req.cwd.is_dir() {
        return Err("its working directory no longer exists".to_string());
    }
    let store = projects_store().map_err(|e| format!("{e:#}"))?;
    let transcript = transcript_in(&store, &req.conversation)
        .ok_or_else(|| format!("conversation '{}' has no transcript", req.conversation))?;
    Ok((req, transcript))
}

/// Stop the child: SIGTERM, wait up to `grace`, then SIGKILL.
#[cfg(unix)]
pub(crate) fn stop_child(
    child: &mut std::process::Child,
    grace: Option<Duration>,
) -> Result<std::process::ExitStatus> {
    #[allow(unsafe_code)]
    // SAFETY: `child.id()` is the live child's pid.
    let _ = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = std::time::Instant::now() + grace.unwrap_or(STOP_GRACE);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Ok(child.wait()?);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until two reads of the transcript's `(mtime, len)` agree, at most 2 s.
pub(crate) fn wait_transcript_flush(path: &Path) {
    let read = || {
        std::fs::metadata(path)
            .ok()
            .map(|m| (m.modified().ok(), m.len()))
    };
    let deadline = std::time::Instant::now() + FLUSH_MAX;
    let mut last = read();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(FLUSH_POLL);
        let now = read();
        if now == last {
            return;
        }
        last = now;
    }
}

/// The claude args minus every `--resume`/`-r` (and its value) and
/// `--continue`/`-c`: the relaunch appends its own `--resume <conv>`.
pub(crate) fn strip_resume_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        if a == "--resume" || a == "-r" {
            if iter.peek().is_some_and(|v| !v.starts_with('-')) {
                iter.next();
            }
            continue;
        }
        if a.starts_with("--resume=") || a.starts_with("-r=") || a == "--continue" || a == "-c" {
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// The command the supervisor `exec`s: `<exe> start [--with-fallback]
/// <target> -- <claude args minus resume> --resume <conv>`, in the request's
/// cwd, with the hand-off variables set (and `fallback` only when given).
pub(crate) fn resume_command(
    exe: &Path,
    target: &str,
    accepted: &Accepted,
    claude_args: &[String],
    from_sid: &str,
    fallback: Option<&str>,
) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command.arg("start");
    if accepted.request.follows_chain {
        command.arg("--with-fallback");
    }
    command.arg(target).arg("--");
    command.args(strip_resume_args(claude_args));
    command.arg("--resume").arg(&accepted.request.conversation);
    command.current_dir(&accepted.request.cwd);
    for key in RELAUNCH_ENV_KEYS {
        command.env_remove(key);
    }
    command.env(RELAUNCHED_FROM_ENV, from_sid);
    command.env(NONCE_ENV, &accepted.nonce);
    if let Some(orig) = fallback {
        command.env(FALLBACK_ENV, orig);
    }
    command
}

/// Replace this process with `command` (unix `exec`); on Windows spawn it and
/// exit with its code. Returns only on failure.
pub(crate) fn exec(command: &mut std::process::Command) -> std::io::Error {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.exec()
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) => e,
        }
    }
}

/// The supervisor's relaunch after its child stopped and the session was torn
/// down: exec the target, else the original profile, else print how to resume
/// by hand. Never returns.
pub(crate) fn exec_relaunch(
    accepted: &Accepted,
    orig: &str,
    claude_args: &[String],
    from_sid: &str,
) -> ! {
    let last = exec_relaunch_with(accepted, orig, claude_args, from_sid, &mut exec);
    errln!("{last}");
    std::process::exit(1)
}

/// [`exec_relaunch`]'s attempts through `exec`: the target, then the
/// original profile (without a further fallback). Returns the final line when
/// both fail.
pub(crate) fn exec_relaunch_with(
    accepted: &Accepted,
    orig: &str,
    claude_args: &[String],
    from_sid: &str,
    exec: &mut dyn FnMut(&mut std::process::Command) -> std::io::Error,
) -> String {
    let conv = &accepted.request.conversation;
    if let Ok(exe) = std::env::current_exe() {
        let mut target = resume_command(
            &exe,
            &accepted.request.target,
            accepted,
            claude_args,
            from_sid,
            Some(orig),
        );
        let e = exec(&mut target);
        errln!(
            "tollgate: relaunching onto '{}' failed: {e}",
            accepted.request.target
        );
        let mut original = resume_command(&exe, orig, accepted, claude_args, from_sid, None);
        let e = exec(&mut original);
        errln!("tollgate: restarting on '{orig}' failed: {e}");
    }
    format!("tollgate: relaunch failed; resume with: tollgate start {orig} -- --resume {conv}")
}

/// The new process's fallback: a start that failed before its child spawned
/// restarts once on the original profile with its verified lineage and nonce.
pub(crate) fn exec_fallback(orig: &str, claude_args: &[String], error: &anyhow::Error) -> ! {
    errln!("tollgate: {error:#}");
    let last = exec_fallback_with(orig, claude_args, &mut exec);
    errln!("{last}");
    std::process::exit(1)
}

/// [`exec_fallback`]'s one attempt through `exec`; the final line when it
/// fails.
pub(crate) fn exec_fallback_with(
    orig: &str,
    claude_args: &[String],
    exec: &mut dyn FnMut(&mut std::process::Command) -> std::io::Error,
) -> String {
    let conv = crate::start::resume_id_from_args(claude_args)
        .unwrap_or("<conv>")
        .to_string();
    if let Ok(exe) = std::env::current_exe() {
        let mut command = std::process::Command::new(exe);
        command.arg("start").arg(orig).arg("--").args(claude_args);
        for key in [RELAUNCHED_FROM_ENV, NONCE_ENV] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.env_remove(FALLBACK_ENV);
        let e = exec(&mut command);
        errln!("tollgate: restarting on '{orig}' failed: {e}");
    }
    format!("tollgate: relaunch failed; resume with: tollgate start {orig} -- --resume {conv}")
}

// ── terminal state ───────────────────────────────────────────────────────────

/// The controlling terminal's attributes, saved before the child spawns so a
/// SIGKILLed Claude Code cannot leave it raw.
#[cfg(unix)]
pub(crate) struct TtyState {
    fd: libc::c_int,
    termios: libc::termios,
}

#[cfg(unix)]
impl TtyState {
    /// Snapshot `fd`'s termios, `None` when it is not a terminal.
    #[allow(unsafe_code)]
    pub(crate) fn save(fd: libc::c_int) -> Option<Self> {
        // SAFETY: `isatty`/`tcgetattr` read the fd's state into a zeroed
        // struct they fully initialise on success.
        unsafe {
            if libc::isatty(fd) != 1 {
                return None;
            }
            let mut termios: libc::termios = std::mem::zeroed();
            (libc::tcgetattr(fd, &mut termios) == 0).then_some(Self { fd, termios })
        }
    }

    /// Put the snapshot back and leave the alt screen with the cursor shown.
    #[allow(unsafe_code)]
    pub(crate) fn restore(&self) {
        // SAFETY: `tcsetattr` reads the snapshot taken from this same fd.
        unsafe {
            let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &self.termios);
        }
        let seq = b"\x1b[?1049l\x1b[?25h";
        // SAFETY: writes a static buffer of its own length to the fd.
        unsafe {
            let _ = libc::write(self.fd, seq.as_ptr().cast(), seq.len());
        }
    }

    #[cfg(test)]
    pub(crate) fn termios(&self) -> &libc::termios {
        &self.termios
    }
}

#[cfg(test)]
#[path = "../tests/inline/relaunch.rs"]
mod tests;
