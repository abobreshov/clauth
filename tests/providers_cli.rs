//! The new provider surface must remain useful without the daemon or any login.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use tempfile::TempDir;

fn command(home: &TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_clauth"));
    cmd.env("HOME", home.path())
        .env_remove("HERDR_ENV")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

#[test]
fn init_never_overwrites_and_empty_credentials_are_unknown() {
    let home = TempDir::new().unwrap();
    let init = command(&home).args(["providers", "init"]).output().unwrap();
    assert!(init.status.success());
    let file = home.path().join(".clauth/providers.toml");
    let original = std::fs::read(&file).unwrap();
    let again = command(&home).args(["providers", "init"]).output().unwrap();
    assert!(!again.status.success());
    assert_eq!(std::fs::read(&file).unwrap(), original);
    let output = command(&home)
        .args(["providers", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let reports: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reports.as_array().unwrap().len(), 3);
    for report in reports.as_array().unwrap() {
        assert_eq!(report["state"], "not_fetched");
        assert!(report["observed_at_ms"].is_null());
        assert_eq!(report["data"]["buckets"], serde_json::json!([]));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn status_exposes_native_targets_without_changing_legacy_profiles() {
    let home = TempDir::new().unwrap();
    assert!(
        command(&home)
            .args(["providers", "init"])
            .status()
            .unwrap()
            .success()
    );
    let output = command(&home).args(["status", "--json"]).output().unwrap();
    assert!(output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["schema"], 2);
    assert_eq!(status["profiles"], serde_json::json!([]));
    assert_eq!(status["provider_accounts"].as_array().unwrap().len(), 3);
}

#[test]
fn invalid_configuration_is_actionable_and_does_not_echo_secrets() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".clauth")).unwrap();
    std::fs::write(
        home.path().join(".clauth/providers.toml"),
        "mistyped = 'TEST_SECRET_DO_NOT_ECHO'",
    )
    .unwrap();
    let output = command(&home)
        .args(["providers", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("providers.toml"));
    assert!(!error.contains("TEST_SECRET_DO_NOT_ECHO"));
}

#[test]
fn providers_text_lists_the_active_claude_account_and_json_stays_native() {
    let home = TempDir::new().unwrap();
    let clauth = home.path().join(".clauth");
    let profile = clauth.join("profiles").join("beta");
    std::fs::create_dir_all(&profile).unwrap();
    let profiles = clauth.join("profiles.toml");
    std::fs::write(
        &profiles,
        "active_profile = \"beta\"\nprofiles = [\"beta\"]\n",
    )
    .unwrap();
    std::fs::write(
        profile.join("usage_cache.json"),
        r#"{"five_hour":{"utilization":25.0},"fetched_at":5}"#,
    )
    .unwrap();
    let before = std::fs::read(&profiles).unwrap();

    let text_out = command(&home).args(["providers"]).output().unwrap();
    assert!(
        text_out.status.success(),
        "{}",
        String::from_utf8_lossy(&text_out.stderr)
    );
    let text = String::from_utf8(text_out.stdout).unwrap();
    assert!(text.contains("beta"), "{text}");
    assert!(text.to_lowercase().contains("claude"), "{text}");

    let json_out = command(&home)
        .args(["providers", "--json"])
        .output()
        .unwrap();
    assert!(
        json_out.status.success(),
        "{}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let json = String::from_utf8(json_out.stdout).unwrap();
    let reports: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(reports, serde_json::json!([]));
    assert!(!json.to_lowercase().contains("claude"), "{json}");
    assert!(!json.contains("beta"), "{json}");
    assert_eq!(std::fs::read(&profiles).unwrap(), before);
    assert!(!clauth.join("providers.toml").exists());

    if let Ok(dir) = std::env::var("CLAUTH_GOAL_SCRATCH")
        && !dir.is_empty()
    {
        std::fs::write(
            std::path::Path::new(&dir).join("providers-json.txt"),
            format!("--- text ---\n{text}\n--- json ---\n{json}\n"),
        )
        .unwrap();
    }
}

#[test]
fn herdr_launch_requires_caller_context_before_creating_any_pane() {
    let home = TempDir::new().unwrap();
    assert!(
        command(&home)
            .args(["providers", "init"])
            .status()
            .unwrap()
            .success()
    );
    let output = command(&home)
        .args(["providers", "start", "grok", "--herdr"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Herdr-managed pane"));
}
