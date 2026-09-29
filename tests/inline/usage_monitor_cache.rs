#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::monitor::cache`: TTL, the 429 hold, stale retention across a
//! failure, single flight, target identity, owner-only files, alerts through
//! the refresh, and that no credential ever reaches disk.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::providers::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats};
use crate::testutil::HomeSandbox;
use crate::usage::monitor::alert::RecordingNotifier;
use crate::usage::monitor::config::MonitorKind;
use crate::usage::monitor::source::FakeHttp;

const T0: u64 = 1_790_000_000_000;
const SECRET: &str = "sk-or-v1-THIS-MUST-NEVER-HIT-DISK-0123456789";

fn env(name: &str) -> Option<String> {
    (name == "OR_KEY").then(|| SECRET.to_string())
}

fn or_monitor() -> MonitorConfig {
    let mut m = MonitorConfig::new("or", MonitorKind::OpenRouter);
    m.api_key_env = Some("OR_KEY".into());
    m
}

fn stats(balance: &str) -> ThirdPartyStats {
    ThirdPartyStats {
        is_available: true,
        rows: vec![StatRow {
            label: crate::providers::DEEPSEEK_BALANCE_ROW_LABEL.into(),
            value: format!("{balance} USD"),
            kind: StatRowKind::Body,
        }],
        bars: Vec::new(),
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

fn deps<'a>(http: &'a FakeHttp, now_ms: u64) -> RefreshDeps<'a> {
    RefreshDeps {
        http,
        notifier: None,
        env: &env,
        now_ms,
    }
}

fn refreshed(o: RefreshOutcome) -> MonitorCache {
    match o {
        RefreshOutcome::Refreshed(c) => *c,
        other => panic!("expected a refresh, got {other:?}"),
    }
}

#[test]
fn a_refresh_writes_an_owner_only_cache_and_then_waits_out_its_ttl() {
    let _home = HomeSandbox::new();
    let m = or_monitor();
    let http = FakeHttp::stats(|| Ok(stats("12.50")));
    let c = refreshed(refresh_one(&m, &deps(&http, T0), false).unwrap());
    assert_eq!(c.observed_at_ms, Some(T0));
    assert_eq!(c.checked_at_ms, Some(T0));
    assert!(c.failure.is_none());
    assert_eq!(
        load("or").unwrap(),
        c,
        "what was returned is what is on disk"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cache_path("or").unwrap()), 0o600);
        assert_eq!(mode(&monitors_dir().unwrap()), 0o700);
    }

    // Inside the 90 s TTL: not due, no request.
    let o = refresh_one(&m, &deps(&http, T0 + 89_000), false).unwrap();
    assert!(matches!(o, RefreshOutcome::NotDue(_)), "{o:?}");
    assert_eq!(http.calls().len(), 1);
    // `force` (monitor refresh) ignores the TTL.
    refreshed(refresh_one(&m, &deps(&http, T0 + 89_000), true).unwrap());
    // Past the TTL: due again.
    refreshed(refresh_one(&m, &deps(&http, T0 + 89_000 + 90_000), false).unwrap());
    assert_eq!(http.calls().len(), 3);
}

#[test]
fn is_due_follows_ttl_hold_and_identity() {
    let m = or_monitor();
    let mut c = MonitorCache::empty(&m);
    assert!(is_due(&m, None, T0));
    c.checked_at_ms = Some(T0);
    assert!(!is_due(&m, Some(&c), T0 + 1000));
    assert!(is_due(&m, Some(&c), T0 + 90_000));
    assert!(
        is_due(&m, Some(&c), T0 - 5000),
        "a clock step back is not a TTL"
    );
    c.hold_until_ms = Some(T0 + 600_000);
    assert!(!is_due(&m, Some(&c), T0 + 300_000), "held");
    let mut other = m.clone();
    other.api_key_env = Some("ANOTHER".into());
    assert!(
        is_due(&other, Some(&c), T0 + 1000),
        "another target's cache does not count"
    );
}

#[test]
fn a_failure_keeps_the_last_reading_and_a_success_clears_it() {
    let _home = HomeSandbox::new();
    let m = or_monitor();
    let n = AtomicUsize::new(0);
    let http = FakeHttp::stats(move || match n.fetch_add(1, Ordering::SeqCst) {
        0 => Ok(stats("12.50")),
        1 => Err(ThirdPartyError::Network),
        _ => Ok(stats("11.00")),
    });
    refreshed(refresh_one(&m, &deps(&http, T0), true).unwrap());
    let failed = refreshed(refresh_one(&m, &deps(&http, T0 + 100_000), true).unwrap());
    assert_eq!(
        failed.failure.as_ref().unwrap().kind,
        FailureKind::Unavailable
    );
    assert_eq!(
        failed.observed_at_ms,
        Some(T0),
        "the last good reading is kept"
    );
    assert_eq!(failed.checked_at_ms, Some(T0 + 100_000));
    assert_eq!(
        failed.reading.as_ref().unwrap().money[0].amount.as_str(),
        "12.50"
    );
    let ok = refreshed(refresh_one(&m, &deps(&http, T0 + 200_000), true).unwrap());
    assert!(ok.failure.is_none());
    assert_eq!(ok.reading.unwrap().money[0].amount.as_str(), "11.00");
}

#[test]
fn a_429_holds_the_monitor_for_at_least_five_minutes() {
    let _home = HomeSandbox::new();
    let m = or_monitor();
    let http = FakeHttp::stats(|| Err(ThirdPartyError::RateLimited { retry_after: None }));
    let c = refreshed(refresh_one(&m, &deps(&http, T0), false).unwrap());
    assert_eq!(c.failure.as_ref().unwrap().kind, FailureKind::RateLimited);
    assert_eq!(c.hold_until_ms, Some(T0 + RATE_LIMIT_HOLD_MS));
    // Held: even a forced refresh sends nothing.
    let o = refresh_one(&m, &deps(&http, T0 + 200_000), true).unwrap();
    assert!(matches!(o, RefreshOutcome::Held(_)), "{o:?}");
    assert!(!is_due(&m, load("or").as_ref(), T0 + 200_000));
    assert_eq!(http.calls().len(), 1);
    // After the hold: due again.
    assert!(is_due(&m, load("or").as_ref(), T0 + RATE_LIMIT_HOLD_MS));
}

#[test]
fn a_longer_retry_after_extends_the_hold() {
    let _home = HomeSandbox::new();
    let m = or_monitor();
    let http = FakeHttp::stats(|| {
        Err(ThirdPartyError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(1800)),
        })
    });
    let c = refreshed(refresh_one(&m, &deps(&http, T0), false).unwrap());
    assert_eq!(c.hold_until_ms, Some(T0 + 1_800_000));
}

#[test]
fn a_held_lock_means_busy_and_no_request() {
    let _home = HomeSandbox::new();
    let m = or_monitor();
    let dir = monitors_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    let other = crate::profile::open_state_file(&dir.join("or.lock")).unwrap();
    other.lock().unwrap();
    let http = FakeHttp::offline();
    let o = refresh_one(&m, &deps(&http, T0), true).unwrap();
    assert!(matches!(o, RefreshOutcome::Busy), "{o:?}");
    drop(other);
    let http = FakeHttp::stats(|| Ok(stats("1")));
    refreshed(refresh_one(&m, &deps(&http, T0), true).unwrap());
}

#[test]
fn a_changed_target_discards_the_old_cache() {
    let _home = HomeSandbox::new();
    let m = or_monitor();
    let http = FakeHttp::stats(|| Ok(stats("12.50")));
    refreshed(refresh_one(&m, &deps(&http, T0), false).unwrap());
    let mut moved = m.clone();
    moved.billing_key_env = Some("OR_MGMT".into());
    let fail = FakeHttp::stats(|| Err(ThirdPartyError::Network));
    let c = refreshed(refresh_one(&moved, &deps(&fail, T0 + 1000), false).unwrap());
    assert!(
        c.reading.is_none(),
        "the old target's figures are not carried over"
    );
    assert_eq!(c.fingerprint, moved.fingerprint());
}

#[test]
fn no_credential_ever_reaches_disk() {
    let home = HomeSandbox::new();
    let m = or_monitor();
    crate::usage::monitor::config::add(&m).unwrap();
    let http = FakeHttp::stats(|| Err(ThirdPartyError::AuthExpired));
    refreshed(refresh_one(&m, &deps(&http, T0), false).unwrap());
    assert_eq!(
        http.calls(),
        [format!("PROVIDER https://openrouter.ai key={SECRET}")]
    );
    let mut stack = vec![home.home().to_path_buf()];
    let mut files = 0;
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                files += 1;
                let bytes = std::fs::read(&p).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains("THIS-MUST-NEVER"),
                    "{} holds the key",
                    p.display()
                );
            }
        }
    }
    assert!(files >= 2, "monitors.toml and the cache were both scanned");
}

#[test]
fn a_refresh_notifies_once_per_crossing_and_window() {
    let _home = HomeSandbox::new();
    let mut m = or_monitor();
    m.alert_pct = Some(50.0);
    m.budget_usd_month = crate::usage::observation::Amount::parse("10");
    let n = AtomicUsize::new(0);
    let http = FakeHttp::stats(move || {
        let v = match n.fetch_add(1, Ordering::SeqCst) {
            0 => "2.00",
            1 => "6.00",
            2 => "6.50",
            _ => "9.50",
        };
        let mut s = stats("100");
        s.rows.push(StatRow {
            label: "this month".into(),
            value: format!("{v} USD"),
            kind: StatRowKind::Body,
        });
        Ok(s)
    });
    let notes = RecordingNotifier::default();
    let run = |now: u64| {
        let d = RefreshDeps {
            http: &http,
            notifier: Some(&notes),
            env: &env,
            now_ms: now,
        };
        refreshed(refresh_one(&m, &d, true).unwrap())
    };
    run(T0);
    assert!(notes.sent().is_empty(), "20% of budget: nothing to say");
    run(T0 + 100_000);
    let sent = notes.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].summary.contains("past 50%"), "{}", sent[0].summary);
    run(T0 + 200_000);
    assert_eq!(
        notes.sent().len(),
        1,
        "still past 50% this month: no repeat"
    );
    let c = run(T0 + 300_000);
    let sent = notes.sent();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent[1].summary.contains("CRITICAL"),
        "95% of the budget: {}",
        sent[1].summary
    );
    assert_eq!(c.alerts.sent.len(), 2);
    assert_eq!(
        load("or").unwrap().alerts,
        c.alerts,
        "the de-dup state is persisted"
    );
}

#[test]
fn remove_deletes_the_cache_and_lock() {
    let _home = HomeSandbox::new();
    let http = FakeHttp::stats(|| Ok(stats("1")));
    refreshed(refresh_one(&or_monitor(), &deps(&http, T0), false).unwrap());
    assert!(cache_path("or").unwrap().exists());
    remove("or");
    assert!(!cache_path("or").unwrap().exists());
    assert!(load("or").is_none());
}
