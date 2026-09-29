//! Desktop notifications for monitors (plan v3.1 §4.5): one `notify-send`
//! when a monitor's severity crosses into HIGH / CRITICAL, and one when its
//! `alert_pct` is reached. Best effort — a missing `notify-send` is silent.
//!
//! **De-dup.** Each notification carries a key naming what it is about and the
//! window it belongs to (`severity:high:<reset>`, `alert_pct:80:<month>`).
//! The keys already sent live in the monitor's cache ([`AlertState`]), so a
//! reading that stays HIGH, or dips and rises again inside the same window,
//! notifies once; a new window (a later reset, a new month) can notify again.

use serde::{Deserialize, Serialize};

use super::config::MonitorConfig;
use super::observe::{BUDGET_METER, budget_severity, budget_spent_pct, monitor_severity};
use crate::usage::derive::{
    Severity, SeverityBasis, format_money, meter_severity, window_severity,
};
use crate::usage::observation::{AccountObservation, Timestamp};

/// How many sent keys a monitor remembers.
const MAX_SENT_KEYS: usize = 32;

/// What a monitor has already notified about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AlertState {
    /// The severity at the last evaluation.
    #[serde(default)]
    pub(crate) last_severity: Option<Severity>,
    /// The window [`Self::last_severity`] was judged in; a new window starts
    /// the crossing over.
    #[serde(default)]
    pub(crate) last_anchor: Option<String>,
    /// Keys already notified, oldest first.
    #[serde(default)]
    pub(crate) sent: Vec<String>,
}

/// One notification.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Notification {
    pub(crate) key: String,
    pub(crate) summary: String,
    pub(crate) body: String,
    pub(crate) critical: bool,
}

/// Where notifications go.
pub(crate) trait Notifier: Sync {
    fn send(&self, n: &Notification);
}

/// `notify-send`, detached and best effort. A no-op in test builds.
pub(crate) struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    fn send(&self, n: &Notification) {
        if cfg!(test) {
            return;
        }
        if let Ok(mut child) = notify_command(n).spawn() {
            // Reap it off-thread so no zombie outlives the send.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// The `notify-send` invocation for `n`. The daemon holds the monitoring and
/// billing keys in its own env to read balances; a desktop helper inherits
/// none of them ([`crate::providers::billing_key::scrub_helper_env`]).
pub(crate) fn notify_command(n: &Notification) -> std::process::Command {
    let urgency = if n.critical { "critical" } else { "normal" };
    let mut cmd = std::process::Command::new("notify-send");
    cmd.args([
        "-a",
        crate::identity::NAME,
        "-u",
        urgency,
        &n.summary,
        &n.body,
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    crate::providers::billing_key::scrub_helper_env(&mut cmd);
    cmd
}

/// Collects notifications instead of sending them, for tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingNotifier(pub(crate) std::sync::Mutex<Vec<Notification>>);

#[cfg(test)]
impl RecordingNotifier {
    pub(crate) fn sent(&self) -> Vec<Notification> {
        self.0.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
impl Notifier for RecordingNotifier {
    fn send(&self, n: &Notification) {
        if let Ok(mut v) = self.0.lock() {
            v.push(n.clone());
        }
    }
}

/// Decide what to notify about `obs` given what was sent before. Returns the
/// notifications to send and the state to store. Pure.
pub(crate) fn evaluate(
    cfg: &MonitorConfig,
    obs: &AccountObservation,
    prev: &AlertState,
    now_secs: i64,
) -> (Vec<Notification>, AlertState) {
    let mut out = Vec::new();
    let mut state = prev.clone();
    let severity = monitor_severity(obs, now_secs);
    let label = cfg.display_label();

    let judged = severity.map(|sev| (sev, severity_anchor(obs, sev, now_secs)));
    if let Some((sev, (anchor, detail, basis))) = judged.clone()
        && sev >= Severity::High
        && prev
            .last_severity
            .filter(|_| prev.last_anchor.as_deref() == Some(anchor.as_str()))
            .is_none_or(|p| p < sev)
    {
        let key = format!("severity:{}:{anchor}", sev.class());
        if !state.sent.contains(&key) {
            out.push(Notification {
                key,
                summary: format!("{}: {label} {}", crate::identity::NAME, sev.word(basis)),
                body: detail,
                critical: sev == Severity::Critical,
            });
        }
    }

    if let Some(threshold) = cfg.alert_pct
        && let Some((pct, anchor, detail)) = alert_share(obs, now_secs)
        && pct >= threshold
    {
        let key = format!("alert_pct:{threshold}:{anchor}");
        if !state.sent.contains(&key) {
            out.push(Notification {
                key,
                summary: format!("{}: {label} past {threshold}%", crate::identity::NAME),
                body: detail,
                critical: pct >= 100.0,
            });
        }
    }

    state.last_severity = severity;
    state.last_anchor = judged.map(|(_, (anchor, _, _))| anchor);
    for n in &out {
        state.sent.push(n.key.clone());
    }
    let excess = state.sent.len().saturating_sub(MAX_SENT_KEYS);
    state.sent.drain(..excess);
    (out, state)
}

/// The window a severity belongs to (its reset, or the month for a budget or
/// a balance), a one-line body, and the basis that words it (a balance reads
/// LOW, not HIGH).
fn severity_anchor(
    obs: &AccountObservation,
    sev: Severity,
    now_secs: i64,
) -> (String, String, SeverityBasis) {
    if let Some(w) = obs
        .windows
        .iter()
        .filter(|w| window_severity(w) == Some(sev))
        .min_by_key(|w| w.resets_at)
    {
        let pct = w
            .used_pct
            .map_or_else(|| "spent".to_string(), |p| format!("{p:.0}% used"));
        let reset = w
            .resets_at
            .map(|t| {
                format!(
                    " · resets in {}",
                    crate::usage::derive::countdown_to(Some(t), now_secs)
                )
            })
            .unwrap_or_default();
        return (
            reset_anchor(w.resets_at),
            format!("{} {pct}{reset}", w.label),
            SeverityBasis::Usage,
        );
    }
    if let Some(m) = obs
        .money
        .iter()
        .find(|m| m.meter_id == BUDGET_METER && budget_severity(m) == Some(sev))
    {
        return (month_anchor(now_secs), budget_body(m), SeverityBasis::Usage);
    }
    if let Some((m, basis)) = obs.money.iter().find_map(|m| {
        meter_severity(m)
            .filter(|(s, _)| *s == sev)
            .map(|(_, b)| (m, b))
    }) {
        let body = format!("{} {} left", m.label, format_money(&m.amount, &m.currency));
        return (
            format!("{}:{}", m.meter_id, month_anchor(now_secs)),
            body,
            basis,
        );
    }
    let body = obs
        .failure
        .as_ref()
        .map(|f| f.message.clone())
        .unwrap_or_else(|| "quota spent".to_string());
    (month_anchor(now_secs), body, SeverityBasis::Usage)
}

/// The share `alert_pct` is judged on: the budget's spent share when a budget
/// meter exists, else the worst window's used share.
fn alert_share(obs: &AccountObservation, now_secs: i64) -> Option<(f64, String, String)> {
    if let Some(m) = obs.money.iter().find(|m| m.meter_id == BUDGET_METER) {
        return budget_spent_pct(m).map(|p| (p, month_anchor(now_secs), budget_body(m)));
    }
    obs.windows
        .iter()
        .filter_map(|w| w.used_pct.map(|p| (p, w)))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(p, w)| {
            (
                p,
                format!("{}:{}", w.id, reset_anchor(w.resets_at)),
                format!("{} {p:.0}% used", w.label),
            )
        })
}

fn budget_body(m: &crate::usage::observation::MoneyMeter) -> String {
    let cap = m
        .limit
        .as_ref()
        .map(|l| format_money(l, &m.currency))
        .unwrap_or_default();
    if m.amount.is_negative() {
        format!(
            "over the {cap} monthly budget by {}",
            format_money(&m.amount.abs(), &m.currency)
        )
    } else {
        format!(
            "{} left of the {cap} monthly budget",
            format_money(&m.amount, &m.currency)
        )
    }
}

fn reset_anchor(resets_at: Option<Timestamp>) -> String {
    resets_at.map_or_else(|| "none".to_string(), |t| t.secs().to_string())
}

/// `YYYY-MM` (UTC) of `now_secs`.
pub(crate) fn month_anchor(now_secs: i64) -> String {
    chrono::DateTime::from_timestamp(now_secs, 0)
        .map(|d| d.format("%Y-%m").to_string())
        .unwrap_or_else(|| "none".to_string())
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_alert.rs"]
mod tests;
