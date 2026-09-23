//! One fixture with Claude, Codex, Grok, Antigravity, and an API account, plus
//! an unlisted monitor target. Every account list is the shipped draw.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::draw;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::provider_monitor::types::{
    ObservationState, ProviderData, ProviderKind, ProviderReport, QuotaBucket,
};
use crate::providers::Provider;
use crate::providers::{StatRow, StatRowKind, ThirdPartyStats};
use crate::tui::accounts::{AddChoice, RosterSlot};
use crate::tui::app::{
    App, CodexRow, HarnessFilter, Modal, OpenSelection, OverviewPick, Tab, current_overview_pick,
    handle_key,
};
use crate::usage::{PlanInfo, PlanTier, UsageInfo, UsageWindow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::KeyCode;
use std::collections::BTreeMap;

const CLAUDE_PLAN: &str = "Claude Max 20x";
const DEEPSEEK_PLAN: &str = "DeepSeekPlan";
const DEEPSEEK_ROW: &str = "ds-balance-row";
const CODEX_PLAN: &str = "prolite-plan";
const GROK_PLAN: &str = "SuperGrokPlus";
const GROK_WINDOW: &str = "grok-week";
const AGY_PLAN: &str = "GoogleAIPro";
const HIDDEN_PLAN: &str = "HiddenPlan";
const HIDDEN_ID: &str = "secret-monitor";
const HIDDEN_WINDOW: &str = "hidden-window";

fn claude_profile() -> Profile {
    Profile {
        name: "claude-max".into(),
        base_url: None,
        api_key: None,
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: Some(80.0),
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: 12.0,
                resets_at: None,
            }),
            plan: Some(PlanInfo {
                tier: PlanTier::Max(Some(20)),
                ..PlanInfo::default()
            }),
            ..UsageInfo::default()
        }),
        fetch_status: None,
        provider: None,
        third_party_usage: None,
        usage_stale: false,
    }
}

fn deepseek_profile() -> Profile {
    Profile {
        name: "ds-one".into(),
        base_url: Some("https://api.deepseek.com/anthropic".into()),
        api_key: Some("k".into()),
        auto_start: false,
        env: BTreeMap::new(),
        models: Default::default(),
        fallback_threshold: None,
        weekly_threshold: None,
        last_resort: false,
        preferred: false,
        rolling_token: false,
        max_auto_spend: None,
        check_weekly: true,
        check_scoped: true,
        bell_threshold: None,
        disabled: false,
        console: None,
        credentials: None,
        usage: None,
        fetch_status: None,
        provider: Some(crate::providers::Provider::DeepSeek),
        third_party_usage: Some(ThirdPartyStats {
            is_available: true,
            rows: vec![StatRow {
                label: DEEPSEEK_ROW.into(),
                value: "42".into(),
                kind: StatRowKind::Body,
            }],
            bars: Vec::new(),
            plan: Some(DEEPSEEK_PLAN.into()),
            endpoint: None,
            best_effort: false,
        }),
        usage_stale: false,
    }
}

fn report(
    id: &str,
    provider: ProviderKind,
    plan: &str,
    label: &str,
    listed: bool,
) -> ProviderReport {
    ProviderReport {
        id: id.into(),
        provider,
        tool: provider.tool().into(),
        model: None,
        state: ObservationState::Fresh,
        observed_at_ms: None,
        checked_at_ms: None,
        identity_checked_at_observation_only: false,
        data: ProviderData {
            plan: Some(plan.into()),
            buckets: vec![QuotaBucket {
                id: label.into(),
                label: label.into(),
                scope: "shared".into(),
                used_percent: Some(51.0),
                remaining_percent: Some(49.0),
                resets_at: None,
                window_seconds: Some(604_800),
                exhausted: false,
                ..QuotaBucket::default()
            }],
            ..ProviderData::default()
        },
        message: None,
        warning: false,
        listed,
        refresh: Default::default(),
    }
}

fn fixture() -> App {
    let profiles = vec![claude_profile(), deepseek_profile()];
    let names: Vec<ProfileName> = profiles
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: Some("claude-max".into()),
            profiles: names,
            fallback_chain: vec!["claude-max".into()],
            ..AppState::default()
        },
        profiles,
    });
    app.anim_phase_ms = Some(0);
    app.tick_count = 0;
    app.codex_rows = vec![CodexRow {
        name: "codex-one".into(),
        active: false,
        broken: false,
        plan: Some(CODEX_PLAN.into()),
        five_hour: None,
        seven_day: Some(UsageWindow {
            utilization: 91.0,
            resets_at: None,
        }),
        fetched_at: None,
        limit_reached: None,
        reset_credits: None,
        poll: None,
    }];
    app.provider_reports = vec![
        report(
            "grok-desk",
            ProviderKind::Grok,
            GROK_PLAN,
            GROK_WINDOW,
            true,
        ),
        report(
            "agy-main",
            ProviderKind::Antigravity,
            AGY_PLAN,
            "agy-week",
            true,
        ),
        report(
            HIDDEN_ID,
            ProviderKind::Grok,
            HIDDEN_PLAN,
            HIDDEN_WINDOW,
            false,
        ),
    ];
    app
}

fn frame(app: &App) -> String {
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| draw(f, app)).unwrap();
    crate::testutil::buffer_rows(term.backend().buffer()).join("\n")
}

fn twice(app: &mut App, tab: Tab) -> String {
    app.tab = tab;
    let first = frame(app);
    let second = frame(app);
    assert_eq!(
        first, second,
        "{tab:?} redrew differently at one elapsed time"
    );
    first
}

fn name_column(text: &str, name: &str) -> usize {
    text.lines()
        .filter(|line| !line.contains("accounts"))
        .filter_map(|line| {
            let byte = line.find(name)?;
            let col = line[..byte].chars().count();
            // The detail title sits in the right pane. The list name is the left one.
            (col < 24).then_some(col)
        })
        .min()
        .unwrap_or_else(|| panic!("{name} missing from the account list\n{text}"))
}

fn group_order(text: &str) {
    let spark = text.find('✳').expect("claude mark");
    let deep = text.find("DeepSeek").expect("deepseek group");
    let codex = text.find('▣').expect("codex mark");
    let grok = text.find('✶').expect("grok mark");
    let agy = text.find('✧').expect("antigravity mark");
    assert!(
        spark < deep && deep < codex && codex < grok && grok < agy,
        "group order drifted\n{text}"
    );
    let header = text
        .lines()
        .find(|line| line.contains('✳'))
        .expect("claude header");
    assert!(
        !header.contains("claude-max"),
        "the group title paints over an account\n{header}"
    );
}

fn absent_account(text: &str) {
    assert!(
        !text.contains(HIDDEN_ID),
        "unlisted monitor is on an account list\n{text}"
    );
    assert!(
        !text.contains(HIDDEN_PLAN),
        "unlisted plan is on an account list\n{text}"
    );
    assert!(
        !text.contains(HIDDEN_WINDOW),
        "unlisted window is on an account list\n{text}"
    );
}

fn press(app: &mut App, code: KeyCode) {
    handle_key(app, crate::testutil::key(code));
}

fn scratch(name: &str, text: &str) {
    let Ok(dir) = std::env::var("CLAUTH_GOAL_SCRATCH") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let path = std::path::Path::new(&dir);
    std::fs::create_dir_all(path).expect("scratch");
    std::fs::write(path.join(name), text).expect("write scratch");
}

#[test]
fn every_account_list_groups_added_providers_and_hides_unlisted_monitors() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();

    let overview = twice(&mut app, Tab::Overview);
    group_order(&overview);
    absent_account(&overview);
    assert!(
        overview.contains("5 accounts"),
        "header counts every added account\n{overview}"
    );
    assert!(!overview.contains("6 accounts"), "{overview}");
    assert!(
        overview.contains(GROK_PLAN)
            && overview.contains(AGY_PLAN)
            && overview.contains(CODEX_PLAN)
    );
    let overview_cols = [
        name_column(&overview, "ds-one"),
        name_column(&overview, "codex-one"),
        name_column(&overview, "grok-desk"),
        name_column(&overview, "agy-main"),
    ];
    assert!(
        overview_cols.windows(2).all(|pair| pair[0] == pair[1]),
        "overview names drifted: {overview_cols:?}"
    );

    app.open = OpenSelection::Account(RosterSlot::Profile(0));
    let usage_claude = twice(&mut app, Tab::Usage);
    group_order(&usage_claude);
    absent_account(&usage_claude);
    assert!(usage_claude.contains(CLAUDE_PLAN), "{usage_claude}");
    assert!(
        !usage_claude.contains(GROK_PLAN),
        "grok plan leaked into the claude usage\n{usage_claude}"
    );
    assert!(!usage_claude.contains(DEEPSEEK_PLAN), "{usage_claude}");
    assert!(!usage_claude.contains(CODEX_PLAN), "{usage_claude}");
    assert!(!usage_claude.contains(AGY_PLAN), "{usage_claude}");
    let usage_cols = [
        name_column(&usage_claude, "claude-max"),
        name_column(&usage_claude, "ds-one"),
        name_column(&usage_claude, "codex-one"),
        name_column(&usage_claude, "grok-desk"),
        name_column(&usage_claude, "agy-main"),
    ];
    assert!(
        usage_cols.windows(2).all(|pair| pair[0] == pair[1]),
        "{usage_cols:?}"
    );

    app.open = OpenSelection::Account(RosterSlot::Native(0));
    let usage_grok = twice(&mut app, Tab::Usage);
    // The Claude layout: a plan row, the status block, and the shared weekly
    // bucket as a `7d` bar rather than a key/value line under its own label.
    assert!(
        usage_grok.contains(GROK_PLAN)
            && usage_grok.contains("status")
            && usage_grok.contains("7d")
            && usage_grok.contains('█'),
        "{usage_grok}"
    );
    assert!(!usage_grok.contains(GROK_WINDOW), "{usage_grok}");
    assert!(
        !usage_grok.contains(CLAUDE_PLAN),
        "claude plan leaked into grok usage\n{usage_grok}"
    );
    assert!(
        !usage_grok.contains(DEEPSEEK_PLAN) && !usage_grok.contains(DEEPSEEK_ROW),
        "{usage_grok}"
    );
    assert!(
        !usage_grok.contains(CODEX_PLAN) && !usage_grok.contains(AGY_PLAN),
        "{usage_grok}"
    );
    absent_account(&usage_grok);

    app.open = OpenSelection::Account(RosterSlot::Profile(1));
    let usage_ds = twice(&mut app, Tab::Usage);
    assert!(
        usage_ds.contains(DEEPSEEK_PLAN) && usage_ds.contains(DEEPSEEK_ROW),
        "{usage_ds}"
    );
    assert!(
        !usage_ds.contains(CLAUDE_PLAN) && !usage_ds.contains(GROK_PLAN),
        "{usage_ds}"
    );

    app.open = OpenSelection::Account(RosterSlot::Codex(0));
    let usage_codex = twice(&mut app, Tab::Usage);
    assert!(usage_codex.contains(CODEX_PLAN), "{usage_codex}");
    assert!(
        !usage_codex.contains(CLAUDE_PLAN) && !usage_codex.contains(GROK_PLAN),
        "{usage_codex}"
    );

    app.open = OpenSelection::Account(RosterSlot::Profile(0));
    let setup_claude = twice(&mut app, Tab::Setup);
    group_order(&setup_claude);
    absent_account(&setup_claude);
    assert!(
        setup_claude.contains("auto-start"),
        "claude setup lost its own rows\n{setup_claude}"
    );
    assert!(
        !setup_claude.contains(GROK_PLAN) && !setup_claude.contains(CODEX_PLAN),
        "{setup_claude}"
    );

    app.open = OpenSelection::Account(RosterSlot::Native(0));
    let setup_grok = twice(&mut app, Tab::Setup);
    assert!(setup_grok.contains(GROK_PLAN), "{setup_grok}");
    assert!(
        !setup_grok.contains("auto-start"),
        "claude settings opened for grok\n{setup_grok}"
    );
    assert!(
        !setup_grok.contains("api key"),
        "claude credential row opened for grok\n{setup_grok}"
    );
    assert!(
        !setup_grok.contains(CLAUDE_PLAN) && !setup_grok.contains(DEEPSEEK_PLAN),
        "{setup_grok}"
    );
    absent_account(&setup_grok);

    app.open = OpenSelection::Account(RosterSlot::Profile(0));
    app.profile_cursor = 0;
    app.roster_pick = None;
    app.selection_is_add = false;
    let fallback = twice(&mut app, Tab::Fallback);
    assert!(fallback.contains("claude-max"), "{fallback}");
    assert!(
        !fallback.contains("ds-one"),
        "an api account joined the claude chain\n{fallback}"
    );
    assert!(
        !fallback.contains(GROK_PLAN)
            && !fallback.contains(CODEX_PLAN)
            && !fallback.contains(HIDDEN_ID),
        "{fallback}"
    );

    let providers = twice(&mut app, Tab::Providers);
    assert!(
        providers.contains("antigravity"),
        "agy group was not renamed\n{providers}"
    );
    assert!(
        providers.contains(HIDDEN_ID),
        "a monitor reading disappeared\n{providers}"
    );
    let hidden = providers
        .lines()
        .find(|line| line.contains(HIDDEN_WINDOW))
        .expect("hidden window row");
    assert!(hidden.contains('—'), "missing reset was invented: {hidden}");
    let claude_at = providers.find('✳').expect("claude mark");
    let grok_at = providers.find('✶').expect("grok mark");
    let agy_at = providers.find('✧').expect("antigravity mark");
    assert!(claude_at < grok_at && grok_at < agy_at, "{providers}");

    scratch(
        "screens-grouped.txt",
        &format!(
            "# overview\n{overview}\n# usage claude\n{usage_claude}\n# usage grok\n{usage_grok}\n# setup grok\n{setup_grok}\n# fallback\n{fallback}\n# providers\n{providers}\n"
        ),
    );
}

#[test]
fn product_screens_stay_on_their_own_data_when_other_providers_exist() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();
    let tokens = twice(&mut app, Tab::Tokens);
    assert!(
        tokens.contains("stats-cache.json"),
        "tokens frame is empty\n{tokens}"
    );
    assert!(
        !tokens.contains(GROK_PLAN) && !tokens.contains(CODEX_PLAN) && !tokens.contains(AGY_PLAN),
        "{tokens}"
    );

    let status = twice(&mut app, Tab::Status);
    assert!(
        status.contains("status.claude.ai") || status.contains("no status data"),
        "{status}"
    );
    assert!(
        !status.contains(GROK_PLAN)
            && !status.contains(HIDDEN_PLAN)
            && !status.contains(CODEX_PLAN),
        "{status}"
    );

    let plugin = twice(&mut app, Tab::Plugin);
    assert!(
        plugin.contains("no checks yet") || plugin.contains("claude"),
        "{plugin}"
    );
    assert!(
        !plugin.contains(GROK_PLAN) && !plugin.contains(AGY_PLAN) && !plugin.contains(CODEX_PLAN),
        "{plugin}"
    );

    let config = twice(&mut app, Tab::Config);
    assert!(config.contains("SETTINGS"), "{config}");
    assert!(
        !config.contains(GROK_PLAN) && !config.contains("ds-one") && !config.contains("codex-one"),
        "{config}"
    );

    app.open = OpenSelection::Account(RosterSlot::Profile(0));
    app.profile_cursor = 0;
    app.roster_pick = None;
    app.selection_is_add = false;
    let fallback = twice(&mut app, Tab::Fallback);
    assert!(
        fallback.contains("claude-max") && !fallback.contains("ds-one"),
        "{fallback}"
    );

    scratch(
        "screens-scoped.txt",
        &format!(
            "# tokens\n{tokens}\n# status\n{status}\n# plugin\n{plugin}\n# config\n{config}\n# fallback\n{fallback}\n"
        ),
    );
}

#[test]
fn overview_selection_is_the_account_usage_and_setup_open() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();
    app.tab = Tab::Overview;

    // claude-max, ds-one, then codex-one. Passing the API row must not stick.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Usage);
    let usage = frame(&app);
    assert!(
        usage.contains(CODEX_PLAN),
        "usage did not open the codex row overview had selected\n{usage}"
    );
    assert!(
        !usage.contains(DEEPSEEK_PLAN),
        "usage opened the api account overview had already left\n{usage}"
    );

    press(&mut app, KeyCode::Down);
    let grok = frame(&app);
    assert!(
        grok.contains(GROK_PLAN) && !grok.contains(CODEX_PLAN),
        "down from codex did not open grok\n{grok}"
    );

    press(&mut app, KeyCode::Left);
    assert_eq!(app.tab, Tab::Overview);
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Right);
    let back = frame(&app);
    assert!(
        back.contains(DEEPSEEK_PLAN) && !back.contains(GROK_PLAN),
        "leaving grok for a claude row did not clear it\n{back}"
    );

    press(&mut app, KeyCode::Left);
    // ds-one, down to codex, down to grok. Enter must not open add.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert!(app.modals.is_empty(), "enter on grok opened the add dialog");

    app.harness_filter = HarnessFilter::Claude;
    app.tab = Tab::Overview;
    let filtered = frame(&app);
    assert!(
        filtered.contains("grok-desk") && filtered.contains("agy-main"),
        "the claude filter hid added native accounts\n{filtered}"
    );
    assert!(
        filtered.contains("4 accounts"),
        "header dropped the native accounts the table still shows\n{filtered}"
    );
    assert!(
        !filtered.contains("codex-one"),
        "the claude filter still listed codex\n{filtered}"
    );
}

fn highlighted(text: &str) -> String {
    text.lines()
        .find(|line| line.contains('❯'))
        .unwrap_or_else(|| panic!("no highlighted row\n{text}"))
        .to_string()
}

fn open_is_profile(app: &App, idx: usize) {
    assert!(!app.selection_is_add, "add is not the open slot");
    assert!(app.roster_pick.is_none(), "a native slot is still open");
    assert_eq!(app.profile_cursor, idx);
    assert_eq!(
        current_overview_pick(app),
        Some(OverviewPick::Claude(idx)),
        "overview highlight is not that profile"
    );
    assert_eq!(super::panes::open_slot(app), Some(RosterSlot::Profile(idx)));
}

#[test]
fn a_hidden_account_and_the_add_row_stay_the_open_selection() {
    let _home = crate::testutil::HomeSandbox::new();

    // Codex filter: c, c, Right, Up. The hidden API account becomes the one slot.
    let mut app = fixture();
    app.tab = Tab::Overview;
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Up);
    open_is_profile(&app, 1);
    let usage = frame(&app);
    assert!(
        usage.contains(DEEPSEEK_PLAN) && !usage.contains(CODEX_PLAN),
        "up did not open the hidden api account\n{usage}"
    );
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Setup);
    let setup = frame(&app);
    assert!(
        setup.contains("api.deepseek.com")
            && !setup.contains("auto-start")
            && !setup.contains(CODEX_PLAN),
        "setup did not open the same account\n{setup}"
    );
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    assert_eq!(app.tab, Tab::Overview);
    let row = highlighted(&frame(&app));
    assert!(
        row.contains("ds-one") && !row.contains("codex-one"),
        "overview kept highlighting codex\n{row}"
    );
    press(&mut app, KeyCode::Right);
    let again = frame(&app);
    assert!(
        again.contains(DEEPSEEK_PLAN) && !again.contains(CODEX_PLAN),
        "returning to usage opened a different account\n{again}"
    );

    // Claude filter: c, Right, Down, Down. Codex becomes the one slot.
    let mut app = fixture();
    app.tab = Tab::Overview;
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert!(!app.selection_is_add);
    assert_eq!(app.roster_pick, Some(RosterSlot::Codex(0)));
    assert_eq!(current_overview_pick(&app), Some(OverviewPick::Codex(0)));
    assert_eq!(super::panes::open_slot(&app), Some(RosterSlot::Codex(0)));
    let usage = frame(&app);
    assert!(
        usage.contains(CODEX_PLAN) && !usage.contains(DEEPSEEK_PLAN),
        "down did not open codex\n{usage}"
    );
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Setup);
    let setup = frame(&app);
    assert!(
        setup.contains(CODEX_PLAN) && !setup.contains(DEEPSEEK_PLAN),
        "setup did not open codex\n{setup}"
    );
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    let row = highlighted(&frame(&app));
    assert!(
        row.contains("codex-one") && !row.contains("ds-one"),
        "overview kept highlighting the api account\n{row}"
    );

    // Five Downs from claude-max land on + add account. That is not ds-one.
    let mut app = fixture();
    app.tab = Tab::Overview;
    for _ in 0..5 {
        press(&mut app, KeyCode::Down);
    }
    assert!(app.selection_is_add);
    assert!(app.roster_pick.is_none());
    assert!(app.profile_cursor >= app.profile_count());
    assert_eq!(current_overview_pick(&app), Some(OverviewPick::Add));
    assert!(super::panes::open_slot(&app).is_none());
    press(&mut app, KeyCode::Right);
    let usage = frame(&app);
    assert!(
        usage.contains("no accounts yet")
            && !usage.contains(DEEPSEEK_PLAN)
            && !usage.contains(GROK_PLAN)
            && !usage.contains(CODEX_PLAN),
        "add opened an account\n{usage}"
    );
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Setup);
    let setup = frame(&app);
    assert!(
        setup.contains("NEW ACCOUNT") && !setup.contains("api.deepseek.com"),
        "setup resolved add through the previous profile\n{setup}"
    );
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    assert_eq!(current_overview_pick(&app), Some(OverviewPick::Add));
    let row = highlighted(&frame(&app));
    assert!(
        row.contains("add account") && !row.contains("ds-one"),
        "leaving usage snapped the highlight off add\n{row}"
    );

    // Usage Up from + add account selects the last account, same as Overview Up.
    let mut app = fixture();
    app.tab = Tab::Overview;
    for _ in 0..5 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.open, OpenSelection::Account(RosterSlot::Native(1)));
    assert_eq!(current_overview_pick(&app), Some(OverviewPick::Native(1)));
    let usage = frame(&app);
    assert!(
        usage.contains(AGY_PLAN) && !usage.contains(GROK_PLAN),
        "up from add opened the second-to-last account\n{usage}"
    );
    let mut app = fixture();
    app.tab = Tab::Overview;
    for _ in 0..5 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Up);
    assert_eq!(current_overview_pick(&app), Some(OverviewPick::Native(1)));
    let row = highlighted(&frame(&app));
    assert!(
        row.contains("agy-main"),
        "overview up from add did not land on the last account\n{row}"
    );
}

#[test]
fn fallback_does_not_replace_a_grok_or_add_selection() {
    let _home = crate::testutil::HomeSandbox::new();

    // grok-desk, Right until Fallback, then back to Usage. Still Grok.
    let mut app = fixture();
    app.tab = Tab::Overview;
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.open, OpenSelection::Account(RosterSlot::Native(0)));
    for _ in 0..4 {
        press(&mut app, KeyCode::Right);
    }
    assert_eq!(app.tab, Tab::Fallback);
    for _ in 0..3 {
        press(&mut app, KeyCode::Left);
    }
    assert_eq!(app.tab, Tab::Usage);
    assert_eq!(app.open, OpenSelection::Account(RosterSlot::Native(0)));
    let usage = frame(&app);
    assert!(
        usage.contains(GROK_PLAN) && !usage.contains(CLAUDE_PLAN),
        "fallback replaced grok with the claude chain member\n{usage}"
    );

    // + add account survives the same trip.
    let mut app = fixture();
    app.tab = Tab::Overview;
    for _ in 0..5 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(app.open, OpenSelection::Add);
    for _ in 0..4 {
        press(&mut app, KeyCode::Right);
    }
    assert_eq!(app.tab, Tab::Fallback);
    for _ in 0..3 {
        press(&mut app, KeyCode::Left);
    }
    assert_eq!(app.tab, Tab::Usage);
    assert_eq!(app.open, OpenSelection::Add);
    let usage = frame(&app);
    assert!(
        usage.contains("no accounts yet") && !usage.contains(CLAUDE_PLAN),
        "fallback left add for the claude chain member\n{usage}"
    );
}

#[test]
fn new_account_clears_a_codex_or_grok_selection() {
    let _home = crate::testutil::HomeSandbox::new();

    // Setup, Down onto + new, then back to Usage. Not the previous API profile.
    let mut app = fixture();
    app.tab = Tab::Overview;
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.tab, Tab::Setup);
    for _ in 0..5 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(app.open, OpenSelection::Add);
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    assert_eq!(app.tab, Tab::Usage);
    assert_eq!(app.open, OpenSelection::Add);
    let usage = frame(&app);
    assert!(
        usage.contains("no accounts yet") && !usage.contains(DEEPSEEK_PLAN),
        "setup + new opened ds-one on usage\n{usage}"
    );
    press(&mut app, KeyCode::Left);
    assert_eq!(current_overview_pick(&app), Some(OverviewPick::Add));
    let row = highlighted(&frame(&app));
    assert!(
        row.contains("add account") && !row.contains("ds-one"),
        "overview highlight left add\n{row}"
    );

    // n while Codex is open.
    let mut app = fixture();
    app.tab = Tab::Overview;
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.tab, Tab::Setup);
    assert_eq!(app.open, OpenSelection::Add);
    let setup = frame(&app);
    assert!(
        setup.contains("NEW ACCOUNT") && !setup.contains(CODEX_PLAN),
        "n kept the codex settings\n{setup}"
    );

    // n while Grok is open.
    let mut app = fixture();
    app.tab = Tab::Overview;
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.open, OpenSelection::Add);
    let setup = frame(&app);
    assert!(
        setup.contains("NEW ACCOUNT") && !setup.contains(GROK_PLAN),
        "n kept the grok settings\n{setup}"
    );

    // Add-dialog "new API account" while Codex is open, then while Grok is open.
    for downs in [2usize, 3] {
        let mut app = fixture();
        app.tab = Tab::Overview;
        for _ in 0..downs {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Char('n'));
        let steps = {
            let Some(Modal::AddAccount(form)) = app.modals.last() else {
                panic!("n on overview did not open add");
            };
            form.choices
                .iter()
                .position(|choice| matches!(choice, AddChoice::NewApi(Provider::DeepSeek)))
                .expect("deepseek is a choice")
        };
        for _ in 0..steps {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.tab, Tab::Setup);
        assert_eq!(app.open, OpenSelection::Add);
        let setup = frame(&app);
        assert!(
            setup.contains("NEW ACCOUNT")
                && !setup.contains(CODEX_PLAN)
                && !setup.contains(GROK_PLAN),
            "new api account kept the previous provider\n{setup}"
        );
    }
}

fn confirm_message(app: &App) -> String {
    match app.modals.last() {
        Some(Modal::Confirm(state)) => state.message.clone(),
        other => panic!("expected a confirm, got {other:?}"),
    }
}

fn profile_app(names: &[&str]) -> App {
    let profiles: Vec<Profile> = names
        .iter()
        .map(|name| Profile::new((*name).to_string(), None, None))
        .collect();
    let stored: Vec<ProfileName> = profiles
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    let mut app = App::new(AppConfig {
        state: AppState {
            active_profile: stored.first().cloned(),
            profiles: stored,
            ..AppState::default()
        },
        profiles,
    });
    app.tab = Tab::Overview;
    app
}

/// Overview `a` then `e` deletes the highlighted account. Cancel leaves it.
/// A live Claude session asks a second time. Codex is removed. A listed Grok
/// login leaves the overview and stays on the Providers tab.
#[test]
fn overview_delete_removes_the_highlighted_account() {
    let home = crate::testutil::HomeSandbox::new();
    let _live = crate::testutil::arm_live_session(home.home(), "drop-acct");

    let mut app = profile_app(&["keep", "drop-acct"]);
    press(&mut app, KeyCode::Down);
    assert_eq!(
        current_overview_pick(&app),
        Some(OverviewPick::Claude(1)),
        "the second account is highlighted"
    );

    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('e'));
    assert_eq!(confirm_message(&app), "delete 'drop-acct'?");
    press(&mut app, KeyCode::Enter);
    assert!(
        app.config().find(&ProfileName::from("drop-acct")).is_some(),
        "cancel leaves the account"
    );
    assert!(app.modals.is_empty());

    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('e'));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Enter);
    assert_eq!(confirm_message(&app), "delete 'drop-acct' anyway?");
    assert!(app.config().find(&ProfileName::from("drop-acct")).is_some());
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Enter);
    assert!(
        app.config().find(&ProfileName::from("drop-acct")).is_none(),
        "the second confirm deletes the live account"
    );
    assert!(app.config().find(&ProfileName::from("keep")).is_some());
    let overview = account_rows(&frame(&app));
    assert!(
        overview.contains("keep") && !overview.contains("drop-acct"),
        "overview still shows the deleted account\n{overview}"
    );

    crate::codex_profiles::CodexState::update(|state| {
        state.add_profile("codex-drop");
        Ok(())
    })
    .expect("codex roster");
    app.codex_rows = crate::tui::app::codex_rows();
    // After the profile delete the highlight sits on keep. Step onto codex.
    press(&mut app, KeyCode::Down);
    assert!(
        matches!(current_overview_pick(&app), Some(OverviewPick::Codex(0))),
        "codex row is highlighted, got {:?}",
        current_overview_pick(&app)
    );
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('e'));
    assert_eq!(confirm_message(&app), "delete 'codex-drop'?");
    press(&mut app, KeyCode::Enter);
    assert!(
        crate::codex_profiles::CodexState::load()
            .expect("codex state")
            .holds("codex-drop"),
        "cancel leaves the codex account"
    );
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('e'));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Enter);
    assert!(
        !crate::codex_profiles::CodexState::load()
            .expect("codex state")
            .holds("codex-drop"),
        "confirm removes the codex account"
    );
    let overview = account_rows(&frame(&app));
    assert!(
        !overview.contains("codex-drop"),
        "overview still shows the deleted codex account\n{overview}"
    );

    crate::provider_monitor::config::ensure_native_target(
        ProviderKind::Grok,
        Some("https://auth.x.ai::desk".into()),
    )
    .expect("list grok");
    app.provider_reports = crate::provider_monitor::reports().expect("reports");
    // Highlight is wherever the codex delete left it. Walk until the grok row.
    for _ in 0..6 {
        if matches!(current_overview_pick(&app), Some(OverviewPick::Native(_))) {
            break;
        }
        press(&mut app, KeyCode::Down);
    }
    assert!(
        matches!(current_overview_pick(&app), Some(OverviewPick::Native(_))),
        "grok row is highlighted, got {:?}",
        current_overview_pick(&app)
    );
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Char('e'));
    assert_eq!(confirm_message(&app), "delete 'grok'?");
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Enter);
    let saved = crate::provider_monitor::config::load().expect("providers.toml");
    let grok = saved
        .targets
        .iter()
        .find(|target| target.id == "grok")
        .expect("the monitor target stays");
    assert!(!grok.listed, "the account is no longer listed");
    app.tab = Tab::Overview;
    let overview = account_rows(&frame(&app));
    assert!(
        !overview.contains("grok"),
        "overview still lists the removed grok account\n{overview}"
    );
    app.tab = Tab::Providers;
    let providers = account_rows(&frame(&app));
    assert!(
        providers.contains("grok"),
        "the providers tab dropped the monitor\n{providers}"
    );
}

/// The account table, without the header toast. A delete toast names the
/// account that just left, and that line is not a row.
fn account_rows(text: &str) -> String {
    text.lines()
        .filter(|line| !line.contains("deleted '") && !line.contains("removed '"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn toast_bodies(app: &App) -> Vec<String> {
    app.toasts.iter().map(|t| t.body.clone()).collect()
}

#[test]
fn usage_r_refreshes_a_codex_or_native_row_like_a_claude_one() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();
    app.tab = Tab::Usage;

    app.open = OpenSelection::Account(RosterSlot::Codex(0));
    press(&mut app, KeyCode::Char('r'));
    assert!(
        crate::usage::codex_poll("codex-one").is_some_and(|p| p.queued),
        "the codex profile is queued for the next poll"
    );

    app.open = OpenSelection::Account(RosterSlot::Native(0));
    press(&mut app, KeyCode::Char('r'));

    let toasts = toast_bodies(&app);
    assert!(
        toasts.iter().any(|t| t == "refreshing 'codex-one'"),
        "{toasts:?}"
    );
    assert!(
        toasts.iter().any(|t| t == "refreshing 'grok-desk'"),
        "{toasts:?}"
    );
    assert!(
        !toasts.iter().any(|t| t.contains("its own provider")),
        "{toasts:?}"
    );
}

#[test]
fn overview_r_queues_codex_profiles_with_the_claude_accounts() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();
    app.codex_rows[0].name = "codex-overview-r".into();
    app.tab = Tab::Overview;
    press(&mut app, KeyCode::Char('r'));
    assert!(crate::usage::codex_poll("codex-overview-r").is_some_and(|p| p.queued));
    assert!(
        toast_bodies(&app)
            .iter()
            .any(|t| t == "refreshing every account")
    );
}

#[test]
fn codex_and_native_usage_use_the_claude_layout() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();

    app.open = OpenSelection::Account(RosterSlot::Codex(0));
    let codex = twice(&mut app, Tab::Usage);
    scratch("usage-codex.txt", &codex);
    for piece in ["plan", CODEX_PLAN, "status", "7d", "91%", "█", "░"] {
        assert!(
            codex.contains(piece),
            "codex usage lacks {piece:?}\n{codex}"
        );
    }
    assert!(
        !codex.contains("5h %") && !codex.contains("7d %"),
        "{codex}"
    );

    app.open = OpenSelection::Account(RosterSlot::Native(1));
    let agy = twice(&mut app, Tab::Usage);
    scratch("usage-agy.txt", &agy);
    for piece in ["plan", AGY_PLAN, "status", "7d", "51%", "█"] {
        assert!(agy.contains(piece), "agy usage lacks {piece:?}\n{agy}");
    }
    assert!(
        !agy.contains("│ provider "),
        "the old key/value card is gone\n{agy}"
    );
}

fn menu_actions(app: &App) -> Vec<crate::tui::app::ActionMenuAction> {
    crate::tui::app::build_action_menu(app)
        .items
        .iter()
        .map(|item| item.action.clone())
        .collect()
}

/// Setup on a Codex row opens like a Claude account: a header block, then
/// action rows with the arrow, the focused hint, and a delete that arms first.
#[test]
fn setup_opens_a_codex_account_in_the_claude_layout() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();
    app.tab = Tab::Setup;
    app.open = OpenSelection::Account(RosterSlot::Codex(0));

    let closed = frame(&app);
    for piece in [
        "type",
        "codex",
        "plan",
        CODEX_PLAN,
        "re-login",
        "delete account",
    ] {
        assert!(closed.contains(piece), "setup lacks {piece:?}\n{closed}");
    }
    assert!(
        !closed.contains("❯ re-login"),
        "no row is focused yet\n{closed}"
    );

    press(&mut app, KeyCode::Enter);
    let open = frame(&app);
    scratch("setup-codex.txt", &open);
    assert!(open.contains("❯ re-login"), "{open}");
    assert!(
        open.contains("captures the login"),
        "the focused row explains itself\n{open}"
    );
    assert!(
        open.contains("↵ run"),
        "the footer names the row action\n{open}"
    );

    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let armed = frame(&app);
    assert!(armed.contains("press again to delete"), "{armed}");
    assert!(
        app.codex_rows
            .iter()
            .any(|r| r.name.as_str() == "codex-one"),
        "one press only arms"
    );
    press(&mut app, KeyCode::Up);
    assert!(!frame(&app).contains("press again"), "moving off disarms");

    press(&mut app, KeyCode::Esc);
    assert!(!frame(&app).contains("❯ re-login"), "esc leaves the rows");
}

/// Setup on a listed Grok login removes it from the overview on the second
/// press and keeps its monitor.
#[test]
fn setup_removes_a_grok_login_from_the_overview_on_the_second_press() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::provider_monitor::config::ensure_native_target(
        ProviderKind::Grok,
        Some("https://auth.x.ai::desk".into()),
    )
    .expect("list grok");
    let mut app = profile_app(&["keep"]);
    app.provider_reports = crate::provider_monitor::reports().expect("reports");
    let idx = app
        .provider_reports
        .iter()
        .position(|r| r.id == "grok")
        .expect("grok report");
    app.tab = Tab::Setup;
    app.open = OpenSelection::Account(RosterSlot::Native(idx));

    press(&mut app, KeyCode::Enter);
    let open = frame(&app);
    scratch("setup-grok.txt", &open);
    for piece in ["type", "grok", "❯ remove from overview"] {
        assert!(open.contains(piece), "setup lacks {piece:?}\n{open}");
    }
    assert!(
        !open.contains("re-login"),
        "grok signs in with its own tool\n{open}"
    );

    press(&mut app, KeyCode::Enter);
    assert!(frame(&app).contains("press again to remove"));
    press(&mut app, KeyCode::Enter);
    let saved = crate::provider_monitor::config::load().expect("providers.toml");
    let grok = saved
        .targets
        .iter()
        .find(|t| t.id == "grok")
        .expect("the monitor target stays");
    assert!(!grok.listed, "the second press unlists it");
    assert!(
        toast_bodies(&app)
            .iter()
            .any(|t| t == "removed 'grok' from the overview")
    );
}

/// Setup `a` on a Codex or Grok row offers what Overview offers for it, and
/// never the Claude preset actions a stale Claude cursor used to leak in.
#[test]
fn setup_actions_on_a_native_row_are_its_own() {
    use crate::tui::app::ActionMenuAction;
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = fixture();
    app.tab = Tab::Setup;
    // Step from `+ new` onto a native row, the path that left the Claude cursor
    // on the add row.
    app.open = OpenSelection::Add;
    app.profile_cursor = app.profile_count();
    app.open = OpenSelection::Account(RosterSlot::Native(0));
    let actions = menu_actions(&app);
    assert!(
        actions.contains(&ActionMenuAction::RefreshUsage),
        "{actions:?}"
    );
    assert!(
        actions.contains(&ActionMenuAction::DeleteAccount),
        "{actions:?}"
    );
    assert!(
        !actions.contains(&ActionMenuAction::ApplyPreset),
        "{actions:?}"
    );

    app.open = OpenSelection::Account(RosterSlot::Codex(0));
    let actions = menu_actions(&app);
    assert!(
        actions.contains(&ActionMenuAction::DeleteAccount),
        "{actions:?}"
    );
    assert!(
        !actions.contains(&ActionMenuAction::Duplicate),
        "{actions:?}"
    );

    app.tab = Tab::Usage;
    let actions = menu_actions(&app);
    assert!(
        actions.contains(&ActionMenuAction::RefreshUsage),
        "{actions:?}"
    );
    assert!(
        !actions.contains(&ActionMenuAction::DeleteAccount),
        "usage has no delete\n{actions:?}"
    );
}
