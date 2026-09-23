//! Passive Codex account monitoring. The official CLI owns credential refresh.
//!
//! Wire fields follow openai/codex rust-v0.155.1 backend-client and generated
//! quota models. Each pool keeps its own windows, even with equal durations.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use super::config::TargetConfig;
use super::types::{CreditBalance, ProviderData, ProviderError, QuotaBucket, percent};

pub(crate) fn fetch(_target: &TargetConfig, auth: &Value) -> Result<ProviderData, ProviderError> {
    let token = text(&auth["tokens"]["access_token"])
        .ok_or_else(|| ProviderError::auth("sign in with codex to monitor ChatGPT usage"))?;
    let authorization = format!("Bearer {token}");
    let mut headers = vec![("Authorization", authorization.as_str())];
    if let Some(account) = text(&auth["tokens"]["account_id"]) {
        headers.push(("chatgpt-account-id", account));
    }
    let body = super::get_json("https://chatgpt.com/backend-api/wham/usage", &headers)?;
    parse(&body, Utc::now().timestamp())
}

pub(crate) fn parse(body: &Value, now_secs: i64) -> Result<ProviderData, ProviderError> {
    if !body.is_object() {
        return Err(ProviderError::invalid());
    }
    let mut data = ProviderData {
        plan: text(&body["plan_type"]).map(str::to_owned),
        ..ProviderData::default()
    };
    if let Some(pool) = body.get("rate_limit").filter(|v| !v.is_null()) {
        append_pool(&mut data, pool, "shared", "Codex", &[], now_secs)?;
    }
    if let Some(additional) = body.get("additional_rate_limits").filter(|v| !v.is_null()) {
        for (index, entry) in additional
            .as_array()
            .ok_or_else(ProviderError::invalid)?
            .iter()
            .enumerate()
        {
            if !entry.is_object() {
                return Err(ProviderError::invalid());
            }
            let models: Vec<String> = text(&entry["normal_model_slug"])
                .map(str::to_owned)
                .into_iter()
                .collect();
            let label = text(&entry["limit_name"])
                .or_else(|| text(&entry["metered_feature"]))
                .or_else(|| models.first().map(String::as_str))
                .unwrap_or("Additional Codex quota");
            let feature = text(&entry["metered_feature"])
                .or_else(|| text(&entry["limit_id"]))
                .or_else(|| models.first().map(String::as_str))
                .or_else(|| text(&entry["limit_name"]));
            let mut id = feature
                .map(|s| format!("additional:{s}"))
                .unwrap_or_else(|| format!("additional:{index}"));
            // Preserve malformed duplicate pools instead of merging or overwriting them.
            if data
                .buckets
                .iter()
                .any(|b| b.id.starts_with(&format!("{id}:")))
            {
                id = format!("{id}:duplicate:{index}");
            }
            if let Some(pool) = entry.get("rate_limit").filter(|v| !v.is_null()) {
                append_pool(&mut data, pool, &id, label, &models, now_secs)?;
            }
        }
    }
    // This verdict has no window attribution. Do not fabricate 100% on any window.
    let reached = text(&body["rate_limit_reached_type"]["type"]);
    if body["spend_control"]["reached"].as_bool() == Some(true) {
        data.buckets
            .push(verdict("spend_control", "Codex spend control", &[], true));
    }
    if let Some(kind) = reached.filter(|kind| *kind != "unknown") {
        data.buckets.push(verdict("account_limit", kind, &[], true));
    }
    if body["credits"]["unlimited"].as_bool() != Some(true)
        && let Some(balance) = number(&body["credits"]["balance"])
    {
        data.credits.push(CreditBalance {
            label: "Codex credits".into(),
            remaining: balance,
            unit: "credits".into(),
        });
    }
    if let Some(balance) = body["rate_limit_reset_credits"]["available_count"].as_u64() {
        data.credits.push(CreditBalance {
            label: "Codex limit resets".into(),
            remaining: balance as f64,
            unit: "resets".into(),
        });
    }
    if data.plan.is_none() && data.buckets.is_empty() && data.credits.is_empty() {
        return Err(ProviderError::invalid());
    }
    Ok(data)
}

fn append_pool(
    data: &mut ProviderData,
    pool: &Value,
    id: &str,
    label: &str,
    models: &[String],
    now_secs: i64,
) -> Result<(), ProviderError> {
    let fields = pool.as_object().ok_or_else(ProviderError::invalid)?;
    let start = data.buckets.len();
    // Enumerate actual window fields, rather than collapse them into 5h/7d slots.
    for (key, window) in fields
        .iter()
        .filter(|(k, v)| k.ends_with("_window") && !v.is_null())
    {
        if !window.is_object() {
            return Err(ProviderError::invalid());
        }
        let used_percent = percent(window["used_percent"].as_f64());
        let resets_at = window["reset_at"]
            .as_i64()
            .filter(|v| *v > 0)
            .and_then(DateTime::<Utc>::from_timestamp_secs)
            .or_else(|| {
                let delta = window["reset_after_seconds"].as_i64().filter(|v| *v >= 0)?;
                DateTime::<Utc>::from_timestamp_secs(now_secs.checked_add(delta)?)
            })
            .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true));
        data.buckets.push(QuotaBucket {
            id: format!("{id}:{key}"),
            label: format!("{label} {}", key.replace('_', " ")),
            scope: if models.is_empty() { "shared" } else { "model" }.into(),
            models: models.to_vec(),
            used_percent,
            remaining_percent: used_percent.map(|p| 100.0 - p),
            resets_at,
            window_seconds: window["limit_window_seconds"].as_u64().filter(|v| *v > 0),
            exhausted: used_percent == Some(100.0),
        });
    }
    let blocked =
        pool["limit_reached"].as_bool() == Some(true) || pool["allowed"].as_bool() == Some(false);
    let has_verdict = pool["limit_reached"].is_boolean() || pool["allowed"].is_boolean();
    if blocked || (data.buckets.len() == start && has_verdict) {
        data.buckets.push(verdict(
            &format!("{id}:availability"),
            label,
            models,
            blocked,
        ));
    }
    Ok(())
}

fn verdict(id: &str, label: &str, models: &[String], exhausted: bool) -> QuotaBucket {
    QuotaBucket {
        id: id.into(),
        label: label.into(),
        scope: if models.is_empty() { "shared" } else { "model" }.into(),
        models: models.to_vec(),
        exhausted,
        ..QuotaBucket::default()
    }
}

fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.trim().is_empty())
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn equal_duration_pools_and_future_windows_are_preserved() {
        let body = json!({
            "plan_type": "pro", "rate_limit": {
                "primary_window": {"used_percent": 20, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": 40, "limit_window_seconds": 604800}
            },
            "additional_rate_limits": [
                {"limit_name": "Model A", "metered_feature": "model-a", "normal_model_slug": "a",
                 "rate_limit": {"primary_window": {"used_percent": 90, "limit_window_seconds": 18000}}},
                {"limit_name": "Model B", "metered_feature": "model-b", "normal_model_slug": "b",
                 "rate_limit": {"primary_window": {"used_percent": 10, "limit_window_seconds": 18000},
                                "tertiary_window": {"used_percent": 5, "limit_window_seconds": 7776000}}}
            ]
        });
        let data = parse(&body, 0).unwrap();
        assert_eq!(data.plan.as_deref(), Some("pro"));
        assert_eq!(data.buckets.len(), 5);
        assert_eq!(
            data.buckets
                .iter()
                .map(|b| (&*b.id, b.used_percent))
                .collect::<Vec<_>>(),
            [
                ("shared:primary_window", Some(20.0)),
                ("shared:secondary_window", Some(40.0)),
                ("additional:model-a:primary_window", Some(90.0)),
                ("additional:model-b:primary_window", Some(10.0)),
                ("additional:model-b:tertiary_window", Some(5.0))
            ]
        );
        assert_eq!(data.buckets[2].models, ["a"]);
        assert_eq!(data.buckets[2].scope, "model");
        assert_eq!(data.buckets[4].window_seconds, Some(7776000));
    }

    #[test]
    fn malformed_or_empty_response_cannot_look_like_unused_quota() {
        for body in [
            json!({}),
            json!(null),
            json!([]),
            json!({"rate_limit":{}}),
            json!({"additional_rate_limits": "bad"}),
            json!({"rate_limit":{"primary_window": "bad"}}),
        ] {
            assert!(parse(&body, 0).is_err());
        }
        for window in [
            json!({}),
            json!({"used_percent": 101}),
            json!({"used_percent": -1}),
            json!({"used_percent": "bad"}),
        ] {
            let data = parse(&json!({"rate_limit": {"primary_window": window}}), 0).unwrap();
            assert_eq!(data.buckets[0].used_percent, None);
            assert_eq!(data.buckets[0].remaining_percent, None);
        }
    }

    #[test]
    fn resets_prefer_absolute_and_fall_back_without_overflow() {
        let data = parse(
            &json!({"rate_limit": {
                "primary_window": {"reset_at": 2000, "reset_after_seconds": 10},
                "secondary_window": {"reset_at": 0, "reset_after_seconds": 60}
            }}),
            1000,
        )
        .unwrap();
        assert_eq!(
            data.buckets[0].resets_at.as_deref(),
            Some("1970-01-01T00:33:20Z")
        );
        assert_eq!(
            data.buckets[1].resets_at.as_deref(),
            Some("1970-01-01T00:17:40Z")
        );
        let overflow = parse(
            &json!({"rate_limit":{"primary_window":{"reset_after_seconds": i64::MAX}}}),
            1000,
        )
        .unwrap();
        assert_eq!(overflow.buckets[0].resets_at, None);
    }

    #[test]
    fn named_model_hard_verdict_does_not_fabricate_percentage() {
        let data = parse(&json!({"additional_rate_limits":[{
            "metered_feature":"named-limit", "limit_name":"Model quota", "normal_model_slug":"future-model",
            "rate_limit":{"limit_reached":true, "primary_window":{"used_percent":96}}
        }]}), 0).unwrap();
        assert_eq!(data.buckets.len(), 2);
        assert_eq!(data.buckets[0].used_percent, Some(96.0));
        let blocked = &data.buckets[1];
        assert_eq!(blocked.id, "additional:named-limit:availability");
        assert!(blocked.exhausted);
        assert_eq!(blocked.models, ["future-model"]);
        assert_eq!(blocked.remaining_percent, None);
    }

    #[test]
    fn account_and_spend_verdicts_and_credit_units_are_explicit() {
        let data = parse(
            &json!({
                "rate_limit":{"allowed":false}, "spend_control":{"reached":true},
                "credits":{"balance":"17.5", "unlimited":false},
                "rate_limit_reset_credits":{"available_count":2}
            }),
            0,
        )
        .unwrap();
        assert_eq!(data.buckets.len(), 2);
        assert!(
            data.buckets
                .iter()
                .all(|b| b.exhausted && b.used_percent.is_none())
        );
        assert_eq!(
            data.credits,
            vec![
                CreditBalance {
                    label: "Codex credits".into(),
                    remaining: 17.5,
                    unit: "credits".into()
                },
                CreditBalance {
                    label: "Codex limit resets".into(),
                    remaining: 2.0,
                    unit: "resets".into()
                },
            ]
        );
    }
}
