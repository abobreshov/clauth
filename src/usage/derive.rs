//! Read-time derivations over [`crate::usage::observation`] (plan v3.1 §4.1,
//! §4.5): severity, pace, countdowns, the lead window and money strings.
//!
//! Every function is pure (the clock is a parameter, except [`local_hhmm`],
//! which reads the machine's zone) and pinned by the fixture tables under
//! `tests/fixtures/` (`countdown.json`, `severity.json`, `pace.json`), so the
//! CLI, TUI, JSON and herdr surfaces render one spelling.

use serde::{Deserialize, Serialize};

use super::observation::{
    AccountObservation, Amount, FailureKind, MoneyKind, MoneyMeter, QuotaWindow, Timestamp,
    WINDOW_SESSION,
};

// ── Severity ───────────────────────────────────────────────────────────────────

/// One rung of the severity ladder, ordered `Ok < Mid < High < Critical` so
/// [`worst_of`] is a `max`. Serialises lower-case, which is also the Waybar
/// `class` spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Ok,
    Mid,
    High,
    Critical,
}

/// What a severity was judged from, which picks its word: a used share reads
/// `HIGH` at the third rung, a balance reads `LOW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SeverityBasis {
    Usage,
    Balance,
}

impl Severity {
    /// Lower-case class name (`ok`, `mid`, `high`, `critical`).
    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Mid => "mid",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// The word shown beside the colour (plan §3.3: colour never carries
    /// meaning alone). Usage: `ok`, `mid`, `HIGH`, `CRITICAL`. Balance: `ok`,
    /// `mid`, `LOW`, `CRITICAL`.
    pub(crate) fn word(self, basis: SeverityBasis) -> &'static str {
        match (self, basis) {
            (Self::Ok, _) => "ok",
            (Self::Mid, _) => "mid",
            (Self::High, SeverityBasis::Usage) => "HIGH",
            (Self::High, SeverityBasis::Balance) => "LOW",
            (Self::Critical, _) => "CRITICAL",
        }
    }
}

/// Severity of a used share, percent: `< 50` ok, `≥ 50` mid, `≥ 75` high,
/// `≥ 90` critical. Unclamped input; NaN reads ok (callers pass `None` for an
/// unknown share rather than NaN).
pub(crate) fn severity_from_used_pct(pct: f64) -> Severity {
    if pct >= 90.0 {
        Severity::Critical
    } else if pct >= 75.0 {
        Severity::High
    } else if pct >= 50.0 {
        Severity::Mid
    } else {
        Severity::Ok
    }
}

/// Severity of a remaining balance, compared EXACTLY: `≥ 20` ok, `< 20` mid,
/// `< 5` high (`LOW`), `< 1` or negative critical. Only USD has a ladder;
/// any other currency is `None` (no graded severity, never a guess).
pub(crate) fn severity_from_balance(amount: &Amount, currency: &str) -> Option<Severity> {
    if !currency.eq_ignore_ascii_case("USD") {
        return None;
    }
    let at_least = |n: i64| *amount >= Amount::from_minor(n, 0);
    Some(if at_least(20) {
        Severity::Ok
    } else if at_least(5) {
        Severity::Mid
    } else if at_least(1) {
        Severity::High
    } else {
        Severity::Critical
    })
}

/// Severity of a pace delta in points (plan §4.5 table): `< −10` ok,
/// `≥ −10` mid, `> 0` high, `≥ +10` critical.
pub(crate) fn severity_from_pace(delta: f64) -> Severity {
    if delta >= 10.0 {
        Severity::Critical
    } else if delta > 0.0 {
        Severity::High
    } else if delta >= -10.0 {
        Severity::Mid
    } else {
        Severity::Ok
    }
}

/// The worst severity of `items`; `None` when there are none.
pub(crate) fn worst_of(items: impl IntoIterator<Item = Severity>) -> Option<Severity> {
    items.into_iter().max()
}

/// A window's severity: `Critical` when exhausted, else its used share's rung;
/// `None` when the share is unknown.
pub(crate) fn window_severity(w: &QuotaWindow) -> Option<Severity> {
    if w.exhausted {
        return Some(Severity::Critical);
    }
    w.used_pct.map(severity_from_used_pct)
}

/// A money meter's severity and the basis that words it.
///
/// - `balance`: the USD balance ladder ([`severity_from_balance`]).
/// - `limit` (amount = what is left under the cap): left `≤ 0` is critical;
///   otherwise the used share `(cap − left) / cap` on the usage ladder.
/// - `spend` and `budget`: not graded (`None`) — "used % of lifetime
///   purchases" is never a severity (plan §4.5).
pub(crate) fn meter_severity(m: &MoneyMeter) -> Option<(Severity, SeverityBasis)> {
    match m.kind {
        MoneyKind::Balance => {
            severity_from_balance(&m.amount, &m.currency).map(|s| (s, SeverityBasis::Balance))
        }
        MoneyKind::Limit => {
            if m.amount <= Amount::zero() {
                return Some((Severity::Critical, SeverityBasis::Usage));
            }
            let cap = m.limit.as_ref()?.to_f64();
            if cap <= 0.0 {
                return None;
            }
            let used_pct = (cap - m.amount.to_f64()) / cap * 100.0;
            Some((severity_from_used_pct(used_pct), SeverityBasis::Usage))
        }
        MoneyKind::Spend | MoneyKind::Budget => None,
    }
}

/// The account's overall severity: the worst of every window
/// ([`window_severity`]), every money meter ([`meter_severity`]) and a
/// `QuotaExhausted` failure (critical). With `include_pace`, each window's
/// pace ([`severity_from_pace`]) folds in too, as plan §4.1 specifies; a
/// compact surface may leave it out. `None` when nothing is graded.
pub(crate) fn account_severity(
    obs: &AccountObservation,
    now_secs: i64,
    include_pace: bool,
) -> Option<Severity> {
    let windows = obs.windows.iter().filter_map(window_severity);
    let paces = obs
        .windows
        .iter()
        .filter(|_| include_pace)
        .filter_map(|w| window_pace(w, now_secs))
        .map(|p| severity_from_pace(p.delta));
    let money = obs.money.iter().filter_map(meter_severity).map(|(s, _)| s);
    let failure = obs
        .failure
        .as_ref()
        .filter(|f| f.kind == FailureKind::QuotaExhausted)
        .map(|_| Severity::Critical);
    worst_of(windows.chain(paces).chain(money).chain(failure))
}

// ── Pace ───────────────────────────────────────────────────────────────────────

/// Half-width of the on-pace band, in points.
pub(crate) const PACE_BAND_PTS: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PaceVerdict {
    /// Burning faster than the window elapses (`delta > +5`).
    Ahead,
    /// Within ±5 points, bounds inclusive.
    OnPace,
    /// Burning slower (`delta < −5`).
    Under,
}

/// A window's pace: `delta = used − elapsed`, both in percent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct Pace {
    pub(crate) delta: f64,
    pub(crate) elapsed_pct: f64,
    pub(crate) verdict: PaceVerdict,
}

impl Pace {
    /// `12 pts ahead`, `on pace`, `6 pts under`, `1 pt ahead`. Points
    /// truncate toward zero.
    pub(crate) fn label(&self) -> String {
        let pts = self.delta.abs().trunc() as i64;
        let unit = if pts == 1 { "pt" } else { "pts" };
        match self.verdict {
            PaceVerdict::Ahead => format!("{pts} {unit} ahead"),
            PaceVerdict::OnPace => "on pace".to_string(),
            PaceVerdict::Under => format!("{pts} {unit} under"),
        }
    }

    /// `↑` ahead, `→` on pace, `↓` under.
    pub(crate) fn glyph(&self) -> char {
        match self.verdict {
            PaceVerdict::Ahead => '↑',
            PaceVerdict::OnPace => '→',
            PaceVerdict::Under => '↓',
        }
    }
}

/// Pace from a used share and an elapsed share, both percent.
pub(crate) fn pace(used_pct: f64, elapsed_pct: f64) -> Pace {
    let delta = used_pct - elapsed_pct;
    let verdict = if delta > PACE_BAND_PTS {
        PaceVerdict::Ahead
    } else if delta < -PACE_BAND_PTS {
        PaceVerdict::Under
    } else {
        PaceVerdict::OnPace
    };
    Pace {
        delta,
        elapsed_pct,
        verdict,
    }
}

/// Share of a `window_secs` window elapsed at `now_secs`, given its reset,
/// clamped to `0..=100`.
pub(crate) fn elapsed_pct(resets_at: Timestamp, window_secs: u64, now_secs: i64) -> f64 {
    let len = i64::try_from(window_secs).unwrap_or(i64::MAX).max(1);
    let remaining = (resets_at.secs() - now_secs).clamp(0, len);
    (len - remaining) as f64 / len as f64 * 100.0
}

/// A window's pace; `None` without a used share, a reset time or a length.
pub(crate) fn window_pace(w: &QuotaWindow, now_secs: i64) -> Option<Pace> {
    let used = w.used_pct?;
    let elapsed = elapsed_pct(w.resets_at?, w.window_secs?, now_secs);
    Some(pace(used, elapsed))
}

// ── Countdown ──────────────────────────────────────────────────────────────────

/// The unknown-time placeholder.
pub(crate) const UNKNOWN: &str = "—";

/// Time left as `4d 1h`, `3h 05m`, `5m`, `now` (under a minute, or past),
/// `—` (unknown). Every unit truncates — never rounds up — so a countdown
/// never promises a reset sooner than it comes.
pub(crate) fn countdown(secs: Option<i64>) -> String {
    let Some(secs) = secs else {
        return UNKNOWN.to_string();
    };
    if secs < 60 {
        return "now".to_string();
    }
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{days}d {}h", hours % 24)
    } else if hours > 0 {
        format!("{hours}h {:02}m", mins % 60)
    } else {
        format!("{mins}m")
    }
}

/// [`countdown`] to `resets_at` from `now_secs`.
pub(crate) fn countdown_to(resets_at: Option<Timestamp>, now_secs: i64) -> String {
    countdown(resets_at.map(|t| t.secs() - now_secs))
}

/// `HH:MM` of `at` at a fixed UTC offset — the pure half of [`local_hhmm`].
pub(crate) fn hhmm_at_offset(at: Timestamp, offset_secs: i32) -> String {
    let local = at.secs() + i64::from(offset_secs);
    let day_secs = local.rem_euclid(86_400);
    format!("{:02}:{:02}", day_secs / 3600, (day_secs % 3600) / 60)
}

/// `HH:MM` of `at` in the machine's local zone.
pub(crate) fn local_hhmm(at: Timestamp) -> String {
    use chrono::{Local, Offset, TimeZone};
    let offset = Local
        .timestamp_opt(at.secs(), 0)
        .single()
        .map(|dt| dt.offset().fix().local_minus_utc())
        .unwrap_or(0);
    hhmm_at_offset(at, offset)
}

/// `3h 05m (14:32)` in the local zone; `—` alone when the reset is unknown.
pub(crate) fn countdown_with_local(resets_at: Option<Timestamp>, now_secs: i64) -> String {
    match resets_at {
        Some(at) => format!("{} ({})", countdown_to(Some(at), now_secs), local_hhmm(at)),
        None => UNKNOWN.to_string(),
    }
}

// ── Lead window ────────────────────────────────────────────────────────────────

/// The one window a compact row shows: the session window when there is one;
/// else the worst critical window (highest used share, exhausted first); else
/// the window resetting soonest (future resets only); else the first window.
pub(crate) fn lead_window(windows: &[QuotaWindow], now_secs: i64) -> Option<&QuotaWindow> {
    if let Some(w) = windows.iter().find(|w| w.id == WINDOW_SESSION) {
        return Some(w);
    }
    let critical = windows
        .iter()
        .filter(|w| window_severity(w) == Some(Severity::Critical))
        .max_by(|a, b| {
            a.exhausted.cmp(&b.exhausted).then(
                a.used_pct
                    .unwrap_or(f64::NEG_INFINITY)
                    .total_cmp(&b.used_pct.unwrap_or(f64::NEG_INFINITY)),
            )
        });
    if critical.is_some() {
        return critical;
    }
    windows
        .iter()
        .filter(|w| w.resets_at.is_some_and(|t| t.secs() > now_secs))
        .min_by_key(|w| w.resets_at.map(Timestamp::secs))
        .or_else(|| windows.first())
}

// ── Money ──────────────────────────────────────────────────────────────────────

/// The symbol for a currency code rendered symbol-first.
pub(crate) fn currency_symbol(code: &str) -> Option<&'static str> {
    match code.to_ascii_uppercase().as_str() {
        "USD" => Some("$"),
        "EUR" => Some("€"),
        "GBP" => Some("£"),
        "CNY" => Some("¥"),
        _ => None,
    }
}

/// Money at two decimals, rounded half away from zero, sign OUTSIDE the
/// symbol: `$13.67`, `-$5.71`, `€0.50`, `¥1132.60`; any other code trails:
/// `12.00 XYZ`, `-3.50 XYZ`. A debt that rounds to zero keeps its sign
/// (`-$0.00`), because the account is still overdrawn.
pub(crate) fn format_money(amount: &Amount, currency: &str) -> String {
    let sign = if amount.is_negative() { "-" } else { "" };
    let figure = amount.abs().round_dp(2);
    match currency_symbol(currency) {
        Some(sym) => format!("{sign}{sym}{figure}"),
        None => format!("{sign}{figure} {}", currency.to_ascii_uppercase()),
    }
}

#[cfg(test)]
#[path = "../../tests/inline/usage_derive.rs"]
mod tests;
