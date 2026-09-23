//! No-prompt native-session observation, not task ownership or account binding.
//! Only the direct child is reaped here; the caller must supervise its scope.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant};

use super::native_session::SessionObservation;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

// Grok 1.0.40 emits session/update history frames larger than 64 KiB even
// when no user turn was sent (114,846 bytes observed in the native fixture).
// Keep a finite per-frame bound as well as the aggregate byte/frame budget.
const MAX_FRAME: usize = 262_144;
const MAX_TOTAL: usize = 4 * 1024 * 1024;
const MAX_FRAMES: usize = 256;
const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(20);

/// Load an existing session without sending a prompt. The callback must durably
/// persist the observation before returning. It must not block indefinitely.
/// A callback failure does not imply that no partial durable write happened.
pub(crate) fn bind_existing(
    program: &Path,
    args: &[String],
    cwd: &Path,
    session_id: &str,
    persist: impl FnOnce(SessionObservation) -> Result<()>,
) -> Result<()> {
    validate_launch(program, args, cwd, session_id)?;
    let deadline = Instant::now() + PROTOCOL_TIMEOUT;
    let child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| anyhow!("could not start native session observer"))?;
    let mut child = NativeChild(child);
    let result = (|| {
        let mut protocol = Protocol::new(&mut child.0, deadline)?;
        let initialized = protocol.request(initialize_request(), 1)?;
        validate_initialize(&initialized)?;
        let loaded = protocol.request(load_request(cwd, session_id)?, 2)?;
        let observation = observation(&loaded, session_id, child.0.id())?;
        if child
            .0
            .try_wait()
            .map_err(|_| anyhow!("native observer liveness could not be checked"))?
            .is_some()
        {
            bail!("native observer exited before session observation was persisted");
        }
        // This is a checked-live direct process, not an atomic lifetime lease.
        // No stdin message is sent after session/load, including during persist.
        persist(observation)
            .map_err(|_| anyhow!("native session observation could not be persisted"))?;
        drop(protocol); // Writer closes stdin once its empty queue disconnects.
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

fn validate_launch(program: &Path, args: &[String], cwd: &Path, session: &str) -> Result<()> {
    let canonical_args = matches!(args, [agent, leader, transport]
        if agent == "agent" && leader == "--no-leader" && transport == "stdio")
        || matches!(args, [agent, leader, model_flag, model, transport]
        if agent == "agent" && leader == "--no-leader" && model_flag == "--model"
            && safe_label(model) && transport == "stdio");
    let cwd_text = cwd.to_str().unwrap_or("");
    if !program.is_absolute()
        || !canonical_args
        || !safe_label(session)
        || !cwd.is_absolute()
        || cwd_text.is_empty()
        || cwd_text.len() > 4096
        || cwd_text.chars().any(char::is_control)
        || !cwd.is_dir()
    {
        bail!("invalid native session observer configuration");
    }
    Ok(())
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            },
            "clientInfo": {"name": "clauth-session-observer", "version": "1"}
        }
    })
}

fn load_request(cwd: &Path, session: &str) -> Result<Value> {
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow!("invalid native observer workspace"))?;
    Ok(json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/load",
        "params": {"cwd": cwd, "sessionId": session, "mcpServers": []}
    }))
}

fn validate_initialize(result: &Value) -> Result<()> {
    if result.get("protocolVersion").and_then(Value::as_u64) != Some(1)
        || result
            .pointer("/agentCapabilities/loadSession")
            .and_then(Value::as_bool)
            != Some(true)
    {
        bail!("native observer does not support the required session protocol");
    }
    Ok(())
}

fn observation(result: &Value, session: &str, native_pid: u32) -> Result<SessionObservation> {
    if !result.is_object() || !safe_label(session) || native_pid == 0 {
        bail!("invalid native session observation");
    }
    if let Some(returned) = result.get("sessionId")
        && returned.as_str() != Some(session)
    {
        bail!("native session response identity did not match");
    }
    let mut model = None;
    if let Some(options) = result.get("configOptions") {
        let options = options
            .as_array()
            .ok_or_else(|| anyhow!("invalid native model configuration"))?;
        for option in options {
            let id = option
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("invalid native model configuration"))?;
            if id == "model" {
                let value = option
                    .get("currentValue")
                    .and_then(Value::as_str)
                    .filter(|value| safe_label(value))
                    .ok_or_else(|| anyhow!("invalid native model configuration"))?;
                if model.replace(value.to_owned()).is_some() {
                    bail!("ambiguous native model configuration");
                }
            }
        }
    }
    if let Some(models) = result.get("models") {
        if !models.is_object() {
            bail!("invalid native model configuration");
        }
        if let Some(current) = models.get("currentModelId") {
            let current = current
                .as_str()
                .filter(|value| safe_label(value))
                .ok_or_else(|| anyhow!("invalid native model configuration"))?;
            if model.as_deref().is_some_and(|model| model != current) {
                bail!("native model configuration disagreed");
            }
            if model.is_none() {
                model = Some(current.to_owned());
            }
        }
    }
    Ok(SessionObservation {
        // ACP session/load may omit sessionId; this is the exact correlated
        // request's ID, not an inferred or newly generated native identity.
        native_session_id: session.to_owned(),
        configured_model: model,
        requested_model: None,
        native_pid,
        request_id: Some(2),
        codex: None,
    })
}

enum Incoming {
    Result(Value),
    Deny(Value),
    Notification,
}

fn incoming(frame: &[u8], expected_id: u64) -> Result<Incoming> {
    let message: Value =
        serde_json::from_slice(frame).map_err(|_| anyhow!("invalid native protocol frame"))?;
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        bail!("invalid native protocol version");
    }
    if let Some(method) = message.get("method") {
        if !method
            .as_str()
            .is_some_and(|method| !method.is_empty() && method.len() <= 256)
            || message.get("result").is_some()
            || message.get("error").is_some()
        {
            bail!("invalid native host request");
        }
        return match message.get("id") {
            None => Ok(Incoming::Notification),
            Some(id) if id.as_u64().is_some() || id.as_str().is_some_and(safe_label) => {
                Ok(Incoming::Deny(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "Host operations are disabled"}
                })))
            }
            _ => bail!("invalid native host request identity"),
        };
    }
    if message.get("id").and_then(Value::as_u64) != Some(expected_id) {
        bail!("native protocol response identity did not match");
    }
    if message.get("error").is_some() {
        bail!("native protocol request was rejected");
    }
    let result = message
        .get("result")
        .filter(|value| value.is_object())
        .ok_or_else(|| anyhow!("invalid native protocol result"))?;
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
            .ok_or_else(|| anyhow!("native protocol output unavailable"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("native protocol input unavailable"))?;
        let (frames_tx, output) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("grok-session-read".into())
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
            .map_err(|_| anyhow!("native protocol reader unavailable"))?;
        let (input, requests) = mpsc::sync_channel::<Vec<u8>>(1);
        let (written_tx, written) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("grok-session-write".into())
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
            .map_err(|_| anyhow!("native protocol writer unavailable"))?;
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
            .ok_or_else(|| anyhow!("native session observation timed out"))
    }

    fn write(&self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|_| anyhow!("native protocol request could not be encoded"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME {
            bail!("native protocol request exceeds size limit");
        }
        self.remaining()?;
        // Every write awaits its acknowledgement: the queue is never full.
        self.input
            .try_send(bytes)
            .map_err(|_| anyhow!("native protocol input unavailable"))?;
        self.written
            .recv_timeout(self.remaining()?)
            .map_err(|_| anyhow!("native protocol write failed or timed out"))?
            .map_err(|_| anyhow!("native protocol input failed"))
    }

    fn request(&mut self, message: Value, expected_id: u64) -> Result<Value> {
        self.write(&message)?;
        loop {
            let frame = self
                .output
                .recv_timeout(self.remaining()?)
                .map_err(|_| anyhow!("native protocol response unavailable or timed out"))?
                .map_err(|_| anyhow!("native protocol frame unavailable or oversized"))?;
            self.frames += 1;
            self.bytes += frame.len();
            if self.frames > MAX_FRAMES || self.bytes > MAX_TOTAL {
                bail!("native protocol observation exceeds limits");
            }
            match incoming(&frame, expected_id)? {
                Incoming::Result(value) => return Ok(value),
                Incoming::Deny(value) => self.write(&value)?,
                Incoming::Notification => {}
            }
        }
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
                .map_err(|_| anyhow!("native observer exit could not be checked"))?
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
            .map_err(|_| anyhow!("native observer could not be stopped"))?;
        if !self.wait_until(Instant::now() + Duration::from_secs(2))? {
            bail!("native observer exit remains unconfirmed");
        }
        Ok(())
    }
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        // Never block on wait or join: a descendant may still hold a pipe.
        // Child::kill acts only on this spawned handle, never a receipt's PID.
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

    fn fake_native(dir: &Path) -> std::path::PathBuf {
        let program = dir.join("native-fixture");
        fs::write(&program, concat!(
            "#!/bin/sh\n",
            "IFS= read -r first || exit 1\n",
            "printf '%s\\n' \"$first\" > requests\n",
            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{\"loadSession\":true}}}'\n",
            "IFS= read -r second || exit 1\n",
            "printf '%s\\n' \"$second\" >> requests\n",
            "printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"configOptions\":[{\"id\":\"model\",\"currentValue\":\"grok-fixture\"}]}}'\n",
            "while IFS= read -r unexpected; do exit 42; done\n",
        )).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    fn loaded() -> Value {
        // Invented identifier/model; shape verified against installed Grok ACP.
        json!({"configOptions": [{"id": "model", "name": "Model", "type": "select", "currentValue": "grok-fixture", "options": []}]})
    }

    #[test]
    fn request_sequence_has_no_prompt_or_client_tools() {
        let init = initialize_request();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["id"], 1);
        assert_eq!(init["params"]["clientCapabilities"]["terminal"], false);
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["readTextFile"],
            false
        );
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["writeTextFile"],
            false
        );
        let load = load_request(Path::new("/fixture"), "fixture-session").unwrap();
        assert_eq!(load["method"], "session/load");
        assert_eq!(load["id"], 2);
        assert_eq!(
            load["params"],
            json!({"cwd":"/fixture", "sessionId":"fixture-session", "mcpServers":[]})
        );
    }

    #[test]
    fn correlates_response_id_and_redacts_errors() {
        for value in [
            json!({"jsonrpc":"2.0", "id":1, "result":{}}),
            json!({"jsonrpc":"2.0", "id":"2", "result":{}}),
            json!({"jsonrpc":"2.0", "id":2, "error":{"message":"private-secret"}}),
            json!({"jsonrpc":"2.0", "id":2, "result":{}, "error":{"message":"private-secret"}}),
        ] {
            let error = incoming(&serde_json::to_vec(&value).unwrap(), 2)
                .err()
                .unwrap();
            assert!(!format!("{error:#}").contains("private-secret"));
        }
        assert!(matches!(
            incoming(br#"{"jsonrpc":"2.0","id":2,"result":{}}"#, 2).unwrap(),
            Incoming::Result(_)
        ));
    }

    #[test]
    fn denies_host_callbacks_without_echoing_parameters() {
        let value = json!({"jsonrpc":"2.0", "id":"callback-1", "method":"fs/write_text_file", "params":{"content":"private-secret"}});
        let Incoming::Deny(reply) = incoming(&serde_json::to_vec(&value).unwrap(), 2).unwrap()
        else {
            panic!("expected denial")
        };
        assert_eq!(reply["id"], "callback-1");
        assert_eq!(reply["error"]["code"], -32601);
        assert!(!reply.to_string().contains("private-secret"));
        assert!(matches!(
            incoming(
                br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
                2
            )
            .unwrap(),
            Incoming::Notification
        ));
    }

    #[test]
    fn model_receipt_is_optional_strict_and_consistent() {
        let result = observation(&loaded(), "fixture-session", 123).unwrap();
        assert_eq!(result.configured_model.as_deref(), Some("grok-fixture"));
        assert_eq!(result.request_id, Some(2));
        assert_eq!(result.native_session_id, "fixture-session");
        assert_eq!(
            observation(&json!({}), "fixture-session", 123)
                .unwrap()
                .configured_model,
            None
        );
        for value in [
            json!(null),
            json!(false),
            json!(9),
            json!("bad\nmodel"),
            json!(""),
        ] {
            let mut fixture = loaded();
            fixture["configOptions"][0]["currentValue"] = value;
            assert!(observation(&fixture, "fixture-session", 123).is_err());
        }
        let mut fixture = loaded();
        fixture["models"] = json!({"currentModelId":"other-model"});
        assert!(observation(&fixture, "fixture-session", 123).is_err());
        fixture["models"] = json!({"currentModelId":"grok-fixture"});
        assert!(observation(&fixture, "fixture-session", 123).is_ok());
        assert_eq!(
            observation(
                &json!({"models":{"currentModelId":"grok-fixture"}}),
                "fixture-session",
                123
            )
            .unwrap()
            .configured_model
            .as_deref(),
            Some("grok-fixture")
        );
        fixture["sessionId"] = json!("wrong-session");
        assert!(observation(&fixture, "fixture-session", 123).is_err());
        let mut duplicate = loaded();
        duplicate["configOptions"]
            .as_array_mut()
            .unwrap()
            .push(loaded()["configOptions"][0].clone());
        assert!(observation(&duplicate, "fixture-session", 123).is_err());
    }

    #[test]
    fn large_session_history_frame_is_bounded_and_not_a_receipt() {
        let update = json!({"jsonrpc":"2.0", "method":"session/update", "params":{
            "sessionId":"fixture-session", "update":{"sessionUpdate":"user_message_chunk", "content":{"type":"text", "text":"x".repeat(114_000)}}
        }});
        let mut bytes = serde_json::to_vec(&update).unwrap();
        bytes.push(b'\n');
        assert!(bytes.len() > 65_536 && bytes.len() < MAX_FRAME);
        let frame = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert!(matches!(
            incoming(&frame, 2).unwrap(),
            Incoming::Notification
        ));
    }

    #[test]
    fn framing_is_bounded_and_requires_complete_lines() {
        assert!(read_frame(&mut Cursor::new(b"partial")).is_err());
        assert!(read_frame(&mut Cursor::new(vec![b'x'; MAX_FRAME + 1])).is_err());
        assert!(read_frame(&mut Cursor::new(b"{}\n")).is_ok());
    }

    #[test]
    fn validates_initialization_and_strict_observation_schema() {
        assert!(
            validate_initialize(
                &json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true}})
            )
            .is_ok()
        );
        assert!(
            validate_initialize(
                &json!({"protocolVersion":2,"agentCapabilities":{"loadSession":true}})
            )
            .is_err()
        );
        assert!(validate_initialize(&json!({"protocolVersion":1})).is_err());
        let mut receipt =
            serde_json::to_value(observation(&loaded(), "fixture-session", 123).unwrap()).unwrap();
        receipt["authority"] = json!("owner");
        assert!(serde_json::from_value::<SessionObservation>(receipt).is_err());
    }

    #[test]
    fn canonical_args_reject_prompt_and_transport_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let canonical: Vec<String> = ["agent", "--no-leader", "stdio"].map(str::to_owned).into();
        assert!(
            validate_launch(
                Path::new("/fixture/grok"),
                &canonical,
                dir.path(),
                "fixture-session"
            )
            .is_ok()
        );
        for args in [
            vec!["agent", "stdio"],
            vec![
                "agent",
                "--no-leader",
                "stdio",
                "--prompt",
                "private-secret",
            ],
            vec!["agent", "--no-leader", "--model", "bad\nmodel", "stdio"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            let error = validate_launch(
                Path::new("/fixture/grok"),
                &args,
                dir.path(),
                "fixture-session",
            )
            .unwrap_err();
            assert!(!error.to_string().contains("private-secret"));
        }
    }

    #[test]
    fn real_pipe_exchange_persists_while_child_is_live_without_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_native(dir.path());
        let args = ["agent", "--no-leader", "stdio"].map(str::to_owned);
        let mut receipt = None;
        bind_existing(
            &program,
            &args,
            dir.path(),
            "fixture-session",
            |observation| {
                assert!(Path::new(&format!("/proc/{}", observation.native_pid)).exists());
                receipt = Some(observation);
                Ok(())
            },
        )
        .unwrap();
        let receipt = receipt.unwrap();
        assert_eq!(receipt.native_session_id, "fixture-session");
        assert_eq!(receipt.configured_model.as_deref(), Some("grok-fixture"));
        assert!(!Path::new(&format!("/proc/{}", receipt.native_pid)).exists());
        let requests = fs::read_to_string(dir.path().join("requests")).unwrap();
        let messages: Vec<Value> = requests
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["method"], "initialize");
        assert_eq!(messages[1]["method"], "session/load");
        assert_eq!(messages[1]["params"]["sessionId"], "fixture-session");
    }

    #[test]
    fn persist_failure_is_redacted_and_direct_child_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_native(dir.path());
        let args = ["agent", "--no-leader", "stdio"].map(str::to_owned);
        let mut pid = None;
        let error = bind_existing(
            &program,
            &args,
            dir.path(),
            "fixture-session",
            |observation| {
                pid = Some(observation.native_pid);
                bail!("private-secret callback failure")
            },
        )
        .unwrap_err();
        assert!(!format!("{error:#}").contains("private-secret"));
        assert!(!Path::new(&format!("/proc/{}", pid.unwrap())).exists());
    }
}
