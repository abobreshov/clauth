//! Monitoring ("billing") credentials held by env-var REFERENCE only (plan
//! v3.1 §4.3, the Monitoring credential slot).
//!
//! A profile opts in with one top-level key in its `config.toml`:
//!
//! ```toml
//! billing_key_env = "OPENROUTER_MGMT_KEY"
//! ```
//!
//! Only the variable's NAME is ever stored. Its value (an OpenRouter
//! management key today) is read from the process env at fetch time by
//! [`resolve`], used for the one read that needs it (`GET /api/v1/credits`),
//! and dropped. It is never written to disk, never printed, and never handed
//! to a child: [`scrub_billing_env`] strips every referenced variable from each
//! session spawn (`HarnessEngine::scrub_env`, which `tollgate start` and the
//! MCP delegate both run). The daemon itself is not scrubbed, because it is the
//! process that reads the key; it must be started from an env that holds it.
//!
//! `ProfileConfig` does not model the key, so it is read here straight off the
//! file. `config.toml`'s canonical rewrite carries unmodelled top-level keys
//! across (`profile::preserving_config_render`), so it survives every save.

use crate::profile::ProfileName;

/// The `config.toml` key naming the env var.
pub(crate) const CONFIG_KEY: &str = "billing_key_env";

/// A portable env-var name: `[A-Za-z_][A-Za-z0-9_]*`, at most 128 chars. Anything
/// else is ignored, so a value pasted into the name slot (a key itself) is
/// never looked up, echoed or scrubbed by content.
pub(crate) fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 128
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !crate::runtime::MANAGED_ENV_KEYS.contains(&name)
        && !is_process_env_name(name)
}

/// Variables every process runs on, never a key: the shell's own (`PATH`,
/// `HOME`, the locale), the session's (`XDG_*`, `DISPLAY`, the D-Bus and ssh
/// agent sockets), and the ones tollgate, Claude Code, codex and herdr steer
/// their children with. A referenced name is scrubbed from every session
/// spawn, so one of these named as a key slot would strip it from every
/// `claude` / `codex`; they are refused where a name is accepted and never
/// scrubbed. Compared case-insensitively (Windows env names fold case).
pub(crate) fn is_process_env_name(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "PWD",
        "OLDPWD",
        "LANG",
        "LANGUAGE",
        "TZ",
        "TMPDIR",
        "TMP",
        "TEMP",
        "EDITOR",
        "VISUAL",
        "PAGER",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSH_AUTH_SOCK",
        "COLORTERM",
        "NO_COLOR",
        "COLUMNS",
        "LINES",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SYSTEMROOT",
        "COMSPEC",
        "PATHEXT",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "HERMES_HOME",
    ];
    const PREFIXES: &[&str] = &["LC_", "XDG_", "HERDR_", "TOLLGATE_", "CLAUTH_"];
    let upper = name.to_ascii_uppercase();
    EXACT.contains(&upper.as_str()) || PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// The env-var name `profile` references, when its `config.toml` sets a valid
/// one. `None` for a missing file, no key, a non-string, or an invalid name.
pub(crate) fn billing_key_env(profile: &ProfileName) -> Option<String> {
    let path = crate::profile::profile_subpath(profile, "config.toml").ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    env_name_from_config(&raw)
}

/// [`billing_key_env`] over a `config.toml` body.
pub(crate) fn env_name_from_config(raw: &str) -> Option<String> {
    let table = raw.parse::<toml::Table>().ok()?;
    let name = table.get(CONFIG_KEY)?.as_str()?.trim();
    valid_env_name(name).then(|| name.to_string())
}

/// The management key held in env var `name`, trimmed; `None` when the name
/// is invalid or the variable is unset or blank. The caller must not log,
/// print, persist or forward the value.
pub(crate) fn resolve(name: &str) -> Option<String> {
    if !valid_env_name(name) {
        return None;
    }
    let value = std::env::var(name).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Every monitoring-credential env-var name tollgate references, sorted,
/// de-duplicated: each profile's `billing_key_env`, and every monitor's
/// `api_key_env` / `billing_key_env` (`~/.tollgate/monitors.toml`; plan §4.3:
/// referenced monitoring vars join every scrub list). An unreadable file
/// contributes nothing.
pub(crate) fn referenced_env_vars() -> Vec<String> {
    let mut names: Vec<String> = crate::profile::load_app_state()
        .map(|state| state.profiles.iter().filter_map(billing_key_env).collect())
        .unwrap_or_default();
    if let Ok(monitors) = crate::usage::monitor::config::load() {
        for m in monitors {
            names.extend(m.api_key_env);
            names.extend(m.billing_key_env);
        }
    }
    names.retain(|n| valid_env_name(n));
    names.sort();
    names.dedup();
    names
}

/// The monitoring-ONLY credentials among [`referenced_env_vars`]: each
/// profile's `billing_key_env` and each monitor's `billing_key_env`, never a
/// monitor's `api_key_env` (an inference key some other child, such as the
/// gateway, may legitimately be configured to read).
pub(crate) fn monitoring_only_env_vars() -> Vec<String> {
    let mut names: Vec<String> = crate::profile::load_app_state()
        .map(|state| state.profiles.iter().filter_map(billing_key_env).collect())
        .unwrap_or_default();
    if let Ok(monitors) = crate::usage::monitor::config::load() {
        names.extend(monitors.into_iter().filter_map(|m| m.billing_key_env));
    }
    names.retain(|n| valid_env_name(n));
    names.sort();
    names.dedup();
    names
}

/// Strip the monitoring-only credentials ([`monitoring_only_env_vars`]) from
/// a non-session child the daemon spawns (the gateway). The daemon holds them
/// to read balances; a management key can mint and delete keys, so it never
/// rides into a third-party binary's environment (plan §4.3: "never" in child
/// env). Call before layering the child's own env, so a value the operator
/// set there deliberately still wins.
pub(crate) fn scrub_monitoring_env(command: &mut std::process::Command) {
    for name in monitoring_only_env_vars() {
        command.env_remove(name);
    }
}

/// Strip every referenced monitoring-credential variable from `command`'s
/// inherited env. Called on every session spawn; a management key must never
/// reach `claude`, `codex` or anything they run.
pub(crate) fn scrub_billing_env(command: &mut std::process::Command) {
    for name in referenced_env_vars() {
        command.env_remove(name);
    }
}

/// [`scrub_billing_env`] for a helper the daemon or the CLI spawns that is
/// neither a session nor the gateway: `notify-send`, the browser opener,
/// herdr, git, `ps` / `kill`, the FQDN lookup, the MCP probe. The process
/// that holds a monitoring or billing key in its env to read balances hands
/// none of them on — not a monitor's `api_key_env`, not a `billing_key_env`.
///
/// Test builds skip the scrub when no `HomeSandbox` is live: its name list
/// is read from `~/.tollgate`, which a test must never resolve to the
/// operator's real home, and a test that spawns a fixture helper without a
/// sandbox holds no referenced key to strip.
pub(crate) fn scrub_helper_env(command: &mut std::process::Command) {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return;
    }
    scrub_billing_env(command);
}

/// A new [`std::process::Command`] for `program` with [`scrub_helper_env`]
/// already applied — the one constructor every helper spawn goes through,
/// the platform-only ones included (`/usr/bin/security`, `ps`, `tasklist`,
/// `taskkill`, `powershell`). Those sites sit behind `cfg(target_os = …)`
/// gates a Linux build never compiles, so each builds its command here (or
/// through a portable builder that calls this) rather than scrubbing inline:
/// the scrub is then the same Linux-tested code on every platform, and a
/// gated site cannot forget it. The scrub runs first, so an `env` the caller
/// layers on afterwards (`ps`'s `LC_ALL`) still wins.
pub(crate) fn helper_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    scrub_helper_env(&mut command);
    command
}

#[cfg(test)]
#[path = "../../tests/inline/providers_billing_key.rs"]
mod tests;
