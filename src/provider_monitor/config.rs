use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::types::ProviderKind;

pub(crate) const EXAMPLE: &str = r#"# Native logins stay in the official clients' credential stores.
# Run `clauth providers refresh`, then `clauth providers --json`.
poll_interval_seconds = 120
stale_after_seconds = 360
warning_remaining_percent = 15.0

[[targets]]
id = "codex"
provider = "codex"
# model = "your-model-id"
# auth_file = "~/.codex/auth.json"  # monitoring; defaults to the native store

[[targets]]
id = "grok"
provider = "grok"
# auth_entry = "issuer::account"  # required only if multiple Grok logins exist

[[targets]]
id = "agy"
provider = "antigravity"
# model = "your-model-id"

# Per-target: enabled = false disables polling and launching.
# command = "/absolute/path/to/tool" overrides direct launch only.
# args = [] adds native CLI arguments, without invoking a shell.
"#;

fn poll_default() -> u64 {
    120
}
fn stale_default() -> u64 {
    360
}
fn warning_default() -> f64 {
    15.0
}
fn enabled_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MonitorConfig {
    #[serde(default = "poll_default")]
    pub(crate) poll_interval_seconds: u64,
    #[serde(default = "stale_default")]
    pub(crate) stale_after_seconds: u64,
    #[serde(default = "warning_default")]
    pub(crate) warning_remaining_percent: f64,
    #[serde(default)]
    pub(crate) targets: Vec<TargetConfig>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: poll_default(),
            stale_after_seconds: stale_default(),
            warning_remaining_percent: warning_default(),
            targets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetConfig {
    pub(crate) id: String,
    pub(crate) provider: ProviderKind,
    #[serde(default = "enabled_default")]
    pub(crate) enabled: bool,
    pub(crate) model: Option<String>,
    pub(crate) auth_file: Option<PathBuf>,
    pub(crate) auth_entry: Option<String>,
    pub(crate) command: Option<PathBuf>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    /// Shown as an account on Overview. Omitted on `providers init` targets,
    /// so a monitor target is not an account until the user adds it.
    #[serde(default)]
    pub(crate) listed: bool,
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join("providers.toml"))
}

pub(crate) fn load() -> Result<MonitorConfig> {
    let path = path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MonitorConfig::default()),
        Err(_) => bail!("cannot read {}", path.display()),
    }
}

pub(crate) fn parse(text: &str) -> Result<MonitorConfig> {
    // TOML parser errors can echo user text; configuration must not leak a
    // mistakenly pasted credential into daemon logs.
    let config: MonitorConfig = toml::from_str(text).map_err(|_| {
        anyhow::anyhow!("invalid providers.toml; see `clauth providers example` for the schema")
    })?;
    if !(30..=3600).contains(&config.poll_interval_seconds) {
        bail!("poll_interval_seconds must be between 30 and 3600");
    }
    if config.stale_after_seconds < config.poll_interval_seconds
        || config.stale_after_seconds > 86400
    {
        bail!("stale_after_seconds must be at least the poll interval and at most 86400");
    }
    if super::types::percent(Some(config.warning_remaining_percent)).is_none() {
        bail!("warning_remaining_percent must be between 0 and 100");
    }
    let mut ids = HashSet::new();
    for target in &config.targets {
        if target.id.is_empty()
            || target.id.len() > 64
            || !target
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!("target IDs must be 1..64 ASCII letters, digits, hyphens or underscores");
        }
        if !ids.insert(&target.id) {
            bail!("duplicate provider target ID");
        }
        if target
            .model
            .as_ref()
            .is_some_and(|m| m.trim().is_empty() || m.chars().any(char::is_control))
        {
            bail!("model must be a nonempty model ID without control characters");
        }
    }
    Ok(config)
}

/// Add a native provider target, or mark an existing one as an Overview account.
///
/// `auth_entry` is the Grok `issuer::account` selector, or `None` when the
/// provider has a single native login. A target that already watches that
/// login and is already listed is left as it is. A monitor-only target (the
/// `providers init` rows) becomes listed instead of growing a second copy.
pub(crate) fn ensure_native_target(
    provider: super::types::ProviderKind,
    auth_entry: Option<String>,
) -> Result<bool> {
    let mut config = load()?;
    if let Some(target) = config
        .targets
        .iter_mut()
        .find(|target| target.provider == provider && target.auth_entry == auth_entry)
    {
        if target.listed {
            return Ok(false);
        }
        target.listed = true;
        save(&config)?;
        return Ok(true);
    }
    let id = next_target_id(&config, provider.tool());
    config.targets.push(TargetConfig {
        id,
        provider,
        enabled: true,
        model: None,
        auth_file: None,
        auth_entry,
        command: None,
        args: Vec::new(),
        listed: true,
    });
    save(&config)?;
    Ok(true)
}

/// Take a native login off Overview. The monitor target stays, so the
/// Providers tab still reads it and `+ add account` can list it again.
pub(crate) fn unlist_native_target(id: &str) -> Result<()> {
    let mut config = load()?;
    let Some(target) = config.targets.iter_mut().find(|target| target.id == id) else {
        bail!("provider target '{id}' not found");
    };
    if target.listed {
        target.listed = false;
        save(&config)?;
    }
    Ok(())
}

fn next_target_id(config: &MonitorConfig, base: &str) -> String {
    if !config.targets.iter().any(|target| target.id == base) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let id = format!("{base}-{n}");
        if !config.targets.iter().any(|target| target.id == id) {
            return id;
        }
        n += 1;
    }
}

pub(crate) fn save(config: &MonitorConfig) -> Result<()> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        crate::profile::mkdir_700(dir)?;
    }
    let text = toml::to_string_pretty(config).context("could not serialize providers.toml")?;
    parse(&text)?;
    crate::profile::atomic_write_600(&path, text)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

pub(crate) fn expand(path: &std::path::Path) -> Result<PathBuf> {
    if let Ok(suffix) = path.strip_prefix("~") {
        return Ok(crate::profile::home_dir()
            .context("home directory not found")?
            .join(suffix));
    }
    if !path.is_absolute() {
        bail!("credential and command paths must be absolute or start with ~/");
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_native_target_appends_once_and_roundtrips() {
        let _home = crate::testutil::HomeSandbox::new();
        let added = ensure_native_target(
            crate::provider_monitor::types::ProviderKind::Grok,
            Some("https://auth.x.ai::account-a".into()),
        )
        .unwrap();
        assert!(added);
        let again = ensure_native_target(
            crate::provider_monitor::types::ProviderKind::Grok,
            Some("https://auth.x.ai::account-a".into()),
        )
        .unwrap();
        assert!(!again);
        let config = load().unwrap();
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].id, "grok");
        assert!(config.targets[0].listed, "adding the account lists it");
        assert_eq!(
            config.targets[0].auth_entry.as_deref(),
            Some("https://auth.x.ai::account-a")
        );
        let second = ensure_native_target(
            crate::provider_monitor::types::ProviderKind::Antigravity,
            None,
        )
        .unwrap();
        assert!(second);
        let config = load().unwrap();
        assert_eq!(
            config
                .targets
                .iter()
                .map(|target| target.id.as_str())
                .collect::<Vec<_>>(),
            ["grok", "agy"]
        );
    }

    #[test]
    fn adding_an_account_lists_an_existing_monitor_target() {
        let _home = crate::testutil::HomeSandbox::new();
        let mut config = parse(EXAMPLE).unwrap();
        save(&config).unwrap();
        assert!(config.targets.iter().all(|target| !target.listed));
        let added =
            ensure_native_target(crate::provider_monitor::types::ProviderKind::Grok, None).unwrap();
        assert!(added);
        config = load().unwrap();
        let grok: Vec<_> = config
            .targets
            .iter()
            .filter(|target| target.provider == crate::provider_monitor::types::ProviderKind::Grok)
            .collect();
        assert_eq!(grok.len(), 1, "the init row is reused");
        assert!(grok[0].listed);
        assert!(!config.targets.iter().any(|target| target.provider
            == crate::provider_monitor::types::ProviderKind::Antigravity
            && target.listed));
    }

    #[test]
    fn unlist_native_target_hides_the_account_and_keeps_the_monitor() {
        let _home = crate::testutil::HomeSandbox::new();
        ensure_native_target(crate::provider_monitor::types::ProviderKind::Grok, None).unwrap();
        unlist_native_target("grok").unwrap();
        let config = load().unwrap();
        assert_eq!(config.targets.len(), 1);
        assert!(!config.targets[0].listed);
        unlist_native_target("grok").unwrap();
        assert_eq!(load().unwrap().targets.len(), 1);
        assert!(unlist_native_target("missing").is_err());
    }

    #[test]
    fn example_enables_all_requested_providers() {
        let c = parse(EXAMPLE).unwrap();
        assert_eq!(
            c.targets
                .iter()
                .map(|t| t.provider.tool())
                .collect::<Vec<_>>(),
            ["codex", "grok", "agy"]
        );
        assert!(c.targets.iter().all(|t| t.enabled));
        assert!(
            c.targets.iter().all(|t| !t.listed),
            "init targets are monitors, not overview accounts"
        );
    }

    #[test]
    fn invalid_or_ambiguous_configuration_is_rejected() {
        for text in [
            "poll_interval_seconds = 0",
            "warning_remaining_percent = nan",
            "unknown = 1",
            "[[targets]]\nid='a'\nprovider='grok'\n[[targets]]\nid='a'\nprovider='codex'",
            "[[targets]]\nid='../secret'\nprovider='grok'",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }
}
