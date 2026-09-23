//! Antigravity's own subscription endpoints, using its existing login only.
//! Model observations and shared pools stay separate: Google does not currently
//! return machine-readable model membership for the shared quota groups.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::config::TargetConfig;
use super::types::{ProviderData, ProviderError, QuotaBucket};

const BASE_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:";

pub(crate) fn credentials(target: &TargetConfig) -> Result<Value, ProviderError> {
    let credentials = if let Some(path) = &target.auth_file {
        let path = super::config::expand(path)
            .map_err(|_| ProviderError::auth("Antigravity credential path must be absolute"))?;
        super::read_json(&path)?
    } else {
        native_credentials()?
    };
    Ok(credentials)
}

pub(crate) fn fetch(
    _target: &TargetConfig,
    credentials: &Value,
) -> Result<ProviderData, ProviderError> {
    let token = access_token(credentials, Utc::now())?;
    let authorization = format!("Bearer {token}");
    let headers = [
        ("Authorization", authorization.as_str()),
        ("User-Agent", "antigravity-cli"),
    ];
    // Empty bodies are accepted for the signed-in personal account. In
    // particular, no account/project IDs need to be copied into clauth state.
    let body = json!({});
    let plan = super::post_json(&format!("{BASE_URL}loadCodeAssist"), &headers, &body)?;
    let models = super::post_json(&format!("{BASE_URL}fetchAvailableModels"), &headers, &body)?;
    let summary = super::post_json(
        &format!("{BASE_URL}retrieveUserQuotaSummary"),
        &headers,
        &body,
    )?;
    parse(&plan, &models, &summary)
}

fn access_token(credentials: &Value, now: DateTime<Utc>) -> Result<&str, ProviderError> {
    let token = credentials.get("token").ok_or_else(|| {
        ProviderError::auth("Antigravity login has an unrecognized credential format")
    })?;
    if let Some(expiry) = token.get("expiry") {
        let expiry = expiry
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .ok_or_else(|| ProviderError::auth("Antigravity login has an invalid token expiry"))?;
        if expiry <= now {
            return Err(ProviderError::auth(
                "Antigravity login expired; open agy to renew the official client login",
            ));
        }
    }
    token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| {
            !s.is_empty() && !s.chars().any(char::is_whitespace) && !s.chars().any(char::is_control)
        })
        .ok_or_else(|| ProviderError::auth("Antigravity access token is missing or invalid"))
}

#[cfg(all(target_os = "linux", not(test)))]
fn native_credentials() -> Result<Value, ProviderError> {
    use secret_service::{EncryptionType, blocking::SecretService};
    use std::collections::HashMap;

    let service = SecretService::connect(EncryptionType::Dh)
        .map_err(|_| ProviderError::auth("Antigravity keyring is unavailable; sign in with agy"))?;
    let matches = service
        .search_items(HashMap::from([
            ("service", "gemini"),
            ("username", "antigravity"),
        ]))
        .map_err(|_| ProviderError::auth("cannot read Antigravity keyring login"))?;
    // Never unlock a keyring (which can display a prompt), create an item, or
    // select arbitrarily between multiple login records.
    if matches.unlocked.len() + matches.locked.len() > 1 {
        return Err(ProviderError::auth(
            "multiple Antigravity logins found; configure an explicit auth_file",
        ));
    }
    let item = matches.unlocked.first().ok_or_else(|| {
        ProviderError::auth("Antigravity login is missing or locked; sign in with agy")
    })?;
    let secret = item
        .get_secret()
        .map_err(|_| ProviderError::auth("cannot read Antigravity keyring login"))?;
    serde_json::from_slice(&secret)
        .map_err(|_| ProviderError::auth("Antigravity login has an unrecognized credential format"))
}

#[cfg(any(not(target_os = "linux"), test))]
fn native_credentials() -> Result<Value, ProviderError> {
    Err(ProviderError::auth(
        "configure an Antigravity auth_file on this platform",
    ))
}

fn remaining(value: &Value) -> Option<f64> {
    value
        .get("remainingFraction")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
        .map(|n| n * 100.0)
}

fn reset(value: &Value) -> Option<String> {
    value
        .get("resetTime")
        .and_then(Value::as_str)
        .filter(|s| DateTime::parse_from_rfc3339(s).is_ok())
        .map(str::to_owned)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

fn parse(plan: &Value, models: &Value, summary: &Value) -> Result<ProviderData, ProviderError> {
    let model_map = models
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(ProviderError::invalid)?;
    let groups = summary
        .get("groups")
        .and_then(Value::as_array)
        .ok_or_else(ProviderError::invalid)?;
    let mut data = ProviderData {
        plan: plan
            .get("paidTier")
            .and_then(|t| text(t, "name").or_else(|| text(t, "id")))
            .or_else(|| {
                plan.get("currentTier")
                    .and_then(|t| text(t, "name").or_else(|| text(t, "id")))
            }),
        ..ProviderData::default()
    };
    for group in groups {
        let group_label = text(group, "displayName").unwrap_or_else(|| "Antigravity".into());
        let buckets = group
            .get("buckets")
            .and_then(Value::as_array)
            .ok_or_else(ProviderError::invalid)?;
        for bucket in buckets {
            let id = text(bucket, "bucketId").ok_or_else(ProviderError::invalid)?;
            let label = text(bucket, "displayName").unwrap_or_else(|| id.clone());
            let remaining = remaining(bucket);
            data.buckets.push(QuotaBucket {
                id: format!("pool:{id}"),
                label: format!("{group_label}: {label}"),
                scope: "shared".into(),
                models: vec![], // The endpoint provides no explicit membership.
                used_percent: remaining.map(|n| 100.0 - n),
                remaining_percent: remaining,
                resets_at: reset(bucket),
                window_seconds: match bucket.get("window").and_then(Value::as_str) {
                    Some("5h") => Some(18_000),
                    Some("weekly") => Some(604_800),
                    _ => None,
                },
                exhausted: remaining == Some(0.0),
            });
        }
    }
    for (model_id, model) in model_map {
        if model.get("isInternal").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let quota = model.get("quotaInfo").unwrap_or(&Value::Null);
        let remaining = remaining(quota);
        data.buckets.push(QuotaBucket {
            id: format!("model:{model_id}"),
            label: text(model, "displayName").unwrap_or_else(|| model_id.clone()),
            scope: "model".into(),
            models: vec![model_id.clone()],
            used_percent: remaining.map(|n| 100.0 - n),
            remaining_percent: remaining,
            resets_at: reset(quota),
            window_seconds: None,
            exhausted: remaining == Some(0.0),
        });
    }
    if data.buckets.is_empty() {
        return Err(ProviderError::invalid());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Value, Value, Value) {
        (
            json!({"currentTier":{"id":"free-tier","name":"Antigravity"},"paidTier":{"id":"g1-pro-tier","name":"Google AI Pro"}}),
            json!({"models":{
                "future-example":{"displayName":"Future Model","quotaInfo":{"remainingFraction":0.75,"resetTime":"2030-01-01T05:00:00Z"}},
                "unknown-example":{"displayName":"Unknown Quota"},
                "internal-example":{"isInternal":true,"quotaInfo":{"remainingFraction":1}}
            }}),
            json!({"groups":[{"displayName":"Example Group","buckets":[
                {"bucketId":"example-weekly","displayName":"Weekly Limit Remaining","window":"weekly","remainingFraction":0.25,"resetTime":"2030-01-07T00:00:00Z"},
                {"bucketId":"example-5h","displayName":"Five Hour Limit Remaining","window":"5h","remainingFraction":0,"resetTime":"2030-01-01T05:00:00Z"}
            ]}]}),
        )
    }

    #[test]
    fn paid_subscription_wins_over_free_base_tier() {
        let (plan, models, summary) = fixture();
        let data = parse(&plan, &models, &summary).unwrap();
        assert_eq!(data.plan.as_deref(), Some("Google AI Pro"));
        assert!(data.credits.is_empty());
        assert!(data.subscription_status.is_none());
        let free = parse(
            &json!({"currentTier":{"name":"Antigravity"}}),
            &models,
            &summary,
        )
        .unwrap();
        assert_eq!(free.plan.as_deref(), Some("Antigravity"));
    }

    #[test]
    fn shared_windows_and_model_observations_keep_distinct_meanings() {
        let (plan, models, summary) = fixture();
        let data = parse(&plan, &models, &summary).unwrap();
        assert_eq!(data.buckets.len(), 4);
        let weekly = &data.buckets[0];
        assert_eq!(weekly.remaining_percent, Some(25.0));
        assert_eq!(weekly.used_percent, Some(75.0));
        assert_eq!(weekly.window_seconds, Some(604_800));
        assert_eq!(weekly.resets_at.as_deref(), Some("2030-01-07T00:00:00Z"));
        assert!(weekly.models.is_empty());
        assert!(data.buckets[1].exhausted);
        assert_eq!(data.buckets[1].window_seconds, Some(18_000));
        let model = data
            .buckets
            .iter()
            .find(|b| b.id == "model:future-example")
            .unwrap();
        assert_eq!(model.models, ["future-example"]);
        assert_eq!(model.used_percent, Some(25.0));
        assert_eq!(model.window_seconds, None);
        let unknown = data
            .buckets
            .iter()
            .find(|b| b.id == "model:unknown-example")
            .unwrap();
        assert_eq!(unknown.remaining_percent, None);
        assert!(!unknown.exhausted);
    }

    #[test]
    fn malformed_or_absent_fraction_and_reset_stay_unknown() {
        for fraction in [json!(-0.1), json!(1.1), json!("0.5"), Value::Null] {
            let (plan, mut models, mut summary) = fixture();
            models["models"]["future-example"]["quotaInfo"] =
                json!({"remainingFraction":fraction,"resetTime":"tomorrow"});
            summary["groups"][0]["buckets"][0]["remainingFraction"] = fraction;
            let data = parse(&plan, &models, &summary).unwrap();
            assert_eq!(data.buckets[0].remaining_percent, None);
            let model = data
                .buckets
                .iter()
                .find(|b| b.id == "model:future-example")
                .unwrap();
            assert_eq!(model.remaining_percent, None);
            assert_eq!(model.resets_at, None);
            assert!(!model.exhausted);
        }
        let (plan, models, _) = fixture();
        assert!(parse(&plan, &models, &json!({"error":"example"})).is_err());
    }

    #[test]
    fn expired_or_invalid_credentials_fail_without_request_or_refresh() {
        let now = DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for token in [
            json!({"access_token":"test-only","expiry":"2029-12-31T23:59:59Z"}),
            json!({"access_token":"test-only","expiry":"bad"}),
            json!({"access_token":"test\nonly"}),
            json!({"refresh_token":"never-used"}),
        ] {
            assert!(access_token(&json!({"token":token}), now).is_err());
        }
        let valid = json!({"token":{"access_token":"test-only","expiry":"2030-01-01T00:01:00Z"}});
        assert_eq!(access_token(&valid, now).unwrap(), "test-only");
    }
}
