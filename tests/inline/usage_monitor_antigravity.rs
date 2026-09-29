#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use serde_json::json;
struct Fake {
    owner: bool,
    locked: usize,
    unlocked: usize,
    blob: Vec<u8>,
}
impl KeyringProbe for Fake {
    fn has_owner(&self) -> Result<bool, Failure> {
        Ok(self.owner)
    }
    fn metadata(&self) -> Result<KeyringMetadata, Failure> {
        assert!(self.owner);
        Ok(KeyringMetadata {
            locked: self.locked,
            unlocked: self.unlocked,
        })
    }
    fn secret(&self) -> Result<Zeroizing<Vec<u8>>, Failure> {
        assert_eq!(self.locked, 0);
        assert_eq!(self.unlocked, 1);
        Ok(Zeroizing::new(self.blob.clone()))
    }
}
#[test]
fn agy_no_owner_locked_and_multiple_never_read_secret() {
    for (owner, locked, unlocked) in [(false, 0, 0), (true, 1, 0), (true, 0, 2)] {
        let fake = Fake {
            owner,
            locked,
            unlocked,
            blob: vec![],
        };
        assert!(read_token(&fake, 100).is_err());
    }
}
#[test]
fn agy_blob_accepts_nested_flat_and_base64_prefix() {
    use base64::Engine;
    for body in [
        json!({"token":{"access_token":"ACCESS","expiry":2000000000,"refresh_token":"REFRESH-CANARY"}}),
        json!({"access_token":"ACCESS","expiresAt":2000000000000_i64}),
    ] {
        let bytes = body.to_string().into_bytes();
        assert_eq!(parse_blob(&bytes, 100).unwrap().expose(), "ACCESS");
        let encoded = format!(
            "go-keyring-base64:{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        assert_eq!(
            parse_blob(encoded.as_bytes(), 100).unwrap().expose(),
            "ACCESS"
        );
    }
}
#[test]
fn agy_missing_expiry_and_under_60s_fail_closed() {
    for expiry in [Value::Null, json!(160)] {
        assert_eq!(
            parse_blob(
                json!({"access_token":"ACCESS","expiry":expiry})
                    .to_string()
                    .as_bytes(),
                100
            )
            .unwrap_err()
            .kind,
            FailureKind::AuthRequired
        );
    }
}
#[test]
fn agy_accepts_response_envelope_and_bare_groups() {
    let value = json!({"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0.25,"resetTime":2000000000}]}]});
    for body in [value.clone(), json!({"response":value})] {
        let r = map_summary(&body).unwrap();
        assert_eq!(r.windows[0].used_pct, Some(75.));
        assert_eq!(r.windows[0].window_secs, Some(18000));
        assert!(!r.windows[0].chain_eligible);
    }
}
#[test]
fn agy_no_groups_unavailable() {
    assert_eq!(
        map_summary(&json!({"groups":[]})).unwrap_err().kind,
        FailureKind::Unavailable
    );
}
fn target(home: &std::path::Path) -> MonitorTarget {
    let cfg: super::super::config::MonitorConfig =
        toml::from_str("id='agy'\nkind='antigravity'\n").unwrap();
    super::super::source::resolve_target(&cfg, home, 100, &|_| None)
}
#[test]
fn agy_does_not_fall_through_hosts_on_auth_or_rate_limits() {
    let home = crate::testutil::HomeSandbox::new();
    for code in [401, 403, 429] {
        let mut http = super::super::source::FakeHttp::offline();
        http.send_reply = Box::new(move |_, req| {
            assert!(req.url.contains("daily-cloudcode"));
            Ok(HttpReply {
                retry_after_secs: None,
                status: code,
                body: "{}".into(),
                headers: vec![],
            })
        });
        let fake = Fake {
            owner: true,
            locked: 0,
            unlocked: 1,
            blob: json!({"access_token":"ACCESS","expiry":2000000000})
                .to_string()
                .into_bytes(),
        };
        let failure = fetch_with(&target(home.home()), &http, &fake).unwrap_err();
        assert_eq!(http.calls.lock().unwrap().len(), 1);
        if code == 429 {
            assert_eq!(failure.retry_after.unwrap().secs(), 1000);
        }
    }
}
#[test]
fn agy_tries_production_on_404_and_plan_once_per_day() {
    let home = crate::testutil::HomeSandbox::new();
    let mut http = super::super::source::FakeHttp::offline();
    http.send_reply = Box::new(|_, req| {
        let (status, body) = if req.url.contains("daily-cloudcode") {
            (404, json!({}))
        } else if req.url.ends_with("loadCodeAssist") {
            (200, json!({"response":{"paidTier":{"name":"Pro"}}}))
        } else {
            (
                200,
                json!({"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"x","window":"5h","remainingFraction":1}]}]}),
            )
        };
        Ok(HttpReply {
            retry_after_secs: None,
            status,
            body: body.to_string(),
            headers: vec![],
        })
    });
    let fake = Fake {
        owner: true,
        locked: 0,
        unlocked: 1,
        blob: json!({"access_token":"ACCESS","expiry":2000000000})
            .to_string()
            .into_bytes(),
    };
    let mut target = target(home.home());
    let r = fetch_with(&target, &http, &fake).unwrap();
    assert_eq!(r.plan.as_deref(), Some("Pro"));
    assert_eq!(http.calls.lock().unwrap().len(), 3);
    target.previous = Some(r);
    http.calls.lock().unwrap().clear();
    let r = fetch_with(&target, &http, &fake).unwrap();
    assert_eq!(http.calls.lock().unwrap().len(), 2);
    assert_eq!(r.plan.as_deref(), Some("Pro"));
}
#[test]
fn agy_shape_fixture_maps() {
    let body: Value =
        serde_json::from_str(include_str!("../fixtures/monitors/agy-summary.json")).unwrap();
    assert_eq!(map_summary(&body).unwrap().windows[0].used_pct, Some(75.));
}
#[test]
fn agy_malformed_expiry_does_not_fall_back_to_jwt() {
    use base64::Engine;
    let jwt = format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"exp":2000000000}).to_string())
    );
    assert_eq!(
        parse_blob(
            json!({"access_token":jwt,"expiry":"not-a-date"})
                .to_string()
                .as_bytes(),
            100
        )
        .unwrap_err()
        .kind,
        FailureKind::AuthRequired
    );
}
#[test]
fn agy_blob_over_one_mib_is_refused() {
    assert_eq!(
        parse_blob(&vec![b' '; 1024 * 1024 + 1], 100)
            .unwrap_err()
            .kind,
        FailureKind::Unavailable
    );
}
