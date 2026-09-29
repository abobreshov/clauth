#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Guest mode (plan §4.0, the "guest mode (pre-import)" coexistence row): with
//! upstream clauth's `~/.clauth` present and no completed import, tollgate
//! writes none of the global files upstream owns. Every test stages a fake
//! upstream install inside a `HomeSandbox`; with no `~/.clauth` the whole
//! existing suite runs unchanged, which is the other half of the contract.

use std::path::{Path, PathBuf};

use super::*;
use crate::profile::{AppConfig, AppState, ClaudeCredentials, OAuthToken, Profile, ProfileName};
use crate::testutil::{HomeSandbox, set_mtime, through_handle};

const UPSTREAM_CREDS: &[u8] =
    br#"{"claudeAiOauth":{"accessToken":"upstream-access","refreshToken":"upstream-refresh"}}"#;
const UPSTREAM_SETTINGS: &[u8] =
    br#"{"env":{"UPSTREAM":"1"},"enabledPlugins":{"clauth@clauth":true},"theme":"dark"}"#;
const UPSTREAM_CLAUDE_JSON: &[u8] = br#"{"oauthAccount":{"accountUuid":"upstream-uuid"},"mcpServers":{"clauth":{"command":"clauth"}},"numStartups":3}"#;
const UPSTREAM_CODEX_AUTH: &[u8] = br#"{"tokens":{"refresh_token":"upstream-codex"}}"#;

/// Stage upstream's install: its data dir, and the global files it owns.
fn stage_upstream(home: &Path) {
    std::fs::create_dir_all(home.join(UPSTREAM_DATA_DIR_NAME).join("profiles")).unwrap();
    let claude = home.join(".claude");
    std::fs::create_dir_all(claude.join("plugins")).unwrap();
    std::fs::write(claude.join(".credentials.json"), UPSTREAM_CREDS).unwrap();
    std::fs::write(claude.join("settings.json"), UPSTREAM_SETTINGS).unwrap();
    std::fs::write(
        claude.join("plugins").join("installed_plugins.json"),
        br#"{"version":2,"plugins":{"clauth@clauth":[]}}"#,
    )
    .unwrap();
    std::fs::write(home.join(".claude.json"), UPSTREAM_CLAUDE_JSON).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex").join("auth.json"), UPSTREAM_CODEX_AUTH).unwrap();
}

/// Every global file guest mode must leave alone: (path, bytes, is a symlink).
fn global_files(home: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, bool)> {
    [
        home.join(".claude").join(".credentials.json"),
        home.join(".claude").join("settings.json"),
        home.join(".claude")
            .join("plugins")
            .join("installed_plugins.json"),
        home.join(".claude.json"),
        home.join(".codex").join("auth.json"),
    ]
    .into_iter()
    .map(|p| {
        let bytes = std::fs::read(&p).ok();
        let link = p
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink());
        (p, bytes, link)
    })
    .collect()
}

fn oauth_profile(name: &str) -> Profile {
    let mut p = Profile::new(name.to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: format!("{name}-access"),
            refresh_token: Some(format!("{name}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    });
    p.env
        .insert("TOLLGATE_TEST_ENV".to_string(), name.to_string());
    crate::profile::save_profile(&p).expect("save profile");
    p
}

/// Two logged-in tollgate profiles, `one` recorded active.
fn two_profiles() -> AppConfig {
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: vec![oauth_profile("one"), oauth_profile("two")],
    };
    config.state.profiles = vec!["one".into(), "two".into()];
    config.state.active_profile = Some("one".into());
    crate::profile::save_app_state(&config.state).expect("save state");
    config
}

fn is_guest_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<GuestRefusal>().is_some()
}

fn no_refresh(
    _: &str,
    _: Option<&str>,
) -> std::result::Result<crate::oauth::TokenResponse, crate::oauth::RefreshError> {
    panic!("guest mode must refuse before the AUTH-1 gate spends a refresh")
}

#[test]
fn upstream_active_needs_the_upstream_dir_and_no_completed_import() {
    let home = HomeSandbox::new();
    assert!(!upstream_active(), "no ~/.clauth: not a guest");

    std::fs::create_dir_all(home.home().join(UPSTREAM_DATA_DIR_NAME)).unwrap();
    assert!(upstream_active(), "~/.clauth and no journal: guest");

    let data = home.home().join(DATA_DIR_NAME);
    std::fs::create_dir_all(&data).unwrap();
    let journal = data.join(IMPORT_JOURNAL_FILE);
    for (body, guest, why) in [
        (
            &b"{\"state\":\"pre\"}"[..],
            true,
            "a pre phase is no import",
        ),
        (
            b"{\"state\":\"rolled_back\"}",
            true,
            "a rollback is no import",
        ),
        (b"{\"state\":", true, "a torn journal is no import"),
        (b"[\"complete\"]", true, "not an object"),
        (
            b"{\"steps\":{\"state\":\"complete\"}}",
            true,
            "nested, not top-level",
        ),
        (
            b"{\"state\":\"complete\"}",
            false,
            "a completed import ends guest mode",
        ),
    ] {
        std::fs::write(&journal, body).unwrap();
        assert_eq!(upstream_active(), guest, "{why}");
    }

    // With the journal complete, upstream's dir no longer matters either way.
    std::fs::remove_dir_all(home.home().join(UPSTREAM_DATA_DIR_NAME)).unwrap();
    assert!(!upstream_active());
}

#[test]
fn guest_mode_refuses_every_global_mutation_and_leaves_upstreams_files_byte_identical() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let config = two_profiles();
    let before = global_files(home.home());
    assert!(upstream_active());

    // The global switch, every flavor, refuses with the guest sentence.
    let two = ProfileName::from("two");
    let (config, out) = through_handle(config, |h| crate::actions::switch_profile(h, &two));
    assert!(is_guest_refusal(&out.unwrap_err()));
    let (config, out) = through_handle(config, |h| crate::actions::switch_profile_discard(h, &two));
    assert!(is_guest_refusal(&out.unwrap_err()));
    let (config, out) = through_handle(config, |h| {
        crate::actions::switch_profile_reconciled(h, &two)
    });
    assert!(is_guest_refusal(&out.unwrap_err()));
    let err = crate::actions::switch_profile_cli(config.clone(), &two).unwrap_err();
    assert!(is_guest_refusal(&err));
    assert_eq!(err.to_string(), GUEST_REFUSAL);
    assert_eq!(crate::exit_code(Err(err)), 1, "a refusal exits non-zero");

    // The headless switch (MCP tool, daemon API) refuses as an authored sentence.
    let (config, out) = through_handle(config, |h| {
        crate::actions::switch_profile_noninteractive(h, &two, None, no_refresh)
    });
    match out {
        Err(crate::actions::SwitchError::Refused(sentence)) => {
            assert_eq!(sentence, GUEST_REFUSAL);
        }
        other => panic!("expected the guest refusal, got {other:?}"),
    }

    // Wrap-off refuses; the unattended auto-switch stays put silently.
    let (config, out) = through_handle(config, crate::actions::switch_off);
    assert!(is_guest_refusal(&out.unwrap_err()));
    let (mut config, out) =
        through_handle(config, |h| crate::fallback::auto_switch_if_needed(h, None));
    assert!(out.expect("auto-switch is a silent no-op").is_none());
    assert!(config.is_active(&ProfileName::from("one")), "nothing moved");

    // Capture refuses: the live login is upstream's refresh chain.
    let err = crate::actions::capture_current_login(&mut config, "captured").unwrap_err();
    assert!(is_guest_refusal(&err));
    assert!(config.find(&ProfileName::from("captured")).is_none());

    // The slot primitives refuse; the background legs skip silently.
    for out in [
        crate::claude::link_profile_credentials(&two),
        crate::claude::force_link_profile_credentials(&two),
        crate::claude::clear_claude_credentials(),
        crate::claude::adopt_first_login(&mut config, &ProfileName::from("one")),
    ] {
        assert!(is_guest_refusal(&out.unwrap_err()));
    }
    crate::claude::detach_credentials_link().expect("detach is a silent no-op");
    crate::claude::snapshot_active_credentials(&mut config).expect("snapshot is a no-op");
    crate::claude::force_snapshot_active_credentials(&mut config).expect("no-op");
    let stored = crate::profile::load_profile(&ProfileName::from("one")).unwrap();
    assert_eq!(
        stored
            .credentials
            .and_then(|c| c.claude_ai_oauth)
            .map(|o| o.access_token)
            .as_deref(),
        Some("one-access"),
        "upstream's live login never lands in a tollgate store"
    );

    // settings.json / ~/.claude.json writers skip.
    let one = config.find(&ProfileName::from("one")).unwrap().clone();
    crate::claude::apply_profile_to_claude_settings(&one, &["UPSTREAM".to_string()])
        .expect("settings write is a silent no-op");
    crate::claude_json::strip_home_oauth_account().expect("strip is a silent no-op");
    crate::settings_sync::sync_once().expect("sync");
    crate::claude_json::sync_once().expect("sync");

    // The plugin install and the `mcpServers` wiring are additive guest
    // writes now (`tests/inline/guest_write.rs` pins them). The heal legs
    // touch nothing of upstream's: the `self-heal` hook stands down outside a
    // tollgate session, the repoint re-points only tollgate's own rows, and
    // with nothing of tollgate's registered neither detached leg spawns (both
    // return before the fake-claude assertion).
    crate::plugin_host::self_heal().expect("self-heal is a no-op here");
    let repoint = crate::plugin_host::repoint_registry().unwrap();
    assert!(repoint.line.is_none() && !repoint.changed);
    crate::plugin_host::heal_detached();
    crate::plugin_host::preflight();

    // The codex capture adopts ~/.codex/auth.json, so it refuses too.
    let err =
        crate::actions::codex_login_capture_at("cx", "2026-09-29T00:00:00+00:00").unwrap_err();
    assert!(is_guest_refusal(&err));

    // Account edits on the recorded-active profile keep their global legs off.
    let mut config = config;
    crate::actions::edit_profile_env(
        &mut config,
        &ProfileName::from("one"),
        std::collections::BTreeMap::from([("NEW_KEY".to_string(), "v".to_string())]),
    )
    .expect("the store edit lands; settings.json is skipped");
    crate::actions::clear_profile_credentials(&mut config, &ProfileName::from("one"))
        .expect("logout clears the store and our marker only");
    assert!(config.state.active_profile.is_none());

    assert_eq!(
        global_files(home.home()),
        before,
        "guest mode left every upstream-owned global file byte-identical"
    );
}

#[test]
fn a_first_account_in_guest_mode_is_created_but_never_auto_activated() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let before = global_files(home.home());
    let mut config = AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    };
    let creds = ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "fresh-access".to_string(),
            refresh_token: Some("fresh-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    };
    crate::actions::create_profile_from_login(&mut config, "fresh".to_string(), None, creds, None)
        .expect("the profile is created");
    assert!(config.find(&ProfileName::from("fresh")).is_some());
    assert!(
        config.state.active_profile.is_none(),
        "the first account does not take the global slot in guest mode"
    );
    assert_eq!(global_files(home.home()), before);
}

#[test]
fn a_completed_import_journal_lifts_guest_mode_and_the_switch_links_again() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let config = two_profiles();
    // The live slot mirrors the recorded active account, as after an import.
    let slot = home.home().join(".claude").join(".credentials.json");
    std::fs::write(
        &slot,
        serde_json::to_vec(
            config
                .find(&ProfileName::from("one"))
                .unwrap()
                .credentials
                .as_ref()
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.home().join(DATA_DIR_NAME).join(IMPORT_JOURNAL_FILE),
        br#"{"state":"complete"}"#,
    )
    .unwrap();
    assert!(!upstream_active());

    let two = ProfileName::from("two");
    let (config, out) = through_handle(config, |h| crate::actions::switch_profile(h, &two));
    out.expect("the switch runs once the import completed");
    assert!(config.is_active(&two));
    assert_eq!(
        crate::claude::classify_credentials_link(&two).unwrap(),
        crate::claude::LinkState::LinkedTo,
        "the slot now resolves to 'two'"
    );
    let settings: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.home().join(".claude").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["env"]["TOLLGATE_TEST_ENV"], "two");
}

/// The jsonsync engine's guest rule (plan §4.0: "guest runtime copies are never
/// sync members of the operator base"): a reconcile that names an operator
/// file does nothing. The operator file is neither written nor read as a
/// winner, and the runtime copies do not propagate between each other either.
#[test]
fn guest_sync_keeps_runtime_copies_out_of_the_operator_member_set() {
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(UPSTREAM_DATA_DIR_NAME)).unwrap();
    let dir = home.home().join("sync");
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("base.json");
    let a = dir.join("a.json");
    let b = dir.join("b.json");
    let now = std::time::SystemTime::now();
    let at = |secs: u64| now - std::time::Duration::from_secs(secs);
    std::fs::write(&base, br#"{"theme":"base"}"#).unwrap();
    std::fs::write(&a, br#"{"theme":"a"}"#).unwrap();
    std::fs::write(&b, br#"{"theme":"b"}"#).unwrap();
    set_mtime(&base, at(300));
    set_mtime(&b, at(200));
    set_mtime(&a, at(100));
    let paths = [base.clone(), a.clone(), b.clone()];
    let shared = |_: crate::jsonsync::KeyPath<'_>| crate::jsonsync::KeyRule::Shared;
    let theme = |p: &Path| -> String {
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(p).unwrap()).unwrap()["theme"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // A runtime copy is newest: neither its sibling nor the operator file follows.
    crate::jsonsync::sync_paths(&paths, Some(&base), shared).unwrap();
    assert_eq!(std::fs::read(&base).unwrap(), br#"{"theme":"base"}"#);
    assert_eq!(theme(&a), "a");
    assert_eq!(theme(&b), "b");

    // The operator file is newest: no runtime copy takes it, and it stays unwritten.
    std::fs::write(&base, br#"{"theme":"edited"}"#).unwrap();
    set_mtime(&base, now + std::time::Duration::from_secs(60));
    crate::jsonsync::sync_paths(&paths, Some(&base), shared).unwrap();
    assert_eq!(std::fs::read(&base).unwrap(), br#"{"theme":"edited"}"#);
    assert_eq!(theme(&a), "a");
    assert_eq!(theme(&b), "b");

    // Guest mode lifted: the same members reconcile again.
    let journal = home.home().join(DATA_DIR_NAME).join(IMPORT_JOURNAL_FILE);
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, br#"{"state":"complete"}"#).unwrap();
    assert!(!upstream_active());
    crate::jsonsync::sync_paths(&paths, Some(&base), shared).unwrap();
    assert_eq!(theme(&a), "edited");
    assert_eq!(theme(&b), "edited");
}

#[test]
fn status_json_gains_guest_mode_additively() {
    let body = serde_json::json!({"active_profile": "one", "profiles": []});
    let out = crate::daemon::with_guest_mode(body.clone(), true);
    assert_eq!(out["guest_mode"], true);
    assert_eq!(
        out["active_profile"], "one",
        "every existing field survives"
    );
    assert_eq!(
        crate::daemon::with_guest_mode(body, false)["guest_mode"],
        false
    );
}

#[test]
fn native_monitors_and_secrets_leave_guest_operator_files_unchanged() {
    use crate::usage::monitor::{cli::preset, config, source};
    let sb = HomeSandbox::new();
    stage_upstream(sb.home());
    assert!(upstream_active());
    let grok_home = sb.home().join(".grok");
    std::fs::create_dir_all(&grok_home).unwrap();
    std::fs::write(grok_home.join("auth.json"), r#"{"https://auth.x.ai::test":{"key":"TOKEN-CANARY","expires_at":1,"refresh_token":"REFRESH-CANARY"}}"#).unwrap();
    std::fs::write(grok_home.join("auth.json.lock"), "LOCK-CANARY").unwrap();
    let before = global_files(sb.home());
    let grok_before = std::fs::read(grok_home.join("auth.json")).unwrap();
    for name in ["grok", "agy", "codex-native"] {
        let m = preset(name).unwrap();
        config::add(&m).unwrap();
        let target = source::resolve_target(&m, sb.home(), 1_900_000_000, &|_| None);
        let result = source::source_for(m.kind).fetch(&target, &source::FakeHttp::offline());
        assert!(result.is_err());
    }
    let dir = crate::profile::tollgate_dir().unwrap();
    crate::profile::atomic_write_600(&dir.join("secrets.env"), "LANE4_GUEST_KEY=TOKEN-CANARY\n")
        .unwrap();
    crate::secrets::dispatch(crate::secrets::SecretCommand::List { json: true }).unwrap();
    assert_eq!(
        crate::secrets::resolve("LANE4_GUEST_KEY").as_deref(),
        Some("TOKEN-CANARY")
    );
    crate::secrets::dispatch(crate::secrets::SecretCommand::Rm {
        name: "LANE4_GUEST_KEY".into(),
        yes: true,
    })
    .unwrap();
    assert_eq!(global_files(sb.home()), before);
    assert_eq!(
        std::fs::read(grok_home.join("auth.json")).unwrap(),
        grok_before
    );
    assert_eq!(
        std::fs::read(grok_home.join("auth.json.lock")).unwrap(),
        b"LOCK-CANARY"
    );
}
