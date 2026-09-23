//! Proposals are durable plans, not launch instructions or ownership transfers.
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

fn decoded(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write_json(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

struct Fixture {
    home: TempDir,
    record: Value,
}

impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let workspace = home.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("source.txt"), "unfinished implementation").unwrap();
        let input = home.path().join("register.json");
        write_json(
            &input,
            &json!({
                "objective":"Finish implementation using a portable checkpoint",
                "workspace":workspace,
                "constraints":["Preserve unrelated work"],
                "source":{"tool":"codex", "model":"source-model", "native_session_id":"source-1", "account_ref":null}
            }),
        );
        let record = decoded(cli(
            &home,
            &["tasks", "register", "--from", input.to_str().unwrap()],
        ));
        Self { home, record }
    }

    fn ready() -> Self {
        let mut fixture = Self::new();
        fixture.checkpoint(true, &[]);
        fixture.policy(allow_policy());
        fixture
    }

    fn id(&self) -> &str {
        self.record["task_id"].as_str().unwrap()
    }

    fn generation(&self) -> u64 {
        self.record["generation"].as_u64().unwrap()
    }

    fn show(&self) -> Value {
        decoded(cli(&self.home, &["tasks", "show", self.id()]))
    }

    fn mutate(&mut self, command: &str, input: Value, capture: bool) {
        let path = self.home.path().join("mutation.json");
        write_json(&path, &input);
        let generation = self.generation().to_string();
        let mut args = vec![
            "tasks",
            command,
            self.id(),
            "--session",
            "source-1",
            "--expected-generation",
            &generation,
            "--from",
            path.to_str().unwrap(),
        ];
        if capture {
            args.push("--capture-workspace");
        }
        self.record = decoded(cli(&self.home, &args));
    }

    fn checkpoint(&mut self, capture: bool, resource_ids: &[&str]) {
        self.mutate(
            "checkpoint",
            json!({
                "brief":"Partial implementation is ready for review",
                "completed":["Implemented the initial slice"],
                "remaining_plan":["Review and run tests"],
                "decisions":["Keep the worktree"], "uncertainties":[],
                "next_action":"Inspect the current implementation",
                "constraints":["Preserve unrelated work"], "resource_ids":resource_ids
            }),
            capture,
        );
    }

    fn policy(&mut self, policy: Value) {
        self.mutate("policy", policy, false);
    }

    fn resource(&mut self) {
        self.mutate("resource", json!({
            "id":"test-pane", "kind":"herdr_pane",
            "native_identity":{"instance":"local", "session":"main", "workspace_id":"w9", "tab_id":"w9:t2", "pane_id":"w9:p7"},
            "purpose":"Inspect existing tests", "ownership":"task", "disposition":"adopt", "may_write":false,
            "reconnect":"Inspect the named pane after verifying its identity",
            "cleanup":format!("touch {}", self.home.path().join("must-not-execute").display())
        }), false);
    }

    fn propose(&self, key: &str, tool: &str, model: &str, actor: &str, generation: u64) -> Output {
        cli(
            &self.home,
            &[
                "tasks",
                "propose-handoff",
                self.id(),
                "--request-id",
                key,
                "--to",
                tool,
                "--model",
                model,
                "--session",
                actor,
                "--expected-generation",
                &generation.to_string(),
            ],
        )
    }

    fn propose_current(&mut self, key: &str) {
        self.record =
            decoded(self.propose(key, "grok", "test-model", "source-1", self.generation()));
    }

    fn cancel(&self, key: &str, actor: &str, generation: u64) -> Output {
        cli(
            &self.home,
            &[
                "tasks",
                "cancel-handoff",
                self.id(),
                "--handoff-id",
                key,
                "--session",
                actor,
                "--expected-generation",
                &generation.to_string(),
            ],
        )
    }

    fn assert_rejected_unchanged(&self, output: Output) {
        assert!(!output.status.success(), "operation unexpectedly succeeded");
        assert_eq!(
            self.show(),
            self.record,
            "rejection must not mutate the record"
        );
    }
}

fn allow_policy() -> Value {
    json!({
        "destinations":[{"tool":"grok", "models":["test-model"]}],
        "share_checkpoint":true, "share_workspace":true, "share_resource_metadata":true
    })
}

fn proposal(record: &Value) -> &Value {
    record["handoffs"].as_array().unwrap().last().unwrap()
}

#[test]
fn source_release_requires_a_recorded_execution_and_does_not_launch_one() {
    let mut fixture = Fixture::ready();
    fixture.propose_current("release-attempt");
    let output = cli(
        &fixture.home,
        &[
            "tasks",
            "release-handoff-source",
            fixture.id(),
            "--handoff-id",
            "release-attempt",
            "--execution-id",
            "00000000000000000000000000000000",
            "--session",
            "source-1",
            "--expected-generation",
            &fixture.generation().to_string(),
        ],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!error.contains("unrecognized subcommand"), "{error}");
    assert!(error.contains("execution"), "{error}");
    assert_eq!(fixture.show(), fixture.record);
    assert!(
        !fixture
            .home
            .path()
            .join(".clauth/tasks")
            .join(fixture.id())
            .join("execution.json")
            .exists()
    );
}

#[test]
fn competing_processes_publish_only_one_active_proposal() {
    let fixture = Fixture::ready();
    let generation = fixture.generation().to_string();
    let spawn = |key: &str| {
        Command::new(env!("CARGO_BIN_EXE_clauth"))
            .env("HOME", fixture.home.path())
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("HERDR_ENV")
            .args([
                "tasks",
                "propose-handoff",
                fixture.id(),
                "--request-id",
                key,
                "--to",
                "grok",
                "--model",
                "test-model",
                "--session",
                "source-1",
                "--expected-generation",
                &generation,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn("first");
    let second = spawn("second");
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert_ne!(first.status.success(), second.status.success());
    let saved = fixture.show();
    assert_eq!(saved["generation"], fixture.generation() + 1);
    assert_eq!(saved["handoffs"].as_array().unwrap().len(), 1);
    assert_eq!(proposal(&saved)["state"], "proposed");
    assert_eq!(saved["owner"], fixture.record["owner"]);
}

#[test]
fn proposal_roundtrips_without_launch_or_ownership_claims_and_show_is_cached() {
    let mut fixture = Fixture::ready();
    assert!(fixture.record.get("handoffs").is_none());
    let before = fixture.record.clone();
    fixture.propose_current("request_1-A");
    let saved = &fixture.record;
    let handoff = proposal(saved);
    assert_eq!(
        saved["generation"],
        before["generation"].as_u64().unwrap() + 1
    );
    assert_eq!(handoff["handoff_id"], "request_1-A");
    assert_eq!(handoff["source_generation"], before["generation"]);
    assert_eq!(handoff["proposed_generation"], saved["generation"]);
    assert_eq!(handoff["source_epoch"], before["ownership_epoch"]);
    assert_eq!(handoff["source"], before["owner"]);
    assert_eq!(handoff["destination_tool"], "grok");
    assert_eq!(handoff["destination_model"], "test-model");
    assert_eq!(
        handoff["checkpoint_generation"],
        before["latest_checkpoint"]["generation"]
    );
    assert_eq!(
        handoff["checkpoint_digest"],
        before["latest_checkpoint"]["digest"]
    );
    assert_eq!(handoff["resource_revision"], before["resource_revision"]);
    assert!(!handoff["policy_digest"].as_str().unwrap().is_empty());
    assert_eq!(handoff["state"], "proposed");
    assert!(handoff["closed_generation"].is_null());
    for key in [
        "owner",
        "ownership_epoch",
        "authority_mode",
        "readiness",
        "resources",
        "latest_checkpoint",
    ] {
        assert_eq!(saved[key], before[key], "proposal must preserve {key}");
    }
    assert_eq!(saved["authority_mode"], "unmanaged_checkpoint_only");
    assert_eq!(saved["readiness"]["handoff_ready"], false);
    let history = saved["history"].as_array().unwrap();
    assert_eq!(
        history.len(),
        before["history"].as_array().unwrap().len() + 1
    );
    assert_eq!(history.last().unwrap()["kind"], "handoff_proposed");
    assert_eq!(history.last().unwrap()["generation"], saved["generation"]);
    assert_eq!(fixture.show(), *saved);
    assert!(
        !fixture
            .home
            .path()
            .join(".clauth/tasks")
            .join(fixture.id())
            .join("execution.json")
            .exists()
    );

    std::fs::rename(
        fixture.home.path().join("project"),
        fixture.home.path().join("unavailable-project"),
    )
    .unwrap();
    assert_eq!(
        fixture.show(),
        *saved,
        "show must not recapture the workspace"
    );
}

#[test]
fn exact_replay_is_idempotent_before_and_after_cancellation_without_reactivation() {
    let mut fixture = Fixture::ready();
    let original_generation = fixture.generation();
    fixture.propose_current("same-request");
    let replay = decoded(fixture.propose(
        "same-request",
        "grok",
        "test-model",
        "source-1",
        original_generation,
    ));
    assert_eq!(replay, fixture.record);
    fixture.assert_rejected_unchanged(fixture.propose(
        "another-request",
        "grok",
        "test-model",
        "source-1",
        fixture.generation(),
    ));
    let before_cancel = fixture.record.clone();
    fixture.record = decoded(fixture.cancel("same-request", "source-1", fixture.generation()));
    assert_eq!(
        fixture.generation(),
        before_cancel["generation"].as_u64().unwrap() + 1
    );
    assert_eq!(proposal(&fixture.record)["state"], "cancelled");
    assert_eq!(
        proposal(&fixture.record)["closed_generation"],
        fixture.record["generation"]
    );
    assert_eq!(
        fixture.record["history"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["kind"],
        "handoff_cancelled"
    );
    for key in ["owner", "ownership_epoch", "authority_mode", "readiness"] {
        assert_eq!(fixture.record[key], before_cancel[key]);
    }
    let replay = decoded(fixture.propose(
        "same-request",
        "grok",
        "test-model",
        "source-1",
        original_generation,
    ));
    assert_eq!(
        replay, fixture.record,
        "closed proposal must never reactivate"
    );
    fixture.propose_current("replacement");
    assert_eq!(fixture.record["handoffs"].as_array().unwrap().len(), 2);
    assert_eq!(fixture.record["handoffs"][0]["state"], "cancelled");
    assert_eq!(proposal(&fixture.record)["state"], "proposed");
}

#[test]
fn actor_generation_and_reused_request_arguments_are_guarded() {
    let mut fixture = Fixture::ready();
    let generation = fixture.generation();
    fixture.assert_rejected_unchanged(fixture.propose(
        "request",
        "grok",
        "test-model",
        "wrong-source",
        generation,
    ));
    fixture.assert_rejected_unchanged(fixture.propose(
        "request",
        "grok",
        "test-model",
        "source-1",
        generation - 1,
    ));
    fixture.propose_current("request");
    for (tool, model, actor, source_generation) in [
        ("agy", "test-model", "source-1", generation),
        ("grok", "other-model", "source-1", generation),
        ("grok", "test-model", "other-source", generation),
        ("grok", "test-model", "source-1", generation + 1),
    ] {
        fixture.assert_rejected_unchanged(fixture.propose(
            "request",
            tool,
            model,
            actor,
            source_generation,
        ));
    }
    fixture.assert_rejected_unchanged(fixture.cancel(
        "request",
        "wrong-source",
        fixture.generation(),
    ));
    fixture.assert_rejected_unchanged(fixture.cancel("request", "source-1", generation));
    fixture.assert_rejected_unchanged(fixture.cancel(
        "wrong-request",
        "source-1",
        fixture.generation(),
    ));
}

#[test]
fn request_ids_are_bounded_ascii_identifiers() {
    let mut fixture = Fixture::ready();
    for key in [
        "".to_owned(),
        "a".repeat(65),
        "with space".into(),
        "../path".into(),
        "ü".into(),
    ] {
        fixture.assert_rejected_unchanged(fixture.propose(
            &key,
            "grok",
            "test-model",
            "source-1",
            fixture.generation(),
        ));
    }
    let boundary = "a".repeat(64);
    fixture.propose_current(&boundary);
    assert_eq!(proposal(&fixture.record)["handoff_id"], boundary);
}

#[test]
fn captured_current_workspace_and_checkpoint_are_required() {
    let mut fixture = Fixture::new();
    fixture.policy(allow_policy());
    fixture.assert_rejected_unchanged(fixture.propose(
        "no-checkpoint",
        "grok",
        "test-model",
        "source-1",
        fixture.generation(),
    ));
    fixture.checkpoint(false, &[]);
    fixture.assert_rejected_unchanged(fixture.propose(
        "uncaptured",
        "grok",
        "test-model",
        "source-1",
        fixture.generation(),
    ));
    fixture.checkpoint(true, &[]);
    std::fs::write(
        fixture.home.path().join("project/source.txt"),
        "changed since capture",
    )
    .unwrap();
    fixture.assert_rejected_unchanged(fixture.propose(
        "changed",
        "grok",
        "test-model",
        "source-1",
        fixture.generation(),
    ));
    fixture.checkpoint(true, &[]);
    fixture.propose_current("repaired");
}

#[test]
fn explicit_destination_and_each_relevant_sharing_scope_are_required() {
    let mut fixture = Fixture::new();
    fixture.resource();
    fixture.checkpoint(true, &["test-pane"]);
    fixture.assert_rejected_unchanged(fixture.propose(
        "absent-policy",
        "grok",
        "test-model",
        "source-1",
        fixture.generation(),
    ));
    for field in [
        "share_checkpoint",
        "share_workspace",
        "share_resource_metadata",
    ] {
        let mut policy = allow_policy();
        policy[field] = false.into();
        fixture.policy(policy);
        fixture.assert_rejected_unchanged(fixture.propose(
            "denied-scope",
            "grok",
            "test-model",
            "source-1",
            fixture.generation(),
        ));
    }
    fixture.policy(allow_policy());
    fixture.assert_rejected_unchanged(fixture.propose(
        "wrong-model",
        "grok",
        "other-model",
        "source-1",
        fixture.generation(),
    ));
    fixture.assert_rejected_unchanged(fixture.propose(
        "wrong-tool",
        "agy",
        "test-model",
        "source-1",
        fixture.generation(),
    ));
    fixture.propose_current("allowed");
    assert!(
        !fixture.home.path().join("must-not-execute").exists(),
        "resource recipes remain inert data"
    );
}

#[test]
fn checkpoint_policy_and_resource_mutations_supersede_proposals_and_allow_repair() {
    for mutation in ["checkpoint", "policy", "resource"] {
        let mut fixture = Fixture::ready();
        let source_generation = fixture.generation();
        fixture.propose_current("original");
        let before = fixture.record.clone();
        match mutation {
            "checkpoint" => fixture.checkpoint(true, &[]),
            "policy" => {
                let mut revoked = allow_policy();
                revoked["destinations"] = json!([]);
                fixture.policy(revoked);
            }
            "resource" => fixture.resource(),
            _ => unreachable!(),
        }
        assert_eq!(
            fixture.generation(),
            before["generation"].as_u64().unwrap() + 1,
            "{mutation} advances only once"
        );
        assert_eq!(
            proposal(&fixture.record)["state"],
            "superseded",
            "{mutation}"
        );
        assert_eq!(
            proposal(&fixture.record)["closed_generation"],
            fixture.record["generation"]
        );
        assert_eq!(fixture.record["owner"], before["owner"]);
        assert_eq!(fixture.record["ownership_epoch"], before["ownership_epoch"]);
        let replay = decoded(fixture.propose(
            "original",
            "grok",
            "test-model",
            "source-1",
            source_generation,
        ));
        assert_eq!(
            replay, fixture.record,
            "superseded replay must not reevaluate or reactivate"
        );
        fixture.assert_rejected_unchanged(fixture.cancel(
            "original",
            "source-1",
            fixture.generation(),
        ));
        if mutation == "resource" {
            fixture.assert_rejected_unchanged(fixture.propose(
                "repair",
                "grok",
                "test-model",
                "source-1",
                fixture.generation(),
            ));
            fixture.checkpoint(true, &["test-pane"]);
        }
        if mutation == "policy" {
            fixture.assert_rejected_unchanged(fixture.propose(
                "repair",
                "grok",
                "test-model",
                "source-1",
                fixture.generation(),
            ));
            fixture.policy(allow_policy());
        }
        fixture.propose_current("repair");
        assert_eq!(fixture.record["handoffs"].as_array().unwrap().len(), 2);
        assert_eq!(fixture.record["handoffs"][0]["state"], "superseded");
        assert_eq!(proposal(&fixture.record)["state"], "proposed");
    }
}
