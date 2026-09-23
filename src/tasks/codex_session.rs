//! No-prompt observation of an existing Codex thread through a private stdio
//! app-server. Read-only thread settings are not a host/MCP write fence.
//! Only the direct child is reaped here; the caller must supervise its scope.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::native_session::{CodexSessionMetadata, SessionObservation};

const MAX_FRAME: usize = 262_144;
const MAX_TOTAL: usize = 4 * 1024 * 1024;
const MAX_FRAMES: usize = 256;
const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(20);

/// Resume only an existing thread; never create a thread or start a turn.
/// The callback must durably persist the observation and return promptly.
/// Its failure does not imply that no partial durable write happened.
pub(crate) fn bind_existing(
    program: &Path,
    args: &[String],
    cwd: &Path,
    session_id: &str,
    persist: impl FnOnce(SessionObservation) -> Result<()>,
) -> Result<()> {
    let requested_model = validate_launch(program, args, cwd, session_id)?;
    let deadline = Instant::now() + PROTOCOL_TIMEOUT;
    let child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| anyhow!("could not start Codex session observer"))?;
    let mut child = NativeChild(child);
    let result = (|| {
        let mut protocol = Protocol::new(&mut child.0, deadline)?;
        let initialized = protocol.request(initialize_request(), 1)?;
        validate_initialize(&initialized)?;
        protocol.write(&json!({"method":"initialized", "params":{}}))?;
        let resumed = protocol.request(
            resume_request(cwd, session_id, requested_model.as_deref())?,
            2,
        )?;
        let observed = observation(
            &resumed,
            cwd,
            session_id,
            requested_model.as_deref(),
            child.0.id(),
        )?;
        if child
            .0
            .try_wait()
            .map_err(|_| anyhow!("Codex observer liveness could not be checked"))?
            .is_some()
        {
            bail!("Codex observer exited before observation was persisted");
        }
        // Checked-live direct process, not an atomic lifetime or ownership lease.
        // No turn or additional request is sent while persistence runs.
        persist(observed)
            .map_err(|_| anyhow!("Codex session observation could not be persisted"))?;
        drop(protocol);
        Ok(())
    })();
    let cleanup = child.close();
    result.and(cleanup)
}

fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

fn validate_launch(
    program: &Path,
    args: &[String],
    cwd: &Path,
    session: &str,
) -> Result<Option<String>> {
    let base_matches = matches!(args.get(..3), Some([command, listen, transport])
        if command == "app-server" && listen == "--listen" && transport == "stdio://");
    let model = match args {
        [_, _, _] if base_matches => None,
        [_, _, _, flag, config] if base_matches && flag == "--config" => {
            let encoded = config
                .strip_prefix("model=")
                .ok_or_else(|| anyhow!("invalid Codex observer model override"))?;
            let model: String = serde_json::from_str(encoded)
                .map_err(|_| anyhow!("invalid Codex observer model override"))?;
            if !safe_label(&model)
                || serde_json::to_string(&model)
                    .map_err(|_| anyhow!("invalid Codex observer model override"))?
                    != encoded
            {
                bail!("invalid Codex observer model override");
            }
            Some(model)
        }
        _ => bail!("invalid Codex session observer arguments"),
    };
    let cwd_text = cwd.to_str().unwrap_or("");
    if !program.is_absolute()
        || !safe_label(session)
        || !cwd.is_absolute()
        || cwd_text.is_empty()
        || cwd_text.len() > 4096
        || cwd_text.chars().any(char::is_control)
        || !cwd.is_dir()
    {
        bail!("invalid Codex session observer configuration");
    }
    Ok(model)
}

fn initialize_request() -> Value {
    json!({
        "id":1, "method":"initialize",
        "params": {
            "clientInfo":{"name":"clauth-session-observer", "version":"1"},
            "capabilities":{"experimentalApi":false}
        }
    })
}

fn validate_initialize(result: &Value) -> Result<()> {
    if !result
        .get("userAgent")
        .and_then(Value::as_str)
        .is_some_and(|agent| {
            !agent.is_empty() && agent.len() <= 1024 && !agent.chars().any(char::is_control)
        })
    {
        bail!("invalid Codex initialization response");
    }
    Ok(())
}

fn resume_request(cwd: &Path, thread: &str, model: Option<&str>) -> Result<Value> {
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow!("invalid Codex observer workspace"))?;
    let mut params = json!({
        "threadId":thread, "cwd":cwd, "excludeTurns":true,
        "approvalPolicy":"never", "sandbox":"read-only"
    });
    if let Some(model) = model {
        params["model"] = model.into();
    }
    Ok(json!({"id":2, "method":"thread/resume", "params":params}))
}

fn observation(
    result: &Value,
    cwd: &Path,
    expected_thread: &str,
    requested_model: Option<&str>,
    native_pid: u32,
) -> Result<SessionObservation> {
    if !safe_label(expected_thread)
        || native_pid == 0
        || requested_model.is_some_and(|model| !safe_label(model))
    {
        bail!("invalid Codex session observation");
    }
    let thread = result
        .get("thread")
        .filter(|thread| thread.is_object())
        .ok_or_else(|| anyhow!("invalid Codex thread response"))?;
    if thread.get("id").and_then(Value::as_str) != Some(expected_thread)
        || result.get("cwd").and_then(Value::as_str) != cwd.to_str()
        || thread.get("cwd").and_then(Value::as_str) != cwd.to_str()
    {
        bail!("Codex thread or workspace identity did not match");
    }
    let label = |value: Option<&Value>| -> Result<String> {
        value
            .and_then(Value::as_str)
            .filter(|value| safe_label(value))
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("invalid Codex thread metadata"))
    };
    let session_tree_id = label(thread.get("sessionId"))?;
    let model = label(result.get("model"))?;
    let provider = label(result.get("modelProvider"))?;
    if thread.get("modelProvider").and_then(Value::as_str) != Some(provider.as_str())
        || thread
            .get("model")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(model.as_str()))
    {
        bail!("Codex model metadata disagreed");
    }
    // Installed 0.155.1 request uses kebab-case; returned SandboxPolicy uses
    // camelCase. Do not confuse read-only filesystem policy with a network or
    // whole-process fence: startup services remain outside that guarantee.
    if result.get("approvalPolicy").and_then(Value::as_str) != Some("never")
        || result.pointer("/sandbox/type").and_then(Value::as_str) != Some("readOnly")
    {
        bail!("Codex did not confirm the requested thread permission policy");
    }
    Ok(SessionObservation {
        native_session_id: expected_thread.to_owned(),
        configured_model: Some(model),
        requested_model: requested_model.map(str::to_owned),
        native_pid,
        request_id: Some(2),
        codex: Some(CodexSessionMetadata {
            session_tree_id,
            model_provider: provider,
        }),
    })
}

enum Incoming {
    Result(Value),
    Deny(Value),
    Notification,
}

fn incoming(frame: &[u8], expected_id: u64) -> Result<Incoming> {
    let message: Value =
        serde_json::from_slice(frame).map_err(|_| anyhow!("invalid Codex protocol frame"))?;
    // Installed app-server omits this field, unlike ACP.
    if !message.is_object() || message.get("jsonrpc").is_some() {
        bail!("invalid Codex protocol envelope");
    }
    if let Some(method) = message.get("method") {
        if !method.as_str().is_some_and(|method| {
            !method.is_empty() && method.len() <= 256 && !method.chars().any(char::is_control)
        }) || message.get("result").is_some()
            || message.get("error").is_some()
        {
            bail!("invalid Codex host request");
        }
        return match message.get("id") {
            None => Ok(Incoming::Notification),
            Some(id) if id.as_i64().is_some() || id.as_str().is_some_and(safe_label) => {
                Ok(Incoming::Deny(json!({
                    "id":id, "error":{"code":-32601,"message":"Host operations are disabled"}
                })))
            }
            _ => bail!("invalid Codex host request identity"),
        };
    }
    if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
        bail!("Codex protocol response identity did not match");
    }
    if message.get("error").is_some() {
        bail!("Codex protocol request was rejected");
    }
    let result = message
        .get("result")
        .filter(|value| value.is_object())
        .ok_or_else(|| anyhow!("invalid Codex protocol result"))?;
    Ok(Incoming::Result(result.clone()))
}

type IoResult<T> = std::result::Result<T, ()>;

struct Protocol {
    input: SyncSender<Vec<u8>>,
    written: Receiver<IoResult<()>>,
    output: Receiver<IoResult<Vec<u8>>>,
    deadline: Instant,
    frames: usize,
    bytes: usize,
}

impl Protocol {
    fn new(child: &mut Child, deadline: Instant) -> Result<Self> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Codex protocol output unavailable"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Codex protocol input unavailable"))?;
        let (frames_tx, output) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("codex-session-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let frame = read_frame(&mut reader);
                    let failed = frame.is_err();
                    if frames_tx.send(frame).is_err() || failed {
                        break;
                    }
                }
            })
            .map_err(|_| anyhow!("Codex protocol reader unavailable"))?;
        let (input, requests) = mpsc::sync_channel::<Vec<u8>>(1);
        let (written_tx, written) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("codex-session-write".into())
            .spawn(move || {
                while let Ok(bytes) = requests.recv() {
                    let result = stdin
                        .write_all(&bytes)
                        .and_then(|_| stdin.flush())
                        .map_err(|_| ());
                    let failed = result.is_err();
                    if written_tx.send(result).is_err() || failed {
                        break;
                    }
                }
            })
            .map_err(|_| anyhow!("Codex protocol writer unavailable"))?;
        Ok(Self {
            input,
            written,
            output,
            deadline,
            frames: 0,
            bytes: 0,
        })
    }

    fn remaining(&self) -> Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("Codex session observation timed out"))
    }

    fn write(&self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|_| anyhow!("Codex protocol request could not be encoded"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME {
            bail!("Codex protocol request exceeds size limit");
        }
        self.remaining()?;
        // Every write waits for acknowledgement, so the bounded queue is empty.
        self.input
            .try_send(bytes)
            .map_err(|_| anyhow!("Codex protocol input unavailable"))?;
        self.written
            .recv_timeout(self.remaining()?)
            .map_err(|_| anyhow!("Codex protocol write failed or timed out"))?
            .map_err(|_| anyhow!("Codex protocol input failed"))
    }

    fn request(&mut self, message: Value, expected_id: u64) -> Result<Value> {
        self.write(&message)?;
        loop {
            let frame = self
                .output
                .recv_timeout(self.remaining()?)
                .map_err(|_| anyhow!("Codex protocol response unavailable or timed out"))?
                .map_err(|_| anyhow!("Codex protocol frame unavailable or oversized"))?;
            self.accept_frame_size(frame.len())?;
            match incoming(&frame, expected_id)? {
                Incoming::Result(value) => return Ok(value),
                Incoming::Deny(value) => self.write(&value)?,
                Incoming::Notification => {}
            }
        }
    }

    fn accept_frame_size(&mut self, size: usize) -> Result<()> {
        self.frames += 1;
        self.bytes += size;
        if self.frames > MAX_FRAMES || self.bytes > MAX_TOTAL {
            bail!("Codex protocol observation exceeds limits");
        }
        Ok(())
    }
}

fn read_frame(reader: &mut impl BufRead) -> IoResult<Vec<u8>> {
    let mut frame = Vec::new();
    reader
        .take((MAX_FRAME + 1) as u64)
        .read_until(b'\n', &mut frame)
        .map_err(|_| ())?;
    if frame.len() > MAX_FRAME || frame.last() != Some(&b'\n') {
        return Err(());
    }
    Ok(frame)
}

struct NativeChild(Child);

impl NativeChild {
    fn wait_until(&mut self, deadline: Instant) -> Result<bool> {
        loop {
            if self
                .0
                .try_wait()
                .map_err(|_| anyhow!("Codex observer exit could not be checked"))?
                .is_some()
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn close(&mut self) -> Result<()> {
        self.0.stdin.take();
        if self.wait_until(Instant::now() + Duration::from_secs(1))? {
            return Ok(());
        }
        self.0
            .kill()
            .map_err(|_| anyhow!("Codex observer could not be stopped"))?;
        if !self.wait_until(Instant::now() + Duration::from_secs(2))? {
            bail!("Codex observer exit remains unconfirmed");
        }
        Ok(())
    }
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        // Never join pipe threads: descendants can retain descriptors. Killing
        // this owned handle does not target a PID recovered from a stored record.
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.try_wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    fn args(model: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = ["app-server", "--listen", "stdio://"]
            .map(str::to_owned)
            .into();
        if let Some(model) = model {
            args.extend([
                "--config".into(),
                format!("model={}", serde_json::to_string(model).unwrap()),
            ]);
        }
        args
    }

    fn resumed(cwd: &Path) -> Value {
        json!({
            "thread":{"id":"fixture-thread", "sessionId":"different-tree-root",
                "cwd":cwd,"modelProvider":"openai","model":"fixture-resolved-model"},
            "model":"fixture-resolved-model", "modelProvider":"openai", "cwd":cwd,
            "approvalPolicy":"never", "sandbox":{"type":"readOnly","networkAccess":false}
        })
    }

    fn fake_native(dir: &Path, result: Value) -> std::path::PathBuf {
        let program = dir.join("native-fixture");
        let encoded = serde_json::to_string(&json!({"id":2,"result":result}))
            .unwrap()
            .replace('\'', "'\\''");
        fs::write(&program, format!(concat!(
            "#!/bin/sh\n",
            "IFS= read -r first || exit 1\nprintf '%s\\n' \"$first\" > requests\n",
            "printf '%s\\n' '{{\"id\":1,\"result\":{{\"userAgent\":\"codex-fixture\"}}}}'\n",
            "IFS= read -r second || exit 1\nprintf '%s\\n' \"$second\" >> requests\n",
            "IFS= read -r third || exit 1\nprintf '%s\\n' \"$third\" >> requests\n",
            "printf '%s\\n' '{}'\n",
            "if IFS= read -r unexpected; then printf '%s' \"$unexpected\" > unexpected-input; exit 42; fi\n",
            ": > eof\n"
        ), encoded)).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    #[test]
    fn canonical_command_rejects_transport_prompt_and_config_injection() {
        let dir = tempfile::tempdir().unwrap();
        let program = Path::new("/fixture/codex");
        assert_eq!(
            validate_launch(program, &args(None), dir.path(), "fixture-thread").unwrap(),
            None
        );
        assert_eq!(
            validate_launch(
                program,
                &args(Some("fixture-model")),
                dir.path(),
                "fixture-thread"
            )
            .unwrap()
            .as_deref(),
            Some("fixture-model")
        );
        for config in [
            "model=unquoted",
            "model=\"fixture-model\" ",
            "model=\"bad\\nmodel\"",
            "model=\"fixture\\u002dmodel\"",
            "sandbox_mode=\"danger-full-access\"",
            "model=\"fixture-model\"\nother=true",
        ] {
            let bad = ["app-server", "--listen", "stdio://", "--config", config].map(str::to_owned);
            let error = validate_launch(program, &bad, dir.path(), "fixture-thread").unwrap_err();
            assert!(!error.to_string().contains(config));
        }
        for bad in [
            vec!["app-server", "proxy"],
            vec!["app-server", "--listen", "unix://"],
            vec!["app-server", "--listen", "stdio://", "--prompt", "secret"],
        ] {
            assert!(
                validate_launch(
                    program,
                    &bad.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                    dir.path(),
                    "fixture-thread"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn request_sequence_only_initializes_and_resumes_without_history_or_turns() {
        let init = initialize_request();
        assert_eq!(init["id"], 1);
        assert_eq!(init["method"], "initialize");
        assert!(init.get("jsonrpc").is_none());
        let resume = resume_request(
            Path::new("/fixture"),
            "fixture-thread",
            Some("fixture-model"),
        )
        .unwrap();
        assert_eq!(
            resume,
            json!({"id":2,"method":"thread/resume","params":{
                "threadId":"fixture-thread","cwd":"/fixture","excludeTurns":true,
                "approvalPolicy":"never","sandbox":"read-only","model":"fixture-model"
            }})
        );
    }

    #[test]
    fn keeps_thread_tree_and_requested_configured_models_distinct() {
        let cwd = Path::new("/fixture");
        let observed = observation(
            &resumed(cwd),
            cwd,
            "fixture-thread",
            Some("fixture-requested-alias"),
            123,
        )
        .unwrap();
        assert_eq!(observed.native_session_id, "fixture-thread");
        assert_eq!(
            observed.configured_model.as_deref(),
            Some("fixture-resolved-model")
        );
        assert_eq!(
            observed.requested_model.as_deref(),
            Some("fixture-requested-alias")
        );
        assert_eq!(observed.request_id, Some(2));
        let codex = observed.codex.unwrap();
        assert_eq!(codex.session_tree_id, "different-tree-root");
        assert_eq!(codex.model_provider, "openai");
    }

    #[test]
    fn rejects_wrong_identity_workspace_model_provider_and_permissions() {
        let cwd = Path::new("/fixture");
        for (pointer, value) in [
            ("/thread/id", json!("wrong")),
            ("/thread/sessionId", json!(null)),
            ("/thread/sessionId", json!("bad\nprivate")),
            ("/cwd", json!("/elsewhere")),
            ("/thread/cwd", json!("/elsewhere")),
            ("/model", json!(null)),
            ("/model", json!("")),
            ("/modelProvider", json!("private\nprovider")),
            ("/thread/modelProvider", json!("other")),
            ("/thread/model", json!("other")),
            ("/thread/model", json!(false)),
            ("/approvalPolicy", json!("on-request")),
            ("/sandbox/type", json!("workspaceWrite")),
        ] {
            let mut fixture = resumed(cwd);
            *fixture.pointer_mut(pointer).unwrap() = value;
            assert!(
                observation(&fixture, cwd, "fixture-thread", None, 123).is_err(),
                "{pointer}"
            );
        }
        let mut fixture = resumed(cwd);
        fixture["thread"]["model"] = Value::Null;
        assert!(observation(&fixture, cwd, "fixture-thread", None, 123).is_ok());
        fixture["thread"].as_object_mut().unwrap().remove("model");
        assert!(observation(&fixture, cwd, "fixture-thread", None, 123).is_ok());
    }

    #[test]
    fn correlates_responses_and_redacts_native_errors() {
        for frame in [
            json!({"id":1,"result":{}}),
            json!({"id":"2","result":{}}),
            json!({"id":2,"error":{"message":"private-secret"}}),
            json!({"id":2,"result":{},"error":{"message":"private-secret"}}),
            json!({"jsonrpc":"2.0","id":2,"result":{}}),
        ] {
            let error = incoming(&serde_json::to_vec(&frame).unwrap(), 2)
                .err()
                .unwrap();
            assert!(!error.to_string().contains("private-secret"));
        }
        assert!(matches!(
            incoming(br#"{"id":2,"result":{}}"#, 2).unwrap(),
            Incoming::Result(_)
        ));
    }

    #[test]
    fn unsolicited_requests_are_denied_without_echoing_content() {
        let frame = json!({"id":"callback-1", "method":"item/commandExecution/requestApproval", "params":{"command":"private-secret"}});
        let Incoming::Deny(reply) = incoming(&serde_json::to_vec(&frame).unwrap(), 2).unwrap()
        else {
            panic!("expected denial");
        };
        assert_eq!(reply["id"], "callback-1");
        assert_eq!(reply["error"]["code"], -32601);
        assert!(reply.get("jsonrpc").is_none());
        assert!(!reply.to_string().contains("private-secret"));
        assert!(matches!(
            incoming(br#"{"method":"thread/started","params":{}}"#, 2).unwrap(),
            Incoming::Notification
        ));
    }

    #[test]
    fn initialization_is_bounded_and_framing_requires_complete_lines() {
        assert!(validate_initialize(&json!({"userAgent":"codex-fixture"})).is_ok());
        for agent in [
            json!(null),
            json!(false),
            json!(""),
            json!("bad\nagent"),
            json!("x".repeat(1025)),
        ] {
            assert!(validate_initialize(&json!({"userAgent":agent})).is_err());
        }
        assert!(read_frame(&mut Cursor::new(b"partial")).is_err());
        assert!(read_frame(&mut Cursor::new(vec![b'x'; MAX_FRAME + 1])).is_err());
        assert!(read_frame(&mut Cursor::new(b"{}\n")).is_ok());
    }

    #[test]
    fn aggregate_frames_bytes_and_deadline_are_bounded() {
        let (input, _) = mpsc::sync_channel(1);
        let (_, written) = mpsc::sync_channel(1);
        let (_, output) = mpsc::sync_channel(1);
        let mut protocol = Protocol {
            input,
            written,
            output,
            deadline: Instant::now(),
            frames: MAX_FRAMES,
            bytes: 0,
        };
        assert!(protocol.accept_frame_size(1).is_err());
        protocol.frames = 0;
        protocol.bytes = MAX_TOTAL;
        assert!(protocol.accept_frame_size(1).is_err());
        protocol.deadline = Instant::now() - Duration::from_secs(1);
        assert!(protocol.remaining().is_err());
    }

    #[test]
    fn real_pipe_exchange_persists_live_receipt_without_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_native(dir.path(), resumed(dir.path()));
        let mut receipt = None;
        bind_existing(
            &program,
            &args(None),
            dir.path(),
            "fixture-thread",
            |observed| {
                assert!(Path::new(&format!("/proc/{}", observed.native_pid)).exists());
                assert!(!dir.path().join("eof").exists());
                receipt = Some(observed);
                Ok(())
            },
        )
        .unwrap();
        let receipt = receipt.unwrap();
        assert!(!Path::new(&format!("/proc/{}", receipt.native_pid)).exists());
        assert!(dir.path().join("eof").exists());
        assert!(!dir.path().join("unexpected-input").exists());
        let requests = fs::read_to_string(dir.path().join("requests")).unwrap();
        let requests: Vec<Value> = requests
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(requests[1], json!({"method":"initialized","params":{}}));
        assert_eq!(requests[2]["method"], "thread/resume");
        assert_eq!(requests[2]["params"]["threadId"], "fixture-thread");
        assert!(
            requests
                .iter()
                .all(|request| request.get("jsonrpc").is_none())
        );
    }

    #[test]
    fn wrong_receipt_never_persists_and_callback_failure_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let mut fixture = resumed(dir.path());
        fixture["thread"]["id"] = "wrong-thread".into();
        let program = fake_native(dir.path(), fixture);
        let mut persisted = false;
        assert!(
            bind_existing(&program, &args(None), dir.path(), "fixture-thread", |_| {
                persisted = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!persisted);
        let program = fake_native(dir.path(), resumed(dir.path()));
        let mut pid = None;
        let error = bind_existing(
            &program,
            &args(None),
            dir.path(),
            "fixture-thread",
            |observed| {
                pid = Some(observed.native_pid);
                bail!("private-secret callback failure")
            },
        )
        .unwrap_err();
        assert!(!error.to_string().contains("private-secret"));
        assert!(!Path::new(&format!("/proc/{}", pid.unwrap())).exists());
        assert!(!dir.path().join("unexpected-input").exists());
    }
}
