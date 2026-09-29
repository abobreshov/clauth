//! `tollgate monitor list | add | remove | refresh [id]`.
//!
//! `add` takes every setting as a flag and accepts env var NAMES only: there is
//! no flag that takes a key, and a value pasted into `--api-key-env` is refused
//! ([`super::config::validate_env_name`]), so a secret never reaches argv, shell
//! history or `monitors.toml`.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;
use std::path::PathBuf;

use super::alert::DesktopNotifier;
use super::cache::{self, RefreshDeps, RefreshOutcome};
use super::config::{self, MonitorConfig, MonitorKind};
use super::observe::{monitor_severity, observe_monitor};
use super::source::{LiveHttp, process_env};
use crate::out::outln;
use crate::usage::derive::SeverityBasis;
use crate::usage::observation::{AccountObservation, Freshness};

#[derive(Subcommand, Debug)]
pub(crate) enum MonitorCommand {
    /// List the configured monitors and what their caches hold
    List {
        /// Same as `tollgate monitor --json`.
        #[arg(long, hide = true)]
        json: bool,
    },
    /// Add a monitor to ~/.tollgate/monitors.toml
    ///
    /// Keys are named by environment variable, never passed: `--api-key-env
    /// OPENROUTER_API_KEY` stores the NAME, and the daemon reads the value from
    /// its own environment at fetch time.
    Add(Box<MonitorAddArgs>),
    /// Remove a monitor and its cache
    Remove {
        /// The monitor's id.
        id: String,
    },
    /// Fetch monitors now, ignoring their refresh interval
    ///
    /// Bare, every enabled monitor; with an id, that one (even if disabled).
    /// A monitor held after a 429 stays held.
    Detect {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        explain: bool,
        #[arg(long)]
        apply: bool,
        #[arg(long, requires = "apply")]
        yes: bool,
    },
    Refresh {
        /// Only this monitor.
        id: Option<String>,
        /// Emit the `tollgate usage --json` envelope of the refreshed monitors.
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "DIR")]
        capture: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
pub(crate) struct MonitorAddArgs {
    /// The monitor's id: [a-z0-9][a-z0-9_-]*.
    #[arg(value_parser = validate_preset_or_id)]
    pub(crate) id: String,
    /// What it reads.
    #[arg(long, value_enum)]
    pub(crate) kind: Option<MonitorKind>,
    #[arg(long = "id")]
    pub(crate) override_id: Option<String>,
    /// With `--kind provider`: the typed provider (DeepSeek, Zai, Alibaba,
    /// MiniMax, OpenRouter, OllamaCloud).
    #[arg(long, value_name = "NAME")]
    pub(crate) provider: Option<String>,
    /// Human name.
    #[arg(long)]
    pub(crate) label: Option<String>,
    /// NAME of the env var holding the api key.
    #[arg(long, value_name = "VAR")]
    pub(crate) api_key_env: Option<String>,
    /// NAME of the env var holding a monitoring-only key (preferred for reads).
    #[arg(long, alias = "admin-key-env", value_name = "VAR")]
    pub(crate) billing_key_env: Option<String>,
    /// With `--kind nous`: the Hermes home (default ~/.hermes).
    #[arg(long, value_name = "PATH")]
    pub(crate) hermes_home: Option<String>,
    #[arg(long)]
    pub(crate) tool_home: Option<String>,
    #[arg(long)]
    pub(crate) auth_entry: Option<String>,
    #[arg(long)]
    pub(crate) via: Option<String>,
    #[arg(long)]
    pub(crate) probe: bool,
    #[arg(long)]
    pub(crate) probe_model: Option<String>,
    /// Monthly budget in USD.
    #[arg(long, value_name = "USD")]
    pub(crate) budget_usd_month: Option<String>,
    /// Notify once past this percent of the budget (or the worst window).
    #[arg(long, value_name = "PCT")]
    pub(crate) alert_pct: Option<f64>,
    /// Refresh interval in seconds (min 30, default 90).
    #[arg(long, value_name = "SECS")]
    pub(crate) ttl_secs: Option<u64>,
    /// Add it disabled.
    #[arg(long)]
    pub(crate) disabled: bool,
}

impl MonitorAddArgs {
    /// The monitor these flags describe, validated.
    pub(crate) fn to_config(&self) -> Result<MonitorConfig> {
        let budget = match &self.budget_usd_month {
            Some(raw) => Some(
                crate::usage::observation::Amount::parse(raw.trim_start_matches('$'))
                    .ok_or_else(|| anyhow::anyhow!("--budget-usd-month: not a decimal: {raw:?}"))?,
            ),
            None => None,
        };
        let preset = if self.kind.is_none() {
            Some(preset(&self.id)?)
        } else {
            None
        };
        let defaults = preset.as_ref();
        let m = MonitorConfig {
            id: self
                .override_id
                .clone()
                .unwrap_or_else(|| defaults.map_or_else(|| self.id.clone(), |m| m.id.clone())),
            kind: self
                .kind
                .or_else(|| defaults.map(|m| m.kind))
                .ok_or_else(|| anyhow::anyhow!("missing monitor kind"))?,
            provider: self.provider.clone(),
            label: self
                .label
                .clone()
                .or_else(|| defaults.and_then(|m| m.label.clone())),
            api_key_env: self
                .api_key_env
                .clone()
                .or_else(|| defaults.and_then(|m| m.api_key_env.clone())),
            billing_key_env: self.billing_key_env.clone(),
            hermes_home: self
                .hermes_home
                .clone()
                .or_else(|| defaults.and_then(|m| m.hermes_home.clone())),
            tool_home: self
                .tool_home
                .clone()
                .or_else(|| defaults.and_then(|m| m.tool_home.clone())),
            auth_entry: self.auth_entry.clone(),
            via: self
                .via
                .clone()
                .or_else(|| defaults.and_then(|m| m.via.clone())),
            probe: self.probe,
            probe_model: self.probe_model.clone(),
            budget_usd_month: budget,
            alert_pct: self.alert_pct,
            ttl_secs: self.ttl_secs.or_else(|| {
                if self.probe {
                    Some(1800)
                } else {
                    defaults.and_then(|m| m.ttl_secs)
                }
            }),
            enabled: !self.disabled,
        };
        m.validate()?;
        Ok(m)
    }
}

/// `tollgate monitor [--json] [verb]`: bare lists.
pub(crate) fn dispatch(json: bool, cmd: Option<MonitorCommand>) -> Result<()> {
    match cmd {
        Some(cmd) => run(cmd),
        None => list(json),
    }
}

/// Dispatch one `tollgate monitor` verb.
pub(crate) fn run(cmd: MonitorCommand) -> Result<()> {
    match cmd {
        MonitorCommand::List { json } => list(json),
        MonitorCommand::Add(args) => {
            if args.kind.is_none()
                && let Err(error) = preset(&args.id)
            {
                clap::Error::raw(clap::error::ErrorKind::InvalidValue, error.to_string()).exit();
            }
            let m = args.to_config()?;
            if config::load()?.iter().any(|old| old.id == m.id) {
                bail!(
                    "a monitor with id '{}' already exists; pass --id {}-2",
                    m.id,
                    m.id
                );
            }
            config::add(&m)?;
            for name in [&m.api_key_env, &m.billing_key_env].into_iter().flatten() {
                if process_env(name).is_none() {
                    outln!(
                        "note: ${name} is not set in the environment or the store; run 'tollgate secret set {name}'"
                    );
                }
            }
            outln!(
                "added monitor '{}' ({}); the daemon polls it every {}s",
                m.id,
                m.kind.as_str(),
                m.ttl_ms() / 1000
            );
            Ok(())
        }
        MonitorCommand::Remove { id } => {
            config::validate_id(&id)?;
            if !config::remove(&id)? {
                bail!("no monitor with id '{id}'");
            }
            cache::remove(&id);
            outln!("removed monitor '{id}'");
            Ok(())
        }
        MonitorCommand::Detect {
            json,
            explain,
            apply,
            yes,
        } => super::detect::run(json, explain, apply, yes),
        MonitorCommand::Refresh { id, json, capture } => {
            refresh(id.as_deref(), json, capture.as_deref())
        }
    }
}

/// One `monitor list --json` row. Env var names and whether each is set —
/// never a value.
#[derive(Debug, Serialize)]
pub(crate) struct ListRow {
    #[serde(flatten)]
    pub(crate) config: MonitorConfig,
    pub(crate) api_key_env_set: Option<bool>,
    pub(crate) billing_key_env_set: Option<bool>,
    pub(crate) observation: AccountObservation,
}

/// The `list` rows at `now_ms`, with `env` answering whether a variable is set.
pub(crate) fn list_rows(
    monitors: Vec<MonitorConfig>,
    now_ms: u64,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<ListRow> {
    monitors
        .into_iter()
        .map(|m| {
            let c = cache::load(&m.id).filter(|c| c.matches(&m));
            // The monitor's own TTL, as the collector judges it: `list` and
            // `tollgate usage` must agree on what is stale.
            let observation = observe_monitor(&m, c.as_ref(), now_ms, |at| {
                freshness_at(at, now_ms, m.ttl_ms())
            });
            ListRow {
                api_key_env_set: m.api_key_env.as_deref().map(|n| env(n).is_some()),
                billing_key_env_set: m.billing_key_env.as_deref().map(|n| env(n).is_some()),
                config: m,
                observation,
            }
        })
        .collect()
}

/// Freshness against a cadence, the rule [`crate::usage::collect::CollectCtx::freshness_at_cadence`]
/// applies.
pub(crate) fn freshness_at(at: Option<u64>, now_ms: u64, interval_ms: u64) -> Freshness {
    match at {
        None => Freshness::NotFetched,
        Some(at) => match now_ms.checked_sub(at) {
            Some(age) if age <= crate::profile_json::stale_after_ms(interval_ms) => {
                Freshness::Fresh
            }
            _ => Freshness::Stale {
                since: Some(crate::usage::observation::Timestamp::from_ms(at)),
            },
        },
    }
}

fn list(json: bool) -> Result<()> {
    let monitors = config::load()?;
    let now = crate::usage::now_ms();
    let rows = list_rows(monitors, now, &process_env);
    if json {
        outln!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        outln!(
            "no monitors; add one with `tollgate monitor add <id> --kind <nous|openrouter|provider|ollama_cloud> …`"
        );
        return Ok(());
    }
    let now_secs = i64::try_from(now / 1000).unwrap_or(i64::MAX);
    for row in &rows {
        outln!("{}", list_line(row, now_secs));
    }
    Ok(())
}

/// One human line per monitor: the `usage` plain line, then what the
/// monitor is configured with. Not a contract.
pub(crate) fn list_line(row: &ListRow, now_secs: i64) -> String {
    let m = &row.config;
    let mut parts = vec![crate::usage::report::plain_line(&row.observation, now_secs)];
    parts.push(match &m.provider {
        Some(p) => format!("kind {}/{p}", m.kind.as_str()),
        None => format!("kind {}", m.kind.as_str()),
    });
    if !m.enabled {
        parts.push("disabled".to_string());
    }
    for (name, set) in [
        (&m.api_key_env, row.api_key_env_set),
        (&m.billing_key_env, row.billing_key_env_set),
    ] {
        if let (Some(name), Some(set)) = (name, set) {
            let status = if !set {
                "MISSING"
            } else if crate::secrets::stored_names()
                .iter()
                .any(|stored| stored == name)
                && std::env::var(name).ok().is_none_or(|v| v.trim().is_empty())
            {
                "set (store)"
            } else {
                "set (env)"
            };
            parts.push(format!("${name} {status}"));
        }
    }
    if let Some(b) = &m.budget_usd_month {
        parts.push(format!("budget ${b}/mo"));
    }
    if let Some(p) = m.alert_pct {
        parts.push(format!("alert at {p}%"));
    }
    if let Some(sev) = monitor_severity(&row.observation, now_secs) {
        parts.push(format!("monitor {}", sev.word(SeverityBasis::Usage)));
    }
    parts.join("  ")
}

fn refresh(id: Option<&str>, json: bool, capture: Option<&std::path::Path>) -> Result<()> {
    let monitors = config::load()?;
    let selected: Vec<MonitorConfig> = match id {
        Some(id) => {
            let Some(m) = monitors.into_iter().find(|m| m.id == id) else {
                bail!("no monitor with id '{id}'");
            };
            vec![m]
        }
        None => monitors.into_iter().filter(|m| m.enabled).collect(),
    };
    if selected.is_empty() {
        outln!("no enabled monitors to refresh");
        return Ok(());
    }
    let capture_http = super::capture::CaptureHttp::new(&LiveHttp, capture)?;
    let deps = RefreshDeps {
        http: &capture_http,
        // An interactive refresh also notifies: the operator asked for fresh
        // figures, and the de-dup keys keep the daemon from repeating it.
        notifier: Some(&DesktopNotifier),
        env: &process_env,
        now_ms: crate::usage::now_ms(),
    };
    let mut observations = Vec::new();
    for m in &selected {
        capture_http.set_id(&m.id);
        let outcome = cache::refresh_one(m, &deps, true)?;
        let note = match &outcome {
            RefreshOutcome::Busy => Some("busy: another process is refreshing it".to_string()),
            RefreshOutcome::Held(c) => Some(format!(
                "held after a 429 until {}",
                c.hold_until_ms
                    .map(|ms| crate::usage::observation::Timestamp::from_ms(ms).to_rfc3339())
                    .unwrap_or_default()
            )),
            RefreshOutcome::NotDue(_) | RefreshOutcome::Refreshed(_) => None,
        };
        let obs = observe_monitor(m, outcome.cache(), deps.now_ms, |at| {
            freshness_at(at, deps.now_ms, m.ttl_ms())
        });
        if !json {
            let now_secs = i64::try_from(deps.now_ms / 1000).unwrap_or(i64::MAX);
            let mut line = crate::usage::report::plain_line(&obs, now_secs);
            if let Some(n) = note {
                line.push_str(&format!("  ({n})"));
            }
            outln!("{line}");
        }
        observations.push(obs);
    }
    if json {
        let now_secs = i64::try_from(deps.now_ms / 1000).unwrap_or(i64::MAX);
        let report = crate::usage::report::UsageReport::new(
            observations,
            now_secs,
            crate::identity::upstream_active(),
        );
        outln!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_cli.rs"]
mod tests;

/// A preset is only non-secret configuration; explicit add flags override it.
pub(crate) fn preset(name: &str) -> Result<MonitorConfig> {
    let (kind, id, label) = match name {
        "grok" => (MonitorKind::Grok, "grok", "Grok (CLI login)"),
        "antigravity" | "agy" => (MonitorKind::Antigravity, "agy", "Antigravity (agy login)"),
        "codex-native" => (
            MonitorKind::CodexNative,
            "codex-native",
            "Codex (~/.codex login)",
        ),
        "openai" => (MonitorKind::Openai, "openai", "OpenAI API"),
        "google-ai" | "gemini" => (MonitorKind::GoogleAi, "google-ai", "Google AI Studio key"),
        "nous" | "hermes" => (MonitorKind::Nous, "nous", "Nous (Hermes login)"),
        "nous-key" => (MonitorKind::Nous, "nous-key", "Nous API key"),
        "openrouter" => (MonitorKind::OpenRouter, "openrouter", "OpenRouter"),
        _ => bail!(
            "tollgate: '{name}' is not a preset (grok, antigravity, codex-native, openai, google-ai, nous, nous-key, openrouter); pass --kind to add it by id"
        ),
    };
    let mut m = MonitorConfig {
        id: id.into(),
        kind,
        provider: None,
        label: Some(label.into()),
        api_key_env: None,
        billing_key_env: None,
        hermes_home: None,
        tool_home: None,
        auth_entry: None,
        via: None,
        probe: false,
        probe_model: None,
        budget_usd_month: None,
        alert_pct: None,
        ttl_secs: None,
        enabled: true,
    };
    match name {
        "grok" => {
            m.tool_home = Some("~/.grok".into());
            m.ttl_secs = Some(300);
        }
        "antigravity" | "agy" => {
            m.via = Some("keyring".into());
            m.ttl_secs = Some(600);
        }
        "codex-native" => {
            m.tool_home = Some("~/.codex".into());
            m.ttl_secs = Some(300);
        }
        "openai" => m.api_key_env = Some("OPENAI_API_KEY".into()),
        "google-ai" | "gemini" => m.api_key_env = Some("GEMINI_API_KEY".into()),
        "nous" | "hermes" => m.hermes_home = Some("~/.hermes".into()),
        "nous-key" => m.api_key_env = Some("NOUS_API_KEY".into()),
        "openrouter" => m.api_key_env = Some("OPENROUTER_API_KEY".into()),
        _ => {}
    }
    Ok(m)
}

fn validate_preset_or_id(value: &str) -> std::result::Result<String, String> {
    // Arbitrary valid ids remain accepted with --kind; the cross-field check
    // is performed at dispatch, where clap can produce the requested status.
    config::validate_id(value).map_err(|e| e.to_string())?;
    Ok(value.into())
}
