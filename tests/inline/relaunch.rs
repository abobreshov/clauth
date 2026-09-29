//! Relaunch in place (P6b): the CLI's preconditions and request, the
//! supervisor's claim, stop and resume command, the hand-off nonce, and the
//! terminal restore. No test execs or spawns `claude`: the exec is a seam.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::testutil::{HomeSandbox, live_row, transcript_fixture};

/// Configure blank OAuth profiles (every one admissible).
fn profiles(names: &[&str]) {
    let mut config = crate::profile::load_config().expect("config");
    for name in names {
        crate::actions::create_blank_profile(&mut config, (*name).to_string(), None, None, None)
            .expect("create profile");
    }
}

/// A live, relaunch-capable claude session of `start` whose cwd is `cwd`.
fn live_session(sid: &str, start: &str, cwd: &Path) -> std::fs::File {
    let mut row = live_row(sid, start).with_relaunch(true, None);
    row.follows_chain = false;
    row.cwd = Some(cwd.to_path_buf());
    crate::live_sessions::register(&row).expect("register");
    crate::runtime::hold_session_row_marker(&ProfileName::from(start), false, sid).expect("marker")
}

fn store() -> PathBuf {
    let store = projects_store().expect("store");
    std::fs::create_dir_all(&store).expect("store dir");
    store
}

fn reason(err: anyhow::Error) -> String {
    err.downcast_ref::<RelaunchRefused>()
        .map(|r| r.reason.clone())
        .unwrap_or_else(|| panic!("not a relaunch refusal: {err:#}"))
}

fn request_for(target: &str, conv: &str, cwd: &Path) -> RelaunchRequest {
    RelaunchRequest {
        version: 1,
        target: target.to_string(),
        conversation: conv.to_string(),
        cwd: cwd.to_path_buf(),
        follows_chain: false,
        requested_at_ms: 1,
        requester_pid: 1,
        nonce: None,
    }
}

fn write_request(sid: &str, value: &serde_json::Value) {
    let path = crate::live_sessions::relaunch_path(sid, "").expect("path");
    std::fs::create_dir_all(path.parent().expect("registry dir")).expect("registry dir");
    std::fs::write(path, serde_json::to_vec(value).expect("ser")).expect("request");
}

// 49
#[test]
fn relaunch_refuses_ambiguous_or_missing_conversations() {
    let home = HomeSandbox::new();
    let store = store();
    let cwd = home.home().join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let mut row = live_row("4242-0", "p");
    row.cwd = Some(cwd.clone());
    row.started_at = 0;
    assert_eq!(
        resolve_conversation(&row, None, &store),
        Err("no conversation found for the session".to_string())
    );
    transcript_fixture(&store, &cwd, &["conv-1"]);
    assert_eq!(
        resolve_conversation(&row, None, &store),
        Ok("conv-1".to_string())
    );
    transcript_fixture(&store, &cwd, &["conv-2"]);
    assert_eq!(
        resolve_conversation(&row, None, &store),
        Err("2 conversations match; pass --conversation <id>".to_string())
    );
    assert_eq!(
        resolve_conversation(&row, Some("conv-2"), &store),
        Ok("conv-2".to_string())
    );
    assert!(resolve_conversation(&row, Some("nope"), &store).is_err());
    // Transcripts from before the session started are not its own.
    row.started_at = crate::usage::now_ms() + 60_000;
    assert!(resolve_conversation(&row, None, &store).is_err());
    // A hook record naming this runtime wins over the scan.
    let record = crate::hook_note::record_path("conv-hook", None).expect("path");
    std::fs::create_dir_all(record.parent().expect("dir")).expect("records dir");
    std::fs::write(&record, br#"{"runtime_sid":"4242-0"}"#).expect("record");
    assert_eq!(
        resolve_conversation(&row, None, &store),
        Ok("conv-hook".to_string())
    );
}

// 50
#[test]
fn relaunch_resolves_preconditions_before_any_signal() {
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(".claude")).expect(".claude");
    profiles(&["pr-a", "pr-b", "pr-off"]);
    let mut config = crate::profile::load_config().expect("config");
    crate::actions::disable_profile(&mut config, &ProfileName::from("pr-off")).expect("disable");
    let store = store();
    let cwd = home.home().join("w");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let _marker = live_session("4242-0", "pr-a", &cwd);
    let request_file = crate::live_sessions::relaunch_path("4242-0", "").expect("path");

    // A disabled target is refused by `admit`.
    transcript_fixture(&store, &cwd, &["c1"]);
    let why = reason(prepare("4242-0", "pr-off", None).expect_err("disabled"));
    assert!(why.contains("disabled"), "{why}");
    // An unknown target.
    assert!(prepare("4242-0", "nope", None).is_err());
    // A missing cwd.
    let mut row = crate::live_sessions::get("4242-0").expect("row");
    row.cwd = Some(home.home().join("gone"));
    crate::live_sessions::register(&row).expect("re-register");
    let why = reason(prepare("4242-0", "pr-b", Some("c1")).expect_err("no cwd"));
    assert_eq!(why, "its working directory no longer exists");
    // An isolated session.
    row.cwd = Some(cwd.clone());
    row.isolated = true;
    crate::live_sessions::register(&row).expect("re-register");
    let _iso = crate::runtime::hold_session_row_marker(&ProfileName::from("pr-a"), true, "4242-0")
        .expect("isolated marker");
    let why = reason(prepare("4242-0", "pr-b", None).expect_err("isolated"));
    assert_eq!(
        why,
        "isolated sessions relaunch empty; resume it with 'tollgate resume'"
    );
    // None of it wrote a request, so nothing reached the session.
    assert!(!request_file.exists());
    // A passing prepare writes nothing either.
    row.isolated = false;
    crate::live_sessions::register(&row).expect("re-register");
    let prepared = prepare("4242-0", "pr-b", None).expect("prepared");
    assert_eq!(prepared.conversation, "c1");
    assert_eq!(prepared.request.target, "pr-b");
    assert_eq!(prepared.request.cwd, cwd);
    assert!(!request_file.exists());
}

// 51
#[test]
fn a_non_tty_relaunch_without_yes_is_a_usage_error() {
    let _home = HomeSandbox::new();
    // The test runner's stdin is not a terminal.
    let err = run_cli("4242-0", "p", false, None).expect_err("usage error");
    assert_eq!(crate::exit_code(Err(err)), 2);
    assert!(
        !crate::live_sessions::relaunch_path("4242-0", "")
            .expect("path")
            .exists()
    );
}

// 52
#[cfg(unix)]
#[test]
fn the_supervisor_claims_once_stops_gracefully_and_execs_the_resume_form() {
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(".claude")).expect(".claude");
    profiles(&["cl-a", "cl-b"]);
    let cwd = home.home().join("w");
    std::fs::create_dir_all(&cwd).expect("cwd");
    transcript_fixture(&store(), &cwd, &["conv-9"]);
    let mut request = request_for("cl-b", "conv-9", &cwd);
    request.follows_chain = false;
    write_request("4242-0", &serde_json::to_value(&request).expect("value"));

    let accepted = poll_claim("4242-0").expect("claimed");
    assert!(poll_claim("4242-0").is_none(), "a request is claimed once");
    assert_eq!(accepted.nonce.len(), 32);
    assert!(accepted.nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(
        !crate::live_sessions::relaunch_path("4242-0", "")
            .expect("path")
            .exists()
    );
    let taken: RelaunchRequest = serde_json::from_slice(
        &std::fs::read(crate::live_sessions::relaunch_path("4242-0", "taken").expect("path"))
            .expect("taken"),
    )
    .expect("parse");
    assert_eq!(taken.nonce.as_deref(), Some(accepted.nonce.as_str()));
    let result: RelaunchResult = serde_json::from_slice(
        &std::fs::read(crate::live_sessions::relaunch_path("4242-0", "result").expect("path"))
            .expect("result"),
    )
    .expect("parse");
    assert_eq!(result.outcome, "relaunching");

    // Graceful: the child exits on SIGTERM, well inside the grace.
    let mut child = crate::testutil::fake_supervisor_child();
    std::thread::sleep(Duration::from_millis(100));
    let status = stop_child(&mut child, Some(Duration::from_secs(5))).expect("stopped");
    assert!(status.success(), "the child exited on SIGTERM: {status:?}");

    // The exec'd form: the supervisor's own args minus every resume flag.
    let args: Vec<String> = ["--model", "x", "--resume", "old", "-c", "-r=o", "--foo"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let mut attempts = Vec::new();
    let last = exec_relaunch_with(&accepted, "cl-a", &args, "4242-0", &mut |c| {
        attempts.push((
            c.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            crate::testutil::env_overrides(c),
            c.get_current_dir().map(Path::to_path_buf),
        ));
        std::io::Error::other("posed exec failure")
    });
    let (argv, env, dir) = &attempts[0];
    assert_eq!(
        argv,
        &[
            "start", "cl-b", "--", "--model", "x", "--foo", "--resume", "conv-9"
        ]
    );
    assert_eq!(dir.as_deref(), Some(cwd.as_path()));
    assert_eq!(
        env.get(RELAUNCHED_FROM_ENV),
        Some(&Some("4242-0".to_string()))
    );
    assert_eq!(env.get(NONCE_ENV), Some(&Some(accepted.nonce.clone())));
    assert_eq!(env.get(FALLBACK_ENV), Some(&Some("cl-a".to_string())));
    // 53's half: the target's exec failing restarts the original profile.
    let (argv, env, _) = &attempts[1];
    assert_eq!(argv[..2], ["start".to_string(), "cl-a".to_string()]);
    assert_eq!(env.get(FALLBACK_ENV), Some(&None), "no second fallback");
    assert_eq!(
        last,
        "tollgate: relaunch failed; resume with: tollgate start cl-a -- --resume conv-9"
    );
    let _ =
        std::fs::remove_file(crate::live_sessions::relaunch_path("4242-0", "taken").expect("path"));
}

// 53
#[test]
fn a_failed_relaunch_restarts_the_original_profile() {
    let _home = HomeSandbox::new();
    // The new process: a start that failed before its child spawned restarts
    // on the original profile once, without the hand-off variables.
    let args = vec!["--resume".to_string(), "conv-3".to_string()];
    let mut seen = Vec::new();
    let last = exec_fallback_with("orig", &args, &mut |c| {
        seen.push((
            c.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            crate::testutil::env_overrides(c),
        ));
        std::io::Error::other("posed")
    });
    assert_eq!(seen.len(), 1, "one fallback attempt");
    assert_eq!(seen[0].0, ["start", "orig", "--", "--resume", "conv-3"]);
    for key in RELAUNCH_ENV_KEYS {
        assert_eq!(seen[0].1.get(*key), Some(&None), "{key} is removed");
    }
    assert_eq!(
        last,
        "tollgate: relaunch failed; resume with: tollgate start orig -- --resume conv-3"
    );
}

// 54
#[test]
fn an_unclaimed_request_is_cancelled_after_30_s() {
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(".claude")).expect(".claude");
    profiles(&["un-a", "un-b"]);
    let cwd = home.home().join("w");
    std::fs::create_dir_all(&cwd).expect("cwd");
    transcript_fixture(&store(), &cwd, &["c-un"]);
    let _marker = live_session("4242-0", "un-a", &cwd);
    set_deadlines(Some(Deadlines {
        claim: Duration::from_millis(150),
        register: Duration::from_millis(150),
        poll: Duration::from_millis(10),
    }));
    let prepared = prepare("4242-0", "un-b", None).expect("prepared");
    let why = reason(submit(&prepared).expect_err("nobody claimed"));
    set_deadlines(None);
    assert_eq!(why, "the session did not answer; it is unchanged");
    for suffix in ["", "cancel", "taken", "result"] {
        assert!(
            !crate::live_sessions::relaunch_path("4242-0", suffix)
                .expect("path")
                .exists(),
            "relaunch.{suffix} left behind"
        );
    }
    let row = crate::live_sessions::get("4242-0").expect("row");
    assert_eq!(row.intended_member, None);
}

// 54a
#[test]
fn relaunch_env_is_scrubbed_from_the_child_and_ignored_without_a_matching_nonce() {
    let home = HomeSandbox::new();
    let _pins = crate::testutil::EnvPin::new(
        &home,
        &[
            (RELAUNCHED_FROM_ENV, Some(std::ffi::OsStr::new("4242-0"))),
            (FALLBACK_ENV, Some(std::ffi::OsStr::new("orig"))),
            (NONCE_ENV, Some(std::ffi::OsStr::new("forged"))),
        ],
    );
    // The spawned Claude Code's env lacks all three.
    let engine: &dyn crate::harness::HarnessEngine = &crate::harness::ClaudeEngine;
    let mut command = engine.command();
    engine.scrub_env(&mut command, &[]);
    let env = crate::testutil::env_overrides(&command);
    for key in RELAUNCH_ENV_KEYS {
        assert_eq!(env.get(*key), Some(&None), "{key} reaches the child");
    }
    // No `.relaunch.taken`: a forged hand-off is ignored whole.
    assert_eq!(Handoff::from_env(), Handoff::default());
    // A mismatched nonce is ignored too.
    let taken = crate::live_sessions::relaunch_path("4242-0", "taken").expect("path");
    std::fs::create_dir_all(taken.parent().expect("dir")).expect("dir");
    let mut stored = request_for("t", "c", home.home());
    stored.nonce = Some("the-real-one".to_string());
    std::fs::write(&taken, serde_json::to_vec(&stored).expect("ser")).expect("taken");
    assert_eq!(Handoff::from_env(), Handoff::default());
    assert!(taken.exists(), "a mismatch leaves the file for its owner");
    // The matching nonce is honoured and consumes the file.
    let verified = Handoff::verify(
        Some("4242-0".to_string()),
        Some("orig".to_string()),
        Some("the-real-one".to_string()),
    );
    assert_eq!(verified.from.as_deref(), Some("4242-0"));
    assert_eq!(verified.fallback.as_deref(), Some("orig"));
    assert!(!taken.exists());
}

// 54b
#[test]
fn a_relaunch_request_cannot_inject_claude_args() {
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(".claude")).expect(".claude");
    profiles(&["in-a", "in-b"]);
    let cwd = home.home().join("w");
    std::fs::create_dir_all(&cwd).expect("cwd");
    transcript_fixture(&store(), &cwd, &["c-in"]);
    let mut value = serde_json::to_value(request_for("in-b", "c-in", &cwd)).expect("value");
    value["claude_args"] = serde_json::json!(["--dangerously-skip-permissions"]);
    value["argv"] = serde_json::json!(["--mcp-config", "evil.json"]);
    write_request("4242-0", &value);
    let accepted = poll_claim("4242-0").expect("claimed");
    let taken = std::fs::read_to_string(
        crate::live_sessions::relaunch_path("4242-0", "taken").expect("path"),
    )
    .expect("taken");
    assert!(
        !taken.contains("dangerously") && !taken.contains("evil"),
        "{taken}"
    );
    let own = vec!["--model".to_string(), "m".to_string()];
    let command = resume_command(
        Path::new("/bin/tollgate"),
        "in-b",
        &accepted,
        &own,
        "4242-0",
        None,
    );
    let argv: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        argv,
        ["start", "in-b", "--", "--model", "m", "--resume", "c-in"]
    );
    let _ =
        std::fs::remove_file(crate::live_sessions::relaunch_path("4242-0", "taken").expect("path"));
}

// 54c
#[cfg(target_os = "linux")]
#[test]
#[allow(unsafe_code)]
fn a_sigkilled_child_leaves_the_tty_restored() {
    // A child that ignores SIGTERM is SIGKILLed once the grace runs out.
    let mut stubborn = std::process::Command::new("sh")
        .arg("-c")
        .arg("trap '' TERM; while :; do sleep 0.05; done")
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    std::thread::sleep(Duration::from_millis(100));
    let status = stop_child(&mut stubborn, Some(Duration::from_millis(200))).expect("stopped");
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    // A pty stands in for the operator's terminal.
    // SAFETY: plain libc pty calls on fds this test owns and closes.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0);
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(master)).to_owned();
        let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        assert!(slave >= 0);

        let saved = TtyState::save(slave).expect("a pty is a terminal");
        // The child leaves it raw.
        let mut raw = *saved.termios();
        libc::cfmakeraw(&mut raw);
        assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &raw), 0);

        saved.restore();
        let mut now: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave, &mut now), 0);
        let before = saved.termios();
        assert_eq!(now.c_lflag, before.c_lflag);
        assert_eq!(now.c_iflag, before.c_iflag);
        assert_eq!(now.c_oflag, before.c_oflag);
        assert_eq!(now.c_cflag, before.c_cflag);

        let mut buf = [0u8; 64];
        let n = libc::read(master, buf.as_mut_ptr().cast(), buf.len());
        assert!(n > 0);
        let written = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        assert!(written.contains("\x1b[?1049l\x1b[?25h"), "{written:?}");
        libc::close(slave);
        libc::close(master);
    }
    assert!(
        TtyState::save(-1).is_none(),
        "not a terminal: nothing saved"
    );
}

// 54d
#[test]
fn a_row_without_relaunch_capable_refuses_with_the_manual_resume_line() {
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(".claude")).expect(".claude");
    profiles(&["pre-a", "pre-b"]);
    let mut row = live_row("4242-0", "pre-a");
    row.cwd = Some(home.home().to_path_buf());
    crate::live_sessions::register(&row).expect("register");
    let _marker =
        crate::runtime::hold_session_row_marker(&ProfileName::from("pre-a"), false, "4242-0")
            .expect("marker");
    let why = reason(prepare("4242-0", "pre-b", Some("conv-7")).expect_err("predates relaunch"));
    assert_eq!(
        why,
        "the session predates relaunch; exit and 'tollgate start pre-b -- --resume conv-7'"
    );
}

/// The resume-flag strip, every spelling.
#[test]
fn strip_resume_args_drops_every_resume_and_continue_spelling() {
    let args: Vec<String> = [
        "--resume",
        "a",
        "-r",
        "b",
        "--resume=c",
        "-r=d",
        "--continue",
        "-c",
        "--resume",
        "--model",
        "m",
        "keep",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(strip_resume_args(&args), ["--model", "m", "keep"]);
}
