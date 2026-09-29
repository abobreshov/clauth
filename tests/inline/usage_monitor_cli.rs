#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `tollgate monitor …`: the grammar (no flag takes a secret), `add`'s
//! validation, and `list`'s rows (env var names and set/missing, never a
//! value).

use clap::{CommandFactory as _, Parser as _};

use super::*;
use crate::cli::{Cli, Command};
use crate::testutil::HomeSandbox;

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("tollgate").chain(args.iter().copied()))
}

fn add_args(args: &[&str]) -> MonitorAddArgs {
    let mut full = vec!["monitor", "add"];
    full.extend_from_slice(args);
    match parse(&full).unwrap().command {
        Some(Command::Monitor {
            cmd: Some(MonitorCommand::Add(a)),
            ..
        }) => a,
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_verb_parses() {
    for args in [
        &["monitor"][..],
        &["monitor", "--json"],
        &["monitor", "list"],
        &["monitor", "list", "--json"],
        &["monitor", "remove", "nous"],
        &["monitor", "refresh"],
        &["monitor", "refresh", "nous", "--json"],
        &["monitor", "add", "nous", "--kind", "nous"],
        &[
            "monitor",
            "add",
            "oc",
            "--kind",
            "ollama_cloud",
            "--api-key-env",
            "OLLAMA_API_KEY",
        ],
    ] {
        parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    assert!(parse(&["monitor", "add", "x", "--kind", "bogus"]).is_err());
}

#[test]
fn no_add_flag_can_carry_a_secret_value() {
    let root = Cli::command();
    let add = root
        .find_subcommand("monitor")
        .and_then(|m| m.find_subcommand("add"))
        .unwrap();
    for arg in add.get_arguments() {
        let long = arg.get_long().unwrap_or_default();
        assert!(
            !matches!(long, "api-key" | "key" | "token" | "secret" | "billing-key"),
            "`--{long}` would put a secret on argv"
        );
    }
}

#[test]
fn add_flags_build_a_validated_monitor() {
    let m = add_args(&[
        "or",
        "--kind",
        "openrouter",
        "--api-key-env",
        "OPENROUTER_API_KEY",
        "--budget-usd-month",
        "$25.50",
        "--alert-pct",
        "80",
        "--disabled",
    ])
    .to_config()
    .unwrap();
    assert_eq!(m.kind, MonitorKind::OpenRouter);
    assert_eq!(m.budget_usd_month.unwrap().as_str(), "25.50");
    assert!(!m.enabled);
}

#[test]
fn a_key_value_pasted_as_a_name_is_refused() {
    let err = add_args(&[
        "or",
        "--kind",
        "openrouter",
        "--api-key-env",
        "sk-or-v1-0123456789abcdef0123",
    ])
    .to_config()
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("NAME"), "{msg}");
    assert!(
        !msg.contains("0123456789abcdef"),
        "the value is not echoed: {msg}"
    );
}

#[test]
fn list_rows_say_whether_a_variable_is_set_without_its_value() {
    let _home = HomeSandbox::new();
    let mut or = MonitorConfig::new("or", MonitorKind::OpenRouter);
    or.api_key_env = Some("OR_SET".into());
    or.billing_key_env = Some("OR_MISSING".into());
    let env = |n: &str| (n == "OR_SET").then(|| "sk-or-secret-value".to_string());
    let rows = list_rows(vec![or], crate::usage::now_ms(), &env);
    assert_eq!(rows[0].api_key_env_set, Some(true));
    assert_eq!(rows[0].billing_key_env_set, Some(false));
    let json = serde_json::to_string(&rows).unwrap();
    assert!(!json.contains("sk-or-secret-value"), "{json}");
    assert!(json.contains("\"api_key_env\":\"OR_SET\""), "{json}");
    let line = list_line(&rows[0], crate::usage::now_epoch_secs());
    assert!(line.contains("monitor:or"), "{line}");
    assert!(line.contains("$OR_SET set"), "{line}");
    assert!(line.contains("$OR_MISSING MISSING"), "{line}");
    assert!(!line.contains("sk-or-secret-value"));
}

#[test]
fn add_then_remove_through_the_dispatch() {
    let _home = HomeSandbox::new();
    run(MonitorCommand::Add(add_args(&["nous", "--kind", "nous"]))).unwrap();
    assert_eq!(config::load().unwrap().len(), 1);
    assert!(run(MonitorCommand::Add(add_args(&["nous", "--kind", "nous"]))).is_err());
    run(MonitorCommand::Remove { id: "nous".into() }).unwrap();
    assert!(config::load().unwrap().is_empty());
    assert!(run(MonitorCommand::Remove { id: "nous".into() }).is_err());
}

/// `monitor list` judges a reading against the monitor's own TTL, the rule
/// the collector (`tollgate usage`, the agent API) applies, so the two never
/// disagree on what is stale.
#[test]
fn list_rows_judge_freshness_by_the_monitors_own_ttl() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();
    let observed = now - 30 * 60_000;
    let mut slow = MonitorConfig::new("slow", MonitorKind::Nous);
    slow.ttl_secs = Some(3600);
    let fast = MonitorConfig::new("fast", MonitorKind::Nous);
    let dir = config::monitors_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    for m in [&slow, &fast] {
        let c = cache::MonitorCache {
            version: cache::CACHE_VERSION,
            id: m.id.clone(),
            fingerprint: m.fingerprint(),
            checked_at_ms: Some(observed),
            observed_at_ms: Some(observed),
            reading: Some(crate::usage::monitor::source::Reading::default()),
            failure: None,
            hold_until_ms: None,
            alerts: Default::default(),
        };
        std::fs::write(
            cache::cache_path(&m.id).unwrap(),
            serde_json::to_vec(&c).unwrap(),
        )
        .unwrap();
    }
    let rows = list_rows(vec![slow, fast], now, &|_| None);
    assert_eq!(rows[0].observation.freshness, Freshness::Fresh, "slow");
    assert!(
        matches!(rows[1].observation.freshness, Freshness::Stale { .. }),
        "fast: {:?}",
        rows[1].observation.freshness
    );
}
