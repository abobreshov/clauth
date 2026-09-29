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

// The two spellings everything else is built from, as macros so `concat!` can
// derive the rest at compile time: a derived name cannot drift from its root.
macro_rules! tool_name {
    () => {
        "tollgate"
    };
}
macro_rules! repo_slug {
    () => {
        "abobreshov/clauth"
    };
}

/// The binary / package name, and what every process-name match compares
/// against.
pub(crate) const NAME: &str = tool_name!();

/// The data directory's name under `$HOME` (`~/.tollgate`).
pub(crate) const DATA_DIR_NAME: &str = concat!(".", tool_name!());

/// The prefix of every env var the tool reads or sets on a child
/// (`TOLLGATE_NO_API`, `TOLLGATE_MCP_DEPTH`, …).
#[allow(
    dead_code,
    reason = "the env-var sites still spell the prefix out; §4.0 routes them here"
)]
pub(crate) const ENV_PREFIX: &str = "TOLLGATE_";

/// `owner/repo` of the fork on GitHub. The repo keeps its upstream name until
/// it is renamed.
pub(crate) const REPO_SLUG: &str = repo_slug!();

/// The fork's git fetch URL (the herdr release probe's `git ls-remote`).
pub(crate) const REPO_GIT_URL: &str = concat!("https://github.com/", repo_slug!(), ".git");

/// The prefix of the fork's own release tags (`tollgate-v1.2.3`). The fork's
/// history carries every upstream `v*` tag, and each of those names upstream's
/// tree (herdr plugin id `clauth`, scripts calling `clauth`), so a probe that
/// accepted a bare `v*` tag would install upstream's plugin over the live one.
pub(crate) const RELEASE_TAG_PREFIX: &str = concat!(tool_name!(), "-v");

/// The herdr plugin's manifest id, and the prefix of its qualified actions.
pub(crate) const HERDR_PLUGIN_ID: &str = tool_name!();

/// The qualified herdr action a keybinding points at (opens the dashboard).
pub(crate) const HERDR_OPEN_ACTION: &str = concat!(tool_name!(), ".open");

/// The sidebar template reference to the pane-metadata token the herdr plugin
/// publishes the account under (key `tollgate`, `herdr::HerdrTokens`).
pub(crate) const HERDR_TOKEN: &str = concat!("$", tool_name!());

/// The pane-metadata token key the MCP server publishes delegate state under.
pub(crate) const HERDR_DELEGATE_TOKEN_KEY: &str = concat!(tool_name!(), "_delegate");

/// The sidebar template reference to [`HERDR_DELEGATE_TOKEN_KEY`].
pub(crate) const HERDR_DELEGATE_TOKEN: &str = concat!("$", tool_name!(), "_delegate");

/// Marks the tool's own blocks inside herdr's `config.toml`.
pub(crate) const HERDR_CONFIG_MARKER: &str = concat!("# ", tool_name!(), " herdr plugin");

/// `owner/repo/subdir`, the source `herdr plugin install` fetches the herdr
/// plugin from.
pub(crate) const HERDR_GITHUB_SOURCE: &str = concat!(repo_slug!(), "/herdr-plugin");

/// The Claude Code plugin's `name@marketplace` key.
pub(crate) const CC_PLUGIN: &str = concat!(tool_name!(), "@", tool_name!());

/// The hidden subcommand Claude Code's `apiKeyHelper` runs
/// (`<exe> __tollgate-api-key <profile>`). Upstream's helper spells it
/// `__api-key`; a shared token let each tool's parser read the other's helper
/// and load its OWN profile of the same name, which is the wrong account.
pub(crate) const API_KEY_HELPER_SUBCMD: &str = concat!("__", tool_name!(), "-api-key");

/// Where a value-less `daemon --listen` binds. Upstream uses `0.0.0.0:8443`;
/// this must differ so both daemons can listen at once.
pub(crate) const DEFAULT_LISTEN: &str = "0.0.0.0:8453";

/// The upstream tool's binary name.
pub(crate) const UPSTREAM_NAME: &str = "clauth";

/// The upstream tool's data directory name under `$HOME` (`~/.clauth`).
pub(crate) const UPSTREAM_DATA_DIR_NAME: &str = ".clauth";

/// The upstream tool's herdr plugin id.
#[allow(
    dead_code,
    reason = "read by the coexistence / import work (plan §4.0)"
)]
pub(crate) const UPSTREAM_HERDR_PLUGIN_ID: &str = "clauth";

/// The upstream tool's Claude Code plugin key.
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
