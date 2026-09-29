//! The whitelist view of a Hermes home's `auth.json` (spec §4.7): G11 reads
//! it now, and the part 2 pool view renders it.
//!
//! `deny_unknown_fields` is deliberately NOT set. The structs have no field
//! that could hold `access_token`, `refresh_token`, `agent_key` or `api_key`,
//! and serde skips those values without keeping them. A secret never lands in
//! a tollgate value, so it can never reach a log line, a JSON surface or a
//! tag. Hermes is the only writer of this file, and tollgate reads it with
//! one plain read, no lock, capped at 1 MiB.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::IgnoredAny;

const MAX_AUTH_BYTES: u64 = 1024 * 1024;

/// The `auth.json` version this view was written against (`auth.py:71`).
#[allow(dead_code)] // read by the part 2 pool view (spec §4.7)
pub(crate) const EXPECTED_VERSION: u64 = 1;

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)] // the part 2 pool view renders every field (spec §4.7)
pub(crate) struct PoolAuthView {
    #[serde(default)]
    pub(crate) version: Option<u64>,
    #[serde(default)]
    pub(crate) active_provider: Option<String>,
    /// Key names only: every value is skipped.
    #[serde(default)]
    pub(crate) providers: BTreeMap<String, IgnoredAny>,
    #[serde(default)]
    pub(crate) credential_pool: BTreeMap<String, Vec<PoolEntryView>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)] // the part 2 pool view renders every field (spec §4.7)
pub(crate) struct PoolEntryView {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) auth_type: Option<String>,
    #[serde(default)]
    pub(crate) priority: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) last_status: Option<String>,
    #[serde(default)]
    pub(crate) last_status_at: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) last_error_code: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) last_error_reset_at: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) request_count: Option<u64>,
    #[serde(default)]
    pub(crate) secret_fingerprint: Option<String>,
    #[serde(default)]
    pub(crate) expires_at: Option<serde_json::Value>,
}

#[allow(dead_code)] // part 2 (spec §4.7)
impl PoolAuthView {
    /// Whether the view was written for a version this binary knows; another
    /// version renders `best_effort` (part 2).
    pub(crate) fn version_known(&self) -> bool {
        self.version == Some(EXPECTED_VERSION)
    }

    /// Pool entries for `provider`.
    pub(crate) fn entries(&self, provider: &str) -> &[PoolEntryView] {
        self.credential_pool
            .get(provider)
            .map_or(&[], |v| v.as_slice())
    }
}

/// Read `<home>/auth.json`. `Ok(None)` when absent; a symlink, an oversized
/// file, an unreadable or unparseable one is an `Err` (G11 refuses on it).
pub(crate) fn read_auth_view(home: &Path) -> Result<Option<PoolAuthView>> {
    let path = home.join("auth.json");
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to stat {}", path.display())),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.len() > MAX_AUTH_BYTES {
        bail!("{} is larger than 1 MiB", path.display());
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let view = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(view))
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_pool.rs"]
mod tests;
