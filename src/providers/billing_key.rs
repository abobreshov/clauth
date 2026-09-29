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

/// Every env-var name any configured profile references, sorted, de-duplicated.
pub(crate) fn referenced_env_vars() -> Vec<String> {
    let Ok(state) = crate::profile::load_app_state() else {
        return Vec::new();
    };
    let mut names: Vec<String> = state.profiles.iter().filter_map(billing_key_env).collect();
    names.sort();
    names.dedup();
    names
}

/// Strip every referenced monitoring-credential variable from `command`'s
/// inherited env. Called on every session spawn; a management key must never
/// reach `claude`, `codex` or anything they run.
pub(crate) fn scrub_billing_env(command: &mut std::process::Command) {
    for name in referenced_env_vars() {
        command.env_remove(name);
    }
}

#[cfg(test)]
#[path = "../../tests/inline/providers_billing_key.rs"]
mod tests;
