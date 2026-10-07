//! The herdr resume-command report leg of `tollgate
//! hook-profile-changed-note`: tell the herdr pane a conversation runs in how
//! to bring that conversation back, so a herdr restart restores it as
//! `tollgate resume <session> --profile <account>` instead of herdr's builtin
//! bare `claude --resume <id>` — which loses the account and answers `No
//! conversation found` for isolated-store sessions.
//!
//! The hook is the only component that natively knows both halves of that
//! command: the CC session id arrives in the hook payload, and the resolved
//! account is the same tier-walk answer the account note and the exact-owner
//! stamp carry. As a Claude Code child inside the pane it also inherits
//! `HERDR_PANE_ID`/`HERDR_BIN_PATH`, which the herdr plugin's own watcher
//! cannot see (it joins on pids and only ever learns tollgate's minted ids).
//!
//! Fires on `SessionStart` and on a real attribution change — the two events
//! that can change the resume command (the session id or the profile moved) —
//! never on every tool call. Outside a herdr pane (no `HERDR_PANE_ID`, or no
//! herdr binary), without an attributable account, and on any failure of the
//! bounded report spawn, the leg is silence: herdr's builtin plan stays as the
//! fallback, which is exactly the restore behavior a box had before this leg
//! existed.
//!
//! The report carries `--state working`: every fire happens while Claude Code
//! is working, so the value is true at fire time. herdr rejects a foreign
//! state on a pane its own integration has anchored (its detection keeps the
//! chip) and shows the state only on panes it cannot judge itself, where the
//! value clears when the agent process exits.
//!
//! The seq is minted epoch-ms and persisted on the conversation's note
//! record, because herdr drops a report whose seq is not strictly newer than
//! the stored one — a fresh process whose first mint landed on the same
//! millisecond as the previous process's last report would otherwise be
//! silently lost.

use std::path::Path;

use crate::hook_note::{Payload, ScopeLock, load_record, record_path, store_record};

/// The report's fixed agent fields: tollgate speaks as source `tollgate` about
/// the `claude` agent herdr detects in the pane. `herdr:claude` itself is
/// reserved for herdr's own CC integration; a distinct source is what lets
/// the report ride beside it instead of replacing it.
const SOURCE: &str = "tollgate";
const AGENT: &str = "claude";

/// The state every report carries. The hook fires only while Claude Code is
/// working (`SessionStart` or an attribution change mid-conversation), so the
/// value is true at fire time; herdr discards it on panes its own integration
/// has anchored and clears it with the agent process on the rest.
const STATE: &str = "working";

pub(crate) fn report_for_fire(payload: &Payload, current: Option<&str>, attribution_changed: bool) {
    // A subagent's fire is its own scope's reading, never the pane
    // conversation's attribution.
    if payload.agent_id.is_some() {
        return;
    }
    // Only a `SessionStart` or a real attribution change can move the resume
    // command — its session id or its profile changed — so every other fire
    // (per tool call, per prompt) is a no-op.
    if payload.event != "SessionStart" && !attribution_changed {
        return;
    }
    // No attributable account: nothing to name, herdr's builtin plan stays.
    let Some(profile) = current else {
        return;
    };
    // Outside a herdr pane there is nothing to report to.
    let Some(pane_id) = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.trim().is_empty())
    else {
        return;
    };
    let Some(bin) = crate::herdr::resolved_bin() else {
        return;
    };
    let Some(seq) = mint_seq(&payload.session_id) else {
        return;
    };
    let args = build_report_args(&pane_id, &payload.session_id, profile, seq);
    report(&bin, &args);
}

/// Mint the report seq for this conversation's main scope: epoch-ms, forced
/// past every seq this record has minted. herdr silently keeps the stored
/// argv for any report whose seq is not strictly newer than the last, so the
/// mint must be monotone across the separate hook processes one conversation
/// fires — the record is what turns the wall clock into a per-session clock.
/// `None` when the record cannot be persisted: an unrememberable seq cannot
/// promise monotonicity, and silence keeps herdr's existing argv standing.
fn mint_seq(session_id: &str) -> Option<u64> {
    let Ok(path) = record_path(session_id, None) else {
        return None;
    };
    let hold = ScopeLock::acquire_within(std::time::Duration::from_millis(500));
    if !hold.acquired() {
        return None;
    }
    let mut record = load_record(&path).unwrap_or_default();
    let seq = crate::usage::now_ms().max(record.resume_seq.unwrap_or(0).checked_add(1)?);
    record.resume_seq = Some(seq);
    store_record(&path, &record).ok()?;
    Some(seq)
}

/// The report argv, pane id first — herdr's hand-rolled parser reads the
/// first positional from args[0] and answers `unknown option` to anything
/// else in that slot. Everything after `--` is the resume command herdr
/// re-creates the pane with.
fn build_report_args(pane_id: &str, session_id: &str, profile: &str, seq: u64) -> Vec<String> {
    vec![
        "pane".into(),
        "report-agent".into(),
        pane_id.into(),
        "--source".into(),
        SOURCE.into(),
        "--agent".into(),
        AGENT.into(),
        "--state".into(),
        STATE.into(),
        "--seq".into(),
        seq.to_string(),
        "--".into(),
        "tollgate".into(),
        "resume".into(),
        session_id.into(),
        "--profile".into(),
        profile.into(),
    ]
}

/// One bounded spawn of the herdr CLI; every failure is a logged silence, the
/// same posture the account note keeps. [`crate::herdr::bounded_output`]
/// kills a child that hangs past its deadline, so a stuck herdr costs the
/// hook at most that bound.
fn report(bin: &Path, args: &[String]) {
    let Some(bin) = bin.to_str() else {
        crate::logline::to_logfile(format_args!(
            "tollgate: herdr bin path is not valid UTF-8; resume command not reported"
        ));
        return;
    };
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match crate::herdr::bounded_output(bin, &argv, &[]) {
        Some(output) if output.status.success() => {}
        Some(output) => {
            let status = output.status;
            crate::logline::to_logfile(format_args!(
                "tollgate: herdr pane report-agent exited {status} (resume command not reported)"
            ));
        }
        None => {
            crate::logline::to_logfile(format_args!(
                "tollgate: herdr pane report-agent failed to spawn or timed out (resume command not reported)"
            ));
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/hook_resume.rs"]
mod tests;
