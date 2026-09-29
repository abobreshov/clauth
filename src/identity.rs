//! The tool's identity in one place.
//!
//! tollgate is a hard fork of `uwuclxdy/clauth` that runs beside the upstream
//! install on the same machine (plan `docs/multi-provider-redesign-plan.md`
//! §4.0). Every global resource the two could collide on — the binary name,
//! the data dir, the env prefix, the herdr plugin id, the Claude Code plugin,
//! the daemon's listen port — is named here, so a chokepoint reads one
//! constant rather than a scattered literal.
//!
//! The `UPSTREAM_*` constants name the upstream tool's own resources. They are
//! what the coexistence and `import clauth` work (§4.0) reads to recognise, and
//! never touch by accident, the live upstream install.

/// The binary / package name, and what every process-name match compares
/// against.
pub(crate) const NAME: &str = "tollgate";

/// The data directory's name under `$HOME` (`~/.tollgate`).
pub(crate) const DATA_DIR_NAME: &str = ".tollgate";

/// The prefix of every env var the tool reads or sets on a child
/// (`TOLLGATE_NO_API`, `TOLLGATE_MCP_DEPTH`, …).
#[allow(
    dead_code,
    reason = "the env-var sites still spell the prefix out; §4.0 routes them here"
)]
pub(crate) const ENV_PREFIX: &str = "TOLLGATE_";

/// `owner/repo` of the fork on GitHub. The repo keeps its upstream name until
/// it is renamed.
pub(crate) const REPO_SLUG: &str = "abobreshov/clauth";

/// The herdr plugin's manifest id, and the prefix of its qualified actions.
pub(crate) const HERDR_PLUGIN_ID: &str = "tollgate";

/// `owner/repo/subdir`, the source `herdr plugin install` fetches the herdr
/// plugin from.
pub(crate) const HERDR_GITHUB_SOURCE: &str = "abobreshov/clauth/herdr-plugin";

/// The Claude Code plugin's `name@marketplace` key.
pub(crate) const CC_PLUGIN: &str = "tollgate@tollgate";

/// Where a value-less `daemon --listen` binds. Upstream uses `0.0.0.0:8443`;
/// this must differ so both daemons can listen at once.
pub(crate) const DEFAULT_LISTEN: &str = "0.0.0.0:8453";

/// The upstream tool's binary name.
#[allow(
    dead_code,
    reason = "read by the coexistence / import work (plan §4.0)"
)]
pub(crate) const UPSTREAM_NAME: &str = "clauth";

/// The upstream tool's data directory name under `$HOME` (`~/.clauth`).
#[allow(
    dead_code,
    reason = "read by the coexistence / import work (plan §4.0)"
)]
pub(crate) const UPSTREAM_DATA_DIR_NAME: &str = ".clauth";

/// The upstream tool's herdr plugin id.
#[allow(
    dead_code,
    reason = "read by the coexistence / import work (plan §4.0)"
)]
pub(crate) const UPSTREAM_HERDR_PLUGIN_ID: &str = "clauth";

/// The upstream tool's Claude Code plugin key.
#[allow(
    dead_code,
    reason = "read by the coexistence / import work (plan §4.0)"
)]
pub(crate) const UPSTREAM_CC_PLUGIN: &str = "clauth@clauth";

/// The `owner` half of [`REPO_SLUG`].
pub(crate) fn repo_owner() -> &'static str {
    REPO_SLUG
        .split_once('/')
        .map_or(REPO_SLUG, |(owner, _)| owner)
}

/// The `repo` half of [`REPO_SLUG`].
pub(crate) fn repo_name() -> &'static str {
    REPO_SLUG
        .split_once('/')
        .map_or(REPO_SLUG, |(_, name)| name)
}

#[cfg(test)]
#[path = "../tests/inline/identity.rs"]
mod tests;
