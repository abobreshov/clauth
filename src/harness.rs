//! The harness axis: which coding CLI a profile's credentials drive.
//!
//! A profile's harness is implied by WHICH STATE FILE it lives in — claude
//! profiles in `profiles.toml` ([`crate::profile::AppState`]), codex profiles
//! in `codex-profiles.toml` ([`crate::codex_profiles::CodexState`]), Hermes
//! profiles in `hermes-profiles.toml`
//! ([`crate::hermes::profiles::HermesState`]) — never by
//! a field inside `AppState`, a dir-name suffix, or a load-order handshake.
//! File membership is what an old binary cannot misread: it never opens the
//! codex file, so it cannot drop or corrupt codex state, and `profiles.toml`
//! keeps its exact pre-codex meaning. This enum is only the in-memory answer
//! to "which file did this name come from"; converting a profile across
//! harnesses is delete + recreate, not a flag flip.

use serde::{Deserialize, Serialize};

/// The CLI a profile's stored credentials belong to. Carried on surfaces that
/// outlive one process (live-session rows) as a lowercase string, with
/// [`Harness::Claude`] the serde default so rows written before the axis
/// existed keep meaning what they meant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Harness {
    /// Claude Code — `profiles.toml`, `.credentials.json`, `CLAUDE_CONFIG_DIR`.
    #[default]
    Claude,
    /// OpenAI codex — `codex-profiles.toml`, `auth.json`, `CODEX_HOME`.
    Codex,
    /// Hermes Agent — `hermes-profiles.toml`, a whole home per profile,
    /// `HERMES_HOME` plus a redirected child `HOME` (see [`crate::hermes`]).
    Hermes,
}

impl Harness {
    /// Every harness, in the order the bare-name verbs resolve a name
    /// (claude, then codex, then hermes).
    pub(crate) const ALL: [Harness; 3] = [Harness::Claude, Harness::Codex, Harness::Hermes];

    /// The user-facing spelling, for refusals that must name which harness
    /// holds a name.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Hermes => "hermes",
        }
    }

    /// The tag's behavior — the bridge every dispatch site crosses from
    /// "which file held the name" to "what runs for it".
    pub(crate) fn engine(self) -> &'static dyn HarnessEngine {
        match self {
            Harness::Claude => &ClaudeEngine,
            Harness::Codex => &CodexEngine,
            Harness::Hermes => &HermesEngine,
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The two seams where harness-specific behavior sits behind one shape —
/// credential-install and runtime-spawn — per the codex plan's architecture
/// section. Everything else the plan touches stays an inline `match harness`
/// at its own site (lower claude-regression risk); only these two multiply
/// across every spawn and switch path, which is what earns them a trait.
///
/// Phase 2 puts claude behind the seams verbatim — every method delegates to
/// the fn that held the behavior before, and the callers bind through the
/// trait so the codex engine (arriving with the codex runtime) plugs into
/// call sites that already dispatch.
pub(crate) trait HarnessEngine {
    // ── credential-install ──
    /// Switch-time install of `name`'s stored credentials into this harness's
    /// live slot, with the refuse-guard the non-force path carries. Claude:
    /// the `.credentials.json` link machinery plus the macOS Keychain mirror.
    fn install_credentials(&self, name: &str) -> anyhow::Result<()>;
    /// The force flavor — same install, divergence guard bypassed.
    fn force_install_credentials(&self, name: &str) -> anyhow::Result<()>;

    // ── runtime-spawn ──
    /// The resolved CLI command (Windows shim resolution included).
    fn command(&self) -> std::process::Command;
    /// The env var that pins a spawned session to its tollgate-built home.
    fn home_env_key(&self) -> &'static str;
    /// Drop from `command`'s inherited env every key that must reach the
    /// session only through its own home — this harness's managed set plus
    /// the active profile's custom keys.
    fn scrub_env(&self, command: &mut std::process::Command, active_env_keys: &[String]);
}

/// Claude Code behind the seams: pure delegation, no behavior of its own.
pub(crate) struct ClaudeEngine;

impl HarnessEngine for ClaudeEngine {
    fn install_credentials(&self, name: &str) -> anyhow::Result<()> {
        crate::claude::link_profile_credentials(&crate::profile::ProfileName::from(name))
    }

    fn force_install_credentials(&self, name: &str) -> anyhow::Result<()> {
        crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from(name))
    }

    fn command(&self) -> std::process::Command {
        crate::runtime::claude_command()
    }

    fn home_env_key(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn scrub_env(&self, command: &mut std::process::Command, active_env_keys: &[String]) {
        crate::runtime::scrub_profile_env(command, active_env_keys);
        // The CODEX_HOME half is the cross-harness one; the caller pins
        // CLAUDE_CONFIG_DIR itself right after.
        crate::runtime::scrub_tollgate_homes(command);
        // Monitoring credentials (an OpenRouter management key) never reach
        // a session: only the daemon's usage leg reads them.
        crate::providers::billing_key::scrub_billing_env(command);
    }
}

/// codex behind the seams. The spawn half delegates to the codex runtime; the
/// install half REFUSES by contract — a codex switch is a state slot and
/// sessions bind `auth.json` at start, so no flow may ask this engine to
/// install anything, and a caller that does has dispatched a claude flow onto
/// a codex tag.
pub(crate) struct CodexEngine;

/// The env keys a spawned codex session must receive only through its own home
/// and its own `auth.json`, never inherited. Three groups, each an answer to a
/// different way an inherited value silently unbinds the profile:
///
/// - the PINS. `CODEX_HOME` is the parent's, exactly the key this spawn is
///   about to set right. `CODEX_SQLITE_HOME` decides where codex resolves its
///   state DBs, so an inherited one points every profile's
///   goals/logs/memories/state at ONE directory and the home's durable links
///   are never opened through.
/// - the CREDENTIAL CARRIERS, which outrank the linked `auth.json` in codex's
///   own resolution order (`login/src/auth/manager.rs` `load_auth`, whose
///   first comment reads "API key via env var takes precedence over any other
///   auth method"): `CODEX_API_KEY` first, then `CODEX_ACCESS_TOKEN`, and only
///   then the persistent store this session was built around. `OPENAI_API_KEY`
///   is the same hazard one layer down, since `resolved_mode()` reads a bare
///   key as an auth mode of its own. Left inherited, `tollgate start <p>` spends
///   a DIFFERENT account than the one it names — the exact confusion tollgate
///   exists to prevent.
/// - the ENDPOINT OVERRIDES codex ships for its own tests. They decide where a
///   refresh POSTs, where a revoke goes, and which client id is claimed. A
///   codex refresh token is single-use, so an inherited override spends the
///   profile's one live chain against a host tollgate did not choose.
const CODEX_MANAGED_ENV_KEYS: &[&str] = &[
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
    "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
    "CODEX_APP_SERVER_LOGIN_CLIENT_ID",
];

impl HarnessEngine for CodexEngine {
    fn install_credentials(&self, name: &str) -> anyhow::Result<()> {
        anyhow::bail!(
            "codex profile '{name}' installs nothing at switch — sessions bind auth.json at start"
        )
    }

    fn force_install_credentials(&self, name: &str) -> anyhow::Result<()> {
        self.install_credentials(name)
    }

    fn command(&self) -> std::process::Command {
        crate::runtime::codex_command()
    }

    fn home_env_key(&self) -> &'static str {
        "CODEX_HOME"
    }

    fn scrub_env(&self, command: &mut std::process::Command, active_env_keys: &[String]) {
        for key in CODEX_MANAGED_ENV_KEYS {
            command.env_remove(key);
        }
        for key in active_env_keys {
            command.env_remove(key);
        }
        // The CLAUDE_CONFIG_DIR half matters here: `tollgate which` in the codex
        // session would answer as the ancestor claude session (the runtime
        // claim deliberately outranks the codex arm).
        crate::runtime::scrub_tollgate_homes(command);
        crate::providers::billing_key::scrub_billing_env(command);
    }
}

/// Hermes Agent behind the seams. Like codex, the install half REFUSES: a
/// Hermes profile is a whole home bound at launch through `HERMES_HOME`, with
/// no live slot to install into. The spawn half is the resolved entrypoint
/// (spec §4.5: re-resolved at every launch, never the Omarchy shim) and the
/// static §4.3 `SCRUB`. The launch adds the dynamic half of the scrub (the
/// provider-plugin scan) and the `HOME` redirect itself, in
/// `start::hermes_spawn_command`.
pub(crate) struct HermesEngine;

impl HarnessEngine for HermesEngine {
    fn install_credentials(&self, name: &str) -> anyhow::Result<()> {
        anyhow::bail!(
            "Hermes profile '{name}' installs nothing at switch: Hermes switches by relaunch \
             ('tollgate start {name}')"
        )
    }

    fn force_install_credentials(&self, name: &str) -> anyhow::Result<()> {
        self.install_credentials(name)
    }

    /// The resolved entrypoint. A failed resolution yields a command that
    /// cannot spawn rather than a bare `hermes` PATH lookup, which on an
    /// Omarchy machine is the self-installing shim.
    fn command(&self) -> std::process::Command {
        match crate::hermes::resolve::resolve_entrypoint(
            &crate::hermes::resolve::ResolveEnv::from_process(None),
        ) {
            Ok(install) => std::process::Command::new(install.entry),
            Err(_) => std::process::Command::new(crate::hermes::resolve::UNRESOLVED_ENTRYPOINT),
        }
    }

    fn home_env_key(&self) -> &'static str {
        "HERMES_HOME"
    }

    fn scrub_env(&self, command: &mut std::process::Command, active_env_keys: &[String]) {
        crate::hermes::guards::scrub_static(command);
        for key in active_env_keys {
            command.env_remove(key);
        }
        crate::runtime::scrub_tollgate_homes(command);
        crate::providers::billing_key::scrub_billing_env(command);
    }
}

#[cfg(test)]
#[path = "../tests/inline/harness.rs"]
mod tests;
