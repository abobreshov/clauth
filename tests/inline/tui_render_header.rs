use super::*;
use crate::profile::{AppConfig, AppState, Profile, ProfileName};
use crate::tui::app::{App, Tab};
use crate::usage::{UsageInfo, UsageWindow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::collections::BTreeMap;

fn oauth_profile(name: &str, five_hour_pct: f64) -> (Profile, Option<UsageInfo>) {
    (
        Profile {
            name: name.into(),
            base_url: None,
            api_key: None,
            auto_start: false,
            usage_probe: false,
            usage_probe_interval_ms: crate::profile::DEFAULT_USAGE_PROBE_INTERVAL_MS,
            env: BTreeMap::new(),
            models: Default::default(),
            fallback_threshold: None,
            weekly_threshold: None,
            last_resort: false,
            preferred: false,
            preferred_days: Vec::new(),
            rolling_token: false,
            max_auto_spend: None,
            check_weekly: true,
            check_scoped: true,
            check_credit_cap: true,
            bell_threshold: None,
            disabled: false,
            console: None,
            credentials: None,
            has_long_lived_sidecar: false,
            unreadable_credentials_store: None,
            setup_token_digest: None,
            provider: None,
        },
        Some(UsageInfo {
            five_hour: Some(UsageWindow {
                utilization: five_hour_pct,
                resets_at: None,
            }),
            ..UsageInfo::default()
        }),
    )
}

fn provider_profile(name: &str) -> (Profile, Option<UsageInfo>) {
    (
        Profile {
            name: name.into(),
            base_url: Some("https://api.example.com".to_string()),
            api_key: Some("key".to_string()),
            auto_start: false,
            usage_probe: false,
            usage_probe_interval_ms: crate::profile::DEFAULT_USAGE_PROBE_INTERVAL_MS,
            env: BTreeMap::new(),
            models: Default::default(),
            fallback_threshold: None,
            weekly_threshold: None,
            last_resort: false,
            preferred: false,
            preferred_days: Vec::new(),
            rolling_token: false,
            max_auto_spend: None,
            check_weekly: true,
            check_scoped: true,
            check_credit_cap: true,
            bell_threshold: None,
            disabled: false,
            console: None,
            credentials: None,
            has_long_lived_sidecar: false,
            unreadable_credentials_store: None,
            setup_token_digest: None,
            provider: None,
        },
        None,
    )
}

fn app_with(profiles: Vec<(Profile, Option<UsageInfo>)>, active: Option<&str>) -> App {
    let names: Vec<ProfileName> = profiles.iter().map(|(p, _)| p.name.clone()).collect();
    let config = AppConfig {
        state: AppState {
            active_profile: active.map(Into::into),
            profiles: names,
            ..AppState::default()
        },
        profiles: profiles.iter().map(|(p, _)| p.clone()).collect(),
    };
    let app = App::new(config);
    // Seed the OAuth store so the per-frame snapshot the header reads picks
    // each profile's reading up.
    let mut store = app.usage_store.lock().unwrap();
    for (p, usage) in &profiles {
        if let Some(info) = usage {
            store.insert(p.name.to_string(), info.clone());
        }
    }
    drop(store);
    app
}

/// Renders only the header block (sized by `header_height`).
fn render_header_rows(app: &App, width: u16) -> Vec<String> {
    let height = header_height(app);
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    let readings = app.readings_snapshot();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, app, &readings);
    })
    .unwrap();
    crate::testutil::buffer_rows(term.backend().buffer())
}

/// A header row's content past the logo column (0..10).
fn row_content(app: &App, width: u16, row: usize) -> String {
    render_header_rows(app, width)[row]
        .chars()
        .skip(10)
        .collect()
}

// ── `gauge_fit` — collapse ladder: bar before name ──────────────────────

#[test]
fn gauge_fit_shows_full_name_bar_pct_when_roomy() {
    let fit = gauge_fit(100, 8, Some(42.0));
    assert_eq!(
        fit,
        GaugeFit {
            name_w: 8,
            bar_cells: 10,
            visible: true,
        }
    );
}

#[test]
fn gauge_fit_shrinks_bar_first_before_name() {
    let fit = gauge_fit(23, 8, Some(42.0));
    assert_eq!(fit.name_w, 8, "name must stay full while bar still shrinks");
    assert_eq!(fit.bar_cells, 7, "bar must shrink first");
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_bar_entirely_before_touching_name() {
    let fit = gauge_fit(18, 8, Some(42.0));
    assert_eq!(fit.name_w, 8, "name must not shrink after bar drops");
    assert_eq!(fit.bar_cells, 0, "bar must drop before name trims");
    assert!(fit.visible);
}

#[test]
fn gauge_fit_truncates_name_only_after_bar_is_already_gone() {
    let fit = gauge_fit(12, 8, Some(42.0));
    assert_eq!(fit.bar_cells, 0);
    assert_eq!(fit.name_w, 7);
    assert!(fit.visible);
}

#[test]
fn gauge_fit_drops_name_only_after_bar_is_already_gone() {
    let fit = gauge_fit(7, 8, Some(42.0));
    assert_eq!(fit.bar_cells, 0);
    assert_eq!(fit.name_w, 0);
    assert!(fit.visible, "the percent figure alone should still render");
}

#[test]
fn gauge_fit_hides_entirely_below_the_percent_width() {
    let fit = gauge_fit(3, 8, Some(42.0));
    assert_eq!(fit, GaugeFit::HIDDEN);
}

#[test]
fn gauge_fit_provider_profile_never_shows_a_bar() {
    let roomy = gauge_fit(100, 10, None);
    assert_eq!(
        roomy,
        GaugeFit {
            name_w: 10,
            bar_cells: 0,
            visible: true
        }
    );

    let tight = gauge_fit(9, 10, None);
    assert_eq!(tight.bar_cells, 0);
    assert_eq!(tight.name_w, 8);

    let dash_only = gauge_fit(1, 10, None);
    assert_eq!(dash_only.name_w, 0);
    assert!(
        !dash_only.visible,
        "no bar and no tail means nothing to render"
    );
}

// ── `header_height` ─────────────────────────────────────────────────────

#[test]
fn header_height_is_always_three() {
    let _home = crate::testutil::HomeSandbox::new();
    let with_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    assert_eq!(header_height(&with_active), 3);

    let no_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    assert_eq!(header_height(&no_active), 3);

    let mut compact = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    compact.compact = true;
    assert_eq!(header_height(&compact), 3);
}

// ── Row 1: the live count, the gauge and the status indicator ────────────
//
// Row 1's text column starts after the 10-cell glyph column, so a terminal `W`
// columns wide offers it `W - 10`. The indicator `● status.claude.ai` is 18
// cells (dot, space, 16-char feed) plus a 3-cell reserve, so the gauge is
// fitted to `W - 31` and the indicator is gated on the gauge as rendered. No
// account count and no harness filter name takes part: both live on the
// accounts panel's title row. These fixtures run no session, so the live
// count reads `[ 0 live ]` wherever the width leaves it room.

#[test]
fn row1_is_the_live_count_the_gauge_and_the_status_indicator_when_wide() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    // At 120 the text column is 110 cells: `[ 0 live ]  [ — tok/s ] · ` is 26,
    // the gauge's widest rung renders 25 (8 name + ` ` + `42%` + ` ` + 12
    // bracketed bar), the indicator 18, and the 41 cells between them are the
    // elastic gap. The fit budgets the pct at 4 (`100%`), so at a two-digit
    // figure one spare cell falls to the gap.
    let chars: Vec<char> = row_content(&app, 120, 1).chars().collect();
    let left: String = chars[..51].iter().collect();
    let gap: String = chars[51..92].iter().collect();
    let dot: String = chars[92..].iter().collect();
    assert_eq!(
        left, "[ 0 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]",
        "the live count, the tok/s chip, then the gauge, lead row 1"
    );
    assert!(
        gap.chars().all(|c| c == ' '),
        "the elastic gap carries whitespace alone: {gap:?}"
    );
    assert_eq!(dot, "● status.claude.ai", "the indicator closes the row");
}

/// The ladder's first rung on the buffer: the bar gives a cell before the name
/// is touched. The bar at its widest (10 cells) puts the gauge at 25 and needs
/// a `W - 31 >= 26` budget, so 57 is the first width that holds it.
#[test]
fn row1_gauge_shrinks_its_bar_before_it_touches_the_name() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    assert_eq!(
        row_content(&app, 57, 1).trim_end(),
        "uwuclxdy 42% [████░░░░░░]    ● status.claude.ai",
        "at its own fit width the bar is full"
    );
    assert_eq!(
        row_content(&app, 56, 1).trim_end(),
        "uwuclxdy 42% [████░░░░░]    ● status.claude.ai",
        "one column narrower the bar gives a cell and the name stays whole"
    );
}

/// A provider profile has no usage window, so its gauge is the name and its
/// 1-cell trailing gap alone: no bar, no percent, no dash standing in for
/// either. The trailing gap merges into the row's elastic gap, so no string
/// pin can discriminate its width — the name text is the observable pin.
#[test]
fn row1_gauge_for_a_provider_profile_carries_no_bar() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![provider_profile("z.ai")], Some("z.ai"));
    app.tab = Tab::Tokens;

    // 90 - 10 = 80 text cells; the count is 10 of them, the tok/s chip 13 with
    // its gap, the gauge 5 (`z.ai` plus its 1-cell trailing gap), the
    // indicator 18.
    let row = row_content(&app, 90, 1);
    let left: String = row.chars().take(30).collect();
    assert_eq!(
        left, "[ 0 live ]  [ — tok/s ] · z.ai",
        "the live count and the tok/s chip lead, then the gauge: the name alone"
    );
    let rest: String = row.chars().skip(30).collect();
    let (gap, dot) = rest.split_at(80 - 30 - 18);
    assert!(
        gap.chars().all(|c| c == ' '),
        "the elastic gap carries whitespace alone: {gap:?}"
    );
    assert_eq!(dot, "● status.claude.ai", "the indicator closes the row");
    assert!(!row.contains('█'), "provider must not render a bar");
    assert!(!row.contains('%'), "provider must not render a percent");
}

/// Two ways the active profile cannot be shown — compact mode, and a config
/// with no active slot — leave row 1 to the live count and the indicator.
/// Both are pinned on the tab where the gauge otherwise renders, so neither
/// passes by the Overview tab's own gauge-off rule.
#[test]
fn row1_carries_no_gauge_in_compact_mode_or_without_an_active_profile() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut compact = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    compact.compact = true;
    compact.tab = Tab::Tokens;
    let mut no_active = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);
    no_active.tab = Tab::Tokens;

    // (90 - 10) - 23 - 18 = 39: the count and the tok/s chip on the left, the
    // indicator right-aligned.
    let expected = format!(
        "[ 0 live ]  [ — tok/s ]{}● status.claude.ai",
        " ".repeat(39)
    );
    for (case, app) in [("compact", &compact), ("no active profile", &no_active)] {
        let rows = render_header_rows(app, 90);
        assert_eq!(rows.len(), 3);
        assert!(
            !rows.iter().any(|r| r.contains("uwuclxdy")),
            "{case}: the gauge is on no header row"
        );
        assert_eq!(
            row_content(app, 90, 1),
            expected,
            "{case}: row 1 is the count and the indicator alone"
        );
    }
}

/// The ladder's tail on the buffer: the bar is already gone, the name holds
/// while a `W - 31 >= 13` budget remains (13 = 8 name + ` ` + the percent's
/// 4 budgeted cells), and one column under it the name clips rather than the
/// percent.
#[test]
fn row1_gauge_falls_to_the_name_and_percent_before_the_name_clips() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    assert_eq!(
        row_content(&app, 44, 1).trim_end(),
        "uwuclxdy 42%    ● status.claude.ai",
        "44 is the first width whose budget holds the whole name"
    );
    assert_eq!(
        row_content(&app, 43, 1).trim_end(),
        "uwuclx… 42%    ● status.claude.ai",
        "one column narrower the name carries its truncation ellipsis"
    );
}

/// The gauge's last rung is the percent alone, and the indicator is charged
/// the gauge as rendered: it holds the row at that rung and one column under
/// it, when the gauge has gone.
#[test]
fn row1_gauge_falls_to_the_percent_alone_and_then_away() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    assert_eq!(
        row_content(&app, 35, 1).trim_end(),
        "42%    ● status.claude.ai",
        "the percent holds a `W - 31 >= 4` budget down to 35"
    );
    assert_eq!(
        row_content(&app, 34, 1).trim_end(),
        "      ● status.claude.ai",
        "one column narrower the gauge is gone, the indicator staying"
    );
}

// ── Row 1: the fleet's live-session count ahead of the gauge ────────────────
//
// `[ 3 live ] · ` leads row 1: a counter chip holding the count of every live
// session across the fleet, not the active account's. It takes only the width
// the gauge and the indicator leave, so it is the first thing row 1 sheds: it
// renders only while gauge + chip + indicator + the 3-cell reserve all fit.

/// Three sessions, two on the active account and one on `kerry`, an account
/// no longer in the config: the prefix counts all three, so a count taken over
/// the configured accounts alone would read 2.
fn app_with_three_live(tab: Tab) -> App {
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = tab;
    app.live_sessions = crate::live_sessions::LiveTally::of([
        crate::testutil::live_row("4242-0", "uwuclxdy"),
        crate::live_sessions::LiveSession {
            follows_chain: false,
            ..crate::testutil::live_row("4242-1", "uwuclxdy")
        },
        crate::testutil::live_row("4343-0", "kerry"),
    ]);
    app
}

/// Compact mode hides the gauge, not the fleet's count: the count stands alone
/// the way it does on the Overview.
#[test]
fn row1_keeps_the_live_count_in_compact_mode() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with_three_live(Tab::Tokens);
    app.compact = true;

    assert_eq!(
        row_content(&app, 90, 1),
        format!(
            "[ 3 live ]  [ — tok/s ]{}● status.claude.ai",
            " ".repeat(39)
        ),
    );
}

#[test]
fn row1_leads_with_the_fleet_live_count_before_the_gauge() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_three_live(Tab::Tokens);

    // 120 - 10 = 110 text cells: `[ 3 live ]  [ — tok/s ] · ` is 26, the gauge
    // 25, the indicator 18, and the 41 cells between them are the elastic gap.
    assert_eq!(
        row_content(&app, 120, 1),
        format!(
            "[ 3 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]{}● status.claude.ai",
            " ".repeat(41)
        ),
    );
}

#[test]
fn row1_carries_the_live_count_alone_where_no_gauge_renders() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_three_live(Tab::Overview);

    // No gauge on the Overview: the count and the tok/s chip stand alone.
    // (90 - 10) - 23 - 18 = 39 cells of gap.
    assert_eq!(
        row_content(&app, 90, 1),
        format!(
            "[ 3 live ]  [ — tok/s ]{}● status.claude.ai",
            " ".repeat(39)
        ),
    );
}

/// Zero shows (cloudy, 2026-09-29): a count that vanished at zero would move
/// the gauge 13 cells each time the first session starts or the last one ends.
/// Swept over every width and three row shapes (gauge with a bar, a provider's
/// name-only gauge, no gauge), so a count-conditional term anywhere in the
/// shed gate reds wherever it bites inside 24..=140 columns.
#[test]
fn row1_shows_zero_live_so_the_gauge_holds_its_place() {
    let _home = crate::testutil::HomeSandbox::new();
    type Build = fn() -> App;
    let shapes: [(&str, Build); 3] = [
        ("oauth", || {
            let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
            app.tab = Tab::Tokens;
            app
        }),
        ("provider", || {
            let mut app = app_with(vec![provider_profile("z.ai")], Some("z.ai"));
            app.tab = Tab::Tokens;
            app
        }),
        ("no gauge", || {
            let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
            app.tab = Tab::Overview;
            app
        }),
    ];
    for (shape, build) in shapes {
        let idle = build();
        let mut busy = build();
        busy.live_sessions =
            crate::live_sessions::LiveTally::of([crate::testutil::live_row("4242-0", "uwuclxdy")]);
        for width in 24..=140u16 {
            assert_eq!(
                row_content(&idle, width, 1).replacen("[ 0 live ]", "[ 1 live ]", 1),
                row_content(&busy, width, 1),
                "{shape} at {width}: the first session changes the digit alone"
            );
        }
    }
}

/// The shed seam: the max gauge + suffix charge (39) + the live chip's split
/// ceiling 16 + separator 3 + indicator 18 + reserve 3 = 79 text cells, so 89
/// is the narrowest width holding the live chip, and at 88 the chip drops whole
/// with its ` · ` while the gauge keeps its full bar.
#[test]
fn row1_sheds_the_live_count_before_the_gauge_or_the_indicator() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with_three_live(Tab::Tokens);

    assert_eq!(
        row_content(&app, 89, 1),
        format!(
            "[ 3 live ] · uwuclxdy 42% [████░░░░░░]{}● status.claude.ai",
            " ".repeat(23)
        ),
        "at its own fit width the chip renders"
    );
    assert_eq!(
        row_content(&app, 88, 1),
        format!(
            "uwuclxdy 42% [████░░░░░░]{}● status.claude.ai",
            " ".repeat(35)
        ),
        "one column narrower the count drops whole, the gauge untouched"
    );
}

fn tally_of(n: usize) -> crate::live_sessions::LiveTally {
    crate::live_sessions::LiveTally::of(
        (0..n).map(|i| crate::testutil::live_row(&format!("4242-{i}"), "uwuclxdy")),
    )
}

fn split_tally(own: usize, delegates: usize) -> crate::live_sessions::LiveTally {
    crate::live_sessions::LiveTally::of(
        (0..own)
            .map(|i| crate::testutil::live_row(&format!("4242-{i}"), "uwuclxdy"))
            .chain((0..delegates).map(|i| crate::live_sessions::LiveSession {
                is_delegate: true,
                ..crate::testutil::live_row(&format!("91{i}-0"), "uwuclxdy")
            })),
    )
}

/// The chip's cells by role: brackets and unit `TEXT_DIM` without bold, the
/// number `TEXT` + bold, the separator to the gauge `TEXT_DIM`; at zero the
/// number drops to `TEXT_DIM` without bold. Pinned per cell off the theme's
/// own colors.
#[test]
fn the_live_chip_cells_carry_the_counter_chip_grammar() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dim = super::theme::text_dim_color();
    let text = super::theme::text_color();
    for (n, number_fg, number_bold) in [(0, dim, false), (3, text, true)] {
        let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
        app.tab = Tab::Tokens;
        app.set_live_sessions(tally_of(n));
        let height = header_height(&app);
        let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, &app, &app.readings_snapshot());
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let cell = |col: usize| &buf.content[120 + 10 + col];
        let text_of: String = (0..13).map(|c| cell(c).symbol().to_string()).collect();
        assert_eq!(text_of, format!("[ {n} live ]  ["), "{n}: the chip's text");
        for (col, role) in [
            (0, "open bracket"),
            (4, "unit"),
            (7, "unit"),
            (9, "close bracket"),
            // The ` · ` before the gauge moved past the tok/s chip.
            (24, "separator"),
        ] {
            assert_eq!(cell(col).fg, dim, "{n}: the {role} renders TEXT_DIM");
            assert!(
                !cell(col).modifier.contains(Modifier::BOLD),
                "{n}: the {role} carries no bold"
            );
        }
        assert_eq!(cell(2).fg, number_fg, "{n}: the number's color");
        assert_eq!(
            cell(2).modifier.contains(Modifier::BOLD),
            number_bold,
            "{n}: the number's weight"
        );
    }
}

/// The chips sit exactly two cells apart at every count: once the chip has
/// read `10`, a drop to `9` renders the one-digit chip tight, no blank cell
/// held after its closing bracket, and the gauge moves left one cell.
#[test]
fn the_live_chip_renders_tight_after_a_wider_count() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;

    app.set_live_sessions(tally_of(10));
    let at_ten = row_content(&app, 120, 1);
    assert!(
        at_ten.starts_with("[ 10 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "ten: {at_ten:?}"
    );

    app.set_live_sessions(tally_of(9));
    let at_nine = row_content(&app, 120, 1);
    assert!(
        at_nine.starts_with("[ 9 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "nine after ten renders tight: {at_nine:?}"
    );
    assert_eq!(
        at_ten.chars().position(|c| c == 'u'),
        Some(27),
        "the gauge's first cell sits after the two-digit chip and its separator"
    );
    assert_eq!(
        at_nine.chars().position(|c| c == 'u'),
        Some(26),
        "the gauge follows the one-digit chip across 10 → 9"
    );
}

/// A TUI opening onto ten live sessions and then reading nine renders the
/// one-digit chip tight: the construction tally holds no width either.
#[test]
fn the_live_chip_after_a_ten_count_construction_renders_tight() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut markers = Vec::new();
    for i in 0..10 {
        let id = format!("4242-{i}");
        crate::live_sessions::register(&crate::testutil::live_row(&id, "uwuclxdy"))
            .expect("register a live row");
        markers.push(
            crate::runtime::hold_session_row_marker(&"uwuclxdy".into(), false, &id)
                .expect("hold the row's marker"),
        );
    }
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    assert!(
        row_content(&app, 120, 1).starts_with("[ 10 live ]  [ — tok/s ] · uwuclxdy"),
        "the construction tally reads all ten"
    );
    app.set_live_sessions(tally_of(9));
    assert!(
        row_content(&app, 120, 1).starts_with("[ 9 live ]  [ — tok/s ] · uwuclxdy"),
        "nine after a ten-count construction renders tight"
    );
}

/// A three-digit count renders whole, two cells before the tok/s chip.
#[test]
fn a_count_wider_than_the_hold_renders_whole() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.live_sessions = tally_of(100);
    assert!(
        row_content(&app, 120, 1).starts_with("[ 100 live ]  [ — tok/s ] · uwuclxdy"),
        "a three-digit count renders whole, two cells before the tok/s chip"
    );
}

/// A tally that could not read renders `—` where the number sits, dim and
/// unbolded, never a `0` the app did not read; the chip holds its place.
#[test]
fn an_unread_tally_renders_a_dash_not_a_zero() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(crate::live_sessions::LiveTally::unread());
    let height = header_height(&app);
    let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, &app, &app.readings_snapshot());
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let row1: String = crate::testutil::buffer_rows(&buf)[1]
        .chars()
        .skip(10)
        .collect();
    assert!(
        row1.starts_with("[ — live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "{row1:?}"
    );
    let dash = &buf.content[120 + 10 + 2];
    assert_eq!(
        dash.fg,
        super::theme::text_dim_color(),
        "the dash renders TEXT_DIM"
    );
    assert!(
        !dash.modifier.contains(Modifier::BOLD),
        "the dash carries no bold"
    );
}

/// The gauge rung's cells by role: the percent leads the bar, both in the util
/// color (80% → DANGER), the brackets hug the bar as TEXT_DIM chrome,
/// unbolded; every gap inside the rung is one cell. Pinned per cell off the
/// theme's own colors.
#[test]
fn the_gauge_rung_cells_carry_the_chrome_and_util_roles() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 80.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let height = header_height(&app);
    let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, &app, &app.readings_snapshot());
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let cell = |col: usize| &buf.content[120 + 10 + 26 + col];
    let text_of: String = (0..25).map(|c| cell(c).symbol().to_string()).collect();
    assert_eq!(text_of, "uwuclxdy 80% [████████░░]", "{text_of:?}");
    let dim = super::theme::text_dim_color();
    let danger = super::theme::danger_color();
    for (col, role) in [(13, "open bracket"), (24, "close bracket")] {
        assert_eq!(cell(col).fg, dim, "{role} renders TEXT_DIM");
        assert!(
            !cell(col).modifier.contains(Modifier::BOLD),
            "{role} carries no bold"
        );
    }
    for col in 9..=11 {
        assert_eq!(
            cell(col).fg,
            danger,
            "the percent renders the util color at {col}"
        );
    }
    for col in 14..=23 {
        assert_eq!(
            cell(col).fg,
            danger,
            "the bar renders the util color at {col}"
        );
    }
}

/// The split form's cells by role: the own number `TEXT` + bold, the `+` dim,
/// the delegates number `ACCENT` + bold, brackets and unit dim without bold.
/// Pinned per cell off the theme's own colors, and the whole row by equality.
#[test]
fn the_split_live_chip_renders_own_then_delegates_with_their_styles() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dim = super::theme::text_dim_color();
    let text = super::theme::text_color();
    let accent = super::theme::accent_color();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(split_tally(6, 7));
    let height = header_height(&app);
    let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, &app, &app.readings_snapshot());
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let cell = |col: usize| &buf.content[120 + 10 + col];
    let text_of: String = (0..15).map(|c| cell(c).symbol().to_string()).collect();
    assert_eq!(text_of, "[ 6+7 live ]  [", "the split chip's text");
    let row1: String = crate::testutil::buffer_rows(&buf)[1]
        .chars()
        .skip(10)
        .collect();
    assert_eq!(
        row1,
        format!(
            "[ 6+7 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]{}● status.claude.ai",
            " ".repeat(39)
        ),
        "the split chip leads row 1"
    );
    assert_eq!(cell(2).fg, text, "the own number renders TEXT");
    assert!(
        cell(2).modifier.contains(Modifier::BOLD),
        "the own number is bold"
    );
    assert_eq!(cell(3).fg, dim, "the + renders TEXT_DIM");
    assert!(
        !cell(3).modifier.contains(Modifier::BOLD),
        "the + is unbolded"
    );
    assert_eq!(cell(4).fg, accent, "the delegates number renders ACCENT");
    assert!(
        cell(4).modifier.contains(Modifier::BOLD),
        "the delegates number is bold"
    );
    for col in [0, 1, 5, 6, 7, 8, 9, 10, 11] {
        assert_eq!(cell(col).fg, dim, "chrome renders TEXT_DIM at {col}");
        assert!(
            !cell(col).modifier.contains(Modifier::BOLD),
            "chrome carries no bold at {col}"
        );
    }
}

/// A lone delegate part renders `[ 7 live ]` with the 7 in `ACCENT` + bold:
/// the colour alone marks which part it is, no `0+` prefix. The whole row is
/// pinned by equality — the chip is 2 cells narrower than the split form, so
/// the gauge gap grows by 2 — and every chrome cell reads dim without bold.
#[test]
fn a_lone_delegate_part_renders_in_accent_alone() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dim = super::theme::text_dim_color();
    let accent = super::theme::accent_color();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(split_tally(0, 7));
    let height = header_height(&app);
    let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, &app, &app.readings_snapshot());
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let cell = |col: usize| &buf.content[120 + 10 + col];
    let row1: String = crate::testutil::buffer_rows(&buf)[1]
        .chars()
        .skip(10)
        .collect();
    assert_eq!(
        row1,
        format!(
            "[ 7 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]{}● status.claude.ai",
            " ".repeat(41)
        ),
        "the lone delegate part leads row 1 whole"
    );
    assert_eq!(cell(2).fg, accent, "the lone delegate number is ACCENT");
    assert!(
        cell(2).modifier.contains(Modifier::BOLD),
        "the lone delegate number is bold"
    );
    for col in [0, 1, 3, 4, 5, 6, 7, 8, 9] {
        assert_eq!(cell(col).fg, dim, "chrome renders TEXT_DIM at {col}");
        assert!(
            !cell(col).modifier.contains(Modifier::BOLD),
            "chrome carries no bold at {col}"
        );
    }
}

/// The split form renders tight too: after `6+7`, a drop to `6` leaves no
/// blank cell after the closing bracket, and the gauge moves left two cells.
#[test]
fn the_split_chip_renders_tight_after_the_split() {
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(split_tally(6, 7));
    let at_split = row_content(&app, 120, 1);
    assert!(
        at_split.starts_with("[ 6+7 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "{at_split:?}"
    );
    app.set_live_sessions(split_tally(6, 0));
    let at_six = row_content(&app, 120, 1);
    assert!(
        at_six.starts_with("[ 6 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "six after the split renders tight: {at_six:?}"
    );
    assert_eq!(
        at_split.chars().position(|c| c == 'u'),
        Some(28),
        "the gauge's first cell sits after the split chip and its separator"
    );
    assert_eq!(
        at_six.chars().position(|c| c == 'u'),
        Some(26),
        "the gauge follows the one-digit chip across 6+7 -> 6"
    );
}

#[test]
fn row2_is_tabs_only() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    let row2 = row_content(&app, 90, 2);
    assert!(row2.contains("overview"), "tabs on row 2");
    assert!(
        !row2.contains("uwuclxdy"),
        "gauge not on row 2 (it's on row 1)"
    );
}

// ── `[ daemon ]` header chip (always present; health → color) ────────────────

#[test]
fn the_daemon_chip_is_always_present_and_maps_health_to_color() {
    // `HomeSandbox` outermost per the ranked-lock rule: `HOME_TEST_LOCK` before
    // any ranked guard.
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use crate::daemon::DaemonHealth;
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));

    // Absent → the chip still renders, dim, on the right edge.
    app.daemon_health = DaemonHealth::Absent;
    assert_eq!(
        super::daemon_chip_color(&app),
        super::theme::text_dim_color(),
        "absent → dim"
    );
    let row0 = row_content(&app, 100, 0);
    assert!(
        row0.trim_end().ends_with("[ daemon ]"),
        "absent → the chip is still on the row: {row0:?}"
    );

    // Fresh → green.
    app.daemon_health = DaemonHealth::Fresh;
    assert_eq!(
        super::daemon_chip_color(&app),
        super::theme::success_color(),
        "fresh → green"
    );

    // Stale → amber.
    app.daemon_health = DaemonHealth::Stale;
    assert_eq!(
        super::daemon_chip_color(&app),
        super::theme::warning_color(),
        "stale → amber"
    );
}

/// The chip's pill grammar on the buffer: `[ ` and ` ]` are TEXT_DIM chrome,
/// the word between them bold in the health color (dim when absent) — pinned on
/// cells, so a chip painted in one flat style reds.
#[test]
fn the_daemon_chip_cells_carry_the_pill_grammar() {
    use crate::daemon::DaemonHealth;
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    let word_w = "daemon".chars().count();

    for (health, expected) in [
        (DaemonHealth::Absent, super::theme::text_dim_color()),
        (DaemonHealth::Fresh, super::theme::success_color()),
        (DaemonHealth::Stale, super::theme::warning_color()),
    ] {
        app.daemon_health = health;
        let width = 100;
        let height = header_height(&app);
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, &app, &app.readings_snapshot());
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row = crate::testutil::buffer_rows(buf)[0]
            .chars()
            .skip(10)
            .collect::<String>();
        let chip = 10 + row.find("[ daemon ]").expect("chip renders");

        // Cells 0-1 are `[ `, the last two ` ]`: chrome either side.
        for cell in [chip, chip + 1, chip + word_w + 2, chip + word_w + 3] {
            assert_eq!(
                buf.content[cell].fg,
                super::theme::text_dim_color(),
                "{health:?}: the bracket at cell {cell} is TEXT_DIM chrome"
            );
            assert!(
                !buf.content[cell].modifier.contains(Modifier::BOLD),
                "{health:?}: the bracket at cell {cell} is not bold"
            );
        }
        // Cells 2..word_w+2 are the word: bold, in the health color.
        for i in 0..word_w {
            let cell = chip + 2 + i;
            assert_eq!(
                buf.content[cell].fg, expected,
                "{health:?}: the word carries the health color at cell {cell}"
            );
            assert!(
                buf.content[cell].modifier.contains(Modifier::BOLD),
                "{health:?}: the word is bold at cell {cell}"
            );
        }
    }
}

/// The status indicator is right-aligned across an elastic gap, and it DROPS
/// whole when the row cannot hold it: a dot left to render would clip the feed
/// mid-word. With no gauge on the row, the indicator's own fit (`col >= 18 + 3`)
/// never passes inside the sub-30 band — there the logo column yields its
/// cells to the `Min(20)` text column, capping it at 20 — so the gate decides
/// at the band's edge: at 31 the text column is 21 and the indicator renders
/// with its 3-cell gap; at 30 it drops whole.
#[test]
fn the_status_indicator_drops_rather_than_clipping_when_the_row_runs_short() {
    let _home = crate::testutil::HomeSandbox::new();
    let app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], None);

    assert_eq!(
        row_content(&app, 31, 1),
        "   ● status.claude.ai",
        "exact fit: the indicator renders with its 3-cell gap"
    );
    assert_eq!(
        row_content(&app, 30, 1).trim_end(),
        "",
        "one column narrower it drops whole rather than clipping the feed"
    );
}

// ── The counts live on the accounts panel, never in the header ───────────
//
// The by-harness counts moved onto the accounts panel's title, which carries
// them as its title-right meta slot (`tui_render_overview.rs` pins that slot's
// own shed). These two pins are the guard against a count, an `ACCOUNTS`
// label or a harness filter name coming back to a header row.

/// No header row names the accounts at any width, tab or harness filter: the
/// sweep walks every seam the old count line had — the label, the count, each
/// gauge rung, the indicator — on both tabs the gauge differs between.
#[test]
fn no_header_row_counts_accounts_at_any_width_tab_or_filter() {
    use crate::tui::app::HarnessFilter;
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1", "cx2", "cx3"]);
    let mut app = app_with(
        vec![oauth_profile("uwuclxdy", 42.0), provider_profile("z.ai")],
        Some("uwuclxdy"),
    );
    // A live fleet puts the one sanctioned middot on row 1 (`[ 2 live ]  [ — tok/s ] · `,
    // or `[ 2 live ] · ` where the tok/s chip has shed), so the guard below runs
    // with it present and strips exactly that one.
    app.live_sessions = crate::live_sessions::LiveTally::of([
        crate::testutil::live_row("4242-0", "uwuclxdy"),
        crate::testutil::live_row("4242-1", "z.ai"),
    ]);

    for filter in [
        HarnessFilter::All,
        HarnessFilter::Claude,
        HarnessFilter::Codex,
    ] {
        app.harness_filter = filter;
        for tab in [Tab::Overview, Tab::Tokens] {
            app.tab = tab;
            for width in 24..=140u16 {
                for (row, content) in render_header_rows(&app, width).iter().enumerate() {
                    let lower = content.to_lowercase();
                    assert!(
                        !lower.contains("account"),
                        "{filter:?} on {tab:?} at {width}: header row {row} names the \
                         accounts: {content:?}"
                    );
                    // The row-18 wording (`52 claude · 1 codex`) and the chip
                    // form (`[ 52 · 1 ]`) carry no "account" substring: the
                    // harness words (space-led, so the feed's own
                    // `status.claude.ai` never trips them) and the middot each
                    // red them on their own, whatever the wording a count
                    // comes back in.
                    assert!(
                        !lower.contains(" claude") && !lower.contains("codex"),
                        "{filter:?} on {tab:?} at {width}: header row {row} names a \
                         harness: {content:?}"
                    );
                    assert!(
                        !content
                            .replacen("[ 2 live ]  [ — tok/s ] · ", "", 1)
                            .replacen("[ 2 live ] · ", "", 1)
                            .contains('·'),
                        "{filter:?} on {tab:?} at {width}: header row {row} carries a \
                         count middot: {content:?}"
                    );
                }
            }
        }
    }
}

/// The word `ACCOUNTS` belongs to the accounts panel's title alone: a full
/// Overview frame carries it once, on the panel's top border. A second
/// `ACCOUNTS` label one row above that border, where the header used to carry
/// the account count, reds this.
#[test]
fn the_word_accounts_renders_on_the_panel_title_alone() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::write_codex_roster(&["cx1"]);
    let mut app = app_with(
        vec![oauth_profile("uwuclxdy", 42.0), provider_profile("z.ai")],
        None,
    );
    app.tab = Tab::Overview;

    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| crate::tui::render::draw(f, &app)).unwrap();
    let rows = crate::testutil::buffer_rows(term.backend().buffer());

    for (y, row) in rows.iter().take(header_height(&app) as usize).enumerate() {
        assert!(
            !row.contains("ACCOUNTS"),
            "header row {y} carries no ACCOUNTS: {row:?}"
        );
    }

    let carrying: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("ACCOUNTS"))
        .map(|(y, _)| y)
        .collect();
    assert_eq!(
        carrying.len(),
        1,
        "the word renders exactly once on the whole screen: {carrying:?}"
    );
    assert!(
        rows[carrying[0]].starts_with("╭─ ACCOUNTS "),
        "and it is the accounts panel's title: {:?}",
        rows[carrying[0]]
    );
}

#[test]
fn fractional_header_percentages_round_to_two_decimals_without_clipping_the_feed() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("fraction", 42.127)], Some("fraction"));
    app.tab = Tab::Tokens;
    assert!(row_content(&app, 120, 1).contains("42.13%"));
    for width in 24..=140 {
        let row = row_content(&app, width, 1);
        assert!(!row.contains("42.127"));
        if row.contains("status") {
            assert!(row.ends_with("status.claude.ai"), "{width}: {row}");
        }
    }
}

// ── `TollgateLogo` pixel-art widget tests ─────────────────────────────────

/// The wired mark first, then the alternatives `header.rs` keeps commented.
const LOGO_CANDIDATES: [(&str, [&str; 3]); 3] = [
    ("booth", LOGO),
    ("gantry", ["▗▄▄▄▄▄▄▄▖", "▐▌▄ ▄ ▄▐▌", "▐▌     ▐▌"]),
    ("gauge", [" ▗▄▀▀▀▄▖ ", "▗▘   ▞ ▝▖", "▐▄▄▄▟▄▄▄▌"]),
];

/// The upstream clauth mascot this mark replaced.
const UPSTREAM_MASCOT: [&str; 4] = [" ▐▛███▜▌ ", " ▐█████▌ ", "▝▜█████▛▘", "  ▘▘ ▝▝  "];

/// The header drawn with the animation clock pinned at `anim_ms`.
fn render_header_at(anim_ms: u64, width: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
    let mut app = app_with(Vec::new(), None);
    app.anim_phase_ms = Some(anim_ms);
    let mut term = Terminal::new(TestBackend::new(width, header_height(&app))).unwrap();
    term.draw(|f| {
        let area = f.area();
        super::draw(f, area, &app, &app.readings_snapshot());
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    (crate::testutil::buffer_rows(&buf), buf)
}

fn is_block_glyph(c: char) -> bool {
    c == ' ' || ('\u{2580}'..='\u{259f}').contains(&c)
}

#[test]
fn logo_bitmap_dimensions_and_cell_colour_constraints_are_valid() {
    assert_eq!(LOGO_PIXEL_WIDTH, 24);
    assert_eq!(LOGO_PIXEL_HEIGHT, 10);
    assert_eq!(LOGO_CELL_WIDTH, 12);
    assert_eq!(LOGO_CELL_HEIGHT, 5);
    assert_eq!(LOGO_BITMAP.len(), LOGO_PIXEL_HEIGHT);
    for (y, row) in LOGO_BITMAP.iter().enumerate() {
        assert_eq!(
            row.len(),
            LOGO_PIXEL_WIDTH,
            "row {y} must be 24 pixels wide"
        );
    }

    // Every 2×2 block must contain at most 2 distinct colours (and at most 1
    // non-background colour) so terminal cell rendering is completely lossless.
    for cy in 0..LOGO_CELL_HEIGHT {
        for cx in 0..LOGO_CELL_WIDTH {
            let py = cy * 2;
            let px = cx * 2;
            let sub = [
                LOGO_BITMAP[py][px],
                LOGO_BITMAP[py][px + 1],
                LOGO_BITMAP[py + 1][px],
                LOGO_BITMAP[py + 1][px + 1],
            ];
            let mut non_bg: Vec<Px> = sub.into_iter().filter(|&p| p != Px::Bg).collect();
            non_bg.dedup();
            for i in 0..non_bg.len() {
                for j in (i + 1)..non_bg.len() {
                    assert_eq!(
                        non_bg[i], non_bg[j],
                        "cell ({cx}, {cy}) mixes incompatible foreground colours: {sub:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn quadrant_character_mapping_follows_bitmask_convention() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let expected_lut: [char; 16] = [
        ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
    ];
    assert_eq!(QUADRANT_GLYPHS, expected_lut);

    for (mask, &expected_char) in expected_lut.iter().enumerate() {
        let ul = if (mask & 1) != 0 { Px::Booth } else { Px::Bg };
        let ur = if (mask & 2) != 0 { Px::Booth } else { Px::Bg };
        let ll = if (mask & 4) != 0 { Px::Booth } else { Px::Bg };
        let lr = if (mask & 8) != 0 { Px::Booth } else { Px::Bg };
        let (ch, fg, bg) = quadrant_cell(ul, ur, ll, lr);
        assert_eq!(
            ch, expected_char,
            "bitmask {mask:#06b} must map to {expected_char:?}"
        );
        assert_eq!(bg, theme::bg());
        if mask != 0 {
            assert_eq!(fg, Px::Booth.color());
        }
    }
}

#[test]
fn barrier_occupies_constant_logical_y_coordinates_and_identical_segment_thickness() {
    let is_barrier = |p: Px| matches!(p, Px::BarrierRed | Px::BarrierWhite);

    let mut barrier_cols: Vec<(usize, Vec<usize>, Px)> = Vec::new();
    for x in 0..LOGO_PIXEL_WIDTH {
        let ys: Vec<(usize, Px)> = LOGO_BITMAP
            .iter()
            .enumerate()
            .filter_map(|(y, row)| is_barrier(row[x]).then_some((y, row[x])))
            .collect();
        if let Some(&(_, color)) = ys.first() {
            for &(y, px) in &ys {
                assert_eq!(
                    px, color,
                    "barrier column ({x}, {y}) must have uniform segment colour"
                );
            }
            let y_coords: Vec<usize> = ys.into_iter().map(|(y, _)| y).collect();
            barrier_cols.push((x, y_coords, color));
        }
    }

    assert!(
        !barrier_cols.is_empty(),
        "logo must contain horizontal barrier pixels"
    );

    // Barrier X-coordinates must be contiguous from start to end.
    let first_x = barrier_cols.first().unwrap().0;
    let last_x = barrier_cols.last().unwrap().0;
    assert_eq!(
        barrier_cols.len(),
        last_x - first_x + 1,
        "barrier must be horizontally contiguous with no gaps"
    );

    // Every X column in the barrier must occupy the exact same logical Y-coordinates (0° horizontal).
    let expected_ys = &barrier_cols[0].1;
    let expected_thickness = expected_ys.len();
    assert!(expected_thickness > 0);
    for (x, ys, _) in &barrier_cols {
        assert_eq!(
            ys, expected_ys,
            "barrier at x={x} must occupy constant logical Y-coordinates {expected_ys:?}"
        );
        assert_eq!(
            ys.len(),
            expected_thickness,
            "barrier at x={x} must have constant thickness {expected_thickness}"
        );
    }

    // Group contiguous columns by colour into segments and verify identical thickness & width,
    // plus alternating coral-red and off-white colours.
    let mut segments: Vec<(Px, usize, usize)> = Vec::new();
    for (x, ys, color) in barrier_cols {
        if let Some(last) = segments.last_mut()
            && last.0 == color
        {
            last.1 += 1;
            assert_eq!(last.2, ys.len());
        } else {
            segments.push((color, 1, ys.len()));
        }
        let _ = x;
    }
    assert!(
        segments.len() >= 3,
        "barrier must have alternating red and white segments: {segments:?}"
    );
    let seg_width = segments[0].1;
    let seg_thickness = segments[0].2;
    for (idx, &(color, width, thickness)) in segments.iter().enumerate() {
        assert_eq!(
            thickness, seg_thickness,
            "segment {idx} must have identical thickness"
        );
        assert_eq!(width, seg_width, "segment {idx} must have identical width");
        let expected_color = if idx % 2 == 0 {
            Px::BarrierRed
        } else {
            Px::BarrierWhite
        };
        assert_eq!(
            color, expected_color,
            "segment {idx} must alternate BarrierRed and BarrierWhite"
        );
    }
}

#[test]
fn logo_contains_no_yellow_or_orange_pixels_and_renders_booth_and_usage_bars() {
    let all_variants = [
        Px::Bg,
        Px::Booth,
        Px::Prompt,
        Px::BarrierRed,
        Px::BarrierWhite,
        Px::UsageGrey,
        Px::UsageBlue,
        Px::UsageGreen,
    ];
    for px in all_variants {
        let (r, g, b) = px.rgb();
        // Yellow/orange colours have high red and green significantly above blue (R > 180, G > B + 20).
        let is_yellow_or_orange = r > 180 && g > b.saturating_add(20);
        assert!(
            !is_yellow_or_orange,
            "pixel variant {px:?} rgb({r}, {g}, {b}) must not be yellow or orange"
        );
    }

    // Verify usage bars: muted grey, blue, and green vertical bars of different heights
    // aligned to a common baseline below the barrier.
    let bar_metrics = |target: Px| -> (usize, usize, usize) {
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for (y, row) in LOGO_BITMAP.iter().enumerate() {
            for (x, &p) in row.iter().enumerate() {
                if p == target {
                    xs.push(x);
                    ys.push(y);
                }
            }
        }
        xs.sort_unstable();
        xs.dedup();
        assert_eq!(xs.len(), 1, "{target:?} must form a single vertical bar");
        let min_y = *ys.iter().min().unwrap();
        let max_y = *ys.iter().max().unwrap();
        let height = max_y - min_y + 1;
        assert_eq!(
            ys.len(),
            height,
            "{target:?} bar must be vertically contiguous"
        );
        (xs[0], max_y, height)
    };

    let (grey_x, grey_base, grey_h) = bar_metrics(Px::UsageGrey);
    let (blue_x, blue_base, blue_h) = bar_metrics(Px::UsageBlue);
    let (green_x, green_base, green_h) = bar_metrics(Px::UsageGreen);

    assert_eq!(
        grey_base, blue_base,
        "grey and blue bars share a common baseline"
    );
    assert_eq!(
        blue_base, green_base,
        "blue and green bars share a common baseline"
    );
    assert!(
        grey_x < blue_x && blue_x < green_x,
        "bars are ordered left-to-right"
    );
    assert!(
        grey_h != blue_h && blue_h != green_h && grey_h != green_h,
        "all three usage bars must have different heights: ({grey_h}, {blue_h}, {green_h})"
    );

    let _home = crate::testutil::HomeSandbox::new();
    for tier in [theme::Tier::Full, theme::Tier::Compatible] {
        let _tier = crate::testutil::TierSandbox::new(tier);
        let warning = theme::warning_color();
        let orange = theme::accent_2_color();
        let width = 100usize;
        let (rows, buf) = render_header_at(1000, width as u16);
        for (y, want) in LOGO.iter().enumerate() {
            let cells: String = rows[y].chars().take(10).collect();
            assert_eq!(cells, format!("{want} "), "{tier:?} header logo row {y}");
            for x in 0..10 {
                let cell = &buf.content[y * width + x];
                assert!(
                    cell.fg != warning && cell.bg != warning,
                    "{tier:?}: no yellow/warning colour in logo at ({x}, {y})"
                );
                assert!(
                    cell.fg != orange && cell.bg != orange,
                    "{tier:?}: no orange/accent_2 colour in logo at ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn rendering_logo_does_not_panic_when_area_is_smaller_than_12_by_5() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    for width in 0..=15u16 {
        for height in 0..=7u16 {
            let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 20, 10));
            let area = Rect::new(2, 1, width, height);
            TollgateLogo.render(area, &mut buf);

            // Cells outside `area` must remain untouched (' ').
            for y in 0..10u16 {
                for x in 0..20u16 {
                    let inside = x >= area.x
                        && x < area.x + area.width
                        && y >= area.y
                        && y < area.y + area.height;
                    if !inside {
                        assert_eq!(
                            buf[(x, y)].symbol(),
                            " ",
                            "cell ({x}, {y}) outside {area:?} must not be overwritten"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn logo_candidates_fill_the_three_by_nine_footprint_in_block_glyphs() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 10, 3));
    TollgateLogo.render(Rect::new(0, 0, 10, 3), &mut buf);
    let drawn = crate::testutil::buffer_rows(&buf);

    for (y, row) in LOGO.iter().enumerate() {
        assert_eq!(row.chars().count(), 9, "LOGO row {y} is nine cells");
        assert!(
            row.chars().all(is_block_glyph),
            "LOGO row {y} is block and quadrant glyphs only: {row:?}"
        );
        assert_eq!(
            drawn[y],
            format!("{row} "),
            "LOGO row {y} draws as written, the tenth cell a gap"
        );
    }

    for (name, rows) in LOGO_CANDIDATES {
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(row.chars().count(), 9, "{name} row {y} is nine cells");
            assert!(
                row.chars().all(is_block_glyph),
                "{name} row {y} is block and quadrant glyphs only: {row:?}"
            );
            assert!(
                row.chars().any(|c| c != ' '),
                "{name} row {y} carries ink: {row:?}"
            );
        }
        assert!(
            !UPSTREAM_MASCOT.iter().any(|m| rows.contains(m)),
            "{name} shares no row with the upstream mascot"
        );
    }
}

#[test]
fn every_logo_alternative_stays_commented_beside_the_wired_one() {
    let source = include_str!("../../src/tui/render/header.rs");
    for (name, rows) in &LOGO_CANDIDATES[1..] {
        let line = format!(
            "// {:<8}[\"{}\", \"{}\", \"{}\"]",
            format!("{name}:"),
            rows[0],
            rows[1],
            rows[2]
        );
        assert!(
            source.contains(&line),
            "header.rs keeps the {name} alternative as one comment line: {line}"
        );
    }
    for mascot in UPSTREAM_MASCOT {
        assert!(
            !source.contains(mascot),
            "the upstream mascot row {mascot:?} is gone from header.rs"
        );
    }
}

#[test]
fn the_header_gauge_keeps_a_wide_active_name_whole() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("日本語", 42.0)], Some("日本語"));
    app.tab = Tab::Tokens;
    let chars: Vec<char> = row_content(&app, 120, 1).chars().collect();
    let left: String = chars[..49].iter().collect();
    assert_eq!(left, "[ 0 live ]  [ — tok/s ] · 日 本 語  42% [████░░░░░░]");
}

/// P1: the header gauge labels its reading with at most two nearest-rounded
/// decimals, with and without its bar.
#[test]
fn the_header_gauge_label_keeps_two_rounded_decimals() {
    for bar_cells in [10, 0] {
        for (pct, want) in [
            (73.42, "73.42%"),
            (1.000_000_000_000_000_9, "1%"),
            (99.999, "100%"),
        ] {
            let fit = GaugeFit {
                name_w: 0,
                bar_cells,
                visible: true,
            };
            let spans = gauge_spans(fit, "", Some(pct), 0);
            let labels: Vec<&str> = spans
                .iter()
                .map(|s| s.content.as_ref())
                .filter(|c| c.ends_with('%'))
                .collect();
            assert_eq!(labels, [want], "{pct} with {bar_cells} bar cells");
        }
    }
}

// ── tps: `[ R tok/s ]` after the live chip, `(S t/s)` after the gauge ─────────

fn tps_snapshot(age_ms: u64, rate: f64, active_speed: Option<f64>) -> crate::tps::Snapshot {
    let stamp = chrono::DateTime::from_timestamp_millis(
        i64::try_from(crate::usage::now_ms() - age_ms).unwrap(),
    )
    .unwrap()
    .format("%Y-%m-%dT%H:%M:%S%.3f+00:00")
    .to_string();
    let figures = |rate: f64, speed: Option<f64>| crate::tps::Figures {
        live: crate::tps::Figure { rate, speed },
        ..crate::tps::Figures::default()
    };
    let mut snap = crate::tps::Snapshot {
        version: crate::tps::CACHE_VERSION,
        generated_at: stamp,
        total: figures(rate, active_speed),
        ..crate::tps::Snapshot::default()
    };
    snap.accounts
        .insert("uwuclxdy".into(), figures(rate, active_speed));
    snap
}

/// The tok/s counter chip renders at 0 and at 1 (one decimal below 10), right
/// after the live chip; the gauge's speed suffix hides while the active
/// account has no live speed and reads `(92 t/s)` once it has one.
#[test]
fn the_tps_chip_shows_at_zero_and_one_and_the_speed_suffix_hides_when_absent() {
    let _home = crate::testutil::HomeSandbox::new();
    for (rate, speed, want) in [
        (
            0.0,
            None,
            "[ 0 live ]  [ 0 tok/s ] · uwuclxdy 42% [████░░░░░░]   ",
        ),
        (
            1.0,
            None,
            "[ 0 live ]  [ 1.0 tok/s ] · uwuclxdy 42% [████░░░░░░]   ",
        ),
        (
            67.2,
            Some(92.4),
            "[ 0 live ]  [ 67 tok/s ] · uwuclxdy 42% [████░░░░░░] (92 t/s)   ",
        ),
    ] {
        let mut fresh = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
        fresh.tab = Tab::Tokens;
        fresh.set_live_sessions(tally_of(0));
        fresh.set_tps(Some(tps_snapshot(1_000, rate, speed)));
        let row = row_content(&fresh, 120, 1);
        assert!(row.starts_with(want), "{rate}: {row:?}");
    }
}

/// No producer (no cache, or one past `STALE_AFTER_MS`) reads `—`, never a
/// fabricated 0, and draws no speed suffix.
#[test]
fn the_tps_chip_reads_a_dash_without_a_fresh_producer() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(tally_of(0));
    for snap in [
        None,
        Some(tps_snapshot(
            crate::tps::STALE_AFTER_MS + 1_000,
            50.0,
            Some(90.0),
        )),
    ] {
        let stale = snap.is_some();
        app.set_tps(snap);
        let row = row_content(&app, 120, 1);
        assert!(
            row.starts_with("[ 0 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]   "),
            "stale={stale}: {row:?}"
        );
    }
}

/// The tok/s chip's cells by role: brackets and unit `TEXT_DIM` without bold,
/// the number `TEXT` + bold at a non-zero rate, `TEXT_DIM` at zero. Pinned per
/// cell off the theme's own colors.
#[test]
fn the_tps_chip_cells_carry_the_counter_chip_grammar() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let dim = super::theme::text_dim_color();
    let text = super::theme::text_color();
    for (rate, number_fg, number_bold) in [(0.0, dim, false), (67.2, text, true)] {
        let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
        app.tab = Tab::Tokens;
        app.set_live_sessions(tally_of(0));
        app.set_tps(Some(tps_snapshot(1_000, rate, None)));
        let height = header_height(&app);
        let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, &app, &app.readings_snapshot());
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let cell = |col: usize| &buf.content[120 + 10 + col];
        // `[ 0 live ]` is 10 cells, then the 2-cell chip gap, so the tok/s chip
        // opens at column 12.
        let chip_at = 12usize;
        let number = crate::tps::format_figure(rate);
        let chip_len = 2 + number.chars().count() + 1 + 5 + 1 + 1;
        let text_of: String = (chip_at..chip_at + chip_len)
            .map(|c| cell(c).symbol().to_string())
            .collect();
        assert_eq!(
            text_of,
            format!("[ {number} tok/s ]"),
            "{rate}: the chip's text"
        );
        let close = chip_at + chip_len - 1;
        for (col, role) in [
            (chip_at, "open bracket"),
            (chip_at + 1, "inner gap"),
            (close, "close bracket"),
        ] {
            assert_eq!(cell(col).fg, dim, "{rate}: the {role} renders TEXT_DIM");
            assert!(
                !cell(col).modifier.contains(Modifier::BOLD),
                "{rate}: the {role} carries no bold"
            );
        }
        let unit_t = chip_at + 2 + number.chars().count() + 1;
        assert_eq!(cell(unit_t).fg, dim, "{rate}: the unit renders TEXT_DIM");
        assert!(
            !cell(unit_t).modifier.contains(Modifier::BOLD),
            "{rate}: the unit carries no bold"
        );
        assert_eq!(
            cell(chip_at + 2).fg,
            number_fg,
            "{rate}: the number's color"
        );
        assert_eq!(
            cell(chip_at + 2).modifier.contains(Modifier::BOLD),
            number_bold,
            "{rate}: the number's weight"
        );
    }

    // The no-reading `—` renders TEXT_DIM without bold, never the bright
    // non-zero style.
    {
        let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
        app.tab = Tab::Tokens;
        app.set_live_sessions(tally_of(0));
        app.set_tps(None);
        let height = header_height(&app);
        let mut term = Terminal::new(TestBackend::new(120, height)).unwrap();
        term.draw(|f| {
            let area = f.area();
            super::draw(f, area, &app, &app.readings_snapshot());
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let dash = &buf.content[120 + 10 + 14];
        assert_eq!(dash.fg, dim, "the dash renders TEXT_DIM");
        assert!(
            !dash.modifier.contains(Modifier::BOLD),
            "the dash carries no bold"
        );
    }
}

/// The tok/s chip renders tight after a wider rate: `1.0` (three cells) leaves
/// no blank cell behind when the rate falls to `67`, and the ` · ` before the
/// gauge follows the closing bracket directly in every state.
#[test]
fn the_tps_chip_renders_tight_after_a_wider_rate() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(tally_of(0));
    app.set_tps(Some(tps_snapshot(1_000, 1.0, None)));
    app.set_tps(Some(tps_snapshot(1_000, 67.2, None)));
    let row = row_content(&app, 120, 1);
    assert!(
        row.starts_with("[ 0 live ]  [ 67 tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "{row:?}"
    );
    // The quiet `0` and `—` states render tight too.
    app.set_tps(Some(tps_snapshot(1_000, 0.0, None)));
    let row = row_content(&app, 120, 1);
    assert!(
        row.starts_with("[ 0 live ]  [ 0 tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "{row:?}"
    );
    app.set_tps(None);
    let row = row_content(&app, 120, 1);
    assert!(
        row.starts_with("[ 0 live ]  [ — tok/s ] · uwuclxdy 42% [████░░░░░░]"),
        "{row:?}"
    );
}

/// The line-2 ladder at the widths where each piece sheds: the tok/s chip
/// first, then the live chip, then the speed suffix, then the gauge's bar.
#[test]
fn the_line_two_ladder_sheds_the_tps_chip_then_the_live_chip_then_the_suffix() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(tally_of(0));
    app.set_tps(Some(tps_snapshot(1_000, 67.2, Some(92.4))));
    for (w, expected) in [
        (
            120,
            "[ 0 live ]  [ 67 tok/s ] · uwuclxdy 42% [████░░░░░░] (92 t/s)                               ● status.claude.ai",
        ),
        (
            80,
            "uwuclxdy 42% [████░░░░░░] (92 t/s)                  ● status.claude.ai",
        ),
        (
            70,
            "uwuclxdy 42% [████░░░░░░] (92 t/s)        ● status.claude.ai",
        ),
        (60, "uwuclxdy 42% [████░░░░░░]       ● status.claude.ai"),
        (56, "uwuclxdy 42% [████░░░░░]    ● status.claude.ai"),
    ] {
        assert_eq!(
            row_content(&app, w, 1).trim_end(),
            expected,
            "row 1 at {w} cols"
        );
    }
}

/// Freshness is judged at render time, never cached at set time: the same
/// snapshot reads fresh at 29 s and stale at 31 s once the pinned clock passes
/// the 30 s window.
#[test]
fn a_snapshot_ages_past_the_stale_window_per_frame() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(tally_of(0));
    let now = crate::usage::now_ms();
    app.set_tps(Some(tps_snapshot(29_000, 67.2, Some(92.4))));

    app.tps_now_override = Some(now);
    assert!(
        row_content(&app, 120, 1).starts_with("[ 0 live ]  [ 67 tok/s ] · uwuclxdy"),
        "29 s reads fresh"
    );

    app.tps_now_override = Some(now + 2_000);
    assert!(
        row_content(&app, 120, 1).starts_with("[ 0 live ]  [ — tok/s ] · uwuclxdy"),
        "31 s reads stale without a re-set"
    );
}

/// The ` (S t/s)` suffix renders through the chip's capped figure, so it never
/// outgrows the ` (9999 t/s)` width the chip shed checks charge.
#[test]
fn the_speed_suffix_stays_inside_its_charged_ceiling() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut app = app_with(vec![oauth_profile("uwuclxdy", 42.0)], Some("uwuclxdy"));
    app.tab = Tab::Tokens;
    app.set_live_sessions(tally_of(0));
    app.set_tps(Some(tps_snapshot(1_000, 1.0, Some(12_345.0))));
    let row = row_content(&app, 160, 1);
    assert!(row.contains("[████░░░░░░] (12k t/s)"), "{row:?}");
}

#[test]
fn wide_active_names_keep_the_status_indicator_whole_at_every_width() {
    let _home = crate::testutil::HomeSandbox::new();
    for name in [
        "abcdefghijklmnopqrstuvwxyz",
        "cjk-日本語アカウント",
        "emoji-🧭⚠️-account",
    ] {
        let mut app = app_with(vec![oauth_profile(name, 49.0)], Some(name));
        app.tab = Tab::Usage;
        for width in 24..=160 {
            let row = row_content(&app, width, 1);
            if row.contains("status") {
                assert!(
                    row.ends_with("● status.claude.ai"),
                    "{name} at {width}: {row}"
                );
            }
        }
    }
}
