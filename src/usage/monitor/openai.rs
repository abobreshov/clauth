//! Free key health read and a separate, hourly admin-only costs leg.
use super::config::MonitorKind;
use super::source::{HttpReply, MonitorHttp, MonitorTarget, Reading, Secret, UsageSource};
use crate::usage::keyed_http::{Auth, Method, Request};
use crate::usage::observation::{
    Amount, AuthKind, Failure, FailureKind, KeyHealth, KeyHealthState, MoneyKind, MoneyMeter,
    MoneyScope, Period, PeriodKind, QuotaWindow, SourceId, Timestamp, WindowScope,
};
use chrono::Datelike;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::BTreeMap;
pub(crate) const NOTE: &str = "no balance endpoint exists for OpenAI keys";
pub(crate) struct OpenaiSource;
impl UsageSource for OpenaiSource {
    fn source_id(&self, _: &MonitorTarget) -> SourceId {
        SourceId::OpenaiApi
    }
    fn auth_kind(&self, target: &MonitorTarget) -> AuthKind {
        if !target.api_key_configured && target.monitoring_key {
            AuthKind::ReadOnly
        } else {
            AuthKind::ApiKey
        }
    }
    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
        let mut reading = Reading {
            note: Some(NOTE.into()),
            ..Reading::default()
        };
        if let Some(key) = &target.api_key {
            let reply = get(http, "https://api.openai.com/v1/models", key)?;
            let (state, verdict) = classify(&reply, target.now_secs);
            reading.key_health = Some(KeyHealth {
                state,
                checked_at: Timestamp::from_secs(target.now_secs),
            });
            reading.verdict = verdict;
            if reply.status == 200 {
                reading.windows = rate_windows(&reply.headers, "openai", target.now_secs);
            }
        }
        if target.api_key_configured && target.api_key.is_none() {
            reading.key_health = Some(KeyHealth {
                state: KeyHealthState::Unknown,
                checked_at: Timestamp::from_secs(target.now_secs),
            });
            reading.verdict = Some(Failure::new(
                FailureKind::AuthRequired,
                "OpenAI API key missing; set the configured secret",
            ));
        }
        if target.monitoring_key
            && let Some(admin) = &target.key
        {
            let prior = target.previous.as_ref();
            if prior
                .and_then(|p| p.costs_at)
                .is_some_and(|at| target.now_secs >= at && target.now_secs - at < 3600)
            {
                if let Some(prior) = prior {
                    retain_costs(&mut reading, prior, target.now_secs);
                    reading.costs_at = prior.costs_at;
                    reading.costs_failure = prior.costs_failure.clone().or_else(|| {
                        prior
                            .note
                            .as_deref()
                            .filter(|note| note.contains("costs unavailable"))
                            .map(|_| {
                                Failure::new(FailureKind::Unavailable, "OpenAI costs unavailable")
                            })
                    });
                    if reading.verdict.is_none()
                        || reading
                            .costs_failure
                            .as_ref()
                            .is_some_and(|failure| failure.kind == FailureKind::RateLimited)
                    {
                        reading.verdict = reading.costs_failure.clone();
                    }
                    if prior
                        .note
                        .as_deref()
                        .is_some_and(|s| s.contains("admin key") || s.contains("costs unavailable"))
                    {
                        reading.note = prior.note.clone();
                    }
                }
            } else {
                reading.costs_at = Some(target.now_secs);
                match costs(http, admin, target.now_secs) {
                    Ok(CostsResult::Meters(money)) => {
                        reading.money = money;
                        reading.costs_observed_at = Some(target.now_secs);
                    }
                    Ok(CostsResult::AdminRequired) => {
                        if let Some(prior) = prior {
                            retain_costs(&mut reading, prior, target.now_secs);
                        }
                        reading.note = Some(format!(
                            "{NOTE}; costs need an OpenAI admin key (sk-admin-…)"
                        ))
                    }
                    Err(failure) => {
                        reading.costs_failure = Some(failure.clone());
                        reading.note = Some(format!(
                            "{NOTE}; costs unavailable; keeping the last reading"
                        ));
                        if reading.verdict.is_none() || failure.kind == FailureKind::RateLimited {
                            reading.verdict = Some(failure);
                        }
                        if let Some(prior) = prior {
                            retain_costs(&mut reading, prior, target.now_secs);
                        }
                    }
                }
            }
        }
        if target.api_key.is_none() && !target.monitoring_key {
            return Err(Failure::new(
                FailureKind::AuthRequired,
                "OpenAI key missing; set the configured secret",
            ));
        }
        Ok(reading)
    }
}
/// Costs age is independent of the frequently polled key-health reading.
fn retain_costs(reading: &mut Reading, prior: &Reading, now: i64) {
    let observed = prior.costs_observed_at.or(prior.costs_at);
    reading.costs_observed_at = observed;
    let Some(observed) = observed else { return };
    if now < observed || now - observed > 7 * 24 * 3600 {
        return;
    }
    let month = |timestamp: i64| {
        chrono::DateTime::from_timestamp(timestamp, 0).map(|date| (date.year(), date.month()))
    };
    let current = month(now);
    reading.money = prior
        .money
        .iter()
        .filter(|meter| {
            meter.meter_id == "spend.monthly"
                && meter.kind == MoneyKind::Spend
                && month(
                    meter
                        .period
                        .as_ref()
                        .and_then(|period| period.start)
                        .map_or(observed, |start| start.secs()),
                ) == current
        })
        .cloned()
        .collect();
}
fn get(http: &dyn MonitorHttp, url: &str, key: &Secret) -> Result<HttpReply, Failure> {
    http.send(
        MonitorKind::Openai,
        &Request {
            method: Method::Get,
            url,
            auth: Auth::Bearer(key),
            extra: &[],
            json_body: None,
        },
    )
}
fn classify(reply: &HttpReply, now: i64) -> (KeyHealthState, Option<Failure>) {
    let code = serde_json::from_str::<serde_json::Value>(&reply.body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/code")
                .and_then(|c| c.as_str().map(str::to_owned))
        });
    let (state, kind, message) = match reply.status {
        200 => return (KeyHealthState::Valid, None),
        401 => (
            KeyHealthState::Invalid,
            FailureKind::AuthRequired,
            "OpenAI rejected the API key",
        ),
        403 => (
            KeyHealthState::Blocked,
            FailureKind::AuthRequired,
            "OpenAI blocked the API key",
        ),
        429 if code.as_deref() == Some("credit_balance_exhausted") => (
            KeyHealthState::OutOfCredits,
            FailureKind::QuotaExhausted,
            "OpenAI credits depleted",
        ),
        429 if matches!(
            code.as_deref(),
            Some("organization_spend_limit_exceeded" | "project_spend_limit_exceeded")
        ) =>
        {
            (
                KeyHealthState::SpendCapped,
                FailureKind::QuotaExhausted,
                "OpenAI spend limit reached",
            )
        }
        429 => (
            KeyHealthState::Unknown,
            FailureKind::RateLimited,
            "rate limited by OpenAI",
        ),
        _ => (
            KeyHealthState::Unknown,
            FailureKind::Unavailable,
            "OpenAI key health unavailable",
        ),
    };
    let mut f = Failure::new(kind, message);
    if kind == FailureKind::RateLimited {
        f.retry_after = reply.retry_after_secs.map(|s| {
            Timestamp::from_secs(now.saturating_add(i64::try_from(s).unwrap_or(i64::MAX)))
        });
    }
    (state, Some(f))
}
/// Parse Go duration components, returning finite nonnegative seconds.
pub(crate) fn parse_duration(raw: &str) -> Option<f64> {
    let mut rest = raw;
    let mut total = 0.0;
    if rest == "0" {
        return Some(0.0);
    }
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .count();
        if len == 0 {
            return None;
        }
        let value = rest[..len].parse::<f64>().ok()?;
        rest = &rest[len..];
        let (unit, factor) = [
            ("ms", 0.001),
            ("us", 0.000001),
            ("µs", 0.000001),
            ("ns", 0.000000001),
            ("h", 3600.0),
            ("m", 60.0),
            ("s", 1.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        rest = &rest[unit.len()..];
        total += value * factor;
    }
    (total.is_finite() && total >= 0.0).then_some(total)
}
/// Headers may be absent: no inferred headroom and no paid inference probe.
pub(crate) fn rate_windows(
    headers: &[(String, String)],
    prefix: &str,
    now: i64,
) -> Vec<QuotaWindow> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let mut windows = Vec::new();
    for (unit, id, label) in [("requests", "rpm", "Requests"), ("tokens", "tpm", "Tokens")] {
        let limit =
            header(&format!("x-ratelimit-limit-{unit}")).and_then(|v| v.parse::<f64>().ok());
        let remaining =
            header(&format!("x-ratelimit-remaining-{unit}")).and_then(|v| v.parse::<f64>().ok());
        if let (Some(limit), Some(remaining)) = (limit, remaining)
            && limit.is_finite()
            && limit > 0.0
            && remaining.is_finite()
        {
            let mut window =
                QuotaWindow::new(format!("{prefix}.{id}"), label, WindowScope::Account);
            let used = limit - remaining;
            let percentage = used / limit * 100.0;
            if !used.is_finite() || !percentage.is_finite() {
                continue;
            }
            window.used_pct = Some(percentage);
            window.limit = Some(limit);
            window.used = Some(used);
            window.exhausted = remaining <= 0.0;
            window.chain_eligible = false;
            window.resets_at = header(&format!("x-ratelimit-reset-{unit}"))
                .and_then(parse_duration)
                .filter(|seconds| *seconds <= i64::MAX as f64)
                .map(|seconds| Timestamp::from_secs(now.saturating_add(seconds.ceil() as i64)));
            windows.push(window);
        }
    }
    windows
}
#[derive(Deserialize)]
struct CostPage {
    data: Vec<CostBucket>,
    #[serde(default)]
    next_page: Option<String>,
    #[serde(default)]
    has_more: bool,
}
#[derive(Deserialize)]
struct CostBucket {
    #[serde(default)]
    results: Vec<CostResult>,
}
#[derive(Deserialize)]
struct CostResult {
    amount: CostAmount,
}
#[derive(Deserialize)]
struct CostAmount {
    value: Box<RawValue>,
    currency: String,
}
enum CostsResult {
    Meters(Vec<MoneyMeter>),
    AdminRequired,
}
fn costs(http: &dyn MonitorHttp, key: &Secret, now: i64) -> Result<CostsResult, Failure> {
    let date = chrono::DateTime::from_timestamp(now, 0).ok_or_else(invalid_costs)?;
    let start = date
        .with_day(1)
        .and_then(|d| d.date_naive().and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp())
        .ok_or_else(invalid_costs)?;
    let mut totals: BTreeMap<String, Amount> = BTreeMap::new();
    let mut page = None;
    for page_index in 0..3 {
        let mut url = format!(
            "https://api.openai.com/v1/organization/costs?start_time={start}&bucket_width=1d&limit=31"
        );
        if let Some(page) = &page {
            url.push_str(&format!("&page={page}"));
        }
        let reply = get(http, &url, key)?;
        if reply.status == 403 {
            return Ok(CostsResult::AdminRequired);
        }
        if reply.status == 429 {
            let mut failure =
                Failure::new(FailureKind::RateLimited, "rate limited by OpenAI costs");
            failure.retry_after = reply.retry_after_secs.map(|seconds| {
                Timestamp::from_secs(now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX)))
            });
            return Err(failure);
        }
        if reply.status != 200 {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "OpenAI costs unavailable",
            ));
        }
        let body: CostPage = serde_json::from_str(&reply.body).map_err(|_| invalid_costs())?;
        for item in body.data.into_iter().flat_map(|b| b.results) {
            if item.amount.currency.len() != 3
                || !item
                    .amount
                    .currency
                    .bytes()
                    .all(|b| b.is_ascii_alphabetic())
            {
                return Err(invalid_costs());
            }
            let raw = item.amount.value.get();
            let raw = if raw.starts_with('"') {
                serde_json::from_str::<String>(raw).map_err(|_| invalid_costs())?
            } else {
                raw.into()
            };
            let value = Amount::parse(&raw).ok_or_else(invalid_costs)?;
            let currency = item.amount.currency.to_ascii_uppercase();
            let total = totals.entry(currency).or_insert_with(Amount::zero);
            *total = sum(total, &value).ok_or_else(invalid_costs)?;
        }
        if !body.has_more && body.next_page.is_none() {
            break;
        }
        if page_index == 2 {
            return Err(invalid_costs());
        }
        let Some(next) = body.next_page else {
            return Err(invalid_costs());
        };
        page = Some(super::source::encode_cost_cursor(&next).ok_or_else(invalid_costs)?);
    }
    let meters = totals
        .into_iter()
        .map(|(currency, amount)| {
            let mut meter = MoneyMeter::new(
                "spend.monthly",
                "reported spend (lags)",
                MoneyKind::Spend,
                amount,
                currency,
                MoneyScope::Organization,
            );
            let mut period = Period::of(PeriodKind::Monthly);
            period.start = Some(Timestamp::from_secs(start));
            period.end = Some(Timestamp::from_secs(now));
            meter.period = Some(period);
            meter
        })
        .collect();
    Ok(CostsResult::Meters(meters))
}
fn invalid_costs() -> Failure {
    Failure::new(
        FailureKind::InvalidResponse,
        "OpenAI costs response has an unknown shape",
    )
}
/// Exact checked decimal aggregation. Overflow is refused, never rounded.
fn sum(a: &Amount, b: &Amount) -> Option<Amount> {
    fn parts(a: &Amount) -> Option<(i128, u32)> {
        let raw = a.as_str();
        let scale = raw.split_once('.').map_or(0, |(_, f)| f.len());
        let digits = raw.replace('.', "");
        Some((digits.parse().ok()?, u32::try_from(scale).ok()?))
    }
    let (a, sa) = parts(a)?;
    let (b, sb) = parts(b)?;
    let scale = sa.max(sb);
    let total = a
        .checked_mul(10_i128.checked_pow(scale - sa)?)?
        .checked_add(b.checked_mul(10_i128.checked_pow(scale - sb)?)?)?;
    let digits = total.unsigned_abs().to_string();
    let padded = format!("{digits:0>width$}", width = scale as usize + 1);
    let split = padded.len() - scale as usize;
    let raw = if scale == 0 {
        padded
    } else {
        format!("{}.{}", &padded[..split], &padded[split..])
    };
    Amount::parse(&format!("{}{raw}", if total < 0 { "-" } else { "" }))
}
#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_openai.rs"]
mod tests;
