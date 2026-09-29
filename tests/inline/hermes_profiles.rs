#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `hermes-profiles.toml`: the codex roster's contract for a third file.

use super::*;
use crate::testutil::HomeSandbox;

fn write_roster(body: &str) {
    let dir = crate::profile::tollgate_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    std::fs::write(dir.join("hermes-profiles.toml"), body).unwrap();
}

fn entry(name: &str) -> HermesProfile {
    HermesProfile {
        name: name.to_string(),
        provider: Provider::Openrouter,
        model: Some("anthropic/claude-sonnet-4.5".to_string()),
        mode: Mode::Account,
        auth: Auth::Env,
        key_env: Some("OPENROUTER_API_KEY".to_string()),
        key_fingerprint: Some("sha256:0123456789abcdef".to_string()),
        created_at: "2026-09-29T12:00:00Z".to_string(),
    }
}

/// Test 3: a missing file is an empty roster; the spec's example file reads
/// back field for field; an unknown key is tolerated on load and dropped on
/// the next rewrite; a no-op update leaves bytes and mtime alone.
#[test]
fn hermes_state_roundtrip_drops_unknown_keys_and_skips_noop_save() {
    let _home = HomeSandbox::new();
    assert_eq!(HermesState::load().unwrap(), HermesState::default());

    write_roster(
        "schema_version = 1\nfrom_the_future = true\n[settings]\nbin = \"/abs/bin/hermes\"\n\
         version_policy = \"refuse\"\n[[profiles]]\nname = \"or-main\"\nprovider = \"openrouter\"\n\
         model = \"anthropic/claude-sonnet-4.5\"\nmode = \"account\"\nauth = \"env\"\n\
         key_env = \"OPENROUTER_API_KEY\"\nkey_fingerprint = \"sha256:0123456789abcdef\"\n\
         created_at = \"2026-09-29T12:00:00Z\"\nnew_field = 3\n",
    );
    let state = HermesState::load().unwrap();
    assert_eq!(state.profiles(), [entry("or-main")]);
    assert_eq!(
        state.settings().bin.as_deref(),
        Some(std::path::Path::new("/abs/bin/hermes"))
    );
    assert_eq!(state.settings().version_policy, Some(VersionPolicy::Refuse));
    assert_eq!(state.canonical_name("OR-MAIN").as_deref(), Some("or-main"));

    // A no-op update: nothing is rewritten, the unknown keys survive on disk.
    let path = hermes_state_path().unwrap();
    let before = (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    HermesState::update(|_| Ok(())).unwrap();
    let after = (
        std::fs::read(&path).unwrap(),
        std::fs::metadata(&path).unwrap().modified().unwrap(),
    );
    assert_eq!(
        before, after,
        "a no-op update must leave bytes and mtime alone"
    );

    // A real change rewrites through the serializer: the unknown keys go.
    HermesState::update(|s| {
        s.set_fingerprint("or-main", "sha256:fedcba9876543210");
        Ok(())
    })
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("from_the_future") && !text.contains("new_field"),
        "{text}"
    );
    let reread = HermesState::load().unwrap();
    assert_eq!(
        reread.find("or-main").unwrap().key_fingerprint.as_deref(),
        Some("sha256:fedcba9876543210")
    );
    assert_eq!(
        reread.settings().version_policy,
        Some(VersionPolicy::Refuse)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

/// The key's value is never a roster field: only its fingerprint is.
#[test]
fn the_roster_never_serializes_a_key_value() {
    let _home = HomeSandbox::new();
    HermesState::update(|s| {
        s.add_profile(entry("or-main"));
        Ok(())
    })
    .unwrap();
    let text = std::fs::read_to_string(hermes_state_path().unwrap()).unwrap();
    assert!(text.contains("key_fingerprint = \"sha256:0123456789abcdef\""));
    assert!(!text.contains("sk-"), "{text}");
}

#[test]
fn another_schema_version_refuses_the_load() {
    let _home = HomeSandbox::new();
    write_roster("schema_version = 2\n");
    let err = HermesState::load().unwrap_err();
    assert!(err.to_string().contains("schema_version 2"), "{err}");
}
