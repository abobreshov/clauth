//! Durable checkpoint records are portable data, never an ownership fence by themselves.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn cli(home: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clauth"))
        .env("HOME", home.path())
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("HERDR_ENV")
        .args(args)
        .output()
        .unwrap()
}

fn write_json(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn new_task(home: &TempDir) -> Value {
    new_task_for_tool(home, "codex")
}

fn new_task_for_tool(home: &TempDir, tool: &str) -> Value {
    let workspace = home.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let input = home.path().join("new-task.json");
    write_json(
        &input,
        &json!({
            "objective": "Finish the portable checkpoint demo",
            "workspace": workspace,
            "constraints": ["Do not change unrelated files"],
            "source": {"tool":tool, "model":null, "native_session_id":"source-1", "account_ref":null}
        }),
    );
    let result = cli(
        home,
        &["tasks", "register", "--from", input.to_str().unwrap()],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

#[test]
fn native_binding_refuses_unsupported_tool_or_uncontrolled_arguments() {
    for (tool, extra_config, expected) in [
        ("claude", "", "only Codex, Grok and agy"),
        (
            "grok",
            "args=['--leader']\n",
            "does not accept configured arguments",
        ),
    ] {
        let home = TempDir::new().unwrap();
        let task = new_task_for_tool(&home, tool);
        let id = task["task_id"].as_str().unwrap();
        std::fs::write(
            home.path().join(".clauth/providers.toml"),
            format!("[[targets]]\nid='native'\nprovider='{tool}'\n{extra_config}"),
        )
        .unwrap();
        let result = cli(
            &home,
            &[
                "tasks",
                "bind-session",
                id,
                "--target",
                "native",
                "--session",
                "source-1",
                "--expected-generation",
                "1",
            ],
        );
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !home
                .path()
                .join(".clauth/tasks")
                .join(id)
                .join("execution.json")
                .exists()
        );
    }
}

#[test]
fn agy_binding_rejects_native_prompt_arguments_before_launch() {
    let home = TempDir::new().unwrap();
    let task = new_task_for_tool(&home, "agy");
    let id = task["task_id"].as_str().unwrap();
    std::fs::write(
        home.path().join(".clauth/providers.toml"),
        "[[targets]]\nid='agy'\nprovider='antigravity'\nargs=['--print','must-not-send']\n",
    )
    .unwrap();
    let output = cli(
        &home,
        &[
            "tasks",
            "bind-session",
            id,
            "--target",
            "agy",
            "--session",
            "source-1",
            "--expected-generation",
            "1",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not accept configured arguments"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !home
            .path()
            .join(".clauth/tasks")
            .join(id)
            .join("execution.json")
            .exists()
    );
}

#[test]
fn codex_binding_rejects_native_prompt_arguments_before_launch() {
    let home = TempDir::new().unwrap();
    let task = new_task_for_tool(&home, "codex");
    let id = task["task_id"].as_str().unwrap();
    std::fs::write(
        home.path().join(".clauth/providers.toml"),
        "[[targets]]\nid='codex'\nprovider='codex'\nargs=['exec','must-not-send']\n",
    )
    .unwrap();
    let output = cli(
        &home,
        &[
            "tasks",
            "bind-session",
            id,
            "--target",
            "codex",
            "--session",
            "source-1",
            "--expected-generation",
            "1",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not accept configured arguments"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !home
            .path()
            .join(".clauth/tasks")
            .join(id)
            .join("execution.json")
            .exists()
    );
}

fn checkpoint(home: &TempDir, resource_ids: Value) -> std::path::PathBuf {
    let input = home.path().join("checkpoint.json");
    write_json(
        &input,
        &json!({
            "brief":"Implementation is half complete. Tests are running in the registered pane.",
            "completed":["Created initial implementation"],
            "remaining_plan":["Inspect the test result and finish verification"],
            "decisions":["Preserve the existing worktree"],
            "uncertainties":[],
            "next_action":"Read the registered test pane",
            "constraints":["Do not change unrelated files"],
            "resource_ids":resource_ids
        }),
    );
    input
}

#[test]
fn task_and_checkpoint_survive_separate_processes_without_claiming_handoff() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    assert_eq!(task["generation"], 1);
    assert_eq!(task["authority_mode"], "unmanaged_checkpoint_only");
    let input = checkpoint(&home, json!([]));
    let saved = cli(
        &home,
        &[
            "tasks",
            "checkpoint",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--from",
            input.to_str().unwrap(),
        ],
    );
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let shown = cli(&home, &["tasks", "show", id]);
    assert!(shown.status.success());
    let reread: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(reread["generation"], 2);
    assert_eq!(reread["owner"]["native_session_id"], "source-1");
    assert!(!reread["latest_checkpoint"].is_null());
    let stale = cli(
        &home,
        &[
            "tasks",
            "checkpoint",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--from",
            input.to_str().unwrap(),
        ],
    );
    assert!(
        !stale.status.success(),
        "a stale agent must not overwrite the checkpoint"
    );
}

#[test]
fn resource_inventory_must_be_included_in_compacted_checkpoint() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let resource = home.path().join("resource.json");
    write_json(
        &resource,
        &json!({
            "id":"test-pane", "kind":"herdr_pane",
            "native_identity":{"instance":"local", "session":"main", "workspace_id":"w9", "tab_id":"w9:t2", "pane_id":"w9:p7"},
            "purpose":"Run existing tests", "ownership":"task", "disposition":"adopt", "may_write":false,
            "reconnect":"Read the same Herdr pane after verifying its identity", "cleanup":format!("touch {}", home.path().join("must-not-execute").display())
        }),
    );
    let registered = cli(
        &home,
        &[
            "tasks",
            "resource",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--from",
            resource.to_str().unwrap(),
        ],
    );
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    assert!(
        !home.path().join("must-not-execute").exists(),
        "resource recipes are data, not commands to execute"
    );
    let incomplete = checkpoint(&home, json!([]));
    let rejected = cli(
        &home,
        &[
            "tasks",
            "checkpoint",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "2",
            "--from",
            incomplete.to_str().unwrap(),
        ],
    );
    assert!(!rejected.status.success());
    let complete = checkpoint(&home, json!(["test-pane"]));
    let accepted = cli(
        &home,
        &[
            "tasks",
            "checkpoint",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "2",
            "--from",
            complete.to_str().unwrap(),
        ],
    );
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[test]
fn invalid_input_never_echoes_pasted_secrets() {
    let home = TempDir::new().unwrap();
    let input = home.path().join("bad.json");
    std::fs::write(&input, "{TEST_SECRET_SHOULD_NOT_LEAK}").unwrap();
    let result = cli(
        &home,
        &["tasks", "register", "--from", input.to_str().unwrap()],
    );
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("TEST_SECRET_SHOULD_NOT_LEAK"));
}

#[test]
fn task_id_cannot_escape_store() {
    let home = TempDir::new().unwrap();
    let result = cli(&home, &["tasks", "show", "../../outside"]);
    assert!(!result.status.success());
    assert!(!home.path().join("outside").exists());
}

#[test]
fn two_processes_cannot_both_publish_the_same_generation() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let input = checkpoint(&home, json!([]));
    let results = std::thread::scope(|scope| {
        let run = || {
            cli(
                &home,
                &[
                    "tasks",
                    "checkpoint",
                    id,
                    "--session",
                    "source-1",
                    "--expected-generation",
                    "1",
                    "--from",
                    input.to_str().unwrap(),
                ],
            )
        };
        let first = scope.spawn(run);
        let second = scope.spawn(run);
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.status.success()).count(), 1);
    let shown = cli(&home, &["tasks", "show", id]);
    let task: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(task["generation"], 2);
}

#[test]
fn preflight_binds_checkpoint_to_workspace_and_explicit_destination_policy() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let source = home.path().join("project/source.txt");
    std::fs::write(&source, "unfinished work").unwrap();
    let input = checkpoint(&home, json!([]));
    let saved = cli(
        &home,
        &[
            "tasks",
            "checkpoint",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--capture-workspace",
            "--from",
            input.to_str().unwrap(),
        ],
    );
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let before = cli(
        &home,
        &[
            "tasks",
            "check-handoff",
            id,
            "--to",
            "grok",
            "--model",
            "test-model",
        ],
    );
    assert!(
        before.status.success(),
        "{}",
        String::from_utf8_lossy(&before.stderr)
    );
    let before: Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(before["workspace_matches_checkpoint"], true);
    assert_eq!(
        before["sharing_allowed"], false,
        "absence of policy must not imply consent"
    );
    let policy = home.path().join("policy.json");
    write_json(
        &policy,
        &json!({
            "destinations":[{"tool":"grok", "models":["test-model"]}],
            "share_checkpoint":true, "share_workspace":true, "share_resource_metadata":true
        }),
    );
    let allowed = cli(
        &home,
        &[
            "tasks",
            "policy",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "2",
            "--from",
            policy.to_str().unwrap(),
        ],
    );
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let check = cli(
        &home,
        &[
            "tasks",
            "check-handoff",
            id,
            "--to",
            "grok",
            "--model",
            "test-model",
        ],
    );
    let check: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(check["sharing_allowed"], true);
    assert_eq!(check["consent_authenticated"], false);
    assert_eq!(check["authority_mode"], "unmanaged_checkpoint_only");
    assert_eq!(check["workspace_matches_checkpoint"], true);
    assert_eq!(
        check["handoff_ready"], false,
        "preflight does not prove source release or native receipts"
    );
    let different = cli(
        &home,
        &[
            "tasks",
            "check-handoff",
            id,
            "--to",
            "grok",
            "--model",
            "other-model",
        ],
    );
    let different: Value = serde_json::from_slice(&different.stdout).unwrap();
    assert_eq!(different["sharing_allowed"], false);
    std::fs::write(&source, "changed after compaction").unwrap();
    let changed = cli(
        &home,
        &[
            "tasks",
            "check-handoff",
            id,
            "--to",
            "grok",
            "--model",
            "test-model",
        ],
    );
    let changed: Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(changed["workspace_matches_checkpoint"], false);
    assert_eq!(changed["handoff_ready"], false);
    let shown = cli(&home, &["tasks", "show", id]);
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(
        shown["generation"], 3,
        "read-only preflight must not mutate task state"
    );
    assert!(!shown.to_string().contains("unfinished work"));
    std::fs::rename(
        home.path().join("project"),
        home.path().join("unavailable-project"),
    )
    .unwrap();
    let cached = cli(&home, &["tasks", "show", id]);
    assert!(
        cached.status.success(),
        "cached task reads must not need a live workspace"
    );
    let missing = cli(
        &home,
        &[
            "tasks",
            "check-handoff",
            id,
            "--to",
            "grok",
            "--model",
            "test-model",
        ],
    );
    assert!(missing.status.success());
    let missing: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing["workspace_matches_checkpoint"], Value::Null);
    assert_eq!(missing["handoff_ready"], false);
}

#[test]
fn sharing_policy_updates_are_revision_guarded_and_can_revoke_permission() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let policy = home.path().join("policy.json");
    write_json(
        &policy,
        &json!({
            "destinations":[{"tool":"grok", "models":["*"]}],
            "share_checkpoint":true, "share_workspace":true, "share_resource_metadata":true
        }),
    );
    let first = cli(
        &home,
        &[
            "tasks",
            "policy",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--from",
            policy.to_str().unwrap(),
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let stale = cli(
        &home,
        &[
            "tasks",
            "policy",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--from",
            policy.to_str().unwrap(),
        ],
    );
    assert!(!stale.status.success());
    write_json(
        &policy,
        &json!({
            "destinations":[], "share_checkpoint":false, "share_workspace":false, "share_resource_metadata":false
        }),
    );
    let revoked = cli(
        &home,
        &[
            "tasks",
            "policy",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "2",
            "--from",
            policy.to_str().unwrap(),
        ],
    );
    assert!(revoked.status.success());
    let check = cli(
        &home,
        &[
            "tasks",
            "check-handoff",
            id,
            "--to",
            "grok",
            "--model",
            "any-model",
        ],
    );
    assert!(check.status.success());
    let check: Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(check["sharing_allowed"], false);
}

#[cfg(unix)]
#[test]
fn replacing_workspace_with_symlink_cannot_commit_an_unreadable_checkpoint() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let original = home.path().join("project");
    let moved = home.path().join("moved-project");
    std::fs::rename(&original, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &original).unwrap();
    let input = checkpoint(&home, json!([]));
    let attempted = cli(
        &home,
        &[
            "tasks",
            "checkpoint",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--capture-workspace",
            "--from",
            input.to_str().unwrap(),
        ],
    );
    assert!(!attempted.status.success());
    let shown = cli(&home, &["tasks", "show", id]);
    assert!(
        shown.status.success(),
        "failed capture must leave a readable task"
    );
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["generation"], 1);
    assert_eq!(shown["latest_checkpoint"], Value::Null);
}

#[test]
fn workspace_metadata_is_bound_to_the_checkpoint_digest() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let input = checkpoint(&home, json!([]));
    let saved = cli(
        &home,
        &[
            "tasks",
            "checkpoint",
            id,
            "--session",
            "source-1",
            "--expected-generation",
            "1",
            "--capture-workspace",
            "--from",
            input.to_str().unwrap(),
        ],
    );
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let path = home.path().join(".clauth/tasks").join(id).join("task.json");
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["latest_checkpoint"]["workspace_snapshot"]["bytes"] = json!(42);
    write_json(&path, &record);
    let rejected = cli(&home, &["tasks", "show", id]);
    assert!(
        !rejected.status.success(),
        "workspace metadata must not be mutable outside the checkpoint digest"
    );
}

#[test]
fn execution_commands_check_task_actor_and_revision_before_launch_or_stop() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let wrong = cli(
        &home,
        &[
            "tasks",
            "run",
            id,
            "--target",
            "codex",
            "--session",
            "wrong-session",
            "--expected-generation",
            "1",
            "--",
            "--version",
        ],
    );
    assert!(!wrong.status.success());
    assert!(
        String::from_utf8_lossy(&wrong.stderr).contains("actor does not match"),
        "{}",
        String::from_utf8_lossy(&wrong.stderr)
    );
    let stale = cli(
        &home,
        &[
            "tasks",
            "run",
            id,
            "--target",
            "codex",
            "--session",
            "source-1",
            "--expected-generation",
            "0",
            "--",
            "--version",
        ],
    );
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale task generation"));
    let stop = cli(
        &home,
        &[
            "tasks",
            "stop-execution",
            id,
            "--execution-id",
            "00000000000000000000000000000000",
            "--session",
            "wrong-session",
            "--expected-generation",
            "1",
        ],
    );
    assert!(!stop.status.success());
    assert!(String::from_utf8_lossy(&stop.stderr).contains("actor does not match"));
    assert!(
        !home
            .path()
            .join(".clauth/tasks")
            .join(id)
            .join("execution.json")
            .exists()
    );
}

#[test]
fn execution_launch_rejects_disabled_wrong_tool_and_monitoring_only_targets() {
    let home = TempDir::new().unwrap();
    let task = new_task(&home);
    let id = task["task_id"].as_str().unwrap();
    let config = home.path().join(".clauth/providers.toml");
    for (toml, message) in [
        (
            "[[targets]]\nid='test'\nprovider='codex'\nenabled=false\n",
            "disabled",
        ),
        (
            "[[targets]]\nid='test'\nprovider='grok'\n",
            "must match the registered source tool",
        ),
        (
            "[[targets]]\nid='test'\nprovider='codex'\nauth_file='/not-a-native-login'\n",
            "monitoring-only",
        ),
    ] {
        std::fs::write(&config, toml).unwrap();
        let result = cli(
            &home,
            &[
                "tasks",
                "run",
                id,
                "--target",
                "test",
                "--session",
                "source-1",
                "--expected-generation",
                "1",
                "--",
                "--version",
            ],
        );
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert!(
        !home
            .path()
            .join(".clauth/tasks")
            .join(id)
            .join("execution.json")
            .exists()
    );
}
