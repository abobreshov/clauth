#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::monitor::poll`: the due set, the synchronous poll body, and the
//! tick entry point staying idle without monitors.

use super::*;
use crate::providers::{StatRow, StatRowKind, ThirdPartyStats};
use crate::testutil::HomeSandbox;
use crate::usage::monitor::alert::RecordingNotifier;
use crate::usage::monitor::config::MonitorKind;
use crate::usage::monitor::source::FakeHttp;

fn or(id: &str, enabled: bool) -> MonitorConfig {
    let mut m = MonitorConfig::new(id, MonitorKind::OpenRouter);
    m.api_key_env = Some("K".into());
    m.enabled = enabled;
    m
}

#[test]
fn only_enabled_due_monitors_are_polled() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();
    let http = FakeHttp::stats(|| {
        Ok(ThirdPartyStats {
            is_available: true,
            rows: vec![StatRow {
                label: crate::providers::DEEPSEEK_BALANCE_ROW_LABEL.into(),
                value: "3.00 USD".into(),
                kind: StatRowKind::Body,
            }],
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
        })
    });
    let notes = RecordingNotifier::default();
    let deps = RefreshDeps {
        http: &http,
        notifier: Some(&notes),
        env: &|_| Some("k".into()),
        now_ms: now,
    };
    let all = vec![or("a", true), or("b", false), or("c", true)];
    let due = due_monitors(all.clone(), now);
    assert_eq!(
        due.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["a", "c"]
    );
    poll_due(&due, &deps);
    assert_eq!(http.calls().len(), 2);
    assert!(
        due_monitors(all, now + 1000).is_empty(),
        "fresh caches are not due"
    );
    // $3 left is LOW on the balance ladder: one notification per monitor,
    // worded as a balance.
    let sent = notes.sent();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[0].summary.ends_with(" LOW"), "{}", sent[0].summary);
    assert!(sent[0].body.contains("$3.00 left"), "{}", sent[0].body);
}

#[test]
fn the_tick_entry_point_is_idle_without_monitors() {
    let _home = HomeSandbox::new();
    let before = crate::testutil::pending_background_tasks();
    LAST_SCAN_MS.store(0, std::sync::atomic::Ordering::Relaxed);
    poll_detached();
    assert_eq!(crate::testutil::pending_background_tasks(), before);
    assert!(
        !crate::usage::monitor::config::monitors_dir()
            .unwrap()
            .exists(),
        "an idle poll writes nothing"
    );
}
