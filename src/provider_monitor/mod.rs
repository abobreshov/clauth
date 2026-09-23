//! Native client subscription monitoring, independently configurable from
//! clauth's credential-switching profiles. Never rotates native refresh tokens.

mod antigravity;
mod cli;
mod codex;
pub(crate) mod config;
mod grok;
pub(crate) mod types;

pub(crate) use cli::{ClaudeReading, current_claude_reading, remaining_percent};
pub(crate) use cli::{ProviderArgs, run};
pub(crate) use grok::detect_login_choices;
pub(crate) use types::{ProviderData, ProviderError, ProviderReport};

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use config::{MonitorConfig, TargetConfig};
use types::{ObservationState, ProviderKind, RefreshState};

const MAX_JSON_BYTES: u64 = 2 * 1024 * 1024;

static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
});

pub(crate) fn http_agent() -> &'static ureq::Agent {
    &HTTP
}

pub(crate) fn read_json(path: &Path) -> std::result::Result<Value, ProviderError> {
    let file = std::fs::File::open(path).map_err(|_| {
        ProviderError::auth("native credentials unavailable; sign in with the official tool")
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProviderError::auth("cannot read native credentials"))?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err(ProviderError::auth("native credential file is too large"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| ProviderError::auth("native credential file is not valid JSON"))
}

pub(crate) fn credential_path(
    target: &TargetConfig,
    default_relative: &str,
) -> std::result::Result<PathBuf, ProviderError> {
    if let Some(path) = &target.auth_file {
        config::expand(path)
            .map_err(|_| ProviderError::auth("credential path must be absolute or start with ~/"))
    } else {
        // Unit tests must not discover the operator's alternate native store.
        #[cfg(not(test))]
        if target.provider == ProviderKind::Codex
            && let Some(home) = std::env::var_os("CODEX_HOME")
        {
            let home = PathBuf::from(home);
            if !home.is_absolute() {
                return Err(ProviderError::auth("CODEX_HOME must be an absolute path"));
            }
            return Ok(home.join("auth.json"));
        }
        crate::profile::home_dir()
            .map(|home| home.join(default_relative))
            .map_err(|_| ProviderError::auth("home directory not found"))
    }
}

fn response_json(
    mut response: ureq::http::Response<ureq::Body>,
) -> std::result::Result<Value, ProviderError> {
    if !response.status().is_success() {
        return Err(ProviderError::http(response.status().as_u16()));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_JSON_BYTES)
        .read_to_string()
        .map_err(|_| ProviderError::invalid())?;
    serde_json::from_str(&body).map_err(|_| ProviderError::invalid())
}

pub(crate) fn get_json(
    url: &str,
    headers: &[(&str, &str)],
) -> std::result::Result<Value, ProviderError> {
    let mut req = http_agent().get(url);
    for (key, value) in headers {
        req = req.header(*key, *value);
    }
    response_json(req.call().map_err(|_| ProviderError::network())?)
}

pub(crate) fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> std::result::Result<Value, ProviderError> {
    let mut req = http_agent().post(url);
    for (key, value) in headers {
        req = req.header(*key, *value);
    }
    response_json(
        req.header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|_| ProviderError::network())?,
    )
}

#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    binding: String,
    #[serde(default)]
    credential_binding: Option<String>,
    #[serde(default)]
    failures: u32,
    report: ProviderReport,
}

fn credentials(target: &TargetConfig) -> std::result::Result<Value, ProviderError> {
    match target.provider {
        ProviderKind::Codex => read_json(&credential_path(target, ".codex/auth.json")?),
        ProviderKind::Grok => read_json(&credential_path(target, ".grok/auth.json")?),
        ProviderKind::Antigravity => antigravity::credentials(target),
    }
}

fn credential_binding(value: &Value) -> String {
    // A private digest is sufficient for invalidation. Never publish login
    // contents, token bytes, email addresses or this digest in status output.
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

fn native_keyring(target: &TargetConfig) -> bool {
    target.provider == ProviderKind::Antigravity && target.auth_file.is_none()
}

fn cache_identity_matches(data: &Cached, current: Option<&str>) -> bool {
    data.credential_binding
        .as_deref()
        .is_some_and(|old| Some(old) == current)
}

#[cfg(test)]
fn cached_report(root: &Path, target: &TargetConfig) -> ProviderReport {
    cached_entry(root, target).0
}

/// The report a read shows for `target`, with the failure streak that paces
/// its next check. A report that no longer belongs to the current login starts
/// a fresh streak, because the next check is for a different account.
fn cached_entry(root: &Path, target: &TargetConfig) -> (ProviderReport, u32) {
    let Some(cache) = cached(root, target) else {
        return (empty_report(target), 0);
    };
    let failures = cache.failures;
    // Failed reads contain no saved data and remain useful diagnostic results.
    if cache.credential_binding.is_none() && cache.report.observed_at_ms.is_none() {
        return (cache.report, failures);
    }
    if native_keyring(target) && cache.credential_binding.is_some() {
        return (cache.report, failures);
    }
    let current = credentials(target).ok().as_ref().map(credential_binding);
    if cache_identity_matches(&cache, current.as_deref()) {
        (cache.report, failures)
    } else {
        let mut report = empty_report(target);
        report.message =
            Some("native login changed or is unavailable; refresh provider usage".into());
        (report, 0)
    }
}

fn binding(target: &TargetConfig) -> String {
    // Model/launch choices share one monitored account and one request budget.
    // Resolve environment-dependent native stores before identifying the pool.
    let store = match target.provider {
        ProviderKind::Codex => credential_path(target, ".codex/auth.json").ok(),
        ProviderKind::Grok => credential_path(target, ".grok/auth.json").ok(),
        ProviderKind::Antigravity => target
            .auth_file
            .as_ref()
            .and_then(|p| config::expand(p).ok()),
    };
    hex::encode(Sha256::digest(
        serde_json::json!({
            "provider": target.provider,
            "store": store,
            "entry": target.auth_entry,
        })
        .to_string()
        .as_bytes(),
    ))
}

fn cache_dir() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join("provider-usage"))
}

fn cache_path(root: &Path, target: &TargetConfig) -> PathBuf {
    root.join(format!("{}.json", binding(target)))
}

fn cached(root: &Path, target: &TargetConfig) -> Option<Cached> {
    let mut data: Cached =
        serde_json::from_slice(&std::fs::read(cache_path(root, target)).ok()?).ok()?;
    data.report.id.clone_from(&target.id);
    data.report.model.clone_from(&target.model);
    (data.binding == binding(target)).then_some(data)
}

fn empty_report(target: &TargetConfig) -> ProviderReport {
    ProviderReport {
        id: target.id.clone(),
        provider: target.provider,
        tool: target.provider.tool().into(),
        model: target.model.clone(),
        state: ObservationState::NotFetched,
        observed_at_ms: None,
        checked_at_ms: None,
        identity_checked_at_observation_only: native_keyring(target),
        data: ProviderData::default(),
        message: Some("waiting for provider refresh".into()),
        warning: false,
        listed: false,
        refresh: RefreshState::default(),
    }
}

fn stamp_listed(mut report: ProviderReport, target: &TargetConfig) -> ProviderReport {
    report.listed = target.listed;
    report
}

/// The monitor's floor and ceiling on the poll interval, the same range
/// `providers.toml` accepts for `poll_interval_seconds`.
const MIN_POLL_MS: u64 = 30_000;
const MAX_POLL_MS: u64 = 3_600_000;

/// The poll interval a check is paced by. The scheduler and the TUI pass the
/// Config tab refresh interval, so native logins refresh on the same cadence as
/// Claude accounts, floored at the monitor's 30 s minimum. Without one (the
/// `clauth providers status` path) `poll_interval_seconds` applies.
fn poll_ms(cfg: &MonitorConfig, interval_ms: Option<u64>) -> u64 {
    interval_ms.map_or(cfg.poll_interval_seconds.saturating_mul(1000), |ms| {
        ms.clamp(MIN_POLL_MS, MAX_POLL_MS)
    })
}

/// How old a fresh reading may get before it reads as stale. With a refresh
/// interval this is the Claude rule, so a native row goes stale when a Claude
/// row on the same cadence would, and never sooner than `stale_after_seconds`.
fn stale_after_ms(cfg: &MonitorConfig, interval_ms: Option<u64>) -> u64 {
    let configured = cfg.stale_after_seconds.saturating_mul(1000);
    interval_ms.map_or(configured, |ms| {
        configured.max(crate::profile_json::stale_after_ms(
            ms.clamp(MIN_POLL_MS, MAX_POLL_MS),
        ))
    })
}

fn age_report_after(
    mut report: ProviderReport,
    cfg: &MonitorConfig,
    stale_after_ms: u64,
    now: u64,
) -> ProviderReport {
    if report.state == ObservationState::Fresh
        && report
            .observed_at_ms
            .is_none_or(|at| now.saturating_sub(at) > stale_after_ms)
    {
        report.state = ObservationState::Stale;
        report.message = Some("cached usage is stale; run the daemon or refresh providers".into());
    }
    report.warning = report.state == ObservationState::Fresh
        && report.data.buckets.iter().any(|b| {
            b.exhausted
                || b.remaining_percent
                    .is_some_and(|p| p <= cfg.warning_remaining_percent)
        });
    report
}

/// Disk-only, safe for MCP, rendering, and the public status feed.
pub(crate) fn reports() -> Result<Vec<ProviderReport>> {
    reports_with(None)
}

/// [`reports`] paced by `interval_ms`, the Config tab refresh interval. The TUI
/// reads through this so the staleness cue and the refresh countdown follow the
/// same cadence the scheduler polls at. Each report also carries this process's
/// queued and in-flight refreshes. Disk-only, like [`reports`].
pub(crate) fn reports_with(interval_ms: Option<u64>) -> Result<Vec<ProviderReport>> {
    let cfg = config::load()?;
    let root = cache_dir()?;
    let now = crate::usage::now_ms();
    let poll = poll_ms(&cfg, interval_ms);
    let stale_after = stale_after_ms(&cfg, interval_ms);
    let queued = pending_ids();
    let in_flight = in_flight_bindings();
    Ok(cfg
        .targets
        .iter()
        .filter(|t| t.enabled)
        .map(|t| {
            let (report, failures) = cached_entry(&root, t);
            let mut report = stamp_listed(age_report_after(report, &cfg, stale_after, now), t);
            report.refresh = RefreshState {
                failures,
                next_check_ms: report
                    .checked_at_ms
                    .map(|at| at.saturating_add(retry_delay_ms(poll, failures))),
                queued: queued.all || queued.ids.contains(&t.id),
                refreshing: in_flight.contains(&binding(t)),
            };
            report
        })
        .collect())
}

fn retry_delay_ms(poll_ms: u64, failures: u32) -> u64 {
    poll_ms
        .saturating_mul(1u64 << failures.min(4))
        .min(MAX_POLL_MS)
}

fn due(cache: Option<&Cached>, poll_ms: u64, now: u64, force: bool) -> bool {
    let Some(cache) = cache else {
        return true;
    };
    // Manual refresh can retry authentication after login, but must respect 429 backoff.
    if force && cache.report.state != ObservationState::RateLimited {
        return true;
    }
    cache
        .report
        .checked_at_ms
        .is_none_or(|at| now.saturating_sub(at) >= retry_delay_ms(poll_ms, cache.failures))
}

fn record_result(
    target: &TargetConfig,
    old: Option<Cached>,
    outcome: std::result::Result<ProviderData, ProviderError>,
    now: u64,
) -> Cached {
    let failures = old.as_ref().map_or(0, |c| c.failures);
    let mut report = old
        .map(|c| c.report)
        .unwrap_or_else(|| empty_report(target));
    report.checked_at_ms = Some(now);
    let failures = match outcome {
        Ok(data) => {
            report.data = data;
            report.observed_at_ms = Some(now);
            report.state = ObservationState::Fresh;
            report.message = None;
            0
        }
        Err(error) => {
            report.state = error.state;
            report.message = Some(error.message.into());
            report.warning = false;
            failures.saturating_add(1)
        }
    };
    Cached {
        binding: binding(target),
        credential_binding: None,
        failures,
        report,
    }
}

fn fetch(
    target: &TargetConfig,
    credentials: &Value,
) -> std::result::Result<ProviderData, ProviderError> {
    match target.provider {
        ProviderKind::Codex => codex::fetch(target, credentials),
        ProviderKind::Grok => grok::fetch(target, credentials),
        ProviderKind::Antigravity => antigravity::fetch(target, credentials),
    }
}

fn refresh_one(root: &Path, target: &TargetConfig, poll_ms: u64, force: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(root.join(format!("{}.lock", binding(target))))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(_) => anyhow::bail!("cannot acquire provider refresh lock"),
    }
    let old = cached(root, target);
    if !due(old.as_ref(), poll_ms, crate::usage::now_ms(), force) {
        return Ok(());
    }
    let _in_flight = InFlight::enter(binding(target));
    let mut data = match credentials(target) {
        Ok(snapshot) => {
            let before = credential_binding(&snapshot);
            let old = old.filter(|c| cache_identity_matches(c, Some(&before)));
            let outcome = fetch(target, &snapshot);
            let after = credentials(target).ok().as_ref().map(credential_binding);
            if after.as_deref() != Some(&before) {
                record_result(
                    target,
                    None,
                    Err(ProviderError::auth(
                        "native login changed during refresh; retry with a stable login",
                    )),
                    crate::usage::now_ms(),
                )
            } else {
                let mut data = record_result(target, old, outcome, crate::usage::now_ms());
                data.credential_binding = Some(before);
                data
            }
        }
        Err(error) => record_result(target, None, Err(error), crate::usage::now_ms()),
    };
    data.report.identity_checked_at_observation_only = native_keyring(target);
    crate::profile::atomic_write_600(&cache_path(root, target), serde_json::to_vec_pretty(&data)?)?;
    Ok(())
}

/// Refresh every due target now, on the calling thread. `force` also checks
/// targets whose cadence has not come round. Paced by `poll_interval_seconds`:
/// this is the `clauth providers status` / `refresh` path.
pub(crate) fn refresh(force: bool) -> Result<()> {
    refresh_pass(
        &Pending {
            all: force,
            ids: HashSet::new(),
        },
        None,
    )
}

fn refresh_pass(pending: &Pending, interval_ms: Option<u64>) -> Result<()> {
    let cfg = config::load()?;
    if cfg.targets.is_empty() {
        return Ok(());
    }
    let root = cache_dir()?;
    crate::profile::mkdir_700(&root)?;
    let poll = poll_ms(&cfg, interval_ms);
    // Targets that share a login are one account: forcing any of them forces
    // the one check they share.
    let forced: HashSet<String> = cfg
        .targets
        .iter()
        .filter(|t| pending.forces(t))
        .map(binding)
        .collect();
    // Distinct providers cannot starve one another; each persists its own result.
    std::thread::scope(|scope| {
        let mut sources = HashSet::new();
        let handles: Vec<_> = cfg
            .targets
            .iter()
            .filter(|t| t.enabled)
            .filter(|t| sources.insert(binding(t)))
            .map(|t| {
                let root = &root;
                let force = forced.contains(&binding(t));
                scope.spawn(move || refresh_one(root, t, poll, force))
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("provider refresh worker failed"))??;
        }
        Ok(())
    })
}

/// Manual refreshes asked for in this process and not yet started. The next
/// pass takes them all at once, so a request made while a pass runs is served
/// by the pass after it rather than dropped.
#[derive(Debug, Clone, Default)]
struct Pending {
    all: bool,
    ids: HashSet<String>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        !self.all && self.ids.is_empty()
    }

    fn forces(&self, target: &TargetConfig) -> bool {
        self.all || self.ids.contains(&target.id)
    }
}

static REFRESHING: AtomicBool = AtomicBool::new(false);
static PENDING: LazyLock<Mutex<Pending>> = LazyLock::new(|| Mutex::new(Pending::default()));
/// Bindings this process is fetching now.
static IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
/// The refresh interval the last caller paced by, in ms. Zero until one does.
static INTERVAL_MS: AtomicU64 = AtomicU64::new(0);

/// Marks a binding in flight for as long as the fetch holds it.
struct InFlight(String);

impl InFlight {
    fn enter(binding: String) -> Self {
        if let Ok(mut set) = IN_FLIGHT.lock() {
            set.insert(binding.clone());
        }
        Self(binding)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut set) = IN_FLIGHT.lock() {
            set.remove(&self.0);
        }
    }
}

fn pending_ids() -> Pending {
    PENDING.lock().map(|p| p.clone()).unwrap_or_default()
}

fn take_pending() -> Pending {
    PENDING
        .lock()
        .map(|mut p| std::mem::take(&mut *p))
        .unwrap_or_default()
}

fn in_flight_bindings() -> HashSet<String> {
    IN_FLIGHT.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Called only by the scheduler's existing fetch-lease owner, with the Config
/// tab refresh interval. Network/keyring work stays off its tick thread, so
/// provider failures do not delay fallback.
pub(crate) fn schedule_refresh(interval_ms: u64) {
    INTERVAL_MS.store(interval_ms, Ordering::Relaxed);
    schedule();
}

/// Check every enabled target on the next pass, whatever its cadence (`r` on
/// Overview or Providers). A rate-limited target still waits out its backoff.
pub(crate) fn request_refresh(interval_ms: u64) {
    if let Ok(mut pending) = PENDING.lock() {
        pending.all = true;
    }
    INTERVAL_MS.store(interval_ms, Ordering::Relaxed);
    schedule();
}

/// Check the one target `id` on the next pass (`r` on its Usage row). Runs in
/// this process even while another one holds the usage-fetch lease: the
/// per-target file lock is what keeps two processes off the same account.
pub(crate) fn request_refresh_target(id: &str, interval_ms: u64) {
    if let Ok(mut pending) = PENDING.lock() {
        pending.ids.insert(id.to_string());
    }
    INTERVAL_MS.store(interval_ms, Ordering::Relaxed);
    schedule();
}

fn schedule() {
    // No detached home-reading worker is needed for the default empty config.
    match config::load() {
        Ok(cfg) if cfg.targets.iter().any(|t| t.enabled) => {}
        Ok(_) => {
            take_pending();
            return;
        }
        Err(error) => {
            crate::logline::logline!("clauth: provider monitor: {error}");
            return;
        }
    }
    if REFRESHING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // The running pass looks for pending requests before it exits.
        return;
    }
    #[cfg(test)]
    let done = crate::testutil::register_background_task();
    std::thread::spawn(move || {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                REFRESHING.store(false, Ordering::Release);
            }
        }
        let reset_guard = Reset;
        loop {
            let pending = take_pending();
            let interval = Some(INTERVAL_MS.load(Ordering::Relaxed)).filter(|ms| *ms > 0);
            if let Err(e) = refresh_pass(&pending, interval) {
                crate::logline::logline!("clauth: provider monitor: {e}");
            }
            if pending_ids().is_empty() {
                break;
            }
        }
        drop(reset_guard);
        // A request that arrived between the last check and the reset found
        // the flag still set and left its work pending. Start it now.
        if !pending_ids().is_empty() {
            schedule();
        }
        #[cfg(test)]
        let _ = done.send(());
    });
}

pub(crate) fn init_config() -> Result<PathBuf> {
    use std::io::Write;
    let path = config::path()?;
    if let Some(parent) = path.parent() {
        crate::profile::mkdir_700(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .context("providers.toml already exists or cannot be created; it was not overwritten")?;
    file.write_all(config::EXAMPLE.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> TargetConfig {
        let mut target = config::parse(config::EXAMPLE).unwrap().targets.remove(0);
        target.auth_file = Some(PathBuf::from("/synthetic/codex-auth.json"));
        target
    }

    #[test]
    fn codex_and_grok_payloads_keep_every_returned_window() {
        let codex = codex::parse(
            &serde_json::json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {"used_percent": 20.0, "limit_window_seconds": 18000},
                    "secondary_window": {"used_percent": 70.0, "limit_window_seconds": 18000}
                }
            }),
            0,
        )
        .unwrap();
        let grok = grok::parse(
            &serde_json::json!({"subscriptionTier": "SuperGrok"}),
            &serde_json::json!({"config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-09-01T00:00:00+00:00",
                    "end": "2026-09-08T00:00:00+00:00"
                },
                "creditUsagePercent": 23.5,
                "productUsage": [
                    {"product": "GrokBuild", "usagePercent": 20.0},
                    {"product": "GrokChat", "usagePercent": 3.5}
                ]
            }}),
        )
        .unwrap();
        let codex_remaining: Vec<_> = codex
            .buckets
            .iter()
            .map(|bucket| (bucket.id.as_str(), bucket.remaining_percent))
            .collect();
        assert_eq!(
            codex_remaining,
            [
                ("shared:primary_window", Some(80.0)),
                ("shared:secondary_window", Some(30.0))
            ]
        );
        assert_ne!(
            codex.buckets[0].remaining_percent,
            codex.buckets[1].remaining_percent
        );
        assert_eq!(grok.buckets.len(), 1);
        assert_eq!(grok.buckets[0].remaining_percent, Some(76.5));
        assert_eq!(
            grok.attribution
                .iter()
                .map(|item| (item.label.as_str(), item.used_percent))
                .collect::<Vec<_>>(),
            [("GrokBuild", 20.0), ("GrokChat", 3.5)]
        );
    }

    #[test]
    fn failed_poll_retains_data_but_revokes_freshness_and_warning() {
        let t = target();
        let first = record_result(
            &t,
            None,
            Ok(ProviderData {
                plan: Some("pro".into()),
                ..Default::default()
            }),
            1000,
        );
        let failed = record_result(&t, Some(first), Err(ProviderError::http(401)), 2000);
        assert_eq!(failed.report.observed_at_ms, Some(1000));
        assert_eq!(failed.report.checked_at_ms, Some(2000));
        assert_eq!(failed.report.data.plan.as_deref(), Some("pro"));
        assert_eq!(failed.report.state, ObservationState::AuthRequired);
        assert!(!failed.report.warning);
    }

    #[test]
    fn stale_data_is_not_a_fresh_quota_warning() {
        let cfg = MonitorConfig::default();
        let first = record_result(&target(), None, Ok(ProviderData::default()), 1000);
        assert_eq!(
            age_report_after(first.report, &cfg, stale_after_ms(&cfg, None), 500_000).state,
            ObservationState::Stale
        );
    }

    #[test]
    fn forced_refresh_does_not_bypass_rate_limit_backoff() {
        let cfg = MonitorConfig::default();
        let failed = record_result(&target(), None, Err(ProviderError::http(429)), 1000);
        let poll = poll_ms(&cfg, None);
        assert!(!due(Some(&failed), poll, 1001, true));
        assert!(due(Some(&failed), poll, 1_000_000, false));
    }

    #[test]
    fn changed_target_does_not_reuse_another_accounts_cache() {
        let t = target();
        let mut different = t.clone();
        different.auth_file = Some(PathBuf::from("/other/auth.json"));
        assert_ne!(binding(&t), binding(&different));
    }

    #[test]
    fn model_targets_share_one_quota_source() {
        let t = target();
        let mut other = t.clone();
        other.id = "codex-other-model".into();
        other.model = Some("another-model".into());
        assert_eq!(binding(&t), binding(&other));
    }

    #[test]
    fn cached_read_rejects_replaced_native_credentials_and_maps_target() {
        let root = tempfile::tempdir().unwrap();
        let auth = root.path().join("auth.json");
        let mut t = target();
        t.auth_file = Some(auth.clone());
        let first = serde_json::json!({"tokens":{"access_token":"synthetic-a"}});
        std::fs::write(&auth, first.to_string()).unwrap();
        let mut c = record_result(
            &t,
            None,
            Ok(ProviderData {
                plan: Some("first-account".into()),
                ..Default::default()
            }),
            1000,
        );
        c.credential_binding = Some(credential_binding(&first));
        std::fs::write(cache_path(root.path(), &t), serde_json::to_vec(&c).unwrap()).unwrap();
        t.id = "another-model-target".into();
        t.model = Some("model-b".into());
        let report = cached_report(root.path(), &t);
        assert_eq!(report.id, "another-model-target");
        assert_eq!(report.model.as_deref(), Some("model-b"));
        assert_eq!(report.data.plan.as_deref(), Some("first-account"));
        std::fs::write(&auth, r#"{"tokens":{"access_token":"synthetic-b"}}"#).unwrap();
        let report = cached_report(root.path(), &t);
        assert_eq!(report.state, ObservationState::NotFetched);
        assert!(report.data.plan.is_none());
        std::fs::remove_file(auth).unwrap();
        assert!(cached_report(root.path(), &t).data.plan.is_none());
    }

    #[test]
    fn the_refresh_interval_paces_native_polls_above_the_monitor_floor() {
        let cfg = MonitorConfig::default();
        assert_eq!(
            poll_ms(&cfg, None),
            120_000,
            "CLI path keeps providers.toml"
        );
        assert_eq!(poll_ms(&cfg, Some(90_000)), 90_000, "Config tab cadence");
        assert_eq!(poll_ms(&cfg, Some(10_000)), 30_000, "never below 30 s");
        assert_eq!(poll_ms(&cfg, Some(7_200_000)), 3_600_000, "never above 1 h");
    }

    #[test]
    fn a_native_reading_goes_stale_when_a_claude_one_would() {
        let cfg = MonitorConfig::default();
        assert_eq!(stale_after_ms(&cfg, None), 360_000);
        // The Claude rule at 90 s: 2 × max(90 s, 5 min) + 90 s.
        assert_eq!(
            stale_after_ms(&cfg, Some(90_000)),
            crate::profile_json::stale_after_ms(90_000)
        );
        let first = record_result(&target(), None, Ok(ProviderData::default()), 1000);
        let at_six_minutes = 1000 + 400_000;
        assert_eq!(
            age_report_after(
                first.report.clone(),
                &cfg,
                stale_after_ms(&cfg, None),
                at_six_minutes
            )
            .state,
            ObservationState::Stale
        );
        assert_eq!(
            age_report_after(
                first.report,
                &cfg,
                stale_after_ms(&cfg, Some(90_000)),
                at_six_minutes
            )
            .state,
            ObservationState::Fresh
        );
    }

    fn checked_at(interval_ms: Option<u64>) -> std::collections::HashMap<String, Option<u64>> {
        reports_with(interval_ms)
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r.checked_at_ms))
            .collect()
    }

    #[test]
    fn a_targeted_refresh_checks_only_that_login() {
        let _home = crate::testutil::HomeSandbox::new();
        init_config().unwrap();
        refresh(true).unwrap();
        let before = checked_at(Some(90_000));
        assert!(before.values().all(Option::is_some), "{before:?}");
        std::thread::sleep(Duration::from_millis(5));
        refresh_pass(
            &Pending {
                all: false,
                ids: HashSet::from(["grok".to_string()]),
            },
            Some(90_000),
        )
        .unwrap();
        let after = checked_at(Some(90_000));
        assert_ne!(
            after["grok"], before["grok"],
            "the requested login was checked"
        );
        assert_eq!(
            after["codex"], before["codex"],
            "a login inside its cadence waits"
        );
        assert_eq!(after["agy"], before["agy"]);
    }

    #[test]
    fn reports_carry_the_failure_streak_and_the_next_check() {
        let _home = crate::testutil::HomeSandbox::new();
        init_config().unwrap();
        refresh(true).unwrap();
        for report in reports_with(Some(90_000)).unwrap() {
            let at = report.checked_at_ms.expect("checked");
            assert_eq!(report.refresh.failures, 1, "{}", report.id);
            // One failure doubles the 90 s cadence.
            assert_eq!(report.refresh.next_check_ms, Some(at + 180_000));
            assert!(!report.refresh.refreshing);
        }
    }

    #[test]
    fn scheduled_worker_finishes_before_home_sandbox_release() {
        let _home = crate::testutil::HomeSandbox::new();
        init_config().unwrap();
        request_refresh(90_000);
        crate::testutil::join_background_tasks();
        assert!(!REFRESHING.load(Ordering::Acquire));
        let reports = reports().unwrap();
        assert_eq!(reports.len(), 3);
        assert!(
            reports
                .iter()
                .all(|r| r.state == ObservationState::AuthRequired)
        );
    }
}
