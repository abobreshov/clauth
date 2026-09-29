#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::monitor::source`: target resolution (which key, never printed),
//! the monitoring allowlist, the provider-backed source over a fake HTTP, and
//! the hermetic guard on the live HTTP.

use super::*;
use crate::providers::{StatRow, StatRowKind};
use crate::usage::observation::MoneyKind;

const NOW: i64 = 1_790_000_000;

fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_string())
    }
}

fn openrouter(api: Option<&str>, billing: Option<&str>) -> MonitorConfig {
    let mut m = MonitorConfig::new("or", MonitorKind::OpenRouter);
    m.api_key_env = api.map(str::to_string);
    m.billing_key_env = billing.map(str::to_string);
    m
}

#[test]
fn the_billing_key_wins_when_it_is_set() {
    let env = env_of(&[("API", "sk-api-value"), ("MGMT", "sk-mgmt-value")]);
    let t = resolve_target(
        &openrouter(Some("API"), Some("MGMT")),
        std::path::Path::new("/h"),
        NOW,
        &env,
    );
    assert_eq!(t.key.as_ref().unwrap().expose(), "sk-mgmt-value");
    assert_eq!(t.key_env.as_deref(), Some("MGMT"));
    assert!(t.monitoring_key);
}

#[test]
fn an_unset_billing_key_falls_back_to_the_api_key() {
    let env = env_of(&[("API", "sk-api-value")]);
    let t = resolve_target(
        &openrouter(Some("API"), Some("MGMT")),
        std::path::Path::new("/h"),
        NOW,
        &env,
    );
    assert_eq!(t.key.as_ref().unwrap().expose(), "sk-api-value");
    assert_eq!(t.key_env.as_deref(), Some("API"));
    assert!(!t.monitoring_key);
}

#[test]
fn a_target_never_prints_its_key() {
    let env = env_of(&[("API", "sk-super-secret-value")]);
    let t = resolve_target(
        &openrouter(Some("API"), None),
        std::path::Path::new("/h"),
        NOW,
        &env,
    );
    let debug = format!("{t:?}");
    assert!(!debug.contains("sk-super-secret-value"), "{debug}");
    assert!(debug.contains("[redacted]"), "{debug}");
}

#[test]
fn the_allowlist_admits_only_the_nous_account_and_billing_reads() {
    for ok in [
        "https://portal.nousresearch.com/api/oauth/account",
        "https://portal.nousresearch.com/api/billing/state",
        "https://portal.nousresearch.com/api/billing/subscription?x=1",
    ] {
        assert!(bearer_url_allowed(ok), "{ok}");
    }
    for bad in [
        "https://inference-api.nousresearch.com/v1/chat/completions",
        "https://portal.nousresearch.com/api/oauth/token",
        "https://portal.nousresearch.com/api/billing/",
        "https://portal.nousresearch.com.evil.tld/api/oauth/account",
        "https://portal.nousresearch.com@evil.tld/api/oauth/account",
        "https://portal.nousresearch.com:8443/api/oauth/account",
        "https://portal.nousresearch.com/api/billing/../oauth/token",
        "http://portal.nousresearch.com/api/oauth/account",
        "https://openrouter.ai/api/v1/credits",
    ] {
        assert!(!bearer_url_allowed(bad), "{bad}");
    }
}

#[test]
fn live_http_refuses_an_off_list_url_before_sending() {
    let err = LiveHttp
        .get_bearer(
            "https://inference-api.nousresearch.com/v1/models",
            &Secret::new("tok"),
        )
        .unwrap_err();
    assert_eq!(err.kind, FailureKind::Unavailable);
    assert!(err.message.contains("allowlist"), "{}", err.message);
}

#[test]
#[should_panic(expected = "reached the real network")]
fn live_http_panics_in_tests_instead_of_sending() {
    let _ = LiveHttp.get_bearer(
        "https://portal.nousresearch.com/api/oauth/account",
        &Secret::new("tok"),
    );
}

fn wallet_stats() -> ThirdPartyStats {
    let row = |label: &str, value: &str| StatRow {
        label: label.to_string(),
        value: value.to_string(),
        kind: StatRowKind::Body,
    };
    ThirdPartyStats {
        is_available: true,
        rows: vec![
            row(crate::providers::DEEPSEEK_BALANCE_ROW_LABEL, "12.50 USD"),
            row("this month", "3.20 USD"),
        ],
        bars: Vec::new(),
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

fn target_for(cfg: &MonitorConfig, env: &dyn Fn(&str) -> Option<String>) -> MonitorTarget {
    resolve_target(cfg, std::path::Path::new("/h"), NOW, env)
}

#[test]
fn the_provider_source_maps_a_fetch_and_sends_the_resolved_key() {
    let http = FakeHttp::stats(|| Ok(wallet_stats()));
    let env = env_of(&[("API", "sk-api-value")]);
    let cfg = openrouter(Some("API"), None);
    let target = target_for(&cfg, &env);
    let reading = source_for(cfg.kind).fetch(&target, &http).unwrap();
    assert_eq!(
        http.calls(),
        ["PROVIDER https://openrouter.ai key=sk-api-value"]
    );
    let wallet = reading
        .money
        .iter()
        .find(|m| m.meter_id == "wallet")
        .unwrap();
    assert_eq!(wallet.kind, MoneyKind::Balance);
    assert_eq!(wallet.amount.as_str(), "12.50");
    assert_eq!(wallet.scope_origin, ScopeOrigin::Provider);
    assert!(reading.money.iter().any(|m| m.meter_id == "spend.monthly"));
    assert!(reading.verdict.is_none());
    assert_eq!(
        source_for(cfg.kind).source_id(&target),
        SourceId::OpenRouter
    );
    assert_eq!(source_for(cfg.kind).auth_kind(&target), AuthKind::ApiKey);
}

#[test]
fn a_monitoring_key_marks_its_meters_unbound() {
    let http = FakeHttp::stats(|| Ok(wallet_stats()));
    let env = env_of(&[("MGMT", "sk-mgmt")]);
    let cfg = openrouter(None, Some("MGMT"));
    let target = target_for(&cfg, &env);
    let reading = source_for(cfg.kind).fetch(&target, &http).unwrap();
    assert!(
        reading
            .money
            .iter()
            .all(|m| m.scope_origin == ScopeOrigin::MonitoringCredential { bound: false })
    );
    assert_eq!(source_for(cfg.kind).auth_kind(&target), AuthKind::ReadOnly);
}

#[test]
fn a_missing_env_var_is_auth_required_with_no_request() {
    let http = FakeHttp::offline();
    let cfg = openrouter(Some("OPENROUTER_API_KEY"), None);
    let target = target_for(&cfg, &|_| None);
    let err = source_for(cfg.kind).fetch(&target, &http).unwrap_err();
    assert_eq!(err.kind, FailureKind::AuthRequired);
    assert!(
        err.message.contains("$OPENROUTER_API_KEY"),
        "{}",
        err.message
    );
    assert!(http.calls().is_empty());
}

#[test]
fn provider_errors_become_typed_failures() {
    let env = env_of(&[("API", "k")]);
    let cfg = openrouter(Some("API"), None);
    let target = target_for(&cfg, &env);
    let cases: Vec<(ThirdPartyError, FailureKind)> = vec![
        (ThirdPartyError::AuthExpired, FailureKind::AuthRequired),
        (ThirdPartyError::Network, FailureKind::Unavailable),
        (ThirdPartyError::Status, FailureKind::Unavailable),
        (ThirdPartyError::Parse, FailureKind::InvalidResponse),
        (ThirdPartyError::ConsoleExpired, FailureKind::ConsoleExpired),
    ];
    for (e, want) in cases {
        assert_eq!(third_party_failure(e, &target).kind, want);
    }
    let limited = third_party_failure(
        ThirdPartyError::RateLimited {
            retry_after: Some(Duration::from_secs(600)),
        },
        &target,
    );
    assert_eq!(limited.kind, FailureKind::RateLimited);
    assert_eq!(limited.retry_after, Some(Timestamp::from_secs(NOW + 600)));
}

#[test]
fn an_unfunded_account_rides_as_a_verdict() {
    let http = FakeHttp::stats(|| {
        let mut s = wallet_stats();
        s.is_available = false;
        Ok(s)
    });
    let env = env_of(&[("API", "k")]);
    let cfg = openrouter(Some("API"), None);
    let reading = source_for(cfg.kind)
        .fetch(&target_for(&cfg, &env), &http)
        .unwrap();
    assert_eq!(
        reading.verdict.map(|f| f.kind),
        Some(FailureKind::QuotaExhausted)
    );
}

#[test]
fn ollama_cloud_reports_not_built_without_a_request() {
    let http = FakeHttp::offline();
    let mut cfg = MonitorConfig::new("oc", MonitorKind::OllamaCloud);
    cfg.api_key_env = Some("OLLAMA_API_KEY".into());
    let env = env_of(&[("OLLAMA_API_KEY", "k")]);
    let err = source_for(cfg.kind)
        .fetch(&target_for(&cfg, &env), &http)
        .unwrap_err();
    assert_eq!(err.kind, FailureKind::Unavailable);
    assert!(err.message.contains("not built"));
    assert!(http.calls().is_empty());
}

#[test]
fn a_generic_provider_monitor_reads_its_named_provider() {
    let http = FakeHttp::stats(|| Ok(wallet_stats()));
    let mut cfg = MonitorConfig::new("ds", MonitorKind::Provider);
    cfg.provider = Some("deepseek".into());
    cfg.api_key_env = Some("DS".into());
    let env = env_of(&[("DS", "sk-ds")]);
    let target = target_for(&cfg, &env);
    source_for(cfg.kind).fetch(&target, &http).unwrap();
    assert_eq!(
        http.calls(),
        ["PROVIDER https://api.deepseek.com key=sk-ds"]
    );
    assert_eq!(source_for(cfg.kind).source_id(&target), SourceId::DeepSeek);
}
