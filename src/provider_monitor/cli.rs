use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use crate::out::outln;

#[derive(Debug, Args)]
pub(crate) struct ProviderArgs {
    /// Emit cached plan, quota buckets, reset times and freshness as JSON.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Option<ProviderCommand>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProviderCommand {
    /// Read Codex and Grok limits. Does not change the active clauth account.
    Status,
    /// Create ~/.clauth/providers.toml without overwriting an existing file.
    Init,
    /// Print the documented TOML configuration template.
    Example,
    /// Refresh native provider usage, retaining last good data on failure.
    Refresh,
    /// Launch a configured tool/model with its existing native login.
    Start {
        target: String,
        /// Start in a sibling Herdr pane; requires HERDR_ENV=1.
        #[arg(long)]
        herdr: bool,
        /// Working directory; defaults to this directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Native CLI arguments after --, passed without a shell.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

pub(crate) fn run(args: ProviderArgs) -> Result<()> {
    match args.command {
        Some(ProviderCommand::Init) => {
            outln!("created {}", super::init_config()?.display());
            return Ok(());
        }
        Some(ProviderCommand::Example) => {
            outln!("{}", super::config::EXAMPLE);
            return Ok(());
        }
        Some(ProviderCommand::Status) => super::refresh(false)?,
        Some(ProviderCommand::Refresh) => super::refresh(true)?,
        Some(ProviderCommand::Start {
            target,
            herdr,
            cwd,
            args,
        }) => return start(&target, herdr, cwd, &args),
        None => {}
    }
    let reports = super::reports()?;
    if args.json {
        outln!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }
    for line in current_claude_lines() {
        outln!("{line}");
    }
    outln!("");
    if reports.is_empty() {
        outln!("No Codex, Grok or agy targets configured. Run `clauth providers init`.");
    }
    for report in reports {
        for line in report_lines(&report) {
            outln!("{line}");
        }
        outln!("");
    }
    Ok(())
}

/// The active Claude account as read from disk.
///
/// Read-only: the active marker and the usage cache are not rewritten, and
/// this does not select a different account.
pub(crate) struct ClaudeReading {
    pub(crate) account: Option<String>,
    pub(crate) plan: Option<String>,
    pub(crate) usage: Option<crate::usage::UsageInfo>,
}

pub(crate) fn current_claude_reading() -> ClaudeReading {
    let Some(name) = crate::profile::active_profile_name() else {
        return ClaudeReading {
            account: None,
            plan: None,
            usage: None,
        };
    };
    let usage = if crate::profile::stored_usage_cache_is_third_party(&name) {
        crate::profile_cache::load_profile_cache::<crate::providers::ThirdPartyStats>(
            &name,
            crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        )
        .and_then(|stats| stats.to_usage_info())
    } else {
        crate::profile_cache::load_profile_cache(&name, crate::profile_cache::USAGE_CACHE_FILE)
    };
    let profile =
        crate::profile::load_profile(&crate::profile::ProfileName::from(name.as_str())).ok();
    let login = profile
        .as_ref()
        .and_then(|profile| profile.credentials.as_ref())
        .and_then(|creds| creds.claude_ai_oauth.as_ref())
        .map(|oauth| {
            crate::usage::PlanTier::from_login(
                oauth.subscription_type.as_deref(),
                oauth.rate_limit_tier(),
            )
        })
        .unwrap_or_default();
    let fetched = usage
        .as_ref()
        .and_then(|usage| usage.plan.as_ref())
        .map(|plan| plan.tier.clone())
        .filter(|tier| *tier != crate::usage::PlanTier::Unknown);
    let plan = crate::usage::PlanTier::resolve(fetched, login)
        .and_then(|tier| tier.short_label())
        .or_else(|| {
            crate::profile::stored_provider(&name)
                .map(|provider| provider.display_name().to_string())
        });
    ClaudeReading {
        account: Some(name.to_string()),
        plan,
        usage,
    }
}

/// The active Claude account, in the same shape as a native provider report.
pub(crate) fn current_claude_lines() -> Vec<String> {
    let reading = current_claude_reading();
    claude_lines(
        reading.account.as_deref(),
        reading.plan.as_deref(),
        reading.usage.as_ref(),
    )
}

/// Format the current Claude account. `account` `None` means nothing is active.
/// Every window on `usage` is listed; a second window is not folded into the first.
pub(crate) fn claude_lines(
    account: Option<&str>,
    plan: Option<&str>,
    usage: Option<&crate::usage::UsageInfo>,
) -> Vec<String> {
    let Some(account) = account.filter(|name| !name.is_empty()) else {
        return vec!["claude · no active account".to_string()];
    };
    let state = match usage {
        None => "NotFetched",
        Some(usage) if usage.fetched_at.is_some() => "Fresh",
        Some(_) => "Stale",
    };
    let mut lines = vec![format!(
        "{account} · claude · {} · {state}",
        plan.unwrap_or("plan unknown")
    )];
    let Some(usage) = usage else {
        lines.push("  Quota unavailable (not unlimited)".to_string());
        return strip_controls(lines);
    };
    if usage.plan.as_ref().is_some_and(|plan| plan.is_canceled()) {
        lines.push("  subscription canceled".to_string());
    }
    let windows = usage.windows();
    if windows.is_empty() {
        lines.push("  Quota unavailable (not unlimited)".to_string());
    }
    for (label, window) in windows {
        let (remaining, exhausted) = remaining_percent(window.utilization);
        let remaining = remaining
            .map(|percent| format!("{percent:.1}% remaining"))
            .unwrap_or_else(|| "remaining unknown".to_string());
        let reset = window.resets_at.as_deref().unwrap_or("unknown");
        lines.push(format!(
            "  {label}: {remaining}{} · resets {reset}",
            if exhausted { " · exhausted" } else { "" }
        ));
    }
    strip_controls(lines)
}

/// `used` is a utilization percent. Out-of-range or non-finite input does not
/// become a remaining amount. `used >= 100` is exhausted, with nothing remaining.
pub(crate) fn remaining_percent(used: f64) -> (Option<f64>, bool) {
    if !used.is_finite() || used < 0.0 {
        return (None, false);
    }
    if used >= 100.0 {
        return (Some(0.0), true);
    }
    (Some(100.0 - used), false)
}

fn strip_controls(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| line.chars().filter(|c| !c.is_control()).collect())
        .collect()
}

pub(crate) fn report_lines(report: &super::ProviderReport) -> Vec<String> {
    let mut lines = vec![format!(
        "{} · {} · {} · {:?}{}",
        report.id,
        report.tool,
        report.data.plan.as_deref().unwrap_or("plan unknown"),
        report.state,
        if report.warning {
            " · ACCOUNT QUOTA WARNING"
        } else {
            ""
        }
    )];
    if report.identity_checked_at_observation_only {
        lines.push("  Login identity checked at observation only (native keyring)".into());
    }
    if let Some(message) = &report.message {
        lines.push(format!("  {message}"));
    }
    if report.data.buckets.is_empty() {
        lines.push("  Quota unavailable (not unlimited)".into());
    }
    for bucket in &report.data.buckets {
        let remaining = bucket
            .remaining_percent
            .map(|p| format!("{p:.1}% remaining"))
            .unwrap_or_else(|| "remaining unknown".into());
        let reset = bucket.resets_at.as_deref().unwrap_or("unknown");
        lines.push(format!(
            "  {}: {}{} · resets {}",
            bucket.label,
            remaining,
            if bucket.exhausted {
                " · exhausted"
            } else {
                ""
            },
            reset
        ));
    }
    for credit in &report.data.credits {
        lines.push(format!(
            "  {}: {} {}",
            credit.label, credit.remaining, credit.unit
        ));
    }
    // Provider labels are untrusted terminal input. Keep the structured JSON
    // unchanged, but never print escape/control sequences to an operator's TTY.
    lines
        .into_iter()
        .map(|line| line.chars().filter(|c| !c.is_control()).collect())
        .collect()
}

fn launch_args(target: &super::config::TargetConfig, extra: &[String]) -> Vec<String> {
    let mut args = target.args.clone();
    if let Some(model) = &target.model {
        args.extend(["--model".into(), model.clone()]);
    }
    args.extend_from_slice(extra);
    args
}

fn start(id: &str, herdr: bool, cwd: Option<PathBuf>, extra: &[String]) -> Result<()> {
    let config = super::config::load()?;
    let target = config
        .targets
        .iter()
        .find(|t| t.id == id)
        .context("provider target not found in providers.toml")?;
    if !target.enabled {
        bail!("provider target is disabled");
    }
    // A custom monitored store is not necessarily the store the official CLI
    // would use. Refuse to label a launch as this account without that binding.
    if target.auth_file.is_some() || target.auth_entry.is_some() {
        bail!(
            "custom auth_file/auth_entry targets are monitoring-only; launch with the official tool's account selection"
        );
    }
    let cwd = cwd
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()
        .context("launch directory is unavailable")?;
    if !cwd.is_dir() {
        bail!("launch directory is not a directory");
    }
    let args = launch_args(target, extra);
    if herdr {
        if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
            bail!("--herdr requires a Herdr-managed pane");
        }
        if target.command.is_some() {
            bail!(
                "--herdr uses Herdr's native tool lookup; command overrides are direct-launch only"
            );
        }
        let output = Command::new("herdr")
            .args([
                "pane",
                "split",
                "--current",
                "--direction",
                "right",
                "--no-focus",
                "--cwd",
            ])
            .arg(&cwd)
            .output()
            .context("cannot reach Herdr")?;
        if !output.status.success() {
            bail!("Herdr could not create the provider pane");
        }
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let pane = value
            .pointer("/result/pane/pane_id")
            .and_then(|v| v.as_str())
            .context("Herdr returned no pane ID")?;
        let name = format!("clauth-{}", std::process::id());
        let result = Command::new("herdr")
            .args([
                "agent",
                "start",
                &name,
                "--kind",
                target.provider.tool(),
                "--pane",
                pane,
                "--",
            ])
            .args(args)
            .status()?;
        if !result.success() {
            bail!("Herdr agent startup did not complete; inspect pane {pane}");
        }
        outln!("started {} in {pane}", target.id);
    } else {
        let binary = match &target.command {
            Some(p) => super::config::expand(p)?,
            None => PathBuf::from(target.provider.tool()),
        };
        let result = Command::new(binary)
            .args(args)
            .current_dir(cwd)
            .status()
            .context("could not launch the provider tool")?;
        if !result.success() {
            bail!("provider tool exited with {result}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_passes_model_and_args_literally() {
        let mut cfg = super::super::config::parse(super::super::config::EXAMPLE).unwrap();
        let target = &mut cfg.targets[1];
        target.model = Some("model-a".into());
        target.args = vec!["--permission-mode".into(), "plan".into()];
        assert_eq!(
            launch_args(target, &["a; $(not-a-shell)".into()]),
            [
                "--permission-mode",
                "plan",
                "--model",
                "model-a",
                "a; $(not-a-shell)"
            ]
        );
    }

    #[test]
    fn claude_lines_keep_every_window_of_the_active_account() {
        use crate::usage::{PlanInfo, PlanTier, ScopedWindow, UsageInfo, UsageWindow};
        let usage = UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Max(Some(20)),
                ..PlanInfo::default()
            }),
            five_hour: Some(UsageWindow {
                utilization: 20.0,
                resets_at: Some("2026-09-22T01:00:00Z".into()),
            }),
            seven_day: Some(UsageWindow {
                utilization: 70.0,
                resets_at: Some("2026-09-28T00:00:00Z".into()),
            }),
            weekly_scoped: vec![ScopedWindow {
                label: "7d opus".into(),
                window: UsageWindow {
                    utilization: 10.0,
                    resets_at: None,
                },
            }],
            fetched_at: Some(1),
            ..UsageInfo::default()
        };
        let lines = claude_lines(Some("beta"), Some("Max 20x"), Some(&usage));
        let text = lines.join("\n");
        assert!(text.contains("beta · claude · Max 20x · Fresh"), "{text}");
        assert!(text.contains("5h: 80.0% remaining"), "{text}");
        assert!(text.contains("7d: 30.0% remaining"), "{text}");
        assert!(text.contains("7d opus: 90.0% remaining"), "{text}");
        assert_ne!(
            lines
                .iter()
                .find(|line| line.contains("5h:"))
                .map(String::as_str),
            lines
                .iter()
                .find(|line| line.contains("7d:"))
                .map(String::as_str)
        );
    }

    #[test]
    fn claude_lines_name_a_missing_active_account_and_an_unread_quota() {
        assert_eq!(
            claude_lines(None, None, None),
            ["claude · no active account"]
        );
        let lines = claude_lines(Some("beta\u{1b}[2J"), None, None);
        assert!(lines.iter().all(|line| !line.chars().any(char::is_control)));
        assert!(lines[0].contains("NotFetched"));
        assert!(lines[1].contains("Quota unavailable"));
    }

    #[test]
    fn current_claude_lines_read_the_active_account_cache() {
        let _home = crate::testutil::HomeSandbox::new();
        let name = crate::profile::ProfileName::from("beta");
        let mut state = crate::profile::AppState::default();
        state.profiles.push(name.clone());
        state.active_profile = Some(name.clone());
        crate::profile::save_app_state(&state).expect("save profiles");
        let dir = crate::profile::profile_dir(&name).expect("profile dir");
        crate::profile::mkdir_700(&dir).expect("mkdir profile");
        let usage = crate::usage::UsageInfo {
            five_hour: Some(crate::usage::UsageWindow {
                utilization: 25.0,
                resets_at: None,
            }),
            fetched_at: Some(5),
            ..crate::usage::UsageInfo::default()
        };
        crate::profile_cache::write_profile_cache(
            &name,
            crate::profile_cache::USAGE_CACHE_FILE,
            &usage,
        );
        let lines = current_claude_lines();
        let text = lines.join("\n");
        assert!(text.contains("beta · claude"), "{text}");
        assert!(text.contains("5h: 75.0% remaining"), "{text}");
    }

    #[test]
    fn provider_labels_cannot_emit_terminal_controls() {
        let target = super::super::config::parse(super::super::config::EXAMPLE)
            .unwrap()
            .targets
            .remove(0);
        let mut report = super::super::empty_report(&target);
        report.data.plan = Some("hostile\u{1b}]52;c;clipboard\u{7}\nplan".into());
        let lines = report_lines(&report);
        assert!(lines.iter().all(|line| !line.chars().any(char::is_control)));
        assert!(report.data.plan.as_ref().unwrap().contains('\u{1b}'));
    }
}
