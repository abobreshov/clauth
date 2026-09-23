//! Exercise real plugin scripts without any live Herdr connection.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn report(event: &str, pane: bool, live_agent: Option<&str>) -> (String, String) {
    let temp = tempfile::TempDir::new().unwrap();
    let herdr = temp.path().join("herdr");
    let clauth = temp.path().join("clauth");
    let snapshot = live_agent
        .map(|agent| serde_json::json!({"result":{"pane":{"agent":agent}}}).to_string())
        .unwrap_or_default();
    std::fs::write(&herdr, format!("#!/bin/sh\ncase \"$1:$2\" in\npane:get) printf '%s\\n' '{snapshot}';;\npane:report-metadata) printf '%s\\n' \"$*\" >> \"$HOME/reports\";;\npane:process-info) exit 1;;\nesac\n")).unwrap();
    std::fs::write(&clauth, "#!/bin/sh\ncase \"$1:$4\" in\nwhich:) echo claude-only-account;;\nherdr:pane_tag) echo on;;\nherdr:border_label) echo on;;\nesac\n").unwrap();
    for script in [&herdr, &clauth] {
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // A live claim prevents this one-shot script test starting a watcher.
    std::fs::write(
        temp.path().join("watch-fixture-pane.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    let output = Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/herdr-plugin/report-profile.sh"
        ))
        .env("HOME", temp.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                temp.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("HERDR_BIN_PATH", &herdr)
        .env("HERDR_PANE_ID", if pane { "fixture-pane" } else { "" })
        .env("HERDR_PLUGIN_ID", "clauth")
        .env("HERDR_PLUGIN_STATE_DIR", temp.path())
        .env("HERDR_PLUGIN_EVENT_JSON", event)
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"focused_pane_agent":"grok"}"#,
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8(output.stdout).unwrap(),
        std::fs::read_to_string(temp.path().join("reports")).unwrap_or_default(),
    )
}

#[test]
fn non_claude_events_clear_old_claude_labels_instead_of_leaving_them() {
    for agent in ["codex", "grok", "agy", "cursor"] {
        let (stdout, reports) = report(&format!(r#"{{"agent":"{agent}"}}"#), true, Some(agent));
        assert!(stdout.is_empty());
        assert!(
            reports.contains("--clear-token clauth"),
            "stale token must be cleared for {agent}: {reports}"
        );
        assert!(reports.contains("--clear-display-agent"));
        assert!(!reports.contains("claude-only-account"));
    }
}

#[test]
fn watcher_and_stale_claude_events_publish_only_claude_scoped_metadata() {
    for event in ["", r#"{"agent":"claude"}"#] {
        let (stdout, reports) = report(event, true, Some("claude"));
        assert_eq!(stdout, "claude-only-account\n");
        assert!(reports.contains("--token clauth=claude-only-account"));
        assert!(
            reports.contains("--agent claude"),
            "Herdr must constrain the label to the current Claude occupant: {reports}"
        );
    }
}

#[test]
fn non_claude_focused_action_does_not_report_the_global_claude_account() {
    let (stdout, reports) = report("", false, None);
    assert!(stdout.is_empty());
    assert!(reports.is_empty());
}

#[test]
fn stale_hook_and_empty_watcher_context_cannot_override_live_provider() {
    for event in ["", r#"{"agent":"claude"}"#] {
        for live_agent in [Some("codex"), Some("grok"), Some("agy"), None] {
            let (stdout, reports) = report(event, true, live_agent);
            assert!(stdout.is_empty());
            assert!(reports.contains("--clear-token clauth"));
            assert!(reports.contains("--clear-display-agent"));
            assert!(!reports.contains("claude-only-account"));
        }
    }
}
