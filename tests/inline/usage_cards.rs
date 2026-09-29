#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The metric-card layout: the whole report pinned as plain-text goldens at 80
//! and 120 columns (`tests/fixtures/usage_cards_{80,120}.txt`), plus the bar,
//! the severity words, the spend grouping and the status lines.

use super::*;
use crate::usage::observation::{
    Amount, Failure, FailureKind, MoneyKind, MoneyMeter, MoneyScope, PeriodKind, Timestamp,
};

include!("../fixtures/usage_cards_sample.rs");

const GOLDEN_80: &str = include_str!("../fixtures/usage_cards_80.txt");
const GOLDEN_120: &str = include_str!("../fixtures/usage_cards_120.txt");

fn ctx(width: usize) -> CardCtx {
    CardCtx {
        width,
        now_secs: SAMPLE_NOW,
        offset_secs: SAMPLE_OFFSET,
        guest_mode: true,
    }
}

fn render(width: usize) -> String {
    report_lines(&sample_accounts(), &ctx(width))
        .iter()
        .map(|l| format!("{}\n", plain_text(l)))
        .collect()
}

#[test]
fn the_card_report_matches_the_80_column_golden() {
    let got = render(80);
    assert_eq!(got, GOLDEN_80, "the 80-column card report changed:\n{got}");
}

#[test]
fn the_card_report_matches_the_120_column_golden() {
    let got = render(120);
    assert_eq!(
        got, GOLDEN_120,
        "the 120-column card report changed:\n{got}"
    );
}

/// No line of either golden overflows its width, and every bar row is exactly
/// as wide as the layout — the bar fills the card, whatever the terminal.
#[test]
fn every_card_line_fits_its_width_and_bars_span_it() {
    for width in [80usize, 120] {
        for line in report_lines(&sample_accounts(), &ctx(width)) {
            let text = plain_text(&line);
            let cells = text.chars().count();
            assert!(cells <= width, "{cells} > {width}: {text:?}");
            if text.contains('░') || text.contains('█') {
                assert_eq!(cells, width, "a bar row spans the width: {text:?}");
            }
        }
    }
}

#[test]
fn a_bar_is_exact_width_with_the_elapsed_marker_in_place() {
    let segs = bar_segs(Some(50.0), Some(25.0), 20, Ink::Sev(Severity::Mid));
    let text: String = segs.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(text.chars().count(), 20);
    assert_eq!(text, "█████│████░░░░░░░░░░");
    // Unknown share: an empty track, no marker.
    let empty: String = bar_segs(None, None, 6, Ink::Dim)
        .iter()
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(empty, "░░░░░░");
    // Over 100 % fills, and a marker at 100 % stays inside the bar.
    let full: String = bar_segs(Some(140.0), Some(100.0), 5, Ink::Dim)
        .iter()
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(full, "████│");
}

#[test]
fn a_window_card_carries_countdown_local_time_pace_and_severity_word() {
    let accounts = sample_accounts();
    let weekly = accounts[0].window("weekly").unwrap();
    let rows: Vec<String> = window_card(weekly, &ctx(80))
        .iter()
        .map(|l| plain_text(l))
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(rows[0].starts_with("  Weekly · 7d"), "{rows:?}");
    // 4d 1h from 10:00Z is 11:00Z, 13:00 at UTC+2.
    assert!(rows[0].ends_with("resets in 4d 1h (13:00)"), "{rows:?}");
    assert!(rows[1].ends_with("81% ↑ HIGH"), "{rows:?}");
    assert_eq!(rows[2], "  42% elapsed · 38 pts ahead");
    // The severity word is coloured by its rung, and bold.
    let word = window_card(weekly, &ctx(80))[1]
        .iter()
        .find(|s| s.text.contains("HIGH"))
        .cloned()
        .unwrap();
    assert_eq!(word.ink, Ink::Sev(Severity::High));
    assert!(word.bold);
}

#[test]
fn an_unknown_reset_reads_no_reset_time_and_draws_no_marker() {
    let mut w = QuotaWindow::new(
        "session",
        "5h",
        crate::usage::observation::WindowScope::Shared,
    );
    w.used_pct = Some(10.0);
    let rows = window_card(&w, &ctx(60));
    assert!(plain_text(&rows[0]).ends_with("no reset time"));
    assert!(!plain_text(&rows[1]).contains('│'));
    assert_eq!(rows.len(), 2, "no pace, no footnote");
}

#[test]
fn spend_meters_fold_into_one_row_and_a_key_cap_is_graded_as_usage() {
    let accounts = sample_accounts();
    let rows: Vec<String> = money_cards(&accounts[1], &ctx(80))
        .iter()
        .map(|l| plain_text(l))
        .collect();
    assert!(rows[0].starts_with("  Credit balance") && rows[0].ends_with("$13.67 left"));
    assert!(rows[1].starts_with("  Spend"));
    assert!(
        rows[1].ends_with("today $0.00 · week $4.08 · month $4.46"),
        "{rows:?}"
    );
    assert_eq!(rows.iter().filter(|r| r.contains("week $4.08")).count(), 1);
    assert!(rows[2].ends_with("$9.00 left of $50.00"), "{rows:?}");
    assert!(rows[3].ends_with("82% used HIGH"), "{rows:?}");
    assert_eq!(rows[4], "  provider limit · monthly");
}

#[test]
fn a_low_balance_says_low_and_a_debt_says_critical() {
    let meter = |amount: &str| {
        MoneyMeter::new(
            "wallet",
            "Balance",
            MoneyKind::Balance,
            Amount::parse(amount).unwrap(),
            "USD",
            MoneyScope::Profile,
        )
    };
    let mut o = sample_accounts().remove(1);
    o.money = vec![meter("3.00")];
    let low = plain_text(&money_cards(&o, &ctx(60))[0]);
    assert!(low.ends_with("$3.00 left LOW"), "{low}");
    o.money = vec![meter("-5.71")];
    let debt = plain_text(&money_cards(&o, &ctx(60))[0]);
    assert!(debt.ends_with("-$5.71 left CRITICAL"), "{debt}");
    // No ladder outside USD: no word, never a guess.
    let mut cny = meter("3.00");
    cny.currency = "CNY".to_string();
    o.money = vec![cny];
    let other = plain_text(&money_cards(&o, &ctx(60))[0]);
    assert!(other.ends_with("¥3.00 left"), "{other}");
}

#[test]
fn a_spent_cap_is_critical_even_without_a_limit() {
    let mut o = sample_accounts().remove(1);
    let mut cap = MoneyMeter::new(
        "key_limit",
        "Key cap",
        MoneyKind::Limit,
        Amount::parse("0").unwrap(),
        "USD",
        MoneyScope::Key,
    );
    cap.period = None;
    o.money = vec![cap];
    let rows: Vec<String> = money_cards(&o, &ctx(60))
        .iter()
        .map(|l| plain_text(l))
        .collect();
    assert!(rows[1].ends_with("100% used CRITICAL"), "{rows:?}");
}

#[test]
fn stale_and_failing_accounts_say_so_with_an_action_hint() {
    let accounts = sample_accounts();
    let stale: Vec<String> = status_lines(&accounts[1], &ctx(80))
        .iter()
        .map(|l| plain_text(l))
        .collect();
    assert_eq!(stale, vec!["  ⏸ updated 12m ago"]);
    let mut o = accounts[2].clone();
    let mut f = Failure::new(FailureKind::RateLimited, "429 from the usage endpoint");
    f.retry_after = Some(Timestamp(SAMPLE_NOW + 300));
    o.failure = Some(f);
    let failing: Vec<String> = status_lines(&o, &ctx(80))
        .iter()
        .map(|l| plain_text(l))
        .collect();
    assert_eq!(
        failing,
        vec!["  ⚠ 429 from the usage endpoint · retry in 5m · backing off, retries on its own"]
    );
    o.failure = Some(Failure::new(FailureKind::AuthRequired, "token revoked"));
    o.origin = crate::usage::observation::Origin::Profile;
    o.label = "work".to_string();
    assert_eq!(failure_hint(&o).unwrap(), "run `tollgate login work`");
}

#[test]
fn the_header_marks_active_guest_and_origin() {
    let accounts = sample_accounts();
    let head = |o, guest| {
        let mut c = ctx(80);
        c.guest_mode = guest;
        plain_text(&header_line(o, &c))
    };
    assert_eq!(
        head(&accounts[0], true),
        "work · Anthropic · Max 20x · ● active [guest]"
    );
    assert_eq!(
        head(&accounts[0], false),
        "work · Anthropic · Max 20x · ● active"
    );
    assert_eq!(
        head(&accounts[2], true),
        "oll-main · Ollama Cloud · pro (monitor)"
    );
    assert_eq!(
        head(&accounts[3], true),
        "personal · Anthropic · ● active (clauth)"
    );
}

#[test]
fn accounts_group_by_provider_in_first_seen_order() {
    let text = render(80);
    let anthropic = text.find("── Anthropic").unwrap();
    let openrouter = text.find("── OpenRouter").unwrap();
    let ollama = text.find("── Ollama Cloud").unwrap();
    assert!(anthropic < openrouter && openrouter < ollama);
    // The upstream account joins its provider's group rather than trailing.
    let personal = text.find("personal · Anthropic").unwrap();
    assert!(anthropic < personal && personal < openrouter);
    assert_eq!(text.matches("── Anthropic").count(), 1);
}

#[test]
fn an_empty_report_says_how_to_add_an_account() {
    let rows: Vec<String> = report_lines(&[], &ctx(80))
        .iter()
        .map(|l| plain_text(l))
        .collect();
    assert_eq!(
        rows.last().unwrap(),
        "no accounts yet. add one with `tollgate login <name>`."
    );
}

#[test]
fn a_zero_period_spend_group_keeps_its_three_rows_distinct() {
    let accounts = sample_accounts();
    let row = plain_text(&money_cards(&accounts[1], &ctx(120))[1]);
    for part in ["today $0.00", "week $4.08", "month $4.46"] {
        assert_eq!(row.matches(part).count(), 1, "{row}");
    }
    let _ = PeriodKind::Daily;
}
