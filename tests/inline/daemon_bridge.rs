#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The mod bridge: `~/.tollgate/bridge.sock`'s bind rules, the session a socket
//! peer resolves to, and the control routes that queue commands for a mod.
//!
//! The peer resolver is a table the test owns (pid -> instance), so a test can
//! pose a second process on one id, a `/clear` moving an id, or an unknown
//! peer; the one end-to-end test resolves the test process's own pid as the
//! socket reports it.

#![cfg(unix)]

use super::*;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::daemon::api::devices::Tier;
use crate::daemon::api::panes::{HerdrProbeOut, PaneProbe};
use crate::daemon::api::routes::ApiContext;
use crate::testutil::{DEVICE, HomeSandbox, OTHER_TOKEN, TOKEN, body_json, call, req, seed_device};

type Table = Arc<Mutex<HashMap<u32, Instance>>>;

fn instance(session_id: &str, proc_start: &str) -> Instance {
    Instance {
        session_id: session_id.to_string(),
        proc_start: proc_start.to_string(),
    }
}

/// A resolver over a table the test can rewrite mid-test.
fn table(rows: &[(u32, Instance)]) -> (Table, PeerResolver) {
    let table: Table = Arc::new(Mutex::new(rows.iter().cloned().collect()));
    let read = Arc::clone(&table);
    let resolver: PeerResolver = Box::new(move |pid| read.lock().unwrap().get(&pid).cloned());
    (table, resolver)
}

fn config() -> crate::profile::ConfigHandle {
    Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ))
}

/// herdr absent: a session with no mod has nothing to fall back to.
fn no_herdr() -> PaneProbe {
    Box::new(|_, _| HerdrProbeOut::NotInstalled)
}

fn ctx_over(hub: &Arc<Hub>) -> Arc<ApiContext> {
    let status_path = crate::profile::tollgate_dir()
        .expect("tollgate dir")
        .join("status.json");
    ApiContext::for_tests_with_bridge(config(), status_path, no_herdr(), Arc::clone(hub))
}

fn start(hub: &Hub, pid: u32, session: &str) {
    let body = serde_json::json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w/app", "interactive": true});
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "start for {session} from {pid}: {answer}");
}

fn commands(hub: &Hub, pid: u32, session: &str) -> serde_json::Value {
    let (status, answer) = hub.poll(pid, session, Duration::ZERO);
    assert_eq!(status, 200, "poll {session} from {pid}: {answer}");
    answer["commands"].clone()
}

fn open_ask(hub: &Hub, pid: u32, session: &str, seq: u64, ask: &str) -> serde_json::Value {
    let body = serde_json::json!({"session": session, "seq": seq, "kind": "ask_opened", "ask": ask, "tool": "Bash", "input": {"command": "ls"}});
    let (status, answer) = hub.event(pid, body.to_string().as_bytes());
    assert_eq!(status, 200, "ask_opened {ask}: {answer}");
    answer
}

/// The socket is the daemon's own: 0600 in a 0700 dir.
#[test]
fn the_socket_is_owner_only_in_an_owner_only_dir() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".tollgate/bridge.sock");
    let _listener = bind(&path).expect("bind");
    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    let sock_mode = std::fs::symlink_metadata(&path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700);
    assert_eq!(sock_mode & 0o777, 0o600);
}

/// A path a live listener holds is never unlinked: the bind refuses and the
/// incumbent keeps answering.
#[test]
fn a_live_listener_at_the_path_is_refused() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".tollgate/bridge.sock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let squatter = UnixListener::bind(&path).unwrap();
    assert!(bind(&path).is_err(), "bound over a live listener");
    UnixStream::connect(&path).expect("the incumbent still answers");
    drop(squatter);
}

/// A crashed daemon's file (no listener behind it) is replaced.
#[test]
fn a_stale_socket_file_is_replaced() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".tollgate/bridge.sock");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    drop(UnixListener::bind(&path).unwrap());
    assert!(path.exists(), "the stale file is in place");
    let _listener = bind(&path).expect("a stale file is replaced");
    UnixStream::connect(&path).expect("the new listener answers");
}

/// The session id is resolved from the peer, never taken off the wire: an
/// event naming another session is refused and registers nothing.
#[test]
fn an_event_naming_another_session_is_refused() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 200, "s-two");

    let forged = serde_json::json!({"session": "s-two", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    let (status, answer) = hub.event(100, forged.to_string().as_bytes());
    assert_eq!(status, 403);
    assert_eq!(answer["error"], "session_mismatch");

    let (status, answer) = hub.poll(100, "s-two", Duration::ZERO);
    assert_eq!(status, 403);
    assert_eq!(answer["error"], "session_mismatch");
}

/// A peer the resolver knows nothing about is refused.
#[test]
fn an_unknown_peer_is_refused() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let body = serde_json::json!({"session": "s-one", "seq": 1, "kind": "start", "cwd": "/w", "interactive": true});
    let (status, answer) = hub.event(300, body.to_string().as_bytes());
    assert_eq!(status, 403);
    assert_eq!(answer["error"], "unknown_peer");
}

/// A prompt from a control device reaches the named session's poll once,
/// and no other session's.
#[test]
fn a_prompt_reaches_only_its_own_sessions_poll() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-two", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-two");
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"fix it"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        body_json(&resp),
        serde_json::json!({"ok": true, "via": "mod"})
    );

    assert_eq!(commands(&hub, 200, "s-two"), serde_json::json!([]));
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "prompt", "text": "fix it"}])
    );
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([]),
        "delivered once"
    );
}

/// An interrupt reaches the mod as its own command.
#[test]
fn an_interrupt_reaches_the_mod() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/interrupt",
            Some(TOKEN),
            "{}",
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        body_json(&resp),
        serde_json::json!({"ok": true, "via": "mod"})
    );
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "interrupt"}])
    );
}

/// The audit line names the device, the session and the text's length,
/// never the text.
#[test]
fn the_prompt_audit_line_carries_no_text() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let ctx = ctx_over(&hub);
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"secret-canary"}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "tollgate api: device '{DEVICE}' prompted session 's-one' via mod text_len=13"
        )]
    );
}

/// Every command route is control tier; a view device queues nothing.
#[test]
fn a_view_device_cannot_drive_a_session() {
    let _home = HomeSandbox::new();
    seed_device("viewer", Tier::View, OTHER_TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let ctx = ctx_over(&hub);

    for (path, body) in [
        ("/api/v1/sessions/s-one/prompt", r#"{"text":"x"}"#),
        ("/api/v1/sessions/s-one/interrupt", "{}"),
        ("/api/v1/sessions/s-one/asks/a1", r#"{"decision":"allow"}"#),
    ] {
        let resp = call(&ctx, &req("POST", path, Some(OTHER_TOKEN), body));
        assert_eq!(resp.status, 403, "{path}");
        assert_eq!(body_json(&resp)["error"], "control_required", "{path}");
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
}

/// An ask is answered once: the answer reaches the mod, a second answer and
/// an unknown ask are refused by name.
#[test]
fn an_ask_is_answered_once() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    assert_eq!(open_ask(&hub, 100, "s-one", 2, "a1")["hold"], true);
    let ctx = ctx_over(&hub);

    let first = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/a1",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(first.status, 200);
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "a1", "decision": "allow"}])
    );

    let second = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/a1",
            Some(TOKEN),
            r#"{"decision":"deny"}"#,
        ),
    );
    assert_eq!(second.status, 409);
    assert_eq!(body_json(&second)["error"], "ask_closed");

    let unknown = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/zz",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(unknown.status, 404);
    assert_eq!(body_json(&unknown)["error"], "ask_not_found");
}

/// An `AskUserQuestion` ask takes its answers as the body names them.
#[test]
fn a_question_ask_takes_answers() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "q1");
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/q1",
            Some(TOKEN),
            r#"{"answers":{"Which colour?":"blue"}}"#,
        ),
    );
    assert_eq!(resp.status, 200);
    assert_eq!(
        commands(&hub, 100, "s-one"),
        serde_json::json!([{"kind": "answer", "ask": "q1", "answers": {"Which colour?": "blue"}}])
    );
}

/// A body naming both, or neither, of `decision` and `answers` is refused
/// before anything is queued.
#[test]
fn an_ask_answer_names_exactly_one_form() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let ctx = ctx_over(&hub);

    for body in [
        "{}",
        r#"{"decision":"allow","answers":{"q":"a"}}"#,
        r#"{"decision":"maybe"}"#,
        r#"{"answers":"x"}"#,
        r#"{"answers":[]}"#,
    ] {
        let resp = call(
            &ctx,
            &req("POST", "/api/v1/sessions/s-one/asks/a1", Some(TOKEN), body),
        );
        assert_eq!(resp.status, 400, "{body}");
        assert_eq!(body_json(&resp)["error"], "bad_request", "{body}");
    }
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
}

/// The desk answering first closes the ask for every device.
#[test]
fn a_desk_answer_closes_the_ask() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    open_ask(&hub, 100, "s-one", 2, "a1");
    let closed = serde_json::json!({"session": "s-one", "seq": 3, "kind": "ask_closed", "ask": "a1", "by": "desk"});
    assert_eq!(hub.event(100, closed.to_string().as_bytes()).0, 200);
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/asks/a1",
            Some(TOKEN),
            r#"{"decision":"allow"}"#,
        ),
    );
    assert_eq!(resp.status, 409);
    assert_eq!(body_json(&resp)["error"], "ask_closed");
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
}

/// The mod holds only when a remote answer is possible: a bound listener
/// and a paired control device. Each half alone answers `hold: false`, and
/// the poll carries the same flag as `remote`.
#[test]
fn the_hold_flag_needs_a_listener_and_a_control_device() {
    for (listening, tier, want) in [
        (true, Some(Tier::Control), true),
        (true, Some(Tier::View), false),
        (true, None, false),
        (false, Some(Tier::Control), false),
    ] {
        let _home = HomeSandbox::new();
        if let Some(ref tier) = tier {
            seed_device(DEVICE, tier.clone(), TOKEN);
        }
        let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
        let hub = Hub::new(listening, resolver);
        start(&hub, 100, "s-one");
        let answer = open_ask(&hub, 100, "s-one", 2, "a1");
        assert_eq!(answer["hold"], want, "listening={listening} tier={tier:?}");
        let (_, polled) = hub.poll(100, "s-one", Duration::ZERO);
        assert_eq!(
            polled["remote"], want,
            "listening={listening} tier={tier:?}"
        );
    }
}

/// `/clear` keeps the pid and moves the record's id: the old session closes,
/// so a command for it answers 404 and its poll is refused.
#[test]
fn a_cleared_session_closes() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (table, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    table.lock().unwrap().insert(100, instance("s-three", "1"));
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"x"}"#,
        ),
    );
    assert_eq!(resp.status, 404);
    assert_eq!(body_json(&resp)["error"], "session_not_live");
    let (status, _) = hub.poll(100, "s-one", Duration::ZERO);
    assert_eq!(status, 403);
}

/// A recycled pid (same pid, another process start) is a new instance: the
/// old session closes rather than passing to the newcomer.
#[test]
fn a_recycled_pid_does_not_inherit_a_session() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (table, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    table.lock().unwrap().insert(100, instance("s-one", "99"));
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"x"}"#,
        ),
    );
    assert_eq!(resp.status, 404);
    assert_eq!(body_json(&resp)["error"], "session_not_live");
}

/// Two live processes resolving to one session id make it ambiguous: a
/// command for it is refused rather than handed to either.
#[test]
fn two_instances_on_one_id_are_ambiguous() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[(100, instance("s-one", "1")), (200, instance("s-one", "2"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    start(&hub, 200, "s-one");
    let ctx = ctx_over(&hub);

    let resp = call(
        &ctx,
        &req(
            "POST",
            "/api/v1/sessions/s-one/prompt",
            Some(TOKEN),
            r#"{"text":"x"}"#,
        ),
    );
    assert_eq!(resp.status, 409);
    assert_eq!(body_json(&resp)["error"], "session_ambiguous");
    assert_eq!(commands(&hub, 100, "s-one"), serde_json::json!([]));
    assert_eq!(commands(&hub, 200, "s-one"), serde_json::json!([]));
}

/// An event at or below the instance's high-water mark is dropped by name.
#[test]
fn a_stale_sequence_is_dropped() {
    let _home = HomeSandbox::new();
    let (_t, resolver) = table(&[(100, instance("s-one", "1"))]);
    let hub = Hub::new(true, resolver);
    start(&hub, 100, "s-one");
    let turn = |seq: u64| serde_json::json!({"session": "s-one", "seq": seq, "kind": "turn_start", "turn": format!("t{seq}")});
    assert_eq!(hub.event(100, turn(3).to_string().as_bytes()).0, 200);
    let (status, answer) = hub.event(100, turn(2).to_string().as_bytes());
    assert_eq!(status, 409);
    assert_eq!(answer["error"], "stale_sequence");
}

/// A session with no mod and no herdr pane to fall back to has no reach.
#[test]
fn a_session_with_neither_mod_nor_pane_is_not_live() {
    let _home = HomeSandbox::new();
    seed_device(DEVICE, Tier::Control, TOKEN);
    let (_t, resolver) = table(&[]);
    let hub = Hub::new(true, resolver);
    let ctx = ctx_over(&hub);
    for (path, body) in [
        ("/api/v1/sessions/s-nine/prompt", r#"{"text":"x"}"#),
        ("/api/v1/sessions/s-nine/interrupt", "{}"),
    ] {
        let resp = call(&ctx, &req("POST", path, Some(TOKEN), body));
        assert_eq!(resp.status, 404, "{path}");
        assert_eq!(body_json(&resp)["error"], "session_not_live", "{path}");
    }
}

/// End to end over the real socket: the server reads the peer pid off the
/// connection (the test process itself), so an event naming the resolved
/// session is taken and one naming another is refused.
#[test]
fn the_socket_resolves_the_connecting_process() {
    let sb = HomeSandbox::new();
    let path = sb.home().join(".tollgate/bridge.sock");
    let (_t, resolver) = table(&[(std::process::id(), instance("s-self", "1"))]);
    let hub = Hub::new(true, resolver);
    let listener = bind(&path).expect("bind");
    let server = spawn_server(listener, Arc::clone(&hub)).expect("spawn");

    let post = |session: &str| -> String {
        let body = serde_json::json!({"session": session, "seq": 1, "kind": "start", "cwd": "/w", "interactive": true}).to_string();
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "POST /event HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    };
    assert!(
        post("s-self").starts_with("HTTP/1.1 200"),
        "the resolved session is taken"
    );
    assert!(
        post("s-other").starts_with("HTTP/1.1 403"),
        "another session is refused"
    );
    server.stop();
}
