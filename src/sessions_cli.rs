//! `tollgate sessions [--json] [--tokens]`, `tollgate resume <id|latest>
//! [--profile <name>]`, `tollgate info <id|latest>`, and the two-name form of
//! `tollgate switch <sid> <profile>` (one name is the global account switch,
//! `main::cmd_switch`) — the CLI surface over the session index
//! ([`crate::sessions`]). The index owns the heavy work (transcript walk,
//! preview redaction, token/cost annotation, owner stamping); this module
//! only flattens it, renders it, and drives the account-aware resume spawn.
//! `switch` is the exception: it touches no transcript at all, only the
//! live-session registry ([`crate::live_sessions`]).
//!
//! # What each command reads
//! Only `sessions` browses, so only `sessions` builds the index. `resume` and
//! `info` want one row, and take [`crate::sessions::find_session`] /
//! [`crate::sessions::newest_session`] instead: a filename-and-mtime walk, then
//! the head of the single transcript they resolved to. Those two read the shared
//! store only, so a target the index would have found in a live isolated runtime
//! is reported against [`crate::sessions::live_isolated_holds`] (the same tier-1
//! walk) rather than called missing — see [`Resolved`]. The token and cost
//! figures are a third tier above even the index — a full read of every
//! transcript — so `sessions` leaves them blank until `--tokens` asks.
//!
//! Over the maintainer's own 12k-session, 5.4 GB store: `tollgate info latest`
//! 11.3 s → 44 ms, `tollgate sessions` 20.7 s → 11.3 s, and `--tokens` reproduces
//! the old listing byte for byte.
//!
//! # Exit codes (the `tollgate sessions` scripting contract)
//! - `0` success.
//! - `1` a genuine error, INCLUDING "no sessions found".
//! - `2` a usage error (bad flag/args).
//! - `3` (`switch <sid> <p> --wait` only) the session committed the profile
//!   but its key helper had not served it when the wait ran out, or failed.
//!
//! `1` vs `2` is carried by [`crate::UsageError`] and mapped in
//! [`crate::exit_code`]: a `sessions`/`resume`/`info` dispatch arm returns a
//! `UsageError` for a malformed invocation, and any other `Err` (an empty index
//! included) maps to `1`.

use std::io::IsTerminal as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::out::{errln, out, outln};
use crate::profile::{AppConfig, load_config};
use crate::runtime::Isolation;
use crate::sessions::{IsolatedHold, SessionInfo, SessionRef, WorkspaceGroup};

/// `tollgate sessions [--json] [--tokens]` — the full inventory, newest-first.
/// Both a TTY and a pipe print a table (the `--json` flag, not the tty, selects
/// machine output; this is deliberately NOT showagent's pipe-prints-different
/// behavior). An empty index is exit 1 ("no sessions found") on both paths, per
/// the scripting contract above.
pub(crate) fn run_sessions(json: bool, tokens: bool) -> Result<()> {
    let groups = build_listing(tokens);

    let flat = flatten_newest_first(&groups);
    if flat.is_empty() {
        anyhow::bail!("no sessions found");
    }
    if json {
        outln!("{}", sessions_json(&flat));
    } else {
        emit_sessions_table(&groups, tokens);
    }
    Ok(())
}

/// The listing's data, with the token/cost annotation left off unless asked for.
///
/// That annotation reads every transcript in the store IN FULL — a tier above
/// the index's own bounded head+tail reads, and the reason it is opt-in: a
/// listing should not cost a multi-gigabyte parse to show ids and previews.
/// Skipped, `tokens`/`cost` stay `None`, which the table renders as no columns
/// at all and `--json` as `null` — the same `null` a session with no
/// token-bearing row gets, so a consumer that wants the figures asks for them
/// rather than inferring anything from a blank.
fn build_listing(tokens: bool) -> Vec<WorkspaceGroup> {
    let mut groups = crate::sessions::build_index();
    if tokens {
        // A cold price cache prices nothing (blank cost), never blocks the listing.
        let price = crate::pricing::load_cached();
        crate::sessions::annotate_all(&mut groups, price.as_ref());
    }
    crate::sessions::annotate_owners(&mut groups);
    groups
}

/// `tollgate resume <id|latest> [--profile <name>]` — resume a session through the
/// existing `tollgate start` spawn path (runtime prep, signal forwarding, lifetime
/// guard), with `--resume <id>` injected and the session's recorded workspace as
/// the child cwd. Never a second spawn implementation. `latest` = the newest
/// session `tollgate sessions` would list first; any other value is an exact id
/// match. Either can name a session a live isolated run holds, which is refused
/// by name rather than resumed or silently swapped for another.
pub(crate) fn run_resume(target: &str, profile_flag: Option<&str>) -> Result<()> {
    crate::platform::init();
    crate::runtime::gc_stale_runtimes();
    let config = load_config()?;

    let session = match resolve_session(target) {
        Resolved::Ready(session) => session,
        Resolved::Held(hold) => return Err(held_refusal(target, &hold)),
        Resolved::Missing => anyhow::bail!("no session found for '{target}'"),
    };

    // Resume must land in the recorded workspace, else `--resume` would run in
    // the wrong dir (or fail to find the transcript). Refuse rather than spawn.
    let workspace = session.workspace().ok_or_else(|| {
        anyhow::anyhow!(
            "can't resume '{}': no workspace recorded for it",
            session.id
        )
    })?;
    if !workspace.is_dir() {
        anyhow::bail!(
            "can't resume '{}': workspace '{}' no longer exists",
            session.id,
            workspace.display()
        );
    }

    // The owner drives the interactive profile default.
    let last_ran = crate::sessions::owner_of(&session.id);
    let active = config.state.active_profile.as_deref().unwrap_or_default();
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (default_profile, should_prompt) =
        resume_profile_choice(profile_flag, is_tty, last_ran.as_deref(), active);
    let chosen = if should_prompt {
        prompt_profile(&config, &default_profile)?
    } else {
        default_profile
    };

    let canonical = resolve_profile_name(&config, &chosen)?;

    // Guest mode: the session's `projects/` is tollgate's guest store, never
    // the operator's, so the transcript is copied there for `--resume` to find
    // (`runtime::seed_guest_resume`; a no-op otherwise).
    crate::runtime::seed_guest_resume(&session.path)?;

    let resume_args = vec!["--resume".to_string(), session.id];
    // Shared isolation: a resume adopts the chosen account against the shared
    // store, the same lifecycle a bare `tollgate start <name>` uses. A resume never
    // opts into the fallback chain — there is no `--with-fallback` on this
    // surface to ask for it. In guest mode the shared runtime it builds is the
    // guest one (`runtime::build_runtime_dir_with_active_env`), so a resume
    // links neither `plugins/` nor `projects/` at the operator's trees.
    crate::start::run(
        &config,
        &canonical,
        &resume_args,
        Isolation::Shared,
        Some(&workspace),
        false,
        None,
    )
}

/// `tollgate info <id|latest>` — print the exact `tollgate resume` command, the
/// workspace, and the on-disk storage path. Never launches anything.
pub(crate) fn run_info(target: &str) -> Result<()> {
    let (session, held_by) = match resolve_session(target) {
        Resolved::Ready(session) => (session, None),
        // `info` launches nothing, so a held session is reportable where it is
        // not resumable: its storage path is the one thing that says where the
        // transcript actually lives, and nothing else on any surface prints it.
        Resolved::Held(hold) => (hold.session, Some(hold.profile)),
        Resolved::Missing => anyhow::bail!("no session found for '{target}'"),
    };
    outln!("{}", info_lines(&session, held_by.as_deref()));
    Ok(())
}

/// The three lines `tollgate info` prints. Pure, so both variants are assertable
/// without capturing stdout. A held session gets no resume command: printing one
/// that Claude Code would answer `No conversation found` for is worse than
/// saying why there isn't one.
fn info_lines(session: &SessionRef, held_by: Option<&str>) -> String {
    let resume = match held_by {
        Some(profile) => {
            format!("unavailable while a live isolated run under '{profile}' holds this session")
        }
        None => format!("tollgate resume {}", session.id),
    };
    format!(
        "resume:    {resume}\nworkspace: {}\nstorage:   {}",
        session.workspace().unwrap_or_default().display(),
        session.path.display(),
    )
}

/// The flags of `tollgate switch`'s session form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SwitchFlags {
    pub(crate) wait: bool,
    pub(crate) relaunch: bool,
    pub(crate) yes: bool,
    pub(crate) conversation: Option<String>,
}

/// A usage error whose exact line is printed here (exit 2).
fn usage_line(line: &str) -> anyhow::Error {
    errln!("{line}");
    anyhow::Error::new(crate::relaunch::Reported(2))
}

/// The flag rules of `tollgate switch`: every session flag needs the
/// two-name form, and `--yes` / `--conversation` need `--relaunch`.
pub(crate) fn check_switch_flags(two_names: bool, flags: &SwitchFlags) -> Result<()> {
    let any = flags.wait || flags.relaunch || flags.yes || flags.conversation.is_some();
    if any && !two_names {
        return Err(usage_line(
            "tollgate: --wait, --relaunch, --yes and --conversation need a session id and a profile",
        ));
    }
    if (flags.yes || flags.conversation.is_some()) && !flags.relaunch {
        return Err(usage_line(
            "tollgate: --yes and --conversation need --relaunch",
        ));
    }
    Ok(())
}

/// A live claude session's row, or the refusal `tollgate switch` prints for
/// a missing, codex or dead one.
pub(crate) fn live_claude_row(sid: &str) -> Result<crate::live_sessions::LiveSession> {
    let Some(row) = crate::live_sessions::get(sid) else {
        // Arity alone chose the session form, so a name that also resolves to
        // a configured profile gets pointed at the spelling that switches the
        // global account (the same two-roster resolution `cmd_switch` takes)
        // instead of a dead end.
        let resolves_anywhere = load_config()
            .ok()
            .is_some_and(|config| config.canonical_name(sid).is_some())
            || crate::codex_profiles::CodexState::load()
                .ok()
                .is_some_and(|state| state.canonical_name(sid).is_some());
        let hint = if resolves_anywhere {
            format!("\nto switch the global account: `tollgate switch {sid}`")
        } else {
            String::new()
        };
        anyhow::bail!("no live session '{sid}'\nsee `tollgate sessions`{hint}");
    };
    // A codex row has no executor: codex reads auth.json once at start, so a
    // mid-session intent would stand forever as a silent no-op.
    if row.harness == crate::harness::Harness::Codex {
        anyhow::bail!("session '{sid}' is a codex session; switch is claude-only");
    }
    // The same liveness probe the tally and the decision leg use, current
    // member first: a row whose flock is gone is a stale row awaiting GC, not
    // a session to move.
    let probe = crate::profile::ProfileName::from(
        row.current_member.as_deref().unwrap_or(&row.start_profile),
    );
    if !crate::runtime::session_row_is_live(&probe, row.isolated, &row.session_id) {
        anyhow::bail!(
            "session '{sid}' is no longer running\n\
             its row is reaped by the next `tollgate daemon` or `tollgate resume`"
        );
    }
    Ok(row)
}

/// What the switch request core did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestOutcome {
    /// The session already runs the member.
    AlreadyOn,
    /// Executor A: the intent is recorded; the session's executor acts on it.
    IntentRecorded,
    /// The row cannot hot-swap; the code names why.
    RelaunchRequired(String),
    /// Executor B's pre-check or the session itself refused the member.
    Refused(String),
    /// Executor B committed the member at this key generation.
    Committed(u64),
    /// No commit inside the commit wait.
    Requested,
}

/// One request's outcome plus the names its lines print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwitchRequest {
    pub(crate) sid: String,
    pub(crate) target: String,
    /// The member the session ran on when asked.
    pub(crate) current: String,
    pub(crate) outcome: RequestOutcome,
}

/// How long the request core waits for a commit, and `--wait` for the
/// helper to serve it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SwitchWaits {
    pub(crate) commit: std::time::Duration,
    pub(crate) served: std::time::Duration,
    pub(crate) commit_poll: std::time::Duration,
    pub(crate) served_poll: std::time::Duration,
}

impl SwitchWaits {
    const PRODUCTION: Self = Self {
        commit: std::time::Duration::from_secs(5),
        served: std::time::Duration::from_secs(65),
        commit_poll: std::time::Duration::from_millis(100),
        served_poll: std::time::Duration::from_millis(250),
    };
}

#[cfg(test)]
thread_local! {
    static WAITS: std::cell::Cell<Option<SwitchWaits>> = const { std::cell::Cell::new(None) };
}

/// Test clock seam: shrink the request core's waits.
#[cfg(test)]
pub(crate) fn set_switch_waits(waits: Option<SwitchWaits>) {
    WAITS.with(|w| w.set(waits));
}

fn switch_waits() -> SwitchWaits {
    #[cfg(test)]
    if let Some(w) = WAITS.with(std::cell::Cell::get) {
        return w;
    }
    SwitchWaits::PRODUCTION
}

/// The request core every surface shares: resolve, branch on the row's
/// executor, and for executor B write the intent and wait (5 s) for the
/// session to commit it or refuse it.
pub(crate) fn request_session_switch(sid: &str, profile: &str) -> Result<SwitchRequest> {
    let row = live_claude_row(sid)?;
    let config = load_config()?;
    let canonical = resolve_profile_name(&config, profile)?;
    // `current_member` is None until a session's first swap, so a session that
    // never moved runs as its launch profile.
    let current = row
        .current_member
        .clone()
        .unwrap_or_else(|| row.start_profile.clone());
    let done = |outcome| {
        Ok(SwitchRequest {
            sid: sid.to_string(),
            target: canonical.as_str().to_string(),
            current: current.clone(),
            outcome,
        })
    };
    if current == canonical.as_str() {
        return done(RequestOutcome::AlreadyOn);
    }
    match row.executor() {
        crate::hot_swap::Executor::Oauth => {
            crate::live_sessions::update_as_daemon(sid, |fields| {
                fields.set_intended_member(canonical.as_str());
            })?;
            done(RequestOutcome::IntentRecorded)
        }
        crate::hot_swap::Executor::RelaunchOnly { reason } => {
            done(RequestOutcome::RelaunchRequired(reason))
        }
        crate::hot_swap::Executor::None => {
            anyhow::bail!("session '{sid}' has no in-session executor; switch by relaunch")
        }
        crate::hot_swap::Executor::ApiKey => {
            let Some(class) = row.launch_class.as_ref() else {
                return done(RequestOutcome::RelaunchRequired("registry".to_string()));
            };
            if let Err(code) = crate::hot_swap::check_target(&canonical, class) {
                return done(RequestOutcome::Refused(code.to_string()));
            }
            let g0 = row.key_generation.unwrap_or(0);
            let t0 = crate::usage::now_ms();
            crate::live_sessions::update_as_daemon(sid, |fields| {
                fields.set_intended_member(canonical.as_str());
            })?;
            let waits = switch_waits();
            let deadline = std::time::Instant::now() + waits.commit;
            loop {
                if let Some(now) = crate::live_sessions::get(sid) {
                    let generation = now.key_generation.unwrap_or(0);
                    if now.current_member.as_deref() == Some(canonical.as_str()) && generation > g0
                    {
                        return done(RequestOutcome::Committed(generation));
                    }
                    if let Some(refusal) = now
                        .swap_refusal
                        .filter(|r| r.member == canonical.as_str() && r.at_ms >= t0)
                    {
                        return done(RequestOutcome::Refused(refusal.code));
                    }
                }
                if std::time::Instant::now() >= deadline {
                    return done(RequestOutcome::Requested);
                }
                std::thread::sleep(waits.commit_poll);
            }
        }
    }
}

/// What `--wait` saw after a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Served(u64),
    /// The helper ran for the commit and failed with this code.
    Stalled(String),
    /// The wait ran out with the commit not served.
    TimedOut,
}

/// Poll the row's [`crate::hot_swap::SwapView`] until the helper serves
/// `generation`, reports a failure for it, or the wait runs out.
pub(crate) fn wait_until_served(sid: &str, generation: u64) -> WaitOutcome {
    let waits = switch_waits();
    let deadline = std::time::Instant::now() + waits.served;
    loop {
        if let Some(row) = crate::live_sessions::get(sid) {
            let ack = crate::live_sessions::read_helper_ack(sid);
            let view = crate::hot_swap::SwapView::of(&row, ack.as_ref());
            if view
                .served
                .as_ref()
                .is_some_and(|p| p.generation >= generation)
            {
                return WaitOutcome::Served(generation);
            }
            if let Some(code) = view.stall_code {
                return WaitOutcome::Stalled(code);
            }
        }
        if std::time::Instant::now() >= deadline {
            return WaitOutcome::TimedOut;
        }
        std::thread::sleep(waits.served_poll);
    }
}

/// The relaunch hint every B refusal ends with.
fn relaunch_hint(sid: &str, target: &str) -> String {
    format!("relaunch instead: tollgate switch {sid} {target} --relaunch")
}

/// `tollgate switch <sid> <profile> [--wait] [--relaunch ...]` — move a live
/// session to another profile.
///
/// Executor A records the intent through the same
/// [`crate::live_sessions::update_as_daemon`] seam the daemon's decision leg
/// writes, and the session's own executor performs or refuses it. Executor B
/// commits in the session within a tick; this waits five seconds for that and
/// reports it. `--relaunch` stops the session and resumes its conversation
/// under the profile ([`crate::relaunch`]).
pub(crate) fn run_switch(sid: &str, profile: &str, flags: &SwitchFlags) -> Result<()> {
    if flags.relaunch {
        return crate::relaunch::run_cli(sid, profile, flags.yes, flags.conversation.as_deref());
    }
    let request = request_session_switch(sid, profile)?;
    let SwitchRequest {
        sid,
        target,
        current,
        outcome,
    } = &request;
    match outcome {
        RequestOutcome::AlreadyOn => {
            outln!("{}", already_on_line(sid, target));
            Ok(())
        }
        RequestOutcome::IntentRecorded => {
            outln!("{}", switch_receipt(sid, target));
            Ok(())
        }
        RequestOutcome::RelaunchRequired(code) => {
            let cc = crate::hot_swap::cached_cc_version_lockfree();
            errln!(
                "tollgate: session '{sid}' cannot hot-swap ({})\n{}",
                crate::hot_swap::reason_text(code, cc.as_deref()),
                relaunch_hint(sid, target)
            );
            Err(anyhow::Error::new(crate::relaunch::Reported(1)))
        }
        RequestOutcome::Refused(code) => {
            errln!(
                "tollgate: session '{sid}' stays on '{current}': '{target}' is not hot-swappable ({})\n{}",
                crate::hot_swap::reason_text(code, None),
                relaunch_hint(sid, target)
            );
            Err(anyhow::Error::new(crate::relaunch::Reported(1)))
        }
        RequestOutcome::Requested => {
            outln!(
                "tollgate: requested '{target}' for session '{sid}'; the session has not \
                 committed it yet (swapping…)"
            );
            Ok(())
        }
        RequestOutcome::Committed(generation) => {
            outln!("{}", committed_lines(sid, target, *generation));
            if !flags.wait {
                return Ok(());
            }
            match wait_until_served(sid, *generation) {
                WaitOutcome::Served(n) => {
                    outln!(
                        "tollgate: session '{sid}' is served by '{target}' (key generation {n})"
                    );
                    Ok(())
                }
                WaitOutcome::Stalled(code) => {
                    errln!(
                        "tollgate: session '{sid}' is committed to '{target}' but its key helper \
                         failed ({code}); Claude Code keeps the previous key until it is rejected"
                    );
                    Err(anyhow::Error::new(crate::relaunch::Reported(3)))
                }
                WaitOutcome::TimedOut => {
                    errln!(
                        "tollgate: session '{sid}' is committed to '{target}' but its key helper \
                         has not served it yet (still swapping…; the session has made no request \
                         since the commit)"
                    );
                    Err(anyhow::Error::new(crate::relaunch::Reported(3)))
                }
            }
        }
    }
}

/// The two lines an executor-B commit prints.
fn committed_lines(sid: &str, target: &str, generation: u64) -> String {
    format!(
        "tollgate: session '{sid}' committed to '{target}' (api-key hot swap, key generation \
         {generation})\ntollgate: swapping… Claude Code picks the new key up on its next request"
    )
}

/// The success receipt. Pure so the copy is assertable without capturing
/// stdout; the timing sentence is the multi-session design record's own
/// wording for when a switch lands.
fn switch_receipt(sid: &str, profile: &str) -> String {
    format!(
        "tollgate: pointed session '{sid}' at '{profile}'\n\
         the switch lands at the session's next request, never before it — \
         a refused move is logged and the session stays put"
    )
}

/// The no-op line for a session already running the named profile: the
/// executor treats an intent equal to the current member as the steady state,
/// so writing one would promise a move that never comes.
fn already_on_line(sid: &str, profile: &str) -> String {
    format!("tollgate: session '{sid}' is already on '{profile}'")
}

/// Pick the resume profile default and whether to prompt for it, across the four
/// branches:
/// 1. explicit `--profile` → that profile, forced (never prompt).
/// 2. piped/non-TTY, no flag → the active profile, forced (can't prompt).
/// 3. TTY, no flag, known last-ran → prompt, defaulting to the last-ran profile.
/// 4. TTY, no flag, unknown last-ran → prompt, defaulting to the active profile.
///
/// Pure and returns `(default_profile, should_prompt)` so the four branches are
/// unit-testable without a terminal.
fn resume_profile_choice(
    flag: Option<&str>,
    is_tty: bool,
    last_ran: Option<&str>,
    active: &str,
) -> (String, bool) {
    if let Some(explicit) = flag {
        return (explicit.to_string(), false);
    }
    if !is_tty {
        return (active.to_string(), false);
    }
    match last_ran {
        Some(p) => (p.to_string(), true),
        None => (active.to_string(), true),
    }
}

/// Resolve a chosen profile name to its canonical spelling, or an error listing
/// the available names — mirrors `main::resolve_or_bail`.
pub(crate) fn resolve_profile_name(
    config: &AppConfig,
    chosen: &str,
) -> Result<crate::profile::ProfileName> {
    config
        .canonical_name(chosen)
        .map(crate::profile::ProfileName::from)
        .ok_or_else(|| {
            let available = config.names().join(", ");
            anyhow::anyhow!("profile '{chosen}' not found\navailable: {available}")
        })
}

/// The candidate list [`prompt_profile`] offers, plus the resolved default:
/// every enabled profile name ([`AppConfig::enabled_profiles`], the same view
/// `which`/`status` read), with `default` swapped for the first enabled name
/// when the caller's default is itself disabled — a stale `last_ran_profile`
/// that's since been disabled must not show as the bracketed default for a
/// name that isn't even listed. Pure so the disabled-exclusion is
/// unit-testable without a terminal.
fn resume_candidates<'a>(config: &'a AppConfig, default: &'a str) -> (Vec<&'a str>, &'a str) {
    let enabled: Vec<&str> = config.enabled_profiles().map(|p| p.name.as_str()).collect();
    let resolved = if enabled.contains(&default) {
        default
    } else {
        enabled.first().copied().unwrap_or(default)
    };
    (enabled, resolved)
}

/// Interactive profile prompt: list the enabled profiles (the default
/// marked), read a line, and take the default on empty input. TTY-only —
/// reached only when [`resume_profile_choice`] returns `should_prompt`. An
/// explicit `--profile <disabled>` skips this prompt entirely and is still
/// caught by `start::run`'s authoritative refusal.
fn prompt_profile(config: &AppConfig, default: &str) -> Result<String> {
    let (enabled, default) = resume_candidates(config, default);
    outln!("resume under which account?");
    for name in enabled.iter().copied() {
        let marker = if name == default { "  (default)" } else { "" };
        outln!("  {name}{marker}");
    }
    out!("profile [{default}]: ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let picked = line.trim();
    Ok(if picked.is_empty() {
        default.to_string()
    } else {
        picked.to_string()
    })
}

/// Flatten every group's sessions into one newest-first list. Groups are already
/// newest-first, but a flat cross-workspace order needs the same key
/// (`updated` desc, id asc) as [`crate::sessions`]'s within-group sort.
fn flatten_newest_first(groups: &[WorkspaceGroup]) -> Vec<&SessionInfo> {
    let mut all: Vec<&SessionInfo> = groups.iter().flat_map(|g| g.sessions.iter()).collect();
    all.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    all
}

/// What a `<id|latest>` target resolved to. "Not in the shared store" is not
/// "not there": `tollgate sessions` browses live isolated stores too, so a target
/// naming one of those is a real session that a resume simply cannot reach yet,
/// and saying "no session found" for it would be false.
enum Resolved {
    /// A session a resume can reach.
    Ready(SessionRef),
    /// A live isolated run holds it.
    Held(IsolatedHold),
    /// No session of that name anywhere.
    Missing,
}

/// Resolve `latest` to the newest session, or any other value to an exact id
/// match.
///
/// Both forms are the targeted lookup, never [`crate::sessions::build_index`]:
/// `resume` and `info` each use one row, and building the whole index to find it
/// reads every transcript in the store twice over. `latest` keeps the index's
/// own newest-first ordering over the sessions a resume can reach, so the two
/// surfaces agree except where the listing's first row is a nested transcript
/// Claude Code will not open.
fn resolve_session(target: &str) -> Resolved {
    let found = if target == "latest" {
        crate::sessions::newest_session()
    } else {
        crate::sessions::find_session(target)
    };
    match shadowing_hold(target, found.as_ref()) {
        Some(hold) => Resolved::Held(hold),
        None => found.map_or(Resolved::Missing, Resolved::Ready),
    }
}

/// The live isolated transcript that makes the shared store's answer the wrong
/// one to act on.
///
/// For an exact id: the run holding that id, asked only once the shared store
/// has come up empty. A rescue in flight can leave one id in both stores, and
/// there the shared copy is the reachable one, so a hit is never second-guessed.
///
/// For `latest`: a transcript strictly newer than the newest reachable session.
/// Without this the newest session on the machine drops out of the search and
/// `latest` quietly names the second newest — a session the operator never
/// asked for, spending an account window on the wrong conversation. An exact
/// mtime tie leaves the reachable session the answer. Only the isolated
/// transcripts a rescue could make resumable count here, matching what
/// [`crate::sessions::newest_session`] ranges over; a nested one is never
/// anybody's `latest`, in either store.
fn shadowing_hold(target: &str, found: Option<&SessionRef>) -> Option<IsolatedHold> {
    if target != "latest" {
        if found.is_some() {
            return None;
        }
        return crate::sessions::live_isolated_holds()
            .into_iter()
            .find(|h| h.session.id == target);
    }
    let newest = crate::sessions::live_isolated_top_level_holds()
        .into_iter()
        .max_by(|a, b| {
            a.session
                .updated
                .cmp(&b.session.updated)
                .then_with(|| a.session.path.cmp(&b.session.path))
        })?;
    found
        .is_none_or(|f| newest.session.updated > f.updated)
        .then_some(newest)
}

/// The refusal for a target a live isolated run holds. Names the run's profile
/// and how the session becomes reachable, since "wait for it" is only actionable
/// if the operator knows the run ending is what moves it.
fn held_refusal(target: &str, hold: &IsolatedHold) -> anyhow::Error {
    let what = if target == "latest" {
        format!("the newest session ('{}')", hold.session.id)
    } else {
        format!("'{}'", hold.session.id)
    };
    anyhow::anyhow!(
        "can't resume '{target}': {what} belongs to a live isolated run under profile '{}', \
         whose store a resume can't read\n\
         it moves into the shared store when that run ends",
        hold.profile,
    )
}

/// The stable `tollgate sessions --json` array (newest-first). Documented fields
/// only: `id`, `last_ran_profile`, `workspace`, `updated`, `first_message`,
/// `last_message`, `tokens`, `cost`. Absent `tokens`/`cost` serialize to JSON
/// `null` (never `0`) — and without `--tokens` nothing asked for them, so every
/// row's pair is `null`. `updated` is ISO-8601 UTC
/// (`YYYY-MM-DDTHH:MM:SS+00:00`), matching the rest of tollgate's timestamps —
/// and deliberately NOT the human table's shape, which renders the same
/// instant in local wall clock with a relative age (the 2026-08-22
/// prose-stamp ruling).
fn sessions_json(sessions: &[&SessionInfo]) -> serde_json::Value {
    serde_json::Value::Array(sessions.iter().map(|s| session_json_row(s)).collect())
}

fn session_json_row(s: &SessionInfo) -> serde_json::Value {
    serde_json::json!({
        "id": s.id,
        "last_ran_profile": s.last_ran_profile,
        "workspace": s.workspace,
        "updated": crate::sessions::updated_iso(s.updated),
        "first_message": s.first_message,
        "last_message": s.last_message,
        "tokens": s.tokens,
        "cost": s.cost,
    })
}

/// Human table: a workspace header per group, then one row per session. The
/// index already redacted the previews, so nothing is masked here. The token and
/// cost columns appear only under `--tokens`; two permanently blank columns
/// would otherwise eat the width the previews want.
fn emit_sessions_table(groups: &[WorkspaceGroup], tokens: bool) {
    // One clock read for the whole table: every row's age is relative to the
    // same instant, and `session_row` stays pure (its `now` is a parameter,
    // never a read hidden inside).
    let now = SystemTime::now();
    for group in groups {
        let ws = if group.workspace.is_empty() {
            "(unknown workspace)"
        } else {
            &group.workspace
        };
        outln!("{ws}");
        for s in &group.sessions {
            outln!("{}", session_row(s, tokens, now));
        }
    }
}

/// One session's table row. Pure, so which columns a flag puts in it is
/// assertable without capturing stdout: `now` arrives from the caller, never
/// a clock read hidden inside. The `updated` cell is the 2026-08-22
/// prose-stamp ruling's shape — LOCAL wall clock (`YYYY-MM-DD HH:MM:SS`)
/// paired with its relative age, `2026-08-28 14:03:11 · 3h 12m ago`; the
/// machine ISO shape lives on in the `--json` row only. The token total and
/// its cost are blank when the annotation found none — never `0`, which
/// would read as a real figure — and absent entirely when `tokens` never
/// asked for them.
fn session_row(s: &SessionInfo, tokens: bool, now: SystemTime) -> String {
    let usage = if tokens {
        format!(
            "  {tokens:>10}  {cost:>8}",
            tokens = s.tokens.map(|t| t.to_string()).unwrap_or_default(),
            cost = s.cost.map(|c| format!("${c:.2}")).unwrap_or_default(),
        )
    } else {
        String::new()
    };
    let secs = s
        .updated
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The None arm is unreachable for a filesystem mtime (chrono's range
    // dwarfs any OS's); the dash is the table's no-data glyph, never a UTC
    // fallback — a bare stamp reads as local.
    let updated = crate::format::local_stamp(secs).unwrap_or_else(|| "-".to_string());
    let age_secs = now
        .duration_since(s.updated)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // `humanize_duration` spells ≤0 `now`, so the ` ago` pairing must skip
    // the non-positive ages: a sub-second-fresh file or a future mtime would
    // render `now ago`.
    let age = if age_secs <= 0 {
        "now".to_string()
    } else {
        format!("{} ago", crate::usage::humanize_duration(age_secs))
    };
    format!(
        "  {id:<8}  {profile:<12}  {updated} · {age}{usage}  {preview}",
        id = short_id(&s.id),
        profile = s.last_ran_profile.as_deref().unwrap_or("-"),
        preview = preview_pair(s),
    )
}

/// The first block of a uuid session id, enough to eyeball in the table (the
/// full id is what `tollgate resume`/`info` take). A non-uuid stem shows whole.
fn short_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// `first | last` message preview, each bounded so a long line can't blow the
/// row width. Already-redacted text, so re-truncation is safe.
fn preview_pair(s: &SessionInfo) -> String {
    let first = crate::format::truncate(s.first_message.as_deref().unwrap_or(""), 50);
    let last = crate::format::truncate(s.last_message.as_deref().unwrap_or(""), 50);
    match (first.is_empty(), last.is_empty()) {
        (true, true) => String::new(),
        (false, true) => first,
        (true, false) => last,
        (false, false) => format!("{first} | {last}"),
    }
}

#[cfg(test)]
#[path = "../tests/inline/sessions_cli.rs"]
mod tests;
