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

/// The import journal's file name under the data dir (`~/.tollgate`). The
/// `import clauth` transaction (plan §4.0, M0–M8) writes it; a top-level
/// `"state": "complete"` is what ends guest mode.
pub(crate) const IMPORT_JOURNAL_FILE: &str = "import-journal.json";

/// The one sentence every user-facing global mutation answers with in guest
/// mode ([`upstream_active`]).
pub(crate) const GUEST_REFUSAL: &str = concat!(
    tool_name!(),
    ": upstream clauth manages ~/.claude on this machine (guest mode). Use '",
    tool_name!(),
    " start <profile>' for a per-session account, or import clauth first (not yet available)."
);

/// Guest mode (plan §4.0, the "guest mode (pre-import)" coexistence row): the
/// upstream tool's data dir (`~/.clauth`) exists and this tool has not completed
/// an import of it. Upstream then owns every global file the two share —
/// `~/.claude/.credentials.json`, `~/.claude/settings.json`, `~/.claude.json`,
/// `~/.codex/auth.json`, the Claude Code plugin registry, the herdr config — so
/// this tool writes none of them: user-facing global mutations refuse with
/// [`GUEST_REFUSAL`], background legs skip their global writes, and per-session
/// runtimes under `~/.tollgate` keep working.
///
/// Resolved through [`crate::profile::home_dir`], so a test's
/// `testutil::HomeSandbox` decides it. In test builds a caller with no sandbox
/// alive answers `false` rather than panicking there: the existing suite calls
/// gated functions on scratch paths with no home at all, and "no upstream
/// install" is what every one of them assumes.
pub(crate) fn upstream_active() -> bool {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return false;
    }
    let Ok(home) = crate::profile::home_dir() else {
        return false;
    };
    if !home.join(UPSTREAM_DATA_DIR_NAME).exists() {
        return false;
    }
    !import_completed(&home.join(DATA_DIR_NAME).join(IMPORT_JOURNAL_FILE))
}

/// Whether the import journal at `path` records a completed import: a JSON
/// object whose top-level `state` is `"complete"`. A missing, unreadable or
/// unparseable journal, and any other state (a `pre` phase, a rolled-back
/// import), is not one — guest mode stays on.
fn import_completed(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.get("state")
                .and_then(serde_json::Value::as_str)
                .map(|s| s == "complete")
        })
        .unwrap_or(false)
}

/// The refusal a user-facing global mutation raises in guest mode. Its own
/// type so the CLI prints the sentence as-is (`main::exit_code`) and a remote
/// surface can reflect it as a refusal rather than a failure.
#[derive(Debug)]
pub(crate) struct GuestRefusal;

impl std::fmt::Display for GuestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(GUEST_REFUSAL)
    }
}

impl std::error::Error for GuestRefusal {}

/// `Err(GuestRefusal)` in guest mode, `Ok` otherwise: the one-line gate a
/// user-facing global mutation opens with.
pub(crate) fn refuse_in_guest_mode() -> anyhow::Result<()> {
    if upstream_active() {
        return Err(GuestRefusal.into());
    }
    Ok(())
}

/// Whether guest mode refuses a mutation (write or delete) of the macOS
/// Keychain item at `service`. Only the DEFAULT `Claude Code-credentials`
/// item: that is the one a global `claude` reads, so upstream clauth owns it
/// exactly as it owns `~/.claude/.credentials.json`. A per-session namespaced
/// item (`Claude Code-credentials-<hash>`) belongs to a runtime this tool
/// created and stays writable. `keychain.rs` asks this at its two lowest
/// writers, so every default-item mutation — a switch install, a rotation
/// mirror, a rolling re-stamp, a sign-out — passes the gate. Pure so it is
/// pinned on every platform; the module that calls it compiles on macOS only.
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the only production caller is the macOS Keychain layer; the gate is pinned on every platform"
    )
)]
pub(crate) fn guest_refuses_keychain_service(service: &str) -> bool {
    service == crate::claude::CLAUDE_KEYCHAIN_SERVICE && upstream_active()
}

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

#[cfg(test)]
#[path = "../tests/inline/guest_mode.rs"]
mod guest_mode_tests;
