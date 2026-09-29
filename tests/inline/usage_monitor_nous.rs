#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::monitor::nous`: the account mapping (hand-built fixture shaped
//! like `/api/oauth/account`), the used-share guard, and the borrowed Hermes
//! token — read only while unexpired, never written, and an expired one
//! costs no request.

use super::*;
use crate::usage::monitor::config::{MonitorConfig, MonitorKind};
use crate::usage::monitor::source::{FakeHttp, resolve_target, source_for};

const ACCOUNT: &str = include_str!("../fixtures/nous_account.json");
/// 2026-09-29T12:00:00Z.
fn now() -> i64 {
    Timestamp::parse("2026-09-29T12:00:00Z").unwrap().secs()
}

#[test]
fn the_fixture_maps_to_a_monthly_window_and_exact_meters() {
    let r = map_account(ACCOUNT).unwrap();
    assert_eq!(r.plan.as_deref(), Some("Plus"));
    assert_eq!(r.windows.len(), 1);
    let w = &r.windows[0];
    assert_eq!(w.id, WINDOW_SUBSCRIPTION);
    assert_eq!(w.label, "Monthly credits");
    assert!(!w.chain_eligible);
    assert_eq!(w.window_secs, None);
    // (22 − 7.90) / 22 × 100 = 64.0909…
    let pct = w.used_pct.unwrap();
    assert_eq!(format!("{pct:.2}"), "64.09");
    assert_eq!(w.resets_at, Timestamp::parse("2026-10-15T00:00:00Z"));
    assert!(!w.exhausted);

    let meter = |id: &str| r.money.iter().find(|m| m.meter_id == id).unwrap();
    let sub = meter("subscription");
    assert_eq!(sub.kind, MoneyKind::Balance);
    assert_eq!(
        sub.amount.as_str(),
        "7.90",
        "exact decimal, trailing zero kept"
    );
    assert_eq!(sub.limit.as_ref().unwrap().as_str(), "22");
    assert_eq!(sub.currency, "USD");
    let period = sub.period.unwrap();
    assert_eq!(period.kind, PeriodKind::Monthly);
    assert_eq!(period.end, Timestamp::parse("2026-10-15T00:00:00Z"));
    assert_eq!(period.start, Timestamp::parse("2026-09-15T00:00:00Z"));
    assert!(period.derived);
    assert!(sub.additive);
    assert_eq!(meter("top_up").amount.as_str(), "5.25");
    assert!(meter("top_up").additive);
    assert_eq!(meter("rollover").amount.as_str(), "0.00");
    assert!(!meter("rollover").additive);
    assert_eq!(meter("total_usable").amount.as_str(), "13.15");
    assert!(
        !meter("total_usable").additive,
        "a derived total is never summed"
    );
    assert!(r.verdict.is_none());
}

#[test]
fn the_used_share_guard_matches_hermes() {
    let a = |s: &str| Amount::parse(s).unwrap();
    assert_eq!(used_pct(Some(&a("22")), Some(&a("22"))), Some(0.0));
    assert_eq!(used_pct(Some(&a("0")), Some(&a("0"))), None, "no pool");
    assert_eq!(
        used_pct(Some(&a("22")), Some(&a("30"))),
        None,
        "above the pool"
    );
    assert_eq!(used_pct(None, Some(&a("1"))), None);
    // Debt reads above 100, unclamped.
    let debt = used_pct(Some(&a("20")), Some(&a("-2"))).unwrap();
    assert!((debt - 110.0).abs() < 1e-9, "{debt}");
}

#[test]
fn numbers_and_strings_both_parse_exactly() {
    let body = r#"{"subscription":{"monthly_credits":"110","credits_remaining":109.5,
        "current_period_end":1790000000},"paid_service_access":{"purchased_credits_remaining":0}}"#;
    let r = map_account(body).unwrap();
    assert_eq!(r.money[0].amount.as_str(), "109.5");
    assert_eq!(r.money[0].limit.as_ref().unwrap().as_str(), "110");
    assert_eq!(
        r.windows[0].resets_at,
        Some(Timestamp::from_secs(1_790_000_000))
    );
    assert!(r.plan.is_none());
}

#[test]
fn paid_access_off_is_a_depleted_verdict() {
    let body = r#"{"paid_service_access":{"paid_access":false,"total_usable_credits":"0"}}"#;
    let r = map_account(body).unwrap();
    assert!(r.windows.is_empty(), "no subscription, no window");
    assert_eq!(r.verdict.map(|f| f.kind), Some(FailureKind::QuotaExhausted));
}

#[test]
fn an_unknown_shape_is_invalid_response() {
    for body in ["not json", "[]", "{}", r#"{"email":"x"}"#] {
        assert_eq!(
            map_account(body).unwrap_err().kind,
            FailureKind::InvalidResponse,
            "{body}"
        );
    }
}

// ── the Hermes token ───────────────────────────────────────────────────────────

fn hermes_home(auth: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("auth.json"), auth).unwrap();
    dir
}

fn auth_json(token: &str, expires: &str) -> String {
    format!(
        r#"{{"version":1,"active_provider":"nous","providers":{{"nous":{{
            "access_token":"{token}","refresh_token":"rt-NEVER-READ","expires_at":{expires},
            "token_type":"Bearer"}}}},"credential_pool":{{"nous":[{{"id":"a","access_token":"pool"}}]}}}}"#
    )
}

#[test]
fn an_unexpired_token_is_read() {
    let home = hermes_home(&auth_json("at-live", "\"2026-09-29T13:00:00Z\""));
    assert_eq!(
        read_hermes_token(home.path(), now()).unwrap().expose(),
        "at-live"
    );
    // Epoch seconds and milliseconds are both accepted.
    let secs = hermes_home(&auth_json("at-s", &(now() + 600).to_string()));
    assert_eq!(
        read_hermes_token(secs.path(), now()).unwrap().expose(),
        "at-s"
    );
    let ms = hermes_home(&auth_json("at-ms", &((now() + 600) * 1000).to_string()));
    assert_eq!(
        read_hermes_token(ms.path(), now()).unwrap().expose(),
        "at-ms"
    );
}

#[test]
fn an_expired_or_undated_token_is_auth_required() {
    for expires in [
        "\"2026-09-29T11:00:00Z\"".to_string(),
        // Inside the skew: Hermes is about to replace it.
        (now() + 10).to_string(),
        "null".to_string(),
    ] {
        let home = hermes_home(&auth_json("at", &expires));
        let err = read_hermes_token(home.path(), now()).unwrap_err();
        assert_eq!(err.kind, FailureKind::AuthRequired, "{expires}");
        assert!(err.message.contains("hermes"), "{}", err.message);
        assert!(!err.message.contains("rt-NEVER-READ"));
    }
}

#[test]
fn a_missing_login_is_auth_required_and_a_torn_file_is_unavailable() {
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(
        read_hermes_token(empty.path(), now()).unwrap_err().kind,
        FailureKind::AuthRequired
    );
    let no_nous = hermes_home(r#"{"providers":{"openrouter":{}}}"#);
    assert_eq!(
        read_hermes_token(no_nous.path(), now()).unwrap_err().kind,
        FailureKind::AuthRequired
    );
    let torn = hermes_home(r#"{"providers":{"nous":{"access_tok"#);
    assert_eq!(
        read_hermes_token(torn.path(), now()).unwrap_err().kind,
        FailureKind::Unavailable
    );
}

fn nous_cfg(home: &std::path::Path) -> MonitorConfig {
    let mut m = MonitorConfig::new("nous", MonitorKind::Nous);
    m.hermes_home = Some(home.to_string_lossy().into_owned());
    m
}

#[test]
fn an_expired_token_costs_no_request() {
    let home = hermes_home(&auth_json("at-old", "\"2026-09-01T00:00:00Z\""));
    let cfg = nous_cfg(home.path());
    let target = resolve_target(&cfg, std::path::Path::new("/"), now(), &|_| None);
    let http = FakeHttp::offline();
    let err = source_for(cfg.kind).fetch(&target, &http).unwrap_err();
    assert_eq!(err.kind, FailureKind::AuthRequired);
    assert!(
        err.message.contains("run hermes to refresh"),
        "{}",
        err.message
    );
    assert!(http.calls().is_empty(), "no network for an expired token");
}

#[test]
fn a_live_token_reads_the_portal_account_and_leaves_auth_json_untouched() {
    let auth = auth_json("at-live", "\"2026-09-29T13:00:00Z\"");
    let home = hermes_home(&auth);
    let before = std::fs::metadata(home.path().join("auth.json"))
        .unwrap()
        .modified()
        .unwrap();
    let cfg = nous_cfg(home.path());
    let target = resolve_target(&cfg, std::path::Path::new("/"), now(), &|_| None);
    let http = FakeHttp::bearer(200, ACCOUNT);
    let reading = source_for(cfg.kind).fetch(&target, &http).unwrap();
    assert_eq!(
        http.calls(),
        ["GET https://portal.nousresearch.com/api/oauth/account bearer=at-live"]
    );
    assert_eq!(reading.windows.len(), 1);
    assert_eq!(
        std::fs::read_to_string(home.path().join("auth.json")).unwrap(),
        auth,
        "auth.json is never written"
    );
    let after = std::fs::metadata(home.path().join("auth.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        source_for(cfg.kind).auth_kind(&target),
        AuthKind::NativeLogin
    );
    assert_eq!(source_for(cfg.kind).source_id(&target), SourceId::Nous);
}

#[test]
fn portal_statuses_become_typed_failures() {
    let home = hermes_home(&auth_json("at", "\"2026-09-29T13:00:00Z\""));
    let cfg = nous_cfg(home.path());
    let target = resolve_target(&cfg, std::path::Path::new("/"), now(), &|_| None);
    for (status, want) in [
        (401, FailureKind::AuthRequired),
        (403, FailureKind::AuthRequired),
        (500, FailureKind::Unavailable),
        (429, FailureKind::RateLimited),
    ] {
        let http = FakeHttp::bearer(status, "{}");
        let err = source_for(cfg.kind).fetch(&target, &http).unwrap_err();
        assert_eq!(err.kind, want, "{status}");
    }
    let limited = FakeHttp {
        bearer_reply: Box::new(|_| {
            Ok(HttpReply {
                status: 429,
                body: String::new(),
                retry_after_secs: Some(900),
            })
        }),
        ..FakeHttp::offline()
    };
    let err = source_for(cfg.kind).fetch(&target, &limited).unwrap_err();
    assert_eq!(err.retry_after, Some(Timestamp::from_secs(now() + 900)));
}

#[test]
fn an_api_key_account_reports_money_unavailable_without_a_request() {
    let mut cfg = MonitorConfig::new("nous-key", MonitorKind::Nous);
    cfg.api_key_env = Some("NOUS_API_KEY".into());
    let target = resolve_target(&cfg, std::path::Path::new("/"), now(), &|_| {
        Some("sk-nous-x".to_string())
    });
    let http = FakeHttp::offline();
    let r = source_for(cfg.kind).fetch(&target, &http).unwrap();
    assert!(r.money.is_empty() && r.windows.is_empty());
    let v = r.verdict.unwrap();
    assert_eq!(v.kind, FailureKind::Unavailable);
    assert!(v.message.contains("no balance endpoint"));
    assert!(http.calls().is_empty());
    assert_eq!(source_for(cfg.kind).auth_kind(&target), AuthKind::ApiKey);
}
