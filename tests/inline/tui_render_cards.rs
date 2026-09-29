#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Usage tab's monitoring / upstream cards: extra rail entries below the
//! profiles, the shared metric-card layout in the detail pane, coloured
//! through the palette, and the rail's cursor over both lists.

use super::*;
use crate::profile::{AppConfig, AppState, ProfileName};
use crate::tui::app::{App, set_usage_extras, step_usage_cursor, usage_extras_from};
use crate::tui::theme::Tier;
use crate::usage::observation::AccountObservation;

include!("../fixtures/usage_cards_sample.rs");

fn monitor() -> AccountObservation {
    sample_accounts().remove(2)
}

fn upstream() -> AccountObservation {
    sample_accounts().remove(3)
}

fn app_with_extras(extras: Vec<AccountObservation>) -> App {
    let work = crate::testutil::blank_profile(&ProfileName::from("work"));
    let mut app = App::new(AppConfig {
        state: AppState {
            profiles: vec![ProfileName::from("work")],
            ..AppState::default()
        },
        profiles: vec![work],
    });
    set_usage_extras(&mut app, extras);
    app
}

#[test]
fn a_monitor_card_renders_in_the_usage_tab_with_palette_colours() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["work"]);
    let _tier = crate::testutil::TierSandbox::new(Tier::Full);
    let mut app = app_with_extras(vec![monitor(), upstream()]);
    app.usage_extra_cursor = Some(0);

    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).expect("terminal");
    term.draw(|f| super::super::usage::draw(f, f.area(), &app))
        .expect("draw");
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    let text = rows.join("\n");

    // The rail: the profile, then both extras, tagged by origin.
    assert!(text.contains("work"), "{text}");
    assert!(text.contains("oll-main · monitor"), "{text}");
    assert!(text.contains("personal · clauth"), "{text}");
    // The detail: header, failure with its hint, the window card, the balance.
    assert!(text.contains("Ollama Cloud · pro (monitor)"), "{text}");
    assert!(text.contains("⚠ ollama.com did not answer · retries on the next poll"));
    assert!(text.contains("Session · 5h"));
    assert!(text.contains("no reset time"));
    assert!(text.contains("93% CRITICAL"));
    assert!(text.contains("$0.40 left CRITICAL"));

    // The bar's fill wears the critical colour of the installed palette.
    let (y, row) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| r.contains("93% CRITICAL"))
        .unwrap();
    let x = row.chars().position(|c| c == '█').unwrap();
    let cell = &buf[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())];
    assert_eq!(cell.fg, crate::tui::theme::danger_color());
}

#[test]
fn without_extras_the_usage_tab_is_unchanged() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["work"]);
    let app = app_with_extras(Vec::new());
    let mut term =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 20)).expect("terminal");
    term.draw(|f| super::super::usage::draw(f, f.area(), &app))
        .expect("draw");
    let text = crate::testutil::buffer_rows(term.backend().buffer()).join("\n");
    assert!(!text.contains("· monitor"));
    assert!(text.contains("work"));
}

#[test]
fn the_rail_cursor_walks_profiles_then_extras_and_wraps() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["work"]);
    let mut app = app_with_extras(vec![monitor(), upstream()]);
    assert_eq!((app.profile_cursor, app.usage_extra_cursor), (0, None));
    step_usage_cursor(&mut app, 1);
    assert_eq!(app.usage_extra_cursor, Some(0));
    assert_eq!(app.profile_cursor, 0, "the other tabs' cursor stays put");
    step_usage_cursor(&mut app, 1);
    assert_eq!(app.usage_extra_cursor, Some(1));
    step_usage_cursor(&mut app, 1);
    assert_eq!((app.profile_cursor, app.usage_extra_cursor), (0, None));
    step_usage_cursor(&mut app, -1);
    assert_eq!(app.usage_extra_cursor, Some(1));
}

#[test]
fn a_refresh_keeps_the_selected_extra_by_id() {
    let _home = crate::testutil::HomeSandbox::new();
    crate::testutil::register_names(&["work"]);
    let mut app = app_with_extras(vec![monitor(), upstream()]);
    app.usage_extra_cursor = Some(1);
    // The upstream account moves to the front: the selection follows it.
    set_usage_extras(&mut app, vec![upstream(), monitor()]);
    assert_eq!(app.usage_extra_cursor, Some(0));
    // Gone: back to the profile rail.
    set_usage_extras(&mut app, vec![monitor()]);
    assert_eq!(app.usage_extra_cursor, None);
}

fn hook_a(_: &crate::usage::collect::CollectCtx<'_>) -> Vec<AccountObservation> {
    vec![monitor()]
}

fn hook_b(_: &crate::usage::collect::CollectCtx<'_>) -> Vec<AccountObservation> {
    // A duplicate id (first wins) and an upstream account.
    vec![monitor(), upstream()]
}

#[test]
fn the_extras_come_from_the_hooks_first_id_wins() {
    let codex = crate::codex_profiles::CodexState::default();
    let ctx = crate::usage::collect::CollectCtx {
        config: None,
        codex: &codex,
        now_ms: u64::try_from(SAMPLE_NOW).unwrap() * 1000,
        interval_ms: 60_000,
        guest_mode: true,
        include_disabled: false,
    };
    let got = usage_extras_from(&ctx, &[hook_a], &[hook_b]);
    let ids: Vec<&str> = got.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["monitor:oll-main", "upstream:personal"]);
}

#[test]
fn observation_lines_put_the_header_first_and_fit_the_pane() {
    let lines = observation_lines(&monitor(), 60, SAMPLE_NOW, false);
    let first: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(first, "oll-main · Ollama Cloud · pro (monitor)");
    assert!(lines.iter().all(|l| l.width() <= 60));
}
