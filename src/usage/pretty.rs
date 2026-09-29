//! `tollgate usage` text and Waybar output (plan v3.1 §4.5).
//!
//! The text report is the metric-card layout of [`super::cards`], grouped by
//! provider. It is coloured from the active palette (Omarchy or Catppuccin,
//! 24-bit or xterm-256 by tier) on a terminal, and plain — the same layout,
//! no escapes — under `NO_COLOR`, off a terminal, or with `--plain`.
//! `--waybar` prints one `{text, tooltip, class, percentage}` line for a
//! Waybar / Omarchy bar module. `--json` stays [`super::report`]'s envelope.
//! `--watch N` repeats any of them every N seconds.

use anyhow::Result;
use serde::Serialize;

use super::cards::{CardCtx, CardLine, plain_text, report_lines};
use super::collect::{CollectOpts, collect};
use super::derive::{account_severity, countdown_to, format_money, hhmm_at_offset, lead_window};
use super::observation::{AccountObservation, MoneyKind, Origin};
use crate::out::{out, outln};
use crate::tui::theme;

/// Everything `tollgate usage` was asked for.
#[derive(Debug, Clone, Default)]
pub(crate) struct UsageArgs {
    pub(crate) json: bool,
    pub(crate) plain: bool,
    pub(crate) waybar: bool,
    /// Repeat every N seconds.
    pub(crate) watch: Option<u64>,
    pub(crate) opts: CollectOpts,
}

/// Whether the text report carries colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextMode {
    Colour,
    Plain,
}

/// Colour only on a terminal, without `NO_COLOR` (any non-empty value,
/// <https://no-color.org>) and without `--plain`.
pub(crate) fn text_mode(is_tty: bool, no_color: Option<&str>, plain: bool) -> TextMode {
    if is_tty && !plain && no_color.is_none_or(str::is_empty) {
        TextMode::Colour
    } else {
        TextMode::Plain
    }
}

/// One line as ANSI, each segment in its palette colour.
fn line_ansi(line: &CardLine) -> String {
    let mut out = String::new();
    for seg in line {
        if seg.text.trim().is_empty() {
            out.push_str(&seg.text);
            continue;
        }
        let sgr = theme::ansi_style(theme::ink_color(seg.ink), seg.bold);
        out.push_str(&sgr);
        out.push_str(&seg.text);
        if !sgr.is_empty() {
            out.push_str(theme::ANSI_RESET);
        }
    }
    out.trim_end().to_string()
}

/// The text report, one `\n`-terminated line per row.
pub(crate) fn render_text(
    accounts: &[AccountObservation],
    ctx: &CardCtx,
    mode: TextMode,
) -> String {
    let mut out = String::new();
    for line in report_lines(accounts, ctx) {
        match mode {
            TextMode::Colour => out.push_str(&line_ansi(&line)),
            TextMode::Plain => out.push_str(&plain_text(&line)),
        }
        out.push('\n');
    }
    out
}

/// A Waybar custom-module line (`return-type: json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WaybarOut {
    pub(crate) text: String,
    pub(crate) tooltip: String,
    /// The lead account's severity class (`ok`, `mid`, `high`, `critical`),
    /// `none` when nothing is graded.
    pub(crate) class: String,
    /// The lead window's used share, 0–100, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) percentage: Option<u8>,
}

/// The account a one-line surface leads with: the active tollgate profile,
/// else any active account, else the worst-graded, else the first.
pub(crate) fn lead_account(
    accounts: &[AccountObservation],
    now_secs: i64,
) -> Option<&AccountObservation> {
    accounts
        .iter()
        .find(|o| o.active && o.origin == Origin::Profile)
        .or_else(|| accounts.iter().find(|o| o.active))
        .or_else(|| {
            accounts
                .iter()
                .filter(|o| account_severity(o, now_secs, false).is_some())
                .max_by_key(|o| account_severity(o, now_secs, false))
        })
        .or_else(|| accounts.first())
}

/// The first balance or limit meter's figure, `$13.67`.
fn lead_money(o: &AccountObservation) -> Option<String> {
    o.money
        .iter()
        .find(|m| m.additive && matches!(m.kind, MoneyKind::Balance | MoneyKind::Limit))
        .map(|m| format_money(&m.amount, &m.currency))
}

/// `29% · 1h 12m` for the lead window, else the lead money figure.
fn compact(o: &AccountObservation, now_secs: i64) -> Option<String> {
    if let Some(w) = lead_window(&o.windows, now_secs) {
        let pct = w
            .used_pct
            .map_or_else(|| "—".to_string(), |p| format!("{p:.0}%"));
        return Some(match w.resets_at {
            Some(_) => format!("{pct} · {}", countdown_to(w.resets_at, now_secs)),
            None => pct,
        });
    }
    lead_money(o)
}

/// One tooltip line: `● work (Anthropic): 5h 29% resets 13:53 · 7d 40% · Balance $13.67`.
fn tooltip_line(o: &AccountObservation, offset_secs: i32) -> String {
    let mut parts: Vec<String> = o
        .windows
        .iter()
        .map(|w| {
            let pct = w
                .used_pct
                .map_or_else(|| "—".to_string(), |p| format!("{p:.0}%"));
            match w.resets_at {
                Some(at) => format!(
                    "{} {pct} resets {}",
                    w.label,
                    hhmm_at_offset(at, offset_secs)
                ),
                None => format!("{} {pct}", w.label),
            }
        })
        .collect();
    parts.extend(
        o.money
            .iter()
            .filter(|m| m.additive && matches!(m.kind, MoneyKind::Balance | MoneyKind::Limit))
            .map(|m| format!("{} {}", m.label, format_money(&m.amount, &m.currency))),
    );
    if let Some(f) = &o.failure {
        parts.push(format!("⚠ {}", f.message));
    }
    let mark = if o.active { "● " } else { "" };
    let body = if parts.is_empty() {
        "no figures".to_string()
    } else {
        parts.join(" · ")
    };
    format!("{mark}{} ({}): {body}", o.label, o.provider)
}

/// The Waybar line for `accounts`.
pub(crate) fn waybar(
    accounts: &[AccountObservation],
    now_secs: i64,
    offset_secs: i32,
) -> WaybarOut {
    let Some(lead) = lead_account(accounts, now_secs) else {
        return WaybarOut {
            text: "—".to_string(),
            tooltip: "tollgate: no accounts".to_string(),
            class: "none".to_string(),
            percentage: None,
        };
    };
    let percentage = lead_window(&lead.windows, now_secs)
        .and_then(|w| w.used_pct)
        .map(|p| p.round().clamp(0.0, 100.0) as u8);
    WaybarOut {
        text: compact(lead, now_secs).unwrap_or_else(|| lead.label.clone()),
        tooltip: accounts
            .iter()
            .map(|o| tooltip_line(o, offset_secs))
            .collect::<Vec<_>>()
            .join("\n"),
        class: account_severity(lead, now_secs, false)
            .map_or("none", |s| s.class())
            .to_string(),
        percentage,
    }
}

/// The local zone's UTC offset at `now_secs`.
pub(crate) fn local_offset_secs(now_secs: i64) -> i32 {
    use chrono::{Local, Offset, TimeZone};
    Local
        .timestamp_opt(now_secs, 0)
        .single()
        .map_or(0, |dt| dt.offset().fix().local_minus_utc())
}

/// Columns to lay out in: the terminal's width on a TTY, else `$COLUMNS`,
/// else 80.
fn layout_width(is_tty: bool) -> usize {
    if is_tty && let Ok((w, _)) = ratatui::crossterm::terminal::size() {
        return usize::from(w);
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or(80)
}

/// Seed the tier and palette from `profiles.toml` (`theme`, `palette`) for a
/// coloured report.
fn init_colours() {
    let config = crate::profile::load_config().ok();
    let tier = config
        .as_ref()
        .and_then(|c| c.state.theme)
        .map(|t| match t {
            crate::profile::ThemeName::Full => theme::Tier::Full,
            crate::profile::ThemeName::Compatible => theme::Tier::Compatible,
        });
    theme::init(tier);
    let setting = config
        .as_ref()
        .map(|c| c.state.palette_setting())
        .unwrap_or_default();
    if let Ok(home) = crate::profile::home_dir() {
        theme::init_palette(setting, &home);
    }
}

/// `tollgate usage [--json | --waybar | --plain] [--watch N] [filters]`.
pub(crate) fn run(args: &UsageArgs) -> Result<()> {
    use std::io::IsTerminal as _;
    let is_tty = std::io::stdout().is_terminal();
    let no_color = std::env::var("NO_COLOR").ok();
    let mode = text_mode(is_tty, no_color.as_deref(), args.plain);
    if mode == TextMode::Colour && !args.json && !args.waybar {
        init_colours();
    }
    loop {
        if args.json {
            super::report::run(true, &args.opts)?;
        } else {
            let accounts = collect(&args.opts);
            let now_secs = crate::usage::now_epoch_secs();
            let offset_secs = local_offset_secs(now_secs);
            if args.waybar {
                outln!(
                    "{}",
                    serde_json::to_string(&waybar(&accounts, now_secs, offset_secs))?
                );
            } else {
                let ctx = CardCtx {
                    width: layout_width(is_tty),
                    now_secs,
                    offset_secs,
                    guest_mode: crate::identity::upstream_active(),
                };
                if args.watch.is_some() && is_tty {
                    // Home + clear, so each frame replaces the last.
                    out!("\x1b[H\x1b[2J");
                }
                out!("{}", render_text(&accounts, &ctx, mode));
            }
        }
        let Some(secs) = args.watch else {
            return Ok(());
        };
        std::thread::sleep(std::time::Duration::from_secs(secs.max(1)));
    }
}

#[cfg(test)]
#[path = "../../tests/inline/usage_pretty.rs"]
mod tests;
