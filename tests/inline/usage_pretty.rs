#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `tollgate usage` sinks: when colour applies, the plain path (the card
//! golden, no escapes), the ANSI path (palette colours at each tier, and
//! nothing but escapes added), and the Waybar line.

use super::*;
use crate::tui::theme::Tier;

include!("../fixtures/usage_cards_sample.rs");

const GOLDEN_80: &str = include_str!("../fixtures/usage_cards_80.txt");

fn ctx(width: usize) -> CardCtx {
    CardCtx {
        width,
        now_secs: SAMPLE_NOW,
        offset_secs: SAMPLE_OFFSET,
        guest_mode: true,
    }
}

/// Drop every `ESC [ … m` sequence.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn colour_needs_a_tty_no_no_color_and_no_plain() {
    assert_eq!(text_mode(true, None, false), TextMode::Colour);
    assert_eq!(text_mode(false, None, false), TextMode::Plain, "not a TTY");
    assert_eq!(
        text_mode(true, Some("1"), false),
        TextMode::Plain,
        "NO_COLOR"
    );
    assert_eq!(text_mode(true, None, true), TextMode::Plain, "--plain");
    // An empty NO_COLOR does not count (no-color.org).
    assert_eq!(text_mode(true, Some(""), false), TextMode::Colour);
}

#[test]
fn the_plain_report_is_the_card_golden_without_escapes() {
    let got = render_text(&sample_accounts(), &ctx(80), TextMode::Plain);
    assert!(!got.contains('\x1b'));
    assert_eq!(got, GOLDEN_80);
}

#[test]
fn the_coloured_report_adds_only_palette_escapes() {
    let _tier = crate::testutil::TierSandbox::new(Tier::Full);
    let got = render_text(&sample_accounts(), &ctx(80), TextMode::Colour);
    assert_eq!(
        strip_ansi(&got),
        GOLDEN_80,
        "colour changes nothing but colour"
    );
    // CRITICAL in bold Catppuccin red, HIGH in the orange accent_2.
    assert!(
        got.contains("\x1b[1;38;2;243;139;168m CRITICAL\x1b[0m"),
        "{got}"
    );
    assert!(got.contains("\x1b[1;38;2;217;119;87m HIGH\x1b[0m"), "{got}");
    // The active account's name in accent_2.
    assert!(got.contains("\x1b[1;38;2;217;119;87mwork\x1b[0m"));
}

#[test]
fn the_compatible_tier_speaks_xterm_256() {
    let _tier = crate::testutil::TierSandbox::new(Tier::Compatible);
    let got = render_text(&sample_accounts(), &ctx(80), TextMode::Colour);
    assert!(!got.contains("38;2;"));
    assert!(got.contains("\x1b[1;38;5;211m CRITICAL\x1b[0m"), "{got}");
    assert_eq!(strip_ansi(&got), GOLDEN_80);
}

#[test]
fn the_waybar_line_leads_with_the_active_account() {
    let out = waybar(&sample_accounts(), SAMPLE_NOW, SAMPLE_OFFSET);
    let json = serde_json::to_string(&out).unwrap();
    assert_eq!(
        json,
        r#"{"text":"42% · 3h 05m","tooltip":"● work (Anthropic): 5h 42% resets 15:05 · 7d 81% resets 13:00 · 7d opus 100% resets 13:00\nor-main (OpenRouter): Credit balance $13.67 · Key cap $9.00\noll-main (Ollama Cloud): 5h 93% · Credit balance $0.40 · ⚠ ollama.com did not answer\n● personal (Anthropic): 5h 29% resets 13:12","class":"critical","percentage":42}"#
    );
    assert!(!json.contains('\n'), "one line for Waybar");
}

#[test]
fn waybar_falls_back_to_money_then_to_an_empty_state() {
    let mut accounts = sample_accounts();
    accounts.retain(|o| o.label == "or-main");
    let out = waybar(&accounts, SAMPLE_NOW, SAMPLE_OFFSET);
    assert_eq!(out.text, "$13.67");
    assert_eq!(out.class, "high", "the key cap at 82% is HIGH");
    assert_eq!(out.percentage, None);
    assert!(!serde_json::to_string(&out).unwrap().contains("percentage"));

    let empty = waybar(&[], SAMPLE_NOW, SAMPLE_OFFSET);
    assert_eq!(
        serde_json::to_string(&empty).unwrap(),
        r#"{"text":"—","tooltip":"tollgate: no accounts","class":"none"}"#
    );
}

/// With no active account, the worst-graded one leads.
#[test]
fn without_an_active_account_the_worst_leads() {
    let mut accounts = sample_accounts();
    for o in &mut accounts {
        o.active = false;
    }
    accounts.remove(0);
    let lead = lead_account(&accounts, SAMPLE_NOW).unwrap();
    assert_eq!(lead.label, "oll-main");
}

/// Nothing an observation carries besides its figures leaks into the bar: not
/// the endpoint.
#[test]
fn the_waybar_line_carries_no_endpoint() {
    let mut accounts = sample_accounts();
    accounts[1].endpoint = Some("https://openrouter.ai/api".to_string());
    let json = serde_json::to_string(&waybar(&accounts, SAMPLE_NOW, SAMPLE_OFFSET)).unwrap();
    assert!(!json.contains("openrouter.ai"));
}

fn parse_usage(args: &[&str]) -> Result<crate::cli::Command, clap::Error> {
    use clap::Parser as _;
    let argv = std::iter::once("tollgate").chain(args.iter().copied());
    crate::cli::Cli::try_parse_from(argv).map(|c| c.command.expect("a subcommand"))
}

#[test]
fn the_usage_flags_parse_and_conflict_as_documented() {
    let crate::cli::Command::Usage {
        json,
        plain,
        waybar,
        watch,
        ..
    } = parse_usage(&["usage", "--waybar", "--watch", "5"]).unwrap()
    else {
        panic!("usage parses to Command::Usage");
    };
    assert_eq!((json, plain, waybar, watch), (false, false, true, Some(5)));
    assert!(matches!(
        parse_usage(&["usage", "--plain", "--provider", "openrouter"]).unwrap(),
        crate::cli::Command::Usage { plain: true, .. }
    ));
    // --json is its own format; --plain and --waybar exclude each other; a
    // zero-second watch would spin.
    assert!(parse_usage(&["usage", "--json", "--plain"]).is_err());
    assert!(parse_usage(&["usage", "--json", "--waybar"]).is_err());
    assert!(parse_usage(&["usage", "--plain", "--waybar"]).is_err());
    assert!(parse_usage(&["usage", "--watch", "0"]).is_err());
}
