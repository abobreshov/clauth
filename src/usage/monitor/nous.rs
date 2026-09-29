//! Nous Research Portal, read through Hermes' own login (plan v3.1 §4.7
//! "Nous Research — Portal + Hermes", §4.3 D10 reader).
//!
//! **The borrowed token.** Hermes owns the Nous OAuth chain and refreshes its
//! access token ~120 s before expiry; reusing its single-use refresh token
//! would revoke the chain. So this source reads ONLY `providers.nous`
//! `access_token` + `expires_at` out of `<hermes_home>/auth.json`, and only
//! while that token is unexpired:
//!
//! - one plain read of the file (Hermes replaces it atomically), no lock, no
//!   write, never the refresh token (it is not a field of [`HermesNous`], so
//!   serde skips it), never Hermes' resolver;
//! - an expired (or undated) token is `AuthRequired` "run hermes to refresh"
//!   with NO network call;
//! - an unreadable file (mid-write, schema churn) is `Unavailable` and the
//!   last reading stays visible, stale.
//!
//! **Usage.** `GET https://portal.nousresearch.com/api/oauth/account` with
//! the token as Bearer, on the monitoring allowlist
//! ([`super::source::bearer_url_allowed`]). An api-key account
//! (`NOUS_API_KEY`) makes no call by default. An explicit probe sends
//! one token only to a `:free` inference model, never to the portal.
//!
//! **Mapping.** `subscription` → a `subscription` window labelled `Monthly
//! credits`: `used_pct = (monthly_credits − credits_remaining) /
//! monthly_credits × 100`, unclamped (debt reads above 100), `None` unless
//! `monthly_credits` is finite and positive and `credits_remaining` is finite
//! and at most `monthly_credits`; `resets_at = current_period_end`; not
//! chain-eligible. Money (USD credits, exact decimals): `subscription`
//! (Balance, `limit` = monthly credits, monthly period ending at
//! `current_period_end`, start derived), `top_up` (purchased credits),
//! `rollover` and `total_usable` (both non-additive: never summed).

use super::config::MonitorKind;
use crate::usage::keyed_http::{Auth, Method, Request};
use serde::Deserialize;
use serde_json::Value;

use super::source::{
    HttpReply, MonitorHttp, MonitorTarget, NOUS_PORTAL_ORIGIN, Reading, Secret, UsageSource,
};
use crate::usage::observation::{
    Amount, AuthKind, Failure, FailureKind, KeyHealth, KeyHealthState, MoneyKind, MoneyMeter,
    MoneyScope, Period, PeriodKind, QuotaWindow, SourceId, Timestamp, WindowScope,
};

/// The account read.
pub(crate) const NOUS_ACCOUNT_PATH: &str = "/api/oauth/account";
/// The window id of the monthly credit pool.
pub(crate) const WINDOW_SUBSCRIPTION: &str = "subscription";
/// A token this close to expiry is treated as expired: Hermes is about to
/// replace it, and a request racing that replacement gains nothing.
const EXPIRY_SKEW_SECS: i64 = 30;
/// Largest `auth.json` read.
const MAX_AUTH_BYTES: u64 = 1024 * 1024;

pub(crate) struct NousSource;

impl UsageSource for NousSource {
    fn source_id(&self, _target: &MonitorTarget) -> SourceId {
        SourceId::Nous
    }

    fn auth_kind(&self, target: &MonitorTarget) -> AuthKind {
        if target.key_env.is_some() {
            AuthKind::ApiKey
        } else {
            AuthKind::NativeLogin
        }
    }

    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
        if target.key_env.is_some() {
            return fetch_key(target, http);
        }
        let token = read_hermes_token(&target.hermes_home, target.now_secs)?;
        let reply = http.get_bearer(&format!("{NOUS_PORTAL_ORIGIN}{NOUS_ACCOUNT_PATH}"), &token)?;
        check_status(&reply, target.now_secs)?;
        map_account(&reply.body)
    }
}

const INFERENCE_ORIGIN: &str = "https://inference-api.nousresearch.com";
const KEY_NOTE: &str =
    "Nous API keys have no read endpoint; enable probe for key health (one free 1-token call)";
const MODEL_TTL_SECS: i64 = 86_400;
fn health(state: KeyHealthState, now: i64) -> KeyHealth {
    KeyHealth {
        state,
        checked_at: Timestamp::from_secs(now),
    }
}
fn fetch_key(target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
    if !target.cfg.probe {
        return Ok(Reading {
            key_health: Some(health(KeyHealthState::Unknown, target.now_secs)),
            note: Some(KEY_NOTE.into()),
            ..Reading::default()
        });
    }
    let key = target.api_key.as_ref().ok_or_else(|| {
        Failure::new(
            FailureKind::AuthRequired,
            "Nous API key missing from environment or store",
        )
    })?;
    let auto = target.cfg.probe_model.is_none();
    let (mut model, mut model_at) = match target.cfg.probe_model.as_ref() {
        Some(model) => (model.clone(), None),
        None => match target
            .previous
            .as_ref()
            .and_then(|r| r.probe_model.as_ref().zip(r.probe_model_at))
            .filter(|(m, at)| {
                m.ends_with(":free")
                    && *at <= target.now_secs
                    && target.now_secs.saturating_sub(*at) < MODEL_TTL_SECS
            }) {
            Some((model, at)) => (model.clone(), Some(at)),
            None => (pick_free_model(http)?, Some(target.now_secs)),
        },
    };
    for attempt in 0..2 {
        // Independent send-time guard: even malformed config or cache can never
        // make the health probe send a request to a paid model.
        if !model.ends_with(":free") {
            return Err(Failure::new(
                FailureKind::AuthRequired,
                "Nous probe model must end :free",
            ));
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "." }],
            "max_tokens": 1,
            "stream": false,
        }))
        .map_err(|_| {
            Failure::new(
                FailureKind::Unavailable,
                "could not construct Nous free probe",
            )
        })?;
        let reply = http.send(
            MonitorKind::Nous,
            &Request {
                method: Method::Post,
                url: &format!("{INFERENCE_ORIGIN}/v1/chat/completions"),
                auth: Auth::Bearer(key),
                extra: &[],
                json_body: Some(&body),
            },
        )?;
        if auto && attempt == 0 && reply.status == 404 && model_not_found(&reply.body) {
            model = pick_free_model(http)?;
            model_at = Some(target.now_secs);
            continue;
        }
        let mut reading = match reply.status {
            200 => map_probe_headers(&reply.headers, target.now_secs),
            401 => Reading {
                key_health: Some(health(KeyHealthState::Invalid, target.now_secs)),
                verdict: Some(Failure::new(
                    FailureKind::AuthRequired,
                    "Nous says the key is invalid, blocked or out of funds",
                )),
                ..Reading::default()
            },
            429 => {
                let mut failure =
                    Failure::new(FailureKind::RateLimited, "rate limited by Nous inference");
                failure.retry_after = reply.retry_after_secs.map(|s| {
                    Timestamp::from_secs(
                        target
                            .now_secs
                            .saturating_add(i64::try_from(s).unwrap_or(i64::MAX)),
                    )
                });
                return Err(failure);
            }
            403 => Reading {
                key_health: Some(health(KeyHealthState::Blocked, target.now_secs)),
                verdict: Some(Failure::new(
                    FailureKind::AuthRequired,
                    "Nous says the key is invalid, blocked or out of funds",
                )),
                ..Reading::default()
            },
            status => {
                return Err(Failure::new(
                    FailureKind::Unavailable,
                    &format!("Nous free probe answered HTTP {status}"),
                ));
            }
        };
        if auto {
            reading.probe_model = Some(model);
            reading.probe_model_at = model_at;
        }
        return Ok(reading);
    }
    Err(Failure::new(
        FailureKind::Unavailable,
        "Nous free model is no longer available",
    ))
}
fn model_not_found(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let error = value.get("error").unwrap_or(&value);
    error.get("code").and_then(Value::as_str) == Some("model_not_found")
        || error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|m| {
                let m = m.to_ascii_lowercase();
                m.contains("model") && m.contains("not found")
            })
}
fn pick_free_model(http: &dyn MonitorHttp) -> Result<String, Failure> {
    let reply = http.send(
        MonitorKind::Nous,
        &Request {
            method: Method::Get,
            url: &format!("{INFERENCE_ORIGIN}/v1/models"),
            auth: Auth::None,
            extra: &[],
            json_body: None,
        },
    )?;
    if reply.status != 200 {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "Nous public model listing unavailable",
        ));
    }
    let value: Value = serde_json::from_str(&reply.body).map_err(|_| {
        Failure::new(
            FailureKind::InvalidResponse,
            "Nous public model listing unreadable",
        )
    })?;
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .filter(|m| m.ends_with(":free"))
        .min()
        .map(str::to_owned)
        .ok_or_else(|| {
            Failure::new(
                FailureKind::Unavailable,
                "Nous public model listing has no free model",
            )
        })
}
fn map_probe_headers(headers: &[(String, String)], now: i64) -> Reading {
    let mut reading = Reading {
        key_health: Some(health(KeyHealthState::Valid, now)),
        windows: super::openai::rate_windows(headers, "nous", now),
        ..Reading::default()
    };
    for (id, label, suffix, additive) in [
        (
            "total_usable",
            "Total usable credits",
            "remaining-micros",
            false,
        ),
        (
            "subscription",
            "Subscription credits",
            "subscription-remaining-micros",
            true,
        ),
        (
            "top_up",
            "Top-up credits",
            "purchased-remaining-micros",
            true,
        ),
        ("rollover", "Rollover credits", "rollover-micros", false),
    ] {
        let name = format!("x-nous-credits-{suffix}");
        if let Some(value) = headers
            .iter()
            .find(|(n, _)| n == &name)
            .and_then(|(_, v)| v.parse::<i64>().ok())
        {
            reading
                .money
                .push(meter(id, label, Amount::from_minor(value, 6), additive));
        }
    }
    if headers
        .iter()
        .any(|(n, v)| n == "x-nous-credits-paid-access" && v.eq_ignore_ascii_case("false"))
    {
        reading.verdict = Some(Failure::new(
            FailureKind::QuotaExhausted,
            "Nous credits depleted (free models still work)",
        ));
    }
    reading
}

// ── the Hermes token ───────────────────────────────────────────────────────────

/// The D10 reader: only `providers.nous`. Every other key of `auth.json`
/// (the refresh token, the credential pool) is skipped by serde, never held.
#[derive(Deserialize)]
struct HermesAuth {
    #[serde(default)]
    providers: Option<HermesProviders>,
}

#[derive(Deserialize)]
struct HermesProviders {
    #[serde(default)]
    nous: Option<HermesNous>,
}

#[derive(Deserialize)]
struct HermesNous {
    #[serde(default)]
    access_token: Option<Secret>,
    #[serde(default)]
    expires_at: Option<Value>,
}

/// Hermes' Nous access token from `<home>/auth.json`, only while unexpired.
pub(crate) fn read_hermes_token(home: &std::path::Path, now_secs: i64) -> Result<Secret, Failure> {
    use std::io::Read as _;
    let path = home.join("auth.json");
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Failure::new(
                FailureKind::AuthRequired,
                "no Hermes login found; run hermes and log in to Nous",
            ));
        }
        Err(_) => {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "could not read Hermes' auth.json",
            ));
        }
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_AUTH_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_AUTH_BYTES
    {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "could not read Hermes' auth.json",
        ));
    }
    let Ok(auth) = serde_json::from_slice::<HermesAuth>(&bytes) else {
        // Mid-write or schema churn: keep the last reading, stale.
        return Err(Failure::new(
            FailureKind::Unavailable,
            "Hermes' auth.json is unreadable right now; keeping the last reading",
        ));
    };
    let Some(nous) = auth.providers.and_then(|p| p.nous) else {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "Hermes holds no Nous login; run hermes and log in to Nous",
        ));
    };
    let Some(token) = nous.access_token.filter(|t| !t.expose().trim().is_empty()) else {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "Hermes holds no Nous access token; run hermes to refresh",
        ));
    };
    let expires = nous.expires_at.as_ref().and_then(value_timestamp);
    match expires {
        Some(exp) if exp.secs() > now_secs.saturating_add(EXPIRY_SKEW_SECS) => Ok(token),
        Some(_) => Err(Failure::new(
            FailureKind::AuthRequired,
            "Hermes' Nous token has expired; run hermes to refresh",
        )),
        None => Err(Failure::new(
            FailureKind::AuthRequired,
            "Hermes' Nous token carries no expiry; run hermes to refresh",
        )),
    }
}

// ── the account ────────────────────────────────────────────────────────────────

/// An HTTP status as a failure, or `Ok` for a 2xx.
fn check_status(reply: &HttpReply, now_secs: i64) -> Result<(), Failure> {
    match reply.status {
        200..=299 => Ok(()),
        401 | 403 => Err(Failure::new(
            FailureKind::AuthRequired,
            "Nous rejected Hermes' token; run hermes to refresh",
        )),
        429 => {
            let mut f = Failure::new(FailureKind::RateLimited, "rate limited by the Nous portal");
            f.retry_after = reply.retry_after_secs.map(|s| {
                Timestamp::from_secs(now_secs.saturating_add(i64::try_from(s).unwrap_or(i64::MAX)))
            });
            Err(f)
        }
        s => Err(Failure::new(
            FailureKind::Unavailable,
            &format!("the Nous portal answered HTTP {s}"),
        )),
    }
}

#[derive(Deserialize)]
struct Account {
    #[serde(default)]
    subscription: Option<Subscription>,
    #[serde(default)]
    paid_service_access: Option<PaidAccess>,
    #[serde(default)]
    paid_access: Option<bool>,
}

#[derive(Deserialize)]
struct Subscription {
    #[serde(default)]
    plan: Option<Value>,
    #[serde(default)]
    tier: Option<Value>,
    #[serde(default)]
    monthly_credits: Option<Value>,
    #[serde(default)]
    credits_remaining: Option<Value>,
    #[serde(default)]
    rollover_credits: Option<Value>,
    #[serde(default)]
    current_period_end: Option<Value>,
}

#[derive(Deserialize)]
struct PaidAccess {
    #[serde(default)]
    subscription_credits_remaining: Option<Value>,
    #[serde(default)]
    purchased_credits_remaining: Option<Value>,
    #[serde(default)]
    total_usable_credits: Option<Value>,
    #[serde(default)]
    paid_access: Option<bool>,
}

/// The `/api/oauth/account` body as a [`Reading`]. Pure.
pub(crate) fn map_account(body: &str) -> Result<Reading, Failure> {
    let account: Account = serde_json::from_str(body).map_err(|_| {
        Failure::new(
            FailureKind::InvalidResponse,
            "the Nous portal answered in an unknown shape",
        )
    })?;
    if account.subscription.is_none() && account.paid_service_access.is_none() {
        return Err(Failure::new(
            FailureKind::InvalidResponse,
            "the Nous account carries neither a subscription nor paid access",
        ));
    }
    let mut reading = Reading::default();
    let sub = account.subscription.as_ref();
    let paid = account.paid_service_access.as_ref();
    let monthly = sub
        .and_then(|s| s.monthly_credits.as_ref())
        .and_then(value_amount);
    let remaining = sub
        .and_then(|s| s.credits_remaining.as_ref())
        .and_then(value_amount);
    let period_end = sub
        .and_then(|s| s.current_period_end.as_ref())
        .and_then(value_timestamp);

    reading.plan = sub.and_then(|s| {
        let plan = s.plan.as_ref().and_then(value_label);
        let tier = s.tier.as_ref().and_then(value_label);
        match (plan, tier) {
            (Some(p), Some(t)) if !p.eq_ignore_ascii_case(&t) => Some(format!("{p} ({t})")),
            (Some(p), _) => Some(p),
            (None, t) => t,
        }
    });

    if let Some(s) = sub {
        let mut w = QuotaWindow::new(WINDOW_SUBSCRIPTION, "Monthly credits", WindowScope::Account);
        w.resets_at = s.current_period_end.as_ref().and_then(value_timestamp);
        w.used_pct = used_pct(monthly.as_ref(), remaining.as_ref());
        if let (Some(m), Some(r)) = (&monthly, &remaining) {
            w.used = Some(m.to_f64() - r.to_f64()).filter(|v| v.is_finite());
            w.limit = Some(m.to_f64()).filter(|v| v.is_finite());
            w.exhausted = *r <= Amount::zero() && *m > Amount::zero();
        }
        reading.windows.push(w);
    }

    let sub_left = paid
        .and_then(|p| p.subscription_credits_remaining.as_ref())
        .and_then(value_amount)
        .or_else(|| remaining.clone());
    if let Some(left) = sub_left {
        let mut m = meter("subscription", "Subscription credits", left, true);
        m.limit = monthly.clone();
        m.period = Some(monthly_period(period_end));
        reading.money.push(m);
    }
    if let Some(top_up) = paid
        .and_then(|p| p.purchased_credits_remaining.as_ref())
        .and_then(value_amount)
    {
        reading
            .money
            .push(meter("top_up", "Top-up credits", top_up, true));
    }
    if let Some(rollover) = sub
        .and_then(|s| s.rollover_credits.as_ref())
        .and_then(value_amount)
    {
        // Whether rollover is already inside `credits_remaining` is not
        // published, so it is never summed with the other pools.
        reading
            .money
            .push(meter("rollover", "Rollover credits", rollover, false));
    }
    if let Some(total) = paid
        .and_then(|p| p.total_usable_credits.as_ref())
        .and_then(value_amount)
    {
        reading
            .money
            .push(meter("total_usable", "Total usable credits", total, false));
    }

    let paid_access = paid.and_then(|p| p.paid_access).or(account.paid_access);
    if paid_access == Some(false) {
        reading.verdict = Some(Failure::new(
            FailureKind::QuotaExhausted,
            "Nous paid access is off: credits depleted",
        ));
    }
    Ok(reading)
}

/// `(monthly − remaining) / monthly × 100`, unclamped, behind Hermes' guard:
/// `None` unless monthly is finite and positive and remaining is finite and
/// at most monthly.
pub(crate) fn used_pct(monthly: Option<&Amount>, remaining: Option<&Amount>) -> Option<f64> {
    let m = monthly?.to_f64();
    let r = remaining?.to_f64();
    if !(m.is_finite() && m > 0.0 && r.is_finite() && r <= m) {
        return None;
    }
    Some((m - r) / m * 100.0)
}

fn meter(id: &str, label: &str, amount: Amount, additive: bool) -> MoneyMeter {
    let mut m = MoneyMeter::new(
        id,
        label,
        MoneyKind::Balance,
        amount,
        "USD",
        MoneyScope::Profile,
    );
    m.additive = additive;
    m
}

/// The monthly period ending at `end`, its start derived one calendar month
/// earlier.
fn monthly_period(end: Option<Timestamp>) -> Period {
    let mut p = Period::of(PeriodKind::Monthly);
    p.end = end;
    p.start = end.and_then(|e| {
        let at = chrono::DateTime::from_timestamp(e.secs(), 0)?;
        at.checked_sub_months(chrono::Months::new(1))
            .map(|s| Timestamp::from_secs(s.timestamp()))
    });
    p.derived = p.start.is_some();
    p
}

/// A JSON number or decimal string as an exact [`Amount`].
pub(crate) fn value_amount(v: &Value) -> Option<Amount> {
    match v {
        Value::String(s) => Amount::parse(s),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Amount::parse(&i.to_string()),
            (None, Some(u), _) => Amount::parse(&u.to_string()),
            (None, None, Some(f)) => Amount::from_f64(f),
            _ => None,
        },
        _ => None,
    }
}

/// An RFC 3339 string, or epoch seconds / milliseconds.
pub(crate) fn value_timestamp(v: &Value) -> Option<Timestamp> {
    let from_number = |n: f64| {
        if !n.is_finite() || n <= 0.0 {
            return None;
        }
        // Past ~2001-09 in ms, i.e. far beyond any epoch-seconds stamp.
        let secs = if n > 1e12 { n / 1000.0 } else { n };
        Some(Timestamp::from_secs(secs as i64))
    };
    match v {
        Value::String(s) => {
            Timestamp::parse(s).or_else(|| s.trim().parse().ok().and_then(from_number))
        }
        Value::Number(n) => n.as_f64().and_then(from_number),
        _ => None,
    }
}

/// A string or number as a short label.
fn value_label(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_nous.rs"]
mod tests;
