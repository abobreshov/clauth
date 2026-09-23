//! Opt-in Linux integration test: real disposable user scopes, no AI clients.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;
use tempfile::TempDir;

const WORKER: &str = r#"
import os, signal, sys, time
r, w = os.pipe()
pid = os.fork()
if pid:
    os.close(w)
    assert os.read(r, 1) == b'1'
    os.close(r)
    print(pid, flush=True)
    sys.exit(0)
os.close(r)
os.setsid()
signal.signal(signal.SIGTERM, signal.SIG_IGN)
null = os.open(os.devnull, os.O_RDWR)
for fd in (0, 1, 2): os.dup2(null, fd)
os.close(null)
deadline = time.monotonic() + 45
with open(sys.argv[1], 'ab', buffering=0) as out:
    out.write(b'x')
    os.write(w, b'1')
    os.close(w)
    while time.monotonic() < deadline:
        out.write(b'x')
        time.sleep(0.05)
os._exit(0)
"#;

struct Fixture {
    home: TempDir,
    task: String,
    marker: PathBuf,
    execution: Option<Value>,
    original_record: Option<Vec<u8>>,
}

impl Fixture {
    fn new() -> Self {
        Self::new_for_tool("codex")
    }

    fn new_for_tool(tool: &str) -> Self {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let input = home.path().join("task.json");
        std::fs::write(&input, serde_json::to_vec(&json!({
            "objective":"Disposable local execution lifecycle test",
            "workspace":workspace,"constraints":[],
            "source":{"tool":tool,"model":null,"native_session_id":"fixture-source","account_ref":null}
        })).unwrap()).unwrap();
        let mut fixture = Self {
            marker: home.path().join("writer.marker"),
            home,
            task: String::new(),
            execution: None,
            original_record: None,
        };
        let result = fixture.cli(&["tasks", "register", "--from", input.to_str().unwrap()]);
        let task = success_json(result);
        fixture.task = task["task_id"].as_str().unwrap().into();
        std::fs::write(fixture.home.path().join("worker.py"), WORKER).unwrap();
        std::fs::write(
            fixture.home.path().join(".clauth/providers.toml"),
            "[[targets]]\nid = \"fixture\"\nprovider = \"codex\"\ncommand = \"/usr/bin/python3\"\n",
        )
        .unwrap();
        fixture
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_clauth"))
            .env("HOME", self.home.path())
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("HERDR_ENV")
            .args(args)
            .output()
            .unwrap()
    }

    fn start(&mut self) -> Output {
        let result = self.cli(&[
            "tasks",
            "run",
            &self.task,
            "--target",
            "fixture",
            "--session",
            "fixture-source",
            "--expected-generation",
            "1",
            "--",
            self.home.path().join("worker.py").to_str().unwrap(),
            self.marker.to_str().unwrap(),
        ]);
        if let Ok(bytes) = std::fs::read(self.record_path()) {
            self.original_record = Some(bytes);
            let observed = self.cli(&["tasks", "execution", &self.task]);
            if observed.status.success() {
                self.execution = serde_json::from_slice(&observed.stdout).ok();
            }
        }
        result
    }

    fn record_path(&self) -> PathBuf {
        self.home
            .path()
            .join(".clauth/tasks")
            .join(&self.task)
            .join("execution.json")
    }

    fn stop(&self, execution: &str) -> Output {
        let task = success_json(self.cli(&["tasks", "show", &self.task]));
        let generation = task["generation"].as_u64().unwrap().to_string();
        self.cli(&[
            "tasks",
            "stop-execution",
            &self.task,
            "--execution-id",
            execution,
            "--session",
            "fixture-source",
            "--expected-generation",
            &generation,
        ])
    }

    fn prepare_handoff(&self) -> Value {
        let input = self.home.path().join("handoff-input.json");
        std::fs::write(&input, serde_json::to_vec(&json!({
            "brief":"Continue the disposable task", "completed":[], "remaining_plan":["Finish"],
            "decisions":[], "uncertainties":[], "next_action":"Review checkpoint", "constraints":[], "resource_ids":[]
        })).unwrap()).unwrap();
        success_json(self.cli(&[
            "tasks",
            "checkpoint",
            &self.task,
            "--session",
            "fixture-source",
            "--expected-generation",
            "1",
            "--capture-workspace",
            "--from",
            input.to_str().unwrap(),
        ]));
        std::fs::write(
            &input,
            serde_json::to_vec(&json!({
                "destinations":[{"tool":"grok", "models":["fixture-model"]}],
                "share_checkpoint":true, "share_workspace":true, "share_resource_metadata":true
            }))
            .unwrap(),
        )
        .unwrap();
        success_json(self.cli(&[
            "tasks",
            "policy",
            &self.task,
            "--session",
            "fixture-source",
            "--expected-generation",
            "2",
            "--from",
            input.to_str().unwrap(),
        ]));
        success_json(self.cli(&[
            "tasks",
            "propose-handoff",
            &self.task,
            "--session",
            "fixture-source",
            "--expected-generation",
            "3",
            "--request-id",
            "fixture-handoff",
            "--to",
            "grok",
            "--model",
            "fixture-model",
        ]))
    }

    fn release(&self, execution: &str, generation: &str) -> Output {
        self.cli(&[
            "tasks",
            "release-handoff-source",
            &self.task,
            "--handoff-id",
            "fixture-handoff",
            "--execution-id",
            execution,
            "--session",
            "fixture-source",
            "--expected-generation",
            generation,
        ])
    }

    fn marker_len(&self) -> u64 {
        std::fs::metadata(&self.marker).unwrap().len()
    }
}

#[test]
#[ignore = "requires Linux user systemd/cgroup v2; creates and stops disposable scopes"]
fn native_session_response_is_durably_bound_without_sending_a_prompt() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new_for_tool("grok");
    let native = fixture.home.path().join("fake_grok.py");
    std::fs::write(
        &native,
        r#"#!/usr/bin/python3
import json, sys
from pathlib import Path
assert sys.argv[1:] == ['agent', '--no-leader', 'stdio']
for line in sys.stdin:
    request = json.loads(line)
    with Path(__file__).with_suffix('.requests').open('a') as log:
        log.write(request['method'] + '\n')
    if request['method'] == 'initialize':
        result = {'protocolVersion':1,'agentCapabilities':{'loadSession':True}}
    elif request['method'] == 'session/load':
        assert request['params']['sessionId'] == 'fixture-source'
        result = {'configOptions':[{'id':'model','currentValue':'grok-fixture'}]}
    else:
        sys.exit(3)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#,
    )
    .unwrap();
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        fixture.home.path().join(".clauth/providers.toml"),
        format!(
            "[[targets]]\nid='fixture'\nprovider='grok'\ncommand='{}'\n",
            native.display()
        ),
    )
    .unwrap();
    let result = fixture.cli(&[
        "tasks",
        "bind-session",
        &fixture.task,
        "--target",
        "fixture",
        "--session",
        "fixture-source",
        "--expected-generation",
        "1",
    ]);
    // Retain exact cleanup identity even if a subsequent assertion fails.
    if let Ok(bytes) = std::fs::read(fixture.record_path()) {
        fixture.original_record = Some(bytes);
        let observed = fixture.cli(&["tasks", "execution", &fixture.task]);
        fixture.execution = serde_json::from_slice(&observed.stdout).ok();
    }
    let report = success_json(result);
    let observation = &report["native_session_observation"];
    assert_eq!(report["execution_mode"], "grok_session_binding");
    assert_eq!(
        observation["response"]["native_session_id"],
        "fixture-source"
    );
    assert_eq!(observation["response"]["configured_model"], "grok-fixture");
    assert_eq!(observation["response"]["request_id"], 2);
    assert!(observation["response"]["native_pid"].as_u64().unwrap() > 0);
    assert!(!observation["native_start"].as_str().unwrap().is_empty());
    assert_eq!(report["native_identity_verified"], false);
    assert_eq!(report["handoff_ready"], false);
    let reread = success_json(fixture.cli(&["tasks", "execution", &fixture.task]));
    assert_eq!(reread["native_session_observation"], *observation);
    assert_eq!(
        std::fs::read_to_string(native.with_extension("requests")).unwrap(),
        "initialize\nsession/load\n"
    );
    let task = success_json(fixture.cli(&["tasks", "show", &fixture.task]));
    assert_eq!(task["generation"], 1);
    assert_eq!(task["ownership_epoch"], 1);
    assert_eq!(task["authority_mode"], "unmanaged_checkpoint_only");
}

#[test]
#[ignore = "requires Linux user systemd/cgroup v2; creates and stops disposable scopes"]
fn agy_init_is_durably_bound_without_prompt_or_configured_model_claim() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new_for_tool("agy");
    let native = fixture.home.path().join("fake_agy.py");
    std::fs::write(&native, r#"#!/usr/bin/python3
import json, os, sys
from pathlib import Path
assert sys.argv[1:] == ['--input-format=stream-json', '--output-format=stream-json', '--conversation', 'fixture-source', '--model', 'agy-fixture']
print(json.dumps({'event':'init','conversation_id':'fixture-source','init':{'cwd':os.getcwd(),'model':'agy-fixture'}}), flush=True)
assert sys.stdin.buffer.read() == b''
Path(__file__).with_suffix('.zero-input').write_text('verified')
"#).unwrap();
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(fixture.home.path().join(".clauth/providers.toml"), format!(
        "[[targets]]\nid='fixture'\nprovider='antigravity'\ncommand='{}'\nmodel='agy-fixture'\n", native.display()
    )).unwrap();
    let result = fixture.cli(&[
        "tasks",
        "bind-session",
        &fixture.task,
        "--target",
        "fixture",
        "--session",
        "fixture-source",
        "--expected-generation",
        "1",
    ]);
    if let Ok(bytes) = std::fs::read(fixture.record_path()) {
        fixture.original_record = Some(bytes);
        let observed = fixture.cli(&["tasks", "execution", &fixture.task]);
        fixture.execution = serde_json::from_slice(&observed.stdout).ok();
    }
    let report = success_json(result);
    let observation = &report["native_session_observation"];
    assert_eq!(report["execution_mode"], "agy_session_binding");
    assert_eq!(
        observation["response"]["native_session_id"],
        "fixture-source"
    );
    assert!(observation["response"]["configured_model"].is_null());
    assert_eq!(observation["response"]["requested_model"], "agy-fixture");
    assert!(observation["response"]["request_id"].is_null());
    assert_eq!(report["native_identity_verified"], false);
    assert_eq!(report["handoff_ready"], false);
    let reread = success_json(fixture.cli(&["tasks", "execution", &fixture.task]));
    assert_eq!(reread["native_session_observation"], *observation);
    assert_eq!(
        std::fs::read_to_string(native.with_extension("zero-input")).unwrap(),
        "verified"
    );
    let task = success_json(fixture.cli(&["tasks", "show", &fixture.task]));
    assert_eq!(task["generation"], 1);
    assert_eq!(task["ownership_epoch"], 1);
    assert_eq!(task["authority_mode"], "unmanaged_checkpoint_only");
}

#[test]
#[ignore = "requires Linux user systemd/cgroup v2; creates and stops disposable scopes"]
fn codex_resume_records_thread_and_tree_without_starting_a_turn() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new_for_tool("codex");
    let native = fixture.home.path().join("fake_codex.py");
    std::fs::write(&native, r#"#!/usr/bin/python3
import json, os, sys
from pathlib import Path
assert sys.argv[1:] == ['app-server', '--listen', 'stdio://', '--config', 'model="codex-fixture"']
for index, line in enumerate(sys.stdin):
    request = json.loads(line)
    assert 'jsonrpc' not in request
    with Path(__file__).with_suffix('.requests').open('a') as log:
        log.write(request['method'] + '\n')
    if index == 0:
        assert request['method'] == 'initialize' and request['id'] == 1
        result = {'userAgent':'codex/0.155.1'}
    elif index == 1:
        assert request['method'] == 'initialized' and 'id' not in request
        continue
    elif index == 2:
        assert request['method'] == 'thread/resume' and request['id'] == 2
        params = request['params']
        assert params['threadId'] == 'fixture-source'
        assert params['cwd'] == os.getcwd() and params['excludeTurns'] is True
        assert params['approvalPolicy'] == 'never' and params['sandbox'] == 'read-only'
        assert params['model'] == 'codex-fixture'
        result = {'thread':{'id':'fixture-source','sessionId':'fixture-tree','cwd':os.getcwd(),'model':'codex-fixture','modelProvider':'openai'},'cwd':os.getcwd(),'model':'codex-fixture','modelProvider':'openai','approvalPolicy':'never','sandbox':{'type':'readOnly','networkAccess':False}}
    else:
        sys.exit(3)
    print(json.dumps({'id':request['id'],'result':result}), flush=True)
assert index == 2
"#).unwrap();
    std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        fixture.home.path().join(".clauth/providers.toml"),
        format!(
            "[[targets]]\nid='fixture'\nprovider='codex'\ncommand='{}'\nmodel='codex-fixture'\n",
            native.display()
        ),
    )
    .unwrap();
    let result = fixture.cli(&[
        "tasks",
        "bind-session",
        &fixture.task,
        "--target",
        "fixture",
        "--session",
        "fixture-source",
        "--expected-generation",
        "1",
    ]);
    if let Ok(bytes) = std::fs::read(fixture.record_path()) {
        fixture.original_record = Some(bytes);
        let observed = fixture.cli(&["tasks", "execution", &fixture.task]);
        fixture.execution = serde_json::from_slice(&observed.stdout).ok();
    }
    let report = success_json(result);
    let observation = &report["native_session_observation"];
    assert_eq!(report["execution_mode"], "codex_session_binding");
    assert_eq!(
        observation["response"]["native_session_id"],
        "fixture-source"
    );
    assert_eq!(
        observation["response"]["codex"]["session_tree_id"],
        "fixture-tree"
    );
    assert_eq!(observation["response"]["codex"]["model_provider"], "openai");
    assert_eq!(observation["response"]["configured_model"], "codex-fixture");
    assert_eq!(observation["response"]["requested_model"], "codex-fixture");
    assert_eq!(observation["response"]["request_id"], 2);
    assert_eq!(report["native_identity_verified"], false);
    assert_eq!(report["handoff_ready"], false);
    let reread = success_json(fixture.cli(&["tasks", "execution", &fixture.task]));
    assert_eq!(reread["native_session_observation"], *observation);
    assert_eq!(
        std::fs::read_to_string(native.with_extension("requests")).unwrap(),
        "initialize\ninitialized\nthread/resume\n"
    );
    let task = success_json(fixture.cli(&["tasks", "show", &fixture.task]));
    assert_eq!(task["generation"], 1);
    assert_eq!(task["ownership_epoch"], 1);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Restore only this fixture's record if an assertion interrupted tampering.
        if let Some(bytes) = &self.original_record {
            let _ = std::fs::write(self.record_path(), bytes);
        }
        if let Some(report) = &self.execution {
            if let Some(id) = report["execution_id"].as_str() {
                // A stop-first intent deliberately blocks ordinary source
                // operations. Cleanup must reconcile that same intent.
                let _ = self.release(id, "4");
                let _ = self.stop(id);
            }
            if let Some(unit) = report["unit"].as_str() {
                // Clear only the disposable unit's failed status, never all units.
                let _ = Command::new("systemctl")
                    .args(["--user", "reset-failed", "--", unit])
                    .output();
            }
        }
    }
}

fn success_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
#[ignore = "requires Linux user systemd/cgroup v2; creates and stops disposable scopes"]
fn stop_first_release_preserves_independent_writer_and_blocks_source_relaunch() {
    let mut source = Fixture::new();
    let mut independent = Fixture::new();
    assert!(source.start().status.success());
    assert!(independent.start().status.success());
    let before = source.prepare_handoff();
    assert_eq!(before["generation"], 4);
    let execution = source.execution.as_ref().unwrap()["execution_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let wrong = source.release("00000000000000000000000000000000", "4");
    assert!(!wrong.status.success());
    assert_eq!(
        success_json(source.cli(&["tasks", "show", &source.task])),
        before
    );
    let after = success_json(source.release(&execution, "4"));
    assert_eq!(after["generation"], 6);
    assert_eq!(after["history"][4]["kind"], "source_stop_requested");
    assert_eq!(after["history"][5]["kind"], "source_scope_empty");
    assert_eq!(
        after["handoffs"][0]["source_release"]["state"],
        "scope_empty"
    );
    assert_eq!(
        after["handoffs"][0]["source_release"]["recorded_scope_empty"],
        true
    );
    assert_eq!(after["ownership_epoch"], 1);
    assert_eq!(after["owner"], before["owner"]);
    assert_eq!(after["readiness"]["handoff_ready"], false);
    let stopped_size = source.marker_len();
    let independent_size = independent.marker_len();
    std::thread::sleep(Duration::from_millis(180));
    assert_eq!(source.marker_len(), stopped_size);
    assert!(independent.marker_len() > independent_size);
    assert_eq!(success_json(source.release(&execution, "4")), after);
    assert_eq!(success_json(source.release(&execution, "6")), after);
    let restarted = source.cli(&[
        "tasks",
        "run",
        &source.task,
        "--session",
        "fixture-source",
        "--expected-generation",
        "6",
        "--target",
        "fixture",
        "--",
        "--version",
    ]);
    assert!(!restarted.status.success());
    assert!(String::from_utf8_lossy(&restarted.stderr).contains("relaunch is blocked"));
    assert_eq!(
        success_json(source.cli(&["tasks", "execution", &source.task]))["execution_id"],
        execution
    );
}

#[test]
#[ignore = "requires Linux user systemd/cgroup v2; creates and stops disposable scopes"]
fn stop_first_release_persists_intent_before_signal_and_parks_shutdown_edits() {
    let mut source = Fixture::new();
    let worker = WORKER.replace(
        "signal.signal(signal.SIGTERM, signal.SIG_IGN)",
        r#"
def stopping(sig, frame):
    import json
    from pathlib import Path
    task_dir = Path(os.environ['HOME']) / '.clauth' / 'tasks' / os.environ['CLAUTH_TASK_ID']
    task = json.loads((task_dir / 'task.json').read_text())
    assert task['handoffs'][0]['source_release']['state'] == 'stop_requested'
    (Path(os.environ['HOME']) / 'intent-before-signal').write_text('verified')
    (Path.cwd() / 'shutdown-edit.txt').write_text('written during SIGTERM')
    os._exit(0)
signal.signal(signal.SIGTERM, stopping)
"#,
    );
    std::fs::write(source.home.path().join("worker.py"), worker).unwrap();
    assert!(source.start().status.success());
    source.prepare_handoff();
    let execution = source.execution.as_ref().unwrap()["execution_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = source.release(&execution, "4");
    assert!(
        !result.status.success(),
        "workspace drift must not report successful release"
    );
    let task: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(task["handoffs"][0]["source_release"]["state"], "parked");
    assert_eq!(
        task["handoffs"][0]["source_release"]["failure"],
        "workspace_changed"
    );
    assert_eq!(
        task["handoffs"][0]["source_release"]["recorded_scope_empty"],
        true
    );
    assert_eq!(
        std::fs::read_to_string(source.home.path().join("intent-before-signal")).unwrap(),
        "verified"
    );
    assert_eq!(
        std::fs::read_to_string(source.home.path().join("project/shutdown-edit.txt")).unwrap(),
        "written during SIGTERM"
    );
    assert_eq!(task["readiness"]["handoff_ready"], false);
    let retry = source.release(&execution, "4");
    assert!(!retry.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&retry.stdout).unwrap(),
        task
    );
}

#[test]
#[ignore = "requires Linux user systemd/cgroup v2; creates and stops disposable scopes"]
fn stop_first_release_parks_control_failure_and_retries_only_the_same_scope() {
    use std::os::unix::fs::PermissionsExt;
    let mut source = Fixture::new();
    assert!(source.start().status.success());
    source.prepare_handoff();
    let execution = source.execution.as_ref().unwrap()["execution_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bin = source.home.path().join("control-failure-bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("systemctl");
    // Only the stop command is failed; observations still use the real manager.
    std::fs::write(
        &fake,
        "#!/bin/sh\ncase \" $* \" in *' stop '*) exit 1;; esac\nexec /usr/bin/systemctl \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_clauth"))
        .env("HOME", source.home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("HERDR_ENV")
        .args([
            "tasks",
            "release-handoff-source",
            &source.task,
            "--handoff-id",
            "fixture-handoff",
            "--execution-id",
            &execution,
            "--session",
            "fixture-source",
            "--expected-generation",
            "4",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let parked: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(parked["handoffs"][0]["source_release"]["state"], "parked");
    assert_eq!(
        parked["handoffs"][0]["source_release"]["failure"],
        "stop_unproven"
    );
    assert_eq!(
        parked["handoffs"][0]["source_release"]["recorded_scope_empty"],
        false
    );
    let before = source.marker_len();
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        source.marker_len() > before,
        "failed stop must not pretend the source is stopped"
    );
    let restarted = source.cli(&[
        "tasks",
        "run",
        &source.task,
        "--session",
        "fixture-source",
        "--expected-generation",
        "6",
        "--target",
        "fixture",
        "--",
        "--version",
    ]);
    assert!(!restarted.status.success());
    assert!(String::from_utf8_lossy(&restarted.stderr).contains("relaunch is blocked"));
    assert!(
        !source
            .release("00000000000000000000000000000000", "4")
            .status
            .success()
    );
    let recovered = success_json(source.release(&execution, "4"));
    assert_eq!(recovered["generation"], 7);
    assert_eq!(
        recovered["handoffs"][0]["source_release"]["state"],
        "scope_empty"
    );
    assert_eq!(
        recovered["handoffs"][0]["source_release"]["failure"],
        Value::Null
    );
    assert_eq!(recovered["readiness"]["handoff_ready"], false);
}

#[test]
#[ignore = "requires Linux user systemd/cgroup v2; creates and stops disposable scopes"]
fn detached_writers_stop_only_in_the_exact_recorded_execution() {
    let mut source = Fixture::new();
    let output = source.start();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = source.execution.as_ref().unwrap();
    assert_eq!(report["state"], "active");
    assert_eq!(report["recorded_scope_empty"], false);
    assert_eq!(report["native_identity_verified"], false);
    assert_eq!(report["handoff_ready"], false);
    let execution = report["execution_id"].as_str().unwrap().to_owned();
    let before = source.marker_len();
    std::thread::sleep(Duration::from_millis(180));
    assert!(
        source.marker_len() > before,
        "detached writer must survive parent exit"
    );

    let duplicate = source.start();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("previous execution"));
    let stale = source.stop("00000000000000000000000000000000");
    assert!(
        !stale.status.success(),
        "stale execution ID must not stop the writer"
    );

    let mut resource = Fixture::new();
    let result = resource.start();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let original = source.original_record.as_ref().unwrap().clone();
    let mut foreign: Value = serde_json::from_slice(&original).unwrap();
    foreign["binding"]["invocation_id"] = json!("00000000000000000000000000000000");
    std::fs::write(source.record_path(), serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(
        !source.stop(&execution).status.success(),
        "foreign invocation must refuse control"
    );
    let before = source.marker_len();
    std::thread::sleep(Duration::from_millis(180));
    assert!(source.marker_len() > before);
    std::fs::write(source.record_path(), original).unwrap();

    let stopped = success_json(source.stop(&execution));
    assert_eq!(stopped["state"], "local_empty");
    assert_eq!(stopped["recorded_scope_empty"], true);
    assert_eq!(stopped["handoff_ready"], false);
    let stopped_size = source.marker_len();
    let resource_size = resource.marker_len();
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(source.marker_len(), stopped_size);
    assert!(
        resource.marker_len() > resource_size,
        "independent resource must remain alive"
    );
    // Repeated exact stops are harmless and preserve the immutable run identity.
    assert_eq!(
        success_json(source.stop(&execution))["execution_id"],
        execution
    );
    let old_unit = source.execution.as_ref().unwrap()["unit"].as_str().unwrap();
    let cleared = Command::new("systemctl")
        .args(["--user", "reset-failed", "--", old_unit])
        .output()
        .unwrap();
    assert!(cleared.status.success());
    let rerun = source.start();
    assert!(
        rerun.status.success(),
        "{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    let replacement = source.execution.as_ref().unwrap()["execution_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(replacement, execution);
    assert!(
        source
            .record_path()
            .parent()
            .unwrap()
            .join(format!("execution-{execution}.json"))
            .is_file()
    );
    assert!(
        !source.stop(&execution).status.success(),
        "old stop must not stop replacement run"
    );
    let before = source.marker_len();
    std::thread::sleep(Duration::from_millis(180));
    assert!(source.marker_len() > before);
    assert_eq!(
        success_json(source.stop(&replacement))["recorded_scope_empty"],
        true
    );
}
