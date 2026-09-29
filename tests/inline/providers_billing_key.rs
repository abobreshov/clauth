#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Monitoring-credential env references: the name validation, the
//! `config.toml` read (and its survival across the canonical rewrite), the
//! scheduler target that carries the name, and the scrub that keeps every
//! referenced variable out of session spawns. No test sets or reads a real
//! key: the values used are env-var NAMES, and `resolve` is only exercised on
//! names that are invalid or certainly unset.

use super::*;

use crate::harness::{ClaudeEngine, CodexEngine, HarnessEngine};
use crate::profile::{AppState, Profile, ProfileName, save_app_state, save_profile};
use crate::testutil::{HomeSandbox, env_overrides};

const OR_URL: &str = "https://openrouter.ai/api";

fn config_path(name: &str) -> std::path::PathBuf {
    crate::profile::profile_subpath(&ProfileName::from(name), "config.toml").unwrap()
}

/// Save `profiles` and register them, then give each named one a
/// `billing_key_env` line at the top of its `config.toml`.
fn seed(profiles: &[(&str, Option<&str>, Option<&str>)]) {
    let mut names = Vec::new();
    for (name, base_url, env) in profiles {
        let p = Profile::new(
            name.to_string(),
            base_url.map(str::to_string),
            Some("sk-or-v1-placeholder-not-a-key".to_string()),
        );
        save_profile(&p).unwrap();
        names.push(ProfileName::from(*name));
        if let Some(env) = env {
            let path = config_path(name);
            let body = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&path, format!("billing_key_env = \"{env}\"\n{body}")).unwrap();
        }
    }
    save_app_state(&AppState {
        profiles: names,
        ..AppState::default()
    })
    .unwrap();
}

#[test]
fn env_names_must_be_portable_and_unmanaged() {
    for good in ["OPENROUTER_MGMT_KEY", "_X", "or_key_2", "A"] {
        assert!(valid_env_name(good), "{good}");
    }
    for bad in [
        "",
        "2KEY",
        "MY-KEY",
        "HAS SPACE",
        "sk-or-v1-abcdef0123456789",
        "PATH=/x",
        // A tollgate-managed key is already owned by the profile's fields.
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
    ] {
        assert!(!valid_env_name(bad), "{bad}");
    }
    assert!(!valid_env_name(&"K".repeat(129)));
}

#[test]
fn the_config_key_is_read_by_name_only() {
    assert_eq!(
        env_name_from_config("billing_key_env = \"OR_MGMT\"\nbase_url = \"x\"\n").as_deref(),
        Some("OR_MGMT")
    );
    assert_eq!(
        env_name_from_config("billing_key_env = \"  OR_MGMT  \"\n").as_deref(),
        Some("OR_MGMT")
    );
    for raw in [
        "",
        "base_url = \"x\"\n",
        "billing_key_env = 5\n",
        "billing_key_env = \"sk-or-v1-pasted-value-not-a-name\"\n",
        "billing_key_env = \"\"\n",
        "not toml ===",
        // Inside a table it is not the profile's top-level key.
        "[console]\nbilling_key_env = \"OR_MGMT\"\n",
    ] {
        assert_eq!(env_name_from_config(raw), None, "{raw:?}");
    }
}

#[test]
fn resolve_never_reads_an_invalid_or_unset_name() {
    assert_eq!(resolve("NOT A NAME"), None);
    assert_eq!(resolve("ANTHROPIC_AUTH_TOKEN"), None);
    assert_eq!(
        resolve("TOLLGATE_TEST_BILLING_KEY_CERTAINLY_UNSET_7F3A9C"),
        None
    );
}

#[test]
fn the_key_survives_the_canonical_config_rewrite() {
    let _home = HomeSandbox::new();
    seed(&[("or", Some(OR_URL), Some("OR_MGMT"))]);
    let name = ProfileName::from("or");
    assert_eq!(billing_key_env(&name).as_deref(), Some("OR_MGMT"));
    // `load_profile` rewrites config.toml when it drifts from the canonical
    // render; the unmodelled key is carried across, not dropped.
    crate::profile::load_profile(&name).unwrap();
    save_profile(&crate::profile::load_profile(&name).unwrap()).unwrap();
    assert_eq!(billing_key_env(&name).as_deref(), Some("OR_MGMT"));
    let body = std::fs::read_to_string(config_path("or")).unwrap();
    assert!(body.contains("billing_key_env = \"OR_MGMT\""), "{body}");
    assert_eq!(
        body.matches("billing_key_env").count(),
        1,
        "carried once, never duplicated"
    );
}

#[test]
fn referenced_env_vars_collects_every_profile_once() {
    let _home = HomeSandbox::new();
    seed(&[
        ("or-a", Some(OR_URL), Some("OR_MGMT")),
        ("or-b", Some(OR_URL), Some("OR_MGMT")),
        ("or-c", Some(OR_URL), Some("OTHER_ORG_MGMT")),
        ("plain", None, None),
    ]);
    assert_eq!(referenced_env_vars(), ["OR_MGMT", "OTHER_ORG_MGMT"]);
}

#[test]
fn referenced_env_vars_is_empty_without_profiles() {
    let _home = HomeSandbox::new();
    assert!(referenced_env_vars().is_empty());
}

#[test]
fn every_session_spawn_scrubs_the_referenced_billing_vars() {
    let _home = HomeSandbox::new();
    seed(&[
        ("or-a", Some(OR_URL), Some("OR_MGMT")),
        ("or-c", Some(OR_URL), Some("OTHER_ORG_MGMT")),
    ]);
    let engines: [&dyn HarnessEngine; 2] = [&ClaudeEngine, &CodexEngine];
    for engine in engines {
        let mut cmd = std::process::Command::new("probe");
        cmd.env("OR_MGMT", "placeholder").env("UNRELATED", "1");
        engine.scrub_env(&mut cmd, &[]);
        let env = env_overrides(&cmd);
        assert_eq!(env.get("OR_MGMT"), Some(&None), "explicit value scrubbed");
        assert_eq!(
            env.get("OTHER_ORG_MGMT"),
            Some(&None),
            "an inherited value is removed too"
        );
        assert_eq!(env.get("UNRELATED"), Some(&Some("1".to_string())));
    }
}

#[test]
fn the_openrouter_target_carries_the_env_name_and_others_do_not() {
    let _home = HomeSandbox::new();
    seed(&[
        ("or", Some(OR_URL), Some("OR_MGMT")),
        (
            "ds",
            Some("https://api.deepseek.com/anthropic"),
            Some("OR_MGMT"),
        ),
    ]);
    let profiles: Vec<Profile> = ["or", "ds"]
        .iter()
        .map(|n| crate::profile::load_profile(&ProfileName::from(*n)).unwrap())
        .collect();
    let entries = crate::usage::collect_third_party_entries(&profiles);
    let env_of = |name: &str| {
        let e = entries.iter().find(|e| e.name.as_str() == name).unwrap();
        match &e.target {
            crate::providers::ThirdPartyTarget::Known {
                billing_key_env, ..
            } => billing_key_env.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(env_of("or").as_deref(), Some("OR_MGMT"));
    assert_eq!(env_of("ds"), None, "only OpenRouter reads a management key");

    // The name never moves the inference key's fingerprint.
    let mut bare = profiles[0].clone();
    bare.name = ProfileName::from("or-bare");
    assert_eq!(
        crate::usage::profile_credential_fingerprint(&profiles[0]),
        crate::usage::profile_credential_fingerprint(&bare)
    );
}
