#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The claude engine is delegation — behavior is gated by the flows that run
//! through it (the switch and spawn suites) — so what is pinned here are the
//! wire facts a seam must not let drift.

use super::*;

#[test]
fn the_claude_engine_carries_the_spawn_facts_unchanged() {
    let engine: &dyn HarnessEngine = &ClaudeEngine;
    assert_eq!(engine.home_env_key(), "CLAUDE_CONFIG_DIR");
    assert_eq!(
        engine.command().get_program(),
        crate::runtime::claude_command().get_program(),
        "one command resolution behind the seam, not a second spelling"
    );
}

/// The scrub through the seam is the shared scrub: managed keys and the
/// active profile's custom keys are dropped from the child env, everything
/// else inherits.
#[test]
fn the_claude_scrub_is_the_shared_scrub() {
    // The scrub reads the configured profiles' monitoring-credential env
    // names (`providers::billing_key`), so it needs a home.
    let _home = crate::testutil::HomeSandbox::new();
    let engine: &dyn HarnessEngine = &ClaudeEngine;
    let mut cmd = std::process::Command::new("probe");
    cmd.env("ANTHROPIC_BASE_URL", "https://a")
        .env("UNRELATED", "1");
    engine.scrub_env(&mut cmd, &["MY_CUSTOM".to_string()]);

    let env = crate::testutil::env_overrides(&cmd);
    assert_eq!(
        env.get("ANTHROPIC_BASE_URL"),
        Some(&None),
        "a managed key is scrubbed even when explicitly set"
    );
    assert_eq!(
        env.get("MY_CUSTOM"),
        Some(&None),
        "the active profile's custom key is scrubbed from the inherited env"
    );
    assert_eq!(
        env.get("UNRELATED"),
        Some(&Some("1".to_string())),
        "an unmanaged key rides through untouched"
    );
}

#[test]
fn the_codex_engine_carries_its_own_spawn_facts() {
    let engine: &dyn HarnessEngine = &CodexEngine;
    assert_eq!(engine.home_env_key(), "CODEX_HOME");
    assert_eq!(
        engine.command().get_program(),
        crate::runtime::codex_command().get_program()
    );
    let err = engine
        .install_credentials("cx")
        .expect_err("a codex switch installs nothing — the seam refuses by contract");
    assert_eq!(
        err.to_string(),
        "codex profile 'cx' installs nothing at switch — sessions bind auth.json at start"
    );
}

/// The codex scrub drops its managed keys and the claude actives, and strips
/// an inherited CLAUDE_CONFIG_DIR only when it names a tree tollgate built — an
/// operator's own custom dir is not tollgate's to strip.
#[test]
fn the_codex_scrub_is_managed_keys_plus_tollgate_runtime_hygiene() {
    let home = crate::testutil::HomeSandbox::new();
    let engine: &dyn HarnessEngine = &CodexEngine;

    let runtime_dir = home
        .home()
        .join(".tollgate/profiles/started/runtime-4242-0");
    {
        let _env = crate::testutil::ConfigDirSandbox::new(&home, &runtime_dir);
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &["A_KEY".to_string()]);
        let env = crate::testutil::env_overrides(&cmd);
        assert_eq!(env.get("CODEX_HOME"), Some(&None));
        assert_eq!(env.get("OPENAI_API_KEY"), Some(&None));
        assert_eq!(
            env.get("CODEX_SQLITE_HOME"),
            Some(&None),
            "an inherited state-DB home would pool every profile's DBs in one dir"
        );
        for carrier in ["CODEX_API_KEY", "CODEX_ACCESS_TOKEN"] {
            assert_eq!(
                env.get(carrier),
                Some(&None),
                "{carrier} outranks the linked auth.json in codex's own load_auth order"
            );
        }
        for endpoint in [
            "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
            "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
            "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
        ] {
            assert_eq!(
                env.get(endpoint),
                Some(&None),
                "{endpoint} would spend the profile's single-use chain elsewhere"
            );
        }
        assert_eq!(env.get("A_KEY"), Some(&None));
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR"),
            Some(&None),
            "an inherited tollgate runtime claim is scrubbed from a codex spawn"
        );
    }
    {
        let _env = crate::testutil::ConfigDirSandbox::new(&home, &home.home().join("custom-dir"));
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &[]);
        let env = crate::testutil::env_overrides(&cmd);
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR"),
            None,
            "an operator's custom dir is left alone"
        );
    }
}

/// The claude engine's mirror hygiene: an inherited tollgate codex home is
/// scrubbed from a claude spawn, a foreign CODEX_HOME is not.
#[test]
fn the_claude_scrub_strips_only_a_tollgate_codex_home() {
    let home = crate::testutil::HomeSandbox::new();
    let engine: &dyn HarnessEngine = &ClaudeEngine;

    let codex_home = home.home().join(".tollgate/profiles/cx/codex-home-4242-0");
    {
        let _env = crate::testutil::CodexHomeSandbox::new(&home, &codex_home);
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &[]);
        assert_eq!(
            crate::testutil::env_overrides(&cmd).get("CODEX_HOME"),
            Some(&None),
            "a tollgate codex home claim is scrubbed from a claude spawn"
        );
    }
    {
        let _env = crate::testutil::CodexHomeSandbox::new(&home, &home.home().join(".codex"));
        let mut cmd = std::process::Command::new("probe");
        engine.scrub_env(&mut cmd, &[]);
        assert_eq!(
            crate::testutil::env_overrides(&cmd).get("CODEX_HOME"),
            None,
            "the operator's own CODEX_HOME is not tollgate's to strip"
        );
    }
}

/// Test 1: the Hermes tag is the lowercase `"hermes"` on the wire, and a row
/// written before the axis existed (no `harness` key) still reads as claude.
#[test]
fn harness_hermes_roundtrips_lowercase_and_old_rows_stay_readable() {
    assert_eq!(
        serde_json::to_string(&Harness::Hermes).unwrap(),
        "\"hermes\""
    );
    assert_eq!(
        serde_json::from_str::<Harness>("\"hermes\"").unwrap(),
        Harness::Hermes
    );
    assert_eq!(Harness::Hermes.as_str(), "hermes");
    assert_eq!(
        Harness::ALL,
        [Harness::Claude, Harness::Codex, Harness::Hermes],
        "the bare-name resolution order"
    );
    let old = serde_json::json!({
        "session_id": "4242-0",
        "start_profile": "work",
        "pid": 4242,
        "started_at": 0,
        "isolated": false,
    });
    let row: crate::live_sessions::LiveSession =
        serde_json::from_value(old).expect("a pre-axis row parses");
    assert_eq!(row.harness, Harness::Claude);
    let mut hermes = serde_json::to_value(&row).unwrap();
    hermes["harness"] = serde_json::json!("hermes");
    let back: crate::live_sessions::LiveSession = serde_json::from_value(hermes).unwrap();
    assert_eq!(back.harness, Harness::Hermes);
}

/// Test 2: a Hermes profile installs nothing at switch, by contract; both
/// install methods refuse and name Hermes.
#[test]
fn hermes_engine_install_credentials_bails() {
    let engine: &dyn HarnessEngine = Harness::Hermes.engine();
    assert_eq!(engine.home_env_key(), "HERMES_HOME");
    for err in [
        engine.install_credentials("or-main").unwrap_err(),
        engine.force_install_credentials("or-main").unwrap_err(),
    ] {
        let text = err.to_string();
        assert!(text.contains("Hermes profile 'or-main'"), "{text}");
        assert!(text.contains("relaunch"), "{text}");
    }
}
