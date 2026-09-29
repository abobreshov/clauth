//! Read-time projections of the EXISTING usage caches onto the observation
//! model: the Anthropic OAuth [`UsageInfo`], the codex [`UsageInfo`] (same
//! type, codex fields) and the third-party [`ThirdPartyStats`].
//!
//! Pure and side-effect free — they never fetch, never write and never change
//! what the chain reads; the scheduler's own types stay the source of truth
//! and these only translate. [`crate::usage::collect`] reads the caches and
//! calls them.
//!
//! Lapsed windows (a `resets_at` already past at `now_secs`) are dropped, the
//! same rule `status.json` applies (#74): past its reset a figure is the
//! previous window's last reading, not a current one.

use super::fetch::{LABEL_5H, LABEL_7D, UsageInfo, UsageWindow};
use super::observation::{
    AccountObservation, Amount, Failure, FailureKind, MoneyKind, MoneyMeter, MoneyScope, Period,
    PeriodKind, QuotaWindow, SESSION_WINDOW_SECS, Timestamp, WEEKLY_WINDOW_SECS, WINDOW_SESSION,
    WINDOW_WEEKLY, WINDOW_WEEKLY_MODEL_PREFIX, WindowScope,
};
use crate::providers::{StatRowKind, ThirdPartyStats, UsageBar};

// ── UsageInfo (Anthropic OAuth + codex) ────────────────────────────────────────

/// The quota windows of a [`UsageInfo`]: `five_hour` → `session` (5h),
/// `seven_day` → `weekly` (7d), both shared-pool and chain-eligible; each
/// `weekly_scoped` window → `weekly:<model>` (model-scoped, not
/// chain-eligible). Lapsed windows are dropped.
pub(crate) fn usage_windows(usage: &UsageInfo, now_secs: i64) -> Vec<QuotaWindow> {
    let mut out = Vec::new();
    if let Some(w) = &usage.five_hour {
        out.push(fold_window(
            WINDOW_SESSION,
            LABEL_5H,
            w,
            SESSION_WINDOW_SECS,
            WindowScope::Shared,
            true,
        ));
    }
    if let Some(w) = &usage.seven_day {
        out.push(fold_window(
            WINDOW_WEEKLY,
            LABEL_7D,
            w,
            WEEKLY_WINDOW_SECS,
            WindowScope::Shared,
            true,
        ));
    }
    for s in &usage.weekly_scoped {
        let model = s
            .label
            .strip_prefix(LABEL_7D)
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(s.label.as_str())
            .to_string();
        out.push(fold_window(
            &format!("{WINDOW_WEEKLY_MODEL_PREFIX}{model}"),
            &s.label,
            &s.window,
            WEEKLY_WINDOW_SECS,
            WindowScope::Model {
                models: vec![model.clone()],
            },
            false,
        ));
    }
    out.retain(|w| is_live(w.resets_at, now_secs));
    out
}

fn fold_window(
    id: &str,
    label: &str,
    w: &UsageWindow,
    window_secs: u64,
    scope: WindowScope,
    chain_eligible: bool,
) -> QuotaWindow {
    let mut q = QuotaWindow::new(id, label, scope);
    q.used_pct = w.utilization.is_finite().then_some(w.utilization);
    q.exhausted = w.utilization >= 100.0;
    q.resets_at = w.resets_at.as_deref().and_then(Timestamp::parse);
    q.window_secs = Some(window_secs);
    q.chain_eligible = chain_eligible;
    q
}

/// Live unless its reset is known and already past.
fn is_live(resets_at: Option<Timestamp>, now_secs: i64) -> bool {
    resets_at.is_none_or(|t| now_secs < t.secs())
}

/// The money meters of an Anthropic [`UsageInfo`]:
///
/// - `spend` (when enabled or capped) → `spend.extra`: Spend, organisation
///   scope, monthly, `limit` = the cap. Already in major units.
/// - legacy `extra_usage` (only when enabled AND no visible `spend` block,
///   the same precedence the Usage tab applies) → `spend.extra`, converted
///   from minor units (cents) exactly; its `daily` / `weekly` sub-objects →
///   `spend.extra.daily` / `spend.extra.weekly` as published.
/// - each `window_dollars` row with a `used` figure → `window.<label>`.
pub(crate) fn usage_money(usage: &UsageInfo) -> Vec<MoneyMeter> {
    let mut out = Vec::new();
    let spend_shown = usage.spend.as_ref().is_some_and(|s| s.is_visible());
    if let Some(spend) = usage.spend.as_ref().filter(|s| s.is_visible())
        && let Some(used) = spend.used.and_then(Amount::from_f64)
    {
        let mut m = MoneyMeter::new(
            "spend.extra",
            "Extra usage",
            MoneyKind::Spend,
            used,
            spend.currency.as_deref().unwrap_or("USD"),
            MoneyScope::Organization,
        );
        m.limit = spend.limit.and_then(Amount::from_f64);
        m.period = Some(Period::of(PeriodKind::Monthly));
        out.push(m);
    }
    if let Some(extra) = &usage.extra_usage {
        let currency = extra.currency.as_deref().unwrap_or("USD");
        if extra.is_enabled
            && !spend_shown
            && let Some(used) = extra.used_credits.and_then(Amount::from_f64)
        {
            let mut m = MoneyMeter::new(
                "spend.extra",
                "Extra usage",
                MoneyKind::Spend,
                used.scaled_down(2),
                currency,
                MoneyScope::Organization,
            );
            m.limit = extra
                .monthly_limit
                .and_then(Amount::from_f64)
                .map(|l| l.scaled_down(2));
            m.period = Some(Period::of(PeriodKind::Monthly));
            out.push(m);
        }
        for (suffix, label, kind, raw) in [
            (
                "daily",
                "Extra usage (24h)",
                PeriodKind::Daily,
                &extra.daily,
            ),
            (
                "weekly",
                "Extra usage (7d)",
                PeriodKind::Weekly,
                &extra.weekly,
            ),
        ] {
            let Some(period) = raw.as_ref().and_then(super::fetch::ExtraPeriod::from_value) else {
                continue;
            };
            let Some(used) = period.used_credits.and_then(Amount::from_f64) else {
                continue;
            };
            let mut m = MoneyMeter::new(
                format!("spend.extra.{suffix}"),
                label,
                MoneyKind::Spend,
                used,
                period.currency.as_deref().unwrap_or(currency),
                MoneyScope::Organization,
            );
            m.limit = period.monthly_limit.and_then(Amount::from_f64);
            m.period = Some(Period::of(kind));
            out.push(m);
        }
    }
    for d in &usage.window_dollars {
        let Some(used) = d.used.and_then(Amount::from_f64) else {
            continue;
        };
        let mut m = MoneyMeter::new(
            format!("window.{}", d.label),
            format!("{} spend", d.label),
            MoneyKind::Spend,
            used,
            "USD",
            MoneyScope::Organization,
        );
        m.limit = d.limit.and_then(Amount::from_f64);
        out.push(m);
    }
    out
}

/// Fill `obs` from an Anthropic OAuth [`UsageInfo`]: windows, money, the plan
/// (when `obs.plan` is still unset) and a `SubscriptionInactive` failure for a
/// canceled subscription (when no failure is set yet).
pub(crate) fn apply_oauth_usage(obs: &mut AccountObservation, usage: &UsageInfo, now_secs: i64) {
    obs.windows = usage_windows(usage, now_secs);
    obs.money = usage_money(usage);
    if obs.plan.is_none() {
        obs.plan = usage.plan.as_ref().and_then(|p| p.tier.short_label());
    }
    if obs.failure.is_none() && usage.plan.as_ref().is_some_and(|p| p.is_canceled()) {
        obs.failure = Some(Failure::new(
            FailureKind::SubscriptionInactive,
            "subscription canceled",
        ));
    }
}

/// Fill `obs` from a codex [`UsageInfo`]: windows (the two slots are
/// chain-eligible, the codex chain judges them), the ChatGPT plan (when unset),
/// banked resets, and `QuotaExhausted` with the server's reason when the
/// account is blocked (when no failure is set yet).
pub(crate) fn apply_codex_usage(obs: &mut AccountObservation, usage: &UsageInfo, now_secs: i64) {
    obs.windows = usage_windows(usage, now_secs);
    obs.windows.extend(
        usage
            .codex_additional_windows
            .iter()
            .filter(|w| is_live(w.resets_at, now_secs))
            .cloned(),
    );
    if let Some(credits) = &usage.codex_credits
        && credits.unlimited != Some(true)
        && let Some(balance) = credits.balance.as_deref().and_then(Amount::parse)
    {
        obs.money.push(MoneyMeter::new(
            "codex.credits",
            "Codex credits",
            MoneyKind::Balance,
            balance,
            "credits",
            MoneyScope::Profile,
        ));
    }
    if obs.failure.is_none() && usage.codex_spend_control_reached == Some(true) {
        obs.failure = Some(Failure::new(
            FailureKind::QuotaExhausted,
            "Codex spend control reached",
        ));
    }
    if obs.plan.is_none() {
        obs.plan = usage.plan.as_ref().and_then(|p| p.codex_plan.clone());
    }
    obs.banked_resets = usage.codex_reset_credits;
    if obs.failure.is_none()
        && let Some(reason) = &usage.codex_limit_reached
    {
        obs.failure = Some(Failure::new(
            FailureKind::QuotaExhausted,
            &reason.replace('_', " "),
        ));
    }
}

// ── ThirdPartyStats ────────────────────────────────────────────────────────────

/// The quota windows of a third-party cache: one per bar. `5h` / `7d` bars map
/// to `session` / `weekly` (shared, chain-eligible unless `best_effort`, the
/// same rule as [`ThirdPartyStats::to_usage_info`]); any other bar gets a slug
/// of its label as id and account scope. Lengths come from the label
/// (`30d` → 2 592 000 s). A bar's `used` / `total` ride along. Lapsed bars are
/// dropped.
pub(crate) fn third_party_windows(stats: &ThirdPartyStats, now_secs: i64) -> Vec<QuotaWindow> {
    stats
        .bars
        .iter()
        .map(|b| bar_window(b, stats.best_effort))
        .filter(|w| is_live(w.resets_at, now_secs))
        .collect()
}

fn bar_window(b: &UsageBar, best_effort: bool) -> QuotaWindow {
    let (id, scope, chain) = match b.label.as_str() {
        LABEL_5H => (WINDOW_SESSION.to_string(), WindowScope::Shared, true),
        LABEL_7D => (WINDOW_WEEKLY.to_string(), WindowScope::Shared, true),
        other => (slug(other), WindowScope::Account, false),
    };
    let mut q = QuotaWindow::new(id, &b.label, scope);
    q.used_pct = b.pct.is_finite().then_some(b.pct);
    q.exhausted = b.pct >= 100.0;
    q.resets_at = b.resets_at.as_deref().and_then(Timestamp::parse);
    q.window_secs = label_secs(&b.label);
    q.chain_eligible = chain && !best_effort;
    q.used = b.used;
    q.limit = b.total;
    q
}

/// `5h` → 18 000, `7d` → 604 800, `30d`, `1w`, `90m`, `24h`; `None` otherwise.
pub(crate) fn label_secs(label: &str) -> Option<u64> {
    let label = label.trim();
    let unit = label.chars().last()?;
    let n: u64 = label[..label.len() - unit.len_utf8()].parse().ok()?;
    let per = match unit {
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        _ => return None,
    };
    n.checked_mul(per).filter(|s| *s > 0)
}

/// Lower-case ASCII slug: runs of anything but `[a-z0-9]` become one `_`.
fn slug(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "window".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parse a cached money row value — the `"<decimal> <CODE>"` spelling the
/// providers write (`"31.45 CNY"`, `"-0.20 USD"`) — into an exact amount and
/// an upper-case three-letter ISO 4217 code (so `"84 days"` is not money). The cache holds no raw numbers, so this row
/// string IS the rawest form on disk; the DeepSeek value is the provider's own
/// decimal string verbatim, the OpenRouter one its `{:.2}` rendering.
pub(crate) fn parse_money_value(value: &str) -> Option<(Amount, String)> {
    let mut parts = value.split_whitespace();
    let amount = Amount::parse(parts.next()?)?;
    let code = parts.next()?;
    if parts.next().is_some() || code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((amount, code.to_ascii_uppercase()))
}

/// The money meters of a third-party cache, from its money-valued rows.
///
/// Known row labels map to typed meters: the wallet row (`api balance`, or
/// legacy `total`) → `wallet` (Balance, additive); DeepSeek's `granted` /
/// `topped up` → `wallet.granted` / `wallet.topped_up` (Balance, components of
/// the wallet, so `additive = false`); OpenRouter's `used` + `purchased` →
/// `spend.lifetime` (Spend, `limit` = purchased), `today` / `this week` /
/// `this month` → `spend.daily|weekly|monthly` (key scope), `key limit` +
/// `key limit left` → `key_limit` (Limit, amount = left, `limit` = cap).
/// An unknown label with a money value becomes `row.<slug>`, Balance when the
/// label names a balance / credit / remaining figure, Spend when it names
/// spend / cost / usage, and is skipped otherwise. Rows that are not
/// `"<decimal> <CODE>"` (token counts, day counts, headings) are skipped.
pub(crate) fn third_party_money(stats: &ThirdPartyStats) -> Vec<MoneyMeter> {
    let mut out: Vec<MoneyMeter> = Vec::new();
    let mut used_lifetime: Option<(Amount, String)> = None;
    let mut purchased: Option<(Amount, String)> = None;
    let mut key_cap: Option<(Amount, String)> = None;
    let mut key_left: Option<(Amount, String)> = None;
    for row in &stats.rows {
        if matches!(row.kind, StatRowKind::Heading) {
            continue;
        }
        let Some((amount, currency)) = parse_money_value(&row.value) else {
            continue;
        };
        let label = row.label.as_str();
        let (meter_id, kind, scope, period, additive) = match label {
            l if crate::providers::is_balance_row(l) => (
                "wallet".to_string(),
                MoneyKind::Balance,
                MoneyScope::Profile,
                None,
                true,
            ),
            "granted" => (
                "wallet.granted".to_string(),
                MoneyKind::Balance,
                MoneyScope::Profile,
                None,
                false,
            ),
            "topped up" => (
                "wallet.topped_up".to_string(),
                MoneyKind::Balance,
                MoneyScope::Profile,
                None,
                false,
            ),
            "used" => {
                used_lifetime = Some((amount, currency));
                continue;
            }
            "purchased" => {
                purchased = Some((amount, currency));
                continue;
            }
            "key limit" => {
                key_cap = Some((amount, currency));
                continue;
            }
            "key limit left" => {
                key_left = Some((amount, currency));
                continue;
            }
            "today" => (
                "spend.daily".to_string(),
                MoneyKind::Spend,
                MoneyScope::Key,
                Some(PeriodKind::Daily),
                true,
            ),
            "this week" => (
                "spend.weekly".to_string(),
                MoneyKind::Spend,
                MoneyScope::Key,
                Some(PeriodKind::Weekly),
                true,
            ),
            "this month" => (
                "spend.monthly".to_string(),
                MoneyKind::Spend,
                MoneyScope::Key,
                Some(PeriodKind::Monthly),
                true,
            ),
            other => {
                let lower = other.to_ascii_lowercase();
                let kind = if ["balance", "credit", "remaining"]
                    .iter()
                    .any(|w| lower.contains(w))
                {
                    MoneyKind::Balance
                } else if ["spend", "spent", "cost", "usage"]
                    .iter()
                    .any(|w| lower.contains(w))
                {
                    MoneyKind::Spend
                } else {
                    continue;
                };
                (
                    format!("row.{}", slug(other)),
                    kind,
                    MoneyScope::Profile,
                    None,
                    false,
                )
            }
        };
        let mut m = MoneyMeter::new(meter_id, row_label(label), kind, amount, currency, scope);
        m.period = period.map(Period::of);
        m.additive = additive;
        out.push(m);
    }
    if let Some((used, currency)) = used_lifetime {
        let mut m = MoneyMeter::new(
            "spend.lifetime",
            "Lifetime spend",
            MoneyKind::Spend,
            used,
            currency,
            MoneyScope::Profile,
        );
        m.limit = purchased.map(|(a, _)| a);
        m.period = Some(Period::of(PeriodKind::Lifetime));
        out.push(m);
    }
    if let Some((left, currency)) = key_left {
        let mut m = MoneyMeter::new(
            "key_limit",
            "Key cap",
            MoneyKind::Limit,
            left,
            currency,
            MoneyScope::Key,
        );
        m.limit = key_cap.map(|(a, _)| a);
        out.push(m);
    }
    out
}

/// Human meter label for a row label.
fn row_label(label: &str) -> String {
    match label {
        l if crate::providers::is_balance_row(l) => "Balance".to_string(),
        "granted" => "Granted".to_string(),
        "topped up" => "Topped up".to_string(),
        "today" => "Spend today".to_string(),
        "this week" => "Spend this week".to_string(),
        "this month" => "Spend this month".to_string(),
        other => other.to_string(),
    }
}

/// Fill `obs` from a third-party cache: windows, money, the plan (when unset),
/// `best_effort`, and `QuotaExhausted` ("balance too low") when the provider
/// says the account cannot fund a call (when no failure is set yet).
pub(crate) fn apply_third_party(
    obs: &mut AccountObservation,
    stats: &ThirdPartyStats,
    now_secs: i64,
) {
    match &stats.observed {
        // A typed provider's meters from its raw JSON numbers: exact, so the
        // rounded display rows are not re-parsed (OpenRouter v2).
        Some(observed) => {
            obs.windows = observed
                .windows
                .iter()
                .filter(|w| is_live(w.resets_at, now_secs))
                .cloned()
                .collect();
            obs.money = observed.money.clone();
        }
        None => {
            obs.windows = third_party_windows(stats, now_secs);
            obs.money = third_party_money(stats);
        }
    }
    if obs.plan.is_none() {
        obs.plan = stats.plan.clone();
    }
    obs.best_effort = stats.best_effort;
    if obs.failure.is_none() && !stats.is_available {
        obs.failure = Some(Failure::new(
            FailureKind::QuotaExhausted,
            crate::providers::LOW_BALANCE,
        ));
    }
}

#[cfg(test)]
#[path = "../../tests/inline/usage_project.rs"]
mod tests;
