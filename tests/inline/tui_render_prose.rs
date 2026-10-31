#![allow(clippy::unwrap_used, clippy::expect_used)]

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::super::super::theme;
use super::{cmd, fit, key, plain, spans, wrap};

fn dim() -> Style {
    theme::dim()
}

fn key_style(base: Style) -> Style {
    base.fg(theme::accent_color()).add_modifier(Modifier::BOLD)
}

fn cmd_style(base: Style) -> Style {
    base.fg(theme::accent_color())
        .remove_modifier(Modifier::BOLD)
}

#[test]
fn a_key_renders_accent_bold_between_prose_at_its_own_tier() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let text = format!("press {} to resolve", key("d"));
    assert_eq!(
        spans(&text, dim()),
        vec![
            Span::styled("press ", dim()),
            Span::styled("d", key_style(dim())),
            Span::styled(" to resolve", dim()),
        ]
    );
}

#[test]
fn a_command_renders_accent_and_drops_a_bold_base() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let title = Style::default().fg(theme::text_color()).bold();
    let text = format!("run {} first", cmd("tollgate daemon"));
    let out = spans(&text, title);
    assert_eq!(
        out,
        vec![
            Span::styled("run ", title),
            Span::styled("tollgate daemon", cmd_style(title)),
            Span::styled(" first", title),
        ]
    );
    assert!(!out[1].style.add_modifier.contains(Modifier::BOLD));
    assert!(out[1].style.sub_modifier.contains(Modifier::BOLD));
}

#[test]
fn a_marked_span_keeps_the_base_background() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let base = Style::default()
        .fg(theme::warning_color())
        .bg(theme::bg_warning_color());
    let out = spans(&key("d"), base);
    assert_eq!(out, vec![Span::styled("d", key_style(base))]);
    assert_eq!(out[0].style.bg, Some(theme::bg_warning_color()));
}

#[test]
fn plain_drops_every_marker() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let text = format!(
        "{} or {} then {}",
        key("↵"),
        key("esc"),
        cmd("herdr config check")
    );
    assert_eq!(plain(&text), "↵ or esc then herdr config check");
    assert_eq!(plain("no markers here"), "no markers here");
}

#[test]
fn unmarked_copy_is_one_prose_span() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    assert_eq!(
        spans("[server.admin] stays literal", dim()),
        vec![Span::styled("[server.admin] stays literal", dim())]
    );
    assert!(spans("", dim()).is_empty());
}

#[test]
fn an_opener_with_no_closer_marks_nothing() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let text = "\u{E000}d to resolve";
    assert_eq!(
        spans(text, dim()),
        vec![Span::styled("d to resolve", dim())]
    );
    assert_eq!(plain(text), "d to resolve");
    let cmd_open = "run \u{E001}tollgate login";
    assert_eq!(
        spans(cmd_open, dim()),
        vec![Span::styled("run tollgate login", dim())]
    );
}

#[test]
fn wrap_counts_visible_width_only_and_keeps_styles_across_lines() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    // Visible: "re-login with tollgate login work" (33 chars). At width 20 the
    // 19-cell command cannot finish line 1, so it moves whole to line 2; at
    // width 19 it still fits a line of its own, since the markers' code points
    // never count toward the budget.
    let text = format!("re-login with {}", cmd("tollgate login work"));
    let want = vec![
        vec![Span::styled("re-login with", dim())],
        vec![Span::styled("tollgate login work", cmd_style(dim()))],
    ];
    assert_eq!(wrap(&text, 20, dim()), want, "width 20");
    assert_eq!(wrap(&text, 19, dim()), want, "width 19");
}

/// A command keeps to one row wherever it fits one: prose after it wraps on
/// its own, and a command after a short lead word joins that word's row when
/// both fit.
#[test]
fn wrap_never_breaks_a_command_that_fits_a_row() {
    let text = format!("run {} to install it", cmd("tollgate herdr install"));
    assert_eq!(
        wrap(&text, 26, dim()),
        vec![
            vec![
                Span::styled("run ", dim()),
                Span::styled("tollgate herdr install", cmd_style(dim())),
            ],
            vec![Span::styled("to install it", dim())],
        ]
    );
    assert_eq!(
        wrap(&text, 22, dim()),
        vec![
            vec![Span::styled("run", dim())],
            vec![Span::styled("tollgate herdr install", cmd_style(dim()))],
            vec![Span::styled("to install it", dim())],
        ]
    );
}

/// A command wider than the row breaks at its own spaces, never inside a
/// word, the way prose does.
#[test]
fn wrap_breaks_a_command_wider_than_the_row_at_its_spaces() {
    let text = format!("run {}", cmd("tollgate login some-long-name"));
    assert_eq!(
        wrap(&text, 14, dim()),
        vec![
            vec![
                Span::styled("run ", dim()),
                Span::styled("tollgate", cmd_style(dim())),
            ],
            vec![Span::styled("login", cmd_style(dim()))],
            vec![Span::styled("some-long-name", cmd_style(dim()))],
        ]
    );
}

/// A key whose name holds a space (`⇧ tab`) is one wrap unit like a command:
/// it never breaks across two rows.
#[test]
fn wrap_keeps_a_key_that_holds_a_space_whole() {
    let text = format!("press {} to go back", key("⇧ tab"));
    assert_eq!(
        wrap(&text, 9, dim()),
        vec![
            vec![Span::styled("press", dim())],
            vec![
                Span::styled("⇧ tab", key_style(dim())),
                Span::styled(" to", dim()),
            ],
            vec![Span::styled("go back", dim())],
        ]
    );
}

/// Prose-only copy wraps exactly as base `fd02ea48` did. The rows are literal
/// values taken by running base's `wrap` in a copy, never computed by the code
/// under test: the control-char, whitespace-run, wide-glyph and combining-mark
/// cases each split differently from a naive char count.
#[test]
fn prose_only_wraps_pin_base_rows() {
    let cases: [(&str, usize, &[&str]); 9] = [
        (
            "the quick brown fox jumps over the lazy dog and keeps on running",
            12,
            &[
                "the quick",
                "brown fox",
                "jumps over",
                "the lazy dog",
                "and keeps on",
                "running",
            ],
        ),
        (
            "the quick brown fox jumps over the lazy dog and keeps on running",
            20,
            &[
                "the quick brown fox",
                "jumps over the lazy",
                "dog and keeps on",
                "running",
            ],
        ),
        (
            "the quick brown fox jumps over the lazy dog and keeps on running",
            33,
            &[
                "the quick brown fox jumps over",
                "the lazy dog and keeps on running",
            ],
        ),
        ("abc\u{1b}defghijkl.", 4, &["abcd", "efgh", "ijkl", "."]),
        ("a  b\tc\u{a0}d e", 5, &["a b c", "d e"]),
        ("a  b\tc\u{a0}d e", 4, &["a b", "c d", "e"]),
        (
            "\u{6f22}\u{6f22}\u{6f22} \u{6f22}\u{6f22}",
            5,
            &["\u{6f22}\u{6f22}", "\u{6f22}", "\u{6f22}\u{6f22}"],
        ),
        (
            "\u{26a0}\u{fe0f}\u{26a0}\u{fe0f}\u{26a0}\u{fe0f}",
            4,
            &["\u{26a0}\u{fe0f}\u{26a0}\u{fe0f}", "\u{26a0}\u{fe0f}"],
        ),
        ("e\u{301}e f", 2, &["e\u{301}e", "f"]),
    ];
    for (text, width, want) in cases {
        let got: Vec<String> = wrap(text, width, dim())
            .into_iter()
            .map(|line| line.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        let want: Vec<String> = want.iter().map(|row| (*row).to_string()).collect();
        assert_eq!(got, want, "{text:?} at width {width}");
    }
}

#[test]
fn wrap_fits_a_line_its_markers_would_push_over_the_budget() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    // Visible "press d to resolve" is 18 chars; marked it is 20 code points.
    let text = format!("press {} to resolve", key("d"));
    let lines = wrap(&text, 18, dim());
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        vec![
            Span::styled("press ", dim()),
            Span::styled("d", key_style(dim())),
            Span::styled(" to resolve", dim()),
        ]
    );
}

#[test]
fn wrap_collapses_whitespace_runs_like_wrap_words() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let text = format!("a   {}   b", key("q"));
    assert_eq!(
        wrap(&text, 40, dim()),
        vec![vec![
            Span::styled("a ", dim()),
            Span::styled("q", key_style(dim())),
            Span::styled(" b", dim()),
        ]]
    );
}

#[test]
fn fit_cuts_visible_characters_and_keeps_a_line_that_fits_whole() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    // "  tollgate herdr install" is 24 visible chars, 26 marked.
    let text = format!("  {}", cmd("tollgate herdr install"));
    assert_eq!(
        fit(&text, 24, dim()),
        vec![
            Span::styled("  ", dim()),
            Span::styled("tollgate herdr install", cmd_style(dim())),
        ]
    );
    assert_eq!(
        fit(&text, 12, dim()),
        vec![
            Span::styled("  ", dim()),
            Span::styled("tollgate ", cmd_style(dim())),
            Span::styled("…", dim()),
        ]
    );
    assert!(fit(&text, 0, dim()).is_empty());
}

#[test]
fn the_literal_macros_mark_exactly_as_the_functions_do() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    assert_eq!(super::key_lit!("\u{21b5}"), key("\u{21b5}"));
    assert_eq!(
        super::cmd_lit!("herdr config check"),
        cmd("herdr config check")
    );
}

#[test]
fn wrap_of_empty_text_is_one_empty_line() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    assert_eq!(wrap("", 10, dim()), vec![Vec::<Span<'static>>::new()]);
}

#[test]
fn mark_commands_marks_a_backticked_command_and_leaves_other_spans_literal() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    use super::mark_commands;
    assert_eq!(
        mark_commands("revoked: run `tollgate login work`"),
        format!("revoked: run {}", cmd("tollgate login work"))
    );
    assert_eq!(
        mark_commands("add `[server.admin]` (error `internal`)"),
        "add `[server.admin]` (error `internal`)"
    );
    assert_eq!(
        mark_commands("`herdr config check` and `codex login` then `x`"),
        format!(
            "{} and {} then `x`",
            cmd("herdr config check"),
            cmd("codex login")
        )
    );
    // A command led by the binary's full path is still a command.
    assert_eq!(
        mark_commands("`/opt/bin/herdr check --config /etc/s.toml` refused"),
        format!(
            "{} refused",
            cmd("/opt/bin/herdr check --config /etc/s.toml")
        )
    );
    assert_eq!(
        mark_commands("`/usr/bin/herdr.exe config check`"),
        cmd("/usr/bin/herdr.exe config check")
    );
    // A binary path holding a space, in either separator.
    assert_eq!(
        mark_commands("`/Users/A B/bin/herdr check --config /x.toml` refused"),
        format!(
            "{} refused",
            cmd("/Users/A B/bin/herdr check --config /x.toml")
        )
    );
    assert_eq!(
        mark_commands("`C:\\Program Files\\herdr\\herdr.exe check` refused"),
        format!(
            "{} refused",
            cmd("C:\\Program Files\\herdr\\herdr.exe check")
        )
    );
    // A path after another word is an argument, not the binary.
    assert_eq!(
        mark_commands("`foo /usr/bin/tollgate x` y"),
        "`foo /usr/bin/tollgate x` y"
    );
    // A binary path past the 4096-byte cap is no binary.
    let long = format!("`/{}tollgate login x`", "d/".repeat(2100));
    assert_eq!(mark_commands(&long), long);
    // A trailing space is no second word.
    assert_eq!(mark_commands("`tollgate ` x"), "`tollgate ` x");
    // Two words that do not open with a CLI are not a command.
    assert_eq!(
        mark_commands("call `GET /api/v1/panes` first"),
        "call `GET /api/v1/panes` first"
    );
    // A bare tool name is a mention, not a command to type.
    assert_eq!(
        mark_commands("install `tollgate` first"),
        "install `tollgate` first"
    );
    // An unclosed backtick and a span crossing a line stay as written.
    assert_eq!(mark_commands("run `tollgate login"), "run `tollgate login");
    assert_eq!(
        mark_commands("`tollgate login\nwork` here"),
        "`tollgate login\nwork` here"
    );
    assert_eq!(mark_commands(""), "");
}

/// A control character inside a hard-split word changes nothing the screen
/// shows: every later chunk keeps its graphemes and the word's tail survives.
/// The rows are base `fd02ea48` output, measured in a copy.
#[test]
fn a_control_char_never_shifts_a_hard_split() {
    assert_eq!(
        super::super::panes::wrap_words("abc\u{1b}defghijkl.", 4),
        vec!["abcd", "efgh", "ijkl", "."]
    );
    let got: Vec<String> = wrap("abc\u{1b}defghijkl.", 4, dim())
        .into_iter()
        .map(|line| line.iter().map(|span| span.content.as_ref()).collect())
        .collect();
    assert_eq!(got, vec!["abcd", "efgh", "ijkl", "."]);
}

/// Punctuation pressed against a command belongs to its word: when the whole
/// word no longer fits beside the row, the command still takes a row it fits
/// and the glued punctuation opens the next row — never a break inside the
/// command.
#[test]
fn wrap_cuts_a_glued_command_at_the_command_edges() {
    let text = format!("run {})", cmd("tollgate login glm0 --api-key <key>"));
    assert_eq!(
        wrap(&text, 35, dim()),
        vec![
            vec![Span::styled("run", dim())],
            vec![Span::styled(
                "tollgate login glm0 --api-key <key>",
                cmd_style(dim())
            )],
            vec![Span::styled(")", dim())],
        ]
    );
    // Leading glue stays on the row it fits, then the command takes its own.
    let text = format!("({})", cmd("QA QB"));
    assert_eq!(
        wrap(&text, 6, dim()),
        vec![
            vec![
                Span::styled("(", dim()),
                Span::styled("QA QB", cmd_style(dim())),
            ],
            vec![Span::styled(")", dim())],
        ]
    );
}

/// Whitespace inside a fitting command collapses to one ordinary space and a
/// trailing one drops, so the rendered cells paste as the command; a
/// non-breaking space is data, never a break point or a collapse.
#[test]
fn wrap_collapses_whitespace_inside_a_fitting_command() {
    assert_eq!(
        wrap(&format!("run {}", cmd("tollgate\tlogin")), 40, dim()),
        vec![vec![
            Span::styled("run ", dim()),
            Span::styled("tollgate login", cmd_style(dim())),
        ]]
    );
    assert_eq!(
        wrap(&format!("x {}", cmd("a   b ")), 40, dim()),
        vec![vec![
            Span::styled("x ", dim()),
            Span::styled("a b", cmd_style(dim())),
        ]]
    );
    assert_eq!(
        wrap(&format!("n {}", cmd("a\u{a0}b")), 40, dim()),
        vec![vec![
            Span::styled("n ", dim()),
            Span::styled("a\u{a0}b", cmd_style(dim())),
        ]]
    );
}

fn row_texts(text: &str, width: usize) -> Vec<String> {
    wrap(text, width, dim())
        .into_iter()
        .map(|line| line.iter().map(|span| span.content.as_ref()).collect())
        .collect()
}

/// A command whose own text ends in whitespace keeps one space before prose
/// pressed against its closing marker: the two words never fuse on screen.
#[test]
fn a_command_ending_in_whitespace_keeps_a_space_before_glued_prose() {
    assert_eq!(
        row_texts(&format!("run {}x", cmd("tollgate login ")), 40),
        vec!["run tollgate login x"]
    );
    assert_eq!(
        row_texts(&format!("run {}x", cmd("tollgate login\t")), 40),
        vec!["run tollgate login x"]
    );
}

/// A command holding only whitespace draws nothing and adds no blank cell
/// between the prose words around it.
#[test]
fn a_whitespace_only_command_adds_no_blank_cell() {
    assert_eq!(row_texts(&format!("a {} b", cmd("  ")), 40), vec!["a b"]);
}

#[test]
fn wrapped_key_styling_reaches_the_terminal_buffer() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let marked = format!("press {} to resolve", key("d"));
    let rows = wrap(&marked, 18, dim())
        .into_iter()
        .map(ratatui::text::Line::from)
        .collect();
    let buf = crate::testutil::lines_buffer(rows, 18);
    crate::testutil::assert_prose_part(&buf, "press d to resolve", "d", true);
}

#[test]
fn fit_truncates_wide_text_by_cells_and_preserves_graphemes_and_styles() {
    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let family = "👨‍👩‍👧‍👦";
    for (marked, width, expected) in [
        (cmd("漢漢漢"), 5, "漢漢…".to_string()),
        (cmd(&format!("{family}x")), 2, "…".to_string()),
        (cmd(&format!("{family}xy")), 3, format!("{family}…")),
        (cmd("e\u{301}e\u{301}x"), 2, "e\u{301}…".to_string()),
        (cmd("漢"), 1, "…".to_string()),
    ] {
        let fitted = fit(&marked, width, dim());
        let visible: String = fitted.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(visible, expected);
        assert!(fitted.iter().map(Span::width).sum::<usize>() <= width);
        assert_eq!(fitted.last().unwrap().style, dim());
        for span in &fitted[..fitted.len() - 1] {
            assert_eq!(span.style, cmd_style(dim()));
        }
    }
    assert_eq!(
        fit(&key("e\u{301}"), 1, dim()),
        spans(&key("e\u{301}"), dim())
    );
    assert_eq!(fit(&cmd(family), 2, dim()), spans(&cmd(family), dim()));
}

#[test]
fn fit_budgets_display_cells_and_keeps_a_wide_glyph_whole() {
    assert_eq!(fit("日本語", 5, dim()), vec![Span::styled("日本…", dim())]);
    assert_eq!(fit("日本語", 6, dim()), vec![Span::styled("日本語", dim())]);
    assert_eq!(fit("🧭🧭", 3, dim()), vec![Span::styled("🧭…", dim())]);
}

#[test]
fn fit_keeps_a_multicodepoint_grapheme_whole() {
    assert_eq!(fit("⚠️⚠️", 3, dim()), vec![Span::styled("⚠️…", dim())]);
    assert_eq!(fit("👨‍👩‍👧👨‍👩‍👧", 3, dim()), vec![Span::styled("👨‍👩‍👧…", dim())]);
}

#[test]
fn fit_drops_controls_before_measuring() {
    assert_eq!(fit("\n⚠️x", 3, dim()), vec![Span::styled("⚠️x", dim())]);
    assert_eq!(
        fit("\te\u{301}xyz", 2, dim()),
        vec![Span::styled("e\u{301}…", dim())]
    );
}

#[test]
fn fit_budgets_lam_alef_per_grapheme() {
    assert_eq!(fit("لالالا", 3, dim()), vec![Span::styled("لا…", dim())]);
}
