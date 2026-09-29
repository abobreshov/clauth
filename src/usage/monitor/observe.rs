//! Monitors as [`AccountObservation`]s: config + cache → one `monitor:<id>`
//! observation, the user budget meter, and the collector hook.
//!
//! Read-only: nothing here fetches or writes. Figures older than
//! [`super::cache::STALE_RETENTION_MS`] (7 days) are dropped (plan §3.2: last
//! good data stays visible while it is honestly stale, up to 7 days).

use super::cache::{MonitorCache, STALE_RETENTION_MS};
use super::config::MonitorConfig;
use super::source::{MonitorTarget, resolve_target, skeleton};
use crate::usage::collect::CollectCtx;
use crate::usage::derive::{Severity, account_severity};
use crate::usage::observation::{
    AccountObservation, Amount, Freshness, MoneyKind, MoneyMeter, MoneyScope, Period, PeriodKind,
    ScopeOrigin, Timestamp,
};

/// The budget meter's id.
pub(crate) const BUDGET_METER: &str = "budget.monthly";

/// The collector hook ([`crate::usage::collect::MONITOR_SOURCES`]): every
/// configured monitor off its cache. Disabled monitors only with
/// `include_disabled`. An unreadable `monitors.toml` yields none (`tollgate
/// monitor list` names the error).
pub(crate) fn monitor_observations(ctx: &CollectCtx<'_>) -> Vec<AccountObservation> {
    let Ok(monitors) = super::config::load() else {
        return Vec::new();
    };
    monitors
        .iter()
        .filter(|m| m.enabled || ctx.include_disabled)
        .map(|m| {
            let cache = super::cache::load(&m.id).filter(|c| c.matches(m));
            // Judged against the monitor's own TTL, not the profile cadence.
            observe_monitor(m, cache.as_ref(), ctx.now_ms, |at| {
                ctx.freshness_at_cadence(at, m.ttl_ms())
            })
        })
        .collect()
}

/// One monitor's observation at `now_ms`. `freshness` judges the age of the
/// cached figures (the collector's cadence rule).
pub(crate) fn observe_monitor(
    cfg: &MonitorConfig,
    cache: Option<&MonitorCache>,
    now_ms: u64,
    freshness: impl Fn(Option<u64>) -> Freshness,
) -> AccountObservation {
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    // The target only decides source id and auth kind here: no credential is
    // read (the env reader answers nothing) and no path is used.
    let mut target: MonitorTarget =
        resolve_target(cfg, std::path::Path::new(""), now_secs, &|_| None);
    target.monitoring_key = cfg.billing_key_env.is_some();
    let mut obs = skeleton(cfg, &target);
    let Some(cache) = cache else {
        attach_budget(&mut obs, cfg);
        return obs;
    };
    obs.checked_at = cache.checked_at_ms.map(Timestamp::from_ms);
    let observed = cache
        .observed_at_ms
        .filter(|at| now_ms.saturating_sub(*at) <= STALE_RETENTION_MS);
    if let (Some(at), Some(reading)) = (observed, cache.reading.as_ref()) {
        obs.plan = reading.plan.clone();
        obs.windows = reading
            .windows
            .iter()
            .filter(|w| w.resets_at.is_none_or(|t| now_secs < t.secs()))
            .cloned()
            .collect();
        obs.money = reading.money.clone();
        obs.best_effort = reading.best_effort;
        obs.observed_at = Some(Timestamp::from_ms(at));
        obs.freshness = freshness(Some(at));
        obs.failure = reading.verdict.clone();
    }
    if let Some(f) = &cache.failure {
        obs.failure = Some(f.clone());
    }
    attach_budget(&mut obs, cfg);
    obs
}

/// Append the `budget.monthly` meter when a budget is configured and this
/// month's spend is known.
fn attach_budget(obs: &mut AccountObservation, cfg: &MonitorConfig) {
    if let Some(m) = budget_meter(cfg, &obs.money) {
        obs.money.push(m);
    }
}

/// The user budget as a Budget meter: `amount` = budget − this month's USD
/// spend (negative once over), `limit` = the budget. `None` without a budget
/// or without a spend figure to measure against.
///
/// This month's spend is the `spend.monthly` USD meter when the source
/// publishes one (OpenRouter), else the used part of a monthly USD balance
/// pool with a known size (Nous `subscription`: limit − left).
pub(crate) fn budget_meter(cfg: &MonitorConfig, money: &[MoneyMeter]) -> Option<MoneyMeter> {
    let budget = cfg.budget_usd_month.clone()?;
    let usd = |m: &&MoneyMeter| m.currency.eq_ignore_ascii_case("USD");
    let (spent, period) = money
        .iter()
        .filter(usd)
        .find(|m| m.kind == MoneyKind::Spend && m.meter_id == "spend.monthly")
        .map(|m| (m.amount.clone(), m.period))
        .or_else(|| {
            money
                .iter()
                .filter(usd)
                .filter(|m| m.kind == MoneyKind::Balance)
                .filter(|m| m.period.is_some_and(|p| p.kind == PeriodKind::Monthly))
                .find_map(|m| Some((sub_exact(m.limit.as_ref()?, &m.amount)?, m.period)))
        })?;
    let mut m = MoneyMeter::new(
        BUDGET_METER,
        "Monthly budget",
        MoneyKind::Budget,
        sub_exact(&budget, &spent)?,
        "USD",
        MoneyScope::Profile,
    );
    m.limit = Some(budget);
    m.scope_origin = ScopeOrigin::UserLabel;
    m.additive = false;
    m.period = Some(period.unwrap_or_else(|| Period::of(PeriodKind::Monthly)));
    Some(m)
}

/// The spent share of a budget meter, percent (unclamped).
pub(crate) fn budget_spent_pct(m: &MoneyMeter) -> Option<f64> {
    let cap = m.limit.as_ref()?.to_f64();
    (cap > 0.0).then(|| (cap - m.amount.to_f64()) / cap * 100.0)
}

/// A budget meter's severity: its spent share on the usage ladder, critical
/// once nothing is left. The core grades budgets
/// ([`crate::usage::derive::meter_severity`]); this is that rung for a
/// `Budget` meter only.
pub(crate) fn budget_severity(m: &MoneyMeter) -> Option<Severity> {
    if m.kind != MoneyKind::Budget {
        return None;
    }
    crate::usage::derive::meter_severity(m).map(|(s, _)| s)
}

/// A monitor's severity: the account's own ([`account_severity`], no pace),
/// which folds its budget in like every other surface.
pub(crate) fn monitor_severity(obs: &AccountObservation, now_secs: i64) -> Option<Severity> {
    account_severity(obs, now_secs, false)
}

/// `a − b`, exactly. `None` past 18 fractional digits or i64 range.
pub(crate) fn sub_exact(a: &Amount, b: &Amount) -> Option<Amount> {
    fn scaled(x: &Amount, scale: usize) -> Option<i128> {
        let s = x.as_str();
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (int, frac) = body.split_once('.').unwrap_or((body, ""));
        let digits = format!("{int}{frac:0<scale$}");
        let v: i128 = digits.parse().ok()?;
        Some(if neg { -v } else { v })
    }
    let frac_len = |x: &Amount| x.as_str().split_once('.').map_or(0, |(_, f)| f.len());
    let scale = frac_len(a).max(frac_len(b));
    if scale > 18 {
        return None;
    }
    let diff = scaled(a, scale)?.checked_sub(scaled(b, scale)?)?;
    Some(Amount::from_minor(
        i64::try_from(diff).ok()?,
        u32::try_from(scale).ok()?,
    ))
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_observe.rs"]
mod tests;
