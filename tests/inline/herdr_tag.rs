#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `herdr::tag`: the `$tollgate` pane tag built from observations — the text
//! for each observation kind (session / weekly / monthly share, balance, cap,
//! spend), the flags (stale, HIGH, CRITICAL, failure), the severity line, the
//! 80-char cap, profile lookup per harness, and the native-pane match that
//! answers nothing when ambiguous. Pure over hand-built observations and the
//! recorded 0.9.x pane-list shape; nothing here reads a cache or a network.

use super::*;
use crate::cli::{Cli, Command, HerdrCommand};
use crate::usage::observation::{
    AuthKind, Failure, FailureKind, MoneyScope, Period, Timestamp, WINDOW_SESSION, WindowScope,
};
use clap::{CommandFactory as _, Parser as _};

const NOW: i64 = 1_790_000_000;

/// herdr 0.9.x `pane list` with one pane per agent kind the tag cares about
/// (claude, codex, hermes, grok, agy) plus an idle shell. Hand-assembled on
/// the recorded `pane-list.json` shape (sorted keys, `agent` absent on an
/// idle pane, `tokens` carrying the tag and its severity).
const PANE_LIST_09: &str = include_str!("../fixtures/herdr/pane-list-0.9-native.json");

fn obs(origin: Origin, name: &str, source: SourceId) -> AccountObservation {
    let mut o = AccountObservation::new(
        account_id(origin, name),
        source,
        AuthKind::Subscription,
        origin,
        name,
    );
    o.freshness = Freshness::Fresh;
    o
}

fn window(id: &str, used: f64) -> QuotaWindow {
    let mut w = QuotaWindow::new(id, id, WindowScope::Account);
    w.used_pct = Some(used);
    w
}

fn meter(kind: MoneyKind, amount: &str) -> MoneyMeter {
    MoneyMeter::new(
        "m",
        "m",
        kind,
        crate::usage::observation::Amount::parse(amount).unwrap(),
        "usd",
        MoneyScope::Key,
    )
}

// ── text per observation kind ────────────────────────────────────────────────

#[test]
fn a_session_share_reads_as_the_bare_percent() {
    let mut o = obs(Origin::Profile, "leadtone", SourceId::AnthropicOauth);
    o.windows.push(window(WINDOW_SESSION, 2.4));
    o.windows.push(window(WINDOW_WEEKLY, 40.0));
    let tag = format_tag("leadtone", &o, NOW);
    assert_eq!(
        tag.text, "leadtone 2%",
        "the lead (session) window, rounded"
    );
    assert_eq!(tag.severity, Some(Severity::Ok));
    assert_eq!(tag_lines(&tag), ["leadtone 2%", "ok"]);
}

#[test]
fn weekly_and_monthly_leads_carry_their_window_suffix() {
    let mut weekly = obs(Origin::CodexProfile, "cx-work", SourceId::Codex);
    weekly.windows.push(window(WINDOW_WEEKLY, 23.0));
    assert_eq!(format_tag("cx-work", &weekly, NOW).text, "cx-work 23%w");

    let mut scoped = obs(Origin::Profile, "opus-only", SourceId::AnthropicOauth);
    scoped.windows.push(window("weekly:opus", 11.0));
    assert_eq!(format_tag("opus-only", &scoped, NOW).text, "opus-only 11%w");

    let mut monthly = obs(Origin::Monitor, "nous-main", SourceId::Nous);
    monthly.windows.push(window(WINDOW_MONTH, 64.0));
    let tag = format_tag("nous-main", &monthly, NOW);
    assert_eq!(tag.text, "nous-main 64% mo");
    assert_eq!(tag.severity, Some(Severity::Mid), "64% used is mid");
}

#[test]
fn a_balance_account_reads_as_money() {
    let mut o = obs(Origin::Profile, "or-main", SourceId::OpenRouter);
    o.money.push(meter(MoneyKind::Spend, "4.08"));
    o.money.push(meter(MoneyKind::Balance, "13.67"));
    let tag = format_tag("or-main", &o, NOW);
    assert_eq!(
        tag.text, "or-main $13.67",
        "a balance outranks a spend meter"
    );
    assert_eq!(
        tag.severity,
        Some(Severity::Mid),
        "$13.67 is under the $20 rung"
    );
}

#[test]
fn a_cap_reads_as_what_is_left_and_spend_names_its_period() {
    let mut capped = obs(Origin::Profile, "or-alt", SourceId::OpenRouter);
    let mut cap = meter(MoneyKind::Limit, "9");
    cap.limit = Some(crate::usage::observation::Amount::parse("50").unwrap());
    capped.money.push(cap);
    let tag = format_tag("or-alt", &capped, NOW);
    assert_eq!(
        tag.text, "or-alt $9.00 left ⚠",
        "82% of the cap used is HIGH"
    );
    assert_eq!(tag.severity, Some(Severity::High));

    let mut spend = obs(Origin::Profile, "ds", SourceId::DeepSeek);
    let mut m = meter(MoneyKind::Spend, "4.08");
    m.period = Some(Period::of(PeriodKind::Monthly));
    spend.money.push(m);
    let tag = format_tag("ds", &spend, NOW);
    assert_eq!(tag.text, "ds $4.08/mo");
    assert_eq!(tag.severity, None, "spend is never graded");
    assert_eq!(tag_lines(&tag), ["ds $4.08/mo"], "no severity line");
}

#[test]
fn a_window_burning_ahead_of_its_clock_warns() {
    // 34% used with 30% of the 5h window elapsed: 4 pts ahead is HIGH (pace).
    let mut o = obs(Origin::Profile, "zai-work", SourceId::Zai);
    let mut w = window(WINDOW_SESSION, 34.0);
    w.window_secs = Some(18_000);
    w.resets_at = Some(Timestamp::from_secs(NOW + 12_600));
    o.windows.push(w);
    let tag = format_tag("zai-work", &o, NOW);
    assert_eq!(tag.text, "zai-work 34% ⚠");
    assert_eq!(tag_lines(&tag), ["zai-work 34% ⚠", "high"]);
}

#[test]
fn an_exhausted_window_is_critical_and_stale_figures_pause() {
    let mut full = obs(Origin::Profile, "maxed", SourceId::AnthropicOauth);
    let mut w = QuotaWindow::new(WINDOW_WEEKLY, "7d", WindowScope::Account);
    w.exhausted = true;
    full.windows.push(w);
    let tag = format_tag("maxed", &full, NOW);
    assert_eq!(tag.text, "maxed 100%w ‼");
    assert_eq!(tag.severity, Some(Severity::Critical));

    let mut stale = obs(Origin::Monitor, "oll-main", SourceId::OllamaCloud);
    stale.freshness = Freshness::Stale { since: None };
    stale.windows.push(window(WINDOW_WEEKLY, 23.0));
    assert_eq!(format_tag("oll-main", &stale, NOW).text, "oll-main 23%w ⏸");
}

#[test]
fn a_failure_warns_and_an_unread_account_shows_its_name_alone() {
    let mut failed = obs(Origin::Profile, "gone", SourceId::AnthropicOauth);
    failed.failure = Some(Failure::new(FailureKind::AuthRequired, "login expired"));
    let tag = format_tag("gone", &failed, NOW);
    assert_eq!(tag.text, "gone ⚠");
    assert_eq!(tag.severity, None, "a failure alone grades nothing");

    let mut unread = obs(Origin::Profile, "fresh", SourceId::AnthropicOauth);
    unread.freshness = Freshness::NotFetched;
    assert_eq!(format_tag("fresh", &unread, NOW).text, "fresh");
}

#[test]
fn the_tag_fits_herdrs_token_cap_and_one_line() {
    let long = "x".repeat(200);
    let mut o = obs(Origin::Profile, &long, SourceId::AnthropicOauth);
    o.windows.push(window(WINDOW_SESSION, 95.0));
    let tag = format_tag(&long, &o, NOW);
    assert_eq!(tag.text.chars().count(), MAX_TAG_CHARS);
    assert!(
        tag.text.ends_with(" 95% ‼"),
        "the label gives way, the numbers stay: {}",
        tag.text
    );

    let tag = format_tag("two\nlines\t", &o, NOW);
    assert_eq!(tag.text, "twolines 95% ‼", "control characters are dropped");
}

// ── resolution ───────────────────────────────────────────────────────────────

fn roster() -> Vec<AccountObservation> {
    let mut claude = obs(Origin::Profile, "work", SourceId::AnthropicOauth);
    claude.windows.push(window(WINDOW_SESSION, 12.0));
    let mut codex = obs(Origin::CodexProfile, "work", SourceId::Codex);
    codex.windows.push(window(WINDOW_WEEKLY, 70.0));
    let mut hermes = obs(Origin::Monitor, "herm", SourceId::Hermes);
    hermes.windows.push(window(WINDOW_SESSION, 5.0));
    let grok_a = obs(Origin::Monitor, "grok-a", SourceId::Grok);
    let grok_b = obs(Origin::Monitor, "grok-b", SourceId::Grok);
    let mut agy_off = obs(Origin::Monitor, "agy-old", SourceId::Antigravity);
    agy_off.disabled = true;
    vec![claude, codex, hermes, grok_a, grok_b, agy_off]
}

#[test]
fn a_profile_resolves_in_its_harnesss_roster() {
    let all = roster();
    let claude = resolve_tag(&all, Some("work"), "claude", NOW).unwrap();
    assert_eq!(claude.text, "work 12%");
    let codex = resolve_tag(&all, Some("work"), "codex", NOW).unwrap();
    assert_eq!(
        codex.text, "work 70%w",
        "a codex pane reads the codex roster"
    );
}

#[test]
fn a_profile_with_no_observation_keeps_its_bare_name() {
    let tag = resolve_tag(&roster(), Some("ghost"), "claude", NOW).unwrap();
    assert_eq!(tag.text, "ghost");
    assert_eq!(
        tag_lines(&tag),
        ["ghost"],
        "no severity: the script clears it"
    );
}

#[test]
fn a_native_pane_is_tagged_only_when_one_account_can_be_it() {
    let all = roster();
    let hermes = resolve_tag(&all, None, "hermes", NOW).unwrap();
    assert_eq!(hermes.text, "herm 5%", "one Hermes account: tagged with it");
    assert_eq!(
        resolve_tag(&all, None, "grok", NOW),
        None,
        "two grok accounts: ambiguous, no tag"
    );
    assert_eq!(
        resolve_tag(&all, None, "agy", NOW),
        None,
        "the only Antigravity account is disabled: no candidate"
    );
    for agent in ["claude", "codex", "cursor", ""] {
        assert_eq!(
            resolve_tag(&all, None, agent, NOW),
            None,
            "{agent:?} with no profile gets no tag from the binary"
        );
    }
}

/// A hermes pane is Hermes' own account: a Nous monitor reading Hermes'
/// login tags it, a Nous API-key monitor never does (Hermes may not call
/// that key at all).
#[test]
fn a_hermes_pane_matches_a_nous_monitor_only_on_hermes_login() {
    let mut login = obs(Origin::Monitor, "nous-main", SourceId::Nous);
    login.auth = AuthKind::NativeLogin;
    login.windows.push(window("subscription", 64.0));
    let mut keyed = obs(Origin::Monitor, "nous-key", SourceId::Nous);
    keyed.auth = AuthKind::ApiKey;

    let tag = resolve_tag(&[login.clone(), keyed.clone()], None, "hermes", NOW).unwrap();
    assert!(tag.text.starts_with("nous-main "), "{}", tag.text);
    assert_eq!(resolve_tag(&[keyed], None, "hermes", NOW), None);
    let hermes = obs(Origin::Monitor, "herm", SourceId::Hermes);
    assert_eq!(
        resolve_tag(&[login, hermes], None, "hermes", NOW),
        None,
        "Hermes state and its Nous login: two candidates, ambiguous"
    );
}

#[test]
fn a_tag_carries_no_account_id() {
    for o in roster() {
        let tag = format_tag(&o.label, &o, NOW);
        assert!(
            !tag.text.contains(':'),
            "only the name and numbers ride in pane metadata: {}",
            tag.text
        );
    }
}

#[test]
fn herdrs_agent_ids_map_to_their_rosters() {
    assert_eq!(pane_agent("claude"), PaneAgent::Claude);
    assert_eq!(pane_agent("codex"), PaneAgent::Codex);
    assert_eq!(
        pane_agent("hermes"),
        PaneAgent::Native(&[SourceId::Hermes, SourceId::Nous])
    );
    assert_eq!(pane_agent("grok"), PaneAgent::Native(&[SourceId::Grok]));
    assert_eq!(
        pane_agent("agy"),
        PaneAgent::Native(&[SourceId::Antigravity])
    );
    for other in ["cursor", "pi", "hermes-agent", "Claude", ""] {
        assert_eq!(pane_agent(other), PaneAgent::Other, "{other:?}");
    }
}

/// The recorded 0.9.x pane list: every pane's `agent` reads, and the native
/// match over it tags exactly the unambiguous pane.
#[test]
fn the_09_pane_list_tags_only_the_unambiguous_native_pane() {
    let panes = crate::herdr::parse_pane_list(PANE_LIST_09.as_bytes()).expect("0.9.x parses");
    let agents: Vec<Option<&str>> = panes.iter().map(|p| p.agent.as_deref()).collect();
    assert_eq!(
        agents,
        [
            Some("claude"),
            Some("codex"),
            Some("hermes"),
            Some("grok"),
            Some("agy"),
            None
        ]
    );
    assert_eq!(
        panes[0].tokens.as_ref().and_then(|t| t.tollgate.as_deref()),
        Some("leadtone 2%"),
        "the published tag reads back off the pane"
    );

    let all = roster();
    let tagged: Vec<(String, Option<String>)> = panes
        .iter()
        .map(|p| {
            let agent = p.agent.as_deref().unwrap_or("");
            (
                p.pane_id.clone(),
                resolve_tag(&all, None, agent, NOW).map(|t| t.text),
            )
        })
        .collect();
    let with_tag: Vec<&(String, Option<String>)> =
        tagged.iter().filter(|(_, t)| t.is_some()).collect();
    assert_eq!(
        with_tag,
        [&("w2:p3".to_string(), Some("herm 5%".to_string()))],
        "only the hermes pane has exactly one candidate account"
    );
}

// ── CLI surface ──────────────────────────────────────────────────────────────

#[test]
fn herdr_tag_parses_both_script_shapes_and_stays_out_of_help() {
    let parse = |args: &[&str]| match Cli::try_parse_from(args).expect("parses").command {
        Some(Command::Herdr {
            cmd: HerdrCommand::Tag { agent, profile },
        }) => (agent, profile),
        other => panic!("not the tag arm: {other:?}"),
    };
    assert_eq!(
        parse(&["tollgate", "herdr", "tag", "--agent", "claude", "--", "fit"]),
        (Some("claude".to_string()), Some("fit".to_string()))
    );
    assert_eq!(
        parse(&["tollgate", "herdr", "tag", "--agent", "hermes"]),
        (Some("hermes".to_string()), None)
    );
    assert_eq!(
        parse(&[
            "tollgate", "herdr", "tag", "--agent", "codex", "--", "-dash"
        ]),
        (Some("codex".to_string()), Some("-dash".to_string())),
        "the `--` the script passes keeps a dash-leading name positional"
    );

    let help = Cli::command()
        .find_subcommand_mut("herdr")
        .expect("herdr subcommand")
        .render_long_help()
        .to_string();
    assert!(
        !help.contains("tag text"),
        "the scripts' read path is hidden"
    );
    assert!(help.contains("link"), "link is a human surface");
    assert!(help.contains("unlink"));
}

#[test]
fn the_tab_flag_parses_a_home_tab_and_refuses_anything_else() {
    let cli = Cli::try_parse_from(["tollgate", "--tab", "usage"]).expect("parses");
    assert_eq!(cli.tab, Some(crate::profile::HomeTab::Usage));
    assert!(cli.command.is_none(), "the TUI path");
    let err = Cli::try_parse_from(["tollgate", "--tab", "graphs"]).expect_err("unknown tab");
    assert!(err.to_string().contains("valid tabs: overview, usage"));
}

/// The shipped manifest's `usage` action opens the `usage` entrypoint, whose
/// command is a `tollgate` argv that parses to the Usage tab — the action
/// lands on the usage view.
#[test]
fn the_manifests_usage_action_opens_the_usage_tab() {
    let manifest: toml::Value = toml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/herdr-plugin/herdr-plugin.toml"
    )))
    .expect("manifest parses");
    assert_eq!(manifest["id"].as_str(), Some("tollgate"));
    let action = manifest["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"].as_str() == Some("usage"))
        .expect("a usage action");
    let argv: Vec<&str> = action["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(argv, ["sh", "open-pane.sh", "usage"]);

    let pane = manifest["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_str() == Some("usage"))
        .expect("a usage entrypoint");
    let argv: Vec<&str> = pane["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(argv[0], crate::identity::NAME);
    let cli = Cli::try_parse_from(&argv).expect("the entrypoint argv parses");
    assert_eq!(cli.tab, Some(crate::profile::HomeTab::Usage));
    assert!(cli.command.is_none());
}
