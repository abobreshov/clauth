//! `~/.tollgate/monitors.toml`: the monitoring-only accounts (plan v3.1 §4.2,
//! §4.3). A monitor is a usage source that is NOT a Claude Code / codex
//! profile: a Nous Portal account read through Hermes' login, a billing-only
//! OpenRouter key, another provider's key.
//!
//! ```toml
//! [[monitor]]
//! id = "nous"
//! kind = "nous"              # nous | ollama_cloud | openrouter | provider
//! label = "Nous (Hermes)"
//! hermes_home = "~/.hermes"  # nous only; default ~/.hermes
//! budget_usd_month = "20"    # optional user budget → a Budget meter + severity
//! alert_pct = 80             # optional: notify once past this share
//!
//! [[monitor]]
//! id = "or-billing"
//! kind = "openrouter"
//! api_key_env = "OPENROUTER_API_KEY"   # the NAME of an env var, never its value
//! billing_key_env = "OPENROUTER_MGMT"  # optional monitoring-only key, preferred for reads
//! ```
//!
//! **No secret is ever stored here.** Keys are referenced by env var NAME and
//! the value is read from the process environment at fetch time
//! ([`super::source::resolve_target`]). A table carrying a secret-shaped key
//! (`api_key`, `token`, …) is refused with a message naming the `_env` field.
//!
//! The file is created 0600 (atomic replace) and edited in place through
//! `toml_edit`, so an operator's comments survive `tollgate monitor add` /
//! `remove`. Unknown keys are refused at the top level and inside a
//! `[[monitor]]` table, so a typo can never silently drop a setting.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::providers::Provider;
use crate::usage::observation::Amount;

/// The file's name under the data dir.
pub(crate) const MONITORS_FILE: &str = "monitors.toml";
/// The per-monitor cache directory's name under the data dir.
pub(crate) const MONITORS_DIR: &str = "monitors";
/// The lock serialising edits of [`MONITORS_FILE`].
const MONITORS_LOCK: &str = ".monitors.lock";
/// Default refresh cadence of one monitor.
pub(crate) const DEFAULT_TTL_SECS: u64 = 90;
/// The fastest cadence a monitor may ask for.
pub(crate) const MIN_TTL_SECS: u64 = 30;
/// Where Hermes keeps its home when `hermes_home` is not set.
pub(crate) const DEFAULT_HERMES_HOME: &str = "~/.hermes";
/// Largest `monitors.toml` read.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Longest monitor id.
const MAX_ID_LEN: usize = 48;

/// What a monitor reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MonitorKind {
    /// Nous Portal, through Hermes' unexpired OAuth access token.
    #[value(name = "nous")]
    Nous,
    /// Ollama Cloud (`ollama.com/api/usage`).
    #[value(name = "ollama_cloud")]
    OllamaCloud,
    /// OpenRouter's credits + key endpoints.
    #[serde(rename = "openrouter")]
    #[value(name = "openrouter")]
    OpenRouter,
    /// Any other typed provider, named by `provider = "<Provider variant>"`.
    #[value(name = "provider")]
    Provider,
}

impl MonitorKind {
    /// The spelling `monitors.toml` and the CLI use.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Nous => "nous",
            Self::OllamaCloud => "ollama_cloud",
            Self::OpenRouter => "openrouter",
            Self::Provider => "provider",
        }
    }
}

/// One `[[monitor]]` table. Every field is non-secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MonitorConfig {
    /// Stable id: `[a-z0-9][a-z0-9_-]*`, at most 48 chars. The account id is
    /// `monitor:<id>` and the cache is `~/.tollgate/monitors/<id>.json`.
    pub(crate) id: String,
    pub(crate) kind: MonitorKind,
    /// `kind = "provider"` only: the typed provider (`DeepSeek`, `Zai`,
    /// `MiniMax`, `OpenRouter`), by variant or display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    /// Human name; the id when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    /// NAME of the env var holding the api key. Its value is read at fetch
    /// time and never stored, logged or serialised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_key_env: Option<String>,
    /// NAME of the env var holding a monitoring-only key (an OpenRouter
    /// management key, an Ollama monitor key). Preferred over `api_key_env`
    /// for reads when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) billing_key_env: Option<String>,
    /// `kind = "nous"` only: the Hermes home whose `auth.json` holds the
    /// login. Default `~/.hermes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hermes_home: Option<String>,
    /// A monthly budget in USD: adds a `budget.monthly` Budget meter and
    /// grades it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) budget_usd_month: Option<Amount>,
    /// Notify once when the budget's spent share (or, with no budget, the
    /// worst window's used share) reaches this percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) alert_pct: Option<f64>,
    /// Refresh cadence override, seconds (min 30, default 90).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ttl_secs: Option<u64>,
    #[serde(default = "default_enabled")]
    pub(crate) enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl MonitorConfig {
    /// A minimal enabled monitor; callers fill the rest.
    #[cfg(test)]
    pub(crate) fn new(id: impl Into<String>, kind: MonitorKind) -> Self {
        Self {
            id: id.into(),
            kind,
            provider: None,
            label: None,
            api_key_env: None,
            billing_key_env: None,
            hermes_home: None,
            budget_usd_month: None,
            alert_pct: None,
            ttl_secs: None,
            enabled: true,
        }
    }

    /// The label shown for this monitor.
    pub(crate) fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    /// Refresh cadence, ms.
    pub(crate) fn ttl_ms(&self) -> u64 {
        self.ttl_secs
            .unwrap_or(DEFAULT_TTL_SECS)
            .max(MIN_TTL_SECS)
            .saturating_mul(1000)
    }

    /// The typed provider a `provider` / `openrouter` monitor reads.
    pub(crate) fn typed_provider(&self) -> Option<Provider> {
        match self.kind {
            MonitorKind::OpenRouter => Some(Provider::OpenRouter),
            MonitorKind::Provider => self.provider.as_deref().and_then(parse_provider),
            MonitorKind::Nous | MonitorKind::OllamaCloud => None,
        }
    }

    /// The Hermes home, `~` expanded against `home`.
    pub(crate) fn hermes_home_in(&self, home: &Path) -> PathBuf {
        expand_home(
            self.hermes_home.as_deref().unwrap_or(DEFAULT_HERMES_HOME),
            home,
        )
    }

    /// The non-secret identity of what this monitor reads. A cache written
    /// under a different fingerprint describes another target and is
    /// discarded.
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.kind.as_str(),
            self.provider.as_deref().unwrap_or(""),
            self.api_key_env.as_deref().unwrap_or(""),
            self.billing_key_env.as_deref().unwrap_or(""),
            self.hermes_home.as_deref().unwrap_or(""),
        )
    }

    /// Check one monitor on its own (uniqueness is [`validate_all`]'s).
    pub(crate) fn validate(&self) -> Result<()> {
        let id = &self.id;
        validate_id(id)?;
        for (field, name) in [
            ("api_key_env", &self.api_key_env),
            ("billing_key_env", &self.billing_key_env),
        ] {
            if let Some(name) = name {
                validate_env_name(name)
                    .with_context(|| format!("monitor '{id}': invalid {field}"))?;
            }
        }
        if let Some(label) = &self.label
            && (label.trim().is_empty() || label.chars().any(char::is_control))
        {
            bail!("monitor '{id}': label must be non-empty printable text");
        }
        match self.kind {
            MonitorKind::Provider => {
                let Some(raw) = self.provider.as_deref() else {
                    bail!("monitor '{id}': kind = \"provider\" needs provider = \"<name>\"");
                };
                let Some(p) = parse_provider(raw) else {
                    bail!(
                        "monitor '{id}': unknown provider {raw:?} (known: {})",
                        known_provider_names().join(", ")
                    );
                };
                if p == Provider::Alibaba {
                    bail!(
                        "monitor '{id}': Alibaba's usage needs a console session, not a key; \
                         add it as a profile with `tollgate login` instead"
                    );
                }
            }
            _ if self.provider.is_some() => {
                bail!(
                    "monitor '{id}': provider is only valid with kind = \"provider\" (kind is {:?})",
                    self.kind.as_str()
                );
            }
            _ => {}
        }
        if self.hermes_home.is_some() && self.kind != MonitorKind::Nous {
            bail!("monitor '{id}': hermes_home is only valid with kind = \"nous\"");
        }
        if let Some(h) = &self.hermes_home
            && !(h.starts_with('/') || h == "~" || h.starts_with("~/"))
        {
            bail!("monitor '{id}': hermes_home must be absolute or start with ~/");
        }
        if matches!(
            self.kind,
            MonitorKind::OpenRouter | MonitorKind::Provider | MonitorKind::OllamaCloud
        ) && self.api_key_env.is_none()
            && self.billing_key_env.is_none()
        {
            bail!(
                "monitor '{id}': kind = {:?} needs api_key_env or billing_key_env (an env var NAME)",
                self.kind.as_str()
            );
        }
        if let Some(b) = &self.budget_usd_month
            && *b <= Amount::zero()
        {
            bail!("monitor '{id}': budget_usd_month must be above zero");
        }
        if let Some(p) = self.alert_pct
            && !(p.is_finite() && p > 0.0 && p <= 100.0)
        {
            bail!("monitor '{id}': alert_pct must be in (0, 100]");
        }
        if let Some(t) = self.ttl_secs
            && t < MIN_TTL_SECS
        {
            bail!("monitor '{id}': ttl_secs must be at least {MIN_TTL_SECS}");
        }
        Ok(())
    }
}

/// `[a-z0-9][a-z0-9_-]*`, at most 48 chars: safe as a file stem and an id.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !ok {
        bail!("invalid monitor id {id:?}: use [a-z0-9][a-z0-9_-]*, at most {MAX_ID_LEN} chars");
    }
    Ok(())
}

/// An env var NAME: `[A-Za-z_][A-Za-z0-9_]*`, at most 64 chars, and not
/// shaped like a key. A value pasted where the name goes (`sk-or-…`, a long
/// token) is refused, so a secret can never reach `monitors.toml` or argv
/// through this field.
pub(crate) fn validate_env_name(name: &str) -> Result<()> {
    let shaped = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !shaped {
        bail!(
            "expected the NAME of an environment variable (like OPENROUTER_API_KEY), not its value"
        );
    }
    let digits = name.bytes().filter(u8::is_ascii_digit).count();
    let lower = name.bytes().filter(u8::is_ascii_lowercase).count();
    if name.len() >= 24 && digits >= 4 && lower >= 4 {
        bail!(
            "that looks like a key, not a variable name — export the key and pass the \
             variable's NAME instead"
        );
    }
    Ok(())
}

/// Every provider a `kind = "provider"` monitor can name. Kept as a list
/// rather than an exhaustive match so a new [`Provider`] variant does not
/// break this file; add it here to make it nameable.
// TODO(merge): add Provider::OllamaCloud once the Ollama branch lands.
const NAMEABLE_PROVIDERS: &[Provider] = &[
    Provider::DeepSeek,
    Provider::Zai,
    Provider::Alibaba,
    Provider::OpenRouter,
    Provider::MiniMax,
];

/// A provider by variant name (`DeepSeek`) or display name (`Z.ai`),
/// case-insensitively.
pub(crate) fn parse_provider(raw: &str) -> Option<Provider> {
    let raw = raw.trim();
    NAMEABLE_PROVIDERS.iter().copied().find(|p| {
        format!("{p:?}").eq_ignore_ascii_case(raw) || p.display_name().eq_ignore_ascii_case(raw)
    })
}

fn known_provider_names() -> Vec<String> {
    NAMEABLE_PROVIDERS
        .iter()
        .filter(|p| **p != Provider::Alibaba)
        .map(|p| format!("{p:?}"))
        .collect()
}

/// `~` / `~/x` against `home`; anything else as given.
pub(crate) fn expand_home(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(raw),
    }
}

// ── file ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorsFile {
    #[serde(default)]
    monitor: Vec<MonitorConfig>,
}

/// Keys that would hold a secret value; refused with a pointer to the `_env`
/// field instead of serde's generic unknown-field error.
const SECRET_KEYS: &[&str] = &[
    "api_key",
    "billing_key",
    "key",
    "token",
    "access_token",
    "secret",
    "password",
];

/// Parse and validate `monitors.toml` text.
pub(crate) fn parse(text: &str) -> Result<Vec<MonitorConfig>> {
    let raw: toml::Table = toml::from_str(text).context("monitors.toml is not valid TOML")?;
    if let Some(toml::Value::Array(tables)) = raw.get("monitor") {
        for t in tables {
            let Some(t) = t.as_table() else { continue };
            if let Some(k) = SECRET_KEYS.iter().find(|k| t.contains_key(**k)) {
                let id = t.get("id").and_then(toml::Value::as_str).unwrap_or("?");
                bail!(
                    "monitors.toml: monitor '{id}' sets `{k}` — secrets are never stored here; \
                     name an environment variable with `api_key_env` / `billing_key_env` instead"
                );
            }
        }
    }
    let file: MonitorsFile = toml::from_str(text).context("monitors.toml")?;
    validate_all(&file.monitor)?;
    Ok(file.monitor)
}

/// Every monitor valid, ids unique.
pub(crate) fn validate_all(monitors: &[MonitorConfig]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for m in monitors {
        m.validate()?;
        if !seen.insert(m.id.as_str()) {
            bail!("monitors.toml: duplicate monitor id '{}'", m.id);
        }
    }
    Ok(())
}

/// `~/.tollgate/monitors.toml`.
pub(crate) fn monitors_path() -> Result<PathBuf> {
    Ok(crate::profile::tollgate_dir()?.join(MONITORS_FILE))
}

/// `~/.tollgate/monitors/`.
pub(crate) fn monitors_dir() -> Result<PathBuf> {
    Ok(crate::profile::tollgate_dir()?.join(MONITORS_DIR))
}

/// The configured monitors; an absent file is none. Read-only.
pub(crate) fn load() -> Result<Vec<MonitorConfig>> {
    let path = monitors_path()?;
    match read_capped(&path)? {
        Some(text) => parse(&text).with_context(|| format!("{}", path.display())),
        None => Ok(Vec::new()),
    }
}

fn read_capped(path: &Path) -> Result<Option<String>> {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("read {}", path.display()))?;
    if text.len() as u64 > MAX_FILE_BYTES {
        bail!("{} is larger than 1 MiB", path.display());
    }
    Ok(Some(text))
}

/// Run `edit` over the file's document under the edit lock, validate the
/// result as a whole, and replace the file atomically at 0600. Nothing is
/// written when `edit` or the validation fails.
fn edit_file(edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>) -> Result<()> {
    let dir = crate::profile::tollgate_dir()?;
    crate::profile::mkdir_700(&dir).with_context(|| format!("create {}", dir.display()))?;
    let lock = crate::profile::open_state_file(&dir.join(MONITORS_LOCK))
        .context("open the monitors.toml lock")?;
    crate::lock::lock_file_with_timeout(&lock, std::time::Duration::from_secs(5))?;
    let path = monitors_path()?;
    let text = read_capped(&path)?.unwrap_or_default();
    let mut doc: toml_edit::DocumentMut =
        text.parse().context("monitors.toml is not valid TOML")?;
    edit(&mut doc)?;
    let out = doc.to_string();
    parse(&out).context("the edited monitors.toml would not load")?;
    crate::profile::atomic_write_600(&path, out.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Append `m` as a new `[[monitor]]` table. Refuses an invalid monitor and a
/// duplicate id.
pub(crate) fn add(m: &MonitorConfig) -> Result<()> {
    m.validate()?;
    edit_file(|doc| {
        if tables(doc).is_some_and(|a| a.iter().any(|t| table_id(t) == Some(&m.id))) {
            bail!("a monitor with id '{}' already exists", m.id);
        }
        let array = doc
            .entry("monitor")
            .or_insert(toml_edit::Item::ArrayOfTables(
                toml_edit::ArrayOfTables::new(),
            ))
            .as_array_of_tables_mut()
            .context("monitors.toml: `monitor` is not an array of tables")?;
        array.push(to_table(m));
        Ok(())
    })
}

/// Remove the monitor with `id`. `Ok(false)` when there is none.
pub(crate) fn remove(id: &str) -> Result<bool> {
    let mut removed = false;
    edit_file(|doc| {
        let Some(array) = doc
            .get_mut("monitor")
            .and_then(toml_edit::Item::as_array_of_tables_mut)
        else {
            return Ok(());
        };
        let found = array.iter().position(|t| table_id(t) == Some(id));
        if let Some(idx) = found {
            array.remove(idx);
            removed = true;
        }
        if array.is_empty() {
            doc.remove("monitor");
        }
        Ok(())
    })?;
    Ok(removed)
}

fn tables(doc: &toml_edit::DocumentMut) -> Option<&toml_edit::ArrayOfTables> {
    doc.get("monitor")
        .and_then(toml_edit::Item::as_array_of_tables)
}

fn table_id(t: &toml_edit::Table) -> Option<&str> {
    t.get("id").and_then(toml_edit::Item::as_str)
}

fn to_table(m: &MonitorConfig) -> toml_edit::Table {
    use toml_edit::value;
    let mut t = toml_edit::Table::new();
    t["id"] = value(m.id.clone());
    t["kind"] = value(m.kind.as_str());
    let strings = [
        ("provider", &m.provider),
        ("label", &m.label),
        ("api_key_env", &m.api_key_env),
        ("billing_key_env", &m.billing_key_env),
        ("hermes_home", &m.hermes_home),
    ];
    for (k, v) in strings {
        if let Some(v) = v {
            t[k] = value(v.clone());
        }
    }
    if let Some(b) = &m.budget_usd_month {
        // A string keeps the decimal exact.
        t["budget_usd_month"] = value(b.as_str());
    }
    if let Some(p) = m.alert_pct {
        t["alert_pct"] = value(p);
    }
    if let Some(ttl) = m.ttl_secs {
        t["ttl_secs"] = value(i64::try_from(ttl).unwrap_or(i64::MAX));
    }
    if !m.enabled {
        t["enabled"] = value(false);
    }
    t
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_config.rs"]
mod tests;
