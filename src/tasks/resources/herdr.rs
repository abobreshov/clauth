//! Explicit local socket observations only. Matching fields are neither a
//! Herdr-authenticated instance UUID nor pane adoption/lifecycle authority.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{State, observe_process, process_host};

const LIMIT: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(3);
const FIELDS: [&str; 7] = [
    "instance",
    "session",
    "workspace_id",
    "tab_id",
    "pane_id",
    "socket_path",
    "terminal_id",
];

pub(super) fn capture(socket: &Path, pane: &str) -> Result<BTreeMap<String, String>, State> {
    capture_with_timeout(socket, pane, TIMEOUT)
}

pub(super) fn inspect(expected: &BTreeMap<String, String>) -> (State, &'static str) {
    // Lifecycle fields (pane pid/start) may accompany the seven wire fields.
    // They are not part of the socket observation and cannot widen a match.
    if FIELDS.iter().any(|key| !expected.contains_key(*key))
        || !expected.get("instance").is_some_and(|value| {
            value.strip_prefix("herdr-unix-v1:").is_some_and(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        })
        || expected.get("session") != expected.get("socket_path")
    {
        return (
            State::Unsupported,
            "pane declaration lacks a supported explicit socket identity",
        );
    }
    match capture(Path::new(&expected["socket_path"]), &expected["pane_id"]) {
        Ok(observed)
            if FIELDS
                .iter()
                .all(|key| observed.get(*key) == expected.get(*key)) =>
        {
            (
                State::Matched,
                "local socket peer and pane identity fields matched at observation time",
            )
        }
        Ok(_) | Err(State::Mismatch) => (
            State::Mismatch,
            "local socket peer or pane identity fields differ",
        ),
        Err(State::Missing) => (State::Missing, "declared local socket is not visible"),
        Err(_) => (
            State::Unavailable,
            "stable local socket and pane identity could not be observed",
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SocketIdentity {
    path: PathBuf,
    dev: u64,
    inode: u64,
    uid: u32,
}

fn io_state(error: std::io::Error) -> State {
    if error.kind() == std::io::ErrorKind::NotFound {
        State::Missing
    } else {
        State::Unavailable
    }
}

fn socket_identity(path: &Path) -> Result<SocketIdentity, State> {
    let text = path.to_str().ok_or(State::Unavailable)?;
    if !path.is_absolute() || text.len() > 4096 || text.chars().any(char::is_control) {
        return Err(State::Unavailable);
    }
    let canonical = path.canonicalize().map_err(io_state)?;
    let text = canonical.to_str().ok_or(State::Unavailable)?;
    if text.len() > 4096 || text.chars().any(char::is_control) {
        return Err(State::Unavailable);
    }
    let metadata = std::fs::symlink_metadata(&canonical).map_err(io_state)?;
    if !metadata.file_type().is_socket() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(State::Unavailable);
    }
    Ok(SocketIdentity {
        path: canonical,
        dev: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
    })
}

fn matching_pid_view() -> Result<(), State> {
    // SO_PEERCRED's PID is in the caller's namespace, whereas /proc/PID uses
    // the mounted procfs view. Never interpret one namespace's PID in another.
    // process_host proves this via a single-level /proc/self/status NSpid.
    process_host().map(|_| ())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_' | b'.'))
}

fn capture_with_timeout(
    socket: &Path,
    pane: &str,
    timeout: Duration,
) -> Result<BTreeMap<String, String>, State> {
    let deadline = Instant::now() + timeout;
    if !valid_id(pane) {
        return Err(State::Unavailable);
    }
    matching_pid_view()?;
    let host = process_host()?;
    let before = socket_identity(socket)?;
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|_| State::Unavailable)?;
    let address = SocketAddrUnix::new(&before.path).map_err(|_| State::Unavailable)?;
    // A busy asynchronous connect fails closed; no blocking connect fallback.
    rustix::net::connect(&fd, &address).map_err(|_| State::Unavailable)?;
    let mut stream = UnixStream::from(fd);
    let credentials =
        rustix::net::sockopt::socket_peercred(&stream).map_err(|_| State::Unavailable)?;
    if credentials.uid.as_raw() != before.uid || credentials.uid != rustix::process::geteuid() {
        return Err(State::Unavailable);
    }
    let pid = u32::try_from(credentials.pid.as_raw_pid()).map_err(|_| State::Unavailable)?;
    let process = observe_process(pid).map_err(|_| State::Unavailable)?;
    if process.get("host") != Some(&host) || socket_identity(socket)? != before {
        return Err(State::Unavailable);
    }
    let request = json!({"id":"clauth-resource", "method":"pane.get", "params":{"pane_id":pane}});
    let mut wire = serde_json::to_vec(&request).map_err(|_| State::Unavailable)?;
    wire.push(b'\n');
    write_bounded(&mut stream, &wire, deadline)?;
    let reply = read_bounded(&mut stream, deadline)?;
    let mut identity = pane_identity(&reply, pane)?;
    if Instant::now() >= deadline {
        return Err(State::Unavailable);
    }
    matching_pid_view()?;
    if rustix::net::sockopt::socket_peercred(&stream).map_err(|_| State::Unavailable)?
        != credentials
        || observe_process(pid).map_err(|_| State::Unavailable)? != process
        || process_host()? != host
        || socket_identity(socket)? != before
        || socket_identity(&before.path)? != before
    {
        return Err(State::Unavailable);
    }
    let start = process.get("start_identity").ok_or(State::Unavailable)?;
    let fingerprint = serde_json::to_vec(&json!([
        "herdr-unix-v1",
        credentials.uid.as_raw(),
        pid,
        start,
        host,
        before.dev,
        before.inode
    ]))
    .map_err(|_| State::Unavailable)?;
    identity.insert(
        "instance".into(),
        format!("herdr-unix-v1:{}", hex::encode(Sha256::digest(fingerprint))),
    );
    let path = before.path.to_str().ok_or(State::Unavailable)?.to_owned();
    // This is a socket-local identifier, NOT a claimed Herdr session name.
    identity.insert("session".into(), path.clone());
    identity.insert("socket_path".into(), path);
    if Instant::now() >= deadline {
        return Err(State::Unavailable);
    }
    Ok(identity)
}

fn pane_identity(bytes: &[u8], expected_pane: &str) -> Result<BTreeMap<String, String>, State> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| State::Unavailable)?;
    if value.get("id").and_then(Value::as_str) != Some("clauth-resource")
        || value.get("error").is_some()
        || value.pointer("/result/type").and_then(Value::as_str) != Some("pane_info")
    {
        return Err(State::Unavailable);
    }
    let mut result = BTreeMap::new();
    for key in ["pane_id", "terminal_id", "workspace_id", "tab_id"] {
        let id = value
            .get("result")
            .and_then(|result| result.get("pane"))
            .and_then(|pane| pane.get(key))
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .ok_or(State::Unavailable)?;
        result.insert(key.to_owned(), id.to_owned());
    }
    if result["pane_id"] != expected_pane {
        return Err(State::Mismatch);
    }
    Ok(result)
}

fn pause(deadline: Instant) -> Result<(), State> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(State::Unavailable)?;
    std::thread::sleep(remaining.min(Duration::from_millis(2)));
    Ok(())
}

fn write_bounded(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), State> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err(State::Unavailable);
        }
        match stream.write(bytes) {
            Ok(0) => return Err(State::Unavailable),
            Ok(n) => bytes = &bytes[n..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => pause(deadline)?,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(State::Unavailable),
        }
    }
    Ok(())
}

fn read_bounded(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, State> {
    let mut result = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(State::Unavailable);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(State::Unavailable),
            Ok(n) => {
                if result.len() + n > LIMIT {
                    return Err(State::Unavailable);
                }
                result.extend_from_slice(&chunk[..n]);
                if let Some(newline) = result.iter().position(|b| *b == b'\n') {
                    if result[newline + 1..]
                        .iter()
                        .any(|b| !b.is_ascii_whitespace())
                    {
                        return Err(State::Unavailable);
                    }
                    result.truncate(newline);
                    return Ok(result);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => pause(deadline)?,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(State::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    fn reply() -> Value {
        json!({"id":"clauth-resource", "result":{"type":"pane_info", "pane":{
            "pane_id":"w1:p1", "terminal_id":"terminal-1", "workspace_id":"w1", "tab_id":"w1:t1",
            "title":"secret-title", "token":"secret-token", "argv":["secret-argv"]
        }}})
    }

    fn server(responses: Vec<Vec<u8>>) -> (TempDir, PathBuf, std::thread::JoinHandle<()>) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = std::thread::spawn(move || {
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "fixture connection timed out");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = String::new();
                BufReader::new(&mut stream).read_line(&mut request).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&request).unwrap(),
                    json!({
                        "id":"clauth-resource", "method":"pane.get", "params":{"pane_id":"w1:p1"}
                    })
                );
                // Oversize responses may be disconnected before writing finishes.
                let _ = stream.write_all(&response);
            }
        });
        (dir, path, worker)
    }

    fn wire(value: Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn explicit_socket_roundtrip_whitelists_identity_and_matches_same_peer() {
        let (_dir, path, worker) = server(vec![wire(reply()), wire(reply())]);
        let captured = capture(&path, "w1:p1").unwrap();
        assert_eq!(captured.len(), 7);
        assert_eq!(captured["pane_id"], "w1:p1");
        assert_eq!(captured["terminal_id"], "terminal-1");
        assert_eq!(captured["session"], path.to_str().unwrap());
        assert_eq!(captured["socket_path"], captured["session"]);
        assert!(captured["instance"].starts_with("herdr-unix-v1:"));
        assert!(!format!("{captured:?}").contains("secret"));
        assert_eq!(inspect(&captured).0, State::Matched);
        worker.join().unwrap();
    }

    #[test]
    fn mismatched_id_pane_and_native_errors_fail_without_echoing_content() {
        let mut wrong_id = reply();
        wrong_id["id"] = "secret-wrong-id".into();
        let mut wrong_pane = reply();
        wrong_pane["result"]["pane"]["pane_id"] = "w1:p2".into();
        let mut error = reply();
        error["error"] = json!({"message":"secret-error"});
        for (response, expected) in [
            (wrong_id, State::Unavailable),
            (wrong_pane, State::Mismatch),
            (error, State::Unavailable),
        ] {
            let (_dir, path, worker) = server(vec![wire(response)]);
            let result = capture(&path, "w1:p1");
            assert_eq!(result, Err(expected));
            assert!(!format!("{result:?}").contains("secret"));
            worker.join().unwrap();
        }
    }

    #[test]
    fn every_declared_identity_field_is_compared() {
        for key in ["instance", "terminal_id", "workspace_id", "tab_id"] {
            let (_dir, path, worker) = server(vec![wire(reply()), wire(reply())]);
            let mut expected = capture(&path, "w1:p1").unwrap();
            expected.insert(
                key.into(),
                if key == "instance" {
                    format!("herdr-unix-v1:{}", "0".repeat(64))
                } else {
                    "different".into()
                },
            );
            assert_eq!(inspect(&expected).0, State::Mismatch, "{key}");
            worker.join().unwrap();
        }
    }

    #[test]
    fn incomplete_legacy_identity_does_not_discover_or_connect() {
        let expected = BTreeMap::from([
            ("instance".into(), "local".into()),
            ("session".into(), "main".into()),
            ("workspace_id".into(), "w1".into()),
            ("tab_id".into(), "w1:t1".into()),
            ("pane_id".into(), "w1:p1".into()),
        ]);
        assert_eq!(inspect(&expected).0, State::Unsupported);
        assert_eq!(
            capture(Path::new("relative.sock"), "w1:p1"),
            Err(State::Unavailable)
        );
        let dir = TempDir::new().unwrap();
        let regular = dir.path().join("not-a-socket");
        std::fs::write(&regular, "not a socket").unwrap();
        assert_eq!(capture(&regular, "w1:p1"), Err(State::Unavailable));
    }

    #[test]
    fn oversized_and_truncated_frames_are_rejected() {
        for response in [
            vec![b'x'; LIMIT + 1],
            b"{\"secret\":\"truncated\"}".to_vec(),
        ] {
            let (_dir, path, worker) = server(vec![response]);
            assert_eq!(capture(&path, "w1:p1"), Err(State::Unavailable));
            worker.join().unwrap();
        }
    }

    #[test]
    fn silent_peer_is_bounded_by_total_deadline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("silent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let started = Instant::now();
        assert_eq!(
            capture_with_timeout(&path, "w1:p1", Duration::from_millis(20)),
            Err(State::Unavailable)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(listener);
    }
}
