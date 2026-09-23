//! The Setup detail for a Codex, Grok or Antigravity account, in the Claude
//! layout: a header block of what the account is, a blank line, then the
//! account actions as selectable rows with the same arrow, highlight, armed
//! label and focused hint as a Claude account's rows.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::super::super::accounts::RosterSlot;
use super::super::super::app::{App, CodexRow, ConfigFocus, NativeSetupRow, native_setup_rows};
use super::super::super::theme;
use super::super::panes::{
    DIAG_AUTH_BROKEN, bold_when, draw_scrolled_lines, help_tooltip_lines, highlight_row, key_cell,
    pill, section_box_verbatim,
};
use super::{KEY_GUTTER, KEY_W};
use crate::provider_monitor::ProviderReport;
use crate::provider_monitor::types::{ObservationState, ProviderKind};

/// What the header block and the rows need to know about the open account.
struct NativeSnap {
    title: String,
    kind: &'static str,
    plan: Option<String>,
    /// A dead login, named as a status pill like a disabled Claude account.
    status: Option<&'static str>,
    /// Further header rows, as (key, value).
    facts: Vec<(&'static str, String)>,
    /// Codex deletes the account; Grok and Antigravity leave the overview.
    removes_from_overview: bool,
}

pub(super) fn draw_native_settings(frame: &mut Frame<'_>, area: Rect, app: &App, slot: RosterSlot) {
    let snap = match slot {
        RosterSlot::Codex(idx) => app.codex_rows.get(idx).map(codex_snap),
        RosterSlot::Native(idx) => app.provider_reports.get(idx).map(report_snap),
        RosterSlot::Profile(_) => None,
    };
    let Some(snap) = snap else {
        return;
    };
    let actions_focused = app.config_focus == ConfigFocus::Actions;
    let block = section_box_verbatim(&snap.title, actions_focused, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = native_setup_rows(app);
    let cursor = app.config_action_cursor.min(rows.len().saturating_sub(1));
    let width = inner.width as usize;
    let mut lines = header_lines(&snap);
    lines.push(Line::from(""));
    let mut focus = (0usize, 1usize);
    for (i, row) in rows.iter().enumerate() {
        let selected = actions_focused && i == cursor;
        let line = row_line(*row, selected, app.native_setup_armed, &snap);
        if selected {
            focus.0 = lines.len();
            lines.push(highlight_row(line, width));
            lines.extend(help_tooltip_lines(row_hint(*row, &snap), width));
            focus.1 = lines.len();
        } else {
            lines.push(line);
        }
    }
    draw_scrolled_lines(frame, inner, lines, focus);
}

fn codex_snap(row: &CodexRow) -> NativeSnap {
    let login = if row.active {
        "codex runs as this account"
    } else {
        "stored, not the one codex runs as"
    };
    NativeSnap {
        title: row.name.to_string(),
        kind: "codex",
        plan: row.plan.clone(),
        status: row.broken.then_some(DIAG_AUTH_BROKEN),
        facts: vec![("login", login.to_string())],
        removes_from_overview: false,
    }
}

fn report_snap(report: &ProviderReport) -> NativeSnap {
    let kind = match report.provider {
        ProviderKind::Grok => "grok",
        ProviderKind::Antigravity => "antigravity",
        ProviderKind::Codex => "codex",
    };
    let mut facts = Vec::new();
    // No launch command here: `clauth providers start` refuses a target pinned
    // by `auth_file` or `auth_entry`, and the report does not say which it is.
    if let Some(model) = report.model.as_deref() {
        facts.push(("model", model.to_string()));
    }
    NativeSnap {
        title: report.id.clone(),
        kind,
        plan: report.data.plan.clone(),
        status: (report.state == ObservationState::AuthRequired).then_some("sign-in needed"),
        facts,
        removes_from_overview: true,
    }
}

fn key(text: &str) -> Span<'static> {
    Span::styled(key_cell(text, KEY_W, KEY_GUTTER), theme::label())
}

fn header_lines(snap: &NativeSnap) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(status) = snap.status {
        let mut spans = vec![key("status")];
        spans.extend(pill(status.to_string(), theme::danger().bold()));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(vec![
        key("type"),
        Span::styled(snap.kind, theme::accent()),
    ]));
    let plan = snap
        .plan
        .as_deref()
        .map(|p| p.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|p| !p.is_empty());
    lines.push(Line::from(vec![
        key("plan"),
        match plan {
            Some(plan) => Span::styled(plan, theme::body()),
            None => Span::styled("—", theme::faint()),
        },
    ]));
    for (name, value) in &snap.facts {
        let value: String = value.chars().filter(|c| !c.is_control()).collect();
        lines.push(Line::from(vec![
            key(name),
            Span::styled(value, theme::body()),
        ]));
    }
    lines
}

fn row_line(row: NativeSetupRow, selected: bool, armed: bool, snap: &NativeSnap) -> Line<'static> {
    let arrow = if selected {
        Span::styled("❯ ", theme::accent().bold())
    } else {
        Span::raw("  ")
    };
    let label = match row {
        NativeSetupRow::Relogin => {
            return Line::from(vec![
                arrow,
                Span::styled("re-login", bold_when(theme::accent(), selected)),
            ]);
        }
        NativeSetupRow::Delete if armed && selected && snap.removes_from_overview => {
            "press again to remove"
        }
        NativeSetupRow::Delete if armed && selected => "press again to delete",
        NativeSetupRow::Delete if snap.removes_from_overview => "remove from overview",
        NativeSetupRow::Delete => "delete account",
    };
    Line::from(vec![arrow, Span::styled(label, theme::danger().bold())])
}

fn row_hint(row: NativeSetupRow, snap: &NativeSnap) -> &'static str {
    match row {
        NativeSetupRow::Relogin => {
            "captures the login `codex login` left in ~/.codex into this account"
        }
        NativeSetupRow::Delete if snap.removes_from_overview => {
            "takes it off the account lists; the providers tab keeps watching it, and add \
             account puts it back"
        }
        NativeSetupRow::Delete => "removes the codex account and its stored login",
    }
}
