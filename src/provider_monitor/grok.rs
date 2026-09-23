//! Grok consumer usage uses the CLI proxy, not the public xAI inference API.
//! Product percentages describe contributions to a single shared allowance.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::config::TargetConfig;
use super::types::{ProviderData, ProviderError, QuotaBucket, UsageAttribution, percent};

const USER_URL: &str = "https://cli-chat-proxy.grok.com/v1/user?include=subscription";
const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const BUCKET_ID: &str = "grok-shared";

pub(crate) fn fetch(target: &TargetConfig, auth: &Value) -> Result<ProviderData, ProviderError> {
    let token = access_token(auth, target.auth_entry.as_deref(), Utc::now().timestamp())?;
    let authorization = format!("Bearer {token}");
    let headers = [
        ("Authorization", authorization.as_str()),
        ("X-XAI-Token-Auth", "xai-grok-cli"),
        ("Accept", "application/json"),
    ];
    let user = super::get_json(USER_URL, &headers)?;
    let billing = super::get_json(BILLING_URL, &headers)?;
    parse(&user, &billing)
}

fn official_login(key: &str) -> bool {
    let issuer = key.split("::").next().unwrap_or_default();
    matches!(
        issuer,
        "https://auth.x.ai" | "https://accounts.x.ai/sign-in"
    )
}

/// Official Grok logins in `auth`, as `(map key, account label)`.
///
/// The map key is the `issuer::account` selector `providers.toml` stores in
/// `auth_entry`. The label is the account half of that key. The token, which
/// lives under the entry's `key` field, is never copied out.
pub(crate) fn login_choices(auth: &Value) -> Vec<(String, String)> {
    let Some(entries) = auth.as_object() else {
        return Vec::new();
    };
    let mut choices: Vec<(String, String)> = entries
        .keys()
        .filter(|key| official_login(key))
        .map(|key| {
            let label = key
                .split("::")
                .nth(1)
                .filter(|label| {
                    !label.is_empty() && label.len() <= 64 && !label.chars().any(char::is_control)
                })
                .unwrap_or("xAI login");
            (key.clone(), label.to_string())
        })
        .collect();
    choices.sort();
    choices
}

/// Read `~/.grok/auth.json` and list official logins. A missing or unreadable
/// file is an empty list: the add screen then does not offer Grok.
pub(crate) fn detect_login_choices() -> Vec<(String, String)> {
    let Ok(path) = super::credential_path(&bare_grok_target(), ".grok/auth.json") else {
        return Vec::new();
    };
    let Ok(auth) = super::read_json(&path) else {
        return Vec::new();
    };
    login_choices(&auth)
}

fn bare_grok_target() -> super::config::TargetConfig {
    super::config::TargetConfig {
        id: "grok".to_string(),
        provider: super::types::ProviderKind::Grok,
        enabled: true,
        model: None,
        auth_file: None,
        auth_entry: None,
        command: None,
        args: Vec::new(),
        listed: false,
    }
}

fn access_token<'a>(
    auth: &'a Value,
    selected: Option<&str>,
    now: i64,
) -> Result<&'a str, ProviderError> {
    let entries = auth.as_object().ok_or_else(|| {
        ProviderError::auth("Grok credentials are not recognized; run grok login")
    })?;
    let entry = if let Some(key) = selected {
        entries
            .get(key)
            .filter(|_| official_login(key))
            .ok_or_else(|| {
                ProviderError::auth("configured Grok login is unavailable or is not an xAI login")
            })?
    } else {
        let mut compatible = entries
            .iter()
            .filter(|(key, entry)| official_login(key) && entry.is_object());
        let (_, entry) = compatible
            .next()
            .ok_or_else(|| ProviderError::auth("no Grok consumer login found; run grok login"))?;
        if compatible.next().is_some() {
            return Err(ProviderError::auth(
                "multiple Grok logins found; select auth_entry in providers.toml",
            ));
        }
        entry
    };
    if let Some(expiry) = entry.get("expires_at").filter(|v| !v.is_null()) {
        let expiry = expiry
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .ok_or_else(|| ProviderError::auth("Grok login expiry is invalid; run grok login"))?;
        if expiry.timestamp() <= now {
            return Err(ProviderError::auth(
                "Grok login expired; open Grok to refresh its login",
            ));
        }
    }
    entry
        .get("key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .ok_or_else(|| ProviderError::auth("Grok access token is missing; run grok login"))
}

fn label(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

pub(crate) fn parse(user: &Value, billing: &Value) -> Result<ProviderData, ProviderError> {
    let config = billing
        .get("config")
        .filter(|v| v.is_object())
        .ok_or_else(ProviderError::invalid)?;
    // A recognizable period may still have an unknown percentage. Never turn
    // absent, malformed, or out-of-range usage into an available allowance.
    if config.get("creditUsagePercent").is_none()
        && !config.get("currentPeriod").is_some_and(Value::is_object)
    {
        return Err(ProviderError::invalid());
    }
    let used = percent(config.get("creditUsagePercent").and_then(Value::as_f64));
    let period = &config["currentPeriod"];
    let start = period["start"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
    let end = period["end"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
    let window = start
        .zip(end)
        .and_then(|(start, end)| u64::try_from((end - start).num_seconds()).ok())
        .filter(|seconds| *seconds > 0);
    let bucket_label = match period["type"].as_str() {
        Some("USAGE_PERIOD_TYPE_WEEKLY") => "Grok shared weekly allowance",
        Some("USAGE_PERIOD_TYPE_MONTHLY") => "Grok shared monthly allowance",
        _ => "Grok shared allowance",
    };
    let attribution = config["productUsage"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|product| {
            Some(UsageAttribution {
                label: label(&product["product"])?,
                used_percent: percent(product["usagePercent"].as_f64())?,
                bucket_id: BUCKET_ID.into(),
            })
        })
        .collect();
    Ok(ProviderData {
        plan: label(&user["subscriptionTier"]),
        buckets: vec![QuotaBucket {
            id: BUCKET_ID.into(),
            label: bucket_label.into(),
            scope: "shared".into(),
            used_percent: used,
            remaining_percent: used.map(|p| 100.0 - p),
            resets_at: end.map(|time| time.to_rfc3339()),
            window_seconds: window,
            exhausted: used.is_some_and(|p| p >= 100.0),
            ..Default::default()
        }],
        attribution,
        // The API's monetary/credit `val` units have not been established.
        // Do not invent a unit or conflate them with subscription usage.
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn billing() -> Value {
        json!({"config": {
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-09-01T12:00:00+00:00",
                "end": "2026-09-08T12:00:00+00:00"
            },
            "creditUsagePercent": 23.5,
            "productUsage": [{"product":"GrokBuild", "usagePercent":20.0},
                {"product":"GrokChat", "usagePercent":3.5}],
            "isUnifiedBillingUser": true,
            "onDemandCap": {"val":0},
            "prepaidBalance": {"val":100}
        }})
    }

    #[test]
    fn shared_percentage_reset_and_product_attribution() {
        let data = parse(&json!({"subscriptionTier":"SuperGrokPlus"}), &billing()).unwrap();
        assert_eq!(data.plan.as_deref(), Some("SuperGrokPlus"));
        assert_eq!(data.buckets.len(), 1);
        let bucket = &data.buckets[0];
        assert_eq!(bucket.scope, "shared");
        assert_eq!(bucket.used_percent, Some(23.5));
        assert_eq!(bucket.remaining_percent, Some(76.5));
        assert_eq!(bucket.window_seconds, Some(604800));
        assert_eq!(
            bucket.resets_at.as_deref(),
            Some("2026-09-08T12:00:00+00:00")
        );
        assert_eq!(data.attribution.len(), 2);
        assert_eq!(data.attribution[0].used_percent, 20.0);
        assert!(data.attribution.iter().all(|a| a.bucket_id == bucket.id));
        assert!(data.credits.is_empty());
    }

    #[test]
    fn missing_and_malformed_percentages_remain_unknown() {
        for invalid in [Value::Null, json!("23.5"), json!(-1), json!(101)] {
            let mut response = billing();
            response["config"]["creditUsagePercent"] = invalid;
            let data = parse(&json!({}), &response).unwrap();
            assert_eq!(data.buckets[0].used_percent, None);
            assert_eq!(data.buckets[0].remaining_percent, None);
            assert!(!data.buckets[0].exhausted);
        }
        let mut response = billing();
        response["config"]
            .as_object_mut()
            .unwrap()
            .remove("creditUsagePercent");
        assert_eq!(
            parse(&json!({}), &response).unwrap().buckets[0].remaining_percent,
            None
        );
        assert!(parse(&json!({}), &json!({"config":{}})).is_err());
    }

    #[test]
    fn full_usage_is_exhausted_and_bad_period_does_not_invent_reset() {
        let mut response = billing();
        response["config"]["creditUsagePercent"] = json!(100);
        response["config"]["currentPeriod"]["end"] = json!("bad date");
        let data = parse(&json!({}), &response).unwrap();
        assert!(data.buckets[0].exhausted);
        assert_eq!(data.buckets[0].remaining_percent, Some(0.0));
        assert_eq!(data.buckets[0].resets_at, None);
        assert_eq!(data.buckets[0].window_seconds, None);
    }

    #[test]
    fn login_choices_name_the_account_and_omit_the_token() {
        let auth = json!({
            "https://auth.x.ai::account-a": {"key":"secret-token-a"},
            "https://accounts.x.ai/sign-in::account-b": {"key":"secret-token-b"},
            "https://evil.example::nope": {"key":"secret-token-c"}
        });
        let choices = login_choices(&auth);
        assert_eq!(
            choices,
            [
                (
                    "https://accounts.x.ai/sign-in::account-b".to_string(),
                    "account-b".to_string()
                ),
                (
                    "https://auth.x.ai::account-a".to_string(),
                    "account-a".to_string()
                ),
            ]
        );
        let rendered = format!("{choices:?}");
        assert!(!rendered.contains("secret-token"));
    }

    #[test]
    fn ambiguous_credentials_require_explicit_selection() {
        let auth = json!({
            "https://auth.x.ai::account-a": {"key":"example-a"},
            "https://auth.x.ai::account-b": {"key":"example-b"}
        });
        let error = access_token(&auth, None, 0).unwrap_err();
        assert_eq!(
            error.state,
            super::super::types::ObservationState::AuthRequired
        );
        assert_eq!(
            access_token(&auth, Some("https://auth.x.ai::account-b"), 0).unwrap(),
            "example-b"
        );
        assert!(access_token(&auth, Some("missing"), 0).is_err());
    }

    #[test]
    fn expired_credentials_and_unrelated_issuers_are_rejected() {
        let auth = json!({"https://auth.x.ai::account-a": {
            "key":"example", "expires_at":"2026-09-01T00:00:00Z"
        }});
        let expiry = DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .timestamp();
        assert!(access_token(&auth, None, expiry).is_err());
        assert_eq!(access_token(&auth, None, expiry - 1).unwrap(), "example");
        let unrelated = json!({"https://auth.x.ai.attacker.test::account-a":{"key":"example"}});
        assert!(access_token(&unrelated, None, 0).is_err());
        assert!(
            access_token(
                &unrelated,
                Some("https://auth.x.ai.attacker.test::account-a"),
                0
            )
            .is_err()
        );
    }
}
