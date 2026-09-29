#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::monitor::alert::evaluate`: a crossing into HIGH / CRITICAL notifies
//! once per window, `alert_pct` once per window, and nothing repeats.

use super::*;
use crate::usage::monitor::config::MonitorKind;
use crate::usage::observation::{AuthKind, Origin, QuotaWindow, SourceId, WindowScope};

const NOW: i64 = 1_790_000_000;

fn obs(pct: f64, resets_in: i64) -> AccountObservation {
    let mut o = AccountObservation::new(
        "monitor:m".into(),
        SourceId::Nous,
        AuthKind::NativeLogin,
        Origin::Monitor,
        "m",
    );
    let mut w = QuotaWindow::new("subscription", "Monthly credits", WindowScope::Account);
    w.used_pct = Some(pct);
    w.resets_at = Some(Timestamp::from_secs(NOW + resets_in));
    o.windows.push(w);
    o
}

fn cfg() -> MonitorConfig {
    let mut c = MonitorConfig::new("m", MonitorKind::Nous);
    c.label = Some("Nous".into());
    c
}

#[test]
fn nothing_below_high() {
    let (n, s) = evaluate(&cfg(), &obs(60.0, 3600), &AlertState::default(), NOW);
    assert!(n.is_empty());
    assert_eq!(s.last_severity, Some(Severity::Mid));
}

#[test]
fn crossing_into_high_then_critical_notifies_each_once() {
    let (n, s) = evaluate(&cfg(), &obs(80.0, 3600), &AlertState::default(), NOW);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].summary, "tollgate: Nous HIGH");
    assert!(
        n[0].body.starts_with("Monthly credits 80% used"),
        "{}",
        n[0].body
    );
    assert!(!n[0].critical);
    let (again, s) = evaluate(&cfg(), &obs(82.0, 3600), &s, NOW + 100);
    assert!(again.is_empty(), "still HIGH: no repeat");
    let (crit, s) = evaluate(&cfg(), &obs(95.0, 3600), &s, NOW + 200);
    assert_eq!(crit.len(), 1);
    assert!(crit[0].critical);
    // A dip and a rise inside the same window stays quiet.
    let (_, s) = evaluate(&cfg(), &obs(50.0, 3600), &s, NOW + 300);
    let (rise, s) = evaluate(&cfg(), &obs(96.0, 3600), &s, NOW + 400);
    assert!(rise.is_empty(), "same window, already notified");
    // The next window may notify again.
    let (next, _) = evaluate(&cfg(), &obs(97.0, 90_000), &s, NOW + 500);
    assert_eq!(next.len(), 1);
}

#[test]
fn alert_pct_notifies_once_per_window() {
    let mut c = cfg();
    c.alert_pct = Some(40.0);
    let (n, s) = evaluate(&c, &obs(45.0, 3600), &AlertState::default(), NOW);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].summary, "tollgate: Nous past 40%");
    let (again, _) = evaluate(&c, &obs(48.0, 3600), &s, NOW + 60);
    assert!(again.is_empty());
}

#[test]
fn the_sent_keys_are_bounded() {
    let mut s = AlertState::default();
    for i in 0..(MAX_SENT_KEYS as i64 + 10) {
        let (_, next) = evaluate(
            &cfg(),
            &obs(99.0, 3600 + i * 100_000),
            &AlertState {
                sent: s.sent.clone(),
                ..AlertState::default()
            },
            NOW,
        );
        s = next;
    }
    assert_eq!(s.sent.len(), MAX_SENT_KEYS);
}

#[test]
fn month_anchor_is_utc_year_month() {
    let t = Timestamp::parse("2026-09-30T23:59:59Z").unwrap().secs();
    assert_eq!(month_anchor(t), "2026-09");
    assert_eq!(month_anchor(t + 1), "2026-10");
}
