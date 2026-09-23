//! Observe an existing agy conversation before any prompt is supplied.
//! Init model metadata is a requested-option echo, not configured-model proof.
//! Only the direct child is reaped; descendants require the caller's scope.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use super::native_session::SessionObservation;

const MAX_FRAME: usize = 262_144;
const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(20);

/// The callback must durably persist the observation and return promptly.
/// Stdin remains open and is never written, including while the callback runs.
/// A failed callback does not guarantee that no partial durable write happened.
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
        .map_err(|_| anyhow!("could not start native conversation observer"))?;
    let mut child = NativeChild(child);
    let result = (|| {
        let stdout = child
            .0
            .stdout
            .take()
            .ok_or_else(|| anyhow!("native conversation output unavailable"))?;
        let (send, receive) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("agy-session-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let frame = read_frame(&mut reader);
                // Keep the output pipe open through persistence: returning the
                // reader avoids early broken-pipe termination of the native host.
                let _ = send.send(frame.map(|frame| (frame, reader)));
            })
            .map_err(|_| anyhow!("native conversation reader unavailable"))?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| anyhow!("native conversation observation timed out"))?;
        let (frame, _stdout) = receive
            .recv_timeout(remaining)
            .map_err(|_| anyhow!("native conversation init unavailable or timed out"))?
            .map_err(|_| anyhow!("native conversation init unavailable or oversized"))?;
        let observed = observation(&frame, cwd, session_id, requested_model, child.0.id())?;
        if child
            .0
            .try_wait()
            .map_err(|_| anyhow!("native conversation observer liveness could not be checked"))?
            .is_some()
        {
            bail!("native conversation observer exited before observation was persisted");
        }
        // A liveness check is not an atomic lifetime lease or account proof.
        persist(observed)
            .map_err(|_| anyhow!("native conversation observation could not be persisted"))
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

fn validate_launch<'a>(
    program: &Path,
    args: &'a [String],
    cwd: &Path,
    session: &str,
) -> Result<Option<&'a str>> {
    let requested_model = match args {
        [input, output, conversation, supplied]
            if input == "--input-format=stream-json"
                && output == "--output-format=stream-json"
                && conversation == "--conversation"
                && supplied == session =>
        {
            None
        }
        [input, output, conversation, supplied, model_flag, model]
            if input == "--input-format=stream-json"
                && output == "--output-format=stream-json"
                && conversation == "--conversation"
                && supplied == session
                && model_flag == "--model"
                && safe_label(model) =>
        {
            Some(model.as_str())
        }
        _ => bail!("invalid native conversation observer arguments"),
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
        bail!("invalid native conversation observer configuration");
    }
    Ok(requested_model)
}

fn observation(
    frame: &[u8],
    cwd: &Path,
    session: &str,
    requested_model: Option<&str>,
    native_pid: u32,
) -> Result<SessionObservation> {
    if frame.len() > MAX_FRAME || native_pid == 0 || !safe_label(session) {
        bail!("invalid native conversation observation");
    }
    let message: Value = serde_json::from_slice(frame)
        .map_err(|_| anyhow!("invalid native conversation init frame"))?;
    if message.get("event").and_then(Value::as_str) != Some("init")
        || message.get("conversation_id").and_then(Value::as_str) != Some(session)
    {
        bail!("native conversation init identity did not match");
    }
    let init = message
        .get("init")
        .filter(|init| init.is_object())
        .ok_or_else(|| anyhow!("invalid native conversation init payload"))?;
    // Installed agy 1.2.7 emitted cwd in both the verified create and reload
    // fixtures. Fail closed if a different version stops providing this proof.
    if init.get("cwd").and_then(Value::as_str) != cwd.to_str() {
        bail!("native conversation workspace did not match");
    }
    let echoed_model = match init.get("model") {
        None => None,
        Some(value) => {
            let model = value
                .as_str()
                .filter(|value| safe_label(value))
                .ok_or_else(|| anyhow!("invalid native conversation model metadata"))?;
            if requested_model.is_some_and(|requested| requested != model) {
                bail!("native conversation requested model did not match");
            }
            Some(model.to_owned())
        }
    };
    Ok(SessionObservation {
        native_session_id: session.to_owned(),
        configured_model: None,
        requested_model: echoed_model,
        native_pid,
        request_id: None,
        codex: None,
    })
}

fn read_frame(reader: &mut impl BufRead) -> std::result::Result<Vec<u8>, ()> {
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
                .map_err(|_| anyhow!("native conversation observer exit could not be checked"))?
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
        // This is the first operation on stdin since spawn. No bytes were sent.
        self.0.stdin.take();
        if self.wait_until(Instant::now() + Duration::from_secs(1))? {
            return Ok(());
        }
        self.0
            .kill()
            .map_err(|_| anyhow!("native conversation observer could not be stopped"))?;
        if !self.wait_until(Instant::now() + Duration::from_secs(2))? {
            bail!("native conversation observer exit remains unconfirmed");
        }
        Ok(())
    }
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        // Do not join a reader: descendants may retain a pipe after direct exit.
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.try_wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    fn init(cwd: &Path) -> Value {
        json!({"event":"init", "conversation_id":"fixture-session", "init":{"cwd":cwd, "permission_mode":"request-review"}})
    }

    fn args(model: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = [
            "--input-format=stream-json",
            "--output-format=stream-json",
            "--conversation",
            "fixture-session",
        ]
        .map(str::to_owned)
        .into();
        if let Some(model) = model {
            args.extend(["--model".into(), model.into()]);
        }
        args
    }

    fn fake_native(dir: &Path, frame: &Value) -> std::path::PathBuf {
        let program = dir.join("native-fixture");
        let encoded = serde_json::to_string(frame).unwrap().replace('\'', "'\\''");
        fs::write(&program, format!(
            "#!/bin/sh\nprintf '%s\\n' '{encoded}'\ninput=\nif IFS= read -r input || [ -n \"$input\" ]; then\n  printf '%s' \"$input\" > unexpected-input\n  exit 42\nelse\n  : > zero-input\nfi\n"
        )).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    #[test]
    fn real_pipe_receipt_keeps_input_open_without_writing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut frame = init(dir.path());
        frame["init"]["model"] = json!("fixture-model");
        let program = fake_native(dir.path(), &frame);
        let mut receipt = None;
        bind_existing(
            &program,
            &args(Some("fixture-model")),
            dir.path(),
            "fixture-session",
            |observed| {
                assert!(Path::new(&format!("/proc/{}", observed.native_pid)).exists());
                assert!(!dir.path().join("zero-input").exists());
                receipt = Some(observed);
                Ok(())
            },
        )
        .unwrap();
        let receipt = receipt.unwrap();
        assert_eq!(receipt.native_session_id, "fixture-session");
        assert_eq!(receipt.requested_model.as_deref(), Some("fixture-model"));
        assert_eq!(receipt.configured_model, None);
        assert_eq!(receipt.request_id, None);
        assert!(dir.path().join("zero-input").exists());
        assert!(!dir.path().join("unexpected-input").exists());
        assert!(!Path::new(&format!("/proc/{}", receipt.native_pid)).exists());
    }

    #[test]
    fn missing_model_is_not_inferred_from_requested_argument() {
        let cwd = Path::new("/fixture");
        let frame = serde_json::to_vec(&init(cwd)).unwrap();
        let observed = observation(
            &frame,
            cwd,
            "fixture-session",
            Some("requested-but-unconfirmed"),
            123,
        )
        .unwrap();
        assert_eq!(observed.requested_model, None);
        assert_eq!(observed.configured_model, None);
        assert_eq!(observed.request_id, None);
    }

    #[test]
    fn rejects_wrong_session_workspace_missing_cwd_and_non_init() {
        let cwd = Path::new("/fixture");
        for (field, value) in [
            ("conversation_id", json!("wrong-session")),
            ("event", json!("result")),
            ("init", json!({})),
            ("init", json!(null)),
            ("init", json!({"cwd":"/wrong-workspace"})),
        ] {
            let mut frame = init(cwd);
            frame[field] = value;
            assert!(
                observation(
                    &serde_json::to_vec(&frame).unwrap(),
                    cwd,
                    "fixture-session",
                    None,
                    123
                )
                .is_err()
            );
        }
    }

    #[test]
    fn model_metadata_is_bounded_optional_and_requested_only() {
        let cwd = Path::new("/fixture");
        for value in [
            json!(null),
            json!(false),
            json!(4),
            json!(""),
            json!("bad\nmodel"),
            json!("x".repeat(257)),
            json!("wrong-model"),
        ] {
            let mut frame = init(cwd);
            frame["init"]["model"] = value;
            assert!(
                observation(
                    &serde_json::to_vec(&frame).unwrap(),
                    cwd,
                    "fixture-session",
                    Some("fixture-model"),
                    123
                )
                .is_err()
            );
        }
        let mut frame = init(cwd);
        frame["init"]["model"] = json!("native-default-echo");
        let observed = observation(
            &serde_json::to_vec(&frame).unwrap(),
            cwd,
            "fixture-session",
            None,
            123,
        )
        .unwrap();
        assert_eq!(observed.configured_model, None);
        assert_eq!(
            observed.requested_model.as_deref(),
            Some("native-default-echo")
        );
    }

    #[test]
    fn rejects_extra_prompt_and_permission_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let program = Path::new("/fixture/agy");
        assert!(validate_launch(program, &args(None), dir.path(), "fixture-session").is_ok());
        for extra in [
            "--print=private-secret",
            "--dangerously-skip-permissions",
            "--continue",
        ] {
            let mut arguments = args(None);
            arguments.push(extra.into());
            let error =
                validate_launch(program, &arguments, dir.path(), "fixture-session").unwrap_err();
            assert!(!format!("{error:#}").contains("private-secret"));
        }
        assert!(validate_launch(program, &args(None), dir.path(), "wrong-session").is_err());
        assert!(
            validate_launch(
                program,
                &args(None),
                Path::new("relative"),
                "fixture-session"
            )
            .is_err()
        );
    }

    #[test]
    fn frames_are_bounded_and_errors_do_not_echo_native_content() {
        assert!(read_frame(&mut Cursor::new(vec![b'x'; MAX_FRAME + 1])).is_err());
        assert!(read_frame(&mut Cursor::new(b"partial")).is_err());
        assert!(read_frame(&mut Cursor::new(b"{}\n")).is_ok());
        for frame in [
            b"private-secret invalid JSON".as_slice(),
            br#"{"event":"error","message":"private-secret"}"#,
        ] {
            let error = observation(frame, Path::new("/fixture"), "fixture-session", None, 123)
                .unwrap_err();
            assert!(!format!("{error:#}").contains("private-secret"));
        }
    }

    #[test]
    fn callback_failure_is_redacted_and_child_is_reaped_without_input() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake_native(dir.path(), &init(dir.path()));
        let mut pid = None;
        let error = bind_existing(
            &program,
            &args(None),
            dir.path(),
            "fixture-session",
            |observed| {
                pid = Some(observed.native_pid);
                bail!("private-secret callback failure")
            },
        )
        .unwrap_err();
        assert!(!format!("{error:#}").contains("private-secret"));
        assert!(!Path::new(&format!("/proc/{}", pid.unwrap())).exists());
        assert!(dir.path().join("zero-input").exists());
        assert!(!dir.path().join("unexpected-input").exists());
    }

    #[test]
    fn wrong_native_receipt_never_calls_persist_and_reaps_child() {
        let dir = tempfile::tempdir().unwrap();
        let mut frame = init(dir.path());
        frame["conversation_id"] = json!("wrong-session");
        let program = fake_native(dir.path(), &frame);
        let mut persisted = false;
        assert!(
            bind_existing(&program, &args(None), dir.path(), "fixture-session", |_| {
                persisted = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!persisted);
        assert!(dir.path().join("zero-input").exists());
    }
}
