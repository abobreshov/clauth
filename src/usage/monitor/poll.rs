//! The daemon's monitor leg: on each tick, fetch the monitors that are due,
//! off the tick thread (plan v3.1 §4.2 "one scheduler, one owner per target").
//!
//! [`poll_detached`] is what the tick calls. It scans `monitors.toml` at most
//! every [`SCAN_EVERY_MS`], and when anything is due and no poll is already
//! running it hands the due set to one background thread, so a slow provider
//! can never hold the tick past the watchdog. [`poll_due`] is that thread's
//! body, synchronous and injectable, which is what the tests drive.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::alert::DesktopNotifier;
use super::cache::{RefreshDeps, RefreshOutcome, is_due, load, refresh_one};
use super::config::MonitorConfig;
use super::source::{LiveHttp, process_env};
use crate::logline::logline;

/// The shortest gap between two scans of `monitors.toml`.
pub(crate) const SCAN_EVERY_MS: u64 = 10_000;

static LAST_SCAN_MS: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// The due, enabled monitors at `now_ms`.
pub(crate) fn due_monitors(monitors: Vec<MonitorConfig>, now_ms: u64) -> Vec<MonitorConfig> {
    monitors
        .into_iter()
        .filter(|m| m.enabled && is_due(m, load(&m.id).as_ref(), now_ms))
        .collect()
}

/// Refresh every monitor in `due`, in order. Failures are logged by id and
/// kind only — never a credential, never a provider body.
pub(crate) fn poll_due(due: &[MonitorConfig], deps: &RefreshDeps<'_>) {
    for m in due {
        match refresh_one(m, deps, false) {
            Ok(RefreshOutcome::Refreshed(c)) => {
                if let Some(f) = &c.failure {
                    logline!("tollgate: monitor {}: {:?}: {}", m.id, f.kind, f.message);
                }
            }
            Ok(RefreshOutcome::Busy | RefreshOutcome::NotDue(_) | RefreshOutcome::Held(_)) => {}
            Err(e) => logline!("tollgate: monitor {}: refresh failed: {e:#}", m.id),
        }
    }
}

/// The tick's entry point: scan, and spawn one poll for the due monitors.
/// Never blocks on the network.
pub(crate) fn poll_detached() {
    let now = crate::usage::now_ms();
    let last = LAST_SCAN_MS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < SCAN_EVERY_MS && now >= last {
        return;
    }
    LAST_SCAN_MS.store(now, Ordering::Relaxed);
    let monitors = match super::config::load() {
        Ok(m) => m,
        Err(e) => {
            logline!("tollgate: monitors.toml: {e:#}");
            return;
        }
    };
    let due = due_monitors(monitors, now);
    if due.is_empty() || IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    #[cfg(test)]
    let done = crate::testutil::register_background_task();
    std::thread::spawn(move || {
        // Cleared on unwind too, so one panicking fetch cannot park the leg.
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        let _clear = Clear;
        let deps = RefreshDeps {
            http: &LiveHttp,
            notifier: Some(&DesktopNotifier),
            env: &process_env,
            now_ms: crate::usage::now_ms(),
        };
        poll_due(&due, &deps);
        #[cfg(test)]
        let _ = done.send(());
    });
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_poll.rs"]
mod tests;
