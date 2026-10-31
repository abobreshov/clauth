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

/// The journal contract, every state (spec import-clauth.md §3.2): only
/// `complete` ends guest mode; `import_state` names each one for the status
/// surfaces and an interrupted one (`pre`, `in_progress`, `rolling_back`)
/// reads as such. A missing journal is `none`, a torn one `unreadable`.
#[test]
fn every_journal_state_reads_back_and_only_complete_ends_guest_mode() {
    use crate::identity::{ImportState, import_state};
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(UPSTREAM_DATA_DIR_NAME)).unwrap();
    let data = home.home().join(DATA_DIR_NAME);
    std::fs::create_dir_all(&data).unwrap();
    assert_eq!(import_state(), ImportState::None);
    let journal = data.join(IMPORT_JOURNAL_FILE);
    for (state, want, interrupted) in [
        ("pre", ImportState::Pre, true),
        ("in_progress", ImportState::InProgress, true),
        ("complete", ImportState::Complete, false),
        ("rolling_back", ImportState::RollingBack, true),
        ("rolled_back", ImportState::RolledBack, false),
        ("aborted", ImportState::Aborted, false),
    ] {
        std::fs::write(&journal, format!("{{\"state\":\"{state}\"}}")).unwrap();
        assert_eq!(import_state(), want, "{state}");
        assert_eq!(want.as_str(), state);
        assert_eq!(want.is_interrupted(), interrupted, "{state}");
        assert_eq!(upstream_active(), state != "complete", "{state}");
    }
    std::fs::write(&journal, "{\"state\":").unwrap();
    assert_eq!(import_state(), ImportState::Unreadable);
    std::fs::write(&journal, "{\"state\":\"sideways\"}").unwrap();
    assert_eq!(import_state(), ImportState::Unreadable);
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
    // The heal legs below read agentgear's marker tree and resolve `claude`
    // off the process env (`HOME`, `XDG_DATA_HOME`, `PATH`). Pin all of them to
    // the sandbox with no `claude` on PATH, so the pre-check reads this box,
    // not the operator's real install, and nothing can spawn.
    #[cfg(unix)]
    let _claude = crate::testutil::FakeClaude::new_without_claude(&home);
    #[cfg(not(unix))]
    crate::plugin_host::arm_heal_throttle_for_test();
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
        crate::actions::switch_profile_noninteractive(h, &two, None, no_refresh, None)
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
    let (mut config, out) = through_handle(config, |h| {
        crate::fallback::auto_switch_if_needed(
            h,
            None,
            &std::sync::Arc::new(crate::lockorder::RankedMutex::new(
                std::collections::HashMap::new(),
            )),
            &std::sync::Arc::new(crate::lockorder::RankedMutex::new(
                std::collections::HashMap::new(),
            )),
            &std::sync::Arc::new(crate::lockorder::RankedMutex::new(
                std::collections::HashMap::new(),
            )),
            &std::sync::Arc::new(crate::lockorder::RankedMutex::new(
                std::collections::HashMap::new(),
            )),
            &std::sync::Arc::new(crate::lockorder::RankedMutex::new(
                std::collections::HashMap::new(),
            )),
            &std::sync::Arc::new(crate::lockorder::RankedMutex::new(
                std::collections::HashMap::new(),
            )),
            &std::sync::Arc::new(crate::lockorder::RankedMutex::new(
                std::collections::HashMap::new(),
            )),
        )
    });
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
    // The strip is infallible now; in guest mode it returns before any read.
    crate::claude_json::strip_home_oauth_account();
    crate::settings_sync::sync_once().expect("sync");
    crate::claude_json::sync_once().expect("sync");

    // The plugin install and the `mcpServers` wiring are additive guest
    // writes now (`tests/inline/guest_write.rs` pins them). The heal legs
    // touch nothing of upstream's: the `self-heal` hook stands down outside a
    // tollgate session, the repoint re-points only tollgate's own rows, and
    // with nothing of tollgate's registered (and no `claude` to drive) the
    // pre-check heals nothing.
    crate::plugin_host::self_heal().expect("self-heal is a no-op here");
    let repoint = crate::plugin_host::repoint_registry().unwrap();
    assert!(repoint.line.is_none() && !repoint.changed);
    crate::plugin_host::heal_detached();
    crate::plugin_host::preflight();

    // The codex capture adopts ~/.codex/auth.json, so it refuses too.
    let err = crate::actions::codex_login_capture_at(
        "cx",
        "2026-09-29T00:00:00+00:00",
        crate::actions::CodexLandMode::CreateOrReauth,
    )
    .err()
    .expect("guest mode refuses the codex capture");
    assert!(is_guest_refusal(&err));

    // Account edits on the recorded-active profile keep their global legs off.
    let mut config = config;
    crate::actions::edit_profile_env(
        &mut config,
        &ProfileName::from("one"),
        std::collections::BTreeMap::from([("NEW_KEY".to_string(), "v".to_string())]),
    )
    .expect("the store edit lands; settings.json is skipped");
    // C-G81: `tollgate login`'s model write; C-G78: the presets rebind. Both
    // re-apply the active profile to `settings.json` only through the gated
    // `apply_profile_to_claude_settings`.
    crate::actions::set_profile_default_model(
        &mut config,
        &ProfileName::from("one"),
        &crate::testutil::profile_id("one"),
        "opus",
    )
    .expect("the model lands in the store; settings.json is skipped");
    let moved = crate::actions::apply_rebind_account(
        &mut config,
        &ProfileName::from("one"),
        &[crate::presets::RowMove {
            row: "sonnet",
            old: None,
            new: Some("rebound-sonnet".to_string()),
        }],
    )
    .expect("the rebind lands in the store; settings.json is skipped");
    assert_eq!(moved, 1);
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
    crate::actions::create_profile_from_login(
        &mut config,
        "fresh".to_string(),
        Default::default(),
        creds,
        None,
        None,
    )
    .expect("the profile is created");
    assert!(config.find(&ProfileName::from("fresh")).is_some());
    assert!(
        config.state.active_profile.is_none(),
        "the first account does not take the global slot in guest mode"
    );
    assert_eq!(global_files(home.home()), before);

    // C-G80: `capture_new_profile` lands through `capture_into_profile`,
    // whose no-active arm is gated the same way: still no active profile, so
    // outside guest mode this capture would take the global slot.
    let snapshot = crate::actions::CaptureSnapshot {
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "captured-access".to_string(),
                refresh_token: Some("captured-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
                ..OAuthToken::default_extra()
            }),
        }),
        base_url: None,
        api_key: None,
        anchor: crate::actions::AnchorAction::Unproven,
        plan: None,
    };
    crate::actions::capture_into_profile(
        &mut config,
        "captured".to_string(),
        Default::default(),
        snapshot,
    )
    .expect("the captured profile is created");
    assert!(config.find(&ProfileName::from("captured")).is_some());
    assert!(
        config.state.active_profile.is_none(),
        "a captured first account does not take the global slot in guest mode"
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
    crate::jsonsync::sync_paths(&paths, Some(&base), None, shared).unwrap();
    assert_eq!(std::fs::read(&base).unwrap(), br#"{"theme":"base"}"#);
    assert_eq!(theme(&a), "a");
    assert_eq!(theme(&b), "b");

    // The operator file is newest: no runtime copy takes it, and it stays unwritten.
    std::fs::write(&base, br#"{"theme":"edited"}"#).unwrap();
    set_mtime(&base, now + std::time::Duration::from_secs(60));
    crate::jsonsync::sync_paths(&paths, Some(&base), None, shared).unwrap();
    assert_eq!(std::fs::read(&base).unwrap(), br#"{"theme":"edited"}"#);
    assert_eq!(theme(&a), "a");
    assert_eq!(theme(&b), "b");

    // Guest mode lifted: the same members reconcile again.
    let journal = home.home().join(DATA_DIR_NAME).join(IMPORT_JOURNAL_FILE);
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, br#"{"state":"complete"}"#).unwrap();
    assert!(!upstream_active());
    crate::jsonsync::sync_paths(&paths, Some(&base), None, shared).unwrap();
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

/// Test 66 (import spec §2.3): the guest refusal ends by naming the command
/// that ends guest mode, now that the import exists.
#[test]
fn guest_refusal_names_the_import_command() {
    assert!(
        GUEST_REFUSAL.ends_with(&format!(
            ", or run '{} import clauth --dry-run' to import clauth.",
            crate::identity::NAME
        )),
        "{GUEST_REFUSAL}"
    );
    assert!(!GUEST_REFUSAL.contains("not yet available"));
}

/// Every file under the operator trees guest mode leaves alone: relative
/// path → bytes (or the link target for a symlink).
fn operator_trees(home: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, at: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(meta) = at.symlink_metadata() else {
            return;
        };
        let rel = at.strip_prefix(root).unwrap().to_path_buf();
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(at).unwrap();
            out.insert(rel, target.to_string_lossy().as_bytes().to_vec());
        } else if meta.is_dir() {
            out.insert(rel, b"<dir>".to_vec());
            for entry in std::fs::read_dir(at).unwrap().flatten() {
                walk(root, &entry.path(), out);
            }
        } else {
            out.insert(rel, std::fs::read(at).unwrap());
        }
    }
    let mut out = std::collections::BTreeMap::new();
    for top in [
        ".claude",
        ".claude.json",
        UPSTREAM_DATA_DIR_NAME,
        ".codex",
        ".hermes",
    ] {
        walk(home, &home.join(top), &mut out);
    }
    out
}

/// Hot-swap spec test 56. In guest mode a B session's start, its hot swap,
/// its helper and a relaunch claim write only under `~/.tollgate`: every
/// operator tree (`~/.claude`, `~/.claude.json`, `~/.clauth`, `~/.codex`,
/// `~/.hermes`) is byte-identical afterwards.
#[cfg(unix)]
#[test]
fn a_guest_b_swap_and_relaunch_leave_every_operator_tree_byte_identical() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    std::fs::create_dir_all(home.home().join(".hermes")).unwrap();
    std::fs::write(
        home.home().join(".hermes").join("config.yaml"),
        b"model: m\n",
    )
    .unwrap();
    assert!(upstream_active());

    const OR: &str = "https://openrouter.ai/api";
    let a = crate::testutil::api_key_profile("g-a", OR, "sk-g-a");
    crate::testutil::write_api_key_profile(&a);
    crate::testutil::write_api_key_profile(&crate::testutil::api_key_profile("g-b", OR, "sk-g-b"));
    let bin = home.home().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("claude"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::ffi::OsString::from(format!("{}:/usr/bin:/bin", bin.display()));
    let _path = crate::testutil::EnvPin::new(&home, &[("PATH", Some(path.as_os_str()))]);
    let _gate = crate::hot_swap::S1GateOverride::pass(&home, &["2.1.283"]);
    crate::hot_swap::set_cc_probe(Some(Box::new(|| Some("2.1.283 (Claude Code)".to_string()))));
    let cwd = home.home().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let before = operator_trees(home.home());

    let launch = crate::runtime::LaunchInfo {
        launcher: crate::live_sessions::Launcher::Own,
        hot_swap: crate::hot_swap::HotSwapPolicy::Allowed,
        spawn_cwd: Some(cwd.clone()),
        relaunch_capable: true,
        relaunched_from: None,
    };
    let rt = crate::runtime::ProfileRuntime::acquire_with(
        &a,
        crate::runtime::Isolation::Shared,
        &[],
        false,
        &launch,
    )
    .expect("a guest B start");
    crate::hot_swap::set_cc_probe(None);
    assert_eq!(*rt.executor(), crate::hot_swap::Executor::ApiKey);
    let sid = rt.session_id().to_string();

    // The hot swap: the session's own watchdog commits the request.
    let request =
        crate::sessions_cli::request_session_switch(&sid, "g-b", crate::sessions_cli::Surface::Cli)
            .expect("request");
    assert_eq!(
        request.outcome,
        crate::sessions_cli::RequestOutcome::Committed(1)
    );
    let mut key = Vec::new();
    crate::hot_swap::run_session_helper(&sid, &mut key).expect("the helper serves");
    assert_eq!(key, b"sk-g-b");

    // The MCP session form moves it again: allowed in guest mode, unlike the
    // global form, and it writes only the session's own row.
    crate::testutil::write_api_key_profile(&crate::testutil::api_key_profile("g-c", OR, "sk-g-c"));
    let payload = crate::mcp::session_switch_payload(&sid, "g-c");
    assert_eq!(payload["ok"], serde_json::json!(true), "{payload}");
    assert_eq!(payload["state"], serde_json::json!("swapping"), "{payload}");
    assert_eq!(payload["committed_member"], serde_json::json!("g-c"));

    // The relaunch, up to the claim: the conversation lives in the guest store.
    crate::testutil::transcript_fixture(
        &crate::relaunch::projects_store().unwrap(),
        &cwd,
        &["conv-guest"],
    );
    let prepared = crate::relaunch::prepare(&sid, "g-a", None).expect("prepared");
    std::fs::write(
        crate::live_sessions::relaunch_path(&sid, "").unwrap(),
        serde_json::to_vec(&prepared.request).unwrap(),
    )
    .unwrap();
    assert!(crate::relaunch::poll_claim(&sid).is_some());
    rt.keep_relaunch_taken();
    drop(rt);

    assert_eq!(
        operator_trees(home.home()),
        before,
        "a guest B session wrote into an operator tree"
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
// Slice-1 review: refusal must also apply on the active guest path.
#[cfg(unix)]
#[test]
fn guest_codex_native_refuses_upstream_profile_store_symlink() {
    use crate::usage::monitor::{
        codex_native::CodexNativeSource,
        config::{MonitorConfig, MonitorKind},
        source::{FakeHttp, UsageSource, resolve_target},
    };
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    assert!(upstream_active());
    let upstream = home.home().join(".clauth/profiles/operator/auth.json");
    std::fs::create_dir_all(upstream.parent().unwrap()).unwrap();
    std::fs::write(&upstream, UPSTREAM_CODEX_AUTH).unwrap();
    let global = home.home().join(".codex/auth.json");
    std::fs::remove_file(&global).unwrap();
    std::os::unix::fs::symlink(&upstream, &global).unwrap();
    let cfg = MonitorConfig::new("native", MonitorKind::CodexNative);
    let target = resolve_target(&cfg, home.home(), 1900000000, &|_| None);
    let http = FakeHttp::offline();
    let failure = CodexNativeSource.fetch(&target, &http).unwrap_err();
    assert_eq!(
        failure.kind,
        crate::usage::observation::FailureKind::Unavailable
    );
    assert!(failure.message.contains("upstream:operator"));
    assert!(http.calls().is_empty());
    assert_eq!(std::fs::read(&upstream).unwrap(), UPSTREAM_CODEX_AUTH);
    assert_eq!(std::fs::read_link(&global).unwrap(), upstream);
    assert!(upstream_active());
}

/// Upstream's live login, as a login a bare-item save would take.
fn upstream_login() -> ClaudeCredentials {
    ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "upstream-access".to_string(),
            refresh_token: Some("upstream-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..OAuthToken::default_extra()
        }),
    }
}

/// C-G56, C-G57, C-G58 (ledger): the bare-item saves and the live-file capture
/// refuse in guest mode before they read or write anything, since the item's
/// login and the live file's are upstream clauth's refresh chain. No profile
/// is created and every global file stays byte-identical.
#[test]
fn guest_mode_refuses_the_bare_item_saves_and_the_live_file_capture() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let mut config = two_profiles();
    let before = global_files(home.home());
    assert!(upstream_active());

    let err =
        crate::actions::save_bare_item_as_profile(&mut config, "saved", upstream_login(), None)
            .unwrap_err();
    assert!(is_guest_refusal(&err), "{err:#}");
    let err = crate::actions::save_bare_item_as_profile_fresh(
        &mut config,
        "saved",
        upstream_login(),
        None,
    )
    .unwrap_err();
    assert!(is_guest_refusal(&err), "{err:#}");
    assert!(config.find(&ProfileName::from("saved")).is_none());
    assert!(
        !crate::profile::profile_dir(&ProfileName::from("saved"))
            .unwrap()
            .exists(),
        "no profile dir was created"
    );

    let err = crate::actions::capture_live_file_snapshot().unwrap_err();
    assert!(is_guest_refusal(&err), "{err:#}");
    // C-G58: the capture of the login in use refuses too, and on macOS its
    // refusal sits above the bare-item branch's `security` read.
    let err = crate::actions::capture_snapshot().unwrap_err();
    assert!(is_guest_refusal(&err), "{err:#}");
    let body = include_str!("../../src/actions.rs")
        .split_once("pub(crate) fn capture_snapshot(")
        .expect("capture_snapshot defined")
        .1
        .split_once("\n}\n")
        .expect("capture_snapshot closes")
        .0;
    let refuse = body
        .find("refuse_foreign_live_login()?")
        .expect("capture_snapshot refuses first");
    let read = body
        .find("read_bare_keychain_item()")
        .expect("capture_snapshot reads the bare item on macOS");
    assert!(refuse < read, "the refusal runs before the bare-item read");

    assert_eq!(global_files(home.home()), before);
}

/// C-G68 (ledger): deleting the recorded-active profile in guest mode
/// removes tollgate's own record and dir, and leaves upstream's live
/// credentials and `settings.json` byte-identical: guest mode wired nothing
/// into either, so there is nothing to unwire.
#[test]
fn guest_mode_deletes_the_active_profile_without_unwiring_upstreams_slot() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let mut config = two_profiles();
    let one = ProfileName::from("one");
    assert!(config.is_active(&one));
    // C-G10: a dead tree's rescue tombstone the delete's rescue leg lifts, and
    // the operator's transcript store it must leave alone.
    let projects = home.home().join(".claude").join("projects").join("-ws");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(projects.join("operator.jsonl"), b"{\"op\":1}\n").unwrap();
    let tomb = crate::profile::profile_dir(&one)
        .unwrap()
        .join("runtime-isolated-4260-0.rescuing");
    std::fs::create_dir_all(tomb.join("projects").join("-ws")).unwrap();
    std::fs::write(tomb.join("projects/-ws/tomb.jsonl"), b"{\"t\":1}\n").unwrap();
    let before = global_files(home.home());

    let rotation = crate::actions::rotation_guard_for_mutation(&one).unwrap();
    crate::actions::delete_profile(&mut config, &one, false, &rotation)
        .expect("guest mode deletes a tollgate profile");
    drop(rotation);
    assert!(config.find(&one).is_none());
    assert!(!crate::profile::profile_dir(&one).unwrap().exists());

    assert_eq!(
        global_files(home.home()),
        before,
        "the delete left upstream's live slot and settings.json alone"
    );
    let operator_store: Vec<_> = std::fs::read_dir(&projects)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        operator_store,
        vec![std::ffi::OsString::from("operator.jsonl")],
        "the delete's rescue added nothing to ~/.claude/projects"
    );
    assert_eq!(
        std::fs::read(projects.join("operator.jsonl")).unwrap(),
        b"{\"op\":1}\n",
        "the operator's transcript is untouched"
    );
    assert_eq!(
        std::fs::read(
            crate::runtime::guest_projects_store()
                .unwrap()
                .join("-ws/tomb.jsonl")
        )
        .unwrap(),
        b"{\"t\":1}\n",
        "the tombstone's transcript landed in the guest store"
    );
}

/// C-G67, C-G79 (ledger): a reauth of the recorded-active profile in guest
/// mode (`tollgate login <active>` lands through `overwrite_captured_profile`)
/// rewrites only tollgate's store. The guest arm in
/// `overwrite_captured_profile_with` skips both the relink and the
/// `settings.json` write, so upstream's live slot and settings stay
/// byte-identical; without it the `was_active` arm's force-link refuses.
#[test]
fn guest_mode_reauth_of_the_active_profile_rewrites_only_the_store() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let mut config = two_profiles();
    let one = ProfileName::from("one");
    let account = crate::testutil::profile_id("one");
    let before = global_files(home.home());
    let snapshot = crate::actions::CaptureSnapshot {
        credentials: Some(ClaudeCredentials {
            claude_ai_oauth: Some(OAuthToken {
                access_token: "one-reauthed".to_string(),
                refresh_token: Some("one-reauthed-refresh".to_string()),
                expires_at: None,
                scopes: None,
                subscription_type: None,
                ..OAuthToken::default_extra()
            }),
        }),
        base_url: None,
        api_key: None,
        anchor: crate::actions::AnchorAction::Unproven,
        plan: None,
    };

    crate::actions::overwrite_captured_profile(&mut config, &one, &account, snapshot)
        .expect("guest mode reauths tollgate's own store");

    assert!(config.is_active(&one), "the reauth keeps the record");
    let stored = crate::profile::load_profile(&one).unwrap();
    assert_eq!(
        stored
            .credentials
            .and_then(|c| c.claude_ai_oauth)
            .map(|o| o.access_token)
            .as_deref(),
        Some("one-reauthed"),
        "the store took the new login"
    );
    assert_eq!(
        global_files(home.home()),
        before,
        "the reauth left upstream's live slot and settings.json alone"
    );
}
