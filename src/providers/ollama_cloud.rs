//! Ollama Cloud provider: quota windows, per-model request counts and the
//! recent-spend figure from `GET https://ollama.com/api/usage`.
//!
//! The route is undocumented. It is what the ollama.com settings page calls,
//! and it takes the same `Authorization: Bearer <key>` inference key a profile
//! already holds (keys minted at <https://ollama.com/settings/keys>). Two body
//! shapes are known, and both parse (plan v3.1 §4.7):
//!
//! - **Legacy**: `limits.session` + `limits.weekly`, the 5-hour and 7-day
//!   windows. They map to the `5h` / `7d` bars, which
//!   [`ThirdPartyStats::to_usage_info`] hands to the chain.
//! - **Monthly pool** (new pricing): `limits.monthly` alone. It maps to a
//!   `month` bar that the chain never reads.
//!
//! Each window's `usage` is a FRACTION (`0.819` = 81.9 %). It becomes the bar's
//! percent unclamped (`1.2` → 120 %), because an overdrawn window is a fact a
//! reader needs, and a missing `usage` produces no bar at all (a
//! `usage not reported` row instead), never an invented 0 %. The API
//! publishes no reset instant for any window.
//!
//! `limits.<window>.models[]` carries per-model `request_count`s, rendered as
//! a `<label> requests` block. One malformed model row is skipped and never
//! fails the parse. `activity.cost` is a dollar STRING (`"0.00000"`) for
//! `activity.period` (`last_4_weeks`, with `starting_at` / `ending_at`), kept
//! exact as a `spend (last 4 weeks)` row. `activity.models` is ignored.
//!
//! Failures: 401 → [`ThirdPartyError::AuthExpired`]; a 429 whose body names a
//! usage limit (`"you've reached your session usage limit"`) →
//! [`ThirdPartyError::QuotaExhausted`], any other 429 →
//! [`ThirdPartyError::RateLimited`]; a 200 body in neither shape →
//! [`ThirdPartyError::Parse`].
//!
//! **The local daemon** (`http://127.0.0.1:11434` / `http://localhost:11434`)
//! is a different transport to the same service: it signs requests with its
//! own key (`ollama signin`, machine-global, never read here) and has no
//! `/api/usage`. A profile pointed at it is typed [`super::Provider::OllamaDaemon`]
//! rather than handed to the generic scanner, and its "fetch" answers without
//! touching the network: usage needs an ollama.com API key.
//!
//! [`fetch_ollama_cloud_usage`] is the one fetch path, shared by the profile
//! scheduler and any monitoring source; it takes its HTTP transport as a
//! parameter so tests drive it with canned replies and no socket.

use std::time::Duration;

use serde_json::Value;

use super::{StatRow, StatRowKind, ThirdPartyError, ThirdPartyStats, UsageBar, url_matches_host};
use crate::usage::observation::{
    AccountObservation, Amount, AuthKind, Failure, FailureKind, Freshness, ModelCount, MoneyKind,
    MoneyMeter, MoneyScope, Period, PeriodKind, QuotaWindow, SourceId, Timestamp, WINDOW_MONTH,
    WINDOW_SESSION, WINDOW_WEEKLY, WindowScope,
};

pub(super) const DISPLAY_NAME: &str = "Ollama Cloud";

/// Display name of a profile routed through the local daemon.
pub(super) const DAEMON_DISPLAY_NAME: &str = "Ollama daemon";

pub(super) const ORIGIN: &str = "https://ollama.com";

/// The usage route: same origin as inference, same key.
pub(crate) const USAGE_URL: &str = "https://ollama.com/api/usage";

/// Where an operator mints the inference key, per
/// <https://docs.ollama.com/api/authentication> ("create an API key at
/// ollama.com/settings/keys").
pub(super) const CONSOLE_URL: &str = "https://ollama.com/settings/keys";

/// The two spellings `ollama serve` listens on by default. Anything else on
/// port 11434 is some other program and stays generic.
const DAEMON_ORIGINS: [&str; 2] = ["http://127.0.0.1:11434", "http://localhost:11434"];

/// Pacing key for the daemon arm. It never sends a request, so this only has
/// to be stable and distinct from every real host.
pub(super) const DAEMON_ORIGIN: &str = "http://127.0.0.1:11434";

/// Bar labels. `5h` / `7d` are the labels the chain maps
/// ([`crate::usage::LABEL_5H`] / [`crate::usage::LABEL_7D`]); `month` becomes
/// window id [`WINDOW_MONTH`] with no nominal length.
pub(crate) const LABEL_SESSION: &str = crate::usage::LABEL_5H;
pub(crate) const LABEL_WEEKLY: &str = crate::usage::LABEL_7D;
pub(crate) const LABEL_MONTH: &str = "month";

/// Value of the row standing in for a window whose `usage` is absent.
pub(crate) const NOT_REPORTED: &str = "usage not reported";

/// Heading suffix of a window's per-model request block: `5h requests`.
const REQUESTS_SUFFIX: &str = " requests";

/// Label prefix of the spend row: `spend (last 4 weeks)`.
const SPEND_LABEL: &str = "spend";

/// Label of the faint row carrying the spend period's bounds.
const PERIOD_LABEL: &str = "spend period";

/// Separator between the two bounds in the [`PERIOD_LABEL`] row.
const PERIOD_SEP: &str = " to ";

/// What a monthly pool at or past 100 % means until spike S4(d) finds a
/// purchased-balance or auto-billing field: the INCLUDED credits are gone,
/// and whether use continues depends on money `/api/usage` does not show
/// (plan §4.5 severity additions).
pub(crate) const INCLUDED_USED_UP: &str = "included credits used up";
const INCLUDED_USED_UP_DETAIL: &str =
    "extra use draws on purchased credits / team billing (balance unknown)";

/// What the daemon arm says in place of figures.
pub(crate) const DAEMON_NEEDS_KEY: &str = "usage needs an ollama.com API key";

/// The failure message a daemon account's observation carries.
const DAEMON_FAILURE: &str =
    "usage needs an ollama.com API key; mint one at ollama.com/settings/keys";

pub(super) fn matches_base_url(url: &str) -> bool {
    url_matches_host(url, ORIGIN)
}

/// Whether `url` is the local daemon's default listener.
pub(super) fn matches_daemon_url(url: &str) -> bool {
    DAEMON_ORIGINS.iter().any(|o| url_matches_host(url, o))
}

// ── HTTP seam ──────────────────────────────────────────────────────────────────

/// One HTTP answer, reduced to what the classifier reads.
#[derive(Debug, Clone)]
pub(crate) struct HttpReply {
    pub(crate) status: u16,
    /// The `retry-after` header in delta-seconds form, when present.
    pub(crate) retry_after: Option<Duration>,
    pub(crate) body: String,
}

/// The transport [`fetch_ollama_cloud_usage`] sends its one GET through.
/// [`LiveHttp`] is the real one; tests pass a canned reply.
pub(crate) trait UsageHttp {
    /// `GET url` with `Authorization: Bearer <bearer>`. `Err` only when no
    /// HTTP answer arrived at all; every status is an `Ok` reply.
    fn get(&self, url: &str, bearer: &str) -> Result<HttpReply, ThirdPartyError>;
}

/// The shared usage agent ([`crate::usage::http_agent`]): short timeouts,
/// statuses on the `Ok` side.
pub(crate) struct LiveHttp;

impl UsageHttp for LiveHttp {
    fn get(&self, url: &str, bearer: &str) -> Result<HttpReply, ThirdPartyError> {
        let mut response = crate::usage::http_agent()
            .get(url)
            .header("Authorization", &format!("Bearer {bearer}"))
            .header("Accept", "application/json")
            .call()
            .map_err(|_| ThirdPartyError::Network)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::usage::parse_retry_after);
        // An error body is read for the 429 classification; one that fails to
        // read is empty, which classifies as a plain rate limit.
        let body = response.body_mut().read_to_string().unwrap_or_default();
        Ok(HttpReply {
            status,
            retry_after,
            body,
        })
    }
}

// ── Fetch ──────────────────────────────────────────────────────────────────────

/// The scheduler's entry: [`fetch_ollama_cloud_usage`] over [`LiveHttp`].
pub(super) fn fetch(api_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    fetch_ollama_cloud_usage(api_key, &LiveHttp)
}

/// Fetch and parse one key's usage. The ONE fetch path for Ollama Cloud: the
/// profile scheduler calls it through [`fetch`], and a monitoring source
/// holding a monitor-only key calls it directly with its own transport.
///
/// A blank key is refused as [`ThirdPartyError::AuthExpired`] before any
/// request, so it never reaches the wire.
pub(crate) fn fetch_ollama_cloud_usage(
    key: &str,
    http: &dyn UsageHttp,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(ThirdPartyError::AuthExpired);
    }
    let reply = http.get(USAGE_URL, key)?;
    classify(&reply)
}

/// Pure reply → stats or typed failure.
pub(crate) fn classify(reply: &HttpReply) -> Result<ThirdPartyStats, ThirdPartyError> {
    match reply.status {
        200..=299 => parse_usage(&reply.body),
        // `{"error":"invalid credentials"}`: the key is revoked or wrong, and
        // only a new key clears it.
        401 => Err(ThirdPartyError::AuthExpired),
        429 if names_usage_limit(&reply.body) => Err(ThirdPartyError::QuotaExhausted {
            retry_after: reply.retry_after,
        }),
        429 => Err(ThirdPartyError::RateLimited {
            retry_after: reply.retry_after,
        }),
        _ => Err(ThirdPartyError::Status),
    }
}

/// Whether a 429 body is Ollama's quota verdict (`"you've reached your
/// session usage limit, please wait or upgrade to continue"`) rather than a
/// request-rate throttle. Matches `usage limit` so the weekly and monthly
/// spellings of the same verdict classify alike.
fn names_usage_limit(body: &str) -> bool {
    body.to_ascii_lowercase().contains("usage limit")
}

// ── Parse ──────────────────────────────────────────────────────────────────────

/// One parsed window.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    /// `usage × 100`, unclamped; `None` when `usage` is absent or not a number.
    pct: Option<f64>,
    models: Vec<(String, u64)>,
}

/// Pure body → stats. Tolerant within a known shape and strict about the
/// shape itself: the root must be an object carrying `limits` or `activity`,
/// and `limits`, when present, must be an object. Anything else (an
/// `{"error":…}` 200, an HTML page, a bare array) is [`ThirdPartyError::Parse`].
pub(crate) fn parse_usage(body: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    let root: Value = serde_json::from_str(body).map_err(|_| ThirdPartyError::Parse)?;
    let root = root.as_object().ok_or(ThirdPartyError::Parse)?;
    let limits = match root.get("limits") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => return Err(ThirdPartyError::Parse),
    };
    let activity = root.get("activity").and_then(Value::as_object);
    if limits.is_none() && activity.is_none() {
        return Err(ThirdPartyError::Parse);
    }

    let window = |key: &str| limits.and_then(|l| l.get(key)).and_then(parse_window);
    let windows = [
        (LABEL_SESSION, window("session")),
        (LABEL_WEEKLY, window("weekly")),
        (LABEL_MONTH, window("monthly")),
    ];

    let mut bars: Vec<UsageBar> = Vec::new();
    let mut rows: Vec<StatRow> = Vec::new();
    for (label, w) in &windows {
        let Some(w) = w else { continue };
        match w.pct {
            Some(pct) => bars.push(UsageBar {
                label: (*label).to_string(),
                pct,
                resets_at: None,
                used: None,
                total: None,
            }),
            None => rows.push(StatRow {
                label: (*label).to_string(),
                value: NOT_REPORTED.to_string(),
                kind: StatRowKind::Faint,
            }),
        }
        if *label == LABEL_MONTH && w.pct.is_some_and(|p| p >= 100.0) {
            rows.push(StatRow {
                label: INCLUDED_USED_UP.to_string(),
                value: INCLUDED_USED_UP_DETAIL.to_string(),
                kind: StatRowKind::Body,
            });
        }
    }
    if !bars.is_empty() {
        rows.push(StatRow {
            label: "resets".to_string(),
            value: "no reset time from API".to_string(),
            kind: StatRowKind::Faint,
        });
    }

    if let Some(activity) = activity {
        rows.extend(spend_rows(activity));
    }

    for (label, w) in &windows {
        let Some(w) = w else { continue };
        if w.models.is_empty() {
            continue;
        }
        rows.push(StatRow {
            label: format!("{label}{REQUESTS_SUFFIX}"),
            value: String::new(),
            kind: StatRowKind::Heading,
        });
        for (name, count) in &w.models {
            rows.push(StatRow {
                label: name.clone(),
                value: format!("{count}{REQUESTS_SUFFIX}"),
                kind: StatRowKind::Body,
            });
        }
    }

    Ok(ThirdPartyStats {
        is_available: true,
        rows,
        bars,
        // The route carries no plan; the label comes from config.
        plan: None,
        endpoint: None,
        best_effort: false,
    })
}

/// One `limits.<window>` value. `None` for anything but an object (a stray
/// scalar drops that window, not the body).
fn parse_window(v: &Value) -> Option<Window> {
    let obj = v.as_object()?;
    let pct = obj
        .get("usage")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite())
        .map(|f| f * 100.0);
    let models = obj
        .get("models")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse_model_row).collect())
        .unwrap_or_default();
    Some(Window { pct, models })
}

/// One `models[]` row, or `None` (skipped) when it lacks a non-empty string
/// `name` or a non-negative integer `request_count`.
fn parse_model_row(v: &Value) -> Option<(String, u64)> {
    let obj = v.as_object()?;
    let name = obj.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let count = obj.get("request_count")?.as_u64()?;
    Some((name.to_string(), count))
}

/// The spend row (and its period row) from `activity`. Nothing when `cost` is
/// absent or not a plain decimal.
fn spend_rows(activity: &serde_json::Map<String, Value>) -> Vec<StatRow> {
    let cost = match activity.get("cost") {
        Some(Value::String(s)) => Amount::parse(s),
        Some(Value::Number(n)) => Amount::parse(&n.to_string()),
        _ => None,
    };
    let Some(cost) = cost else {
        return Vec::new();
    };
    let period = activity.get("period").and_then(Value::as_object);
    let kind = period
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .map(|t| t.trim().replace('_', " "))
        .filter(|t| !t.is_empty());
    let label = match &kind {
        Some(k) => format!("{SPEND_LABEL} ({k})"),
        None => SPEND_LABEL.to_string(),
    };
    let mut rows = vec![StatRow {
        label,
        value: format!("{} USD", cost.as_str()),
        kind: StatRowKind::Body,
    }];
    let bound = |key: &str| {
        period
            .and_then(|p| p.get(key))
            .and_then(Value::as_str)
            .and_then(crate::usage::iso_to_epoch_secs)
            .map(crate::usage::epoch_secs_to_iso)
    };
    if let (Some(start), Some(end)) = (bound("starting_at"), bound("ending_at")) {
        rows.push(StatRow {
            label: PERIOD_LABEL.to_string(),
            value: format!("{start}{PERIOD_SEP}{end}"),
            kind: StatRowKind::Faint,
        });
    }
    rows
}

// ── Daemon ─────────────────────────────────────────────────────────────────────

/// The daemon arm's answer: no request, one row saying what would unlock
/// usage.
pub(super) fn daemon_stats() -> ThirdPartyStats {
    ThirdPartyStats::from_rows(vec![StatRow {
        label: String::new(),
        value: DAEMON_NEEDS_KEY.to_string(),
        kind: StatRowKind::Faint,
    }])
}

// ── Observation ────────────────────────────────────────────────────────────────

/// Refine an observation [`crate::usage::project::apply_third_party`] built
/// from an Ollama cache, adding what the generic projection cannot know:
///
/// - **Ollama Cloud**: the `month` window is never `exhausted` and never
///   chain-eligible and has no nominal length (a calendar pool, not a rolling
///   window); a `usage not reported` row becomes a window with
///   `used_pct: None`; each `<label> requests` block becomes that window's
///   `breakdown`; the spend row becomes one `spend.period` meter (Spend, USD,
///   exact decimal) with the provider's `[starting_at, ending_at)` bounds.
/// - **Ollama daemon**: `NativeLogin`, no figures, `NotFetched`, and an
///   `AuthRequired` failure naming the key it would need.
///
/// Every other source is left untouched.
pub(crate) fn refine_observation(obs: &mut AccountObservation, stats: Option<&ThirdPartyStats>) {
    match obs.source {
        SourceId::Ollama => refine_daemon(obs),
        SourceId::OllamaCloud => {
            if let Some(stats) = stats {
                refine_cloud(obs, stats);
            }
        }
        _ => {}
    }
}

fn refine_daemon(obs: &mut AccountObservation) {
    obs.auth = AuthKind::NativeLogin;
    obs.windows.clear();
    obs.money.clear();
    obs.freshness = Freshness::NotFetched;
    obs.observed_at = None;
    if obs.failure.is_none() {
        obs.failure = Some(Failure::new(FailureKind::AuthRequired, DAEMON_FAILURE));
    }
}

/// `(window id, chain-eligible, nominal length)` for a bar label.
fn window_spec(label: &str) -> Option<(&'static str, bool, Option<u64>)> {
    match label {
        LABEL_SESSION => Some((
            WINDOW_SESSION,
            true,
            Some(crate::usage::observation::SESSION_WINDOW_SECS),
        )),
        LABEL_WEEKLY => Some((
            WINDOW_WEEKLY,
            true,
            Some(crate::usage::observation::WEEKLY_WINDOW_SECS),
        )),
        LABEL_MONTH => Some((WINDOW_MONTH, false, None)),
        _ => None,
    }
}

fn refine_cloud(obs: &mut AccountObservation, stats: &ThirdPartyStats) {
    for w in &mut obs.windows {
        if w.id == WINDOW_MONTH {
            w.exhausted = false;
            w.chain_eligible = false;
            w.window_secs = None;
        }
    }

    // Windows the body named without a `usage`: present, figure unknown.
    for row in &stats.rows {
        if row.value != NOT_REPORTED {
            continue;
        }
        let Some((id, chain, secs)) = window_spec(&row.label) else {
            continue;
        };
        if obs.windows.iter().any(|w| w.id == id) {
            continue;
        }
        let scope = if chain {
            WindowScope::Shared
        } else {
            WindowScope::Account
        };
        let mut q = QuotaWindow::new(id, row.label.as_str(), scope);
        q.chain_eligible = chain && !stats.best_effort;
        q.window_secs = secs;
        obs.windows.push(q);
    }
    let order = |id: &str| match id {
        WINDOW_SESSION => 0,
        WINDOW_WEEKLY => 1,
        WINDOW_MONTH => 2,
        _ => 3,
    };
    obs.windows.sort_by_key(|w| order(&w.id));

    // Per-model request blocks → the matching window's breakdown.
    let mut current: Option<&'static str> = None;
    for row in &stats.rows {
        if matches!(row.kind, StatRowKind::Heading) {
            current = row
                .label
                .strip_suffix(REQUESTS_SUFFIX)
                .and_then(window_spec)
                .map(|(id, _, _)| id);
            continue;
        }
        let Some(id) = current else { continue };
        let Some(requests) = row
            .value
            .strip_suffix(REQUESTS_SUFFIX)
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(w) = obs.windows.iter_mut().find(|w| w.id == id) {
            w.breakdown.push(ModelCount {
                model: row.label.clone(),
                requests,
            });
        }
    }

    obs.money = cloud_money(stats);
}

/// The one Ollama money figure: recent spend, exact, over the provider's own
/// period. Never a balance: the route publishes no purchased-credit balance.
fn cloud_money(stats: &ThirdPartyStats) -> Vec<MoneyMeter> {
    let Some(row) = stats
        .rows
        .iter()
        .find(|r| r.label == SPEND_LABEL || r.label.starts_with(&format!("{SPEND_LABEL} (")))
    else {
        return Vec::new();
    };
    let Some((amount, currency)) = crate::usage::project::parse_money_value(&row.value) else {
        return Vec::new();
    };
    let kind_words = row
        .label
        .strip_prefix(SPEND_LABEL)
        .map(|s| s.trim().trim_start_matches('(').trim_end_matches(')'))
        .unwrap_or_default();
    let mut period = Period::of(period_kind(kind_words));
    if let Some((start, end)) = stats
        .rows
        .iter()
        .find(|r| r.label == PERIOD_LABEL)
        .and_then(|r| r.value.split_once(PERIOD_SEP))
    {
        period.start = Timestamp::parse(start);
        period.end = Timestamp::parse(end);
    }
    let label = if kind_words.is_empty() {
        "Spend".to_string()
    } else {
        format!("Spend ({kind_words})")
    };
    let mut m = MoneyMeter::new(
        "spend.period",
        label,
        MoneyKind::Spend,
        amount,
        currency,
        MoneyScope::Profile,
    );
    m.period = Some(period);
    vec![m]
}

/// `activity.period.type` words → a [`PeriodKind`]. `last 4 weeks` is a
/// trailing span, not a calendar week or month, so it is `Custom`.
fn period_kind(words: &str) -> PeriodKind {
    match words {
        "day" | "daily" | "today" => PeriodKind::Daily,
        "week" | "weekly" => PeriodKind::Weekly,
        "month" | "monthly" => PeriodKind::Monthly,
        _ => PeriodKind::Custom,
    }
}

#[cfg(test)]
#[path = "../../tests/inline/providers_ollama_cloud.rs"]
mod tests;
