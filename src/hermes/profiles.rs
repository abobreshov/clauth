//! `~/.tollgate/hermes-profiles.toml` — the Hermes roster (spec §3).
//!
//! The codex roster's contract, for a third file: a missing file is an empty
//! roster, unknown keys are tolerated on load and dropped on the next rewrite,
//! and the only writer is [`HermesState::update`], which holds the state lock
//! across load → mutate → save and skips a save that changed nothing. An old
//! binary never opens this file, so it cannot drop or corrupt a Hermes entry.
//!
//! The roster holds bindings, never secrets: the env-mode key lives in the
//! home's `.env`, and the roster carries only Hermes' own fingerprint of it.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::lock::StateLock;
use crate::profile::{atomic_write_600, mkdir_700, read_toml_file, tollgate_dir};

/// The only schema this binary writes.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The providers a Hermes profile can be bound to in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Provider {
    #[serde(rename = "nous")]
    Nous,
    #[serde(rename = "openrouter")]
    Openrouter,
    #[serde(rename = "ollama-cloud")]
    OllamaCloud,
}

impl Provider {
    /// Hermes' own provider id, which is also the `--provider` value.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Provider::Nous => "nous",
            Provider::Openrouter => "openrouter",
            Provider::OllamaCloud => "ollama-cloud",
        }
    }

    /// The user-facing name, for the key prompt.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Provider::Nous => "Nous",
            Provider::Openrouter => "OpenRouter",
            Provider::OllamaCloud => "Ollama Cloud",
        }
    }

    /// The env var the provider plugin declares for its key
    /// (`plugins/model-providers/<p>/__init__.py`).
    pub(crate) fn key_env(self) -> &'static str {
        match self {
            Provider::Nous => "NOUS_API_KEY",
            Provider::Openrouter => "OPENROUTER_API_KEY",
            Provider::OllamaCloud => "OLLAMA_API_KEY",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `account` holds one account of one provider; `pool` holds Hermes'
/// credential pool for one provider (D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Account,
    Pool,
}

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Mode::Account => "account",
            Mode::Pool => "pool",
        }
    }
}

/// How the home authenticates: a key line in the home `.env` tollgate
/// manages, Hermes' own OAuth login, or Hermes' pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Auth {
    Env,
    Oauth,
    Pool,
}

impl Auth {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Auth::Env => "env",
            Auth::Oauth => "oauth",
            Auth::Pool => "pool",
        }
    }
}

/// `version_policy`: what a Hermes outside the verified 0.19.x series does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum VersionPolicy {
    #[default]
    Warn,
    Refuse,
}

/// `[settings]`: both keys optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settings {
    /// Entrypoint override (§4.5 step 1); it must still pass the shebang check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bin: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version_policy: Option<VersionPolicy>,
}

/// One `[[profiles]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HermesProfile {
    pub(crate) name: String,
    pub(crate) provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) mode: Mode,
    pub(crate) auth: Auth,
    /// `auth = env` only: the var the home `.env` binds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key_env: Option<String>,
    /// `auth = env` only: `sha256:` + 16 hex, Hermes' own format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key_fingerprint: Option<String>,
    pub(crate) created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HermesState {
    #[serde(default = "default_schema")]
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settings: Option<Settings>,
    #[serde(default)]
    profiles: Vec<HermesProfile>,
}

fn default_schema() -> u32 {
    SCHEMA_VERSION
}

impl Default for HermesState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            settings: None,
            profiles: Vec::new(),
        }
    }
}

impl HermesState {
    /// Read the roster, lock-free: a missing file is an empty roster.
    pub(crate) fn load() -> Result<Self> {
        let path = hermes_state_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let state: Self = read_toml_file(&path)?;
        if state.schema_version != SCHEMA_VERSION {
            bail!(
                "{} has schema_version {}; this tollgate reads {SCHEMA_VERSION}",
                path.display(),
                state.schema_version
            );
        }
        Ok(state)
    }

    pub(crate) fn profiles(&self) -> &[HermesProfile] {
        &self.profiles
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.profiles.iter().map(|p| p.name.clone()).collect()
    }

    pub(crate) fn settings(&self) -> Settings {
        self.settings.clone().unwrap_or_default()
    }

    /// Exact-match lookup.
    pub(crate) fn find(&self, name: &str) -> Option<&HermesProfile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    pub(crate) fn holds(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// Case-insensitive lookup returning the canonical casing.
    pub(crate) fn canonical_name(&self, query: &str) -> Option<String> {
        self.profiles
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(query))
            .map(|p| p.name.clone())
    }

    /// Append `profile`; an entry with the same name is replaced, so a retry
    /// cannot double it.
    pub(crate) fn add_profile(&mut self, profile: HermesProfile) {
        self.profiles.retain(|p| p.name != profile.name);
        self.profiles.push(profile);
    }

    pub(crate) fn remove_profile(&mut self, name: &str) {
        self.profiles.retain(|p| p.name != name);
    }

    /// Record the env-mode key's fingerprint (§4.2 step 7).
    pub(crate) fn set_fingerprint(&mut self, name: &str, fingerprint: &str) {
        if let Some(p) = self.profiles.iter_mut().find(|p| p.name == name) {
            p.key_fingerprint = Some(fingerprint.to_string());
        }
    }

    /// The one mutation path: load under the state lock, hand the closure
    /// the on-disk state, persist only what changed. The codex contract
    /// ([`crate::codex_profiles::CodexState::update`]).
    pub(crate) fn update<T>(f: impl FnOnce(&mut HermesState) -> Result<T>) -> Result<T> {
        let lock = StateLock::acquire()?;
        let mut state = Self::load()?;
        let before = state.clone();
        let out = f(&mut state)?;
        if state != before {
            state.save(&lock)?;
        }
        Ok(out)
    }

    fn save(&self, _witness: &StateLock) -> Result<()> {
        mkdir_700(&tollgate_dir()?)?;
        atomic_write_600(&hermes_state_path()?, toml::to_string_pretty(self)?)
            .context("failed to write hermes-profiles.toml")
    }
}

pub(crate) fn hermes_state_path() -> Result<PathBuf> {
    Ok(tollgate_dir()?.join("hermes-profiles.toml"))
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_profiles.rs"]
mod tests;
