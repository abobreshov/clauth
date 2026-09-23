//! Public resource observations are read-only evidence, never resource control.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

struct OwnedChild(Child);

impl OwnedChild {
    fn sleeper() -> Self {
        Self(
            Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn stop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }

    fn assert_live(&mut self) {
        assert!(self.0.try_wait().unwrap().is_none());
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn captured(file: &mut File) -> Vec<u8> {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes).unwrap();
    assert!(bytes.len() <= 1_048_576, "CLI output exceeded test budget");
    bytes
}

fn cli(home: &TempDir, args: &[&str]) -> Output {
    // Files avoid a pipe deadlock; a deadline bounds an unexpectedly stuck CLI.
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_clauth"))
            .env("HOME", home.path())
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("HERDR_ENV")
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .args(args)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.stop();
            panic!("resource CLI exceeded test deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Output {
        status,
        stdout: captured(&mut stdout),
        stderr: captured(&mut stderr),
    }
}

fn success(home: &TempDir, args: &[&str]) -> Value {
    let output = cli(home, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn task(home: &TempDir) -> Value {
    let workspace = home.path().join("project");
    fs::create_dir(&workspace).unwrap();
    let input = home.path().join("task-input.json");
    write_json(
        &input,
        &json!({
            "objective":"Observe disposable resource identities",
            "workspace":workspace,
            "constraints":["Do not execute resource descriptions"],
            "source":{"tool":"codex", "model":null, "native_session_id":"resource-source", "account_ref":null}
        }),
    );
    success(
        home,
        &["tasks", "register", "--from", input.to_str().unwrap()],
    )
}

fn identity(home: &TempDir, child: &OwnedChild) -> Value {
    success(
        home,
        &["tasks", "process-identity", &child.0.id().to_string()],
    )
}

fn register(home: &TempDir, task: &Value, resource_id: &str, kind: &str, identity: Value) -> Value {
    let input = home.path().join("resource-input.json");
    write_json(
        &input,
        &json!({
            "id":resource_id,
            "kind":kind,
            "native_identity":identity,
            "purpose":"Disposable identity observation fixture",
            "ownership":"task",
            "disposition":"release",
            "may_write":false,
            "reconnect":format!("touch '{}'", home.path().join("reconnect-ran").display()),
            "cleanup":format!("touch '{}'", home.path().join("cleanup-ran").display())
        }),
    );
    success(
        home,
        &[
            "tasks",
            "resource",
            task["task_id"].as_str().unwrap(),
            "--session",
            "resource-source",
            "--expected-generation",
            &task["generation"].as_u64().unwrap().to_string(),
            "--from",
            input.to_str().unwrap(),
        ],
    )
}

fn record_bytes(home: &TempDir, task: &Value) -> Vec<u8> {
    fs::read(
        home.path()
            .join(".clauth/tasks")
            .join(task["task_id"].as_str().unwrap())
            .join("task.json"),
    )
    .unwrap()
}

fn assert_no_callbacks(home: &TempDir) {
    assert!(!home.path().join("reconnect-ran").exists());
    assert!(!home.path().join("cleanup-ran").exists());
}

fn inspect_unchanged(home: &TempDir, task: &Value, resource_id: &str, expected: &str) -> Value {
    let before = record_bytes(home, task);
    let task_id = task["task_id"].as_str().unwrap();
    let report = success(
        home,
        &[
            "tasks",
            "inspect-resource",
            task_id,
            "--resource",
            resource_id,
        ],
    );
    assert_eq!(report["schema"], 1);
    assert_eq!(report["task_id"], task["task_id"]);
    assert_eq!(report["generation"], task["generation"]);
    assert_eq!(report["resource_revision"], task["resource_revision"]);
    assert_eq!(report["resource_id"], resource_id);
    assert_eq!(report["identity_state"], expected);
    assert_eq!(report["control_verified"], false);
    assert_eq!(report["lifecycle_independence_verified"], false);
    assert_eq!(report["handoff_ready"], false);
    assert!(!report["limitations"].as_array().unwrap().is_empty());
    assert_eq!(
        before,
        record_bytes(home, task),
        "observation mutated durable task history"
    );
    assert_eq!(*task, success(home, &["tasks", "show", task_id]));
    assert_no_callbacks(home);
    report
}

#[test]
fn captured_live_identity_matches_without_journal_mutation_or_resource_control() {
    let home = TempDir::new().unwrap();
    let mut child = OwnedChild::sleeper();
    let captured = identity(&home, &child);
    assert_eq!(captured.as_object().unwrap().len(), 3);
    assert_eq!(captured["pid"], child.0.id().to_string());
    assert!(
        captured["start_identity"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    let host = captured["host"].as_str().unwrap();
    assert!(host.starts_with("linux-v1:"));
    assert_eq!(host.split(':').count(), 6);
    assert!(
        !home.path().join(".clauth/tasks").exists(),
        "capture must not register a task"
    );
    let registered = register(
        &home,
        &task(&home),
        "held-process",
        "process",
        captured.clone(),
    );
    assert_eq!(registered["resources"][0]["native_identity"], captured);
    let report = inspect_unchanged(&home, &registered, "held-process", "matched");
    assert_eq!(report["disposition"], "release");
    child.assert_live();
    inspect_unchanged(&home, &registered, "held-process", "matched");
    child.assert_live();
}

#[test]
fn wrong_start_and_foreign_versioned_host_are_mismatches() {
    let home = TempDir::new().unwrap();
    let mut child = OwnedChild::sleeper();
    let captured = identity(&home, &child);
    let mut wrong_start = captured.clone();
    let ticks = captured["start_identity"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    wrong_start["start_identity"] = json!(ticks.checked_add(1).unwrap().to_string());
    let registered = register(&home, &task(&home), "wrong-start", "process", wrong_start);
    inspect_unchanged(&home, &registered, "wrong-start", "mismatch");

    let mut wrong_host = captured.clone();
    let mut fields: Vec<String> = captured["host"]
        .as_str()
        .unwrap()
        .split(':')
        .map(str::to_owned)
        .collect();
    let replacement = if fields[1].starts_with('0') { "1" } else { "0" };
    fields[1].replace_range(0..1, replacement);
    wrong_host["host"] = json!(fields.join(":"));
    let registered = register(&home, &registered, "wrong-host", "process", wrong_host);
    inspect_unchanged(&home, &registered, "wrong-host", "mismatch");
    child.assert_live();
}

#[test]
fn legacy_host_is_unsupported_instead_of_assuming_local_identity() {
    let home = TempDir::new().unwrap();
    let mut child = OwnedChild::sleeper();
    let mut captured = identity(&home, &child);
    captured["host"] = json!("legacy-workstation-description");
    let registered = register(&home, &task(&home), "legacy", "process", captured);
    inspect_unchanged(&home, &registered, "legacy", "unsupported");
    child.assert_live();
}

#[test]
fn exited_fixture_process_is_missing_without_changing_declaration() {
    let home = TempDir::new().unwrap();
    let mut child = OwnedChild::sleeper();
    let captured = identity(&home, &child);
    let registered = register(&home, &task(&home), "exited", "process", captured);
    child.stop();
    inspect_unchanged(&home, &registered, "exited", "missing");
    let capture = cli(
        &home,
        &["tasks", "process-identity", &child.0.id().to_string()],
    );
    assert!(!capture.status.success());
    assert!(capture.stdout.is_empty());
}

#[test]
fn external_and_unregistered_resources_do_not_execute_descriptions() {
    let home = TempDir::new().unwrap();
    let registered = register(
        &home,
        &task(&home),
        "external",
        "external",
        json!({"uri":"https://example.invalid/disposable-resource"}),
    );
    inspect_unchanged(&home, &registered, "external", "unsupported");
    let before = record_bytes(&home, &registered);
    let result = cli(
        &home,
        &[
            "tasks",
            "inspect-resource",
            registered["task_id"].as_str().unwrap(),
            "--resource",
            "never-registered",
        ],
    );
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not registered"));
    assert_eq!(before, record_bytes(&home, &registered));
    assert_no_callbacks(&home);
}

#[test]
fn process_identity_rejects_zero_malformed_and_out_of_range_pids() {
    let home = TempDir::new().unwrap();
    for pid in ["0", "not-a-pid", "-1", "4294967296"] {
        let result = cli(&home, &["tasks", "process-identity", pid]);
        assert!(
            !result.status.success(),
            "invalid PID unexpectedly accepted"
        );
        assert!(result.stdout.is_empty());
    }
    assert!(!home.path().join(".clauth/tasks").exists());
}

struct FakeHerdr {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<Vec<Value>, &'static str>>>,
}

impl FakeHerdr {
    fn new(home: &TempDir, responses: Vec<Value>) -> Self {
        let socket = home.path().join("herdr-fixture.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut requests = Vec::new();
            for response in responses {
                let mut stream = loop {
                    if stopping.load(Ordering::SeqCst) || Instant::now() >= deadline {
                        return Err("fake Herdr accept stopped or timed out");
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return Err("fake Herdr accept failed"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                requests.push(read_herdr_request(&mut stream, &stopping)?);
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                stream
                    .write_all(&bytes)
                    .map_err(|_| "fake Herdr response failed")?;
            }
            Ok(requests)
        });
        Self {
            socket,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(mut self, expected_connections: usize) {
        let requests = self.worker.take().unwrap().join().unwrap().unwrap();
        assert_eq!(requests.len(), expected_connections);
        for request in requests {
            assert_eq!(
                request,
                json!({"id":"clauth-resource", "method":"pane.get", "params":{"pane_id":"w-test:p1"}})
            );
        }
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            // Accept/read/write loops all have short timeouts; no leaked worker.
            let _ = worker.join();
        }
    }
}

fn read_herdr_request(stream: &mut UnixStream, stop: &AtomicBool) -> Result<Value, &'static str> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return Err("fake Herdr read stopped or timed out");
        }
        let mut chunk = [0; 512];
        match stream.read(&mut chunk) {
            Ok(0) => return Err("fake Herdr request ended before newline"),
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return Err("fake Herdr request read failed"),
        }
        if bytes.len() > 16_384 {
            return Err("fake Herdr request exceeded limit");
        }
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            if newline + 1 != bytes.len() {
                return Err("unexpected extra Herdr request bytes");
            }
            return serde_json::from_slice(&bytes[..newline])
                .map_err(|_| "fake Herdr request malformed");
        }
    }
}

fn pane_response() -> Value {
    json!({"id":"clauth-resource", "result":{"type":"pane_info", "pane":{
        "pane_id":"w-test:p1", "terminal_id":"term-test", "workspace_id":"w-test", "tab_id":"w-test:t1",
        "tokens":{"secret":"must-not-leak"}, "terminal_title":"must-not-leak"
    }}})
}

fn capture_herdr(home: &TempDir, socket: &Path) -> Value {
    let output = cli(
        home,
        &[
            "tasks",
            "herdr-identity",
            "--socket",
            socket.to_str().unwrap(),
            "--pane",
            "w-test:p1",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("must-not-leak"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("must-not-leak"));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn herdr_capture_whitelists_exact_pane_and_inspection_preserves_task() {
    let home = TempDir::new().unwrap();
    let fixture = FakeHerdr::new(&home, vec![pane_response(), pane_response()]);
    let captured = capture_herdr(&home, &fixture.socket);
    let mut keys: Vec<_> = captured
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "instance",
            "pane_id",
            "session",
            "socket_path",
            "tab_id",
            "terminal_id",
            "workspace_id"
        ]
    );
    assert_eq!(captured["pane_id"], "w-test:p1");
    assert_eq!(captured["terminal_id"], "term-test");
    assert_eq!(captured["workspace_id"], "w-test");
    assert_eq!(captured["tab_id"], "w-test:t1");
    let canonical = fixture.socket.canonicalize().unwrap();
    assert_eq!(captured["session"], canonical.to_str().unwrap());
    assert_eq!(captured["socket_path"], canonical.to_str().unwrap());
    let instance = captured["instance"].as_str().unwrap();
    let digest = instance.strip_prefix("herdr-unix-v1:").unwrap();
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert!(!home.path().join(".clauth/tasks").exists());
    let registered = register(&home, &task(&home), "herdr-pane", "herdr_pane", captured);
    let report = inspect_unchanged(&home, &registered, "herdr-pane", "matched");
    assert!(!report.to_string().contains("must-not-leak"));
    assert!(!String::from_utf8_lossy(&record_bytes(&home, &registered)).contains("must-not-leak"));
    fixture.finish(2);
}

#[test]
fn herdr_changed_terminal_mismatches_even_when_pane_id_is_reused() {
    let home = TempDir::new().unwrap();
    let mut replaced = pane_response();
    replaced["result"]["pane"]["terminal_id"] = json!("replacement-terminal");
    let fixture = FakeHerdr::new(&home, vec![pane_response(), replaced]);
    let captured = capture_herdr(&home, &fixture.socket);
    let registered = register(&home, &task(&home), "herdr-pane", "herdr_pane", captured);
    inspect_unchanged(&home, &registered, "herdr-pane", "mismatch");
    fixture.finish(2);
}

#[test]
fn herdr_errors_missing_identity_and_cross_pane_receipts_cannot_match() {
    let mut incomplete = pane_response();
    incomplete["result"]["pane"]
        .as_object_mut()
        .unwrap()
        .remove("terminal_id");
    let mut wrong_pane = pane_response();
    wrong_pane["result"]["pane"]["pane_id"] = json!("w-test:p2");
    for response in [
        json!({"id":"clauth-resource", "error":{"message":"must-not-leak"}}),
        incomplete,
        wrong_pane,
    ] {
        let home = TempDir::new().unwrap();
        let fixture = FakeHerdr::new(&home, vec![pane_response(), response]);
        let captured = capture_herdr(&home, &fixture.socket);
        let registered = register(&home, &task(&home), "herdr-pane", "herdr_pane", captured);
        let before = record_bytes(&home, &registered);
        let output = cli(
            &home,
            &[
                "tasks",
                "inspect-resource",
                registered["task_id"].as_str().unwrap(),
                "--resource",
                "herdr-pane",
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("must-not-leak"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("must-not-leak"));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_ne!(report["identity_state"], "matched");
        assert!(matches!(
            report["identity_state"].as_str(),
            Some("mismatch" | "missing" | "unavailable" | "unsupported")
        ));
        for field in [
            "control_verified",
            "lifecycle_independence_verified",
            "handoff_ready",
        ] {
            assert_eq!(report[field], false);
        }
        assert_eq!(report["generation"], registered["generation"]);
        assert_eq!(report["resource_revision"], registered["resource_revision"]);
        assert_eq!(before, record_bytes(&home, &registered));
        assert_no_callbacks(&home);
        fixture.finish(2);
    }
}
