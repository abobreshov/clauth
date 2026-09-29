//! `tollgate herdr tag`: the text of a pane's `$tollgate` sidebar token, built
//! from the observation core (plan v3.1 §4.6 H1, H2).
//!
//! The herdr plugin's `report-profile.sh` resolves WHICH account a pane burns
//! (the live-session join, the adopted codex login, `tollgate which`) and asks
//! this command what to SHOW for it, so the script stays a thin shell over the
//! binary. The answer is at most two lines on stdout:
//!
//! 1. the tag text: the account's name and its lead metric, plus flags —
//!    `leadtone 2%`, `or-main $13.67`, `zai-work 34% ⚠`, `oll-main 23%w ⏸`;
//! 2. the severity class (`ok`, `mid`, `high`, `critical`), which the script
//!    publishes as the `tollgate_severity` token so a sidebar rule can colour
//!    the row. Absent when nothing is graded.
//!
//! An account tollgate knows by name but holds no observation for prints its
//! name alone (the tag the plugin showed before observations existed). A
//! native pane (hermes, grok, agy — harnesses tollgate does not launch) is
//! matched to a monitor observation by source only when exactly one account
//! can be the one it burns; otherwise the command prints nothing and the
//! script clears the tag. A wrong account on a pane is worse than none.
//!
//! Tags carry only a profile or monitor label and numbers (H1): other herdr
//! clients read pane metadata, so no id, email or key fragment rides along.
//! Reads caches only, through [`crate::usage::collect::collect`]: no network,
//! and nothing beyond what every `load_config` entry point already ensures.

use anyhow::Result;

use crate::out::outln;
use crate::usage::collect::{CollectOpts, collect};
use crate::usage::derive::{Severity, account_severity, format_money, lead_window};
use crate::usage::observation::{
    AccountObservation, AuthKind, Freshness, MoneyKind, MoneyMeter, Origin, PeriodKind,
    QuotaWindow, SourceId, WINDOW_MONTH, WINDOW_WEEKLY, WINDOW_WEEKLY_MODEL_PREFIX, account_id,
};

/// herdr caps a metadata token value at 80 characters (herdr 0.9.1 CLI
/// reference, `pane report-metadata`); a longer one is refused whole.
pub(crate) const MAX_TAG_CHARS: usize = 80;

/// The flag a stale observation's tag carries (plan §4.5's `⏸`).
pub(crate) const STALE_FLAG: &str = "⏸";
/// The flag a HIGH-severity (or failed) observation's tag carries.
pub(crate) const HIGH_FLAG: &str = "⚠";
/// The flag a CRITICAL observation's tag carries.
pub(crate) const CRITICAL_FLAG: &str = "‼";

/// How the pane's agent (herdr's canonical agent id) relates to tollgate's
/// accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneAgent {
    /// Claude Code: a `profiles.toml` profile (`claude:<name>`).
    Claude,
    /// codex: a `codex-profiles.toml` profile (`codex:<name>`).
    Codex,
    /// A harness tollgate does not launch, matched to an observation by its
    /// source alone.
    Native(&'static [SourceId]),
    /// Anything else spends no tollgate account.
    Other,
}

/// herdr's canonical agent ids (herdr 0.9.1 config reference: `claude`,
/// `codex`, `hermes`, `grok`, `agy`, …) mapped to the accounts they can burn.
///
/// A native agent maps only to the sources that ARE that harness's own
/// account, never to a provider it may be configured to call: a Hermes pane
/// maps to Hermes' local state and to a Nous monitor that reads Hermes' own
/// login ([`AuthKind::NativeLogin`], see [`native_match`]), not to every Nous
/// or OpenRouter key, since which of those it calls is Hermes' own config
/// (H2's `HERMES_HOME` join is what can narrow further, once Hermes profiles
/// exist).
pub(crate) fn pane_agent(agent: &str) -> PaneAgent {
    match agent {
        "claude" => PaneAgent::Claude,
        "codex" => PaneAgent::Codex,
        "hermes" => PaneAgent::Native(&[SourceId::Hermes, SourceId::Nous]),
        "grok" => PaneAgent::Native(&[SourceId::Grok]),
        "agy" => PaneAgent::Native(&[SourceId::Antigravity]),
        _ => PaneAgent::Other,
    }
}

/// What the command prints for one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneTag {
    /// The token text, already capped at [`MAX_TAG_CHARS`].
    pub(crate) text: String,
    /// The `tollgate_severity` value; `None` clears it.
    pub(crate) severity: Option<Severity>,
}

/// The tag for `profile` (the account the script resolved) or, with no
/// profile, for the one observation a native `agent` can burn. `None` means
/// "no tag": the script publishes the clear.
pub(crate) fn resolve_tag(
    accounts: &[AccountObservation],
    profile: Option<&str>,
    agent: &str,
    now_secs: i64,
) -> Option<PaneTag> {
    let kind = pane_agent(agent);
    match profile.map(str::trim).filter(|p| !p.is_empty()) {
        Some(name) => {
            let origin = if kind == PaneAgent::Codex {
                Origin::CodexProfile
            } else {
                Origin::Profile
            };
            let id = account_id(origin, name);
            Some(match accounts.iter().find(|o| o.id == id) {
                Some(obs) => format_tag(name, obs, now_secs),
                // Known by name, nothing observed: the name alone, the tag
                // the plugin published before observations existed.
                None => PaneTag {
                    text: cap(&clean_label(name)),
                    severity: None,
                },
            })
        }
        None => {
            let sources = match kind {
                PaneAgent::Codex => &[SourceId::Codex][..],
                PaneAgent::Native(sources) => sources,
                _ => return None,
            };
            let obs = native_match(accounts, sources)?;
            Some(format_tag(&obs.label, obs, now_secs))
        }
    }
}

/// The one enabled observation from `sources`, or `None` when there is none
/// or more than one (ambiguous: no tag rather than a guess).
///
/// A Nous observation counts only when it reads the harness's own login
/// ([`AuthKind::NativeLogin`]: the monitor borrows Hermes' `auth.json`); a
/// Nous API key is a provider Hermes may or may not be configured to call.
pub(crate) fn native_match<'a>(
    accounts: &'a [AccountObservation],
    sources: &[SourceId],
) -> Option<&'a AccountObservation> {
    let mut candidates = accounts.iter().filter(|o| {
        !o.disabled
            && sources.contains(&o.source)
            && (!matches!(o.source, SourceId::Nous | SourceId::Codex)
                || o.auth == AuthKind::NativeLogin)
    });
    let only = candidates.next()?;
    candidates.next().is_none().then_some(only)
}

/// `<label> <lead metric>[ ⏸][ ⚠|‼]` for one observation.
///
/// The metric is the lead window's used share (`2%`, `23%w`, `64% mo`) when
/// the account has windows, else its first balance, cap, budget or spend meter
/// (`$13.67`, `$9.00 left`, `$4.08/mo`), else nothing. `⏸` marks stale figures;
/// `‼` a CRITICAL account, `⚠` a HIGH one or one with a failure on record.
/// Pace folds into the severity (plan §4.1), so a window burning well ahead
/// of its clock warns before its share alone would.
pub(crate) fn format_tag(label: &str, obs: &AccountObservation, now_secs: i64) -> PaneTag {
    let severity = account_severity(obs, now_secs, true);
    let mut tail: Vec<String> = Vec::new();
    if let Some(metric) = lead_metric(obs, now_secs) {
        tail.push(metric);
    }
    if matches!(obs.freshness, Freshness::Stale { .. }) {
        tail.push(STALE_FLAG.to_string());
    }
    let alarm = match severity {
        Some(Severity::Critical) => Some(CRITICAL_FLAG),
        Some(Severity::High) => Some(HIGH_FLAG),
        _ => obs.failure.as_ref().map(|_| HIGH_FLAG),
    };
    if let Some(flag) = alarm {
        tail.push(flag.to_string());
    }
    let label = clean_label(label);
    let tail = tail.join(" ");
    let text = if tail.is_empty() {
        cap(&label)
    } else {
        // The label gives way first: the numbers and flags are the point.
        let room = MAX_TAG_CHARS.saturating_sub(tail.chars().count() + 1);
        let label: String = label.chars().take(room).collect();
        cap(&format!("{label} {tail}"))
    };
    PaneTag { text, severity }
}

/// The lead metric alone: the lead window's share, else a money figure.
pub(crate) fn lead_metric(obs: &AccountObservation, now_secs: i64) -> Option<String> {
    lead_window(&obs.windows, now_secs)
        .and_then(window_metric)
        .or_else(|| money_metric(&obs.money))
}

/// A window's used share, rounded to a whole percent, with a suffix naming a
/// non-session window: `w` weekly (any model), ` mo` monthly.
fn window_metric(w: &QuotaWindow) -> Option<String> {
    let pct = w.used_pct.or(w.exhausted.then_some(100.0))?;
    if !pct.is_finite() {
        return None;
    }
    let suffix = if w.id == WINDOW_WEEKLY || w.id.starts_with(WINDOW_WEEKLY_MODEL_PREFIX) {
        "w"
    } else if w.id == WINDOW_MONTH {
        " mo"
    } else {
        ""
    };
    Some(format!("{}%{suffix}", pct.round().max(0.0) as i64))
}

/// The first meter by kind preference — balance, cap, budget, spend — as a
/// short money string.
fn money_metric(money: &[MoneyMeter]) -> Option<String> {
    let order = [
        MoneyKind::Balance,
        MoneyKind::Limit,
        MoneyKind::Budget,
        MoneyKind::Spend,
    ];
    let meter = order
        .iter()
        .find_map(|kind| money.iter().find(|m| m.kind == *kind))?;
    let figure = format_money(&meter.amount, &meter.currency);
    Some(match meter.kind {
        MoneyKind::Balance | MoneyKind::Budget => figure,
        MoneyKind::Limit => format!("{figure} left"),
        MoneyKind::Spend => {
            let per = match meter.period.as_ref().map(|p| p.kind) {
                Some(PeriodKind::Daily) => "/d",
                Some(PeriodKind::Weekly) => "/wk",
                Some(PeriodKind::Monthly) => "/mo",
                _ => "",
            };
            format!("{figure}{per}")
        }
    })
}

/// A label fit for one metadata line: control characters (a newline would
/// split the two-line answer) dropped, blanks trimmed.
fn clean_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// `text` cut to herdr's token cap, on a char boundary.
fn cap(text: &str) -> String {
    text.chars().take(MAX_TAG_CHARS).collect()
}

/// The command's stdout for a resolved tag: the text, then the severity
/// class when one is graded.
pub(crate) fn tag_lines(tag: &PaneTag) -> Vec<String> {
    let mut lines = vec![tag.text.clone()];
    if let Some(severity) = tag.severity {
        lines.push(severity.class().to_string());
    }
    lines
}

/// `tollgate herdr tag [--agent <kind>] [<profile>]`: prints [`tag_lines`], or
/// nothing when the pane gets no tag. Always exits 0: the reporter treats an
/// empty answer as "clear", and a predating binary answers the same.
pub(crate) fn run(profile: Option<&str>, agent: Option<&str>) -> Result<()> {
    let accounts = collect(&CollectOpts {
        // A disabled profile still burns the pane it runs in.
        include_disabled: true,
        ..CollectOpts::default()
    });
    let now_secs = crate::usage::now_epoch_secs();
    if let Some(tag) = resolve_tag(&accounts, profile, agent.unwrap_or(""), now_secs) {
        for line in tag_lines(&tag) {
            outln!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/herdr_tag.rs"]
mod tests;
