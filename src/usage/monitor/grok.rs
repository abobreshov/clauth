//! Borrowed Grok login: narrow reads, never refresh or lock its store.
use super::{
    config::MonitorKind,
    source::{HttpReply, MonitorHttp, MonitorTarget, Reading, Secret, UsageSource},
};
use crate::usage::{
    keyed_http::{Auth, Method, Request},
    observation::{AuthKind, Failure, FailureKind, QuotaWindow, Share, SourceId, WindowScope},
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, io::Read, path::Path};

pub(crate) struct GrokSource;
#[derive(Deserialize, Debug)]
pub(crate) struct GrokEntry {
    pub(crate) key: Option<Secret>,
    #[serde(default, deserialize_with = "present_expiry")]
    pub(crate) expires_at: Option<Value>,
}
/// Preserve an explicitly null expiry: only a missing field permits JWT fallback.
pub(crate) fn present_expiry<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}
pub(crate) fn official_login(name: &str) -> bool {
    matches!(
        name.split("::").next(),
        Some("https://auth.x.ai" | "https://accounts.x.ai/sign-in")
    )
}
pub(crate) fn read_entries(home: &Path) -> Result<BTreeMap<String, GrokEntry>, Failure> {
    let file = std::fs::File::open(home.join("auth.json")).map_err(|e| {
        Failure::new(
            if e.kind() == std::io::ErrorKind::NotFound {
                FailureKind::AuthRequired
            } else {
                FailureKind::Unavailable
            },
            "no Grok login; run grok",
        )
    })?;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::new(FailureKind::Unavailable, "could not read grok's auth.json"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "grok's auth.json is too large",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        Failure::new(
            FailureKind::Unavailable,
            "grok's auth.json is being rewritten; keeping the last reading",
        )
    })
}
pub(crate) fn read_token(home: &Path, selected: Option<&str>, now: i64) -> Result<Secret, Failure> {
    let mut entries = read_entries(home)?;
    let entry = if let Some(name) = selected {
        if !official_login(name) {
            return Err(Failure::new(
                FailureKind::AuthRequired,
                "configured Grok login is not official",
            ));
        }
        entries.remove(name)
    } else {
        let names: Vec<_> = entries
            .keys()
            .filter(|s| official_login(s))
            .cloned()
            .collect();
        if names.len() > 1 {
            return Err(Failure::new(
                FailureKind::AuthRequired,
                "several Grok logins; set auth_entry (see `tollgate monitor detect --explain`)",
            ));
        }
        names.first().and_then(|s| entries.remove(s))
    }
    .ok_or_else(|| Failure::new(FailureKind::AuthRequired, "no Grok login; run grok"))?;
    let token = entry
        .key
        .filter(|s| !s.expose().is_empty() && !s.expose().chars().any(char::is_whitespace))
        .ok_or_else(|| Failure::new(FailureKind::AuthRequired, "no Grok access token; run grok"))?;
    let expiry = match entry.expires_at.as_ref() {
        Some(value) => super::nous::value_timestamp(value).map(|t| t.secs()),
        None => crate::codex_auth::jwt_exp_ms(token.expose()).map(|ms| ms / 1000),
    };
    match expiry {
        Some(exp) if exp > now.saturating_add(60) => Ok(token),
        Some(_) => Err(Failure::new(
            FailureKind::AuthRequired,
            "Grok token expired; open grok to refresh it",
        )),
        None => Err(Failure::new(
            FailureKind::AuthRequired,
            "grok's token carries no expiry; open grok",
        )),
    }
}
pub(crate) fn status(reply: &HttpReply, now: i64) -> Result<(), Failure> {
    match reply.status {
        200..=299 => Ok(()),
        401 | 403 => Err(Failure::new(
            FailureKind::AuthRequired,
            "open grok to refresh its login",
        )),
        429 => {
            let mut f = Failure::new(FailureKind::RateLimited, "Grok rate limited");
            f.retry_after = reply
                .headers
                .iter()
                .find(|(k, _)| k == "retry-after")
                .and_then(|(_, v)| crate::usage::parse_retry_after_at(v, now))
                .map(|d| {
                    crate::usage::observation::Timestamp::from_secs(
                        now.saturating_add(d.as_secs() as i64),
                    )
                });
            Err(f)
        }
        _ => Err(Failure::new(
            FailureKind::Unavailable,
            "Grok usage unavailable",
        )),
    }
}
impl UsageSource for GrokSource {
    fn source_id(&self, _: &MonitorTarget) -> SourceId {
        SourceId::Grok
    }
    fn auth_kind(&self, _: &MonitorTarget) -> AuthKind {
        AuthKind::NativeLogin
    }
    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
        let home = target.cfg.tool_home_in(&target.home);
        let token = read_token(&home, target.cfg.auth_entry.as_deref(), target.now_secs)?;
        let get = |path: &str| {
            let url = format!("https://cli-chat-proxy.grok.com{path}");
            let r = http.send(
                MonitorKind::Grok,
                &Request {
                    method: Method::Get,
                    url: &url,
                    auth: Auth::Bearer(&token),
                    extra: &[("X-XAI-Token-Auth", "xai-grok-cli")],
                    json_body: None,
                },
            )?;
            status(&r, target.now_secs)?;
            serde_json::from_str::<Value>(&r.body)
                .map_err(|_| Failure::new(FailureKind::Unavailable, "Grok response unrecognized"))
        };
        let billing = get("/v1/billing?format=credits")?;
        let user = get("/v1/user?include=subscription")?;
        let settings = if user
            .get("subscriptionTier")
            .and_then(Value::as_str)
            .is_none()
        {
            Some(get("/v1/settings")?)
        } else {
            None
        };
        map_billing(&billing, &user, settings.as_ref())
    }
}
pub(crate) fn map_billing(
    billing: &Value,
    user: &Value,
    settings: Option<&Value>,
) -> Result<Reading, Failure> {
    let config = billing.get("config").unwrap_or(billing);
    if !config.is_object() {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "Grok billing response unrecognized",
        ));
    }
    let period = &config["currentPeriod"];
    let start = super::nous::value_timestamp(&period["start"]);
    let end = super::nous::value_timestamp(&period["end"]);
    let secs = start
        .zip(end)
        .and_then(|(s, e)| u64::try_from(e.secs() - s.secs()).ok())
        .filter(|s| *s > 0);
    let label = match period["type"].as_str() {
        Some("USAGE_PERIOD_TYPE_WEEKLY") => "7d".into(),
        Some("USAGE_PERIOD_TYPE_MONTHLY") => "30d".into(),
        _ => secs
            .map(|s| format!("{}d", s / 86400))
            .unwrap_or_else(|| "shared".into()),
    };
    let mut window = QuotaWindow::new("grok.shared", label, WindowScope::Shared);
    window.used_pct = config["creditUsagePercent"]
        .as_f64()
        .filter(|p| p.is_finite());
    window.exhausted = window.used_pct.is_some_and(|p| p >= 100.);
    window.resets_at = end;
    window.window_secs = secs;
    window.attribution = config["productUsage"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Share {
                label: p["product"].as_str()?.to_string(),
                used_pct: p["usagePercent"].as_f64().filter(|v| v.is_finite())?,
            })
        })
        .collect();
    let verdict = window.exhausted.then(|| {
        Failure::new(
            FailureKind::QuotaExhausted,
            "Grok shared allowance exhausted",
        )
    });
    Ok(Reading {
        plan: user["subscriptionTier"]
            .as_str()
            .or_else(|| settings.and_then(|v| v["subscription_tier_display"].as_str()))
            .map(str::to_string),
        windows: vec![window],
        verdict,
        ..Reading::default()
    })
}
#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_grok.rs"]
mod tests;
