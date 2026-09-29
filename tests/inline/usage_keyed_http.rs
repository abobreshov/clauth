//! The key-bearing transport against real loopback sockets: a redirect is
//! refused and never followed, an oversize body is refused, and a server that
//! sends its headers and then stalls cannot hold the caller past the deadline.

use super::*;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

/// Serve exactly one connection with `respond`, on a thread.
fn serve_once(
    respond: impl FnOnce(TcpStream) + Send + 'static,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let url = format!("http://{}/api/v1/key", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            respond(stream);
        }
    });
    (url, handle)
}

/// Read the request head so the client is past its send phase.
fn read_head(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => buf.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn the_shared_agent_has_a_finite_deadline_and_follows_no_redirect() {
    let cfg = agent().config();
    assert_eq!(cfg.max_redirects(), 0);
    let t = cfg.timeouts();
    assert_eq!(t.global, Some(CALL_DEADLINE));
    assert_eq!(t.recv_body, Some(CALL_DEADLINE));
    assert!(t.connect.is_some() && t.recv_response.is_some());
}

#[test]
fn a_redirect_is_refused_and_the_target_never_sees_the_key() {
    let target = TcpListener::bind(("127.0.0.1", 0)).expect("bind target");
    target.set_nonblocking(true).unwrap();
    let location = format!("http://{}/steal", target.local_addr().unwrap());
    let (url, server) = serve_once(move |mut s| {
        let _ = read_head(&mut s);
        let _ = write!(
            s,
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    });
    let reply = get_bearer(&url, "sk-test-placeholder");
    server.join().unwrap();
    assert_eq!(reply, None, "a 3xx is no answer");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        target.accept().is_err(),
        "the redirect target must never be contacted"
    );
}

#[test]
fn an_oversize_body_is_refused() {
    let (url, server) = serve_once(|mut s| {
        let _ = read_head(&mut s);
        let len = MAX_BODY_BYTES as usize + 1024;
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
        );
        let _ = s.write_all(&vec![b'a'; len]);
    });
    let reply = get_bearer(&url, "sk-test-placeholder").expect("a status arrived");
    server.join().unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.body, None, "a body over 2 MiB is unreadable");
}

#[test]
fn a_body_within_the_cap_reads_with_its_status_and_retry_after() {
    let (url, server) = serve_once(|mut s| {
        let head = read_head(&mut s);
        assert!(head.contains("Bearer sk-test-placeholder"), "{head}");
        let _ = write!(
            s,
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
    });
    let reply = get_bearer(&url, "sk-test-placeholder").expect("reply");
    server.join().unwrap();
    assert_eq!(reply.status, 429);
    assert_eq!(reply.retry_after, Some(Duration::from_secs(30)));
    assert_eq!(reply.body.as_deref(), Some("{}"));
}

/// Headers arrive, then the body stalls with the socket held open: the read
/// ends at the deadline instead of hanging the caller.
#[test]
fn a_stalled_body_ends_at_the_deadline() {
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (url, server) = serve_once(move |mut s| {
        let _ = read_head(&mut s);
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"da"
        );
        let _ = s.flush();
        // Hold the socket open until the client gave up.
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
    });
    let agent = build_agent(Duration::from_millis(800));
    let started = Instant::now();
    let reply = get_bearer_with(&agent, &url, "sk-test-placeholder");
    let took = started.elapsed();
    let _ = release_tx.send(());
    server.join().unwrap();
    assert!(took < Duration::from_secs(5), "stalled for {took:?}");
    // Either the call or the body read gave up; no body was invented.
    assert!(reply.as_ref().is_none_or(|r| r.body.is_none()), "{reply:?}");
}

#[test]
fn send_keeps_only_allowlisted_response_headers() {
    let (url, server) = serve_once(|mut s| {
        read_head(&mut s);
        write!(s, "HTTP/1.1 200 OK\r\nX-Ratelimit-Limit-Requests: 60\r\nX-Nous-Credits-Remaining-Micros: 1000\r\nSet-Cookie: TOKEN-CANARY\r\nX-Secret: TOKEN-CANARY\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
    });
    let reply = send(
        agent(),
        &Request {
            method: Method::Get,
            url: &url,
            auth: Auth::None,
            extra: &[],
            json_body: None,
        },
    )
    .unwrap();
    server.join().unwrap();
    assert_eq!(
        reply.headers,
        vec![
            ("x-ratelimit-limit-requests".into(), "60".into()),
            ("x-nous-credits-remaining-micros".into(), "1000".into())
        ]
    );
    assert!(!format!("{:?}", reply.headers).contains("CANARY"));
}

#[test]
fn send_never_follows_a_redirect_for_post() {
    let target = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    target.set_nonblocking(true).unwrap();
    let location = format!("http://{}/steal", target.local_addr().unwrap());
    let (url, server) = serve_once(move |mut s| {
        let head = read_head(&mut s);
        assert!(head.starts_with("POST "));
        assert!(head.to_lowercase().contains("user-agent: antigravity"));
        write!(s, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let token = crate::usage::monitor::source::Secret::new("TOKEN-CANARY");
    assert!(
        send(
            agent(),
            &Request {
                method: Method::Post,
                url: &url,
                auth: Auth::Bearer(&token),
                extra: &[("User-Agent", "antigravity")],
                json_body: Some(b"{}")
            }
        )
        .is_none()
    );
    server.join().unwrap();
    assert!(target.accept().is_err());
}

#[test]
fn response_header_bounds_and_exact_names() {
    assert!(response_header_allowed("x-nous-tool-pool-free"));
    assert!(!response_header_allowed("retry-after-secret"));
    let (url, server) = serve_once(|mut s| {
        read_head(&mut s);
        write!(s, "HTTP/1.1 200 OK\r\nX-Ratelimit-Limit: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "1".repeat(300)).unwrap();
    });
    let reply = get_bearer(&url, "CANARY").unwrap();
    server.join().unwrap();
    assert_eq!(reply.headers[0].1.len(), 256);
}

fn synthetic_reply(status: u16) -> Reply {
    Reply {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        retry_after: None,
        body: Some("{\"value\":1}".into()),
    }
}
fn recording_observer() -> (ResponseObserver, Arc<std::sync::Mutex<Vec<u16>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recording = seen.clone();
    (
        Arc::new(move |reply| recording.lock().unwrap().push(reply.status)),
        seen,
    )
}
#[test]
fn response_observer_records_only_inside_its_scope() {
    let (observer, seen) = recording_observer();
    assert!(!response_observer_active());
    observe_response(&synthetic_reply(100));
    let answer = with_response_observer(observer, || {
        assert!(response_observer_active());
        observe_response(&synthetic_reply(200));
        42
    });
    assert_eq!(answer, 42);
    assert!(!response_observer_active());
    observe_response(&synthetic_reply(201));
    assert_eq!(*seen.lock().unwrap(), [200]);
}
#[test]
fn nested_response_observers_restore_the_outer_scope() {
    let (outer, outer_seen) = recording_observer();
    let (inner, inner_seen) = recording_observer();
    with_response_observer(outer, || {
        observe_response(&synthetic_reply(200));
        with_response_observer(inner, || observe_response(&synthetic_reply(401)));
        observe_response(&synthetic_reply(201));
    });
    assert_eq!(*outer_seen.lock().unwrap(), [200, 201]);
    assert_eq!(*inner_seen.lock().unwrap(), [401]);
    assert!(!response_observer_active());
}
#[test]
fn response_observers_restore_on_unwind() {
    let (outer, seen) = recording_observer();
    with_response_observer(outer, || {
        let (inner, inner_seen) = recording_observer();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_response_observer(inner, || {
                observe_response(&synthetic_reply(401));
                panic!("fixture unwind");
            })
        }));
        assert!(result.is_err());
        assert_eq!(*inner_seen.lock().unwrap(), [401]);
        observe_response(&synthetic_reply(200));
    });
    assert_eq!(*seen.lock().unwrap(), [200]);
    assert!(!response_observer_active());
}
#[test]
fn response_observers_are_isolated_between_threads() {
    let (observer, seen) = recording_observer();
    with_response_observer(observer, || {
        std::thread::spawn(|| {
            assert!(!response_observer_active());
            observe_response(&synthetic_reply(401));
        })
        .join()
        .unwrap();
        observe_response(&synthetic_reply(200));
    });
    assert_eq!(*seen.lock().unwrap(), [200]);
}
#[test]
fn response_observer_callbacks_can_install_reentrant_scopes() {
    let nested_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let inner_seen = nested_seen.clone();
    let observer: ResponseObserver = Arc::new(move |reply| {
        let recording = inner_seen.clone();
        let nested: ResponseObserver =
            Arc::new(move |reply| recording.lock().unwrap().push(reply.status));
        with_response_observer(nested, || {
            observe_response(&synthetic_reply(reply.status + 1))
        });
    });
    with_response_observer(observer, || {
        observe_response(&synthetic_reply(200));
        observe_response(&synthetic_reply(202));
    });
    assert_eq!(*nested_seen.lock().unwrap(), [201, 203]);
    assert!(!response_observer_active());
}
