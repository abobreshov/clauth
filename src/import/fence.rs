//! The import fence (spec §4.2, the plan's M3): every lock an upstream or
//! tollgate writer of the imported state would take, held exclusively for the
//! whole transaction, in one fixed order under one `ImportFence` rank.
//!
//! 1. `~/.clauth/clauthd.lock`
//! 2. `~/.clauth/clauthd-standby.lock`
//! 3. `~/.clauth/usage-fetch.lock`
//! 4. `~/.tollgate/tollgated.lock`
//! 5. `~/.tollgate/tollgated-standby.lock`
//! 6. `~/.tollgate/usage-fetch.lock`
//! 7. `~/.clauth/rotation-locks/<p>.lock` for every upstream profile, sorted
//!    by name bytes
//! 8. `~/.clauth/.lock` (upstream state, 2 s bounded wait)
//! 9. tollgate's state flock (`StateLock`, 2 s bounded wait, rank `State`)
//!
//! Items 1–8 are raw `File` flocks, each non-blocking but item 8; none goes
//! through `RotationGuard`, which would re-enter rank 100 once per profile.
//! After item 9 nothing below `State` is entered. While items 1 and 3 are
//! held `daemon::probe::upstream_refresher_active` reads true, so a tollgate
//! refresher that slips in stands down.

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use super::{Finding, Paths, procs};
use crate::lockorder::{RankGuard, rank};

/// The bounded wait on fence items 8 and 9.
pub(crate) const ITEM_WAIT: Duration = Duration::from_secs(2);

/// Fence items 1–8 in acquisition order (item 9 is the state flock).
pub(crate) fn item_paths(paths: &Paths, upstream_profiles: &[String]) -> Vec<PathBuf> {
    let mut out = vec![
        paths.source.join("clauthd.lock"),
        paths.source.join("clauthd-standby.lock"),
        paths.source.join("usage-fetch.lock"),
        paths.target.join("tollgated.lock"),
        paths.target.join("tollgated-standby.lock"),
        paths.target.join("usage-fetch.lock"),
    ];
    let mut names: Vec<&String> = upstream_profiles.iter().collect();
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    names.dedup();
    for name in names {
        out.push(
            crate::runtime::upstream_rotation_lock_path(name).unwrap_or_else(|_| {
                paths
                    .source
                    .join("rotation-locks")
                    .join(format!("{name}.lock"))
            }),
        );
    }
    out.push(paths.source.join(crate::guest_write::UPSTREAM_LOCK_FILE));
    out
}

/// Tollgate's state lock file (item 9).
pub(crate) fn state_lock_path(paths: &Paths) -> PathBuf {
    paths.target.join(crate::lock::LOCK_FILENAME)
}

/// A dry-run lock row: `free`, `held` or `absent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LockRow {
    pub(crate) path: String,
    pub(crate) state: &'static str,
}

/// Probe every fence file that exists, read-only: open without creating,
/// `try_lock_shared`, release. A held one is a `lock_held` blocker (spec
/// §4.1: the M-1 probes gate on the same files M3 takes). Creates nothing.
pub(crate) fn probe(paths: &Paths, upstream_profiles: &[String]) -> (Vec<LockRow>, Vec<Finding>) {
    let mut rows = Vec::new();
    let mut blockers = Vec::new();
    let mut all = item_paths(paths, upstream_profiles);
    all.push(state_lock_path(paths));
    for path in all {
        let shown = paths.tilde(&path);
        let state = if !path.exists() {
            "absent"
        } else if procs::held(&path) {
            blockers.push(lock_held(&shown));
            "held"
        } else {
            "free"
        };
        rows.push(LockRow { path: shown, state });
    }
    (rows, blockers)
}

fn lock_held(shown: &str) -> Finding {
    Finding::new(
        "lock_held",
        format!("{shown} is held by another process; stop it first"),
    )
    .with_path(shown.to_string())
}

/// Tries per try-locked item, [`TRY_RETRY`] apart: a presence probe
/// (`daemon_health`, a concurrent dry run's `procs::held`) takes these flocks
/// for an instant, and one busy read must not refuse the import.
const TRY_ATTEMPTS: u32 = 3;
const TRY_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

fn try_lock_retrying(file: &File) -> bool {
    for attempt in 0..TRY_ATTEMPTS {
        if file.try_lock().is_ok() {
            return true;
        }
        if attempt + 1 < TRY_ATTEMPTS {
            std::thread::sleep(TRY_RETRY);
        }
    }
    false
}

/// The held fence. Dropping it releases in reverse order (9 → 1), then pops
/// the `ImportFence` rank.
pub(crate) struct Fence {
    state: Option<crate::lock::StateLock>,
    files: Vec<File>,
    _rank: RankGuard,
}

impl Fence {
    /// Take items 1–9 in order. The first busy item releases everything
    /// already taken and answers a `lock_held` blocker; nothing is written
    /// beyond creating an absent lock file (spec I17: an absent file cannot
    /// be held against a later creator).
    pub(crate) fn acquire(paths: &Paths, upstream_profiles: &[String]) -> Result<Self, Finding> {
        let rank = RankGuard::enter::<rank::ImportFence>();
        let mut fence = Fence {
            state: None,
            files: Vec::new(),
            _rank: rank,
        };
        let items = item_paths(paths, upstream_profiles);
        let last = items.len().saturating_sub(1);
        for (i, path) in items.iter().enumerate() {
            let shown = paths.tilde(path);
            if let Some(dir) = path.parent()
                && crate::profile::mkdir_700(dir).is_err()
            {
                return Err(lock_held(&shown));
            }
            let file = crate::profile::open_state_file(path).map_err(|_| lock_held(&shown))?;
            let taken = if i == last {
                crate::lock::lock_file_with_timeout(&file, ITEM_WAIT).is_ok()
            } else {
                try_lock_retrying(&file)
            };
            if !taken {
                return Err(lock_held(&shown));
            }
            fence.files.push(file);
        }
        match crate::lock::StateLock::acquire_with_timeout(ITEM_WAIT) {
            Ok(lock) => fence.state = Some(lock),
            Err(_) => return Err(lock_held(&paths.tilde(&state_lock_path(paths)))),
        }
        super::seams::log(|| "fence acquired".to_string());
        Ok(fence)
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        let held = self.state.is_some();
        drop(self.state.take());
        while let Some(file) = self.files.pop() {
            drop(file);
        }
        if held {
            super::seams::log(|| "fence released".to_string());
        }
    }
}
