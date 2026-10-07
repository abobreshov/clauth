#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

//! Pins for the herdr resume-command report leg (`hook_resume`): the trigger
//! (`SessionStart`, a real attribution change, nothing else), the report
//! argv, the no-op cases (no pane seat, no attributable profile, a subagent
//! fire, an unchanged account), the durable strictly-increasing seq, and the
//! swallow-don't-fail posture. A shim herdr binary (via `HERDR_BIN_PATH`)
//! appends its argv to a log file, so every assertion reads the real spawned
//! argv. Nothing here touches a live herdr socket.
//!
//! unix-only: the shim is POSIX shell, which Windows cannot execute.

use std::time::Duration;

use super::*;
use crate::hook_note::{Payload, load_record, record_path, store_record};
use crate::testutil::{
    EnvPin, HomeSandbox, echo_shim, exit1_shim, hang_shim, herdr_pane_env, report_lines, seq_of,
    wait_for_lines,
};

/// A main-scope payload carrying only what these tests vary.
fn payload(event: &str, session: &str) -> Payload {
    Payload {
        event: event.to_string(),
        session_id: session.to_string(),
        agent_id: None,
        tool_name: None,
        source: None,
        transcript: None,
    }
}

/// The line one shim report records, with its own seq spliced back in — the
/// seq itself is pinned by the range and monotonicity assertions.
fn expected_line(pane: &str, sid: &str, profile: &str, seq: u64) -> String {
    format!(
        "pane report-agent {pane} --source tollgate --agent claude --state working \
         --seq {seq} -- tollgate resume {sid} --profile {profile}"
    )
}

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn a_session_start_in_a_herdr_pane_reports_the_resume_argv() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sid123");
    let before = crate::usage::now_ms();
    report_for_fire(&fire, Some("prof"), false);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
    let seq = seq_of(&lines[0]);
    assert!(seq >= before, "seq {seq} must sit on the wall clock");
    assert_eq!(lines[0], expected_line("w1:p1", "sid123", "prof", seq));
}

#[test]
fn an_attribution_change_reports_the_new_profile() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("PostToolUse", "sidmove");
    report_for_fire(&fire, Some("moved-to"), true);
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        expected_line("w1:p1", "sidmove", "moved-to", seq_of(&lines[0]))
    );
}

#[test]
fn the_report_argv_is_pinned() {
    let expected: Vec<String> = [
        "pane",
        "report-agent",
        "w1:p1",
        "--source",
        "tollgate",
        "--agent",
        "claude",
        "--state",
        "working",
        "--seq",
        "42",
        "--",
        "tollgate",
        "resume",
        "sid",
        "--profile",
        "prof",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(build_report_args("w1:p1", "sid", "prof", 42), expected);
}

#[test]
fn seqs_strictly_increase_and_beat_any_stored_value() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidseq");
    report_for_fire(&fire, Some("prof"), false);
    let first = seq_of(&wait_for_lines(home.home(), 1, WAIT)[0]);
    // A stored seq in the future forces the mint past it: herdr silently keeps
    // the stored argv for any report whose seq is not strictly newer, so the
    // mint must be per-session monotone, not just wall-time-fresh.
    let path = record_path("sidseq", None).unwrap();
    let mut record = load_record(&path).unwrap_or_default();
    let stored: u64 = 1 << 62;
    record.resume_seq = Some(stored);
    store_record(&path, &record).unwrap();
    report_for_fire(&fire, Some("other"), true);
    let lines = wait_for_lines(home.home(), 2, WAIT);
    let second = seq_of(&lines[1]);
    assert!(
        second > first,
        "seqs must strictly increase: {first} then {second}"
    );
    assert_eq!(second, stored + 1, "the mint must beat the stored value");
}

#[test]
fn a_fire_outside_a_herdr_pane_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, None, Some(&shim));
    let fire = payload("SessionStart", "sidnopane");
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn a_pane_id_without_a_resolvable_herdr_bin_reports_nothing() {
    let home = HomeSandbox::new();
    // An empty PATH: the bin gate must not fall through to the operator's
    // real herdr.
    let empty = tempfile::tempdir_in(home.home()).unwrap();
    let _env = EnvPin::new(
        &home,
        &[
            ("HERDR_PANE_ID", Some(std::ffi::OsStr::new("w1:p1"))),
            ("PATH", Some(empty.path().as_os_str())),
        ],
    );
    let fire = payload("SessionStart", "sidnobin");
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn an_unattributed_fire_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidnoacct");
    report_for_fire(&fire, None, false);
    report_for_fire(&fire, None, true);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn an_unchanged_account_tool_call_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("PostToolUse", "sidstay");
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn a_subagent_fire_reports_nothing() {
    let home = HomeSandbox::new();
    let shim = echo_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let mut fire = payload("SessionStart", "sidsub");
    fire.agent_id = Some("agent-1".to_string());
    report_for_fire(&fire, Some("prof"), false);
    assert!(report_lines(home.home()).is_empty());
}

#[test]
fn a_failing_herdr_report_is_swallowed() {
    let home = HomeSandbox::new();
    let shim = exit1_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidfail");
    report_for_fire(&fire, Some("prof"), false);
    // The log line proves the spawn was attempted; reaching this assert at
    // all proves the failing status was swallowed.
    let lines = wait_for_lines(home.home(), 1, WAIT);
    assert_eq!(lines.len(), 1);
}

#[test]
fn a_hung_herdr_report_is_killed_within_its_deadline() {
    let home = HomeSandbox::new();
    let shim = hang_shim(home.home(), "herdr");
    let _env = herdr_pane_env(&home, Some("w1:p1"), Some(&shim));
    let fire = payload("SessionStart", "sidhang");
    let started = std::time::Instant::now();
    report_for_fire(&fire, Some("prof"), false);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the report must stay bounded, took {:?}",
        started.elapsed()
    );
}
