#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::derive` against the shared fixture tables
//! (`tests/fixtures/{countdown,severity,pace}.json`) plus the money, lead-window
//! and aggregate-severity rules. The tables are the one spelling every surface
//! renders, so a change here is a change to all of them.

use super::*;
use crate::usage::observation::{
    AccountObservation, AuthKind, Failure, MoneyScope, Origin, SourceId, WindowScope,
};
use serde_json::Value;

const COUNTDOWN: &str = include_str!("../fixtures/countdown.json");
const SEVERITY: &str = include_str!("../fixtures/severity.json");
const PACE: &str = include_str!("../fixtures/pace.json");

fn table(json: &str, key: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(json).expect("fixture parses");
    v[key].as_array().expect("fixture table").clone()
}

fn sev(s: &str) -> Severity {
    serde_json::from_value(Value::String(s.to_string())).expect("severity name")
}

fn ts(s: &str) -> Timestamp {
    Timestamp::parse(s).expect("fixture timestamp")
}

#[test]
fn countdown_matches_the_fixture_table() {
    let rows = table(COUNTDOWN, "countdown");
    assert!(rows.len() >= 10, "the table stopped loading");
    for row in rows {
        let secs = row["secs"].as_i64();
        assert_eq!(
            countdown(secs),
            row["want"].as_str().unwrap(),
            "secs={secs:?}"
        );
    }
}

#[test]
fn countdown_to_measures_from_now_and_unknown_is_a_dash() {
    let now = ts("2026-09-29T10:00:00+00:00").secs();
    assert_eq!(
        countdown_to(Some(ts("2026-09-29T13:05:59+00:00")), now),
        "3h 05m"
    );
    assert_eq!(countdown_to(None, now), "—");
    assert_eq!(countdown_with_local(None, now), "—");
}

#[test]
fn local_time_matches_the_fixture_table() {
    for row in table(COUNTDOWN, "local_time") {
        let at = ts(row["at"].as_str().unwrap());
        let offset = row["offset_secs"].as_i64().unwrap() as i32;
        assert_eq!(
            hhmm_at_offset(at, offset),
            row["want"].as_str().unwrap(),
            "{row}"
        );
    }
}

#[test]
fn countdown_with_local_appends_the_local_clock_in_parentheses() {
    let at = ts("2026-09-29T13:05:00+00:00");
    let got = countdown_with_local(Some(at), at.secs() - 11_100);
    assert_eq!(got, format!("3h 05m ({})", local_hhmm(at)));
}

#[test]
fn used_pct_severity_matches_the_fixture_table() {
    for row in table(SEVERITY, "used_pct") {
        let pct = row["pct"].as_f64().unwrap();
        assert_eq!(
            severity_from_used_pct(pct),
            sev(row["want"].as_str().unwrap()),
            "{pct}"
        );
    }
}

#[test]
fn balance_severity_matches_the_fixture_table() {
    for row in table(SEVERITY, "balance") {
        let amount = Amount::parse(row["amount"].as_str().unwrap()).unwrap();
        let want = row["want"].as_str().map(sev);
        assert_eq!(
            severity_from_balance(&amount, row["currency"].as_str().unwrap()),
            want,
            "{row}"
        );
    }
}

#[test]
fn pace_severity_matches_the_fixture_table() {
    for row in table(SEVERITY, "pace") {
        let delta = row["delta"].as_f64().unwrap();
        assert_eq!(
            severity_from_pace(delta),
            sev(row["want"].as_str().unwrap()),
            "{delta}"
        );
    }
}

#[test]
fn severity_words_and_classes_match_the_fixture_table() {
    for row in table(SEVERITY, "words") {
        let name = row["severity"].as_str().unwrap();
        let s = sev(name);
        assert_eq!(s.class(), name);
        assert_eq!(s.word(SeverityBasis::Usage), row["usage"].as_str().unwrap());
        assert_eq!(
            s.word(SeverityBasis::Balance),
            row["balance"].as_str().unwrap()
        );
    }
}

#[test]
fn worst_of_matches_the_fixture_table() {
    for row in table(SEVERITY, "worst_of") {
        let of: Vec<Severity> = row["of"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| sev(v.as_str().unwrap()))
            .collect();
        assert_eq!(worst_of(of), row["want"].as_str().map(sev), "{row}");
    }
}

#[test]
fn pace_matches_the_fixture_table() {
    for row in table(PACE, "pace") {
        let p = pace(
            row["used"].as_f64().unwrap(),
            row["elapsed"].as_f64().unwrap(),
        );
        let verdict: PaceVerdict = serde_json::from_value(row["verdict"].clone()).unwrap();
        assert_eq!(p.verdict, verdict, "{row}");
        assert_eq!(p.label(), row["label"].as_str().unwrap(), "{row}");
        assert_eq!(
            p.glyph().to_string(),
            row["glyph"].as_str().unwrap(),
            "{row}"
        );
    }
}

#[test]
fn window_pace_derives_elapsed_from_the_reset_and_length() {
    for row in table(PACE, "window") {
        let now = ts(row["now"].as_str().unwrap()).secs();
        let mut w = QuotaWindow::new("session", "5h", WindowScope::Shared);
        w.used_pct = row["used"].as_f64();
        w.resets_at = Some(ts(row["resets_at"].as_str().unwrap()));
        w.window_secs = row["window_secs"].as_u64();
        let p = window_pace(&w, now).expect("pace");
        let want = row["elapsed"].as_f64().unwrap();
        assert!(
            (p.elapsed_pct - want).abs() < 1e-9,
            "{row}: {}",
            p.elapsed_pct
        );
        assert_eq!(p.label(), row["label"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn window_pace_needs_a_share_a_reset_and_a_length() {
    let mut w = QuotaWindow::new("month", "month", WindowScope::Account);
    w.used_pct = Some(40.0);
    assert_eq!(window_pace(&w, 0), None, "no reset, no length");
    w.resets_at = Some(Timestamp(100));
    assert_eq!(window_pace(&w, 0), None, "no length");
    w.window_secs = Some(100);
    w.used_pct = None;
    assert_eq!(window_pace(&w, 0), None, "no share");
}

#[test]
fn format_money_puts_the_sign_outside_the_symbol_and_rounds_half_away() {
    let cases = [
        ("13.67", "USD", "$13.67"),
        ("-5.71", "USD", "-$5.71"),
        ("5.7", "usd", "$5.70"),
        ("0.005", "USD", "$0.01"),
        ("0.004", "USD", "$0.00"),
        ("-0.004", "USD", "-$0.00"),
        ("9.995", "USD", "$10.00"),
        ("0.5", "EUR", "€0.50"),
        ("12", "GBP", "£12.00"),
        ("1132.60", "CNY", "¥1132.60"),
        ("12", "XYZ", "12.00 XYZ"),
        ("-3.5", "abc", "-3.50 ABC"),
        (
            "123456789012345678901234.125",
            "USD",
            "$123456789012345678901234.13",
        ),
    ];
    for (amount, currency, want) in cases {
        let a = Amount::parse(amount).unwrap();
        assert_eq!(format_money(&a, currency), want, "{amount} {currency}");
    }
}

fn window(id: &str, used: Option<f64>, resets: Option<i64>) -> QuotaWindow {
    let mut w = QuotaWindow::new(id, id, WindowScope::Shared);
    w.used_pct = used;
    w.resets_at = resets.map(Timestamp);
    w
}

#[test]
fn lead_window_prefers_the_session_window() {
    let ws = [
        window("weekly", Some(99.0), Some(500)),
        window("session", Some(10.0), Some(900)),
    ];
    assert_eq!(lead_window(&ws, 0).unwrap().id, "session");
}

#[test]
fn lead_window_without_a_session_takes_the_worst_critical_window() {
    let mut exhausted = window("month", Some(91.0), None);
    exhausted.exhausted = true;
    let ws = [
        window("weekly", Some(95.0), Some(10)),
        window("weekly:opus", Some(97.0), Some(900)),
        window("30d", Some(20.0), Some(5)),
    ];
    assert_eq!(lead_window(&ws, 0).unwrap().id, "weekly:opus");
    let ws = [ws[0].clone(), exhausted];
    assert_eq!(
        lead_window(&ws, 0).unwrap().id,
        "month",
        "an exhausted window outranks a higher share"
    );
}

#[test]
fn lead_window_with_nothing_critical_takes_the_soonest_future_reset() {
    let ws = [
        window("weekly", Some(40.0), Some(900)),
        window("30d", Some(20.0), Some(300)),
        window("lapsed", Some(20.0), Some(50)),
        window("undated", Some(20.0), None),
    ];
    assert_eq!(lead_window(&ws, 100).unwrap().id, "30d");
    let undated = [window("a", Some(1.0), None), window("b", Some(2.0), None)];
    assert_eq!(
        lead_window(&undated, 0).unwrap().id,
        "a",
        "falls back to the first"
    );
    assert!(lead_window(&[], 0).is_none());
}

fn meter(kind: MoneyKind, amount: &str, currency: &str, limit: Option<&str>) -> MoneyMeter {
    let mut m = MoneyMeter::new(
        "m",
        "m",
        kind,
        Amount::parse(amount).unwrap(),
        currency,
        MoneyScope::Key,
    );
    m.limit = limit.map(|l| Amount::parse(l).unwrap());
    m
}

#[test]
fn meter_severity_grades_balances_and_caps_but_never_spend() {
    assert_eq!(
        meter_severity(&meter(MoneyKind::Balance, "3.10", "USD", None)),
        Some((Severity::High, SeverityBasis::Balance))
    );
    assert_eq!(
        meter_severity(&meter(MoneyKind::Balance, "3.10", "CNY", None)),
        None
    );
    // Key cap: $41 of $50 used → 82% → HIGH.
    assert_eq!(
        meter_severity(&meter(MoneyKind::Limit, "9", "USD", Some("50"))),
        Some((Severity::High, SeverityBasis::Usage))
    );
    // Nothing left under the cap is critical whatever the cap.
    assert_eq!(
        meter_severity(&meter(MoneyKind::Limit, "0", "USD", None)),
        Some((Severity::Critical, SeverityBasis::Usage))
    );
    assert_eq!(
        meter_severity(&meter(MoneyKind::Limit, "-1.5", "USD", Some("10"))),
        Some((Severity::Critical, SeverityBasis::Usage))
    );
    assert_eq!(
        meter_severity(&meter(MoneyKind::Spend, "900", "USD", Some("10"))),
        None
    );
    assert_eq!(
        meter_severity(&meter(MoneyKind::Budget, "0", "USD", Some("10"))),
        None
    );
}

#[test]
fn account_severity_is_the_worst_of_windows_money_and_an_exhausted_failure() {
    let mut obs = AccountObservation::new(
        "claude:w".to_string(),
        SourceId::AnthropicOauth,
        AuthKind::Subscription,
        Origin::Profile,
        "w",
    );
    assert_eq!(account_severity(&obs, 0, false), None, "nothing graded");
    obs.windows.push(window("session", Some(55.0), None));
    assert_eq!(account_severity(&obs, 0, false), Some(Severity::Mid));
    obs.money.push(meter(MoneyKind::Balance, "4", "USD", None));
    assert_eq!(account_severity(&obs, 0, false), Some(Severity::High));
    obs.failure = Some(Failure::new(FailureKind::QuotaExhausted, "balance too low"));
    assert_eq!(account_severity(&obs, 0, false), Some(Severity::Critical));
}

#[test]
fn account_severity_folds_pace_only_when_asked() {
    let mut obs = AccountObservation::new(
        "claude:w".to_string(),
        SourceId::AnthropicOauth,
        AuthKind::Subscription,
        Origin::Profile,
        "w",
    );
    // 30% used, 10% elapsed: +20 pts → pace critical, share ok.
    let mut w = window("session", Some(30.0), Some(1_000 + 16_200));
    w.window_secs = Some(18_000);
    obs.windows.push(w);
    assert_eq!(account_severity(&obs, 1_000, false), Some(Severity::Ok));
    assert_eq!(
        account_severity(&obs, 1_000, true),
        Some(Severity::Critical)
    );
}
