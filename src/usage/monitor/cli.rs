//! `tollgate monitor list | add | remove | refresh [id]`.
//!
//! `add` takes every setting as a flag and accepts env var NAMES only: there is
//! no flag that takes a key, and a value pasted into `--api-key-env` is refused
//! ([`super::config::validate_env_name`]), so a secret never reaches argv, shell
//! history or `monitors.toml`.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

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
    Add(MonitorAddArgs),
    /// Remove a monitor and its cache
    Remove {
        /// The monitor's id.
        id: String,
    },
    /// Fetch monitors now, ignoring their refresh interval
    ///
    /// Bare, every enabled monitor; with an id, that one (even if disabled).
    /// A monitor held after a 429 stays held.
    Refresh {
        /// Only this monitor.
        id: Option<String>,
        /// Emit the `tollgate usage --json` envelope of the refreshed monitors.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub(crate) struct MonitorAddArgs {
    /// The monitor's id: [a-z0-9][a-z0-9_-]*.
    pub(crate) id: String,
    /// What it reads.
    #[arg(long, value_enum)]
    pub(crate) kind: MonitorKind,
    /// With `--kind provider`: the typed provider (DeepSeek, Zai, MiniMax,
    /// OpenRouter).
    #[arg(long, value_name = "NAME")]
    pub(crate) provider: Option<String>,
    /// Human name.
    #[arg(long)]
    pub(crate) label: Option<String>,
    /// NAME of the env var holding the api key.
    #[arg(long, value_name = "VAR")]
    pub(crate) api_key_env: Option<String>,
    /// NAME of the env var holding a monitoring-only key (preferred for reads).
    #[arg(long, value_name = "VAR")]
    pub(crate) billing_key_env: Option<String>,
    /// With `--kind nous`: the Hermes home (default ~/.hermes).
    #[arg(long, value_name = "PATH")]
    pub(crate) hermes_home: Option<String>,
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
        let m = MonitorConfig {
            id: self.id.clone(),
            kind: self.kind,
            provider: self.provider.clone(),
            label: self.label.clone(),
            api_key_env: self.api_key_env.clone(),
            billing_key_env: self.billing_key_env.clone(),
            hermes_home: self.hermes_home.clone(),
            budget_usd_month: budget,
            alert_pct: self.alert_pct,
            ttl_secs: self.ttl_secs,
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
            let m = args.to_config()?;
            config::add(&m)?;
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
        MonitorCommand::Refresh { id, json } => refresh(id.as_deref(), json),
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
            parts.push(format!("${name} {}", if set { "set" } else { "MISSING" }));
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

fn refresh(id: Option<&str>, json: bool) -> Result<()> {
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
    let deps = RefreshDeps {
        http: &LiveHttp,
        // An interactive refresh also notifies: the operator asked for fresh
        // figures, and the de-dup keys keep the daemon from repeating it.
        notifier: Some(&DesktopNotifier),
        env: &process_env,
        now_ms: crate::usage::now_ms(),
    };
    let mut observations = Vec::new();
    for m in &selected {
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
