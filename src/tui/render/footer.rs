//! Bottom strip: key hints, or a footer alert in place when one is active.

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::super::app::{
    App, CodexDetailRow, CodexLoginSession, ConfigFocus, ConfigRow, FallbackHint, FooterAlert,
    GLOBAL_CONFIG_ROWS, GlobalConfigRow, HERDR_OPTIONS, HerdrOption, KeyOwner, LoginSession,
    MainItemKind, Modal, PaneState, PluginFocus, ScrollStamp, SetupSelection, StatusFocus, Tab,
    TokenView, build_action_menu, config_rows, escape_control, fallback_hint, has_sub_focus,
    herdr_config_writable, keyboard_owner, pane_state, token_model_count,
};
use super::super::theme;
use super::format::{
    grapheme_cells, prefix_in_cells, spinner_frame, trim_trailing_ellipsis_graphemes, truncate,
};
use super::prose;

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

/// The note editor's grammar: ⏎ saves the draft, ⌃j inserts a newline, esc
/// cancels. ←→ move the caret inside the draft (never switch tabs).
const NOTE_EDITOR_FIELD: &[(&str, &str)] = &[
    ("↵", "save"),
    ("⌃j", "newline"),
    ("←→", "caret"),
    ("esc", "cancel"),
];

/// The help modal's own keys while its keymap overflows: ↑↓ scrolls, `q` (or
/// `esc` / `?`) closes. The modal's own sections already document both, so the
/// footer keeps the two discoverable keys alone; while the body fits, `q back`
/// alone acts — a key that scrolls nothing advertises nothing.
const HELP_KEYS: &[(&str, &str)] = &[("↑↓", "scroll"), ("q", "back")];

/// The lone `q back` of a scroll modal whose body fits.
const Q_BACK: &[(&str, &str)] = &[("q", "back")];

/// A two-button confirm: ←→ (or tab) moves the highlight between the buttons,
/// ↵ commits the highlighted one, `q` backs out.
const BUTTON_KEYS: &[(&str, &str)] = &[("←→", "pick"), ("↵", "choose"), ("q", "back")];

/// The loss warning's three buttons: ←→ steps and tab wraps between them, ↵
/// chooses the highlighted one, `esc` and `q` both `go back`.
const LOSS_WARNING_KEYS: &[(&str, &str)] =
    &[("←→", "pick"), ("↵", "choose"), ("esc · q", "go back")];

/// The same confirm with `↑↓ scroll` after the choose keys when its body
/// overflows, so a narrow terminal sheds the scroll hint before the choice
/// keys.
const BUTTON_SCROLL_KEYS: &[(&str, &str)] = &[
    ("←→", "pick"),
    ("↵", "choose"),
    ("↑↓", "scroll"),
    ("q", "back"),
];

/// A vertical option list: ↑↓ moves the cursor, ↵ picks the row under it,
/// `q` backs out.
const LIST_KEYS: &[(&str, &str)] = &[("↑↓", "pick"), ("↵", "choose"), ("q", "back")];

/// The preset picker adds `d` (delete a saved preset; the body also teaches
/// it) and ↵ applies the picked preset. A built-in row has no file to drop, so
/// its footer drops `d delete` — the same predicate the `d` handler reads.
const PRESET_PICKER_KEYS: &[(&str, &str)] = &[
    ("↑↓", "pick"),
    ("↵", "apply"),
    ("d", "delete"),
    ("q", "back"),
];

/// The preset picker over a built-in row: no `d delete`.
const PRESET_PICKER_BUILTIN_KEYS: &[(&str, &str)] =
    &[("↑↓", "pick"), ("↵", "apply"), ("q", "back")];

/// A name-entry modal that closes on `q`: ↵ saves the name, ←→ moves the
/// caret, `q` backs out.
const CAPTURE_NAME_KEYS: &[(&str, &str)] = &[("↵", "save"), ("←→", "caret"), ("q", "back")];

/// A name-entry modal where `q` is data: ↵ saves the name, ←→ moves the
/// caret, `esc` goes back (closes it) — `back`, never `cancel`, so beside a
/// login in flight the word can't read as cancelling the login.
const NAME_PROMPT_KEYS: &[(&str, &str)] = &[("↵", "save"), ("←→", "caret"), ("esc", "back")];

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // The footer row is the last full-width row of the frame it belongs to;
    // the scroll-bound stamps record that frame's area, so the footer compares
    // against it and drops a resize's stale bound.
    let frame_area = Rect::new(0, 0, area.width, area.y + area.height);

    // 1-col breathing room on each side; the alert row (which replaces this in
    // place) shares the same inset so the left margin never jumps.
    let area = inset_x(area, 1);

    let owner = keyboard_owner(app);

    // The save cue is a state mirror, computed each frame (not a stored alert):
    // it shows while the quit waits for a running write on every tab, and `x`
    // cannot dismiss it. It outranks any stored alert.
    if app.exit_wait {
        let msg = save_cue(&saving_row_name(app), area.width);
        draw_alert(frame, area, SAVE_CUE_LEADER, theme::info(), &msg);
        return;
    }

    // A live alert replaces the footer in place — one footer row, never
    // stacked — and outranks the login line: a ctrl+c arm can happen while a
    // login runs, and the quit prompt must be visible then.
    if let Some(alert) = &app.footer_alert {
        let FooterAlert::Warn(msg) = alert;
        draw_alert(frame, area, "! ", theme::warning(), msg);
        return;
    }

    // A login in flight owns the footer while no alert is live.
    if let Some(session) = &app.login {
        // An open editor takes esc/q before the login's cancel does, so its own
        // grammar is the hint. Any open modal owns esc/q too (the login modal
        // collapses, others handle their own keys), so the hint is that modal's
        // own row behind the login lead; the login modal's open code field is
        // the exception, where `q` is data and ↵ submits.
        let keys = login_keys(owner, app, frame_area, session.paste_field.is_some());
        draw_login(frame, area, session, keys, app.tick_count);
        return;
    }

    // A codex browser re-login in flight owns the footer the same way: the
    // modal is the live view, this row is the collapsed one carrying the name.
    // As on the claude login line, any open non-login modal owns esc/q first,
    // so its own row rides behind the codex lead; collapsed, esc cancels.
    if let Some(session) = &app.codex_login {
        let keys = login_keys(owner, app, frame_area, false);
        draw_codex_login(frame, area, session, keys, app.tick_count);
        return;
    }

    // An editor owns the keys it claims, `←→` included, so its own grammar is
    // the whole row: `q` only where the editor itself binds it, `? help` only
    // where it lets `?` through. Every owner takes the arrows, so `←→ tabs`
    // rides only while nothing owns the keyboard.
    let mut hints: Vec<(&str, &str)> =
        match owner.zip(owner.and_then(|o| owner_hints(o, app, frame_area))) {
            Some((owner, own)) => {
                let mut hints = own.to_vec();
                if !owner.claims(app, KeyCode::Char('?')) {
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
                .then_some(if pane_state(app) == PaneState::Rest {
                    TAB_NAV
                } else {
                    ("←", "back")
                })
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

    // `e` opens the cursor's Setup editor and nothing without a row under the
    // cursor: an empty roster, or a filter hiding every row. Its handler keeps
    // the same condition.
    if app.tab == Tab::Overview && owner.is_none() && app.current_main_item().is_none() {
        hints.retain(|(key, _)| *key != "e");
    }

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

/// A published scroll bound is this frame's only when its stamp still matches
/// the frame's area and the modal-open generation: a resize changes the area,
/// and an open or close changes the generation, so a bound left over from
/// either errs the footer to the bound's no-scroll row.
fn bound_is_current(stamp: ScrollStamp, area: Rect, app: &App) -> bool {
    stamp.width == area.width
        && stamp.height == area.height
        && stamp.generation == app.modal_generation.get()
}

/// An owner's own grammar, the whole hint row while it holds the keyboard
/// bar the `? help` that [`draw`] derives from what the owner claims. For the
/// modal owner the row is read off the top modal's real handler, one arm per
/// modal, so a new modal that skips its arm fails to compile. `None` only for
/// the two login modals, whose login-in-flight footers [`draw_login`] and
/// `draw_codex_login` own.
fn owner_hints(
    owner: KeyOwner,
    app: &App,
    area: Rect,
) -> Option<&'static [(&'static str, &'static str)]> {
    match owner {
        KeyOwner::Modal => match app.modals.last() {
            // `↑↓ scroll` rides only while the body actually overflows — the
            // same published bound the scroll handler clamps against — and the
            // bound is this frame's, so a key that scrolls nothing, or a bound
            // a resize or reopen left stale, advertises nothing.
            Some(Modal::Help)
                if app.help_max_scroll.get() > 0
                    && bound_is_current(app.help_max_scroll_stamp.get(), area, app) =>
            {
                Some(HELP_KEYS)
            }
            Some(Modal::Help) => Some(Q_BACK),
            Some(Modal::Confirm(state))
                if state.scroll.max_scroll.get() > 0
                    && bound_is_current(state.scroll.stamp.get(), area, app) =>
            {
                Some(BUTTON_SCROLL_KEYS)
            }
            Some(Modal::Confirm(_)) => Some(BUTTON_KEYS),
            // The loss warning's three buttons never scroll: it stays the plain
            // button row, with `esc` named beside `q` as `go back`.
            Some(Modal::LossWarning(_)) => Some(LOSS_WARNING_KEYS),
            Some(Modal::PresetApply(form))
                if form.scroll.max_scroll.get() > 0
                    && bound_is_current(form.scroll.stamp.get(), area, app) =>
            {
                Some(BUTTON_SCROLL_KEYS)
            }
            Some(Modal::PresetApply(_)) => Some(BUTTON_KEYS),
            Some(Modal::EnvCollision(state)) => {
                if state.reviewing_overwrite {
                    if state.max_scroll.get() > 0
                        && bound_is_current(state.max_scroll_stamp.get(), area, app)
                    {
                        Some(BUTTON_SCROLL_KEYS)
                    } else {
                        Some(BUTTON_KEYS)
                    }
                } else {
                    Some(LIST_KEYS)
                }
            }
            Some(Modal::Divergence(_) | Modal::DivergenceTarget(_)) => Some(LIST_KEYS),
            // Its `s`/`p`/`c` keys sit in its own rows, the action menu's
            // hotkey shape.
            Some(Modal::ActionMenu(_) | Modal::BareItem(_) | Modal::BareItemOwner(_)) => {
                Some(LIST_KEYS)
            }
            // `d delete` drops on a built-in row, whose `d` only toasts a
            // refusal — the same predicate the `d` handler reads.
            Some(Modal::PresetPicker(state))
                if state
                    .presets
                    .get(state.cursor.min(state.presets.len().saturating_sub(1)))
                    .is_some_and(|p| !p.builtin) =>
            {
                Some(PRESET_PICKER_KEYS)
            }
            Some(Modal::PresetPicker(_)) => Some(PRESET_PICKER_BUILTIN_KEYS),
            Some(Modal::MoveSession(_)) => Some(LIST_KEYS),
            Some(Modal::CaptureName(_)) => Some(CAPTURE_NAME_KEYS),
            Some(Modal::NamePrompt(_)) => Some(NAME_PROMPT_KEYS),
            // The re-bind confirm walks an account list the cursor steps over,
            // so its ruled copy leads `↑↓ row`; `?` falls through it so
            // [`draw`] derives `? help` beside the row.
            Some(Modal::Rebind(_)) => Some(&[
                ("↑↓", "row"),
                ("space", "toggle"),
                ("↵", "choose"),
                ("q", "not now"),
            ]),
            Some(Modal::Login | Modal::CodexLogin) | None => None,
        },
        KeyOwner::SetupField
        | KeyOwner::MemberField
        | KeyOwner::RefreshInterval
        | KeyOwner::ContextNudge
        | KeyOwner::WeeklyThreshold
        | KeyOwner::HerdrTag => Some(TYPED_FIELD),
        KeyOwner::NewAccountField => Some(NEW_ACCOUNT_FIELD),
        KeyOwner::NoteEditor => Some(NOTE_EDITOR_FIELD),
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
/// the keyboard. A modal's row comes from [`owner_hints`] instead, so this
/// never renders under one.
fn tab_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    // `q` label: "back" in a sub-focus, "quit" at top level.
    // (While armed the alert row shows instead, so this label stays "quit".)
    let q_label: &str = if has_sub_focus(app) { "back" } else { "quit" };

    // Usage hints derive from the selected native or read-only account.
    if app.tab == Tab::Usage {
        let mut hints = usage_hints(app);
        hints.push(("q", q_label));
        return hints;
    }

    let tail: &[(&str, &str)] = match app.tab {
        Tab::Overview if app.overview_selected_extra().is_some() => {
            &[("↵", "usage"), ("a", "actions"), ("?", "help")]
        }
        Tab::Overview => &[
            ("⇧↑↓", "reorder"),
            ("e", "edit"),
            ("a", "actions"),
            ("?", "help"),
        ],
        Tab::Usage => &[], // handled above: its hints derive per row
        Tab::Tokens => match app.token_view {
            TokenView::Dashboard if token_model_count(app) == 0 => &[
                ("r", "reload"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Dashboard => &[
                ("↵", "models"),
                ("r", "reload"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models if app.model_detail_focus == StatusFocus::Detail => &[
                ("↑↓", "scroll"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models if token_model_count(app) == 0 => &[
                ("↑↓", "model"),
                ("c", "count cache"),
                ("t", "period"),
                ("a", "actions"),
                ("?", "help"),
            ],
            TokenView::Models => &[
                ("↑↓", "model"),
                ("↵", "detail"),
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
            ConfigFocus::Actions => match app.setup_selection() {
                // the hint derives from the row under the cursor, never a dead
                // `↵ edit / toggle` or a key the pane does not bind.
                SetupSelection::Item(MainItemKind::Codex(_)) => {
                    if app.codex_detail_armed {
                        &[("↵", "confirm delete"), ("esc", "cancel"), ("?", "help")]
                    } else {
                        match CodexDetailRow::ALL.get(app.config_action_cursor) {
                            Some(CodexDetailRow::ReLogin) => &[
                                ("↑↓", "row"),
                                ("↵", "re-login"),
                                ("a", "actions"),
                                ("?", "help"),
                            ],
                            Some(CodexDetailRow::Delete) => &[
                                ("↑↓", "row"),
                                ("↵", "delete"),
                                ("a", "actions"),
                                ("?", "help"),
                            ],
                            None => &[("a", "actions"), ("?", "help")],
                        }
                    }
                }
                SetupSelection::Item(MainItemKind::Hermes(_)) => &[("a", "actions"), ("?", "help")],
                // Claude rows and `+ new` keep the row-aware claude arm:
                // the `model` row cycles on space; `alert at` steps on
                // `+`/`-`; env rows edit a value or open the add-env editor.
                SetupSelection::Item(MainItemKind::Profile(_)) | SetupSelection::New => {
                    match config_rows(app).get(app.config_action_cursor) {
                        Some(ConfigRow::Model) => &[
                            ("↑↓", "row"),
                            ("space", "cycle"),
                            ("↵", "custom"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        Some(ConfigRow::BellThreshold) => &[
                            ("↑↓", "row"),
                            ("+", "raise"),
                            ("-", "lower"),
                            ("↵", "type"),
                            ("a", "actions"),
                            ("?", "help"),
                        ],
                        // `alert at`'s keys, where a larger value probes less
                        // often.
                        Some(ConfigRow::UsageProbeInterval) => &[
                            ("↑↓", "row"),
                            ("+", "less often"),
                            ("-", "more often"),
                            ("↵", "type"),
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
                        Some(ConfigRow::Type) => &[
                            ("↑↓", "row"),
                            ("space/↵", "cycle"),
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
            | FallbackHint::DetailCheckCreditCap
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
    if app.tab == Tab::Plugin && app.plugin.focus == PluginFocus::Detail {
        if app.plugin.diagnostic_focus {
            hints.retain(|(key, _)| !matches!(*key, "space/↵" | "+" | "-" | "↵"));
            if let Some((_, label)) = hints.first_mut() {
                *label = "row";
            }
        }
        if app.plugin.detail_max_scroll.get() > 0 {
            hints.insert(1, ("pgup/pgdn", "read"));
        }
    }
    // An armed 2-step's way out is its `esc cancel`: `q` ascends out of the card
    // the same way there, so its armed confirm already names the way out.
    let setup_armed =
        app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions && app.codex_detail_armed;
    if !(setup_armed
        || (app.tab == Tab::Fallback && fallback_hint(app) == FallbackHint::DetailRemoveArmed))
    {
        hints.push(("q", q_label));
    }
    hints
}

/// Which keys act on the Usage tab's current row and focus: the one predicate
/// set the footer's hints and the help modal's Usage rows both derive from, so
/// neither advertises a key the shown row ignores.
pub(crate) struct UsageKeyFacts {
    /// In detail focus (↑↓ scrolls, esc/← ascend).
    pub(crate) detail: bool,
    /// `↵` descends: list focus with at least one item row.
    pub(crate) enter: bool,
    /// `r` acts, and what it re-reads.
    pub(crate) r: Option<UsageRefresh>,
    /// `n` opens the note editor (a claude row with a profile to hold it).
    pub(crate) n: bool,
}

/// What a Usage `r` re-reads on the current row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageRefresh {
    /// The focused claude account's usage.
    RefreshAccount,
}

/// The per-row key facts: notes and refresh act on native profiles;
/// the detail key descends on any item
/// row. The empty roster keeps the empty state's promise.
pub(crate) fn usage_key_facts(app: &App) -> UsageKeyFacts {
    let detail = app.usage_detail_focus == StatusFocus::Detail;
    let (r, n) = if app.usage_extra_cursor.is_some() {
        (None, false)
    } else {
        match app.usage_selection() {
            SetupSelection::Item(MainItemKind::Profile(_)) if app.profile_count() > 0 => {
                (Some(UsageRefresh::RefreshAccount), true)
            }
            SetupSelection::Item(MainItemKind::Profile(_))
            | SetupSelection::Item(MainItemKind::Codex(_))
            | SetupSelection::Item(MainItemKind::Hermes(_))
            | SetupSelection::New => (None, false),
        }
    };
    UsageKeyFacts {
        detail,
        enter: !detail && (!app.setup_items().is_empty() || !app.usage_extras.is_empty()),
        r,
        n,
    }
}

/// The Usage tab's own hints, derived from [`usage_key_facts`] per row.
fn usage_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut hints: Vec<(&'static str, &'static str)> = Vec::new();
    let facts = usage_key_facts(app);
    if facts.detail {
        hints.push(("↑↓", "scroll"));
    } else {
        hints.push(("↑↓", "account"));
        if facts.enter {
            hints.push(("↵", "detail"));
        }
    }
    match facts.r {
        Some(UsageRefresh::RefreshAccount) => hints.push(("r", "refresh account")),
        None => {}
    }
    if facts.n {
        hints.push(("n", "note"));
    }
    hints.push(("a", "actions"));
    hints.push(("?", "help"));
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
    while row_width(hints) > width {
        match hints.iter().rposition(|h| !essential_hint(h)) {
            Some(i) => {
                hints.remove(i);
            }
            None => break,
        }
    }
}

/// A hint the row keeps under every shed: the screen's only way out or around,
/// discoverability, and armed confirms. [`draw_login`] mirrors this when it
/// caps a profile name, so a long name never crowds these out.
fn essential_hint(hint: &(&str, &str)) -> bool {
    let (key, label) = *hint;
    matches!(
        (key, label),
        ("←→", "tabs" | "day") | ("?" | "q" | "esc", _)
    ) || label.starts_with("confirm")
}

/// The cells the essential groups alone occupy — the width [`shed_to_width`]
/// can never go below.
fn essential_hint_width(hints: &[(&str, &str)]) -> usize {
    let kept: Vec<_> = hints.iter().copied().filter(essential_hint).collect();
    kept.iter()
        .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
        .sum::<usize>()
        + kept.len().saturating_sub(1) * 3
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

/// Name the retained write a pending quit is waiting for.
fn saving_row_name(app: &App) -> String {
    if let Some(name) = &app.deleting {
        return escape_control(name.as_str());
    }
    crate::usage::spending_account(&app.activity)
        .map_or_else(String::new, |name| escape_control(&name))
}

/// The info leader the save cue and its width measurement share, so the two
/// can't drift apart.
const SAVE_CUE_LEADER: &str = "i ";

/// The save cue's message: `saving {row}… · ⌃c ⌃c quit now`, the row name cut
/// (grapheme-safe, one `…`) so the leader and `· ⌃c ⌃c quit now` stay whole
/// down to the width floor. The `⌃c ⌃c` run is one key span. One closure holds
/// the copy, measured with an empty name to set the name's budget and rendered
/// with the cut name, so the measurement and the message cannot drift.
fn save_cue(row: &str, width: u16) -> String {
    let key = prose::key("\u{2303}c \u{2303}c");
    let cue = |name: &str| format!("saving {name}\u{2026} · {key} quit now");
    let fixed = grapheme_cells(SAVE_CUE_LEADER) + grapheme_cells(&prose::plain(&cue("")));
    let budget = (width as usize).saturating_sub(fixed);
    let mut name = prefix_in_cells(row, budget).0;
    // A cut that stops right after a `…` inside the name would double it with
    // the always-on `…`; trim a kept trailing `…` only when the cut dropped
    // something, so an uncut name keeps its own characters.
    if grapheme_cells(&name) < grapheme_cells(row) {
        name = trim_trailing_ellipsis_graphemes(&name);
    }
    cue(&name)
}

/// Render a footer alert in place of the hint bar: a semantic leader glyph
/// (`! ` warning, `i ` info) in its color, then the message in `TEXT_DIM` with
/// key runs rendered as keys, cut to fit the row (grapheme-safe, trailing `…`).
fn draw_alert(frame: &mut Frame<'_>, area: Rect, leader: &str, leader_style: Style, msg: &str) {
    let mut spans = vec![Span::styled(leader, leader_style)];
    let width = (area.width as usize).saturating_sub(grapheme_cells(leader));
    spans.extend(prose::fit(msg, width, theme::dim()));
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
    /// The login modal is open (collapsed stage): `q` collapses it.
    Back,
    /// The login modal is collapsed: `esc` cancels the login.
    Cancel,
    /// The login modal's code field is open: `↵` submits, `esc` restores the
    /// `p  paste code` row; `q` is data.
    Paste,
    /// An open editor or a non-login modal takes the keys first: its own row.
    Owner(&'static [(&'static str, &'static str)]),
}

/// The [`LoginKeys`] a login-in-flight footer shows, shared by the claude and
/// codex lines: an open editor/modal owns esc/q first (its own row); the login
/// modal's open code field is `Paste`; collapsed it is `Back`; with no modal,
/// `Cancel`. `paste_open` names the claude modal's open code field — the codex
/// modal has no paste door, so its line passes `false`.
fn login_keys(owner: Option<KeyOwner>, app: &App, frame_area: Rect, paste_open: bool) -> LoginKeys {
    match owner.and_then(|o| owner_hints(o, app, frame_area)) {
        Some(hints) => LoginKeys::Owner(hints),
        None => match app.modals.last() {
            Some(Modal::Login) if paste_open => LoginKeys::Paste,
            Some(_) => LoginKeys::Back,
            None => LoginKeys::Cancel,
        },
    }
}

/// The hint set a login-in-flight footer shows for its [`LoginKeys`]. `paste`
/// is the login's own paste-field hints (the claude modal's `↵ submit · esc
/// back`; the codex modal has no paste door, so none).
fn login_key_hints(
    keys: LoginKeys,
    paste: &'static [(&'static str, &'static str)],
) -> &'static [(&'static str, &'static str)] {
    match keys {
        LoginKeys::Back => &[("q", "back")],
        LoginKeys::Cancel => &[("esc", "cancel")],
        LoginKeys::Paste => paste,
        LoginKeys::Owner(hints) => hints,
    }
}

/// Login-in-progress line. Independent of `footer_alert` so key handling that
/// clears alerts never hides it. The live stage renders in the login modal;
/// this row is the collapsed view and carries the name alone. The trailing
/// hint tracks what esc/q actually do this frame: an open editor takes them
/// first (its own row); with any non-login modal open the hint is that modal's
/// own row behind the login lead; the login modal's open code field takes
/// `↵ submit   esc back`; collapsed, both cancel the login (`esc cancel`).
fn draw_login(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &LoginSession,
    keys: LoginKeys,
    tick: u64,
) {
    let hints = login_key_hints(keys, &[("↵", "submit"), ("esc", "back")]);
    // The name's budget is the row width minus the spinner, the `logging in ''`
    // envelope and the essential hints' cells, so a long profile name never
    // crowds out the one key that leaves the modal.
    let name_budget = (area.width as usize)
        .saturating_sub(1 + 1 + "logging in ''".len() + 3)
        .saturating_sub(essential_hint_width(hints));
    let name = truncate(&session.name, name_budget);
    let mut spans = vec![
        Span::styled(format!("{} ", spinner_frame(tick)), theme::accent()),
        Span::styled(format!("logging in '{name}'"), theme::dim()),
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

/// Codex-login-in-progress line, the [`draw_login`] shape without the paste
/// door: the name, plus the hint that tracks what esc/q do this frame.
fn draw_codex_login(
    frame: &mut Frame<'_>,
    area: Rect,
    session: &CodexLoginSession,
    keys: LoginKeys,
    tick: u64,
) {
    let hints = login_key_hints(keys, &[]);
    let mut spans = vec![
        Span::styled(format!("{} ", spinner_frame(tick)), theme::accent()),
        Span::styled(format!("logging in '{}'", session.name), theme::dim()),
    ];
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

#[cfg(test)]
#[path = "../../../tests/inline/tui_render_footer.rs"]
mod tests;
