//! Bottom strip: key hints, or a footer alert in place when one is active.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, ConfigFocus, ConfigRow, FallbackHint, FooterAlert, GLOBAL_CONFIG_ROWS, GlobalConfigRow,
    HERDR_OPTIONS, HerdrOption, KeyOwner, LoginSession, Modal, PluginFocus, StatusFocus, Tab,
    TokenView, build_action_menu, config_rows, fallback_hint, has_sub_focus, herdr_config_writable,
    keyboard_owner,
};
use super::super::theme;
use super::format::spinner_frame;

const TAB_NAV: (&str, &str) = ("←→", "tabs");

/// The guest-mode footer hint: the command that ends guest mode.
pub(crate) const GUEST_IMPORT_HINT: (&str, &str) = ("import:", "tollgate import clauth --dry-run");

/// A typed field's whole grammar: `q` is data there, so it gets no hint. Esc
/// puts the field back to its saved value, and says so in the field's own
/// terms, since beside a login in flight a bare `cancel` reads as the login's.
const TYPED_FIELD: &[(&str, &str)] = &[("↵", "save"), ("←→", "caret"), ("esc", "revert")];

/// The `+ new` form's typed field: ⏎ and esc both end the edit and keep the
/// typed value; nothing saves until the form's own `create account` row.
const NEW_ACCOUNT_FIELD: &[(&str, &str)] = &[("↵", "done"), ("←→", "caret"), ("esc", "done")];

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // 1-col breathing room on each side; the alert row (which replaces this in
    // place) shares the same inset so the left margin never jumps.
    let area = inset_x(area, 1);

    let owner = keyboard_owner(app);

    // A login in flight owns the footer, independent of `footer_alert` so key
    // handling that clears alerts can't hide it.
    if let Some(session) = &app.login {
        // An open editor takes esc/q before the login's cancel does, so its own
        // grammar is the hint. Any open modal owns esc/q too (the login modal
        // collapses, others handle their own keys), so the hint flips to
        // `q back` for the whole stack; the login modal's open code field is
        // the exception, where `q` is data and ↵ submits.
        let keys = match owner.and_then(owner_hints) {
            Some(hints) => LoginKeys::Owner(hints),
            None => match app.modals.last() {
                Some(Modal::Login) if session.paste_field.is_some() => LoginKeys::Paste,
                Some(_) => LoginKeys::Back,
                None => LoginKeys::Cancel,
            },
        };
        draw_login(frame, area, session, keys, app.tick_count);
        return;
    }

    // A live alert replaces the hint bar in place — one footer row, never stacked.
    if let Some(alert) = &app.footer_alert {
        draw_alert(frame, area, alert);
        return;
    }

    // An editor owns the keys it claims, `←→` included, so its own grammar is
    // the whole row: `q` only where the editor itself binds it, `? help` only
    // where it lets `?` through. Every owner takes the arrows, so `←→ tabs`
    // rides only while nothing owns the keyboard.
    let mut hints: Vec<(&str, &str)> = match owner.zip(owner.and_then(owner_hints)) {
        Some((owner, own)) => {
            let mut hints = own.to_vec();
            if !owner.claims(KeyCode::Char('?')) {
                let at = hints
                    .iter()
                    .position(|(key, _)| *key == "q")
                    .unwrap_or(hints.len());
                hints.insert(at, ("?", "help"));
            }
            hints
        }
        None => owner
            .is_none()
            .then_some(TAB_NAV)
            .into_iter()
            .chain(tab_hints(app))
            .collect(),
    };

    // `a` opens nothing where the menu is empty: a tab with no action of its
    // own while a daemon start or stop is in flight. Reading the real menu
    // keeps the hint honest per row instead of leaving each arm's literal to
    // drift.
    if build_action_menu(app).items.is_empty() {
        hints.retain(|(key, _)| *key != "a");
    }

    // Guest mode names its way out, the `[ guest ]` pill's footer line
    // (import spec §2.5): last, so it is the first hint a narrow row sheds.
    if app.guest_mode && owner.is_none() {
        hints.push(GUEST_IMPORT_HINT);
    }

    shed_to_width(&mut hints, area.width as usize);

    let mut spans: Vec<Span<'_>> = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme::faint()));
        }
        spans.push(Span::styled(*key, theme::accent().bold()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, theme::dim()));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// An owner's own grammar, the whole hint row while it holds the keyboard
/// bar the `? help` that [`draw`] derives from what the owner claims. `None`
/// for the modal stack, which has no hint set of its own: the screen's hints
/// stay under it.
fn owner_hints(owner: KeyOwner) -> Option<&'static [(&'static str, &'static str)]> {
    match owner {
        KeyOwner::Modal => None,
        KeyOwner::SetupField
        | KeyOwner::MemberField
        | KeyOwner::RefreshInterval
        | KeyOwner::ContextNudge
        | KeyOwner::WeeklyThreshold
        | KeyOwner::HerdrTag => Some(TYPED_FIELD),
        KeyOwner::NewAccountField => Some(NEW_ACCOUNT_FIELD),
        // `←→` walk the chips and each space saves, so nothing reads as a
        // commit; `q` leaves the picker. ⏎ leaves it too, so it takes no group
        // of its own beside `q back`: three screen-specific groups at most.
        KeyOwner::DayPicker => Some(&[
            ("←→", "day"),
            ("space", "toggle"),
            ("↑↓", "row"),
            ("q", "back"),
        ]),
    }
}

/// The screen's own keys, then `q`: the row under `←→ tabs` while nothing owns
/// the keyboard, and under a modal.
fn tab_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    // `q` label: "back" in a sub-focus, "quit" at top level.
    // (While armed the alert row shows instead, so this label stays "quit".)
    let q_label: &str = if has_sub_focus(app) { "back" } else { "quit" };

    let tail: &[(&str, &str)] = match app.tab {
        // A read-only row reorders nothing; ⏎ opens it on the Usage tab.
        Tab::Overview
            if app.harness_filter.shows_claude() && app.overview_selected_extra().is_some() =>
        {
            &[
                ("↵", "usage"),
                ("a", "actions"),
                ("c", "harness"),
                ("?", "help"),
            ]
        }
        Tab::Overview => &[
            ("⇧↑↓", "reorder"),
            ("a", "actions"),
            ("c", "harness"),
            ("?", "help"),
        ],
        Tab::Usage => &[
            ("↑↓", "account"),
            ("r", "refresh account"),
            ("a", "actions"),
            ("?", "help"),
        ],
        Tab::Tokens => match app.token_view {
            TokenView::Dashboard => &[
                ("↵", "models"),
                ("r", "reload"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models => &[
                ("↑↓", "model"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
        },
        Tab::Setup => match app.config_focus {
            ConfigFocus::Profiles => &[
                ("↑↓", "account"),
                ("↵", "configure"),
                ("n", "new"),
                ("a", "actions"),
                ("?", "help"),
            ],
            ConfigFocus::Actions => {
                // Row-aware: the `model` row cycles on space; env rows edit a value
                // or open the add-env key editor.
                match config_rows(app).get(app.config_action_cursor) {
                    Some(ConfigRow::Model) => &[
                        ("↑↓", "row"),
                        ("space", "cycle"),
                        ("↵", "custom"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    Some(ConfigRow::EnvEntry(_)) => &[
                        ("↑↓", "row"),
                        ("↵", "edit value"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    Some(ConfigRow::EnvAdd) => &[
                        ("↑↓", "row"),
                        ("↵", "add env"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    Some(ConfigRow::ModelOverrideAdd) => &[
                        ("↑↓", "row"),
                        ("↵", "add override"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                    _ => &[
                        ("↑↓", "row"),
                        ("↵", "edit / toggle"),
                        ("a", "actions"),
                        ("?", "help"),
                    ],
                }
            }
        },
        Tab::Config => {
            if GLOBAL_CONFIG_ROWS
                .get(app.global_config_cursor)
                .is_some_and(|r| {
                    matches!(
                        r,
                        GlobalConfigRow::RefreshInterval
                            | GlobalConfigRow::ContextNudge
                            | GlobalConfigRow::WeeklyThreshold
                    )
                })
            {
                &[
                    ("↑↓", "row"),
                    ("space", "cycle"),
                    ("↵", "custom"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "cycle / toggle"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            }
        }
        Tab::Status => match app.status.focus {
            StatusFocus::List => &[
                ("↑↓", "incident"),
                ("↵", "open"),
                ("r", "refresh"),
                ("a", "actions"),
                ("?", "help"),
            ],
            StatusFocus::Detail => &[("↑↓", "scroll"), ("a", "actions"), ("?", "help")],
        },
        Tab::Plugin => plugin_hints(app),
        Tab::Fallback => match fallback_hint(app) {
            FallbackHint::Empty => &[("a", "actions"), ("?", "help")],
            FallbackHint::ChainMember => &[
                ("↑↓", "move"),
                ("⇧↑↓", "reorder"),
                ("↵", "open"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::ChainAdd => &[
                ("↑↓", "move"),
                ("↵", "add"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailThreshold => &[
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailWeeklyAt => &[
                ("↑↓", "row"),
                ("+", "raise"),
                ("-", "lower"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailCheckWeekly
            | FallbackHint::DetailCheckScoped
            | FallbackHint::DetailLastResort
            | FallbackHint::DetailPreferred => &[
                ("↑↓", "row"),
                ("space/↵", "toggle"),
                ("a", "actions"),
                ("?", "help"),
            ],
            // The row's own keys lead so the narrow-width trim, which drops the
            // rightmost non-essential hint first, sheds `↑↓ row` before them.
            FallbackHint::DetailPreferredDays => &[
                ("space", "preset"),
                ("↵", "days"),
                ("↑↓", "row"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailMaxSpend => &[
                ("↑↓", "row"),
                ("↵", "type"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemove => &[
                ("↑↓", "row"),
                ("↵", "remove"),
                ("a", "actions"),
                ("?", "help"),
            ],
            FallbackHint::DetailRemoveArmed => {
                &[("↵", "confirm remove"), ("esc", "cancel"), ("?", "help")]
            }
            FallbackHint::DetailAdd => &[
                ("↑↓", "pick"),
                ("↵", "add"),
                ("a", "actions"),
                ("?", "help"),
            ],
        },
    };

    let mut hints = tail.to_vec();
    // The armed remove's way out is its `esc cancel`: `q` ascends out of the
    // card the same way there, so `q back` would be a second name for it.
    if !(app.tab == Tab::Fallback && fallback_hint(app) == FallbackHint::DetailRemoveArmed) {
        hints.push(("q", q_label));
    }
    hints
}

/// Measured degradation for narrow terminals: while the row of `hints`, each
/// `key label` group 3 spaces from the next, overflows `width` cells, drop the
/// rightmost non-essential hint. Navigation the screen offers no other way
/// (`←→ tabs`, the picker's `←→ day`), discoverability (`? help`), exits
/// (`q`/`esc`) and armed confirms always survive; a field's `←→ caret` sheds,
/// since inside a text field those keys are self-evident. On a desktop-width
/// terminal everything fits and nothing changes.
fn shed_to_width(hints: &mut Vec<(&str, &str)>, width: usize) {
    let row_width = |hints: &[(&str, &str)]| -> usize {
        let cells: usize = hints
            .iter()
            .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
            .sum();
        cells + hints.len().saturating_sub(1) * 3
    };
    let essential = |(key, label): &(&str, &str)| {
        matches!(
            (*key, *label),
            ("←→", "tabs" | "day") | ("?" | "q" | "esc", _)
        ) || label.starts_with("confirm")
    };
    while row_width(hints) > width {
        match hints.iter().rposition(|h| !essential(h)) {
            Some(i) => {
                hints.remove(i);
            }
            None => break,
        }
    }
}

/// Plugin tab hints. `f` only fixes a row that actually offers one — never
/// advertised where pressing it is a no-op. The herdr detail walks focusable
/// option rows instead of scrolling, so its hints name the row's own keys.
fn plugin_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    match app.plugin.focus {
        PluginFocus::List => {
            if app.plugin.selected_fix().is_some() {
                &[
                    ("↑↓", "row"),
                    ("↵", "detail"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("↵", "detail"),
                    ("r", "refresh"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            }
        }
        PluginFocus::Detail => plugin_detail_hints(app),
    }
}

/// Plugin detail hints, row-aware for the herdr options section: the
/// tag-refresh row advertises its stepper keys, the delegate-row row its
/// confirm — and an inert delegate-row row advertises no activation key at
/// all, since the key is a no-op there.
fn plugin_detail_hints(app: &App) -> &'static [(&'static str, &'static str)] {
    let fix = app.plugin.selected_fix().is_some();
    if !app
        .plugin
        .selected_check()
        .is_some_and(|c| c.label == "herdr")
    {
        return if fix {
            &[
                ("↑↓", "scroll"),
                ("r", "refresh"),
                ("f", "fix"),
                ("a", "actions"),
                ("?", "help"),
            ]
        } else {
            &[
                ("↑↓", "scroll"),
                ("r", "refresh"),
                ("a", "actions"),
                ("?", "help"),
            ]
        };
    }
    // `r` and `f` keep working while the options rows hold the cursor, so they
    // keep their hints (f only when the check offers a fix).
    match HERDR_OPTIONS.get(app.plugin.herdr_options_cursor) {
        Some(HerdrOption::TagRefresh) => {
            if fix {
                &[
                    ("↑↓", "row"),
                    ("+", "raise"),
                    ("-", "lower"),
                    ("↵", "type"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("+", "raise"),
                    ("-", "lower"),
                    ("↵", "type"),
                    ("r", "refresh"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            }
        }
        Some(HerdrOption::DelegateRowText) if herdr_config_writable(app) => {
            if fix {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "rewrite row"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "rewrite row"),
                    ("r", "refresh"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            }
        }
        // The inert delegate-row row advertises no activation key — it is a
        // no-op there.
        Some(HerdrOption::DelegateRowText) => {
            if fix {
                &[
                    ("↑↓", "row"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("r", "refresh"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            }
        }
        _ => {
            if fix {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "cycle / toggle"),
                    ("r", "refresh"),
                    ("f", "fix"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            } else {
                &[
                    ("↑↓", "row"),
                    ("space/↵", "cycle / toggle"),
                    ("r", "refresh"),
                    ("a", "actions"),
                    ("?", "help"),
                ]
            }
        }
    }
}

/// Shrink a rect by `pad` columns on each side (clamped), leaving the row intact.
fn inset_x(area: Rect, pad: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(pad),
        width: area.width.saturating_sub(pad.saturating_mul(2)),
        ..area
    }
}

/// Render a footer alert in place of the hint bar.
/// `! <message>` — glyph in `WARNING`, message in `TEXT_DIM`.
fn draw_alert(frame: &mut Frame<'_>, area: Rect, alert: &FooterAlert) {
    let FooterAlert::Warn(msg) = alert;
    let spans = vec![
        Span::styled("! ", Style::default().fg(theme::warning_color())),
        Span::styled(msg.as_str(), theme::dim()),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}

/// What esc/q/↵ do this frame for the login line's trailing hint.
#[derive(Clone, Copy)]
enum LoginKeys {
    /// A modal is open: `q` steps back out of it.
    Back,
    /// The login modal is collapsed: `esc` cancels the login.
    Cancel,
    /// The login modal's code field is open: `↵` submits, `esc` restores the
    /// `p  paste code` row; `q` is data.
    Paste,
    /// An open editor takes the keys first: its own grammar.
    Owner(&'static [(&'static str, &'static str)]),
}

/// Login-in-progress line. Independent of `footer_alert` so key handling that
/// clears alerts never hides it. The live stage renders in the login modal;
/// this row is the collapsed view and carries the name alone. The trailing
/// hint tracks what esc/q actually do this frame: an open editor takes them
/// first (its own row); with any modal open they go to the modal (`q back`;
/// the open code field takes `↵ submit   esc back`); collapsed, both cancel
/// the login (`esc cancel`).
fn draw_login(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &LoginSession,
    keys: LoginKeys,
    tick: u64,
) {
    let hints: &[(&str, &str)] = match keys {
        LoginKeys::Back => &[("q", "back")],
        LoginKeys::Cancel => &[("esc", "cancel")],
        LoginKeys::Paste => &[("↵", "submit"), ("esc", "back")],
        LoginKeys::Owner(hints) => hints,
    };
    let mut spans = vec![
        Span::styled(format!("{} ", spinner_frame(tick)), theme::accent()),
        Span::styled(format!("logging in '{}'", session.name), theme::dim()),
    ];
    // Each hint here carries a 3-space lead, while `shed_to_width` counts a gap
    // only between groups: the name and one lead come off its budget.
    let lead = spans.iter().map(Span::width).sum::<usize>() + 3;
    let mut hints = hints.to_vec();
    shed_to_width(&mut hints, (area.width as usize).saturating_sub(lead));
    for (key, action) in hints {
        spans.push(Span::styled(format!("   {key} "), theme::accent().bold()));
        spans.push(Span::styled(action, theme::dim()));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(theme::base())
            .alignment(Alignment::Left),
        area,
    );
}
