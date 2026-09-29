//! Google AI Studio API keys expose health, without spend or quota.
use super::config::MonitorKind;
use super::source::{MonitorHttp, MonitorTarget, Reading, UsageSource};
use crate::usage::keyed_http::{Auth, Method, Request};
use crate::usage::observation::{
    AuthKind, Failure, FailureKind, KeyHealth, KeyHealthState, SourceId, Timestamp,
};
pub(crate) const NOTE: &str =
    "spend and quota not available for API keys (per project, shown only in Google AI Studio)";
pub(crate) struct GoogleAiSource;
impl UsageSource for GoogleAiSource {
    fn source_id(&self, _: &MonitorTarget) -> SourceId {
        SourceId::GoogleAi
    }
    fn auth_kind(&self, _: &MonitorTarget) -> AuthKind {
        AuthKind::ApiKey
    }
    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
        let Some(key) = &target.api_key else {
            return Ok(Reading {
                note: Some(NOTE.into()),
                key_health: Some(KeyHealth {
                    state: KeyHealthState::Unknown,
                    checked_at: Timestamp::from_secs(target.now_secs),
                }),
                verdict: Some(Failure::new(
                    FailureKind::AuthRequired,
                    "Google AI key missing; set the configured secret",
                )),
                ..Reading::default()
            });
        };
        let reply = http.send(
            MonitorKind::GoogleAi,
            &Request {
                method: Method::Get,
                url: "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1",
                auth: Auth::GoogApiKey(key),
                extra: &[],
                json_body: None,
            },
        )?;
        let invalid_key = serde_json::from_str::<serde_json::Value>(&reply.body)
            .ok()
            .is_some_and(|body| {
                body.pointer("/error/code").and_then(|v| v.as_str()) == Some("API_KEY_INVALID")
                    || body
                        .pointer("/error/details")
                        .and_then(|v| v.as_array())
                        .is_some_and(|details| {
                            details.iter().any(|d| {
                                d.get("reason").and_then(|v| v.as_str()) == Some("API_KEY_INVALID")
                            })
                        })
            });
        let (state, verdict) = match reply.status {
            200 => (KeyHealthState::Valid, None),
            400 if invalid_key => (
                KeyHealthState::Invalid,
                Some(Failure::new(
                    FailureKind::AuthRequired,
                    "Google AI rejected the API key",
                )),
            ),
            403 => (
                KeyHealthState::Blocked,
                Some(Failure::new(
                    FailureKind::AuthRequired,
                    "Google AI blocked the API key",
                )),
            ),
            402 => (
                KeyHealthState::OutOfCredits,
                Some(Failure::new(
                    FailureKind::QuotaExhausted,
                    "prepaid credits depleted",
                )),
            ),
            429 => {
                let mut failure =
                    Failure::new(FailureKind::RateLimited, "rate limited by Google AI");
                let delay = reply.retry_after_secs.map(|s| s as f64).or_else(|| {
                    serde_json::from_str::<serde_json::Value>(&reply.body)
                        .ok()
                        .and_then(|v| {
                            v.pointer("/error/details")
                                .and_then(|d| d.as_array())
                                .and_then(|details| {
                                    details.iter().find_map(|d| {
                                        d.get("retryDelay")
                                            .and_then(|r| r.as_str())
                                            .and_then(super::openai::parse_duration)
                                    })
                                })
                        })
                });
                failure.retry_after = delay
                    .filter(|s| *s <= i64::MAX as f64)
                    .map(|s| Timestamp::from_secs(target.now_secs.saturating_add(s.ceil() as i64)));
                (KeyHealthState::Unknown, Some(failure))
            }
            _ => (
                KeyHealthState::Unknown,
                Some(Failure::new(
                    FailureKind::Unavailable,
                    "Google AI key health unavailable",
                )),
            ),
        };
        Ok(Reading {
            note: Some(NOTE.into()),
            key_health: Some(KeyHealth {
                state,
                checked_at: Timestamp::from_secs(target.now_secs),
            }),
            verdict,
            ..Reading::default()
        })
    }
}
#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_google_ai.rs"]
mod tests;
