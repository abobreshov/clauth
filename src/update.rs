//! Self-update, compiled out of this build.
//!
//! The upstream updater fetched the newest release from upstream's GitHub
//! repo and verified it against upstream's pinned minisign key. This fork can
//! verify neither, and a self-replace from upstream would silently turn the
//! fork back into upstream (plan `docs/multi-provider-redesign-plan.md` §4.0).
//! So every update path is a no-op by construction: no release API URL, no
//! pinned key, no download, no self-replace.
//!
//! TODO(plan §4.0 R1): a fork-signed updater — the fork's own release API
//! URL, its own pinned minisign key and asset prefix from `identity` — lands
//! here once the fork publishes signed releases.

use std::thread::JoinHandle;

/// What every user-facing update command or knob answers in this build.
pub(crate) const DISABLED_MESSAGE: &str =
    "self-update is disabled in this build; reinstall from source";

/// The one shared update gate. Both consumers — [`spawn`] and
/// `herdr::heal_detached` (a network reinstall of the herdr plugin) — route
/// through it. Always `false` in this build, whatever the persisted
/// `[update].auto_update` setting says: nothing may download and install.
pub(crate) fn updates_enabled(saved_auto_update: bool) -> bool {
    let _ = saved_auto_update;
    false
}

/// The background update check. A no-op in this build: never spawns a thread,
/// never touches the network, always `None`.
pub(crate) fn spawn(saved_auto_update: bool) -> Option<JoinHandle<()>> {
    debug_assert!(!updates_enabled(saved_auto_update));
    None
}

#[cfg(test)]
#[path = "../tests/inline/update.rs"]
mod tests;
