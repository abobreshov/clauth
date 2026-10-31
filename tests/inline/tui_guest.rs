//! Guest-mode TUI entry points refuse before opening browsers, spending a
//! refresh token, staging upstream's login, or consulting its bare Keychain
//! item. Every fixture enables the real identity gate through its HOME.

use super::*;
use crate::testutil::HomeSandbox;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static SOURCE_CALLS: AtomicUsize = AtomicUsize::new(0);
static CODEX_WORKER_CALLS: AtomicUsize = AtomicUsize::new(0);

fn forbidden_settle(_: crate::claude::BareItemWriter) -> Result<crate::claude::BareItemSettle> {
    SOURCE_CALLS.fetch_add(1, Ordering::SeqCst);
    panic!("guest TUI consulted the upstream bare Keychain item");
}

fn forbidden_read() -> Option<serde_json::Value> {
    SOURCE_CALLS.fetch_add(1, Ordering::SeqCst);
    panic!("guest TUI read the upstream bare Keychain item");
}

fn forbidden_save(_: &serde_json::Value) -> crate::claude::BareItemSave {
    SOURCE_CALLS.fetch_add(1, Ordering::SeqCst);
    panic!("guest TUI saved the upstream bare Keychain login");
}

fn forbidden_source() -> BareItemSource {
    BareItemSource {
        settle: forbidden_settle,
        read: forbidden_read,
        save: forbidden_save,
    }
}

fn forbidden_codex_worker(
    _: &dyn Fn(&str),
    _: &crate::codex_login::LoginCancel,
) -> std::result::Result<crate::codex_login::CodexLoginOutcome, String> {
    CODEX_WORKER_CALLS.fetch_add(1, Ordering::SeqCst);
    panic!("guest TUI started a Codex browser login");
}

fn forbidden_refresh(
    _: &str,
    _: Option<&str>,
) -> std::result::Result<oauth::TokenResponse, oauth::RefreshError> {
    panic!("guest TUI spent a refresh before refusing the switch");
}

const LIVE_CLAUDE: &[u8] = br#"{"claudeAiOauth":{"accessToken":"fixture-live","refreshToken":"fixture-refresh","expiresAt":4102444800000}}"#;
const LIVE_CODEX: &[u8] =
    br#"{"tokens":{"access_token":"fixture-codex","refresh_token":"fixture-codex-refresh"}}"#;

fn write_fixture(home: &Path, relative: &str, bytes: &[u8]) {
    let path = home.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture dir");
    std::fs::write(path, bytes).expect("fixture file");
}

fn guest_app(home: &HomeSandbox) -> App {
    SOURCE_CALLS.store(0, Ordering::SeqCst);
    CODEX_WORKER_CALLS.store(0, Ordering::SeqCst);
    let work = crate::testutil::blank_profile(&ProfileName::from("work"));
    let mut app = App::new(AppConfig {
        state: crate::profile::AppState {
            profiles: vec![work.name.clone()],
            active_profile: Some(work.name.clone()),
            preset_binds_offered: Some(crate::presets::builtin_binds_digest()),
            ..crate::profile::AppState::default()
        },
        profiles: vec![work],
    });
    join_test_workers();
    app.bare_item_source = Some(forbidden_source());
    app.codex_login_worker = forbidden_codex_worker;
    // Enable guest mode after construction: the writer must consult the real
    // identity gate rather than the header's startup-only display flag.
    std::fs::create_dir_all(home.home().join(crate::identity::UPSTREAM_DATA_DIR_NAME))
        .expect("upstream fixture");
    write_fixture(home.home(), ".claude/.credentials.json", LIVE_CLAUDE);
    write_fixture(
        home.home(),
        ".claude/settings.json",
        br#"{"theme":"upstream"}"#,
    );
    write_fixture(home.home(), ".claude.json", br#"{"numStartups":7}"#);
    write_fixture(
        home.home(),
        ".claude/plugins/installed_plugins.json",
        br#"{"version":2,"plugins":{"foreign@foreign":[{"scope":"user","version":"1"}]}}"#,
    );
    write_fixture(home.home(), ".codex/auth.json", LIVE_CODEX);
    assert!(
        home.home()
            .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
            .is_dir(),
        "the upstream fixture exists; behavior below must enforce its ownership"
    );
    assert!(
        !app.guest_mode,
        "this is the header flag, not the write gate"
    );
    app
}

fn assert_refused(app: &App) {
    let expected = prose::mark_commands(crate::identity::GUEST_REFUSAL);
    assert!(
        app.toasts.iter().any(|t| t.body == expected),
        "the user receives the guest refusal: {:?}",
        app.toasts.iter().map(|t| &t.body).collect::<Vec<_>>(),
    );
}

fn assert_no_source_calls() {
    join_test_workers();
    assert_eq!(SOURCE_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(CODEX_WORKER_CALLS.load(Ordering::SeqCst), 0);
}

fn codex_draft(app: &mut App) {
    let mut draft = build_draft_new();
    draft.account_type = NewAccountType::Codex;
    draft.name = InputState::new("new-codex");
    app.config_draft = Some(draft);
}

fn snapshot() -> Box<CaptureSnapshot> {
    Box::new(CaptureSnapshot {
        credentials: None,
        base_url: Some("https://fixture.invalid".to_string()),
        api_key: None,
        anchor: crate::actions::AnchorAction::Unproven,
        plan: None,
    })
}

fn globals(home: &HomeSandbox) -> Vec<(PathBuf, Vec<u8>)> {
    [
        ".claude/.credentials.json",
        ".claude/settings.json",
        ".claude/plugins/installed_plugins.json",
        ".claude.json",
        ".codex/auth.json",
    ]
    .into_iter()
    .map(|relative| {
        let path = home.home().join(relative);
        let bytes = std::fs::read(&path).expect("fixture bytes");
        (path, bytes)
    })
    .collect()
}

fn assert_globals_unchanged(before: &[(PathBuf, Vec<u8>)]) {
    for (path, bytes) in before {
        assert_eq!(
            std::fs::read(path).expect("live slot remains"),
            *bytes,
            "{}",
            path.display()
        );
        assert!(
            !path
                .symlink_metadata()
                .expect("live slot metadata")
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn guest_claude_browser_login_refuses_before_allocating_a_session() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    let generation = app.login_generation;
    start_login(&mut app, "fresh".to_string(), true);
    assert!(app.login.is_none());
    assert_eq!(app.login_generation, generation);
    assert_refused(&app);
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_codex_relogin_refuses_before_preflight_or_a_browser_worker() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    let generation = app.codex_login_generation;
    start_codex_login(&mut app, "not-in-roster");
    assert!(app.codex_login.is_none());
    assert_eq!(app.codex_login_generation, generation);
    assert_refused(&app);
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_codex_new_login_refuses_before_a_browser_worker() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    codex_draft(&mut app);
    start_codex_new_login(&mut app);
    assert!(app.codex_login.is_none());
    assert!(
        app.config_draft
            .as_ref()
            .expect("draft")
            .captured_login
            .is_none()
    );
    assert_refused(&app);
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_codex_capture_stages_nothing() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    codex_draft(&mut app);
    stage_codex_capture(&mut app);
    assert!(
        app.config_draft
            .as_ref()
            .expect("draft")
            .captured_login
            .is_none()
    );
    assert_refused(&app);
    assert_eq!(
        std::fs::read(home.home().join(".codex/auth.json")).expect("slot"),
        LIVE_CODEX
    );
}

#[test]
fn guest_codex_create_rechecks_a_stale_capture_stash() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    codex_draft(&mut app);
    app.config_draft.as_mut().expect("draft").captured_login = Some(DraftLogin::CodexCapture);
    let before = globals(&home);
    commit_codex_account(&mut app);
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert!(
        !home
            .home()
            .join(crate::identity::DATA_DIR_NAME)
            .join("profiles/new-codex/auth.json")
            .exists()
    );
}

#[test]
fn guest_claude_capture_starts_no_worker_and_stages_nothing() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    app.config_draft = Some(build_draft_new());
    spawn_capture_login(&mut app);
    assert!(
        app.config_draft
            .as_ref()
            .expect("draft")
            .captured_login
            .is_none()
    );
    assert_refused(&app);
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_capture_name_refuses_before_deactivating_the_current_account() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let before = globals(&home);
    commit_capture_name(&mut app, "fresh".to_string(), snapshot(), true);
    assert_eq!(
        app.config().state.active_profile,
        Some(ProfileName::from("work"))
    );
    assert!(app.config().find(&ProfileName::from("fresh")).is_none());
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_switch_gate_never_settles_or_spends_a_refresh() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    spawn_switch_gate(&mut app, ProfileName::from("fresh"), forbidden_refresh);
    assert_refused(&app);
    assert!(!any_busy(&app.activity));
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_switch_to_the_already_active_account_refuses_before_relinking() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let before = globals(&home);
    perform_switch(&mut app, &ProfileName::from("work"));
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_switch_off_refuses_before_divergence_or_bare_item_prompts() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let before = globals(&home);
    perform_switch_off(&mut app);
    assert_refused(&app);
    assert!(app.modals.is_empty());
    assert_eq!(
        app.config().state.active_profile,
        Some(ProfileName::from("work"))
    );
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_bare_item_settlement_never_calls_the_upstream_source() {
    let home = HomeSandbox::new();
    let _app = guest_app(&home);
    let global_before = globals(&home);
    let outcome = settle_bare_keychain_item(
        Some(forbidden_source()),
        crate::claude::BareItemWriter::Switch,
    )
    .expect("guest settlement is a no-op");
    assert!(matches!(
        outcome,
        crate::claude::BareItemSettle::Settled(crate::claude::BareItemSettled { placed: None, .. })
    ));
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_bare_item_gate_never_starts_settlement() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    let placement = bare_item_gate(&mut app, BareItemWrite::SwitchOff);
    assert!(
        matches!(placement, Some(None)),
        "local operations continue without a placement"
    );
    assert_no_source_calls();
    assert_eq!(app.bare_item_checks.load(Ordering::SeqCst), 0);
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_stale_bare_item_save_and_replace_answers_never_read_the_source() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    let before = local_store_bytes(&home);
    answer_bare_item(&mut app, BareItemWrite::SwitchOff, BareItemChoice::Save);
    answer_bare_item(&mut app, BareItemWrite::SwitchOff, BareItemChoice::Replace);
    assert_eq!(local_store_bytes(&home), before);
    assert_refused(&app);
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_stale_bare_item_owner_choice_never_reads_or_saves_the_source() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    let before = local_store_bytes(&home);
    save_bare_item_into_owner(
        &mut app,
        BareItemWrite::SwitchOff,
        ProfileName::from("work"),
        ProfileId::mint().expect("fixture account id"),
        Some([0; 32]),
    );
    assert_eq!(local_store_bytes(&home), before);
    assert_refused(&app);
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

#[test]
fn guest_startup_reconciliation_leaves_upstreams_login_unclaimed() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    {
        let cfg = app.config();
        crate::profile::save_profile(&cfg.profiles[0]).expect("persist empty local account");
        crate::profile::save_app_state(&cfg.state).expect("persist local account roster");
    }
    let before = globals(&home);
    reconcile_startup(&mut app);
    assert!(app.startup_seeded);
    assert!(app.divergence_pending.is_none());
    assert!(app.modals.is_empty());
    assert!(app.config().profiles[0].credentials.is_none());
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_shutdown_preserves_live_slots_and_saves_tollgates_own_state() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    {
        let cfg = app.config();
        crate::profile::save_profile(&cfg.profiles[0]).expect("persist empty local account");
        crate::profile::save_app_state(&cfg.state).expect("persist local account roster");
    }
    let before = globals(&home);
    shutdown(&mut app).expect("guest shutdown");
    assert!(
        app.config().profiles[0].credentials.is_none(),
        "shutdown skips adopting upstream's login into the local account"
    );
    assert!(
        crate::profile::load_profile(&ProfileName::from("work"))
            .expect("local account remains")
            .credentials
            .is_none(),
        "upstream's refresh chain is absent from the local store"
    );
    assert_globals_unchanged(&before);
    assert!(
        home.home()
            .join(crate::identity::DATA_DIR_NAME)
            .join("profiles.toml")
            .exists()
    );
    assert_no_source_calls();
}

#[test]
fn guest_codex_create_rechecks_a_stale_minted_login_stash() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    codex_draft(&mut app);
    app.config_draft.as_mut().expect("draft").captured_login = Some(DraftLogin::CodexMint(
        Box::new(crate::codex_login::CodexLoginOutcome {
            auth_json: LIVE_CODEX.to_vec(),
            account_id: None,
        }),
    ));
    let before = globals(&home);
    commit_codex_account(&mut app);
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert!(
        !home
            .home()
            .join(crate::identity::DATA_DIR_NAME)
            .join("profiles/new-codex/auth.json")
            .exists()
    );
}

#[test]
fn guest_stale_bare_item_name_prompt_creates_no_profile() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    app.bare_item_naming = Some(BareItemNaming {
        login: serde_json::from_slice(LIVE_CLAUDE).expect("synthetic login"),
        account: None,
        write: BareItemWrite::SwitchOff,
    });
    app.open_modal(Modal::NamePrompt(NamePromptForm {
        input: InputState::new("captured-upstream"),
        action: NamePromptAction::SaveBareItem,
    }));
    let before = globals(&home);
    let stores = local_store_bytes(&home);
    handle_name_prompt_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_eq!(local_store_bytes(&home), stores);
    assert!(
        app.config()
            .find(&ProfileName::from("captured-upstream"))
            .is_none()
    );
    assert_no_source_calls();
}

#[test]
fn guest_bootstrap_never_settles_or_relinks_the_upstream_slot() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let before = globals(&home);
    let probe = PlacementProbe::new();
    app.spawn_bootstrap();
    assert_no_source_calls();
    assert_globals_unchanged(&before);
    assert!(
        app.boot_bare_item.lock().expect("boot placement").is_none(),
        "guest bootstrap never places the item"
    );
    probe.assert_empty();
}

#[test]
fn guest_late_switch_answer_does_not_relink_upstreams_slot() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let before = globals(&home);
    finalize_switch(&mut app, &ProfileName::from("work"), None);
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_stale_divergence_actions_preserve_upstreams_slot() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let before = globals(&home);
    run_discard_divergence(&mut app, "work");
    run_divergence_choice(&mut app, "work", DivergenceChoice::Overwrite);
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

fn local_store_bytes(home: &HomeSandbox) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(path: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(meta) = path.symlink_metadata() else {
            return;
        };
        if meta.is_dir() {
            for entry in std::fs::read_dir(path).expect("fixture store directory") {
                walk(&entry.expect("fixture store entry").path(), out);
            }
        } else if meta.is_file() {
            out.push((
                path.to_path_buf(),
                std::fs::read(path).expect("fixture store bytes"),
            ));
        }
    }
    let mut out = Vec::new();
    walk(&home.home().join(crate::identity::DATA_DIR_NAME), &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn saved_guest_app(home: &HomeSandbox) -> App {
    let app = guest_app(home);
    {
        let mut cfg = app.config();
        let profile = cfg
            .find_mut(&ProfileName::from("work"))
            .expect("fixture profile");
        profile.credentials =
            Some(serde_json::from_slice(LIVE_CLAUDE).expect("fixture credentials"));
        profile
            .env
            .insert("TOLLGATE_GUEST_EXISTING".to_string(), "before".to_string());
        crate::profile::save_profile(profile).expect("persist fixture profile");
        crate::profile::save_app_state(&cfg.state).expect("persist fixture state");
    }
    claim_profile_id(&ProfileName::from("work")).expect("fixture account id");
    app
}

fn fixture_account() -> ProfileId {
    ProfileId::mint().expect("fixture account id")
}

struct PlacementProbe;
impl PlacementProbe {
    fn new() -> Self {
        *BARE_ITEM_PLACEMENTS.lock().expect("placement probe") = Some((
            serde_json::from_slice(LIVE_CLAUDE).expect("fixture item"),
            Vec::new(),
        ));
        Self
    }
    fn assert_empty(&self) {
        join_test_workers();
        let held = BARE_ITEM_PLACEMENTS.lock().expect("placement probe");
        let (_, seen) = held.as_ref().expect("armed probe");
        assert!(seen.is_empty(), "guest code entered placements: {seen:?}");
    }
}
impl Drop for PlacementProbe {
    fn drop(&mut self) {
        *BARE_ITEM_PLACEMENTS.lock().expect("placement probe") = None;
    }
}

/// C-G50: inject the nonempty production source seam even on Linux, so
/// removing the constructor's gate fails independently of the host OS.
#[test]
fn guest_app_construction_discards_the_default_bare_item_source() {
    SOURCE_CALLS.store(0, Ordering::SeqCst);
    CODEX_WORKER_CALLS.store(0, Ordering::SeqCst);
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(crate::identity::UPSTREAM_DATA_DIR_NAME))
        .expect("upstream fixture");
    write_fixture(home.home(), ".claude/.credentials.json", LIVE_CLAUDE);
    struct SourcePin(Option<BareItemSource>);
    impl Drop for SourcePin {
        fn drop(&mut self) {
            TEST_BARE_ITEM_SOURCE.with(|source| source.set(self.0));
        }
    }
    let _source =
        SourcePin(TEST_BARE_ITEM_SOURCE.with(|source| source.replace(Some(forbidden_source()))));
    assert!(
        home.home()
            .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
            .is_dir(),
        "the upstream fixture exists; construction must enforce its ownership"
    );
    let app = App::new(AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    });
    assert!(
        app.bare_item_source.is_none(),
        "a guest owns no handle to the bare item"
    );
    assert_eq!(
        std::fs::read(home.home().join(".claude/.credentials.json"))
            .expect("upstream slot remains"),
        LIVE_CLAUDE
    );
    assert_no_source_calls();
}

/// C-G52: the post-save path never settles or carries a placement, including
/// an answer saved before ownership changed.
#[test]
fn guest_settle_after_save_skips_the_source_and_old_placement() {
    let home = HomeSandbox::new();
    let _app = guest_app(&home);
    let global_before = globals(&home);
    for saved in [None, Some([1; 32])] {
        let outcome = settle_after_save(
            Some(forbidden_source()),
            crate::claude::BareItemWriter::Switch,
            Ok(saved),
        );
        assert!(
            matches!(
                outcome,
                BareItemOutcome::Settled(Ok(crate::claude::BareItemSettle::Settled(
                    crate::claude::BareItemSettled { placed: None, .. }
                )))
            ),
            "guest save settlement must carry no permission to replace the item"
        );
    }
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

/// C-G52 and C-G54: an active local-profile deletion is allowed, but its
/// sign-out does not settle or enter a bare-item placement.
#[test]
fn guest_active_profile_delete_skips_bare_settlement_and_placement() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let before = globals(&home);
    let probe = PlacementProbe::new();
    finish_delete(&mut app, &ProfileName::from("work"), false);
    join_test_workers();
    drain_profile_delete(&mut app);
    assert!(
        app.config().find(&ProfileName::from("work")).is_none(),
        "the local delete still lands"
    );
    assert_globals_unchanged(&before);
    probe.assert_empty();
    assert_no_source_calls();
}

/// C-G54: even a directly resumed old placement is not entered in guest mode.
#[test]
fn guest_placement_entry_and_gate_never_enter_a_bare_item_placement() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let global_before = globals(&home);
    let probe = PlacementProbe::new();
    assert!(enter_bare_item_placement(Some([2; 32])).is_none());
    app.bare_item_ready = Some(Some([3; 32]));
    assert!(matches!(
        bare_item_gate(&mut app, BareItemWrite::SwitchOff),
        Some(None)
    ));
    probe.assert_empty();
    assert_no_source_calls();
    assert_globals_unchanged(&global_before);
}

/// C-G69 covers every resume arm, including those that normally act on local
/// stores. A stale bare-item answer must not authorize any of them in guest mode.
#[test]
fn guest_every_resumed_bare_item_write_preserves_globals_and_stores() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let account = fixture_account();
    let work = ProfileName::from("work");
    let writes = vec![
        BareItemWrite::BootRelink(work.clone()),
        BareItemWrite::Switch(work.clone()),
        BareItemWrite::SwitchOff,
        BareItemWrite::ClearSessionToken(work.clone()),
        BareItemWrite::Rename,
        BareItemWrite::NewAccount,
        BareItemWrite::Delete {
            name: work.clone(),
            force: false,
        },
        BareItemWrite::CaptureOverwrite {
            snapshot: snapshot(),
            name: "work".to_string(),
            account: account.clone(),
            from_divergence: true,
        },
        BareItemWrite::AdoptDivergence {
            snapshot: snapshot(),
            name: "work".to_string(),
            account: account.clone(),
        },
        BareItemWrite::BlankCredentials("work".to_string()),
        BareItemWrite::DiscardDivergence("work".to_string()),
        BareItemWrite::DivergenceOverwrite("work".to_string()),
        BareItemWrite::CaptureName {
            name: "fresh".to_string(),
            snapshot: snapshot(),
            from_divergence: true,
        },
        BareItemWrite::Login {
            name: "work".to_string(),
            account,
            snapshot: snapshot(),
        },
    ];
    let before = globals(&home);
    let stores = local_store_bytes(&home);
    let probe = PlacementProbe::new();
    for write in writes {
        app.toasts.clear();
        resume_bare_item_write(&mut app, write, Some([4; 32]));
        assert_refused(&app);
        assert_globals_unchanged(&before);
        assert_eq!(
            local_store_bytes(&home),
            stores,
            "no resumed write adopts into a local store"
        );
        assert!(app.bare_item_ready.is_none());
    }
    probe.assert_empty();
    assert_no_source_calls();
}

/// C-G70 exercises the actual result channel, after a worker answered while
/// the application became a guest.
#[test]
fn guest_drained_switch_gate_preserves_global_files() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let before = globals(&home);
    app.switch_gate_tx
        .send(SwitchGateResult::gated(
            ProfileName::from("work"),
            oauth::AuthGate::Ready,
        ))
        .expect("gate answer");
    drain_switch_gates(&mut app);
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_capture_overwrite_confirm_preserves_global_files() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let account = claim_profile_id(&ProfileName::from("work")).expect("account");
    let before = globals(&home);
    let stores = local_store_bytes(&home);
    run_confirm_action(
        &mut app,
        ConfirmAction::CaptureOverwrite(snapshot(), "work".to_string(), account, true),
    );
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_eq!(local_store_bytes(&home), stores);
    assert_no_source_calls();
}

/// U5 / C-G72 pins the active marker on disk as well as the live slots.
#[test]
fn guest_adopt_divergence_refuses_before_saving_the_active_marker() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let spare = crate::testutil::blank_profile(&ProfileName::from("spare"));
    crate::profile::save_profile(&spare).expect("save inactive target");
    {
        let mut cfg = app.config();
        cfg.state.profiles.push(spare.name.clone());
        cfg.profiles.push(spare);
        crate::profile::save_app_state(&cfg.state).expect("save both profiles");
    }
    let account = claim_profile_id(&ProfileName::from("spare")).expect("target account");
    let before = globals(&home);
    let marker = home
        .home()
        .join(crate::identity::DATA_DIR_NAME)
        .join("profiles.toml");
    let state = std::fs::read(&marker).expect("initial marker");
    let stores = local_store_bytes(&home);
    run_confirm_action(
        &mut app,
        ConfirmAction::AdoptDivergence(snapshot(), "spare".to_string(), account),
    );
    assert_refused(&app);
    assert_eq!(
        app.config().state.active_profile,
        Some(ProfileName::from("work"))
    );
    assert_eq!(std::fs::read(marker).expect("marker remains"), state);
    assert_eq!(local_store_bytes(&home), stores);
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_finished_login_snapshot_preserves_global_files() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let account = claim_profile_id(&ProfileName::from("work")).expect("account");
    let before = globals(&home);
    let stores = local_store_bytes(&home);
    apply_login_snapshot(&mut app, "work".to_string(), account, *snapshot());
    assert_refused(&app);
    assert_globals_unchanged(&before);
    assert_eq!(local_store_bytes(&home), stores);
    assert_no_source_calls();
}

/// C-G75's permitted half, a control rather than a gate test: an endpoint
/// account created through the real create row saves locally and writes no
/// global file. This path has no activation leg in any mode. The gated leg (a
/// staged login on a fresh install) is
/// `guest_claude_create_row_refuses_staged_login_before_activation`.
#[test]
fn guest_claude_create_row_saves_a_local_profile_without_global_activation() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    {
        let mut cfg = app.config();
        cfg.state.active_profile = None;
    }
    app.tab = Tab::Setup;
    let mut draft = build_draft_new();
    draft.name = InputState::new("new-local");
    draft.base_url = InputState::new("https://fixture.invalid");
    draft.api_key = InputState::new("fixture-local-key");
    app.config_draft = Some(draft);
    let before = globals(&home);
    run_config_row(&mut app, ConfigRow::Create);
    assert!(app.config().find(&ProfileName::from("new-local")).is_some());
    assert!(
        app.config().state.active_profile.is_none(),
        "an endpoint create activates nothing"
    );
    assert!(
        app.toasts
            .iter()
            .any(|toast| toast.kind == ToastKind::Success
                && toast.body.contains("created 'new-local'")),
        "the permitted local create succeeds"
    );
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

/// C-G76 commits every typed setup field through the production edit funnel.
#[test]
fn guest_setup_rename_endpoint_and_model_commits_preserve_globals() {
    for (field, value) in [
        (ConfigRow::Name, "renamed"),
        (ConfigRow::BaseUrl, "https://fixture.invalid"),
        (ConfigRow::Model, "fixture-model"),
    ] {
        let home = HomeSandbox::new();
        let mut app = saved_guest_app(&home);
        app.tab = Tab::Setup;
        let name = ProfileName::from("work");
        let mut draft = build_draft_existing(&app, &name);
        if field == ConfigRow::BaseUrl {
            // This fixture owns an OAuth pair. A managed endpoint needs its
            // own inference key; load_profile deliberately drops an endpoint
            // that would otherwise receive that OAuth bearer.
            draft.api_key = InputState::new("fixture-endpoint-key");
        }
        *draft.field_mut(field).expect("typed field") = InputState::new(value);
        draft.active = Some(field);
        app.config_draft = Some(draft);
        let before = globals(&home);
        assert_eq!(commit_config_field(&mut app, field), CommitResult::Landed);
        let target = if field == ConfigRow::Name {
            ProfileName::from(value)
        } else {
            name
        };
        let saved = crate::profile::load_profile(&target).expect("local commit persisted");
        match field {
            ConfigRow::Name => assert_eq!(saved.name, target),
            ConfigRow::BaseUrl => assert_eq!(saved.base_url.as_deref(), Some(value)),
            ConfigRow::Model => assert_eq!(saved.models.default.as_deref(), Some(value)),
            _ => unreachable!(),
        }
        assert_globals_unchanged(&before);
        assert_no_source_calls();
    }
}

#[test]
fn guest_setup_env_add_edit_save_all_and_discard_preserve_globals() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    app.tab = Tab::Setup;
    let name = ProfileName::from("work");
    app.config_draft = Some(build_draft_existing(&app, &name));
    let before = globals(&home);
    assert!(env_add_commit(&mut app, &name, "TOLLGATE_GUEST_ADDED"));
    assert_globals_unchanged(&before);
    let idx = app
        .config()
        .find(&name)
        .expect("profile")
        .env
        .keys()
        .position(|k| k == "TOLLGATE_GUEST_ADDED")
        .expect("added env");
    app.config_draft.as_mut().expect("draft").env_value = InputState::new("after");
    assert_eq!(
        commit_config_field(&mut app, ConfigRow::EnvEntry(idx)),
        CommitResult::Landed
    );
    assert_eq!(
        app.config()
            .find(&name)
            .expect("profile")
            .env
            .get("TOLLGATE_GUEST_ADDED")
            .map(String::as_str),
        Some("after")
    );
    assert_globals_unchanged(&before);

    assert!(env_add_commit(&mut app, &name, "TOLLGATE_GUEST_CANCELLED"));
    let idx = app
        .config()
        .find(&name)
        .expect("profile")
        .env
        .keys()
        .position(|k| k == "TOLLGATE_GUEST_CANCELLED")
        .expect("temporary env");
    cancel_config_edit(&mut app, ConfigRow::EnvEntry(idx));
    assert!(
        !app.config()
            .find(&name)
            .expect("profile")
            .env
            .contains_key("TOLLGATE_GUEST_CANCELLED")
    );
    assert_globals_unchanged(&before);

    let mut draft = build_draft_existing(&app, &name);
    draft.model = InputState::new("saved-model");
    draft.active = Some(ConfigRow::Model);
    draft.field_seed = Some(String::new());
    app.config_draft = Some(draft);
    assert!(try_save_all(
        &mut app,
        &[Dirty::SetupField],
        LossWarningTarget::Switch(Tab::Overview)
    ));
    assert_eq!(
        app.config()
            .find(&name)
            .expect("profile")
            .models
            .default
            .as_deref(),
        Some("saved-model")
    );
    assert_globals_unchanged(&before);

    app.tab = Tab::Setup;
    let mut draft = build_draft_existing(&app, &name);
    draft.model = InputState::new("discarded-model");
    draft.active = Some(ConfigRow::Model);
    app.config_draft = Some(draft);
    discard_dirty(&mut app, &[Dirty::SetupField]);
    assert_eq!(
        app.config()
            .find(&name)
            .expect("profile")
            .models
            .default
            .as_deref(),
        Some("saved-model")
    );
    assert_globals_unchanged(&before);
    app.config_draft = Some(build_draft_existing(&app, &name));
    cycle_model(&mut app);
    assert_ne!(
        app.config()
            .find(&name)
            .expect("profile")
            .models
            .default
            .as_deref(),
        Some("saved-model")
    );
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_preset_rebind_updates_only_the_local_models() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let before = globals(&home);
    let form = RebindForm {
        blocks: vec![crate::presets::RebindBlock {
            rows: vec![crate::presets::RowMove {
                row: "default",
                old: None,
                new: Some("rebound-model".to_string()),
            }],
            accounts: vec!["work".to_string()],
        }],
        checked: vec![vec![true]],
        cursor: 0,
        choice: true,
        scroll: Default::default(),
        starts: Default::default(),
        block_ends: Default::default(),
        total: Default::default(),
        viewport: Default::default(),
        digest: "guest-rebind".to_string(),
    };
    apply_rebind(&mut app, form);
    assert_eq!(
        app.config()
            .find(&ProfileName::from("work"))
            .expect("profile")
            .models
            .default
            .as_deref(),
        Some("rebound-model")
    );
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}

#[test]
fn guest_rotate_confirms_never_spend_refresh_tokens() {
    let home = HomeSandbox::new();
    let mut app = saved_guest_app(&home);
    let before = globals(&home);
    let stores = local_store_bytes(&home);
    for action in [
        ConfirmAction::RotateAll,
        ConfirmAction::RotateOne("work".to_string()),
    ] {
        app.toasts.clear();
        run_confirm_action(&mut app, action);
        assert_refused(&app);
        assert_globals_unchanged(&before);
        assert_eq!(local_store_bytes(&home), stores);
    }
    let _endpoints = crate::testutil::EndpointSandbox::new(&home, "http://127.0.0.1:1");
    assert!(matches!(
        oauth::refresh_result("fixture-refresh", None),
        Err(oauth::RefreshError::Transient(oauth::TokenFailure::Guest))
    ));
    assert_no_source_calls();
}

#[cfg(unix)]
#[test]
fn guest_tui_codex_delete_preserves_the_operator_auth_link() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    crate::codex_profiles::CodexState::update(|state| state.add_profile("cx"))
        .expect("codex roster");
    let target = crate::profile::profile_dir(&ProfileName::from("cx"))
        .expect("codex store")
        .join("auth.json");
    std::fs::create_dir_all(target.parent().expect("store parent")).expect("store directory");
    std::fs::write(&target, LIVE_CODEX).expect("codex store");
    let slot = home.home().join(".codex/auth.json");
    std::fs::remove_file(&slot).expect("remove our regular fixture");
    std::os::unix::fs::symlink(&target, &slot).expect("adopted link fixture");
    app.codex_rows = codex_rows();
    perform_codex_delete(&mut app, "cx");
    assert_eq!(
        std::fs::read_link(&slot).expect("operator link preserved"),
        target
    );
    assert!(
        slot.symlink_metadata()
            .expect("slot remains")
            .file_type()
            .is_symlink()
    );
    assert!(
        !crate::codex_profiles::CodexState::load()
            .expect("roster")
            .holds("cx"),
        "the local delete still lands"
    );
}

#[cfg(unix)]
#[test]
fn guest_tui_install_confirm_preserves_foreign_registry_keys() {
    use crate::testutil::{ConfigDirSandbox, EnvPin, FakeClaude};
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    let claude = home.home().join(".claude");
    let _config = ConfigDirSandbox::new(&home, &claude);
    let fake = FakeClaude::new(&home);
    let _clobber = EnvPin::new(&home, &[("CLAUDE_SHIM_CLOBBER", Some("1".as_ref()))]);
    write_fixture(
        home.home(),
        ".claude/plugins/known_marketplaces.json",
        br#"{"foreign":{"source":{"source":"github","repo":"fixture/foreign"}}}"#,
    );
    fn foreign(home: &Path) -> Vec<serde_json::Map<String, serde_json::Value>> {
        use crate::guest_write::{
            CLAUDE_JSON_KEYS, INSTALLED_PLUGINS_KEYS, KNOWN_MARKETPLACES_KEYS, SETTINGS_KEYS,
        };
        [
            (home.join(".claude.json"), CLAUDE_JSON_KEYS),
            (home.join(".claude/settings.json"), SETTINGS_KEYS),
            (
                home.join(".claude/plugins/installed_plugins.json"),
                INSTALLED_PLUGINS_KEYS,
            ),
            (
                home.join(".claude/plugins/known_marketplaces.json"),
                KNOWN_MARKETPLACES_KEYS,
            ),
        ]
        .into_iter()
        .map(|(path, owned)| {
            crate::guest_write::foreign_view(
                &crate::guest_write::read_object(&path)
                    .expect("fixture JSON")
                    .expect("fixture exists"),
                owned,
            )
        })
        .collect()
    }
    let before = foreign(home.home());
    run_confirm_action(&mut app, ConfirmAction::InstallPlugin);
    assert!(
        fake.log()
            .lines()
            .any(|l| l == "plugin install tollgate@tollgate --scope user"),
        "the TUI's confirmed install reaches the guarded leaf"
    );
    assert_eq!(
        foreign(home.home()),
        before,
        "only tollgate-owned keys may change"
    );
    let installed = crate::guest_write::read_object(&claude.join("plugins/installed_plugins.json"))
        .expect("registry")
        .expect("installed registry");
    assert!(
        installed
            .get("plugins")
            .and_then(|p| p.get(crate::identity::CC_PLUGIN))
            .is_some(),
        "the allowed owned write still lands"
    );
}

#[test]
fn guest_claude_create_row_refuses_staged_login_before_activation() {
    let home = HomeSandbox::new();
    let mut app = guest_app(&home);
    {
        let mut cfg = app.config();
        cfg.state.active_profile = None;
    }
    app.tab = Tab::Setup;
    let mut captured = snapshot();
    captured.credentials =
        Some(serde_json::from_slice(LIVE_CLAUDE).expect("synthetic captured login"));
    let mut draft = build_draft_new();
    draft.name = InputState::new("new-captured");
    draft.captured_login = Some(DraftLogin::LiveLogin(captured));
    app.config_draft = Some(draft);
    let before = globals(&home);
    let stores = local_store_bytes(&home);
    run_config_row(&mut app, ConfigRow::Create);
    assert_refused(&app);
    assert!(
        app.config()
            .find(&ProfileName::from("new-captured"))
            .is_none()
    );
    assert!(app.config().state.active_profile.is_none());
    assert_eq!(local_store_bytes(&home), stores);
    assert_globals_unchanged(&before);
    assert_no_source_calls();
}
