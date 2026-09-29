#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::testutil::HomeSandbox;
#[test]
fn detect_lists_grok_auth_entries_without_values() {
    let _home = HomeSandbox::new();
    let home = crate::profile::home_dir().unwrap();
    std::fs::create_dir_all(home.join(".grok")).unwrap();
    std::fs::write(home.join(".grok/auth.json"),r#"{"https://auth.x.ai::client-a":{"key":"TOKEN-CANARY","expires_at":1900000000,"refresh_token":"REFRESH-CANARY"},"https://auth.x.ai::client-b":{"key":"CANARY-B"}}"#).unwrap();
    let rows = discover(&home, &BTreeSet::new(), &[], true);
    assert_eq!(rows.len(), 2);
    let dump = serde_json::to_string(&rows).unwrap();
    assert!(dump.contains("client-a"));
    assert!(dump.contains("epoch_s"));
    assert!(!dump.contains("TOKEN-CANARY"));
    assert!(!dump.contains("REFRESH-CANARY"));
}
#[test]
fn detect_proposes_codex_native_only_for_a_regular_file() {
    let _home = HomeSandbox::new();
    let home = crate::profile::home_dir().unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join("target"), "{}").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(home.join("target"), home.join(".codex/auth.json")).unwrap();
        let rows = discover(&home, &BTreeSet::new(), &[], false);
        assert_eq!(rows[0].state, "skipped");
        std::fs::remove_file(home.join(".codex/auth.json")).unwrap();
    }
    std::fs::write(home.join(".codex/auth.json"), "{}").unwrap();
    let rows = discover(&home, &BTreeSet::new(), &[], false);
    assert_eq!(rows[0].preset, "codex-native");
}
#[test]
fn detect_maps_stored_names_to_presets_and_skips_configured_fingerprints() {
    let _home = HomeSandbox::new();
    let home = crate::profile::home_dir().unwrap();
    let names = BTreeSet::from(["OPENAI_API_KEY".into(), "GOOGLE_API_KEY".into()]);
    let rows = discover(&home, &names, &[preset("openai").unwrap()], false);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].state, "monitored");
    assert_eq!(rows[1].preset, "google-ai");
    assert_eq!(rows[1].flags, ["--api-key-env", "GOOGLE_API_KEY"]);
}
#[test]
fn detect_apply_needs_yes_off_a_tty() {
    let _home = HomeSandbox::new();
    if !std::io::stdin().is_terminal() {
        assert!(
            run(false, false, true, false)
                .unwrap_err()
                .to_string()
                .contains("--yes")
        );
    }
}
#[cfg(unix)]
#[test]
fn detect_makes_no_network_call_and_spawns_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let home = crate::profile::home_dir().unwrap();
    std::fs::create_dir_all(home.join(".local/bin")).unwrap();
    let marker = home.join("spawned");
    let fake = home.join(".local/bin/agy");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    run(true, true, false, false).unwrap();
    assert!(!marker.exists());
}
#[test]
fn auth_shape_parser_skips_refresh_and_unknown_values() {
    let shape: SafeShape=serde_json::from_str(r#"{"tokens":{"access_token":"TOKEN-CANARY","refresh_token":{"secret":"REFRESH-CANARY"},"account_id":"ACCOUNT-CANARY"},"providers":{"nous":{"access_token":"NOUS-CANARY","expires_at":"2030-01-01T00:00:00Z","refresh_token":"REFRESH-CANARY"}},"unknown":{"secret":"UNKNOWN-CANARY"}}"#).unwrap();
    let dump = shape.0.to_string();
    assert!(!dump.contains("CANARY"));
    assert_eq!(shape.0["tokens"]["refresh_token"], Value::Null);
    assert_eq!(shape.0["unknown"], Value::Null);
    assert_eq!(expiry(&shape.0["providers"]["nous"]), "rfc3339");
}
