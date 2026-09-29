#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::monitor::observe`: cache → observation, the 7-day retention, the
//! budget meter and its severity, and the collector hook end to end.

use super::*;
use crate::codex_profiles::CodexState;
use crate::testutil::HomeSandbox;
use crate::usage::collect::{CollectOpts, MONITOR_SOURCES, collect_with};
use crate::usage::monitor::cache::{MonitorCache, RefreshDeps, refresh_one};
use crate::usage::monitor::config::MonitorKind;
use crate::usage::monitor::source::{FakeHttp, Reading};
use crate::usage::observation::{
    AuthKind, Failure, FailureKind, Origin, QuotaWindow, SourceId, WindowScope,
};

const DAY_MS: u64 = 86_400_000;
const NOW_MS: u64 = 1_790_000_000_000;

fn amount(s: &str) -> Amount {
    Amount::parse(s).unwrap()
}

fn fresh(_: Option<u64>) -> Freshness {
    Freshness::Fresh
}

fn spend_monthly(v: &str) -> MoneyMeter {
    let mut m = MoneyMeter::new(
        "spend.monthly",
        "Spend this month",
        MoneyKind::Spend,
        amount(v),
        "USD",
        MoneyScope::Key,
    );
    m.period = Some(Period::of(PeriodKind::Monthly));
    m
}

fn nous_pool(limit: &str, left: &str) -> MoneyMeter {
    let mut m = MoneyMeter::new(
        "subscription",
        "Subscription credits",
        MoneyKind::Balance,
        amount(left),
        "USD",
        MoneyScope::Profile,
    );
    m.limit = Some(amount(limit));
    m.period = Some(Period::of(PeriodKind::Monthly));
    m
}

fn with_budget(b: &str) -> MonitorConfig {
    let mut c = MonitorConfig::new("m", MonitorKind::Nous);
    c.budget_usd_month = Some(amount(b));
    c
}

#[test]
fn sub_exact_is_exact() {
    assert_eq!(
        sub_exact(&amount("20"), &amount("3.20")).unwrap().as_str(),
        "16.80"
    );
    assert_eq!(
        sub_exact(&amount("1.5"), &amount("0.25")).unwrap().as_str(),
        "1.25"
    );
    assert_eq!(
        sub_exact(&amount("0"), &amount("3.2")).unwrap().as_str(),
        "-3.2"
    );
    assert_eq!(
        sub_exact(&amount("0.1"), &amount("0.000000001"))
            .unwrap()
            .as_str(),
        "0.099999999"
    );
}

#[test]
fn a_budget_measures_the_monthly_spend_meter() {
    let m = budget_meter(&with_budget("20"), &[spend_monthly("3.20")]).unwrap();
    assert_eq!(m.meter_id, BUDGET_METER);
    assert_eq!(m.kind, MoneyKind::Budget);
    assert_eq!(m.amount.as_str(), "16.80");
    assert_eq!(m.limit.as_ref().unwrap().as_str(), "20");
    assert_eq!(m.scope_origin, ScopeOrigin::UserLabel);
    assert!(!m.additive);
    assert_eq!(budget_severity(&m), Some(Severity::Ok));
}

#[test]
fn a_budget_measures_a_monthly_pool_when_no_spend_meter_exists() {
    // Nous: 22 credits, 7.90 left → 14.10 spent.
    let m = budget_meter(&with_budget("20"), &[nous_pool("22", "7.90")]).unwrap();
    assert_eq!(m.amount.as_str(), "5.90");
    assert!((budget_spent_pct(&m).unwrap() - 70.5).abs() < 1e-9);
    assert_eq!(budget_severity(&m), Some(Severity::Mid));
    let over = budget_meter(&with_budget("10"), &[nous_pool("22", "7.90")]).unwrap();
    assert_eq!(over.amount.as_str(), "-4.10");
    assert_eq!(budget_severity(&over), Some(Severity::Critical));
}

#[test]
fn no_budget_or_no_spend_figure_means_no_budget_meter() {
    assert!(
        budget_meter(
            &MonitorConfig::new("m", MonitorKind::Nous),
            &[spend_monthly("1")]
        )
        .is_none()
    );
    let wallet = MoneyMeter::new(
        "wallet",
        "Balance",
        MoneyKind::Balance,
        amount("5"),
        "USD",
        MoneyScope::Profile,
    );
    assert!(budget_meter(&with_budget("20"), &[wallet]).is_none());
    let mut cny = spend_monthly("3");
    cny.currency = "CNY".into();
    assert!(
        budget_meter(&with_budget("20"), &[cny]).is_none(),
        "USD only"
    );
}

fn window(pct: f64, resets_in_secs: i64) -> QuotaWindow {
    let mut w = QuotaWindow::new("subscription", "Monthly credits", WindowScope::Account);
    w.used_pct = Some(pct);
    w.resets_at = Some(Timestamp::from_secs(
        (NOW_MS / 1000) as i64 + resets_in_secs,
    ));
    w
}

fn cache_with(reading: Reading, observed_ms: u64, failure: Option<Failure>) -> MonitorCache {
    let cfg = MonitorConfig::new("m", MonitorKind::Nous);
    MonitorCache {
        version: crate::usage::monitor::cache::CACHE_VERSION,
        id: "m".into(),
        fingerprint: cfg.fingerprint(),
        checked_at_ms: Some(NOW_MS - 1000),
        observed_at_ms: Some(observed_ms),
        reading: Some(reading),
        failure,
        hold_until_ms: None,
        alerts: Default::default(),
    }
}

#[test]
fn no_cache_is_an_unfetched_skeleton() {
    let mut cfg = MonitorConfig::new("m", MonitorKind::Nous);
    cfg.enabled = false;
    cfg.label = Some("Nous".into());
    let obs = observe_monitor(&cfg, None, NOW_MS, fresh);
    assert_eq!(obs.id, "monitor:m");
    assert_eq!(obs.origin, Origin::Monitor);
    assert_eq!(obs.source, SourceId::Nous);
    assert_eq!(obs.auth, AuthKind::NativeLogin);
    assert_eq!(obs.label, "Nous");
    assert!(obs.disabled);
    assert_eq!(obs.freshness, Freshness::NotFetched);
    assert!(obs.windows.is_empty() && obs.money.is_empty());
}

#[test]
fn a_cached_reading_projects_and_a_later_failure_keeps_it_visible() {
    let reading = Reading {
        plan: Some("Plus".into()),
        windows: vec![window(64.0, 3600)],
        money: vec![nous_pool("22", "7.90")],
        ..Reading::default()
    };
    let cfg = MonitorConfig::new("m", MonitorKind::Nous);
    let ok = observe_monitor(
        &cfg,
        Some(&cache_with(reading.clone(), NOW_MS - 5000, None)),
        NOW_MS,
        fresh,
    );
    assert_eq!(ok.plan.as_deref(), Some("Plus"));
    assert_eq!(ok.windows.len(), 1);
    assert_eq!(ok.money.len(), 1);
    assert_eq!(ok.observed_at, Some(Timestamp::from_ms(NOW_MS - 5000)));
    assert_eq!(ok.checked_at, Some(Timestamp::from_ms(NOW_MS - 1000)));
    assert!(ok.failure.is_none());

    let failed = observe_monitor(
        &cfg,
        Some(&cache_with(
            reading,
            NOW_MS - 5000,
            Some(Failure::new(
                FailureKind::AuthRequired,
                "run hermes to refresh",
            )),
        )),
        NOW_MS,
        fresh,
    );
    assert_eq!(failed.windows.len(), 1, "stale figures stay visible");
    assert_eq!(failed.failure.unwrap().kind, FailureKind::AuthRequired);
}

#[test]
fn figures_past_seven_days_and_lapsed_windows_drop() {
    let reading = Reading {
        windows: vec![window(10.0, 3600), window(20.0, -60)],
        money: vec![nous_pool("22", "1")],
        ..Reading::default()
    };
    let cfg = MonitorConfig::new("m", MonitorKind::Nous);
    let current = observe_monitor(
        &cfg,
        Some(&cache_with(reading.clone(), NOW_MS - DAY_MS, None)),
        NOW_MS,
        fresh,
    );
    assert_eq!(current.windows.len(), 1, "the lapsed window is dropped");
    let old = observe_monitor(
        &cfg,
        Some(&cache_with(reading, NOW_MS - 8 * DAY_MS, None)),
        NOW_MS,
        fresh,
    );
    assert!(old.windows.is_empty() && old.money.is_empty());
    assert_eq!(old.freshness, Freshness::NotFetched);
    assert!(old.checked_at.is_some(), "the attempt is still dated");
}

#[test]
fn monitor_severity_folds_the_budget_in() {
    let reading = Reading {
        windows: vec![window(10.0, 3600)],
        money: vec![nous_pool("22", "7.90")],
        ..Reading::default()
    };
    let obs = observe_monitor(
        &with_budget("10"),
        Some(&cache_with(reading, NOW_MS, None)),
        NOW_MS,
        fresh,
    );
    assert!(obs.money.iter().any(|m| m.meter_id == BUDGET_METER));
    let now_secs = (NOW_MS / 1000) as i64;
    assert_eq!(
        crate::usage::derive::account_severity(&obs, now_secs, false),
        Some(Severity::Mid),
        "the core ladder grades the $7.90 balance, not the budget"
    );
    assert_eq!(monitor_severity(&obs, now_secs), Some(Severity::Critical));
}

// ── the collector hook ────────────────────────────────────────────────────────

fn ctx<'a>(codex: &'a CodexState, now_ms: u64, include_disabled: bool) -> CollectCtx<'a> {
    CollectCtx {
        config: None,
        codex,
        now_ms,
        interval_ms: crate::profile::AppState::default().refresh_interval_ms,
        guest_mode: false,
        include_disabled,
    }
}

fn tree(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn the_collector_reads_monitors_from_their_caches_and_writes_nothing() {
    let home = HomeSandbox::new();
    let hermes = home.home().join("hermes");
    std::fs::create_dir_all(&hermes).unwrap();
    std::fs::write(
        hermes.join("auth.json"),
        r#"{"providers":{"nous":{"access_token":"at","expires_at":"2099-01-01T00:00:00Z"}}}"#,
    )
    .unwrap();
    let mut nous = MonitorConfig::new("nous", MonitorKind::Nous);
    nous.hermes_home = Some(hermes.to_string_lossy().into_owned());
    nous.budget_usd_month = Some(amount("20"));
    let mut off = MonitorConfig::new("off", MonitorKind::Nous);
    off.enabled = false;
    crate::usage::monitor::config::add(&nous).unwrap();
    crate::usage::monitor::config::add(&off).unwrap();

    let now = crate::usage::now_ms();
    // The fixture's period end moved far out, so the window stays live
    // whatever the wall clock reads.
    let body = include_str!("../fixtures/nous_account.json")
        .replace("2026-10-15T00:00:00Z", "2099-10-15T00:00:00Z");
    let http = FakeHttp::bearer(200, &body);
    let deps = RefreshDeps {
        http: &http,
        notifier: None,
        env: &|_| None,
        now_ms: now,
    };
    refresh_one(&nous, &deps, false).unwrap();

    let before = tree(home.home());
    let codex = CodexState::default();
    let got = collect_with(
        &ctx(&codex, now, false),
        &CollectOpts::default(),
        MONITOR_SOURCES,
        &[],
    );
    assert_eq!(tree(home.home()), before, "the hook writes nothing");
    assert_eq!(got.len(), 1, "the disabled monitor is hidden");
    let obs = &got[0];
    assert_eq!(obs.id, "monitor:nous");
    assert_eq!(obs.freshness, Freshness::Fresh);
    assert_eq!(format!("{:.2}", obs.windows[0].used_pct.unwrap()), "64.09");
    let budget = obs
        .money
        .iter()
        .find(|m| m.meter_id == BUDGET_METER)
        .unwrap();
    assert_eq!(budget.amount.as_str(), "5.90");

    let all = collect_with(
        &ctx(&codex, now, true),
        &CollectOpts::default(),
        MONITOR_SOURCES,
        &[],
    );
    assert_eq!(
        all.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["monitor:nous", "monitor:off"]
    );
    assert!(all[1].disabled);

    let by_provider = collect_with(
        &ctx(&codex, now, false),
        &CollectOpts {
            provider: Some("nous".into()),
            ..CollectOpts::default()
        },
        MONITOR_SOURCES,
        &[],
    );
    assert_eq!(by_provider.len(), 1, "--provider nous finds the monitor");
}

#[test]
fn a_cache_for_another_target_is_ignored() {
    let _home = HomeSandbox::new();
    let mut m = MonitorConfig::new("m", MonitorKind::Nous);
    m.hermes_home = Some("/one".into());
    crate::usage::monitor::config::add(&m).unwrap();
    let dir = crate::usage::monitor::config::monitors_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    let mut stale = cache_with(
        Reading {
            windows: vec![window(50.0, 3600)],
            ..Reading::default()
        },
        crate::usage::now_ms(),
        None,
    );
    stale.fingerprint = "nous||||/other".into();
    std::fs::write(dir.join("m.json"), serde_json::to_vec(&stale).unwrap()).unwrap();
    let codex = CodexState::default();
    let got = monitor_observations(&ctx(&codex, crate::usage::now_ms(), false));
    assert_eq!(got.len(), 1);
    assert!(got[0].windows.is_empty());
    assert_eq!(got[0].freshness, Freshness::NotFetched);
}

#[test]
fn an_invalid_monitors_toml_yields_no_monitors() {
    let _home = HomeSandbox::new();
    let path = crate::usage::monitor::config::monitors_path().unwrap();
    crate::profile::mkdir_700(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "[[monitor]]\nid = \"Bad Id\"\nkind = \"nous\"\n").unwrap();
    let codex = CodexState::default();
    assert!(monitor_observations(&ctx(&codex, crate::usage::now_ms(), true)).is_empty());
}
