use super::super::panes::wrap_words;

#[test]
fn short_line_unchanged() {
    assert_eq!(wrap_words("hello world", 36), vec!["hello world"]);
}

#[test]
fn wraps_at_word_boundary() {
    let out = wrap_words("terminal too small · enlarge for full layout", 36);
    assert_eq!(out.len(), 2, "expected 2 wrapped lines, got {out:?}");
    for l in &out {
        assert!(
            l.chars().count() <= 36,
            "line exceeds cap: {l:?} ({} chars)",
            l.chars().count()
        );
    }
}

#[test]
fn empty_input_yields_one_empty_line() {
    assert_eq!(wrap_words("", 36), vec![""]);
}

#[test]
fn single_word_exceeding_cap_hard_breaks() {
    let long_word = "a".repeat(80);
    let out = wrap_words(&long_word, 36);
    assert_eq!(out.len(), 3);
    for l in &out {
        assert!(l.chars().count() <= 36);
    }
}

#[test]
fn wide_glyphs_wrap_by_cells() {
    // `漢` takes two cells: three fill a 6-cell line, and a 5-cell line holds
    // two before the word is split.
    assert_eq!(wrap_words("漢漢漢 漢漢", 6), vec!["漢漢漢", "漢漢"]);
    assert_eq!(wrap_words("漢漢漢 漢漢", 5), vec!["漢漢", "漢", "漢漢"]);
}

#[test]
fn an_emoji_presentation_sequence_splits_as_one_two_cell_glyph() {
    // `⚠️` is U+26A0 + U+FE0F: two chars, one grapheme, two cells.
    let w = "\u{26A0}\u{FE0F}";
    assert_eq!(
        wrap_words(&w.repeat(10), 6),
        vec![w.repeat(3), w.repeat(3), w.repeat(3), w.to_string()]
    );
}

#[test]
fn col_width_equals_content_width_plus_chrome() {
    let msg = "· enlarge for full layout";
    let content_cap: u16 = 36;
    let max_content_width = [msg]
        .iter()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap()
        .min(content_cap);
    let col_width = max_content_width + 3;
    assert_eq!(max_content_width, 25);
    assert_eq!(col_width, 28);
}

use super::super::super::app::{App, ToastKind};
use crate::profile::{AppConfig, AppState};
use ratatui::{Terminal, backend::TestBackend, text::Span};

fn rendered_toast(kind: ToastKind, body: String) -> (u16, Vec<String>) {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.toast(kind, body);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let buf = terminal.backend().buffer();
    let bar_x = (0..80).find(|&x| buf[(x, 2)].symbol() == "┃").unwrap();
    let rows = (2..24)
        .take_while(|&y| buf[(bar_x, y)].symbol() == "┃")
        .map(|y| {
            let mut row = String::new();
            let mut x = bar_x;
            while x < 78 {
                let symbol = buf[(x, y)].symbol();
                row.push_str(symbol);
                x += Span::raw(symbol).width().max(1) as u16;
            }
            row.trim_end().to_string()
        })
        .collect();
    (bar_x, rows)
}

#[test]
fn toast_height_and_ellipsis_follow_cell_width() {
    let _home = crate::testutil::HomeSandbox::new();
    for glyph in ["x", "漢", "⚠️"] {
        let (_, rows) = rendered_toast(
            ToastKind::Danger,
            format!("move failed\n{}", glyph.repeat(200)),
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "┃ error: move failed");
        assert!(rows[2].ends_with('…'));
        for row in rows {
            assert!(Span::raw(row).width() <= 59);
        }
    }
}

#[test]
fn truncated_rows_do_not_reserve_toast_width() {
    let _home = crate::testutil::HomeSandbox::new();
    let (bar_x, rows) = rendered_toast(ToastKind::Danger, format!("a\nb\nc\n{}", "y".repeat(50)));
    assert_eq!(rows, vec!["┃ error: a", "┃ b", "┃ c…"]);
    assert_eq!(bar_x, 78 - (8 + 3));
}

#[test]
fn three_rows_stay_whole_and_every_severity_has_a_word() {
    let _home = crate::testutil::HomeSandbox::new();
    for (kind, label) in [
        (ToastKind::Info, "info"),
        (ToastKind::Success, "success"),
        (ToastKind::Warning, "warning"),
        (ToastKind::Danger, "error"),
    ] {
        let (_, rows) = rendered_toast(kind, "head\nfirst detail\nsecond detail".to_string());
        assert_eq!(
            rows,
            vec![
                format!("┃ {label}: head"),
                "┃ first detail".to_string(),
                "┃ second detail".to_string()
            ]
        );
    }
}

#[test]
fn shared_commands_in_toasts_keep_their_accent_without_bold() {
    use ratatui::style::Modifier;
    let _home = crate::testutil::HomeSandbox::new();
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: vec![],
    });
    app.toast(
        ToastKind::Danger,
        crate::format::login_expired(&"work".into()).toast(),
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|f| super::draw(f, f.area(), &app)).unwrap();
    let cells =
        crate::testutil::needle_cells(terminal.backend().buffer(), "tollgate login work").unwrap();
    for cell in cells {
        assert_eq!(cell.fg, crate::tui::theme::accent_color());
        assert!(!cell.modifier.contains(Modifier::BOLD));
    }
}
