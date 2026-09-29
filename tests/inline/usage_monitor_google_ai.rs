#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::source::{FakeHttp, HttpReply, resolve_target};
use super::*;
use crate::testutil::HomeSandbox;
fn fetch(status: u16, body: &str) -> (Reading, Vec<String>) {
    let home = HomeSandbox::new();
    let mut cfg = super::super::config::MonitorConfig::new("gemini", MonitorKind::GoogleAi);
    cfg.api_key_env = Some("GEMINI_TEST_KEY".into());
    let target = resolve_target(&cfg, home.home(), 1000, &|_| Some("KEY-CANARY".into()));
    let body = body.to_string();
    let http = FakeHttp {
        send_reply: Box::new(move |_, _| {
            Ok(HttpReply {
                status,
                body: body.clone(),
                headers: vec![],
                retry_after_secs: None,
            })
        }),
        ..FakeHttp::offline()
    };
    (GoogleAiSource.fetch(&target, &http).unwrap(), http.calls())
}
#[test]
fn gemini_200_is_valid_with_no_windows_or_money() {
    let (reading, _) = fetch(200, "{}");
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Valid);
    assert!(reading.windows.is_empty());
    assert!(reading.money.is_empty());
}
#[test]
fn gemini_key_is_a_header_never_a_query() {
    let (_, calls) = fetch(200, "{}");
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("models?pageSize=1 auth=google:KEY-CANARY"));
    assert!(!calls[0].contains("key="));
}
#[test]
fn gemini_400_invalid_403_blocked_402_out_of_credits_429_rate_limited() {
    for (status, state, kind) in [
        (400, KeyHealthState::Invalid, FailureKind::AuthRequired),
        (403, KeyHealthState::Blocked, FailureKind::AuthRequired),
        (
            402,
            KeyHealthState::OutOfCredits,
            FailureKind::QuotaExhausted,
        ),
        (429, KeyHealthState::Unknown, FailureKind::RateLimited),
    ] {
        let (reading, _) = fetch(
            status,
            r#"{"error":{"details":[{"reason":"API_KEY_INVALID"}]}}"#,
        );
        assert_eq!(reading.key_health.unwrap().state, state);
        assert_eq!(reading.verdict.unwrap().kind, kind);
    }
}
#[test]
fn gemini_note_is_always_present() {
    for status in [200, 400, 403, 402, 429, 500] {
        assert_eq!(fetch(status, "{}").0.note.as_deref(), Some(NOTE));
    }
}
#[test]
fn gemini_retry_info_delay_is_used() {
    let (reading, _) = fetch(
        429,
        r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"1.5s"}]}}"#,
    );
    assert_eq!(reading.verdict.unwrap().retry_after.unwrap().secs(), 1002);
}

#[test]
fn gemini_other_400_keeps_health_unknown() {
    let (reading, _) = fetch(400, r#"{"error":{"details":[{"reason":"OTHER_ERROR"}]}}"#);
    assert_eq!(reading.key_health.unwrap().state, KeyHealthState::Unknown);
    assert_eq!(reading.verdict.unwrap().kind, FailureKind::Unavailable);
}
