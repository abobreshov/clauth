//! The agentgear [`PluginHost`] derive plus the four lifecycle wrappers tollgate
//! calls: the Plugin tab's one-key install, the SessionStart self-heal hook, the
//! `tollgate start` pre-flight, and the throttled detached heal `tollgate mcp` and
//! the daemon share. The hook cannot be the migration trigger — a marketplace
//! that fails to load means the plugin never loads, so the hook never fires —
//! which is why the pre-flight and the detached heal both key off the same gate.
//! Beside the heal runs the installPath convergence leg: CC records plugin
//! installPaths through the installing session's runtime tree, which dies with
//! the session, so every boundary (pre-flight, detached heal, `self-heal` after
//! a binary install) re-roots the dead ones to their `~/.claude` twins through
//! agentgear's byte-surgical re-point.
//!
//! Guest mode (upstream clauth installed, plan §4.0): the registry and
//! `settings.json` are shared with upstream, but `tollgate@tollgate`'s rows in
//! them are tollgate's own, so the install and the heals still run — only into
//! `~/.claude` or a tollgate runtime, never an upstream session's config dir,
//! and each under `guest_write::guest_guarded`, which holds upstream's lock and
//! undoes any change the `claude plugin` child made to another tool's keys.
//! The `self-heal` hook stands down in a session that is not tollgate's
//! (`identity::hook_stands_down`), since upstream's sessions load the plugin
//! too.
//!
//! tollgate's plugin tree lives in `plugins/` (not the default `plugin/`), so the
//! derive's `tree` attr and `build.rs`'s `assert_plugin_version_at` both name
//! it. The tree itself stays a stock Claude Code plugin — `plugin.json` + the
//! `hooks/` dir — and agentgear supplies the lifecycle around it: materialize
//! the tree, drive `claude plugin marketplace add` + `plugin install`, verify
//! through `plugin list --json`, and stamp a marker self-heal keys on.
//!
//! The `claude`-shelling paths here are the ONLY lifecycle call sites;
//! nothing else in the crate shells out to `claude plugin` (the Plugin tab's
//! probe reads the registry files directly, and the manual `mcpServers` fallback
//! is a settings write). The lifecycle is pinned hermetically by the
//! fake-`claude` tests in `tests/inline/tui_app.rs` and the self-heal pin in
//! `tests/inline/plugin_host.rs` — both `#[cfg(unix)]` (the fake CLI is a
//! shell shim), so a Windows CI leg does not run them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agentgear::{Outcome, PluginHost, Scope, Source};

/// The plugin host for the committed `plugins/` tree. Claude-only: the default
/// `agents` list already names just `claude`, so no agent feature flags beyond
/// the crate defaults (derive + claude + embed) are enabled.
#[derive(PluginHost)]
#[plugin(name = "tollgate", tree = "$CARGO_MANIFEST_DIR/plugins")]
pub(crate) struct TollgatePlugin;

/// The Plugin tab's one-key install: a user-scope install from the embedded
/// tree. The single spelling site — the tab's confirm handler and its pin test
/// both go through here, so `Scope::User` + `Source::Embedded` live in one
/// place and the copy-paste hint they replace has no other home to drift into.
pub(crate) fn install() -> anyhow::Result<Outcome> {
    // Guest mode: the registry and `settings.json` are shared with upstream
    // clauth. tollgate's own `tollgate@tollgate` rows are additive, so the
    // install runs under `guest_write`'s guard, which holds upstream's lock and
    // puts back any upstream key the `claude plugin` child touched. Only
    // `~/.claude` is a target: under a `CLAUDE_CONFIG_DIR` the child would write
    // that session's config instead — a guest session's private plugin copy,
    // gone at teardown, or an upstream session's runtime.
    if crate::identity::upstream_active()
        && let crate::identity::PluginTarget::OwnRuntime(dir)
        | crate::identity::PluginTarget::Foreign(dir) = crate::identity::plugin_target()
    {
        anyhow::bail!(
            "guest mode: CLAUDE_CONFIG_DIR is {}, so the plugin would land in that session's config, not ~/.claude; run the install from a shell outside Claude Code",
            dir.display()
        );
    }
    let report = guarded(|| {
        Ok(TollgatePlugin::install_report(
            Scope::User,
            Source::Embedded,
        )?)
    })?;
    merged(
        &report,
        Some("claude is not on PATH; nothing was installed"),
    )
}

/// `claude` is not on `PATH`, so the plugin step never ran: the error
/// `install` / `uninstall` raise instead of agentgear's silent `NoOp`.
#[derive(Debug)]
pub(crate) struct ClaudeAbsent(pub(crate) &'static str);

impl std::fmt::Display for ClaudeAbsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ClaudeAbsent {}

/// Collapse an agentgear report the way its merged lifecycle methods do (a
/// failed agent is an error, else the first real change wins), except that
/// with `absent` set a `claude` agent skipped as not detected is that error
/// instead of a silent `NoOp`: "no changes needed" would claim an install
/// that never ran.
fn merged(
    report: &agentgear::AgentReport,
    absent: Option<&'static str>,
) -> anyhow::Result<Outcome> {
    use agentgear::{AgentStatus, SkipReason};
    let mut out = Outcome::NoOp;
    for result in &report.results {
        match &result.status {
            AgentStatus::Failed(detail) => {
                anyhow::bail!("{}: {detail}", result.agent);
            }
            AgentStatus::Skipped(SkipReason::NotDetected) if result.agent == "claude" => {
                if let Some(why) = absent {
                    return Err(anyhow::Error::new(ClaudeAbsent(why)));
                }
            }
            AgentStatus::Converged(o) if out == Outcome::NoOp => out = o.clone(),
            _ => {}
        }
    }
    Ok(out)
}

/// What `tollgate plugin uninstall` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Uninstalled {
    /// agentgear's outcome for `tollgate@tollgate` (`Removed`, or `NoOp`
    /// when it was not installed).
    pub(crate) plugin: Outcome,
    /// Whether `mcpServers.tollgate` was removed from `~/.claude.json`.
    pub(crate) mcp_entry: bool,
}

/// `tollgate plugin uninstall`: remove `tollgate@tollgate` (agentgear's
/// uninstall: `claude plugin uninstall` + the refcount-gated marketplace
/// remove) and the manual `mcpServers.tollgate` wiring, and nothing else.
///
/// Both halves run under `guest_write`'s owned-keys guard in EVERY mode, not
/// only guest mode: the `claude plugin` child is snapshot-and-restored so a
/// foreign key it changes (upstream clauth's `clauth@clauth` rows above all)
/// is put back, and the `~/.claude.json` edit is refused if it would change
/// anything but tollgate's entry. The import's retire and rollback flows
/// and an owner undoing an install all need exactly that and no more. Only
/// `~/.claude` is a target: under a `CLAUDE_CONFIG_DIR` the child would edit
/// that session's config instead.
pub(crate) fn uninstall() -> anyhow::Result<Uninstalled> {
    if let crate::identity::PluginTarget::OwnRuntime(dir)
    | crate::identity::PluginTarget::Foreign(dir) = crate::identity::plugin_target()
    {
        anyhow::bail!(
            "CLAUDE_CONFIG_DIR is {}, so the uninstall would edit that session's config, not ~/.claude; run it from a shell outside Claude Code",
            dir.display()
        );
    }
    let report = uninstall_plugin_report()?;
    let plugin = merged(
        &report,
        Some(
            "claude is not on PATH, so tollgate@tollgate could not be uninstalled; nothing was changed",
        ),
    )?;
    let mcp_entry = crate::plugin_probe::unwire_mcp_server()?;
    Ok(Uninstalled { plugin, mcp_entry })
}

/// The plugin half of [`uninstall`] alone: `tollgate@tollgate` and its
/// marketplace, under the owned-keys guard, `mcpServers` left as it is. The
/// undo of the import's retire step R2, which installed only the plugin.
pub(crate) fn uninstall_plugin() -> anyhow::Result<Outcome> {
    merged(&uninstall_plugin_report()?, None)
}

fn uninstall_plugin_report() -> anyhow::Result<agentgear::AgentReport> {
    crate::guest_write::owned_keys_guarded(&guarded_files(), || {
        Ok(TollgatePlugin::uninstall_report(Scope::User)?)
    })
}

/// The one line `tollgate plugin uninstall` prints.
pub(crate) fn uninstall_line(u: &Uninstalled) -> String {
    let plugin = match u.plugin {
        Outcome::NoOp => "tollgate@tollgate was not installed".to_string(),
        ref other => format!("tollgate@tollgate {other}"),
    };
    let mcp = if u.mcp_entry {
        "mcpServers.tollgate removed from ~/.claude.json"
    } else {
        "no mcpServers.tollgate in ~/.claude.json"
    };
    format!("tollgate plugin uninstall: {plugin}; {mcp}")
}

/// The shared registry files a `claude plugin` child writes, each with the
/// keys tollgate owns in it: the config dir the child resolves (see
/// [`registry_dir`]), `settings.json` and the two `plugins/` registries.
fn guarded_files() -> Vec<(PathBuf, &'static [crate::guest_write::OwnedKey])> {
    use crate::guest_write::{INSTALLED_PLUGINS_KEYS, KNOWN_MARKETPLACES_KEYS, SETTINGS_KEYS};
    let Some(dir) = registry_dir() else {
        return Vec::new();
    };
    vec![
        (dir.join("settings.json"), SETTINGS_KEYS),
        (
            dir.join("plugins").join("installed_plugins.json"),
            INSTALLED_PLUGINS_KEYS,
        ),
        (
            dir.join("plugins").join("known_marketplaces.json"),
            KNOWN_MARKETPLACES_KEYS,
        ),
    ]
}

/// Run a lifecycle leg that writes the shared registry: `run()` outside guest
/// mode, and under `guest_write::guest_guarded` in it.
fn guarded<T>(run: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    crate::guest_write::guest_guarded(&guarded_files(), run)
}

/// Guest mode's gate for the heal legs: `false` when the config dir a heal
/// would write belongs to upstream (an upstream session's runtime, where
/// tollgate's MCP server or hooks may be loaded). `~/.claude` heals under the
/// guard; a tollgate runtime's private copy is tollgate's own.
fn heal_allowed() -> bool {
    !crate::identity::upstream_active()
        || !matches!(
            crate::identity::plugin_target(),
            crate::identity::PluginTarget::Foreign(_)
        )
}

/// The SessionStart hook body (`tollgate self-heal`). Repairs a broken
/// registration, never resurrects an uninstall — agentgear's marker gate makes
/// a deliberately removed plugin stay removed. A healthy session prints
/// nothing, so a hook that fires on every session start injects no noise into
/// the conversation; a repair (or a failure) is worth saying out loud. The
/// installPath convergence leg runs beside the heal: a CC install recorded
/// through a dead runtime tree dangles even when tollgate's own registration is
/// healthy, so neither leg gates the other.
pub(crate) fn self_heal() -> anyhow::Result<()> {
    // Guest mode: the plugin is registered in the shared `~/.claude`, so this
    // hook also fires in upstream clauth's sessions; there it does nothing.
    if crate::identity::hook_stands_down() {
        return Ok(());
    }
    heal_now()
}

/// The heal body the hook and the start pre-flight share: the registration
/// heal, then the installPath convergence leg, each line printed.
fn heal_now() -> anyhow::Result<()> {
    if !heal_allowed() {
        return Ok(());
    }
    if let Some(line) = self_heal_line()? {
        crate::out::outln!("{line}");
    }
    if let Some(line) = repoint_registry()?.line {
        crate::out::outln!("{line}");
    }
    Ok(())
}

/// What one registry convergence pass reports. `line` is `Some` when the pass
/// rewrote or named anything; `changed` is true only for rewrites — a named
/// skip moves no bytes, so it reports without counting as a change.
#[derive(Debug)]
pub(crate) struct RepointOutcome {
    pub(crate) line: Option<String>,
    pub(crate) changed: bool,
}

/// The installPath convergence leg. CC records every plugin install's
/// `installPath` through the installing session's runtime tree (the tree's
/// `plugins/` symlinks onto the shared `~/.claude/plugins`, so the files
/// survive while the recorded path dies with the tree). No `claude plugin`
/// command re-spells a recorded path, so this rewrites the dead ones to their
/// `~/.claude` twins through agentgear's byte-surgical re-point: only the
/// mapped values change, the file is never reformatted, a concurrent change
/// restarts the pass, and a deleted registry is refused. A path that still
/// resolves (its tree lives) is left alone — the next pass after the tree
/// dies converges it. Rewrites and skips both name themselves in the line;
/// `changed` separates the two for call sites that rate-limit reporting.
pub(crate) fn repoint_registry() -> anyhow::Result<RepointOutcome> {
    // Guest mode: `installed_plugins.json` is shared with upstream clauth, so
    // only tollgate's own rows are re-pointed (see `registry_remap`), under the
    // guard, and never in an upstream session's config dir.
    if !heal_allowed() {
        return Ok(RepointOutcome {
            line: None,
            changed: false,
        });
    }
    let Ok(tollgate) = crate::profile::tollgate_dir() else {
        return Ok(RepointOutcome {
            line: None,
            changed: false,
        });
    };
    let Ok(claude) = crate::profile::claude_dir() else {
        return Ok(RepointOutcome {
            line: None,
            changed: false,
        });
    };
    let registry = claude.join("plugins").join("installed_plugins.json");
    // Hoisted: the two prefix spellings are one computation, not one per
    // quoted value the remap is asked about.
    let profiles = tollgate.join("profiles");
    let prefix_fwd = format!("{}/", profiles.display());
    let prefix_back = format!("{}\\", profiles.display());
    let guest = crate::identity::upstream_active();
    // This leg always writes `~/.claude`'s registry, whatever
    // `CLAUDE_CONFIG_DIR` says, so it guards that file rather than
    // `guarded_files()`.
    let files = [(registry.clone(), crate::guest_write::INSTALLED_PLUGINS_KEYS)];
    let report = crate::guest_write::guest_guarded(&files, || {
        Ok(agentgear::repoint_install_paths(
            &registry,
            |path: &str| registry_remap(path, &prefix_fwd, &prefix_back, &claude, guest),
        )?)
    })?;
    let changed = report.changed();
    if report.rewritten.is_empty() && report.skipped.is_empty() {
        return Ok(RepointOutcome {
            line: None,
            changed,
        });
    }
    let mut parts = Vec::new();
    for r in &report.rewritten {
        parts.push(format!("re-pointed {} -> {}", r.from, r.to));
    }
    for s in &report.skipped {
        parts.push(format!("left {} ({})", s.path, s.reason));
    }
    Ok(RepointOutcome {
        line: Some(format!("tollgate self-heal: {}", parts.join("; "))),
        changed,
    })
}

/// The remap decision for one recorded path. Both separator spellings match:
/// CC records `\`-spelled paths on windows and `/` on posix, and the leg must
/// converge either — a windows box runs the same dangling-path shape through
/// its symlink tree.
fn registry_remap(
    path: &str,
    prefix_fwd: &str,
    prefix_back: &str,
    claude: &Path,
    guest: bool,
) -> agentgear::Remap {
    if !path.starts_with(prefix_fwd) && !path.starts_with(prefix_back) {
        return agentgear::Remap::Keep;
    }
    // Guest mode: another tool's plugin row is upstream's to converge, even
    // one recorded through a tollgate runtime. tollgate's own rows live in its
    // marketplace's cache dir (`cache/tollgate/…`).
    if guest && !is_own_cache_path(path) {
        return agentgear::Remap::Keep;
    }
    if Path::new(path).exists() {
        // A live tree still resolves the path today; leave it, it converges
        // the day the tree dies.
        return agentgear::Remap::Keep;
    }
    let Some(suffix) = path
        .split_once("plugins/")
        .map(|(_, s)| s)
        .or_else(|| path.split_once("plugins\\").map(|(_, s)| s))
    else {
        return agentgear::Remap::Skip("no plugins/ segment".to_string());
    };
    // Component-wise: a one-string join carrying the forward-slash suffix
    // renders mixed separators on windows (`\plugins\cache/a/b`), which
    // resolves but is not the native spelling CC records — joining per
    // component yields the canonical per-platform path.
    let twin = suffix
        .split(['/', '\\'])
        .fold(claude.join("plugins"), |path, component| {
            path.join(component)
        });
    if twin.exists() {
        // The spelling lands verbatim; safe because CC derives its cache dirs
        // from marketplace/plugin/version slugs, never from user input, so no
        // quote or backslash escape can occur in the joined path.
        agentgear::Remap::Rewrite(twin.display().to_string())
    } else {
        agentgear::Remap::Skip(format!("no twin at {}", twin.display()))
    }
}

/// Whether a recorded installPath sits in tollgate's own marketplace cache
/// (`…/plugins/cache/tollgate/…`), either separator.
fn is_own_cache_path(path: &str) -> bool {
    let fwd = format!("plugins/cache/{}/", crate::identity::NAME);
    let back = format!("plugins\\cache\\{}\\", crate::identity::NAME);
    path.contains(&fwd) || path.contains(&back)
}

/// Skip-only reports are named once per process in the detached leg: the
/// daemon tick would otherwise repeat the same "left X (no twin)" line every
/// tick until a re-login lands the twin. Rewrites always report — each one
/// can only happen once.
static REPORTED_SKIPS: AtomicBool = AtomicBool::new(false);

/// The detached leg's repoint slice: rewrites pass through, a skip-only line
/// passes once per process. Split from [`heal_detached`] so a test can pin
/// the rate limit without a terminal.
fn detached_repoint_line() -> anyhow::Result<Option<String>> {
    let outcome = repoint_registry()?;
    let Some(line) = outcome.line else {
        return Ok(None);
    };
    if outcome.changed || !REPORTED_SKIPS.swap(true, Ordering::Relaxed) {
        Ok(Some(line))
    } else {
        Ok(None)
    }
}

/// Clear the skip-report flag so a test can drive the detached leg fresh. The
/// statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`.
#[cfg(test)]
pub(crate) fn reset_skip_report_for_test() {
    assert!(
        crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
        "skip-report statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
    );
    REPORTED_SKIPS.store(false, Ordering::Relaxed);
}

/// What the hook says, or `None` when there is nothing to say: the outcome
/// becomes a line only when the heal changed something. Split from
/// [`self_heal`] so a test can pin the contract without a terminal.
pub(crate) fn self_heal_line() -> anyhow::Result<Option<String>> {
    if !heal_allowed() {
        return Ok(None);
    }
    let outcome = guarded(|| Ok(TollgatePlugin::self_heal()?))?;
    Ok((!matches!(outcome, Outcome::NoOp)).then(|| format!("tollgate self-heal: {outcome}")))
}

/// The `tollgate start` pre-flight: the migration trigger that heals a broken or
/// divergent tollgate marketplace registration before `claude` launches. The hook
/// self-heal cannot be this trigger — a marketplace that fails to load means the
/// plugin never loads, so the hook never fires.
///
/// The gate below is two plain registry reads; a healthy registration spawns
/// nothing. A heal failure is logged and never fails the start: the session
/// still launches, and the hook (once the plugin loads again) keeps trying.
pub(crate) fn preflight() {
    // Guest mode: the heal runs under the guard against `~/.clauth`'s lock,
    // and never into an upstream session's config dir.
    if !heal_allowed() {
        return;
    }
    match repoint_registry() {
        Ok(outcome) => {
            if let Some(line) = outcome.line {
                crate::out::outln!("{line}");
            }
        }
        Err(e) => crate::logline::logline!("tollgate: plugin path re-point failed: {e:#}"),
    }
    if !preflight_gate() {
        return;
    }
    if let Err(e) = heal_now() {
        crate::logline::logline!("tollgate: plugin pre-flight heal failed: {e:#}");
    }
}

/// How long the floor between attempts holds. See [`HealThrottle`].
const HEAL_THROTTLE_MS: u64 = 30 * 60 * 1000;

pub(crate) static HEAL_THROTTLE: HealThrottle = HealThrottle::new();

/// One heal's attempt limiter: at most one attempt per process every
/// [`HEAL_THROTTLE_MS`], success or failure, and never two in flight. Both
/// detached callers (`tollgate mcp` boot, daemon tick) can fire once per second
/// on a box whose `claude` is missing, and an untried retry every tick would
/// spawn + fail forever. The in-flight flag bounds overlap; the floor bounds
/// frequency from the attempt STARTING (not finishing), so a slow heal still
/// cannot stack a second one behind it. The claude heal and the herdr heal
/// each hold their own instance, so one healing never defers the other.
/// Fields are `pub(crate)` for the inline test's direct pinning; every writer
/// outside a test goes through [`HealThrottle::claim`].
pub(crate) struct HealThrottle {
    pub(crate) in_flight: AtomicBool,
    pub(crate) last_start_ms: AtomicU64,
}

/// A claimed in-flight flag. The caller builds it on the claiming thread and
/// moves it into the worker, so an early return, a panic, or the worker
/// finishing all clear the flag rather than wedging the throttle shut for the
/// rest of the process.
pub(crate) struct HealClaim<'a>(&'a AtomicBool);

impl Drop for HealClaim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl HealThrottle {
    pub(crate) const fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            last_start_ms: AtomicU64::new(0),
        }
    }

    /// Claim an attempt at `now_ms`: `None` when one is in flight or the last
    /// attempt started within the floor window. The floor stamps here, on the
    /// claiming thread, so the window between claim and spawn is covered too.
    pub(crate) fn claim(&self, now_ms: u64) -> Option<HealClaim<'_>> {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return None;
        }
        let claim = HealClaim(&self.in_flight);
        if now_ms.saturating_sub(self.last_start_ms.load(Ordering::Relaxed)) < HEAL_THROTTLE_MS {
            return None;
        }
        self.last_start_ms.store(now_ms, Ordering::Relaxed);
        Some(claim)
    }

    /// Clear both flags so a test can drive a heal fresh. The statics serialize
    /// on `HOME_TEST_LOCK`; call under a `HomeSandbox`.
    #[cfg(all(test, unix))]
    pub(crate) fn reset_for_test(&self) {
        assert!(
            crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
            "heal throttle statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
        );
        self.in_flight.store(false, Ordering::Release);
        self.last_start_ms.store(0, Ordering::Relaxed);
    }

    /// Stamp the floor at now, so every attempt is refused for the next
    /// window. For a test that drives a caller of the heal (the daemon tick)
    /// and is about something else. The statics serialize on `HOME_TEST_LOCK`;
    /// call under a `HomeSandbox`.
    #[cfg(test)]
    pub(crate) fn arm_for_test(&self) {
        assert!(
            crate::lockorder::holds::<crate::lockorder::rank::HomeTest>(),
            "heal throttle statics serialize on `HOME_TEST_LOCK`; call under a `HomeSandbox`"
        );
        self.in_flight.store(false, Ordering::Release);
        self.last_start_ms
            .store(crate::usage::now_ms(), Ordering::Relaxed);
    }
}

/// The shared detached heal: `tollgate mcp` runs it before its stdio handshake and
/// the daemon once per tick. The gate runs INLINE (two registry reads, no
/// spawn); only a "heal" verdict spawns anything, and then on its own thread so
/// neither caller is ever blocked by a `claude plugin` spawn. The throttle lives
/// here, not at either call site, so a call site cannot forget it.
///
/// The outcome and any failure go through [`logline!`] — stderr for both callers
/// — never `out::outln!`: `tollgate mcp`'s stdout is a JSON-RPC stream, and one
/// stray line corrupts the session.
pub(crate) fn heal_detached() {
    // Guest mode: never into an upstream session's config dir (`tollgate mcp`
    // loaded there by the shared plugin); elsewhere the legs run guarded.
    if !heal_allowed() {
        return;
    }
    match detached_repoint_line() {
        Ok(Some(line)) => crate::logline::logline!("{line}"),
        Ok(None) => {}
        Err(e) => crate::logline::logline!("tollgate: plugin path re-point failed: {e:#}"),
    }
    if !preflight_gate() {
        return;
    }
    let Some(claim) = HEAL_THROTTLE.claim(crate::usage::now_ms()) else {
        return;
    };
    // Fail closed in test builds. The worker shells out to `claude` and lets
    // agentgear write its marker tree, and BOTH resolve off the process
    // environment: `PATH`, `HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`. Only
    // `FakeClaude` pins those, which is why the predicate is its sentinel rather
    // than tollgate's own home override — that override is real and is still not
    // the thing making this hermetic.
    #[cfg(test)]
    assert!(
        std::env::var_os("CLAUDE_SHIM_STATE").is_some(),
        "heal_detached would spawn the operator's real `claude` and write their \
         real agentgear tree — stage a `FakeClaude` beside the `HomeSandbox`, or \
         call `arm_heal_throttle_for_test` if the heal is not what the test is about"
    );
    // Registered on THIS thread, before the spawn, the way the MCP background
    // delegate does it: `join_background_tasks` drains whatever is registered at
    // the moment it runs, so registering inside the worker lets a sandbox
    // teardown clear the home override with a heal still running — which then
    // resolves the operator's REAL `$HOME` and takes real locks under
    // `~/.tollgate`.
    #[cfg(test)]
    let done = crate::testutil::register_background_task();
    std::thread::spawn(move || {
        let _claim = claim;
        match self_heal_line() {
            Ok(Some(line)) => crate::logline::logline!("{line}"),
            Ok(None) => {}
            Err(e) => crate::logline::logline!("tollgate: plugin heal failed: {e:#}"),
        }
        // Last action, after every `$HOME`-touching step: `HealInFlight` touches
        // one atomic and nothing else, so its later drop is safe past the send.
        #[cfg(test)]
        let _ = done.send(());
    });
}

/// `unix` because every caller is a `#[cfg(unix)]` test: the heal's fake
/// `claude` is a shell shim. A bare `cfg(test)` gate is dead code on Windows,
/// which `-D warnings` reds there and nowhere else.
#[cfg(all(test, unix))]
pub(crate) fn reset_heal_throttle_for_test() {
    HEAL_THROTTLE.reset_for_test();
}

/// Stamp the floor at now, so [`heal_detached`] refuses every attempt for the
/// next window. For a test that drives a caller of the heal (the daemon tick)
/// and is about something else: the gate's `expected_pointer` reads
/// `dirs::data_dir()`, which no sandbox can pin on Windows, so a seeded-healthy
/// registry is a unix-only way to keep such a test spawn-free. Arming the floor
/// is the cross-platform one. Serialized with every other user by `HomeSandbox`'s
/// own lock.
#[cfg(test)]
pub(crate) fn arm_heal_throttle_for_test() {
    HEAL_THROTTLE.arm_for_test();
}

/// Whether the pre-flight should run the heal: `true` for every registry shape a
/// user-scope heal converges, `false` for a registration already sitting at
/// agentgear's materialized `current@claude` pointer with its generated manifest
/// present and every user-scope plugin entry's files resolvable, and `false` for
/// a box that holds nothing of ours in either registry. Read-only; a file this
/// cannot read counts as "heal" (conservative — the heal is idempotent, so a
/// needless run costs nothing but its own reads).
pub(crate) fn preflight_gate() -> bool {
    let Some(dir) = registry_dir() else {
        return true;
    };
    let Some(expected) = expected_pointer() else {
        return true;
    };
    let marketplaces = read_registry(&dir.join("plugins").join("known_marketplaces.json"));
    let installed = read_registry(&dir.join("plugins").join("installed_plugins.json"));

    // The "never installed" box. A false positive here makes every `tollgate mcp`
    // boot and daemon tick spawn `claude plugin list --json` for nothing, and
    // agentgear can never install from a heal anyway, so it converges nothing.
    // ABSENT is the load-bearing half: a config dir that has never held a plugin
    // has no `plugins/` directory at all, so keying this on "parses, names
    // nothing" alone would miss exactly the population it is written for.
    // Unreadable is the other verdict, and it still heals.
    let marketplaces_empty = match &marketplaces {
        Registry::Missing => true,
        Registry::Unreadable => false,
        Registry::Parsed(doc) => doc.get(crate::identity::NAME).is_none(),
    };
    let installed_empty = match &installed {
        Registry::Missing => true,
        Registry::Unreadable => false,
        // Absence of the KEY, never "not an array": a foreign or corrupt value
        // under it is still something of ours registered, and the old gate healed
        // on it. Matches the marketplace conjunct above.
        Registry::Parsed(doc) => doc["plugins"][crate::identity::CC_PLUGIN].is_null(),
    };
    if marketplaces_empty && installed_empty {
        return false;
    }

    marketplace_needs_heal(marketplaces.doc(), &expected)
        || plugin_entries_need_heal(installed.doc())
}

/// The materialized pointer agentgear's `materialize` publishes: its locked
/// layout is `<data_dir>/<plugin>/current@<client>` (agentgear design
/// §materialize), which the lifecycle's own re-point logic compares against
/// too. Derived here rather than called because tollgate builds against the
/// published agentgear crate, and the layout is the contract either way.
pub(crate) fn expected_pointer() -> Option<PathBuf> {
    dirs::data_dir().map(|base| base.join(crate::identity::NAME).join("current@claude"))
}

/// The config dir the heal itself operates on. agentgear's CLI wrapper keeps
/// `CLAUDE_CONFIG_DIR` in the child env, so the gate must read the same dir CC
/// resolves: the non-empty override when one is set, else `~/.claude`. An empty
/// override is "cannot tell" — the heal's own guard refuses it with a named
/// error, which is the report a start should surface rather than a silent skip.
fn registry_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        Some(_) => None,
        None => crate::profile::claude_dir().ok(),
    }
}

/// What one registry-file read answers. `Missing` is a positive fact — nothing
/// was ever registered there — where `Unreadable` (a permission error, a
/// truncated write, a half-parsed file) says only that this pass cannot tell.
/// Collapsing the two is what made the gate heal on every never-installed box.
enum Registry {
    Missing,
    Unreadable,
    Parsed(serde_json::Value),
}

impl Registry {
    /// The parsed document, or `None` for a file this pass could not read. The
    /// two need-heal predicates take that shape because both treat "cannot
    /// read" and "absent" alike; only the never-installed check separates them.
    fn doc(&self) -> Option<&serde_json::Value> {
        match self {
            Registry::Parsed(doc) => Some(doc),
            Registry::Missing | Registry::Unreadable => None,
        }
    }
}

fn read_registry(path: &Path) -> Registry {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Registry::Missing,
        Err(_) => return Registry::Unreadable,
    };
    serde_json::from_slice(&bytes).map_or(Registry::Unreadable, Registry::Parsed)
}

/// The marketplace half of the gate: the `tollgate` entry must be a directory
/// source registered exactly at the materialized pointer, with its generated
/// manifest present. Absent, github-sourced, diverged, or manifest-deleted all
/// heal — that is the deadlock the migration exists to break (a github entry's
/// next catalog refresh pulls a tree without the manifest and the plugin loads 0
/// hooks).
fn marketplace_needs_heal(doc: Option<&serde_json::Value>, expected: &Path) -> bool {
    let Some(doc) = doc else {
        return true;
    };
    let Some(entry) = doc.get(crate::identity::NAME) else {
        return true;
    };
    if entry["source"]["source"].as_str() != Some("directory") {
        return true;
    }
    let Some(path) = entry["source"]["path"].as_str() else {
        return true;
    };
    Path::new(path) != expected
        || !Path::new(path)
            .join(".claude-plugin")
            .join("marketplace.json")
            .exists()
}

/// The plugin half of the gate: a user-scope `tollgate@tollgate` entry whose files or
/// load state are gone. A per-session config dir leaves exactly this behind when
/// its runtime tree is collected — the entry survives, its `installPath` dies —
/// and only the heal can rewrite it. Project-scope entries never decide here: the
/// heal is user-scope and cannot fix them, so counting them would churn a heal
/// every start for nothing.
fn plugin_entries_need_heal(doc: Option<&serde_json::Value>) -> bool {
    let Some(doc) = doc else {
        return true;
    };
    let Some(rows) = doc["plugins"][crate::identity::CC_PLUGIN].as_array() else {
        return false;
    };
    rows.iter()
        .filter(|row| row["scope"].as_str().unwrap_or("user") == "user")
        .any(|row| {
            !row["errors"].as_array().is_none_or(Vec::is_empty)
                || row["installPath"]
                    .as_str()
                    .is_none_or(|p| !Path::new(p).exists())
        })
}

#[cfg(test)]
#[path = "../tests/inline/plugin_host.rs"]
mod tests;
