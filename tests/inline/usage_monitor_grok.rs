#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use serde_json::json;
fn stage(home: &Path, value: Value) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("auth.json"), value.to_string()).unwrap();
}
#[test]
fn grok_epoch_seconds_and_millis_both_parse() {
    let sandbox = crate::testutil::HomeSandbox::new();
    for expiry in [
        json!(2000000000),
        json!(2000000000000_i64),
        json!("2033-05-18T03:33:20Z"),
    ] {
        stage(
            sandbox.home(),
            json!({"https://auth.x.ai::test":{"key":"ACCESS","expires_at":expiry,"refresh_token":"REFRESH-CANARY"}}),
        );
        let token = read_token(sandbox.home(), None, 1900000000).unwrap();
        assert_eq!(token.expose(), "ACCESS");
        assert!(!format!("{:?}", read_entries(sandbox.home()).unwrap()).contains("REFRESH-CANARY"));
        assert!(!sandbox.home().join("auth.json.lock").exists());
    }
}
#[test]
fn grok_expired_and_undated_tokens_fail_closed() {
    let sandbox = crate::testutil::HomeSandbox::new();
    for expiry in [json!(100), Value::Null] {
        stage(
            sandbox.home(),
            json!({"https://auth.x.ai::test":{"key":"ACCESS","expires_at":expiry}}),
        );
        assert_eq!(
            read_token(sandbox.home(), None, 1000).unwrap_err().kind,
            FailureKind::AuthRequired
        );
    }
}
#[test]
fn grok_two_official_logins_need_auth_entry() {
    let sandbox = crate::testutil::HomeSandbox::new();
    stage(
        sandbox.home(),
        json!({"https://auth.x.ai::a":{"key":"a","expires_at":2000000000},"https://accounts.x.ai/sign-in::b":{"key":"b","expires_at":2000000000}}),
    );
    assert!(read_token(sandbox.home(), None, 100).is_err());
    assert_eq!(
        read_token(sandbox.home(), Some("https://auth.x.ai::a"), 100)
            .unwrap()
            .expose(),
        "a"
    );
}
#[test]
fn grok_garbage_auth_is_unavailable() {
    let sandbox = crate::testutil::HomeSandbox::new();
    std::fs::write(sandbox.home().join("auth.json"), "{").unwrap();
    assert_eq!(
        read_entries(sandbox.home()).unwrap_err().kind,
        FailureKind::Unavailable
    );
}
#[test]
fn grok_weekly_maps_shared_attribution_and_hides_money() {
    let reading=map_billing(&json!({"config":{"creditUsagePercent":123.0,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":2000000000,"end":2000604800},"productUsage":[{"product":"Chat","usagePercent":123.0}],"prepaidBalance":{"val":10000}}}),&json!({"subscriptionTier":"Super"}),None).unwrap();
    assert_eq!(reading.windows.len(), 1);
    let w = &reading.windows[0];
    assert_eq!(w.used_pct, Some(123.));
    assert_eq!(w.window_secs, Some(604800));
    assert_eq!(w.attribution[0].label, "Chat");
    assert!(!w.chain_eligible);
    assert!(reading.money.is_empty());
    assert_eq!(reading.plan.as_deref(), Some("Super"));
}
#[test]
fn grok_missing_percent_is_unknown_not_zero() {
    let r = map_billing(&json!({"config":{"currentPeriod":{}}}), &json!({}), None).unwrap();
    assert_eq!(r.windows[0].used_pct, None);
}
fn jwt(exp: i64) -> String {
    use base64::Engine;
    format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"exp":exp}).to_string())
    )
}
#[test]
fn grok_jwt_exp_is_used_when_expiry_absent() {
    let home = crate::testutil::HomeSandbox::new();
    stage(
        home.home(),
        json!({"https://auth.x.ai::test":{"key":jwt(2000000000)}}),
    );
    assert!(read_token(home.home(), None, 100).is_ok());
}
#[cfg(unix)]
#[test]
fn grok_never_modifies_auth_lock() {
    use std::os::unix::fs::MetadataExt;
    let home = crate::testutil::HomeSandbox::new();
    stage(
        home.home(),
        json!({"https://auth.x.ai::test":{"key":"ACCESS","expires_at":2000000000}}),
    );
    let lock = home.home().join("auth.json.lock");
    std::fs::write(&lock, "CANARY").unwrap();
    let before = std::fs::metadata(&lock).unwrap();
    read_token(home.home(), None, 100).unwrap();
    let after = std::fs::metadata(&lock).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.len()),
        (after.ino(), after.mtime(), after.len())
    );
}
#[test]
fn grok_settings_only_when_user_lacks_tier() {
    let home = crate::testutil::HomeSandbox::new();
    let tool = home.home().join(".grok");
    stage(
        &tool,
        json!({"https://auth.x.ai::test":{"key":"ACCESS","expires_at":2000000000}}),
    );
    for tier in [true, false] {
        let cfg: super::super::config::MonitorConfig =
            toml::from_str("id='grok'\nkind='grok'\n").unwrap();
        let target = super::super::source::resolve_target(&cfg, home.home(), 100, &|_| None);
        let mut http = super::super::source::FakeHttp::offline();
        http.send_reply = Box::new(move |_, req| {
            let body = if req.url.contains("/billing") {
                json!({"config":{"creditUsagePercent":12}})
            } else if req.url.contains("/user") {
                if tier {
                    json!({"subscriptionTier":"Pro"})
                } else {
                    json!({})
                }
            } else {
                json!({"subscription_tier_display":"Fallback"})
            };
            Ok(HttpReply {
                retry_after_secs: None,
                status: 200,
                body: body.to_string(),
                headers: vec![],
            })
        });
        let r = GrokSource.fetch(&target, &http).unwrap();
        assert_eq!(
            r.plan.as_deref(),
            Some(if tier { "Pro" } else { "Fallback" })
        );
        assert_eq!(http.calls.lock().unwrap().len(), if tier { 2 } else { 3 });
    }
}
#[test]
fn grok_401_maps_to_open_grok() {
    let f = status(
        &HttpReply {
            retry_after_secs: None,
            status: 401,
            body: "{}".into(),
            headers: vec![],
        },
        100,
    )
    .unwrap_err();
    assert_eq!(f.kind, FailureKind::AuthRequired);
    assert_eq!(f.message, "open grok to refresh its login");
}
#[test]
fn grok_shape_fixture_maps() {
    let body: Value =
        serde_json::from_str(include_str!("../fixtures/monitors/grok-billing.json")).unwrap();
    assert_eq!(
        map_billing(&body, &json!({}), None).unwrap().windows[0].used_pct,
        Some(123.)
    );
}
#[test]
fn grok_malformed_expiry_does_not_fall_back_to_jwt() {
    let home = crate::testutil::HomeSandbox::new();
    stage(
        home.home(),
        json!({"https://auth.x.ai::test":{"key":jwt(2000000000),"expires_at":"not-a-date"}}),
    );
    assert_eq!(
        read_token(home.home(), None, 100).unwrap_err().kind,
        FailureKind::AuthRequired
    );
}

#[test]
fn grok_null_expiry_does_not_fall_back_to_jwt() {
    let home = crate::testutil::HomeSandbox::new();
    stage(
        home.home(),
        json!({"https://auth.x.ai::test":{"key":jwt(2000000000),"expires_at":null}}),
    );
    assert_eq!(
        read_token(home.home(), None, 100).unwrap_err().kind,
        FailureKind::AuthRequired
    );
}
