//! `tollgate usage` — every account's observation, as JSON or one line each.
//!
//! `--json` prints the stable envelope [`UsageReport`]
//! (`{schema_version, generated_at, guest_mode, accounts}`, plan §4.1): the
//! local read API agents call. The plain form is one line per account and is
//! NOT a contract yet (a styled renderer replaces it).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::collect::{CollectOpts, collect};
use super::derive::{
    account_severity, countdown_with_local, format_money, lead_window, meter_severity, window_pace,
    window_severity,
};
use super::observation::{AccountObservation, Freshness, MoneyKind, SCHEMA_VERSION};
use crate::out::outln;

/// The `usage --json` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageReport {
    /// [`SCHEMA_VERSION`].
    pub(crate) schema_version: u32,
    /// RFC 3339 instant the report was assembled.
    pub(crate) generated_at: String,
    /// Upstream clauth owns `~/.claude` on this machine (plan §4.0).
    pub(crate) guest_mode: bool,
    pub(crate) accounts: Vec<AccountObservation>,
}

impl UsageReport {
    /// Wrap `accounts` in the envelope, stamped `now_secs`.
    pub(crate) fn new(accounts: Vec<AccountObservation>, now_secs: i64, guest_mode: bool) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            generated_at: crate::usage::epoch_secs_to_iso(now_secs),
            guest_mode,
            accounts,
        }
    }
}

/// `tollgate usage [--json] [--all] [--account A] [--provider P]`.
pub(crate) fn run(json: bool, opts: &CollectOpts) -> Result<()> {
    let accounts = collect(opts);
    let now_secs = crate::usage::now_epoch_secs();
    if json {
        let report = UsageReport::new(accounts, now_secs, crate::identity::upstream_active());
        outln!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if accounts.is_empty() {
        outln!("no accounts yet. add one with `tollgate login <name>`.");
        return Ok(());
    }
    for obs in &accounts {
        outln!("{}", plain_line(obs, now_secs));
    }
    Ok(())
}

/// One plain line: active mark, id, provider, plan, every window lead first
/// (`5h 42% ↑ 12 pts ahead (3h 05m (14:32))`), the first balance / limit meter, the severity class,
/// and the freshness / failure flags.
pub(crate) fn plain_line(obs: &AccountObservation, now_secs: i64) -> String {
    let mut parts: Vec<String> = vec![
        format!("{} {}", if obs.active { '*' } else { ' ' }, obs.id),
        obs.provider.clone(),
    ];
    if let Some(plan) = &obs.plan {
        parts.push(plan.clone());
    }
    // Lead window first, the rest in source order.
    let lead = lead_window(&obs.windows, now_secs);
    let ordered = lead.into_iter().chain(
        obs.windows
            .iter()
            .filter(|w| lead.is_none_or(|l| !std::ptr::eq(*w, l))),
    );
    let windows: Vec<String> = ordered
        .map(|w| {
            let pct = w
                .used_pct
                .map_or_else(|| "?".to_string(), crate::format::format_pct);
            let sev = window_severity(w)
                .filter(|s| *s >= super::derive::Severity::High)
                .map(|s| format!(" {}", s.word(super::derive::SeverityBasis::Usage)))
                .unwrap_or_default();
            let pace = window_pace(w, now_secs)
                .map(|p| format!(" {} {}", p.glyph(), p.label()))
                .unwrap_or_default();
            format!(
                "{} {pct}{sev}{pace} ({})",
                w.label,
                countdown_with_local(w.resets_at, now_secs)
            )
        })
        .collect();
    if !windows.is_empty() {
        parts.push(windows.join(" · "));
    }
    if let Some(m) = obs
        .money
        .iter()
        .find(|m| matches!(m.kind, MoneyKind::Balance | MoneyKind::Limit))
    {
        let word = meter_severity(m)
            .map(|(s, basis)| format!(" {}", s.word(basis)))
            .unwrap_or_default();
        parts.push(format!(
            "{} {}{word}",
            m.label,
            format_money(&m.amount, &m.currency)
        ));
    }
    if let Some(sev) = account_severity(obs, now_secs, false) {
        parts.push(sev.class().to_string());
    }
    match obs.freshness {
        Freshness::Fresh => {}
        Freshness::Stale { .. } => parts.push("(stale)".to_string()),
        Freshness::NotFetched => parts.push("(not fetched)".to_string()),
    }
    if let Some(f) = &obs.failure {
        parts.push(format!("({})", f.message));
    }
    parts.join("  ")
}

#[cfg(test)]
#[path = "../../tests/inline/usage_report.rs"]
mod tests;
