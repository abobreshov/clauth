//! The per-monitor cache, `~/.tollgate/monitors/<id>.json`, and the one
//! refresh path that writes it (plan v3.1 §4.2, §3.2).
//!
//! - **TTL:** a monitor is due [`MonitorConfig::ttl_ms`] (default 90 s) after
//!   its last attempt.
//! - **Backoff:** a 429 holds the monitor for at least 5 minutes (longer when
//!   the source's `retry-after` says so); nothing, not even `monitor refresh`,
//!   fetches a held monitor.
//! - **Stale retention:** a failed attempt keeps the last good reading; the
//!   read side drops it past 7 days.
//! - **Single flight:** a refresh holds `<id>.lock` (a non-blocking flock) for
//!   its whole read-fetch-write, so two processes (the daemon, a `monitor
//!   refresh`) never fetch one monitor at once; the loser reports busy. The
//!   winner re-reads the cache under the lock, so a refresh that just landed
//!   elsewhere is not repeated.
//! - **Identity:** the cache records the monitor's non-secret fingerprint; a
//!   cache written for another target (a changed kind, env var or Hermes home)
//!   is discarded.
//!
//! The file is written atomically at 0600 and never holds a credential: a
//! [`super::source::Secret`] has no `Serialize`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::alert::{AlertState, Notifier, evaluate};
use super::config::{MonitorConfig, monitors_dir};
use super::observe::observe_monitor;
use super::source::{EnvReader, MonitorHttp, Reading, resolve_target, source_for};
use crate::usage::observation::{Failure, FailureKind, Freshness};

/// Bumped when the cache shape changes incompatibly; an older file is
/// discarded.
pub(crate) const CACHE_VERSION: u32 = 1;
/// How long a reading stays visible after its fetch.
pub(crate) const STALE_RETENTION_MS: u64 = 7 * 86_400_000;
/// The shortest hold after a 429.
pub(crate) const RATE_LIMIT_HOLD_MS: u64 = 5 * 60_000;
/// Largest cache file read.
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024;

/// One monitor's persisted state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MonitorCache {
    pub(crate) version: u32,
    pub(crate) id: String,
    /// [`MonitorConfig::fingerprint`] at write time.
    pub(crate) fingerprint: String,
    /// The last attempt, successful or not, epoch ms.
    pub(crate) checked_at_ms: Option<u64>,
    /// The last successful fetch, epoch ms.
    pub(crate) observed_at_ms: Option<u64>,
    /// The last good figures.
    pub(crate) reading: Option<Reading>,
    /// The last attempt's failure; `None` after a success.
    pub(crate) failure: Option<Failure>,
    /// No fetch before this instant (a 429 hold), epoch ms.
    pub(crate) hold_until_ms: Option<u64>,
    #[serde(default)]
    pub(crate) alerts: AlertState,
}

impl MonitorCache {
    fn empty(cfg: &MonitorConfig) -> Self {
        Self {
            version: CACHE_VERSION,
            id: cfg.id.clone(),
            fingerprint: cfg.fingerprint(),
            checked_at_ms: None,
            observed_at_ms: None,
            reading: None,
            failure: None,
            hold_until_ms: None,
            alerts: AlertState::default(),
        }
    }

    /// Written for this monitor's current target.
    pub(crate) fn matches(&self, cfg: &MonitorConfig) -> bool {
        self.version == CACHE_VERSION && self.id == cfg.id && self.fingerprint == cfg.fingerprint()
    }

    /// Held by a 429 at `now_ms`.
    pub(crate) fn held(&self, now_ms: u64) -> bool {
        self.hold_until_ms.is_some_and(|until| now_ms < until)
    }
}

/// `~/.tollgate/monitors/<id>.json`.
pub(crate) fn cache_path(id: &str) -> Result<PathBuf> {
    Ok(monitors_dir()?.join(format!("{id}.json")))
}

fn lock_path(id: &str) -> Result<PathBuf> {
    Ok(monitors_dir()?.join(format!("{id}.lock")))
}

/// The cache for `id`; `None` when absent, unreadable or of another version.
/// Tolerant: a torn or foreign file reads as no cache.
pub(crate) fn load(id: &str) -> Option<MonitorCache> {
    use std::io::Read as _;
    let path = cache_path(id).ok()?;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return None;
    }
    serde_json::from_slice::<MonitorCache>(&bytes)
        .ok()
        .filter(|c| c.version == CACHE_VERSION && c.id == id)
}

fn save(cache: &MonitorCache) -> Result<()> {
    let path = cache_path(&cache.id)?;
    let bytes = serde_json::to_vec_pretty(cache).context("serialise the monitor cache")?;
    crate::profile::atomic_write_600(&path, bytes)
        .with_context(|| format!("write {}", path.display()))
}

/// Remove `id`'s cache and lock files (after `monitor remove`).
pub(crate) fn remove(id: &str) {
    for path in [cache_path(id), lock_path(id)].into_iter().flatten() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether `cfg` should be fetched at `now_ms` given its cache.
pub(crate) fn is_due(cfg: &MonitorConfig, cache: Option<&MonitorCache>, now_ms: u64) -> bool {
    let Some(cache) = cache.filter(|c| c.matches(cfg)) else {
        return true;
    };
    if cache.held(now_ms) {
        return false;
    }
    cache
        .checked_at_ms
        .is_none_or(|at| now_ms.saturating_sub(at) >= cfg.ttl_ms() || now_ms < at)
}

/// What one refresh did.
#[derive(Debug)]
pub(crate) enum RefreshOutcome {
    /// Another process holds this monitor's lock and is fetching it.
    Busy,
    /// Not due yet (only without `force`).
    NotDue(Box<MonitorCache>),
    /// A 429 hold is in force (the cache's `hold_until_ms`).
    Held(Box<MonitorCache>),
    /// Fetched; the cache as written.
    Refreshed(Box<MonitorCache>),
}

impl RefreshOutcome {
    /// The cache this outcome leaves on disk, when known.
    pub(crate) fn cache(&self) -> Option<&MonitorCache> {
        match self {
            Self::Busy => None,
            Self::NotDue(c) | Self::Held(c) | Self::Refreshed(c) => Some(c),
        }
    }
}

/// Everything a refresh needs from the outside world.
pub(crate) struct RefreshDeps<'a> {
    pub(crate) http: &'a dyn MonitorHttp,
    /// `None` skips alert evaluation (the alert state is left for the daemon).
    pub(crate) notifier: Option<&'a dyn Notifier>,
    pub(crate) env: EnvReader<'a>,
    pub(crate) now_ms: u64,
}

/// Refresh one monitor: take its lock, re-read its cache, fetch when due (or
/// `force`d) and not held, write the result, and evaluate alerts.
pub(crate) fn refresh_one(
    cfg: &MonitorConfig,
    deps: &RefreshDeps<'_>,
    force: bool,
) -> Result<RefreshOutcome> {
    let dir = monitors_dir()?;
    crate::profile::mkdir_700(&dir).with_context(|| format!("create {}", dir.display()))?;
    let lock =
        crate::profile::open_state_file(&lock_path(&cfg.id)?).context("open the monitor lock")?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(RefreshOutcome::Busy),
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(e).context("lock the monitor cache");
        }
    }
    let now_ms = deps.now_ms;
    let prev = load(&cfg.id).filter(|c| c.matches(cfg));
    if let Some(c) = prev.as_ref().filter(|c| c.held(now_ms)) {
        return Ok(RefreshOutcome::Held(Box::new(c.clone())));
    }
    if !force && !is_due(cfg, prev.as_ref(), now_ms) {
        let c = prev.unwrap_or_else(|| MonitorCache::empty(cfg));
        return Ok(RefreshOutcome::NotDue(Box::new(c)));
    }

    let home = crate::profile::home_dir()?;
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    let target = resolve_target(cfg, &home, now_secs, deps.env);
    let result = source_for(cfg.kind).fetch(&target, deps.http);
    drop(target);

    let mut next = prev.unwrap_or_else(|| MonitorCache::empty(cfg));
    next.checked_at_ms = Some(now_ms);
    match result {
        Ok(reading) => {
            next.reading = Some(reading);
            next.observed_at_ms = Some(now_ms);
            next.failure = None;
            next.hold_until_ms = None;
        }
        Err(failure) => {
            // A quota 429 (Ollama's "usage limit reached") is still a 429:
            // held the same way, so a spent window is not re-polled.
            if matches!(
                failure.kind,
                FailureKind::RateLimited | FailureKind::QuotaExhausted
            ) {
                let retry_ms = failure
                    .retry_after
                    .and_then(|t| u64::try_from(t.secs()).ok())
                    .map(|s| s.saturating_mul(1000));
                let floor = now_ms.saturating_add(RATE_LIMIT_HOLD_MS);
                next.hold_until_ms = Some(retry_ms.map_or(floor, |r| r.max(floor)));
            }
            next.failure = Some(failure);
        }
    }

    if let Some(notifier) = deps.notifier {
        let obs = observe_monitor(cfg, Some(&next), now_ms, |at| match at {
            Some(_) => Freshness::Fresh,
            None => Freshness::NotFetched,
        });
        let (notes, state) = evaluate(cfg, &obs, &next.alerts, now_secs);
        for n in &notes {
            notifier.send(n);
        }
        next.alerts = state;
    }

    save(&next)?;
    drop(lock);
    Ok(RefreshOutcome::Refreshed(Box::new(next)))
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_cache.rs"]
mod tests;
