//! Metric cards in the TUI: the shared [`crate::usage::cards`] layout as
//! ratatui lines, coloured through the palette, plus the Usage tab's
//! detail pane for monitoring-only / upstream accounts (`monitor:` /
//! `upstream:` observations), which have no profile to render the classic way.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{App, StatusFocus};
use super::super::theme;
use super::format::{spinner_frame, spinner_style};
use super::panes::{draw_scrollbar, section_box_verbatim};
use crate::usage::ProfileActivity;
use crate::usage::cards::{CardCtx, CardLine, Seg, account_body, header_line};
use crate::usage::observation::AccountObservation;

/// One segment as a styled span.
fn span(seg: &Seg) -> Span<'static> {
    let mut style = Style::default().fg(theme::ink_color(seg.ink));
    if seg.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(seg.text.clone(), style)
}

/// A card line as a ratatui line.
pub(super) fn card_line(line: &CardLine) -> Line<'static> {
    Line::from(line.iter().map(span).collect::<Vec<_>>())
}

/// The card lines of `obs` laid out for `width` columns: the header (provider,
/// plan, markers), a blank, then the status lines and every card.
pub(super) fn observation_lines(
    obs: &AccountObservation,
    width: u16,
    now_secs: i64,
    guest_mode: bool,
) -> Vec<Line<'static>> {
    let ctx = CardCtx {
        width: usize::from(width),
        now_secs,
        offset_secs: crate::usage::pretty::local_offset_secs(now_secs),
        guest_mode,
    };
    let mut lines = vec![card_line(&header_line(obs, &ctx)), Line::default()];
    lines.extend(account_body(obs, &ctx).iter().map(card_line));
    lines
}

/// The detail pane for an extra account: its label as the box title, then the
/// shared card layout.
pub(super) fn draw_observation_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    obs: &AccountObservation,
    app: &App,
) {
    let block = section_box_verbatim(
        &obs.label,
        app.usage_detail_focus == StatusFocus::Detail,
        false,
        area.width,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = observation_lines(
        obs,
        inner.width,
        crate::usage::now_epoch_secs(),
        app.guest_mode,
    );
    if app.is_monitor_refreshing(&obs.id)
        && let Some(first) = lines.first_mut()
    {
        let st = spinner_style(ProfileActivity::Fetching);
        first.spans.push(Span::styled(" · ", theme::dim()));
        first.spans.push(Span::styled(
            format!("{} fetching", spinner_frame(app.tick_count)),
            st,
        ));
    }
    let total = lines.len();
    let viewport = usize::from(inner.height);
    let max = if viewport == 0 {
        0
    } else {
        total.saturating_sub(viewport)
    };
    app.usage_detail_max_scroll.set(max);
    app.usage_detail_viewport.set(viewport);
    let scroll = app.usage_detail_scroll.min(max);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(scroll)
                .take(viewport)
                .collect::<Vec<_>>(),
        )
        .style(theme::base()),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll, viewport);
}

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_cards.rs"]
mod tests;
