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
