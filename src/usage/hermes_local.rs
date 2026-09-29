//! `hermes_local`: a Hermes home's own usage ledger as a `hermes:<name>`
//! observation (hermes spec §4.6).
//!
//! Hermes records every model call it bills in `<home>/state.db`
//! (`session_model_usage`), and its Nous rate-limit cooldown in
//! `<home>/rate_limits/nous.json`. [`refresh`] reads both and writes
//! `profiles/<name>/hermes_usage_cache.json`; [`hermes_observations`], the
//! collect hook, reads caches only.
//!
//! The read never runs Hermes or its interpreter. It runs the `sqlite3` CLI
//! with `-readonly -json` (D-H3: no bundled C SQLite in every build), with
//! stdin null and a 5 s kill timeout, and the only value interpolated into its
//! SQL is a tollgate-computed integer (the UTC month start). A missing
//! `sqlite3` is a typed `Unavailable`, never an error.
//!
//! A pool home is exact per home, not per pool entry: `session_model_usage`
//! does not say which credential served a call.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::collect::CollectCtx;
use super::observation::{
    AccountObservation, Amount, AuthKind, Failure, FailureKind, LocalEstimate, Origin, Period,
    PeriodKind, SourceId, Timestamp, account_id,
};
use crate::hermes::home::HermesPaths;
use crate::hermes::profiles::{HermesProfile, HermesState};
use crate::logline::logline;
use crate::profile::ProfileName;
use crate::runtime::RotationGuard;

/// The cache file beside the profile's Hermes home.
pub(crate) const CACHE_FILE: &str = "hermes_usage_cache.json";
/// The cache schema this binary writes.
pub(crate) const CACHE_SCHEMA: u32 = 1;
/// The `state.db` schema the queries were written against
/// (`hermes_state.py:155`, `SCHEMA_VERSION = 22`).
pub(crate) const EXPECTED_DB_SCHEMA: i64 = 22;
/// How often the daemon re-reads one profile's `state.db`.
pub(crate) const REFRESH_EVERY_MS: u64 = 60_000;
/// How long a last good reading outlives a failed read (§5, sqlite3 slow).
pub(crate) const KEEP_LAST_GOOD_MS: u64 = 7 * 24 * 3600 * 1000;
/// The estimate's basis phrase.
pub(crate) const BASIS: &str = "hermes state.db (billed where known, else estimated)";

const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RATE_FILE_BYTES: u64 = 64 * 1024;
/// The shortest gap between two daemon scans of the roster.
const SCAN_EVERY_MS: u64 = 10_000;

/// Every `session_model_usage` column the usage query reads
/// (`hermes_state.py:836-856`). A db missing any of them is `schema_unknown`.
pub(crate) const USAGE_COLUMNS: &[&str] = &[
    "billing_provider",
    "model",
    "api_call_count",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "actual_cost_usd",
    "estimated_cost_usd",
    "first_seen",
    "last_seen",
];

/// Why the last read produced no figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CacheError {
    #[serde(rename = "sqlite3_missing")]
    Sqlite3Missing,
    SchemaUnknown,
    DbUnreadable,
    Timeout,
}

impl CacheError {
    /// The snake_case spelling this serialises as (`sqlite3_missing`, …).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CacheError::Sqlite3Missing => "sqlite3_missing",
            CacheError::SchemaUnknown => "schema_unknown",
            CacheError::DbUnreadable => "db_unreadable",
            CacheError::Timeout => "timeout",
        }
    }

    /// The one sentence the observation's `Unavailable` failure carries.
    pub(crate) fn message(self) -> &'static str {
        match self {
            CacheError::Sqlite3Missing => "install sqlite3 to read Hermes' local usage",
            CacheError::SchemaUnknown => "Hermes state.db schema not recognised",
            CacheError::DbUnreadable => "Hermes state.db could not be read",
            CacheError::Timeout => "reading Hermes state.db timed out",
        }
    }
}

/// One `(billing_provider, model)` group of the month.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageRow {
    pub(crate) billing_provider: String,
    pub(crate) model: String,
    pub(crate) api_calls: u64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) reasoning_tokens: u64,
    /// Billed cost where Hermes knows it (> 0), else its estimate, summed by
    /// sqlite and printed with `printf('%.6f')`: a decimal string, never an
    /// `f64` through tollgate's formatting.
    pub(crate) cost_usd: Amount,
}

/// `hermes_usage_cache.json`, schema 1 (spec §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HermesUsageCache {
    pub(crate) schema_version: u32,
    /// When the shown rows were read; `0` when no read has succeeded.
    pub(crate) read_at_ms: u64,
    pub(crate) db_schema_version: Option<i64>,
    /// The UTC month start the rows cover, RFC 3339.
    pub(crate) period_start: String,
    pub(crate) rows: Vec<UsageRow>,
    /// The latest `last_seen` of a row billed to anthropic this month.
    pub(crate) anthropic_since_ms: Option<u64>,
    /// `rate_limits/nous.json` `reset_at`, epoch seconds.
    pub(crate) nous_reset_at: Option<i64>,
    pub(crate) error: Option<CacheError>,
    /// The Hermes version of the last `tollgate start` teardown that wrote
    /// this cache; carried over by the daemon's reads.
    #[serde(default)]
    pub(crate) hermes_version: Option<String>,
}

impl HermesUsageCache {
    /// Σ `cost_usd`, exact: the rows' six-decimal strings summed in micros.
    pub(crate) fn total_cost(&self) -> Amount {
        let micros: i64 = self
            .rows
            .iter()
            .filter_map(|r| micros_of(&r.cost_usd))
            .fold(0i64, i64::saturating_add);
        Amount::from_minor(micros, 6)
    }
}

/// An amount in millionths, `None` when it has more than six decimals (sqlite
/// printed `%.6f`, so it never does).
fn micros_of(amount: &Amount) -> Option<i64> {
    let raw = amount.as_str();
    let (neg, body) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if frac.len() > 6 {
        return None;
    }
    let int: i64 = int.parse().ok()?;
    let frac: i64 = format!("{frac:0<6}").parse().ok()?;
    let v = int.checked_mul(1_000_000)?.checked_add(frac)?;
    Some(if neg { -v } else { v })
}

// ── the month ────────────────────────────────────────────────────────────────

/// The UTC month start of `now_secs`, epoch seconds.
pub(crate) fn month_start_secs(now_secs: i64) -> i64 {
    use chrono::Datelike as _;
    let Some(now) = chrono::DateTime::from_timestamp(now_secs, 0) else {
        return 0;
    };
    now.date_naive()
        .with_day(1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map_or(0, |dt| dt.and_utc().timestamp())
}

/// `2026-09-01T00:00:00Z`.
pub(crate) fn rfc3339_z(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

// ── the sqlite3 read ─────────────────────────────────────────────────────────

/// Q1;Q2;Q3 (spec §4.6 step 2). `period_start_secs` is the only interpolated
/// value, and it is an integer tollgate computed.
pub(crate) fn usage_sql(period_start_secs: i64) -> String {
    format!(
        "SELECT version FROM schema_version LIMIT 1; \
         SELECT name FROM pragma_table_info('session_model_usage'); \
         SELECT billing_provider AS billing_provider, model AS model, \
         SUM(api_call_count) AS api_calls, SUM(input_tokens) AS input_tokens, \
         SUM(output_tokens) AS output_tokens, SUM(cache_read_tokens) AS cache_read_tokens, \
         SUM(cache_write_tokens) AS cache_write_tokens, \
         SUM(reasoning_tokens) AS reasoning_tokens, \
         printf('%.6f', SUM(CASE WHEN actual_cost_usd > 0 THEN actual_cost_usd \
         ELSE estimated_cost_usd END)) AS cost_usd, MAX(last_seen) AS last_seen \
         FROM session_model_usage \
         WHERE COALESCE(last_seen, first_seen, 0) >= {period_start_secs} \
         GROUP BY 1, 2 ORDER BY 1, 2"
    )
}

/// What one read of `state.db` yields.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DbReading {
    pub(crate) db_schema_version: Option<i64>,
    pub(crate) rows: Vec<UsageRow>,
    pub(crate) anthropic_since_ms: Option<u64>,
}

#[derive(Deserialize)]
struct RawRow {
    billing_provider: Option<String>,
    model: Option<String>,
    api_calls: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cost_usd: Option<String>,
    last_seen: Option<f64>,
}

fn count(v: Option<i64>) -> u64 {
    v.map_or(0, |n| u64::try_from(n).unwrap_or(0))
}

/// Parse sqlite3's `-json` output for Q1;Q2;Q3: one JSON array per statement
/// that returned rows, concatenated (an empty result prints nothing). Each
/// array is recognised by its keys, not its position, because an empty Q3
/// prints no array at all.
pub(crate) fn parse_sqlite_json(stdout: &[u8]) -> Result<DbReading, CacheError> {
    let stream = serde_json::Deserializer::from_slice(stdout)
        .into_iter::<Vec<serde_json::Map<String, serde_json::Value>>>();
    let mut version: Option<i64> = None;
    let mut columns: Option<Vec<String>> = None;
    let mut raw_rows: Vec<RawRow> = Vec::new();
    for array in stream {
        let array = array.map_err(|_| CacheError::DbUnreadable)?;
        let Some(first) = array.first() else {
            continue;
        };
        if first.contains_key("billing_provider") {
            for obj in array {
                let row: RawRow = serde_json::from_value(serde_json::Value::Object(obj))
                    .map_err(|_| CacheError::SchemaUnknown)?;
                raw_rows.push(row);
            }
        } else if first.len() == 1 && first.contains_key("version") {
            version = first.get("version").and_then(serde_json::Value::as_i64);
        } else if first.len() == 1 && first.contains_key("name") {
            columns = Some(
                array
                    .iter()
                    .filter_map(|o| o.get("name").and_then(|v| v.as_str()).map(str::to_string))
                    .collect(),
            );
        } else {
            return Err(CacheError::SchemaUnknown);
        }
    }
    let columns = columns.ok_or(CacheError::SchemaUnknown)?;
    if !USAGE_COLUMNS.iter().all(|c| columns.iter().any(|n| n == c)) {
        return Err(CacheError::SchemaUnknown);
    }
    let mut reading = DbReading {
        db_schema_version: version,
        ..DbReading::default()
    };
    for raw in raw_rows {
        let provider = raw.billing_provider.unwrap_or_default();
        let cost = match raw.cost_usd.as_deref().map(str::trim) {
            None | Some("") => Amount::zero(),
            Some(s) => Amount::parse(s).ok_or(CacheError::SchemaUnknown)?,
        };
        if crate::hermes::guards::is_anthropic(&provider)
            && let Some(last) = raw.last_seen.filter(|v| v.is_finite() && *v > 0.0)
        {
            let ms = (last * 1000.0) as u64;
            reading.anthropic_since_ms = Some(reading.anthropic_since_ms.map_or(ms, |m| m.max(ms)));
        }
        reading.rows.push(UsageRow {
            billing_provider: provider,
            model: raw.model.unwrap_or_default(),
            api_calls: count(raw.api_calls),
            input_tokens: count(raw.input_tokens),
            output_tokens: count(raw.output_tokens),
            cache_read_tokens: count(raw.cache_read_tokens),
            cache_write_tokens: count(raw.cache_write_tokens),
            reasoning_tokens: count(raw.reasoning_tokens),
            cost_usd: cost,
        });
    }
    Ok(reading)
}

/// Run `sqlite3 -readonly -json` on `<home>/state.db`. A home Hermes never
/// used (no `state.db`) reads as empty; `path` is the PATH to find `sqlite3`
/// on (injected by the tests).
pub(crate) fn read_state_db(
    home: &Path,
    period_start_secs: i64,
    path: Option<&OsStr>,
) -> Result<DbReading, CacheError> {
    let Some(sqlite) = crate::hermes::resolve::which_on(path, "sqlite3") else {
        return Err(CacheError::Sqlite3Missing);
    };
    let db = home.join("state.db");
    match db.symlink_metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(CacheError::DbUnreadable),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DbReading::default()),
        Err(_) => return Err(CacheError::DbUnreadable),
    }
    // A helper, like herdr or notify-send: no monitoring or billing key rides
    // into it.
    let mut command = crate::providers::billing_key::helper_command(sqlite);
    command
        .arg("-readonly")
        .arg("-json")
        .arg("-cmd")
        .arg(".timeout 2000")
        .arg(&db)
        .arg(usage_sql(period_start_secs));
    let out = match crate::hermes::run_bounded(command, SQLITE_TIMEOUT, "sqlite3") {
        Ok(out) => out,
        Err(e) if format!("{e:#}").contains("timed out") => return Err(CacheError::Timeout),
        Err(_) => return Err(CacheError::DbUnreadable),
    };
    match parse_sqlite_json(&out.stdout) {
        Ok(reading) if out.status.success() => Ok(reading),
        Ok(_) => Err(CacheError::DbUnreadable),
        Err(CacheError::SchemaUnknown) => Err(CacheError::SchemaUnknown),
        Err(e) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("no such table") || stderr.contains("no such column") {
                Err(CacheError::SchemaUnknown)
            } else {
                Err(e)
            }
        }
    }
}

/// `<home>/rate_limits/nous.json` `reset_at` (epoch seconds), capped at
/// 64 KiB. A missing, torn or unparseable file is ignored.
pub(crate) fn read_nous_reset(home: &Path) -> Option<i64> {
    let path = home.join("rate_limits").join("nous.json");
    let meta = path.symlink_metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_RATE_FILE_BYTES {
        return None;
    }
    #[derive(Deserialize)]
    struct Nous {
        reset_at: Option<f64>,
    }
    let bytes = std::fs::read(&path).ok()?;
    let nous: Nous = serde_json::from_slice(&bytes).ok()?;
    nous.reset_at
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v as i64)
}

// ── the cache ────────────────────────────────────────────────────────────────

/// The profile's cache, when it exists and parses.
pub(crate) fn load(name: &str) -> Option<HermesUsageCache> {
    let paths = HermesPaths::for_name(name).ok()?;
    let path = paths.profile.join(CACHE_FILE);
    let meta = path.symlink_metadata().ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    let cache: HermesUsageCache = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (cache.schema_version == CACHE_SCHEMA).then_some(cache)
}

/// Read `name`'s home and write its cache (spec §4.6). A failed read keeps
/// the last good rows for [`KEEP_LAST_GOOD_MS`] and records why.
/// `hermes_version` is the version a `start` teardown launched; `None`
/// carries the cached one over.
///
/// The `sqlite3` child runs with no lock held. The write then takes the
/// profile's RotationGuard without waiting and re-checks the home under it,
/// so it can never land inside a `hermes delete` (which holds that guard) and
/// recreate the directory the delete removed. A busy guard skips the write
/// and still returns the fresh reading: the next refresh writes it.
pub(crate) fn refresh_with(
    name: &str,
    now_ms: u64,
    path: Option<&OsStr>,
    hermes_version: Option<&str>,
) -> Result<HermesUsageCache> {
    let paths = HermesPaths::for_name(name)?;
    if !paths.home.is_dir() {
        bail!("Hermes profile '{name}' has no home");
    }
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    let period = month_start_secs(now_secs);
    let previous = load(name);
    let nous_reset_at = read_nous_reset(&paths.home);
    let version = hermes_version
        .map(str::to_string)
        .or_else(|| previous.as_ref().and_then(|c| c.hermes_version.clone()));
    let cache = match read_state_db(&paths.home, period, path) {
        Ok(reading) => HermesUsageCache {
            schema_version: CACHE_SCHEMA,
            read_at_ms: now_ms,
            db_schema_version: reading.db_schema_version,
            period_start: rfc3339_z(period),
            rows: reading.rows,
            anthropic_since_ms: reading.anthropic_since_ms,
            nous_reset_at,
            error: None,
            hermes_version: version,
        },
        Err(error) => match previous.filter(|p| {
            p.read_at_ms > 0 && now_ms.saturating_sub(p.read_at_ms) <= KEEP_LAST_GOOD_MS
        }) {
            Some(last_good) => HermesUsageCache {
                error: Some(error),
                nous_reset_at,
                hermes_version: version,
                ..last_good
            },
            None => HermesUsageCache {
                schema_version: CACHE_SCHEMA,
                read_at_ms: 0,
                db_schema_version: None,
                period_start: rfc3339_z(period),
                rows: Vec::new(),
                anthropic_since_ms: None,
                nous_reset_at,
                error: Some(error),
                hermes_version: version,
            },
        },
    };
    let bytes = serde_json::to_vec_pretty(&cache)?;
    let Some(_guard) = RotationGuard::try_acquire(&ProfileName::from(name))? else {
        return Ok(cache);
    };
    if !paths.home.is_dir() {
        bail!("Hermes profile '{name}' has no home");
    }
    crate::profile::atomic_write_600(&paths.profile.join(CACHE_FILE), bytes)
        .with_context(|| format!("failed to write {CACHE_FILE} for '{name}'"))?;
    Ok(cache)
}

/// [`refresh_with`] against the process PATH and clock.
pub(crate) fn refresh(name: &str, hermes_version: Option<&str>) -> Result<HermesUsageCache> {
    let path = std::env::var_os("PATH");
    refresh_with(
        name,
        crate::usage::now_ms(),
        path.as_deref(),
        hermes_version,
    )
}

// ── the daemon leg ───────────────────────────────────────────────────────────

static LAST_SCAN_MS: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// The roster names whose cache was last read `REFRESH_EVERY_MS` or more
/// before `now_ms` (or never, or a failed read older than that).
pub(crate) fn due_profiles(profiles: &[HermesProfile], now_ms: u64) -> Vec<String> {
    profiles
        .iter()
        .filter(|p| {
            let last = load(&p.name).map(|c| {
                // A failed read has no `read_at_ms`; its file mtime dates the try.
                if c.read_at_ms > 0 {
                    c.read_at_ms
                } else {
                    crate::profile_cache::profile_cache_mtime_ms(
                        &crate::profile::ProfileName::from(p.name.as_str()),
                        CACHE_FILE,
                    )
                    .unwrap_or(0)
                }
            });
            last.is_none_or(|at| now_ms.saturating_sub(at) >= REFRESH_EVERY_MS || at > now_ms)
        })
        .map(|p| p.name.clone())
        .collect()
}

/// The daemon tick's entry point (beside the monitor poll): scan the roster
/// at most every 10 s, and refresh the due profiles on one background thread,
/// so a slow `sqlite3` never holds the tick. Runs only `sqlite3`, never
/// Hermes, its interpreter or `mise`.
pub(crate) fn refresh_detached() {
    let now = crate::usage::now_ms();
    let last = LAST_SCAN_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SCAN_EVERY_MS && now >= last {
        return;
    }
    LAST_SCAN_MS.store(now, Ordering::Relaxed);
    let Ok(state) = HermesState::load() else {
        return;
    };
    let due = due_profiles(state.profiles(), now);
    if due.is_empty() || IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    #[cfg(test)]
    let done = crate::testutil::register_background_task();
    std::thread::spawn(move || {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        let _clear = Clear;
        for name in due {
            if let Err(e) = refresh(&name, None) {
                logline!("tollgate: hermes '{name}': usage read failed: {e:#}");
            }
        }
        #[cfg(test)]
        let _ = done.send(());
    });
}

#[cfg(test)]
pub(crate) fn reset_scan_gate_for_test() {
    LAST_SCAN_MS.store(0, Ordering::Relaxed);
}

// ── the observation ──────────────────────────────────────────────────────────

/// The collect hook: one `hermes:<name>` per roster profile, from its cache.
/// An unreadable roster yields nothing (§5).
pub(crate) fn hermes_observations(ctx: &CollectCtx<'_>) -> Vec<AccountObservation> {
    let state = HermesState::load().unwrap_or_default();
    state
        .profiles()
        .iter()
        .map(|p| observe(p, load(&p.name).as_ref(), ctx))
        .collect()
}

/// One profile's observation at the context's clock.
pub(crate) fn observe(
    profile: &HermesProfile,
    cache: Option<&HermesUsageCache>,
    ctx: &CollectCtx<'_>,
) -> AccountObservation {
    let name = profile.name.as_str();
    let mut obs = AccountObservation::new(
        account_id(Origin::HermesProfile, name),
        SourceId::Hermes,
        AuthKind::NativeLogin,
        Origin::HermesProfile,
        name,
    );
    obs.plan = Some(format!(
        "{} · {} home",
        profile.provider,
        profile.mode.as_str()
    ));
    let Some(cache) = cache else {
        return obs;
    };
    let now_secs = ctx.now_secs();
    if cache.read_at_ms > 0 {
        obs.observed_at = Some(Timestamp::from_ms(cache.read_at_ms));
        obs.freshness = ctx.freshness_at_cadence(Some(cache.read_at_ms), REFRESH_EVERY_MS);
        let month = month_start_secs(now_secs);
        // Last month's rows are not this month's estimate.
        if cache.period_start == rfc3339_z(month) {
            obs.estimate = Some(LocalEstimate {
                amount: cache.total_cost(),
                currency: "USD".to_string(),
                period: Some(Period {
                    kind: PeriodKind::Monthly,
                    start: Some(Timestamp::from_secs(month)),
                    end: Some(Timestamp::from_secs(now_secs)),
                    derived: true,
                }),
                basis: BASIS.to_string(),
            });
        }
    }
    obs.best_effort = cache
        .db_schema_version
        .is_some_and(|v| v != EXPECTED_DB_SCHEMA)
        || cache
            .hermes_version
            .as_deref()
            .is_some_and(|v| !crate::hermes::resolve::in_verified_series(v));
    obs.failure = match (cache.nous_reset_at, cache.error) {
        (Some(reset), _) if reset > now_secs => Some(Failure {
            retry_after: Some(Timestamp::from_secs(reset)),
            ..Failure::new(
                FailureKind::RateLimited,
                "Nous rate-limited this home; Hermes waits for the reset",
            )
        }),
        (_, Some(error)) => Some(Failure::new(FailureKind::Unavailable, error.message())),
        _ => None,
    };
    obs
}

#[cfg(test)]
#[path = "../../tests/inline/usage_hermes_local.rs"]
mod tests;
