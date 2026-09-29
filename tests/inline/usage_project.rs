#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::project`: today's caches projected onto the observation model.
//! Every input is cache BYTES in the shape the scheduler writes (hand-built
//! for OAuth / codex / OpenRouter, the shared `testutil` captures for the
//! DeepSeek and bar shapes) parsed through the production types, so a reader
//! drift reds here.

use super::*;
use crate::testutil::{
    CAPTURED_TWO_WALLET_DS_CACHE, DEEPSEEK_CACHE_BYTES, DEEPSEEK_UNFUNDED_CACHE_BYTES,
    THIRD_PARTY_BARS_CACHE_BYTES,
};
use crate::usage::observation::{AuthKind, Origin, PeriodKind, ScopeOrigin, SourceId};

/// 2026-09-29T10:00:00Z.
const NOW: i64 = 1_790_676_000;

fn at(s: &str) -> i64 {
    Timestamp::parse(s).unwrap().secs()
}

fn amt(s: &str) -> Amount {
    Amount::parse(s).unwrap()
}

fn skeleton(source: SourceId) -> AccountObservation {
    AccountObservation::new(
        "claude:t".to_string(),
        source,
        AuthKind::Subscription,
        Origin::Profile,
        "t",
    )
}

/// A Max 5x account's `usage_cache.json`: both shared windows, two per-model
/// weekly windows (one already lapsed), a visible `spend` block, the legacy
/// `extra_usage` object and one `window_dollars` row.
const OAUTH_CACHE: &str = r#"{
  "plan": {"tier": {"Max": 5}, "subscription_status": "active"},
  "five_hour": {"utilization": 42.5, "resets_at": "2026-09-29T12:30:00+00:00"},
  "seven_day": {"utilization": 101.0, "resets_at": "2026-10-02T00:00:00Z"},
  "weekly_scoped": [
    {"label": "7d opus", "utilization": 61.0, "resets_at": "2026-10-02T00:00:00Z"},
    {"label": "7d fable", "utilization": 99.0, "resets_at": "2026-09-28T00:00:00Z"}
  ],
  "window_dollars": [{"label": "5h", "used": 1.25, "limit": 40.0}],
  "extra_usage": {"is_enabled": true, "monthly_limit": 5000.0, "used_credits": 1234.0, "currency": "USD"},
  "spend": {"enabled": true, "used": 12.34, "limit": 50.0, "percent": 24.68, "currency": "USD"},
  "fetched_at": 1790675400000
}"#;

#[test]
fn oauth_cache_projects_windows_with_ids_scopes_and_chain_flags() {
    let usage: UsageInfo = serde_json::from_str(OAUTH_CACHE).unwrap();
    let ws = usage_windows(&usage, NOW);
    let ids: Vec<&str> = ws.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        ["session", "weekly", "weekly:opus"],
        "the lapsed fable window drops"
    );

    let session = &ws[0];
    assert_eq!(session.label, "5h");
    assert_eq!(session.used_pct, Some(42.5));
    assert!(!session.exhausted);
    assert_eq!(
        session.resets_at.unwrap().secs(),
        at("2026-09-29T12:30:00Z")
    );
    assert_eq!(session.window_secs, Some(18_000));
    assert_eq!(session.scope, WindowScope::Shared);
    assert!(session.chain_eligible);

    let weekly = &ws[1];
    assert_eq!(weekly.used_pct, Some(101.0), "unclamped");
    assert!(weekly.exhausted);
    assert_eq!(weekly.window_secs, Some(604_800));
    assert!(weekly.chain_eligible);

    let opus = &ws[2];
    assert_eq!(opus.label, "7d opus");
    assert_eq!(
        opus.scope,
        WindowScope::Model {
            models: vec!["opus".to_string()]
        }
    );
    assert!(
        !opus.chain_eligible,
        "only the 5h / 7d fold is chain-eligible"
    );
}

#[test]
fn oauth_cache_projects_spend_and_window_dollars_exactly() {
    let usage: UsageInfo = serde_json::from_str(OAUTH_CACHE).unwrap();
    let money = usage_money(&usage);
    let ids: Vec<&str> = money.iter().map(|m| m.meter_id.as_str()).collect();
    assert_eq!(
        ids,
        ["spend.extra", "window.5h"],
        "the visible spend block wins over the legacy extra_usage bar"
    );
    let spend = &money[0];
    assert_eq!(spend.kind, MoneyKind::Spend);
    assert_eq!(spend.amount.as_str(), "12.34");
    assert_eq!(spend.limit, Some(amt("50")));
    assert_eq!(spend.currency, "USD");
    assert_eq!(spend.scope, MoneyScope::Organization);
    assert_eq!(spend.scope_origin, ScopeOrigin::Provider);
    assert_eq!(spend.period.unwrap().kind, PeriodKind::Monthly);
    assert_eq!(money[1].amount, amt("1.25"));
    assert_eq!(money[1].limit, Some(amt("40")));
}

#[test]
fn legacy_extra_usage_converts_cents_exactly_when_no_spend_block_shows() {
    let mut usage: UsageInfo = serde_json::from_str(OAUTH_CACHE).unwrap();
    usage.spend = None;
    usage.window_dollars.clear();
    let money = usage_money(&usage);
    assert_eq!(money.len(), 1);
    assert_eq!(money[0].meter_id, "spend.extra");
    assert_eq!(money[0].amount.as_str(), "12.34", "1234 cents");
    assert_eq!(
        money[0].limit.as_ref().unwrap().as_str(),
        "50.00",
        "5000 cents, shifted two places exactly"
    );

    usage.extra_usage.as_mut().unwrap().is_enabled = false;
    assert!(
        usage_money(&usage).is_empty(),
        "a disabled extra bar shows nothing"
    );
}

#[test]
fn extra_usage_period_breakdowns_ride_as_their_own_meters() {
    let mut usage: UsageInfo = serde_json::from_str(OAUTH_CACHE).unwrap();
    let extra = usage.extra_usage.as_mut().unwrap();
    extra.daily = Some(serde_json::json!({"used_credits": 0.75, "utilization": 3.0}));
    extra.weekly = Some(serde_json::json!(null));
    let ids: Vec<String> = usage_money(&usage)
        .into_iter()
        .map(|m| m.meter_id)
        .collect();
    assert!(ids.contains(&"spend.extra.daily".to_string()), "{ids:?}");
    assert!(
        !ids.contains(&"spend.extra.weekly".to_string()),
        "a null breakdown is absent"
    );
}

#[test]
fn apply_oauth_usage_sets_plan_and_flags_a_canceled_subscription() {
    let usage: UsageInfo = serde_json::from_str(OAUTH_CACHE).unwrap();
    let mut obs = skeleton(SourceId::AnthropicOauth);
    apply_oauth_usage(&mut obs, &usage, NOW);
    assert_eq!(obs.plan.as_deref(), Some("Max 5x"));
    assert_eq!(obs.windows.len(), 3);
    assert!(obs.failure.is_none());

    let mut canceled = usage.clone();
    canceled.plan.as_mut().unwrap().subscription_status = Some("canceled".to_string());
    let mut obs = skeleton(SourceId::AnthropicOauth);
    obs.plan = Some("Free".to_string());
    apply_oauth_usage(&mut obs, &canceled, NOW);
    assert_eq!(
        obs.plan.as_deref(),
        Some("Free"),
        "a producer-set plan is kept"
    );
    assert_eq!(
        obs.failure.as_ref().unwrap().kind,
        FailureKind::SubscriptionInactive
    );
}

/// A codex `usage_cache.json`: the plan in `codex_plan`, a blocked account, two
/// banked resets.
const CODEX_CACHE: &str = r#"{
  "plan": {"tier": "Unknown", "codex_plan": "plus"},
  "five_hour": {"utilization": 100.0, "resets_at": "2026-09-29T11:00:00+00:00"},
  "seven_day": {"utilization": 30.0, "resets_at": "2026-10-03T00:00:00+00:00"},
  "codex_limit_reached": "rate_limit_reached",
  "codex_reset_credits": 2
}"#;

#[test]
fn codex_cache_projects_windows_plan_resets_and_the_block() {
    let usage: UsageInfo = serde_json::from_str(CODEX_CACHE).unwrap();
    let mut obs = skeleton(SourceId::Codex);
    apply_codex_usage(&mut obs, &usage, NOW);
    assert_eq!(obs.plan.as_deref(), Some("plus"));
    assert_eq!(obs.banked_resets, Some(2));
    let ids: Vec<&str> = obs.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["session", "weekly"]);
    assert!(obs.windows[0].exhausted);
    assert!(obs.windows.iter().all(|w| w.chain_eligible));
    let failure = obs.failure.unwrap();
    assert_eq!(failure.kind, FailureKind::QuotaExhausted);
    assert_eq!(failure.message, "rate limit reached");
    assert!(obs.money.is_empty(), "codex publishes no money");
}

#[test]
fn deepseek_cache_projects_an_exact_wallet_and_non_additive_components() {
    let stats: ThirdPartyStats = serde_json::from_str(DEEPSEEK_CACHE_BYTES).unwrap();
    let money = third_party_money(&stats);
    let got: Vec<(&str, &str, &str, bool)> = money
        .iter()
        .map(|m| {
            (
                m.meter_id.as_str(),
                m.amount.as_str(),
                m.currency.as_str(),
                m.additive,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("wallet", "31.45", "CNY", true),
            ("wallet.granted", "0.00", "CNY", false),
            ("wallet.topped_up", "31.45", "CNY", false),
        ]
    );
    assert!(money.iter().all(|m| m.kind == MoneyKind::Balance));
    assert!(third_party_windows(&stats, NOW).is_empty());
}

#[test]
fn a_two_wallet_cache_keeps_both_currencies() {
    let stats: ThirdPartyStats = serde_json::from_str(CAPTURED_TWO_WALLET_DS_CACHE).unwrap();
    let wallets: Vec<(String, String)> = third_party_money(&stats)
        .into_iter()
        .filter(|m| m.meter_id == "wallet")
        .map(|m| (m.amount.to_string(), m.currency))
        .collect();
    assert_eq!(
        wallets,
        [
            ("0.00".to_string(), "USD".to_string()),
            ("498.18".to_string(), "CNY".to_string())
        ]
    );
}

#[test]
fn an_unfunded_cache_reports_quota_exhausted_beside_its_figures() {
    let stats: ThirdPartyStats = serde_json::from_str(DEEPSEEK_UNFUNDED_CACHE_BYTES).unwrap();
    let mut obs = skeleton(SourceId::DeepSeek);
    apply_third_party(&mut obs, &stats, NOW);
    assert_eq!(
        obs.failure.as_ref().unwrap().kind,
        FailureKind::QuotaExhausted
    );
    assert_eq!(obs.failure.as_ref().unwrap().message, "balance too low");
    assert_eq!(obs.meter("wallet").unwrap().amount, Amount::zero());
}

/// An OpenRouter `third_party_cache.json` in the exact row set `openrouter::stats`
/// writes, overdrawn: a negative wallet, lifetime used / purchased, the three
/// period rows and the key cap pair.
const OPENROUTER_CACHE: &str = r#"{"is_available":false,"rows":[
  {"label":"credits","value":"","kind":"heading"},
  {"label":"api balance","value":"-0.20 USD","kind":"danger"},
  {"label":"used","value":"50.20 USD","kind":"body"},
  {"label":"purchased","value":"50.00 USD","kind":"body"},
  {"label":"today","value":"0.00 USD","kind":"body"},
  {"label":"this week","value":"4.08 USD","kind":"body"},
  {"label":"this month","value":"4.46 USD","kind":"body"},
  {"label":"key limit","value":"50.00 USD","kind":"body"},
  {"label":"key limit left","value":"9.00 USD","kind":"body"},
  {"label":"free tier","value":"","kind":"faint"},
  {"label":"","value":"balance too low","kind":"danger"}
],"bars":[],"best_effort":false}"#;

#[test]
fn openrouter_cache_projects_a_negative_wallet_spend_periods_and_the_key_cap() {
    let stats: ThirdPartyStats = serde_json::from_str(OPENROUTER_CACHE).unwrap();
    let money = third_party_money(&stats);
    let ids: Vec<&str> = money.iter().map(|m| m.meter_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "wallet",
            "spend.daily",
            "spend.weekly",
            "spend.monthly",
            "spend.lifetime",
            "key_limit"
        ]
    );
    let wallet = &money[0];
    assert_eq!(wallet.amount.as_str(), "-0.20", "debt survives");
    assert!(wallet.amount.is_negative());
    let weekly = &money[2];
    assert_eq!(weekly.kind, MoneyKind::Spend);
    assert_eq!(weekly.scope, MoneyScope::Key);
    assert_eq!(weekly.period.unwrap().kind, PeriodKind::Weekly);
    assert_eq!(weekly.amount.as_str(), "4.08");
    let lifetime = &money[4];
    assert_eq!(lifetime.amount.as_str(), "50.20");
    assert_eq!(lifetime.limit, Some(amt("50")));
    assert_eq!(lifetime.period.unwrap().kind, PeriodKind::Lifetime);
    let cap = &money[5];
    assert_eq!(cap.kind, MoneyKind::Limit);
    assert_eq!(cap.amount.as_str(), "9.00", "amount is what is left");
    assert_eq!(cap.limit, Some(amt("50.00")));
}

#[test]
fn bar_cache_projects_windows_with_lengths_and_absolute_amounts() {
    let stats: ThirdPartyStats = serde_json::from_str(THIRD_PARTY_BARS_CACHE_BYTES).unwrap();
    let before = at("2026-08-14T00:00:00Z");
    let ws = third_party_windows(&stats, before);
    let got: Vec<(&str, &str, Option<u64>, bool)> = ws
        .iter()
        .map(|w| {
            (
                w.id.as_str(),
                w.label.as_str(),
                w.window_secs,
                w.chain_eligible,
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("session", "5h", Some(18_000), true),
            ("weekly", "7d", Some(604_800), true),
            ("30d", "30d", Some(2_592_000), false),
        ]
    );
    assert_eq!(ws[2].scope, WindowScope::Account);
    assert_eq!(ws[2].used, Some(123.4));
    assert_eq!(ws[2].limit, Some(4000.0));
    assert_eq!(ws[0].used_pct, Some(12.5));
    assert!(
        third_party_money(&stats).is_empty(),
        "token rows are not money"
    );

    let after_5h = at("2026-08-16T00:00:00Z");
    let ids: Vec<String> = third_party_windows(&stats, after_5h)
        .into_iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(ids, ["weekly", "30d"], "a lapsed bar drops");

    let mut obs = skeleton(SourceId::Zai);
    apply_third_party(&mut obs, &stats, before);
    assert_eq!(obs.plan.as_deref(), Some("pro"));
    assert!(obs.failure.is_none());
}

#[test]
fn best_effort_bars_never_become_chain_eligible() {
    let mut stats: ThirdPartyStats = serde_json::from_str(THIRD_PARTY_BARS_CACHE_BYTES).unwrap();
    stats.best_effort = true;
    let ws = third_party_windows(&stats, at("2026-08-14T00:00:00Z"));
    assert!(ws.iter().all(|w| !w.chain_eligible));
    let mut obs = skeleton(SourceId::Generic);
    apply_third_party(&mut obs, &stats, at("2026-08-14T00:00:00Z"));
    assert!(obs.best_effort);
}

#[test]
fn money_values_parse_only_the_amount_code_shape() {
    assert_eq!(
        parse_money_value("3640.55 CNY"),
        Some((amt("3640.55"), "CNY".to_string()))
    );
    assert_eq!(parse_money_value("-0.20 usd").unwrap().1, "USD");
    for bad in [
        "123.4M  (1.2k calls)",
        "12 / 100",
        "84 days",
        "",
        "5",
        "5 USD extra",
        "5 U5D",
    ] {
        assert!(parse_money_value(bad).is_none(), "{bad:?}");
    }
}

#[test]
fn unknown_money_rows_are_typed_by_their_label_or_skipped() {
    let stats: ThirdPartyStats = serde_json::from_str(
        r#"{"is_available":true,"rows":[
          {"label":"Credit Remaining","value":"7.5 USD","kind":"body"},
          {"label":"monthly cost","value":"1.25 USD","kind":"body"},
          {"label":"mystery","value":"9 USD","kind":"body"}
        ],"bars":[],"best_effort":true}"#,
    )
    .unwrap();
    let got: Vec<(String, MoneyKind)> = third_party_money(&stats)
        .into_iter()
        .map(|m| (m.meter_id, m.kind))
        .collect();
    assert_eq!(
        got,
        [
            ("row.credit_remaining".to_string(), MoneyKind::Balance),
            ("row.monthly_cost".to_string(), MoneyKind::Spend),
        ]
    );
}

#[test]
fn label_lengths() {
    assert_eq!(label_secs("5h"), Some(18_000));
    assert_eq!(label_secs("1w"), Some(604_800));
    assert_eq!(label_secs("90m"), Some(5_400));
    assert_eq!(label_secs("0d"), None);
    assert_eq!(label_secs("weekly"), None);
    assert_eq!(label_secs(""), None);
}
