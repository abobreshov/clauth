//! The Usage pane for a Codex, Grok or Antigravity account, in the Claude
//! layout: a `plan` row and any account facts, the `status` block in the same
//! vocabulary as a Claude account's, then one two-line bar per window.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::super::app::{App, CodexRow};
use super::super::super::theme;
use super::super::format::{
    ResetFmt, activity_verb, reset_in_secs_at, reset_phrase, spinner_frame, spinner_style,
};
use super::super::overview::{shared_column, shared_window};
use super::super::panes::{DIAG_AUTH_BROKEN, pill, section_box_verbatim};
use super::{
    DiagRow, Stat, WindowGates, key_span, make_window_stat, ordinal, render_stat_block,
    render_status_rows, streak_style,
};
use crate::provider_monitor::ProviderReport;
use crate::provider_monitor::types::{ObservationState, ProviderKind, QuotaBucket};
use crate::usage::{
    CodexPoll, CodexPollOutcome, ProfileActivity, UsageWindow, now_epoch_secs, now_ms,
};

/// Everything a native Usage pane draws, gathered before any line is built so
/// the builder is a pure function of it.
pub(super) struct NativeUsage {
    pub(super) plan: Option<String>,
    /// Header rows under `plan`, as (key, value).
    pub(super) facts: Vec<(&'static str, String)>,
    pub(super) status: NativeStatus,
    pub(super) windows: Vec<NativeWindow>,
}

/// One bar. `label` follows the Claude spelling (`5h`, `7d`, `7d <pool>`) when
/// the window's length is known, so the pace marker and average pace work.
#[derive(Debug, Clone)]
pub(super) struct NativeWindow {
    pub(super) label: String,
    pub(super) window: UsageWindow,
}

/// Where a native account's refresh stands, named in the Claude status rows.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct NativeStatus {
    /// Queued or fetching in this process; `Idle` otherwise.
    pub(super) activity: ProfileActivity,
    pub(super) fetch: NativeFetch,
    /// Consecutive failed checks, for the retry ordinal.
    pub(super) failures: u32,
    /// Epoch ms of the next scheduled check.
    pub(super) next_ms: Option<u64>,
    /// The reading is older than the stale threshold for the refresh interval.
    pub(super) stale: bool,
    /// The provider says a limit was reached.
    pub(super) limit_reached: bool,
    /// The fix text under the fetch row, when there is one.
    pub(super) hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeFetch {
    /// The last check returned a reading.
    Fresh,
    /// No check has returned yet.
    Waiting,
    Failed,
    RateLimited,
    /// A 401 the refresh leg is already retrying.
    AuthFailing,
    /// The stored login is dead; only a new sign-in clears it.
    AuthBroken,
    /// No login to check with.
    SignInNeeded,
}

pub(super) fn draw_codex_usage(frame: &mut Frame<'_>, area: Rect, app: &App, idx: usize) {
    let Some(row) = app.codex_rows.get(idx) else {
        return;
    };
    let interval_ms = app
        .refresh_interval
        .load(std::sync::atomic::Ordering::Relaxed);
    let usage = codex_usage(row, interval_ms, now_ms());
    draw_native(frame, area, app, row.name.as_str(), &usage);
}

pub(super) fn draw_native_usage(frame: &mut Frame<'_>, area: Rect, app: &App, idx: usize) {
    let Some(report) = app.provider_reports.get(idx) else {
        return;
    };
    draw_native(frame, area, app, &report.id, &report_usage(report));
}

fn draw_native(frame: &mut Frame<'_>, area: Rect, app: &App, title: &str, usage: &NativeUsage) {
    let block = section_box_verbatim(title, false, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (gates, reset_fmt) = {
        let cfg = app.config();
        (
            WindowGates {
                show_estimates: cfg.state.show_estimates,
                show_pace: cfg.state.show_pace,
            },
            ResetFmt::from_state(&cfg.state),
        )
    };
    let lines = native_usage_lines(
        usage,
        inner.width,
        app.tick_count,
        gates,
        reset_fmt,
        now_epoch_secs(),
    );
    frame.render_widget(Paragraph::new(lines).style(theme::base()), inner);
}

/// A codex profile's pane from its roster row and this process's poll record.
pub(super) fn codex_usage(row: &CodexRow, interval_ms: u64, now: u64) -> NativeUsage {
    let poll = row.poll.unwrap_or_default();
    let has_reading = row.five_hour.is_some() || row.seven_day.is_some();
    let activity = if poll.fetching {
        ProfileActivity::Fetching
    } else if poll.queued {
        ProfileActivity::Queued
    } else {
        ProfileActivity::Idle
    };
    let (fetch, hint) = if row.broken {
        (
            NativeFetch::AuthBroken,
            Some(format!(
                "the login was revoked; capture it again: clauth login {} --codex",
                row.name
            )),
        )
    } else {
        codex_fetch(&poll, has_reading, row.name.as_str())
    };
    // A TUI standing down behind the daemon has no poll record; the cache age
    // still dates the reading, and the daemon polls on the same interval.
    let next_ms = row
        .poll
        .and_then(|p| p.next_poll_ms(interval_ms))
        .or_else(|| row.fetched_at.map(|at| at.saturating_add(interval_ms)));
    let stale = has_reading
        && row.fetched_at.is_none_or(|at| {
            now.saturating_sub(at) > crate::profile_json::stale_after_ms(interval_ms)
        });
    let mut facts = Vec::new();
    if let Some(credits) = row.reset_credits.filter(|n| *n > 0) {
        facts.push(("resets", format!("{credits} available")));
    }
    let mut windows = Vec::new();
    for (label, window) in [("5h", &row.five_hour), ("7d", &row.seven_day)] {
        if let Some(window) = window {
            windows.push(NativeWindow {
                label: label.to_string(),
                window: window.clone(),
            });
        }
    }
    NativeUsage {
        plan: row.plan.clone(),
        facts,
        status: NativeStatus {
            activity,
            fetch,
            failures: poll.failures,
            next_ms,
            stale,
            limit_reached: row.limit_reached.is_some(),
            hint,
        },
        windows,
    }
}

fn codex_fetch(poll: &CodexPoll, has_reading: bool, name: &str) -> (NativeFetch, Option<String>) {
    match poll.outcome {
        None if has_reading => (NativeFetch::Fresh, None),
        None => (NativeFetch::Waiting, None),
        Some(CodexPollOutcome::Fresh) => (NativeFetch::Fresh, None),
        Some(CodexPollOutcome::Unauthorized) => (
            NativeFetch::AuthFailing,
            Some("the access token is stale; the next refresh renews it".to_string()),
        ),
        Some(CodexPollOutcome::RateLimited) => (NativeFetch::RateLimited, None),
        Some(CodexPollOutcome::Failed) => (
            NativeFetch::Failed,
            Some("the usage request failed; showing the last reading".to_string()),
        ),
        Some(CodexPollOutcome::NoLogin) => (
            NativeFetch::SignInNeeded,
            Some(format!(
                "no stored login; capture it: clauth login {name} --codex"
            )),
        ),
    }
}

/// A Grok or Antigravity pane from its monitor report.
pub(super) fn report_usage(report: &ProviderReport) -> NativeUsage {
    let refresh = report.refresh;
    let activity = if refresh.refreshing {
        ProfileActivity::Fetching
    } else if refresh.queued {
        ProfileActivity::Queued
    } else {
        ProfileActivity::Idle
    };
    let message = report.message.clone();
    let (fetch, hint) = match report.state {
        ObservationState::Fresh | ObservationState::Stale => (NativeFetch::Fresh, None),
        ObservationState::NotFetched => (NativeFetch::Waiting, None),
        ObservationState::AuthRequired => (NativeFetch::SignInNeeded, message),
        ObservationState::RateLimited => (NativeFetch::RateLimited, None),
        ObservationState::Unavailable | ObservationState::InvalidResponse => {
            (NativeFetch::Failed, message)
        }
    };
    let shared: Vec<&QuotaBucket> = report
        .data
        .buckets
        .iter()
        .filter(|b| b.scope != "model")
        .collect();
    NativeUsage {
        plan: report.data.plan.clone(),
        facts: report_facts(report),
        status: NativeStatus {
            activity,
            fetch,
            failures: refresh.failures,
            next_ms: refresh.next_check_ms,
            stale: report.state == ObservationState::Stale,
            limit_reached: shared.iter().any(|b| b.exhausted),
            hint,
        },
        windows: report_windows(&shared),
    }
}

fn report_facts(report: &ProviderReport) -> Vec<(&'static str, String)> {
    let mut facts = Vec::new();
    if report.provider == ProviderKind::Grok && !report.data.attribution.is_empty() {
        let products = report
            .data
            .attribution
            .iter()
            .map(|a| format!("{} {}", a.label, crate::format::format_pct(a.used_percent)))
            .collect::<Vec<_>>()
            .join(" · ");
        facts.push(("products", products));
    }
    if let Some(model) = report.model.as_deref() {
        facts.push(("model", model.to_string()));
    }
    facts
}

/// The shared buckets as bars: every 5h window, then every weekly one, then
/// any whose length is unknown under the provider's own label. Model buckets
/// never reach here: they are readings mapped onto these pools, not budgets.
fn report_windows(shared: &[&QuotaBucket]) -> Vec<NativeWindow> {
    let column = |want: Option<bool>| -> Vec<&QuotaBucket> {
        shared
            .iter()
            .copied()
            .filter(|b| shared_column(b) == want)
            .collect()
    };
    let mut windows = Vec::new();
    for (want, prefix) in [
        (Some(true), Some("5h")),
        (Some(false), Some("7d")),
        (None, None),
    ] {
        let buckets = column(want);
        // A second pool in the same column needs its name to tell them apart.
        let named = buckets.len() > 1;
        for bucket in buckets {
            let Some(window) = shared_window(bucket) else {
                continue;
            };
            let label = match prefix {
                Some(prefix) if named => format!("{prefix} {}", pool_name(bucket)),
                Some(prefix) => prefix.to_string(),
                None => bucket.label.clone(),
            };
            windows.push(NativeWindow { label, window });
        }
    }
    windows
}

/// `Gemini Models: Weekly Limit Remaining` → `gemini`,
/// `Claude and GPT models: …` → `claude and gpt`.
fn pool_name(bucket: &QuotaBucket) -> String {
    let head = bucket
        .label
        .split(':')
        .next()
        .unwrap_or(&bucket.label)
        .trim()
        .to_lowercase();
    let head = head.strip_suffix(" models").unwrap_or(&head).trim();
    if head.is_empty() {
        bucket.id.clone()
    } else {
        head.chars().filter(|c| !c.is_control()).collect()
    }
}

pub(super) fn native_usage_lines(
    usage: &NativeUsage,
    inner_w: u16,
    tick: u64,
    gates: WindowGates,
    reset_fmt: ResetFmt,
    now_secs: i64,
) -> Vec<Line<'static>> {
    let mut lines = vec![plan_line(usage.plan.as_deref())];
    for (key, value) in &usage.facts {
        lines.push(Line::from(vec![
            key_span(key),
            Span::styled(value.clone(), theme::body()),
        ]));
    }
    lines.extend(native_status_lines(
        &usage.status,
        tick,
        inner_w as usize,
        now_ms(),
    ));
    lines.push(Line::from(""));

    if usage.windows.is_empty() {
        let msg = match usage.status.fetch {
            NativeFetch::Waiting => "loading",
            NativeFetch::SignInNeeded | NativeFetch::AuthBroken => {
                "not signed in, sign in with the official tool"
            }
            _ => "no usage available",
        };
        lines.push(Line::from(Span::styled(format!("  {msg}"), theme::faint())));
        return lines;
    }
    let stats: Vec<Stat> = usage
        .windows
        .iter()
        .map(|w| {
            let trailing = reset_in_secs_at(&w.window, now_secs)
                .map(|secs| format!("  {}", reset_phrase(secs, reset_fmt)))
                .unwrap_or_default();
            make_window_stat(
                &w.label,
                w.window.utilization,
                w.window.resets_at.as_deref(),
                now_secs,
                String::new(),
                trailing,
                gates,
            )
        })
        .collect();
    lines.extend(render_stat_block(&stats, inner_w));
    lines
}

fn plan_line(plan: Option<&str>) -> Line<'static> {
    let plan = plan
        .map(|p| p.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|p| !p.is_empty());
    Line::from(vec![
        key_span("plan"),
        match plan {
            Some(plan) => Span::styled(plan, theme::body()),
            None => Span::styled("—".to_string(), theme::faint()),
        },
    ])
}

/// The `status` block in the Claude vocabulary: a spinner while a refresh is
/// queued or running; otherwise dead-login pills first, then `limit reached`
/// and `stale`, then the fetch row with its countdown and fix hint.
pub(super) fn native_status_lines(
    status: &NativeStatus,
    tick: u64,
    width: usize,
    now_ms: u64,
) -> Vec<Line<'static>> {
    if status.activity != ProfileActivity::Idle {
        return vec![Line::from(vec![
            key_span("status"),
            Span::styled(
                format!("{} {}", spinner_frame(tick), activity_verb(status.activity)),
                spinner_style(status.activity),
            ),
        ])];
    }

    let bold = |style: ratatui::style::Style| style.add_modifier(Modifier::BOLD);
    let mut rows: Vec<DiagRow> = Vec::new();
    // A dead login dominates: nothing below can be true of an account that
    // cannot be read, so the block ends at its pill, as it does for Claude.
    match status.fetch {
        NativeFetch::AuthBroken => {
            rows.push(DiagRow {
                content: pill(DIAG_AUTH_BROKEN.to_string(), bold(theme::danger())),
                hint: status.hint.clone(),
            });
            return render_status_rows(rows, width);
        }
        NativeFetch::SignInNeeded => {
            rows.push(DiagRow {
                content: pill("sign-in needed".to_string(), bold(theme::danger())),
                hint: status.hint.clone(),
            });
            return render_status_rows(rows, width);
        }
        _ => {}
    }
    if status.limit_reached {
        rows.push(DiagRow {
            content: pill("limit reached".to_string(), bold(theme::warning())),
            hint: None,
        });
    }
    if status.stale {
        rows.push(DiagRow {
            content: pill("stale".to_string(), bold(theme::warning())),
            hint: None,
        });
    }

    let countdown = status.next_ms.map(|next| {
        let secs = (next.saturating_sub(now_ms) / 1000) as i64;
        format!("{secs}s")
    });
    let retry = |c: &str| {
        if status.failures > 1 {
            format!("  {} retry in {c}", ordinal(status.failures))
        } else {
            format!("  retry in {c}")
        }
    };
    let bracket = |label: &'static str, style| {
        vec![
            Span::styled("[ ", theme::dim()),
            Span::styled(label, style),
            Span::styled(" ]", theme::dim()),
        ]
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut hint = None;
    match status.fetch {
        NativeFetch::Failed => {
            spans.extend(bracket("failed", bold(theme::danger())));
            if let Some(c) = countdown {
                spans.push(Span::styled(retry(&c), theme::faint()));
            }
            hint = status.hint.clone();
        }
        NativeFetch::RateLimited => {
            spans.extend(bracket("rate limited", streak_style(status.failures)));
            if let Some(c) = countdown {
                spans.push(Span::styled(retry(&c), theme::faint()));
            }
            hint = status.hint.clone();
        }
        NativeFetch::AuthFailing => {
            spans.extend(bracket("auth failing", streak_style(status.failures)));
            if let Some(c) = countdown {
                spans.push(Span::styled(retry(&c), theme::faint()));
            }
            hint = status.hint.clone();
        }
        NativeFetch::Waiting => spans.extend([
            Span::styled("◌ ", theme::accent()),
            Span::styled("waiting for the first refresh", theme::dim()),
        ]),
        NativeFetch::Fresh | NativeFetch::AuthBroken | NativeFetch::SignInNeeded => match countdown
        {
            Some(c) => spans.extend([
                Span::styled("◌ ", theme::accent()),
                Span::styled(format!("refresh in {c}"), theme::dim()),
            ]),
            None => spans.extend([
                Span::styled("◌ ", theme::dim()),
                Span::styled("up to date", theme::dim()),
            ]),
        },
    }
    rows.push(DiagRow {
        content: spans,
        hint,
    });
    render_status_rows(rows, width)
}

#[cfg(test)]
#[path = "../../../../tests/inline/tui_render_native_usage.rs"]
mod tests;
