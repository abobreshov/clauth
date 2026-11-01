//! Application state, keymap, and tick logic.
//!
//! Layout invariants:
//!     the visible filter, while Claude selections sync `profile_cursor` with Usage and Setup.
//!   - Config: master-detail — account list + `+ new` row + inline editor
//!     (`config_draft`). No popups for create / edit / rename / delete.
//!   - Fallback: master-detail — ordered chain + `+ add` on the left; inline
//!     threshold stepper / remove / add-picker on the right. No popups.
//!   - Modals stack: top of `modals` owns input; events reach the screen below
//!     only when the stack is empty.

use std::collections::{HashMap, HashSet, VecDeque};

use std::panic::{AssertUnwindSafe, catch_unwind};

use std::sync::Arc;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use std::time::{Duration, Instant};

use anyhow::Result;

use chrono::Weekday;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use ratatui::style::Style;

use ratatui::text::Span;

use crate::actions::{
    CaptureSnapshot, ChainEditRefusal, ChainRefusal, EnvKeyCollision, capture_into_profile,
    capture_live_file_snapshot, capture_snapshot, classify_env_key, clear_profile_api_key,
    clear_profile_credentials, create_blank_profile_with, create_profile_from_login,
    delete_profile_taking_config, duplicate_profile, edit_profile_endpoint, edit_profile_env,
    edit_profile_model, edit_profile_preset, find_matching_oauth_profile,
    overwrite_captured_profile, rename_profile, reorder_profile, rotation_guard_for_mutation,
    set_chain_order, set_member_threshold, set_wrap_off, snapshot_is_empty, switch_off,
    switch_profile, validate_foreign_harness_free, validate_name_chars, validate_profile_name,
};

use crate::claude::{
    LinkState, adopt_first_login, classify_credentials_link, claude_settings_env_keys,
    detach_credentials_link, force_link_profile_credentials, force_snapshot_active_credentials,
    is_first_login, link_profile_credentials, live_credentials_are_shell, read_claude_credentials,
    snapshot_active_credentials,
};

use crate::fallback::{
    DEFAULT_THRESHOLD, MAX_THRESHOLD, MIN_THRESHOLD, SwitchAction, auto_switch_if_needed,
    parse_threshold, threshold_for,
};

use crate::format::format_threshold_tokens;

use crate::harness::Harness;

use crate::lock::with_state_lock;

use crate::lockorder::{RankedGuard, RankedMutex};

use crate::oauth;

use crate::plugin_host::ExeChange;

use crate::profile::{
    AppConfig, ClockFormat, ConfigHandle, ConsoleSite, DivergenceChoice, HerdrSettings, HomeTab,
    MAX_CONTEXT_NUDGE_TOKENS, MAX_REFRESH_INTERVAL_MS, MAX_WEEKLY_SWITCH_PCT,
    MIN_CONTEXT_NUDGE_TOKENS, MIN_REFRESH_INTERVAL_MS, MIN_WEEKLY_SWITCH_PCT, ModelSettings,
    PopupWidth, Profile, ProfileId, ProfileName, ReloadFingerprint, ResetDisplay, ThemeName,
    WalkOrder, claim_profile_id, load_config, reload_fingerprint, save_app_state,
    save_profile_config,
};

use crate::profile_cache::{USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms};

use crate::profile_json::{stale_after_ms, usage_cache_file};

use crate::status::{self, Incident, StatusEvent};

use crate::tui::render::prose::{self, cmd_lit};

use crate::tui::render::{ListView, follow_scroll_offset, truncate};

use crate::tui::theme;

use crate::update;
use std::thread::JoinHandle;

use crate::usage::{
    ActivityStore, FetchLeg, FetchStatus, KickBlocks, LastFetchedAt, MemoryStores,
    NextRefreshPerProfile, OpResult, OpResultReceiver, OpResultSender, PendingSwitch,
    PendingSwitchOff, PendingSwitchTarget, PollStreaks, ProfileActivity, RefetchQueue,
    SkippedUnreadable, StartupReceiver, StartupSender, StartupSignal, StatusStore,
    SuppressedAuthExpiredStore, ThirdPartyBroken, ThirdPartyList, ThirdPartyStatusStore,
    ThirdPartyStreaks, ThirdPartyUsageStore, TokenList, UsageInfo, UsageStore, WeeklyResetKicks,
    any_busy, apply_pending_forgets, bootstrap_fetch, bootstrap_third_party, clear_activity,
    collect_oauth_seed_names, collect_third_party_entries, collect_third_party_seed_names,
    collect_tokens, is_idle, mark_activity, now_ms, spawn_refresher, switch_gate_in_flight,
    token_spend_in_flight, windows_maxed,
};

#[derive(Debug, Clone)]
pub(crate) struct InputState {
    pub(crate) value: String,
    /// Caret position in UTF-8 byte offset. Always falls on a grapheme boundary.
    pub(crate) cursor: usize,
    /// The horizontal window's persisted cell offset (a grapheme boundary),
    /// read and written at draw like the pane scroll offsets — see
    /// `render::panes::field_window`.
    pub(crate) offset: std::cell::Cell<usize>,
}

impl InputState {
    pub(crate) fn new(initial: &str) -> Self {
        Self {
            value: initial.to_string(),
            cursor: initial.len(),
            offset: std::cell::Cell::new(0),
        }
    }

    /// Insert `ch` at the caret. A char that joins the grapheme after it (a
    /// base char typed before a combining mark) carries the caret past the
    /// whole joined grapheme.
    pub(crate) fn insert(&mut self, ch: char) {
        self.value.insert(self.cursor, ch);
        self.cursor = snap_to_grapheme(&self.value, self.cursor + ch.len_utf8());
    }

    pub(crate) fn backspace(&mut self) {
        let prev = self.prev_grapheme();
        self.value.replace_range(prev..self.cursor, "");
        self.cursor = snap_to_grapheme(&self.value, prev);
    }

    pub(crate) fn delete(&mut self) {
        let next = self.next_grapheme();
        self.value.replace_range(self.cursor..next, "");
        self.cursor = snap_to_grapheme(&self.value, self.cursor);
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.prev_grapheme();
    }

    pub(crate) fn right(&mut self) {
        self.cursor = self.next_grapheme();
    }

    fn prev_grapheme(&self) -> usize {
        grapheme_bounds(&self.value)
            .into_iter()
            .rfind(|b| *b < self.cursor)
            .unwrap_or_default()
    }

    fn next_grapheme(&self) -> usize {
        grapheme_bounds(&self.value)
            .into_iter()
            .find(|b| *b > self.cursor)
            .unwrap_or(self.value.len())
    }

    /// Delete the word (run of non-spaces, plus any trailing spaces) left of the
    /// caret — the `ctrl+w` editor verb.
    pub(crate) fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let head = &self.value[..self.cursor];
        let trimmed = head.trim_end_matches(' ');
        let start = snap_to_grapheme(head, trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0));
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.value.len();
    }

    pub(crate) fn trimmed(&self) -> &str {
        self.value.trim()
    }

    pub(crate) fn trimmed_some(&self) -> Option<String> {
        let t = self.trimmed();
        (!t.is_empty()).then(|| t.to_string())
    }
}

/// A typed editor's buffer beside the value it opened from. Changed work is
/// `input.value != seed`; a clean edit re-seeds both from a newer stored value
/// (caret at the end) while a dirty edit keeps its text whatever the store
/// does. This is the one shape every editor that opens from a stored value
/// uses, so a later editor can never compare against the live store by
/// accident (the stale-but-clean false dirty).
#[derive(Debug, Clone)]
pub(crate) struct SeededInput {
    pub(crate) input: InputState,
    pub(crate) seed: String,
}

impl SeededInput {
    pub(crate) fn new(initial: &str) -> Self {
        Self {
            input: InputState::new(initial),
            seed: initial.to_string(),
        }
    }

    /// Whether the buffer differs from the value it opened from.
    pub(crate) fn dirty(&self) -> bool {
        self.input.value != self.seed
    }

    /// Re-seed a clean edit (buffer == seed) from a NEWER stored value; an
    /// unchanged value is a no-op and keeps the caret, and a dirty edit is left
    /// alone. Returns whether the buffer changed.
    pub(crate) fn reseed(&mut self, new_seed: &str) -> bool {
        if self.input.value == self.seed && new_seed != self.seed {
            self.input = InputState::new(new_seed);
            self.seed = new_seed.to_string();
            true
        } else {
            false
        }
    }
}

/// The byte offsets of `s`'s grapheme boundaries, ascending, `0` through
/// `s.len()`: the steps an editor's caret takes. The segmenter is ratatui's
/// (`styled_graphemes`, the one the buffer draws with), which drops control
/// graphemes; a control char always stands alone as a grapheme, so each one
/// is a step of its own here and only the runs between them are segmented. A
/// CR LF pair, one grapheme to the segmenter, steps as two.
pub(crate) fn grapheme_bounds(s: &str) -> Vec<usize> {
    let mut bounds = vec![0];
    let mut at = 0;
    for run in s.split_inclusive(char::is_control) {
        let text = run.strip_suffix(char::is_control).unwrap_or(run);
        for g in Span::raw(text).styled_graphemes(Style::default()) {
            at += g.symbol.len();
            bounds.push(at);
        }
        if text.len() < run.len() {
            at += run.len() - text.len();
            bounds.push(at);
        }
    }
    bounds
}

/// The first grapheme boundary of `s` at or after byte `at`.
fn snap_to_grapheme(s: &str, at: usize) -> usize {
    grapheme_bounds(s)
        .into_iter()
        .find(|b| *b >= at)
        .unwrap_or(s.len())
}

/// The note editor's multi-line draft. The stored text is `lines` joined by
/// `\n`; the caret is `(row, col)`, `col` counting the graphemes before it, so
/// it always sits on a grapheme boundary.
#[derive(Debug, Clone)]
pub(crate) struct NoteBuffer {
    lines: Vec<String>,
    row: usize,
    col: usize,
    /// The slot's horizontal window start in cells, read and written at draw
    /// like the panes' scroll offsets.
    pub(crate) offset: std::cell::Cell<usize>,
}

impl NoteBuffer {
    pub(crate) fn new(text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        let row = lines.len() - 1;
        let col = grapheme_bounds(&lines[row]).len() - 1;
        Self {
            lines,
            row,
            col,
            offset: std::cell::Cell::new(0),
        }
    }

    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(crate) fn row_count(&self) -> usize {
        self.lines.len()
    }

    pub(crate) fn line(&self, index: usize) -> &str {
        &self.lines[index]
    }

    /// The caret's row and its byte offset in that row's line, for the native
    /// cursor position.
    pub(crate) fn caret(&self) -> (usize, usize) {
        (self.row, self.cursor_byte())
    }

    fn line_len(&self) -> usize {
        grapheme_bounds(&self.lines[self.row]).len() - 1
    }

    fn clamp_col(&mut self) {
        self.col = self.col.min(self.line_len());
    }

    fn cursor_byte(&self) -> usize {
        let line = &self.lines[self.row];
        grapheme_bounds(line)
            .get(self.col)
            .copied()
            .unwrap_or(line.len())
    }

    /// Put the caret on the first grapheme boundary of its line at or after
    /// byte `at`.
    fn set_cursor_byte(&mut self, at: usize) {
        let bounds = grapheme_bounds(&self.lines[self.row]);
        self.col = bounds
            .iter()
            .position(|b| *b >= at)
            .unwrap_or(bounds.len() - 1);
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        let at = self.cursor_byte();
        self.lines[self.row].insert(at, ch);
        self.set_cursor_byte(at + ch.len_utf8());
    }

    pub(crate) fn insert_newline(&mut self) {
        let at = self.cursor_byte();
        let rest = self.lines[self.row].split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub(crate) fn backspace(&mut self) {
        if self.col > 0 {
            let at = self.cursor_byte();
            let prev = grapheme_bounds(&self.lines[self.row])
                .into_iter()
                .rfind(|b| *b < at)
                .unwrap_or_default();
            self.lines[self.row].replace_range(prev..at, "");
            self.set_cursor_byte(prev);
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            let seam = self.lines[self.row].len();
            self.lines[self.row].push_str(&line);
            self.set_cursor_byte(seam);
        }
    }

    pub(crate) fn delete(&mut self) {
        let at = self.cursor_byte();
        if self.col < self.line_len() {
            let next = grapheme_bounds(&self.lines[self.row])
                .into_iter()
                .find(|b| *b > at)
                .unwrap_or(self.lines[self.row].len());
            self.lines[self.row].replace_range(at..next, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.set_cursor_byte(at);
    }

    pub(crate) fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len();
        }
    }

    pub(crate) fn right(&mut self) {
        if self.col < self.line_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub(crate) fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.clamp_col();
        }
    }

    pub(crate) fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.clamp_col();
        }
    }

    pub(crate) fn home(&mut self) {
        self.col = 0;
    }

    pub(crate) fn end(&mut self) {
        self.col = self.line_len();
    }

    /// Delete the word (run of non-spaces, plus any preceding spaces) left of
    /// the caret within the line; at a line start, joins the line up.
    pub(crate) fn delete_word(&mut self) {
        if self.col == 0 {
            self.backspace();
            return;
        }
        let at = self.cursor_byte();
        let head = self.lines[self.row][..at].trim_end();
        let start = head
            .char_indices()
            .rfind(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        let start = snap_to_grapheme(&self.lines[self.row], start);
        self.lines[self.row].replace_range(start..at, "");
        self.set_cursor_byte(start);
    }
}

/// The usage tab's note editor state. The account is captured at open, so a
/// config reload mid-edit can never retarget the draft.
#[derive(Debug, Clone)]
pub(crate) struct NoteEditor {
    pub(crate) profile: crate::profile::ProfileName,
    pub(crate) buf: NoteBuffer,
    /// The note text the editor opened from; the changed-work derivation and
    /// re-seed compare against this, never a fresh disk read.
    pub(crate) seed: String,
}

/// One interactive line in the Fallback tab's detail pane for a chain member.
/// `Threshold` is a stepper (±5 on `+`/`-`); `CheckWeekly`/`CheckScoped`/
/// `CheckCreditCap`/`LastResort`/`Preferred` are boolean toggles (space/⏎,
/// per the enumerated-row grammar);
/// `Remove` arms then confirms. The chain-global wrap-off setting lives on the
/// program-wide Config tab, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackRow {
    Threshold,
    /// Per-account override of the chain-wide weekly (7d) switch line
    /// (`Profile::weekly_threshold`; the Config tab's `weekly limit` stays
    /// the default). ⏎ opens an inline editor; an EMPTY commit clears the
    /// override back to the chain-wide value. Inert (dimmed, no-op) while the
    /// member's `check_weekly` gate is off — the line isn't judged there.
    WeeklyAt,
    /// Whether auto-switching checks this account's aggregate weekly usage
    /// (`Profile::check_weekly`, on by default). Off, only the 100% hard cap
    /// blocks it.
    CheckWeekly,
    /// Whether auto-switching checks this account's per-model weekly windows,
    /// e.g. "7d fable" (`Profile::check_scoped`, on by default). Off, the
    /// account stays in rotation for use with other models.
    CheckScoped,
    /// Whether auto-switching checks this account's usage-credit cap
    /// (`Profile::check_credit_cap`, on by default). Off, the account stays in
    /// rotation even with its cap spent.
    CheckCreditCap,
    LastResort,
    /// Whether this member is the chain's preferred (home) account
    /// (`Profile::preferred`, off by default). A pure on/off toggle, exclusive
    /// across the chain (a radio, like `LastResort`) and mutually exclusive with
    /// it. Marking it opts the chain into returning here once it reads clear and
    /// fresh again.
    Preferred,
    /// The weekdays this member is home (`Profile::preferred_days`): a cycle row
    /// over `PREFERRED_DAY_PRESETS` at rest, a multi-select chip row once ⏎
    /// descends into it (`CardEdit::Days`). A list matching no preset trails the
    /// run in its canonical spelling and is one more stop in the cycle while the
    /// card stays open (`App::fallback_day_stop`).
    PreferredDays,
    /// Dollar ceiling on what the chain may spend of this member's
    /// pay-as-you-go budget unattended (`Profile::max_auto_spend`, $0 default).
    /// Inert unless `AppState::spend_budget_switching` is also on — see
    /// `fallback::spend_room`. ⏎ opens an inline editor.
    MaxSpend,
    Remove,
}

impl FallbackRow {
    /// `space` acts only on state controls: a stepper, continuous or typed-only
    /// row (whose typed editor `space` opens), a toggle, or the `preferred days`
    /// cycle. The `remove` action row acts on `↵` alone.
    pub(crate) fn is_state_control(self) -> bool {
        match self {
            FallbackRow::Threshold
            | FallbackRow::WeeklyAt
            | FallbackRow::CheckWeekly
            | FallbackRow::CheckScoped
            | FallbackRow::CheckCreditCap
            | FallbackRow::LastResort
            | FallbackRow::Preferred
            | FallbackRow::PreferredDays
            | FallbackRow::MaxSpend => true,
            FallbackRow::Remove => false,
        }
    }
}

/// One editable line in the Setup tab's detail pane. Built per selection by
/// [`config_rows`]: auto-start only for OAuth; trailing row is `delete` or
/// `create`. `Name`/`BaseUrl`/`ApiKey` are text rows; the rest are toggles/actions.
/// `EnvEntry(i)` indexes the profile's sorted custom-env snapshot; `EnvAdd` is the
/// trailing `+ add env` row. Both appear only for existing accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigRow {
    /// Only on a fresh form; an existing account keeps its read-only
    /// `type  oauth|api` line instead.
    Type,
    Name,
    /// OAuth-only auto-start toggle. `config_rows` renders it in the second
    /// slot (right below `Name`); declared here so the enum tracks that order.
    AutoStart,
    /// `Profile::usage_probe`, for an OAuth account with no pair stored (the
    /// setup-token shape `/usage` cannot read): a toggle like `AutoStart`,
    /// space/⏎ flip it.
    UsageProbe,
    /// `Profile::usage_probe_interval_ms`, labelled `probe every`, listed only
    /// while the probe is on: the `alert at` grammar, ⏎ types the seconds and
    /// `+`/`-` step them by 30.
    UsageProbeInterval,
    /// `Profile::bell_threshold`, labelled `alert at`: the 5h % the bell rings
    /// at. ⏎ types a value (empty turns it off), `+`/`-` step it by 5.
    BellThreshold,
    BaseUrl,
    ApiKey,
    /// Default model (CC `model` setting). Hybrid: space cycles aliases, ⏎ types a custom value.
    Model,
    /// `ANTHROPIC_DEFAULT_FABLE_MODEL` — full id, free text.
    FableModel,
    /// `ANTHROPIC_DEFAULT_OPUS_MODEL` — full id, free text.
    OpusModel,
    /// `ANTHROPIC_DEFAULT_SONNET_MODEL` — full id, free text.
    SonnetModel,
    /// `ANTHROPIC_DEFAULT_HAIKU_MODEL` — full id, free text.
    HaikuModel,
    /// `CLAUDE_CODE_SUBAGENT_MODEL` — full id, free text.
    SubagentModel,
    /// The `+ model override` reveal row. Shown only while the unset alias
    /// overrides are collapsed; ⏎ expands fable/opus/sonnet/haiku/subagent inline.
    ModelOverrideAdd,
    /// A custom `key = value` env entry, indexed into the profile's sorted env
    /// snapshot. ⏎ edits its VALUE. An emptied value REMOVES the key — the
    /// unset, never a blank string — so the entry drops from `settings.json`
    /// on the next settings apply (`commit_env_value`).
    EnvEntry(usize),
    /// The `+ add env` row — ⏎ opens a key editor that runs the collision check.
    EnvAdd,
    /// Browser OAuth login: mint fresh tokens into this account (or, on the
    /// `+ new` form, create the account from the login). Async — runs on a worker.
    Login,
    /// `+ new`-form only: stash the live login Claude Code is using now into
    /// the draft, like [`ConfigRow::Login`] stashes its mint; `create account`
    /// commits it. Renders only while the live login is real and unowned
    /// (`App::unsaved_live_login`).
    CaptureLogin,
    /// Drop this account's stored OAuth credentials, keeping the profile shell.
    DeleteCreds,
    /// CLA-SPLIT escape hatch: delete this account's `session-token.json`, so
    /// switches go back to installing its own `credentials.json`. Shown only
    /// while a sidecar exists (`claude::session_token_status`, so a MIS-FILLED
    /// one gets the row too — that state needs an exit as much as a live mint),
    /// and dimmed + inert when the account stores no other login, since
    /// clearing would leave it with no credentials at all. Arms on the first
    /// ⏎ and confirms on the second, like `Disabled`'s disable direction:
    /// it changes what every future switch installs and, on the active account,
    /// moves a running session's credentials.
    ClearSessionToken,
    /// User-disabled account-action row (`Profile::disabled`), same class as
    /// `Delete`: enabling fires the shared
    /// `actions::enable_profile` immediately (harmless), disabling arms on the
    /// first ⏎ and confirms on the second, via `actions::disable_profile`.
    /// Dimmed and inert while the account is active or holds a live `tollgate
    /// start` session — the same gate the CLI enforces.
    Disabled,
    Delete,
    Create,
}

impl ConfigRow {
    /// Text rows capture keystrokes and commit on ⏎; the rest act on ⏎. The
    /// `Model` row is hybrid (space cycles, ⏎ opens a custom field), and the env
    /// rows seed their buffer before editing, so all three are driven out of band
    /// and are deliberately not text rows here. `BellThreshold` is a text row
    /// whose `+`/`-` at rest step it without opening the editor.
    pub(crate) fn is_text(self) -> bool {
        matches!(
            self,
            ConfigRow::Name
                | ConfigRow::BaseUrl
                | ConfigRow::ApiKey
                | ConfigRow::OpusModel
                | ConfigRow::SonnetModel
                | ConfigRow::HaikuModel
                | ConfigRow::FableModel
                | ConfigRow::SubagentModel
                | ConfigRow::BellThreshold
                | ConfigRow::UsageProbeInterval
        )
    }

    /// `space` acts only on state controls: a cycle, a toggle, or a stepper /
    /// continuous row whose typed editor `space` opens. Action rows and text
    /// fields act on `↵` alone.
    pub(crate) fn is_state_control(self) -> bool {
        match self {
            ConfigRow::Type
            | ConfigRow::Model
            | ConfigRow::AutoStart
            | ConfigRow::UsageProbe
            | ConfigRow::UsageProbeInterval
            | ConfigRow::BellThreshold => true,
            ConfigRow::Name
            | ConfigRow::BaseUrl
            | ConfigRow::ApiKey
            | ConfigRow::FableModel
            | ConfigRow::OpusModel
            | ConfigRow::SonnetModel
            | ConfigRow::HaikuModel
            | ConfigRow::SubagentModel
            | ConfigRow::ModelOverrideAdd
            | ConfigRow::EnvEntry(_)
            | ConfigRow::EnvAdd
            | ConfigRow::Login
            | ConfigRow::CaptureLogin
            | ConfigRow::DeleteCreds
            | ConfigRow::ClearSessionToken
            | ConfigRow::Disabled
            | ConfigRow::Delete
            | ConfigRow::Create => false,
        }
    }
}

/// The `model` row alias cycle (space advances it). `None` renders as `default`
/// (no `model` key); a custom id set via ⏎ is outside this list.
pub(crate) const MODEL_PRESETS: [&str; 5] = ["fable", "opus", "sonnet", "haiku", "opusplan"];

/// The `+ new` form's account type: which harness the form creates for, picked
/// the Overview cursor's section; any other tab opens on claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum NewAccountType {
    #[default]
    Claude,
    Codex,
}

impl NewAccountType {
    /// The cycle-row label.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            NewAccountType::Claude => "claude",
            NewAccountType::Codex => "codex",
        }
    }

    /// The next value `space` cycles to, wrapping.
    pub(crate) fn next(self) -> Self {
        match self {
            NewAccountType::Claude => NewAccountType::Codex,
            NewAccountType::Codex => NewAccountType::Claude,
        }
    }

    /// The Overview cursor's section, as `n` presets it: a claude row → claude,
    /// any other context fall back to claude.
    pub(crate) fn from_main_item(item: Option<MainItemKind>) -> Self {
        match item {
            Some(MainItemKind::Profile(_)) => NewAccountType::Claude,
            Some(MainItemKind::Codex(_)) => NewAccountType::Codex,
            Some(MainItemKind::Hermes(_)) | None => NewAccountType::Claude,
        }
    }
}

/// The codex detail pane's focusable action rows, in render order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexDetailRow {
    ReLogin,
    Delete,
}

impl CodexDetailRow {
    pub(crate) const ALL: [CodexDetailRow; 2] = [CodexDetailRow::ReLogin, CodexDetailRow::Delete];
}

/// The alias-override rows, in the order Setup lists them.
const OVERRIDE_ROWS: [ConfigRow; 5] = [
    ConfigRow::FableModel,
    ConfigRow::OpusModel,
    ConfigRow::SonnetModel,
    ConfigRow::HaikuModel,
    ConfigRow::SubagentModel,
];

/// The weekly-line preset ladder: one source for the Config row's segmented
/// control AND `step_weekly_threshold`'s cycle. 100 reproduces the old
/// hard-cap behavior (switch only once the API already refuses).
pub(crate) const WEEKLY_PRESETS: [f64; 4] = [90.0, 95.0, 98.0, 100.0];

/// `Weekday::Mon..=Sun`: the order the day picker lays out its chips and
/// writes a toggled list in.
pub(crate) const WEEKDAYS_ALL: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

const DAYS_WEEKDAYS: [Weekday; 5] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
];

const DAYS_WEEKENDS: [Weekday; 2] = [Weekday::Sat, Weekday::Sun];

/// The `preferred days` preset ladder: one source for the row's display AND
/// `step_preferred_days`' cycle, in cycle order. Space walks it forward and
/// wraps past the last rung onto the card's remembered custom list, else back
/// to `never`; a set matching no rung steps to `never`.
pub(crate) const PREFERRED_DAY_PRESETS: [(&str, &[Weekday]); 4] = [
    ("never", &[]),
    ("weekdays", &DAYS_WEEKDAYS),
    ("weekends", &DAYS_WEEKENDS),
    ("every day", &WEEKDAYS_ALL),
];

/// The preset rung a day set matches, or `None` for a custom set. Order-blind:
/// `["sun", "sat"]` is the `weekends` rung whatever the file wrote.
pub(crate) fn preferred_days_preset(days: &[Weekday]) -> Option<usize> {
    PREFERRED_DAY_PRESETS.iter().position(|(_, preset)| {
        preset.len() == days.len() && days.iter().all(|d| preset.contains(d))
    })
}

/// The preset rung's days at `index`, or the empty set — the `never` value.
pub(crate) fn preferred_days_at(index: usize) -> Vec<Weekday> {
    PREFERRED_DAY_PRESETS
        .get(index)
        .map(|(_, days)| days.to_vec())
        .unwrap_or_default()
}

/// Presets for the burn-aware early-switch floor (percent). The projection may
/// not switch below the chosen value, so wasted headroom is capped at
/// `100 - floor`. Default 98 (see [`crate::profile::DEFAULT_BURN_FLOOR_PCT`]).
pub(crate) const BURN_FLOOR_PRESETS: [f64; 4] = [97.0, 98.0, 99.0, 100.0];

/// Presets for the burn-aware projection horizon cap (ms). The projection looks
/// ahead by `min(refresh_interval, this)`; a shorter cap keeps the switch
/// nearer 100. Default 60 s (see [`crate::profile::DEFAULT_BURN_HORIZON_MS`]).
pub(crate) const BURN_HORIZON_PRESETS: [u64; 4] = [30_000, 45_000, 60_000, 90_000];

/// One row on the program-wide Config tab. These back real persisted globals in
/// [`AppState`] — no decorative toggles. ⏎/space cycles or flips in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlobalConfigRow {
    /// Color-depth tier: `full` (truecolor) / `compatible` (xterm-256).
    /// Persists to `[theme]` and live-swaps the active palette.
    Theme,
    /// Shape of every reset countdown (`AppState.reset_display`, issue #39):
    /// `relative` (stock) / `clock` / `both`. ⏎/space cycles.
    ResetShape,
    /// Notation for the wall-clock half of a reset stamp
    /// (`AppState.clock_format`): `24h` / `12h`. Dimmed + inert while
    /// `reset display` is `relative`, since nothing renders a clock then.
    ClockNotation,
    /// The tab every launch opens on (`AppState.home_tab`): space/⏎
    /// cycles the eight tabs in [`Tab::ALL`] order. The first herdr launch
    /// overrides it — that one landing opens the Plugin tab with the herdr
    /// row's detail descended.
    HomeTab,
    /// Chain-wide "when spent" behavior (`AppState.switch_off_when_spent`) — surfaced here as
    /// a program-wide default alongside the Fallback detail row.
    SwitchOffWhenSpent,
    /// Chain-wide weekly (7d) exhaustion line
    /// (`AppState.weekly_switch_threshold`, default 98) — space steps presets,
    /// ⏎ opens the custom-value editor (50–100, decimals allowed).
    WeeklyThreshold,
    /// Global refresh interval; space cycles presets in-place, ⏎ opens the
    /// custom-value editor (10–3600 s).
    RefreshInterval,
    /// Context-window nudge threshold in tokens
    /// (`AppState.context_nudge_threshold_tokens`, default off): at or past it,
    /// the context hook tells a running session once per value. Space cycles
    /// off → 300k → 400k → 600k → 900k → off; ⏎ opens the custom-value editor
    /// (50k-2M tokens, trailing `k` allowed).
    ContextNudge,
    /// Default action when CC overwrites the credentials symlink. ⏎/space cycles.
    DivergenceDefault,
    /// Keep every key of a credential store other than the login and its
    /// identity keys with the account whose store it is
    /// (`AppState.preserve_non_login_keys`) — ON by default. A `/design-login`
    /// then stays with the account it was made on across switches and relinks.
    /// Read off disk where it is used, so a flip applies to the next relink.
    /// ENUMERATED on/off, ⏎ mirrors space.
    PreserveNonLoginKeys,
    /// Opt-in burn-aware auto-switch (`AppState.burn_aware_switching`, issue #8
    /// follow-up b) — off by default, projects the ACTIVE profile's
    /// utilization ahead of the next poll instead of the static threshold.
    BurnAware,
    /// Walk-order mode (`AppState.walk_order`, issue #86): `chain` (default —
    /// today's chain-position walk) / `soonest weekly reset` / `most weekly
    /// headroom`. Space cycles.
    /// Decides WHERE each accept pass lands; `switch mode` decides WHEN the
    /// active is left, so the two stay orthogonal.
    WalkOrder,
    /// Burn-aware early-switch floor (`AppState.burn_switch_floor_pct`) — space
    /// cycles [`BURN_FLOOR_PRESETS`]. Dimmed + inert unless burn-aware is on.
    BurnFloor,
    /// Burn-aware projection horizon cap (`AppState.burn_horizon_cap_ms`) —
    /// space cycles [`BURN_HORIZON_PRESETS`]. Dimmed + inert unless burn-aware
    /// is on.
    BurnHorizon,
    /// Opt-in master switch for real-money fallback
    /// (`AppState.spend_budget_switching`) — off by default. On, the chain may
    /// hop to a member whose subscription is spent but whose account still has
    /// pay-as-you-go budget, bounded by that member's `max auto-spend` ceiling
    /// (Fallback tab, $0 by default). BOTH halves are needed, so flipping this
    /// alone still spends nothing.
    SpendBudget,
    /// What happens once a billing account has spent its `max auto-spend`
    /// budget (`AppState.switch_off_when_budget_spent`, default switch-off). Deliberately
    /// NOT `SwitchOffWhenSpent`: that row answers "the chain is out of free quota", where
    /// staying costs nothing, and this one answers "the money ran out", where
    /// staying is the spending.
    SwitchOffWhenBudgetSpent,
    /// Preemptive rotation (`AppState.preemptive_rotation`) — ON by default.
    /// Rotates a profile ahead of token expiry rather than waiting for a 401,
    /// with enough lead that the running `claude` never reaches its own
    /// refresh threshold. Off falls back to rotating only on rejection.
    PreemptiveRotation,
    /// Whether the background usage fetch still polls spent (100%-capped)
    /// accounts (`AppState.refresh_spent_accounts`, default on). Off skips a
    /// spent account until its window resets — a fetch-leg optimization only,
    /// never a switch/fallback input. ENUMERATED on/off, ⏎ mirrors space.
    RefreshSpentAccounts,
    /// of this build (`update::DISABLED_MESSAGE`): the row renders off and
    /// activating it toasts that message instead of flipping the saved value.
    AutoUpdate,
    /// Whether the `auto_start` auto-start kick is interleaved across accounts, so
    /// their 5h windows open `5h / N` apart instead of all at once
    /// (`AppState.auto_start_queue`, default OFF — for one account the queue is
    /// a no-op, so the operator opts in). ENUMERATED on/off, ⏎ mirrors space.
    AutoStartQueue,
}

/// Inline editor state for the Config detail pane. Built on entry, torn down
/// when focus returns to the account list.
#[derive(Debug, Clone)]
pub(crate) struct ConfigDraft {
    /// `None` while creating; `Some(name)` while editing. Existing accounts
    /// commit per-field on ⏎; new drafts buffer until the `create` row fires.
    pub(crate) editing_name: Option<String>,
    pub(crate) name: InputState,
    pub(crate) base_url: InputState,
    pub(crate) api_key: InputState,
    pub(crate) model: InputState,
    pub(crate) opus_model: InputState,
    pub(crate) sonnet_model: InputState,
    pub(crate) haiku_model: InputState,
    pub(crate) fable_model: InputState,
    pub(crate) subagent_model: InputState,
    pub(crate) bell_threshold: InputState,
    /// `Profile::usage_probe_interval_ms` in seconds, as the `probe every`
    /// buffer spells it.
    pub(crate) usage_probe_interval: InputState,
    /// Value buffer for the env entry currently being edited (seeded on entry).
    /// Shared across `EnvEntry` rows since only one is active at a time.
    pub(crate) env_value: InputState,
    /// Key buffer for the `+ add env` row's key editor.
    pub(crate) env_new_key: InputState,
    /// Key of an env entry added this draft whose value has had no commit
    /// since the add. Escape on that entry's editor removes the row (cancel
    /// semantics) instead of keeping the empty placeholder; any value commit
    /// on the key clears it.
    pub(crate) env_just_added: Option<String>,
    /// `Some(row)` while a text row (or the `model` custom field) owns the keyboard.
    pub(crate) active: Option<ConfigRow>,
    /// The value the open field's buffer held when it opened — the seed the
    /// changed-work derivation and the re-seed compare against (an out-of-band
    /// store change under a clean suspended edit re-seeds, never warns).
    pub(crate) field_seed: Option<String>,
    /// The account-action row (`Delete`, `ClearSessionToken`, or `Disabled` in
    /// the disable direction only — enabling is immediate, never armed)
    /// currently armed by a first ⏎; a second ⏎ on that same row confirms. Any
    /// cursor move, or the confirm itself, disarms back to `None`.
    pub(crate) armed_action: Option<ConfigRow>,
    /// API-account re-login is in flight: committing the base-url field advances
    /// to the api-key field (re-enter both, mirroring `login --base-url --api-key`)
    /// instead of ending the edit. Cleared on the api-key commit or any ⎋.
    pub(crate) relogin_chain: bool,
    /// `+ model override` reveal state. Draft-scoped: a fresh draft starts
    /// collapsed (set overrides still render; unset ones hide behind the chip).
    pub(crate) overrides_expanded: bool,
    /// The override rows this draft opened with a value, or the last preset
    /// stamp filled. They stay listed until the next stamp or the draft's end,
    /// so emptying one never pulls the row out from under the cursor; a fresh
    /// draft tucks an unset one behind the chip again.
    pub(crate) overrides_shown: Vec<ConfigRow>,
    /// A `+ new`-form login stash, held in memory until `create account`
    /// consumes it (capture-then-commit): the browser mint `+ login` produced,
    /// or the live login `+ capture current login` snapshotted. Carries the
    /// probed account uuid alongside the credentials so the anchor is seeded
    /// under the name the create actually commits — the draft's name is still
    /// editable until then. Dropped with the draft; never set on an existing
    /// account's draft.
    pub(crate) captured_login: Option<DraftLogin>,

    /// The `+ new` form's account type (the `type` cycle row).
    pub(crate) account_type: NewAccountType,

    /// A refusal shown on the name row's `└` line (a codex name held by either
    /// roster). Set at press time; cleared when the name or the type changes.
    pub(crate) name_refusal: Option<String>,
    /// A refusal shown on the create row's `└` line (a codex create with
    /// nothing staged).
    pub(crate) create_refusal: Option<String>,

    /// Env switches a preset stamped onto a `+ new` draft
    /// ([`crate::presets::Preset::env`], allowlisted), committed with the
    /// create. Empty on an existing account's draft, which applies a preset
    /// straight to disk instead.
    pub(crate) preset_env: std::collections::BTreeMap<String, String>,
}

impl ConfigDraft {
    /// The model block the six model buffers spell, each trimmed, a blank
    /// one unset.
    fn typed_models(&self) -> ModelSettings {
        ModelSettings {
            default: self.model.trimmed_some(),
            opus: self.opus_model.trimmed_some(),
            sonnet: self.sonnet_model.trimmed_some(),
            haiku: self.haiku_model.trimmed_some(),
            fable: self.fable_model.trimmed_some(),
            subagent: self.subagent_model.trimmed_some(),
        }
    }

    /// The alias-override rows whose buffer holds a value, in row order.
    fn filled_overrides(&self) -> Vec<ConfigRow> {
        OVERRIDE_ROWS
            .into_iter()
            .filter(|&row| self.field(row).is_some_and(|i| !i.value.trim().is_empty()))
            .collect()
    }

    /// The edit buffer behind a text, `Model`, or actively-edited env row; `None`
    /// for toggle/action rows. The env buffers resolve only while their row is the
    /// active one — an idle `EnvEntry` renders from the read-only snapshot instead.
    pub(crate) fn field(&self, row: ConfigRow) -> Option<&InputState> {
        Some(match row {
            ConfigRow::Name => &self.name,
            ConfigRow::BaseUrl => &self.base_url,
            ConfigRow::ApiKey => &self.api_key,
            ConfigRow::Model => &self.model,
            ConfigRow::OpusModel => &self.opus_model,
            ConfigRow::SonnetModel => &self.sonnet_model,
            ConfigRow::HaikuModel => &self.haiku_model,
            ConfigRow::FableModel => &self.fable_model,
            ConfigRow::SubagentModel => &self.subagent_model,
            ConfigRow::BellThreshold => &self.bell_threshold,
            ConfigRow::UsageProbeInterval => &self.usage_probe_interval,
            ConfigRow::EnvEntry(_) if self.active == Some(row) => &self.env_value,
            ConfigRow::EnvAdd if self.active == Some(ConfigRow::EnvAdd) => &self.env_new_key,
            ConfigRow::AutoStart
            | ConfigRow::UsageProbe
            | ConfigRow::Type
            | ConfigRow::ModelOverrideAdd
            | ConfigRow::EnvEntry(_)
            | ConfigRow::EnvAdd
            | ConfigRow::Login
            | ConfigRow::CaptureLogin
            | ConfigRow::DeleteCreds
            | ConfigRow::ClearSessionToken
            | ConfigRow::Disabled
            | ConfigRow::Delete
            | ConfigRow::Create => return None,
        })
    }

    pub(crate) fn field_mut(&mut self, row: ConfigRow) -> Option<&mut InputState> {
        Some(match row {
            ConfigRow::Name => &mut self.name,
            ConfigRow::BaseUrl => &mut self.base_url,
            ConfigRow::ApiKey => &mut self.api_key,
            ConfigRow::Model => &mut self.model,
            ConfigRow::OpusModel => &mut self.opus_model,
            ConfigRow::SonnetModel => &mut self.sonnet_model,
            ConfigRow::HaikuModel => &mut self.haiku_model,
            ConfigRow::FableModel => &mut self.fable_model,
            ConfigRow::SubagentModel => &mut self.subagent_model,
            ConfigRow::BellThreshold => &mut self.bell_threshold,
            ConfigRow::UsageProbeInterval => &mut self.usage_probe_interval,
            ConfigRow::EnvEntry(_) if self.active == Some(row) => &mut self.env_value,
            ConfigRow::EnvAdd if self.active == Some(ConfigRow::EnvAdd) => &mut self.env_new_key,
            ConfigRow::AutoStart
            | ConfigRow::UsageProbe
            | ConfigRow::Type
            | ConfigRow::ModelOverrideAdd
            | ConfigRow::EnvEntry(_)
            | ConfigRow::EnvAdd
            | ConfigRow::Login
            | ConfigRow::CaptureLogin
            | ConfigRow::DeleteCreds
            | ConfigRow::ClearSessionToken
            | ConfigRow::Disabled
            | ConfigRow::Delete
            | ConfigRow::Create => return None,
        })
    }

    /// Whether `apply preset` may act on this `+ new` draft: the form's
    /// model rows, so a preset stamped there edits fields the form never
    /// shows. The one predicate the action-menu offer and the apply share.
    pub(crate) fn apply_preset_armed(&self) -> bool {
        self.account_type == NewAccountType::Claude
    }

    /// Whether this `+ new` draft holds anything a fresh [`build_draft_new`]
    /// `+ new` form never edits env rows or an existing account, so the env
    /// buffers are always empty here; every input is checked anyway so a
    /// future field is covered.
    fn holds_typed_work(&self) -> bool {
        let inputs = [
            &self.name,
            &self.base_url,
            &self.api_key,
            &self.model,
            &self.opus_model,
            &self.sonnet_model,
            &self.haiku_model,
            &self.fable_model,
            &self.subagent_model,
            &self.bell_threshold,
            &self.usage_probe_interval,
            &self.env_value,
            &self.env_new_key,
        ];
        inputs.iter().any(|i| !i.value.is_empty())
            || self.captured_login.is_some()
            || !self.preset_env.is_empty()
    }
}

/// What a `+ new` draft's stash holds: the browser mint `+ login` finished
/// with, or the live login `+ capture current login` snapshotted off
/// `~/.claude/.credentials.json`, or the codex form's mint / capture. One slot
/// — a second stash source replaces the first, gated on a confirm.
#[derive(Debug, Clone)]
pub(crate) enum DraftLogin {
    Mint(Box<crate::oauth_login::LoginOutcome>),
    LiveLogin(Box<CaptureSnapshot>),
    /// The codex `+ login` browser flow's minted chain, staged until
    /// `create account` lands it through [`crate::actions::land_codex_login`].
    CodexMint(Box<crate::codex_login::CodexLoginOutcome>),
    /// The codex `+ capture current login` marker: `create account` moves the
    /// operator's own login through [`crate::actions::codex_login_capture`].
    CodexCapture,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfirmState {
    pub(crate) message: String,
    pub(crate) detail: Option<String>,
    /// Highlighted choice: false = cancel, true = confirm.
    pub(crate) choice: bool,
    /// Body scroll state for an overflowing confirm body: `↑`/`↓` scroll one
    /// line, `PageUp`/`PageDown` one page, while the buttons stay pinned. The
    /// render publishes the bound and the viewport; the key handler clamps.
    pub(crate) scroll: ConfirmScroll,
    pub(crate) on_confirm: ConfirmAction,
}

/// The loss warning a switch or quit opens over changed work. `save` is the
/// default focus on the three-button form; `go back` is on the two-button form
/// (changed work with nothing to save, the `+ new` form). `discard` drops the
/// work, `save` commits it, and whichever lands then performs the target fixed
/// at that keypress.
#[derive(Debug, Clone)]
pub(crate) struct LossWarningState {
    pub(crate) focus: LossWarningButton,
    /// The landing the warning resolves into.
    pub(crate) target: LossWarningTarget,
    /// The changed work `discard` drops and `save` commits, in screen order.
    pub(crate) drop: LossWarningDrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LossWarningButton {
    GoBack,
    Discard,
    Save,
}

/// What the warning's buttons resolve into: the switch or quit a key asked
/// for, or a cross-screen jump's own landing (one variant per jump).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LossWarningTarget {
    Switch(Tab),
    Quit,

    /// `start_new_account`'s landing on Setup's `+ new` row.
    NewAccount,
    /// `start_edit_selected`'s landing on the Overview row's Setup editor.
    EditAccount(EditTarget),
}

/// save resolves after a reorder or reload may have moved every index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditTarget {
    Profile(ProfileName),
    Item(SetupItem),
}

/// The changed work set. `dirty` is non-empty by construction: the warning is
/// never opened over unchanged work.
#[derive(Debug, Clone)]
pub(crate) struct LossWarningDrop {
    pub(crate) dirty: Vec<Dirty>,
}

impl LossWarningDrop {
    /// The screens holding changed work, in `Tab::ALL` order (the order the
    /// derivation produced), each once.
    pub(crate) fn screens(&self) -> Vec<Tab> {
        let mut out = Vec::new();
        for dirty in &self.dirty {
            let screen = dirty.screen();
            if !out.contains(&screen) {
                out.push(screen);
            }
        }
        out
    }

    /// Whether any changed item has a `save` (only a `+ new` form has none).
    pub(crate) fn saveable(&self) -> bool {
        self.dirty.iter().any(|d| d.saveable())
    }
}

/// One changed edit or staged pick. The derivation is the single source every
/// trigger (switch, quit, retarget, `save` validation, the body's screen list)
/// reads, so a new editor can never warn on a switch and not on a quit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dirty {
    /// A Setup existing-account field (an open typed field).
    SetupField,
    /// The `+ new` form holding typed work, a field open or not, at rest or
    /// suspended.
    NewAccountForm,
    /// A Fallback member-card typed field (`rotate at`, `weekly at`, `max
    /// spend`).
    FallbackField,
    /// The Config tab's refresh-interval custom field.
    RefreshInterval,
    /// The Config tab's context-nudge custom field.
    ContextNudge,
    /// The Config tab's weekly-threshold custom field.
    WeeklyThreshold,
    /// The Plugin tab's herdr tag-refresh field.
    HerdrTag,

    /// The Usage tab's note editor.
    Note,
}

impl Dirty {
    pub(crate) fn screen(&self) -> Tab {
        match self {
            Self::SetupField | Self::NewAccountForm => Tab::Setup,
            Self::FallbackField => Tab::Fallback,
            Self::RefreshInterval | Self::ContextNudge | Self::WeeklyThreshold => Tab::Config,
            Self::Note => Tab::Usage,
            Self::HerdrTag => Tab::Plugin,
        }
    }

    pub(crate) fn saveable(&self) -> bool {
        !matches!(self, Self::NewAccountForm)
    }
}

/// The lowercase tab label the tab bar shows, for the body's screen list.
fn tab_label(tab: Tab) -> String {
    tab.title().to_lowercase()
}

/// `the a, b and c tabs.` — one screen singular, the rest plural, no Oxford
/// comma.
fn join_screens(screens: &[Tab]) -> String {
    let labels: Vec<String> = screens.iter().copied().map(tab_label).collect();
    match labels.as_slice() {
        [] => String::new(),
        [one] => format!("the {one} tab."),
        [first, second] => format!("the {first} and {second} tabs."),
        [rest @ .., last] => format!("the {} and {last} tabs.", rest.join(", ")),
    }
}

impl LossWarningState {
    /// The body as ONE paragraph: the screen-list sentence, a space, then the
    /// question the buttons answer. The renderer wraps it at the modal's
    /// width, never one line per sentence.
    pub(crate) fn body_lines(&self) -> Vec<String> {
        let screens = join_screens(&self.drop.screens());
        let question = match (self.target.clone(), self.drop.saveable()) {
            (LossWarningTarget::Quit, true) => "save them before quitting?",
            (LossWarningTarget::Quit, false) => "discard them and quit?",
            (_, true) => "save them before switching?",
            (_, false) => "discard them and switch?",
        };
        vec![format!("there are unsaved edits on {screens} {question}")]
    }
}

/// What a published scroll bound was computed for: the frame's width and height
/// and the modal-open generation. The footer trusts a bound only while both
/// still match the current frame, so a bound left over from a resize or a
/// closed-and-reopened modal never shows a dead `↑↓ scroll`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ScrollStamp {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) generation: u64,
}

/// The scroll a confirm modal's body carries. `offset` is the key handler's
/// own state; `max_scroll` and `viewport` are what the last render drew, so a
/// resize changes the page step on the next press. `Default` keeps the many
/// one-line confirms at the top of their (fitting) bodies.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfirmScroll {
    pub(crate) offset: u16,
    pub(crate) max_scroll: std::cell::Cell<u16>,
    pub(crate) viewport: std::cell::Cell<u16>,
    pub(crate) stamp: std::cell::Cell<ScrollStamp>,
}

/// The three buttons of the apply-preset confirm, left to right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresetApplyButton {
    Cancel,
    Keep,
    Clear,
}

/// What `apply preset` stamps: the `+ new` draft, or a saved account. Fixed
/// when the picker opens, so a reload moving the roster under an open picker
/// or confirm never re-routes the apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PresetTarget {
    NewAccount,
    Account(String),
}

impl From<&str> for PresetTarget {
    fn from(name: &str) -> Self {
        Self::Account(name.to_string())
    }
}

impl From<String> for PresetTarget {
    fn from(name: String) -> Self {
        Self::Account(name)
    }
}

impl std::fmt::Display for PresetTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NewAccount => f.write_str("the new account"),
            Self::Account(name) => write!(f, "'{name}'"),
        }
    }
}

/// `apply preset` over an account (or the `+ new` draft) holding a value
/// where the preset leaves the field unset: `keep them` or `clear them`.
#[derive(Debug, Clone)]
pub(crate) struct PresetApplyForm {
    pub(crate) target: PresetTarget,
    /// The picked row, `builtin` included, so the apply re-reads the store the
    /// row came from — the saved `qwen` beside the built-in `Qwen` stays the
    /// file, never the binary's copy.
    pub(crate) preset: crate::presets::Preset,
    /// The confirm's first line, naming the preset and its target.
    pub(crate) message: String,
    /// Fields whose value the preset's own replaces, in Setup row order.
    pub(crate) replaces: Vec<&'static str>,
    /// Fields holding a value the preset leaves unset, in Setup row order.
    pub(crate) leaves: Vec<&'static str>,
    pub(crate) focus: PresetApplyButton,
    pub(crate) scroll: ConfirmScroll,
}

#[derive(Debug, Clone)]
pub(crate) enum ConfirmAction {
    /// `bool` = `from_divergence`, carried through for deferred-detach semantics.
    CaptureConflict(Box<CaptureSnapshot>, bool),
    /// Capture-name collision (issue #7): the typed name already belongs to
    /// another profile. `String` = that profile's existing (canonical-cased)
    /// name, `ProfileId` = the account it named when the confirm opened,
    /// `bool` = `from_divergence`, all carried through to
    /// `overwrite_captured_profile` the same way `CaptureConflict` carries them
    /// to `capture_into_profile`.
    CaptureOverwrite(Box<CaptureSnapshot>, String, ProfileId, bool),
    /// Divergence "save elsewhere" → a chosen (non-active) profile. `String` =
    /// that profile, `ProfileId` = the account it named when the confirm
    /// opened. Saves the live login into it and force-links it active —
    /// the guarded relink can't resolve a CC-written divergence (the on-disk
    /// byte formats differ), so this path forces the live link onto the target.
    AdoptDivergence(Box<CaptureSnapshot>, String, ProfileId),
    Switch(String),
    /// Confirm before discarding CC's freshly-written credentials and relinking.
    DiscardDivergence(String),
    /// Force-rotate all refresh tokens; active sessions may be logged out.
    RotateAll,
    /// Force-rotate one account's refresh token (action-menu "rotate access
    /// token" on the focused account).
    RotateOne(String),
    /// Disable one account (action-menu "disable account" off the Setup pane,
    /// standing in for the row's arm-then-confirm).
    DisableOne(String),
    /// Plugin tab: write the `mcpServers.tollgate` entry into `~/.claude.json`.
    /// Reversible local write — non-destructive, so it keeps the plain button.
    WireMcpServers,
    /// Setup tab: drop a profile's stored OAuth credentials, keeping the shell.
    BlankCredentials(String),
    /// Setup `+ new` draft: a login already stashed a mint (the `✓ logged in`
    /// row), so re-running would silently replace it. Confirm first, then
    /// re-dispatch `start_login`. `bool` = `is_new`, carried to the restart.
    RestartLogin(String, bool),
    /// Setup `+ new` draft: `+ capture current login` pressed while a browser
    /// mint is already stashed. Confirm, then stash the snapshot in its place.
    CaptureOverMintStash(Box<CaptureSnapshot>),
    /// Setup `+ new` draft: `space` on the `type` cycle while a staged login is
    /// held. Confirm, then change the type (dropping the staged login).
    CycleAccountType(NewAccountType),
    /// Delete row on a profile with a live `tollgate start` session: the unforced
    /// guard in `delete_profile_taking_config` refuses this, so confirm the deauth risk here
    /// and re-run the delete with `force`.
    DeleteLiveSession(String),
    /// Fallback tab `+ add`: the candidate would mix api-key and oauth accounts
    /// in the chain. Confirm carries the add through; cancel returns to the
    /// picker. Non-destructive — the member can be removed after adding.
    AddChainCandidate(String),
    /// Setup menu `save as preset`: a custom preset already holds the typed
    /// name. `(preset name, source account)` — confirming re-saves over it.
    OverwritePreset(String, String),
    /// Setup menu `apply preset`: the target already carries values the preset
    /// would replace. `(target, the picked preset, the fields the confirm
    /// names)` — the preset is re-read at apply, and a diff that moved since
    /// asks again.
    ApplyPresetOver(PresetTarget, crate::presets::Preset, Vec<&'static str>),
    /// Preset picker `d`: drop a custom preset from disk. Carries the picked
    /// row's whole preset so the confirm re-reads the store that row came from.
    DeletePreset(crate::presets::Preset),
    /// Info-only modal: an action the user asked for is refused for a reason
    /// they should read (rotating a macOS profile whose running session holds
    /// its login in a Keychain entry tollgate cannot write). Confirming just
    /// dismisses; `run_confirm_action` does nothing.
    Acknowledge,
    /// Plugin tab: run `crate::herdr::heal` on the named config file.
    HealHerdrConfig(std::path::PathBuf),
    /// Plugin tab herdr options: flip the `delegate row text` knob, persist it,
    /// then heal herdr's config so the sidebar row matches the new knob. The
    /// heal is what the confirm gates — it rewrites herdr's own file.
    HerdrDelegateRowText(std::path::PathBuf),
    /// Plugin tab: install the tollgate plugin through agentgear at user scope.
    /// A write into CC's plugin registry (driven via the `claude` CLI), so it
    /// keeps the confirm modal like every other mutating fix.
    InstallPlugin,
    /// Plugin tab: rewrite the shell's completion script through
    /// `completions::install`, the `tollgate completions install` writer, which
    /// also keeps the shell rc's `source` line current. `stale` picks the verb
    /// the confirm and a failure toast say.
    Completions {
        shell: String,
        stale: bool,
    },

    /// The shared daemon-start confirm's `start`: spawn `<this binary> daemon`
    /// detached.
    StartDaemon,

    RelinkCredentials(String),
}

/// The startup re-bind confirm: move accounts whose endpoint matches a
/// built-in preset onto that preset's current model ids. Accounts sharing one
/// change set sit in one block (the rows once, then a toggle line per account).
#[derive(Debug, Clone)]
pub(crate) struct RebindForm {
    /// Blocks in first-seen order; each holds its shared rows + account names.
    pub(crate) blocks: Vec<crate::presets::RebindBlock>,
    /// Per-block per-account checked state, parallel to `blocks`.
    pub(crate) checked: Vec<Vec<bool>>,
    /// Flat toggle-line index the cursor sits on, across blocks in render order.
    pub(crate) cursor: usize,
    /// Highlighted button: false = `not now`, true = `apply`.
    pub(crate) choice: bool,
    /// Followable body scroll offset, published and followed by the render.
    pub(crate) scroll: std::cell::Cell<usize>,
    /// Published by the last render: the chunked start line of each account
    /// toggle (flat order), its one-line block end, the total line count and the
    /// viewport, so the page keys move the cursor the way the other choice
    /// menus do.
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    /// The digest this offer stands for; stored on either exit.
    pub(crate) digest: String,
}

impl RebindForm {
    pub(crate) fn account_count(&self) -> usize {
        self.blocks.iter().map(|b| b.accounts.len()).sum()
    }

    /// Map a flat toggle-line index to `(block, account)`.
    pub(crate) fn account_at(&self, idx: usize) -> Option<(usize, usize)> {
        let mut remaining = idx;
        for (bi, block) in self.blocks.iter().enumerate() {
            if remaining < block.accounts.len() {
                return Some((bi, remaining));
            }
            remaining -= block.accounts.len();
        }
        None
    }

    /// The `(name, rows)` moves for every checked account, in render order.
    pub(crate) fn checked_moves(&self) -> Vec<(String, Vec<crate::presets::RowMove>)> {
        let mut out = Vec::new();
        for (block, checked) in self.blocks.iter().zip(&self.checked) {
            for (account, &on) in block.accounts.iter().zip(checked) {
                if on {
                    out.push((account.clone(), block.rows.clone()));
                }
            }
        }
        out
    }
}

/// One-field name prompt shared by the Setup menu's two naming actions. The
/// variant carries what the typed name is FOR, so the modal itself stays a plain
/// input and every validation rule lives in the handler that consumes it.
#[derive(Debug, Clone)]
pub(crate) struct NamePromptForm {
    pub(crate) input: InputState,
    pub(crate) action: NamePromptAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NamePromptAction {
    /// Copy this account onto a new one under the typed name.
    DuplicateProfile(String),
    /// Save this account's base url + models as a preset under the typed name.
    SavePreset(String),
    /// [`Modal::BareItem`]'s `save` with no profile cached with the login's
    /// account: a new profile takes the login ([`App::bare_item_naming`]),
    /// then the held write resumes.
    SaveBareItem,
}

impl NamePromptAction {
    /// Modal title, and the blurb under it — both read off the same variant so a
    /// new action can't ship with the other one's copy.
    pub(crate) fn title(&self) -> &'static str {
        match self {
            Self::DuplicateProfile(_) => "DUPLICATE",
            Self::SavePreset(_) => "SAVE PRESET",
            Self::SaveBareItem => "CAPTURE",
        }
    }

    pub(crate) fn blurb(&self) -> String {
        match self {
            Self::DuplicateProfile(source) => {
                format!("copies every setting of '{source}' except its login.")
            }
            Self::SavePreset(source) => {
                format!("stores '{source}'s base url and model settings under this name.")
            }
            Self::SaveBareItem => "name for the new profile:".to_string(),
        }
    }
}

/// Preset picker for `apply preset`. Built-ins lead the list (see
/// [`crate::presets::list_presets`]), so the cursor's index is the list's index
/// and nothing has to track the split.
#[derive(Debug, Clone)]
pub(crate) struct PresetPickerForm {
    /// The account the pick is stamped onto.
    pub(crate) target: PresetTarget,
    pub(crate) presets: Vec<crate::presets::Preset>,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct CaptureNameForm {
    pub(crate) snapshot: Box<CaptureSnapshot>,
    pub(crate) input: InputState,
    /// Set when initiated by `NewProfile` divergence. Detach + deactivate of
    /// the prior profile is deferred to the success arm so cancel leaves it intact.
    pub(crate) from_divergence: bool,
}

/// Credential-divergence prompt. Raised on demand (the <kbd>d</kbd> key from
/// the non-blocking banner) and when an action that REQUIRES resolution is
/// attempted (switch / switch-off / plugin fix) — never auto-pushed at startup
/// or from the 1Hz poll, so a divergence can't lock the whole TUI
/// (`divergence_pending` + the banner carry the signal instead).
#[derive(Debug, Clone)]
pub(crate) struct DivergenceForm {
    pub(crate) active: String,
    /// The profile the live login was identified as belonging to
    /// ([`crate::actions::identify_live_login_owner`], local evidence), when
    /// that profile is NOT the active one. Surfaces a first-class "switch to
    /// it" action — the near-always-right resolution for a CC re-login into a
    /// known account, which the generic three options all get wrong.
    pub(crate) sibling: Option<ProfileName>,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

/// The bare macOS Keychain item as a TUI writer reads it, always on a worker:
/// the settle before the write ([`crate::claude::settle_bare_keychain_item`]),
/// the item as the user's answer finds it
/// ([`crate::claude::read_bare_keychain_item`]), and where a `save` puts its
/// login ([`crate::claude::bare_item_save`], off the settle's remembered
/// lookup). `None` on [`App`] where no such item exists.
#[derive(Clone, Copy)]
pub(crate) struct BareItemSource {
    pub(crate) settle: fn(crate::claude::BareItemWriter) -> Result<crate::claude::BareItemSettle>,
    pub(crate) read: fn() -> Option<serde_json::Value>,
    pub(crate) save: fn(&serde_json::Value) -> crate::claude::BareItemSave,
}

impl std::fmt::Debug for BareItemSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BareItemSource")
    }
}

/// The source this build reads: the real Keychain on macOS.
#[cfg(all(target_os = "macos", not(test)))]
fn default_bare_item_source() -> Option<BareItemSource> {
    Some(BareItemSource {
        settle: crate::claude::settle_bare_keychain_item,
        read: crate::claude::read_bare_keychain_item,
        save: crate::claude::bare_item_save,
    })
}

/// No bare item exists off macOS; a test installs its own source.
#[cfg(not(all(target_os = "macos", not(test))))]
fn default_bare_item_source() -> Option<BareItemSource> {
    #[cfg(test)]
    {
        TEST_BARE_ITEM_SOURCE.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// A TUI write that replaces the bare item, held while its settle runs on a
/// worker or the user answers [`Modal::BareItem`], then run again once it may
/// write ([`resume_bare_item_write`]).
#[derive(Debug, Clone)]
pub(crate) enum BareItemWrite {
    /// The bootstrap's relink of the active profile.
    BootRelink(ProfileName),
    Switch(ProfileName),
    SwitchOff,
    ClearSessionToken(ProfileName),
    /// Renaming the active profile in the open Setup draft.
    Rename,
    /// The `+ new` form's create, whose fresh login becomes the active one.
    NewAccount,
    Delete {
        name: ProfileName,
        force: bool,
    },
    CaptureOverwrite {
        snapshot: Box<CaptureSnapshot>,
        name: String,
        account: ProfileId,
        from_divergence: bool,
    },
    AdoptDivergence {
        snapshot: Box<CaptureSnapshot>,
        name: String,
        account: ProfileId,
    },
    BlankCredentials(String),
    DiscardDivergence(String),
    /// The divergence prompt's `overwrite` of the active profile.
    DivergenceOverwrite(String),
    CaptureName {
        name: String,
        snapshot: Box<CaptureSnapshot>,
        from_divergence: bool,
    },
    /// A finished browser login applied without a confirm.
    Login {
        name: String,
        account: ProfileId,
        snapshot: Box<CaptureSnapshot>,
    },
}

impl BareItemWrite {
    pub(crate) fn writer(&self) -> crate::claude::BareItemWriter {
        match self {
            Self::SwitchOff | Self::Delete { .. } | Self::BlankCredentials(_) => {
                crate::claude::BareItemWriter::SignOut
            }
            Self::BootRelink(_) => crate::claude::BareItemWriter::Relink,
            Self::Switch(_)
            | Self::ClearSessionToken(_)
            | Self::Rename
            | Self::NewAccount
            | Self::CaptureOverwrite { .. }
            | Self::AdoptDivergence { .. }
            | Self::DiscardDivergence(_)
            | Self::DivergenceOverwrite(_)
            | Self::CaptureName { .. }
            | Self::Login { .. } => crate::claude::BareItemWriter::Switch,
        }
    }

    /// The first line of the toast a failed settle raises, the write's own.
    fn failed(&self) -> &'static str {
        match self {
            Self::BootRelink(_) | Self::ClearSessionToken(_) => "relink failed",
            Self::Switch(_) => "switch failed",
            Self::SwitchOff => "switch-off failed",
            Self::Rename => "rename failed",
            Self::NewAccount => "create failed",
            Self::Delete { .. } => "delete failed",
            Self::CaptureOverwrite { .. } | Self::DivergenceOverwrite(_) => "overwrite failed",
            Self::AdoptDivergence { .. } => "save failed",
            Self::BlankCredentials(_) => "log out failed",
            Self::DiscardDivergence(_) => "discard failed",
            Self::CaptureName { .. } => "capture failed",
            Self::Login { .. } => "login failed",
        }
    }
}

/// An answer to [`Modal::BareItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BareItemChoice {
    Save,
    Replace,
    Cancel,
}

impl BareItemChoice {
    pub(crate) const ALL: [Self; 3] = [Self::Save, Self::Replace, Self::Cancel];

    pub(crate) fn key(self) -> char {
        match self {
            Self::Save => 's',
            Self::Replace => 'p',
            Self::Cancel => 'c',
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Replace => "replace",
            Self::Cancel => "cancel",
        }
    }

    /// What the choice does, naming the held write's operation.
    pub(crate) fn description(self, writer: crate::claude::BareItemWriter) -> String {
        let action = writer.action();
        match self {
            Self::Save => {
                format!("save it as an account (asks for a name if none matches), then {action}")
            }
            Self::Replace => {
                format!("{action} anyway; that account will need a browser login again")
            }
            Self::Cancel => "leave everything as it is".to_string(),
        }
    }
}

/// The bare item holds a login no tollgate account saved and the settle cannot
/// place it: the held write waits on the user's choice.
#[derive(Debug, Clone)]
pub(crate) struct BareItemForm {
    pub(crate) unplaced: crate::claude::BareItemUnplaced,
    pub(crate) write: BareItemWrite,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

impl BareItemForm {
    pub(crate) fn new(unplaced: crate::claude::BareItemUnplaced, write: BareItemWrite) -> Self {
        Self {
            unplaced,
            write,
            cursor: 0,
            scroll: std::cell::Cell::new(0),
            viewport: std::cell::Cell::new(0),
            starts: std::cell::RefCell::new(Vec::new()),
            block_ends: std::cell::RefCell::new(Vec::new()),
            total: std::cell::Cell::new(0),
        }
    }
}

/// `save` found two profiles cached with the login's account: the held write
/// waits on which one takes it.
#[derive(Debug, Clone)]
pub(crate) struct BareItemOwnerForm {
    pub(crate) owners: [ProfileName; 2],
    /// The account each of `owners` held when the question opened: the pick
    /// lands only while its name still holds it.
    pub(crate) accounts: [ProfileId; 2],
    /// The login the question is about ([`crate::claude::bare_item_login_key`]):
    /// the pick saves the item only while it still holds that login.
    pub(crate) login: Option<[u8; 32]>,
    pub(crate) write: BareItemWrite,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

impl BareItemOwnerForm {
    pub(crate) fn new(
        owners: [ProfileName; 2],
        accounts: [ProfileId; 2],
        login: Option<[u8; 32]>,
        write: BareItemWrite,
    ) -> Self {
        Self {
            owners,
            accounts,
            login,
            write,
            cursor: 0,
            scroll: std::cell::Cell::new(0),
            viewport: std::cell::Cell::new(0),
            starts: std::cell::RefCell::new(Vec::new()),
            block_ends: std::cell::RefCell::new(Vec::new()),
            total: std::cell::Cell::new(0),
        }
    }
}

/// What a bare-item worker posts back to the UI thread for a held write.
pub(crate) struct BareItemAnswer {
    write: BareItemWrite,
    outcome: BareItemOutcome,
}

enum BareItemOutcome {
    /// A settle answered, the first or one after a save.
    Settled(Result<crate::claude::BareItemSettle>),
    /// `replace`: the key the writer guards take to replace the item as the
    /// answer found it.
    Replace(Option<[u8; 32]>),
    /// `save` found no profile cached with the login's account: a new one
    /// takes it, under a name the user types.
    NameNeeded {
        login: crate::profile::ClaudeCredentials,
        account: Option<crate::profile::AccountId>,
    },
    /// `save` found two profiles cached with the login's account: the user
    /// picks which one takes it. Carries the account each name held when
    /// the save named it, and the login's key.
    OwnerNeeded([ProfileName; 2], [ProfileId; 2], Option<[u8; 32]>),
}

/// The non-blocking divergence signal behind the accounts-pane banner: set by
/// the 1Hz poll / startup reconcile, cleared the moment the live link matches
/// again. <kbd>d</kbd> opens the resolver from it.
#[derive(Debug, Clone)]
pub(crate) struct DivergenceNotice {
    pub(crate) active: String,
    /// Locally identified owner of the live login when it is a NON-active
    /// profile (see [`DivergenceForm::sibling`]); drives the banner wording.
    pub(crate) sibling: Option<ProfileName>,
    /// SipHash of the live access token at identification time — the memo that
    /// keeps the 1Hz poll from re-running the owner lookup while the live login
    /// is unchanged.
    pub(crate) fingerprint: Option<u64>,
}

impl DivergenceNotice {
    /// Banner copy for the one system banner: names the live login's owner when
    /// known, else the generic mismatch, ending in the `d` affordance. Lowercase
    /// fragments, mid-dot separators.
    pub(crate) fn banner_message(&self) -> String {
        match &self.sibling {
            Some(owner) => format!(
                "live login is '{owner}' · not the active '{}' · press {} to resolve",
                self.active,
                prose::key("d")
            ),
            None => format!(
                "live login no longer matches '{}' · press {} to resolve",
                self.active,
                prose::key("d")
            ),
        }
    }
}

/// One row on the Divergence prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DivergenceAction {
    /// The live login belongs to this (non-active) profile: capture it there
    /// and make that profile active — the [`ConfirmAction::AdoptDivergence`]
    /// path the "save elsewhere" picker already uses, promoted to the top.
    SwitchToOwner(String),
    Choice(DivergenceChoice),
}

impl DivergenceForm {
    pub(crate) fn actions(&self) -> Vec<DivergenceAction> {
        let mut v = Vec::with_capacity(4);
        if let Some(owner) = &self.sibling {
            v.push(DivergenceAction::SwitchToOwner(owner.to_string()));
        }
        v.extend([
            DivergenceAction::Choice(DivergenceChoice::Overwrite),
            DivergenceAction::Choice(DivergenceChoice::NewProfile),
            DivergenceAction::Choice(DivergenceChoice::Discard),
        ]);
        v
    }
}

/// "Save the live login to another profile" picker, opened from the Divergence
/// modal's second action. Row 0 is `+ new profile`; rows 1.. are existing
/// profiles (the active/diverged one excluded). A refresh-token match is
/// pre-selected so re-logging an account you already hold is one keypress.
#[derive(Debug, Clone)]
pub(crate) struct DivergenceTargetForm {
    pub(crate) targets: Vec<String>,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

/// A choice on the env-key collision prompt (3 vertical options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvCollisionChoice {
    /// Add the custom field anyway — its value overrides the colliding source.
    Overwrite,
    /// Leave the existing value untouched; don't add. Jumps to the existing
    /// custom field when the collision is with one.
    KeepExisting,
    /// Back out, no change.
    Cancel,
}

/// Prompt shown when a freshly typed custom env key already exists in one of the
/// three sources ([`EnvKeyCollision`]). Modeled on [`DivergenceForm`] — a message
/// plus arrow-selected options. `cancel` is the default-focused (safe) choice.
#[derive(Debug, Clone)]
pub(crate) struct EnvCollisionForm {
    pub(crate) profile: String,
    pub(crate) key: String,
    /// Human reason for the collision (`set by the base url field`, …).
    pub(crate) reason: String,
    /// Sorted `EnvEntry` index of the colliding own-field, for `keep existing`.
    pub(crate) existing_idx: Option<usize>,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) reviewing_overwrite: bool,
    pub(crate) confirm_overwrite: bool,
    pub(crate) max_scroll: std::cell::Cell<u16>,
    pub(crate) max_scroll_stamp: std::cell::Cell<ScrollStamp>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) overwrite_needs_review: std::cell::Cell<bool>,
}

impl EnvCollisionForm {
    pub(crate) fn options() -> [EnvCollisionChoice; 3] {
        [
            EnvCollisionChoice::Overwrite,
            EnvCollisionChoice::KeepExisting,
            EnvCollisionChoice::Cancel,
        ]
    }
}

/// One entry in the action menu.
#[derive(Debug, Clone)]
pub(crate) struct ActionItem {
    pub(crate) label: &'static str,
    /// Single-letter shortcut auto-assigned by the hotkey algorithm; `None` when
    /// all candidate characters were already taken.
    pub(crate) hotkey: Option<char>,
    /// The logical action to fire when this item is selected.
    pub(crate) action: ActionMenuAction,
}

/// Logical actions the action menu can dispatch. Each variant maps 1-to-1 to
/// an existing key handler so there is no duplicated dispatch logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionMenuAction {
    // Global
    NewAccount,
    RefreshUsage,
    /// Force-refresh every account — what the global `r` does off the Usage and
    /// Tokens tabs, and the only way to reach it from Usage (where `r` is the
    /// focused account's own refresh).
    RefreshAll,
    RotateTokens,
    /// The focused account's disabled flag, off the Setup pane (Overview and
    /// Usage, which carry no row for it).
    DisableProfile,
    EnableProfile,
    /// Open the focused account's provider console — where its api key is
    /// minted. Offered only for a recognised third-party endpoint, since that
    /// is the only case tollgate knows a page for.
    OpenProviderConsole,
    /// `tollgate daemon`, detached from the TUI, offered while none runs.
    StartDaemon,
    /// `--replace`'s termination with no successor, offered while one runs.
    StopDaemon,

    // Setup tab — all three act on the focused account and none has a key.
    /// Copy every setting of the focused account onto a new one, credentials
    /// excluded. Prompts for the new name.
    Duplicate,
    /// Store the focused account's base url + model settings as a named
    /// [`crate::presets::Preset`]. Prompts for the name.
    SaveAsPreset,
    /// Stamp a preset's base url + model settings onto the focused account.
    /// Opens the picker.
    ApplyPreset,
    // Status tab
    RefreshStatus,
    OpenIncidentLink,
    // Usage
    ToggleEstimates,
    TogglePace,
    // Tokens
    TokensPeriodLifetime,
    TokensPeriodDaily,
    TokensPeriodWeekly,
    TokensPeriodMonthly,
    TokensShowAll,
    TokensShowClaude,
    TokensShowOthers,
    ToggleCountCache,
    ReloadTokenStats,
    VerifyLogin,
}

/// What a `start daemon` / `stop daemon` worker reports, once.
#[derive(Debug)]
pub(crate) enum DaemonControlResult {
    Start(std::result::Result<crate::daemon::StartOutcome, String>),
    Stop(std::result::Result<crate::daemon::DaemonStop, String>),
}

#[derive(Debug, Clone)]
pub(crate) enum RowIdentity {
    Profile(ProfileName),
}

/// State for the action-menu modal.
#[derive(Debug, Clone)]
pub(crate) struct ActionMenuState {
    /// Account-scoped items first, then the tab-global ones. One flat list so
    /// the cursor and the hotkey scan stay index-free of the split; the render
    /// draws the group rule at `scoped_len`.
    pub(crate) items: Vec<ActionItem>,
    /// How many leading `items` act on [`ActionMenuState::context`].
    pub(crate) scoped_len: usize,
    /// The focused account, named in the title bar so the scoped items don't
    /// each need a suffix. `None` whenever the scoped group is empty — a name
    /// on a menu of tab-global actions would claim a scope that isn't there.
    pub(crate) context: Option<String>,
    /// The row the scoped actions were built for; `None` when the menu has no
    /// scoped actions. Every per-row pick resolves this identity at dispatch,
    /// so a roster or listing change that drops the row between open and pick
    /// acts on nothing, never on a neighbour the cursor re-homed onto.
    pub(crate) row_identity: Option<RowIdentity>,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    /// Visible menu rows from the last render (the page step).
    pub(crate) viewport: std::cell::Cell<usize>,
    /// Chunked row starts/ends per item and the total row count, published by
    /// the render so the page step accounts for the group-rule line.
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

impl ActionMenuState {
    /// Build and assign hotkeys in source order per the SKILL.md algorithm.
    /// Reserved keys: `a` `x` `?` `q`.
    pub(crate) fn new(
        scoped: Vec<ActionMenuAction>,
        global: Vec<ActionMenuAction>,
        context: Option<String>,
        row_identity: Option<RowIdentity>,
    ) -> Self {
        const RESERVED: &[char] = &['a', 'x', '?', 'q'];
        let scoped_len = scoped.len();
        let context = (scoped_len > 0).then_some(context).flatten();
        let mut claimed: Vec<char> = Vec::new();
        let items = scoped
            .into_iter()
            .chain(global)
            .map(|action| {
                let label = action.label();
                // An explicit override wins when free; else scan the first 3 alpha
                // chars of the label per the SKILL.md algorithm.
                let hotkey = action
                    .preferred_hotkey()
                    .filter(|c| !RESERVED.contains(c) && !claimed.contains(c))
                    .or_else(|| {
                        label
                            .chars()
                            .filter(|c| c.is_alphabetic())
                            .map(|c| c.to_lowercase().next().unwrap_or(c))
                            .take(3)
                            .find(|c| !RESERVED.contains(c) && !claimed.contains(c))
                    })
                    .inspect(|c| claimed.push(*c));
                ActionItem {
                    label,
                    hotkey,
                    action,
                }
            })
            .collect();
        Self {
            items,
            scoped_len,
            context,
            row_identity,
            cursor: 0,
            scroll: std::cell::Cell::new(0),
            viewport: std::cell::Cell::new(0),
            starts: std::cell::RefCell::new(Vec::new()),
            block_ends: std::cell::RefCell::new(Vec::new()),
            total: std::cell::Cell::new(0),
        }
    }
}

impl ActionMenuAction {
    /// Explicit hotkey override, taking priority over the label scan when free.
    /// `rotate tokens` pins `t` (mnemonic + matches the global rotate key) instead
    /// of falling to `o` after `refresh usage` claims `r`.
    fn preferred_hotkey(&self) -> Option<char> {
        match self {
            Self::VerifyLogin => Some('v'),
            Self::RotateTokens => Some('t'),
            Self::ToggleEstimates => Some('e'),
            Self::TogglePace => Some('p'),
            // `r` belongs to the focused account's own refresh, and `e`/`p` to
            // the Usage page keys above, so refresh-all takes the third letter
            // of its label — pinned rather than scanned so it doesn't move with
            // the tab. (Uppercase `R` is out: hotkeys are case-insensitive.)
            Self::RefreshAll => Some('f'),
            // One letter across both halves of the disable/enable pair, so the
            // key doesn't move when the account's state flips.
            Self::DisableProfile | Self::EnableProfile => Some('d'),
            // Mirror the Tokens tab's page keys so the menu teaches them.
            Self::ToggleCountCache => Some('c'),
            Self::ReloadTokenStats => Some('r'),
            // Period rows all start with "period: ", so pin each to its
            // distinguishing word instead of the label scan.
            Self::TokensPeriodLifetime => Some('l'),
            Self::TokensPeriodDaily => Some('d'),
            Self::TokensPeriodWeekly => Some('w'),
            Self::TokensPeriodMonthly => Some('m'),
            _ => None,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::VerifyLogin => "verify login",
            Self::NewAccount => "new account",
            Self::RefreshUsage => "refresh usage",
            Self::RefreshAll => "refresh all accounts",
            Self::RotateTokens => "rotate access token",
            Self::DisableProfile => "disable account",
            Self::EnableProfile => "enable account",
            Self::OpenProviderConsole => "open provider console",
            Self::StartDaemon => "start daemon",
            Self::StopDaemon => "stop daemon",
            Self::Duplicate => "duplicate account",
            Self::SaveAsPreset => "save as preset",
            Self::ApplyPreset => "apply preset",
            Self::RefreshStatus => "refresh status",
            Self::OpenIncidentLink => "open in browser",
            Self::ToggleEstimates => "toggle estimates",
            Self::TogglePace => "toggle pace marker",
            Self::TokensPeriodLifetime => "period: lifetime",
            Self::TokensPeriodDaily => "period: daily",
            Self::TokensPeriodWeekly => "period: weekly",
            Self::TokensPeriodMonthly => "period: monthly",
            Self::TokensShowAll => "show all models",
            Self::TokensShowClaude => "show claude models",
            Self::TokensShowOthers => "show other models",
            Self::ToggleCountCache => "toggle cache counting",
            Self::ReloadTokenStats => "reload stats",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Modal {
    Confirm(ConfirmState),
    /// Credential divergence prompt.
    Divergence(DivergenceForm),
    CaptureName(CaptureNameForm),
    /// One-field name prompt behind the Setup menu's `duplicate account` and
    /// `save as preset`.
    NamePrompt(NamePromptForm),
    /// Setup menu `apply preset`: pick which preset stamps the account.
    PresetPicker(PresetPickerForm),
    /// `apply preset` over set fields the preset leaves unset: keep or clear.
    PresetApply(PresetApplyForm),
    /// Divergence "save elsewhere": pick which profile the live login lands in.
    DivergenceTarget(DivergenceTargetForm),

    Help,
    /// Context-sensitive action menu opened by `a`.
    ActionMenu(ActionMenuState),
    /// Custom env key collides with an existing source; overwrite/keep/cancel.
    EnvCollision(EnvCollisionForm),
    /// In-flight login progress; renders live from [`App::login`], the inline
    /// code field ([`LoginSession::paste_field`]) included. esc/q collapse it
    /// to the footer indicator — the login keeps running.
    Login,
    /// In-flight codex browser re-login progress; renders live from
    /// [`App::codex_login`]. Its own shell: `r` opens the browser again, `c`
    /// copies the authorize link, no paste door — codex's callback answers on
    /// this machine alone.
    CodexLogin,

    /// Startup re-bind confirm: move matching accounts onto the built-in
    /// presets' current ids.
    Rebind(RebindForm),
    /// A screen switch away from unsaved work another screen also writes (see
    /// Tab bar → Switching tabs → Loss warning).
    LossWarning(LossWarningState),
    /// A write would replace a macOS Keychain login no tollgate account saved:
    /// save it, replace it, or cancel the write.
    BareItem(BareItemForm),
    /// [`Modal::BareItem`]'s `save` over a login two profiles are cached with
    /// the account of: pick which one takes it.
    BareItemOwner(BareItemOwnerForm),
    MoveSession(MoveSessionForm),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    Info,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub(crate) kind: ToastKind,
    pub(crate) body: String,
    pub(crate) born: Instant,
}

const ROTATE_ALL_MSG: &str = "rotate all access tokens?";

#[cfg(target_os = "macos")]
const ROTATE_ALL_DETAIL: &str =
    "accounts with a live tollgate start or background session are skipped.";
#[cfg(not(target_os = "macos"))]
const ROTATE_ALL_DETAIL: &str = "running sessions pick up the new tokens on their next request.";

/// The macOS-only single-rotate refusal, in both the shapes it is surfaced in:
/// an acknowledge modal from the action menu, and a toast if a session starts
/// between arming the confirm and running it.
///
/// Defined on every platform though only macOS reads them (the branch is
/// `cfg!`, so it still compiles here) — that keeps the strings reviewable and
/// exact-match pinnable from a Linux run.
const ROTATE_LIVE_SESSION_MSG: &str = "has a live tollgate start session";

const ROTATE_BACKGROUND_SESSION_MSG: &str = "has a background session";

const ROTATE_UNREADABLE_JOBS_MSG: &str = "can't be checked for background sessions";

const ROTATE_LIVE_SESSION_DETAIL: &str = "macos keeps its login in a keychain entry tollgate can't write, so rotating would sign the \
     session out.";

const ROTATE_LIVE_SESSION_TOAST: &str = "macos keeps its login where tollgate can't rotate it";

/// The single-rotate refusal sentence for a blocked rotate, one suffix per
/// cause ([`crate::runtime::RotationBlock`]), so the operator reads what
/// actually blocks it: a launcher's own session, a background job, or a job
/// store tollgate cannot read.
fn rotate_blocked_message(name: &str, block: crate::runtime::RotationBlock) -> String {
    let suffix = match block {
        crate::runtime::RotationBlock::LiveSession => ROTATE_LIVE_SESSION_MSG,
        crate::runtime::RotationBlock::BackgroundJob => ROTATE_BACKGROUND_SESSION_MSG,
        crate::runtime::RotationBlock::UnreadableJobStore(_) => ROTATE_UNREADABLE_JOBS_MSG,
    };
    format!("'{name}' {suffix}")
}

/// What disabling costs, for the action menu's confirm. The Setup row states the
/// same thing in its own hint (`config::row_hint`) before arming.
const DISABLE_DETAIL: &str =
    "it drops out of auto-switch, usage polling, and status until re-enabled.";

const TOAST_CAPACITY: usize = 3;

const TOAST_TTL_NORMAL: Duration = Duration::from_secs(5);

const TOAST_TTL_DANGER: Duration = Duration::from_secs(6);

/// Top-level views (⇥/⇧⇥/←→). All tabs share background workers; only the
/// body and keymap differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    /// Accounts table + at-a-glance usage.
    Overview,
    /// Per-account usage breakdown.
    Usage,
    /// Global token usage across past Claude Code sessions (from `~/.claude`).
    Tokens,
    /// Per-account settings (endpoint, rename, auto-start, delete).
    Setup,
    /// Fallback chain editor — ordering and per-member thresholds.
    Fallback,
    /// Program-wide settings: theme tier and global defaults.
    Config,
    /// Claude service status feed (incidents from status.claude.com).
    Status,
    /// Claude Code integration health: MCP wiring, plugin install, per-profile runtime.
    Plugin,
}

impl Tab {
    pub(crate) const ALL: [Tab; 8] = [
        Tab::Overview,
        Tab::Usage,
        Tab::Tokens,
        Tab::Setup,
        Tab::Fallback,
        Tab::Config,
        Tab::Status,
        Tab::Plugin,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Usage => "Usage",
            Tab::Tokens => "Tokens",
            Tab::Setup => "Setup",
            Tab::Fallback => "Fallback",
            Tab::Config => "Config",
            Tab::Status => "Status",
            Tab::Plugin => "Plugin",
        }
    }

    pub(crate) fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub(crate) fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub(crate) fn prev(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

impl From<Tab> for HomeTab {
    fn from(tab: Tab) -> Self {
        match tab {
            Tab::Overview => HomeTab::Overview,
            Tab::Usage => HomeTab::Usage,
            Tab::Tokens => HomeTab::Tokens,
            Tab::Setup => HomeTab::Setup,
            Tab::Fallback => HomeTab::Fallback,
            Tab::Config => HomeTab::Config,
            Tab::Status => HomeTab::Status,
            Tab::Plugin => HomeTab::Plugin,
        }
    }
}

impl From<HomeTab> for Tab {
    fn from(tab: HomeTab) -> Self {
        match tab {
            HomeTab::Overview => Tab::Overview,
            HomeTab::Usage => Tab::Usage,
            HomeTab::Tokens => Tab::Tokens,
            HomeTab::Setup => Tab::Setup,
            HomeTab::Fallback => Tab::Fallback,
            HomeTab::Config => Tab::Config,
            HomeTab::Status => Tab::Status,
            HomeTab::Plugin => Tab::Plugin,
        }
    }
}

/// Which view the Tokens tab shows. `Dashboard` is the landing page (totals +
/// charts); `Models` is the descend-into per-model master-detail breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenView {
    Dashboard,
    Models,
}

/// One card of the Tokens dashboard. Focus is stored as this (not an index)
/// so it keeps meaning across a resize that changes the layout's reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardKind {
    First,
    Total,
    Trend,
    Models,
    Comp,
    Hour,
    Activity,
}

/// Display filter over the Tokens tab's grouped model list (top-models card +
/// the Models view), set from the tab's action menu. Session-only — a lens,
/// not a setting. The aggregate cards (today/total/daily/…) stay unfiltered:
/// the daily trend has no per-model split, so filtering only some cards would
/// let "today" contradict the trend next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TokenFilter {
    #[default]
    All,
    Claude,
    Others,
}

impl TokenFilter {
    pub(crate) fn matches(self, model: &str) -> bool {
        match self {
            TokenFilter::All => true,
            TokenFilter::Claude => crate::tokens::is_anthropic(model),
            TokenFilter::Others => !crate::tokens::is_anthropic(model),
        }
    }

    /// Title-right meta badge for the filtered model surfaces; `None` when off.
    pub(crate) fn badge(self) -> Option<&'static str> {
        match self {
            TokenFilter::All => None,
            TokenFilter::Claude => Some("claude only"),
            TokenFilter::Others => Some("others only"),
        }
    }
}

/// Time-window lens over the Tokens tab, cycled with `t` or set from the
/// action menu. Session-only — a lens, not a setting, like [`TokenFilter`].
/// `Lifetime` (the default) is the untouched all-time dashboard; the other
/// three scope the cards to today / this calendar week / this calendar month.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TokenPeriod {
    #[default]
    Lifetime,
    Daily,
    Weekly,
    Monthly,
}

impl TokenPeriod {
    /// Cycle order for the `t` key, wrapping.
    pub(crate) fn next(self) -> Self {
        match self {
            TokenPeriod::Lifetime => TokenPeriod::Daily,
            TokenPeriod::Daily => TokenPeriod::Weekly,
            TokenPeriod::Weekly => TokenPeriod::Monthly,
            TokenPeriod::Monthly => TokenPeriod::Lifetime,
        }
    }

    /// Title-right meta badge naming the scoped window; `None` when lifetime.
    pub(crate) fn badge(self) -> Option<&'static str> {
        match self {
            TokenPeriod::Lifetime => None,
            TokenPeriod::Daily => Some("today"),
            TokenPeriod::Weekly => Some("this week"),
            TokenPeriod::Monthly => Some("this month"),
        }
    }

    /// Like [`Self::badge`] but always names the lens — `lifetime` included —
    /// so the lens-bearing surfaces never render an empty badge slot (a badge
    /// that only sometimes appears reads as an anomaly, not a lens).
    pub(crate) fn lens_badge(self) -> &'static str {
        self.badge().unwrap_or("lifetime")
    }

    /// Calendar bucket for the trend cards; `None` = per-day rows.
    pub(crate) fn bucket(self) -> Option<crate::tokens::Bucket> {
        match self {
            TokenPeriod::Weekly => Some(crate::tokens::Bucket::Week),
            TokenPeriod::Monthly => Some(crate::tokens::Bucket::Month),
            TokenPeriod::Lifetime | TokenPeriod::Daily => None,
        }
    }
}

/// Which Config pane owns the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFocus {
    Profiles,
    Actions,
}

/// Which Fallback pane has focus. `Chain`: ordered list (↑↓, ⇧↑↓ reorders, ⏎
/// enters). `Detail`: threshold stepper + remove row, or add-candidate picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackFocus {
    Chain,
    Detail,
}

/// Which Status pane has focus. `List`: the incident selector (↑↓ + ⏎ descends).
/// `Detail`: the selected incident's timeline (↑↓ scrolls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusFocus {
    List,
    Detail,
}

/// UI-thread-only state for the Status tab. Fed by [`StatusEvent`]s drained in
/// `on_tick`; never shared with the background thread (which only sends events).
#[derive(Debug)]
pub(crate) struct StatusState {
    /// Incidents in feed order (newest first).
    pub(crate) incidents: Vec<Incident>,
    /// Wall-clock ms of the last successful fetch / cache load; `None` until any
    /// event arrives. Drives the "cached / stale" age cue.
    pub(crate) fetched_at_ms: Option<u64>,
    /// True when the last event was `Cached` (startup cache or fetch-failed
    /// fallback) — render the data as cached rather than fresh.
    pub(crate) cached: bool,
    /// Set only when a fetch failed with nothing cached to show.
    pub(crate) error: Option<String>,
    /// A manual refresh is in flight → the panel title shows a spinner.
    pub(crate) fetching: bool,
    /// Selected incident index in `incidents`.
    pub(crate) cursor: usize,
    pub(crate) focus: StatusFocus,
    pub(crate) selector: ListView,
    /// Scroll offset (lines) into the detail timeline.
    pub(crate) detail_scroll: u16,
    /// Max valid `detail_scroll` from the last detail render (`total - viewport`).
    /// The render pass owns the real geometry, so it writes the bound here (via
    /// `&App` interior mutability) and the key handler clamps against it — that
    /// stops a held ↓ from inflating `detail_scroll` toward `u16::MAX`.
    pub(crate) detail_max_scroll: std::cell::Cell<u16>,
    /// Visible detail-timeline rows from the last render (the page step).
    pub(crate) detail_viewport: std::cell::Cell<u16>,
    /// Newest incident id already signalled, so a refresh only toasts genuinely
    /// new incidents (not the initial load).
    pub(crate) seen_latest: Option<String>,
}

impl Default for StatusState {
    fn default() -> Self {
        Self {
            incidents: Vec::new(),
            fetched_at_ms: None,
            cached: false,
            error: None,
            fetching: false,
            cursor: 0,
            focus: StatusFocus::List,
            selector: ListView::default(),
            detail_scroll: 0,
            detail_max_scroll: std::cell::Cell::new(0),
            detail_viewport: std::cell::Cell::new(0),
            seen_latest: None,
        }
    }
}

impl StatusState {
    /// The incident currently under the cursor, if any.
    pub(crate) fn selected(&self) -> Option<&Incident> {
        self.incidents.get(self.cursor)
    }

    /// Worst impact among active incidents: critical > major > minor > maintenance.
    /// Returns [`crate::status::Impact::None`] when nothing is active.
    pub(crate) fn worst_active_impact(&self) -> crate::status::Impact {
        self.incidents
            .iter()
            .filter(|i| i.is_active())
            .map(|i| &i.impact)
            .max_by_key(|i| i.severity())
            .cloned()
            .unwrap_or(crate::status::Impact::None)
    }
}

/// An incident is active when its lifecycle status is not terminal
/// (`resolved` / `completed`). Thin wrapper over [`Incident::is_active`] for the
/// render layer.
pub(crate) fn incident_is_active(incident: &Incident) -> bool {
    incident.is_active()
}

/// Health bucket for a row's status dot — the same success / warning / danger
/// buckets as the header `● status.claude.ai` dot, plus a neutral `Idle` for a
/// profile that is neither linked nor running a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Health {
    Ok,
    Warn,
    Danger,
    Idle,
}

/// A computed integration-check row (global, profile-independent).
#[derive(Debug, Clone)]
pub(crate) struct Check {
    pub(crate) label: &'static str,
    pub(crate) health: Health,
    /// Full readout for the detail pane, one entry per line. (Checks are
    /// dot-only in the list — the dot color carries the verdict, the readout
    /// lives here — so there is no separate terse value.)
    pub(crate) detail: Vec<String>,
    pub(crate) fix: Option<PluginFix>,
}

pub(crate) fn fix_verb(fix: &PluginFix) -> &'static str {
    match fix {
        PluginFix::WireMcpServers => "wire mcp server",
        PluginFix::InstallPlugin => "install plugin",
        PluginFix::Completions { stale: true, .. } => "update completions",
        PluginFix::Completions { stale: false, .. } => "install completions",
        PluginFix::HealHerdrConfig(_) => "heal herdr config",
        PluginFix::RelinkCredentials(_) => "repair credentials",
        PluginFix::RepairDivergence(_) => "snapshot credentials",
    }
}

/// The `f  <verb>` detail line for a fix.
pub(crate) fn fix_line(fix: &PluginFix) -> String {
    format!("f  {}", fix_verb(fix))
}

/// One editable row in the herdr detail's options section, in tab order. The
/// values live in `AppState.herdr` (profiles.toml) and persist immediately on
/// every change — there is no separate save step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrOption {
    PopupWidth,
    PaneTag,
    TagRefresh,
    BorderLabel,
    DelegateDot,
    DelegateRowText,
}

/// The options section's row order — also the ↑↓ walk order.
pub(crate) const HERDR_OPTIONS: [HerdrOption; 6] = [
    HerdrOption::PopupWidth,
    HerdrOption::PaneTag,
    HerdrOption::TagRefresh,
    HerdrOption::BorderLabel,
    HerdrOption::DelegateDot,
    HerdrOption::DelegateRowText,
];

impl HerdrOption {
    /// The form-row label, lowercase per the contract.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PopupWidth => "popup width",
            Self::PaneTag => "pane tag",
            Self::TagRefresh => "tag refresh",
            Self::BorderLabel => "border label",
            Self::DelegateDot => "delegate dot",
            Self::DelegateRowText => "delegate row text",
        }
    }
}

/// The key that armed the 2-step quit. One arm remembers its key, so `q`'s
/// completion and `ctrl+c`'s never cross: the other key re-arms as itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuitKey {
    /// The top-level `q` armed it.
    Q,
    /// `ctrl+c` armed it.
    CtrlC,
}

/// A transient message that replaces the hint bar in place until dismissed.
/// `x` clears it, as does every key that disarms the quit: all but `⌃c` and a
/// `q` that arms or confirms it.
/// Add variants as new conditions arise; keep one active at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FooterAlert {
    /// `! <message>` in `WARNING` color — user must act or acknowledge.
    Warn(String),
}

/// Severity for the full-width body banner. Ordering matters: `Danger`
/// outranks `Warning`, so when two conditions could both be live the
/// higher-severity banner wins (see `update_banner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BannerSeverity {
    Warning,
    Danger,
}

/// A sticky system-wide condition rendered as a full-width row at the top of
/// the body (below the header, above the screen content). Computed from app
/// state each frame; not stored as a notification and not user-dismissable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Banner {
    pub(crate) severity: BannerSeverity,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MainItemKind {
    Profile(usize),
    Codex(usize),
    Hermes(usize),
}

/// One structural entry in the shared band walk ([`App::band_entries`]):
/// items are the selectable rows. Headings and spacers are never selectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BandEntry {
    Spacer,
    /// A heading line's text (already control-escaped, with its two-cell
    /// indent); the caller computes the focus tier from its own selection.
    Heading(String),
    Item(MainItemKind),
}

/// What Setup's selector has selected: a harness item, or the trailing `+ new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupSelection {
    Item(MainItemKind),
    New,
}

/// The shared claude-row cursor: its flat profile index plus a monotonic move
/// count. Private fields force every writer through `Cursor::set`, which bumps
/// the count only when the index actually changes — a write of the same value
/// is no move. A hidden held selection records the count right after its own
/// placement and restores only while the count still equals it, so a move away
/// and back (which the old equality-on-the-shown-selection compare could not
/// see) closes the window for good.
mod profile_cursor {
    /// A claude-row cursor (see the module docs).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(crate) struct Cursor {
        index: usize,
        moves: u64,
    }

    impl Cursor {
        /// The current flat profile index.
        pub(crate) fn read(&self) -> usize {
            self.index
        }

        /// Land the cursor on `index`, bumping the move count only when the
        /// index changed.
        pub(crate) fn set(&mut self, index: usize) {
            if self.index != index {
                self.index = index;
                self.moves += 1;
            }
        }

        /// The number of index-changing moves since the cursor was created.
        pub(crate) fn count(&self) -> u64 {
            self.moves
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetupItem {
    Codex(ProfileName),
    Hermes(String),
}

/// One tab's identity-held selection slot: the stored codex/pool identity
/// ([`SetupItem`]), its flat index when stored (for the dropped-for-good
/// clamp), and a held-but-hidden identity (`remembered`) that restores only
/// while the shared claude cursor's move count still equals the one the hide
/// recorded (`remembered_moves`). Setup and Usage each own one slot
/// ([`App::setup`] / [`App::usage`]); every clear site calls
/// [`HeldSelection::clear`] so the two tabs' rules cannot fork.
#[derive(Default)]
struct HeldSelection {
    item: Option<SetupItem>,
    index: Option<usize>,
    remembered: Option<SetupItem>,
    /// The shared claude cursor's move count right after the hide's own
    /// placement. The restore gate: a held identity returns only while the
    /// count still equals this, so ANY writer that moves the shared cursor —
    /// a move away and back on another tab included — closes the window for
    /// good, present and future writers alike.
    remembered_moves: Option<u64>,
}

impl HeldSelection {
    /// Drop a held-but-hidden identity: a move or a descend adopted the shown
    /// row, so the read must not restore the old identity over it.
    fn clear(&mut self) {
        self.remembered = None;
        self.remembered_moves = None;
    }
}

/// Which tab's [`HeldSelection`] a shared place/rehome call targets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionSlotKind {
    Setup,
    Usage,
}

impl SelectionSlotKind {
    fn tab(self) -> Tab {
        match self {
            Self::Setup => Tab::Setup,
            Self::Usage => Tab::Usage,
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Setup => Self::Usage,
            Self::Usage => Self::Setup,
        }
    }
}

/// The Overview's visible harness filter; selection maps onto the rows it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HarnessFilter {
    #[default]
    All,
    Claude,
    Codex,
    Hermes,
}

impl HarnessFilter {
    /// `c` cycles: all → claude → codex → hermes → all.
    pub(crate) fn next(self) -> Self {
        match self {
            HarnessFilter::All => HarnessFilter::Claude,
            HarnessFilter::Claude => HarnessFilter::Codex,
            HarnessFilter::Codex => HarnessFilter::Hermes,
            HarnessFilter::Hermes => HarnessFilter::All,
        }
    }
    pub(crate) fn shows_claude(self) -> bool {
        matches!(self, HarnessFilter::All | HarnessFilter::Claude)
    }
    pub(crate) fn shows_codex(self) -> bool {
        matches!(self, HarnessFilter::All | HarnessFilter::Codex)
    }
    pub(crate) fn shows_hermes(self) -> bool {
        matches!(self, HarnessFilter::All | HarnessFilter::Hermes)
    }
    /// Harness name for the accounts panel's left meta slot; `None` while both
    /// harnesses show, so the unfiltered panel renders no left slot.
    pub(crate) fn label_name(self) -> Option<&'static str> {
        match self {
            HarnessFilter::All => None,
            HarnessFilter::Claude => Some("claude"),
            HarnessFilter::Codex => Some("codex"),
            HarnessFilter::Hermes => Some("hermes"),
        }
    }
}

/// One codex account as the Overview renders it — name, plan, the two windows
/// and whether its chain is quarantined. The windows come from the per-profile
/// usage cache the codex leg writes; the plan from that cache, else from the
/// store's id_token claim; `broken` from the quarantine record beside the
/// store. Deliberately NOT a `Profile`: synthesizing one would put a record
/// with no credentials into every claude path that walks `config.profiles`.
#[derive(Debug, Clone)]
pub(crate) struct CodexRow {
    pub(crate) name: ProfileName,
    pub(crate) active: bool,
    pub(crate) broken: bool,
    pub(crate) plan: Option<String>,
    pub(crate) five_hour: Option<crate::usage::UsageWindow>,
    pub(crate) seven_day: Option<crate::usage::UsageWindow>,
    /// Banked usage-limit resets off the same cache, floored at one
    /// available: `None` (or zero) renders no chip on the row.
    pub(crate) resets: Option<i64>,
    /// Epoch-ms stamp dating this row's windows: the codex usage cache's own
    /// `fetched_at` (epoch ms) when it is not in the future, else the cache
    /// file's mtime (a pre-`fetched_at` cache, or a stamp past the clock).
    /// `None` = no readable cache, so the pane reads `not read yet`.
    pub(crate) fetched_at: Option<u64>,
}

/// Read the codex roster into the [`App::codex_rows`] snapshot. Lock-free: the
/// roster is one small TOML and each reading is the profile's own cache file
/// plus the small files beside its store (the quarantine record, and the store
/// itself on a cache miss for the plan), the same set `status --json` reads
/// with no daemon running.
pub(crate) fn codex_rows() -> Vec<CodexRow> {
    let Ok(state) = crate::codex_profiles::CodexState::load() else {
        return Vec::new();
    };
    let active = state.active_profile().cloned();
    state
        .profiles()
        .iter()
        .map(|name| {
            let cached: Option<crate::usage::UsageInfo> = crate::profile_cache::load_profile_cache(
                name,
                crate::profile_cache::USAGE_CACHE_FILE,
            );
            CodexRow {
                name: name.clone(),
                active: active.as_ref().is_some_and(|a| a == name),
                broken: crate::codex_auth::read_quarantine(name.as_str()).is_some(),
                plan: crate::codex_auth::plan_label(
                    name.as_str(),
                    cached
                        .as_ref()
                        .and_then(|u| u.plan.as_ref())
                        .and_then(|p| p.codex_plan.as_deref()),
                ),
                five_hour: cached.as_ref().and_then(|u| u.five_hour.clone()),
                seven_day: cached.as_ref().and_then(|u| u.seven_day.clone()),
                resets: cached
                    .as_ref()
                    .and_then(|u| u.codex_reset_credits)
                    .filter(|count| *count > 0),
                fetched_at: crate::usage::usage_cache_clock_of(name, cached.as_ref()),
            }
        })
        .collect()
}

/// An in-flight login. The worker blocks up to the login bound in
/// `oauth_login` for the first door — the browser callback or a pasted code —
/// while the UI stays live and applies the result in `on_tick`. `generation`
/// discards a stale result from a login the user superseded (esc-cancel or a
/// fresh login start).
pub(crate) struct LoginSession {
    pub(crate) name: String,
    /// `None` → the mint lands in the `+ new` draft (capture-then-commit);
    /// `Some` → re-login the account this [`ProfileId`] was claimed from at
    /// start, in place, and only while `name` still holds it.
    pub(crate) account: Option<ProfileId>,
    pub(crate) generation: u64,
    /// The URL `r` re-opens: an OAuth login's browser link, known at start; the
    /// console login's page, once its worker announces it.
    pub(crate) url: Option<String>,
    /// Live milestone for the modal's stage line.
    pub(crate) stage: LoginStage,
    /// The door that delivered the code, as the worker reported it; `Browser`
    /// until then. The modal's stage copy follows it, so it is never set
    /// from the UI's own paste: the browser callback may have won first.
    pub(crate) method: LoginMethod,
    /// An OAuth login's paste door. `None` for the console login, which has no
    /// hosted link and no code to paste, so `c` and `p` are inert there.
    pub(crate) paste: Option<PasteDoor>,
    /// The login modal's inline code field, open (`Some`) from `p` until esc,
    /// a good submit, or the door closing. It lives here, not on the modal, so
    /// it dies with the session and never outlives the door it feeds. The
    /// typed bytes are a bearer-grade secret until exchanged: `LoginSession`
    /// derives no `Debug`, so they cannot ride a `{:?}`.
    pub(crate) paste_field: Option<InputState>,
}

/// In-flight codex browser re-login: the account name, the generation, the
/// authorize URL once the worker announces it (`r` re-opens it, `c` copies it),
/// and the cancel handle the worker's listener wait honours. No paste door and
/// no stage — codex's callback answers on this machine alone, so the worker's
/// next report is the minted chain or the failure.
pub(crate) struct CodexLoginSession {
    pub(crate) name: String,
    pub(crate) generation: u64,
    pub(crate) url: Option<String>,
    pub(crate) cancel: crate::codex_login::LoginCancel,
    /// `true` for a `+ new` form login (staged by `create account`), `false`
    /// for a re-login of an existing account (lands itself). Only the former
    /// counts as staged for the type-cycle confirm.
    pub(crate) is_new: bool,
}

/// The codex browser login the `re-login` worker runs: the announce callback
/// and the cancel handle, producing the minted outcome or its error sentence.
#[cfg(test)]
pub(crate) type CodexLoginWorker =
    fn(
        &dyn Fn(&str),
        &crate::codex_login::LoginCancel,
    ) -> std::result::Result<crate::codex_login::CodexLoginOutcome, String>;

/// The paste half of an OAuth login: the link `c` copies and a paste is
/// checked against, and the sender a parsed code goes down to the worker.
pub(crate) struct PasteDoor {
    pub(crate) links: crate::oauth_login::LoginLinks,
    pub(crate) tx: std::sync::mpsc::Sender<crate::oauth_login::ManualCode>,
}

impl LoginSession {
    /// The paste door while it can still win: the login is waiting on its
    /// first code. Once a door delivered one there is nothing to race, and the
    /// console login never had a door. Every `c`/`p` affordance gates on this.
    pub(crate) fn open_door(&self) -> Option<&PasteDoor> {
        match self.stage {
            LoginStage::WaitingBrowser => self.paste.as_ref(),
            LoginStage::ExchangingCode(_) | LoginStage::Verifying => None,
        }
    }
}

// Re-exported so the render code and the tests keep addressing it as this
// module's type: the CLI's login reports the same two doors off the same enum.
pub(crate) use crate::oauth_login::LoginMethod;

/// Where an in-flight login currently sits, mapped from
/// [`crate::oauth_login::LoginProgress`] worker events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginStage {
    /// Waiting for the first door: the browser round-trip, or a pasted code.
    WaitingBrowser,
    /// A code landed through this door; exchanging it for tokens.
    ExchangingCode(LoginMethod),
    /// Tokens minted; verifying them against the API.
    Verifying,
}

/// Worker→UI login channel payload: the console login's announced URL, a stage
/// bump (the first of which names the door that delivered the code), or the
pub(crate) enum LoginEvent {
    Url(String),
    Stage(LoginStage),
}

/// What a finished login worker produced. Both flows (the two-door OAuth
/// login, the Alibaba console) report through the same [`LoginEvent`] channel
/// and are drawn by the same modal, and they diverge only at apply time: an
/// Anthropic login replaces the profile's credentials, an Alibaba console login
/// replaces its usage session and touches nothing else.
///
/// The drain routes on this payload rather than on anything recorded in
/// [`LoginSession`], so a session and its result cannot disagree about which
/// flow ran.
pub(crate) enum LoginResult {
    Oauth(Box<crate::oauth_login::LoginOutcome>),
    Console(Box<crate::alibaba_login::ConsoleLoginOutcome>),
}

/// The scheduler walk's answer to "where does the chain land when the active
/// crosses its threshold", plus the ACTIVE's picked usage, resolved threshold
/// and in-memory burn rate — the inputs the Overview projected-switch caption
/// renders. See [`App::auto_switch_projection`].
pub(crate) struct AutoSwitchProjection {
    pub(crate) action: Option<SwitchAction>,
    pub(crate) active_usage: Option<UsageInfo>,
    pub(crate) active_threshold: f64,
    pub(crate) active_rate: Option<f64>,
}

impl AutoSwitchProjection {
    fn none() -> Self {
        Self {
            action: None,
            active_usage: None,
            active_threshold: crate::fallback::DEFAULT_THRESHOLD,
            active_rate: None,
        }
    }
}

pub(crate) struct App {
    /// Shared config; locked by the UI thread and the background refresher.
    /// Release before HTTP to avoid stalling the UI.
    pub(crate) config: ConfigHandle,

    pub(crate) usage_store: UsageStore,
    pub(crate) usage_status: StatusStore,
    pub(crate) usage_tokens: TokenList,
    pub(crate) next_refresh_per_profile: NextRefreshPerProfile,
    /// Per-profile activity state; read by the render loop for spinners, written
    /// by workers. Never held across HTTP.
    pub(crate) activity: ActivityStore,
    /// Drained in `on_tick`; each result clears the activity slot and surfaces errors.
    pub(crate) op_results: OpResultReceiver,
    /// Sender side; cloned into workers so they can report without holding a lock.
    pub(crate) op_sender: OpResultSender,
    /// Off-thread AUTH-1 switch-gate answers; drained in `on_tick` by
    /// `drain_switch_gates`, which completes or refuses the pending switch.
    pub(crate) switch_gates: std::sync::mpsc::Receiver<SwitchGateResult>,
    /// Sender side; cloned into each switch-gate worker.
    pub(crate) switch_gate_tx: std::sync::mpsc::Sender<SwitchGateResult>,
    /// Startup signals from reconcile/bootstrap workers; drained in `on_tick`.
    pub(crate) startup_results: StartupReceiver,
    /// Sender side for startup workers.
    pub(crate) startup_sender: StartupSender,
    pub(crate) last_fetched: LastFetchedAt,
    /// Per-profile consecutive-failure counters (429s + transient refresh
    /// failures) driving exponential usage-fetch backoff.
    pub(crate) poll_streaks: PollStreaks,
    /// Per-profile kick-429 blocks (the messages endpoint rejecting the 5h
    /// auto-start kick) — feeds the usage tab's blocked pill.
    pub(crate) kick_blocks: KickBlocks,
    /// Interleaved auto-start queue (`usage::auto_start_queue`): the anchor the 5h-window
    /// queue spaces against, plus per-profile election health. Held here for
    /// the same reason as `kick_blocks` — the Fallback card's detail render
    /// reads it every frame, and a per-frame history replay is not an option.
    /// Shared with the refresher this TUI spawns, so what the queue chip shows
    /// is what the gate is actually using.
    pub(crate) auto_start_queue: crate::usage::AutoStartQueueState,
    /// Scheduler-posted auto-switch decisions; drained in `on_tick`.
    pub(crate) pending_switch: PendingSwitch,
    /// Set by the scheduler when the whole chain is spent; `on_tick` drains it
    /// and turns off all accounts.
    pub(crate) pending_switch_off: PendingSwitchOff,
    pub(crate) refetch_queue: RefetchQueue,

    pub(crate) third_party_tokens: ThirdPartyList,
    pub(crate) third_party_usage_store: ThirdPartyUsageStore,
    pub(crate) third_party_status: ThirdPartyStatusStore,
    pub(crate) third_party_streaks: ThirdPartyStreaks,
    pub(crate) third_party_broken: ThirdPartyBroken,
    pub(crate) skipped_unreadable: SkippedUnreadable,
    /// Scheduler-only stores held here so `forget_memory` can reach the whole
    /// process's name-keyed state.
    pub(crate) weekly_reset_kicks: WeeklyResetKicks,
    pub(crate) suppressed_auth_expired: SuppressedAuthExpiredStore,
    /// The #74 degraded cue per profile, written each tick by `apply_usage`
    /// (it reads the DISK cache age) and read by the per-frame snapshot. A
    /// plain map, not a ranked store: written and read only on the UI thread.
    pub(crate) stale_cues: HashMap<String, bool>,
    /// Per-name stats of the durable key-rejection records the last time
    /// [`sync_broken_verdicts`] checked them. The stat is the read gate: a
    /// per-second stat per profile is cheap, a per-second read of every record
    /// is not.
    pub(crate) broken_verdict_mtimes: crate::usage::DeadLoginStats,
    /// [`sync_broken_verdicts`]'s once-per-second gate (`None` = run on the
    /// first tick).
    pub(crate) last_broken_verdict_sync: Option<Instant>,
    /// Every profile's credit refusal on record
    /// ([`crate::profile_cache::CreditRefusal`]), re-read by
    /// [`sync_credit_refusals`] so the per-frame projection never reads disk.
    pub(crate) credit_refusals: HashMap<ProfileName, crate::profile_cache::CreditRefusal>,
    /// The records' stats [`sync_credit_refusals`] last read, so a pass re-reads
    /// only the records that moved.
    pub(crate) credit_refusal_watch: crate::profile_cache::CreditRefusalWatch,
    /// [`sync_credit_refusals`]'s once-per-second gate (`None` = run on the
    /// first tick).
    pub(crate) last_credit_refusal_sync: Option<Instant>,
    pub(crate) tab: Tab,
    /// Running inside a herdr pane (`HERDR_ENV=1` at `cmd_tui`): the header
    /// carries a `[ herdr ]` tag and the TUI lands on the Plugin tab's herdr
    /// row. Read-only after construction — the mode is decided once at launch.
    pub(crate) herdr_mode: bool,
    pub(crate) modals: Vec<Modal>,
    /// Bumped on every modal open and close, so a scroll bound stamped with an
    /// older generation is never trusted as this frame's.
    pub(crate) modal_generation: std::cell::Cell<u64>,
    /// First help-modal row on screen (↑↓ scrolls). Reset when the modal opens.
    pub(crate) help_scroll: u16,
    /// Max valid `help_scroll` from the last help render (`total - viewport`).
    /// Interior mutability because the render pass takes `&App`; clamping there
    /// stops a held ↓ from inflating `help_scroll` past the end.
    pub(crate) help_max_scroll: std::cell::Cell<u16>,
    pub(crate) help_max_scroll_stamp: std::cell::Cell<ScrollStamp>,

    /// Visible help-modal rows from the last render — the key handler's page
    /// step (`PageUp`/`PageDown`), published like `help_max_scroll`.
    pub(crate) help_viewport: std::cell::Cell<u16>,

    /// Selected account index, shared across Overview/Usage/Setup tabs.
    /// On Setup may also rest on the trailing `+ new` row (== profile_count).
    pub(crate) profile_cursor: profile_cursor::Cursor,
    /// Setup's identity-held selection slot. A claude row selected in Setup IS
    /// `profile_cursor` (or its `+ new` synthetic at `profile_count()`); a
    /// cursor, and a pool hidden by an unread listing is held in
    /// `slot.remembered` until the read restores it (a move or a descend
    /// replaces it).
    setup: HeldSelection,
    /// Usage's identity-held selection slot — the same model as `setup` above,
    /// in its own slot so Usage's walk never moves the shared `profile_cursor`.
    usage: HeldSelection,
    pub(crate) overview_selector: ListView,
    pub(crate) overview_cursor: usize,
    /// The one remembered Overview item: a cursor-move key sets it to the item
    /// now under the cursor, and every placement (a filter change, tab entry,
    /// a clamp) returns the cursor to it where the item set still holds it. A
    /// claude row is remembered as `profile_cursor` itself (the stored index
    /// stays put; a filter merely hides it, so the round trip restores it.
    pub(crate) overview_remembered: Option<MainItemKind>,
    pub(crate) usage_selector: ListView,
    pub(crate) usage_detail_focus: StatusFocus,
    pub(crate) usage_detail_scroll: usize,
    pub(crate) usage_detail_max_scroll: std::cell::Cell<usize>,
    pub(crate) usage_detail_viewport: std::cell::Cell<usize>,
    pub(crate) setup_selector: ListView,
    pub(crate) chain_selector: ListView,
    pub(crate) models_selector: ListView,
    pub(crate) setup_detail_offset: std::cell::Cell<usize>,
    pub(crate) setup_detail_viewport: std::cell::Cell<usize>,
    pub(crate) setup_detail_row_starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) setup_detail_block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) setup_detail_tips: std::cell::RefCell<Vec<usize>>,
    pub(crate) setup_detail_total: std::cell::Cell<usize>,
    pub(crate) fallback_detail_offset: std::cell::Cell<usize>,
    pub(crate) fallback_detail_viewport: std::cell::Cell<usize>,
    pub(crate) fallback_detail_row_starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) fallback_detail_block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) fallback_detail_tips: std::cell::RefCell<Vec<usize>>,
    pub(crate) fallback_detail_total: std::cell::Cell<usize>,
    pub(crate) global_config_offset: std::cell::Cell<usize>,
    pub(crate) global_config_viewport: std::cell::Cell<usize>,
    pub(crate) global_config_row_starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) global_config_block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) global_config_tips: std::cell::RefCell<Vec<usize>>,
    pub(crate) global_config_total: std::cell::Cell<usize>,
    /// Which harness the Overview lists (`c` cycles). A view filter only — see
    /// [`HarnessFilter`].
    pub(crate) harness_filter: HarnessFilter,
    /// Which Setup pane has focus.
    pub(crate) config_focus: ConfigFocus,
    /// Cursor into the detail rows on the Setup tab's right pane.
    pub(crate) config_action_cursor: usize,
    /// The descended codex detail's armed 2-step (`delete account`). A separate
    /// field from the claude draft's `armed_action` because the codex detail
    /// has no `ConfigDraft`. The second ⏎ acts on the identity the SELECTION
    /// still holds: any path that moved it while armed disarms first, so the
    /// selection IS the armed identity (no separate field, the F2 call).
    pub(crate) codex_detail_armed: bool,

    /// Inline editor for the Config detail pane; `Some` only while Actions has focus.
    pub(crate) config_draft: Option<ConfigDraft>,
    /// The `+ new` form's draft while the form is not the active Setup pane —
    /// suspended across a switch or while another account's detail is open, so
    /// descending into `+ new` again restores its typed values. One slot cannot
    /// hold both this and `config_draft` (an existing account's draft), which
    /// is why the form gets its own home.
    pub(crate) new_account_draft: Option<ConfigDraft>,
    /// What the preset picker's open did to the Setup pane, so a cancel of the
    /// pick sequence can reverse it losslessly.
    pub(crate) preset_picker_open: PresetPickerOpen,
    /// Set by [`App::reload_if_state_changed`] before the swap: Setup's
    /// selection was `+ new` before the reload, so the tick's clamp keeps it
    /// on the trailing row (a reload that moves the roster must never demote
    /// `+ new` onto the last account). Read once by the tick, never by the
    /// tab-switch or delete clamps.
    pub(crate) reload_was_on_new: bool,
    /// Cursor into `chain_items()` on the Fallback left pane.
    pub(crate) chain_cursor: usize,
    /// Which Fallback pane has focus.
    pub(crate) fallback_focus: FallbackFocus,
    /// Cursor into the Fallback right pane (member rows or add-candidate list).
    pub(crate) fallback_detail_cursor: usize,
    /// The member card's one open edit, pinned to the member it opened on, or
    /// `None` at rest.
    pub(crate) fallback_edit: Option<MemberEdit<CardEdit>>,
    /// The custom day list `space` last stepped past on the open card, kept as
    /// one more stop in the preset cycle until the card closes. UI state only:
    /// never saved, dropped with the card.
    pub(crate) fallback_day_stop: Option<MemberEdit<Vec<Weekday>>>,
    /// Cursor into [`GLOBAL_CONFIG_ROWS`] on the program-wide Config tab.
    pub(crate) global_config_cursor: usize,
    /// `Some` while the refresh-interval custom-value field is open (⏎ opens,
    /// owns keyboard). Space/`+`/`-` still cycle the presets when `None`.
    pub(crate) refresh_interval_draft: Option<SeededInput>,
    /// In-flight custom value for the Config tab's context-nudge editor
    /// (`None` = not editing). Same lifecycle as `refresh_interval_draft`.
    pub(crate) context_nudge_draft: Option<SeededInput>,
    /// In-flight custom value for the Config tab's weekly-threshold editor
    /// (`None` = not editing). Same lifecycle as `refresh_interval_draft`.
    pub(crate) weekly_threshold_draft: Option<SeededInput>,
    /// The selected account's note (the usage tab's `notes:` row). Loaded on
    /// cursor moves, tab switches with a clamped cursor, and saves — never per
    /// frame.
    pub(crate) note_text: Option<String>,
    /// The open note editor, while `n` has one open on the usage tab.
    pub(crate) note_editor: Option<NoteEditor>,

    pub(crate) toasts: VecDeque<Toast>,
    /// Whether the terminal is currently too short for the normal layout (< 14 rows).
    /// Tracked across frames so the "too small" toast fires only on the transition in.
    pub(crate) compact: bool,
    /// Whether the terminal is below [`Self::WIDTH_FLOOR`] — the narrowest
    /// width at which the Overview accounts row still reads whole (the binding
    /// surface; the Fallback picker and blurb fit narrower).
    pub(crate) below_width_floor: bool,

    /// Update check thread handle; awaited on TUI exit for clean shutdown.
    pub(crate) update_handle: Option<JoinHandle<()>>,

    /// In-flight login worker of any method (Setup tab); `None` when idle.
    pub(crate) login: Option<LoginSession>,

    /// What the login modal's `c` hands the hosted link to: OSC 52 on the
    /// terminal's stdout. Replaceable so a test keeps the escape off the
    /// terminal running the suite, which would take the fixture's link onto
    /// its clipboard.
    pub(crate) clipboard: fn(&str) -> std::io::Result<()>,
    /// Monotonic login id; bumped on each start so a superseded worker's result
    /// is discarded when it lands.
    pub(crate) login_generation: u64,
    pub(crate) login_event_rx: std::sync::mpsc::Receiver<(u64, LoginEvent)>,
    pub(crate) login_event_tx: std::sync::mpsc::Sender<(u64, LoginEvent)>,
    pub(crate) login_result_rx:
        std::sync::mpsc::Receiver<(u64, std::result::Result<LoginResult, String>)>,
    pub(crate) login_result_tx:
        std::sync::mpsc::Sender<(u64, std::result::Result<LoginResult, String>)>,

    /// In-flight codex browser re-login (the Setup codex detail's `re-login`
    /// row); `None` while idle. Its own session and channels: codex's callback
    /// answers on this machine alone, so the flow is one worker with no paste
    /// door. The worker LANDS the minted chain itself (behind the rotation
    /// flock), so the result channel carries the canonical name or the error
    /// sentence — the UI drains only the outcome, never parks on the flock.
    pub(crate) codex_login: Option<CodexLoginSession>,
    pub(crate) codex_login_generation: u64,
    pub(crate) codex_login_url_rx: std::sync::mpsc::Receiver<(u64, String)>,
    pub(crate) codex_login_url_tx: std::sync::mpsc::Sender<(u64, String)>,
    pub(crate) codex_login_result_rx:
        std::sync::mpsc::Receiver<(u64, String, std::result::Result<String, String>)>,
    pub(crate) codex_login_result_tx:
        std::sync::mpsc::Sender<(u64, String, std::result::Result<String, String>)>,
    /// The `+ new` form's codex `+ login` result: the minted chain (staged,
    /// never landed) or its error sentence, keyed by generation so a cancelled
    /// login's late result is dropped.
    pub(crate) codex_new_login_result_rx: std::sync::mpsc::Receiver<(
        u64,
        std::result::Result<crate::codex_login::CodexLoginOutcome, String>,
    )>,
    pub(crate) codex_new_login_result_tx: std::sync::mpsc::Sender<(
        u64,
        std::result::Result<crate::codex_login::CodexLoginOutcome, String>,
    )>,
    /// The codex browser login the `re-login` worker runs. A plain fn field so
    /// a test swaps in a hermetic stand-in that never binds codex's loopback
    /// ports or opens a browser (the `SchedulerState.fetcher` field precedent).
    /// The non-test worker always runs [`codex_login::login_with`].
    #[cfg(test)]
    pub(crate) codex_login_worker: CodexLoginWorker,
    /// Claude status feed state; UI-thread-only (no shared lock).
    pub(crate) status: StatusState,
    /// Status feed events from the background thread; drained in `on_tick`.
    pub(crate) status_events: std::sync::mpsc::Receiver<StatusEvent>,
    /// Manual-refresh signal to the status thread; a `()` triggers a refetch.
    pub(crate) status_refresh: std::sync::mpsc::Sender<()>,

    /// `[ daemon ]` header-chip state: daemon presence + `status.json` health,
    /// re-probed on a throttled cadence in `on_tick`. UI-thread-only.
    pub(crate) daemon_health: crate::daemon::DaemonHealth,
    /// Throttle for the `daemon_health` flock probe; construct probes once
    /// itself, so this starts stamped over a real answer. Seeding a placeholder
    /// instead would render `Absent` — "no daemon runs" — as fact for the whole
    /// first interval, and the first paint happens before any `on_tick`.
    pub(crate) last_daemon_probe: Instant,

    /// How a fresh daemon's binary changed, as last read beside
    /// `daemon_health` ([`crate::daemon::daemon_binary_change`]): the plugin
    /// card's line for that kind holds exactly while it is set, and each change
    /// into `Some` tints the Plugin tab. Starts `None`, so a first changed
    /// reading counts.
    pub(crate) daemon_binary_change: Option<ExeChange>,
    /// The kinds of daemon binary change whose toast was raised; each kind
    /// toasts once per run.
    pub(crate) daemon_toasted_kinds: ToastedKinds,
    /// The kinds of change to this TUI's own binary whose toast was raised;
    /// each kind toasts once per run, never on a probe made while this
    pub(crate) own_toasted_kinds: ToastedKinds,
    /// A `start daemon` / `stop daemon` worker is running; the action menu
    /// offers neither verb until its outcome lands. UI-thread-only.
    pub(crate) daemon_control_busy: bool,
    /// Those workers' outcomes; drained in `on_tick`.
    pub(crate) daemon_control_rx: std::sync::mpsc::Receiver<DaemonControlResult>,
    /// Sender side; cloned into each worker.
    pub(crate) daemon_control_tx: std::sync::mpsc::Sender<DaemonControlResult>,
    /// Single-fetcher lease (#27), shared with the scheduler tick. The bootstrap
    /// switch one-shot runs only if THIS instance holds it.
    pub(crate) fetch_lease: Arc<crate::daemon::FetchLease>,

    /// Global token-usage stats read from `~/.claude` (stats-cache + recent
    /// transcript top-up); `None` until the loader posts its first result.
    pub(crate) token_stats: Option<crate::tokens::TokenStats>,
    /// Set when the loader reported a failure and no stats are cached — the tab
    /// shows an error instead of a perpetual "parsing stats-cache.json".
    pub(crate) tokens_failed: bool,
    /// True while the JSONL transcript top-up is known to be in flight: set when
    /// a `Base` first seeds the tab or on a manual reload, cleared on the next
    /// `Loaded`/`Failed`. Silent periodic refreshes never light it (their `Base`
    /// hits an already-populated tab). Drives the loading spinners on the tab.
    pub(crate) tokens_topping_up: bool,
    /// Latest `(done, total)` transcript-sweep count from the loader; rendered
    /// only while `tokens_topping_up` (silent periodic sweeps also emit it),
    /// cleared on `Loaded`/`Failed` and on a manual reload.
    pub(crate) tokens_progress: Option<(usize, usize)>,
    /// Which view the Tokens tab is showing (Dashboard landing vs Models detail).
    pub(crate) token_view: TokenView,
    /// Cursor into the grouped model list on the Tokens `Models` view.
    pub(crate) token_model_cursor: usize,
    pub(crate) model_detail_focus: StatusFocus,
    pub(crate) model_detail_scroll: u16,
    pub(crate) model_detail_max_scroll: std::cell::Cell<u16>,
    pub(crate) model_detail_viewport: std::cell::Cell<u16>,
    /// Model filter over the Tokens tab's model surfaces (action-menu driven).
    pub(crate) token_filter: TokenFilter,
    pub(crate) token_period: TokenPeriod,
    /// Focused card on the Tokens dashboard. Held across period/filter changes
    /// and across a resize (it names the card, not a layout slot).
    pub(crate) token_dash_focus: CardKind,
    /// First fully-visible card (index into the published reading order),
    /// reconciled each frame by [`follow_dash_focus`] and held in a `Cell` so
    /// the render pass can publish its correction (precedent: `token_dash_viewport`).
    pub(crate) token_dash_first: std::cell::Cell<usize>,
    /// Dashboard card reading order, published by the render pass so the key
    /// handler can walk `↑`/`↓` in the same order the frame drew.
    pub(crate) token_dash_order: std::cell::Cell<[CardKind; 7]>,
    /// Dashboard card `(top, bottom)` rows in reading order, published by the
    /// render pass for the key handler's whole-card scroll; empty until the
    /// first dashboard frame draws.
    pub(crate) token_dash_rows: std::cell::RefCell<Vec<(u16, u16)>>,
    /// Dashboard viewport height (body rows), published by the render pass.
    pub(crate) token_dash_viewport: std::cell::Cell<u16>,
    /// Token-stats load results from the background loader; drained in `on_tick`.
    pub(crate) tokens_events: std::sync::mpsc::Receiver<crate::tokens::TokensEvent>,
    /// Manual-refresh signal to the token loader; a `()` triggers a reload.
    pub(crate) tokens_refresh: std::sync::mpsc::Sender<()>,

    /// Model price table for the Tokens tab's API-equivalent cost lens; `None`
    /// until the pricing loader posts a result (and `—` is shown meanwhile).
    pub(crate) price_table: Option<crate::pricing::PriceTable>,
    /// Set when the pricing loader reported a failure and no table is cached —
    /// the cost lens reads `rates unavailable` instead of a perpetual
    /// `rates loading`.
    pub(crate) price_failed: bool,
    /// Pricing load results from the background loader; drained in `on_tick`.
    pub(crate) pricing_events: std::sync::mpsc::Receiver<crate::pricing::PricingEvent>,
    /// Manual-refresh signal to the pricing loader; a `()` triggers a refetch.
    pub(crate) pricing_refresh: std::sync::mpsc::Sender<()>,

    pub(crate) last_reload_fp: ReloadFingerprint,
    /// The identity snapshot the reload compare diffs against, refreshed on
    /// every applied forget.
    pub(crate) snapshot:
        Arc<RankedMutex<crate::usage::SnapshotMap, crate::lockorder::rank::Snapshot>>,
    /// The live credentials hold a login no profile owns (a login at all, and
    /// `find_matching_oauth_profile` misses) — the condition the `+ new`
    /// form's `+ capture current login` row renders on. Recomputed on
    /// construct, on entering the Setup tab, on config reload, and at every
    /// in-TUI mutation (switch, capture, create, delete, login, logout), whose
    /// own write is drained but left unadopted so the next reload re-reads it.
    pub(crate) unsaved_live_login: bool,
    /// Origin of the ambient-animation phase clock; read through [`App::anim_ms`].
    pub(crate) started_at: Instant,

    /// Pinned animation phase for render tests that must sample a specific point
    /// of the wave. Backdating `started_at` instead would subtract from
    /// `Instant::now()`, which panics on a host whose uptime is shorter than the
    /// offset. Never compiled into the binary.
    #[cfg(test)]
    pub(crate) anim_phase_ms: Option<u64>,
    /// Pinned wall clock for render tests that must advance a tps snapshot past
    /// its stale window without sleeping. `fresh_tps` reads this when set, so
    /// freshness is still judged per call. Never compiled into the binary.
    #[cfg(test)]
    pub(crate) tps_now_override: Option<u64>,
    /// The day-list notices last surfaced, empty while the lists are ordinary.
    /// Holding the MESSAGES rather than a flag is what makes the gate right on
    /// both axes `AppConfig::day_claim_collision` documents: each string
    /// carries the day and the accounts, so it goes stale at midnight and on a
    /// config edit, and is byte-equal on every tick between. Holding the SET
    /// rather than one string is what keeps a second notice from repainting
    /// the first.
    pub(crate) day_claim_notices: Vec<String>,
    /// Tick counter; advances the activity spinner frame each `on_tick`.
    pub(crate) tick_count: u64,
    pub(crate) quit: bool,
    /// write or a token spend): the screen keeps drawing and the tick exits the
    /// moment no write runs. A second quit during the wait exits at once.
    pub(crate) exit_wait: bool,
    /// The armed 2-step quit: the key that armed it, `None` when disarmed. A
    /// second press of the SAME key confirms; the other quit key re-arms as
    /// itself.
    pub(crate) armed_quit: Option<QuitKey>,
    /// Live footer alert; replaces the hint bar in place while `Some`.
    /// Cleared on disarm, tab switch, or explicit `x` dismiss.
    pub(crate) footer_alert: Option<FooterAlert>,
    /// Active banner; recomputed each tick from sticky system conditions.
    /// `None` when no condition holds (banner row absent from layout).
    pub(crate) banner: Option<Banner>,
    /// Last time the 1Hz divergence poll ran. `None` means un-sampled, which the
    /// gate reads as due — the state a fixture writes to force a poll, since
    /// backdating an `Instant` panics on a host booted more recently than the
    /// interval.
    pub(crate) last_divergence_check: Option<Instant>,
    /// The non-blocking divergence banner's backing signal: `Some` while the
    /// live login no longer matches the active profile, `None` the moment the
    /// link is clean again. Set/refreshed by the 1Hz poll and startup
    /// reconcile; <kbd>d</kbd> opens the resolver from it. In-memory only — a
    /// restart re-evaluates.
    pub(crate) divergence_pending: Option<DivergenceNotice>,

    /// Set once reconcile reports back; gates bootstrap spawn.
    pub(crate) reconcile_done: bool,
    /// Set once `spawn_bootstrap` is dispatched; prevents double-dispatch.
    pub(crate) bootstrap_started: bool,
    /// Set once `reconcile_startup` seeded the stores, so the gate's
    /// synchronous seed in [`maybe_spawn_bootstrap`] only runs on the
    /// acting-default arms that reconcile left to it.
    pub(crate) startup_seeded: bool,
    /// Tunable per-profile refresh interval; the scheduler reads this each tick.
    pub(crate) refresh_interval: Arc<AtomicU64>,
    /// Set before bootstrap spawn, cleared on every worker exit path.
    /// `ConfirmAction::RotateAll` checks this alongside `any_busy` to block a
    /// concurrent rotate-all from racing the bootstrap's relink + initial fetch.
    pub(crate) bootstrap_active: Arc<AtomicBool>,
    /// Signal to the scheduler thread that the UI is shutting down.
    pub(crate) shutting_down: Arc<AtomicBool>,
    /// Per-tab background-event indicator; `None` = no pending activity.
    /// Set when a background event fires on a tab that isn't currently active;
    /// cleared in `switch_tab` when the user visits that tab.
    pub(crate) tab_activity: [Option<ToastKind>; Tab::ALL.len()],

    /// The 5h bell's latch: which accounts rang for their current crossing.
    pub(crate) bell_fired: crate::bell::BellLatch,

    /// Cached parsed usage history per profile from usage_history.jsonl. The
    /// file itself belongs to the scheduler's fetch path (`apply_outcome`),
    /// which may be running in another process, so this side is read-only.
    pub(crate) history_cache: HashMap<String, Vec<(u64, UsageInfo)>>,
    /// Last-seen content fingerprint (byte length, tail hash) per profile
    /// history file, for cache invalidation.
    pub(crate) history_fp: HashMap<String, (u64, u64)>,

    /// Cached parsed wallet series per profile from wallet_history.jsonl —
    /// the balance readings the wallet-burn rate replays. Same discipline as
    /// [`Self::history_cache`]: the third-party fetch leg is the only writer,
    /// this side is read-only and re-reads on a fingerprint change.
    pub(crate) wallet_cache: HashMap<String, Vec<crate::usage::WalletSample>>,
    /// Last-seen content fingerprint per profile wallet-history file.
    wallet_fp: HashMap<String, (u64, u64)>,

    /// Cached long-lived-token status per profile, keyed by name (absent when a
    /// profile has no sidecar). Read by the Overview render for the `⊘` danger
    /// marker, so it needn't stat each `session-token.json` per frame. Seeded
    /// at construct and refreshed on config reload; the expiry stamp it
    /// carries is compared to live `now_ms` at render time, so the clock
    /// ticking past expiry never needs a re-read — only an add / delete /
    /// re-mint does, which `reload_fingerprint` now catches.
    pub(crate) session_tokens: HashMap<String, crate::claude::SessionTokenStatus>,
    /// Live sessions per account, for the Overview `live` column, the Fallback
    /// member card and the header's fleet count. Cached because collecting it
    /// is a readdir plus an `open` + `try_lock` per row per marker layout, and
    /// every one of those surfaces reads it every frame. Refreshed by
    /// [`poll_live_sessions`] rather than on a config reload: sessions come and
    /// go without touching config.
    pub(crate) live_sessions: crate::live_sessions::LiveTally,
    /// Throttle for the per-tick re-tally; construct seeds the tally itself, so
    /// this starts stamped. `None` means un-sampled, which the gate reads as due
    /// — the state a fixture writes to force a re-tally, since backdating an
    /// `Instant` panics on a host booted more recently than the interval.
    last_live_sessions_refresh: Option<Instant>,
    /// The job-store snapshot the Setup detail's live-session field reads from,
    /// so a frame never walks `~/.claude/jobs`. Cached like `live_sessions`:
    /// refreshed by [`poll_live_sessions`] on its own tick, never on a config
    /// reload (jobs come and go without touching config), and seeded in
    /// [`App::new`] so the first frame already reads a real snapshot.
    pub(crate) job_trees: crate::claude_jobs::JobTrees,
    /// The members a handed-off chain row keeps live, derived from the same
    /// per-tick `job_trees` snapshot (one registry read, never one per frame),
    /// so a Setup-detail frame pays a set lookup instead of a registry walk.
    pub(crate) handed_off: HashSet<String>,
    /// The last tps snapshot read off the producer's cache, `None` when the
    /// cache is absent. Seeded in [`App::new`] beside `live_sessions`, so the
    /// header's `[ — tok/s ]` and the other no-reading forms render from the
    /// first frame; re-read on the tick by [`poll_tps`].
    pub(crate) tps: Option<crate::tps::Snapshot>,
    /// Throttle for the per-tick tps cache re-read.
    last_tps_refresh: Instant,

    /// The codex roster as the Overview lists it and the header counts it.
    /// Cached like `live_sessions`: [`codex_rows`] is a roster read plus a few
    /// small files per account, the header draws on every tab every frame, and
    /// the roster moves on a human timescale (a CLI verb in another terminal).
    /// One snapshot for both surfaces is also what keeps the header's count
    /// equal to the rows the Overview draws.
    pub(crate) codex_rows: Vec<CodexRow>,
    /// Throttle for the per-tick codex re-read; same contract as
    /// `last_live_sessions_refresh`.
    last_codex_rows_refresh: Option<Instant>,

    /// Whether the operator's own codex login is capturable right now, computed
    /// `+ capture current login` row's visibility reads this cache, never the
    /// capture's disk reads, on the render path.
    pub(crate) codex_operator_capturable: bool,

    /// Throttle for the per-tick form-predicate re-read
    /// ([`Self::refresh_form_predicates`]); `None` means un-sampled, which the
    /// poll reads as due.
    last_form_predicates_refresh: Option<Instant>,

    /// The account a delete worker is removing; `None` while none runs. A
    /// second delete while one runs toasts that it is still running.
    pub(crate) deleting: Option<ProfileName>,
    /// Delete worker results; drained in `on_tick`.
    delete_rx: std::sync::mpsc::Receiver<DeleteResult>,
    delete_tx: std::sync::mpsc::Sender<DeleteResult>,
    /// Where the bare-item workers read the macOS Keychain item.
    pub(crate) bare_item_source: Option<BareItemSource>,
    /// Bare-item worker answers for held writes; drained in `on_tick`.
    bare_item_rx: std::sync::mpsc::Receiver<BareItemAnswer>,
    bare_item_tx: std::sync::mpsc::Sender<BareItemAnswer>,
    /// The settled placement a resumed write takes at its gate instead of
    /// settling again; set only across [`resume_bare_item_write`]'s call.
    bare_item_ready: Option<Option<[u8; 32]>>,
    /// Non-switch Keychain checks still running on their workers
    /// ([`begin_bare_item_check`]): the [`BARE_ITEM_CHECKING`] toast stays
    /// up past its TTL while any runs, and goes once none does.
    bare_item_checks: Arc<AtomicUsize>,
    /// The bootstrap worker's settle, when it let the boot relink write: the
    /// placement the startup one-shot's write holds. `None` keeps the
    /// one-shot from writing while a source exists.
    boot_bare_item: Arc<std::sync::Mutex<Option<Option<[u8; 32]>>>>,
    /// [`App::refresh_unsaved_live_login`]'s worker answers, tagged with the
    /// request they answer; drained in `on_tick`.
    unsaved_login_rx: std::sync::mpsc::Receiver<(u64, bool)>,
    unsaved_login_tx: std::sync::mpsc::Sender<(u64, bool)>,
    /// The latest refresh asked for; an older answer is dropped.
    unsaved_login_generation: u64,
    /// The `+ capture current login` row's worker answers
    /// ([`spawn_capture_login`]); drained in `on_tick`.
    capture_login_rx: std::sync::mpsc::Receiver<Result<CaptureSnapshot>>,
    capture_login_tx: std::sync::mpsc::Sender<Result<CaptureSnapshot>>,
    /// The login [`NamePromptAction::SaveBareItem`]'s prompt names a new
    /// profile for, the account its lookup proved, and the write it holds.
    pub(crate) bare_item_naming: Option<BareItemNaming>,

    /// Guest mode ([`crate::identity::upstream_active`]) at launch: upstream
    /// Upstream owns `~/.claude`, so the header carries a `[ guest ]` pill.
    /// Decided once by `tui::run` via [`App::with_guest_mode`].
    pub(crate) guest_mode: bool,
    /// Monitoring-only and upstream accounts (`monitor:` / `upstream:`
    /// observations from the collect hooks), listed below the profiles on the
    /// Usage tab's rail. Re-read from caches every
    /// [`USAGE_EXTRAS_POLL_MS`]; empty while no hook is registered.
    pub(crate) usage_extras: Vec<crate::usage::observation::AccountObservation>,
    /// `Some(i)` while the Usage rail's (or the Overview's) selection rests on
    /// `usage_extras[i]` rather than on `profile_cursor`'s profile. Shared by
    /// the two tabs that list the extras, so ⏎ on an Overview extra opens the
    /// Usage tab on it; every other tab keeps reading `profile_cursor`, which
    /// the extras never move.
    pub(crate) usage_extra_cursor: Option<usize>,
    /// When the extras were last read.
    usage_extras_polled: Option<std::time::Instant>,
    /// Monitor observation IDs (`"monitor:<id>"`) with an active manual refresh
    /// worker in flight; read by the Overview and Usage renders to draw a
    /// spinner while refreshing.
    pub(crate) monitor_refresh_inflight: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Set by a monitor refresh worker on completion so `poll_usage_extras`
    /// re-reads the updated cache immediately on the next tick.
    pub(crate) monitor_refresh_dirty: Arc<AtomicBool>,
    /// The live palette reloader (plan §4.5), set by `tui::run` only — tests
    /// and headless constructions never touch the operator's theme files.
    palette_watch: Option<super::theme::PaletteWatch>,
    /// The selected profile's explicit identity probe, if one is running.
    pub(crate) verify_login_inflight: Option<ProfileName>,
    pub(crate) verify_login_rx:
        std::sync::mpsc::Receiver<(ProfileName, crate::usage::VerifyOutcome)>,
    pub(crate) verify_login_tx: std::sync::mpsc::Sender<(ProfileName, crate::usage::VerifyOutcome)>,
    /// The `m` modal's session-move outcomes; drained in `on_tick`.
    pub(crate) session_move_rx: std::sync::mpsc::Receiver<SessionMoveResult>,
    /// Sender side; cloned into each move worker.
    pub(crate) session_move_tx: std::sync::mpsc::Sender<SessionMoveResult>,
    /// Plugin tab state; UI-thread-only, recomputed on focus + `r` (no thread).
    pub(crate) plugin: PluginState,
    /// Throttle for the Plugin tab's per-tick live refresh (session counts + link
    /// state); recompute fires at most once per `PLUGIN_REFRESH_INTERVAL`.
    pub(crate) last_plugin_refresh: Instant,
    /// The Hermes roster as the Overview lists it, re-read with the codex
    /// rows (same throttle).
    pub(crate) hermes_rows: Vec<HermesRow>,
    /// The Hermes row the cursor rests on while the `Hermes` filter shows
    /// only those rows (their only selection: Enter and `s` toast M-SWITCH).
    pub(crate) hermes_cursor: usize,
}

/// What [`NamePromptAction::SaveBareItem`]'s ⏎ saves, then resumes.
#[derive(Debug)]
pub(crate) struct BareItemNaming {
    pub(crate) login: crate::profile::ClaudeCredentials,
    pub(crate) account: Option<crate::profile::AccountId>,
    pub(crate) write: BareItemWrite,
}

/// Read every named profile's long-lived-token status for the Overview cache.
/// Reads each `session-token.json` off disk, so callers gather the names under a
/// brief config guard and call this with the guard already dropped — the read
/// never happens while the config lock is held.
fn collect_session_tokens(names: &[String]) -> HashMap<String, crate::claude::SessionTokenStatus> {
    names
        .iter()
        .filter_map(|n| {
            // ONE sidecar read per profile, the same derivation `build_snap`
            // uses: Misfilled ⇔ NotLongLived, everything readable else is
            // LongLived. (The cache held the `SidecarKind` too while the
            // Overview wore a type tag; upstream 5dde024 removed the tag, and
            // the kind went with it.)
            let (kind, oauth) = crate::claude::sidecar_summary(&ProfileName::from(n.clone()))?;
            let status = match kind {
                crate::claude::SidecarKind::Misfilled => {
                    crate::claude::SessionTokenStatus::NotLongLived
                }
                _ => crate::claude::SessionTokenStatus::LongLived(oauth.expires_at),
            };
            Some((n.clone(), status))
        })
        .collect()
}

/// Cloned `Arc`s bundled for [`spawn_refresher`] and the bootstrap worker;
/// carries no lock rank and is safe to construct while holding any lock.
struct WorkerHandles {
    config: ConfigHandle,
    usage_tokens: TokenList,
    usage_store: UsageStore,
    usage_status: StatusStore,
    refresh_interval: Arc<AtomicU64>,
    next_refresh_per_profile: NextRefreshPerProfile,
    activity: ActivityStore,
    last_fetched: LastFetchedAt,
    poll_streaks: PollStreaks,
    kick_blocks: KickBlocks,
    auto_start_queue: crate::usage::AutoStartQueueState,
    pending_switch: PendingSwitch,
    pending_switch_off: PendingSwitchOff,
    refetch_queue: RefetchQueue,
    third_party_tokens: ThirdPartyList,
    third_party_usage_store: ThirdPartyUsageStore,
    third_party_status: ThirdPartyStatusStore,
    third_party_streaks: ThirdPartyStreaks,
    third_party_broken: ThirdPartyBroken,
    skipped_unreadable: SkippedUnreadable,
    weekly_reset_kicks: WeeklyResetKicks,
    suppressed_auth_expired: SuppressedAuthExpiredStore,
    shutting_down: Arc<AtomicBool>,
    fetch_lease: Arc<crate::daemon::FetchLease>,
    bootstrap_active: Arc<AtomicBool>,
    startup_sender: StartupSender,
    snapshot: Arc<
        crate::lockorder::RankedMutex<crate::usage::SnapshotMap, crate::lockorder::rank::Snapshot>,
    >,
}

impl WorkerHandles {
    /// Clone every `Arc` the background workers share with the UI thread out of
    /// `app`.
    fn from_app(app: &App) -> Self {
        Self {
            config: Arc::clone(&app.config),
            usage_tokens: Arc::clone(&app.usage_tokens),
            usage_store: Arc::clone(&app.usage_store),
            usage_status: Arc::clone(&app.usage_status),
            refresh_interval: Arc::clone(&app.refresh_interval),
            next_refresh_per_profile: Arc::clone(&app.next_refresh_per_profile),
            activity: Arc::clone(&app.activity),
            last_fetched: Arc::clone(&app.last_fetched),
            poll_streaks: Arc::clone(&app.poll_streaks),
            kick_blocks: Arc::clone(&app.kick_blocks),
            auto_start_queue: Arc::clone(&app.auto_start_queue),
            pending_switch: Arc::clone(&app.pending_switch),
            pending_switch_off: Arc::clone(&app.pending_switch_off),
            refetch_queue: Arc::clone(&app.refetch_queue),
            third_party_tokens: Arc::clone(&app.third_party_tokens),
            third_party_usage_store: Arc::clone(&app.third_party_usage_store),
            third_party_status: Arc::clone(&app.third_party_status),
            third_party_streaks: Arc::clone(&app.third_party_streaks),
            third_party_broken: Arc::clone(&app.third_party_broken),
            skipped_unreadable: Arc::clone(&app.skipped_unreadable),
            weekly_reset_kicks: Arc::clone(&app.weekly_reset_kicks),
            suppressed_auth_expired: Arc::clone(&app.suppressed_auth_expired),
            shutting_down: Arc::clone(&app.shutting_down),
            fetch_lease: Arc::clone(&app.fetch_lease),
            bootstrap_active: Arc::clone(&app.bootstrap_active),
            startup_sender: app.startup_sender.clone(),
            snapshot: Arc::clone(&app.snapshot),
        }
    }
}

/// A cheap content fingerprint of a series log: its byte length folded with a
/// hash of its last 256 bytes. The re-read gates key on this instead of the
/// mtime, because NTFS quantizes file times on windows — an append can land
/// with a byte-identical `LastWriteTime` (measured 2026-09-13: 4/10 real-box
/// runs) — and the writer is whichever process holds the fetch lease, so the
/// mtime is the wrong signal. `None` when the file cannot be read, which
/// skips the re-read exactly like the old unreadable-mtime path did.
fn series_fingerprint(path: &std::path::Path) -> Option<(u64, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    use std::hash::Hasher;
    use std::io::{Read, Seek, SeekFrom};
    // Clamped to the file's own start: End(-256) on a file under 256 bytes
    // seeks before position 0 and errors (measured: EINVAL), which would read
    // as "unreadable" and silently skip the re-read.
    file.seek(SeekFrom::Start(len.saturating_sub(256))).ok()?;
    let mut tail = Vec::with_capacity(256);
    file.read_to_end(&mut tail).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&tail, &mut hasher);
    Some((len, hasher.finish()))
}

/// One frame's usage readings, cloned off the four stores (each locked once
/// and released) plus the per-tick stale map, so a render entry that holds
/// the config guard reads only this snapshot — never a store lock under
/// `Config` (the stores rank below it).
#[derive(Debug, Default)]
pub(crate) struct ReadingsSnapshot {
    /// Picked usage per profile name (the walk's `picked_usage` side).
    pub(crate) usage: HashMap<String, UsageInfo>,
    /// Picked fetch status per profile name (a third-party name's own
    /// status; every other name OAuth-first with the third-party fallback).
    pub(crate) fetch_status: HashMap<String, FetchStatus>,
    /// Raw provider stats per profile name (the balance rows' source).
    pub(crate) third_party: HashMap<String, crate::providers::ThirdPartyStats>,
    /// Raw OAuth store per profile name, unpicked: the queue chips' canceled
    /// source, the same one the scheduler's election reads, so a hybrid whose
    /// OAuth plan is canceled drops from both.
    pub(crate) oauth: HashMap<String, UsageInfo>,
    /// The #74 degraded cue per profile name, cloned off `App::stale_cues`.
    pub(crate) stale: HashMap<String, bool>,
}

impl ReadingsSnapshot {
    pub(crate) fn usage_of(&self, name: &str) -> Option<&UsageInfo> {
        self.usage.get(name)
    }
    pub(crate) fn status_of(&self, name: &str) -> Option<FetchStatus> {
        self.fetch_status.get(name).copied()
    }
    pub(crate) fn stats_of(&self, name: &str) -> Option<&crate::providers::ThirdPartyStats> {
        self.third_party.get(name)
    }
    pub(crate) fn stale_of(&self, name: &str) -> bool {
        self.stale.get(name).copied().unwrap_or(false)
    }
}

impl App {
    pub(crate) fn new(config: AppConfig) -> Self {
        // This process holds reading stores, so change sites record their
        // forgets here and the per-site drains + `on_tick` apply them.
        crate::usage::register_memory_stores();
        // Take the reload fingerprint BEFORE seeding the snapshot: a write
        // landing between the two then moves the fingerprint, so the first
        // reload re-reads config while the snapshot already holds the newer
        // identity — never a spurious forget off a stale snapshot.
        let last_reload_fp = reload_fingerprint();
        // Seed the identity snapshot from disk before the first compare, so a
        // freshly-built TUI forgets nothing its own startup already seeded.
        let snapshot = crate::usage::new_snapshot(
            &config
                .profiles
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
        );
        let usage_store: UsageStore = Arc::new(RankedMutex::new(HashMap::new()));
        let usage_status: StatusStore = Arc::new(RankedMutex::new(HashMap::new()));
        let usage_tokens: TokenList = Arc::new(RankedMutex::new(collect_tokens(&config)));
        let next_refresh_per_profile: NextRefreshPerProfile =
            Arc::new(RankedMutex::new(HashMap::new()));
        let activity: ActivityStore = Arc::new(RankedMutex::new(HashMap::new()));
        let (op_sender, op_results) = std::sync::mpsc::channel::<OpResult>();
        let (switch_gate_tx, switch_gates) = std::sync::mpsc::channel::<SwitchGateResult>();
        let (startup_sender, startup_results) = std::sync::mpsc::channel::<StartupSignal>();
        let last_fetched: LastFetchedAt = Arc::new(RankedMutex::new(HashMap::new()));
        let poll_streaks: PollStreaks = Arc::new(RankedMutex::new(HashMap::new()));
        let kick_blocks: KickBlocks = Arc::new(RankedMutex::new(HashMap::new()));
        let auto_start_queue = crate::usage::new_auto_start_queue_state();
        let pending_switch: PendingSwitch = Arc::new(RankedMutex::new(HashSet::new()));
        let pending_switch_off: PendingSwitchOff = Arc::new(RankedMutex::new(false));
        let refetch_queue: RefetchQueue = Arc::new(RankedMutex::new(HashSet::new()));
        let third_party_tokens: ThirdPartyList = Arc::new(RankedMutex::new(
            collect_third_party_entries(&config.profiles),
        ));
        let third_party_usage_store: ThirdPartyUsageStore =
            Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_status: ThirdPartyStatusStore = Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_streaks: ThirdPartyStreaks = Arc::new(RankedMutex::new(HashMap::new()));
        let third_party_broken: ThirdPartyBroken = Arc::new(RankedMutex::new(HashMap::new()));
        let skipped_unreadable: SkippedUnreadable = Arc::new(RankedMutex::new(HashSet::new()));
        let weekly_reset_kicks: WeeklyResetKicks = Arc::new(RankedMutex::new(HashSet::new()));
        let suppressed_auth_expired: SuppressedAuthExpiredStore =
            Arc::new(RankedMutex::new(HashMap::new()));
        let refresh_interval = Arc::new(AtomicU64::new(config.state.refresh_interval_ms));

        let mut history_cache: HashMap<String, Vec<(u64, UsageInfo)>> = HashMap::new();
        let mut history_fp: HashMap<String, (u64, u64)> = HashMap::new();
        for profile in &config.profiles {
            let name = &profile.name;
            let data = crate::profile::load_usage_history(name);
            if !data.is_empty() {
                if let Ok(path) = crate::profile::profile_history_path(name)
                    && let Some(fp) = series_fingerprint(&path)
                {
                    history_fp.insert(name.to_string(), fp);
                }
                history_cache.insert(name.to_string(), data);
            }
        }

        // The wallet series mirrors it: same read-only discipline, same
        // fingerprint invalidation, only the file and the type differ — and
        // only third-party profiles carry one, so the loop stats no OAuth
        // profile's absent file.
        let mut wallet_cache: HashMap<String, Vec<crate::usage::WalletSample>> = HashMap::new();
        let mut wallet_fp: HashMap<String, (u64, u64)> = HashMap::new();
        for profile in config
            .profiles
            .iter()
            .filter(|p| p.usage_cache_is_third_party())
        {
            let name = &profile.name;
            let data = crate::profile::load_wallet_history(name);
            if !data.is_empty() {
                if let Ok(path) = crate::profile::profile_wallet_history_path(name)
                    && let Some(fp) = series_fingerprint(&path)
                {
                    wallet_fp.insert(name.to_string(), fp);
                }
                wallet_cache.insert(name.to_string(), data);
            }
        }

        // Kick the best-effort update check; verdict lands in `update_results`, toasted from `on_tick`.

        let update_handle = update::spawn(config.state.update.auto_update);

        // Status feed worker: streams incidents over `status_events`; a `()` on
        // `status_refresh` triggers a manual refetch. The channels are always
        // created (so the drains stay inert), but the thread is skipped under
        // `cfg!(test)`: a detached worker could outlive a test's `HOME_OVERRIDE`
        // scope and its cache write would then resolve the real `~/.tollgate`.
        let (status_sender, status_events) = std::sync::mpsc::channel::<StatusEvent>();
        let (status_refresh, status_refresh_rx) = std::sync::mpsc::channel::<()>();
        if cfg!(test) {
            // Drop the worker's ends so they aren't flagged unused; the stored
            // `status_refresh` sender simply has no receiver in tests.
            drop((status_sender, status_refresh_rx));
        } else {
            status::spawn(status_sender, status_refresh_rx);
        }

        // Token-usage loader: reads `~/.claude/stats-cache.json` + recent
        // transcripts off the UI thread. Same test-skip rationale as the status
        // worker — a detached thread could outlive a test's `HOME_OVERRIDE`. The
        // `~/.claude` path is resolved once here so the worker never re-resolves
        // `home_dir()` (which would race the override).
        let (tokens_sender, tokens_events) =
            std::sync::mpsc::channel::<crate::tokens::TokensEvent>();
        let (tokens_refresh, tokens_refresh_rx) = std::sync::mpsc::channel::<()>();
        if cfg!(test) {
            drop((tokens_sender, tokens_refresh_rx));
        } else if let Ok(claude_dir) = crate::profile::claude_dir() {
            let tollgate_dir = crate::profile::tollgate_dir().ok();
            crate::tokens::spawn(tokens_sender, tokens_refresh_rx, claude_dir, tollgate_dir);
        } else {
            drop((tokens_sender, tokens_refresh_rx));
        }

        // Pricing loader: fetches the ai-pricelog index rates for the Tokens
        // tab's cost lens, disk-cached under `~/.tollgate`. Same test-skip
        // rationale as the status/token workers — a detached thread could
        // outlive a test's `HOME_OVERRIDE` and write the real `~/.tollgate`.
        let (pricing_sender, pricing_events) =
            std::sync::mpsc::channel::<crate::pricing::PricingEvent>();
        let (pricing_refresh, pricing_refresh_rx) = std::sync::mpsc::channel::<()>();
        if cfg!(test) {
            drop((pricing_sender, pricing_refresh_rx));
        } else {
            crate::pricing::spawn(pricing_sender, pricing_refresh_rx);
        }

        let (daemon_control_tx, daemon_control_rx) = std::sync::mpsc::channel();
        let (login_event_tx, login_event_rx) = std::sync::mpsc::channel();
        let (login_result_tx, login_result_rx) = std::sync::mpsc::channel();
        let (delete_tx, delete_rx) = std::sync::mpsc::channel();
        let (bare_item_tx, bare_item_rx) = std::sync::mpsc::channel();
        let (unsaved_login_tx, unsaved_login_rx) = std::sync::mpsc::channel();
        let (capture_login_tx, capture_login_rx) = std::sync::mpsc::channel();
        let (codex_login_url_tx, codex_login_url_rx) = std::sync::mpsc::channel();
        let (codex_login_result_tx, codex_login_result_rx) = std::sync::mpsc::channel();
        let (codex_new_login_result_tx, codex_new_login_result_rx) = std::sync::mpsc::channel();

        // Seed the token cache and the live tally before `config` moves into the
        // handle below.
        let live_sessions = crate::live_sessions::LiveTally::collect(&config);
        let tps = crate::tps::load_cache();
        let daemon_health = crate::daemon::daemon_health();
        // The Overview pool actions read this record without the Plugin tab
        // ever opening: load it at construction, the same cheap local read the
        // Services recompute makes.
        let session_tokens = collect_session_tokens(
            &config
                .profiles
                .iter()
                .map(|p| p.name.to_string())
                .collect::<Vec<_>>(),
        );
        let initial_note = config
            .profiles
            .first()
            .and_then(|p| crate::profile_notes::load_note(&p.name));
        // The handed-off set derives from the job snapshot, so both are read
        // once here and refreshed as a pair on the tick.
        let job_trees = crate::claude_jobs::job_trees();
        let handed_off = crate::runtime::handed_off_live_members(&job_trees);

        let (verify_login_tx, verify_login_rx) = std::sync::mpsc::channel();
        let (session_move_tx, session_move_rx) = std::sync::mpsc::channel();
        let mut app = Self {
            config: Arc::new(RankedMutex::new(config)),
            usage_store,
            usage_status,
            usage_tokens,
            next_refresh_per_profile,
            activity,
            op_results,
            op_sender,
            switch_gates,
            switch_gate_tx,
            startup_results,
            startup_sender,
            last_fetched,
            poll_streaks,
            kick_blocks,
            auto_start_queue,
            pending_switch,
            pending_switch_off,
            refetch_queue,
            third_party_tokens,
            third_party_usage_store,
            third_party_status,
            third_party_streaks,
            third_party_broken,
            skipped_unreadable,
            weekly_reset_kicks,
            suppressed_auth_expired,
            stale_cues: HashMap::new(),
            broken_verdict_mtimes: HashMap::new(),
            last_broken_verdict_sync: None,
            credit_refusals: HashMap::new(),
            credit_refusal_watch: crate::profile_cache::CreditRefusalWatch::default(),
            last_credit_refusal_sync: None,
            tab: Tab::Overview,
            harness_filter: HarnessFilter::default(),
            herdr_mode: false,
            modals: Vec::new(),
            modal_generation: std::cell::Cell::new(0),
            help_scroll: 0,
            help_max_scroll: std::cell::Cell::new(0),
            help_max_scroll_stamp: std::cell::Cell::new(ScrollStamp::default()),
            help_viewport: std::cell::Cell::new(0),
            profile_cursor: profile_cursor::Cursor::default(),
            setup: HeldSelection::default(),
            usage: HeldSelection::default(),
            overview_selector: ListView::default(),
            overview_cursor: 0,
            overview_remembered: None,
            usage_selector: ListView::default(),
            usage_detail_focus: StatusFocus::List,
            usage_detail_scroll: 0,
            usage_detail_max_scroll: std::cell::Cell::new(0),
            usage_detail_viewport: std::cell::Cell::new(0),
            setup_selector: ListView::default(),
            chain_selector: ListView::default(),
            models_selector: ListView::default(),
            setup_detail_offset: std::cell::Cell::new(0),
            setup_detail_viewport: std::cell::Cell::new(0),
            setup_detail_row_starts: std::cell::RefCell::new(Vec::new()),
            setup_detail_block_ends: std::cell::RefCell::new(Vec::new()),
            setup_detail_tips: std::cell::RefCell::new(Vec::new()),
            setup_detail_total: std::cell::Cell::new(0),
            fallback_detail_offset: std::cell::Cell::new(0),
            fallback_detail_viewport: std::cell::Cell::new(0),
            fallback_detail_row_starts: std::cell::RefCell::new(Vec::new()),
            fallback_detail_block_ends: std::cell::RefCell::new(Vec::new()),
            fallback_detail_tips: std::cell::RefCell::new(Vec::new()),
            fallback_detail_total: std::cell::Cell::new(0),
            global_config_offset: std::cell::Cell::new(0),
            global_config_viewport: std::cell::Cell::new(0),
            global_config_row_starts: std::cell::RefCell::new(Vec::new()),
            global_config_block_ends: std::cell::RefCell::new(Vec::new()),
            global_config_tips: std::cell::RefCell::new(Vec::new()),
            global_config_total: std::cell::Cell::new(0),
            config_focus: ConfigFocus::Profiles,
            config_action_cursor: 0,
            codex_detail_armed: false,
            fallback_focus: FallbackFocus::Chain,
            fallback_detail_cursor: 0,
            fallback_edit: None,
            fallback_day_stop: None,
            global_config_cursor: 0,
            refresh_interval_draft: None,
            context_nudge_draft: None,
            weekly_threshold_draft: None,
            note_text: initial_note,
            note_editor: None,
            config_draft: None,
            new_account_draft: None,
            preset_picker_open: PresetPickerOpen::None,
            reload_was_on_new: false,
            chain_cursor: 0,
            toasts: VecDeque::new(),
            compact: false,
            below_width_floor: false,
            update_handle,
            login: None,
            clipboard: crate::platform::copy_to_clipboard_osc52,
            login_generation: 0,
            login_event_rx,
            login_event_tx,
            login_result_rx,
            login_result_tx,
            codex_login: None,
            codex_login_generation: 0,
            codex_login_url_rx,
            codex_login_url_tx,
            codex_login_result_rx,
            codex_login_result_tx,
            codex_new_login_result_rx,
            codex_new_login_result_tx,
            #[cfg(test)]
            codex_login_worker: |_announce, _cancel| {
                Err("test seam: no real codex browser login".to_string())
            },
            status: StatusState::default(),
            status_events,
            status_refresh,
            daemon_health,
            daemon_binary_change: None,
            daemon_toasted_kinds: ToastedKinds::default(),
            own_toasted_kinds: ToastedKinds::default(),
            last_daemon_probe: Instant::now(),
            daemon_control_busy: false,
            daemon_control_rx,
            daemon_control_tx,
            fetch_lease: Arc::new(crate::daemon::FetchLease::new()),
            token_stats: None,
            tokens_failed: false,
            tokens_topping_up: false,
            tokens_progress: None,
            token_view: TokenView::Dashboard,
            token_model_cursor: 0,
            model_detail_focus: StatusFocus::List,
            model_detail_scroll: 0,
            model_detail_max_scroll: std::cell::Cell::new(0),
            model_detail_viewport: std::cell::Cell::new(0),
            token_filter: TokenFilter::default(),
            token_period: TokenPeriod::default(),
            token_dash_focus: CardKind::First,
            token_dash_first: std::cell::Cell::new(0),
            token_dash_order: std::cell::Cell::new([
                CardKind::First,
                CardKind::Total,
                CardKind::Trend,
                CardKind::Models,
                CardKind::Comp,
                CardKind::Hour,
                CardKind::Activity,
            ]),
            token_dash_rows: std::cell::RefCell::new(Vec::new()),
            token_dash_viewport: std::cell::Cell::new(0),
            tokens_events,
            tokens_refresh,
            price_table: None,
            price_failed: false,
            pricing_events,
            pricing_refresh,
            last_reload_fp,
            snapshot,
            unsaved_live_login: false,
            started_at: Instant::now(),
            #[cfg(test)]
            anim_phase_ms: None,
            #[cfg(test)]
            tps_now_override: None,
            day_claim_notices: Vec::new(),
            tick_count: 0,
            quit: false,
            exit_wait: false,
            armed_quit: None,
            footer_alert: None,
            banner: None,
            last_divergence_check: Some(Instant::now()),
            divergence_pending: None,
            reconcile_done: false,
            bootstrap_started: false,
            startup_seeded: false,
            refresh_interval,
            bootstrap_active: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            tab_activity: [None; Tab::ALL.len()],
            bell_fired: crate::bell::BellLatch::default(),
            history_cache,
            history_fp,
            wallet_cache,
            wallet_fp,
            session_tokens,
            live_sessions,
            last_live_sessions_refresh: Some(Instant::now()),
            job_trees,
            handed_off,
            tps,
            last_tps_refresh: Instant::now(),
            codex_rows: codex_rows(),
            last_codex_rows_refresh: Some(Instant::now()),
            codex_operator_capturable: false,
            last_form_predicates_refresh: None,
            deleting: None,
            delete_rx,
            delete_tx,
            bare_item_source: if crate::identity::upstream_active() {
                None
            } else {
                default_bare_item_source()
            },
            bare_item_rx,
            bare_item_tx,
            bare_item_ready: None,
            bare_item_checks: Arc::new(AtomicUsize::new(0)),
            boot_bare_item: Arc::new(std::sync::Mutex::new(None)),
            unsaved_login_rx,
            unsaved_login_tx,
            unsaved_login_generation: 0,
            capture_login_rx,
            capture_login_tx,
            bare_item_naming: None,
            monitor_refresh_inflight: Arc::new(std::sync::Mutex::new(HashSet::new())),
            usage_extra_cursor: None,
            hermes_cursor: 0,
            verify_login_inflight: None,
            verify_login_rx,
            hermes_rows: hermes_rows(),
            plugin: PluginState::default(),
            session_move_tx,
            verify_login_tx,
            palette_watch: None,
            guest_mode: false,
            monitor_refresh_dirty: Arc::new(AtomicBool::new(false)),
            session_move_rx,
            last_plugin_refresh: Instant::now(),
            usage_extras_polled: None,
            usage_extras: Vec::new(),
        };
        // A dead login survives a restart: the live mirror starts from the
        // durable verdicts, through the seed the daemon runs too.
        crate::usage::seed_dead_logins(&app.config, &app.third_party_broken);
        app.refresh_unsaved_live_login();
        app.refresh_form_predicates();
        let rebind_form = {
            let config = app.config();
            Self::startup_rebind_form(&config)
        };
        if let Some(form) = rebind_form {
            app.open_modal(Modal::Rebind(form));
        }
        app
    }

    /// The first-launch re-bind offer: open a confirm only when some account
    /// has rows to move AND this release's bind-table digest was not yet
    /// offered (an absent digest counts as not offered). Pure — the caller
    /// decides when to push the modal.
    fn startup_rebind_form(config: &AppConfig) -> Option<RebindForm> {
        let offers = crate::presets::rebind_offers(config);
        if offers.is_empty() {
            return None;
        }
        let digest = crate::presets::builtin_binds_digest();
        if config.state.preset_binds_offered.as_deref() == Some(digest.as_str()) {
            return None;
        }
        let blocks = crate::presets::group_offers(offers);
        let checked = blocks
            .iter()
            .map(|b| vec![true; b.accounts.len()])
            .collect();
        Some(RebindForm {
            blocks,
            checked,
            cursor: 0,
            choice: false,
            scroll: std::cell::Cell::new(0),
            starts: std::cell::RefCell::new(Vec::new()),
            block_ends: std::cell::RefCell::new(Vec::new()),
            total: std::cell::Cell::new(0),
            viewport: std::cell::Cell::new(0),
            digest,
        })
    }

    /// The live key-rejected member set, read once per render (never per walk
    /// candidate): the broken fingerprint map intersected with the CURRENT
    /// config profiles (the same `dead_login_fingerprint` the scheduler scans
    /// use), so a repair that landed in the config drops the name the
    /// moment the render reads it — never gated on the separately refreshed
    /// `ThirdPartyList`.
    pub(crate) fn key_rejected_names(&self) -> HashSet<ProfileName> {
        let broken = self
            .third_party_broken
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let cfg = self.config();
        crate::usage::current_key_rejected_names(&broken, &cfg.profiles)
            .into_iter()
            .collect()
    }

    /// Whether a switch to `name` would install a long-lived session token: the
    /// sidecar half of [`crate::fallback::no_usage_source`], answered off the
    /// per-frame [`Self::session_tokens`] cache. The ONE reader of that cache
    /// for this question, so the Fallback marker, the member card, the Overview
    /// chain row and the add confirm cannot read it four different ways.
    ///
    /// Same verdict [`crate::claude::has_session_token`] gives off disk, which
    /// is what a daemon (no cache, one read at boot) asks instead: a
    /// `NotLongLived` mis-fill is `false` here exactly as it is there, because
    /// the split stays disengaged and `credentials.json` remains what switches
    /// install. An expired long-lived token is still `true`: it stops serving,
    /// it does not make the account measurable.
    pub(crate) fn installs_session_token(&self, name: &ProfileName) -> bool {
        matches!(
            self.session_tokens.get(name.as_str()),
            Some(crate::claude::SessionTokenStatus::LongLived(_))
        )
    }

    /// Landing, applied at construction (before the first paint). The FIRST
    /// herdr launch opens the Plugin tab with the herdr selector row under the
    /// cursor and its detail pane descended, then marks the landing done in
    /// `[herdr] first_landing_done` — once, forever. Every other launch — a
    /// plain TUI, and herdr after the first — opens the top-level `home_tab`
    /// (default overview); the herdr header tag is unaffected. The first
    /// landing's probe runs here — `HERDR_ENV=1` proves herdr is present, and
    /// each of its three subprocesses is bounded at `herdr::PROBE_TIMEOUT`
    /// (2 s, worst case 6 s total) — so the landing row is real at first paint
    /// instead of waiting for `r`; later launches skip it like plain ones (the
    /// probe stays `r`-gated), and the cursor clamp inside the recompute
    /// below keeps the landing row valid when herdr does not resolve. The
    /// `claude --version` probe stays `r`-gated: construction must not block
    /// the first paint on a spawn. Nothing else changes — no key handling, no
    /// focus stealing after construction.
    pub(crate) fn with_herdr_mode(mut self, herdr_mode: bool) -> Self {
        self.herdr_mode = herdr_mode;
        let first_landing = herdr_mode
            && !self.config().state.herdr.first_landing_done
            && !self.modals.iter().any(|m| matches!(m, Modal::Rebind(_)));
        if first_landing {
            self.tab = Tab::Plugin;
            self.plugin.cursor = HERDR_SELECTOR_ROW;
            self.plugin.focus = PluginFocus::Detail;
            // Skipped under test (a spawned probe would read the real
            // registry); the landing test injects the probe instead.
            self.plugin.herdr = Some(if cfg!(test) {
                None
            } else {
                crate::herdr::probe()
            });
            recompute_plugin_checks(&mut self, false);
            {
                let mut cfg = self.config();
                cfg.state.herdr.first_landing_done = true;
                let _ = save_app_state(&cfg.state);
            }
            // Adopt the marker write like every other in-TUI save: without the
            // bump, the app's own profiles.toml rewrite reads as an external
            // change and re-runs a full config reload on the first tick.
            self.last_reload_fp = reload_fingerprint();
        } else {
            let home: Tab = self.config().state.home_tab().into();
            self.tab = home;
        }
        self
    }

    /// The production write path for the live-session tally after construction.
    pub(crate) fn set_live_sessions(&mut self, tally: crate::live_sessions::LiveTally) {
        self.live_sessions = tally;
    }

    /// The production write path for the tps snapshot read off the producer's
    /// cache; `None` = no producer.
    pub(crate) fn set_tps(&mut self, snapshot: Option<crate::tps::Snapshot>) {
        // A fresh snapshot that dropped a day the previous snapshot held means
        // the producer just folded that day into the speed ledger and dropped it
        // from the cache in the same tick. Nudge the tokens worker so it
        // re-reads the ledger at once, instead of leaving the merged day sums
        // blank until its next 90 s cycle. A day that stays (no fold, or a fold
        // whose ledger save failed keeps the day in the cache) fires nothing.
        if let (Some(prev), Some(next)) = (&self.tps, &snapshot) {
            let next_is_fresh = next
                .age_ms(self.tps_now_ms())
                .is_some_and(|age| age <= crate::tps::STALE_AFTER_MS);
            if next_is_fresh && prev.days.keys().any(|d| !next.days.contains_key(d)) {
                let _ = self.tokens_refresh.send(());
            }
        }
        self.tps = snapshot;
    }

    /// The tok/s chip's figure ceiling, in cells: the widest whole figure
    /// (`9999`) the header's whole-`k` form reaches, past which the figure
    /// abbreviates to `10k` and the chip never grows.
    pub(crate) const TPS_RATE_FIGURE_W: usize = 4;

    /// The header chip's number: the fresh rate, or `—` for no producer / a
    /// stale or unparseable cache.
    pub(crate) fn tps_rate_text(&self) -> String {
        match self.fresh_tps() {
            Some(s) => crate::tps::format_figure_capped(s.total.live.rate, Self::TPS_RATE_FIGURE_W),
            None => "—".to_owned(),
        }
    }

    /// The tps snapshot while a fresh producer stands behind it: `None` for no
    /// snapshot, one past [`crate::tps::STALE_AFTER_MS`], or an unparseable /
    /// future `generated_at`. A surface renders a figure only off this, never
    /// off [`App::tps`] alone.
    pub(crate) fn fresh_tps(&self) -> Option<&crate::tps::Snapshot> {
        let snap = self.tps.as_ref()?;
        let now = self.tps_now_ms();
        snap.age_ms(now)
            .is_some_and(|age| age <= crate::tps::STALE_AFTER_MS)
            .then_some(snap)
    }

    /// The wall clock the tps surfaces read: the test override when pinned,
    /// else the real clock. Shared so every surface's "now" agrees.
    pub(crate) fn tps_now_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some(ms) = self.tps_now_override {
            return ms;
        }
        crate::usage::now_ms()
    }

    /// Phase clock every ambient animation keys off: milliseconds since the TUI
    /// opened, wrapped by each effect's own period.
    pub(crate) fn anim_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some(ms) = self.anim_phase_ms {
            return ms;
        }
        self.started_at.elapsed().as_millis() as u64
    }

    /// Lock the shared AppConfig. Order: AppConfig outer, `with_state_lock` inner.
    pub(crate) fn config(&self) -> RankedGuard<'_, AppConfig> {
        Self::lock_config(&self.config)
    }

    /// The `config` lock for a worker holding only the cloned [`ConfigHandle`]
    /// — same lock and poison policy, without an `&App`.
    pub(crate) fn lock_config(config: &ConfigHandle) -> RankedGuard<'_, AppConfig> {
        #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        config.lock().expect("config mutex poisoned")
    }

    /// Spawn the bootstrap on a background thread (never blocks first paint).
    /// Re-links credentials, then marks every cache-due work-list name Queued so
    /// the first paint shows a spinner instead of a stale countdown. The store
    /// seed itself runs synchronously at the gate ([`maybe_spawn_bootstrap`]) on
    /// every arm — OAuth caches via `bootstrap_fetch`, api-key/provider caches
    /// via `bootstrap_third_party`, whatever the cache's age, each work-list name
    /// stamped at its cache clock so the refresh cadence resumes across the
    /// restart; a display-only name gets its reading alone, never a status or
    /// stamp. The scheduler then refreshes on the normal cadence; profiles with
    /// no fresh cache are left for it to fetch in the background. No proactive
    /// token rotation (401-recovery is lazy). Posts
    /// `StartupSignal::BootstrapDone` when done; the UI thread then rebuilds the
    /// token snapshot, starts the scheduler, applies usage, and runs the startup
    /// auto-switch.
    pub(crate) fn spawn_bootstrap(&mut self) {
        let h = WorkerHandles::from_app(self);
        let done = BootstrapDoneGuard {
            bootstrap_active: h.bootstrap_active,
            startup_sender: h.startup_sender,
        };
        let source = self.bare_item_source;
        let bare_item_tx = self.bare_item_tx.clone();
        let boot_bare_item = Arc::clone(&self.boot_bare_item);
        // Re-establish the credentials symlink (shutdown replaced it with a
        // plain file); without this, CC refreshes bypass the profile.
        let active = if crate::identity::upstream_active() {
            None
        } else {
            self.config().state.active_profile.as_ref().cloned()
        };
        let check = active
            .clone()
            .and_then(|active| begin_bare_item_check(self, &BareItemWrite::BootRelink(active)));

        spawn_worker(move || {
            let _done = done;

            if let Some(active) = active.filter(|_| !crate::identity::upstream_active()) {
                let write = BareItemWrite::BootRelink(active.clone());
                let writer = write.writer();
                let answer = BareItemAnswerGuard::new(bare_item_tx, write);
                let _check = check;
                // One settle answers for the relink here and the startup
                // one-shot's write after it. A login only the user can place
                // holds the relink until they answer; a failed settle leaves
                // the relink to its own guard, which keeps an unsaved login in
                // place.
                match settle_bare_keychain_item(source, writer) {
                    Ok(crate::claude::BareItemSettle::Settled(settled)) => {
                        if let Some(notice) = &settled.notice {
                            crate::logline::logline!("tollgate: {notice}");
                        }
                        if let Ok(mut boot) = boot_bare_item.lock() {
                            *boot = Some(settled.placed);
                        }
                        let _placed = enter_bare_item_placement(settled.placed);
                        let _ = link_profile_credentials(&active);
                        answer.disarm();
                    }
                    Ok(crate::claude::BareItemSettle::NeedsChoice(unplaced)) => {
                        answer.send(BareItemOutcome::Settled(Ok(
                            crate::claude::BareItemSettle::NeedsChoice(unplaced),
                        )));
                    }
                    Err(e) => {
                        crate::logline::logline!("tollgate: {e:#}");
                        let _ = link_profile_credentials(&active);
                        answer.disarm();
                    }
                }
            }

            // The store seed runs synchronously at the gate
            // (`maybe_spawn_bootstrap`) on every arm, so the worker keeps only
            // the relink and the `Queued` marking. `snapshot` /
            // `third_party_entries` are the enabled-only work-lists that drive
            // that marking — a display-only name is never queued for a fetch it
            // will never get.
            let (seeds, third_party_entries) = {
                let cfg = Self::lock_config(&h.config);
                (
                    crate::usage::collect_token_seeds(&cfg),
                    crate::usage::collect_third_party_entries(&cfg.profiles),
                )
            };
            // Anchor + stored-token reads run after the guard drops.
            let snapshot = crate::usage::build_token_entries(seeds);
            let interval_ms = h.refresh_interval.load(Ordering::Relaxed);

            // Seeded-but-due and stale/missing profiles are fetched by the
            // scheduler's first tick (started in `finish_bootstrap`); 5h windows
            // are armed by the windowless scan in `on_tick` after bootstrap
            // clears `bootstrap_active` — no startup-only fetch or kick pass.

            // Profiles already due on the first tick — never-fetched (no cache) or
            // a cache older than one interval — are marked Queued now so the
            // overview timer and usage header show a pending spinner from first
            // paint instead of a stale `0s` countdown over the past deadline (a
            // never-fetched profile's deadline is `0 + interval`, always in the
            // past). The first tick re-marks (idempotent); each worker flips itself
            // to Fetching when its request fires and clears on landing.
            let now = now_ms();
            let due_now: Vec<crate::usage::LegKey> = match h.last_fetched.lock() {
                Ok(lf) => snapshot
                    .iter()
                    .map(|e| FetchLeg::OAuth.key(e.name.clone()))
                    .chain(
                        third_party_entries
                            .iter()
                            .map(|e| FetchLeg::ThirdParty.key(e.name.clone())),
                    )
                    .filter(|key| {
                        lf.get(key).is_none_or(|stamp| {
                            stamp.as_millis().saturating_add(interval_ms) <= now
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            for key in &due_now {
                crate::usage::mark_fetch_activity(&h.activity, key, ProfileActivity::Queued);
            }
        });
    }

    /// Recency-weighted burn rate (%/h) for `name`'s 5h window, computed from
    /// the in-memory `history_cache` — never touches disk. Shared by the
    /// Overview ETA line (`render/overview.rs`) and the burn-aware auto-switch
    /// one-shot below so neither the render pass nor this UI-thread check ever
    /// reads `usage_history.jsonl` while holding the config guard;
    /// `fallback::burn_rate_for_profile` is the disk-reading twin used off the
    /// render/UI thread (the scheduler tick, after locks are dropped).
    pub(crate) fn active_burn_rate(
        &self,
        name: &ProfileName,
        usage_info: &UsageInfo,
    ) -> Option<f64> {
        let five_h = usage_info.five_hour.as_ref().map(|w| ("5h", w))?;
        crate::usage::compute_burn_rates_from_history(
            self.history_cache
                .get(name.as_str())
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            std::slice::from_ref(&five_h),
            crate::usage::BURN_LOOKBACK_MS,
            crate::usage::BURN_MIN_SAMPLES,
            crate::usage::BURN_GAP_CUT_MS,
        )
        .remove("5h")
        .flatten()
    }

    /// The startup one-shot's gate + burn rate, both off the stores (never a
    /// reading copy, never disk): `(active_fresh, rate)`. Freshness reads the
    /// picker's status store via `decision_fresh_any` — the same verdict the
    /// scheduler's scan gates on. The rate is the ACTIVE's picked 5h window from
    /// `usage_store` paired with the in-memory `history_cache`; `None` for a
    /// third-party-classified active, whose provider window never pairs with the
    /// OAuth burn history (maintainer ruling 2026-09-10).
    fn active_bootstrap_state(&self) -> (bool, Option<f64>) {
        let (name, third_party) = {
            let cfg = self.config();
            let Some(name) = cfg.state.active_profile.as_ref().cloned() else {
                return (false, None);
            };
            let third_party = cfg
                .find(&name)
                .is_some_and(|p| p.usage_cache_is_third_party());
            (name, third_party)
        };
        let fresh = crate::usage::decision_fresh_any(
            &self.usage_status,
            &self.third_party_status,
            third_party,
            &name,
        );
        let rate = if third_party {
            None
        } else {
            self.usage_store
                .lock()
                .ok()
                .and_then(|s| s.get(name.as_str()).cloned())
                .and_then(|u| self.active_burn_rate(&name, &u))
        };
        (fresh, rate)
    }

    /// The projected next switch for the Overview caption: the scheduler walk's
    /// answer over the ACTIVE's reading replaced by one that reads exhausted at
    /// the crossing (its threshold), so the caption names where the walk lands
    /// when the active crosses — never the walk's decision for the active as it
    /// reads now (a healthy active below its threshold has none). Also carries
    /// the ACTIVE's real picked usage, resolved threshold and in-memory burn
    /// rate, which the caption's eta guard reads. Built in one config-then-stores
    /// sequence (stores rank below `Config`), so the caller must not hold the
    /// config guard. `key_rejected` is the caller's live set, computed once per
    /// frame.
    pub(crate) fn auto_switch_projection(
        &self,
        key_rejected: &HashSet<ProfileName>,
    ) -> AutoSwitchProjection {
        let (mut snapshot, third_party_names, polled) = {
            let cfg = self.config();
            let Some(snapshot) = crate::fallback::snapshot_chain(&cfg, key_rejected) else {
                return AutoSwitchProjection::none();
            };
            if snapshot.chain.len() <= 1 {
                return AutoSwitchProjection::none();
            }
            let third_party_names: HashSet<ProfileName> = cfg
                .profiles
                .iter()
                .filter(|p| p.usage_cache_is_third_party())
                .map(|p| p.name.clone())
                .collect();
            let polled = crate::usage::polled_third_party_names(&cfg);
            (snapshot, third_party_names, polled)
        };
        let active = snapshot.active.clone();
        let usage = crate::usage::fill_chain_snapshot_stores(
            &mut snapshot,
            &self.usage_store,
            &self.third_party_usage_store,
            &self.usage_status,
            &self.third_party_status,
            &self.poll_streaks,
            &self.third_party_streaks,
            &self.kick_blocks,
            &third_party_names,
            &polled,
        );
        // The caption names where the walk lands, so it honours the live skip
        // set — but it runs every frame, so it READS the set as the last scan
        // left it (no `stored_login` read, no write): the scan already heals
        // each cadence, and the render path never reads disk per frame.
        snapshot.skipped_unreadable =
            crate::usage::read_skipped_unreadable(&self.skipped_unreadable)
                .into_iter()
                .collect();
        snapshot.broken.extend(key_rejected.iter().cloned());
        // The walk passes over a member on record as refusing a credit-billed
        // request in its headroom and home passes and takes it after its
        // serving-sink pass, off the copy `sync_credit_refusals` keeps.
        snapshot.credit_skip = snapshot
            .chain
            .iter()
            .map(|m| &m.name)
            .filter(|name| self.credit_refusals.contains_key(*name))
            .cloned()
            .collect();
        let active_usage = usage.get(active.as_str()).cloned();
        let active_threshold = snapshot
            .chain
            .iter()
            .find(|m| m.name == active)
            .map(|m| m.threshold)
            .unwrap_or(crate::fallback::DEFAULT_THRESHOLD);
        // A third-party-classified active has no burn history to pair with its
        // provider window (maintainer ruling 2026-09-10), so no rate.
        let active_rate = if third_party_names.contains(&active) {
            None
        } else {
            active_usage
                .as_ref()
                .and_then(|u| self.active_burn_rate(&active, u))
        };
        // The caption projects "when the active crosses at ~eta, where does the
        // walk land": re-run the SAME walk over a usage clone whose only change
        // is the active's 5h utilization set to its threshold. The threshold
        // reads exhausted in every mode — `util >= threshold` is the static
        // disjunct in both arms of `is_exhausted_projected` (its `Some(rate)`
        // arm ORs `projected_exhausted || util >= threshold`, its `None` arm is
        // exactly that) and the direct check when burn-aware is off — so the
        // floor, the rate and the mode never change the answer.
        let mut projected = usage.clone();
        if let Some(info) = projected.get_mut(active.as_str())
            && let Some(window) = info.five_hour.as_mut()
        {
            window.utilization = active_threshold;
        }
        let action = crate::fallback::next_auto_switch_target_with_usage(
            &snapshot,
            &projected,
            &third_party_names,
            crate::fallback::ActiveBurnRate::Supplied(active_rate),
            false,
            false,
        );
        AutoSwitchProjection {
            action,
            active_usage,
            active_threshold,
            active_rate,
        }
    }

    /// The funded wallet's burn rate for `profile`, computed from the
    /// in-memory `wallet_cache` — never touches disk. The wallet twin of
    /// [`Self::active_burn_rate`]: the Usage tab's balance row and the
    /// overview drains line read it here, so neither render pass reads
    /// `wallet_history.jsonl` while holding the config guard.
    pub(crate) fn wallet_rate_for(
        &self,
        name: &str,
        stats: Option<&crate::providers::ThirdPartyStats>,
    ) -> Option<crate::usage::WalletRate> {
        let stats = stats?;
        crate::usage::funded_wallet_rate(
            self.wallet_cache
                .get(name)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
            &stats.rows,
        )
    }

    /// The profile's peak-rate state off the live price table, sampled now:
    /// the recognized provider's own store rows — peak/off-peak is a property
    /// of the provider, never of the pinned models. `None` when no table is
    /// loaded or the profile has no store-backed provider (OAuth, generic
    /// endpoints, OpenRouter) — the Usage tab's `pricing` row and the
    /// overview's `▲` marker then do not render.
    pub(crate) fn peak_state_for(&self, profile: &Profile) -> Option<crate::pricing::PeakState> {
        self.price_table
            .as_ref()
            .and_then(|t| t.peak_state_for_profile(profile.provider, now_ms() as i64 / 1000))
    }

    /// UI-thread tail of bootstrap: rebuilds token snapshot, starts scheduler,
    /// applies usage, runs startup auto-switch. No HTTP.
    fn finish_bootstrap(&mut self) {
        // An exit wait starts no new write: a `BootstrapDone` that lands during
        // the wait must not relink credentials or run the startup auto-switch
        // after the user quit. The worker's `BootstrapDoneGuard` clears
        // `bootstrap_active` before it posts this signal, so nothing waits on it.
        if self.exit_wait {
            return;
        }
        self.refresh_tokens();
        self.start_scheduler();
        self.apply_usage();
        // Single-fetcher lease (#27): the startup switch one-shot is a switch
        // DECISION, so it may run only if THIS instance is the fetcher. Acquire
        // the shared lease now — the scheduler's first tick sleeps a cadence
        // before its own acquire, so the UI thread wins it here when it's free;
        // if another instance holds it, that instance owns every switch decision
        // and a one-shot here would race it (the #27 thrash).
        if !self.fetch_lease.acquire() {
            return;
        }
        // Run the startup one-shot on Fresh data only. A Cached seed's numbers
        // are unverified — stale in either direction — so switching on them
        // risks acting on a window the account no longer has. Stale profiles
        // are due on the scheduler's first tick, which fetches then
        // auto-switches off the corrected numbers.
        let switched = {
            let (active_fresh, rate) = self.active_bootstrap_state();
            // The one-shot may switch or switch off under its own locks, over
            // the bare item the bootstrap worker settled for its relink; it
            // holds that settle's placement and settles nothing itself. With
            // no settle to hold (one the user must answer, or one that
            // failed) the one-shot skips: the scheduler's next decision
            // reaches the switch gate, which settles again.
            let boot_placed = self.boot_bare_item.lock().ok().and_then(|mut b| b.take());
            let _placed = boot_placed.map(enter_bare_item_placement);
            let active_fresh =
                active_fresh && (self.bare_item_source.is_none() || boot_placed.is_some());
            if active_fresh {
                // In-memory rate only (`history_cache`) — never a disk read
                // under any guard; see `active_burn_rate`.
                auto_switch_if_needed(
                    &self.config,
                    rate,
                    &self.usage_store,
                    &self.third_party_usage_store,
                    &self.usage_status,
                    &self.third_party_status,
                    &self.poll_streaks,
                    &self.third_party_streaks,
                    &self.kick_blocks,
                )
                .ok()
                .flatten()
            } else {
                None
            }
        };
        match switched {
            Some(SwitchAction::To(target)) => {
                // Destination-based, as in the pending-switch drain: landing on
                // the home account reads as a return whether the return pass or
                // an exhaustion walk onto a clear preferred put us there.
                let key_rejected = self.key_rejected_names();
                let returned = self
                    .config()
                    .is_home_today(&ProfileName::from(target.clone()), &key_rejected);
                let msg = if returned {
                    format!("returned to preferred account '{target}'")
                } else {
                    format!("auto-switched to '{target}'")
                };
                self.toast(ToastKind::Warning, msg);
            }
            Some(SwitchAction::Off) => {
                self.refresh_tokens();
                self.toast(
                    ToastKind::Warning,
                    "all accounts spent\nswitched off to halt usage".to_string(),
                );
            }
            None => {}
        }
    }

    /// Bundle scheduler `Arc`s and launch the background refresher.
    fn start_scheduler(&self) {
        let h = WorkerHandles::from_app(self);
        spawn_refresher(
            h.config,
            h.usage_tokens,
            h.usage_store,
            h.usage_status,
            h.refresh_interval,
            h.next_refresh_per_profile,
            h.activity,
            h.last_fetched,
            h.poll_streaks,
            h.kick_blocks,
            h.auto_start_queue,
            h.weekly_reset_kicks,
            h.pending_switch,
            h.pending_switch_off,
            h.refetch_queue,
            h.third_party_tokens,
            h.third_party_usage_store,
            h.third_party_status,
            h.third_party_streaks,
            h.third_party_broken,
            h.skipped_unreadable,
            h.suppressed_auth_expired,
            h.shutting_down,
            // Single-fetcher lease (#27): the TUI competes for `usage-fetch.lock`
            // like any instance, standing its refresher down while another holds
            // it. Shared with `finish_bootstrap` so its startup switch one-shot
            // only runs when this TUI is the fetcher.
            h.fetch_lease,
            // The TUI's Usage tab and chain render read the third-party store,
            // so its stand-down hydration keeps the display superset.
            // Pinned by construction, not a test: no test drives this call site
            // (`spawn_refresher` builds its own state under `cfg!(test)`).
            true,
            h.snapshot,
        );
    }

    /// Bundle the reading stores `forget_memory` clears, cloning every `Arc` so
    /// the UI thread reaches the same maps the refresh thread shares.
    fn memory_stores(&self) -> MemoryStores {
        MemoryStores {
            usage: Arc::clone(&self.usage_store),
            usage_status: Arc::clone(&self.usage_status),
            last_fetched: Arc::clone(&self.last_fetched),
            next_refresh: Arc::clone(&self.next_refresh_per_profile),
            poll_streaks: Arc::clone(&self.poll_streaks),
            kick_blocks: Arc::clone(&self.kick_blocks),
            weekly_reset_kicks: Arc::clone(&self.weekly_reset_kicks),
            auto_start_queue: Arc::clone(&self.auto_start_queue),
            third_party_usage: Arc::clone(&self.third_party_usage_store),
            third_party_status: Arc::clone(&self.third_party_status),
            third_party_streaks: Arc::clone(&self.third_party_streaks),
            third_party_broken: Arc::clone(&self.third_party_broken),
            suppressed: Arc::clone(&self.suppressed_auth_expired),
            interval_ms: Arc::clone(&self.refresh_interval),
            snapshot: Arc::clone(&self.snapshot),
        }
    }

    /// Drain the forgets a change site noted and apply each to memory, with no
    /// config guard held (the stores rank BELOW `Config`).
    fn apply_forgets(&self) {
        apply_pending_forgets(&self.memory_stores());
    }

    /// Build the per-frame readings snapshot. The picker's per-name
    /// classification and the refused setup tokens (off the dead-login set,
    /// read before) are read under a transient config guard, DROPPED before
    /// any store lock is taken (the stores rank BELOW `Config`, so holding both
    /// would invert the order the debug build asserts on); the four stores are
    /// then locked once each, in rank order, and released.
    pub(crate) fn readings_snapshot(&self) -> ReadingsSnapshot {
        // The dead-login set the `⊘` reads, so the card and the mark never
        // judge one account apart.
        let key_rejected = self.key_rejected_names();
        let (third_party, token_refused): (HashMap<String, bool>, HashSet<String>) = {
            let cfg = self.config();
            (
                cfg.profiles
                    .iter()
                    .map(|p| (p.name.to_string(), p.usage_cache_is_third_party()))
                    .collect(),
                cfg.profiles
                    .iter()
                    .filter(|p| {
                        crate::usage::dead_login_is_setup_token(p) && key_rejected.contains(&p.name)
                    })
                    .map(|p| p.name.to_string())
                    .collect(),
            )
        };
        // ThirdPartyUsage 270, ThirdPartyStatus 280, Usage 300, UsageStatus 350.
        // A poisoned lock still holds the last good figure: read through it, or
        // one panic while a store is held blanks every reading for the rest of
        // the session.
        let third_party_usage = self
            .third_party_usage_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let third_party_status = self
            .third_party_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let usage_store = self
            .usage_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let status_store = self
            .usage_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut usage = HashMap::new();
        let mut fetch_status = HashMap::new();
        for (name, tp) in &third_party {
            // The same picker `apply_usage` assigned: a third-party name takes
            // its provider stats' `to_usage_info()`, any other name the OAuth
            // store's entry. `None` removes the entry (a provider that stopped
            // publishing windows reads as no reading, not a frozen one).
            if let Some(info) = crate::fallback::picked_usage(
                *tp,
                usage_store.get(name),
                third_party_usage.get(name),
            ) {
                usage.insert(name.clone(), info);
            }
            // Fetch status follows the picker's side too, but a refused setup
            // token reads as the dead login it is ahead of both: a stored pair
            // keeps its `/usage` poll fresh while every session a switch starts
            // runs on the token, and with the probe off nothing writes a status.
            let status = if token_refused.contains(name) {
                Some(FetchStatus::AuthExpired)
            } else if *tp {
                third_party_status.get(name).copied()
            } else if status_store.contains_key(name) {
                status_store.get(name).copied()
            } else {
                third_party_status.get(name).copied()
            };
            if let Some(s) = status {
                fetch_status.insert(name.clone(), s);
            }
        }
        ReadingsSnapshot {
            usage,
            fetch_status,
            third_party: third_party_usage,
            oauth: usage_store,
            stale: self.stale_cues.clone(),
        }
    }

    pub(crate) fn apply_usage(&mut self) {
        // The per-frame snapshot builder doubles as the per-tick reader: the
        // bell, history/wallet re-read sets and the stale map all derive from
        // its one picker pass, never a second one.
        let readings = self.readings_snapshot();
        let (bells, history_names, wallet_names, stale_map) = {
            let cfg = self.config();
            let now = now_ms();
            let interval_ms = cfg.state.refresh_interval_ms;
            let refresh_spent_accounts = cfg.state.refresh_spent_accounts;
            let mut stale_map = HashMap::new();
            let mut bells = Vec::new();
            let mut history_names = Vec::new();
            let mut wallet_names = Vec::new();
            for p in &cfg.profiles {
                let name = p.name.to_string();
                let interval_ms = crate::usage::usage_interval_ms(p, interval_ms);
                // #74 degraded cue: cache age past the derived threshold reads
                // stale, independent of fetch_status. Same threshold, same
                // maxed-window exemption, and same age source as
                // `status.json`'s `age_stale` arm — the exemption reads the
                // DISK cache (`load_profile_cache`), never the live store,
                // because the two can diverge on a spent account the
                // scheduler dropped from its due set. OAuth goes through the
                // one age contract (`oauth_age`), the same one `status.json`
                // and the MCP payloads read; third-party figures keep the
                // cache mtime.
                let oauth_usage = if p.usage_cache_is_third_party() {
                    None
                } else {
                    load_profile_cache::<UsageInfo>(&p.name, USAGE_CACHE_FILE)
                };
                let spent_skipped = !refresh_spent_accounts
                    && oauth_usage
                        .as_ref()
                        .is_some_and(|u| windows_maxed(u, (now / 1000) as i64));
                let past_threshold = if p.usage_cache_is_third_party() {
                    profile_cache_mtime_ms(&p.name, usage_cache_file(p))
                        .is_some_and(|at| now.saturating_sub(at) > stale_after_ms(interval_ms))
                } else {
                    crate::profile_json::oauth_age(oauth_usage.as_ref(), now).is_stale(
                        stale_after_ms(interval_ms),
                        oauth_usage
                            .as_ref()
                            .is_some_and(crate::profile_json::publishes_a_live_window),
                    )
                };
                let stale = !spent_skipped && past_threshold;
                stale_map.insert(name.clone(), stale);
                // The same reading the daemon's bell judges: a lapsed 5h
                // window is no figure, and an aged one is not fresh.
                let util = readings
                    .usage_of(&name)
                    .and_then(|u| u.five_hour.as_ref())
                    .filter(|w| crate::profile_json::window_row_is_live(w))
                    .map(|u| u.utilization);
                let fresh = readings.status_of(&name) == Some(FetchStatus::Fresh) && !stale;
                bells.push((name.clone(), p.bell_threshold, util, fresh));
                if readings.usage.contains_key(&name) {
                    history_names.push(name.clone());
                }
                if p.usage_cache_is_third_party() {
                    wallet_names.push(name);
                }
            }
            (bells, history_names, wallet_names, stale_map)
        };
        self.stale_cues = stale_map;
        // `bells` is the full profile set this pass observed, so an account
        // deleted while ringing forgets and re-arms if re-created past the line.
        {
            let present: HashSet<&str> =
                bells.iter().map(|(name, _, _, _)| name.as_str()).collect();
            self.bell_fired.forget_absent(&present);
        }
        for (name, threshold, util, fresh) in bells {
            if let Some(message) = self.bell_fired.observe(&name, threshold, util, fresh) {
                self.toast(ToastKind::Warning, message);
                self.set_tab_activity(Tab::Overview, ToastKind::Warning);
            }
        }

        // Re-read any history log that changed on disk. The file is written by
        // whichever process holds the fetch lease — this one or a headless
        // daemon — so its CONTENT is the only reliable signal that new samples
        // landed: NTFS quantizes file times on windows and a rapid append can
        // land with a byte-identical mtime (measured 2026-09-13: 4/10 real-box
        // runs), so an mtime gate silently serves a stale series there.
        for name in &history_names {
            if let Ok(path) = crate::profile::profile_history_path(&ProfileName::from(name.clone()))
                && let Some(fp) = series_fingerprint(&path)
                && self.history_fp.get(name) != Some(&fp)
            {
                self.history_cache.insert(
                    name.clone(),
                    crate::profile::load_usage_history(&ProfileName::from(name.clone())),
                );
                self.history_fp.insert(name.clone(), fp);
            }
        }
        // The wallet series re-reads the same way.
        for name in &wallet_names {
            if let Ok(path) =
                crate::profile::profile_wallet_history_path(&ProfileName::from(name.clone()))
                && let Some(fp) = series_fingerprint(&path)
                && self.wallet_fp.get(name) != Some(&fp)
            {
                self.wallet_cache.insert(
                    name.clone(),
                    crate::profile::load_wallet_history(&ProfileName::from(name.clone())),
                );
                self.wallet_fp.insert(name.clone(), fp);
            }
        }
    }

    /// Refresh `unsaved_live_login` off the live credentials + current config.
    /// Flips come from the live file moving (a login minted elsewhere), the
    /// profile set changing, or a switch swapping the live file — every site
    /// that can produce one calls this (see the field's list).
    /// The snapshot reads the macOS Keychain item (a `security` call), so it
    /// runs on a worker and lands through [`drain_unsaved_live_login`]. A test
    /// computes it in place: a detached worker would outlive the test's
    /// `HomeSandbox` (the [`spawn_switch_gate`] precedent).
    pub(crate) fn refresh_unsaved_live_login(&mut self) {
        let config = Arc::clone(&self.config);
        let unsaved = move || {
            capture_snapshot().is_ok_and(|snap| {
                !snapshot_is_empty(&snap)
                    && find_matching_oauth_profile(
                        &Self::lock_config(&config),
                        snap.credentials.as_ref(),
                    )
                    .is_none()
            })
        };
        if cfg!(test) {
            self.unsaved_live_login = unsaved();
            return;
        }
        self.unsaved_login_generation += 1;
        let generation = self.unsaved_login_generation;
        let tx = self.unsaved_login_tx.clone();
        spawn_worker(move || {
            let _ = tx.send((generation, unsaved()));
        });
    }

    /// Refresh the two form predicates the render path reads from the cache —
    /// capturable flag. Computed on the tick (never per frame), the
    /// [`Self::unsaved_live_login`] precedent: each is a handful of disk reads
    /// the render path must not make.
    pub(crate) fn refresh_form_predicates(&mut self) {
        self.codex_operator_capturable = crate::actions::codex_operator_login_capturable();
    }

    /// Reload config if state mtime changed. Returns true on reload.
    pub(crate) fn reload_if_state_changed(&mut self) -> bool {
        let current = reload_fingerprint();
        if current == self.last_reload_fp {
            return false;
        }
        if let Ok(fresh) = load_config() {
            // Capture Setup's selection BEFORE the swap, so the tick's clamp
            // can keep `+ new` on the trailing row even when the roster moved
            // (a post-swap cursor read cannot tell `+ new` from a deleted last
            // account). The delete and tab-switch clamps never read this flag.
            self.reload_was_on_new =
                self.tab == Tab::Setup && self.setup_selection() == SetupSelection::New;
            let profile_names: Vec<ProfileName> =
                fresh.profiles.iter().map(|p| p.name.clone()).collect();
            let probes_off = crate::usage::probes_switched_off(&self.config(), &fresh);
            *self.config() = fresh;
            // Snapshot under the config guard, then DROP it before the disk
            // reads AND before locking the token mutexes — the anchor read must
            // not widen the config-lock hold, and TOKENS/THIRD_PARTY rank
            // OUTSIDE CONFIG, so writing them while config is held inverts the
            // global lock order (same shape as `refresh_tokens`).
            let (seeds, third_party, third_party_seed_names, names) = {
                let cfg = self.config();
                (
                    crate::usage::collect_token_seeds(&cfg),
                    crate::usage::collect_third_party_entries(&cfg.profiles),
                    collect_third_party_seed_names(&cfg),
                    cfg.profiles
                        .iter()
                        .map(|p| p.name.to_string())
                        .collect::<Vec<String>>(),
                )
            };
            let tokens = crate::usage::build_token_entries(seeds);
            // The reload re-read disk, so it adopts the fingerprint it just
            // re-read. The token list is rebuilt BEFORE the compare's forget
            // (the epoch bump): a partition landing between the two must clone
            // the REBUILT entry, never the old entry under a post-bump epoch —
            // that pairing stamps cadence for the old credential and delays
            // the new login's first fetch. The reverse window, a partition
            // between the rebuild and the compare while the snapshot half
            // still holds the old identity, drops the rebuilt entry as stale
            // for one tick; it self-heals on the next compare, never a wrong
            // reading.
            #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
            {
                *self
                    .usage_tokens
                    .lock()
                    .expect("usage_tokens mutex poisoned") = tokens;
                *self
                    .third_party_tokens
                    .lock()
                    .expect("third_party_tokens mutex poisoned") = third_party;
            }
            // The compare runs after the token-list write, and BEFORE the store
            // seeds and the broken-verdict union-add below: the union-add must
            // re-add a re-keyed name's verdict AFTER the compare cleared it, so
            // the seeded verdict survives the reload.
            crate::usage::reload_and_forget(&self.memory_stores(), &profile_names);
            // A probe switched off in another process leaves the account here
            // as the switch-off in this one does. After the compare, since its
            // forget re-seeds the reading from disk.
            for name in &probes_off {
                crate::usage::forget_probe_reading(&self.memory_stores(), name);
            }
            self.last_reload_fp = current;
            // Seed from disk the display-superset names the store lacks (a
            // profile added since the last load). A name the store already
            // holds keeps its live reading, status and cadence stamp: the disk
            // seed would rewind them to the cache's. The seed writes the
            // READING only — no status and no `last_fetched` stamp at this site
            // (6836c1a6's reload wrote neither), so a new name's first fetch is
            // due on the next tick, never deferred by a seeded stamp.
            let unseeded: Vec<String> = {
                let store = self
                    .third_party_usage_store
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                third_party_seed_names
                    .into_iter()
                    .filter(|name| !store.contains_key(name))
                    .collect()
            };
            bootstrap_third_party(
                &self.third_party_usage_store,
                &self.third_party_status,
                &self.last_fetched,
                &unseeded,
                &[],
                self.refresh_interval.load(Ordering::Relaxed),
            );
            // Union-add the durable seed: it can only ADD names whose record
            // matches the CURRENT fingerprint, and a re-keyed name still
            // drops at the intersection in `key_rejected_names`. A recovered
            // name can flicker back in for at most one sync interval — the
            // fetcher's error arm updates the live map before it clears the
            // durable record, so a sync landing in that gap re-reads the
            // still-standing record — and the next `sync_broken_verdicts`
            // pass drops it again.
            crate::usage::seed_dead_logins(&self.config, &self.third_party_broken);
            self.session_tokens = collect_session_tokens(&names);
            self.refresh_unsaved_live_login();
            repin_member_edit(self);
            true
        } else {
            false
        }
    }

    pub(crate) fn refresh_tokens(&self) {
        // Snapshot under the config lock, drop it, THEN read each profile's
        // anchor + stored token off disk — the anchor read must not widen the
        // config-lock hold.
        let (seeds, third_party) = {
            let cfg = self.config();
            (
                crate::usage::collect_token_seeds(&cfg),
                crate::usage::collect_third_party_entries(&cfg.profiles),
            )
        };
        let tokens = crate::usage::build_token_entries(seeds);
        #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        {
            *self
                .usage_tokens
                .lock()
                .expect("usage_tokens mutex poisoned") = tokens;
            *self
                .third_party_tokens
                .lock()
                .expect("third_party_tokens mutex poisoned") = third_party;
        }
    }

    /// Queue every profile for an immediate re-fetch (Overview `r`). Reuses each
    /// profile's cached plan/tier — only the single-profile refresh re-pulls
    /// `/profile`, keeping the global refresh light on the rate-limited host.
    pub(crate) fn manual_refresh(&self) {
        #[expect(clippy::expect_used, reason = "mutex poisoning is unrecoverable")]
        let mut names: Vec<ProfileName> = self
            .usage_tokens
            .lock()
            .expect("usage_tokens mutex poisoned")
            .iter()
            .map(|e| e.name.clone())
            .collect();
        // The probe leg builds its list each tick, so its members come off the
        // config.
        names.extend(
            crate::usage::collect_probe_seeds(&self.config())
                .into_iter()
                .map(|(name, _)| name),
        );
        if let Ok(entries) = self.third_party_tokens.lock() {
            for entry in entries.iter() {
                if !names.contains(&entry.name) {
                    names.push(entry.name.clone());
                }
            }
        }
        crate::usage::clear_codex_polled_at();
        let _ = self.refresh_monitors(None);
        for name in names {
            self.enqueue_refetch(&name, false);
        }
    }

    /// Queue a single profile for an immediate re-fetch (Usage `r` / action
    /// menu), also re-pulling its `/profile` (plan / tier).
    pub(crate) fn manual_refresh_one(&self, name: &ProfileName) {
        self.enqueue_refetch(name, true);
    }

    /// Mark a profile for an immediate re-fetch. `refresh_plan` expires the
    /// `/profile` TTL so the next fetch re-pulls plan/tier — set for an explicit
    /// single-profile refresh, cleared for the bulk refresh-all.
    fn enqueue_refetch(&self, name: &ProfileName, refresh_plan: bool) {
        // Light the selected cache leg immediately. Account-scoped refresh/switch
        // work stays untouched and outranks this marker in the render helper.
        let leg = {
            let config = self.config();
            config.find(name).map(crate::usage::FetchLeg::for_profile)
        };
        if let Some(leg) = leg {
            crate::usage::mark_fetch_activity(
                &self.activity,
                &leg.key(name.clone()),
                ProfileActivity::Queued,
            );
        }
        if refresh_plan {
            crate::usage::expire_profile_ttl(name);
        }
        if let Ok(mut q) = self.refetch_queue.lock() {
            q.insert(name.to_string());
        }
    }

    /// Every modal opens here. Opening one drops a still-pending first herdr
    /// landing: the landing happens on the tick its probe drains or never, so
    /// it can neither move the view behind a modal nor after the user's own
    /// action closed one.
    pub(crate) fn open_modal(&mut self, modal: Modal) {
        self.modal_generation
            .set(self.modal_generation.get().wrapping_add(1));
        // Opening any modal (help, the action menu, a confirm, a login) moves
        // focus away: an armed codex/pool 2-step disarms and its label restores
        // (the cloudy-tui Inline 2-step rule).
        disarm_codex_2step(self);
        self.modals.push(modal);
    }

    /// Every modal closes here. The [`open_modal`] bump alone carries the
    /// scroll soundness (a reopen bumps the generation past any bound published
    /// before it), so the close bump is belt-and-braces.
    pub(crate) fn pop_modal(&mut self) -> Option<Modal> {
        let popped = self.modals.pop();
        if popped.is_some() {
            self.modal_generation
                .set(self.modal_generation.get().wrapping_add(1));
        }
        popped
    }

    /// Drop every modal matching `pred` and bump the modal generation once
    /// when any dropped, so a scroll bound published before the close is
    /// stale. The login modals close through here; [`pop_modal`] covers the
    /// top-modal pop.
    fn retain_modals(&mut self, pred: impl Fn(&Modal) -> bool) {
        let before = self.modals.len();
        self.modals.retain(pred);
        if self.modals.len() != before {
            self.modal_generation
                .set(self.modal_generation.get().wrapping_add(1));
        }
    }

    pub(crate) fn toast(&mut self, kind: ToastKind, body: impl Into<String>) {
        if self.toasts.len() >= TOAST_CAPACITY {
            self.toasts.pop_front();
        }
        self.toasts.push_back(Toast {
            kind,
            body: prose::mark_commands(&body.into()),
            born: Instant::now(),
        });
    }

    /// Disarm the 2-step quit and clear its accompanying footer alert.
    /// Only call this when the armed-quit sequence is interrupted (key switch,
    /// tab change, etc.).
    pub(crate) fn disarm_quit(&mut self) {
        self.armed_quit = None;
        self.footer_alert = None;
    }

    pub(crate) fn prune_toasts(&mut self) {
        let checking = self.bare_item_checks.load(Ordering::Acquire) > 0;
        self.toasts.retain(|toast| {
            if toast.body == BARE_ITEM_CHECKING {
                return checking;
            }
            let ttl = if toast.kind == ToastKind::Danger {
                TOAST_TTL_DANGER
            } else {
                TOAST_TTL_NORMAL
            };
            toast.born.elapsed() < ttl
        });
    }

    /// The narrowest terminal width every surface still reads at whole or
    /// visibly ellipsized. Set by the Overview accounts row: with the 5h bar
    /// shed to its `%` figure, the row's floor is cursor(2) + marker(2) +
    /// name(8) + gap(2) + kind(6) + narrow(1) + timer(5) + `%`(4) = 30 cells
    /// inside a 4-cell-chromed panel, so the terminal needs 34. The Fallback
    /// card's `preferred days` picker only needs 28; the Overview row is the
    /// binding surface.
    const WIDTH_FLOOR: u16 = 34;

    /// Called each frame with the current terminal size. Tracks compact
    /// state and the width floor; the matching warning banner is recomputed
    /// from these flags in `update_banner` each tick, so it self-clears on
    /// resize (no toast).
    pub(crate) fn update_compact(&mut self, terminal_height: u16, terminal_width: u16) {
        self.compact = terminal_height < 14;
        self.below_width_floor = terminal_width < Self::WIDTH_FLOOR;
    }

    /// Mark a background tab with an activity color. Only sets if `tab` is not
    /// the currently-active tab; visiting the tab (via `switch_tab`) clears it.
    /// Higher-severity kinds override lower ones (Danger > Warning > Success > Info).
    pub(crate) fn set_tab_activity(&mut self, tab: Tab, kind: ToastKind) {
        if tab == self.tab {
            return;
        }
        let idx = tab.index();
        let prev = self.tab_activity[idx];
        let severity = |k: ToastKind| match k {
            ToastKind::Danger => 3,
            ToastKind::Warning => 2,
            ToastKind::Success => 1,
            ToastKind::Info => 0,
        };
        if prev.is_none_or(|p| severity(kind) > severity(p)) {
            self.tab_activity[idx] = Some(kind);
        }
    }

    /// The Overview's visible item list under its current harness filter.
    pub(crate) fn main_items(&self) -> Vec<MainItemKind> {
        self.main_items_for(self.harness_filter)
    }

    /// The flat item list for `filter`: the [`App::band_entries`] items in
    /// walk order. One producer with the filter as a parameter — the Overview
    /// passes its filter, Setup passes [`HarnessFilter::All`] — so the two
    /// lists can never drift.
    pub(crate) fn main_items_for(&self, filter: HarnessFilter) -> Vec<MainItemKind> {
        self.band_entries(filter)
            .into_iter()
            .filter_map(|entry| match entry {
                BandEntry::Item(item) => Some(item),
                BandEntry::Spacer | BandEntry::Heading(_) => None,
            })
            .collect()
    }

    /// The shared band walk: claude rows first (no chrome), then the codex
    /// precede it and a section heading. Both selectors render from this one
    /// walk — the Overview's `draw_overview_accounts` and Setup's `draw_selector`
    /// derive their rows, headings, spacers and order from it, and each lifts a
    /// heading only while a row under it holds focus via [`App::heading_ranges`].
    pub(crate) fn band_entries(&self, filter: HarnessFilter) -> Vec<BandEntry> {
        let claude_count = if filter.shows_claude() {
            self.profile_count()
        } else {
            0
        };
        let codex_count = if filter.shows_codex() {
            self.codex_rows.len()
        } else {
            0
        };
        let mut entries = Vec::new();
        let profiles = {
            let cfg = self.config();
            cfg.profiles
                .iter()
                .enumerate()
                .take(claude_count)
                .map(|(i, p)| (i, profile_provider_name(p).to_string()))
                .collect::<Vec<_>>()
        };
        let mut kinds = Vec::<String>::new();
        for (_, provider) in &profiles {
            if !kinds.contains(provider) {
                kinds.push(provider.clone());
            }
        }
        let show_headings = kinds.len() > 1 || !self.usage_extras.is_empty();
        let mut last = None;
        for (idx, provider) in profiles {
            if show_headings && last.as_ref() != Some(&provider) {
                if !entries.is_empty() {
                    entries.push(BandEntry::Spacer);
                }
                entries.push(BandEntry::Heading(provider.clone()));
                last = Some(provider);
            }
            entries.push(BandEntry::Item(MainItemKind::Profile(idx)));
        }
        if codex_count > 0 {
            if claude_count > 0 {
                entries.push(BandEntry::Spacer);
            }
            entries.push(BandEntry::Heading("codex".to_string()));
            entries.extend((0..codex_count).map(|i| BandEntry::Item(MainItemKind::Codex(i))));
        }

        if filter.shows_hermes() && !self.hermes_rows.is_empty() {
            if !entries.is_empty() {
                entries.push(BandEntry::Spacer);
            }
            entries.push(BandEntry::Heading("hermes".to_string()));
            entries.extend(
                (0..self.hermes_rows.len()).map(|i| BandEntry::Item(MainItemKind::Hermes(i))),
            );
        }
        entries
    }

    /// The flat item range each heading covers, in entry order:
    /// `(entry_index, flat_start, flat_end)` (end exclusive). One shared walk
    /// so the Overview and Setup selectors agree on which heading lifts when a
    /// row under it holds focus.
    pub(crate) fn heading_ranges(entries: &[BandEntry]) -> Vec<(usize, usize, usize)> {
        let mut ranges = Vec::new();
        let mut flat = 0;
        let mut open: Option<(usize, usize)> = None;
        for (i, entry) in entries.iter().enumerate() {
            match entry {
                BandEntry::Heading(_) => {
                    if let Some((hi, s)) = open.take() {
                        ranges.push((hi, s, flat));
                    }
                    open = Some((i, flat));
                }
                BandEntry::Item(_) => flat += 1,
                BandEntry::Spacer => {}
            }
        }
        if let Some((hi, s)) = open.take() {
            ranges.push((hi, s, flat));
        }
        ranges
    }

    pub(crate) fn profile_count(&self) -> usize {
        self.config().profiles.len()
    }

    pub(crate) fn profile_name_at(&self, idx: usize) -> Option<ProfileName> {
        self.config().profiles.get(idx).map(|p| p.name.clone())
    }

    /// Clamp `profile_cursor` to `0..profile_count`.
    pub(crate) fn clamp_profile_cursor(&mut self) {
        let max = self.profile_count().saturating_sub(1);
        self.profile_cursor.set(self.profile_cursor.read().min(max));
        self.place_overview_cursor();
        // Every reload path lands here (a roster change can move or remove the
        // selected account), so the note tracks the selection for free.
        self.reload_selected_note();
    }

    /// Re-read the selected account's note from disk.
    pub(crate) fn reload_selected_note(&mut self) {
        self.note_text = self
            .profile_name_at(self.profile_cursor.read())
            .and_then(|name| crate::profile_notes::load_note(&name));
    }

    pub(crate) fn current_main_item(&self) -> Option<MainItemKind> {
        self.main_items().get(self.overview_cursor).copied()
    }

    /// Setup's flat item list: the Overview's items under the all-filter. Its
    /// `len()` is the trailing `+ new` row.
    pub(crate) fn setup_items(&self) -> Vec<MainItemKind> {
        self.main_items_for(HarnessFilter::All)
    }

    /// What Setup's selector has selected: a harness item, or the trailing
    /// `+ new`. A claude row is `profile_cursor` itself (its `+ new` synthetic
    /// [`App::setup`], resolved to its index against the current lists. A
    /// hidden identity lives in the slot's `remembered` alone, so the shown
    /// selection IS the first row; a read list that dropped the identity is
    /// re-homed by [`App::rehome_setup_selection`] on the tick.
    pub(crate) fn setup_selection(&self) -> SetupSelection {
        if let Some(item) = &self.setup.item
            && let Some(resolved) = self.resolve_setup_item(item)
        {
            return SetupSelection::Item(resolved);
        }
        if self.profile_cursor.read() >= self.profile_count() {
            SetupSelection::New
        } else {
            SetupSelection::Item(MainItemKind::Profile(self.profile_cursor.read()))
        }
    }

    /// Resolve a stored Setup identity to its current `MainItemKind` index;
    /// `None` when the current lists no longer hold it.
    fn resolve_setup_item(&self, item: &SetupItem) -> Option<MainItemKind> {
        match item {
            SetupItem::Hermes(name) => self
                .hermes_rows
                .iter()
                .position(|row| &row.name == name)
                .map(MainItemKind::Hermes),

            SetupItem::Codex(name) => self
                .codex_rows
                .iter()
                .position(|row| row.name == *name)
                .map(MainItemKind::Codex),
        }
    }

    /// Whether the identity is still held — a codex name in the roster, a pool
    /// `(provider, account)` in a read listing, or a pool hidden by an unread
    /// listing (which keeps its rows like a filter, never drops them).
    fn holds_setup_item(&self, item: &SetupItem) -> bool {
        match item {
            SetupItem::Hermes(name) => self.hermes_rows.iter().any(|row| &row.name == name),

            SetupItem::Codex(name) => self.codex_rows.iter().any(|row| row.name == *name),
        }
    }

    /// The identity for a resolved harness item; `None` for a claude row (the
    /// shared cursor is its own identity).
    fn setup_item_for(&self, item: MainItemKind) -> Option<SetupItem> {
        match item {
            MainItemKind::Hermes(i) => self
                .hermes_rows
                .get(i)
                .map(|row| SetupItem::Hermes(row.name.clone())),

            MainItemKind::Codex(i) => self
                .codex_rows
                .get(i)
                .map(|row| SetupItem::Codex(row.name.clone())),
            MainItemKind::Profile(_) => None,
        }
    }

    /// The flat cursor position of the current Setup selection (an index into
    /// [`App::setup_items`], its `len()` the trailing `+ new` row) — the input
    /// to the wrapping ↑↓ step. A held-but-hidden identity (an unread pool)
    /// shows on the first row, the Overview's placement rule.
    pub(crate) fn setup_position(&self) -> usize {
        let items = self.setup_items();
        match self.setup_selection() {
            SetupSelection::New => items.len(),
            SetupSelection::Item(item) => items.iter().position(|row| *row == item).unwrap_or(0),
        }
    }

    /// The selection slot `kind` owns.
    fn selection_slot(&self, kind: SelectionSlotKind) -> &HeldSelection {
        match kind {
            SelectionSlotKind::Setup => &self.setup,
            SelectionSlotKind::Usage => &self.usage,
        }
    }

    fn selection_slot_mut(&mut self, kind: SelectionSlotKind) -> &mut HeldSelection {
        match kind {
            SelectionSlotKind::Setup => &mut self.setup,
            SelectionSlotKind::Usage => &mut self.usage,
        }
    }

    /// Land a tab's selection on flat position `pos`: a codex or pool item is
    /// stored as its [`SetupItem`] identity (leaving `profile_cursor` alone); a
    /// claude row or `+ new` clears it and moves the shared cursor. A move is a
    /// choice of a new row, so it replaces the slot's held-but-hidden identity
    /// (`remembered`) and drops a pending pool landing. A shared-claude-cursor
    /// move bumps the move count, which the restore gate in
    /// [`App::rehome_selection`] reads; the other slot's clear rides the user
    /// step alone ([`App::step_selection`]), never this placement path, so a
    /// rehome clears nothing. Setup's past-end (`+ new`) and Usage's past-end
    /// differ only in where the shared cursor rests.
    fn place_selection(&mut self, kind: SelectionSlotKind, pos: usize) {
        self.selection_slot_mut(kind).clear();
        match self.setup_items().get(pos) {
            Some(item @ (MainItemKind::Codex(_) | MainItemKind::Hermes(_))) => {
                let identity = self.setup_item_for(*item);
                let slot = self.selection_slot_mut(kind);
                slot.item = identity;
                slot.index = Some(pos);
            }

            Some(MainItemKind::Profile(i)) => {
                let slot = self.selection_slot_mut(kind);
                slot.item = None;
                slot.index = None;
                if self.profile_cursor.read() != *i {
                    self.profile_cursor.set(*i);
                    self.reload_selected_note();
                }
            }
            None => {
                let slot = self.selection_slot_mut(kind);
                slot.item = None;
                slot.index = None;
                self.profile_cursor.set(match kind {
                    SelectionSlotKind::Setup => self.profile_count(),
                    SelectionSlotKind::Usage => self.profile_count().saturating_sub(1),
                });
            }
        }
    }

    fn place_setup_selection(&mut self, pos: usize) {
        self.place_selection(SelectionSlotKind::Setup, pos);
    }

    /// A user step (a ↑/↓ or page key, never a rehome) lands `kind`'s
    /// selection on flat position `pos`. A step whose row actually changed ends
    /// the other tab's held-but-hidden identity — the Overview's rule
    /// ([`App::move_overview_cursor`]): any index change clears the hold, and
    /// the shared claude cursor's move count cannot see all of them (a step off
    /// count). The clear reaches Setup <-> Usage only; the Overview's own
    /// `overview_remembered` is untouched. It rides only this user-move path,
    /// so a rehome's own `place_selection` (a listing blip, no user move) never
    /// ends the other tab's hold.
    fn step_selection(&mut self, kind: SelectionSlotKind, pos: usize) {
        let current = match kind {
            SelectionSlotKind::Setup => self.setup_position(),
            SelectionSlotKind::Usage => self.usage_position(),
        };
        self.place_selection(kind, pos);
        if current != pos {
            self.selection_slot_mut(kind.other()).clear();
        }
    }

    /// The flat [`App::setup_items`] index `item` sits at (a claude row's is
    /// its profile index; `+ new` is the list's `len()`).
    fn flat_index_of(&self, item: MainItemKind) -> usize {
        self.setup_items()
            .iter()
            .position(|row| *row == item)
            .unwrap_or(0)
    }

    /// Ascend a tab out of its descended pane before a re-place lands it on a
    /// different row.
    fn leave_selection_detail(&mut self, kind: SelectionSlotKind) {
        match kind {
            SelectionSlotKind::Setup => {
                if self.config_focus == ConfigFocus::Actions {
                    leave_config_detail(self);
                }
            }
            SelectionSlotKind::Usage => {
                if self.usage_detail_focus == StatusFocus::Detail {
                    self.usage_detail_focus = StatusFocus::List;
                }
            }
        }
    }

    /// Re-place a tab's selection after the codex or pool snapshot changed,
    /// the Overview's placement rule. Runs only while the slot's own tab shows
    /// (the Overview's precedent: [`App::place_overview_cursor`] returns early
    /// off the Overview), so a pool read on another tab never rewrites the
    /// shared claude cursor its row is. A selection that still resolves stays
    /// (its flat index refreshed beside it); a hidden-but-held one (an unread
    /// pool) ascends out of the detail, holds the identity separately in the
    /// slot's `remembered` and shows on the first row; a dropped-for-good one
    /// lands on the row now at the flat index it held, clamped to the last
    /// item row (`+ new` only when no item row exists) — across bands, never
    /// the shared claude cursor by accident. A held identity comes back only
    /// while the shared claude cursor's move count still equals the one the
    /// hide recorded (`remembered_moves`) — any writer that moves the cursor
    /// makes the restore a no-op by construction.
    fn rehome_selection(&mut self, kind: SelectionSlotKind) {
        if self.tab != kind.tab() {
            return;
        }
        // A held-but-hidden identity restores when the read returns it — only
        // if the shared claude cursor has not moved since the hide's own
        // placement.
        if let Some(remembered) = self.selection_slot(kind).remembered.clone() {
            let unmoved =
                self.selection_slot(kind).remembered_moves == Some(self.profile_cursor.count());
            if unmoved && let Some(resolved) = self.resolve_setup_item(&remembered) {
                let index = self.flat_index_of(resolved);
                let slot = self.selection_slot_mut(kind);
                slot.item = Some(remembered);
                slot.index = Some(index);
                slot.remembered = None;
                slot.remembered_moves = None;
                return;
            }
            if !self.holds_setup_item(&remembered) {
                self.selection_slot_mut(kind).clear();
            }
        }
        let Some(item) = self.selection_slot(kind).item.clone() else {
            return;
        };
        if let Some(resolved) = self.resolve_setup_item(&item) {
            self.selection_slot_mut(kind).index = Some(self.flat_index_of(resolved));
            return; // still resolves
        }
        if self.holds_setup_item(&item) {
            // Hidden but held: ascend, hold the identity separately (like
            // `overview_remembered`) and put the selection on the shown first
            // row, recording the cursor's move count right after that
            // placement. It comes back only while the count still equals it.
            self.leave_selection_detail(kind);
            if self.selection_slot(kind).remembered.is_none() {
                self.place_selection(kind, 0);
                let moves = self.profile_cursor.count();
                let slot = self.selection_slot_mut(kind);
                slot.remembered = Some(item);
                slot.remembered_moves = Some(moves);
            }
            return;
        }
        // Dropped for good: the row now at the flat index it held, clamped to
        // the last item row (`+ new` only when no item row exists), across
        // bands.
        let items = self.setup_items();
        let held = self.selection_slot(kind).index.unwrap_or(0);
        let pos = if items.is_empty() {
            0
        } else {
            held.min(items.len() - 1)
        };
        self.place_selection(kind, pos);
        self.leave_selection_detail(kind);
    }

    fn rehome_setup_selection(&mut self) {
        self.rehome_selection(SelectionSlotKind::Setup);
    }

    fn rehome_usage_selection(&mut self) {
        self.rehome_selection(SelectionSlotKind::Usage);
    }

    /// The current Setup selection as a codex-row index, `None` off a codex row.
    pub(crate) fn setup_codex_index(&self) -> Option<usize> {
        match self.setup_selection() {
            SetupSelection::Item(MainItemKind::Codex(i)) if i < self.codex_rows.len() => Some(i),
            _ => None,
        }
    }

    /// What Usage has selected: always a harness item — a claude row is
    /// [`App::usage`] resolved against the current lists. With no claude
    /// the caret row and the pane agree; `New` (Setup's trailing `+ new`
    /// variant) is produced only when no item exists at all, where the claude
    /// pane draws its empty state. A hidden identity re-homes on the tick via
    /// [`App::rehome_usage_selection`].
    pub(crate) fn usage_selection(&self) -> SetupSelection {
        if let Some(item) = &self.usage.item
            && let Some(resolved) = self.resolve_setup_item(item)
        {
            return SetupSelection::Item(resolved);
        }
        if self.profile_count() > 0 {
            return SetupSelection::Item(MainItemKind::Profile(
                self.profile_cursor.read().min(self.profile_count() - 1),
            ));
        }
        match self.setup_items().first() {
            Some(item) => SetupSelection::Item(*item),
            None => SetupSelection::New,
        }
    }

    /// The flat cursor position of the current Usage selection (an index into
    /// [`App::setup_items`]). A selection that no longer resolves (a hidden
    /// pool) reads the first row, the Overview's placement rule.
    pub(crate) fn usage_position(&self) -> usize {
        let items = self.setup_items();
        match self.usage_selection() {
            SetupSelection::Item(item) => items.iter().position(|row| *row == item).unwrap_or(0),
            SetupSelection::New => 0,
        }
    }

    /// The current Usage selection as a codex-row index, `None` off a codex row.
    pub(crate) fn usage_codex_index(&self) -> Option<usize> {
        match self.usage_selection() {
            SetupSelection::Item(MainItemKind::Codex(i)) if i < self.codex_rows.len() => Some(i),
            _ => None,
        }
    }

    /// Whether the item universe still holds `item` (regardless of filter). A
    /// row is gone only when its own list was read and lacks it; an unread
    /// pool (`NotRead` / `Absent`) hides its rows like a filter, never drops
    /// them, so the next read restores the cursor.
    fn holds_item(&self, item: MainItemKind) -> bool {
        match item {
            MainItemKind::Hermes(i) => i < self.hermes_rows.len(),

            MainItemKind::Profile(i) => i < self.profile_count(),
            MainItemKind::Codex(i) => i < self.codex_rows.len(),
        }
    }

    /// Place the Overview cursor on the remembered item's row when the current
    /// view shows it; on row 0 when the item still exists but the filter hides
    /// it (`c` never writes it); when the item set no longer holds it, or
    /// nothing is remembered, keep the cursor's index clamped to the view and
    /// remember the item now there. Identity stays by (kind, index); a
    /// remembered claude row is `profile_cursor` itself, never a stored copy.
    fn place_overview_cursor(&mut self) {
        if self.tab != Tab::Overview {
            return;
        }
        let items = self.main_items();
        let remembered = match self.overview_remembered {
            Some(MainItemKind::Profile(_)) => {
                Some(MainItemKind::Profile(self.profile_cursor.read()))
            }
            other => other,
        };
        let target = remembered.and_then(|item| items.iter().position(|row| *row == item));
        if let Some(idx) = target {
            self.overview_cursor = idx;
        } else {
            match remembered {
                Some(item) if self.holds_item(item) => {
                    self.overview_cursor = 0;
                }
                _ => {
                    self.overview_cursor = self.overview_cursor.min(items.len().saturating_sub(1));
                    self.overview_remembered = items.get(self.overview_cursor).copied();
                }
            }
        }
        self.sync_overview_profile_selection();
    }

    /// The placement invariant: while the Overview shows and its cursor rests
    /// on a claude row k, `profile_cursor == k`, so the `a` menu names the
    /// highlighted claude row (off the Overview, entering it re-places first).
    /// The note reloads only when the selection actually moved.
    fn sync_overview_profile_selection(&mut self) {
        if let Some(MainItemKind::Profile(idx)) = self.current_main_item()
            && self.profile_cursor.read() != idx
        {
            self.profile_cursor.set(idx);
            self.reload_selected_note();
        }
    }

    /// A cursor-move key: when the index changed, the remembered item is the
    /// one now under the cursor (a move onto a claude row remembers it as
    /// `profile_cursor` itself). A key that leaves the index where it was (a
    /// one-row view) remembers nothing — it was no choice of a new row.
    fn move_overview_cursor(&mut self, next: usize) {
        if next != self.overview_cursor {
            self.overview_cursor = next;
            self.overview_remembered = self.current_main_item();
            self.sync_overview_profile_selection();
            // A selection change on another tab replaces a held-but-hidden
            // identity in both slots: the read must not restore it over this
            // pick.
            self.setup.clear();
            self.usage.clear();
            // And drops a pending pool landing: the user moved on, so the
            // re-read must not yank the Overview onto the added row.
        }
    }

    /// Set the guest-mode flag the header's `[ guest ]` pill reads.
    pub(crate) fn with_guest_mode(mut self, guest_mode: bool) -> Self {
        self.guest_mode = guest_mode;
        self
    }

    /// Hand the live palette reloader to the tick (`tui::run`).
    pub(crate) fn with_palette_watch(mut self, watch: super::theme::PaletteWatch) -> Self {
        self.palette_watch = Some(watch);
        self
    }

    /// An explicit `--tab` (the herdr `tollgate.usage` action's `--tab usage`)
    /// outranks both the configured home tab and the first herdr landing:
    /// the caller asked for that view. Applied at construction like
    /// [`Self::with_herdr_mode`]'s landing, so the first paint is on it.
    pub(crate) fn with_open_tab(mut self, tab: Option<HomeTab>) -> Self {
        if let Some(tab) = tab {
            self.tab = tab.into();
        }
        self
    }

    /// Whether a monitor observation (`"monitor:<id>"`) currently has an
    /// in-flight manual refresh.
    pub(crate) fn is_monitor_refreshing(&self, obs_id: &str) -> bool {
        self.monitor_refresh_inflight
            .lock()
            .is_ok_and(|set| set.contains(obs_id))
    }

    /// Refresh one monitor observation (`"monitor:<id>"`), returning `true`
    /// when a matching enabled monitor exists in `monitors.toml`.
    pub(crate) fn refresh_monitor_one(&self, obs_id: &str) -> bool {
        let Some(id) = obs_id.strip_prefix("monitor:") else {
            return false;
        };
        self.refresh_monitors(Some(id)) > 0
    }

    /// Spawn a forced refresh for all enabled monitors (or a single `target_id`),
    /// marking each in `monitor_refresh_inflight` while the worker runs.
    pub(crate) fn refresh_monitors(&self, target_id: Option<&str>) -> usize {
        self.refresh_monitors_with(target_id, true)
    }

    /// Spawn a background refresh for any enabled monitor that is currently due,
    /// marking each in `monitor_refresh_inflight` while the worker runs.
    #[cfg(not(test))]
    pub(crate) fn refresh_due_monitors(&self) -> usize {
        self.refresh_monitors_with(None, false)
    }

    fn refresh_monitors_with(&self, target_id: Option<&str>, force: bool) -> usize {
        use crate::usage::observation::{Origin, account_id};
        let Ok(monitors) = crate::usage::monitor::config::load() else {
            return 0;
        };
        let now_ms = crate::usage::now_ms();
        let targets: Vec<crate::usage::monitor::config::MonitorConfig> = monitors
            .into_iter()
            .filter(|m| {
                m.enabled
                    && target_id.is_none_or(|id| m.id == id)
                    && (force
                        || crate::usage::monitor::cache::is_due(
                            m,
                            crate::usage::monitor::cache::load(&m.id).as_ref(),
                            now_ms,
                        ))
            })
            .collect();
        if targets.is_empty() {
            return 0;
        }
        let mut to_spawn = Vec::new();
        if let Ok(mut inflight) = self.monitor_refresh_inflight.lock() {
            for m in targets {
                let key = account_id(Origin::Monitor, &m.id);
                if inflight.insert(key) {
                    to_spawn.push(m);
                }
            }
        }
        let count = to_spawn.len();
        if to_spawn.is_empty() {
            return 0;
        }
        #[cfg(test)]
        let test_http = TEST_MONITOR_HTTP.with(|cell| cell.borrow().clone());
        #[cfg(test)]
        if test_http.is_none() {
            return count;
        }
        let inflight = Arc::clone(&self.monitor_refresh_inflight);
        let dirty = Arc::clone(&self.monitor_refresh_dirty);
        spawn_worker(move || {
            #[cfg(not(test))]
            crate::secrets::reload_if_changed();
            for m in to_spawn {
                let key = account_id(Origin::Monitor, &m.id);
                struct ClearInflight {
                    inflight: Arc<std::sync::Mutex<HashSet<String>>>,
                    dirty: Arc<AtomicBool>,
                    key: String,
                }
                impl Drop for ClearInflight {
                    fn drop(&mut self) {
                        if let Ok(mut set) = self.inflight.lock() {
                            set.remove(&self.key);
                        }
                        self.dirty.store(true, Ordering::Release);
                    }
                }
                let _clear = ClearInflight {
                    inflight: Arc::clone(&inflight),
                    dirty: Arc::clone(&dirty),
                    key,
                };
                #[cfg(not(test))]
                let http_ref: &dyn crate::usage::monitor::source::MonitorHttp =
                    &crate::usage::monitor::source::LiveHttp;
                #[cfg(test)]
                let Some(ref http_arc) = test_http else {
                    continue;
                };
                #[cfg(test)]
                let http_ref: &dyn crate::usage::monitor::source::MonitorHttp = http_arc.as_ref();
                let deps = crate::usage::monitor::cache::RefreshDeps {
                    http: http_ref,
                    notifier: None,
                    env: &crate::usage::monitor::source::process_env,
                    now_ms: crate::usage::now_ms(),
                };
                let _ = crate::usage::monitor::cache::refresh_one(&m, &deps, force);
            }
        });
        count
    }

    /// The Overview's read-only rows in display order, as indices into
    /// [`App::usage_extras`]: upstream-clauth accounts first (the
    /// `clauth (read-only)` group), then monitors grouped by provider. Empty
    /// while no collect hook yields anything, which keeps the Overview
    /// exactly as it was.
    pub(crate) fn overview_extras(&self) -> Vec<usize> {
        let upstream = |o: &crate::usage::observation::AccountObservation| {
            o.origin == crate::usage::observation::Origin::Upstream
        };
        // A Hermes profile renders in the Overview's own Hermes section, not
        // among the read-only extras (the Usage tab still lists it).
        let hermes = |o: &crate::usage::observation::AccountObservation| {
            o.origin == crate::usage::observation::Origin::HermesProfile
        };
        let (first, rest): (Vec<usize>, Vec<usize>) = (0..self.usage_extras.len())
            .filter(|&i| !hermes(&self.usage_extras[i]))
            .partition(|&i| upstream(&self.usage_extras[i]));
        let mut out = group_extra_indices_by_provider(first, &self.usage_extras);
        out.extend(group_extra_indices_by_provider(rest, &self.usage_extras));
        out
    }

    /// The extra the Overview selection rests on: the shared
    /// [`App::usage_extra_cursor`], else — with no profile to rest on — the
    /// first read-only row, the same fallback the Usage rail applies.
    pub(crate) fn overview_selected_extra(&self) -> Option<usize> {
        if !self.harness_filter.shows_claude() {
            return None;
        }
        match self.usage_extra_cursor {
            Some(i) if i < self.usage_extras.len() => Some(i),
            _ if self.main_items().is_empty() => self.overview_extras().first().copied(),
            _ => None,
        }
    }
}

/// Seed the OAuth and third-party stores from their disk caches with the
/// display superset (disabled + keyless third-party included; status and
/// stamp for the third-party work list only, [`bootstrap_third_party`]).
/// Called from [`reconcile_startup`] on every arm that leaves the drain
/// nothing to resolve (no active profile, `Clean`, and a `Shell` or
/// `NeedsPrompt` verdict whose resolution is the banner alone), so the first
/// frame shows cached bars; never in `App::new`, which is before reconcile.
/// Runs once: `startup_seeded` limits it to a single call — from
/// [`reconcile_startup`] on the arms above, else from [`maybe_spawn_bootstrap`]
/// at the gate.
fn seed_startup_stores(app: &App) {
    let interval_ms = app.refresh_interval.load(Ordering::Relaxed);
    let (seed_names, third_party, third_party_work) = {
        let cfg = app.config();
        (
            collect_oauth_seed_names(&cfg, interval_ms),
            collect_third_party_seed_names(&cfg),
            collect_third_party_entries(&cfg.profiles),
        )
    };
    bootstrap_fetch(
        &app.usage_store,
        &app.usage_status,
        &app.last_fetched,
        &seed_names,
    );
    bootstrap_third_party(
        &app.third_party_usage_store,
        &app.third_party_status,
        &app.last_fetched,
        &third_party,
        &third_party_work,
        interval_ms,
    );
}

/// Startup credential reconciliation, non-blocking. Judges the live
/// `~/.claude/.credentials.json` through the SAME cascade the 1Hz poll uses
/// ([`reconcile_decision`]): against the install source, never the
/// tollgate-private usage pair — a refreshless live login (a rolling bearer, a
/// static mint) is no divergence (#104). No divergence → snapshot +
/// `ReconcileDone` immediately.
///
/// On divergence we never probe the stored chain via OAuth refresh — a refresh
/// *spends* the single-use refresh token server-side and would rotate the stored
/// identity on every diverged startup. Instead the Divergence modal hands the
/// verdict to the user. None of its actions spend the stored token (Overwrite
/// takes live creds, NewProfile captures them, Discard relinks as-is); a stale
/// access token is refreshed lazily on the next fetch.
pub(super) fn reconcile_startup(app: &mut App) {
    if crate::identity::upstream_active() {
        seed_startup_stores(app);
        app.startup_seeded = true;
        let _ = app.startup_sender.send(StartupSignal::ReconcileDone);
        return;
    }
    let Some(active) = app.config().state.active_profile.as_ref().cloned() else {
        let _ = app.startup_sender.send(StartupSignal::ReconcileDone);
        seed_startup_stores(app);
        app.startup_seeded = true;
        return;
    };

    let verdict = reconcile_decision(app, &active);

    if matches!(verdict, DivergenceVerdict::Clean) {
        let mut cfg = app.config();
        let _ = snapshot_active_credentials(&mut cfg);
        // Drop the config guard before seeding: the stores rank BELOW config,
        // so seeding under the guard would invert the lock order.
        drop(cfg);
        seed_startup_stores(app);
        app.startup_seeded = true;
    } else if divergence_resolves_to_banner(app, &active) {
        // The drain's `resolve_or_note_divergence` will only flag the banner,
        // so nothing it does can change what the seed reads. A default that
        // acts (an overwrite, or a modal it opens) may delete or replace a
        // profile's caches first: `maybe_spawn_bootstrap` seeds after it, at
        // the point the worker's gate opens.
        seed_startup_stores(app);
        app.startup_seeded = true;
    }
    let _ = app
        .startup_sender
        .send(if matches!(verdict, DivergenceVerdict::Clean) {
            StartupSignal::ReconcileDone
        } else {
            StartupSignal::ReconcileNeedsPrompt {
                active: active.to_string(),
            }
        });
}

/// True while `app.tab`'s descend/ascend sub-focus screen is active (Setup's
/// Actions pane, Fallback's Detail pane, Status/Plugin's Detail pane, Tokens'
/// Models view) — the state where `q`/`esc` ascend instead of arming quit /
/// no-op. Single source of truth shared by the `q` handler and the footer's
/// `q back` / `q quit` label; the help-modal esc-row test iterates it too, so
/// a new sub-focus tab without an `esc` help row fails CI.
pub(crate) fn has_sub_focus(app: &App) -> bool {
    (app.tab == Tab::Usage && app.usage_detail_focus == StatusFocus::Detail)
        || (app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions)
        || (app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail)
        || (app.tab == Tab::Status && app.status.focus == StatusFocus::Detail)
        || (app.tab == Tab::Plugin && app.plugin.focus == PluginFocus::Detail)
        || (app.tab == Tab::Tokens && app.token_view == TokenView::Models)
}

/// What owns the keyboard this frame: every key it claims ([`KeyOwner::claims`]),
/// `←`/`→` included, reaches it before any tab switch or global binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyOwner {
    /// The modal stack: its top modal takes every key.
    Modal,
    /// A Setup detail row's inline field on an existing account.
    SetupField,
    /// A Setup detail row's inline field on the `+ new` form, where esc keeps
    /// the typed value (`cancel_config_edit` only reverts a saved account).
    NewAccountField,
    /// A typed field on the Fallback member card: `rotate at`, `weekly at`
    /// or `max spend`.
    MemberField,
    /// The Fallback card's `preferred days` chip picker.
    DayPicker,
    /// The Config tab's `refresh` custom-value field.
    RefreshInterval,
    /// The Config tab's `context nudge` custom-value field.
    ContextNudge,
    /// The Config tab's `weekly limit` custom-value field.
    WeeklyThreshold,
    /// The Plugin tab's herdr tag-refresh field.
    HerdrTag,
    /// The usage tab's note editor (the `n`-opened draft on the selected
    /// account).
    NoteEditor,
}

impl KeyOwner {
    /// Whether this owner takes `code`, or lets it through to the global keys.
    /// The chip picker binds neither `?` nor `x`, so help and toast dismissal
    /// work mid-pick; a typed field takes both, since they are data there. A
    /// inert, so `?` (help) and `x` (dismiss a toast) fall through while `↑↓`
    /// and `q` (back) stay claimed and act in the editor's own handler (`q`
    /// closes the editor and ascends, so a locked editor never outlives the
    /// detail focus).
    pub(crate) fn claims(self, app: &App, code: KeyCode) -> bool {
        if self == Self::DayPicker && matches!(code, KeyCode::Char('?' | 'x')) {
            return false;
        }
        // `?` falls through from the re-bind confirm so the footer derives
        // `? help` and the key opens the help modal above it.
        if self == Self::Modal
            && matches!(app.modals.last(), Some(Modal::Rebind(_)))
            && matches!(code, KeyCode::Char('?'))
        {
            return false;
        }
        // `x` dismisses the topmost toast even while a modal owns input, so a
        // toast drawn over the modal is still dismissable; the typed-entry
        // modals keep `x` as data (name entry, the login code field).
        if self == Self::Modal
            && code == KeyCode::Char('x')
            && !app.toasts.is_empty()
            && !modal_types_x_as_data(app)
        {
            return false;
        }

        true
    }
}

/// Whether the top modal types `x` into a field (data) rather than letting it
/// dismiss a toast: the name-entry modals and the login modal's open code
/// field.
fn modal_types_x_as_data(app: &App) -> bool {
    match app.modals.last() {
        Some(Modal::CaptureName(_) | Modal::NamePrompt(_)) => true,
        Some(Modal::Login) => app.login.as_ref().is_some_and(|s| s.paste_field.is_some()),
        _ => false,
    }
}

/// What owns the keyboard this frame, or `None` when keys take their tab and
/// global senses. The one answer both [`handle_key`] routes by and the footer
/// draws from, so the two cannot disagree about who holds `←`/`→`.
pub(crate) fn keyboard_owner(app: &App) -> Option<KeyOwner> {
    if !app.modals.is_empty() {
        return Some(KeyOwner::Modal);
    }
    match app.tab {
        Tab::Setup => {
            let draft = app
                .config_draft
                .as_ref()
                .filter(|d| app.config_focus == ConfigFocus::Actions && d.active.is_some())?;
            Some(match draft.editing_name {
                Some(_) => KeyOwner::SetupField,
                None => KeyOwner::NewAccountField,
            })
        }
        Tab::Fallback if app.fallback_focus == FallbackFocus::Detail => {
            match app.fallback_edit.as_ref().map(|e| &e.state) {
                Some(CardEdit::Threshold(_) | CardEdit::Weekly(_) | CardEdit::MaxSpend(_)) => {
                    Some(KeyOwner::MemberField)
                }
                Some(CardEdit::Days(_)) => Some(KeyOwner::DayPicker),
                Some(CardEdit::ArmedRemove) | None => None,
            }
        }
        Tab::Config if app.refresh_interval_draft.is_some() => Some(KeyOwner::RefreshInterval),
        Tab::Config if app.context_nudge_draft.is_some() => Some(KeyOwner::ContextNudge),
        Tab::Config if app.weekly_threshold_draft.is_some() => Some(KeyOwner::WeeklyThreshold),
        Tab::Plugin if app.plugin.herdr_tag_draft.is_some() => Some(KeyOwner::HerdrTag),
        Tab::Usage if app.note_editor.is_some() => Some(KeyOwner::NoteEditor),
        Tab::Overview
        | Tab::Usage
        | Tab::Tokens
        | Tab::Fallback
        | Tab::Config
        | Tab::Status
        | Tab::Plugin => None,
    }
}

/// What the tab-switch keys do on the current screen, the one answer
/// [`handle_key`], the footer and the help modal all derive from so their
/// hints cannot drift from the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneState {
    /// Top level: `←`/`→`/`tab`/`⇧tab` all switch screens.
    Rest,
    /// A descended read-only detail: `←` ascends, `→` is inert, `tab`/`⇧tab`
    /// ascend AND switch.
    ReadOnly,
    /// A descended editable detail at rest: `←` ascends (the esc path),
    /// `→`/`tab`/`⇧tab` are inert.
    Editable,
}

pub(crate) fn pane_state(app: &App) -> PaneState {
    if app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions
        || app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail
        || app.tab == Tab::Plugin && app.plugin.focus == PluginFocus::Detail
    {
        PaneState::Editable
    } else if app.tab == Tab::Usage && app.usage_detail_focus == StatusFocus::Detail
        || app.tab == Tab::Tokens
            && app.token_view == TokenView::Models
            && app.model_detail_focus == StatusFocus::Detail
        || app.tab == Tab::Status && app.status.focus == StatusFocus::Detail
    {
        PaneState::ReadOnly
    } else {
        PaneState::Rest
    }
}

/// Back out of the current sub-focus, the `esc`/`←` ascend path. Mirrors the
/// `esc` chain exactly so `←` and `esc` can never diverge.
fn ascend_pane(app: &mut App) {
    if app.tab == Tab::Usage && app.usage_detail_focus == StatusFocus::Detail {
        app.usage_detail_focus = StatusFocus::List;
    } else if app.tab == Tab::Tokens
        && app.token_view == TokenView::Models
        && app.model_detail_focus == StatusFocus::Detail
    {
        app.model_detail_focus = StatusFocus::List;
    } else if app.tab == Tab::Setup && app.config_focus == ConfigFocus::Actions {
        leave_config_detail(app);
    } else if app.tab == Tab::Fallback && app.fallback_focus == FallbackFocus::Detail {
        leave_fallback_detail(app);
    } else if app.tab == Tab::Status && app.status.focus == StatusFocus::Detail {
        app.status.focus = StatusFocus::List;
    } else if app.tab == Tab::Plugin && app.plugin.focus == PluginFocus::Detail {
        app.plugin.focus = PluginFocus::List;
    } else if app.tab == Tab::Tokens && app.token_view == TokenView::Models {
        app.token_view = TokenView::Dashboard;
    }
}

/// The direction a `Tab`/`BackTab` keypress names, or `None` when a reported
/// CONTROL or ALT makes it inert. A bare `Tab` and a `Tab` with SHIFT
/// (the kitty form of shift+tab) are distinguished; `BackTab` switches back
/// with or without its shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabStep {
    Next,
    Prev,
}

fn tab_switch_direction(key: KeyEvent) -> Option<TabStep> {
    // `tab` switches with no modifier besides shift: any other
    // reported modifier (ctrl, alt, super, hyper, meta) makes it inert.
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Some(TabStep::Prev),
        KeyCode::Tab => Some(TabStep::Next),
        KeyCode::BackTab => Some(TabStep::Prev),
        _ => None,
    }
}

/// The tab an `⌥1`–`⌥9` keypress jumps to: `⌥1`–`⌥8` positional, `⌥9` the last
/// tab, `⌥0` unbound. Only `ALT` may be present.
fn alt_jump_target(key: KeyEvent) -> Option<Tab> {
    // `⌥digit` requires ALT alone: any other reported modifier
    // (shift, ctrl, super, hyper, meta) makes it inert.
    if key.modifiers != KeyModifiers::ALT {
        return None;
    }
    match key.code {
        KeyCode::Char(d @ '1'..='9') => {
            let n = d.to_digit(10)? as usize;
            Some(if n == 9 {
                Tab::ALL[Tab::ALL.len() - 1]
            } else {
                Tab::ALL[n - 1]
            })
        }
        _ => None,
    }
}

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    // `ctrl+c` arms or completes its own 2-step quit, ahead of every owner so a
    // modal, an editor or an ⌥-jump never sees the key.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if app.armed_quit == Some(QuitKey::CtrlC) {
            // A second ⌃c over an open switch warning retargets it to the quit
            // form; over a quit warning it does nothing more; elsewhere it
            // quits, or opens the quit warning over changed work.
            let warning_open = matches!(app.modals.last(), Some(Modal::LossWarning(_)));
            if let Some(Modal::LossWarning(state)) = app.modals.last_mut()
                && state.target != LossWarningTarget::Quit
            {
                state.target = LossWarningTarget::Quit;
            }
            app.disarm_quit();
            if !warning_open {
                request_quit(app);
            }
        } else {
            app.armed_quit = Some(QuitKey::CtrlC);
            app.footer_alert = Some(FooterAlert::Warn(format!(
                "press {} again to quit",
                prose::key("\u{2303}c")
            )));
        }
        return;
    }

    let owner = keyboard_owner(app);

    // Every key but a top-level `q` (which reads the arm below) disarms the
    // quit before it does its own job — `x` included, so a toast-pop or alert
    // dismiss never reads the arm; owners included, so a key the modal or
    // editor swallows still disarms.
    let reads_arm = matches!(
        key.code,
        KeyCode::Char('q')
            if !owner.is_some_and(|o| o.claims(app, KeyCode::Char('q')))
                && app.login.is_none()
                && app.codex_login.is_none()
    );

    // The quit is decided and the exit waits for the running writes:
    // only a top-level `q` gets past here (`⌃c` is handled above), everything
    // else only disarms an armed quit — an edit typed now would die with the exit.
    if app.exit_wait && !(reads_arm && !has_sub_focus(app)) {
        app.disarm_quit();
        return;
    }
    if !reads_arm {
        app.disarm_quit();
    }

    // `⌥1`–`⌥9` jump, live inside every editor but inert while a modal owns the
    // keyboard — so it runs before any non-modal owner claims the key.
    if app.modals.is_empty()
        && let Some(target) = alt_jump_target(key)
    {
        request_switch(app, target);
        return;
    }

    // A modal, then an open editor, owns the keyboard: typed digits and
    // letters can't trip global shortcuts, and ←/→ never switch the tab. A key
    // the owner does not claim falls through to the global senses below.
    if let Some(owner) = owner.filter(|owner| owner.claims(app, key.code)) {
        match owner {
            KeyOwner::Modal => handle_modal_key(app, key),
            KeyOwner::SetupField | KeyOwner::NewAccountField => handle_config_edit_key(app, key),
            KeyOwner::MemberField => handle_member_field_key(app, key),
            KeyOwner::DayPicker => handle_day_picker_key(app, key),
            KeyOwner::RefreshInterval => handle_refresh_interval_edit_key(app, key),
            KeyOwner::ContextNudge => handle_context_nudge_edit_key(app, key),
            KeyOwner::WeeklyThreshold => handle_weekly_threshold_edit_key(app, key),
            KeyOwner::HerdrTag => handle_herdr_tag_edit_key(app, key),
            KeyOwner::NoteEditor => handle_note_editor_key(app, key),
        }
        return;
    }

    // Esc/q abandon an in-flight login (the detached worker's result is
    // discarded by the generation guard). Catching q here keeps it symmetric
    // with esc — otherwise q would ascend out of the Setup form (orphaning the
    // draft that receives the mint) or silently arm the 2-step quit under a
    // footer whose login line never yields to the armed alert. Sits BELOW the
    // editor captures so typing a literal `q` into a field can't cancel.
    if app.login.is_some() && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        // BEFORE the commit point (no paste sent) cancels the add (the guard
        // releases once the worker reports, R3-3); a cancel AFTER the paste is
        // sent only closes the modal and the login line — the door, the guard
        // and the committed slot stay, and the complete runs to its result.
        cancel_open_login(app, true);
        app.toast(ToastKind::Info, "login canceled");
        return;
    }

    // The codex re-login's twin: a collapsed login cancels on esc/q the same
    // way — the worker's listener wait stops (its ports free) and `q` never
    // arms the 2-step quit around a live login.
    if app.codex_login.is_some() && matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        cancel_codex_login(app);
        return;
    }

    match key.code {
        KeyCode::Left => {
            if pane_state(app) == PaneState::Rest {
                request_switch(app, app.tab.prev());
            } else {
                ascend_pane(app);
            }
            return;
        }
        KeyCode::Right => {
            // Inert in every descended pane; switches only at rest.
            if pane_state(app) == PaneState::Rest {
                request_switch(app, app.tab.next());
            }
            return;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            // A ctrl/alt variant is inert; a descended editable pane
            // traps the keys entirely.
            if let Some(step) = tab_switch_direction(key)
                && pane_state(app) != PaneState::Editable
            {
                let target = match step {
                    TabStep::Next => app.tab.next(),
                    TabStep::Prev => app.tab.prev(),
                };
                request_switch(app, target);
            }
            return;
        }
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.open_modal(Modal::Help);
            return;
        }
        KeyCode::Char('d') => {
            // Open the divergence resolver from the banner (no-op when the
            // live link is clean).
            if let Some(notice) = app.divergence_pending.clone() {
                open_divergence_modal(app, &notice.active);
            }
            return;
        }
        // Overview only: it is the one screen listing accounts, so the filter
        // has nothing to mean anywhere else. Guarded rather than swallowed so
        // `c` still reaches the per-tab dispatch (Tokens binds it too).
        KeyCode::Char('c') if HARNESS_FILTER_KEY && app.tab == Tab::Overview => {
            // `c` changes the filter, then places the cursor on the remembered
            // item's row when the new view shows it, else row 0; it never
            // writes the remembered item (a filter only hides it).
            app.harness_filter = app.harness_filter.next();
            app.place_overview_cursor();
            app.overview_selector.offset.set(0);
            return;
        }
        // Overview only, so the Usage tab's `e` (estimates toggle) keeps its
        // sense. No row under the cursor (an empty roster or a filter hiding
        // every row) is a no-op.
        KeyCode::Char('e') if app.tab == Tab::Overview => {
            if let Some(item) = app.current_main_item() {
                start_edit_selected(app, item);
            }
            return;
        }
        KeyCode::Char('a') => {
            if app.overview_selected_extra().is_some() && app.tab == Tab::Overview {
                refuse_read_only(app);
            }
            let state = build_action_menu(app);
            if !state.items.is_empty() {
                app.open_modal(Modal::ActionMenu(state));
            }
            return;
        }
        KeyCode::Char('r') => {
            if app.tab == Tab::Status {
                trigger_status_refresh(app);
                return;
            }
            // Service checks re-run synchronously; `r` also re-probes `claude --version`.
            if app.tab == Tab::Plugin {
                recompute_plugin_checks(app, true);
                app.toast(ToastKind::Info, "re-running service checks");
                return;
            }
            if app.tab == Tab::Tokens {
                reload_token_stats(app);
                return;
            }
            if app.tab == Tab::Usage {
                usage_refresh(app);
            } else {
                app.manual_refresh();
                app.toast(ToastKind::Info, "refreshing every account");
            }
            return;
        }
        KeyCode::Char('t') => {
            // Tokens claims `t` for its period lens (as `r` reloads there);
            // rotate-all stays reachable from every other tab.
            if app.tab == Tab::Tokens {
                set_token_period(app, app.token_period.next());
                return;
            }
            app.open_modal(Modal::Confirm(ConfirmState {
                message: ROTATE_ALL_MSG.to_string(),
                detail: Some(ROTATE_ALL_DETAIL.to_string()),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::RotateAll,
            }));
            return;
        }
        KeyCode::Char('n') => {
            // The usage tab claims `n` for the account's note editor, on a
            // roster keeps the empty state's `n to create one` promise.
            // Everywhere else: new account.
            if app.tab == Tab::Usage {
                match app.usage_selection() {
                    SetupSelection::Item(MainItemKind::Profile(_)) if app.profile_count() > 0 => {
                        open_note_editor(app);
                    }
                    _ => start_new_account(app),
                }
            } else {
                start_new_account(app);
            }
            return;
        }
        // Esc backs out of sub-focus; no-op at the top level.
        KeyCode::Esc => {
            ascend_pane(app);
            // At top level, Esc is a no-op — `⌃c ⌃c` or `q q` to quit.
            return;
        }
        // First `q` at the top level arms the 2-step quit; second confirms.
        // When there is a sub-focus to back out of, `q` ascends instead.
        KeyCode::Char('q') => {
            if has_sub_focus(app) {
                app.disarm_quit();
                if app.tab == Tab::Usage {
                    app.usage_detail_focus = StatusFocus::List;
                } else if app.tab == Tab::Tokens && app.model_detail_focus == StatusFocus::Detail {
                    app.model_detail_focus = StatusFocus::List;
                } else if app.tab == Tab::Setup {
                    leave_config_detail(app);
                } else if app.tab == Tab::Fallback {
                    leave_fallback_detail(app);
                } else if app.tab == Tab::Tokens {
                    app.token_view = TokenView::Dashboard;
                } else if app.tab == Tab::Plugin {
                    app.plugin.focus = PluginFocus::List;
                } else {
                    app.status.focus = StatusFocus::List;
                }
            } else if app.armed_quit == Some(QuitKey::Q) {
                request_quit(app);
            } else {
                // Armed by ctrl+c, or not armed: q runs its own job and arms
                // (or re-arms) its own quit.
                app.armed_quit = Some(QuitKey::Q);
                app.footer_alert = Some(FooterAlert::Warn(format!(
                    "press {} again to quit",
                    prose::key("q")
                )));
            }
            return;
        }
        KeyCode::Char('x') => {
            // Dismissal precedence: the topmost toast pops first. The quit arm
            // (and its alert) already disarmed at the gate above.
            if !app.toasts.is_empty() {
                app.toasts.pop_front();
            }
            return;
        }
        _ => {}
    }

    match app.tab {
        Tab::Overview => handle_overview_key(app, key),
        Tab::Usage => handle_usage_key(app, key),
        Tab::Tokens => handle_tokens_key(app, key),
        Tab::Setup => handle_config_key(app, key),
        Tab::Fallback => handle_fallback_key(app, key),
        Tab::Config => handle_global_config_key(app, key),
        Tab::Status => handle_status_key(app, key),
        Tab::Plugin => handle_plugin_key(app, key),
    }
}

/// The Tokens model rows for the active period lens — filtered and ranked
/// exactly as rendered, the single source for both the render and the cursor
/// math. Lifetime keeps the grouped ("others"-folded) rows; scoped periods
/// list raw per-model aggregates (a period's list is short, no tail fold).
pub(crate) fn token_period_models(app: &App) -> Vec<crate::tokens::PeriodModel> {
    use crate::tokens::PeriodModel;
    let Some(stats) = app.token_stats.as_ref() else {
        return Vec::new();
    };
    let mut rows: Vec<PeriodModel> = if let Some(bucket) = app.token_period.bucket() {
        let (from, to) = crate::tokens::current_bucket_bounds(&crate::tokens::today_date(), bucket);
        crate::tokens::period_models(&stats.daily_models, &from, &to)
    } else if app.token_period == TokenPeriod::Daily {
        stats
            .today
            .as_ref()
            .map(|t| {
                // Daily-lens rows carry today's per-hour buckets (unlike
                // lifetime rows, whose aggregate holds no dates), so the
                // top-models and detail views price the same hourly rates
                // the today card does.
                use crate::pricing::HourTokens;
                let mut hours: HashMap<&str, [HourTokens; 24]> = t
                    .model_hours
                    .iter()
                    .map(|m| (m.model.as_str(), m.hours))
                    .collect();
                t.models
                    .iter()
                    .map(|m| PeriodModel {
                        model: m.model.clone(),
                        in_out: m.in_out(),
                        split: m.clone(),
                        split_complete: true,
                        days: vec![crate::tokens::PeriodDay {
                            date: t.date.clone(),
                            split: m.clone(),
                            hours: hours.remove(m.model.as_str()),
                        }],
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        crate::tokens::group_models(&stats.models)
            .iter()
            .map(PeriodModel::from_full)
            .collect()
    };
    rows.retain(|m| app.token_filter.matches(&m.model));
    let basis = crate::tokens::effective_cache_basis(&rows, app.config().state.count_cache);
    rows.sort_unstable_by_key(|m| std::cmp::Reverse(m.metric(basis)));
    rows
}

/// Number of rows in the Tokens `Models` master list under the active
/// period + filter lenses.
pub(crate) fn token_model_count(app: &App) -> usize {
    token_period_models(app).len()
}

/// Keep detail focus on a model only while the active list can select one.
fn clamp_token_model_cursor(app: &mut App) {
    let len = token_model_count(app);
    if len == 0 {
        app.model_detail_focus = StatusFocus::List;
        app.model_detail_scroll = 0;
    }
    if app.token_model_cursor >= len {
        app.token_model_cursor = len.saturating_sub(1);
    }
}

/// Tokens `r` / action-menu reload: re-read the on-disk stats + recent
/// transcripts and refetch model prices for the cost lens.
fn reload_token_stats(app: &mut App) {
    let _ = app.tokens_refresh.send(());
    let _ = app.pricing_refresh.send(());
    // A user-triggered reload is a foreground refresh — light the loading
    // spinners until the loader's next `Loaded`/`Failed` clears them.
    app.tokens_topping_up = true;
    app.tokens_progress = None;
    app.toast(ToastKind::Info, "reloading token usage");
}

/// Tokens tab: `c` toggles cache-counting on either view; the Dashboard moves
/// card focus with ↑/↓ (wrapping) and the page keys (stopping at the ends), and
/// ⏎ descends into Models. From Models, ⏎ descends into the read-only detail.
fn handle_tokens_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('c') {
        toggle_count_cache(app);
        return;
    }
    match app.token_view {
        TokenView::Dashboard => match key.code {
            KeyCode::Enter if token_model_count(app) > 0 => {
                app.token_view = TokenView::Models;
                app.model_detail_focus = StatusFocus::List;
                app.model_detail_scroll = 0;
                app.token_model_cursor = 0;
            }
            KeyCode::Up => dash_move_focus(app, DashDir::Prev),
            KeyCode::Down => dash_move_focus(app, DashDir::Next),
            KeyCode::PageUp => dash_move_focus(app, DashDir::PageUp),
            KeyCode::PageDown => dash_move_focus(app, DashDir::PageDown),
            _ => {}
        },
        TokenView::Models => {
            if app.model_detail_focus == StatusFocus::Detail {
                let max = app.model_detail_max_scroll.get();
                let viewport = app.model_detail_viewport.get();
                match key.code {
                    KeyCode::Up => {
                        app.model_detail_scroll = app.model_detail_scroll.min(max).saturating_sub(1)
                    }
                    KeyCode::Down => {
                        app.model_detail_scroll =
                            app.model_detail_scroll.min(max).saturating_add(1).min(max)
                    }
                    KeyCode::PageUp => {
                        app.model_detail_scroll = page_up_scroll(
                            app.model_detail_scroll as usize,
                            max as usize,
                            viewport as usize,
                        ) as u16
                    }
                    KeyCode::PageDown => {
                        app.model_detail_scroll = page_down_scroll(
                            app.model_detail_scroll as usize,
                            max as usize,
                            viewport as usize,
                        ) as u16
                    }
                    _ => {}
                }
            } else {
                let len = token_model_count(app);
                match key.code {
                    KeyCode::Up if len > 0 => {
                        app.token_model_cursor = (app.token_model_cursor + len - 1).rem_euclid(len);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::Down if len > 0 => {
                        app.token_model_cursor = (app.token_model_cursor + 1).rem_euclid(len);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::PageUp if len > 0 => {
                        app.token_model_cursor =
                            selector_page_up(app.token_model_cursor, &app.models_selector);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::PageDown if len > 0 => {
                        app.token_model_cursor =
                            selector_page_down(app.token_model_cursor, &app.models_selector);
                        app.model_detail_scroll = 0;
                    }
                    KeyCode::Enter if len > 0 => {
                        app.model_detail_focus = StatusFocus::Detail;
                        app.model_detail_scroll = 0;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Direction a dashboard key moves card focus.
enum DashDir {
    Prev,
    Next,
    PageUp,
    PageDown,
}

/// Move dashboard card focus one step (or one page) and reconcile the
/// first-visible card, both through [`follow_dash_focus`].
fn dash_move_focus(app: &mut App, dir: DashDir) {
    let (new_focus, new_first) = {
        let order = app.token_dash_order.get();
        let rows = app.token_dash_rows.borrow();
        if rows.len() != 7 {
            return;
        }
        let viewport = app.token_dash_viewport.get();
        let Some(pos) = order.iter().position(|&k| k == app.token_dash_focus) else {
            return;
        };
        let starts: Vec<usize> = rows.iter().map(|&(top, _)| top as usize).collect();
        let next = match dir {
            DashDir::Prev => (pos + 6) % 7,
            DashDir::Next => (pos + 1) % 7,
            DashDir::PageUp => page_land_up(pos, &starts, viewport as usize),
            DashDir::PageDown => page_land_down(pos, &starts, viewport as usize),
        }
        .min(6);
        let focus = order[next];
        let first = follow_dash_focus(&order, &rows, viewport, focus, app.token_dash_first.get());
        (focus, first)
    };
    app.token_dash_focus = new_focus;
    app.token_dash_first.set(new_first);
}

/// Reconcile the persisted first-visible card against the laid-out geometry and
/// the focused card: it moves only when focus leaves the view. Pure — the key
/// path and the render pass both call it.
pub(crate) fn follow_dash_focus(
    order: &[CardKind; 7],
    rows: &[(u16, u16)],
    viewport: u16,
    focus: CardKind,
    first: usize,
) -> usize {
    if rows.is_empty() || viewport == 0 {
        return 0;
    }
    let last = rows.len() - 1;
    if rows[last].1 <= rows[0].0 + viewport {
        return 0; // grid fits: no scroll
    }
    let Some(mut focus_idx) = order.iter().position(|&k| k == focus) else {
        return first.min(last);
    };
    focus_idx = focus_idx.min(last);
    let mut first = dash_row_start(rows, first.min(last));
    let view_top = rows[first].0;
    let view_bottom = view_top + viewport;
    let (f_top, f_bottom) = rows[focus_idx];
    if f_top < view_top {
        first = dash_row_start(rows, focus_idx);
    } else if f_bottom > view_bottom {
        // Scroll down the minimum that reveals the focused card's bottom.
        let want = f_bottom.saturating_sub(viewport);
        let k = (0..=focus_idx)
            .find(|&k| rows[k].0 >= want)
            .unwrap_or(focus_idx);
        first = dash_row_start(rows, k);
    }
    first
}

/// Clamp the first-visible card after a resize: the view never starts past the
/// last card top that still fills the body (no blank tail while cards above
/// stay hidden), bounded so the focused card's bottom stays in view. Render-only.
pub(crate) fn clamp_dash_first(
    order: &[CardKind; 7],
    rows: &[(u16, u16)],
    viewport: u16,
    focus: CardKind,
    first: usize,
) -> usize {
    if rows.is_empty() || viewport == 0 {
        return 0;
    }
    let last = rows.len() - 1;
    if rows[last].1 <= rows[0].0 + viewport {
        return 0; // grid fits
    }
    let Some(mut focus_idx) = order.iter().position(|&k| k == focus) else {
        return first.min(last);
    };
    focus_idx = focus_idx.min(last);
    let first = dash_row_start(rows, first.min(last));
    if rows[last].1 >= rows[first].0 + viewport {
        return first; // already fills the body
    }
    let max_top = rows[last].1 - viewport;
    let focus_floor = rows[focus_idx].1.saturating_sub(viewport);
    let mut k = first;
    while k > 0 && rows[k].0 > max_top {
        k -= 1;
    }
    if rows[k].0 < focus_floor {
        while k < last && rows[k].0 < focus_floor {
            k += 1;
        }
    }
    dash_row_start(rows, k)
}

/// The first card of `i`'s visual row (side-by-side cards share a top).
fn dash_row_start(rows: &[(u16, u16)], mut i: usize) -> usize {
    while i > 0 && rows[i - 1].0 == rows[i].0 {
        i -= 1;
    }
    i
}

/// Every changed edit and staged pick, in `Tab::ALL` order. The single source
/// the switch, quit, retarget, `save` validation and the body's screen list
/// all read, so a new editor can never warn on a switch and not on a quit. An
/// edit is changed when its buffer differs from the value it opened from; a
/// staged pick when the staged value differs from the saved one. Clean edits
/// never appear here and suspend silently.
fn changed_work(app: &App) -> Vec<Dirty> {
    let mut out = Vec::new();
    // Usage: the note editor (Tab::ALL puts Usage before Setup).
    if app.note_editor.as_ref().is_some_and(note_dirty) {
        out.push(Dirty::Note);
    }
    // Setup: an open existing-account field, then the `+ new` form (typed work,
    // open or suspended, a field open or not).
    if let Some(d) = app.config_draft.as_ref()
        && d.editing_name.is_some()
        && let Some(field) = d.active
        && d.field(field)
            .is_some_and(|input| Some(input.value.as_str()) != d.field_seed.as_deref())
    {
        out.push(Dirty::SetupField);
    }
    let new_form_dirty = app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.editing_name.is_none() && d.holds_typed_work())
        || app
            .new_account_draft
            .as_ref()
            .is_some_and(ConfigDraft::holds_typed_work);
    if new_form_dirty {
        out.push(Dirty::NewAccountForm);
    }
    // Fallback: the member-card typed field. The day picker saves each toggle
    // and the custom stop is UI state dropped with the card, so neither is
    // changed work.
    if fallback_field_dirty(app) {
        out.push(Dirty::FallbackField);
    }
    if app
        .refresh_interval_draft
        .as_ref()
        .is_some_and(SeededInput::dirty)
    {
        out.push(Dirty::RefreshInterval);
    }
    if app
        .context_nudge_draft
        .as_ref()
        .is_some_and(SeededInput::dirty)
    {
        out.push(Dirty::ContextNudge);
    }
    if app
        .weekly_threshold_draft
        .as_ref()
        .is_some_and(SeededInput::dirty)
    {
        out.push(Dirty::WeeklyThreshold);
    }
    if app
        .plugin
        .herdr_tag_draft
        .as_ref()
        .is_some_and(SeededInput::dirty)
    {
        out.push(Dirty::HerdrTag);
    }
    out
}

/// The seed `begin_refresh_interval_edit` opens the field with, in the atomic's
/// whole seconds — the value the editor opened from.
fn refresh_interval_seed(app: &App) -> String {
    (app.refresh_interval.load(Ordering::Relaxed) / 1000).to_string()
}

/// The seed `begin_context_nudge_edit` opens the field with.
fn context_nudge_seed(current: Option<u64>) -> String {
    match current {
        None => String::from("300k"),
        Some(v) if v.is_multiple_of(1_000_000) => format!("{}k", v / 1000),
        Some(v) => format_threshold_tokens(v),
    }
}

/// Whether the Fallback member-card's open typed field differs from the value
/// it opened from (its stored seed). The day picker saves each toggle, so it
/// never differs.
fn fallback_field_dirty(app: &App) -> bool {
    let Some(edit) = app.fallback_edit.as_ref() else {
        return false;
    };
    match &edit.state {
        CardEdit::Threshold(input) | CardEdit::Weekly(input) | CardEdit::MaxSpend(input) => {
            edit.seed.as_deref() != Some(input.value.as_str())
        }
        CardEdit::Days(_) | CardEdit::ArmedRemove => false,
    }
}

/// Whether the note editor's draft differs from the note it opened from.
fn note_dirty(editor: &NoteEditor) -> bool {
    editor.buf.text() != editor.seed
}

/// The changed work a SWITCH away from `screen` warns over: only what sits on
/// the screen being left. A quit keeps the whole set ([`changed_work`]).
fn changed_work_on(app: &App, screen: Tab) -> Vec<Dirty> {
    changed_work(app)
        .into_iter()
        .filter(|d| d.screen() == screen)
        .collect()
}

fn in_flight_exit_write(app: &App) -> bool {
    app.deleting.is_some() || token_spend_in_flight(&app.activity)
}

/// A switch the user asked for: disarm it, warn over changed work, hold on an
/// active tab) does nothing.
fn request_switch(app: &mut App, target: Tab) {
    if app.tab == target {
        return;
    }
    disarm_switch(app);
    let dirty = changed_work_on(app, app.tab);
    if !dirty.is_empty() {
        open_loss_warning(app, LossWarningTarget::Switch(target), dirty);
        return;
    }

    do_switch(app, target);
}

/// A quit the user asked for (a second `⌃c` or `q` at the top level): warn
/// over changed work wherever it sits, hold on an in-flight field or `+ add`
/// save, else quit or enter the exit wait. A quit pressed while a switch or
/// jump is held turns that held target into a held quit (the landing quits,
/// never switches). A second quit during the exit wait, or while a quit is
/// held on a checking save, exits at once.
fn request_quit(app: &mut App) {
    app.disarm_quit();
    if app.exit_wait {
        // A second quit during the exit wait exits at once, whatever still runs.
        app.quit = true;
        app.shutting_down.store(true, Ordering::SeqCst);
        return;
    }

    let dirty = changed_work(app);
    if !dirty.is_empty() {
        open_loss_warning(app, LossWarningTarget::Quit, dirty);
        return;
    }
    quit_or_wait(app);
}

fn quit_or_wait(app: &mut App) {
    if in_flight_exit_write(app) {
        app.exit_wait = true;
        // Stop the scheduler and refuse new spends while the exit waits: the
        // wait resolves when the running write lands, and a spend that starts
        // during it would either extend it or die mid-exchange after the tick
        // that decided the quit.
        app.shutting_down.store(true, Ordering::SeqCst);
    } else {
        app.quit = true;
        app.shutting_down.store(true, Ordering::SeqCst);
    }
}

/// Open the loss warning over `dirty`. `save` is the default focus when any
/// item can save; `go back` is, for the two-button `+ new` form.
fn open_loss_warning(app: &mut App, target: LossWarningTarget, dirty: Vec<Dirty>) {
    let focus = if dirty.iter().any(|d| d.saveable()) {
        LossWarningButton::Save
    } else {
        LossWarningButton::GoBack
    };
    app.open_modal(Modal::LossWarning(LossWarningState {
        focus,
        target,
        drop: LossWarningDrop { dirty },
    }));
}

/// Perform a switch: suspend the pane it leaves, then land.
fn do_switch(app: &mut App, target: Tab) {
    suspend_source(app);
    switch_tab(app, target);
}

/// Disarm everything a switch disarms at the switch itself: the
/// 2-step quit and Fallback's armed remove. Setup's armed action is not set
/// here: an armed Setup draft always rests with no field open, so the switch
/// drops it whole through [`leave_config_detail`], and the `+ new` form has no
/// armable row.
fn disarm_switch(app: &mut App) {
    app.disarm_quit();
    if matches!(
        app.fallback_edit.as_ref().map(|e| &e.state),
        Some(CardEdit::ArmedRemove)
    ) {
        app.fallback_edit = None;
    }
}

/// The one writer of `new_account_draft`. A suspended `+ new` form holding
/// typed work is never overwritten: the incoming draft is handed back to the
/// caller, which keeps it, so no slot write can drop a form and no UI path
/// panics. `Some(draft)` = refused and returned; `None` = stored. A switch or
/// jump over a typed form warns instead, and the preset-picker cancel restores
/// a clean one; the detail descend moves whatever the suspended form holds, and
/// keeps it on its pane when this writer refuses.
fn set_new_account_draft(app: &mut App, draft: ConfigDraft) -> Option<ConfigDraft> {
    if app
        .new_account_draft
        .as_ref()
        .is_some_and(ConfigDraft::holds_typed_work)
    {
        return Some(draft);
    }
    app.new_account_draft = Some(draft);
    None
}

/// Suspend the pane the switch leaves: an open edit stays (only
/// reachable by `⌥digit`, since editors own the arrows and tab), everything
/// else ascends to its selector, keeping unsaved state.
fn suspend_source(app: &mut App) {
    let edit_open = keyboard_owner(app).is_some();
    match app.tab {
        Tab::Usage => {
            // The note editor keeps the Usage descent; a bare detail
            // ascends.
            if !edit_open {
                app.usage_detail_focus = StatusFocus::List;
            }
        }
        Tab::Tokens => app.model_detail_focus = StatusFocus::List,
        Tab::Setup if app.config_focus == ConfigFocus::Actions => {
            if edit_open {
                // Keep the descent and the edit; returning lands back in it.
            } else if app
                .config_draft
                .as_ref()
                .is_some_and(|d| d.editing_name.is_none())
            {
                // The `+ new` form's typed values survive in its own home.
                let Some(draft) = app.config_draft.take() else {
                    return;
                };
                match set_new_account_draft(app, draft) {
                    None => app.config_focus = ConfigFocus::Profiles,
                    // The slot already holds a typed form: put this form back
                    // and keep the descent, so the pane stays on its form and
                    // nothing is dropped.
                    Some(draft) => app.config_draft = Some(draft),
                }
            } else {
                leave_config_detail(app);
            }
        }
        Tab::Fallback if app.fallback_focus == FallbackFocus::Detail => {
            if !edit_open {
                leave_fallback_detail(app);
            }
        }
        Tab::Status => app.status.focus = StatusFocus::List,
        Tab::Plugin
            if app.plugin.focus == PluginFocus::Detail
            // drafts, which a switch must not drop.
            && !edit_open =>
        {
            app.plugin.focus = PluginFocus::List;
        }
        _ => {}
    }
}

/// Switch the active tab and reset that tab's cursor to a sensible default.
fn switch_tab(app: &mut App, tab: Tab) {
    app.tab = tab;
    app.tab_activity[tab.index()] = None;
    // Leaving the Plugin tab cancels a still-pending herdr landing: the user
    // has moved on, so a later recompute must not yank the cursor back.
    // Entering the Overview with nothing remembered seeds the claude row
    // (whose index is `profile_cursor` itself) before the placement runs.
    if tab == Tab::Overview && app.overview_remembered.is_none() {
        app.overview_remembered = Some(MainItemKind::Profile(app.profile_cursor.read()));
    }
    // Clamp cursor: a Config `+ new` selection must land on a real account.
    app.clamp_profile_cursor();
    match tab {
        Tab::Overview => {}
        Tab::Usage => {
            if keyboard_owner(app).is_some() {
                // A suspended note editor returns under its own account:
                // pin the selector to the editor's profile and reload
                // the note, so the header and the editor never disagree about
                // whose note is open. A deleted account closes the editor and
                // ascends, as a deleted account's Setup edit does.
                let profile = app.note_editor.as_ref().map(|e| e.profile.clone());
                if let Some(name) = profile {
                    let idx = app.config().profiles.iter().position(|p| p.name == name);
                    match idx {
                        Some(i) => {
                            app.profile_cursor.set(i);
                            app.reload_selected_note();
                        }
                        None => {
                            // The account is gone (deleted, or renamed by
                            // another process): close the editor so no
                            // invisible editor owns the keys. Keeping a typed
                            // editor open under its old name left it drawn by
                            // nobody while it still captured every key.
                            app.note_editor = None;
                            app.usage_detail_focus = StatusFocus::List;
                        }
                    }
                }
            } else {
                app.usage_detail_focus = StatusFocus::List;
            }
            app.usage_detail_scroll = 0;
            // The codex or pool snapshot may have changed while Usage was
            // hidden; place the selection on entry, before the first frame and
            // the first key — the same rule Setup follows.
            app.rehome_usage_selection();
        }
        Tab::Tokens => {
            // Land on the dashboard; keep the model cursor reset for descend.
            app.token_view = TokenView::Dashboard;
            app.model_detail_focus = StatusFocus::List;
            app.model_detail_scroll = 0;
            app.token_model_cursor = 0;
        }
        Tab::Setup => {
            if keyboard_owner(app).is_some() {
                // A suspended open edit returns into it with the selector pinned
                // to the edit's own subject: an existing account's slot
                // or the `+ new` row. A deleted account closes the edit through
                // `leave_config_detail` rather than resurrecting it.
                match app
                    .config_draft
                    .as_ref()
                    .and_then(|d| d.editing_name.clone())
                {
                    Some(name) => {
                        let name = ProfileName::from(name.as_str());
                        let idx = app.config().profiles.iter().position(|p| p.name == name);
                        match idx {
                            Some(i) => {
                                app.setup.item = None;
                                app.profile_cursor.set(i);
                            }
                            None => leave_config_detail(app),
                        }
                    }
                    None => {
                        app.setup.item = None;
                        app.profile_cursor.set(app.profile_count());
                    }
                }
            } else {
                app.config_focus = ConfigFocus::Profiles;
                app.config_action_cursor = 0;
                disarm_codex_2step(app);
                // A suspended `+ new` form re-homes the selector onto its row.
                // Skipped while a suspended edit owns the selector.
                if app.new_account_draft.is_some() {
                    app.setup.item = None;
                    app.profile_cursor.set(app.profile_count());
                    app.reload_selected_note();
                } else {
                    // Fresh entry: land on the shared claude row (a remembered
                    // codex or pool selection is its own item, kept across a
                    // switch back).
                }
            }
            // The `+ new` form's capture row renders off this flag, and a
            // login may have been minted (or saved) elsewhere since the last
            // Setup visit.
            app.refresh_unsaved_live_login();
            // The codex or pool snapshot may have changed while Setup was
            // hidden; place the selection on entry, before the first frame and
            // the first key — the Overview's precedent (`place_overview_cursor`
            // runs in `switch_tab`), never one tick late on `on_tick`.
            app.rehome_setup_selection();
        }
        Tab::Fallback => {
            if keyboard_owner(app).is_some() {
                // A suspended edit returns into it, pinned to the edit's member
                // (`repin_member_edit` also closes the edit once the member left
                // the chain, as a deleted account's Setup edit does).
                repin_member_edit(app);
                sync_profile_from_chain(app);
            } else {
                app.chain_cursor = chain_cursor_for_profile(app);
                sync_profile_from_chain(app);
                leave_fallback_detail(app);
            }
        }
        Tab::Config => {
            // A suspended editor returns into it; a fresh entry re-homes the
            // cursor. The drafts themselves clear only on their own commit /
            // cancel — an open editor is never stale here.
            if keyboard_owner(app).is_none() {
                app.global_config_cursor = 0;
            }
        }
        Tab::Plugin => {
            recompute_plugin_checks(app, false);
        }
        Tab::Status => {
            // Keep the incident cursor; reset focus to the list per the contract.
            app.status.focus = StatusFocus::List;
        }
    }
    reseed_edits(app);
}

/// Re-seed every suspended open editor from its current stored value on
/// landing, but only the CLEAN ones (buffer == seed): a clean edit whose
/// stored value changed while it was suspended (an out-of-band reload) returns
/// holding the new value, caret at the end, so it never returns changed; a
/// dirty edit keeps its typed text and stays dirty whatever the store did.
fn reseed_edits(app: &mut App) {
    // Setup existing-account field.
    if let Some((name, field)) = app
        .config_draft
        .as_ref()
        .and_then(|d| Some((d.editing_name.clone()?, d.active?)))
    {
        let name = ProfileName::from(name.as_str());
        let saved = {
            let cfg = app.config();
            row_committed_value(cfg.find(&name), &name, field)
        };
        if let Some(d) = app.config_draft.as_mut() {
            let clean = d
                .field(field)
                .is_some_and(|input| d.field_seed.as_deref() == Some(input.value.as_str()));
            if clean && d.field(field).is_some_and(|input| input.value != saved) {
                if let Some(input) = d.field_mut(field) {
                    *input = InputState::new(&saved);
                }
                d.field_seed = Some(saved);
            }
        }
    }
    // Fallback member field.
    let fallback_saved: Option<String> = app.fallback_edit.as_ref().and_then(|edit| {
        let cfg = app.config();
        let profile = cfg.find(&edit.member);
        match &edit.state {
            CardEdit::Threshold(_) => Some(
                profile
                    .map(crate::fallback::threshold_for)
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_default(),
            ),
            CardEdit::Weekly(_) => Some(
                profile
                    .and_then(|p| p.weekly_threshold)
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_default(),
            ),
            CardEdit::MaxSpend(_) => Some(format!(
                "{:.2}",
                profile.and_then(|p| p.max_auto_spend).unwrap_or(0.0)
            )),
            CardEdit::Days(_) | CardEdit::ArmedRemove => None,
        }
    });
    if let Some(saved) = fallback_saved
        && let Some(edit) = app.fallback_edit.as_mut()
        && let Some(input) = edit.state.input_mut()
        && edit.seed.as_deref() == Some(input.value.as_str())
        && input.value != saved
    {
        *input = InputState::new(&saved);
        edit.seed = Some(saved);
    }
    // Config editors.
    let refresh_seed = refresh_interval_seed(app);
    if let Some(draft) = app.refresh_interval_draft.as_mut() {
        draft.reseed(&refresh_seed);
    }
    let nudge_seed = context_nudge_seed(app.config().state.context_nudge_threshold_tokens());
    if let Some(draft) = app.context_nudge_draft.as_mut() {
        draft.reseed(&nudge_seed);
    }
    let weekly_seed = format_weekly_pct(app.config().state.weekly_switch_threshold_pct());
    if let Some(draft) = app.weekly_threshold_draft.as_mut() {
        draft.reseed(&weekly_seed);
    }
    // Services herdr tag.
    let herdr_seed = app.config().state.herdr.tag_watch_secs.to_string();
    if let Some(draft) = app.plugin.herdr_tag_draft.as_mut() {
        draft.reseed(&herdr_seed);
    }
    // current view when clean; a pending pick re-seeds its `saved` too.
    // Usage note editor.
    if let Some(editor) = app.note_editor.as_mut() {
        let saved = crate::profile_notes::load_note(&editor.profile).unwrap_or_default();
        if editor.buf.text() == editor.seed && saved != editor.seed {
            editor.buf = NoteBuffer::new(&saved);
            editor.seed = saved;
        }
    }
}

pub(crate) fn step_usage_cursor(app: &mut App, delta: i32) {
    let own = app.setup_items().len();
    let total = own + app.usage_extras.len();
    if total == 0 {
        return;
    }
    let at = app
        .usage_extra_cursor
        .map_or_else(|| app.usage_position(), |i| own + i);
    let next = (at as i32 + delta).rem_euclid(total as i32) as usize;
    if next < own {
        app.usage_extra_cursor = None;
        app.step_selection(SelectionSlotKind::Usage, next);
    } else {
        app.usage.clear();
        app.usage_extra_cursor = Some(next - own);
    }
}

/// Move Setup's selector by `delta`, wrapping over the item rows plus `+ new`.
fn step_setup_cursor(app: &mut App, delta: i32, len: usize) {
    if len == 0 {
        return;
    }
    let next = (app.setup_position() as i32 + delta).rem_euclid(len as i32) as usize;
    app.step_selection(SelectionSlotKind::Setup, next);
}

/// One page = the current viewport minus one overlap line (minimum 1), so a
/// page down keeps the old bottom line as the new top line. Every scrollable
/// surface steps by this; `↑`/`↓` stay one row/line and keep their wrapping.
pub(crate) fn page_step(viewport: usize) -> usize {
    viewport.saturating_sub(1).max(1)
}

/// The page-down landing row: the last row whose start line is within one page
/// below `cursor`'s start, at least one row forward, stopping at the last row.
/// Shared by [`page_down_cursor_by_rows`] and the Tokens dashboard page keys.
pub(crate) fn page_land_down(cursor: usize, starts: &[usize], viewport: usize) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let last = starts.len() - 1;
    let cursor = cursor.min(last);
    if cursor >= last {
        return last;
    }
    let target = starts[cursor] + page_step(viewport);
    let mut next = cursor + 1;
    while next < last && starts[next + 1] <= target {
        next += 1;
    }
    next
}

/// The page-up landing row: the first row whose start line is within one page
/// above `cursor`'s start, at least one row back, stopping at the first row.
/// Shared by [`page_up_cursor_by_rows`] and the Tokens dashboard page keys.
pub(crate) fn page_land_up(cursor: usize, starts: &[usize], viewport: usize) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let cursor = cursor.min(starts.len() - 1);
    if cursor == 0 {
        return 0;
    }
    let target = starts[cursor].saturating_sub(page_step(viewport));
    let mut prev = cursor - 1;
    while prev > 0 && starts[prev - 1] >= target {
        prev -= 1;
    }
    prev
}

/// Page down on a read-only scroll offset, clamped to the last render's bound.
pub(crate) fn page_down_scroll(scroll: usize, max: usize, viewport: usize) -> usize {
    scroll.min(max).saturating_add(page_step(viewport)).min(max)
}

/// Page up on a read-only scroll offset, clamped to the last render's bound.
pub(crate) fn page_up_scroll(scroll: usize, max: usize, viewport: usize) -> usize {
    scroll.min(max).saturating_sub(page_step(viewport))
}

/// Page down on a cursor list over its published row geometry: the cursor
/// lands on the last row whose start line is within one page (`viewport - 1`
/// lines) below the current row's start, at least one row forward, stopping at
/// the last row — walking back if the follow that landing produces would push
/// the old bottom line off the top (a focused row's tooltip moves with the
/// cursor). `starts[i]` is the rendered line where row `i` begins; `block_ends`
/// is the line its block reaches when focused; `offset`/`total` are the last
/// render's scroll and line count.
pub(crate) fn page_down_cursor_by_rows(
    cursor: usize,
    starts: &[usize],
    block_ends: &[usize],
    viewport: usize,
    offset: usize,
    total: usize,
) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let cursor = cursor.min(starts.len() - 1);
    let mut next = page_land_down(cursor, starts, viewport);
    while next > cursor + 1 {
        let end = block_ends
            .get(next)
            .copied()
            .unwrap_or(starts[next] + 1)
            .max(starts[next] + 1);
        let predicted = follow_scroll_offset(total, viewport, (starts[next], end), offset);
        if predicted <= offset + page_step(viewport) {
            break;
        }
        next -= 1;
    }
    next
}

/// Page up mirror of [`page_down_cursor_by_rows`]: the cursor lands on the
/// first row whose start line is within one page above the current row's
/// start, at least one row back, stopping at the first row — walking forward
/// if the follow would push the old top line off the bottom. The landing row's
/// focused tooltip inserts `tips[prev]` lines above the old top line, so the
/// continuity bound is `offset + tip - page_step`, not `offset - page_step`.
pub(crate) fn page_up_cursor_by_rows(
    cursor: usize,
    starts: &[usize],
    block_ends: &[usize],
    tips: &[usize],
    viewport: usize,
    offset: usize,
    total: usize,
) -> usize {
    if starts.is_empty() {
        return 0;
    }
    let cursor = cursor.min(starts.len() - 1);
    if cursor == 0 {
        return 0;
    }
    let mut prev = page_land_up(cursor, starts, viewport);
    while prev < cursor - 1 {
        let end = block_ends
            .get(prev)
            .copied()
            .unwrap_or(starts[prev] + 1)
            .max(starts[prev] + 1);
        let predicted = follow_scroll_offset(total, viewport, (starts[prev], end), offset);
        let tip = tips.get(prev).copied().unwrap_or(0);
        if predicted
            >= offset
                .saturating_add(tip)
                .saturating_sub(page_step(viewport))
        {
            break;
        }
        prev += 1;
    }
    prev
}

/// Page down on a [`ListView`] cursor (single-line selector rows), reading the
/// geometry the render published.
fn selector_page_down(cursor: usize, list: &ListView) -> usize {
    let starts = list.starts.borrow();
    let block_ends = list.block_ends.borrow();
    page_down_cursor_by_rows(
        cursor,
        &starts,
        &block_ends,
        list.viewport.get(),
        list.offset.get(),
        list.total.get(),
    )
}

/// Page up on a [`ListView`] cursor, the mirror of [`selector_page_down`].
fn selector_page_up(cursor: usize, list: &ListView) -> usize {
    let starts = list.starts.borrow();
    let block_ends = list.block_ends.borrow();
    let tips = list.tips.borrow();
    page_up_cursor_by_rows(
        cursor,
        &starts,
        &block_ends,
        &tips,
        list.viewport.get(),
        list.offset.get(),
        list.total.get(),
    )
}

fn handle_overview_key(app: &mut App, key: KeyEvent) {
    let count = app.main_items().len();
    let extras = if app.harness_filter.shows_claude() {
        app.overview_extras()
    } else {
        Vec::new()
    };
    let extra = app.overview_selected_extra();
    let at = extra
        .and_then(|i| extras.iter().position(|&e| e == i))
        .map_or(app.overview_cursor, |i| count + i);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Up | KeyCode::Down if shift && extra.is_some() => refuse_read_only(app),
        KeyCode::Up if shift => reorder_main_cursor(app, -1),
        KeyCode::Down if shift => reorder_main_cursor(app, 1),
        KeyCode::Up => step_overview_cursor(app, -1),
        KeyCode::Down => step_overview_cursor(app, 1),
        KeyCode::PageUp | KeyCode::PageDown => {
            let next = if key.code == KeyCode::PageDown {
                selector_page_down(at, &app.overview_selector)
            } else {
                selector_page_up(at, &app.overview_selector)
            };
            step_overview_cursor(app, next as i32 - at as i32);
        }
        KeyCode::Char('m') if extra.is_some() => refuse_read_only(app),
        KeyCode::Char('m') => open_move_session(app),
        KeyCode::Enter if extra.is_some() => switch_tab(app, Tab::Usage),
        KeyCode::Enter => activate_main_item(app),
        KeyCode::Char('s')
            if extra.is_none()
                && matches!(app.current_main_item(), Some(MainItemKind::Hermes(_))) =>
        {
            activate_main_item(app)
        }
        _ => {}
    }
}

/// Usage tab: account selection at rest, read-only detail scrolling on descent.
fn handle_usage_key(app: &mut App, key: KeyEvent) {
    if app.usage_detail_focus == StatusFocus::Detail {
        let max = app.usage_detail_max_scroll.get();
        let viewport = app.usage_detail_viewport.get();
        match key.code {
            KeyCode::Up => {
                app.usage_detail_scroll = app.usage_detail_scroll.min(max).saturating_sub(1)
            }
            KeyCode::Down => {
                app.usage_detail_scroll =
                    app.usage_detail_scroll.min(max).saturating_add(1).min(max)
            }
            KeyCode::PageUp => {
                app.usage_detail_scroll = page_up_scroll(app.usage_detail_scroll, max, viewport)
            }
            KeyCode::PageDown => {
                app.usage_detail_scroll = page_down_scroll(app.usage_detail_scroll, max, viewport)
            }
            KeyCode::Char('e') if app.usage_extra_cursor.is_none() => toggle_show_estimates(app),
            KeyCode::Char('p') if app.usage_extra_cursor.is_none() => toggle_show_pace(app),
            _ => {}
        }
        return;
    }
    let own = app.setup_items().len();
    let count = own + app.usage_extras.len();
    let at = app
        .usage_extra_cursor
        .map_or_else(|| app.usage_position(), |i| own + i);
    match key.code {
        KeyCode::Up => {
            step_usage_cursor(app, -1);
            app.usage_detail_scroll = 0;
        }
        KeyCode::Down => {
            step_usage_cursor(app, 1);
            app.usage_detail_scroll = 0;
        }
        KeyCode::PageUp => {
            let next = selector_page_up(at, &app.usage_selector);
            step_usage_cursor(app, next as i32 - at as i32);
            app.usage_detail_scroll = 0;
        }
        KeyCode::PageDown => {
            let next = selector_page_down(at, &app.usage_selector);
            step_usage_cursor(app, next as i32 - at as i32);
            app.usage_detail_scroll = 0;
        }
        KeyCode::Enter if count > 0 => {
            // Descending into the shown row adopts it: a held-but-hidden Usage
            // identity is replaced, never restored over the pane now open (the
            // rule Setup's `enter_config_detail` keeps).
            app.usage.clear();
            app.usage_detail_focus = StatusFocus::Detail;
            app.usage_detail_scroll = 0;
        }
        KeyCode::Char('e') if app.usage_extra_cursor.is_none() => toggle_show_estimates(app),
        KeyCode::Char('p') if app.usage_extra_cursor.is_none() => toggle_show_pace(app),
        _ => {}
    }
}

/// Open the note editor on the selected account, seeded from disk.
fn open_note_editor(app: &mut App) {
    if let Some(i) = app.usage_extra_cursor {
        if let Some(obs) = app.usage_extras.get(i) {
            app.toast(ToastKind::Info, read_only_hint(obs));
        }
        return;
    }
    // Opening the editor adopts the shown row: a held-but-hidden Usage identity
    // is replaced, never restored over the editor now open.
    app.usage.clear();
    let Some(name) = app.profile_name_at(app.profile_cursor.read()) else {
        return;
    };
    let text = match crate::profile_notes::load_note_for_edit(&name) {
        Ok(text) => text.unwrap_or_default(),
        Err(e) => {
            app.toast(ToastKind::Danger, format!("couldn't read the note: {e}"));
            return;
        }
    };
    app.note_editor = Some(NoteEditor {
        profile: name,
        seed: text.clone(),
        buf: NoteBuffer::new(&text),
    });
}

/// `⏎` in the note editor: persist the draft (an empty draft clears the note).
/// A failed save keeps the editor open — the draft is the only copy. Returns
/// whether the commit landed.
fn commit_note(app: &mut App) -> bool {
    let Some(editor) = app.note_editor.take() else {
        return true;
    };
    let text = editor.buf.text();
    match crate::profile_notes::save_note(&editor.profile, &text) {
        Ok(()) => {
            app.reload_selected_note();
            app.toast(ToastKind::Success, "note saved");
            true
        }
        Err(e) => {
            app.note_editor = Some(editor);
            app.toast(
                ToastKind::Danger,
                format!("couldn't save the note: {}", escape_control(&e.to_string())),
            );
            false
        }
    }
}

/// The note editor's keymap: text edits, ⌃j newline, ⏎ save, esc cancel. Every
/// printable is data (a note may contain `q`, `?`, `x`), so the owner claims
/// all keys bar ctrl+c.
fn handle_note_editor_key(app: &mut App, key: KeyEvent) {
    enum NoteKey {
        Commit,
        Cancel,
        Char(char),
        Newline,
        Word,
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
    }
    let action = match key.code {
        KeyCode::Enter => NoteKey::Commit,
        KeyCode::Esc => NoteKey::Cancel,
        KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => NoteKey::Newline,
        KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => NoteKey::Word,
        KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::CONTROL) => return,
        KeyCode::Char(ch) => NoteKey::Char(ch),
        KeyCode::Backspace => NoteKey::Backspace,
        KeyCode::Delete => NoteKey::Delete,
        KeyCode::Left => NoteKey::Left,
        KeyCode::Right => NoteKey::Right,
        KeyCode::Up => NoteKey::Up,
        KeyCode::Down => NoteKey::Down,
        KeyCode::Home => NoteKey::Home,
        KeyCode::End => NoteKey::End,
        _ => return,
    };
    match action {
        NoteKey::Commit => {
            let _ = commit_note(app);
        }
        NoteKey::Cancel => app.note_editor = None,
        _ => {
            let Some(editor) = app.note_editor.as_mut() else {
                return;
            };
            let buf = &mut editor.buf;
            match action {
                NoteKey::Char(ch) => buf.insert_char(ch),
                NoteKey::Newline => buf.insert_newline(),
                NoteKey::Word => buf.delete_word(),
                NoteKey::Backspace => buf.backspace(),
                NoteKey::Delete => buf.delete(),
                NoteKey::Left => buf.left(),
                NoteKey::Right => buf.right(),
                NoteKey::Up => buf.up(),
                NoteKey::Down => buf.down(),
                NoteKey::Home => buf.home(),
                NoteKey::End => buf.end(),
                _ => {}
            }
        }
    }
}

/// Status tab keymap. List focus: ↑↓ moves the incident cursor (wrapping), ⏎
/// descends into the timeline (only when an incident exists). Detail focus: ↑↓
/// scrolls the timeline (clamped to content by the render pass).
fn handle_status_key(app: &mut App, key: KeyEvent) {
    match app.status.focus {
        StatusFocus::List => {
            let len = app.status.incidents.len();
            match key.code {
                KeyCode::Up if len > 0 => {
                    app.status.cursor = (app.status.cursor + len - 1).rem_euclid(len.max(1));
                    app.status.detail_scroll = 0;
                }
                KeyCode::Down if len > 0 => {
                    app.status.cursor = (app.status.cursor + 1).rem_euclid(len.max(1));
                    app.status.detail_scroll = 0;
                }
                KeyCode::PageUp if len > 0 => {
                    app.status.cursor = selector_page_up(app.status.cursor, &app.status.selector);
                    app.status.detail_scroll = 0;
                }
                KeyCode::PageDown if len > 0 => {
                    app.status.cursor = selector_page_down(app.status.cursor, &app.status.selector);
                    app.status.detail_scroll = 0;
                }
                KeyCode::Enter if len > 0 => {
                    app.status.focus = StatusFocus::Detail;
                    app.status.detail_scroll = 0;
                }
                _ => {}
            }
        }
        StatusFocus::Detail => match key.code {
            KeyCode::Up => {
                // Clamp before stepping, for the mirror of the ↓ case: a shorter
                // incident (or a taller terminal) shrinks the bound under an
                // offset that outlived it, and stepping down from the stale
                // value costs one press per row while the render clamps the
                // display, so ↑ reads as dead.
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = app.status.detail_scroll.min(max).saturating_sub(1);
            }
            KeyCode::Down => {
                // Clamp against the last render's content height so a held ↓ can't
                // run the offset past the end (which would make ↑ look dead).
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = app.status.detail_scroll.saturating_add(1).min(max);
            }
            KeyCode::PageUp => {
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = page_up_scroll(
                    app.status.detail_scroll as usize,
                    max as usize,
                    app.status.detail_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::PageDown => {
                let max = app.status.detail_max_scroll.get();
                app.status.detail_scroll = page_down_scroll(
                    app.status.detail_scroll as usize,
                    max as usize,
                    app.status.detail_viewport.get() as usize,
                ) as u16;
            }
            _ => {}
        },
    }
}

/// Signal the status thread to refetch and light the title spinner.
fn trigger_status_refresh(app: &mut App) {
    let _ = app.status_refresh.send(());
    app.status.fetching = true;
    app.toast(ToastKind::Info, "refreshing status");
}

/// `↵` in a field editor: build the edit and either save it (through a confirm
/// for a bind restart or a cascading rename) or show the dry run's refusal.
/// How one changed item's commit resolved under a loss-warning `save`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitResult {
    /// Committed synchronously.
    Landed,
    /// The item refused at commit time; the editor stays open.
    Refused,
    /// The commit opened a further dialog instead of landing.
    Dialog,
}

/// A danger toast body over a multi-line diagnostic: `title` on its own line,
/// then each line of `diagnostic` ANSI-stripped and control-escaped, the lines
/// kept apart with `\n` so the toast splits them into rows. Shared by the
/// herdr heal and the plugin install failure toasts.
fn diagnostic_toast_body(title: &str, diagnostic: &str) -> String {
    format!("{title}\n{}", stderr_display_lines(diagnostic).join("\n"))
}

/// Strip ANSI CSI sequences (`ESC [ params/intermediates final`, params in
/// `0x20..=0x3F`, final in `0x40..=0x7E`) from `text`, so the modal shows
/// text, not terminal escape bytes. Any byte outside those ranges before the
/// final byte ends the sequence without eating text: the `ESC` is kept and
/// scanning resumes after it, so a malformed CSI never swallows a line.
fn strip_ansi_csi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(esc) = rest.find('\x1b') {
        out.push_str(&rest[..esc]);
        let after = &rest[esc + 1..];
        if let Some(body) = after.strip_prefix('[') {
            match body
                .char_indices()
                .find(|(_, c)| ('@'..='~').contains(c) || !('\u{20}'..='\u{3f}').contains(c))
            {
                Some((at, c)) if ('@'..='~').contains(&c) => rest = &body[at + c.len_utf8()..],
                _ => {
                    // No final byte, or a non-parameter byte: not a CSI.
                    out.push('\x1b');
                    rest = after;
                }
            }
        } else {
            out.push('\x1b');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The herdr detail's options keymap: ↑↓ walks the six rows (wrapping), space
/// and ⏎ activate the focused row, `+`/`-` step the tag-refresh stepper, `f`
/// still applies the check's fix. Only the herdr detail routes here — every
/// other detail keeps the scroll-only keymap.
fn handle_herdr_options_key(app: &mut App, key: KeyEvent) {
    let cursor = app.plugin.herdr_options_cursor;
    let rows = HERDR_OPTIONS.len();
    match key.code {
        KeyCode::Up => {
            app.plugin.diagnostic_focus = false;
            app.plugin.herdr_options_cursor = (cursor + rows - 1) % rows;
        }
        KeyCode::Down => {
            app.plugin.diagnostic_focus = false;
            app.plugin.herdr_options_cursor = (cursor + 1) % rows;
        }
        KeyCode::Char('f') => apply_plugin_fix(app),
        KeyCode::Enter | KeyCode::Char(' ') if !app.plugin.diagnostic_focus => {
            activate_herdr_option(app, HERDR_OPTIONS[cursor]);
        }
        KeyCode::Char('+') if !app.plugin.diagnostic_focus => step_herdr_tag_refresh(app, 1),
        KeyCode::Char('-') if !app.plugin.diagnostic_focus => step_herdr_tag_refresh(app, -1),
        _ => {}
    }
}

/// Apply space/⏎ to the focused options row: cycle, toggle, open the tag
/// editor, or confirm the delegate-row rewrite. Every change persists
/// immediately through `save_app_state` (the `cycle_theme` shape).
fn activate_herdr_option(app: &mut App, row: HerdrOption) {
    match row {
        HerdrOption::PopupWidth => cycle_herdr_popup_width(app),
        HerdrOption::PaneTag => {
            toggle_herdr_bool(app, |h| h.pane_tag = !h.pane_tag);
            push_herdr_knob_change();
        }
        HerdrOption::TagRefresh => begin_herdr_tag_edit(app),
        HerdrOption::BorderLabel => {
            toggle_herdr_bool(app, |h| h.border_label = !h.border_label);
            push_herdr_knob_change();
        }
        HerdrOption::DelegateDot => toggle_herdr_bool(app, |h| h.delegate_dot = !h.delegate_dot),
        // Inert while herdr's config does not read+parse: the write this row
        // triggers goes through the parse, so offering it there is a promise
        // the heal cannot keep. The render draws the row faint with the reason.
        HerdrOption::DelegateRowText => {
            if herdr_config_writable(app) {
                open_herdr_row_text_confirm(app);
            }
        }
    }
}

/// True while the herdr config behind the options section reads AND parses —
/// the states where `heal` can actually rewrite the file. `None` (unreadable /
/// no config path) and `parsed == false` both read as not writable. Shared by
/// the key handler and the render so the inert row and the no-op key agree.
pub(crate) fn herdr_config_writable(app: &App) -> bool {
    app.plugin.herdr_config.as_ref().is_some_and(|c| c.parsed)
}

/// `+`/`-` on the tag-refresh stepper: ±1 second, floored at 1. A no-op on
/// every other row.
fn step_herdr_tag_refresh(app: &mut App, delta: i64) {
    if HERDR_OPTIONS[app.plugin.herdr_options_cursor] != HerdrOption::TagRefresh {
        return;
    }
    {
        let mut cfg = app.config();
        let cur = cfg.state.herdr.tag_watch_secs;
        cfg.state.herdr.tag_watch_secs = if delta > 0 {
            cur.saturating_add(1)
        } else {
            cur.saturating_sub(1).max(1)
        };
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Flip one herdr bool knob and persist immediately (the `cycle_theme` save
/// shape — no separate save step).
fn toggle_herdr_bool(app: &mut App, flip: impl FnOnce(&mut HerdrSettings)) {
    {
        let mut cfg = app.config();
        flip(&mut cfg.state.herdr);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Push a `pane_tag`/`border_label` change onto the live herdr panes right
/// now, instead of letting it sit until the next watcher tick
/// (`tag_watch_secs` away, 5 s by default): re-run `report-profile.sh` once
/// per pane, the way `watch-profile.sh` re-runs it. The off direction clears
/// its artifact through the same re-run, so it no longer waits for the tick
/// either. Only the two knobs that script reads trigger this —
/// `delegate_dot`'s gate lives in the MCP server process (a separate binary a
/// TUI cannot hot-patch), and `tag_refresh` belongs to watchers that already
/// hold their own interval, so both keep their current behavior.
///
/// Gated on the plugin-pane environment (`HERDR_ENV=1` plus the injected
/// binary and plugin-root paths): no herdr command exposes the plugin root,
/// so this gate is the only channel, and a bare `tollgate` TUI inside a herdr
/// pane silently skips the push too. Best-effort — a failure anywhere is
/// silent, and the change still lands with the next report (a watcher tick,
/// or the next agent-event hook for panes without one).
fn push_herdr_knob_change() {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        return;
    }
    let Ok(bin) = std::env::var("HERDR_BIN_PATH") else {
        return;
    };
    let Ok(root) = std::env::var("HERDR_PLUGIN_ROOT") else {
        return;
    };
    let script = std::path::PathBuf::from(&root)
        .join("report-profile.sh")
        .to_string_lossy()
        .into_owned();
    spawn_worker(move || {
        let Some(panes) = herdr_pane_ids(&bin) else {
            return;
        };
        for pane in panes {
            rerun_pane_report(&script, &pane);
        }
    });
}

/// `bin pane list` → pane ids, parsed by the one typed parser the pane
/// reporter shares: an unknown payload shape (a herdr release moving a field)
/// reads as no panes rather than a wrong one. Bounded at
/// `crate::herdr::PROBE_TIMEOUT` — a hung herdr delays the push worker, never
/// the key handling.
fn herdr_pane_ids(bin: &str) -> Option<Vec<String>> {
    let out = crate::herdr::bounded_output(bin, &["pane", "list"], &[])?;
    if !out.status.success() {
        return None;
    }
    crate::herdr::parse_pane_list(&out.stdout)
        .map(|panes| panes.into_iter().map(|pane| pane.pane_id).collect())
}

/// Re-run `report-profile.sh` for one pane with the pane id set and the
/// event/context JSON cleared, so the script reads the pane's live agent
/// itself. The script's pidfile gate skips the watcher spawn while a live
/// watch exists, so re-running it alongside the live watchers is safe.
fn rerun_pane_report(script: &str, pane: &str) {
    let _ = crate::herdr::bounded_output(
        script,
        &[],
        &[
            ("HERDR_PANE_ID", std::ffi::OsStr::new(pane)),
            ("HERDR_PLUGIN_EVENT_JSON", std::ffi::OsStr::new("")),
            ("HERDR_PLUGIN_CONTEXT_JSON", std::ffi::OsStr::new("")),
        ],
    );
}

/// Advance the popup-width knob to the next shape, wrapping past `split-top`
/// back to `fit` — space always cycles forward, never clamps.
fn cycle_herdr_popup_width(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.herdr.popup_width = match cfg.state.herdr.popup_width {
            PopupWidth::Fit => PopupWidth::Half,
            PopupWidth::Half => PopupWidth::SplitRight,
            PopupWidth::SplitRight => PopupWidth::SplitTop,
            PopupWidth::SplitTop => PopupWidth::Fit,
        };
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Open the inline typed editor for the tag-refresh stepper, seeded with the
/// current value in whole seconds. ⏎ commits, ⎋ discards — the Config-tab
/// refresh-interval editor's mechanism.
fn begin_herdr_tag_edit(app: &mut App) {
    let secs = app.config().state.herdr.tag_watch_secs;
    app.plugin.herdr_tag_draft = Some(SeededInput::new(&secs.to_string()));
}

/// Keystrokes while the herdr tag-refresh editor is open: ⏎ saves, ⎋ discards.
fn handle_herdr_tag_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.plugin.herdr_tag_draft = None,
        KeyCode::Enter => {
            let _ = commit_herdr_tag_edit(app);
        }
        _ => {
            if let Some(draft) = app.plugin.herdr_tag_draft.as_mut() {
                apply_input_edit(&mut draft.input, key);
            }
        }
    }
}

/// Parse and persist the typed tag-refresh seconds. A value under 1s keeps the
/// draft open so the inline Invalid-input treatment stays on screen — no toast.
/// Returns whether the commit landed.
fn commit_herdr_tag_edit(app: &mut App) -> bool {
    let Some(raw) = app
        .plugin
        .herdr_tag_draft
        .as_ref()
        .map(|i| i.input.trimmed())
    else {
        return true;
    };
    let Some(secs) = parse_herdr_tag_secs(raw) else {
        return false;
    };
    let saved = {
        let mut cfg = app.config();
        cfg.state.herdr.tag_watch_secs = secs;
        save_app_state(&cfg.state)
    };
    if let Err(e) = saved {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        return false;
    }
    app.apply_forgets();
    app.plugin.herdr_tag_draft = None;
    true
}

/// A typed tag-refresh value is valid only as a whole number of seconds, at
/// least 1 — shared by the commit path and the render's inline check.
pub(crate) fn parse_herdr_tag_secs(raw: &str) -> Option<u64> {
    let secs = raw.parse::<u64>().ok()?;
    (secs >= 1).then_some(secs)
}

/// Open the confirm before flipping `delegate row text`: the flip rewrites
/// herdr's own config (the row `tollgate herdr install` appended), so it carries
/// the same confirm gate as the `[f]` heal. The copy names the delegate token
/// so the confirm says what it will write; cancel is the default choice.
fn open_herdr_row_text_confirm(app: &mut App) {
    let Some(path) = app
        .plugin
        .herdr
        .as_ref()
        .and_then(|probe| probe.as_ref())
        .and_then(|probe| probe.config_path.clone())
    else {
        return;
    };
    let turning_on = !app.config().state.herdr.delegate_row_text;
    app.disarm_quit();
    app.modals.push(Modal::Confirm(ConfirmState {
        message: if turning_on {
            "add the delegate token to herdr's sidebar row?".to_string()
        } else {
            "drop the delegate token from herdr's sidebar row?".to_string()
        },
        detail: Some(if turning_on {
            concat!(
                "writes $tollgate_delegate into the row tollgate added in herdr's config.toml, validated by ",
                cmd_lit!("herdr config check"),
                "; a hand-owned row is left alone with a note."
            )
            .to_string()
        } else {
            concat!(
                "rewrites the row tollgate added in herdr's config.toml without $tollgate_delegate, validated by ",
                cmd_lit!("herdr config check"),
                "; a hand-owned row is left alone with a note."
            )
            .to_string()
        }),
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::HerdrDelegateRowText(path),
    }));
}

/// Componentwise numeric comparison of two dotted version strings. `None` when
/// either side is not a dotted run of numbers — an unrecognisable version is
/// unknown, not stale, so a caller stays quiet rather than warning on a shape it
/// has never seen.
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parts = |s: &str| -> Option<Vec<u64>> {
        s.split('.').map(|part| part.parse::<u64>().ok()).collect()
    };
    Some(parts(a)?.cmp(&parts(b)?))
}

/// `probed >= min` componentwise. Either side absent or unparseable reads as
/// satisfied: a false warn on an unrecognisable version is worse than staying
/// quiet.
fn version_satisfies(probed: Option<&str>, min: Option<&str>) -> bool {
    let (Some(probed), Some(min)) = (probed, min) else {
        return true;
    };
    match compare_versions(probed, min) {
        Some(ord) => ord != std::cmp::Ordering::Less,
        None => true,
    }
}

/// The Plugin tab's `herdr` row: the installed herdr's tollgate plugin plus the
/// keybinding/sidebar config `tollgate herdr install` adds. Pure so the verdict
/// logic unit-tests without an `App`; the caller supplies the probe and the
/// config readout (`None` when the config file could not be read at all).
pub(crate) fn herdr_check(
    probe: &crate::herdr::HerdrProbe,
    config: Option<&crate::herdr::ConfigStatus>,
) -> Check {
    let mut detail = Vec::new();
    detail.push(match &probe.version {
        Some(version) => format!("herdr: {}", escape_control(version)),
        None => "herdr: unknown".to_string(),
    });

    let mut danger = false;
    let mut warn = false;
    let mut fix = None;

    // Indented, because herdr's own prose carries colons ("manifest unavailable: No such file or directory") and `detail_line` splits the first `": "` into a key column: left flush, a warning renders as a field named after its first clause and widens that column for every real field above it.
    if let Some(error) = &probe.error {
        danger = true;
        detail.push(format!("  {}", escape_control(error)));
    }

    if let Some(entry) = &probe.entry {
        // `herdr plugin list --json` spells a linked plugin's source `local`, measured against 0.8.0; `link` is the verb that creates one, not the kind it reports.
        let local = entry.source_kind.as_deref() == Some("local");

        if !entry.warnings.is_empty() {
            danger = true;
        }
        if !entry.enabled {
            warn = true;
            detail.push("plugin: disabled".to_string());
        } else if local {
            detail.push("plugin: linked (local)".to_string());
        } else {
            detail.push("plugin: installed (github)".to_string());
        }
        if local && let Some(root) = &entry.plugin_root {
            detail.push(format!("root: {}", escape_control(root)));
        }
        for warning in &entry.warnings {
            detail.push(format!("  {}", escape_control(warning)));
        }
        if !version_satisfies(probe.version.as_deref(), entry.min_herdr_version.as_deref()) {
            warn = true;
            detail.push(format!(
                "plugin needs herdr {} or newer",
                escape_control(entry.min_herdr_version.as_deref().unwrap_or("unknown"))
            ));
        }

        let parsed = config.is_some_and(|c| c.parsed);
        let templated = config.is_some_and(|c| c.sidebar == crate::herdr::SidebarState::Templated);
        match config {
            Some(c) if !c.parsed => {
                warn = true;
                detail.push("herdr's config doesn't parse".to_string());
            }
            Some(c) => {
                match &c.bound_key {
                    Some(key) => detail.push(format!("key: {}", escape_control(key))),
                    None => {
                        warn = true;
                        detail.push("key: not bound".to_string());
                    }
                }
                if c.sidebar == crate::herdr::SidebarState::Templated {
                    detail.push("sidebar: templated".to_string());
                } else {
                    warn = true;
                    detail.push("sidebar: not templated".to_string());
                }
            }
            None => {
                warn = true;
                detail.push("herdr's config can't be read".to_string());
            }
        }

        if parsed && (config.and_then(|c| c.bound_key.as_deref()).is_none() || !templated) {
            detail.push(String::new());
            detail.push("[f] add the keybinding and sidebar row to herdr's config".to_string());
            fix = probe.config_path.clone().map(PluginFix::HealHerdrConfig);
        }
    } else if probe.error.is_none() {
        warn = true;
        detail.push("plugin: not installed".to_string());
        detail.push(String::new());
        detail.push(format!("  {}", prose::cmd("tollgate herdr install")));
    }

    let health = if danger {
        Health::Danger
    } else if warn {
        Health::Warn
    } else {
        Health::Ok
    };

    Check {
        label: "herdr",
        health,
        detail,
        fix,
    }
}

/// Setup's stored identity becomes the new account's `(provider, account)`, and
/// the Overview's remembered row moves onto it. Kept pending until a read
/// Land Setup and the Overview on one item after a create: Setup adopts the
/// item (its selector index cleared; the claude case passes `None` so the
/// selector lists profiles at `profile_cursor`), the Overview's remembered row
/// moves onto it, and the create form closes. One tail for all three creates,
/// so they cannot drift.
fn land_created_item(app: &mut App, setup_item: Option<SetupItem>, overview: MainItemKind) {
    land_created_selection(app, setup_item, overview);
    app.config_focus = ConfigFocus::Profiles;
    app.config_draft = None;
}

/// The shared "land Setup + Overview on one item" tail, split from
/// [`land_created_item`] so the pool add can close its form conditionally: only
/// the form that started the add closes, and only while it is still the open,
fn land_created_selection(app: &mut App, setup_item: Option<SetupItem>, overview: MainItemKind) {
    app.setup.item = setup_item;
    app.setup.index = None;
    app.setup.clear();
    app.overview_remembered = Some(overview);
}

/// Move the detail cursor onto the `name` row, so a refusal stored there (by a
/// press on another row) renders its `└` line immediately.
fn focus_name_row(app: &mut App) {
    if let Some(idx) = config_rows(app).iter().position(|r| *r == ConfigRow::Name) {
        app.config_action_cursor = idx;
    }
}

/// Raise the one "already in progress" Warning toast for a running login.
/// Every site with a login in flight calls this for its copy, so the text
/// cannot drift between surfaces; each caller keeps its own control flow
/// (some still open their modal).
fn toast_login_in_progress(app: &mut App, name: String) {
    app.toast(
        ToastKind::Warning,
        format!("a login for '{name}' is already in progress"),
    );
}

/// The one answer to "the `+ new` draft": the descended form in
/// `config_draft` (a `+ new` form has no `editing_name`, the marker
/// `suspend_source` and `start_new_account` already use), else the at-rest
/// `new_account_draft` while the selection rests on `+ new`, else none. Every
/// reader of the form's type and rows goes through this, so the preview, the
/// predicates and ⏎ can never read different drafts. Call it before any
/// config guard: its fallback branch re-runs `setup_selection`.
pub(crate) fn new_form_draft(app: &App) -> Option<&ConfigDraft> {
    if let Some(d) = app.config_draft.as_ref()
        && d.editing_name.is_none()
    {
        return Some(d);
    }
    if app.setup_selection() == SetupSelection::New {
        return app.new_account_draft.as_ref();
    }
    None
}

/// Whether the `+ new` form's type is claude — the form a preset's endpoint
/// and model fields stamp. The draft is [`new_form_draft`]'s; only a form
/// with no draft is the fresh form, which opens on claude. The one predicate
/// the action-menu offer and the apply's re-check share.
fn new_account_type_is_claude(app: &App) -> bool {
    new_form_draft(app).is_none_or(|d| d.apply_preset_armed())
}

/// Escapes an untrusted value for a detail line or toast slot: a control
/// character, a bidi formatting character, or a prose marker code point is
/// spelled as its visible `\u{…}` form so an untrusted string off the feed, a
/// tool's stdout or stderr, or a record read off another program's file can
/// neither inject into the terminal, reorder a line, nor style the cells after
/// it; every other character passes through unchanged. Applied at the producer
/// where the untrusted value enters the copy.
pub(crate) fn escape_control(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || crate::format::reorders_display(c) || prose::is_marker(c) {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// A compact toast detail from an `anyhow::Error`: the root cause first (it
/// must stay on screen), then the rest of the chain outermost-first, joined by
/// ` · `. Paths stay in full there — they may clip, the cause may not. Logs
/// and the CLI keep the full `{e:#}` Display.
fn error_toast_detail(err: &anyhow::Error) -> String {
    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
    match chain.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [.., root] => {
            let mut parts = vec![root.clone()];
            parts.extend(chain[..chain.len() - 1].iter().cloned());
            parts.join(" · ")
        }
    }
}

/// The clause an outdated plugin install's version line carries; the services
/// renderer tones that line by it, so the copy and its color share one spelling.
pub(crate) const OUTDATED_PLUGIN_MARK: &str = "this tollgate is";

/// Native Plugin install row. PATH, Claude Code, and MCP wiring are checked
/// by separate selector rows; completion upkeep belongs beside the install.
fn plugin_install_check(completions: Option<crate::completions::InstalledScript>) -> Check {
    use crate::plugin_probe as probe;
    let mut detail = Vec::new();
    let mut health = Health::Ok;
    let mut fixes = Vec::new();
    // "global" == active in every project: a CC `user`-scope plugin install. A
    // `local`/`project` install (or a `./.mcp.json`) binds tollgate to one repo.
    let records = probe::installed_records();
    let plugin_global = records.iter().any(|r| r.scope.as_deref() == Some("user"));

    // plugin install record — installed-only verdict (CC exposes no clean
    // per-scope "enabled" boolean, so the row reports presence, scope and
    // version, not enabled/disabled).
    let marketplace = probe::marketplace_known();
    // The operative record is the `user`-scope install when one exists; a stale
    // project/local row that sorts first must not name the install beside a
    // verdict computed off the user row (`plugin_global` above).
    let record_for_check = records
        .iter()
        .find(|r| r.scope.as_deref() == Some("user"))
        .or_else(|| records.first());
    if let Some(record) = record_for_check {
        let scope = record.scope.as_deref();
        // Claude Code caches the plugin by its manifest version, so a user
        // install older than this binary keeps serving the old tree until it
        // is reinstalled, and the install fix updates it. A newer install is a
        // newer tollgate's, which the install never downgrades.
        let this_version = env!("CARGO_PKG_VERSION");
        let outdated = plugin_global
            && record
                .version
                .as_deref()
                .is_some_and(|installed| plugin_version_older(installed, this_version));
        detail.push(format!(
            "installed: yes ({})",
            escape_control(scope.unwrap_or("?"))
        ));
        if let Some(version) = &record.version {
            detail.push(if outdated {
                format!(
                    "version: {} ({OUTDATED_PLUGIN_MARK} {this_version})",
                    escape_control(version)
                )
            } else {
                format!("version: {}", escape_control(version))
            });
        }
        if let Some(sha) = &record.git_commit_sha {
            detail.push(format!(
                "commit: {}",
                escape_control(&sha.chars().take(7).collect::<String>())
            ));
        }
        if let Some(at) = &record.installed_at {
            detail.push(format!(
                "installed at: {}",
                escape_control(at.split('T').next().unwrap_or(at))
            ));
        }
        if let Some(project) = &record.project_path {
            detail.push(format!("project: {}", escape_control(project)));
        }
        if let Some(repo) = marketplace.as_ref().and_then(|m| m.repo.as_ref()) {
            detail.push(format!("marketplace: {}", escape_control(repo)));
        }
        if outdated {
            detail.push(fix_line(&PluginFix::InstallPlugin));
            fixes.push(PluginFix::InstallPlugin);
            health = worst(health, Health::Warn);
        } else if plugin_global {
            health = worst(health, Health::Ok);
        } else {
            detail.push("installed for this project only, not global".to_string());
            detail.push("installs at user scope".to_string());
            detail.push(fix_line(&PluginFix::InstallPlugin));
            fixes.push(PluginFix::InstallPlugin);
            health = worst(health, Health::Warn);
        }
    } else {
        let known = marketplace.is_some();
        detail.push(format!(
            "installed: no ({})",
            if known {
                "marketplace known"
            } else {
                "marketplace unknown"
            }
        ));
        if let Some(repo) = marketplace.as_ref().and_then(|m| m.repo.as_ref()) {
            detail.push(format!("marketplace: {}", escape_control(repo)));
        }
        detail.push("installs at user scope".to_string());
        detail.push(fix_line(&PluginFix::InstallPlugin));
        fixes.push(PluginFix::InstallPlugin);
        health = worst(health, Health::Warn);
    }

    // The installed shell completion script. Its own group, last: a stale one
    // is the least severe plugin problem, so it never displaces the fixes
    // above it, and it warns. A missing one is an invitation, not a problem:
    // it offers the install and moves no health. No line at all while the
    // script is current or the shell is unknown (`completions` is `None`).
    if let Some(script) = completions
        && let Some((word, stale)) = match script.state {
            crate::completions::ScriptState::Stale => Some(("stale", true)),
            crate::completions::ScriptState::Missing => Some(("missing", false)),
            crate::completions::ScriptState::Current => None,
        }
    {
        detail.push(String::new());
        detail.push(format!(
            "completions: {word} ({})",
            escape_control(&script.shell)
        ));
        detail.push(if stale {
            format!(
                "{} script differs from this tollgate's",
                escape_control(&script.shell)
            )
        } else {
            format!("{} script not installed", escape_control(&script.shell))
        });
        let fix = PluginFix::Completions {
            shell: script.shell,
            stale,
        };
        detail.push(fix_line(&fix));
        fixes.push(fix);
        if stale {
            health = worst(health, Health::Warn);
        }
    }

    Check {
        label: "plugin",
        health,
        detail,
        fix: fixes.first().cloned(),
    }
}

/// The more severe of two health buckets (Ok < Warn < Danger), combining a
/// row's checks into one verdict. `Idle` is neutral and never outranks a verdict.
fn worst(a: Health, b: Health) -> Health {
    use Health::{Danger, Warn};
    if a == Danger || b == Danger {
        Danger
    } else if a == Warn || b == Warn {
        Warn
    } else {
        Health::Ok
    }
}

/// Open the selected incident's page in the default browser (detached).
fn open_incident_link(app: &mut App) {
    let Some(link) = app.status.selected().map(|i| i.link.clone()) else {
        return;
    };
    if link.is_empty() {
        app.toast(ToastKind::Info, "no link for this incident");
        return;
    }
    match crate::platform::open_url(&link) {
        Ok(()) => app.toast(ToastKind::Info, "opening in browser"),
        Err(_) => app.toast(ToastKind::Danger, "failed to open browser"),
    }
}

/// Request switch to profile at `idx`; no-ops if already active.
fn request_switch_to(app: &mut App, idx: usize) {
    let cfg = app.config();
    let Some(profile) = cfg.profiles.get(idx) else {
        return;
    };
    let name = profile.name.clone();
    // `switch_profile` already refuses a disabled target (shared guard), but a
    // disabled row must never even offer the confirm — never selectable, not
    // just never landed.
    if cfg.is_active(&name) || profile.is_disabled() {
        return;
    }
    drop(cfg);
    app.modals.push(Modal::Confirm(ConfirmState {
        message: format!("switch to '{name}'?"),
        detail: None,
        choice: true,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::Switch(name.to_string()),
    }));
}

fn activate_main_item(app: &mut App) {
    let Some(item) = app.current_main_item() else {
        return;
    };
    match item {
        MainItemKind::Hermes(i) => {
            if let Some(name) = app.hermes_rows.get(i).map(|row| row.name.clone()) {
                app.toast(ToastKind::Warning, hermes_switch_toast(&name));
            }
        }

        MainItemKind::Profile(idx) => request_switch_to(app, idx),
        MainItemKind::Codex(_) => {}
    }
}

fn reorder_main_cursor(app: &mut App, delta: i32) {
    let Some(MainItemKind::Profile(idx)) = app.current_main_item() else {
        return;
    };
    let new_idx = match delta.signum() {
        -1 if idx > 0 => idx - 1,
        1 if idx + 1 < app.config().profiles.len() => idx + 1,
        _ => return,
    };
    let result = {
        let mut cfg = app.config();
        reorder_profile(&mut cfg, idx, new_idx)
    };
    if let Err(e) = result {
        app.toast(
            ToastKind::Danger,
            format!(
                "reorder failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        );
        return;
    }
    app.profile_cursor.set(new_idx);
    app.move_overview_cursor(new_idx);
}

/// One off-thread AUTH-1 switch-gate answer, posted by `spawn_switch_gate`'s
/// worker and drained by `drain_switch_gates`.
pub(crate) struct SwitchGateResult {
    name: ProfileName,
    gate: SwitchGate,
}

#[cfg(test)]
impl SwitchGateResult {
    /// An answered gate over a settle that changed nothing.
    pub(crate) fn gated(name: ProfileName, gate: oauth::AuthGate) -> Self {
        Self {
            name,
            gate: SwitchGate::Gated {
                gate,
                settled: crate::claude::BareItemSettled::default(),
            },
        }
    }

    /// The AUTH-1 answer, `None` for a switch the settle held.
    pub(crate) fn auth_gate(&self) -> Option<&oauth::AuthGate> {
        match &self.gate {
            SwitchGate::Gated { gate, .. } => Some(gate),
            SwitchGate::Held(_) => None,
        }
    }
}

/// What a switch gate's worker found: the bare item settled and the AUTH-1
/// gate answered, or the settle held the switch before the gate ran.
enum SwitchGate {
    Gated {
        gate: oauth::AuthGate,
        settled: crate::claude::BareItemSettled,
    },
    Held(BareItemOutcome),
}

/// Switch the active profile to `name`. The AUTH-1 pre-install gate
/// (`ensure_installable`) may refresh the target's token over HTTP, so it runs
/// off the UI thread; `drain_switch_gates` completes the relink when it
/// answers. The already-active target skips the gate — nothing new to install
/// (`switch_profile` no-ops on `is_active`), and gating it races a live
/// `claude` refreshing through the symlink — the same exemption as the
/// CLI/MCP paths.
fn perform_switch(app: &mut App, name: &ProfileName) {
    if guest_refused(app) {
        return;
    }

    let active = app.config().state.active_profile.as_ref().cloned();
    if active.as_deref() == Some(name) {
        finalize_switch(app, name, None);
        return;
    }
    spawn_switch_gate(app, name.clone(), oauth::refresh_result);
}

/// Settle the bare item, then run `ensure_installable` for `name`, off the UI
/// thread, and post the answer to the switch-gate channel. The settle runs
/// first: when the switch targets the item's owner, that owner's store holds
/// the login Claude Code already spent, so the gate would refresh a spent
/// token and quarantine the owner before the settle could save its live
/// login. A settle the user must answer, or one that failed, holds the switch
/// without running the gate. A resumed switch whose settle already answered
/// ([`App::bare_item_ready`]) skips it. The `Switching` activity mark is the
/// pending state: it shows the spinner and blocks further switches until the
/// drain clears it. `refresher` is injected so tests gate offline; under
/// `cfg(test)` the gate runs synchronously — a detached worker would race the
/// test's `HomeSandbox` (the gate persists `auth_broken` transitions to home
/// paths).
fn spawn_switch_gate<F>(app: &mut App, name: ProfileName, refresher: F)
where
    F: Fn(&str, Option<&str>) -> std::result::Result<oauth::TokenResponse, oauth::RefreshError>
        + Send
        + 'static,
{
    if guest_refused(app) {
        return;
    }

    mark_activity(&app.activity, &name, ProfileActivity::Switching);
    let config = Arc::clone(&app.config);
    let sender = app.switch_gate_tx.clone();
    let source = app.bare_item_source;
    let ready = app.bare_item_ready.take();
    let gate = move || {
        // Posts an answer on drop if the worker panicked before its own send,
        // so a crashed worker never strands the `Switching` marker (and the
        // exit wait that polls it). Mirrors `BootstrapDoneGuard`. A panic in
        // the settle posts a failed settle, so the switch toasts its own
        // failure rather than a refresh that never ran; one in the gate posts
        // a `Transient` answer.
        struct SwitchGateAnswerGuard {
            name: ProfileName,
            sender: std::sync::mpsc::Sender<SwitchGateResult>,
            settling: bool,
            sent: bool,
        }
        impl Drop for SwitchGateAnswerGuard {
            fn drop(&mut self) {
                if self.sent {
                    return;
                }
                let gate = if self.settling {
                    SwitchGate::Held(BareItemOutcome::Settled(Err(anyhow::anyhow!(
                        BARE_ITEM_WORKER_PANICKED
                    ))))
                } else {
                    SwitchGate::Gated {
                        gate: oauth::AuthGate::Transient(crate::format::Transient::new(
                            crate::format::Cause::InternalLock,
                            crate::format::Retry::Stated,
                        )),
                        settled: crate::claude::BareItemSettled::default(),
                    }
                };
                let _ = self.sender.send(SwitchGateResult {
                    name: self.name.clone(),
                    gate,
                });
            }
        }
        let mut answer = SwitchGateAnswerGuard {
            name: name.clone(),
            sender,
            settling: true,
            sent: false,
        };
        let settle = match ready {
            Some(placed) => Ok(crate::claude::BareItemSettle::Settled(
                crate::claude::BareItemSettled {
                    notice: None,
                    placed,
                },
            )),
            None => settle_bare_keychain_item(source, crate::claude::BareItemWriter::Switch),
        };
        answer.settling = false;
        let gate = match settle {
            Ok(crate::claude::BareItemSettle::Settled(settled)) => SwitchGate::Gated {
                gate: oauth::ensure_installable(&config, &name, refresher, None),
                settled,
            },
            held => SwitchGate::Held(BareItemOutcome::Settled(held)),
        };
        let _ = answer.sender.send(SwitchGateResult { name, gate });
        answer.sent = true;
    };
    if cfg!(test) {
        gate()
    } else {
        spawn_worker(gate)
    }
}

/// True when the live credentials diverge from the stored chain and it's not a
/// first-login adoption (must be reconciled before clearing/relinking). A
/// logged-out shell is exempt — an empty login needs no reconciling.
fn active_diverged_unsaved(active: &ProfileName) -> bool {
    crate::claude::live_diverged_and_unsaved(active).unwrap_or(false)
}

/// Toast and raise the Divergence prompt for `active` (`verb` = blocked action).
fn prompt_divergence(app: &mut App, active: String, verb: &str) {
    app.toast(
        ToastKind::Warning,
        format!("'{active}' has unsaved claude code credentials\nresolve before {verb}"),
    );
    open_divergence_modal(app, &active);
}

/// Push the Divergence prompt, first identifying (locally) which profile the
/// live login belongs to so the near-always-right "switch to it" action can
/// lead the menu. The active profile owning its own newer login is NOT a
/// sibling (that is the adopt path's self-healing domain).
fn open_divergence_modal(app: &mut App, active: &str) {
    let sibling = {
        let cfg = app.config();
        crate::actions::identify_live_login_owner(&cfg).filter(|owner| owner != active)
    };
    app.open_modal(Modal::Divergence(DivergenceForm {
        active: active.to_string(),
        sibling,
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
}

/// The TUI's settle of the bare macOS Keychain item before a write replaces
/// it, outside every lock the write takes: the source's settle, or a settled
/// item where there is no source. Blocking (a `security` read, then
/// `/profile` over HTTP), so only workers call it.
fn settle_bare_keychain_item(
    source: Option<BareItemSource>,
    writer: crate::claude::BareItemWriter,
) -> Result<crate::claude::BareItemSettle> {
    if crate::identity::upstream_active() {
        return Ok(crate::claude::BareItemSettle::Settled(
            crate::claude::BareItemSettled::default(),
        ));
    }
    match source {
        Some(source) => (source.settle)(writer),
        None => Ok(crate::claude::BareItemSettle::Settled(
            crate::claude::BareItemSettled::default(),
        )),
    }
}

/// Hold `placed` for the writer guards on the calling thread, which must be
/// the thread that runs the write: the placement is thread-local.
fn enter_bare_item_placement(placed: Option<[u8; 32]>) -> Option<crate::claude::BareItemPlacement> {
    if crate::identity::upstream_active() {
        return None;
    }
    let placement = crate::claude::BareItemPlacement::enter(placed);
    #[cfg(test)]
    probe_bare_item_placement();
    Some(placement)
}

/// Before a UI-thread write that replaces the bare item: the placement to
/// hold across the write, or `None` when the write is held. A resumed write
/// takes what its settle or the user's answer left ([`App::bare_item_ready`]);
/// a fresh one settles on a worker, and [`drain_bare_item_answers`] runs it
/// again once the settle lets it write, or asks the user.
fn bare_item_gate(
    app: &mut App,
    write: BareItemWrite,
) -> Option<Option<crate::claude::BareItemPlacement>> {
    if crate::identity::upstream_active() {
        return Some(None);
    }
    if let Some(placed) = app.bare_item_ready.take() {
        return Some(enter_bare_item_placement(placed));
    }
    if app.bare_item_source.is_none() {
        return Some(enter_bare_item_placement(None));
    }
    spawn_bare_item_settle(app, write);
    None
}

/// The toast a non-switch write shows while a worker checks the item for it:
/// a `security` read, and in a settle maybe a lookup of the login's owner
/// over the network.
const BARE_ITEM_CHECKING: &str = "checking the macOS Keychain login…";

/// Settle the bare item for `write` on a worker and post the answer.
fn spawn_bare_item_settle(app: &mut App, write: BareItemWrite) {
    let check = begin_bare_item_check(app, &write);
    let source = app.bare_item_source;
    let tx = app.bare_item_tx.clone();
    spawn_worker(move || {
        let writer = write.writer();
        let answer = BareItemAnswerGuard::new(tx, write);
        let _check = check;
        answer.send(BareItemOutcome::Settled(settle_bare_keychain_item(
            source, writer,
        )));
    });
}

/// The second line of the toast a held write's panicked worker leaves.
const BARE_ITEM_WORKER_PANICKED: &str = "tollgate hit an internal error, restart tollgate";

/// A bare-item worker's answer for its held write. Dropped unsent, as a
/// panicked worker drops it, it posts a failed settle, so the write the user
/// asked for toasts its failure rather than vanishing. Declared before the
/// worker's [`BareItemCheck`], so the check is down before this posts.
struct BareItemAnswerGuard {
    tx: std::sync::mpsc::Sender<BareItemAnswer>,
    write: Option<BareItemWrite>,
}

impl BareItemAnswerGuard {
    fn new(tx: std::sync::mpsc::Sender<BareItemAnswer>, write: BareItemWrite) -> Self {
        Self {
            tx,
            write: Some(write),
        }
    }

    fn send(mut self, outcome: BareItemOutcome) {
        if let Some(write) = self.write.take() {
            let _ = self.tx.send(BareItemAnswer { write, outcome });
        }
    }

    /// The write ran on the worker itself, so there is nothing to post.
    fn disarm(mut self) {
        self.write = None;
    }
}

impl Drop for BareItemAnswerGuard {
    fn drop(&mut self) {
        if let Some(write) = self.write.take() {
            let _ = self.tx.send(BareItemAnswer {
                write,
                outcome: BareItemOutcome::Settled(Err(anyhow::anyhow!(BARE_ITEM_WORKER_PANICKED))),
            });
        }
    }
}

/// The account a confirm about to open for `name` lands on, or a toast
/// headed `failed` when none can be claimed.
fn claim_or_toast(app: &mut App, name: &str, failed: &str) -> Option<ProfileId> {
    match claim_profile_id(&ProfileName::from(name)) {
        Ok(account) => Some(account),
        Err(e) => {
            app.toast(ToastKind::Danger, format!("{failed}\n{e:#}"));
            None
        }
    }
}

/// Show that a worker is about to check the item for `write` (a settle, or
/// `replace`'s read): a switch keeps its row's `Switching` mark, the
/// indicator its own gate shows, until its answer drains
/// ([`drain_bare_item_answers`]); every other write counts the check, with
/// [`BARE_ITEM_CHECKING`] up until every counted one answers. The worker
/// holds the returned check until its settle or read returns (the startup
/// relink's, until its relink does).
/// Without a source there is no item to check.
fn begin_bare_item_check(app: &mut App, write: &BareItemWrite) -> Option<BareItemCheck> {
    app.bare_item_source?;
    if let BareItemWrite::Switch(name) = write {
        mark_activity(&app.activity, name, ProfileActivity::Switching);
        return Some(BareItemCheck::Switch {
            activity: Arc::clone(&app.activity),
            name: name.clone(),
        });
    }
    app.bare_item_checks.fetch_add(1, Ordering::AcqRel);
    if !app
        .toasts
        .iter()
        .any(|toast| toast.body == BARE_ITEM_CHECKING)
    {
        app.toast(ToastKind::Info, BARE_ITEM_CHECKING);
    }
    Some(BareItemCheck::Counted(Arc::clone(&app.bare_item_checks)))
}

/// One running check's indicator, held by its worker; a panicked check
/// never leaves it up.
enum BareItemCheck {
    /// Counted in [`App::bare_item_checks`]; ends on drop.
    Counted(Arc<AtomicUsize>),
    /// A held switch's `Switching` mark. Its answer's drain clears it: a
    /// worker clearing it after its send could wipe the mark of the gate that
    /// answer resumes. A panicked worker's unwind clears it, before any
    /// answer its [`BareItemAnswerGuard`] posts.
    Switch {
        activity: ActivityStore,
        name: ProfileName,
    },
}

impl Drop for BareItemCheck {
    fn drop(&mut self) {
        match self {
            Self::Counted(checks) => {
                checks.fetch_sub(1, Ordering::AcqRel);
            }
            Self::Switch { activity, name } => {
                if std::thread::panicking() {
                    clear_activity(activity, name);
                }
            }
        }
    }
}

/// Land the bare-item workers' answers for held writes. Waits while another
/// modal is open, so a question never stacks under one; an exit wait drops
/// them, since it starts no new write.
fn drain_bare_item_answers(app: &mut App) {
    loop {
        if blocking_modal_open(app) && !app.exit_wait {
            return;
        }
        let Ok(BareItemAnswer { write, outcome }) = app.bare_item_rx.try_recv() else {
            return;
        };
        if let BareItemWrite::Switch(name) = &write {
            clear_activity(&app.activity, name);
        }
        if !app.exit_wait {
            land_bare_item_outcome(app, write, outcome);
        }
    }
}

/// A settle that lets the write go shows what it saved (its carry has
/// landed) and runs the write again; one only the user can place asks; a
/// failed one toasts the write's own failure.
fn land_bare_item_outcome(app: &mut App, write: BareItemWrite, outcome: BareItemOutcome) {
    match outcome {
        BareItemOutcome::Settled(Ok(crate::claude::BareItemSettle::Settled(settled))) => {
            if let Some(notice) = settled.notice {
                app.toast(ToastKind::Warning, notice);
            }
            resume_bare_item_write(app, write, settled.placed);
        }
        BareItemOutcome::Settled(Ok(crate::claude::BareItemSettle::NeedsChoice(unplaced))) => {
            app.open_modal(Modal::BareItem(BareItemForm::new(unplaced, write)));
        }
        BareItemOutcome::Settled(Err(e)) => {
            app.toast(ToastKind::Danger, format!("{}\n{e:#}", write.failed()));
        }
        BareItemOutcome::Replace(placed) => resume_bare_item_write(app, write, placed),
        BareItemOutcome::NameNeeded { login, account } => {
            app.bare_item_naming = Some(BareItemNaming {
                login,
                account,
                write,
            });
            app.open_modal(Modal::NamePrompt(NamePromptForm {
                input: InputState::new(""),
                action: NamePromptAction::SaveBareItem,
            }));
        }
        BareItemOutcome::OwnerNeeded(owners, accounts, login) => {
            app.open_modal(Modal::BareItemOwner(BareItemOwnerForm::new(
                owners, accounts, login, write,
            )));
        }
    }
}

/// Run a held write again: its gate takes `placed` instead of settling.
fn resume_bare_item_write(app: &mut App, write: BareItemWrite, placed: Option<[u8; 32]>) {
    if guest_refused(app) {
        return;
    }
    app.bare_item_ready = Some(placed);
    match write {
        BareItemWrite::BootRelink(name) => {
            if app.config().is_active(&name)
                && let Some(_placed) = bare_item_gate(app, BareItemWrite::BootRelink(name.clone()))
            {
                let _ = link_profile_credentials(&name);
                app.refresh_unsaved_live_login();
            }
        }
        BareItemWrite::Switch(name) => spawn_switch_gate(app, name, oauth::refresh_result),
        BareItemWrite::SwitchOff => perform_switch_off(app),
        BareItemWrite::ClearSessionToken(name) => perform_clear_session_token(app, &name),
        BareItemWrite::Rename => {
            commit_rename(app);
        }
        BareItemWrite::NewAccount => commit_new_account(app),
        BareItemWrite::Delete { name, force } => finish_delete(app, &name, force),
        BareItemWrite::CaptureOverwrite {
            snapshot,
            name,
            account,
            from_divergence,
        } => run_confirm_action(
            app,
            ConfirmAction::CaptureOverwrite(snapshot, name, account, from_divergence),
        ),
        BareItemWrite::AdoptDivergence {
            snapshot,
            name,
            account,
        } => {
            run_confirm_action(app, ConfirmAction::AdoptDivergence(snapshot, name, account));
        }
        BareItemWrite::BlankCredentials(name) => {
            run_confirm_action(app, ConfirmAction::BlankCredentials(name));
        }
        BareItemWrite::DiscardDivergence(name) => run_discard_divergence(app, &name),
        BareItemWrite::DivergenceOverwrite(active) => {
            run_divergence_choice(app, &active, DivergenceChoice::Overwrite);
        }
        BareItemWrite::CaptureName {
            name,
            snapshot,
            from_divergence,
        } => commit_capture_name(app, name, snapshot, from_divergence),
        BareItemWrite::Login {
            name,
            account,
            snapshot,
        } => apply_login_snapshot(app, name, account, *snapshot),
    }
    app.bare_item_ready = None;
}

/// [`Modal::BareItem`]: `↑`/`↓` pick, `↵` answers the picked choice, `s`/`p`/`c`
/// answer directly, `esc`/`q` cancel.
fn handle_bare_item_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::BareItem(form)) = app.modals.last_mut() else {
        return;
    };
    let last = BareItemChoice::ALL.len() - 1;
    let choice = match key.code {
        KeyCode::Up => {
            form.cursor = if form.cursor == 0 {
                last
            } else {
                form.cursor - 1
            };
            return;
        }
        KeyCode::Down => {
            form.cursor = if form.cursor >= last {
                0
            } else {
                form.cursor + 1
            };
            return;
        }
        KeyCode::PageUp => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                &[],
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
            return;
        }
        KeyCode::PageDown => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
            return;
        }
        KeyCode::Enter => BareItemChoice::ALL[form.cursor.min(last)],
        KeyCode::Esc | KeyCode::Char('q') => BareItemChoice::Cancel,
        KeyCode::Char(c) => {
            let c = c.to_ascii_lowercase();
            let Some(choice) = BareItemChoice::ALL.into_iter().find(|ch| ch.key() == c) else {
                return;
            };
            choice
        }
        _ => return,
    };
    let Some(Modal::BareItem(form)) = app.pop_modal() else {
        return;
    };
    answer_bare_item(app, form.write, choice);
}

/// `save` and `replace` read the item as the answer finds it, on a worker:
/// `save` puts its login into the profile cached with its account (or asks
/// for a new one's name) and settles again; `replace` hands the write the
/// key its guards replace the item by. `cancel` writes nothing.
fn answer_bare_item(app: &mut App, write: BareItemWrite, choice: BareItemChoice) {
    if guest_refused(app) {
        return;
    }

    let source = app.bare_item_source;
    let tx = app.bare_item_tx.clone();
    match choice {
        BareItemChoice::Cancel => {}
        BareItemChoice::Save => {
            let check = begin_bare_item_check(app, &write);
            spawn_worker(move || {
                let writer = write.writer();
                let answer = BareItemAnswerGuard::new(tx, write);
                let _check = check;
                let item = source.and_then(|source| (source.read)().map(|item| (source, item)));
                let saved = match item {
                    Some((source, item)) => match (source.save)(&item) {
                        crate::claude::BareItemSave::Nothing => Ok(None),
                        crate::claude::BareItemSave::Owner(owner) => {
                            crate::claude::claim_bare_item_owner(&owner).and_then(|account| {
                                crate::claude::save_bare_item_into(&owner, &account, &item, writer)
                            })
                        }
                        crate::claude::BareItemSave::Owners(first, second) => {
                            // Claimed before the question: the pick lands only
                            // on the account each name held when it was asked
                            // about.
                            match crate::claude::claim_bare_item_owner(&first).and_then(|a| {
                                crate::claude::claim_bare_item_owner(&second).map(|b| [a, b])
                            }) {
                                Ok(accounts) => {
                                    answer.send(BareItemOutcome::OwnerNeeded(
                                        [first, second],
                                        accounts,
                                        crate::claude::bare_item_login_key(&item),
                                    ));
                                    return;
                                }
                                Err(e) => Err(e),
                            }
                        }
                        crate::claude::BareItemSave::NewProfile { login, account } => {
                            answer.send(BareItemOutcome::NameNeeded { login, account });
                            return;
                        }
                    },
                    None => Ok(None),
                };
                answer.send(settle_after_save(source, writer, saved));
            });
        }
        BareItemChoice::Replace => {
            let check = begin_bare_item_check(app, &write);
            spawn_worker(move || {
                let answer = BareItemAnswerGuard::new(tx, write);
                let _check = check;
                let placed = source
                    .and_then(|source| (source.read)())
                    .as_ref()
                    .and_then(crate::claude::bare_item_replacement);
                answer.send(BareItemOutcome::Replace(placed));
            });
        }
    }
}

/// [`Modal::BareItemOwner`]: `↑`/`↓` pick, `↵` saves the login into the
/// picked profile on a worker and settles again, `esc`/`q` cancel the held
/// write.
fn handle_bare_item_owner_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::BareItemOwner(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Up | KeyCode::Down => form.cursor = 1 - form.cursor.min(1),
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
        }
        KeyCode::Enter => {
            let Some(Modal::BareItemOwner(form)) = app.pop_modal() else {
                return;
            };
            let [first, second] = form.owners;
            let [first_account, second_account] = form.accounts;
            let (owner, account) = if form.cursor == 0 {
                (first, first_account)
            } else {
                (second, second_account)
            };
            save_bare_item_into_owner(app, form.write, owner, account, form.login);
        }
        _ => {}
    }
}

/// Carry the item's login into `owner` on a worker while the item still
/// holds `login`, the one the question was about, then settle again for the
/// held write. An item holding another login by now is never saved into
/// `owner`, whose account it may not be: the settle asks about it afresh. The
/// save lands only while `owner` still holds `account`, claimed when the
/// question opened.
fn save_bare_item_into_owner(
    app: &mut App,
    write: BareItemWrite,
    owner: ProfileName,
    account: ProfileId,
    login: Option<[u8; 32]>,
) {
    if guest_refused(app) {
        return;
    }

    let source = app.bare_item_source;
    let tx = app.bare_item_tx.clone();
    let check = begin_bare_item_check(app, &write);
    spawn_worker(move || {
        let writer = write.writer();
        let answer = BareItemAnswerGuard::new(tx, write);
        let _check = check;
        let saved = match source.and_then(|source| (source.read)()) {
            Some(item) if login.is_some() && crate::claude::bare_item_login_key(&item) == login => {
                crate::claude::save_bare_item_into(&owner, &account, &item, writer)
            }
            Some(_) | None => Ok(None),
        };
        answer.send(settle_after_save(source, writer, saved));
    });
}

/// What a `save` posts once its carry ran: a store that kept a later login
/// lets the held write go on over the item (`placed`), as the settle's own
/// carry does, since settling again would ask the same question; otherwise
/// the settle runs again.
fn settle_after_save(
    source: Option<BareItemSource>,
    writer: crate::claude::BareItemWriter,
    saved: Result<Option<[u8; 32]>>,
) -> BareItemOutcome {
    if crate::identity::upstream_active() {
        return BareItemOutcome::Settled(Ok(crate::claude::BareItemSettle::Settled(
            crate::claude::BareItemSettled::default(),
        )));
    }
    BareItemOutcome::Settled(saved.and_then(|kept| match kept {
        Some(placed) => Ok(crate::claude::BareItemSettle::Settled(
            crate::claude::BareItemSettled {
                notice: None,
                placed: Some(placed),
            },
        )),
        None => settle_bare_keychain_item(source, writer),
    }))
}

/// Complete a switch on the UI thread: divergence guard, relink via
/// `switch_profile` holding the bare-item placement the gate's settle left,
/// token-snapshot refresh. No HTTP — the bare item settled and the AUTH-1
/// gate answered on the worker (or the target is the already-active
/// exemption) before this runs.
fn finalize_switch(app: &mut App, name: &ProfileName, placed: Option<[u8; 32]>) {
    if guest_refused(app) {
        clear_activity(&app.activity, name);
        return;
    }

    // Guard a diverged outgoing active: `switch_profile` would no-op the
    // snapshot and then `link_profile_credentials` would bail on the regular
    // file, stranding the fresh `/login` chain. Raise the Divergence modal so
    // the user cleans up first; first-login adoption stays a clean switch.
    let outgoing = app.config().state.active_profile.as_ref().cloned();
    if let Some(active) = outgoing
        && active != *name
        && active_diverged_unsaved(&active)
    {
        clear_activity(&app.activity, name);
        prompt_divergence(app, active.to_string(), "switching");
        return;
    }
    let _placed = enter_bare_item_placement(placed);
    let result = switch_profile(&app.config, name);
    clear_activity(&app.activity, name);
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.apply_forgets();
            app.refresh_unsaved_live_login();
            app.toast(ToastKind::Success, format!("switched to '{name}'"));
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!("switch failed\n{}", escape_control(&error_toast_detail(&e))),
        ),
    }
}

/// Turn off all accounts on the UI thread. Mirrors `finalize_switch`'s
/// divergence guard: an unsaved `/login` must be resolved before clearing live
/// credentials. No HTTP.
fn perform_switch_off(app: &mut App) {
    if guest_refused(app) {
        return;
    }

    let Some(active) = app.config().state.active_profile.as_ref().cloned() else {
        return;
    };
    if active_diverged_unsaved(&active) {
        prompt_divergence(app, active.to_string(), "switching off");
        return;
    }
    let Some(_placed) = bare_item_gate(app, BareItemWrite::SwitchOff) else {
        return;
    };
    let result = switch_off(&app.config);
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.apply_forgets();
            app.refresh_unsaved_live_login();
            app.toast(
                ToastKind::Warning,
                "all accounts spent\nswitched off to halt usage".to_string(),
            );
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "switch-off failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

/// Read the diverged live credentials file into a snapshot, or toast + `None`
/// when there's nothing to capture. A divergence names that file, so its
/// prompts capture it and never the macOS Keychain item: the item may hold
/// another account's unsaved login. A plain file read, fine on the UI thread.
fn capture_live_or_toast(app: &mut App) -> Option<CaptureSnapshot> {
    if guest_refused(app) {
        return None;
    }
    snapshot_or_toast(app, capture_live_file_snapshot())
}

/// `snapshot`, or toast + `None` when there's nothing to capture. An
/// all-empty snapshot would persist a credential-less profile behind a
/// success toast (issue #1 — on macOS CC keeps its login in the Keychain, so
/// the credentials file is absent).
fn snapshot_or_toast(app: &mut App, snapshot: Result<CaptureSnapshot>) -> Option<CaptureSnapshot> {
    let snapshot = match snapshot {
        Ok(s) => s,
        Err(e) => {
            app.toast(ToastKind::Danger, format!("capture failed\n{e:#}"));
            return None;
        }
    };
    if snapshot_is_empty(&snapshot) {
        // The Keychain caveat only makes sense on macOS, where a live login can
        // hide in the Keychain tollgate doesn't read; elsewhere it's noise.
        let msg = if cfg!(target_os = "macos") {
            "no live login found\nnothing to capture (macos keychain isn't supported yet)"
        } else {
            "no live login found\nnothing to capture"
        };
        app.toast(ToastKind::Danger, msg);
        return None;
    }
    Some(snapshot)
}

/// The `+ new` form's `+ capture current login`: the login in use, the macOS
/// Keychain item's first ([`capture_snapshot`]), a `security` call, so it
/// runs on a worker and lands through [`drain_capture_login`]. A test reads
/// it in place, the [`App::refresh_unsaved_live_login`] precedent.
fn spawn_capture_login(app: &mut App) {
    if guest_refused(app) {
        return;
    }

    if cfg!(test) {
        land_capture_login(app, capture_snapshot());
        return;
    }
    let tx = app.capture_login_tx.clone();
    spawn_worker(move || {
        let _ = tx.send(capture_snapshot());
    });
}

/// Land the capture row's worker answers. Waits while a modal is open, so
/// the mint-replace confirm never stacks over another; an exit wait drops
/// them.
fn drain_capture_login(app: &mut App) {
    loop {
        if blocking_modal_open(app) && !app.exit_wait {
            return;
        }
        let Ok(snapshot) = app.capture_login_rx.try_recv() else {
            return;
        };
        if !app.exit_wait {
            land_capture_login(app, snapshot);
        }
    }
}

/// Stash the captured login into the open `+ new` draft, refusing a login a
/// profile owns and confirming before it replaces a stashed mint.
fn land_capture_login(app: &mut App, snapshot: Result<CaptureSnapshot>) {
    if guest_refused(app) {
        return;
    }

    // The row only renders on the `+ new` form, where a draft is always
    // open; the empty and owned states are what HIDE it, so hitting one
    // here means the live login moved since the flag was computed.
    let Some(snapshot) = snapshot_or_toast(app, snapshot) else {
        // The login emptied since the flag was computed; drop the row.
        app.refresh_unsaved_live_login();
        return;
    };
    let owner = {
        let cfg = app.config();
        find_matching_oauth_profile(&cfg, snapshot.credentials.as_ref())
    };
    if let Some(owner) = owner {
        // Refuse, not confirm: a NEW account over a login another
        // profile owns is the duplicate `overwrite_captured_profile`
        // exists to prevent, and this row's whole purpose is the
        // unowned case.
        app.toast(
            ToastKind::Danger,
            format!(
                "these credentials already belong to '{owner}'\nswitch to it with:  {}",
                prose::cmd(&format!("tollgate {owner}"))
            ),
        );
        app.refresh_unsaved_live_login();
        return;
    }
    let mint_stashed = app.config_draft.as_ref().is_some_and(|d| {
        d.editing_name.is_none() && matches!(d.captured_login, Some(DraftLogin::Mint(_)))
    });
    if mint_stashed {
        // The browser round-trip a mint cost can't be redone for free;
        // gate replacing it, mirroring the re-login gate.
        app.open_modal(Modal::Confirm(ConfirmState {
            message: "replace the logged-in mint?".to_string(),
            detail: Some("the browser login you already captured will be dropped".to_string()),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::CaptureOverMintStash(Box::new(snapshot)),
        }));
        return;
    }
    if matches!(
        stash_new_form_login(app, DraftLogin::LiveLogin(Box::new(snapshot))),
        StashOutcome::Staged
    ) {
        app.toast(
            ToastKind::Success,
            "current login captured\ncreate account saves it",
        );
    }
}

fn begin_capture(app: &mut App, from_divergence: bool) {
    let Some(snapshot) = capture_live_or_toast(app) else {
        return;
    };
    let existing_match = {
        let cfg = app.config();
        find_matching_oauth_profile(&cfg, snapshot.credentials.as_ref())
    };
    if let Some(existing) = existing_match {
        app.open_modal(Modal::Confirm(ConfirmState {
            message: format!("these credentials already belong to '{existing}'."),
            detail: Some("capture anyway?".to_string()),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::CaptureConflict(Box::new(snapshot), from_divergence),
        }));
        return;
    }
    app.open_modal(Modal::CaptureName(CaptureNameForm {
        snapshot: Box::new(snapshot),
        input: InputState::new(""),
        from_divergence,
    }));
}

/// The outcome of landing a finished login into the live `+ new` draft.
enum StashOutcome {
    /// The stash landed; the cursor parked on `create account`.
    Staged,
    /// The `+ new` form is closed; nothing landed.
    FormClosed,
    /// The form's type no longer matches the stash's harness; dropped unused.
    WrongType,
}

/// Which harness a stash belongs to, so a late login result stages only into a
/// form whose type still matches it (a claude mint never lands in a codex or
fn draft_login_harness(stash: &DraftLogin) -> NewAccountType {
    match stash {
        DraftLogin::Mint(_) | DraftLogin::LiveLogin(_) => NewAccountType::Claude,
        DraftLogin::CodexMint(_) | DraftLogin::CodexCapture => NewAccountType::Codex,
    }
}

/// Land a login stash in the `+ new` draft (`+ login`'s finished mint, or
/// `+ capture current login`'s snapshot): the live form, else the form
/// suspended in `new_account_draft`, which still owns the login it started.
/// On the live form the cursor parks on `create account`, the one step left.
/// [`StashOutcome::FormClosed`] when the form was closed in between;
/// [`StashOutcome::WrongType`] when the form's type no longer matches the
/// stash's harness — a late result the type cycle already moved past, dropped
/// unused (the chain never written).
fn stash_new_form_login(app: &mut App, stash: DraftLogin) -> StashOutcome {
    let harness = draft_login_harness(&stash);
    let live = app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.editing_name.is_none());
    let slot = if live {
        app.config_draft.as_mut()
    } else {
        app.new_account_draft.as_mut()
    };
    let Some(draft) = slot else {
        return StashOutcome::FormClosed;
    };
    if draft.account_type != harness {
        return StashOutcome::WrongType;
    }
    draft.captured_login = Some(stash);
    // The stage it asked for lands: a stale "log in or capture first" refusal
    // no longer describes the form.
    draft.create_refusal = None;
    if live
        && let Some(idx) = config_rows(app)
            .iter()
            .position(|r| *r == ConfigRow::Create)
    {
        app.config_action_cursor = idx;
    }
    StashOutcome::Staged
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ChainItemKind {
    Member(usize),
    Add,
}

pub(crate) fn chain_items(app: &App) -> Vec<ChainItemKind> {
    let cfg = app.config();
    let mut items: Vec<ChainItemKind> = cfg
        .state
        .fallback_chain
        .iter()
        .enumerate()
        .map(|(i, _)| ChainItemKind::Member(i))
        .collect();
    // A disabled, unchained profile is never an add-picker candidate (see
    // `chain_candidates`) — excluding it here too keeps `+ add` from showing
    // when it would open onto an empty picker.
    let any_unchained = cfg
        .profiles
        .iter()
        .any(|p| !p.is_disabled() && !cfg.state.fallback_chain.iter().any(|c| c == &p.name));
    if any_unchained {
        items.push(ChainItemKind::Add);
    }
    items
}

/// Detail rows for a chain member: threshold stepper, last-resort/preferred
/// toggles, preferred-days presets + day picker, remove.
pub(crate) const FALLBACK_ROWS: [FallbackRow; 10] = [
    FallbackRow::Threshold,
    FallbackRow::WeeklyAt,
    FallbackRow::CheckWeekly,
    FallbackRow::CheckScoped,
    FallbackRow::CheckCreditCap,
    FallbackRow::LastResort,
    FallbackRow::Preferred,
    FallbackRow::PreferredDays,
    FallbackRow::MaxSpend,
    FallbackRow::Remove,
];

/// Rows on the program-wide Config tab, in display order. Related knobs sit
/// together instead of interleaving halt above detection; [`GlobalConfigRow::band`]
/// names each run, and the renderer turns a band change into an eyebrow header.
pub(crate) const GLOBAL_CONFIG_ROWS: [GlobalConfigRow; 20] = [
    GlobalConfigRow::Theme,
    GlobalConfigRow::ResetShape,
    GlobalConfigRow::ClockNotation,
    GlobalConfigRow::HomeTab,
    GlobalConfigRow::DivergenceDefault,
    GlobalConfigRow::PreserveNonLoginKeys,
    GlobalConfigRow::RefreshInterval,
    GlobalConfigRow::RefreshSpentAccounts,
    GlobalConfigRow::ContextNudge,
    GlobalConfigRow::AutoStartQueue,
    GlobalConfigRow::PreemptiveRotation,
    GlobalConfigRow::AutoUpdate,
    GlobalConfigRow::WeeklyThreshold,
    GlobalConfigRow::BurnAware,
    GlobalConfigRow::WalkOrder,
    GlobalConfigRow::BurnFloor,
    GlobalConfigRow::BurnHorizon,
    GlobalConfigRow::SwitchOffWhenSpent,
    GlobalConfigRow::SpendBudget,
    GlobalConfigRow::SwitchOffWhenBudgetSpent,
];

impl GlobalConfigRow {
    /// The concern this row belongs to. Rows sharing a band are contiguous in
    /// [`GLOBAL_CONFIG_ROWS`]; the Config renderer opens each run with an eyebrow
    /// header, so a band that stops being contiguous would render twice — which
    /// is what `config_bands_stay_contiguous` pins.
    pub(crate) fn band(self) -> &'static str {
        match self {
            GlobalConfigRow::Theme
            | GlobalConfigRow::ResetShape
            | GlobalConfigRow::ClockNotation
            | GlobalConfigRow::HomeTab => "appearance",
            GlobalConfigRow::DivergenceDefault
            | GlobalConfigRow::PreserveNonLoginKeys
            | GlobalConfigRow::RefreshInterval
            | GlobalConfigRow::RefreshSpentAccounts
            | GlobalConfigRow::ContextNudge
            | GlobalConfigRow::AutoStartQueue
            | GlobalConfigRow::PreemptiveRotation
            | GlobalConfigRow::AutoUpdate => "scheduler",
            GlobalConfigRow::WeeklyThreshold
            | GlobalConfigRow::BurnAware
            | GlobalConfigRow::WalkOrder
            | GlobalConfigRow::BurnFloor
            | GlobalConfigRow::BurnHorizon
            | GlobalConfigRow::SwitchOffWhenSpent => "auto-switch",
            GlobalConfigRow::SpendBudget | GlobalConfigRow::SwitchOffWhenBudgetSpent => {
                "extra usage"
            }
        }
    }
}

/// Config tab keymap (enumerated rows only, per the unified value-row grammar):
/// ↑↓ walks rows; space cycles every row's value forward, wrapping the top
/// value back to the first; ⏎ opens the refresh-interval, context-nudge and
/// weekly-threshold custom-value editors and otherwise mirrors space. No row
/// here binds `+`/`-`
/// (that's reserved for the Fallback tab's continuous `rotate at` threshold).
fn handle_global_config_key(app: &mut App, key: KeyEvent) {
    let last = GLOBAL_CONFIG_ROWS.len() - 1;
    app.global_config_cursor = app.global_config_cursor.min(last);
    match key.code {
        KeyCode::Up => {
            app.global_config_cursor = if app.global_config_cursor == 0 {
                last
            } else {
                app.global_config_cursor - 1
            };
        }
        KeyCode::Down => {
            app.global_config_cursor = if app.global_config_cursor >= last {
                0
            } else {
                app.global_config_cursor + 1
            };
        }
        KeyCode::PageUp => {
            let starts = app.global_config_row_starts.borrow();
            let block_ends = app.global_config_block_ends.borrow();
            let tips = app.global_config_tips.borrow();
            app.global_config_cursor = page_up_cursor_by_rows(
                app.global_config_cursor,
                &starts,
                &block_ends,
                &tips,
                app.global_config_viewport.get(),
                app.global_config_offset.get(),
                app.global_config_total.get(),
            );
        }
        KeyCode::PageDown => {
            let starts = app.global_config_row_starts.borrow();
            let block_ends = app.global_config_block_ends.borrow();
            app.global_config_cursor = page_down_cursor_by_rows(
                app.global_config_cursor,
                &starts,
                &block_ends,
                app.global_config_viewport.get(),
                app.global_config_offset.get(),
                app.global_config_total.get(),
            );
        }
        KeyCode::Char(' ') => {
            run_global_config_row(app, GLOBAL_CONFIG_ROWS[app.global_config_cursor]);
        }
        KeyCode::Enter => {
            let row = GLOBAL_CONFIG_ROWS[app.global_config_cursor];
            if row == GlobalConfigRow::RefreshInterval {
                begin_refresh_interval_edit(app);
            } else if row == GlobalConfigRow::ContextNudge {
                begin_context_nudge_edit(app);
            } else if row == GlobalConfigRow::WeeklyThreshold {
                begin_weekly_threshold_edit(app);
            } else {
                run_global_config_row(app, row);
            }
        }
        _ => {}
    }
}

/// Apply space (or ⏎ on non-refresh rows) on a Config-tab row: cycle the theme
/// tier, flip wrap-off, or step the refresh interval forward through presets
/// (wrapping past the top preset back to the first).
fn run_global_config_row(app: &mut App, row: GlobalConfigRow) {
    match row {
        GlobalConfigRow::Theme => cycle_theme(app),
        GlobalConfigRow::ResetShape => cycle_reset_display(app),
        GlobalConfigRow::HomeTab => cycle_home_tab(app),
        // Inert while the countdown is relative (rendered dimmed): no surface
        // draws a clock then, so the notation would decide nothing.
        GlobalConfigRow::ClockNotation => {
            if app.config().state.reset_display().shows_clock() {
                cycle_clock_format(app);
            }
        }
        GlobalConfigRow::DivergenceDefault => cycle_divergence_default(app),
        GlobalConfigRow::SwitchOffWhenSpent => toggle_wrap_off(app),
        GlobalConfigRow::WeeklyThreshold => step_weekly_threshold(app),
        GlobalConfigRow::RefreshInterval => step_refresh_interval(app),
        GlobalConfigRow::ContextNudge => step_context_nudge(app),
        GlobalConfigRow::BurnAware => toggle_burn_aware_switching(app),
        GlobalConfigRow::WalkOrder => cycle_walk_order(app),
        // Inert while burn-aware is off (rendered dimmed): the floor/cap only
        // shape the projection, which the static path never runs.
        GlobalConfigRow::BurnFloor => {
            if app.config().state.burn_aware_switching {
                step_burn_floor(app);
            }
        }
        GlobalConfigRow::BurnHorizon => {
            if app.config().state.burn_aware_switching {
                step_burn_horizon(app);
            }
        }
        GlobalConfigRow::SpendBudget => toggle_spend_budget_switching(app),
        // Inert while spend budget is off (rendered dimmed): decides no halt when
        // nothing spends, so it stays a true disabled row — the key is a no-op.
        GlobalConfigRow::SwitchOffWhenBudgetSpent => {
            if app.config().state.spend_budget_switching {
                toggle_budget_wrap_off(app);
            }
        }
        GlobalConfigRow::PreemptiveRotation => toggle_preemptive_rotation(app),
        GlobalConfigRow::AutoUpdate => toggle_auto_update(app),
        GlobalConfigRow::PreserveNonLoginKeys => toggle_preserve_non_login_keys(app),
        GlobalConfigRow::RefreshSpentAccounts => toggle_refresh_spent_accounts(app),
        // Inert while no account has `auto_start` on (rendered dimmed): a
        // queue with no possible member spaces nothing, so it stays a true
        // disabled row — the key is a no-op.
        GlobalConfigRow::AutoStartQueue => {
            if app.config().profiles.iter().any(|p| p.auto_start) {
                toggle_auto_start_queue(app);
            }
        }
    }
}

/// Cycle the active theme tier, persist it to `[theme]`, and live-swap the
/// palette so the next frame renders in the new tier without a restart.
fn cycle_theme(app: &mut App) {
    let next = match theme::tier() {
        theme::Tier::Full => theme::Tier::Compatible,
        theme::Tier::Compatible => theme::Tier::Full,
    };
    let name = match next {
        theme::Tier::Full => ThemeName::Full,
        theme::Tier::Compatible => ThemeName::Compatible,
    };
    {
        let mut cfg = app.config();
        cfg.state.theme = Some(name);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
    theme::set_tier(next);
}

/// An open edit on the Fallback member card, pinned to the member it opened on
/// by NAME. A reload can reorder or shrink the chain under an open editor, so a
/// write resolved through `chain_cursor` lands on whichever member took the
/// slot; every write goes to `member` instead, and [`repin_member_edit`] moves
/// `chain_cursor` after it, or closes the edit and returns focus to the chain
/// list once it left the chain or its account left the roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberEdit<T> {
    pub(crate) member: ProfileName,
    pub(crate) state: T,
    /// The value a typed field opened from (its buffer's seed); `None` for the
    /// day picker and the armed remove, which hold no buffer.
    pub(crate) seed: Option<String>,
}

/// What is open on the member card. One at a time: every typed field and the
/// picker own the keyboard, and the armed remove disarms on any row move.
#[derive(Debug, Clone)]
pub(crate) enum CardEdit {
    /// The `rotate at` field. `+`/`-` still step the value while closed.
    Threshold(InputState),
    /// The `weekly at` override field; an EMPTY commit clears the override.
    Weekly(InputState),
    /// The `max spend` ceiling field.
    MaxSpend(InputState),
    /// The `preferred days` chip picker. No draft: each space saves at once.
    Days(DayPicker),
    /// The first ⏎ on `remove`; the second removes this member.
    ArmedRemove,
}

impl CardEdit {
    /// The row this edit is open on.
    pub(crate) fn row(&self) -> FallbackRow {
        match self {
            Self::Threshold(_) => FallbackRow::Threshold,
            Self::Weekly(_) => FallbackRow::WeeklyAt,
            Self::MaxSpend(_) => FallbackRow::MaxSpend,
            Self::Days(_) => FallbackRow::PreferredDays,
            Self::ArmedRemove => FallbackRow::Remove,
        }
    }

    /// The typed buffer, when this edit is a typed field.
    pub(crate) fn input(&self) -> Option<&InputState> {
        match self {
            Self::Threshold(input) | Self::Weekly(input) | Self::MaxSpend(input) => Some(input),
            Self::Days(_) | Self::ArmedRemove => None,
        }
    }

    fn input_mut(&mut self) -> Option<&mut InputState> {
        match self {
            Self::Threshold(input) | Self::Weekly(input) | Self::MaxSpend(input) => Some(input),
            Self::Days(_) | Self::ArmedRemove => None,
        }
    }
}

/// The `preferred days` chip picker: the chip under the caret (a
/// `WEEKDAYS_ALL` index), and whether this descend already raised the
/// claims-nothing warning, which it raises once rather than once per toggle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DayPicker {
    pub(crate) cursor: usize,
    pub(crate) warned: bool,
}

/// Fallback footer hint derived from current focus + selection + edit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackHint {
    Empty,
    ChainMember,
    ChainAdd,
    DetailThreshold,
    DetailWeeklyAt,
    DetailCheckWeekly,
    DetailCheckScoped,
    DetailCheckCreditCap,
    DetailLastResort,
    DetailPreferred,
    DetailPreferredDays,
    DetailMaxSpend,
    DetailRemove,
    DetailRemoveArmed,
    DetailAdd,
}

/// Resolve the Fallback tab's footer hint while no editor owns the keyboard
/// ([`keyboard_owner`]); an open editor's hints are its own.
pub(crate) fn fallback_hint(app: &App) -> FallbackHint {
    if chain_items(app).is_empty() {
        return FallbackHint::Empty;
    }
    match app.fallback_focus {
        FallbackFocus::Chain => match selected_chain_member(app) {
            Some(_) => FallbackHint::ChainMember,
            None => FallbackHint::ChainAdd,
        },
        FallbackFocus::Detail => {
            if selected_chain_member(app).is_none() {
                return FallbackHint::DetailAdd;
            }
            let cursor = app.fallback_detail_cursor.min(FALLBACK_ROWS.len() - 1);
            match FALLBACK_ROWS[cursor] {
                FallbackRow::Threshold => FallbackHint::DetailThreshold,
                FallbackRow::WeeklyAt => FallbackHint::DetailWeeklyAt,
                FallbackRow::CheckWeekly => FallbackHint::DetailCheckWeekly,
                FallbackRow::CheckScoped => FallbackHint::DetailCheckScoped,
                FallbackRow::CheckCreditCap => FallbackHint::DetailCheckCreditCap,
                FallbackRow::LastResort => FallbackHint::DetailLastResort,
                FallbackRow::Preferred => FallbackHint::DetailPreferred,
                FallbackRow::PreferredDays => FallbackHint::DetailPreferredDays,
                FallbackRow::MaxSpend => FallbackHint::DetailMaxSpend,
                FallbackRow::Remove
                    if app
                        .fallback_edit
                        .as_ref()
                        .is_some_and(|e| matches!(e.state, CardEdit::ArmedRemove)) =>
                {
                    FallbackHint::DetailRemoveArmed
                }
                FallbackRow::Remove => FallbackHint::DetailRemove,
            }
        }
    }
}

/// Fallback tab keymap; delegates to chain or detail handler.
fn handle_fallback_key(app: &mut App, key: KeyEvent) {
    match app.fallback_focus {
        FallbackFocus::Chain => handle_fallback_chain_key(app, key),
        FallbackFocus::Detail => handle_fallback_detail_key(app, key),
    }
}

/// Returns the chain row that should be highlighted when entering the fallback tab.
/// If the currently-selected profile (`profile_cursor`) is a member of the chain,
/// returns its position there; otherwise 0.
fn chain_cursor_for_profile(app: &App) -> usize {
    let cfg = app.config();
    let selected_name = cfg
        .profiles
        .get(app.profile_cursor.read())
        .map(|p| p.name.as_str());
    if let Some(name) = selected_name
        && let Some(pos) = cfg.state.fallback_chain.iter().position(|c| c == name)
    {
        return pos;
    }
    0
}

/// If the current `chain_cursor` points at a `Member`, sync `profile_cursor` to that
/// member's index in the profile list. Leaves `profile_cursor` unchanged on `Add` or
/// when the name is not found (e.g. empty chain).
fn sync_profile_from_chain(app: &mut App) {
    let chain_pos = app.chain_cursor;
    let profile_idx = {
        let cfg = app.config();
        match cfg.state.fallback_chain.get(chain_pos) {
            Some(name) => cfg.profiles.iter().position(|p| p.name == *name),
            None => None,
        }
    };
    if let Some(idx) = profile_idx {
        app.profile_cursor.set(idx);
    }
}

fn handle_fallback_chain_key(app: &mut App, key: KeyEvent) {
    let last = chain_items(app).len().saturating_sub(1);
    match key.code {
        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => reorder_chain_member(app, -1),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            reorder_chain_member(app, 1)
        }
        KeyCode::Up => {
            app.chain_cursor = if app.chain_cursor == 0 {
                last
            } else {
                app.chain_cursor - 1
            };
            sync_profile_from_chain(app);
        }
        KeyCode::Down => {
            app.chain_cursor = if app.chain_cursor >= last {
                0
            } else {
                app.chain_cursor + 1
            };
            sync_profile_from_chain(app);
        }
        KeyCode::PageUp => {
            app.chain_cursor = selector_page_up(app.chain_cursor, &app.chain_selector);
            sync_profile_from_chain(app);
        }
        KeyCode::PageDown => {
            app.chain_cursor = selector_page_down(app.chain_cursor, &app.chain_selector);
            sync_profile_from_chain(app);
        }
        KeyCode::Enter => enter_fallback_detail(app),
        _ => {}
    }
}

pub(crate) fn next_divergence_default(
    current: Option<DivergenceChoice>,
) -> Option<DivergenceChoice> {
    match current {
        None => Some(DivergenceChoice::Overwrite),
        Some(DivergenceChoice::Overwrite) => Some(DivergenceChoice::NewProfile),
        Some(DivergenceChoice::NewProfile) => Some(DivergenceChoice::Discard),
        Some(DivergenceChoice::Discard) => None,
    }
}

/// Reset-shape cycle order, wrapping back to the stock relative form.
pub(crate) fn next_reset_display(current: ResetDisplay) -> ResetDisplay {
    match current {
        ResetDisplay::Relative => ResetDisplay::Clock,
        ResetDisplay::Clock => ResetDisplay::Both,
        ResetDisplay::Both => ResetDisplay::Relative,
    }
}

fn cycle_reset_display(app: &mut App) {
    let next = next_reset_display(app.config().state.reset_display());
    {
        let mut cfg = app.config();
        cfg.state.reset_display = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

fn cycle_clock_format(app: &mut App) {
    let next = match app.config().state.clock_format() {
        ClockFormat::H24 => ClockFormat::H12,
        ClockFormat::H12 => ClockFormat::H24,
    };
    {
        let mut cfg = app.config();
        cfg.state.clock_format = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

fn cycle_home_tab(app: &mut App) {
    let next: Tab = app.config().state.home_tab().into();
    let next = next.next();
    {
        let mut cfg = app.config();
        cfg.state.home_tab = Some(next.into());
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

fn cycle_divergence_default(app: &mut App) {
    let next = next_divergence_default(app.config().state.default_divergence);
    {
        let mut cfg = app.config();
        cfg.state.default_divergence = next;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

fn toggle_wrap_off(app: &mut App) {
    let outcome = {
        let mut cfg = app.config();
        let next = !cfg.state.switch_off_when_spent;
        set_wrap_off(&mut cfg, next)
    };
    if let Err(e) = outcome {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&e.to_string())),
        );
    }
    app.apply_forgets();
}

/// Flip the opt-in burn-aware auto-switch mode (issue #8 follow-up b). Shares
/// `switch_off_when_spent`'s persistence shape exactly: mutate the shared `AppConfig`,
/// `save_app_state`, drain forgets — no separate propagation to the
/// scheduler is needed since `next_auto_switch_target` reads the flag straight
/// off the same shared `config` (`snapshot_chain` mirrors `switch_off_when_spent`'s copy into `ChainSnapshot`).
fn toggle_burn_aware_switching(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.burn_aware_switching = !cfg.state.burn_aware_switching;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Cycle the walk-order mode (issue #86): `chain` → `soonest weekly reset` →
/// `most weekly headroom` → `chain`.
/// `cycle_reset_display`'s persistence shape exactly: mutate the shared
/// `AppConfig`, `save_app_state`, drain forgets — no separate
/// propagation to the scheduler, since every walk reads the mode off the same
/// shared `config` (`snapshot_chain` copies it into `ChainSnapshot` like
/// `burn_aware`).
fn cycle_walk_order(app: &mut App) {
    let next = match app.config().state.walk_order() {
        WalkOrder::Chain => WalkOrder::SoonestWeeklyReset,
        WalkOrder::SoonestWeeklyReset => WalkOrder::MostWeeklyHeadroom,
        WalkOrder::MostWeeklyHeadroom => WalkOrder::Chain,
    };
    {
        let mut cfg = app.config();
        cfg.state.walk_order = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Step the burn-aware floor forward through [`BURN_FLOOR_PRESETS`] (space on
/// the Config row), wrapping past the top back to the first — the same
/// segmented-control grammar as the weekly row. Only reachable while burn-aware
/// is on (the row is inert otherwise). Persists the `Option` field so an unset
/// default keeps `skip_serializing_if` omitting it until the first change.
fn step_burn_floor(app: &mut App) {
    let current = app.config().state.burn_switch_floor_pct();
    let next = BURN_FLOOR_PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(BURN_FLOOR_PRESETS[0]);
    {
        let mut cfg = app.config();
        cfg.state.burn_switch_floor_pct = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Step the burn-aware horizon cap forward through [`BURN_HORIZON_PRESETS`].
/// Same grammar + persistence as [`step_burn_floor`].
fn step_burn_horizon(app: &mut App) {
    let current = app.config().state.burn_horizon_cap_ms();
    let next = BURN_HORIZON_PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(BURN_HORIZON_PRESETS[0]);
    {
        let mut cfg = app.config();
        cfg.state.burn_horizon_cap_ms = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Flip the master half of the real-money opt-in
/// (`AppState.spend_budget_switching`). Same persistence shape as
/// `toggle_burn_aware_switching`; no separate propagation to the scheduler is
/// needed since the walk reads the flag off the same shared `config`
/// (`snapshot_chain` mirrors it into `ChainSnapshot`). Flipping this on alone
/// still spends nothing — every member's ceiling defaults to $0.
fn toggle_spend_budget_switching(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.spend_budget_switching = !cfg.state.spend_budget_switching;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Flip what happens once a billing account has spent its budget
/// (`AppState.switch_off_when_budget_spent`). Same persistence shape as
/// `toggle_spend_budget_switching`.
fn toggle_budget_wrap_off(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.switch_off_when_budget_spent = !cfg.state.switch_off_when_budget_spent;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Flip preemptive rotation (on by default, and profile-wide — not per-account).
/// Same persistence shape as `toggle_burn_aware_switching`; the scheduler reads
/// the flag off the shared config each rotation-leg entry, so no separate
/// propagation is needed.
fn toggle_preemptive_rotation(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.preemptive_rotation = !cfg.state.preemptive_rotation;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// never flips the saved `[update].auto_update` value: it says why instead.
fn toggle_auto_update(app: &mut App) {
    app.toast(ToastKind::Info, update::DISABLED_MESSAGE.to_string());
}

/// Flip whether non-login keys stay with their account
/// (`preserve_non_login_keys`, default on). Same persistence shape as
/// `toggle_preemptive_rotation`; every reader loads the flag off disk at the
/// moment it relinks or rewrites a store (`claude::preserve_non_login_keys`),
/// so the next relink in any process acts on the flip.
fn toggle_preserve_non_login_keys(app: &mut App) {
    let error = {
        let mut cfg = app.config();
        cfg.state.preserve_non_login_keys = !cfg.state.preserve_non_login_keys;
        match save_app_state(&cfg.state) {
            Ok(()) => None,
            Err(e) => {
                cfg.state.preserve_non_login_keys = !cfg.state.preserve_non_login_keys;
                Some(e)
            }
        }
    };
    if let Some(error) = error {
        app.toast(
            ToastKind::Danger,
            format!("could not save preserve_non_login_keys: {error:#}"),
        );
    }
    app.last_reload_fp = reload_fingerprint();
}

/// Flip whether the background fetch polls spent (100%-capped) accounts
/// (`refresh_spent_accounts`, default on). Same persistence shape as
/// `toggle_preemptive_rotation`; the scheduler reads the flag off the shared
/// config each tick, so no separate propagation is needed.
fn toggle_refresh_spent_accounts(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.refresh_spent_accounts = !cfg.state.refresh_spent_accounts;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Interleave the auto-start kick across accounts, or let every window reopen the
/// instant it lapses (`usage::auto_start_queue`).
fn toggle_auto_start_queue(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.auto_start_queue = !cfg.state.auto_start_queue;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

fn toggle_show_estimates(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.show_estimates = !cfg.state.show_estimates;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Toggle whether the Tokens tab counts cache in its token figures (persisted).
fn toggle_count_cache(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.count_cache = !cfg.state.count_cache;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

fn toggle_show_pace(app: &mut App) {
    {
        let mut cfg = app.config();
        cfg.state.show_pace = !cfg.state.show_pace;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Advance the global refresh interval to the next-greater preset, wrapping
/// past the top back to the first — space always cycles forward, never clamps.
/// A custom off-ladder value lands on the next preset above it, not one past it.
fn step_refresh_interval(app: &mut App) {
    const PRESETS: [u64; 6] = [15_000, 30_000, 60_000, 90_000, 120_000, 300_000];
    let current = app.refresh_interval.load(Ordering::Relaxed);
    let new_interval = PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(PRESETS[0]);
    app.refresh_interval.store(new_interval, Ordering::Relaxed);
    {
        let mut cfg = app.config();
        cfg.state.refresh_interval_ms = new_interval;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Advance the context-nudge threshold to the next-greater preset, wrapping
/// past the top back to off — space always cycles forward, never clamps. A
/// custom off-ladder value lands on the next preset above it, not one past it;
/// past the top preset it wraps to off, the cycle's first step.
fn step_context_nudge(app: &mut App) {
    const PRESETS: [u64; 4] = [300_000, 400_000, 600_000, 900_000];
    let current = app.config().state.context_nudge_threshold_tokens();
    let next = match current {
        None => Some(PRESETS[0]),
        Some(v) => PRESETS.iter().copied().find(|&p| p > v),
    };
    {
        let mut cfg = app.config();
        cfg.state.context_nudge_threshold_tokens = next;
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Open the inline custom-value editor for the global refresh interval, seeded
/// with the current value in whole seconds. ⏎ commits, ⎋ discards.
fn begin_refresh_interval_edit(app: &mut App) {
    app.refresh_interval_draft = Some(SeededInput::new(&refresh_interval_seed(app)));
}

/// Keystrokes while the refresh-interval field is open: ⏎ saves, ⎋ discards.
fn handle_refresh_interval_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.refresh_interval_draft = None,
        KeyCode::Enter => {
            let _ = commit_refresh_interval_edit(app);
        }
        _ => {
            if let Some(draft) = app.refresh_interval_draft.as_mut() {
                apply_input_edit(&mut draft.input, key);
            }
        }
    }
}

/// Parse and persist the typed custom interval. Invalid input keeps the draft
/// open so the Config card's inline Invalid-input treatment (DANGER value +
/// `└ 10–3600 s` tooltip) stays on screen until corrected — no toast.
/// Returns whether the commit landed.
fn commit_refresh_interval_edit(app: &mut App) -> bool {
    let Some(raw) = app
        .refresh_interval_draft
        .as_ref()
        .map(|i| i.input.trimmed())
    else {
        return true;
    };
    let Some(ms) = parse_refresh_secs(raw) else {
        return false;
    };
    app.refresh_interval.store(ms, Ordering::Relaxed);
    let saved = {
        let mut cfg = app.config();
        cfg.state.refresh_interval_ms = ms;
        save_app_state(&cfg.state)
    };
    if let Err(e) = saved {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        return false;
    }
    app.apply_forgets();
    app.refresh_interval_draft = None;
    true
}

/// A typed custom interval is valid only as a whole number of **seconds** that
/// lands in `MIN_REFRESH_INTERVAL_MS..=MAX_REFRESH_INTERVAL_MS` once scaled to
/// milliseconds. Shared by the commit path and the Config card's inline check.
pub(crate) fn parse_refresh_secs(raw: &str) -> Option<u64> {
    let ms = raw.parse::<u64>().ok()?.checked_mul(1000)?;
    (MIN_REFRESH_INTERVAL_MS..=MAX_REFRESH_INTERVAL_MS)
        .contains(&ms)
        .then_some(ms)
}

/// Open the inline custom-value editor for the context-nudge threshold, seeded
/// with the current value in the row's own vocabulary (`600k`); from off it
/// seeds the first preset, the value one space-press would pick. ⏎ commits, ⎋
/// discards.
fn begin_context_nudge_edit(app: &mut App) {
    let current = app.config().state.context_nudge_threshold_tokens();
    app.context_nudge_draft = Some(SeededInput::new(&context_nudge_seed(current)));
}

/// Keystrokes while the context-nudge field is open: ⏎ saves, ⎋ discards.
fn handle_context_nudge_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.context_nudge_draft = None,
        KeyCode::Enter => {
            let _ = commit_context_nudge_edit(app);
        }
        _ => {
            if let Some(draft) = app.context_nudge_draft.as_mut() {
                apply_input_edit(&mut draft.input, key);
            }
        }
    }
}

/// Parse and persist the typed threshold. Invalid input keeps the draft open
/// so the Config card's inline Invalid-input treatment (DANGER value +
/// `└ 50k-2M tokens` tooltip) stays on screen until corrected — no toast.
/// Returns whether the commit landed.
fn commit_context_nudge_edit(app: &mut App) -> bool {
    let Some(raw) = app.context_nudge_draft.as_ref().map(|i| i.input.trimmed()) else {
        return true;
    };
    let Some(tokens) = parse_context_nudge_tokens(raw) else {
        return false;
    };
    let saved = {
        let mut cfg = app.config();
        cfg.state.context_nudge_threshold_tokens = Some(tokens);
        save_app_state(&cfg.state)
    };
    if let Err(e) = saved {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        return false;
    }
    app.apply_forgets();
    app.context_nudge_draft = None;
    true
}

/// A typed context-nudge threshold is valid only as whole tokens — a plain
/// number or one with a single trailing `k` (case-insensitive, `600k` → 600
/// 000) — that lands in `MIN_CONTEXT_NUDGE_TOKENS..=MAX_CONTEXT_NUDGE_TOKENS`.
/// Shared by the commit path and the Config card's inline check.
pub(crate) fn parse_context_nudge_tokens(raw: &str) -> Option<u64> {
    let (digits, scale) = match raw.strip_suffix(['k', 'K']) {
        Some(prefix) => (prefix, 1_000u64),
        None => (raw, 1u64),
    };
    let tokens = digits.parse::<u64>().ok()?.checked_mul(scale)?;
    (MIN_CONTEXT_NUDGE_TOKENS..=MAX_CONTEXT_NUDGE_TOKENS)
        .contains(&tokens)
        .then_some(tokens)
}

/// Step the weekly exhaustion line forward through the preset ladder (space on
/// the Config row), wrapping past the top back to the first — the same
/// segmented-control grammar as the refresh row. Presets mirror
/// `WEEKLY_PRESETS` in `render/global_config.rs`.
fn step_weekly_threshold(app: &mut App) {
    let current = app.config().state.weekly_switch_threshold_pct();
    let next = WEEKLY_PRESETS
        .iter()
        .copied()
        .find(|&p| p > current)
        .unwrap_or(WEEKLY_PRESETS[0]);
    {
        let mut cfg = app.config();
        cfg.state.weekly_switch_threshold = Some(next);
        let _ = save_app_state(&cfg.state);
    }
    app.apply_forgets();
}

/// Open the inline custom-value editor for the weekly line, seeded with the
/// current value. ⏎ commits, ⎋ discards.
fn begin_weekly_threshold_edit(app: &mut App) {
    let current = app.config().state.weekly_switch_threshold_pct();
    app.weekly_threshold_draft = Some(SeededInput::new(&format_weekly_pct(current)));
}

/// Keystrokes while the weekly-threshold field is open: ⏎ saves, ⎋ discards.
fn handle_weekly_threshold_edit_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.weekly_threshold_draft = None,
        KeyCode::Enter => {
            let _ = commit_weekly_threshold_edit(app);
        }
        _ => {
            if let Some(draft) = app.weekly_threshold_draft.as_mut() {
                apply_input_edit(&mut draft.input, key);
            }
        }
    }
}

/// Parse and persist the typed custom weekly line. Invalid input keeps the
/// draft open with the inline Invalid-input treatment, same as the refresh
/// editor — no toast. Returns whether the commit landed.
fn commit_weekly_threshold_edit(app: &mut App) -> bool {
    let Some(raw) = app
        .weekly_threshold_draft
        .as_ref()
        .map(|i| i.input.trimmed())
    else {
        return true;
    };
    let Some(pct) = parse_weekly_pct(raw) else {
        return false;
    };
    let saved = {
        let mut cfg = app.config();
        cfg.state.weekly_switch_threshold = Some(pct);
        save_app_state(&cfg.state)
    };
    if let Err(e) = saved {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        return false;
    }
    app.apply_forgets();
    app.weekly_threshold_draft = None;
    true
}

/// A typed custom weekly line is valid as a finite percent (decimals allowed)
/// within `MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT`. Shared by the
/// commit path and the Config card's inline check.
pub(crate) fn parse_weekly_pct(raw: &str) -> Option<f64> {
    let pct = raw.parse::<f64>().ok()?;
    (pct.is_finite() && (MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT).contains(&pct))
        .then_some(pct)
}

/// Render a percent without a trailing `.0` on whole numbers — `98`, `97.5`.
pub(crate) fn format_weekly_pct(pct: f64) -> String {
    if pct.fract() == 0.0 {
        format!("{pct:.0}")
    } else {
        format!("{pct}")
    }
}

/// Right-pane keymap for a member: ↑↓ walks rows, `+`/`-` steps the threshold,
/// ⏎ on remove arms then confirms; space stays the state-control key.
/// Delegates to add picker on `+ add`.
fn handle_fallback_detail_key(app: &mut App, key: KeyEvent) {
    if selected_chain_member(app).is_none() {
        handle_fallback_add_key(app, key);
        return;
    }
    let last = FALLBACK_ROWS.len() - 1;
    app.fallback_detail_cursor = app.fallback_detail_cursor.min(last);
    let row = FALLBACK_ROWS[app.fallback_detail_cursor];
    match key.code {
        KeyCode::Up => {
            app.fallback_edit = None;
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down => {
            app.fallback_edit = None;
            app.fallback_detail_cursor = if app.fallback_detail_cursor >= last {
                0
            } else {
                app.fallback_detail_cursor + 1
            };
        }
        KeyCode::PageUp => {
            app.fallback_edit = None;
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            let tips = app.fallback_detail_tips.borrow();
            app.fallback_detail_cursor = page_up_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                &tips,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        KeyCode::PageDown => {
            app.fallback_edit = None;
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            app.fallback_detail_cursor = page_down_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        // Continuous fields only — `max spend` (a dollar ceiling) has no
        // natural step unit and stays typed-only.
        KeyCode::Char('+' | '=') if row == FallbackRow::Threshold => adjust_threshold(app, 5.0),
        KeyCode::Char('-' | '_') if row == FallbackRow::Threshold => adjust_threshold(app, -5.0),
        KeyCode::Char('+' | '=') if row == FallbackRow::WeeklyAt => adjust_weekly(app, 5.0),
        KeyCode::Char('-' | '_') if row == FallbackRow::WeeklyAt => adjust_weekly(app, -5.0),
        // The day row is a cycle row at rest and a chip row once descended:
        // space walks the preset ladder (the `weekly limit` row's grammar), ⏎
        // opens the chip picker. Split here so `run_fallback_row` keeps its
        // single meaning for every other row.
        KeyCode::Char(' ') if row == FallbackRow::PreferredDays => step_preferred_days(app),
        // `remove` is an action row and acts on `↵` alone; every other row's
        // `space` stays a state control.
        KeyCode::Char(' ') if row.is_state_control() => {
            run_fallback_row(app, row);
        }
        KeyCode::Enter => {
            run_fallback_row(app, row);
        }
        _ => {}
    }
}

/// The `+ add` confirm's detail for a candidate tollgate cannot measure: the
/// hazard, then the way out. Two lines, because this dialog asks a question and
/// the answer needs both the cost and the fix in one place.
pub(crate) fn no_usage_confirm_detail(name: &str, active: bool) -> String {
    // The period is added HERE, not in the shared fix line: this dialog speaks
    // in sentences and its other two lines end in one, while the card's hint
    // and the daemon's log line carry the same copy without a full stop, like
    // every other hint and log line beside them.
    format!(
        "{}{}.\n{}.",
        crate::fallback::no_usage_hazard_lead(active),
        crate::fallback::no_usage_hazard(),
        prose::mark_commands(&crate::fallback::no_usage_fix(&[name]))
    )
}

/// `+ add` detail: a candidate picker. ↑↓ walks, ⏎ adds and re-homes focus.
fn handle_fallback_add_key(app: &mut App, key: KeyEvent) {
    let candidates = chain_candidates(app);
    if candidates.is_empty() {
        leave_fallback_detail(app);
        return;
    }
    let last = candidates.len() - 1;
    app.fallback_detail_cursor = app.fallback_detail_cursor.min(last);
    match key.code {
        KeyCode::Up => {
            app.fallback_detail_cursor = if app.fallback_detail_cursor == 0 {
                last
            } else {
                app.fallback_detail_cursor - 1
            };
        }
        KeyCode::Down => {
            app.fallback_detail_cursor = if app.fallback_detail_cursor >= last {
                0
            } else {
                app.fallback_detail_cursor + 1
            };
        }
        KeyCode::PageUp => {
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            let tips = app.fallback_detail_tips.borrow();
            app.fallback_detail_cursor = page_up_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                &tips,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        KeyCode::PageDown => {
            let starts = app.fallback_detail_row_starts.borrow();
            let block_ends = app.fallback_detail_block_ends.borrow();
            app.fallback_detail_cursor = page_down_cursor_by_rows(
                app.fallback_detail_cursor,
                &starts,
                &block_ends,
                app.fallback_detail_viewport.get(),
                app.fallback_detail_offset.get(),
                app.fallback_detail_total.get(),
            );
        }
        KeyCode::Enter => {
            let name = ProfileName::from(candidates[app.fallback_detail_cursor].clone());
            // The sidecar fact from the cache, BEFORE the config guard: the
            // predicate needs it, and reading `session-token.json` here would be
            // a disk read under the lock (`collect_session_tokens` is built to
            // keep those apart).
            let session_token = app.installs_session_token(&name);
            let (would_mix, no_usage, active) = {
                let cfg = app.config();
                (
                    chain_would_mix(&cfg, &name),
                    cfg.find(&name)
                        .is_some_and(|p| crate::fallback::no_usage_source(p, session_token)),
                    // A candidate can already be the active account: it is the
                    // chain it is missing from, not the active slot. Then the
                    // hazard is not a future one.
                    cfg.is_active(&name),
                )
            };
            if would_mix {
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: "mixing api-key and oauth accounts can leave sessions stuck on the \
                              api account."
                        .into(),
                    detail: Some("api → oauth switches may not work until cc restarts.".into()),
                    choice: false,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::AddChainCandidate(name.to_string()),
                }));
            } else if no_usage {
                // The walk counts a member without a reading as having headroom
                // and, once it is active, never moves off it. Said once here,
                // where the operator still decides; the card's marker keeps
                // saying it.
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: "tollgate can't read this account's usage.".into(),
                    detail: Some(no_usage_confirm_detail(name.as_str(), active)),
                    choice: false,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::AddChainCandidate(name.to_string()),
                }));
            } else {
                commit_chain_add(app, &name);
            }
        }
        _ => {}
    }
}

/// Add `name` to the chain, toast, and re-home focus. Shared by the direct
/// add path and the `AddChainCandidate` confirm callback. A failed save
/// reports the house `save failed` toast and skips the success toast and the
/// focus re-homing: the chain is still without the member.
fn commit_chain_add(app: &mut App, name: &ProfileName) {
    if let Err(e) = add_chain_candidate(app, name) {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&e.to_string())),
        );
        return;
    }
    app.toast(ToastKind::Success, format!("added '{name}' to chain"));
    // When the picker empties, `+ add` disappears — land on the new member.
    let remaining = chain_candidates(app);
    if remaining.is_empty() {
        leave_fallback_detail(app);
        app.chain_cursor = chain_items(app).len().saturating_sub(1);
    } else {
        app.fallback_detail_cursor = app.fallback_detail_cursor.min(remaining.len() - 1);
    }
}

/// Chain index under the cursor, or `None` on `+ add`.
fn selected_chain_member(app: &App) -> Option<usize> {
    match chain_items(app).get(app.chain_cursor).copied() {
        Some(ChainItemKind::Member(i)) => Some(i),
        _ => None,
    }
}

/// Profiles not yet in the chain (add-picker candidates). A disabled account
/// is excluded — the fallback-chain editing UI must never offer adding one
/// (it still sits in `fallback_chain` untouched if it was already a member
/// before being disabled; only new additions are blocked here).
pub(crate) fn chain_candidates(app: &App) -> Vec<String> {
    let cfg = app.config();
    cfg.profiles
        .iter()
        .filter(|p| !p.is_disabled() && !cfg.state.fallback_chain.iter().any(|c| c == &p.name))
        .map(|p| p.name.to_string())
        .collect()
}

/// Whether adding `name` to the fallback chain would mix api-key and oauth
/// accounts. CC's env-var handling breaks the api-key → oauth switch direction,
/// so a mixed chain can leave a running bare
/// `claude` session stuck on the api-key account until restart. Fires only
/// when a homogeneous chain would gain its other kind — silent on empty,
/// already-mixed, and same-kind adds.
pub(crate) fn chain_would_mix(cfg: &AppConfig, name: &ProfileName) -> bool {
    let Some(candidate) = cfg.find(name) else {
        return false;
    };
    let mut members = cfg.state.fallback_chain.iter().filter_map(|n| cfg.find(n));
    let Some(first) = members.next() else {
        return false;
    };
    let chain_kind = first.api_key.is_some();
    // A chain that already holds both kinds can't have a mix "created" by a
    // new add, so the modal would be noise.
    if members.any(|p| p.api_key.is_some() != chain_kind) {
        return false;
    }
    candidate.api_key.is_some() != chain_kind
}

/// Enter right pane for the selected chain item. No-op on `+ add` when empty.
fn enter_fallback_detail(app: &mut App) {
    match chain_items(app).get(app.chain_cursor).copied() {
        Some(ChainItemKind::Member(_)) => {}
        Some(ChainItemKind::Add) if !chain_candidates(app).is_empty() => {}
        _ => return,
    }
    app.fallback_detail_cursor = 0;
    app.fallback_edit = None;
    app.fallback_focus = FallbackFocus::Detail;
}

/// Return focus to the chain list, clearing any armed remove or live edit and
/// the card's remembered day-list stop.
fn leave_fallback_detail(app: &mut App) {
    app.fallback_focus = FallbackFocus::Chain;
    app.fallback_detail_cursor = 0;
    app.fallback_edit = None;
    app.fallback_day_stop = None;
}

/// ⇧↑↓: move the selected member up/down, cursor follows. No-op on `+ add`
/// or at boundary. Chain index == cursor for members (they precede `+ add`).
fn reorder_chain_member(app: &mut App, delta: i32) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let target = pos as i32 + delta;
    let landed = {
        let mut cfg = app.config();
        if target < 0 || target as usize >= cfg.state.fallback_chain.len() {
            return;
        }
        let mut order = cfg.state.fallback_chain.clone();
        order.swap(pos, target as usize);
        let moved = order[target as usize].clone();
        set_chain_order(&mut cfg, &order)
            .ok()
            .and_then(|saved| saved.iter().position(|n| n == &moved))
    };
    // The cursor follows the member only when the reorder landed: a failed save
    // (a held state flock, a disk error) leaves the chain and the selection
    // where they were, so the next keypress still acts on the same member. The
    // saved order may have dropped an unresolvable entry, so the member's new
    // slot is looked up, never assumed.
    if let Some(slot) = landed {
        app.chain_cursor = slot;
    }
}

/// ⏎ on a member detail row: a typed row opens its field and `preferred
/// days` its picker; remove arms, then removes on the second press. Each one
/// pins the member under the cursor. Space reaches here for every row but
/// `remove` and `preferred days` (the key handler splits both out).
fn run_fallback_row(app: &mut App, row: FallbackRow) {
    match row {
        FallbackRow::Threshold => {
            if let Some(current) = selected_threshold(app) {
                let field = InputState::new(&format!("{current:.0}"));
                open_card_edit(app, CardEdit::Threshold(field));
            }
        }
        FallbackRow::WeeklyAt => {
            // Inert while the member's weekly gate is off (rendered dimmed):
            // the line isn't judged there, so typing one would silently do
            // nothing — same no-op contract as the spend-budget-off ceiling.
            if let Some((override_pct, check_weekly)) = selected_weekly_override(app)
                && check_weekly
            {
                let seed = override_pct.map(|v| format!("{v:.0}")).unwrap_or_default();
                open_card_edit(app, CardEdit::Weekly(InputState::new(&seed)));
            }
        }
        FallbackRow::CheckWeekly => toggle_member_flag(app, MemberFlag::Weekly),
        FallbackRow::CheckScoped => toggle_member_flag(app, MemberFlag::Scoped),
        FallbackRow::CheckCreditCap => toggle_member_flag(app, MemberFlag::CreditCap),
        FallbackRow::LastResort => toggle_last_resort(app),
        FallbackRow::Preferred => toggle_preferred(app),
        // ⏎ only: space is the preset stepper and never reaches here for this row.
        FallbackRow::PreferredDays => open_day_picker(app),
        FallbackRow::MaxSpend => {
            // Inert while spend budget is off (rendered dimmed): opening the editor
            // would let a ceiling be typed that does nothing, so no-op.
            let armed = app.config().state.spend_budget_switching;
            if armed && let Some(current) = selected_max_spend(app) {
                let field = InputState::new(&format!("{current:.2}"));
                open_card_edit(app, CardEdit::MaxSpend(field));
            }
        }
        FallbackRow::Remove => match app.fallback_edit.take() {
            Some(MemberEdit {
                member,
                state: CardEdit::ArmedRemove,
                ..
            }) => remove_chain_member(app, &member),
            _ => open_card_edit(app, CardEdit::ArmedRemove),
        },
    }
}

/// Open `edit` on the card, pinned to the member under the cursor. A typed
/// field's seed is the value it was just constructed with.
fn open_card_edit(app: &mut App, edit: CardEdit) {
    let seed = edit.input().map(|i| i.value.clone());
    app.fallback_edit = selected_member_name(app).map(|member| MemberEdit {
        member,
        state: edit,
        seed,
    });
}

/// Keystrokes while a typed card field is open: ⏎ saves, ⎋ discards.
fn handle_member_field_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.fallback_edit = None,
        KeyCode::Enter => {
            let _ = commit_member_field(app);
        }
        _ => {
            if let Some(input) = app.fallback_edit.as_mut().and_then(|e| e.state.input_mut()) {
                apply_input_edit(input, key);
            }
        }
    }
}

/// Parse the open field and persist it on the member it is pinned to, whatever
/// sits under the cursor now. Invalid input keeps the field open so the card's
/// inline Invalid-input treatment (DANGER value + range tooltip) stays on
/// screen until corrected — no toast.
fn commit_member_field(app: &mut App) -> bool {
    let Some(MemberEdit { member, state, .. }) = app.fallback_edit.clone() else {
        return false;
    };
    let landed = match state {
        CardEdit::Threshold(input) => {
            let Some(value) = parse_threshold(input.trimmed()) else {
                return false;
            };
            write_threshold(app, &member, value)
        }
        // An EMPTY commit clears the override back to the chain-wide default.
        CardEdit::Weekly(input) => {
            let Some(value) = parse_weekly_override(input.trimmed()) else {
                return false;
            };
            write_weekly_override(app, &member, value)
        }
        CardEdit::MaxSpend(input) => {
            let Some(value) = parse_max_spend(input.trimmed()) else {
                return false;
            };
            write_max_spend(app, &member, value)
        }
        CardEdit::Days(_) | CardEdit::ArmedRemove => return false,
    };
    if landed {
        app.fallback_edit = None;
    }
    landed
}

/// A typed override is a number in `0..=100`, or EMPTY — the explicit
/// clear-back-to-default. `Some(None)` = clear, `Some(Some(v))` = set,
/// `None` = invalid. Shared by the commit path and the detail card's inline
/// Invalid-input check.
pub(crate) fn parse_weekly_override(raw: &str) -> Option<Option<f64>> {
    if raw.is_empty() {
        return Some(None);
    }
    parse_weekly_pct(raw).map(Some)
}

/// The selected member's `(weekly_threshold override, check_weekly)`, or
/// `None` on `+ add`.
fn selected_weekly_override(app: &App) -> Option<(Option<f64>, bool)> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    cfg.find(name).map(|p| (p.weekly_threshold, p.check_weekly))
}

/// Write (or clear) `name`'s weekly-line override and persist.
fn write_weekly_override(app: &mut App, name: &ProfileName, value: Option<f64>) -> bool {
    let save_err = {
        let mut cfg = app.config();
        match cfg.find_mut(name) {
            Some(profile) => {
                profile.weekly_threshold = value;
                save_profile_config(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        false
    } else {
        true
    }
}

/// A typed ceiling is valid only as a finite, non-negative number of dollars.
/// `is_finite` is the load-bearing half: `"inf"` parses as a perfectly good
/// `f64`, and an infinite ceiling means unbounded unattended spending
/// (`fallback::spend_room`). Shared by the commit path and the detail card's
/// inline Invalid-input check.
pub(crate) fn parse_max_spend(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// This member's ceiling in dollars, or `None` on `+ add`. Unset reads as $0 —
/// the never-spend default.
fn selected_max_spend(app: &App) -> Option<f64> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    Some(cfg.find(name).and_then(|p| p.max_auto_spend).unwrap_or(0.0))
}

/// Write `name`'s ceiling and persist.
fn write_max_spend(app: &mut App, name: &ProfileName, value: f64) -> bool {
    let save_err = {
        let mut cfg = app.config();
        match cfg.find_mut(name) {
            Some(profile) => {
                profile.max_auto_spend = Some(value);
                save_profile_config(profile).err()
            }
            None => None,
        }
    };
    if let Some(e) = save_err {
        app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        false
    } else {
        true
    }
}

/// The chain member under the cursor, by name, or `None` on `+ add`.
fn selected_member_name(app: &App) -> Option<ProfileName> {
    let pos = selected_chain_member(app)?;
    app.config().state.fallback_chain.get(pos).cloned()
}

/// `name`'s home days as the in-memory config holds them.
pub(crate) fn member_days(app: &App, name: &ProfileName) -> Option<Vec<Weekday>> {
    app.config().find(name).map(|p| p.preferred_days.clone())
}

/// After a config reload: point `chain_cursor` back at the member the open
/// [`MemberEdit`] is pinned to. When the reload took that member off the chain
/// or its account off the roster, close the edit and hand focus back to the
/// chain list, so no key meant for the edit lands on the member that moved into
/// its slot, and none writes to an account that is gone. A remembered day-list
/// stop goes once the card no longer shows its member.
fn repin_member_edit(app: &mut App) {
    if let Some(member) = app.fallback_edit.as_ref().map(|e| e.member.clone()) {
        let slot = {
            let cfg = app.config();
            cfg.find(&member)
                .and_then(|_| cfg.state.fallback_chain.iter().position(|n| *n == member))
        };
        match slot {
            Some(i) => app.chain_cursor = i,
            None => leave_fallback_detail(app),
        }
    }
    if app
        .fallback_day_stop
        .as_ref()
        .is_some_and(|stop| selected_member_name(app).as_ref() != Some(&stop.member))
    {
        app.fallback_day_stop = None;
    }
}

/// Space on the `preferred days` row at rest: step to the next stop and
/// persist. A set matching no rung (a custom list) is one more stop past the
/// last rung for as long as the card stays open: stepping off it remembers it
/// and lands on `never`, the ladder's first rung (a set has no order, so no
/// rung sits above it the way `step_weekly_threshold` finds the next preset
/// above a custom percent), and the last rung steps back onto it.
fn step_preferred_days(app: &mut App) {
    let Some(name) = selected_member_name(app) else {
        return;
    };
    // Stepped from the rung the card shows, since that is what the press
    // answered; a chain entry with no account behind it has no rung to step.
    let Some(shown) = member_days(app, &name) else {
        return;
    };
    let next = match preferred_days_preset(&shown) {
        Some(i) if i + 1 < PREFERRED_DAY_PRESETS.len() => preferred_days_at(i + 1),
        Some(_) => app
            .fallback_day_stop
            .as_ref()
            .filter(|stop| stop.member == name)
            .map(|stop| stop.state.clone())
            .unwrap_or_default(),
        None => {
            app.fallback_day_stop = Some(MemberEdit {
                member: name.clone(),
                state: shown,
                seed: None,
            });
            preferred_days_at(0)
        }
    };
    if let Some(reason) = write_preferred_days(app, &name, move |_| next) {
        warn_list_claims_nothing(app, &reason);
    }
}

/// ⏎ on the day row: descend into the chip picker, pinned to the member under
/// the cursor, caret on Monday. A chain entry with no account behind it has no
/// row to draw the picker in, so it opens nothing.
fn open_day_picker(app: &mut App) {
    if let Some(member) = selected_member_name(app)
        && member_days(app, &member).is_some()
    {
        app.fallback_edit = Some(MemberEdit {
            member,
            state: CardEdit::Days(DayPicker::default()),
            seed: None,
        });
    }
}

/// Keystrokes while the chip picker is open (a multi-select chip row):
/// ←/→ walk the caret with wrap, space toggles the day under it and saves at
/// once, ⏎/esc/q leave the mode, ↑/↓ leave the mode and the row together.
/// `?` and `x` never reach here ([`KeyOwner::claims`]); every other key is
/// claimed and does nothing, so no other global binding fires mid-pick.
fn handle_day_picker_key(app: &mut App, key: KeyEvent) {
    let days = WEEKDAYS_ALL.len();
    match key.code {
        KeyCode::Left | KeyCode::Right => {
            if let Some(MemberEdit {
                state: CardEdit::Days(picker),
                ..
            }) = app.fallback_edit.as_mut()
            {
                let step = if key.code == KeyCode::Left {
                    days - 1
                } else {
                    1
                };
                picker.cursor = (picker.cursor + step) % days;
            }
        }
        KeyCode::Char(' ') => toggle_picked_day(app),
        KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => app.fallback_edit = None,
        KeyCode::Up | KeyCode::Down => {
            app.fallback_edit = None;
            handle_fallback_detail_key(app, key);
        }
        _ => {}
    }
}

/// Space in the picker: flip the day under the caret on the pinned member's
/// list and save the result in `WEEKDAYS_ALL` order. A dead account's list
/// still saves; its claims-nothing warning is raised once per descend.
fn toggle_picked_day(app: &mut App) {
    let Some(MemberEdit {
        member,
        state: CardEdit::Days(state),
        ..
    }) = app.fallback_edit.clone()
    else {
        return;
    };
    let (Some(&day), Some(shown)) = (WEEKDAYS_ALL.get(state.cursor), member_days(app, &member))
    else {
        return;
    };
    // The caret's day lands where the shown list says the press sends it; the
    // other days come from the list read under the lock, so another writer's
    // days survive and the operator's own press is never inverted.
    let want = !shown.contains(&day);
    let toggle = move |days: &[Weekday]| -> Vec<Weekday> {
        WEEKDAYS_ALL
            .into_iter()
            .filter(|d| if *d == day { want } else { days.contains(d) })
            .collect()
    };
    if let Some(reason) = write_preferred_days(app, &member, toggle)
        && !state.warned
    {
        warn_list_claims_nothing(app, &reason);
        if let Some(MemberEdit {
            state: CardEdit::Days(picker),
            ..
        }) = app.fallback_edit.as_mut()
        {
            picker.warned = true;
        }
    }
}

/// Rewrite `name`'s home days through `edit` and persist. A failed save toasts
/// the error and leaves the in-memory list on what the disk still holds.
/// `Some(reason)` when the saved list claims nothing, for the caller to say so.
///
/// Saved rather than refused because the state is reachable without this row — a
/// list goes inert when the account later leaves the chain or its login breaks —
/// and a row that quietly does nothing is worse than one that says why.
fn write_preferred_days(
    app: &mut App,
    name: &ProfileName,
    edit: impl FnOnce(&[Weekday]) -> Vec<Weekday>,
) -> Option<std::borrow::Cow<'static, str>> {
    let result = {
        let mut cfg = app.config();
        crate::actions::edit_profile_preferred_days(&mut cfg, name, edit)
    };
    match result {
        Ok(saved) => (!saved.is_empty())
            .then(|| {
                let key_rejected = app.key_rejected_names();
                crate::fallback::day_claim_blocker(&app.config(), name, &key_rejected)
            })
            .flatten(),
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!(
                    "preferred days update failed\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            );
            None
        }
    }
}

/// The saved-anyway warning for a day list the chain walk will never read.
fn warn_list_claims_nothing(app: &mut App, reason: &str) {
    app.toast(
        ToastKind::Warning,
        format!(
            "saved, but this list claims nothing: {reason}\nthe chain decides those days \
             without this account"
        ),
    );
}

/// Effective threshold for the selected member, or `None` on `+ add`.
fn selected_threshold(app: &App) -> Option<f64> {
    let pos = selected_chain_member(app)?;
    let cfg = app.config();
    let name = cfg.state.fallback_chain.get(pos)?;
    cfg.find(name).map(threshold_for)
}

/// Write `name`'s threshold and persist.
fn write_threshold(app: &mut App, name: &ProfileName, value: f64) -> bool {
    let save_err = {
        let mut cfg = app.config();
        set_member_threshold(&mut cfg, name, value).err()
    };
    let Some(e) = save_err else {
        return true;
    };
    // A member whose roster row vanished inside the reload window is the
    // baseline's silent no-op: the account has already left, so there is
    // nothing to save and no wire code belongs on an operator toast.
    if let Some(ChainEditRefusal {
        code: ChainRefusal::ProfileNotFound,
        ..
    }) = e.downcast_ref::<ChainEditRefusal>()
    {
        return true;
    }
    app.toast(
        ToastKind::Danger,
        format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
    );
    false
}

/// Step the threshold by `delta`, clamped to 0..=100, and persist.
fn adjust_threshold(app: &mut App, delta: f64) {
    if let Some(name) = selected_member_name(app)
        && let Some(current) = selected_threshold(app)
    {
        write_threshold(
            app,
            &name,
            (current + delta).clamp(MIN_THRESHOLD, MAX_THRESHOLD),
        );
    }
}

/// Step the selected member's `weekly at` override by `delta`, clamped to
/// `MIN_WEEKLY_SWITCH_PCT..=MAX_WEEKLY_SWITCH_PCT`, and persist. Mirrors
/// `run_fallback_row`'s `check_weekly` guard: inert while the member's weekly
/// gate is off (rendered dimmed), same no-op contract as a dimmed row's ⏎.
/// An unset override bases the nudge on the chain-wide resolved default so
/// the value visibly moves off what the dimmed-default row already shows.
fn adjust_weekly(app: &mut App, delta: f64) {
    let (Some(name), Some((override_pct, check_weekly))) =
        (selected_member_name(app), selected_weekly_override(app))
    else {
        return;
    };
    if !check_weekly {
        return;
    }
    let base = override_pct.unwrap_or_else(|| app.config().state.weekly_switch_threshold_pct());
    let next = (base + delta).clamp(MIN_WEEKLY_SWITCH_PCT, MAX_WEEKLY_SWITCH_PCT);
    write_weekly_override(app, &name, Some(next));
}

/// A per-member usage gate a toggle row flips (`Profile::check_weekly` /
/// `Profile::check_scoped` / `Profile::check_credit_cap`).
#[derive(Clone, Copy)]
enum MemberFlag {
    Weekly,
    Scoped,
    CreditCap,
}

/// Flip a per-member usage gate and persist it; rolls back in memory when the
/// save fails so the UI never shows a state the disk doesn't hold (same
/// contract as [`toggle_last_resort`], minus its exclusivity move).
fn toggle_member_flag(app: &mut App, flag: MemberFlag) {
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let failed = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        let Some(profile) = cfg.find_mut(&name) else {
            return;
        };
        fn field(p: &mut crate::profile::Profile, flag: MemberFlag) -> &mut bool {
            match flag {
                MemberFlag::Weekly => &mut p.check_weekly,
                MemberFlag::Scoped => &mut p.check_scoped,
                MemberFlag::CreditCap => &mut p.check_credit_cap,
            }
        }
        *field(profile, flag) = !*field(profile, flag);
        match save_profile_config(profile) {
            Ok(()) => None,
            Err(e) => {
                *field(profile, flag) = !*field(profile, flag);
                Some(e)
            }
        }
    };
    match failed {
        None => app.refresh_tokens(),
        Some(e) => app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        ),
    }
}

/// ⏎/space on the `last resort` row: flip `Profile::last_resort` and persist.
/// The chain has ONE parking spot, so turning the mark on here clears it on
/// every other profile (radio). The target saves first (the user's intent);
/// each cleared profile then saves on its own, and a failed clear reverts only
/// that profile — the chain walk tolerates a transiently double-marked chain
/// (first marked member after the active wins), so nothing lies about disk.
/// The `refresh_tokens()` kick is not load-bearing here (the chain snapshot
/// reads the shared config directly, unlike `auto_start` which lives in the
/// `TokenList`); it's kept so every per-profile toggle path re-derives the
/// scheduler snapshot the same way.
fn toggle_last_resort(app: &mut App) {
    enum Outcome {
        Missing,
        Saved { moved_from: Option<String> },
        SaveFailed(anyhow::Error),
    }
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let outcome = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            None => Outcome::Missing,
            Some(profile) => {
                profile.last_resort = !profile.last_resort;
                let now_on = profile.last_resort;
                // Never both on one profile: "park here to the end" and "always
                // come home here" are contradictory. Clear preferred before the
                // single save so the pair persists atomically; remember the
                // prior value so a save failure rolls back both.
                let cleared_preferred = now_on && profile.preferred;
                if now_on {
                    profile.preferred = false;
                }
                match save_profile_config(profile) {
                    Ok(()) => {
                        let mut moved_from = None;
                        if now_on {
                            for p in cfg
                                .profiles
                                .iter_mut()
                                .filter(|p| p.last_resort && p.name != name)
                            {
                                p.last_resort = false;
                                match save_profile_config(p) {
                                    Ok(()) => {
                                        moved_from.get_or_insert_with(|| p.name.to_string());
                                    }
                                    Err(_) => p.last_resort = true,
                                }
                            }
                        }
                        Outcome::Saved { moved_from }
                    }
                    Err(e) => {
                        if let Some(p) = cfg.find_mut(&name) {
                            p.last_resort = !now_on;
                            if cleared_preferred {
                                p.preferred = true;
                            }
                        }
                        Outcome::SaveFailed(e)
                    }
                }
            }
        }
    };
    match outcome {
        Outcome::Missing => {}
        Outcome::Saved { moved_from } => {
            if let Some(prev) = moved_from {
                app.toast(ToastKind::Info, format!("last resort moved from '{prev}'"));
            }
            app.refresh_tokens();
        }
        Outcome::SaveFailed(e) => app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        ),
    }
}

/// Mark the selected member as the chain's preferred (home) account, or clear
/// it. Twin of [`toggle_last_resort`]: exclusive across the chain (target-first
/// radio, per-sibling rollback) with a `"preferred moved from '{prev}'"` toast,
/// and reciprocally mutually exclusive — turning preferred on clears
/// last_resort on the same profile.
fn toggle_preferred(app: &mut App) {
    enum Outcome {
        Missing,
        Saved { moved_from: Option<String> },
        SaveFailed(anyhow::Error),
    }
    let Some(pos) = selected_chain_member(app) else {
        return;
    };
    let outcome = {
        let mut cfg = app.config();
        let Some(name) = cfg.state.fallback_chain.get(pos).cloned() else {
            return;
        };
        match cfg.find_mut(&name) {
            None => Outcome::Missing,
            Some(profile) => {
                profile.preferred = !profile.preferred;
                let now_on = profile.preferred;
                // Reciprocal of the clear in `toggle_last_resort` — the two
                // flags never coexist on one profile.
                let cleared_last_resort = now_on && profile.last_resort;
                if now_on {
                    profile.last_resort = false;
                }
                match save_profile_config(profile) {
                    Ok(()) => {
                        let mut moved_from = None;
                        if now_on {
                            for p in cfg
                                .profiles
                                .iter_mut()
                                .filter(|p| p.preferred && p.name != name)
                            {
                                p.preferred = false;
                                match save_profile_config(p) {
                                    Ok(()) => {
                                        moved_from.get_or_insert_with(|| p.name.to_string());
                                    }
                                    Err(_) => p.preferred = true,
                                }
                            }
                        }
                        Outcome::Saved { moved_from }
                    }
                    Err(e) => {
                        if let Some(p) = cfg.find_mut(&name) {
                            p.preferred = !now_on;
                            if cleared_last_resort {
                                p.last_resort = true;
                            }
                        }
                        Outcome::SaveFailed(e)
                    }
                }
            }
        }
    };
    match outcome {
        Outcome::Missing => {}
        Outcome::Saved { moved_from } => {
            if let Some(prev) = moved_from {
                app.toast(ToastKind::Info, format!("preferred moved from '{prev}'"));
            }
            app.refresh_tokens();
        }
        Outcome::SaveFailed(e) => app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        ),
    }
}

/// Add a profile to the chain (seeding default threshold if unset) and
/// persist. On a failed whole-state save the in-memory chain rolls back and
/// the profile's config.toml is never touched — the threshold seed writes only
/// after the chain save lands — so a failed add leaves both files as they
/// were; the caller reports the error on the house `save failed` toast.
fn add_chain_candidate(app: &mut App, name: &ProfileName) -> anyhow::Result<()> {
    let mut cfg = app.config();
    let seeded = cfg
        .find(name)
        .is_some_and(|profile| profile.fallback_threshold.is_none());
    cfg.state.fallback_chain.push(name.clone());
    if let Err(e) = save_app_state(&cfg.state) {
        cfg.state.fallback_chain.pop();
        return Err(e);
    }
    if seeded && let Some(profile) = cfg.find_mut(name) {
        profile.fallback_threshold = Some(DEFAULT_THRESHOLD);
        // The add landed, so this is not a failed add: a failed seed save
        // keeps memory in step with the disk it failed to write — the
        // threshold stays unset on both sides, the member stays added.
        if save_profile_config(profile).is_err() {
            profile.fallback_threshold = None;
        }
    }
    Ok(())
}

/// Remove `name` from the chain, persist, and return focus to the list. On a
/// failed save the member rolls back into the in-memory chain, the focus and
/// cursor stay put, and the error surfaces on the house `save failed` toast —
/// never the removal toast.
fn remove_chain_member(app: &mut App, name: &ProfileName) {
    let outcome = {
        let mut cfg = app.config();
        let Some(pos) = cfg.state.fallback_chain.iter().position(|n| n == name) else {
            return;
        };
        cfg.state.fallback_chain.remove(pos);
        match save_app_state(&cfg.state) {
            Ok(()) => Ok(()),
            Err(e) => {
                cfg.state.fallback_chain.insert(pos, name.clone());
                Err(e)
            }
        }
    };
    match outcome {
        Ok(()) => {
            leave_fallback_detail(app);
            let items_len = chain_items(app).len();
            if app.chain_cursor >= items_len {
                app.chain_cursor = items_len.saturating_sub(1);
            }
            app.toast(ToastKind::Info, format!("removed '{name}' from chain"));
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&e.to_string())),
        ),
    }
}

fn handle_modal_key(app: &mut App, key: KeyEvent) {
    let Some(top) = app.modals.last().cloned() else {
        return;
    };
    match top {
        Modal::MoveSession(_) => handle_move_session_key(app, key),
        // The keymap outgrows a short terminal, so ↑↓ scrolls it — the same
        // binding every other scrollable surface here uses. BOTH arms clamp
        // against the bound the last render published: growing the terminal
        // shrinks the range under a `help_scroll` that outlived it, and an
        // unclamped step down would then need one press per stale row before
        // the view moved at all (the render clamps the display meanwhile, so
        // the key just looks dead).
        Modal::Help => match key.code {
            KeyCode::Up => {
                let max = app.help_max_scroll.get();
                app.help_scroll = app.help_scroll.min(max).saturating_sub(1);
            }
            KeyCode::Down => {
                let max = app.help_max_scroll.get();
                app.help_scroll = app.help_scroll.saturating_add(1).min(max);
            }
            KeyCode::PageUp => {
                let max = app.help_max_scroll.get();
                app.help_scroll = page_up_scroll(
                    app.help_scroll as usize,
                    max as usize,
                    app.help_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::PageDown => {
                let max = app.help_max_scroll.get();
                app.help_scroll = page_down_scroll(
                    app.help_scroll as usize,
                    max as usize,
                    app.help_viewport.get() as usize,
                ) as u16;
            }
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q') => {
                app.pop_modal();
            }
            _ => {}
        },
        Modal::Confirm(_) => handle_confirm_key(app, key),
        Modal::LossWarning(_) => handle_loss_warning_key(app, key),
        // The read-only `check output` modal scrolls, pages and closes like the
        // help modal, bar `?`: the body may hold one, so it is no close key here.
        Modal::Divergence(_) => handle_divergence_key(app, key),
        Modal::CaptureName(_) => handle_capture_name_key(app, key),
        Modal::NamePrompt(_) => handle_name_prompt_key(app, key),
        Modal::PresetPicker(_) => handle_preset_picker_key(app, key),
        Modal::PresetApply(_) => handle_preset_apply_key(app, key),
        Modal::DivergenceTarget(_) => handle_divergence_target_key(app, key),
        Modal::ActionMenu(_) => handle_action_menu_key(app, key),
        Modal::EnvCollision(_) => handle_env_collision_key(app, key),
        Modal::Login => handle_login_modal_key(app, key),
        Modal::CodexLogin => handle_codex_login_modal_key(app, key),
        Modal::Rebind(_) => handle_rebind_key(app, key),
        Modal::BareItem(_) => handle_bare_item_key(app, key),
        Modal::BareItemOwner(_) => handle_bare_item_owner_key(app, key),
    }
}

/// Keys on the login progress modal. While the inline code field is open it
/// owns every key ([`handle_paste_field_key`]). Otherwise `r` re-opens the URL
/// the session holds; `c` copies the hosted link and `p` opens the code field
/// on the session, both only while the paste door is open
/// ([`LoginSession::open_door`]), so the console login and a login already
/// exchanging a code leave them inert. esc/q/⏎ collapse to the footer
/// indicator; the login keeps running (the generation is untouched). A real
/// cancel is the top-level esc once collapsed; ⏎ on the login row re-expands.
fn handle_login_modal_key(app: &mut App, key: KeyEvent) {
    if app.login.as_ref().is_some_and(|s| s.paste_field.is_some()) {
        handle_paste_field_key(app, key);
        return;
    }
    match key.code {
        // The console login's URL exists once its worker announced it; before
        // that there is nothing to open, so `r` is a no-op.
        KeyCode::Char('r' | 'R') => {
            if let Some(url) = app.login.as_ref().and_then(|s| s.url.clone()) {
                match crate::platform::open_url(&url) {
                    Ok(()) => app.toast(ToastKind::Info, "opening your browser…"),
                    Err(_) => app.toast(ToastKind::Danger, "couldn't open the browser"),
                }
            }
        }
        // OSC 52 lands the link on the LOCAL terminal's clipboard, even over ssh.
        KeyCode::Char('c' | 'C') => {
            let link = app
                .login
                .as_ref()
                .and_then(LoginSession::open_door)
                .map(|door| door.links.hosted_url.clone());
            if let Some(link) = link {
                match (app.clipboard)(&link) {
                    Ok(()) => app.toast(ToastKind::Info, "link copied to clipboard"),
                    Err(e) => app.toast(
                        ToastKind::Danger,
                        format!("couldn't copy the link to clipboard\n{e}"),
                    ),
                }
            }
        }
        KeyCode::Char('p' | 'P') => {
            if let Some(session) = app.login.as_mut()
                && session.open_door().is_some()
            {
                session.paste_field = Some(InputState::new(""));
            }
        }
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            app.modals.pop();
        }
        _ => {}
    }
}

/// Keys on the codex login modal: `r` re-opens the authorize URL once the
/// worker announced it, `c` copies it (OSC 52, as the claude modal's `c`
/// does); no `p` — codex's callback answers on this machine alone, so there is
/// no paste door. esc/q/⏎ collapse to the footer indicator; the login keeps
/// running (a real cancel is the top-level esc once collapsed).
fn handle_codex_login_modal_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('r' | 'R') => {
            if let Some(url) = app.codex_login.as_ref().and_then(|s| s.url.clone()) {
                match crate::platform::open_url(&url) {
                    Ok(()) => app.toast(ToastKind::Info, "opening your browser…"),
                    Err(_) => app.toast(ToastKind::Danger, "couldn't open the browser"),
                }
            }
        }
        KeyCode::Char('c' | 'C') => {
            if let Some(link) = app.codex_login.as_ref().and_then(|s| s.url.clone()) {
                match (app.clipboard)(&link) {
                    Ok(()) => app.toast(ToastKind::Info, "link copied to clipboard"),
                    Err(e) => app.toast(
                        ToastKind::Danger,
                        format!("couldn't copy the link to clipboard\n{e}"),
                    ),
                }
            }
        }
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            app.pop_modal();
        }
        _ => {}
    }
}

/// Keys while the login modal's code field is open. It is a text field, so
/// every printable character is data (`c`, `q` and `p` included — a paste
/// arrives as one key event per character), capped at `MANUAL_CODE_MAX` at the
/// door so a runaway paste is never held first and refused after. esc clears
/// the field and restores the `p  paste code` row (the modal stays); ⏎ submits.
fn handle_paste_field_key(app: &mut App, key: KeyEvent) {
    let Some(session) = app.login.as_mut() else {
        return;
    };
    let Some(field) = session.paste_field.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => session.paste_field = None,
        KeyCode::Enter => submit_paste_code(app),
        KeyCode::Char(_) if field.value.len() >= crate::oauth_login::MANUAL_CODE_MAX => {}
        _ => apply_input_edit(field, key),
    }
}

/// ⏎ on the code field: parse against the running login's own `state`, send a
/// good code down its paste door and close the field. The session's method is
/// NOT flipped here: the browser callback may already have won, so the
/// worker's own `ExchangingCode` report is what names the door. A bad paste is
/// cleared, not kept: a corrected paste appended to leftover bytes would pass
/// the shape check and burn the exchange. A field whose door closed meanwhile
/// (the drain closes it a tick later) has nothing to feed and closes too.
fn submit_paste_code(app: &mut App) {
    let Some(session) = app.login.as_mut() else {
        return;
    };
    let Some(raw) = session
        .paste_field
        .as_ref()
        .map(|field| field.trimmed().to_string())
    else {
        return;
    };
    if raw.is_empty() {
        return;
    }
    let Some(door) = session.open_door() else {
        session.paste_field = None;
        return;
    };
    match door.links.parse(&raw) {
        Ok(code) => {
            // A late paste is never read: `run` already picked its door, so
            // the send's result is discarded; the result drain handles the rest.
            let _ = door.tx.send(code);
            session.paste_field = None;
        }
        Err(e) => {
            session.paste_field = Some(InputState::new(""));
            app.toast(
                ToastKind::Danger,
                format!("{}\ncleared, paste the code again", e.message()),
            );
        }
    }
}

/// Build the action menu for the current screen/focus context: what acts on the
/// focused account, then what acts on the tab.
pub(crate) fn build_action_menu(app: &App) -> ActionMenuState {
    use ActionMenuAction::*;
    let mut scoped: Vec<ActionMenuAction> = Vec::new();
    let mut actions: Vec<ActionMenuAction> = Vec::new();
    let mut context: Option<String> = None;
    let mut row_identity: Option<RowIdentity> = None;

    match app.tab {
        Tab::Overview => {
            match app.current_main_item() {
                Some(MainItemKind::Profile(_)) => {
                    if let Some((name, display)) = push_account_scope(app, &mut scoped) {
                        context = Some(display);
                        row_identity = Some(RowIdentity::Profile(name));
                    }
                }
                Some(MainItemKind::Codex(_) | MainItemKind::Hermes(_)) | None => {}
            }
            actions.push(RefreshAll);
            actions.push(NewAccount);
        }
        Tab::Usage => {
            // The scoped group reuses the Overview's per-row builders, so a
            // row offers none (its detail rows carry re-login / delete).
            match app.usage_selection() {
                SetupSelection::Item(MainItemKind::Profile(_)) => {
                    if let Some((name, display)) = push_account_scope(app, &mut scoped) {
                        context = Some(display);
                        row_identity = Some(RowIdentity::Profile(name));
                    }
                }
                SetupSelection::Item(MainItemKind::Codex(_) | MainItemKind::Hermes(_))
                | SetupSelection::New => {}
            }
            actions.push(RefreshAll);
            if app.usage_extra_cursor.is_none() {
                actions.push(ToggleEstimates);
                actions.push(TogglePace);
            }
        }
        // Tokens: the period + model-filter lenses (minus the ones already
        // active) plus the page keys (`c` cache basis, `r` reload) for
        // discoverability.
        Tab::Tokens => {
            for (period, action) in [
                (TokenPeriod::Lifetime, TokensPeriodLifetime),
                (TokenPeriod::Daily, TokensPeriodDaily),
                (TokenPeriod::Weekly, TokensPeriodWeekly),
                (TokenPeriod::Monthly, TokensPeriodMonthly),
            ] {
                if app.token_period != period {
                    actions.push(action);
                }
            }
            if app.token_filter != TokenFilter::All {
                actions.push(TokensShowAll);
            }
            if app.token_filter != TokenFilter::Claude {
                actions.push(TokensShowClaude);
            }
            if app.token_filter != TokenFilter::Others {
                actions.push(TokensShowOthers);
            }
            actions.push(ToggleCountCache);
            actions.push(ReloadTokenStats);
        }
        // Both halves of the Setup tab configure the SAME focused account, so
        // they carry the same three items. Every per-row action already has a
        // key (the detail pane is a list of actions and ⏎ runs the row); what
        // is left is the whole-account work no row states: copy it, save its
        // endpoint + models as a template, stamp one back onto it. On `+ new`
        // there is no source account to duplicate or save, but a preset can
        // still stamp the draft's endpoint + model fields.
        Tab::Setup => match app.setup_selection() {
            SetupSelection::Item(MainItemKind::Codex(_) | MainItemKind::Hermes(_)) => {}
            SetupSelection::Item(MainItemKind::Profile(_)) => {
                if let Some((name, _, _)) = focused_account(app) {
                    context = Some(name.to_string());
                    row_identity = Some(RowIdentity::Profile(name.clone()));
                    if app.verify_login_inflight.is_none()
                        && app
                            .config()
                            .find(&name)
                            .is_some_and(|p| p.login_is_oauth() && p.credentials.is_some())
                    {
                        scoped.push(VerifyLogin);
                    }
                    scoped.push(Duplicate);
                    scoped.push(SaveAsPreset);
                    scoped.push(ApplyPreset);
                    // The api-key row is on this tab, so the page that key comes
                    // from belongs next to it.
                    if focused_provider_console(app).is_some() {
                        scoped.push(OpenProviderConsole);
                    }
                }
            }
            // `+ new` only: its draft is what a preset stamps, and the offer
            // gates on the one predicate the apply's re-check shares.
            SetupSelection::New => {
                context = new_form_draft(app).and_then(|d| d.name.trimmed_some());
                if new_account_type_is_claude(app) {
                    scoped.push(ApplyPreset);
                }
            } // actions (re-login / delete account; refresh / remove).
        },
        // Fallback: every action the chain and its detail rows carry is bound to
        // a key of its own (⏎, ⇧↑↓, space, +/-).
        Tab::Fallback => {}
        Tab::Config => {}
        Tab::Plugin => {}
        Tab::Status => {
            actions.push(RefreshStatus);
            if app.status.selected().is_some() {
                actions.push(OpenIncidentLink);
            }
        } // Services: `r` re-runs checks, `f` walks the fix lines, ⏎/esc navigate.
    }
    // Every tab: the daemon verb that applies, none while one is in flight.
    if !app.daemon_control_busy {
        actions.push(match app.daemon_health {
            crate::daemon::DaemonHealth::Absent => StartDaemon,
            crate::daemon::DaemonHealth::Stale | crate::daemon::DaemonHealth::Fresh => StopDaemon,
        });
    }
    // action is in flight, and the daemon-start row also yields none while a
    // daemon control runs (the daemon verb's own rule).

    ActionMenuState::new(scoped, actions, context, row_identity)
}

/// Push what the account under the cursor can be told to do, returning the
/// account's identity and the name the menu titles that group with. Nothing to
/// push when the cursor sits past the accounts (an empty Overview), which is
/// also what leaves the title bare.
fn push_account_scope(
    app: &App,
    scoped: &mut Vec<ActionMenuAction>,
) -> Option<(ProfileName, String)> {
    let (name, _, _) = focused_account(app)?;
    scoped.push(ActionMenuAction::RefreshUsage);
    scoped.push(ActionMenuAction::RotateTokens);
    scoped.push(disabled_toggle_action(app));
    if focused_provider_console(app).is_some() {
        scoped.push(ActionMenuAction::OpenProviderConsole);
    }
    Some((name.clone(), name.to_string()))
}

/// The console page the focused account's endpoint mints its api key on.
/// `None` past the accounts, on an OAuth account, and on an endpoint no
/// provider claims. Both the menu's gate and the handler resolve through this
/// one function, so what is offered and what opens cannot drift apart.
fn focused_provider_console(app: &App) -> Option<&'static str> {
    let name = app.profile_name_at(app.profile_cursor.read())?;
    provider_console_by_name(app, &name)
}

/// [`focused_provider_console`] for a named account — the menu's per-row pick
/// resolves its stored claude identity through this.
fn provider_console_by_name(app: &App, name: &ProfileName) -> Option<&'static str> {
    app.config()
        .profiles
        .iter()
        .find(|p| p.name == *name)?
        .console_url()
}

/// Open the named account's provider console in the default browser
/// (detached), mirroring [`open_incident_link`]. The menu's per-row pick
/// resolves its stored claude identity through this.
fn open_console_account(app: &mut App, name: &ProfileName) {
    let Some(url) = provider_console_by_name(app, name) else {
        app.toast(ToastKind::Info, "no console page for this endpoint");
        return;
    };
    match crate::platform::open_url(url) {
        Ok(()) => app.toast(ToastKind::Info, "opening in browser"),
        Err(_) => app.toast(ToastKind::Danger, "failed to open browser"),
    }
}

/// The focused account's disable-row action, labeled by the state it would
/// leave behind — the same flip the Setup row's own label carries.
fn disabled_toggle_action(app: &App) -> ActionMenuAction {
    let currently_disabled = app
        .config()
        .profiles
        .get(app.profile_cursor.read())
        .is_some_and(Profile::is_disabled);
    if currently_disabled {
        ActionMenuAction::EnableProfile
    } else {
        ActionMenuAction::DisableProfile
    }
}

fn handle_action_menu_key(app: &mut App, key: KeyEvent) {
    if let KeyCode::Char(ch) = key.code {
        let ch_lower = ch.to_lowercase().next().unwrap_or(ch);
        let Some(Modal::ActionMenu(state)) = app.modals.last() else {
            return;
        };
        let action = state
            .items
            .iter()
            .find(|item| item.hotkey == Some(ch_lower))
            .map(|item| item.action.clone());
        if let Some(action) = action {
            let row_identity = state.row_identity.clone();
            app.pop_modal();
            dispatch_menu_pick(app, action, row_identity);
            return;
        }
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
        }
        KeyCode::Up => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                state.cursor = (state.cursor + len - 1) % len;
            }
        }
        KeyCode::Down => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                state.cursor = (state.cursor + 1) % len;
            }
        }
        KeyCode::PageUp => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                let next = {
                    let starts = state.starts.borrow();
                    let block_ends = state.block_ends.borrow();
                    page_up_cursor_by_rows(
                        state.cursor,
                        &starts,
                        &block_ends,
                        &[],
                        state.viewport.get(),
                        state.scroll.get(),
                        state.total.get(),
                    )
                };
                state.cursor = next;
            }
        }
        KeyCode::PageDown => {
            let Some(Modal::ActionMenu(state)) = app.modals.last_mut() else {
                return;
            };
            let len = state.items.len();
            if len > 0 {
                let next = {
                    let starts = state.starts.borrow();
                    let block_ends = state.block_ends.borrow();
                    page_down_cursor_by_rows(
                        state.cursor,
                        &starts,
                        &block_ends,
                        state.viewport.get(),
                        state.scroll.get(),
                        state.total.get(),
                    )
                };
                state.cursor = next;
            }
        }
        KeyCode::Enter => {
            let Some(Modal::ActionMenu(state)) = app.modals.last() else {
                return;
            };
            let action = state
                .items
                .get(state.cursor)
                .map(|item| item.action.clone());
            let row_identity = state.row_identity.clone();
            app.pop_modal();
            if let Some(action) = action {
                dispatch_menu_pick(app, action, row_identity);
            }
        }
        _ => {}
    }
}

/// The account named `name` as `(name, has_oauth_login,
/// usage_cache_is_third_party)`; `None` when the roster no longer holds it.
/// The same triple [`focused_account`] yields, looked up by name instead of the
/// cursor's index — the menu's per-row picks resolve their stored claude
/// identity through this.
fn account_by_name(app: &App, name: &ProfileName) -> Option<(ProfileName, bool, bool)> {
    let cfg = app.config();
    cfg.profiles.iter().find(|p| p.name == *name).map(|p| {
        (
            p.name.clone(),
            p.login_is_oauth(),
            p.usage_cache_is_third_party(),
        )
    })
}

/// The account under the cursor as `(name, has_oauth_login,
/// usage_cache_is_third_party)`. `profile_cursor` is shared across Overview,
/// Usage, and Setup, so this resolves the focused account on any of them.
/// `None` when the cursor sits past the profile list (e.g. `+ new`).
///
/// The OAuth bool is credential typing ([`Profile::login_is_oauth`]), not endpoint
/// routing: the actions it gates (rotate, refresh) act on the stored token chain,
/// so a hybrid holding a real pair behind a `base_url` can rotate it, and an
/// endpoint-only profile cannot.
///
/// The third bool is the shared cache selector, not `is_third_party`: the
/// refresh gate asks whether the account has a usage fetch leg, and the
/// third-party leg fetches generic api-key endpoints too — `is_third_party`
/// would refuse a litellm row the scheduler refreshes every cadence.
fn focused_account(app: &App) -> Option<(ProfileName, bool, bool)> {
    if app.usage_extra_cursor.is_some() && matches!(app.tab, Tab::Overview | Tab::Usage) {
        return None;
    }
    match app.tab {
        Tab::Overview if !matches!(app.current_main_item(), Some(MainItemKind::Profile(_))) => {
            return None;
        }
        Tab::Usage
            if !matches!(
                app.usage_selection(),
                SetupSelection::Item(MainItemKind::Profile(_))
            ) =>
        {
            return None;
        }
        Tab::Setup
            if !matches!(
                app.setup_selection(),
                SetupSelection::Item(MainItemKind::Profile(_))
            ) =>
        {
            return None;
        }
        _ => {}
    }
    let name = app.profile_name_at(app.profile_cursor.read())?;
    account_by_name(app, &name)
}

/// Queue a usage refetch for the named account, or toast why not. One shape
/// for every entry point (Usage `r` and the menu's identity-resolved pick):
/// the OAuth arm and the third-party-cache arm
/// both queue [`App::manual_refresh_one`] — the same refetch the cadence legs
/// would run — and only an endpoint-only account with no credential either leg
/// can use falls through to the nothing-to-refresh toast.
fn refresh_usage_account(app: &mut App, name: &ProfileName) {
    match account_by_name(app, name) {
        Some((name, true, _)) | Some((name, _, true)) => {
            app.manual_refresh_one(&name);
            app.toast(ToastKind::Info, format!("refreshing '{name}'"));
        }
        Some((name, false, false)) => {
            app.toast(ToastKind::Info, format!("'{name}' has no usage to refresh"));
        }
        None => {}
    }
}

/// Queue a usage refetch for the account under the cursor, or toast why not.
fn queue_focused_usage_refresh(app: &mut App) {
    if let Some(obs) = app
        .usage_extra_cursor
        .and_then(|i| app.usage_extras.get(i))
        .cloned()
    {
        if app.refresh_monitor_one(&obs.id) {
            app.toast(ToastKind::Info, format!("refreshing '{}'", obs.label));
        } else {
            app.toast(ToastKind::Info, read_only_hint(&obs));
        }
        return;
    }
    let Some(name) = app.profile_name_at(app.profile_cursor.read()) else {
        return;
    };
    refresh_usage_account(app, &name);
}

/// `r` on the Usage tab, routed by row: a claude row refreshes that account, a
/// codex row is inert (no per-account codex poll trigger exists in the TUI or
/// the scheduler's public surface).
fn usage_refresh(app: &mut App) {
    if app.usage_extra_cursor.is_some() {
        queue_focused_usage_refresh(app);
        return;
    }
    match app.usage_selection() {
        SetupSelection::Item(MainItemKind::Profile(_)) => queue_focused_usage_refresh(app),
        SetupSelection::Item(MainItemKind::Codex(_) | MainItemKind::Hermes(_))
        | SetupSelection::New => {}
    }
}

/// The toast for a claude pick whose account left the roster while the menu
/// was open: the pick acts on nothing and says so.
const CLAUDE_ROW_GONE: &str = "that account is gone";

/// Resolve a menu's stored claude row identity at dispatch: the stored name if
/// the roster still holds it. A name that left the roster raises one info toast
/// and returns `None`; a pool identity (or none) — impossible for the claude
/// actions that call this — returns `None` without a toast.
fn claude_name_at_pick(app: &mut App, identity: Option<RowIdentity>) -> Option<ProfileName> {
    match identity {
        None => None,

        Some(RowIdentity::Profile(name)) if account_by_name(app, &name).is_some() => Some(name),
        Some(RowIdentity::Profile(_)) => {
            app.toast(ToastKind::Info, CLAUDE_ROW_GONE);
            None
        }
    }
}

/// The menu's `refresh usage` pick: refresh the stored claude row, or toast
/// that the account left the roster.
fn refresh_usage_identity(app: &mut App, identity: Option<RowIdentity>) {
    let Some(name) = claude_name_at_pick(app, identity) else {
        return;
    };
    refresh_usage_account(app, &name);
}

/// The `rotate access token` action on the named account — the shared body
/// the menu's identity pick resolves into.
fn rotate_account(app: &mut App, name: &ProfileName) {
    match account_by_name(app, name) {
        // macOS refuses this rotation (`runtime::rotation_block_for`), so
        // say why up front instead of arming a confirm that no-ops.
        Some((name, true, _)) => {
            if let Some(block) = crate::runtime::rotation_block_for(&name) {
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: rotate_blocked_message(name.as_str(), block),
                    detail: Some(ROTATE_LIVE_SESSION_DETAIL.to_string()),
                    choice: true,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::Acknowledge,
                }));
                return;
            }
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("rotate access token for '{name}'?"),
                detail: None,
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::RotateOne(name.to_string()),
            }));
        }
        Some((name, _, _)) => {
            app.toast(ToastKind::Info, format!("'{name}' has no tokens to rotate"));
        }
        None => {}
    }
}

/// The menu's `rotate access token` pick.
fn rotate_tokens_identity(app: &mut App, identity: Option<RowIdentity>) {
    let Some(name) = claude_name_at_pick(app, identity) else {
        return;
    };
    rotate_account(app, &name);
}

/// The menu's disable/enable pick: run the direction the item's label named
/// against the stored claude row, or toast that the account left the roster.
fn toggle_disabled_identity(
    app: &mut App,
    identity: Option<RowIdentity>,
    direction: DisableDirection,
) {
    let Some(name) = claude_name_at_pick(app, identity) else {
        return;
    };
    toggle_account_disabled(app, &name, direction);
}

/// The menu's `open provider console` pick.
fn open_console_identity(app: &mut App, identity: Option<RowIdentity>) {
    let Some(name) = claude_name_at_pick(app, identity) else {
        return;
    };
    open_console_account(app, &name);
}

/// The menu's `duplicate account` pick.
fn duplicate_identity(app: &mut App, identity: Option<RowIdentity>) {
    let Some(name) = claude_name_at_pick(app, identity) else {
        return;
    };
    duplicate_account(app, &name);
}

/// The menu's `save as preset` pick.
fn save_preset_identity(app: &mut App, identity: Option<RowIdentity>) {
    let Some(name) = claude_name_at_pick(app, identity) else {
        return;
    };
    save_preset_account(app, &name);
}

/// The menu's `apply preset` pick: on a claude row it stamps the stored
/// account, on the `+ new` row (which carries no claude identity) it stamps
/// the draft through the cursor path, and a gone account toasts.
fn apply_preset_identity(app: &mut App, identity: Option<RowIdentity>) {
    match identity {
        None => open_preset_picker(app),
        Some(_) => {
            if let Some(name) = claude_name_at_pick(app, identity) {
                apply_preset_account(app, &name);
            }
        }
    }
}

/// Dispatch a menu pick. Every per-row action resolves the row identity the
/// menu was built for (never the cursor re-read at pick time, which a roster or
/// listing change between open and pick could have moved onto a neighbour);
/// the tab-global actions ignore the row identity and run their handlers.
fn dispatch_menu_pick(app: &mut App, action: ActionMenuAction, row_identity: Option<RowIdentity>) {
    match action {
        ActionMenuAction::VerifyLogin => start_verify_login(app),
        // Per-row actions: the identity the menu stored.
        ActionMenuAction::RefreshUsage => refresh_usage_identity(app, row_identity),
        ActionMenuAction::RotateTokens => rotate_tokens_identity(app, row_identity),
        ActionMenuAction::DisableProfile => {
            toggle_disabled_identity(app, row_identity, DisableDirection::Disable);
        }
        ActionMenuAction::EnableProfile => {
            toggle_disabled_identity(app, row_identity, DisableDirection::Enable);
        }
        ActionMenuAction::OpenProviderConsole => open_console_identity(app, row_identity),
        ActionMenuAction::Duplicate => duplicate_identity(app, row_identity),
        ActionMenuAction::SaveAsPreset => save_preset_identity(app, row_identity),
        ActionMenuAction::ApplyPreset => apply_preset_identity(app, row_identity),
        // Tab-global actions: no row identity.
        ActionMenuAction::NewAccount => start_new_account(app),
        ActionMenuAction::RefreshAll => {
            app.manual_refresh();
            app.toast(ToastKind::Info, "refreshing every account");
        }
        ActionMenuAction::StartDaemon => open_daemon_start_confirm(app),
        ActionMenuAction::StopDaemon => stop_daemon(app),
        ActionMenuAction::RefreshStatus => trigger_status_refresh(app),
        ActionMenuAction::OpenIncidentLink => open_incident_link(app),
        ActionMenuAction::ToggleEstimates => {
            if app.usage_extra_cursor.is_none() {
                toggle_show_estimates(app);
            }
        }
        ActionMenuAction::TogglePace => {
            if app.usage_extra_cursor.is_none() {
                toggle_show_pace(app);
            }
        }
        ActionMenuAction::TokensPeriodLifetime => set_token_period(app, TokenPeriod::Lifetime),
        ActionMenuAction::TokensPeriodDaily => set_token_period(app, TokenPeriod::Daily),
        ActionMenuAction::TokensPeriodWeekly => set_token_period(app, TokenPeriod::Weekly),
        ActionMenuAction::TokensPeriodMonthly => set_token_period(app, TokenPeriod::Monthly),
        ActionMenuAction::TokensShowAll => set_token_filter(app, TokenFilter::All),
        ActionMenuAction::TokensShowClaude => set_token_filter(app, TokenFilter::Claude),
        ActionMenuAction::TokensShowOthers => set_token_filter(app, TokenFilter::Others),
        ActionMenuAction::ToggleCountCache => toggle_count_cache(app),
        ActionMenuAction::ReloadTokenStats => reload_token_stats(app),
    }
}

/// Where the daemon's start and stop toasts send the user when either fails.
const DAEMON_LOG_HINT: &str = "see ~/.tollgate/daemon.log";

/// The shared daemon-start confirm, opened by the action menu's `start daemon`
/// [`ConfirmAction::StartDaemon`]. `cancel`/`esc` spawn nothing; `start` runs
/// [`start_daemon`].
fn open_daemon_start_confirm(app: &mut App) {
    app.disarm_quit();
    app.open_modal(Modal::Confirm(ConfirmState {
        message: "start daemon?".to_string(),
        detail: None,
        choice: false,
        scroll: ConfirmScroll::default(),
        on_confirm: ConfirmAction::StartDaemon,
    }));
}

/// The action menu's `start daemon`: spawn `<this binary> daemon` detached,
/// then report, off the UI thread, whether a daemon came up. The worker stays
/// to reap the child, so a daemon this TUI started and stops never lingers as
/// a zombie while the TUI runs.
fn start_daemon(app: &mut App) {
    match crate::daemon::singleton_held() {
        Ok(false) => {}
        Ok(true) => {
            app.toast(ToastKind::Info, "daemon already running");
            reprobe_daemon(app);
            return;
        }
        Err(e) => {
            app.toast(ToastKind::Danger, format!("daemon start failed\n{e:#}"));
            return;
        }
    }
    let exe = match daemon_exe() {
        Ok(exe) => exe,
        Err(e) => {
            app.toast(ToastKind::Danger, format!("daemon start failed\n{e}"));
            return;
        }
    };
    let tx = app.daemon_control_tx.clone();
    app.daemon_control_busy = true;
    app.toast(ToastKind::Info, "starting daemon");
    spawn_worker(move || match crate::daemon::spawn_detached(&exe) {
        Ok(mut child) => {
            let outcome = crate::daemon::await_start(
                &mut child,
                crate::daemon::START_WAIT,
                DAEMON_CONTROL_POLL,
            );
            let _ = tx.send(DaemonControlResult::Start(Ok(outcome)));
            let _ = child.wait();
        }
        Err(e) => {
            let _ = tx.send(DaemonControlResult::Start(Err(format!("{e:#}"))));
        }
    });
}

/// The binary `start daemon` runs: this one, as installed. A test build's
/// binary is the test harness, which would take `daemon` as a filter and run
/// the suite's daemon tests as its "daemon", so a test build refuses.
fn daemon_exe() -> std::io::Result<std::path::PathBuf> {
    if cfg!(test) {
        return Err(std::io::Error::other("a test build starts no daemon"));
    }
    std::env::current_exe().map(|exe| crate::platform::installed_exe_path(&exe))
}

/// The action menu's `stop daemon`: `--replace`'s termination, which can wait
/// out two bounded passes, so it runs off the UI thread.
fn stop_daemon(app: &mut App) {
    let tx = app.daemon_control_tx.clone();
    app.daemon_control_busy = true;
    app.toast(ToastKind::Info, "stopping daemon");
    spawn_worker(move || {
        let outcome = crate::daemon::stop_running().map_err(|e| format!("{e:#}"));
        let _ = tx.send(DaemonControlResult::Stop(outcome));
    });
}

/// How often the start worker re-checks its child.
const DAEMON_CONTROL_POLL: Duration = Duration::from_millis(50);

/// Toast each daemon start/stop outcome, re-arm the menu's verb, and
/// [`reprobe_daemon`].
fn drain_daemon_control(app: &mut App) {
    use crate::daemon::{DaemonStop, StartOutcome};
    while let Ok(result) = app.daemon_control_rx.try_recv() {
        app.daemon_control_busy = false;
        match result {
            DaemonControlResult::Start(Ok(StartOutcome::Holding)) => {
                app.toast(ToastKind::Success, "daemon started");
            }
            DaemonControlResult::Start(Ok(StartOutcome::Exited)) => app.toast(
                ToastKind::Danger,
                format!("daemon exited at start\n{DAEMON_LOG_HINT}"),
            ),
            DaemonControlResult::Start(Ok(StartOutcome::NotYet)) => app.toast(
                ToastKind::Warning,
                format!(
                    "daemon not up after {}s\n{DAEMON_LOG_HINT}",
                    crate::daemon::START_WAIT.as_secs()
                ),
            ),
            DaemonControlResult::Start(Err(e)) => {
                app.toast(ToastKind::Danger, format!("daemon start failed\n{e}"));
            }
            DaemonControlResult::Stop(Ok(DaemonStop::Stopped)) => {
                app.toast(ToastKind::Success, "daemon stopped");
            }
            DaemonControlResult::Stop(Ok(DaemonStop::NotRunning)) => {
                app.toast(ToastKind::Info, "no daemon was running");
            }
            DaemonControlResult::Stop(Ok(DaemonStop::Replaced)) => {
                app.toast(
                    ToastKind::Warning,
                    "daemon stopped\nanother daemon took over",
                );
            }
            DaemonControlResult::Stop(Err(e)) => {
                app.toast(ToastKind::Danger, format!("daemon stop failed\n{e}"));
            }
        }
        reprobe_daemon(app);
    }
}

/// Re-probe the header chip and the menu's verb at once rather than at the
/// next throttled probe.
fn reprobe_daemon(app: &mut App) {
    app.daemon_health = crate::daemon::daemon_health();
    sync_daemon_binary_change(app);
    app.last_daemon_probe = Instant::now();
}

/// Swap the Tokens model filter and re-clamp the Models cursor — the filtered
/// list can be shorter than where the cursor sat.
fn set_token_filter(app: &mut App, filter: TokenFilter) {
    app.token_filter = filter;
    app.model_detail_scroll = 0;
    clamp_token_model_cursor(app);
}

/// Swap the Tokens period lens and re-clamp the Models cursor — the scoped
/// list can be shorter than where the cursor sat.
fn set_token_period(app: &mut App, period: TokenPeriod) {
    app.token_period = period;
    app.model_detail_scroll = 0;
    clamp_token_model_cursor(app);
}

/// Setup tab keymap. Left: ↑↓ + ⏎ enters detail. Right: ↑↓ walks rows, ⏎
/// edits/toggles/arms/creates. Esc (global) returns to list.
fn handle_config_key(app: &mut App, key: KeyEvent) {
    let sel_len = app.setup_items().len() + 1; // includes trailing `+ new` row

    match app.config_focus {
        ConfigFocus::Profiles => match key.code {
            KeyCode::Up => step_setup_cursor(app, -1, sel_len),
            KeyCode::Down => step_setup_cursor(app, 1, sel_len),
            KeyCode::PageUp => {
                let next = selector_page_up(app.setup_position(), &app.setup_selector);
                app.step_selection(SelectionSlotKind::Setup, next);
            }
            KeyCode::PageDown => {
                let next = selector_page_down(app.setup_position(), &app.setup_selector);
                app.step_selection(SelectionSlotKind::Setup, next);
            }
            KeyCode::Enter => enter_config_detail(app),
            _ => {}
        },
        ConfigFocus::Actions => {
            // The codex and pool detail panes run their own row lists and
            // actions; the claude path (and the `+ new` form) keep `config_rows`.
            if let SetupSelection::Item(MainItemKind::Codex(_)) = app.setup_selection() {
                handle_codex_detail_key(app, key);
                return;
            }
            let rows = config_rows(app);
            if rows.is_empty() {
                app.config_focus = ConfigFocus::Profiles;
                app.config_draft = None;
                return;
            }
            let last = rows.len() - 1;
            app.config_action_cursor = app.config_action_cursor.min(last);
            match key.code {
                KeyCode::Up => {
                    disarm_armed_action(app);
                    app.config_action_cursor = if app.config_action_cursor == 0 {
                        last
                    } else {
                        app.config_action_cursor - 1
                    };
                }
                KeyCode::Down => {
                    disarm_armed_action(app);
                    app.config_action_cursor = if app.config_action_cursor >= last {
                        0
                    } else {
                        app.config_action_cursor + 1
                    };
                }
                KeyCode::PageUp => {
                    disarm_armed_action(app);
                    let starts = app.setup_detail_row_starts.borrow();
                    let block_ends = app.setup_detail_block_ends.borrow();
                    let tips = app.setup_detail_tips.borrow();
                    app.config_action_cursor = page_up_cursor_by_rows(
                        app.config_action_cursor,
                        &starts,
                        &block_ends,
                        &tips,
                        app.setup_detail_viewport.get(),
                        app.setup_detail_offset.get(),
                        app.setup_detail_total.get(),
                    );
                }
                KeyCode::PageDown => {
                    disarm_armed_action(app);
                    let starts = app.setup_detail_row_starts.borrow();
                    let block_ends = app.setup_detail_block_ends.borrow();
                    app.config_action_cursor = page_down_cursor_by_rows(
                        app.config_action_cursor,
                        &starts,
                        &block_ends,
                        app.setup_detail_viewport.get(),
                        app.setup_detail_offset.get(),
                        app.setup_detail_total.get(),
                    );
                }
                KeyCode::Enter => {
                    let row = rows[app.config_action_cursor];
                    // ⏎ mirrors space on the cycle rows: the `type` and `family`
                    // rows advance; the `model` row keeps ⏎ for its custom field.
                    match row {
                        ConfigRow::Type => cycle_account_type(app),
                        _ => run_config_row(app, row),
                    }
                }
                KeyCode::Char('+' | '=')
                    if rows[app.config_action_cursor] == ConfigRow::BellThreshold =>
                {
                    step_bell_threshold(app, 5.0);
                }
                KeyCode::Char('-' | '_')
                    if rows[app.config_action_cursor] == ConfigRow::BellThreshold =>
                {
                    step_bell_threshold(app, -5.0);
                }
                KeyCode::Char('+' | '=')
                    if rows[app.config_action_cursor] == ConfigRow::UsageProbeInterval =>
                {
                    step_usage_probe_interval(app, USAGE_PROBE_STEP_MS);
                }
                KeyCode::Char('-' | '_')
                    if rows[app.config_action_cursor] == ConfigRow::UsageProbeInterval =>
                {
                    step_usage_probe_interval(app, -USAGE_PROBE_STEP_MS);
                }
                KeyCode::Char(' ') => {
                    let row = rows[app.config_action_cursor];
                    // `space` drives state controls only; action rows and text
                    // fields act on `↵` alone.
                    if row.is_state_control() {
                        match row {
                            ConfigRow::Model => cycle_model(app),
                            ConfigRow::Type => cycle_account_type(app),
                            _ => run_config_row(app, row),
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Keys on the Setup codex detail pane: ↑↓ walk its action rows (wrapping),
/// page keys page them, ⏎ runs the row — `re-login` starts the codex
/// browser flow, `delete account` arms the inline 2-step then deletes.
/// Clear the codex detail's armed 2-step (flag + identity together, so neither
/// can outlive the other and re-arm on a stale name).
fn disarm_codex_2step(app: &mut App) {
    app.codex_detail_armed = false;
}

fn handle_codex_detail_key(app: &mut App, key: KeyEvent) {
    let rows = CodexDetailRow::ALL;
    let last = rows.len() - 1;
    app.config_action_cursor = app.config_action_cursor.min(last);
    match key.code {
        KeyCode::Up => {
            disarm_codex_2step(app);
            app.config_action_cursor = if app.config_action_cursor == 0 {
                last
            } else {
                app.config_action_cursor - 1
            };
        }
        KeyCode::Down => {
            disarm_codex_2step(app);
            app.config_action_cursor = if app.config_action_cursor >= last {
                0
            } else {
                app.config_action_cursor + 1
            };
        }
        KeyCode::PageUp => {
            disarm_codex_2step(app);
            let starts = app.setup_detail_row_starts.borrow();
            let block_ends = app.setup_detail_block_ends.borrow();
            let tips = app.setup_detail_tips.borrow();
            app.config_action_cursor = page_up_cursor_by_rows(
                app.config_action_cursor,
                &starts,
                &block_ends,
                &tips,
                app.setup_detail_viewport.get(),
                app.setup_detail_offset.get(),
                app.setup_detail_total.get(),
            );
        }
        KeyCode::PageDown => {
            disarm_codex_2step(app);
            let starts = app.setup_detail_row_starts.borrow();
            let block_ends = app.setup_detail_block_ends.borrow();
            app.config_action_cursor = page_down_cursor_by_rows(
                app.config_action_cursor,
                &starts,
                &block_ends,
                app.setup_detail_viewport.get(),
                app.setup_detail_offset.get(),
                app.setup_detail_total.get(),
            );
        }
        KeyCode::Enter => {
            let Some(i) = app.setup_codex_index() else {
                return;
            };
            let Some(name) = app.codex_rows.get(i).map(|r| r.name.clone()) else {
                return;
            };
            match rows[app.config_action_cursor] {
                CodexDetailRow::ReLogin => start_codex_login(app, name.as_str()),
                CodexDetailRow::Delete => {
                    if app.codex_detail_armed {
                        // The second ⏎ acts on the identity the SELECTION
                        // still holds: any path that moved it while armed
                        // disarmed first, so it IS the armed account.
                        app.codex_detail_armed = false;
                        let Some(name) = app
                            .setup_codex_index()
                            .and_then(|i| app.codex_rows.get(i).map(|r| r.name.clone()))
                        else {
                            return;
                        };
                        perform_codex_delete(app, name.as_str());
                    } else {
                        app.codex_detail_armed = true;
                    }
                }
            }
        }
        _ => {}
    }
}

/// Delete a codex account from the Setup detail pane, behind the same
/// rotation guard, live-session gate and store write the CLI's
/// `tollgate delete <name>` runs. The selection follows the drop-placement rule:
/// ascend to the selector, then land on the row now at the deleted row's flat
/// index, clamped to the last item row (`+ new` only when none exists) — never
/// wherever `profile_cursor` happened to sit.
fn perform_codex_delete(app: &mut App, name: &str) {
    // The deleted row's codex-band index, for the flat drop placement (the row
    // now at `profile_count() + old_index`, clamped, across bands).
    let old_index = app
        .codex_rows
        .iter()
        .position(|row| row.name.as_str() == name);
    let result = rotation_guard_for_mutation(&ProfileName::from(name))
        .and_then(|rotation| crate::actions::delete_codex_profile(name, false, &rotation));
    match result {
        Ok(detached) => {
            // The flat index the deleted row held, for the drop placement: the
            // row now at that index, clamped (across bands), the `+ new` row
            // only when no item row exists.
            let flat = app.profile_count() + old_index.unwrap_or(0);
            app.codex_rows = codex_rows();
            disarm_codex_2step(app);
            app.config_focus = ConfigFocus::Profiles;
            app.config_action_cursor = 0;
            let items = app.setup_items();
            let pos = if items.is_empty() {
                0
            } else {
                flat.min(items.len() - 1)
            };
            app.place_setup_selection(pos);
            app.toast(ToastKind::Success, format!("deleted '{name}'"));
            // The CLI's postscript: a delete that detached the operator's own
            // `~/.codex/auth.json` slot logs their codex out from that moment.
            if let Some(slot) = detached {
                app.toast(
                    ToastKind::Warning,
                    format!(
                        "{} followed that profile's chain and is detached now, so your own \
                         codex has no login; run `codex login` to mint a fresh one",
                        slot.display()
                    ),
                );
            }
        }
        Err(e) => {
            disarm_codex_2step(app);
            app.toast(ToastKind::Danger, format!("delete failed\n{e:#}"));
        }
    }
}

/// Start the codex browser re-login: preflight the name, then run the browser
/// flow on a worker — it binds codex's registered loopback ports and opens the
/// browser, never on the UI thread — and show the codex login modal. The worker
/// LANDS the minted chain itself (behind the blocking rotation flock) and sends
/// the canonical name or error sentence down the result channel; the UI drains
/// only that outcome through [`drain_codex_login`].
fn start_codex_login(app: &mut App, name: &str) {
    if guest_refused(app) {
        return;
    }

    if let Some(session) = app.codex_login.as_ref() {
        // A second ⏎ on `re-login` while one runs reopens its modal (the claude
        // precedent); only a different account names the running one.
        if session.name != name {
            toast_login_in_progress(app, session.name.clone());
        }
        open_codex_login_modal(app);
        return;
    }
    // A TUI re-login never recreates a deleted account: it lands only on the
    // one the roster holds now (the CLI keeps its create-or-reauth).
    let (trimmed, account) = match crate::actions::codex_browser_preflight(name) {
        Ok((trimmed, Some(account))) => (trimmed, account),
        Ok((trimmed, None)) => {
            app.toast(
                ToastKind::Danger,
                format!("login failed\ncodex profile '{trimmed}' no longer exists"),
            );
            return;
        }
        Err(e) => {
            app.toast(ToastKind::Danger, format!("login failed\n{e:#}"));
            return;
        }
    };
    app.codex_login_generation += 1;
    let generation = app.codex_login_generation;
    let cancel = crate::codex_login::LoginCancel::new();
    app.codex_login = Some(CodexLoginSession {
        name: trimmed.clone(),
        generation,
        url: None,
        cancel: cancel.clone(),
        is_new: false,
    });
    #[cfg(test)]
    let worker_fn = app.codex_login_worker;
    let url_tx = app.codex_login_url_tx.clone();
    let result_tx = app.codex_login_result_tx.clone();
    spawn_worker(move || {
        let announce = move |url: &str| {
            let _ = url_tx.send((generation, url.to_string()));
        };
        #[cfg(test)]
        let outcome = worker_fn(&announce, &cancel);
        #[cfg(not(test))]
        let outcome =
            crate::codex_login::login_with_cancel(&announce, &cancel).map_err(|e| format!("{e:#}"));
        // A cancel seen here, BEFORE the landing, drops the minted chain
        // unwritten (it was never used, so the account's current chain stays
        // valid) — the cancel toast already said "login canceled". Once the
        // landing starts it is the commit point: its outcome always reaches a
        // toast, even when a cancel arrives during it.
        let res = if cancel.is_canceled() {
            Err("login canceled".to_string())
        } else {
            outcome.and_then(|o| {
                crate::actions::land_codex_login(&trimmed, &o, Some(&account))
                    .map_err(|e| format!("{e:#}"))
            })
        };
        let _ = result_tx.send((generation, trimmed.clone(), res));
    });
    open_codex_login_modal(app);
}

/// Cancel an in-flight codex re-login: signal the worker's listener wait (its
/// registered ports free), drop the session, and toast. The worker's own
/// pre-landing cancel check drops the minted chain unwritten; a landing that
/// already started is the commit point and still toasts its outcome.
fn cancel_codex_login(app: &mut App) {
    if let Some(session) = app.codex_login.take() {
        session.cancel.cancel();
    }
    close_codex_login_modal(app);
    app.toast(ToastKind::Info, "login canceled");
}

fn open_codex_login_modal(app: &mut App) {
    if !app.modals.iter().any(|m| matches!(m, Modal::CodexLogin)) {
        app.open_modal(Modal::CodexLogin);
    }
}

fn close_codex_login_modal(app: &mut App) {
    app.retain_modals(|m| !matches!(m, Modal::CodexLogin));
}

/// Fold a finished codex login into the app on the UI thread: the worker
/// already landed the minted chain (rename guard, live-session refusal,
/// account-swap refusal, store write), so the UI drains only the outcome — the
/// canonical name on success (re-reading the codex snapshot), or the refusal /
/// failure sentence, which toasts and leaves the store unchanged.
fn drain_codex_login(app: &mut App) {
    if crate::identity::upstream_active() {
        cancel_codex_login(app);
        return;
    }
    while let Ok((generation, url)) = app.codex_login_url_rx.try_recv() {
        if app
            .codex_login
            .as_ref()
            .is_some_and(|s| s.generation == generation)
            && let Some(session) = app.codex_login.as_mut()
        {
            session.url = Some(url);
        }
    }
    while let Ok((generation, name, result)) = app.codex_login_result_rx.try_recv() {
        if app.codex_login.as_ref().map(|s| s.generation) == Some(generation) {
            app.codex_login = None;
            close_codex_login_modal(app);
        } else if matches!(&result, Err(e) if e == "login canceled") {
            // A cancel took the session before the landing: the cancel toast
            // already reported it. A landing that had started is the commit
            // point, so any other outcome falls through and toasts.
            continue;
        }
        match result {
            Ok(canonical) => {
                app.codex_rows = codex_rows();
                app.toast(ToastKind::Success, format!("logged in '{canonical}'"));
            }
            Err(e) => app.toast(ToastKind::Danger, format!("login for '{name}' failed\n{e}")),
        }
    }
}

/// Alias overrides collapse: render the ones the draft shows (or, with no
/// draft, the ones the profile sets), tuck the rest behind a single
/// `+ model override` reveal until ⏎ expands them.
fn push_override_rows(
    rows: &mut Vec<ConfigRow>,
    draft: Option<&ConfigDraft>,
    profile: Option<&Profile>,
) {
    let expanded = draft.is_some_and(|d| d.overrides_expanded);
    let override_set = |row: ConfigRow| match draft {
        Some(d) => d.overrides_shown.contains(&row),
        None => {
            let v = match row {
                ConfigRow::OpusModel => profile.and_then(|p| p.models.opus.as_deref()),
                ConfigRow::SonnetModel => profile.and_then(|p| p.models.sonnet.as_deref()),
                ConfigRow::HaikuModel => profile.and_then(|p| p.models.haiku.as_deref()),
                ConfigRow::FableModel => profile.and_then(|p| p.models.fable.as_deref()),
                ConfigRow::SubagentModel => profile.and_then(|p| p.models.subagent.as_deref()),
                _ => None,
            };
            v.is_some_and(|s| !s.trim().is_empty())
        }
    };
    let mut any_collapsed = false;
    for row in OVERRIDE_ROWS {
        if expanded || override_set(row) {
            rows.push(row);
        } else {
            any_collapsed = true;
        }
    }
    if any_collapsed {
        rows.push(ConfigRow::ModelOverrideAdd);
    }
}

/// The `+ new` create-form rows, per the draft's account type. Claude keeps
/// today's rows; codex offers `name`, `+ login`, `+ capture current login`
/// (while the operator's codex login is capturable) and `create account`;
/// account` row.
fn new_account_rows(app: &App, draft: Option<&ConfigDraft>) -> Vec<ConfigRow> {
    let account_type = draft.map(|d| d.account_type).unwrap_or_default();
    let mut rows = vec![ConfigRow::Type];
    match account_type {
        NewAccountType::Claude => {
            rows.push(ConfigRow::Name);
            rows.push(ConfigRow::BaseUrl);
            // The api key only means something once a base url makes this an API
            // account, so it stays hidden until one is typed.
            if draft.is_some_and(|d| !d.base_url.value.trim().is_empty()) {
                rows.push(ConfigRow::ApiKey);
            }
            // Env rows stay existing-account-only.
            rows.push(ConfigRow::Model);
            push_override_rows(&mut rows, draft, None);
            if draft.is_none_or(|d| d.base_url.value.trim().is_empty()) {
                rows.push(ConfigRow::Login);
            }
            // The live-login capture row sits right below `+ login` and survives
            // into api mode (`+ login` doesn't): a live setup can be an endpoint
            // too. Hidden while the live login is saved or absent — nothing to
            // capture.
            if app.unsaved_live_login {
                rows.push(ConfigRow::CaptureLogin);
            }
            rows.push(ConfigRow::Create);
        }
        NewAccountType::Codex => {
            rows.push(ConfigRow::Name);
            rows.push(ConfigRow::Login);
            // Read off the tick's cache, never the capture's disk reads on the
            // render path (the `unsaved_live_login` precedent).
            if app.codex_operator_capturable {
                rows.push(ConfigRow::CaptureLogin);
            }
            rows.push(ConfigRow::Create);
        }
    }
    rows
}

/// Detail rows for the current selection. `+ new` → create form; account →
/// settings (auto-start only for OAuth).
pub(crate) fn config_rows(app: &App) -> Vec<ConfigRow> {
    // Resolve the selection and the `+ new` draft BEFORE the config guard (see
    // `build_snap`): both re-run `setup_selection` under their own short guard.
    let selection = app.setup_selection();
    let new_draft = new_form_draft(app);
    let cfg = app.config();
    let profile_idx = match selection {
        SetupSelection::New => return new_account_rows(app, new_draft),
        SetupSelection::Item(MainItemKind::Profile(i)) => i,
        SetupSelection::Item(MainItemKind::Codex(_) | MainItemKind::Hermes(_)) => {
            return Vec::new();
        }
    };
    let draft = app.config_draft.as_ref();
    let profile = cfg.profiles.get(profile_idx);
    // `is_api` tracks the base-url buffer live (draft wins): api key shows only
    // in API mode, auto-start (OAuth-only) only when it's empty, so both flip the
    // instant a base url is typed.
    let is_api = match draft {
        Some(d) => !d.base_url.value.trim().is_empty(),
        None => profile.map(|p| !p.is_oauth()).unwrap_or(false),
    };
    let mut rows = vec![ConfigRow::Name];
    // auto-start sits right below name (OAuth-only; mutually exclusive with api key).
    if !is_api {
        rows.push(ConfigRow::AutoStart);
        // The probe below it, only where `/usage` has nothing to read and a
        // setup token is stored: beside a pair the switch would do nothing,
        // and with no token the probe has nothing to send.
        if profile
            .is_some_and(|p| crate::usage::usage_poll_pair(p).is_none() && p.has_long_lived_sidecar)
        {
            rows.push(ConfigRow::UsageProbe);
            // Its cadence, only while it runs: the row edits nothing else.
            if profile.is_some_and(|p| p.usage_probe) {
                rows.push(ConfigRow::UsageProbeInterval);
            }
        }
    }
    rows.push(ConfigRow::BellThreshold);
    rows.push(ConfigRow::BaseUrl);
    if is_api {
        rows.push(ConfigRow::ApiKey);
    }
    rows.push(ConfigRow::Model);
    push_override_rows(&mut rows, draft, profile);

    // One row per custom env entry (sorted), then the `+ add env` row. Indices
    // must match the sorted env snapshot used by the renderer and the commit path.
    let env_count = profile.map(|p| p.env.len()).unwrap_or(0);
    rows.extend((0..env_count).map(ConfigRow::EnvEntry));
    rows.push(ConfigRow::EnvAdd);
    // Log in / re-login, then log out once a credential exists — for both OAuth
    // (browser mint) and API (base url + api key) accounts. "Has a credential"
    // reads the OAuth token or the api key depending on the account's credential
    // typing (`login_is_oauth`, not the endpoint-shaped `is_api`): the log-out
    // row acts on what's on disk, so a hybrid's token can't be hidden behind
    // either a base url or an uncommitted draft.
    rows.push(ConfigRow::Login);
    let has_creds = if profile.is_some_and(|p| p.login_is_oauth()) {
        profile.and_then(|p| p.credentials.as_ref()).is_some()
    } else {
        profile
            .and_then(|p| p.api_key.as_deref())
            .is_some_and(|k| !k.trim().is_empty())
    };
    // A torn `credentials.json` loads logged out but is still a login on disk:
    // only a log out clears it, so the row shows over it too (beside, never in
    // place of, the typed credential above).
    let has_creds = has_creds || profile.is_some_and(|p| p.unreadable_credentials_store.is_some());
    if has_creds {
        rows.push(ConfigRow::DeleteCreds);
    }
    // CLA-SPLIT escape hatch, beside the log-out row because it drops the OTHER
    // credential a split profile holds. Gated on ANY long-lived piece existing
    // — the sidecar (`session_token_status`, so a mis-fill gets the same
    // exit), the preserved mint backup, or a set `rolling_token` flag — never
    // on the split being ENGAGED: a flag with no sidecar is exactly the state
    // where hiding the row leaves the daemon to re-create what the operator
    // has no surface left to remove (the CLI's widened nothing-to-clear gate,
    // rendered). The no-other-login refusal is a dim, not a hide — see
    // `run_config_row`.
    if profile.is_some_and(|p| {
        crate::claude::session_token_status(&p.name).is_some()
            || p.rolling_token
            || crate::claude::has_static_backup(&p.name)
    }) {
        rows.push(ConfigRow::ClearSessionToken);
    }
    // The account-actions group's tail: disable/enable (reversible) one severity
    // notch below delete (irreversible), which always stays the very last row.
    rows.push(ConfigRow::Disabled);
    rows.push(ConfigRow::Delete);
    rows
}

/// Enter the detail pane, seeding a draft for the current selection.
fn enter_config_detail(app: &mut App) {
    app.config_action_cursor = 0;
    // Descending into the shown row adopts it: a held-but-hidden identity is
    // replaced, never restored over the pane the user is now editing.
    app.setup.clear();
    // A suspended `+ new` form keeps its home before any arm reaches
    // `config_draft`: the claude arm replaces it and the codex/pool arms drop
    // it. The slot holds forms at rest, so an open field closes first (its
    // typed text stays). When the slot already holds typed work the writer
    // hands the form back; no key path reaches this descend with both forms
    // held, so this one stays on its own pane and row, and the target never
    // opens.
    if let Some(field) = app
        .config_draft
        .as_ref()
        .filter(|d| d.editing_name.is_none())
        .and_then(|d| d.active)
    {
        cancel_config_edit(app, field);
    }
    if app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.editing_name.is_none())
        && let Some(draft) = app.config_draft.take()
        && let Some(draft) = set_new_account_draft(app, draft)
    {
        app.config_draft = Some(draft);
        app.setup.item = None;
        app.setup.index = None;
        app.profile_cursor.set(app.profile_count());
        app.config_focus = ConfigFocus::Actions;
        return;
    }
    match app.setup_selection() {
        SetupSelection::New => {
            // Restore the suspended `+ new` form, else build a fresh one.
            app.config_draft = app
                .new_account_draft
                .take()
                .or_else(|| Some(build_draft_new()));
        }
        SetupSelection::Item(MainItemKind::Profile(i)) => {
            if let Some(name) = app.profile_name_at(i) {
                app.config_draft = Some(build_draft_existing(app, &name));
            } else {
                return;
            }
        }
        SetupSelection::Item(MainItemKind::Codex(_) | MainItemKind::Hermes(_)) => {
            app.config_draft = None;
        } // Codex and Hermes detail panes have no draft: drop a suspended
          // existing-account field draft, or it would keep the keyboard over this
          // pane (`e`'s landing arrives with a suspended edit still in
          // `config_draft`). A `+ new` form never reaches here — it moved to its
          // own slot above.
    }
    app.config_focus = ConfigFocus::Actions;
}

/// Jump to the `+ new` create form (global `n`): disarm, suspend and switch
/// like any other switch, then land on the `+ new` row restoring a
/// suspended form instead of building over it.
fn start_new_account(app: &mut App) {
    request_target(app, LossWarningTarget::NewAccount);
}

/// `start_new_account`'s landing: switch to Setup and open (or restore) the
/// `+ new` form.
fn land_new_account(app: &mut App) {
    // `n` presets the type from the Overview cursor's section; with nothing
    // codex → codex, else claude); any other tab (Setup included) opens on
    // claude. Read before the switch.
    let preset = if app.tab == Tab::Overview {
        match app.current_main_item() {
            Some(item) => NewAccountType::from_main_item(Some(item)),
            None => match app.harness_filter {
                HarnessFilter::Codex => NewAccountType::Codex,
                HarnessFilter::All | HarnessFilter::Claude | HarnessFilter::Hermes => {
                    NewAccountType::Claude
                }
            },
        }
    } else {
        NewAccountType::Claude
    };
    do_switch(app, Tab::Setup);
    app.setup.item = None;
    app.setup.index = None;
    app.setup.clear();
    app.profile_cursor.set(app.profile_count());
    app.config_action_cursor = 0;
    // Restore a suspended `+ new` form: an open-field one still sits in
    // `config_draft` (the suspend kept it), an at-rest one in its own home.
    if app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.editing_name.is_none())
    {
        // Keep the suspended form, field open or not.
    } else if let Some(draft) = app.new_account_draft.take() {
        // A suspended form restores exactly as suspended: type, name and stage
        // all kept (a switch suspends what it leaves). The section preset below
        // applies only to a fresh form.
        app.config_draft = Some(draft);
    } else {
        let mut draft = build_draft_new();
        draft.account_type = preset;
        app.config_draft = Some(draft);
    }
    app.config_focus = ConfigFocus::Actions;
}

/// Jump to the focused Overview row's Setup editor (global `e`): the same
/// loss-warning route as `n`, landing on the row that fixes the account's dead
/// credential (or the editor's first row for a healthy account).
fn start_edit_selected(app: &mut App, item: MainItemKind) {
    let target = match item {
        MainItemKind::Profile(idx) => app.profile_name_at(idx).map(EditTarget::Profile),
        other => app.setup_item_for(other).map(EditTarget::Item),
    };
    if let Some(target) = target {
        request_target(app, LossWarningTarget::EditAccount(target));
    }
}

/// `start_edit_selected`'s landing: switch to Setup, select the item the way an
/// Overview pick does, open its editor, then put the action cursor on the row
/// the dead-credential ladder names. An identity the current roster or listing
/// no longer holds lands nowhere.
fn land_edit_account(app: &mut App, target: EditTarget) {
    let item = match &target {
        EditTarget::Profile(name) => app
            .config()
            .profiles
            .iter()
            .position(|p| p.name == *name)
            .map(MainItemKind::Profile),
        EditTarget::Item(item) => app.resolve_setup_item(item),
    };
    let Some(item) = item else { return };
    do_switch(app, Tab::Setup);
    let pos = app.flat_index_of(item);
    app.place_setup_selection(pos);
    enter_config_detail(app);
    // A refused move keeps the `+ new` form on its own row: the target's
    // landing row is not that form's.
    if app.setup_selection() != SetupSelection::Item(item) {
        return;
    }
    app.config_action_cursor = match &target {
        EditTarget::Profile(name) => edit_landing_row(app, name),
        // The codex and pool panes open on their first row.
        EditTarget::Item(_) => 0,
    };
}

/// The Setup action row `e` lands a claude account on: the dead-credential
/// ladder's row, else the editor's first row.
fn edit_landing_row(app: &App, name: &ProfileName) -> usize {
    // `key_rejected_names` locks the config internally, so read it before the
    // config guard below is held.
    let key_rejected = app.key_rejected_names();
    let target = {
        let cfg = app.config();
        let Some(profile) = cfg.find(name) else {
            return 0;
        };
        match crate::fallback::dead_credential_reason(&cfg, profile, &key_rejected) {
            Some(crate::fallback::BlockedReason::KeyRejected) => ConfigRow::ApiKey,
            Some(
                crate::fallback::BlockedReason::AuthBroken
                | crate::fallback::BlockedReason::LoggedOut,
            ) => ConfigRow::Login,
            // A refused setup token (`TokenRejected`), with or without a stored
            // pair: no row mints a new token (`tollgate login <name> --setup-token`
            // does), so it opens on the first row, above which the token row
            // shows that fix.
            _ => return 0,
        }
    };
    config_row_index(app, target).unwrap_or(0)
}

pub(crate) fn build_draft_new() -> ConfigDraft {
    ConfigDraft {
        editing_name: None,
        name: InputState::new(""),
        base_url: InputState::new(""),
        api_key: InputState::new(""),
        model: InputState::new(""),
        opus_model: InputState::new(""),
        sonnet_model: InputState::new(""),
        haiku_model: InputState::new(""),
        fable_model: InputState::new(""),
        subagent_model: InputState::new(""),
        bell_threshold: InputState::new(""),
        usage_probe_interval: InputState::new(""),
        env_value: InputState::new(""),
        env_new_key: InputState::new(""),
        env_just_added: None,
        active: None,
        field_seed: None,
        armed_action: None,
        relogin_chain: false,
        overrides_expanded: false,
        overrides_shown: Vec::new(),
        captured_login: None,
        preset_env: std::collections::BTreeMap::new(),
        account_type: NewAccountType::Claude,
        name_refusal: None,
        create_refusal: None,
    }
}

fn build_draft_existing(app: &App, name: &ProfileName) -> ConfigDraft {
    let cfg = app.config();
    let profile = cfg.find(name);
    let m = profile.map(|p| p.models.clone()).unwrap_or_default();
    let mut draft = ConfigDraft {
        editing_name: Some(name.to_string()),
        name: InputState::new(name),
        base_url: InputState::new(profile.and_then(|p| p.base_url.as_deref()).unwrap_or("")),
        api_key: InputState::new(profile.and_then(|p| p.api_key.as_deref()).unwrap_or("")),
        model: InputState::new(m.default.as_deref().unwrap_or("")),
        opus_model: InputState::new(m.opus.as_deref().unwrap_or("")),
        sonnet_model: InputState::new(m.sonnet.as_deref().unwrap_or("")),
        haiku_model: InputState::new(m.haiku.as_deref().unwrap_or("")),
        fable_model: InputState::new(m.fable.as_deref().unwrap_or("")),
        subagent_model: InputState::new(m.subagent.as_deref().unwrap_or("")),
        bell_threshold: InputState::new(&row_committed_value(
            profile,
            name,
            ConfigRow::BellThreshold,
        )),
        usage_probe_interval: InputState::new(&row_committed_value(
            profile,
            name,
            ConfigRow::UsageProbeInterval,
        )),
        env_value: InputState::new(""),
        env_new_key: InputState::new(""),
        env_just_added: None,
        active: None,
        field_seed: None,
        armed_action: None,
        relogin_chain: false,
        overrides_expanded: false,
        overrides_shown: Vec::new(),
        captured_login: None,
        preset_env: std::collections::BTreeMap::new(),
        account_type: NewAccountType::Claude,
        name_refusal: None,
        create_refusal: None,
    };
    draft.overrides_shown = draft.filled_overrides();
    draft
}

/// Back out of the Setup detail pane, dropping the draft. A `+ new` draft
/// holding a minted login loses it with the form — say so instead of
/// discarding a real browser round-trip silently. A stashed live login needs
/// no warning: re-pressing the row re-reads the same file.
fn leave_config_detail(app: &mut App) {
    let mint_dropped = app.config_draft.as_ref().is_some_and(|d| {
        d.editing_name.is_none() && matches!(d.captured_login, Some(DraftLogin::Mint(_)))
    });
    app.config_focus = ConfigFocus::Profiles;
    app.config_draft = None;
    // An armed codex/pool 2-step disarms on esc/ascend: the label restores and
    // no stale identity outlives the pane (the Inline 2-step rule).
    disarm_codex_2step(app);
    if mint_dropped {
        app.toast(
            ToastKind::Warning,
            "closing the form dropped the login you just captured",
        );
    }
}

/// Disarm whichever account-action row is armed when the cursor moves off it.
fn disarm_armed_action(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_action = None;
    }
}

/// ⏎ on a detail row: text → capture, toggle → flip, delete → arm/confirm, create → commit.
fn run_config_row(app: &mut App, row: ConfigRow) {
    // ⏎ opens no editor while the base url still names the gateway.

    // Env rows seed their buffer before editing, so they're driven out of band.
    match row {
        ConfigRow::EnvEntry(i) => {
            enter_env_value_edit(app, i);
            return;
        }
        ConfigRow::EnvAdd => {
            enter_env_add_edit(app);
            return;
        }
        ConfigRow::ModelOverrideAdd => {
            // Reveal the unset alias-override rows inline; no buffer to seed.
            if let Some(d) = app.config_draft.as_mut() {
                d.overrides_expanded = true;
            }
            return;
        }
        _ => {}
    }
    // Text rows plus the `model` row's custom field open an inline editor.
    if row.is_text() || row == ConfigRow::Model {
        // The alert buffer reseeds from the profile as it opens: a reload may
        // have moved the value since the draft was built.
        if row == ConfigRow::BellThreshold {
            let saved = app
                .config_draft
                .as_ref()
                .and_then(|d| d.editing_name.clone())
                .map(ProfileName::from)
                .map(|name| {
                    let cfg = app.config();
                    row_committed_value(cfg.find(&name), &name, row)
                });
            if let (Some(saved), Some(d)) = (saved, app.config_draft.as_mut()) {
                d.bell_threshold = InputState::new(&saved);
            }
        }
        // As the alert buffer: `+`/`-` or a reload may have moved the interval.
        if row == ConfigRow::UsageProbeInterval {
            let saved = app
                .config_draft
                .as_ref()
                .and_then(|d| d.editing_name.clone())
                .map(ProfileName::from)
                .map(|name| {
                    let cfg = app.config();
                    row_committed_value(cfg.find(&name), &name, row)
                });
            if let (Some(saved), Some(d)) = (saved, app.config_draft.as_mut()) {
                d.usage_probe_interval = InputState::new(&saved);
            }
        }
        if let Some(d) = app.config_draft.as_mut() {
            d.active = Some(row);
            d.field_seed = d.field(row).map(|i| i.value.clone());
            if let Some(input) = d.field_mut(row) {
                input.end();
            }
        }
        return;
    }
    let name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    match row {
        ConfigRow::Disabled => {
            if let Some(name) = name {
                let gated =
                    app.config().is_active(&name) || crate::runtime::has_live_session(&name);
                if gated {
                    return;
                }
                let currently_disabled = app.config().find(&name).is_some_and(Profile::is_disabled);
                let armed = app
                    .config_draft
                    .as_ref()
                    .is_some_and(|d| d.armed_action == Some(ConfigRow::Disabled));
                // Enabling is harmless — immediate, no arm. Disabling has real
                // operational impact (drops the account from auto-switch/
                // polling/status mid-flight), so it reuses Delete's
                // press-to-arm → confirm class instead of firing on one ⏎.
                if currently_disabled || armed {
                    let direction = if currently_disabled {
                        DisableDirection::Enable
                    } else {
                        DisableDirection::Disable
                    };
                    toggle_profile_disabled(app, &name, direction);
                    // Clear rather than leave a stale arm: unlike Delete (whose
                    // confirm removes the row entirely), this row survives its
                    // own toggle, so a later disable must re-arm from scratch.
                    disarm_armed_action(app);
                } else {
                    arm_action(app, ConfigRow::Disabled);
                }
            }
        }
        ConfigRow::AutoStart => {
            if let Some(name) = name {
                toggle_auto_start(app, &name);
            }
        }
        ConfigRow::UsageProbe => {
            if let Some(name) = name {
                toggle_usage_probe(app, &name);
            }
        }
        ConfigRow::Delete => {
            let armed = app
                .config_draft
                .as_ref()
                .is_some_and(|d| d.armed_action == Some(ConfigRow::Delete));
            match (armed, name) {
                (true, Some(name)) => perform_delete(app, &name),
                _ => arm_action(app, ConfigRow::Delete),
            }
        }
        ConfigRow::Create => {
            let account_type = app
                .config_draft
                .as_ref()
                .map(|d| d.account_type)
                .unwrap_or_default();
            match account_type {
                NewAccountType::Claude => commit_new_account(app),
                NewAccountType::Codex => commit_codex_account(app),
            }
        }
        ConfigRow::Login => {
            // The `+ new` form's type through the one lookup, so this dispatcher
            let account_type = new_form_draft(app)
                .map(|d| d.account_type)
                .unwrap_or_default();
            match account_type {
                NewAccountType::Codex => {
                    start_codex_new_login(app);
                    return;
                }
                NewAccountType::Claude => {}
            }
            // New-form draft has no editing_name → validate the typed name now and
            // create-on-mint; an existing draft re-logs in place.
            let editing = app
                .config_draft
                .as_ref()
                .and_then(|d| d.editing_name.clone());
            match login_row_flow(app, editing.as_deref()) {
                LoginRowFlow::Console { site, region } => {
                    let name = editing.unwrap_or_default();
                    start_console_login(app, name, site, region);
                    return;
                }
                LoginRowFlow::ApiKey => {
                    start_api_relogin(app);
                    return;
                }
                LoginRowFlow::OauthMint => {}
            }
            if let Some((name, is_new)) = oauth_login_target(app, editing) {
                begin_oauth_login(app, name, is_new);
            }
        }
        ConfigRow::CaptureLogin => {
            let account_type = app
                .config_draft
                .as_ref()
                .map(|d| d.account_type)
                .unwrap_or_default();
            if account_type == NewAccountType::Codex {
                stage_codex_capture(app);
                return;
            }
            spawn_capture_login(app);
        }
        ConfigRow::DeleteCreds => {
            if let Some(name) = name {
                let (is_api, has_unreadable_store, has_api_key) = {
                    let cfg = app.config();
                    let profile = cfg.find(&name);
                    (
                        profile.map(|p| !p.login_is_oauth()).unwrap_or(false),
                        profile.is_some_and(|p| p.unreadable_credentials_store.is_some()),
                        profile
                            .and_then(|p| p.api_key.as_deref())
                            .is_some_and(|k| !k.trim().is_empty()),
                    )
                };
                let detail = if is_api && has_unreadable_store {
                    if has_api_key {
                        "blanks the api key and deletes the unreadable credentials.json; keeps the base url, model, and env. re-login any time."
                    } else {
                        "deletes the unreadable credentials.json; keeps the base url, model, and env. re-login any time."
                    }
                } else if is_api {
                    "blanks the api key; keeps the base url, model, and env. re-login any time."
                } else {
                    "blanks the login; keeps the account, model, env, and chain slot. re-login any time."
                };
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: format!("log out of '{name}'?"),
                    detail: Some(detail.to_string()),
                    choice: false,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::BlankCredentials(name.to_string()),
                }));
            }
        }
        ConfigRow::ClearSessionToken => {
            if let Some(name) = name {
                // Same refusal the CLI makes (`cmd_static_token_clear`), on the
                // same condition: with no other login stored, clearing would
                // strip the account's last credential — but only a stored PIECE
                // (a sidecar or a preserved mint) is a credential to strip. A
                // flag-only account disarms without touching one, so it clears
                // here exactly as the CLI does, instead of a silent return
                // behind a row the widening made visible.
                let stores_other_login = {
                    let cfg = app.config();
                    cfg.find(&name)
                        .is_some_and(|p| p.credentials.is_some() || p.api_key.is_some())
                };
                let holds_credential_piece = crate::claude::session_token_status(&name).is_some()
                    || crate::claude::has_static_backup(&name);
                if holds_credential_piece && !stores_other_login {
                    return;
                }
                let armed = app
                    .config_draft
                    .as_ref()
                    .is_some_and(|d| d.armed_action == Some(ConfigRow::ClearSessionToken));
                // Arm/confirm rather than a modal: this is a row-level
                // destructive action, and it retargets every future switch.
                if armed {
                    perform_clear_session_token(app, &name);
                    disarm_armed_action(app);
                } else {
                    arm_action(app, ConfigRow::ClearSessionToken);
                }
            }
        }
        _ => {}
    }
}

/// Confirmed `clear long-lived token`: drop the sidecar, then relink when the
/// account is ACTIVE. The relink is not optional there — the live
/// `~/.claude/.credentials.json` is a symlink INTO the profile store aimed at
/// `session-token.json`, so removing the target alone leaves a dangling link
/// and Claude Code reads no credentials at all. An idle account is left alone:
/// force-linking it would repoint the live slot at a profile nobody switched to.
///
/// Same full-exit contract as the CLI's `static-token --clear`, or the two
/// surfaces fight the daemon differently: the `rolling_token` flag goes FIRST
/// (a set flag re-stamps a fresh bearer over the removal on the daemon's next
/// scan), the preserved mint backup goes too (a "cleared" long-lived token
/// with a year-scale mint still in `session-token.static.json` is not
/// cleared), and a mis-filled sidecar is quarantined as evidence before the
/// removal, exactly as every other repair path does.
///
/// The rotation guard is the same load-bearing piece it is on the CLI path —
/// a rotation in flight that still saw the flag set would re-stamp the
/// sidecar AFTER the removal — but a UI thread must never park behind a
/// timeout-less flock, so this takes `try_acquire` and fails LOUDLY into a
/// toast when a rotation holds the profile, instead of silently losing the
/// race.
///
/// The relink has TWO outcomes and the toast names which one happened. The row
/// is refused only when clearing would strip the account's LAST credential — a
/// stored piece (sidecar or preserved mint) with neither a login nor an api key
/// behind it; a flag-only account disarms regardless, touching no credential.
/// An api-key account clears fine onto an ABSENT install source: the forcing
/// relink then removes the live slot and, on macOS, signs the Keychain out
/// rather than installing anything. Both were reported as "relinked its own
/// login". The refusal is re-checked from DISK under the guard: the in-memory
/// gate in `run_config_row` reads a tick-stale snapshot. `reload_fingerprint`
/// stats `credentials.json`, so an out-of-band log-out moves the fingerprint
/// and the next reload corrects the snapshot; the under-guard re-read closes
/// the sub-tick window between that reload and this action.
///
/// Like every in-TUI save, the write is left unadopted, so the next tick's
/// reload re-reads disk and rebuilds `session_tokens` (the Overview `⊘`
/// marker's cache). The local `remove` below only spares that marker one tick
/// of staleness.
fn perform_clear_session_token(app: &mut App, name: &ProfileName) {
    // The active account's clear relinks its live slot over the bare item, so
    // it settles before anything is cleared: a held clear has changed nothing.
    let relinks = crate::profile::load_app_state()
        .map(|state| state.active_profile.as_ref() == Some(name))
        .unwrap_or_else(|_| app.config().is_active(name));
    let _placed = if relinks {
        let Some(placed) = bare_item_gate(app, BareItemWrite::ClearSessionToken(name.clone()))
        else {
            return;
        };
        Some(placed)
    } else {
        None
    };
    let guard = match crate::runtime::RotationGuard::try_acquire(name) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            // Rendered from the shared cause, not spelled here: one condition
            // reaching an operator from a third surface in a third wording is
            // what sends someone looking for three different problems.
            app.toast(
                ToastKind::Danger,
                crate::format::Transient::new(
                    crate::format::Cause::RotationLockHeld(name.to_string()),
                    crate::format::Retry::Stated,
                )
                .text(),
            );
            return;
        }
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("clear failed\n{}", escape_control(&error_toast_detail(&e))),
            );
            return;
        }
    };
    // The relink below puts the store in front of Claude Code: a staged pair
    // lands first.
    if let Err(refused) = crate::oauth::land_stage_for_install(name, &guard) {
        app.toast(
            ToastKind::Danger,
            format!("clear failed\n{}", refused.text()),
        );
        return;
    }
    // Everything the ACTIONS key off is re-read under the guard — the CLI's
    // own rule, and stricter here: the TUI's config is a tick-stale snapshot,
    // and `reload_fingerprint` stats `credentials.json`, so an out-of-band
    // log-out is corrected on the next reload — but only the under-guard read
    // closes the window between that reload and this action. `run_config_row`'s
    // gate fed the row's rendering and nothing else.
    let on_disk = match crate::profile::load_profile(name) {
        Ok(p) => p,
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("clear failed\n{}", escape_control(&error_toast_detail(&e))),
            );
            return;
        }
    };
    let holds_credential_piece = crate::claude::session_token_status(name).is_some()
        || crate::claude::has_static_backup(name);
    if holds_credential_piece && on_disk.credentials.is_none() && on_disk.api_key.is_none() {
        app.toast(
            ToastKind::Danger,
            format!("'{name}' stores no other login anymore · log in first, then clear"),
        );
        return;
    }
    let rolling_armed = on_disk.rolling_token;
    if rolling_armed {
        // The PERSIST is written from the same under-guard disk read, never
        // the in-memory snapshot — another process's config edit may have
        // landed since the last reload, and `save_profile_config` writes the
        // whole config back. The renderer's in-memory flag flips only AFTER that
        // write lands: flipped first, a failed save leaves the config lying
        // (`reload_fingerprint` never corrects it — config mtime did not move)
        // until an unrelated save makes the lie durable, and the end state is
        // a live rolling sidecar nothing re-stamps.
        let mut disarmed = on_disk.clone();
        disarmed.rolling_token = false;
        if let Err(e) = save_profile_config(&disarmed) {
            app.toast(
                ToastKind::Danger,
                format!("clear failed\n{}", escape_control(&error_toast_detail(&e))),
            );
            return;
        }
        let mut cfg = app.config();
        if let Some(p) = cfg.find_mut(name) {
            p.rolling_token = false;
        }
    }
    if let Err(e) = crate::claude::quarantine_misfilled_sidecar(name)
        .and_then(|_| crate::claude::clear_session_token(name))
    {
        app.toast(
            ToastKind::Danger,
            format!("clear failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        return;
    }
    app.session_tokens.remove(name.as_str());
    // Read AFTER the clear, so it names the store the relink actually finds:
    // `install_source_path` falls back to `credentials.json` only once the
    // sidecar is gone. An api-key account has none, so it is signed out rather
    // than relinked, and the toast used to claim the opposite.
    let has_login = crate::claude::has_stored_oauth_login(name);
    // `active` comes off DISK for the same reason every other fact here does.
    // This process is not the only thing that switches: `daemon::tick` and
    // `fallback` both reach `actions::switch_profile`, so the active account
    // moves with no keypress at all, and the snapshot behind the row is a tick
    // old at best. Acting on a stale one relinks the account the operator just
    // switched AWAY from, or skips the relink on the one that IS active and
    // leaves its live slot a dangling symlink into the file just removed. A
    // config that will not parse is a larger failure than this decision, so
    // that arm keeps the old in-memory read rather than inventing a verdict.
    let active = crate::profile::load_config()
        .map(|c| c.is_active(name))
        .unwrap_or_else(|_| app.config().is_active(name));
    if active && let Err(e) = force_link_profile_credentials(name) {
        app.toast(
            ToastKind::Danger,
            format!("relink failed\n{}", escape_control(&error_toast_detail(&e))),
        );
        return;
    }
    // No `refresh_tokens()`: `collect_tokens` reads `Profile::credentials`, which
    // a sidecar clear never touches.
    let tail = match (active, has_login) {
        (true, true) => ", relinked its own login",
        (true, false) if on_disk.api_key.is_some() => {
            ", signed Claude Code out; it runs on its api key"
        }
        // Reachable only on the flag-only widening (a stored piece with no
        // other login is refused above): nothing backs this account now, and
        // claiming an api key it does not hold would be the toast's own lie.
        (true, false) => ", signed Claude Code out; it stores no login at all",
        (false, true) => "; the next switch installs its own login",
        (false, false) if on_disk.api_key.is_some() => {
            "; it stores no login, so it runs on its api key"
        }
        (false, false) => "; it stores no login at all · log in before switching to it",
    };
    let rolled = if rolling_armed {
        " · re-stamping off"
    } else {
        ""
    };
    // The backup goes LAST, after the relink: failing between the sidecar
    // removal and the relink would leave an active profile's live slot a
    // dangling symlink under a toast reading "clear failed", a broken login
    // reported as nothing-happened. From here the live slot is already sound,
    // so a failed backup removal is exactly what its toast says and no more.
    //
    // `tail` and `rolled` are composed ABOVE so this arm can carry them: the
    // sign-out and the disarm both already happened and are durable, and a
    // failure message that reports neither is the same silence the postscripts
    // exist to refuse. The CLI keeps them for free by printing before its own
    // removal.
    let backup_removed = match crate::claude::clear_static_backup(name) {
        Ok(removed) => removed,
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!(
                    "cleared the long-lived token for '{name}'{tail}{rolled}, but the preserved \
                     mint remains\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            );
            return;
        }
    };
    // Unconditional on the removal, like the CLI's `clear_backup_postscript`:
    // the pre-action disclosure lives in the row hint, but a hint is not a
    // report, and a year-scale credential destroyed with nothing saying so is
    // the silence the `rolled` suffix already refuses for the flag.
    let swept = if backup_removed {
        " · the preserved mint is gone"
    } else {
        ""
    };
    app.toast(
        ToastKind::Success,
        format!("cleared the long-lived token for '{name}'{tail}{rolled}{swept}"),
    );
}

/// API-account "re-login": re-enter the base url + api key inline, mirroring the
/// CLI's `login --base-url --api-key`. Seeds both buffers from the stored values
/// and opens the base-url editor; committing it advances to the api-key editor
/// (the `relogin_chain`), and committing that persists both via `commit_endpoint`.
fn start_api_relogin(app: &mut App) {
    let name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    let (base, key) = {
        let cfg = app.config();
        let p = name.as_ref().and_then(|n| cfg.find(n));
        (
            p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
            p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
        )
    };
    if let Some(d) = app.config_draft.as_mut() {
        d.base_url = InputState::new(&base);
        d.api_key = InputState::new(&key);
        d.base_url.end();
        d.relogin_chain = true;
        d.active = Some(ConfigRow::BaseUrl);
        d.field_seed = d.field(ConfigRow::BaseUrl).map(|i| i.value.clone());
    }
    if let Some(pos) = config_row_index(app, ConfigRow::BaseUrl) {
        app.config_action_cursor = pos;
    }
}

/// Kick an OAuth login: mint it on this thread (no network), open the browser
/// on its loopback link, and hand the wait to a worker that takes the first
/// door — the loopback callback, or a code pasted through the login modal.
/// `is_new` → the mint lands in the `+ new` draft when it arrives; else an
/// existing profile is overwritten (divergence-gated in `apply_login`). A
/// second ⏎ while one is in flight re-expands the progress modal instead of
/// starting another login.
fn start_login(app: &mut App, name: String, is_new: bool) {
    if guest_refused(app) {
        return;
    }

    if login_in_flight(app, &name, is_new) {
        return;
    }
    let account = if is_new {
        None
    } else {
        let Some(account) = claim_or_toast(app, &name, "login failed") else {
            return;
        };
        Some(account)
    };
    let pending = match crate::oauth_login::begin_login() {
        Ok(pending) => pending,
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("login failed\n{}", e.user_message()),
            );
            return;
        }
    };
    let links = pending.links().clone();
    let (paste_tx, paste_rx) = std::sync::mpsc::channel();
    app.login_generation += 1;
    let generation = app.login_generation;
    app.login = Some(LoginSession {
        name,
        account,
        generation,
        url: Some(links.browser_url.clone()),
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: Some(PasteDoor {
            links,
            tx: paste_tx,
        }),
        paste_field: None,
    });
    // Best effort: the modal's `r` retries it, and the hosted link is the
    // way in where no browser can open.
    let _ = crate::platform::open_url(&pending.links().browser_url);
    let event_tx = app.login_event_tx.clone();
    let result_tx = app.login_result_tx.clone();
    spawn_worker(move || {
        let res = pending.run(paste_rx, |progress| {
            let _ = event_tx.send((generation, login_event(progress)));
        });
        // A toast, not stderr: the canned line without the HTTP status. The
        // status is in `~/.tollgate/tollgate.log` via the exchange's `logline!`.
        let _ = result_tx.send((
            generation,
            res.map(|o| LoginResult::Oauth(Box::new(o)))
                .map_err(|e| e.user_message()),
        ));
    });
    open_login_modal(app);
}

/// Which of the three flows the `log in` row runs. An account can satisfy more
/// than one of these predicates, so the ORDER is the behaviour: a Model Studio
/// account is api-typed by `login_is_oauth`, and reading the api-key arm first
/// would route every one of them to the api-key re-entry with nothing else
/// failing. Encoded here rather than as a chain of early returns in the row so
/// the order is a thing a test can pin — driving the row itself binds a
/// loopback listener and opens a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginRowFlow {
    /// Capture this account's Alibaba console session.
    Console {
        site: ConsoleSite,
        region: &'static str,
    },
    /// Re-enter an existing api-key account's base url + key inline.
    ApiKey,
    /// Mint an Anthropic login in the browser. Also the `+ new` form's flow,
    /// which has no account to read a type off yet.
    OauthMint,
}

/// Resolve the `log in` row's flow for the draft's account (`None` on the
/// `+ new` form, which can only mint; so does an unknown name, which has no
/// key to re-enter). The console verdict is [`Profile::console_login_target`],
/// shared with the row's hint and label so the copy cannot describe a
/// different flow than the one ⏎ runs.
fn login_row_flow(app: &App, editing: Option<&str>) -> LoginRowFlow {
    let Some(name) = editing else {
        return LoginRowFlow::OauthMint;
    };
    let cfg = app.config();
    let Some(p) = cfg.find(&ProfileName::from(name)) else {
        return LoginRowFlow::OauthMint;
    };
    if let Some((site, region)) = p.console_login_target() {
        return LoginRowFlow::Console { site, region };
    }
    if p.login_is_oauth() {
        LoginRowFlow::OauthMint
    } else {
        LoginRowFlow::ApiKey
    }
}

/// The account an OAuth-mint row acts on: an existing draft re-logs in place;
/// the `+ new` form validates its typed name now and creates on mint. `None`
/// (with a toast) when the typed name is unusable.
fn oauth_login_target(app: &mut App, editing: Option<String>) -> Option<(String, bool)> {
    match editing {
        Some(name) => Some((name, false)),
        None => {
            let typed = app
                .config_draft
                .as_ref()
                .map(|d| d.name.trimmed().to_string())
                .unwrap_or_default();
            let validation = validate_profile_name(&typed, Harness::Claude, None);
            match validation {
                Ok(()) => Some((typed, true)),
                Err(e) => {
                    app.toast(ToastKind::Danger, escape_control(&error_toast_detail(&e)));
                    None
                }
            }
        }
    }
}

/// The OAuth-mint head of the login row: nothing starts while a login is in
/// flight, and a captured stash is confirmed before it is dropped.
fn begin_oauth_login(app: &mut App, name: String, is_new: bool) {
    if login_in_flight(app, &name, is_new) {
        return;
    }
    // A stash (the `✓ logged in` / `✓ captured current login` done-states)
    // makes ⏎ a stash-replacing re-login; gate it so it can't drop the capture
    // silently. Only the `+ new` draft ever holds a stash.
    let has_stash = app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.captured_login.is_some());
    if has_stash {
        app.modals.push(Modal::Confirm(ConfirmState {
            message: "replace the captured login?".to_string(),
            detail: Some("the login you already captured will be dropped".to_string()),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::RestartLogin(name, is_new),
        }));
    } else {
        start_login(app, name, is_new);
    }
}

/// A login already running owns the progress modal: a ⏎ aimed at the same
/// target re-expands it, one aimed elsewhere says so. Either way nothing new
/// starts — a second worker would race the first for `app.login`.
fn login_in_flight(app: &mut App, name: &str, is_new: bool) -> bool {
    let Some(session) = app.login.as_ref() else {
        return false;
    };
    if session.name != name || session.account.is_none() != is_new {
        toast_login_in_progress(app, session.name.clone());
    }
    open_login_modal(app);
    true
}

/// The worker→UI event for one `oauth_login` milestone.
fn login_event(progress: crate::oauth_login::LoginProgress) -> LoginEvent {
    use crate::oauth_login::LoginProgress;
    match progress {
        LoginProgress::ExchangingCode(door) => LoginEvent::Stage(LoginStage::ExchangingCode(door)),
        LoginProgress::Verifying => LoginEvent::Stage(LoginStage::Verifying),
    }
}

/// Kick the Alibaba console login on a worker — the same browser round-trip and
/// the same progress modal as [`start_login`], capturing that account's usage
/// session instead of an Anthropic credential.
///
/// Never a `+ new` draft's: the console site and region are read off the profile's
/// endpoint, so an account without one has no console to open, and the create
/// form has no endpoint yet. That is why the CLI's own route is
/// create-with-preset, then log in.
fn start_console_login(app: &mut App, name: String, site: ConsoleSite, region: &'static str) {
    if let Some(session) = app.login.as_ref() {
        // Same shape as `start_login`'s guard, the draft check included: a `+ new`
        // draft's login is a different session even when the typed name matches,
        // so re-showing its modal without a word would read as this row's own
        // login having started.
        if session.name != name || session.account.is_none() {
            toast_login_in_progress(app, session.name.clone());
        }
        open_login_modal(app);
        return;
    }
    let Some(account) = claim_or_toast(app, &name, "console login failed") else {
        return;
    };
    app.login_generation += 1;
    let generation = app.login_generation;
    app.login = Some(LoginSession {
        name,
        account: Some(account),
        generation,
        url: None,
        stage: LoginStage::WaitingBrowser,
        method: LoginMethod::Browser,
        paste: None,
        paste_field: None,
    });
    let event_tx = app.login_event_tx.clone();
    let result_tx = app.login_result_tx.clone();
    spawn_worker(move || {
        let res = crate::alibaba_login::login_with(site, region, |url| {
            let _ = event_tx.send((generation, LoginEvent::Url(url.to_string())));
        });
        let _ = result_tx.send((
            generation,
            res.map(|o| LoginResult::Console(Box::new(o)))
                // The toast leads with the root cause, then the rest of the
                // chain outermost-first: the outermost context here is a bare
                // "loopback accept failed" whose io cause is the only part that
                // says what to do about it. No constructor in `alibaba_login`
                // embeds the token or an upstream body.
                .map_err(|e| error_toast_detail(&e)),
        ));
    });
    open_login_modal(app);
}

/// Show the login progress modal (no-op when already open).
fn open_login_modal(app: &mut App) {
    if !app.modals.iter().any(|m| matches!(m, Modal::Login)) {
        app.open_modal(Modal::Login);
    }
}

/// Drop the login progress modal (login finished, failed, or was canceled).
fn close_login_modal(app: &mut App) {
    app.retain_modals(|m| !matches!(m, Modal::Login));
}

/// Arm an account-action row (`Delete`, `ClearSessionToken`, or `Disabled` in
/// the disable direction) on its first ⏎.
fn arm_action(app: &mut App, row: ConfigRow) {
    if let Some(d) = app.config_draft.as_mut() {
        d.armed_action = Some(row);
    }
}

/// Keystrokes while a text row is active: ⏎ commits, ⎋ reverts.
fn handle_config_edit_key(app: &mut App, key: KeyEvent) {
    let Some(active) = app.config_draft.as_ref().and_then(|d| d.active) else {
        return;
    };
    match key.code {
        KeyCode::Esc => cancel_config_edit(app, active),
        KeyCode::Enter => {
            let _ = commit_config_field(app, active);
        }
        _ => {
            if let Some(d) = app.config_draft.as_mut()
                && let Some(input) = d.field_mut(active)
            {
                apply_input_edit(input, key);
            }
        }
    }
    // A keystroke, commit or revert can add or drop rows above the field (a
    // typed base url flips the account's rows), and the cursor is an index.
    let edited = app
        .config_draft
        .as_ref()
        .and_then(|d| d.active)
        .unwrap_or(active);
    if let Some(pos) = config_row_index(app, edited) {
        app.config_action_cursor = pos;
    }
}

/// ⎋ inside a field: existing accounts revert from the live profile;
/// new drafts keep the typed value. Either way, editing ends.
///
/// Escape on a just-added env entry removes the row instead: the add only
/// persisted an empty placeholder, so backing out restores the pre-add state.
fn cancel_config_edit(app: &mut App, field: ConfigRow) {
    let editing_name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    if let Some(name) = editing_name.clone() {
        let just_added = app
            .config_draft
            .as_ref()
            .and_then(|d| d.env_just_added.clone());
        if let (Some(added), ConfigRow::EnvEntry(i)) = (just_added, field) {
            let key = {
                let cfg = app.config();
                cfg.find(&name).and_then(|p| p.env.keys().nth(i).cloned())
            };
            // Keyed on the just-added state, never on an empty buffer: an
            // existing entry the operator is merely editing empty must keep
            // its saved value.
            if key.as_deref() == Some(added.as_str()) {
                cancel_just_added_env(app, &name, &added);
                return;
            }
        }
    }
    if let Some(name) = editing_name {
        let value = {
            let cfg = app.config();
            row_committed_value(cfg.find(&name), &name, field)
        };
        if let Some(d) = app.config_draft.as_mut()
            && let Some(input) = d.field_mut(field)
        {
            *input = InputState::new(&value);
        }
    }
    if let Some(d) = app.config_draft.as_mut() {
        // ⎋ mid re-login abandons the whole base-url → api-key chain, not just the
        // current field.
        d.relogin_chain = false;
        d.active = None;
    }
}

/// Escape on a just-added env entry: drop the placeholder and persist through
/// [`edit_profile_env`] — the same writer the value-commit unset uses — so the
/// active profile's live settings lose the key too. On a failed persist the
/// editor and the flag stay, so a retry still removes the row.
fn cancel_just_added_env(app: &mut App, name: &ProfileName, key: &str) {
    let new_env = {
        let cfg = app.config();
        cfg.find(name).map(|p| {
            let mut env = p.env.clone();
            env.remove(key);
            env
        })
    };
    let Some(new_env) = new_env else {
        // The profile vanished from the in-memory config (an out-of-band
        // delete/rename); disarm the cancel and end the edit instead of
        // leaving the editor stuck open with a zombie flag.
        if let Some(d) = app.config_draft.as_mut() {
            d.env_just_added = None;
            d.active = None;
        }
        return;
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_env(&mut cfg, name, new_env)
    };
    match result {
        Ok(()) => {
            if let Some(d) = app.config_draft.as_mut() {
                d.env_just_added = None;
                d.env_value = InputState::new("");
                d.active = None;
            }
        }
        Err(e) => {
            // `edit_profile_env` mutates the in-memory profile BEFORE its
            // disk writes, so a failed persist leaves memory without the key
            // while the placeholder stays on disk — put it back so memory
            // matches disk and a retry's index lookup finds the same row it
            // cancelled.
            {
                let mut cfg = app.config();
                if let Some(p) = cfg.find_mut(name) {
                    p.env.insert(key.to_string(), String::new());
                }
            }
            app.toast(
                ToastKind::Danger,
                format!(
                    "env update failed\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            )
        }
    }
}

/// The persisted value behind a buffered row, used to revert on ⎋ and to reseed
/// the buffer after a commit. Toggle/action rows have no buffer → empty string.
fn row_committed_value(profile: Option<&Profile>, name: &ProfileName, row: ConfigRow) -> String {
    match row {
        ConfigRow::Name => name.to_string(),
        ConfigRow::BaseUrl => profile.and_then(|p| p.base_url.clone()).unwrap_or_default(),
        ConfigRow::ApiKey => profile.and_then(|p| p.api_key.clone()).unwrap_or_default(),
        ConfigRow::Model => profile
            .and_then(|p| p.models.default.clone())
            .unwrap_or_default(),
        ConfigRow::OpusModel => profile
            .and_then(|p| p.models.opus.clone())
            .unwrap_or_default(),
        ConfigRow::SonnetModel => profile
            .and_then(|p| p.models.sonnet.clone())
            .unwrap_or_default(),
        ConfigRow::HaikuModel => profile
            .and_then(|p| p.models.haiku.clone())
            .unwrap_or_default(),
        ConfigRow::FableModel => profile
            .and_then(|p| p.models.fable.clone())
            .unwrap_or_default(),
        ConfigRow::SubagentModel => profile
            .and_then(|p| p.models.subagent.clone())
            .unwrap_or_default(),
        ConfigRow::BellThreshold => profile
            .and_then(|p| p.bell_threshold)
            .map(format_weekly_pct)
            .unwrap_or_default(),
        ConfigRow::UsageProbeInterval => profile
            .map(|p| (p.usage_probe_interval_ms / 1000).to_string())
            .unwrap_or_default(),
        // The saved value of the i-th sorted env entry; reverts a value edit on ⎋.
        ConfigRow::EnvEntry(i) => profile
            .and_then(|p| p.env.values().nth(i).cloned())
            .unwrap_or_default(),
        ConfigRow::AutoStart
        | ConfigRow::UsageProbe
        | ConfigRow::Type
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::CaptureLogin
        | ConfigRow::DeleteCreds
        | ConfigRow::ClearSessionToken
        | ConfigRow::Disabled
        | ConfigRow::Delete
        | ConfigRow::Create => String::new(),
    }
}

/// ⏎ inside a field: new draft buffers; existing accounts persist per field.
fn commit_config_field(app: &mut App, field: ConfigRow) -> CommitResult {
    let is_new = app
        .config_draft
        .as_ref()
        .map(|d| d.editing_name.is_none())
        .unwrap_or(true);
    if is_new {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
            // A name edit retires the refusals the old name carried.
            if field == ConfigRow::Name {
                d.name_refusal = None;
                d.create_refusal = None;
            }
        }
        return CommitResult::Landed;
    }
    match field {
        ConfigRow::Name => CommitResult::from(commit_rename(app)),
        ConfigRow::BaseUrl | ConfigRow::ApiKey => CommitResult::from(commit_endpoint(app)),
        ConfigRow::Model
        | ConfigRow::OpusModel
        | ConfigRow::SonnetModel
        | ConfigRow::HaikuModel
        | ConfigRow::FableModel
        | ConfigRow::SubagentModel => CommitResult::from(commit_model_field(app, field)),
        ConfigRow::EnvEntry(i) => CommitResult::from(commit_env_value(app, i)),
        ConfigRow::EnvAdd => commit_env_new_key(app),
        ConfigRow::BellThreshold => CommitResult::from(commit_bell_threshold(app)),
        ConfigRow::UsageProbeInterval => CommitResult::from(commit_usage_probe_interval(app)),
        _ => {
            if let Some(d) = app.config_draft.as_mut() {
                d.active = None;
            }
            CommitResult::Landed
        }
    }
}

/// `true` = a synchronous commit landed; `false` = refused (the editor stays
/// open with its own refusal treatment).
impl From<bool> for CommitResult {
    fn from(landed: bool) -> Self {
        if landed { Self::Landed } else { Self::Refused }
    }
}

/// ⏎ on a model field: fold the trimmed buffer into the profile's
/// [`ModelSettings`], persist, then reseed the buffer from the saved value.
fn commit_model_field(app: &mut App, field: ConfigRow) -> bool {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return false;
    };
    let raw = app
        .config_draft
        .as_ref()
        .and_then(|d| d.field(field))
        .map(|i| i.trimmed().to_string())
        .unwrap_or_default();
    let mut models = {
        let cfg = app.config();
        cfg.find(&name)
            .map(|p| p.models.clone())
            .unwrap_or_default()
    };
    apply_model_field(&mut models, field, &raw);
    let result = {
        let mut cfg = app.config();
        edit_profile_model(&mut cfg, &name, models)
    };
    match result {
        Ok(()) => {
            let value = {
                let cfg = app.config();
                row_committed_value(cfg.find(&name), &name, field)
            };
            if let Some(d) = app.config_draft.as_mut() {
                if let Some(input) = d.field_mut(field) {
                    *input = InputState::new(&value);
                }
                d.active = None;
            }
            true
        }
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!(
                    "model update failed\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            );
            false
        }
    }
}

/// ⏎ on the `alert at` field: a value in `0..=100` sets the bell, an empty one
/// turns it off; anything else keeps the editor open, as the Fallback card's
/// threshold editor does.
fn commit_bell_threshold(app: &mut App) -> bool {
    let raw = app
        .config_draft
        .as_ref()
        .map(|d| d.bell_threshold.trimmed().to_string())
        .unwrap_or_default();
    let Some(value) = parse_bell_threshold(&raw) else {
        return false;
    };
    write_bell_threshold(app, value)
}

/// A typed `alert at` value: a number in `0..=100`, or EMPTY, which turns the
/// bell off. `Some(None)` = off, `Some(Some(v))` = set, `None` = refused.
pub(crate) fn parse_bell_threshold(raw: &str) -> Option<Option<f64>> {
    if raw.is_empty() {
        return Some(None);
    }
    crate::fallback::parse_threshold(raw).map(Some)
}

/// `+`/`-` on the `alert at` row: step by `delta` inside the typed range,
/// starting from 95% when the alert is off.
fn step_bell_threshold(app: &mut App, delta: f64) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    let current = app.config().find(&name).and_then(|p| p.bell_threshold);
    let next = match current {
        // Rounded to hundredths, so a step off `5.01` lands on `0.01`, not on
        // a float tail.
        Some(v) => (((v + delta) * 100.0).round() / 100.0).clamp(
            crate::fallback::MIN_THRESHOLD,
            crate::fallback::MAX_THRESHOLD,
        ),
        None => DEFAULT_BELL_THRESHOLD,
    };
    write_bell_threshold(app, Some(next));
}

/// Where `+`/`-` start on an account with no alert: the commented-out default
/// in a fresh `config.toml`.
const DEFAULT_BELL_THRESHOLD: f64 = 95.0;

/// Persist the drafted account's alert threshold, rolling memory back on a
/// failed save, then reseed the row's buffer from what was saved.
fn write_bell_threshold(app: &mut App, value: Option<f64>) -> bool {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return false;
    };
    // `-0` saves as `0`, the value a reload reads back.
    let value = value.map(|v| v + 0.0);
    let result = {
        let mut cfg = app.config();
        match cfg.find_mut(&name) {
            None => None,
            Some(profile) => {
                let before = profile.bell_threshold;
                profile.bell_threshold = value;
                let saved = save_profile_config(profile);
                if saved.is_err()
                    && let Some(p) = cfg.find_mut(&name)
                {
                    p.bell_threshold = before;
                }
                Some(saved)
            }
        }
    };
    match result {
        // The account left under the open editor (an outside delete or
        // rename): end the edit rather than leave a dead ⏎.
        None => {
            if let Some(d) = app.config_draft.as_mut() {
                d.active = None;
            }
            true
        }
        Some(Ok(())) => {
            let shown = {
                let cfg = app.config();
                row_committed_value(cfg.find(&name), &name, ConfigRow::BellThreshold)
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.bell_threshold = InputState::new(&shown);
                d.active = None;
            }
            true
        }
        Some(Err(e)) => {
            app.toast(
                ToastKind::Danger,
                format!(
                    "alert update failed\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            );
            false
        }
    }
}

/// Fold a trimmed buffer into the matching [`ModelSettings`] field. Empty input
/// clears it (scalar → `None`).
fn apply_model_field(models: &mut ModelSettings, field: ConfigRow, raw: &str) {
    let scalar = (!raw.is_empty()).then(|| raw.to_string());
    match field {
        ConfigRow::Model => models.default = scalar,
        ConfigRow::OpusModel => models.opus = scalar,
        ConfigRow::SonnetModel => models.sonnet = scalar,
        ConfigRow::HaikuModel => models.haiku = scalar,
        ConfigRow::FableModel => models.fable = scalar,
        ConfigRow::SubagentModel => models.subagent = scalar,
        // The other rows carry no `ModelSettings` field and never route here
        // (`commit_model_field` is the only caller). Named, so a new
        // `ConfigRow` variant fails the build instead of a silent no-op.
        ConfigRow::Name
        | ConfigRow::AutoStart
        | ConfigRow::UsageProbe
        | ConfigRow::UsageProbeInterval
        | ConfigRow::Type
        | ConfigRow::BellThreshold
        | ConfigRow::BaseUrl
        | ConfigRow::ApiKey
        | ConfigRow::ModelOverrideAdd
        | ConfigRow::EnvEntry(_)
        | ConfigRow::EnvAdd
        | ConfigRow::Login
        | ConfigRow::CaptureLogin
        | ConfigRow::DeleteCreds
        | ConfigRow::ClearSessionToken
        | ConfigRow::Disabled
        | ConfigRow::Delete
        | ConfigRow::Create => {}
    }
}

/// Space on the `model` row: advance the alias cycle and persist. A custom value
/// (set via ⏎) is outside the cycle, so the first space resets it to `default`.
/// `editing_name` is `None` on the still-unsaved `+ new` draft — there's no
/// profile yet to persist into, so it just advances the buffer in place; the
/// value is folded in on `commit_new_account`.
fn cycle_model(app: &mut App) {
    let editing_name = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from);
    let Some(name) = editing_name else {
        if let Some(d) = app.config_draft.as_mut() {
            let next = next_model_preset(d.model.trimmed_some().as_deref()).unwrap_or_default();
            d.model = InputState::new(&next);
        }
        return;
    };
    let mut models = {
        let cfg = app.config();
        cfg.find(&name)
            .map(|p| p.models.clone())
            .unwrap_or_default()
    };
    models.default = next_model_preset(models.default.as_deref());
    let result = {
        let mut cfg = app.config();
        edit_profile_model(&mut cfg, &name, models)
    };
    match result {
        Ok(()) => {
            let value = {
                let cfg = app.config();
                cfg.find(&name)
                    .and_then(|p| p.models.default.clone())
                    .unwrap_or_default()
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.model = InputState::new(&value);
            }
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "model update failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

/// Advance the `model` alias cycle: default → fable → opus → sonnet → haiku →
/// opusplan → default. A value outside [`MODEL_PRESETS`] (a custom id)
/// collapses to default.
fn next_model_preset(current: Option<&str>) -> Option<String> {
    match current {
        None => Some(MODEL_PRESETS[0].to_string()),
        Some(cur) => match MODEL_PRESETS.iter().position(|p| *p == cur) {
            Some(i) if i + 1 < MODEL_PRESETS.len() => Some(MODEL_PRESETS[i + 1].to_string()),
            _ => None,
        },
    }
}

fn staged_login_held(app: &App) -> bool {
    app.login.as_ref().is_some_and(|s| s.account.is_none())
        || app.codex_login.as_ref().is_some_and(|s| s.is_new)
}

/// staged login is gated on a confirm first — the change drops it — and a
/// cancel leaves both the type and the stage as they were.
fn cycle_account_type(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let next = d.account_type.next();
    if staged_login_held(app) || d.captured_login.is_some() {
        app.open_modal(Modal::Confirm(ConfirmState {
            message: "change the account type?".to_string(),
            detail: Some("the login you already captured will be dropped".to_string()),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::CycleAccountType(next),
        }));
        return;
    }
    set_account_type(app, next);
}

/// Land a type change: swap the type, drop the staged login and any refusal the
/// old type's rows held, and keep the cursor on the type row.
fn set_account_type(app: &mut App, next: NewAccountType) {
    if let Some(d) = app.config_draft.as_mut() {
        d.account_type = next;
        d.captured_login = None;
        d.name_refusal = None;
        d.create_refusal = None;
    }
    app.config_action_cursor = 0;
}

fn cancel_open_login(app: &mut App, cancel_claude: bool) {
    if cancel_claude {
        app.login = None;
        close_login_modal(app);
    }
}

/// Cancel the in-flight login a confirmed type change must drop, without the
/// "login canceled" toast — the confirm already said the stage would be
/// otherwise close whatever draft is open later. A claude or codex mint is
/// NOT cancelled — its late result is dropped by the harness check (F3), which
/// toasts the type change. A committed add runs to its result.
fn cancel_staged_new_login(app: &mut App) {
    cancel_open_login(app, false);
}

/// The codex `+ login` head: validate the typed name against BOTH rosters
/// before any browser opens (a held name refuses on the name row's `└`), then
/// run the codex browser flow on a worker and STAGE the minted chain in the
/// draft — `create account` lands it, a closed form drops it unused. The modal
/// is commit 1's codex login modal (open-again `r`, copy-link `c`, no paste
/// door).
fn start_codex_new_login(app: &mut App) {
    if guest_refused(app) {
        return;
    }

    if let Some(session) = app.codex_login.as_ref() {
        // Same shape as the re-login guard: a second ⏎ reopens the modal, one
        // for another name names the running login.
        let typed = app
            .config_draft
            .as_ref()
            .map(|d| d.name.trimmed().to_string())
            .unwrap_or_default();
        if session.name != typed {
            toast_login_in_progress(app, session.name.clone());
        }
        open_codex_login_modal(app);
        return;
    }
    let typed = app
        .config_draft
        .as_ref()
        .map(|d| d.name.trimmed().to_string())
        .unwrap_or_default();
    if let Err(e) = validate_profile_name(&typed, Harness::Codex, None) {
        if let Some(d) = app.config_draft.as_mut() {
            d.name_refusal = Some(format!("{e}"));
        }
        focus_name_row(app);
        return;
    }
    app.codex_login_generation += 1;
    let generation = app.codex_login_generation;
    let cancel = crate::codex_login::LoginCancel::new();
    app.codex_login = Some(CodexLoginSession {
        name: typed.clone(),
        generation,
        url: None,
        cancel: cancel.clone(),
        is_new: true,
    });
    #[cfg(test)]
    let worker_fn = app.codex_login_worker;
    let url_tx = app.codex_login_url_tx.clone();
    let result_tx = app.codex_new_login_result_tx.clone();
    spawn_worker(move || {
        let announce = move |url: &str| {
            let _ = url_tx.send((generation, url.to_string()));
        };
        #[cfg(test)]
        let outcome = worker_fn(&announce, &cancel);
        #[cfg(not(test))]
        let outcome =
            crate::codex_login::login_with_cancel(&announce, &cancel).map_err(|e| format!("{e:#}"));
        // A cancel seen here drops the minted chain unwritten.
        let res = if cancel.is_canceled() {
            Err("login canceled".to_string())
        } else {
            outcome
        };
        let _ = result_tx.send((generation, res));
    });
    open_codex_login_modal(app);
}

/// Drain a finished `+ new` codex `+ login`: stage the minted chain in the
/// draft, or toast the failure.
fn drain_codex_new_login(app: &mut App) {
    if crate::identity::upstream_active() {
        cancel_codex_login(app);
        return;
    }
    while let Ok((generation, result)) = app.codex_new_login_result_rx.try_recv() {
        if app.codex_login.as_ref().map(|s| s.generation) != Some(generation) {
            continue; // superseded or cancelled
        }
        app.codex_login = None;
        close_codex_login_modal(app);
        match result {
            Ok(outcome) => {
                match stash_new_form_login(app, DraftLogin::CodexMint(Box::new(outcome))) {
                    StashOutcome::Staged => {
                        app.toast(ToastKind::Success, "logged in\ncreate account saves it")
                    }
                    StashOutcome::FormClosed => app.toast(
                        ToastKind::Warning,
                        "login finished but the new-account form is no longer open\nlog in again",
                    ),
                    StashOutcome::WrongType => app.toast(
                        ToastKind::Info,
                        "login finished but the form type changed\nnothing was saved",
                    ),
                }
            }
            Err(e) => app.toast(ToastKind::Danger, format!("login failed\n{e}")),
        }
    }
}

/// `+ capture current login` on the codex form: stage a marker that moves the
/// operator's own codex login at `create account`, exactly as
/// `tollgate login <name> --codex` does.
fn stage_codex_capture(app: &mut App) {
    if guest_refused(app) {
        return;
    }

    let typed = app
        .config_draft
        .as_ref()
        .map(|d| d.name.trimmed().to_string())
        .unwrap_or_default();
    if let Err(e) = validate_profile_name(&typed, Harness::Codex, None) {
        if let Some(d) = app.config_draft.as_mut() {
            d.name_refusal = Some(format!("{e}"));
        }
        focus_name_row(app);
        return;
    }
    // The row only renders while the operator's login is capturable; the file
    // may have moved since the flag was computed, so re-check and drop the row.
    if !crate::actions::codex_operator_login_capturable() {
        app.toast(
            ToastKind::Warning,
            "no codex login to capture\nrun `codex login` first",
        );
        return;
    }
    if matches!(
        stash_new_form_login(app, DraftLogin::CodexCapture),
        StashOutcome::Staged
    ) {
        app.toast(
            ToastKind::Success,
            "current codex login captured\ncreate account saves it",
        );
    }
}

/// The warning a codex capture that could not adopt the operator's slot owes
/// the user: the chain is now a separate copy of a single-use rotating chain.
fn codex_separate_copy_warning(outcome: &crate::actions::CodexCaptureOutcome) -> Option<String> {
    match outcome {
        crate::actions::CodexCaptureOutcome::Captured {
            canonical,
            adopted: false,
            ..
        } => Some(format!(
            "captured a separate copy of a single-use rotating chain\nrun codex through {} from here on, or run codex login again",
            prose::cmd(&format!("tollgate start {canonical}"))
        )),
        _ => None,
    }
}

/// `create account` on the codex form: land the staged mint through
/// [`crate::actions::land_codex_login`] or the staged capture through
/// [`crate::actions::codex_login_capture_at`], both create-only — a name either
/// roster holds refuses at create (re-checked under the state lock), never a
/// re-auth. Nothing staged refuses on the create row's `└`.
fn commit_codex_account(app: &mut App) {
    if guest_refused(app) {
        return;
    }

    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let name = d.name.trimmed().to_string();
    let stash = d.captured_login.clone();
    let result = match stash {
        Some(DraftLogin::CodexMint(outcome)) => {
            crate::actions::land_codex_login(&name, &outcome, None).map(|_| None)
        }
        Some(DraftLogin::CodexCapture) => crate::actions::codex_login_capture_at(
            &name,
            &chrono::Utc::now().to_rfc3339(),
            crate::actions::CodexLandMode::CreateOnly,
        )
        .map(|outcome| codex_separate_copy_warning(&outcome)),
        _ => {
            if let Some(d) = app.config_draft.as_mut() {
                d.create_refusal = Some("log in or capture a login first".to_string());
            }
            return;
        }
    };
    match result {
        Ok(warning) => {
            finish_created_codex(app, &name);
            app.toast(ToastKind::Success, format!("created '{name}'"));
            if let Some(warning) = warning {
                app.toast(ToastKind::Warning, warning);
            }
        }
        Err(e) => app.toast(ToastKind::Danger, format!("create failed\n{e:#}")),
    }
}

/// Land a created codex account: re-read the codex snapshot, and put Setup and
/// the Overview on the new row.
fn finish_created_codex(app: &mut App, name: &str) {
    app.codex_rows = codex_rows();
    let new_idx = app
        .codex_rows
        .iter()
        .position(|row| row.name == name)
        .unwrap_or(0);
    land_created_item(
        app,
        Some(SetupItem::Codex(ProfileName::from(name))),
        MainItemKind::Codex(new_idx),
    );
}

/// ⏎ on an `EnvEntry`: seed the shared value buffer from the entry's saved value
/// and open the inline value editor.
fn enter_env_value_edit(app: &mut App, i: usize) {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return;
    };
    let value = {
        let cfg = app.config();
        cfg.find(&name)
            .and_then(|p| p.env.values().nth(i).cloned())
            .unwrap_or_default()
    };
    if let Some(d) = app.config_draft.as_mut() {
        d.env_value = InputState::new(&value);
        d.active = Some(ConfigRow::EnvEntry(i));
        d.field_seed = Some(d.env_value.value.clone());
    }
}

/// ⏎ on `+ add env`: open an empty key editor (commit runs the collision check).
fn enter_env_add_edit(app: &mut App) {
    if let Some(d) = app.config_draft.as_mut() {
        d.env_new_key = InputState::new("");
        d.active = Some(ConfigRow::EnvAdd);
        d.field_seed = Some(d.env_new_key.value.clone());
    }
}

/// ⏎ in an env value editor: fold the trimmed buffer into the entry,
/// persist via [`edit_profile_env`], then reseed from the saved value.
/// An emptied value is an unset — the entry is dropped, so neither the
/// profile's config.toml nor the live settings.json keeps `key = ""`.
fn commit_env_value(app: &mut App, i: usize) -> bool {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return false;
    };
    let value = app
        .config_draft
        .as_ref()
        .map(|d| d.env_value.trimmed().to_string())
        .unwrap_or_default();
    let key = {
        let cfg = app.config();
        cfg.find(&name).and_then(|p| p.env.keys().nth(i).cloned())
    };
    let new_env = {
        let cfg = app.config();
        cfg.find(&name).map(|p| {
            let mut env = p.env.clone();
            if let Some(key) = key.clone() {
                if value.is_empty() {
                    // Unset, not blank: `edit_profile_env` strips the dropped
                    // key from the live settings on its own (prev-key removal).
                    env.remove(&key);
                } else {
                    env.insert(key, value.clone());
                }
            }
            env
        })
    };
    let Some(new_env) = new_env else {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return true;
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_env(&mut cfg, &name, new_env)
    };
    match result {
        Ok(()) => {
            let saved = if value.is_empty() {
                // The entry is gone; `values().nth(i)` would now read the
                // SIBLING that slid into its slot.
                String::new()
            } else {
                let cfg = app.config();
                cfg.find(&name)
                    .and_then(|p| p.env.values().nth(i).cloned())
                    .unwrap_or_default()
            };
            if let Some(d) = app.config_draft.as_mut() {
                if d.env_just_added.as_deref() == key.as_deref() {
                    // A value commit resolves the just-added state: the entry
                    // is either kept with its value or dropped by the unset
                    // gate above — either way the cancel arm disarms.
                    d.env_just_added = None;
                }
                d.env_value = InputState::new(&saved);
                d.active = None;
            }
            true
        }
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!(
                    "env update failed\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            );
            false
        }
    }
}

/// ⏎ in the add-env key editor: validate, run the 3-source collision check, and
/// either prompt (overwrite/keep/cancel) or add the key and drop into value-edit.
fn commit_env_new_key(app: &mut App) -> CommitResult {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return CommitResult::Refused;
    };
    let key = app
        .config_draft
        .as_ref()
        .map(|d| d.env_new_key.trimmed().to_string())
        .unwrap_or_default();
    if key.is_empty() {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return CommitResult::Landed;
    }
    // A settings.json env key is a shell-style name; spaces or `=` are never valid.
    if key.contains(char::is_whitespace) || key.contains('=') {
        app.toast(
            ToastKind::Danger,
            "env key can't contain spaces or '='".to_string(),
        );
        return CommitResult::Refused; // keep the editor open so the user can fix it
    }
    let base_env_keys = claude_settings_env_keys().unwrap_or_default();
    let collision = {
        let cfg = app.config();
        cfg.find(&name)
            .and_then(|p| classify_env_key(p, &base_env_keys, &key))
    };
    if let Some(d) = app.config_draft.as_mut() {
        d.active = None;
    }
    match collision {
        Some(c) => {
            app.open_modal(Modal::EnvCollision(env_collision_form(
                name.to_string(),
                key,
                c,
            )));
            CommitResult::Dialog
        }
        None => {
            if env_add_commit(app, &name, &key) {
                CommitResult::Landed
            } else {
                CommitResult::Refused
            }
        }
    }
}

/// Add (when new) the custom key with an empty value, then focus its row and open
/// the value editor. An existing key (overwrite chosen on the prompt) is edited in
/// place — never re-blanked. Returns whether the commit landed.
fn env_add_commit(app: &mut App, name: &ProfileName, key: &str) -> bool {
    let exists = {
        let cfg = app.config();
        cfg.find(name)
            .map(|p| p.env.contains_key(key))
            .unwrap_or(false)
    };
    if !exists {
        let new_env = {
            let cfg = app.config();
            cfg.find(name).map(|p| {
                let mut env = p.env.clone();
                env.insert(key.to_string(), String::new());
                env
            })
        };
        let Some(new_env) = new_env else {
            return false;
        };
        if let Err(e) = {
            let mut cfg = app.config();
            edit_profile_env(&mut cfg, name, new_env)
        } {
            app.toast(
                ToastKind::Danger,
                format!(
                    "env update failed\n{}",
                    escape_control(&error_toast_detail(&e))
                ),
            );
            return false;
        }
        // The add persisted an empty placeholder; arm the cancel so an escape
        // in the value editor removes the row instead of keeping `key = ""`.
        if let Some(d) = app.config_draft.as_mut() {
            d.env_just_added = Some(key.to_string());
        }
    }
    let idx = {
        let cfg = app.config();
        cfg.find(name)
            .and_then(|p| p.env.keys().position(|k| k == key))
    };
    let Some(idx) = idx else {
        return false;
    };
    if let Some(row_pos) = config_row_index(app, ConfigRow::EnvEntry(idx)) {
        app.config_action_cursor = row_pos;
    }
    enter_env_value_edit(app, idx);
    true
}

/// Position of `row` in the current detail-row list, if present.
fn config_row_index(app: &App, row: ConfigRow) -> Option<usize> {
    config_rows(app).iter().position(|r| *r == row)
}

/// Build the collision prompt for a candidate key against its colliding source.
/// `reason` is a noun phrase ("the base url field", …) so both the prompt message
/// (`used by {reason}`) and the overwrite detail (`overrides {reason}`) read right.
fn env_collision_form(
    profile: String,
    key: String,
    collision: EnvKeyCollision,
) -> EnvCollisionForm {
    let (reason, existing_idx) = match collision {
        EnvKeyCollision::Managed(label) => (label.to_string(), None),
        EnvKeyCollision::ProfileField(idx) => (
            "another custom field on this account".to_string(),
            Some(idx),
        ),
        EnvKeyCollision::BaseSettings => ("your ~/.claude/settings.json".to_string(), None),
    };
    EnvCollisionForm {
        profile,
        key,
        reason,
        existing_idx,
        // Default to the safe choice (cancel), per the modal cancel-default rule.
        cursor: EnvCollisionForm::options().len() - 1,
        scroll: std::cell::Cell::new(0),
        reviewing_overwrite: false,
        confirm_overwrite: false,
        max_scroll: std::cell::Cell::new(0),
        max_scroll_stamp: std::cell::Cell::new(ScrollStamp::default()),
        viewport: std::cell::Cell::new(0),
        overwrite_needs_review: std::cell::Cell::new(false),
    }
}

fn handle_env_collision_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::EnvCollision(state)) = app.modals.last_mut() else {
        return;
    };
    if state.reviewing_overwrite {
        match key.code {
            KeyCode::Up => {
                state.scroll.set(
                    state
                        .scroll
                        .get()
                        .min(state.max_scroll.get() as usize)
                        .saturating_sub(1),
                );
            }
            KeyCode::Down => {
                state.scroll.set(
                    state
                        .scroll
                        .get()
                        .saturating_add(1)
                        .min(state.max_scroll.get() as usize),
                );
            }
            KeyCode::PageUp => {
                state.scroll.set(page_up_scroll(
                    state.scroll.get(),
                    state.max_scroll.get() as usize,
                    state.viewport.get(),
                ));
            }
            KeyCode::PageDown => {
                state.scroll.set(page_down_scroll(
                    state.scroll.get(),
                    state.max_scroll.get() as usize,
                    state.viewport.get(),
                ));
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                state.confirm_overwrite = !state.confirm_overwrite;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                state.reviewing_overwrite = false;
                state.confirm_overwrite = false;
                state.scroll.set(0);
            }
            KeyCode::Enter => {
                if state.confirm_overwrite {
                    let name = ProfileName::from(state.profile.clone());
                    let pending = state.key.clone();
                    app.pop_modal();
                    run_env_collision_choice(
                        app,
                        &name,
                        &pending,
                        None,
                        EnvCollisionChoice::Overwrite,
                    );
                } else {
                    state.reviewing_overwrite = false;
                    state.scroll.set(0);
                }
            }
            _ => {}
        }
        return;
    }
    let last = EnvCollisionForm::options().len() - 1;
    match key.code {
        KeyCode::Up => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
        }
        KeyCode::Enter => {
            let choice = EnvCollisionForm::options()[state.cursor.min(last)];
            if choice == EnvCollisionChoice::Overwrite && state.overwrite_needs_review.get() {
                state.reviewing_overwrite = true;
                state.confirm_overwrite = false;
                state.scroll.set(0);
                return;
            }
            let name = ProfileName::from(state.profile.clone());
            let pending = state.key.clone();
            let existing_idx = state.existing_idx;
            app.pop_modal();
            run_env_collision_choice(app, &name, &pending, existing_idx, choice);
        }
        _ => {}
    }
}

fn run_env_collision_choice(
    app: &mut App,
    name: &ProfileName,
    key: &str,
    existing_idx: Option<usize>,
    choice: EnvCollisionChoice,
) {
    match choice {
        EnvCollisionChoice::Overwrite => {
            let _ = env_add_commit(app, name, key);
        }
        EnvCollisionChoice::KeepExisting => {
            // For an own-field clash, jump to the existing entry; else just dismiss.
            if let Some(idx) = existing_idx
                && let Some(row_pos) = config_row_index(app, ConfigRow::EnvEntry(idx))
            {
                app.config_action_cursor = row_pos;
            }
        }
        EnvCollisionChoice::Cancel => {}
    }
}

/// Rebind every name-keyed suspended field from the old name to the new one
/// after an in-app rename, so a landing never sees the renamed account as gone
/// and drops the suspended work. The Setup draft's own `editing_name` is
/// rebound beside it in [`commit_rename`].
fn rebind_name_keyed_state(app: &mut App, old: &ProfileName, new: &ProfileName) {
    if let Some(editor) = app.note_editor.as_mut()
        && editor.profile == *old
    {
        editor.profile = new.clone();
    }
    if let Some(edit) = app.fallback_edit.as_mut()
        && edit.member == *old
    {
        edit.member = new.clone();
    }
    if let Some(stop) = app.fallback_day_stop.as_mut()
        && stop.member == *old
    {
        stop.member = new.clone();
    }
}

fn commit_rename(app: &mut App) -> bool {
    let Some(d) = app.config_draft.as_ref() else {
        return false;
    };
    let Some(old) = d.editing_name.clone() else {
        return false;
    };
    let new = d.name.trimmed().to_string();
    if new == old {
        if let Some(d) = app.config_draft.as_mut() {
            d.active = None;
        }
        return true;
    }
    let validation = validate_profile_name(&new, Harness::Claude, Some(old.as_str()));
    if let Err(e) = validation {
        app.toast(ToastKind::Danger, escape_control(&error_toast_detail(&e)));
        return false;
    }
    // The rotation guard is taken BEFORE `app.config()`: ROTATION ranks outside
    // `Config`, so acquiring it under the config guard is the inversion
    // `lockorder` asserts on.
    let old_pn = ProfileName::from(old.clone());
    let new_pn = ProfileName::from(new.clone());
    // Renaming the active profile relinks the live slot. A held rename keeps
    // the field open; the settle's answer commits it again.
    let _placed = if app.config().is_active(&old_pn) {
        let Some(placed) = bare_item_gate(app, BareItemWrite::Rename) else {
            return false;
        };
        Some(placed)
    } else {
        None
    };
    let result = rotation_guard_for_mutation(&old_pn).and_then(|rotation| {
        let mut cfg = app.config();
        rename_profile(&mut cfg, &old_pn, &new_pn, &rotation)
    });
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.apply_forgets();
            rebind_name_keyed_state(app, &old_pn, &new_pn);
            if let Some(d) = app.config_draft.as_mut() {
                d.editing_name = Some(new.clone());
                d.name = InputState::new(&new);
                d.active = None;
            }
            app.toast(ToastKind::Success, format!("renamed '{old}' → '{new}'"));
            true
        }
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("rename failed\n{}", escape_control(&error_toast_detail(&e))),
            );
            false
        }
    }
}

fn commit_endpoint(app: &mut App) -> bool {
    let Some(d) = app.config_draft.as_ref() else {
        return false;
    };
    let Some(name) = d.editing_name.clone().map(ProfileName::from) else {
        return false;
    };
    let active_field = d.active;
    let base_url = d.base_url.trimmed_some();
    // An API key only makes sense with a base URL; drop it for OAuth.
    let api_key = if base_url.is_some() {
        d.api_key.trimmed_some()
    } else {
        None
    };
    let result = {
        let mut cfg = app.config();
        edit_profile_endpoint(&mut cfg, &name, base_url, api_key)
    };
    match result {
        Ok(()) => {
            // Re-collect the token and third-party entries so the fetch leg
            // schedules the re-keyed credential's first fetch same-frame. The
            // rejection verdict reads the AppConfig profile fingerprint, never
            // this list, so a stale entry cannot keep the name key-rejected.
            app.refresh_tokens();
            app.apply_forgets();
            // Reseed from the saved profile (API key may have been dropped).
            let (base, key) = {
                let cfg = app.config();
                let p = cfg.find(&name);
                (
                    p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
                    p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
                )
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.base_url = InputState::new(&base);
                d.api_key = InputState::new(&key);
                // Re-login chain: after the base-url step, advance into the
                // api-key editor instead of ending (only while it's still an API
                // account — an emptied base url flipped it to OAuth, key dropped).
                if d.relogin_chain && active_field == Some(ConfigRow::BaseUrl) && !base.is_empty() {
                    d.api_key.end();
                    d.active = Some(ConfigRow::ApiKey);
                    d.field_seed = Some(d.api_key.value.clone());
                } else {
                    d.relogin_chain = false;
                    d.active = None;
                }
            }
            true
        }
        Err(e) => {
            app.toast(
                ToastKind::Danger,
                format!("edit failed\n{}", escape_control(&error_toast_detail(&e))),
            );
            false
        }
    }
}

/// The shared tail after a create leaves the profile on disk: the success path
/// and each failure branch whose profile landed both run it, then keep their
/// own toast. Closes the form onto the new profile so the pane shows it as an
/// ordinary account instead of a dead `+ new` draft.
fn finish_created_profile(app: &mut App, name: &str) {
    app.refresh_tokens();
    app.apply_forgets();
    app.refresh_unsaved_live_login();
    let new_idx = app
        .config()
        .profiles
        .iter()
        .position(|p| p.name == name)
        .unwrap_or(0);
    app.profile_cursor.set(new_idx);
    land_created_item(app, None, MainItemKind::Profile(new_idx));
}

fn commit_new_account(app: &mut App) {
    let Some(d) = app.config_draft.as_ref() else {
        return;
    };
    let name = d.name.trimmed().to_string();
    let base_url = d.base_url.trimmed_some();
    let api_key = if base_url.is_some() {
        d.api_key.trimmed_some()
    } else {
        None
    };
    let mut models = d.typed_models();
    let preset_env = d.preset_env.clone();
    let captured = match d.captured_login.clone() {
        live @ Some(DraftLogin::LiveLogin(_)) => live,
        login @ Some(DraftLogin::Mint(_)) if base_url.is_none() => login,
        _ => None,
    };
    let mint_discarded =
        base_url.is_some() && matches!(d.captured_login, Some(DraftLogin::Mint(_)));
    let endpoint_overridden =
        base_url.is_some() && matches!(d.captured_login, Some(DraftLogin::LiveLogin(_)));
    let models_dropped = !models.is_empty()
        && matches!(&captured, Some(DraftLogin::LiveLogin(snapshot)) if snapshot.base_url != base_url);
    if models_dropped {
        models = ModelSettings::default();
    }
    if captured.is_some() && guest_refused(app) {
        return;
    }
    if let Err(e) = validate_profile_name(&name, Harness::Claude, None) {
        app.toast(ToastKind::Danger, escape_control(&error_toast_detail(&e)));
        return;
    }
    let _placed = if matches!(captured, Some(DraftLogin::Mint(_)))
        && app.config().state.active_profile.is_none()
    {
        let Some(placed) = bare_item_gate(app, BareItemWrite::NewAccount) else {
            return;
        };
        Some(placed)
    } else {
        None
    };
    let result = {
        let mut cfg = app.config();
        match captured {
            Some(DraftLogin::LiveLogin(snapshot)) => {
                capture_into_profile(&mut cfg, name.clone(), models, *snapshot)
            }
            Some(DraftLogin::Mint(login)) => create_profile_from_login(
                &mut cfg,
                name.clone(),
                models,
                login.credentials,
                login.identity,
                login.plan,
            ),
            Some(DraftLogin::CodexMint(_) | DraftLogin::CodexCapture) => {
                unreachable!("claude create excludes codex stashes")
            }
            None => create_blank_profile_with(
                &mut cfg,
                name.clone(),
                base_url,
                api_key,
                models,
                &preset_env,
            ),
        }
    };
    match result {
        Ok(()) => {
            app.apply_forgets();
            if mint_discarded {
                app.toast(
                    ToastKind::Info,
                    "base url set\nthe captured oauth login was discarded",
                );
            }
            if endpoint_overridden || models_dropped {
                app.toast(ToastKind::Info, "created from the captured current login\nthe typed endpoint or model fields were not used");
            }
            finish_created_profile(app, &name);
            app.toast(ToastKind::Success, format!("created '{name}'"));
        }
        Err(e) => {
            let landed = app
                .config()
                .find(&ProfileName::from(name.as_str()))
                .is_some();
            if landed {
                finish_created_profile(app, &name);
            }
            app.toast(
                ToastKind::Danger,
                format!("create failed\n{}", escape_control(&error_toast_detail(&e))),
            );
        }
    }
}

fn perform_delete(app: &mut App, name: &ProfileName) {
    // The unforced guard in `delete_profile_taking_config` refuses a live-session profile,
    // which would otherwise dead-end the TUI on a danger toast with no way
    // forward. Confirm the deauth risk instead of attempting (and failing)
    // the unforced delete first.
    if crate::runtime::has_live_session(name) {
        app.open_modal(Modal::Confirm(ConfirmState {
            message: format!("delete '{name}' anyway?"),
            detail: Some(
                "this account has a live tollgate start session; deleting it may log that \
                 session out."
                    .to_string(),
            ),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::DeleteLiveSession(name.to_string()),
        }));
        return;
    }
    finish_delete(app, name, false);
}

/// How a delete worker ended.
enum DeleteResult {
    /// The delete ran: done, held, or its refusal or failure.
    Ran(ProfileName, Result<DeleteRan>),
    /// The worker panicked; the in-flight marker must still clear.
    Panicked,
}

/// A delete that ran without failing.
enum DeleteRan {
    Deleted,
    /// The bare item holds a login only the user can place: the delete waits
    /// on [`Modal::BareItem`], having removed nothing.
    Held,
}

/// Run the delete on a worker: its rescue walks the account's dead runtime
/// trees and can wait on another process's rescue lock, and the UI thread
/// waits on neither. The outcome lands through [`drain_profile_delete`].
fn finish_delete(app: &mut App, name: &ProfileName, force: bool) {
    if app.deleting.is_some() {
        app.toast(
            ToastKind::Info,
            "a delete is still running\nwait for it, then try again",
        );
        return;
    }
    app.deleting = Some(name.clone());
    let config = Arc::clone(&app.config);
    let tx = app.delete_tx.clone();
    let bare_item_tx = app.bare_item_tx.clone();
    let source = app.bare_item_source;
    let ready = app.bare_item_ready.take();
    let name = name.clone();
    let settling = ready.is_none() && app.config().is_active(&name);
    let check = if settling {
        begin_bare_item_check(
            app,
            &BareItemWrite::Delete {
                name: name.clone(),
                force,
            },
        )
    } else {
        None
    };
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    let stop_deadline = crate::runtime::SupervisorStopDeadlineGuard::current();
    #[cfg(test)]
    let panic_plant = DELETE_WORKER_PANICS.with(std::cell::Cell::get);
    spawn_worker(move || {
        // A test's supervisor-stop deadline is per thread: the worker takes it.
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        let _stop_deadline = stop_deadline.map(crate::runtime::SupervisorStopDeadlineGuard::set);
        // A panic inside the delete still lands a result, so `deleting` never
        // sticks (the `start_pool_mutation` precedent).
        let result = catch_unwind(AssertUnwindSafe(|| {
            #[cfg(test)]
            if panic_plant {
                panic!("test seam: the delete worker panicked");
            }
            // Deleting the active profile signs the live slot out. Settled on
            // this worker, so the placement guards this thread's write; the
            // notice goes to the event log, the worker having no toast of its
            // own. A login only the user can place holds the delete until
            // they answer; a resumed delete takes their answer instead.
            let _placed = if App::lock_config(&config).is_active(&name) {
                let placed = match ready {
                    Some(placed) => placed,
                    None => {
                        // A panicking settle fails as a settle, so the toast
                        // says what every other bare-item worker's does.
                        let settle = catch_unwind(AssertUnwindSafe(|| {
                            settle_bare_keychain_item(
                                source,
                                crate::claude::BareItemWriter::SignOut,
                            )
                        }))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!(BARE_ITEM_WORKER_PANICKED)));
                        // The check ends with the settle, not with the
                        // delete's own rescue walk after it.
                        drop(check);
                        // The drain shows a delete error's outer link alone,
                        // and a settle's cause sits below it.
                        match settle.map_err(|e| anyhow::anyhow!("{e:#}"))? {
                            crate::claude::BareItemSettle::Settled(settled) => {
                                if let Some(notice) = &settled.notice {
                                    crate::logline::logline!("tollgate: {notice}");
                                }
                                settled.placed
                            }
                            held @ crate::claude::BareItemSettle::NeedsChoice(_) => {
                                let _ = bare_item_tx.send(BareItemAnswer {
                                    write: BareItemWrite::Delete {
                                        name: name.clone(),
                                        force,
                                    },
                                    outcome: BareItemOutcome::Settled(Ok(held)),
                                });
                                return Ok(DeleteRan::Held);
                            }
                        }
                    }
                };
                Some(enter_bare_item_placement(placed))
            } else {
                None
            };
            // Before the config lock, for the reason the rename path states.
            rotation_guard_for_mutation(&name)
                .and_then(|rotation| {
                    // The TUI has no `--force` flag, so no refusal names it.
                    delete_profile_taking_config(
                        || App::lock_config(&config),
                        &name,
                        force,
                        false,
                        &rotation,
                    )
                })
                .map(|()| DeleteRan::Deleted)
        }));
        let _ = tx.send(match result {
            Ok(result) => DeleteResult::Ran(name, result),
            Err(_) => DeleteResult::Panicked,
        });
    });
}

/// Makes every delete worker started on the calling thread panic until dropped.
#[cfg(test)]
pub(crate) struct DeleteWorkerPanics(bool);

#[cfg(test)]
impl DeleteWorkerPanics {
    pub(crate) fn set() -> Self {
        Self(DELETE_WORKER_PANICS.with(|c| c.replace(true)))
    }
}

#[cfg(test)]
impl Drop for DeleteWorkerPanics {
    fn drop(&mut self) {
        DELETE_WORKER_PANICS.with(|c| c.set(self.0));
    }
}

/// Land a finished delete: the stores forget the account and its open draft
/// closes, or the refusal or failure is toasted as it reads.
pub(crate) fn drain_profile_delete(app: &mut App) {
    while let Ok(result) = app.delete_rx.try_recv() {
        app.deleting = None;
        match result {
            DeleteResult::Ran(_, Ok(DeleteRan::Held)) => {}
            DeleteResult::Ran(name, Ok(DeleteRan::Deleted)) => {
                app.refresh_tokens();
                app.apply_forgets();
                app.refresh_unsaved_live_login();
                // A draft opened on another account while the worker ran stays.
                if !app
                    .config_draft
                    .as_ref()
                    .is_some_and(|d| d.editing_name.as_deref() != Some(name.as_str()))
                {
                    app.config_focus = ConfigFocus::Profiles;
                    app.config_draft = None;
                }
                app.clamp_profile_cursor();
                app.toast(ToastKind::Success, format!("deleted '{name}'"));
            }
            DeleteResult::Ran(_, Err(e)) => {
                app.toast(
                    ToastKind::Danger,
                    format!("delete failed\n{}", escape_control(&error_toast_detail(&e))),
                );
            }
            DeleteResult::Panicked => app.toast(ToastKind::Danger, "delete failed"),
        }
    }
}

/// Which way a disable/enable control was asked to go. The label or confirm
/// that produced the pick owns the direction; the account's state at run time
/// only decides whether anything is left to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisableDirection {
    Disable,
    Enable,
}

impl DisableDirection {
    /// The past tense every toast and success line for this control reads with.
    fn verb(self) -> &'static str {
        match self {
            Self::Disable => "disabled",
            Self::Enable => "enabled",
        }
    }
}

/// Disable/enable the named account — the menu's per-row pick resolves its
/// stored claude identity through this and hands it the direction its item's
/// label named. Disabling routes through the confirm modal instead of acting on
/// one press (the row's own arm-then-confirm, in the shape a modal can carry);
/// enabling is harmless, so it fires straight away. A pick whose direction the
/// account already holds writes nothing and says so. The two gates the row
/// renders inert speak up here: a menu carries no dimmed item, so a silent
/// no-op would read as a broken pick.
fn toggle_account_disabled(app: &mut App, name: &ProfileName, direction: DisableDirection) {
    if app.config().is_active(name) {
        app.toast(
            ToastKind::Warning,
            format!("'{name}' is the active account\nswitch away before disabling it"),
        );
        return;
    }
    if crate::runtime::has_live_session(name) {
        app.toast(
            ToastKind::Warning,
            format!("'{name}' has a live session\nclose it before disabling"),
        );
        return;
    }
    let already_there = app.config().find(name).is_some_and(Profile::is_disabled)
        == matches!(direction, DisableDirection::Disable);
    if already_there {
        app.toast(
            ToastKind::Info,
            format!("'{name}' is already {}", direction.verb()),
        );
        return;
    }
    match direction {
        DisableDirection::Enable => toggle_profile_disabled(app, name, direction),
        DisableDirection::Disable => {
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("disable '{name}'?"),
                detail: Some(DISABLE_DETAIL.to_string()),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::DisableOne(name.to_string()),
            }));
        }
    }
}

/// Ask for the duplicate's new name. The copy itself runs on ⏎, so esc costs
/// nothing.
fn duplicate_account(app: &mut App, name: &ProfileName) {
    app.open_modal(Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::DuplicateProfile(name.to_string()),
    }));
}

/// The answer to deleting a built-in preset: it has no file to drop.
fn toast_builtin_preset(app: &mut App, name: &str) {
    app.toast(
        ToastKind::Info,
        format!("'{name}' is built in and always available"),
    );
}

/// Ask for the preset name.
fn save_preset_account(app: &mut App, name: &ProfileName) {
    app.open_modal(Modal::NamePrompt(NamePromptForm {
        input: InputState::new(""),
        action: NamePromptAction::SavePreset(name.to_string()),
    }));
}

/// What opening the preset picker did to the Setup pane, so a cancel of the
/// pick sequence can reverse it losslessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PresetPickerOpen {
    /// No undo: the pane was already on a form, or the picker targets an
    /// existing account.
    #[default]
    None,
    /// A fresh `+ new` form was built and descended; a cancel drops it and
    /// ascends.
    Fresh,
    /// A suspended `+ new` form was restored and descended; a cancel returns
    /// it to `new_account_draft` and ascends.
    Restored,
}

/// Reverse [`open_preset_picker`]'s open-side effect when the pick sequence
/// ends with no preset applied: a restored form goes back to its home through
/// the guarded writer, a fresh one is dropped, and the pane ascends to its
/// selector. A no-op when the open changed nothing, and unless the pane holds
/// the `+ new` draft it built or restored — a stale flag over an existing
/// account's draft must not move it.
fn undo_preset_picker_open(app: &mut App) {
    let is_new_form = app
        .config_draft
        .as_ref()
        .is_some_and(|d| d.editing_name.is_none());
    match app.preset_picker_open {
        PresetPickerOpen::None => {}
        PresetPickerOpen::Fresh if is_new_form => {
            app.config_draft = None;
            app.config_focus = ConfigFocus::Profiles;
        }
        PresetPickerOpen::Restored if is_new_form => {
            let Some(draft) = app.config_draft.take() else {
                app.config_focus = ConfigFocus::Profiles;
                app.preset_picker_open = PresetPickerOpen::None;
                return;
            };
            match set_new_account_draft(app, draft) {
                None => app.config_focus = ConfigFocus::Profiles,
                Some(draft) => app.config_draft = Some(draft),
            }
        }
        PresetPickerOpen::Fresh | PresetPickerOpen::Restored => {}
    }
    app.preset_picker_open = PresetPickerOpen::None;
}

/// `apply preset`: open the picker. The list always holds the built-ins, so
/// it is never empty and needs no empty state. On `+ new` the preset's fields
/// are stamped into the draft, not a saved profile.
fn open_preset_picker(app: &mut App) {
    app.preset_picker_open = PresetPickerOpen::None;
    let target = if let Some((name, _, _)) = focused_account(app) {
        PresetTarget::from(name.to_string())
    } else if app.profile_cursor.read() >= app.profile_count() {
        if app.config_draft.is_none() {
            // Restore the suspended `+ new` form (or build a fresh one) and
            // descend onto it, so the preset stamps the user's own form and
            // the form is the active pane, so a switch suspends it and `esc`/`←` ascend from it.
            // `preset_picker_open` records which, so a cancel reverses it.
            let restored = app.new_account_draft.is_some();
            app.config_draft = app
                .new_account_draft
                .take()
                .or_else(|| Some(build_draft_new()));
            app.config_focus = ConfigFocus::Actions;
            app.config_action_cursor = 0;
            app.preset_picker_open = if restored {
                PresetPickerOpen::Restored
            } else {
                PresetPickerOpen::Fresh
            };
        }
        PresetTarget::NewAccount
    } else {
        return;
    };
    open_preset_picker_for(app, target);
}

/// [`open_preset_picker`] for a named account — the menu's `apply preset` pick
/// resolves its stored claude identity through this.
fn apply_preset_account(app: &mut App, name: &ProfileName) {
    app.preset_picker_open = PresetPickerOpen::None;
    open_preset_picker_for(app, PresetTarget::from(name.to_string()));
}

/// Open the preset picker for `target` (the shared tail of
/// [`open_preset_picker`] and [`apply_preset_account`]).
fn open_preset_picker_for(app: &mut App, target: PresetTarget) {
    app.open_modal(Modal::PresetPicker(PresetPickerForm {
        target,
        presets: crate::presets::list_presets(),
        cursor: 0,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
}

/// ⏎ on the name prompt. The name is validated for the action it carries —
/// a duplicate against the profile roster, a preset against the preset store —
/// and a failure toasts with the modal still open so the name can be fixed.
fn handle_name_prompt_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::NamePrompt(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        // Esc closes; `q` is a valid name character and falls through to the
        // input editor. (The other modals' `q` shortcut doesn't apply to a
        // text field.)
        KeyCode::Esc => {
            app.pop_modal();
        }
        KeyCode::Enter => {
            let name = form.input.trimmed().to_string();
            let action = form.action.clone();
            match action {
                NamePromptAction::DuplicateProfile(source) => {
                    let validation = validate_profile_name(&name, Harness::Claude, None);
                    if let Err(e) = validation {
                        app.toast(ToastKind::Danger, escape_control(&error_toast_detail(&e)));
                        return;
                    }
                    app.pop_modal();
                    duplicate_profile_into(
                        app,
                        &ProfileName::from(source),
                        &ProfileName::from(name),
                    );
                }
                NamePromptAction::SavePreset(source) => {
                    // The preset store is its own namespace — neither harness's
                    // roster has a say; only the charset + built-in rules apply.
                    if let Err(e) = validate_name_chars(&name) {
                        app.toast(ToastKind::Danger, escape_control(&error_toast_detail(&e)));
                        return;
                    }
                    if crate::presets::is_builtin(&name) {
                        app.toast(
                            ToastKind::Danger,
                            format!("'{name}' is a built-in preset\npick another name"),
                        );
                        return;
                    }
                    app.pop_modal();
                    if crate::presets::preset_exists(&name) {
                        app.open_modal(Modal::Confirm(ConfirmState {
                            message: format!("preset '{name}' already exists."),
                            detail: Some(
                                "overwrite it with this account's base url and model settings?"
                                    .to_string(),
                            ),
                            choice: false,
                            scroll: ConfirmScroll::default(),
                            on_confirm: ConfirmAction::OverwritePreset(name, source),
                        }));
                        return;
                    }
                    save_preset_from(app, &source, &name);
                }
                NamePromptAction::SaveBareItem => {
                    if guest_refused(app) {
                        return;
                    }
                    let Some(naming) = app.bare_item_naming.take() else {
                        app.pop_modal();
                        return;
                    };
                    let result = {
                        let mut cfg = app.config();
                        crate::actions::save_bare_item_as_profile(
                            &mut cfg,
                            &name,
                            naming.login.clone(),
                            naming.account.clone(),
                        )
                    };
                    if let Err(e) = result {
                        // The prompt stays open on its own login for another name.
                        app.bare_item_naming = Some(naming);
                        app.toast(ToastKind::Danger, format!("{e:#}"));
                        return;
                    }
                    let write = naming.write;
                    app.pop_modal();
                    app.refresh_tokens();
                    app.apply_forgets();
                    app.refresh_unsaved_live_login();
                    app.toast(ToastKind::Success, format!("captured '{name}'"));
                    // The login is saved: settle again before the held write.
                    spawn_bare_item_settle(app, write);
                }
            }
        }
        _ => apply_input_edit(&mut form.input, key),
    }
}

fn handle_preset_picker_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::PresetPicker(state)) = app.modals.last_mut() else {
        return;
    };
    let last = state.presets.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
            undo_preset_picker_open(app);
        }
        KeyCode::Up => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::PageUp => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                &[],
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
        }
        KeyCode::PageDown => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
        }
        // Drop a custom preset. Built-ins have no file to drop, so the pick says
        // so rather than no-opping: a list carries no dimmed row here. The toast
        // fires with the picker still mounted so the user can pick another.
        KeyCode::Char('d') => {
            let Some(preset) = state.presets.get(state.cursor.min(last)).cloned() else {
                return;
            };
            if preset.builtin {
                toast_builtin_preset(app, &preset.name);
                return;
            }
            app.pop_modal();
            // The picker is gone, so no preset will be applied: undo the
            // open-side effect before the delete confirm.
            undo_preset_picker_open(app);
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("delete preset '{}'?", preset.name),
                detail: Some("accounts already stamped from it are untouched.".to_string()),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::DeletePreset(preset),
            }));
        }
        KeyCode::Enter => {
            let picked = state.presets.get(state.cursor.min(last)).cloned();
            let target = state.target.clone();
            app.pop_modal();
            if let Some(preset) = picked {
                offer_preset_apply(app, target, &preset);
            }
        }
        _ => {}
    }
}

/// The endpoint + model block `apply preset` lands on: the draft's buffers on
/// `+ new`, which stand in for an account's values, the saved account's own
/// otherwise. `None` once the account is gone.
fn preset_target_block(
    app: &App,
    target: &PresetTarget,
) -> Option<(Option<String>, ModelSettings)> {
    match target {
        PresetTarget::NewAccount => app
            .config_draft
            .as_ref()
            .map(|d| (d.base_url.trimmed_some(), d.typed_models())),
        PresetTarget::Account(name) => {
            let cfg = app.config();
            cfg.find(&ProfileName::from(name.as_str()))
                .map(|p| (p.base_url.clone(), p.models.clone()))
        }
    }
}

/// Apply `preset` to `target`, asking first whenever it would change a value:
/// the keep / clear confirm when the target holds a value the preset leaves
/// unset, the plain confirm when it only replaces values, straight through
/// otherwise. Naming the fields is the point of asking: "this overwrites
/// settings" says nothing a user can act on.
fn offer_preset_apply(app: &mut App, target: PresetTarget, preset: &crate::presets::Preset) {
    let Some((base_url, models)) = preset_target_block(app, &target) else {
        // The pick sequence ends here with no stamp, so the open's undo is not
        // owed and the flag must not outlive its sequence.
        app.preset_picker_open = PresetPickerOpen::None;
        app.toast(ToastKind::Danger, format!("{target} is gone"));
        return;
    };
    let (replaces, leaves) = preset.apply_diff(base_url.as_deref(), &models);
    let message = format!("apply '{}' to {target}?", preset.name);
    if !leaves.is_empty() {
        app.open_modal(Modal::PresetApply(PresetApplyForm {
            target,
            preset: preset.clone(),
            message,
            replaces,
            leaves,
            focus: PresetApplyButton::Keep,
            scroll: ConfirmScroll::default(),
        }));
    } else if !replaces.is_empty() {
        app.open_modal(Modal::Confirm(ConfirmState {
            message,
            detail: Some(format!("replaces {}.", replaces.join(", "))),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::ApplyPresetOver(target, preset.clone(), replaces),
        }));
    } else {
        apply_preset_to(
            app,
            &target,
            preset,
            crate::presets::Leftover::Keep,
            &[],
            &[],
        );
    }
}

/// Stamp `preset` onto `target`: every field the preset sets, and every field
/// it leaves unset kept or cleared per `leftover`. `replaces` and `leaves` are
/// the fields the confirm named; a reload or a preset edit since it opened can
/// move them, and then the apply asks again over the fresh diff rather than
/// change a field nobody was shown.
///
/// A saved account is written in a single locked transaction
/// ([`edit_profile_preset`]) so a failure leaves the whole profile on its prior
/// state, and its draft is rebuilt, since every buffer it holds is stale. On
/// `+ new` the result lands in the draft's input buffers and is committed when
/// the create form fires.
///
/// The picked row's `builtin` identity travels with it, so the re-read hits the
/// store that row came from: the saved `qwen` beside the built-in `Qwen`
/// re-reads the file, never the binary's copy, and a confirm can sit open
/// across an edit to the store. The account's own api key is preserved — a
/// template names an endpoint, never the credential for one.
fn apply_preset_to(
    app: &mut App,
    target: &PresetTarget,
    preset: &crate::presets::Preset,
    leftover: crate::presets::Leftover,
    replaces: &[&'static str],
    leaves: &[&'static str],
) {
    // Re-read the store the picked row came from, never a second lookup by name
    // alone: `load_preset("qwen")` answers the built-in, so the saved row must
    // resolve through the disk-only loader.
    let loaded = if preset.builtin {
        crate::presets::load_preset(&preset.name)
    } else {
        crate::presets::load_saved_preset(&preset.name)
    };
    let Some(preset) = loaded else {
        // The pick sequence ends here with no stamp: clear the flag so it
        // cannot act on a later, unrelated cancel.
        app.preset_picker_open = PresetPickerOpen::None;
        app.toast(
            ToastKind::Danger,
            format!("preset '{}' is gone", preset.name),
        );
        return;
    };
    let Some((base_url, models)) = preset_target_block(app, target) else {
        app.preset_picker_open = PresetPickerOpen::None;
        app.toast(ToastKind::Danger, format!("{target} is gone"));
        return;
    };
    if preset.apply_diff(base_url.as_deref(), &models) != (replaces.to_vec(), leaves.to_vec()) {
        offer_preset_apply(app, target.clone(), &preset);
        return;
    }
    let name = match target {
        PresetTarget::NewAccount => {
            // The menu offer gates on the same predicate; re-check here so a
            // pick that raced the type off `claude` stamps nothing rather than
            // editing fields the form no longer shows.
            if !new_account_type_is_claude(app) {
                app.preset_picker_open = PresetPickerOpen::None;
                app.toast(ToastKind::Danger, "apply preset is for the claude form");
                return;
            }
            let selected = config_rows(app).get(app.config_action_cursor).copied();
            stamp_draft_from_preset(app, &preset, leftover);
            if let Some(pos) = selected.and_then(|row| config_row_index(app, row)) {
                app.config_action_cursor = pos;
            }
            // A preset landed, so the pick sequence's open-side effect is done.
            app.preset_picker_open = PresetPickerOpen::None;
            return;
        }
        PresetTarget::Account(name) => ProfileName::from(name.as_str()),
    };
    let (new_url, new_models) = preset.applied_to(&base_url, &models, leftover);
    let result = {
        let mut cfg = app.config();
        edit_profile_preset(&mut cfg, &name, new_url, new_models, &preset.env)
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            // A repoint retires the old provider's verdicts — the drain
            // forgets the changed halves, keeping the old streak from accusing
            // the new endpoint. When nothing repointed the drain is a no-op.
            app.apply_forgets();
            if app.config_draft.is_some() {
                let selected = config_rows(app).get(app.config_action_cursor).copied();
                app.config_draft = Some(build_draft_existing(app, &name));
                if let Some(pos) = selected.and_then(|row| config_row_index(app, row)) {
                    app.config_action_cursor = pos;
                }
            }
            app.toast(
                ToastKind::Success,
                format!("applied '{}' to '{name}'", preset.name),
            );
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "apply preset failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

/// Write `preset`'s fields into the `+ new` draft's input buffers, the typed
/// buffers standing in for a saved account's values: the draft ends where the
/// same apply leaves a saved account, alias rows shown the way that account's
/// rebuilt pane shows them.
fn stamp_draft_from_preset(
    app: &mut App,
    preset: &crate::presets::Preset,
    leftover: crate::presets::Leftover,
) {
    let Some(draft) = app.config_draft.as_mut() else {
        return;
    };
    let (base_url, models) = preset.applied_to(
        &draft.base_url.trimmed_some(),
        &draft.typed_models(),
        leftover,
    );
    for (value, field) in [
        (base_url, &mut draft.base_url),
        (models.default, &mut draft.model),
        (models.opus, &mut draft.opus_model),
        (models.sonnet, &mut draft.sonnet_model),
        (models.haiku, &mut draft.haiku_model),
        (models.fable, &mut draft.fable_model),
        (models.subagent, &mut draft.subagent_model),
    ] {
        *field = InputState::new(value.as_deref().unwrap_or_default());
    }
    draft.preset_env.extend(preset.env.clone());
    draft.overrides_expanded = false;
    draft.overrides_shown = draft.filled_overrides();
    let name = preset.name.clone();
    app.toast(
        ToastKind::Success,
        format!("stamped '{name}' onto the new account"),
    );
}

/// Save `source`'s base url + models under `preset`.
fn save_preset_from(app: &mut App, source: &str, preset: &str) {
    let fields = {
        let cfg = app.config();
        cfg.find(&ProfileName::from(source))
            .map(|p| (p.base_url.clone(), p.models.clone()))
    };
    let Some((base_url, models)) = fields else {
        app.toast(ToastKind::Danger, format!("'{source}' is gone"));
        return;
    };
    match crate::presets::save_preset(preset, &base_url, &models) {
        Ok(()) => app.toast(ToastKind::Success, format!("saved preset '{preset}'")),
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "save preset failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

/// Copy `source` onto a new account named `new_name` and select it.
fn duplicate_profile_into(app: &mut App, source: &ProfileName, new_name: &ProfileName) {
    let result = {
        let mut cfg = app.config();
        duplicate_profile(&mut cfg, source, new_name.to_string())
    };
    match result {
        Ok(()) => {
            // A duplicate is a create (the spec's "after any create, claude
            // included"): the same shared tail as the `+ new` create — lands
            // Setup on the new row and the Overview's remembered row onto it.
            finish_created_profile(app, new_name.as_str());
            app.toast(
                ToastKind::Success,
                format!("duplicated '{source}' as '{new_name}'"),
            );
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "duplicate failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

/// Set `name`'s `Profile::disabled` flag the way `direction` names (the Setup
/// `disabled` row and the menu's disable confirm both route here). Inert while
/// `name` is the active profile or holds a live `tollgate start` session — the
/// same gate `actions::disable_profile` itself enforces, checked here TOO so
/// the row stays truly inert (silent no-op, matching the dimmed-row rule):
/// `disable_profile`'s own refusal is a real `bail!`, and without this early
/// return every press would surface a red danger toast instead of doing
/// nothing, which is what a dimmed/disabled row is supposed to mean. A
/// direction the account already holds writes nothing and says so.
/// `disable_profile`/`enable_profile` persist into the live shared
/// `AppConfig` directly (`app.config()`), so the flag renders next frame with
/// no reload round-trip; `refresh_tokens` rebuilds the scheduler's per-profile
/// work lists (`collect_tokens`/`collect_third_party_entries` both filter on
/// `is_disabled`) the same way `toggle_auto_start` does, since a per-profile
/// `config.toml` write doesn't bump the mtime the periodic reload watches.
fn toggle_profile_disabled(app: &mut App, name: &ProfileName, direction: DisableDirection) {
    let gated = app.config().is_active(name) || crate::runtime::has_live_session(name);
    if gated {
        return;
    }
    let result = {
        let mut cfg = app.config();
        match direction {
            DisableDirection::Disable => crate::actions::disable_profile(&mut cfg, name),
            DisableDirection::Enable => crate::actions::enable_profile(&mut cfg, name),
        }
    };
    match result {
        Ok(true) => {
            app.refresh_tokens();
            app.toast(ToastKind::Success, format!("{} '{name}'", direction.verb()));
        }
        Ok(false) => app.toast(
            ToastKind::Info,
            format!("'{name}' is already {}", direction.verb()),
        ),
        Err(e) => app.toast(
            ToastKind::Danger,
            format!("toggle failed\n{}", escape_control(&error_toast_detail(&e))),
        ),
    }
}

fn toggle_auto_start(app: &mut App, name: &ProfileName) {
    enum Outcome {
        NotOAuth,
        Saved(bool),
        SaveFailed(anyhow::Error),
        Missing,
    }
    let outcome = {
        let mut cfg = app.config();
        match cfg.find_mut(name) {
            None => Outcome::Missing,
            Some(profile) if !profile.is_oauth() => Outcome::NotOAuth,
            Some(profile) => {
                profile.auto_start = !profile.auto_start;
                let now_on = profile.auto_start;
                match save_profile_config(profile) {
                    Ok(()) => Outcome::Saved(now_on),
                    Err(e) => {
                        if let Some(p) = cfg.find_mut(name) {
                            p.auto_start = !now_on;
                        }
                        Outcome::SaveFailed(e)
                    }
                }
            }
        }
    };
    match outcome {
        Outcome::Missing => {}
        Outcome::NotOAuth => app.toast(
            ToastKind::Warning,
            "auto-start only works on oauth accounts",
        ),
        Outcome::Saved(_now_on) => {
            // Rebuild the scheduler's token snapshot so the new `auto_start` flag
            // reaches the fetch leg's window-lapsed gate. The per-profile
            // config.toml write doesn't bump the profiles.toml mtime that
            // `reload_if_state_changed` watches, so without this the toggle would
            // lag until the next unrelated snapshot rebuild. The periodic tick
            // then opens a window if this profile now lacks a live one.
            app.refresh_tokens();
        }
        Outcome::SaveFailed(e) => {
            app.toast(
                ToastKind::Danger,
                format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
            );
        }
    }
}

/// Flip `name`'s usage probe and save it, rolling memory back on a failed
/// save. The probe leg reads the shared config each tick, so the switch
/// reaches it without a token-list rebuild.
fn toggle_usage_probe(app: &mut App, name: &ProfileName) {
    let saved = {
        let mut cfg = app.config();
        let Some(profile) = cfg.find_mut(name) else {
            return;
        };
        profile.usage_probe = !profile.usage_probe;
        let saved = save_profile_config(profile)
            .map(|()| (profile.usage_probe, profile.usage_probe_interval_ms));
        if saved.is_err() {
            profile.usage_probe = !profile.usage_probe;
        }
        saved
    };
    match saved {
        // Off, nothing reads the account any more.
        Ok((false, _)) => crate::usage::forget_probe_reading(&app.memory_stores(), name),
        // On, the cached reading comes back at once, as a restart would seed
        // it, dated at the probe's interval. The later of the held stamp and
        // the reading's own keeps the cadence and any `retry-after` wait, so
        // no probe goes out early for it.
        Ok((true, interval_ms)) => crate::usage::reseed_keeping_cadence(
            &app.usage_store,
            &app.usage_status,
            &app.last_fetched,
            name,
            interval_ms,
        ),
        Err(e) => app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&e.to_string())),
        ),
    }
}

/// What one `+`/`-` on the `probe every` row moves the interval by.
const USAGE_PROBE_STEP_MS: i64 = 30_000;

/// Step the drafted account's probe interval by `delta_ms` from wherever it
/// stands. It writes the interval alone, never the switch.
fn step_usage_probe_interval(app: &mut App, delta_ms: i64) {
    let Some(current) = app.config_draft.as_ref().and_then(|d| {
        let cfg = app.config();
        cfg.find(&ProfileName::from(d.editing_name.clone()?))
            .map(|p| p.usage_probe_interval_ms)
    }) else {
        return;
    };
    write_usage_probe_interval(app, current.saturating_add_signed(delta_ms));
}

/// ⏎ on the `probe every` field: whole seconds, an `s` after them allowed,
/// inside the band the loader keeps the interval in; anything else keeps the
/// editor open, as `refresh` and `alert at` do.
fn commit_usage_probe_interval(app: &mut App) -> bool {
    let raw = app
        .config_draft
        .as_ref()
        .map(|d| d.usage_probe_interval.trimmed().to_string())
        .unwrap_or_default();
    let Some(ms) = parse_usage_probe_interval(&raw) else {
        return false;
    };
    write_usage_probe_interval(app, ms)
}

/// A typed `probe every` value in milliseconds: whole seconds, with or without
/// a trailing `s`, between [`crate::profile::MIN_USAGE_PROBE_INTERVAL_MS`] and
/// [`crate::profile::MAX_USAGE_PROBE_INTERVAL_MS`]. `None` = refused.
pub(crate) fn parse_usage_probe_interval(raw: &str) -> Option<u64> {
    let secs = raw.strip_suffix('s').unwrap_or(raw).trim();
    secs.parse::<u64>().ok()?.checked_mul(1000).filter(|ms| {
        (crate::profile::MIN_USAGE_PROBE_INTERVAL_MS..=crate::profile::MAX_USAGE_PROBE_INTERVAL_MS)
            .contains(ms)
    })
}

/// Persist the drafted account's probe interval clamped into its band,
/// rolling memory back on a failed save, then reseed the row's buffer from
/// what was saved.
fn write_usage_probe_interval(app: &mut App, ms: u64) -> bool {
    let Some(name) = app
        .config_draft
        .as_ref()
        .and_then(|d| d.editing_name.clone())
        .map(ProfileName::from)
    else {
        return false;
    };
    let ms = ms.clamp(
        crate::profile::MIN_USAGE_PROBE_INTERVAL_MS,
        crate::profile::MAX_USAGE_PROBE_INTERVAL_MS,
    );
    let result = {
        let mut cfg = app.config();
        match cfg.find_mut(&name) {
            None => None,
            Some(profile) => {
                let before = profile.usage_probe_interval_ms;
                profile.usage_probe_interval_ms = ms;
                let saved = if ms == before {
                    Ok(())
                } else {
                    save_profile_config(profile)
                };
                if saved.is_err()
                    && let Some(p) = cfg.find_mut(&name)
                {
                    p.usage_probe_interval_ms = before;
                }
                Some(saved)
            }
        }
    };
    match result {
        None => {
            if let Some(d) = app.config_draft.as_mut() {
                d.active = None;
            }
            true
        }
        Some(Ok(())) => {
            let shown = {
                let cfg = app.config();
                row_committed_value(cfg.find(&name), &name, ConfigRow::UsageProbeInterval)
            };
            if let Some(d) = app.config_draft.as_mut() {
                d.usage_probe_interval = InputState::new(&shown);
                d.active = None;
            }
            true
        }
        Some(Err(e)) => {
            app.toast(
                ToastKind::Danger,
                format!("save failed\n{}", escape_control(&e.to_string())),
            );
            false
        }
    }
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Confirm(state)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            state.choice = !state.choice;
        }
        KeyCode::Char('y') => state.choice = true,
        KeyCode::Char('n') => state.choice = false,
        KeyCode::Up => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = state.scroll.offset.min(max).saturating_sub(1);
        }
        KeyCode::Down => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = state.scroll.offset.saturating_add(1).min(max);
        }
        KeyCode::PageUp => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = page_up_scroll(
                state.scroll.offset as usize,
                max as usize,
                state.scroll.viewport.get() as usize,
            ) as u16;
        }
        KeyCode::PageDown => {
            let max = state.scroll.max_scroll.get();
            state.scroll.offset = page_down_scroll(
                state.scroll.offset as usize,
                max as usize,
                state.scroll.viewport.get() as usize,
            ) as u16;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            let is_preset = matches!(&state.on_confirm, ConfirmAction::ApplyPresetOver(..));
            app.pop_modal();
            if is_preset {
                undo_preset_picker_open(app);
            }
        }
        KeyCode::Enter => {
            let confirmed = state.choice;
            let action = state.on_confirm.clone();
            let is_preset = matches!(&action, ConfirmAction::ApplyPresetOver(..));
            app.pop_modal();
            if confirmed {
                run_confirm_action(app, action);
            } else if is_preset {
                undo_preset_picker_open(app);
            }
        }
        _ => {}
    }
}

/// The loss warning's grammar over its buttons: `←`/`→` step and stop at the
/// ends, `tab`/`⇧tab` wrap, `↵` chooses, `esc`/`q` = `go back`. `discard`
/// drops the warned work, `save` validates then commits it, and whichever lands
/// performs the switch or quit that opened the warning.
fn handle_loss_warning_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::LossWarning(state)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Left => {
            state.focus = step_loss_button(state.focus, state.drop.saveable(), -1);
        }
        KeyCode::Right => {
            state.focus = step_loss_button(state.focus, state.drop.saveable(), 1);
        }
        KeyCode::Tab => {
            state.focus = wrap_loss_button(state.focus, state.drop.saveable(), 1);
        }
        KeyCode::BackTab => {
            state.focus = wrap_loss_button(state.focus, state.drop.saveable(), -1);
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
        }
        KeyCode::Enter => {
            let focus = state.focus;
            let target = state.target.clone();
            let dirty = state.drop.dirty.clone();
            app.pop_modal();
            match focus {
                LossWarningButton::GoBack => {}
                LossWarningButton::Discard => {
                    discard_dirty(app, &dirty);
                    // A quit goes through the one quit function, so it holds on
                    // an in-flight field save and never exits under a running
                    // write; a switch keeps running behind the switch.
                    resolve_target(app, target);
                }
                LossWarningButton::Save => {
                    let _ = try_save_all(app, &dirty, target);
                }
            }
        }
        _ => {}
    }
}

/// Step the highlighted button one position left (-1) or right (1), stopping at
/// the ends. `[go back, discard]` with nothing to save, `[go back, discard,
/// save]` otherwise.
fn step_loss_button(focus: LossWarningButton, saveable: bool, dir: i8) -> LossWarningButton {
    let idx = loss_button_index(focus);
    let count = loss_button_count(saveable);
    let next = if dir < 0 {
        idx.saturating_sub(1)
    } else {
        (idx + 1).min(count - 1)
    };
    loss_button_at(next)
}

/// Wrap the highlighted button forward (1) or backward (-1).
fn wrap_loss_button(focus: LossWarningButton, saveable: bool, dir: i8) -> LossWarningButton {
    let idx = loss_button_index(focus);
    let count = loss_button_count(saveable);
    let next = if dir < 0 {
        (idx + count - 1) % count
    } else {
        (idx + 1) % count
    };
    loss_button_at(next)
}

fn loss_button_count(saveable: bool) -> usize {
    if saveable { 3 } else { 2 }
}

fn loss_button_index(focus: LossWarningButton) -> usize {
    match focus {
        LossWarningButton::GoBack => 0,
        LossWarningButton::Discard => 1,
        LossWarningButton::Save => 2,
    }
}

fn loss_button_at(index: usize) -> LossWarningButton {
    match index {
        0 => LossWarningButton::GoBack,
        1 => LossWarningButton::Discard,
        _ => LossWarningButton::Save,
    }
}

/// The tab a switch-form target lands on.
fn target_tab(target: &LossWarningTarget) -> Tab {
    match target {
        LossWarningTarget::Switch(tab) => *tab,
        LossWarningTarget::NewAccount | LossWarningTarget::EditAccount(_) => Tab::Setup,
        LossWarningTarget::Quit => Tab::Overview,
    }
}

/// A cross-screen jump's entry: disarm, warn over changed work on the screen
/// being left (with the jump's own landing as the target), else land.
fn request_target(app: &mut App, target: LossWarningTarget) {
    disarm_switch(app);
    if app.tab == target_tab(&target) {
        resolve_target(app, target);
        return;
    }
    let dirty = changed_work_on(app, app.tab);
    if !dirty.is_empty() {
        open_loss_warning(app, target, dirty);
        return;
    }

    resolve_target(app, target);
}

/// Resolve the landing the warning's buttons answered.
fn resolve_target(app: &mut App, target: LossWarningTarget) {
    match target {
        LossWarningTarget::Switch(tab) => do_switch(app, tab),
        LossWarningTarget::Quit => quit_or_wait(app),
        LossWarningTarget::NewAccount => land_new_account(app),
        LossWarningTarget::EditAccount(item) => land_edit_account(app, item),
    }
}

/// Drop every changed item as its `esc` would (the open edit closes, the
fn discard_dirty(app: &mut App, dirty: &[Dirty]) {
    for d in dirty.iter().cloned() {
        match d {
            Dirty::SetupField => {
                if let Some(field) = app.config_draft.as_ref().and_then(|d| d.active) {
                    cancel_config_edit(app, field);
                }
            }
            Dirty::NewAccountForm => {
                if app
                    .config_draft
                    .as_ref()
                    .is_some_and(|d| d.editing_name.is_none())
                {
                    app.config_draft = None;
                }
                app.new_account_draft = None;
                app.config_focus = ConfigFocus::Profiles;
            }
            Dirty::FallbackField => app.fallback_edit = None,
            Dirty::RefreshInterval => app.refresh_interval_draft = None,
            Dirty::ContextNudge => app.context_nudge_draft = None,
            Dirty::WeeklyThreshold => app.weekly_threshold_draft = None,
            Dirty::HerdrTag => app.plugin.herdr_tag_draft = None,
            Dirty::Note => app.note_editor = None,
        }
    }
}

fn try_save_all(app: &mut App, dirty: &[Dirty], target: LossWarningTarget) -> bool {
    let current = changed_work(app);
    if current.iter().any(|d| !dirty.contains(d)) {
        open_loss_warning(app, target, current);
        return false;
    }
    for d in dirty.iter().filter(|d| d.saveable()) {
        if !validate_dirty(app, d.clone()) {
            focus_dirty(app, d.clone());
            let _ = commit_dirty(app, d.clone());
            return false;
        }
    }
    for d in dirty.iter().filter(|d| d.saveable()) {
        if commit_opens_dialog(app, d.clone()) {
            let _ = commit_dirty(app, d.clone());
            return false;
        }
    }
    for d in dirty.iter().cloned() {
        if d.saveable() {
            match commit_dirty(app, d) {
                CommitResult::Landed => {}
                _ => return false,
            }
        }
    }
    let unsaveable: Vec<_> = dirty.iter().filter(|d| !d.saveable()).cloned().collect();
    if !unsaveable.is_empty() {
        open_loss_warning(app, target, unsaveable);
        return false;
    }
    resolve_target(app, target);
    true
}

/// Whether a changed item's own `↵` path accepts it — the same predicate the
/// `↵` path refuses on, so a `save` can never commit what the field's own ⏎
/// would refuse.
fn validate_dirty(app: &App, dirty: Dirty) -> bool {
    match dirty {
        Dirty::SetupField => setup_field_valid(app),
        Dirty::FallbackField => fallback_field_valid(app),
        Dirty::RefreshInterval => app
            .refresh_interval_draft
            .as_ref()
            .is_some_and(|d| parse_refresh_secs(d.input.trimmed()).is_some()),
        Dirty::ContextNudge => app
            .context_nudge_draft
            .as_ref()
            .is_some_and(|d| parse_context_nudge_tokens(d.input.trimmed()).is_some()),
        Dirty::WeeklyThreshold => app
            .weekly_threshold_draft
            .as_ref()
            .is_some_and(|d| parse_weekly_pct(d.input.trimmed()).is_some()),
        Dirty::HerdrTag => app
            .plugin
            .herdr_tag_draft
            .as_ref()
            .is_some_and(|d| parse_herdr_tag_secs(d.input.trimmed()).is_some()),
        Dirty::Note | Dirty::NewAccountForm => true,
    }
}

/// Whether a changed item's `↵` path opens a further dialog instead of
/// restart. The save's dialog pre-check reads this before anything commits.
fn commit_opens_dialog(app: &App, dirty: Dirty) -> bool {
    match dirty {
        Dirty::SetupField => setup_field_opens_dialog(app),
        _ => false,
    }
}

/// Whether the open Setup field's `↵` path accepts its buffer — the same
/// refusals the field's own ⏎ raises (the bell range, a bad name, an env key
/// with a space or `=`).
fn setup_field_valid(app: &App) -> bool {
    let Some(d) = app.config_draft.as_ref() else {
        return true;
    };
    let Some(field) = d.active else {
        return true;
    };
    match field {
        ConfigRow::BellThreshold => parse_bell_threshold(d.bell_threshold.trimmed()).is_some(),
        ConfigRow::UsageProbeInterval => {
            parse_usage_probe_interval(d.usage_probe_interval.trimmed()).is_some()
        }
        ConfigRow::Name => {
            let old = d.editing_name.clone();
            let new = d.name.trimmed().to_string();
            if let Some(old) = old
                && new != old
            {
                validate_profile_name(&new, Harness::Claude, Some(old.as_str())).is_ok()
            } else {
                true
            }
        }
        ConfigRow::EnvAdd => {
            let key = d.env_new_key.trimmed();
            // An empty key closes the editor (a no-op commit), never refuses.
            !(key.contains(char::is_whitespace) || key.contains('='))
        }
        _ => true,
    }
}

/// Whether the open Setup field's `↵` opens the env-collision dialog.
fn setup_field_opens_dialog(app: &App) -> bool {
    let Some(d) = app.config_draft.as_ref() else {
        return false;
    };
    if d.active != Some(ConfigRow::EnvAdd) {
        return false;
    }
    let Some(name) = d.editing_name.clone() else {
        return false;
    };
    let key = d.env_new_key.trimmed().to_string();
    if key.is_empty() || key.contains(char::is_whitespace) || key.contains('=') {
        return false;
    }
    let base_env_keys = claude_settings_env_keys().unwrap_or_default();
    let cfg = app.config();
    cfg.find(&ProfileName::from(name.as_str()))
        .and_then(|p| classify_env_key(p, &base_env_keys, &key))
        .is_some()
}

/// Whether the Fallback member-card field's `↵` path accepts its buffer.
fn fallback_field_valid(app: &App) -> bool {
    let Some(edit) = app.fallback_edit.as_ref() else {
        return true;
    };
    match &edit.state {
        CardEdit::Threshold(input) => parse_threshold(input.trimmed()).is_some(),
        CardEdit::Weekly(input) => parse_weekly_override(input.trimmed()).is_some(),
        CardEdit::MaxSpend(input) => parse_max_spend(input.trimmed()).is_some(),
        CardEdit::Days(_) | CardEdit::ArmedRemove => true,
    }
}

/// Commit one changed item through its own `↵` path.
fn commit_dirty(app: &mut App, dirty: Dirty) -> CommitResult {
    match dirty {
        Dirty::SetupField => {
            if let Some(field) = app.config_draft.as_ref().and_then(|d| d.active) {
                commit_config_field(app, field)
            } else {
                CommitResult::Refused
            }
        }
        Dirty::FallbackField => CommitResult::from(commit_member_field(app)),
        Dirty::RefreshInterval => CommitResult::from(commit_refresh_interval_edit(app)),
        Dirty::ContextNudge => CommitResult::from(commit_context_nudge_edit(app)),
        Dirty::WeeklyThreshold => CommitResult::from(commit_weekly_threshold_edit(app)),
        Dirty::HerdrTag => CommitResult::from(commit_herdr_tag_edit(app)),
        Dirty::Note => CommitResult::from(commit_note(app)),
        Dirty::NewAccountForm => CommitResult::Landed,
    }
}

/// Land on the first failing item's screen so its invalid-input treatment is
/// visible; the item is the open edit itself, so no reopen is needed.
fn focus_dirty(app: &mut App, dirty: Dirty) {
    let screen = dirty.screen();
    if screen != app.tab {
        do_switch(app, screen);
    }
}

/// Keys on the apply-preset keep / clear confirm: the confirm's grammar over
/// three buttons. `←`/`→` step and stop at either end, `tab` wraps, `↵`
/// chooses, esc/q cancel; `↑`/`↓` and the page keys scroll the body.
fn handle_preset_apply_key(app: &mut App, key: KeyEvent) {
    use PresetApplyButton::{Cancel, Clear, Keep};
    let Some(Modal::PresetApply(form)) = app.modals.last_mut() else {
        return;
    };
    let scroll = &mut form.scroll;
    match key.code {
        KeyCode::Left => {
            form.focus = match form.focus {
                Clear => Keep,
                Keep | Cancel => Cancel,
            };
        }
        KeyCode::Right => {
            form.focus = match form.focus {
                Cancel => Keep,
                Keep | Clear => Clear,
            };
        }
        KeyCode::Tab => {
            form.focus = match form.focus {
                Cancel => Keep,
                Keep => Clear,
                Clear => Cancel,
            };
        }
        KeyCode::Up => {
            scroll.offset = scroll.offset.min(scroll.max_scroll.get()).saturating_sub(1);
        }
        KeyCode::Down => {
            scroll.offset = scroll.offset.saturating_add(1).min(scroll.max_scroll.get());
        }
        KeyCode::PageUp => {
            scroll.offset = page_up_scroll(
                scroll.offset as usize,
                scroll.max_scroll.get() as usize,
                scroll.viewport.get() as usize,
            ) as u16;
        }
        KeyCode::PageDown => {
            scroll.offset = page_down_scroll(
                scroll.offset as usize,
                scroll.max_scroll.get() as usize,
                scroll.viewport.get() as usize,
            ) as u16;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
            undo_preset_picker_open(app);
        }
        KeyCode::Enter => {
            let leftover = match form.focus {
                Cancel => None,
                Keep => Some(crate::presets::Leftover::Keep),
                Clear => Some(crate::presets::Leftover::Clear),
            };
            let (target, preset) = (form.target.clone(), form.preset.clone());
            let (replaces, leaves) = (form.replaces.clone(), form.leaves.clone());
            app.pop_modal();
            if let Some(leftover) = leftover {
                apply_preset_to(app, &target, &preset, leftover, &replaces, &leaves);
            } else {
                undo_preset_picker_open(app);
            }
        }
        _ => {}
    }
}

/// Keys on the re-bind confirm. `↑`/`↓` walk the account toggle lines, `space`
/// flips the one under the cursor, `←`/`→` pick the button, `PageUp`/`PageDown`
/// page the cursor through the published geometry. `⏎` runs the highlighted
/// button; `esc`/`q` are `not now`.
fn handle_rebind_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Rebind(form)) = app.modals.last_mut() else {
        return;
    };
    let total = form.account_count();
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            form.choice = !form.choice;
        }
        KeyCode::Up => {
            if total > 0 {
                form.cursor = (form.cursor + total - 1) % total;
            }
        }
        KeyCode::Down => {
            if total > 0 {
                form.cursor = (form.cursor + 1) % total;
            }
        }
        KeyCode::Char(' ') => {
            if let Some((bi, ai)) = form.account_at(form.cursor) {
                form.checked[bi][ai] = !form.checked[bi][ai];
            }
        }
        KeyCode::PageUp => {
            let next = {
                let starts = form.starts.borrow();
                let block_ends = form.block_ends.borrow();
                page_up_cursor_by_rows(
                    form.cursor,
                    &starts,
                    &block_ends,
                    &[],
                    form.viewport.get(),
                    form.scroll.get(),
                    form.total.get(),
                )
            };
            form.cursor = next;
        }
        KeyCode::PageDown => {
            let next = {
                let starts = form.starts.borrow();
                let block_ends = form.block_ends.borrow();
                page_down_cursor_by_rows(
                    form.cursor,
                    &starts,
                    &block_ends,
                    form.viewport.get(),
                    form.scroll.get(),
                    form.total.get(),
                )
            };
            form.cursor = next;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            let form = form.clone();
            app.pop_modal();
            store_rebind_digest(app, &form.digest);
        }
        KeyCode::Enter => {
            let apply = form.choice;
            let form = form.clone();
            app.pop_modal();
            if apply {
                apply_rebind(app, form);
            } else {
                store_rebind_digest(app, &form.digest);
            }
        }
        _ => {}
    }
}

/// Store the current digest and mirror it into the in-memory state, so a later
/// in-TUI `save_app_state` never writes it back absent. A failed store raises
/// the house `save failed` toast and leaves memory unset — the confirm re-offers
/// next launch.
fn store_rebind_digest(app: &mut App, digest: &str) {
    match crate::actions::store_preset_binds_offered(digest.to_string()) {
        Ok(()) => {
            let mut cfg = app.config();
            cfg.state.preset_binds_offered = Some(digest.to_string());
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
        ),
    }
}

/// Write the checked accounts through the per-account model write path. The
/// digest is stored only when every checked account wrote; a failure is a
/// danger toast naming the account. On a partial failure the moved toast (if
/// any rows moved) precedes the failure toasts, mirroring the CLI's order.
/// `apply` with every account unchecked behaves exactly like `not now`: the
/// digest is stored and nothing is toasted.
fn apply_rebind(app: &mut App, form: RebindForm) {
    let moves = form.checked_moves();
    if moves.is_empty() {
        store_rebind_digest(app, &form.digest);
        return;
    }
    let (rows, moved_accounts, failed) = {
        let mut cfg = app.config();
        let mut rows = 0usize;
        let mut moved_accounts = 0usize;
        let mut failed: Vec<(String, String)> = Vec::new();
        for (name, mvs) in &moves {
            let pname = ProfileName::from(name.clone());
            match crate::actions::apply_rebind_account(&mut cfg, &pname, mvs) {
                Ok(applied) => {
                    rows += applied;
                    if applied > 0 {
                        moved_accounts += 1;
                    }
                }
                Err(e) => {
                    let (applied, err) =
                        match e.downcast::<crate::actions::RebindSavedBeforeFailure>() {
                            Ok(saved) => (saved.applied, saved.error),
                            Err(e) => (0, e),
                        };
                    rows += applied;
                    if applied > 0 {
                        moved_accounts += 1;
                    }
                    failed.push((name.clone(), format!("{err:#}")));
                }
            }
        }
        (rows, moved_accounts, failed)
    };
    if failed.is_empty() {
        store_rebind_digest(app, &form.digest);
        if rows == 0 {
            return;
        }
        app.toast(
            ToastKind::Success,
            format!(
                "moved {} onto the built-in preset models",
                crate::presets::rows_on_accounts(rows, moved_accounts)
            ),
        );
        app.apply_forgets();
    } else {
        // The rows that did move are reported first, then each failure.
        if rows > 0 {
            app.toast(
                ToastKind::Success,
                format!(
                    "moved {} onto the built-in preset models",
                    crate::presets::rows_on_accounts(rows, moved_accounts)
                ),
            );
            app.apply_forgets();
        }
        for (name, err) in failed {
            app.toast(
                ToastKind::Danger,
                format!("re-bind failed for '{name}'\n{err}"),
            );
        }
    }
}

/// Run `crate::herdr::heal` with the knob the state now holds and surface the
/// outcome: success, non-empty refusal notes (warning), or failure (danger).
/// Shared by the `[f]` fix and the `delegate row text` options row — the knob
/// rides the heal the way `install` reads it, so the row written matches the
/// `delegate_row_text` set in the TUI.
fn run_herdr_heal(app: &mut App, path: &std::path::Path) {
    // Guest mode needs no gate: the heal rewrites only tollgate's marked
    // blocks, which `herdr::write_validated` enforces under upstream's lock.
    let delegate_row_text = app.config().state.herdr.delegate_row_text;
    match crate::herdr::heal(
        path,
        crate::herdr::DEFAULT_KEY,
        &crate::herdr::herdr_bin(),
        delegate_row_text,
    ) {
        Ok(notes) if notes.is_empty() => {
            // Name the knob state the write landed: the confirm copy for the
            // off direction says "drop the delegate token", so a success toast
            // reading "added the keybinding and sidebar row" would claim the
            // opposite of what the user confirmed.
            app.toast(
                ToastKind::Success,
                format!(
                    "wrote herdr's config: keybinding + sidebar row (delegate token {})",
                    if delegate_row_text { "on" } else { "off" }
                ),
            );
            recompute_plugin_checks(app, false);
        }
        Ok(notes) => {
            // Non-empty notes = pieces tollgate refused to touch (a table it
            // cannot extend by appending). Reporting success over those is a
            // lie, so the notes surface verbatim.
            app.toast(
                ToastKind::Warning,
                format!("herdr's config needs attention\n{}", notes.join("\n")),
            );
            recompute_plugin_checks(app, false);
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "herdr config fix failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

fn run_confirm_action(app: &mut App, action: ConfirmAction) {
    match action {
        ConfirmAction::RelinkCredentials(name) => {
            if guest_refused(app) {
                return;
            }
            let name = ProfileName::from(name);
            match force_link_profile_credentials(&name) {
                Ok(()) => {
                    app.refresh_tokens();
                    app.refresh_unsaved_live_login();
                    app.toast(
                        ToastKind::Success,
                        format!("relinked credentials to '{name}'"),
                    );
                    recompute_plugin_checks(app, false);
                }
                Err(e) => app.toast(ToastKind::Danger, format!("relink failed\n{e}")),
            }
        }

        ConfirmAction::CaptureConflict(snapshot, from_divergence) => {
            app.open_modal(Modal::CaptureName(CaptureNameForm {
                snapshot,
                input: InputState::new(""),
                from_divergence,
            }));
        }
        ConfirmAction::CaptureOverwrite(snapshot, name, account, from_divergence) => {
            if guest_refused(app) {
                return;
            }
            // The overwrite relinks the live slot when the profile is or
            // becomes the active one, a divergence's capture always (it
            // deactivates the prior one first). Settled before that detach,
            // so a held overwrite has changed nothing.
            let relinks = from_divergence || {
                let cfg = app.config();
                cfg.is_active(&ProfileName::from(name.as_str()))
                    || cfg.state.active_profile.is_none()
            };
            let _placed = if relinks {
                let Some(placed) = bare_item_gate(
                    app,
                    BareItemWrite::CaptureOverwrite {
                        snapshot: snapshot.clone(),
                        name: name.clone(),
                        account: account.clone(),
                        from_divergence,
                    },
                ) else {
                    return;
                };
                Some(placed)
            } else {
                None
            };
            // Same deferred detach as the non-colliding path: only disown the
            // prior active profile once the overwrite is actually confirmed,
            // so a cancel (handled entirely by `handle_confirm_key` popping
            // without calling this) leaves it untouched. The writer disowns it
            // after its id check, so a refused overwrite leaves it untouched too.
            let name = ProfileName::from(name);
            let result = {
                let mut cfg = app.config();
                crate::actions::overwrite_captured_profile_with(
                    &mut cfg,
                    &name,
                    &account,
                    *snapshot,
                    from_divergence,
                )
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.apply_forgets();
                    app.refresh_unsaved_live_login();
                    app.toast(
                        ToastKind::Success,
                        format!("overwrote '{name}' with the captured login"),
                    );
                }
                Err(e) => {
                    app.toast(
                        ToastKind::Danger,
                        format!(
                            "overwrite failed\n{}",
                            escape_control(&error_toast_detail(&e))
                        ),
                    );
                }
            }
        }
        ConfirmAction::AdoptDivergence(snapshot, name, account) => {
            if guest_refused(app) {
                return;
            }
            // `name` is never the active profile (the picker lists only others),
            // so `overwrite_captured_profile` just rewrites its stored creds and
            // skips its own guarded relink. Then force the live link onto it and
            // make it active — the divergence is resolved onto the target.
            // That relink replaces the bare item, so it settles first.
            let Some(_placed) = bare_item_gate(
                app,
                BareItemWrite::AdoptDivergence {
                    snapshot: snapshot.clone(),
                    name: name.clone(),
                    account: account.clone(),
                },
            ) else {
                return;
            };
            let name = ProfileName::from(name);
            // Captured before the activation replaces the marker, through the
            // one helper that also answers for a CLEARED marker — this arm can
            // run after a `switch_off` left the departed account's `[env]` in
            // the live file.
            let outgoing_env_keys = crate::actions::outgoing_env_keys(&app.config());
            let result = {
                let mut cfg = app.config();
                overwrite_captured_profile(&mut cfg, &name, &account, *snapshot).and_then(|()| {
                    with_state_lock(|held| {
                        cfg.state.set_active(Some(name.clone()), held);
                        save_app_state(&cfg.state)
                    })
                })
            };
            let result = result
                .and_then(|()| force_link_profile_credentials(&name))
                .and_then(|()| {
                    // This arm activates OUTSIDE `overwrite_captured_profile`,
                    // so the settings write its own activate arms make has to
                    // happen here too: an endpoint that never reaches
                    // `settings.json` routes the live session at Anthropic
                    // under a profile that says otherwise.
                    let cfg = app.config();
                    let Some(profile) = cfg.find(&name) else {
                        return Ok(());
                    };
                    crate::claude::apply_profile_to_claude_settings(profile, &outgoing_env_keys)
                });
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    app.apply_forgets();
                    app.refresh_unsaved_live_login();
                    app.toast(
                        ToastKind::Success,
                        format!("saved the login into '{name}', now active"),
                    );
                }
                Err(e) => app.toast(
                    ToastKind::Danger,
                    format!("save failed\n{}", escape_control(&error_toast_detail(&e))),
                ),
            }
        }
        ConfirmAction::Switch(name) => {
            let name = ProfileName::from(name);
            if switch_gate_in_flight(&app.activity) {
                app.toast(
                    ToastKind::Warning,
                    "another switch is still in flight\ntry again in a moment",
                );
                return;
            }
            if !is_idle(&app.activity, &name) {
                app.toast(
                    ToastKind::Warning,
                    format!("'{name}' is already busy\ntry again in a moment"),
                );
                return;
            }
            perform_switch(app, &name);
        }
        ConfirmAction::DiscardDivergence(name) => run_discard_divergence(app, &name),
        ConfirmAction::RotateAll => {
            if guest_refused(app) {
                return;
            }
            // Refuse if anything is in-flight. Bootstrap is a whole-worker
            // signal separate from per-profile activity; the flag covers the
            // gap between the last Refreshing slot clearing and the next fetch.
            if app.bootstrap_active.load(Ordering::SeqCst) || any_busy(&app.activity) {
                app.toast(
                    ToastKind::Warning,
                    "rotate-all skipped\nanother op is still in flight",
                );
                return;
            }
            // Stamp every candidate `Refreshing` on the UI thread BEFORE the
            // worker spawns, so a quit decided before the worker runs still
            // sees the spend and enters the exit wait. The worker's workers
            // clear their own slot (via `rotate_one_inner`), so the UI thread
            // is not an owner of any slot it raised.
            let snapshots = {
                let cfg = App::lock_config(&app.config);
                oauth::rotation_candidates(&cfg, true)
            };
            for (name, _) in &snapshots {
                mark_activity(&app.activity, name, ProfileActivity::Refreshing);
            }
            let config = Arc::clone(&app.config);
            let refetch = Arc::clone(&app.refetch_queue);
            let activity = Arc::clone(&app.activity);
            let shutting_down = Arc::clone(&app.shutting_down);
            let sender = app.op_sender.clone();
            spawn_worker(move || {
                #[cfg(test)]
                rotate_worker_gate();
                let _ = oauth::refresh_all(
                    &config,
                    snapshots,
                    &refetch,
                    &activity,
                    Some(shutting_down),
                    &sender,
                );
            });
            app.toast(ToastKind::Info, "rotating all tokens");
        }
        ConfirmAction::RotateOne(name) => {
            if guest_refused(app) {
                return;
            }
            // Backstop for a session that started between arming the confirm and
            // running it: on macOS `rotate_one_inner` refuses, and an unguarded
            // "rotating 'X'" toast would be a success-shaped message for a no-op.
            let name = ProfileName::from(name);
            if let Some(block) = crate::runtime::rotation_block_for(&name) {
                app.toast(
                    ToastKind::Warning,
                    format!(
                        "{}\n{ROTATE_LIVE_SESSION_TOAST}",
                        rotate_blocked_message(name.as_str(), block)
                    ),
                );
                return;
            }
            // The per-profile RotationGuard inside rotate_one serialises against a
            // live session's own refresh, so unlike RotateAll this doesn't need the
            // global any_busy gate — an unacquirable guard surfaces as a Danger
            // toast (contention blocks inside `acquire`, it never lands there).
            //
            // Stamp on the UI thread BEFORE the worker spawns so a quit decided
            // before the worker runs still sees this spend. Residual (accepted):
            // the marker now spans the worker's blocking `RotationGuard::acquire`
            // wait, so a quit over a rotate parked behind a foreign holder waits
            // it out; ⌃c ⌃c still exits.
            mark_activity(&app.activity, &name, ProfileActivity::Refreshing);
            let config = Arc::clone(&app.config);
            let refetch = Arc::clone(&app.refetch_queue);
            let activity = Arc::clone(&app.activity);
            let shutting_down = Arc::clone(&app.shutting_down);
            let sender = app.op_sender.clone();
            let target = name.clone();
            spawn_worker(move || {
                #[cfg(test)]
                rotate_worker_gate();
                oauth::rotate_one(
                    &config,
                    &target,
                    &refetch,
                    &activity,
                    Some(shutting_down.as_ref()),
                    &sender,
                );
            });
            app.toast(ToastKind::Info, format!("rotating '{name}'"));
        }
        // Re-checked inside `toggle_profile_disabled`: an account can go active
        // or open a session between arming this confirm and running it.
        ConfirmAction::DisableOne(name) => {
            toggle_profile_disabled(app, &ProfileName::from(name), DisableDirection::Disable);
        }
        ConfirmAction::WireMcpServers => {
            match crate::plugin_probe::wire_mcp_server() {
                Ok(()) => {
                    app.toast(ToastKind::Success, "wired tollgate into ~/.claude.json");
                    // Reflect the new wiring in the rows without a fresh version probe.
                    recompute_plugin_checks(app, false);
                }
                Err(e) => {
                    app.toast(
                        ToastKind::Danger,
                        format!("wire failed\n{}", escape_control(&error_toast_detail(&e))),
                    );
                }
            }
        }
        ConfirmAction::HealHerdrConfig(path) => run_herdr_heal(app, &path),
        ConfirmAction::HerdrDelegateRowText(path) => {
            // Persist the flipped knob FIRST: `run_herdr_heal` reads it back off
            // the state (the way `install` reads it), so heal writes the row the
            // user just asked for. Cancel never reaches here — the modal pops
            // without running the action.
            {
                let mut cfg = app.config();
                cfg.state.herdr.delegate_row_text = !cfg.state.herdr.delegate_row_text;
                let _ = save_app_state(&cfg.state);
            }
            app.apply_forgets();
            run_herdr_heal(app, &path);
        }
        // Asked at the install itself, never off a cached reading: the
        // install registers the plugin tree this process's image carries.
        ConfirmAction::InstallPlugin => match crate::plugin_host::self_exe_change() {
            Some(change) => app.toast(ToastKind::Warning, own_binary_install_refusal(change)),
            None => match crate::plugin_host::install() {
                // A no-op from a row that read "not installed" means the
                // backend never ran — `claude` not on PATH is the reachable
                // case (agentgear skips an undetected backend). A green success
                // toast over a skipped install is a lie, so the warning names
                // it instead.
                Ok(agentgear::Outcome::NoOp) => {
                    app.toast(
                        ToastKind::Warning,
                        "plugin install made no changes\ninstall claude code first, then try again",
                    );
                    recompute_plugin_checks(app, false);
                }
                // Installed / Repaired / Adopted / Updated, or anything
                // agentgear adds later: a real change happened, so the success
                // toast is earned. (`Outcome` is #[non_exhaustive], so the
                // wildcard is the contract, not a shortcut.)
                Ok(outcome) => {
                    app.toast(ToastKind::Success, format!("tollgate plugin {outcome}"));
                    // Reflect the fresh install in the rows without a version
                    // probe.
                    recompute_plugin_checks(app, false);
                }
                Err(e) => app.toast(
                    ToastKind::Danger,
                    diagnostic_toast_body("install failed", &error_toast_detail(&e)),
                ),
            },
        },
        // Asked at the fix itself, never off a cached reading: a replaced image
        // would write its old emission, not what the installed binary prints.
        ConfirmAction::Completions { shell, stale } => {
            match crate::plugin_host::self_exe_change() {
                Some(change) => app.toast(
                    ToastKind::Warning,
                    own_binary_completions_refusal(change, stale),
                ),
                None => match crate::completions::install(Some(&shell)) {
                    Ok(()) => {
                        app.toast(ToastKind::Success, format!("wrote {shell} completions"));
                        // The card's own re-probe: the script on disk just changed.
                        recompute_plugin_checks(app, false);
                    }
                    Err(e) => app.toast(
                        ToastKind::Danger,
                        format!(
                            "{} failed\n{}",
                            if stale {
                                "update completions"
                            } else {
                                "install completions"
                            },
                            escape_control(&error_toast_detail(&e)),
                        ),
                    ),
                },
            }
        }
        ConfirmAction::BlankCredentials(name) => {
            let name = ProfileName::from(name);
            // Logging the active profile out signs the live slot out.
            let _placed = if app.config().is_active(&name) {
                let Some(placed) =
                    bare_item_gate(app, BareItemWrite::BlankCredentials(name.to_string()))
                else {
                    return;
                };
                Some(placed)
            } else {
                None
            };
            let result = {
                let mut cfg = app.config();
                // OAuth accounts drop the whole login; API accounts drop only
                // the api key, keeping the base-url shell.
                let is_oauth = cfg.find(&name).map(|p| p.login_is_oauth()).unwrap_or(true);
                if is_oauth {
                    clear_profile_credentials(&mut cfg, &name)
                } else {
                    clear_profile_api_key(&mut cfg, &name)
                }
            };
            match result {
                Ok(()) => {
                    app.refresh_tokens();
                    // No leg ever fetches a keyless profile, so a stale
                    // `[ key rejected ]` verdict would render forever — the
                    // drain forgets the changed provider half with the
                    // credential.
                    app.apply_forgets();
                    app.refresh_unsaved_live_login();
                    app.toast(ToastKind::Success, format!("logged out of '{name}'"));
                }
                Err(e) => {
                    app.toast(
                        ToastKind::Danger,
                        format!(
                            "log out failed\n{}",
                            escape_control(&error_toast_detail(&e))
                        ),
                    );
                }
            }
        }
        ConfirmAction::RestartLogin(name, is_new) => start_login(app, name, is_new),
        ConfirmAction::CaptureOverMintStash(snapshot) => {
            if matches!(
                stash_new_form_login(app, DraftLogin::LiveLogin(snapshot)),
                StashOutcome::Staged
            ) {
                app.toast(
                    ToastKind::Success,
                    "current login captured\ncreate account saves it",
                );
            }
        }
        ConfirmAction::CycleAccountType(next) => {
            cancel_staged_new_login(app);
            set_account_type(app, next);
        }
        ConfirmAction::DeleteLiveSession(name) => {
            finish_delete(app, &ProfileName::from(name), true)
        }
        ConfirmAction::AddChainCandidate(name) => commit_chain_add(app, &ProfileName::from(name)),
        ConfirmAction::OverwritePreset(preset, source) => save_preset_from(app, &source, &preset),
        ConfirmAction::ApplyPresetOver(target, preset, replaces) => {
            apply_preset_to(
                app,
                &target,
                &preset,
                crate::presets::Leftover::Keep,
                &replaces,
                &[],
            );
        }
        ConfirmAction::DeletePreset(preset) => {
            // A built-in has no file to drop; `delete_preset` by its name would
            // reach a saved file spelled the same.
            if preset.builtin {
                toast_builtin_preset(app, &preset.name);
                return;
            }
            // Re-read the saved file, like `apply_preset_to`: one that vanished
            // since the confirm opened toasts the same gone message and deletes
            // nothing.
            let Some(loaded) = crate::presets::load_saved_preset(&preset.name) else {
                app.toast(
                    ToastKind::Danger,
                    format!("preset '{}' is gone", preset.name),
                );
                return;
            };
            match crate::presets::delete_preset(&loaded.name) {
                Ok(()) => app.toast(
                    ToastKind::Success,
                    format!("deleted preset '{}'", loaded.name),
                ),
                Err(e) => app.toast(
                    ToastKind::Danger,
                    format!(
                        "delete preset failed\n{}",
                        escape_control(&error_toast_detail(&e))
                    ),
                ),
            }
        }
        ConfirmAction::StartDaemon => start_daemon(app),

        ConfirmAction::Acknowledge => {}
    }
}

fn handle_divergence_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::Divergence(state)) = app.modals.last_mut() else {
        return;
    };
    let actions = state.actions();
    let last = actions.len() - 1;
    match key.code {
        KeyCode::Up => {
            state.cursor = if state.cursor == 0 {
                last
            } else {
                state.cursor - 1
            };
        }
        KeyCode::Down => {
            state.cursor = if state.cursor >= last {
                0
            } else {
                state.cursor + 1
            };
        }
        KeyCode::PageUp => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                &[],
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
        }
        KeyCode::PageDown => {
            let starts = state.starts.borrow();
            let block_ends = state.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                state.cursor,
                &starts,
                &block_ends,
                state.viewport.get(),
                state.scroll.get(),
                state.total.get(),
            );
            drop(starts);
            drop(block_ends);
            state.cursor = next;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            // Just close — the prompt is on-demand now; the non-blocking
            // banner keeps carrying the signal until the divergence resolves.
            app.pop_modal();
        }
        KeyCode::Enter => {
            let action = actions[state.cursor.min(last)].clone();
            let active = state.active.clone();
            app.pop_modal();
            match action {
                DivergenceAction::SwitchToOwner(owner) => {
                    // Same confirmed path as the "save elsewhere" picker: the
                    // live login is captured into its own profile, which then
                    // becomes active. Nothing is lost, nothing else rewritten.
                    let Some(snapshot) = capture_live_or_toast(app) else {
                        return;
                    };
                    let Some(account) = claim_or_toast(app, &owner, "save failed") else {
                        return;
                    };
                    app.open_modal(Modal::Confirm(ConfirmState {
                        message: format!("switch to '{owner}'? the live login is its account."),
                        detail: Some(format!(
                            "the login is saved into '{owner}' and '{owner}' becomes the active \
                             account. the running claude is untouched."
                        )),
                        choice: false,
                        scroll: ConfirmScroll::default(),
                        on_confirm: ConfirmAction::AdoptDivergence(
                            Box::new(snapshot),
                            owner,
                            account,
                        ),
                    }));
                }
                DivergenceAction::Choice(choice) => run_divergence_choice(app, &active, choice),
            }
        }
        _ => {}
    }
}

fn run_divergence_choice(app: &mut App, active: &str, choice: DivergenceChoice) {
    if guest_refused(app) {
        return;
    }

    match choice {
        DivergenceChoice::Overwrite => {
            // The relink below replaces the bare item: settled before the
            // snapshot, so a held overwrite has changed nothing.
            let Some(_placed) =
                bare_item_gate(app, BareItemWrite::DivergenceOverwrite(active.to_string()))
            else {
                return;
            };
            let oauth_moved = {
                let mut cfg = app.config();
                force_snapshot_active_credentials(&mut cfg)
            };
            let oauth_moved = match oauth_moved {
                Ok(oauth_moved) => oauth_moved,
                Err(e) => {
                    app.toast(
                        ToastKind::Danger,
                        format!(
                            "overwrite failed\n{}",
                            escape_control(&error_toast_detail(&e))
                        ),
                    );
                    return;
                }
            };
            if oauth_moved {
                // A force snapshot removes the anchor, so an anchored profile
                // forgets once and its `/profile` clock expires; only an
                // unanchored snapshot of the same token forgets nothing.
                crate::usage::expire_profile_ttl(&ProfileName::from(active));
            }
            if let Err(e) = force_link_profile_credentials(&ProfileName::from(active)) {
                app.toast(
                    ToastKind::Danger,
                    format!("relink failed\n{}", escape_control(&error_toast_detail(&e))),
                );
                return;
            }
            app.refresh_tokens();
            // The live login replaced the stored credential, so the old
            // credential's verdicts must not survive it — the drain forgets the
            // halves whose identity changed, not a wait for the next OAuth fetch.
            app.apply_forgets();
            // AUTH-1: the fresh login is the documented recovery for a revoked
            // chain, so a standing auth_broken quarantine is stale — same lift
            // every sibling fresh-login path runs.
            oauth::mark_auth_broken(&app.config, &ProfileName::from(active), false);
            app.toast(
                ToastKind::Success,
                format!("saved live credentials into '{active}'"),
            );
        }
        DivergenceChoice::NewProfile => open_divergence_target_picker(app),
        DivergenceChoice::Discard => {
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("discard the new login and restore '{active}'?"),
                detail: Some(
                    "claude code's freshly written credentials will be overwritten with the account's stored tokens.".to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::DiscardDivergence(active.to_string()),
            }));
        }
    }
}

fn run_discard_divergence(app: &mut App, active: &str) {
    if guest_refused(app) {
        return;
    }

    // Discarding drops the live file's login, never a Keychain login no store
    // saved: the settle saves that into its owner first.
    let Some(_placed) = bare_item_gate(app, BareItemWrite::DiscardDivergence(active.to_string()))
    else {
        return;
    };
    if let Err(e) = force_link_profile_credentials(&ProfileName::from(active)) {
        app.toast(
            ToastKind::Danger,
            format!(
                "discard failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        );
        return;
    }
    app.toast(
        ToastKind::Warning,
        format!("discarded new login\nrestored '{active}'"),
    );
}

/// Open the "save elsewhere" picker from the Divergence modal. Lists every
/// profile except the active (diverged) one; a refresh-token match is
/// pre-selected. With no other profile to pick, drops straight into a
/// new-profile capture.
fn open_divergence_target_picker(app: &mut App) {
    let (targets, preselect) = {
        let cfg = app.config();
        let active = cfg.state.active_profile.as_deref();
        let targets: Vec<String> = cfg
            .profiles
            .iter()
            .map(|p| p.name.as_str().to_string())
            .filter(|n| Some(n.as_str()) != active)
            .collect();
        let live = read_claude_credentials().ok().flatten();
        let preselect = find_matching_oauth_profile(&cfg, live.as_ref())
            .and_then(|m| targets.iter().position(|n| n.as_str() == m.as_str()))
            .map_or(0, |i| i + 1);
        (targets, preselect)
    };
    if targets.is_empty() {
        begin_capture(app, true);
        return;
    }
    app.open_modal(Modal::DivergenceTarget(DivergenceTargetForm {
        targets,
        cursor: preselect,
        scroll: std::cell::Cell::new(0),
        viewport: std::cell::Cell::new(0),
        starts: std::cell::RefCell::new(Vec::new()),
        block_ends: std::cell::RefCell::new(Vec::new()),
        total: std::cell::Cell::new(0),
    }));
}

/// Rows: 0 = `+ new profile` (→ capture-name), 1.. = existing profiles
/// (→ overwrite-confirm on that profile). Both routes carry `from_divergence`,
/// so the prior active is detached only once the action is confirmed and a
/// cancel leaves everything as-is.
fn handle_divergence_target_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::DivergenceTarget(form)) = app.modals.last_mut() else {
        return;
    };
    let last = form.targets.len(); // row 0 is "+ new"; rows 1..=len are profiles
    match key.code {
        KeyCode::Up => {
            form.cursor = if form.cursor == 0 {
                last
            } else {
                form.cursor - 1
            };
        }
        KeyCode::Down => {
            form.cursor = if form.cursor >= last {
                0
            } else {
                form.cursor + 1
            };
        }
        KeyCode::PageUp => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_up_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                &[],
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
        }
        KeyCode::PageDown => {
            let starts = form.starts.borrow();
            let block_ends = form.block_ends.borrow();
            let next = page_down_cursor_by_rows(
                form.cursor,
                &starts,
                &block_ends,
                form.viewport.get(),
                form.scroll.get(),
                form.total.get(),
            );
            drop(starts);
            drop(block_ends);
            form.cursor = next;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
        }
        KeyCode::Enter => {
            let cursor = form.cursor;
            let Some(Modal::DivergenceTarget(form)) = app.pop_modal() else {
                return;
            };
            if cursor == 0 {
                begin_capture(app, true);
                return;
            }
            let Some(target) = form.targets.get(cursor - 1).cloned() else {
                return;
            };
            let Some(snapshot) = capture_live_or_toast(app) else {
                return;
            };
            let Some(account) = claim_or_toast(app, &target, "save failed") else {
                return;
            };
            // Names no surviving endpoint, unlike the browser-login confirm:
            // this snapshot is a LIVE read (`capture_snapshot` reads the live
            // settings' endpoint), so what it carries describes the OUTGOING
            // active profile, not this target. An endpoint-carrying active
            // profile fills both fields and the target's pair is replaced by
            // it; an OAuth-active one fills neither and the preserve arm keeps
            // the target's. The outcome turns on an account this prompt is not
            // about, so it promises nothing about either field.
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("save the live login into '{target}'?"),
                detail: Some(format!(
                    "'{target}' becomes the active account; its old credentials are replaced. usage history, env, and model settings are kept."
                )),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::AdoptDivergence(Box::new(snapshot), target, account),
            }));
        }
        _ => {}
    }
}

fn handle_capture_name_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::CaptureName(form)) = app.modals.last_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pop_modal();
        }
        KeyCode::Enter => {
            let name = form.input.trimmed().to_string();
            // Chars + cross-harness only — the own-roster duplicate branch of
            // `validate_profile_name` is deliberately skipped so a claude
            // collision falls through to the canonical_name lookup below
            // (capture-into-existing) instead of dead-ending with an "already
            // exists" error. A codex-held name has no such flow — a claude
            // login can't be captured into a codex profile — so that half
            // still refuses here.
            let validation = validate_name_chars(&name)
                .map(|_| ())
                .and_then(|()| validate_foreign_harness_free(&name, Harness::Claude));
            if let Err(e) = validation {
                app.toast(ToastKind::Danger, escape_control(&error_toast_detail(&e)));
                return;
            }
            let collision = {
                let cfg = app.config();
                cfg.canonical_name(&name)
            };
            let Some(Modal::CaptureName(form)) = app.pop_modal() else {
                return;
            };
            let CaptureNameForm {
                snapshot,
                from_divergence,
                ..
            } = form;
            if let Some(existing) = collision {
                let Some(account) = claim_or_toast(app, &existing, "overwrite failed") else {
                    return;
                };
                // Issue #7: typing an existing profile's name used to dead-end
                // with an error. Route to the same confirm-modal machinery as
                // every other destructive action instead of a picker/new modal.
                app.open_modal(Modal::Confirm(ConfirmState {
                    message: format!("account '{existing}' already exists."),
                    detail: Some(
                        "overwrite its credentials with the captured login? usage history, env, and model settings are kept.".to_string(),
                    ),
                    choice: false,
                    scroll: ConfirmScroll::default(),
                    on_confirm: ConfirmAction::CaptureOverwrite(
                        snapshot,
                        existing,
                        account,
                        from_divergence,
                    ),
                }));
                return;
            }
            commit_capture_name(app, name, snapshot, from_divergence);
        }
        _ => apply_input_edit(&mut form.input, key),
    }
}

/// Capture `snapshot` into a new profile `name`, the capture prompt's ⏎ once
/// no profile holds the name.
fn commit_capture_name(
    app: &mut App,
    name: String,
    snapshot: Box<CaptureSnapshot>,
    from_divergence: bool,
) {
    if guest_refused(app) {
        return;
    }

    // The new profile becomes the active one, linking the live slot over the
    // bare item, when none is active or a divergence deactivates the prior
    // one. Settled before that detach, so a held capture has changed nothing.
    let relinks = from_divergence || app.config().state.active_profile.is_none();
    let _placed = if relinks {
        let Some(placed) = bare_item_gate(
            app,
            BareItemWrite::CaptureName {
                name: name.clone(),
                snapshot: snapshot.clone(),
                from_divergence,
            },
        ) else {
            return;
        };
        Some(placed)
    } else {
        None
    };
    // Divergence capture: detach + deactivate only after name is
    // confirmed so `capture_into_profile` sees `active_profile.is_none()`
    // and links the new one. On Esc this never runs. One hold, the name
    // check first: a name taken meanwhile refuses with nothing detached.
    let result = {
        let mut cfg = app.config();
        with_state_lock(|held| {
            validate_profile_name(&name, Harness::Claude, None)?;
            if from_divergence {
                let _ = detach_credentials_link();
                cfg.state.set_active(None, held);
                let _ = save_app_state(&cfg.state);
            }
            capture_into_profile(&mut cfg, name.clone(), Default::default(), *snapshot)
        })
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.apply_forgets();
            app.refresh_unsaved_live_login();
            app.toast(ToastKind::Success, format!("captured '{name}'"));
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "capture failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

fn apply_input_edit(input: &mut InputState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => input.delete_word(),
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => input.insert(c),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        _ => {}
    }
}

/// Apply queued status-feed events to `app.status`. Surfaces a new-incident cue
/// (toast on the Status tab, tab-activity color elsewhere) and a manual-refresh
/// failure toast. Manual refresh always clears `fetching`.
fn drain_status_events(app: &mut App) {
    while let Ok(ev) = app.status_events.try_recv() {
        match ev {
            StatusEvent::Fetched {
                incidents,
                fetched_at_ms,
            } => {
                let was_manual = app.status.fetching;
                apply_status_incidents(app, incidents, fetched_at_ms, false, was_manual);
            }
            StatusEvent::Cached {
                incidents,
                fetched_at_ms,
            } => {
                // A cache fallback after a manual refresh means the live fetch
                // failed — surface that as a danger toast.
                let was_manual = app.status.fetching;
                apply_status_incidents(app, incidents, fetched_at_ms, true, false);
                if was_manual {
                    app.status.fetching = false;
                    app.toast(ToastKind::Danger, "status refresh failed\nshowing cached");
                }
            }
            StatusEvent::Failed(msg) => {
                let was_manual = app.status.fetching;
                app.status.fetching = false;
                // Keep an error only while nothing is loaded; avoid toast spam on
                // the steady-state retry loop. A manual failure does toast.
                if app.status.incidents.is_empty() {
                    app.status.error = Some(msg);
                }
                if was_manual {
                    app.toast(ToastKind::Danger, "status refresh failed");
                }
            }
        }
    }
}

/// Replace the incident list, clear the spinner, clamp the cursor, and run the
/// new-incident signal. `cached` marks the data as a cache load / fallback.
/// `manual` is true when this `Fetched` answers a manual refresh (toasts there).
fn apply_status_incidents(
    app: &mut App,
    mut incidents: Vec<Incident>,
    fetched_at_ms: u64,
    cached: bool,
    manual: bool,
) {
    let prev_selected_id = app.status.selected().map(|i| i.id.clone());

    app.status.fetching = false;
    app.status.cached = cached;
    app.status.fetched_at_ms = Some(fetched_at_ms);
    if !cached {
        app.status.error = None;
    }

    let newest_id = incidents.first().map(|i| i.id.clone());
    if let Some(newest) = &newest_id
        && app.status.seen_latest.as_ref() != Some(newest)
    {
        let is_initial = app.status.seen_latest.is_none();
        // Only signal genuinely new incidents, never the initial load.
        if !is_initial && let Some(incident) = incidents.first() {
            let severity = if incident_is_active(incident) {
                ToastKind::Warning
            } else {
                ToastKind::Info
            };
            if app.tab == Tab::Status {
                let title = truncate(&incident.title, 40);
                app.toast(
                    severity,
                    format!("new incident: {}", escape_control(&title)),
                );
            } else {
                app.set_tab_activity(Tab::Status, severity);
            }
        }
        app.status.seen_latest = newest_id.clone();
    }
    let _ = manual;

    incidents.sort_by(|a, b| match (a.is_active(), b.is_active()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => b.started_ms.cmp(&a.started_ms),
    });
    app.status.incidents = incidents;

    if app.status.incidents.is_empty() {
        app.status.cursor = 0;
    } else if app.status.cursor >= app.status.incidents.len() {
        app.status.cursor = app.status.incidents.len() - 1;
    }
    if app.status.selected().map(|i| i.id.clone()) != prev_selected_id {
        app.status.detail_scroll = 0;
    }
}

/// Apply queued token-stats loads. A `Failed` keeps the last good snapshot so a
/// transient parse miss never blanks the tab.
fn drain_tokens_events(app: &mut App) {
    while let Ok(ev) = app.tokens_events.try_recv() {
        match ev {
            // Base seeds the first paint only; on a refresh the prior topped-up
            // snapshot stays visible until the new Loaded lands, so the cadence
            // never flickers the tab back to base-only data.
            crate::tokens::TokensEvent::Base(stats) => {
                app.tokens_failed = false;
                if app.token_stats.is_none() {
                    app.token_stats = Some(*stats);
                    // First paint from the cache: the transcript top-up is now in
                    // flight, so light the loading spinners until `Loaded` lands.
                    app.tokens_topping_up = true;
                }
            }
            crate::tokens::TokensEvent::Progress { done, total } => {
                app.tokens_progress = Some((done, total));
            }
            crate::tokens::TokensEvent::Loaded(stats) => {
                app.tokens_failed = false;
                app.tokens_topping_up = false;
                app.tokens_progress = None;
                app.token_stats = Some(*stats);
                // Re-clamp after the loader replaces the grouped list.
                clamp_token_model_cursor(app);
            }
            // Surface failure only when there is nothing to show — a transient
            // read error mid-session keeps the last good snapshot.
            crate::tokens::TokensEvent::Failed => {
                app.tokens_topping_up = false;
                app.tokens_progress = None;
                if app.token_stats.is_none() {
                    app.tokens_failed = true;
                }
            }
        }
    }
}

fn drain_pricing_events(app: &mut App) {
    while let Ok(ev) = app.pricing_events.try_recv() {
        match ev {
            crate::pricing::PricingEvent::Loaded(table) => {
                app.price_table = Some(*table);
                app.price_failed = false;
            }
            // Surface failure only when there is nothing to show — a transient
            // fetch error mid-session keeps the last good table.
            crate::pricing::PricingEvent::Failed => {
                if app.price_table.is_none() {
                    app.price_failed = true;
                }
            }
        }
    }
}

/// Drain the login worker: track URL/stage events (the door rides the first
/// stage bump, and only the worker's report may set it — a paste sent a moment
/// after the browser callback landed lost), and on a result apply it (stash or
/// overwrite) — discarding a stale result from a superseded login. A stage
/// bump that closes the paste door closes the code field with it: a code typed
/// after the browser callback won has no door to reach.
fn drain_login_events(app: &mut App) {
    while let Ok((generation, event)) = app.login_event_rx.try_recv() {
        if let Some(session) = app.login.as_mut()
            && session.generation == generation
        {
            match event {
                LoginEvent::Url(url) => session.url = Some(url),
                LoginEvent::Stage(stage) => {
                    if let LoginStage::ExchangingCode(door) = stage {
                        session.method = door;
                    }
                    session.stage = stage;
                    if session.open_door().is_none() {
                        session.paste_field = None;
                    }
                }
            }
        }
    }
    while let Ok((generation, result)) = app.login_result_rx.try_recv() {
        if app.login.as_ref().map(|s| s.generation) != Some(generation) {
            continue; // superseded login; drop it
        }
        let Some(session) = app.login.take() else {
            continue;
        };
        close_login_modal(app);
        match result {
            Ok(LoginResult::Oauth(creds)) => apply_login(app, session, *creds),
            Ok(LoginResult::Console(outcome)) => apply_console_login(app, session, *outcome),
            Err(e) => app.toast(
                ToastKind::Danger,
                format!("login for '{}' failed\n{e}", session.name),
            ),
        }
    }
}

/// Fold a completed login into the app on the UI thread. A new-account login
/// stashes the mint into the live `+ new` draft — the profile is created only
/// when `create account` fires (capture-then-commit). A re-login lands only
/// on the account it started for (the write refuses any other) and, since
/// fresh creds replacing stored ones IS a divergence, gates the overwrite on
/// `AppState.default_divergence`: only an `Overwrite` default applies silently;
/// anything else asks first.
fn apply_login(app: &mut App, session: LoginSession, outcome: crate::oauth_login::LoginOutcome) {
    if guest_refused(app) {
        return;
    }
    let email = outcome
        .identity
        .as_ref()
        .and_then(|id| id.email.as_deref())
        .and_then(crate::profile_cache::sanitize_identity_detail)
        .map(|email| escape_control(&email));
    let duplicate_warning = outcome.identity.as_ref().and_then(|identity| {
        crate::profile_cache::duplicate_login_warning_for_account(
            &app.config(),
            &ProfileName::from(session.name.as_str()),
            &identity.account_uuid,
        )
    });
    if let Some(warning) = duplicate_warning {
        app.toast(ToastKind::Warning, warning);
    }
    let Some(account) = session.account else {
        // The `+ new` form may have been closed during the browser round-trip;
        // the mint lives only in the draft, so without one it is dropped. The
        // anchor waits for `create account`: the draft's name is still editable,
        // so the profile this login belongs to has no final name yet.
        match stash_new_form_login(app, DraftLogin::Mint(Box::new(outcome))) {
            StashOutcome::Staged => app.toast(
                ToastKind::Success,
                email.map_or_else(
                    || "logged in\ncreate account saves it".to_string(),
                    |email| format!("logged in\nas {email}\ncreate account saves it"),
                ),
            ),
            StashOutcome::FormClosed => app.toast(
                ToastKind::Warning,
                "login finished but the new-account form is no longer open\nlog in again",
            ),
            StashOutcome::WrongType => app.toast(
                ToastKind::Info,
                "login finished but the form type changed\nnothing was saved",
            ),
        }
        return;
    };

    let has_creds = app
        .config()
        .find(&ProfileName::from(session.name.clone()))
        .is_some_and(|p| p.credentials.is_some());
    // The uuid rides the snapshot: this may land in a confirm modal instead of
    // committing now, and only the commit may seed the anchor. The probe body's
    // plan rides it too, for the commit's `usage_cache.json` seed.
    let snapshot = crate::actions::capture_snapshot_from_outcome(outcome);
    // No stored creds → nothing diverges; adopt silently (mirrors the
    // first-login adopt in `poll_credentials_divergence`).
    let apply_now = !has_creds
        || matches!(
            app.config().state.default_divergence,
            Some(DivergenceChoice::Overwrite)
        );
    if !apply_now {
        // The modal below describes the account under the name, so it opens
        // only while that is still the one the login started for.
        let held_by_account = with_state_lock(|held| {
            crate::profile::require_profile_id(
                &ProfileName::from(session.name.clone()),
                &account,
                held,
            )
        });
        if let Err(e) = held_by_account {
            app.toast(ToastKind::Danger, format!("login failed\n{e:#}"));
            return;
        }
        // Names the endpoint only where the preserve arm will actually keep it
        // — the same rule the CLI reauth prompt enforces, asked of the same
        // predicate. This snapshot is credential-only, so the arm turns on the
        // stored profile alone.
        let keeps_endpoint = app
            .config()
            .find(&ProfileName::from(session.name.clone()))
            .is_some_and(crate::claude::has_own_inference_endpoint);
        app.open_modal(Modal::Confirm(ConfirmState {
            message: format!("replace the stored credentials for '{}'?", session.name),
            detail: Some(
                if keeps_endpoint {
                    "a fresh login finished for this account. the old tokens are dropped; chain slot, env, model settings, and its endpoint and api key stay."
                } else {
                    "a fresh login finished for this account. the old tokens are dropped; chain slot, env, and model settings stay."
                }
                .to_string(),
            ),
            choice: false,
            scroll: ConfirmScroll::default(),
            on_confirm: ConfirmAction::CaptureOverwrite(
                Box::new(snapshot),
                session.name,
                account,
                false,
            ),
        }));
        return;
    }
    apply_login_snapshot(app, session.name, account, snapshot);
}

/// Overwrite profile `name` with a finished login's `snapshot`, no confirm,
/// while `name` still holds `account`.
fn apply_login_snapshot(
    app: &mut App,
    name: String,
    account: ProfileId,
    snapshot: CaptureSnapshot,
) {
    if guest_refused(app) {
        return;
    }

    // The overwrite relinks the live slot when the profile is or becomes the
    // active one.
    let relinks = {
        let cfg = app.config();
        cfg.is_active(&ProfileName::from(name.clone())) || cfg.state.active_profile.is_none()
    };
    let _placed = if relinks {
        let Some(placed) = bare_item_gate(
            app,
            BareItemWrite::Login {
                name: name.clone(),
                account: account.clone(),
                snapshot: Box::new(snapshot.clone()),
            },
        ) else {
            return;
        };
        Some(placed)
    } else {
        None
    };
    let email = match &snapshot.anchor {
        crate::actions::AnchorAction::Proven(identity) => identity
            .email
            .as_deref()
            .and_then(crate::profile_cache::sanitize_identity_detail)
            .map(|email| escape_control(&email)),
        _ => None,
    };
    let result = {
        let mut cfg = app.config();
        overwrite_captured_profile(
            &mut cfg,
            &ProfileName::from(name.clone()),
            &account,
            snapshot,
        )
    };
    match result {
        Ok(()) => {
            app.refresh_tokens();
            app.apply_forgets();
            app.refresh_unsaved_live_login();
            app.toast(
                ToastKind::Success,
                email.map_or_else(
                    || format!("logged in '{name}'"),
                    |email| format!("logged in '{name}'\nas {email}"),
                ),
            );
            toast_duplicate_login(app, &ProfileName::from(name));
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!("login failed\n{}", escape_control(&error_toast_detail(&e))),
        ),
    }
}

/// Fold a captured Alibaba console session into its profile. Replaces the usage
/// session and NOTHING else — not the api key, which the callback also returns
/// scoped to a workspace rather than to this plan (`alibaba_login`'s module doc
/// has the reason it is dropped).
///
/// [`crate::actions::store_console_login`] refuses an account other than the
/// one the login started for, and one repointed at another console front.
/// There is no divergence gate, because a lapsed session cannot be refreshed
/// and re-running this login is the routine repair rather than an overwrite to
/// guard.
fn apply_console_login(
    app: &mut App,
    session: LoginSession,
    outcome: crate::alibaba_login::ConsoleLoginOutcome,
) {
    // `start_console_login` always claims an account; only a `+ new` draft's
    // session has none, and no draft session receives a console result.
    let Some(account) = session.account else {
        return;
    };
    let name = ProfileName::from(session.name);
    let result = {
        let mut cfg = app.config();
        crate::actions::store_console_login(&mut cfg, &name, &account, outcome.console)
    };
    match result {
        Ok(()) => {
            // The stored session is what the usage leg fetches with, and
            // `store_console_login` drops the cache the old one filled, so ask
            // for the figures the new one can actually read. The stale
            // `console login expired` verdict goes with the old session via
            // the drain — it must not survive until the queued fetch.
            app.refresh_tokens();
            app.apply_forgets();
            app.manual_refresh_one(&name);
            // The window is the surprising part and the CLI's own summary leads
            // with it: the 48h runs from the aliyun browser sign-in, so a login
            // inherits what is left of it and can be worth minutes.
            app.toast(
                ToastKind::Success,
                format!(
                    "console session captured for '{name}'\nit expires 48h after your aliyun sign-in, not after this login"
                ),
            );
        }
        Err(e) => app.toast(
            ToastKind::Danger,
            format!(
                "console login failed\n{}",
                escape_control(&error_toast_detail(&e))
            ),
        ),
    }
}

pub(crate) fn on_tick(app: &mut App) {
    app.tick_count = app.tick_count.wrapping_add(1);
    poll_palette(app);
    poll_usage_extras(app);
    drain_session_moves(app);
    drain_verify_login(app);
    // Safety net: a forget noted without an immediate drain still lands before
    // the next frame reads the stores; the per-site drains keep this a no-op.
    app.apply_forgets();

    drain_op_results(app);
    drain_status_events(app);
    drain_tokens_events(app);
    if app.tab == Tab::Tokens
        && app.token_view == TokenView::Models
        && app.token_period.bucket().is_some()
    {
        clamp_token_model_cursor(app);
    }
    drain_pricing_events(app);
    drain_login_events(app);
    drain_codex_login(app);
    drain_codex_new_login(app);
    drain_profile_delete(app);
    // After the delete drain: a delete the bare item held has cleared its
    // in-flight mark before its question can be answered.
    drain_bare_item_answers(app);
    drain_unsaved_live_login(app);
    drain_capture_login(app);

    if app.reload_if_state_changed() {
        app.clamp_profile_cursor();
        // Setup's `+ new` selection is the trailing row, never an index: a
        // reload that moves the roster keeps it there instead of demoting it
        // onto the last account. Captured pre-swap (`reload_was_on_new`) so a
        // deleted last account (whose cursor reads `New` post-swap) is not
        // mistaken for `+ new`. The Overview's own cursor rules are
        // `place_overview_cursor`'s, already run by the clamp.
        if app.reload_was_on_new {
            app.profile_cursor.set(app.profile_count());
        }
        if app.profile_count() == 0 {
            app.usage_detail_focus = StatusFocus::List;
            app.usage_detail_scroll = 0;
        }
    }
    app.apply_usage();

    drain_switch_gates(app);

    let auto_switch_targets: Vec<PendingSwitchTarget> = app
        .pending_switch
        .lock()
        .map(|mut g| g.drain().collect())
        .unwrap_or_default();
    for decision in auto_switch_targets {
        // An exit wait starts no new write: a queued auto-switch would spawn a
        // gate (and could relink credentials) after the user quit. The
        // decisions were drained above and drop with the process.
        if app.exit_wait {
            break;
        }
        let name = ProfileName::from(decision.target.clone());
        if switch_gate_in_flight(&app.activity) || !is_idle(&app.activity, &name) {
            continue;
        }
        // The label keys on the DESTINATION, not on which pass fired: any move
        // landing on the preferred (home) account reads as a homecoming. The
        // healthy-active return pass is the usual source, but the exhaustion walk
        // can also land here when the preferred is the only clear member left —
        // both are genuinely "now on home", so the destination-based label holds
        // without threading the cause through `SwitchAction`.
        let key_rejected = app.key_rejected_names();
        // Revalidate under the fresh config (reloaded above). Only a record that
        // CARRIES a key-rejection cause can be stale: a repair landing between
        // the scan's queue and this dispatch changes the active's fingerprint, so
        // the switch-away it motivated is dropped instead of moving off the
        // just-repaired account. A cause-absent (ordinary exhaustion/home) record
        // executes even beside an ambient stale mark.
        {
            let cfg = app.config();
            if crate::usage::queued_switch_away_is_stale(
                decision.key_rejected_cause.as_ref(),
                &cfg.profiles,
            ) {
                continue;
            }
        }
        // The daemon drain's twin: the queued target's login is re-read ON DISK
        // right before dispatch, because the in-memory `Profile` is stale until
        // the next reload and a login deleted out-of-process after the scan's
        // enqueue must still stop the link. A proven-none store drops; an
        // unreadable store's first skip marks the member and logs one line, the
        // only trace of why it leaves the walk, and it is never linked on a guess.
        match crate::profile::stored_login(&name) {
            crate::profile::StoredLogin::HoldsNone => continue,
            crate::profile::StoredLogin::Unknown => {
                let newly = app
                    .skipped_unreadable
                    .lock()
                    .map(|mut s| s.insert(name.as_str().to_string()))
                    .unwrap_or(false);
                if newly {
                    crate::logline::logline!(
                        "tollgate: not switching to '{name}': {}; skipping it until it reads again",
                        crate::format::STORED_LOGIN_UNREADABLE
                    );
                }
                continue;
            }
            crate::profile::StoredLogin::HoldsLogin => {}
        }
        let returning = app.config().is_home_today(&name, &key_rejected);
        let msg = if returning {
            format!("returning to preferred account '{name}'")
        } else {
            format!("auto-switching to '{name}'")
        };
        app.toast(ToastKind::Warning, msg);
        app.set_tab_activity(Tab::Overview, ToastKind::Warning);
        perform_switch(app, &name);
    }

    drain_pending_switch_off(app);

    drain_startup_signals(app);
    maybe_spawn_bootstrap(app);

    poll_credentials_divergence(app);
    poll_live_sessions(app);
    sync_broken_verdicts(app);
    sync_credit_refusals(app);
    poll_codex_rows(app);
    poll_form_predicates(app);
    poll_plugin_refresh(app);
    drain_daemon_control(app);
    poll_daemon_health(app);
    poll_tps(app);

    // The codex or pool snapshot may have changed under the selection this
    // tick; re-place it by the Overview's rule (ascend + first row for a
    // hidden-but-held row, the row now at the flat index it held for a
    // dropped one).
    app.rehome_setup_selection();
    app.rehome_usage_selection();

    warn_day_claim_notices(app);
    update_banner(app);
    app.prune_toasts();

    // The quit is decided and the exit waits for the running writes (a
    // every drain, so the tick that drains the last write is the one that exits.
    if app.exit_wait && !in_flight_exit_write(app) {
        app.quit = true;
        app.shutting_down.store(true, Ordering::SeqCst);
    }
}

/// Say once, per day and per config change, what today's day lists are doing
/// that the operator did not write them to do. Toast for the operator at the
/// keyboard, `logline!` for the record a headless run leaves behind.
///
/// Gated on the notice text rather than on a bool because the chain pass that
/// resolves the claim re-runs every tick: an ungated warning would repaint the
/// same line until midnight, and a bool one would stay silent when the
/// operator edits a second list in while the first notice is still up.
/// Dropping a notice out of the set is what lets the same state, removed and
/// re-introduced, warn again.
pub(crate) fn warn_day_claim_notices(app: &mut App) {
    let key_rejected = app.key_rejected_names();
    let notices = app.config().day_claim_notices_today(&key_rejected);
    if notices == app.day_claim_notices {
        return;
    }
    let fresh: Vec<String> = notices
        .iter()
        .filter(|m| !app.day_claim_notices.contains(m))
        .cloned()
        .collect();
    for msg in fresh {
        crate::logline::logline!("tollgate: {msg}");
        app.toast(ToastKind::Warning, msg);
    }
    app.day_claim_notices = notices;
}

/// Re-read the codex roster for the Overview's codex section and the header's
/// account count, at most once a second: a roster TOML plus a few small files
/// per account is cheap but not per-frame cheap, and both a `tollgate login --codex`
/// in another terminal and a codex usage fetch land on a human timescale.
/// Ungated by tab and by filter, so a `c` onto the codex view shows the current
/// roster rather than the one from whenever the view last showed it.
fn poll_codex_rows(app: &mut App) {
    const CODEX_ROWS_INTERVAL: Duration = Duration::from_secs(1);
    if app
        .last_codex_rows_refresh
        .is_some_and(|t| t.elapsed() < CODEX_ROWS_INTERVAL)
    {
        return;
    }
    app.last_codex_rows_refresh = Some(Instant::now());
    app.hermes_rows = hermes_rows();
    app.hermes_cursor = app
        .hermes_cursor
        .min(app.hermes_rows.len().saturating_sub(1));
    let remembered_name = match app.overview_remembered {
        Some(MainItemKind::Codex(i)) => app.codex_rows.get(i).map(|r| r.name.clone()),
        _ => None,
    };
    app.codex_rows = codex_rows();
    if let Some(name) = remembered_name
        && let Some(i) = app.codex_rows.iter().position(|r| r.name == name)
    {
        app.overview_remembered = Some(MainItemKind::Codex(i));
    }
    app.place_overview_cursor();
}

/// Re-probe the daemon presence + `status.json` health for the `[ daemon ]`
/// most once a second (a flock try-lock + `status.json` read is cheap but not
/// free, and both change on a human timescale).
fn poll_daemon_health(app: &mut App) {
    const DAEMON_PROBE_INTERVAL: Duration = Duration::from_secs(1);
    if app.last_daemon_probe.elapsed() < DAEMON_PROBE_INTERVAL {
        return;
    }
    app.last_daemon_probe = Instant::now();
    app.daemon_health = crate::daemon::daemon_health();
    sync_daemon_binary_change(app);
    // The image this process runs never comes back, so each kind's toast
    // latches once raised; a removal can precede a replacement (a Windows
    // says what to do. A changed binary read while this process's own
    // toast says it. That credit may be wrong, so such a probe stays quiet
    // without latching, and a change by anything else still toasts once a
    // failed update clears the mark. The mark is read after the change check:
    // the update sets it before its rename, so a rename this probe saw is
    // already marked.
    if let Some(change) = crate::plugin_host::self_exe_change()
        && app.own_toasted_kinds.latch(change)
    {
        app.toast(ToastKind::Warning, own_binary_toast(change));
    }
}

/// The kinds of executable change that already raised their toast this run.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ToastedKinds {
    replaced: bool,
    removed: bool,
}

impl ToastedKinds {
    /// Mark `change` toasted: `true` only the first time for its kind.
    fn latch(&mut self, change: ExeChange) -> bool {
        let toasted = match change {
            ExeChange::Replaced => &mut self.replaced,
            ExeChange::Removed => &mut self.removed,
        };
        !std::mem::replace(toasted, true)
    }
}

/// The toast a fresh daemon on a changed binary raises, once per kind per run.
fn daemon_binary_toast(change: ExeChange) -> &'static str {
    match change {
        ExeChange::Replaced => {
            "the daemon is still running the old tollgate binary and skips the plugin self-heal: restart it with `tollgate daemon --replace`"
        }
        ExeChange::Removed => {
            "the daemon's tollgate binary was removed and it skips the plugin self-heal: reinstall tollgate, then restart it with `tollgate daemon --replace`"
        }
    }
}

/// The plugin card's line while a fresh daemon runs a changed binary; the
/// `key: value` splitter.
pub(crate) fn daemon_binary_card_line(change: ExeChange) -> &'static str {
    match change {
        ExeChange::Replaced => {
            "daemon still on the old binary: plugin self-heal off until it restarts (`tollgate daemon --replace`)"
        }
        ExeChange::Removed => {
            "daemon's binary was removed: plugin self-heal off until tollgate is reinstalled and it restarts (`tollgate daemon --replace`)"
        }
    }
}

/// The toast this TUI raises, once per kind per run, on the first probe that
/// no mark.
fn own_binary_toast(change: ExeChange) -> &'static str {
    match change {
        ExeChange::Replaced => {
            "this tollgate is still running the old binary: quit and reopen it to run the new one"
        }
        ExeChange::Removed => "this tollgate's binary was removed: quit it before reinstalling",
    }
}

/// The refusal in place of the plugin install while this TUI's own executable
/// stays changed: the install would register the plugin tree baked into the
/// old image.
fn own_binary_install_refusal(change: ExeChange) -> &'static str {
    match change {
        ExeChange::Replaced => {
            "this tollgate is still running the old binary: quit and reopen it before installing the plugin"
        }
        ExeChange::Removed => {
            "this tollgate's binary was removed: reinstall tollgate and open it to install the plugin"
        }
    }
}

/// The refusal in place of the completions fix while this TUI's own executable
/// stays changed: the script it would write is the old image's emission, never
/// what the installed binary prints.
fn own_binary_completions_refusal(change: ExeChange, stale: bool) -> String {
    let (gerund, verb) = if stale {
        ("updating", "update")
    } else {
        ("installing", "install")
    };
    match change {
        ExeChange::Replaced => format!(
            "this tollgate is still running the old binary: quit and reopen it before {gerund} completions"
        ),
        ExeChange::Removed => format!(
            "this tollgate's binary was removed: reinstall tollgate and open it to {verb} completions"
        ),
    }
}

/// Re-read how a fresh daemon's binary changed under the health the app holds
/// now. A change into `Some` tints the Plugin tab and raises its kind's
/// toast, once per kind per run; any change rebuilds the plugin card, so its
/// line follows the feed on this cadence rather than waiting for a Services
/// recompute.
fn sync_daemon_binary_change(app: &mut App) {
    let change = crate::daemon::daemon_binary_change(app.daemon_health);
    if change == app.daemon_binary_change {
        return;
    }
    app.daemon_binary_change = change;
    if let Some(change) = change {
        app.set_tab_activity(Tab::Plugin, ToastKind::Warning);
        if app.daemon_toasted_kinds.latch(change) {
            app.toast(ToastKind::Warning, daemon_binary_toast(change));
        }
    }
    recompute_plugin_checks(app, false);
}

/// Re-read the producer's tps cache on the same once-a-second cadence as the
/// daemon probe: one JSON read is cheap but not per-frame cheap, and the
/// lease holder rewrites it on its own tick. This TUI is itself the producer
/// when it holds the lease, reading the cache its scheduler wrote, so the two
/// never disagree.
fn poll_tps(app: &mut App) {
    const TPS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
    if app.last_tps_refresh.elapsed() < TPS_REFRESH_INTERVAL {
        return;
    }
    app.last_tps_refresh = Instant::now();
    let snapshot = crate::tps::load_cache();
    app.set_tps(snapshot);
}

/// A transcript sum's average speed: output tokens ÷ generation seconds,
/// `None` when the sum carries no generation time.
pub(crate) fn day_average(sum: crate::tps::DaySum) -> Option<f64> {
    (sum.gen_ms > 0).then(|| sum.output as f64 / (sum.gen_ms as f64 / 1000.0))
}

/// Re-tally the live-session registry for the Overview `live` column, the
/// Fallback member card and the header's fleet count, at most once a second —
/// a readdir plus an `open` + `try_lock` per row is cheap but not per-frame
/// cheap, and a session starting or exiting is a human-timescale event. Ungated
/// by tab: the header reads it on every tab, and a snapshot a second stale on
/// arrival would show the wrong fleet for that second.
fn poll_live_sessions(app: &mut App) {
    const LIVE_SESSIONS_INTERVAL: Duration = Duration::from_secs(1);
    if app
        .last_live_sessions_refresh
        .is_some_and(|t| t.elapsed() < LIVE_SESSIONS_INTERVAL)
    {
        return;
    }
    app.last_live_sessions_refresh = Some(Instant::now());
    let tally = crate::live_sessions::LiveTally::collect(&app.config());
    app.set_live_sessions(tally);
    // The job-store snapshot and the handed-off set it implies ride the same
    // tick: refreshed here, never per frame, so the Setup detail's live-session
    // field reads a store at most once a second.
    let jobs = crate::claude_jobs::job_trees();
    app.handed_off = crate::runtime::handed_off_live_members(&jobs);
    app.job_trees = jobs;
}

/// Re-sync the durable dead-login verdicts into the live `ThirdPartyBroken`
/// mirror at most once a second, through the pass the daemon runs too
/// ([`crate::usage::sync_dead_logins`]). This is the stood-down TUI's heal
/// path: the fetcher (another process) clears or rewrites
/// `third_party_auth.json` under a credential the local mirror still marks,
/// and the config never changes, so the reload seed cannot see it.
fn sync_broken_verdicts(app: &mut App) {
    const SYNC_INTERVAL: Duration = Duration::from_secs(1);
    if app
        .last_broken_verdict_sync
        .is_some_and(|t| t.elapsed() < SYNC_INTERVAL)
    {
        return;
    }
    app.last_broken_verdict_sync = Some(Instant::now());
    crate::usage::sync_dead_logins(
        &app.config,
        &app.third_party_broken,
        &mut app.broken_verdict_mtimes,
    );
}

/// Re-sync every profile's credit refusal at most once a second, re-reading a
/// record only when its stat moved, as [`sync_broken_verdicts`] does for the
/// dead-login records: the instance that fetches usage writes and clears them,
/// possibly in another process.
fn sync_credit_refusals(app: &mut App) {
    const SYNC_INTERVAL: Duration = Duration::from_secs(1);
    if app
        .last_credit_refusal_sync
        .is_some_and(|t| t.elapsed() < SYNC_INTERVAL)
    {
        return;
    }
    app.last_credit_refusal_sync = Some(Instant::now());
    let names: Vec<ProfileName> = app
        .config()
        .profiles
        .iter()
        .map(|p| p.name.clone())
        .collect();
    app.credit_refusal_watch.refresh(&names);
    app.credit_refusals = app.credit_refusal_watch.refusals();
}

/// The tab-activity color a plugin/herdr health raises; `Idle` colors nothing
/// (the two rows never render it, but a neutral reading is no event).
fn health_activity(health: Health) -> Option<ToastKind> {
    match health {
        Health::Ok => Some(ToastKind::Success),
        Health::Warn => Some(ToastKind::Warning),
        Health::Danger => Some(ToastKind::Danger),
        Health::Idle => None,
    }
}

/// Recompute the sticky banner from current app state. Called every tick.
/// Winner ordering: `DANGER` > `WARNING`; ties broken by whichever condition
/// is checked first. One condition per `if` block in priority order.
fn update_banner(app: &mut App) {
    // Profiles exist but none is active — either switch-off-all (set by
    // `perform_switch_off`) or a profile that was never linked. Claim "all
    // spent" only while some profile still shows a live spent window; without
    // that evidence (e.g. a credential-less sole profile) the honest wording is
    // "no active profile" (issue #2 read the generic banner as a stuck limit).
    // The evidence is the HARD cap, not the configurable soft line: `Off` (what
    // clears the active in the first place) keys on the cap, and a soft-blocked
    // member still serves — calling it spent would be a lie the code disagrees with.
    let readings = app.readings_snapshot();
    let cfg = app.config();
    let no_active = !cfg.profiles.is_empty() && cfg.state.active_profile.is_none();
    // The spent test reads only accounts tollgate polls: a display-only provider
    // account (disabled or keyless) keeps its cached reading for the frame but
    // never counts as spent evidence.
    let any_spent = no_active
        && cfg.profiles.iter().any(|p| {
            crate::usage::reading_is_polled(p)
                && crate::fallback::is_exhausted_hard(p, readings.usage_of(&p.name))
        });
    drop(cfg);

    // Divergence outranks the compact-size nudge (both WARNING): a live-login
    // mismatch is an account-integrity condition to act on, and `d` resolves it
    // even in a small terminal. `no_active` (DANGER) still wins, and it implies
    // no divergence anyway (the poll clears the notice without an active profile).
    let divergence_msg = app
        .divergence_pending
        .as_ref()
        .map(DivergenceNotice::banner_message);

    app.banner = if no_active {
        let message = if any_spent {
            "all accounts spent · switch to an account to resume"
        } else {
            "no active account · select one to resume"
        };
        Some(Banner {
            severity: BannerSeverity::Danger,
            message: message.to_string(),
        })
    } else if let Some(message) = divergence_msg {
        Some(Banner {
            severity: BannerSeverity::Warning,
            message,
        })
    } else if app.compact || app.below_width_floor {
        // Terminal too small for the full layout. Lower severity than the
        // all-spent danger above, so danger wins the tie.
        Some(Banner {
            severity: BannerSeverity::Warning,
            message: "terminal too small · enlarge for full layout".to_string(),
        })
    } else {
        None
    };
}

/// Drain worker op results: clear activity slots, toast errors/successes,
/// rebuild token snapshot on success.
fn drain_op_results(app: &mut App) {
    let mut needs_token_snapshot_rebuild = false;
    while let Ok(OpResult { name, outcome }) = app.op_results.try_recv() {
        // The rotation marker alone: this result arrives a tick or more after
        // its worker returned, so the OAuth leg may already belong to a refetch.
        crate::usage::end_rotation(&app.activity, &ProfileName::from(name.clone()));
        match outcome {
            Ok(crate::usage::RotateDone::Rotated) => {
                needs_token_snapshot_rebuild = true;
                app.toast(ToastKind::Info, format!("rotated token for '{name}'"));
                app.set_tab_activity(Tab::Usage, ToastKind::Info);
            }
            Ok(crate::usage::RotateDone::LoginChanged) => {
                needs_token_snapshot_rebuild = true;
                app.toast(
                    ToastKind::Info,
                    format!("'{name}' login changed during the refresh · kept the change"),
                );
                app.set_tab_activity(Tab::Usage, ToastKind::Info);
            }
            Err(e) => {
                app.toast(
                    ToastKind::Danger,
                    format!(
                        "refresh for '{name}' failed\n{}",
                        escape_control(&error_toast_detail(&e))
                    ),
                );
                app.set_tab_activity(Tab::Usage, ToastKind::Danger);
            }
        }
    }
    if needs_token_snapshot_rebuild {
        app.refresh_tokens();
    }
}

/// Whether a modal other than the startup re-bind confirm holds the tick: the
/// confirm never blocks the switch gates, the wrap-off drain, the divergence
/// poll or bootstrap — a Divergence prompt those raise stacks ABOVE the
/// confirm. Every other modal keeps the pre-confirm gate, so no prompt stacks
/// under one. Callers: [`drain_switch_gates`], [`drain_pending_switch_off`],
/// [`poll_credentials_divergence`], [`maybe_spawn_bootstrap`].
fn blocking_modal_open(app: &App) -> bool {
    app.modals.iter().any(|m| !matches!(m, Modal::Rebind(_)))
}

/// Complete switches whose off-thread AUTH-1 gate answered: clear the pending
/// mark, then relink (`Ready`/`Refreshed`) or refuse with the login hint
/// (`Broken`) / a retry hint (`Transient`). Mirrors `drain_pending_switch_off`'s
/// modal guard — completion can raise the Divergence prompt, which must not
/// stack under an open modal — except during the exit wait, which must drain
/// the answered gate's `Switching` marker or the quit strands on it.
fn drain_switch_gates(app: &mut App) {
    if blocking_modal_open(app) && !app.exit_wait {
        return;
    }
    while let Ok(SwitchGateResult { name, gate }) = app.switch_gates.try_recv() {
        clear_activity(&app.activity, &name);
        let (gate, settled) = match gate {
            SwitchGate::Gated { gate, settled } => (gate, settled),
            SwitchGate::Held(outcome) => {
                if !app.exit_wait {
                    land_bare_item_outcome(app, BareItemWrite::Switch(name), outcome);
                }
                continue;
            }
        };
        // The settle's carry landed on the worker, before the gate ran.
        if let Some(notice) = settled.notice {
            app.toast(ToastKind::Warning, notice);
        }
        match gate {
            oauth::AuthGate::Ready => finalize_switch(app, &name, settled.placed),
            oauth::AuthGate::Refreshed => {
                app.refresh_tokens();
                finalize_switch(app, &name, settled.placed);
            }
            oauth::AuthGate::Broken => app.toast(
                ToastKind::Danger,
                crate::format::login_expired(&name).toast(),
            ),
            oauth::AuthGate::Transient(e) => app.toast(
                if e.is_login_change() {
                    ToastKind::Warning
                } else {
                    ToastKind::Danger
                },
                crate::format::refresh_transient(&name, &e).toast(),
            ),
            // Unreachable: the TUI passes `None` as the spend marker.
            oauth::AuthGate::Stopping => {
                app.toast(ToastKind::Danger, crate::format::DAEMON_STOPPING)
            }
        }
    }
}

/// Drain the wrap-off flag. Only drain when no modal is open — `perform_switch_off`
/// may raise a Divergence prompt; consuming the flag while one is open would
/// let the scheduler re-set it and stack duplicates.
fn drain_pending_switch_off(app: &mut App) {
    if app.exit_wait {
        return;
    }
    if blocking_modal_open(app) {
        return;
    }
    let switch_off_pending = app
        .pending_switch_off
        .lock()
        .map(|mut g| std::mem::replace(&mut *g, false))
        .unwrap_or(false);
    if switch_off_pending {
        perform_switch_off(app);
    }
}

/// Land the latest [`App::refresh_unsaved_live_login`] answer; an older
/// request's answer is stale.
fn drain_unsaved_live_login(app: &mut App) {
    while let Ok((generation, unsaved)) = app.unsaved_login_rx.try_recv() {
        if generation == app.unsaved_login_generation {
            app.unsaved_live_login = unsaved;
        }
    }
}

/// Drain startup signals from reconcile/bootstrap workers.
fn drain_startup_signals(app: &mut App) {
    while let Ok(signal) = app.startup_results.try_recv() {
        match signal {
            StartupSignal::ReconcileDone => {
                app.reconcile_done = true;
            }
            StartupSignal::ReconcileNeedsPrompt { active } => {
                app.reconcile_done = true;
                resolve_or_note_divergence(app, &active);
            }
            StartupSignal::BootstrapDone => {
                app.finish_bootstrap();
            }
        }
    }
}

/// Spawn bootstrap once reconcile is done and no modal is open.
fn maybe_spawn_bootstrap(app: &mut App) {
    // The exit wait starts no new write: spawning the bootstrap would start the
    // scheduler, which could raise a fresh spend after the user quit.
    if app.exit_wait {
        return;
    }
    // The startup re-bind confirm never blocks bootstrap: usage seeding, the
    // scheduler and the startup auto-switch must run even while the confirm
    // waits on a key. Every other modal keeps the pre-diff gate.
    let blocking_modal = blocking_modal_open(app);
    if app.bootstrap_started || !app.reconcile_done || blocking_modal {
        return;
    }
    app.bootstrap_started = true;
    app.bootstrap_active.store(true, Ordering::SeqCst);
    // The seed runs synchronously at the point the worker's gate first opens:
    // on the acting-default arms (an overwrite, or a modal that just settled)
    // `reconcile_startup` left the seed here, so the other accounts' cached
    // bars land on this frame instead of whenever the worker finishes.
    if !app.startup_seeded {
        seed_startup_stores(app);
        app.startup_seeded = true;
    }
    app.spawn_bootstrap();
}

/// 1Hz check: keep [`App::divergence_pending`] (the non-blocking banner) in
/// sync with whether the live login matches what a switch to the active
/// profile installs (the install source). Never pushes the modal — a divergence must not lock the whole TUI out
/// of browsing usage; <kbd>d</kbd> opens the resolver on demand, and
/// switch-shaped actions raise it themselves when resolution is actually
/// required. First-login adoption resolves automatically here, as does a
/// configured `default_divergence` within its owner gate
/// ([`resolve_or_note_divergence`]).
fn poll_credentials_divergence(app: &mut App) {
    if crate::identity::upstream_active() {
        return;
    }
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    if app
        .last_divergence_check
        .is_some_and(|t| t.elapsed() < POLL_INTERVAL)
    {
        return;
    }
    app.last_divergence_check = Some(Instant::now());

    if blocking_modal_open(app) {
        return;
    }
    // The reload fingerprint never stats the live credentials file, so a login
    // CC itself minted mid-session is invisible to every other refresh site.
    // This poll already reads at 1Hz; while the Setup tab is open it also
    // refreshes the `+ capture current login` row's flag — zero accounts is
    // that row's own onboarding window.
    if app.tab == Tab::Setup {
        app.refresh_unsaved_live_login();
    }
    let Some(active) = app.config().state.active_profile.as_ref().cloned() else {
        app.divergence_pending = None;
        return;
    };
    if !matches!(
        reconcile_decision(app, &active),
        DivergenceVerdict::NeedsPrompt
    ) {
        return;
    }
    resolve_or_note_divergence(app, active.as_ref());
}

/// What the shared divergence cascade ([`reconcile_decision`]) makes of the
/// live slot.
enum DivergenceVerdict {
    /// Linked, adopted, or already stored — no divergence to resolve.
    Clean,
    /// A logged-out shell. The poll exempts it outright (no nag, no banner);
    /// the startup reconcile treats it as a divergence so a configured
    /// `default_divergence` Overwrite still performs the boot heal (relink
    /// the stored login over the shell) — the shipped pin's behavior.
    Shell,
    /// A genuine divergence — the user's verdict is needed.
    NeedsPrompt,
}

/// The ONE divergence decision, shared by the startup reconcile and the 1Hz
/// poll (#104): the live login is judged against what a switch INSTALLS
/// (the install source — the sidecar for split profiles), never the
/// tollgate-private usage pair, so a refreshless live login (a rolling bearer,
/// a static mint) is no divergence. Side effects: a first-login adoption runs
/// here; exempt states clear the banner. A logged-out shell is reported as
/// its own verdict so the two callers can disagree on it (see
/// [`DivergenceVerdict::Shell`]).
fn reconcile_decision(app: &mut App, active: &ProfileName) -> DivergenceVerdict {
    if !matches!(
        classify_credentials_link(active).ok(),
        Some(LinkState::Diverged)
    ) {
        app.divergence_pending = None; // resolved; a later divergence re-flags
        return DivergenceVerdict::Clean;
    }
    // A logged-out shell is nothing the poll must resolve: don't nag, and
    // never let a `default_divergence` Overwrite capture its blank tokens
    // over the stored chain.
    if live_credentials_are_shell() {
        app.divergence_pending = None;
        return DivergenceVerdict::Shell;
    }
    // First login on a credential-less profile: adopt silently, don't prompt.
    if is_first_login(active).unwrap_or(false) {
        let result = {
            let mut cfg = app.config();
            adopt_first_login(&mut cfg, active)
        };
        match result {
            Ok(()) => {
                app.refresh_tokens();
                // AUTH-1: an adopted first login is a fresh login — lift a
                // standing auth_broken quarantine like the sibling fresh-login
                // paths do.
                oauth::mark_auth_broken(&app.config, active, false);
                app.apply_forgets();
                app.refresh_unsaved_live_login();
                app.toast(ToastKind::Success, format!("saved login into '{active}'"));
            }
            Err(e) => app.toast(
                ToastKind::Danger,
                format!("adopt failed\n{}", escape_control(&error_toast_detail(&e))),
            ),
        }
        return DivergenceVerdict::Clean;
    }
    // A login already saved in the active profile's store holds nothing unsaved —
    // the next switch re-installs it, losing no login — so it must not raise the
    // banner. This clears tollgate's own symlink AND the macOS regular-file mirror CC
    // writes over it. The switch/defer gates apply the same exemption through
    // `live_diverged_and_unsaved`; the poll must adopt a first login before this
    // point, so it checks the predicate directly here instead.
    if crate::claude::live_login_is_stored(active) {
        app.divergence_pending = None;
        return DivergenceVerdict::Clean;
    }
    DivergenceVerdict::NeedsPrompt
}

/// Apply the configured `default_divergence`, or flag the non-blocking banner
/// when nothing is configured — the shared tail of the startup reconcile and the
/// 1Hz poll.
///
/// The default is owner-gated: it may only resolve a login no stored SIBLING
/// owns. Owner-blind, an `Overwrite`/`NewProfile` default captures a sibling
/// profile's re-login into the active profile — credential misattribution with
/// no user say. A sibling-owned divergence falls through to the banner, whose
/// "switch to it" action is the resolution that case actually wants.
///
/// The banner is never a modal: the user opens the resolver with <kbd>d</kbd>
/// when they want it, and any switch-shaped action raises it itself. Startup and
/// the poll must never lock the TUI.
fn resolve_or_note_divergence(app: &mut App, active: &str) {
    if note_divergence(app, active) {
        return;
    }
    if let Some(choice) = startup_default(app, active) {
        // The default IS the resolution, so drop the banner it would otherwise
        // paint for the tick between resolving and the next poll's re-classify.
        app.divergence_pending = None;
        run_divergence_choice(app, active, choice);
    }
}

/// Set/refresh the non-blocking divergence banner; returns whether a SIBLING
/// profile owns the live login. The (local) owner lookup runs only when the live
/// login actually changed — the fingerprint memo keeps the 1Hz poll to a single
/// file read in the steady state.
fn note_divergence(app: &mut App, active: &str) -> bool {
    let fingerprint = live_creds_fingerprint();
    if let Some(p) = &app.divergence_pending
        && p.active == active
        && p.fingerprint == fingerprint
    {
        return p.sibling.is_some();
    }
    let sibling = live_login_sibling_owner(app, active);
    let sibling_owned = sibling.is_some();
    app.divergence_pending = Some(DivergenceNotice {
        active: active.to_string(),
        sibling,
        fingerprint,
    });
    sibling_owned
}

/// The stored profile other than `active` that owns the live login, if any.
fn live_login_sibling_owner(app: &App, active: &str) -> Option<ProfileName> {
    let cfg = app.config();
    crate::actions::identify_live_login_owner(&cfg).filter(|owner| owner != active)
}

/// The configured `default_divergence` that actually acts at startup, or `None`
/// when the resolution is the banner alone: unset, or a sibling owns the live
/// login (the owner gate skips the default). ONE resolver so the seed's
/// predicate ([`divergence_resolves_to_banner`]) and the drain's action
/// ([`resolve_or_note_divergence`]) cannot drift apart when the gate changes.
fn startup_default(app: &App, active: &str) -> Option<crate::profile::DivergenceChoice> {
    if live_login_sibling_owner(app, active).is_some() {
        return None;
    }
    app.config().state.default_divergence
}

/// Whether [`resolve_or_note_divergence`] will resolve a divergence on
/// `active` by flagging the banner alone: no `default_divergence` is
/// configured, or a sibling owns the live login (the owner gate skips the
/// default).
fn divergence_resolves_to_banner(app: &App, active: &str) -> bool {
    startup_default(app, active).is_none()
}

/// SipHash of the live access token — a cheap identity for "which login sits in
/// `~/.claude/.credentials.json` right now". It changes on every re-login and
/// every refresh, so the banner's owner lookup re-runs exactly when the creds
/// change. `None` when no readable OAuth login is present.
fn live_creds_fingerprint() -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let creds = read_claude_credentials().ok().flatten()?;
    let token = creds.access_token().filter(|t| !t.is_empty())?;
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    Some(hasher.finish())
}

/// Clears `bootstrap_active` and posts `BootstrapDone` when dropped — success
/// or panic, the scheduler and UI thread are never left blocked on a crashed
/// bootstrap.
struct BootstrapDoneGuard {
    bootstrap_active: Arc<AtomicBool>,
    startup_sender: StartupSender,
}

impl Drop for BootstrapDoneGuard {
    fn drop(&mut self) {
        self.bootstrap_active.store(false, Ordering::SeqCst);
        let _ = self.startup_sender.send(StartupSignal::BootstrapDone);
    }
}

/// Test seam: every placement a writer enters while a test watches an item,
/// with the entering thread and whether that thread's writer guards then let
/// the item be replaced. Armed with the item; `None` otherwise.
#[cfg(test)]
pub(crate) static BARE_ITEM_PLACEMENTS: std::sync::Mutex<Option<BareItemPlacements>> =
    std::sync::Mutex::new(None);

/// The item a test watches, and each placement entered while it does: the
/// entering thread and whether the item could then be replaced there.
#[cfg(test)]
pub(crate) type BareItemPlacements = (serde_json::Value, Vec<(std::thread::ThreadId, bool)>);

#[cfg(test)]
fn probe_bare_item_placement() {
    if let Ok(mut probe) = BARE_ITEM_PLACEMENTS.lock()
        && let Some((item, seen)) = probe.as_mut()
    {
        let replaceable = crate::claude::bare_item_may_be_replaced(item, None);
        seen.push((std::thread::current().id(), replaceable));
    }
}

/// Handles of every [`spawn_worker`] thread, so a test can join them BEFORE its
/// `HomeSandbox` drops. `HOME_OVERRIDE` is a process-global cleared on that drop;
/// a worker that outlives it is detached, and its next home read resolves
/// whichever `HOME_OVERRIDE` is installed then (a later test's sandbox), never
/// the operator's real `$HOME`. Serialized by `profile::HOME_TEST_LOCK`, which
/// every sandboxed test holds, so the registry is effectively exclusive.
/// Never compiled into the binary.
#[cfg(test)]
static TEST_WORKERS: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Join every worker spawned so far. Any test whose action reaches
/// [`spawn_worker`] must call this while its sandbox is still alive.
#[cfg(test)]
pub(crate) fn join_test_workers() {
    let handles: Vec<_> = TEST_WORKERS
        .lock()
        .map(|mut w| std::mem::take(&mut *w))
        .unwrap_or_default();
    for handle in handles {
        let _ = handle.join();
    }
}

/// Join every worker spawned so far, waiting at most `bound` for all of them.
/// A handle still unfinished when the bound lapses is handed BACK to the
/// registry before this returns: the caller can then fail naming what it
/// waited on, while [`join_test_workers`] and the next bounded join still wait
/// for that handle — or, in `HomeSandbox::drop`, detach it so a stuck worker
/// cannot hang the teardown again. `Err` carries how many handles were still
/// running at the bound.
#[cfg(test)]
pub(crate) fn join_test_workers_within(bound: Duration) -> Result<(), usize> {
    let handles: Vec<_> = TEST_WORKERS
        .lock()
        .map(|mut w| std::mem::take(&mut *w))
        .unwrap_or_default();
    let deadline = Instant::now() + bound;
    let mut unfinished = Vec::new();
    for handle in handles {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            unfinished.push(handle);
        }
    }
    if unfinished.is_empty() {
        Ok(())
    } else {
        let n = unfinished.len();
        if let Ok(mut workers) = TEST_WORKERS.lock() {
            workers.extend(unfinished);
        }
        Err(n)
    }
}

/// Detach every worker the registry still holds, dropping its handle. Used by
/// `HomeSandbox::drop` after a bounded join misses: the worker is stuck past
/// the bound and the teardown must not hang on it again. A detached worker's
/// next [`crate::profile::home_dir`] resolves whichever `HOME_OVERRIDE` is
/// installed then (a later test's sandbox), never the operator's real `$HOME`.
#[cfg(test)]
pub(crate) fn detach_test_workers() {
    let handles: Vec<_> = TEST_WORKERS
        .lock()
        .map(|mut w| std::mem::take(&mut *w))
        .unwrap_or_default();
    drop(handles);
}

/// Test seam for the rotate workers [`run_confirm_action`] spawns: when a test
/// arms the hold, each spawned rotate worker sends on the parked channel and
/// blocks on the release channel BEFORE touching `rotate_one`/`refresh_all`, so
/// "confirm, then quit before the worker runs" is driven deterministically. The
/// parked send lands before the block, so the test's `recv` is a join, never a
/// clock poll.
#[cfg(test)]
static ROTATE_WORKER_HOLD: std::sync::Mutex<
    Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn arm_rotate_worker_hold()
-> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (parked_tx, parked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    if let Ok(mut hold) = ROTATE_WORKER_HOLD.lock() {
        *hold = Some((parked_tx, release_rx));
    }
    (parked_rx, release_tx)
}

#[cfg(test)]
pub(crate) fn disarm_rotate_worker_hold() {
    if let Ok(mut hold) = ROTATE_WORKER_HOLD.lock() {
        *hold = None;
    }
}

#[cfg(test)]
fn rotate_worker_gate() {
    let hold = ROTATE_WORKER_HOLD
        .lock()
        .map(|mut h| h.take())
        .unwrap_or_default();
    if let Some((parked_tx, release_rx)) = hold {
        let _ = parked_tx.send(());
        let _ = release_rx.recv();
    }
}

/// Spawn a background worker, catching panics so a single thread crash never
/// takes down the process. Callers clone only the Arcs their closure needs.
fn spawn_worker<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let handle = std::thread::spawn(move || {
        let _ = catch_unwind(AssertUnwindSafe(f));
    });
    #[cfg(test)]
    if let Ok(mut workers) = TEST_WORKERS.lock() {
        workers.push(handle);
    }
    #[cfg(not(test))]
    drop(handle);
}

/// Snapshot active credentials and detach the symlink. After shutdown, external
/// writes to `.credentials.json` land in the standalone file, not the profile.
///
/// The bounded wait covers only a running token spend, as the backstop for an
/// exit that skipped the exit wait (a second quit, or a quit while a checking
/// save holds it): a process that dies between the token endpoint's answer and
/// the stage write loses the minted pair. A usage read, a queued fetch or a
/// third-party leg may be abandoned, so none of them delays the exit.
pub(crate) fn shutdown(app: &mut App) -> Result<()> {
    app.shutting_down.store(true, Ordering::SeqCst);
    let wait_start = Instant::now();
    while wait_start.elapsed() < Duration::from_millis(2000) {
        // Drain answered switch gates each iteration without relinking: the
        // gate already landed its pair (staged/landed), and relinking during
        // shutdown would re-point credentials after the user quit. Clearing the
        // marker is what frees the spend wait — an answered gate that stays
        // queued would otherwise burn the whole bound.
        while let Ok(SwitchGateResult { name, .. }) = app.switch_gates.try_recv() {
            clear_activity(&app.activity, &name);
        }
        if !token_spend_in_flight(&app.activity) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Wait up to 5 s for the update check to finish, detaching a still-running
    // check after that (a slow download shouldn't delay TUI exit) — but never
    // while the replace step runs, so this shutdown path can't strand the
    if let Some(handle) = app.update_handle.take() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
    {
        let mut cfg = app.config();
        if !crate::identity::upstream_active() {
            let _ = snapshot_active_credentials(&mut cfg);
        }
        let _ = save_app_state(&cfg.state);
    }
    if !crate::identity::upstream_active() {
        let _ = detach_credentials_link();
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/tui_app.rs"]
mod tests;

/// One live claude session the `m` modal offers to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MoveCandidate {
    pub(crate) sid: String,
    /// The member the session's requests authenticate as (the served one).
    pub(crate) now_on: String,
    /// `oauth`, `api_key` or `relaunch_only`.
    pub(crate) executor: &'static str,
    pub(crate) state: crate::hot_swap::SwapState,
}

/// The Overview's `m` modal: move a live session onto the row's account.
#[derive(Debug, Clone)]
pub(crate) struct MoveSessionForm {
    /// The account the session moves onto.
    pub(crate) target: String,
    pub(crate) sessions: Vec<MoveCandidate>,
    pub(crate) cursor: usize,
    pub(crate) scroll: std::cell::Cell<usize>,
    pub(crate) viewport: std::cell::Cell<usize>,
    pub(crate) starts: std::cell::RefCell<Vec<usize>>,
    pub(crate) block_ends: std::cell::RefCell<Vec<usize>>,
    pub(crate) total: std::cell::Cell<usize>,
}

/// What a session-move worker reports, once: the session request core's
/// outcome, or its refusal text.
#[derive(Debug)]
pub(crate) struct SessionMoveResult {
    pub(crate) sid: String,
    pub(crate) target: String,
    pub(crate) outcome: std::result::Result<crate::sessions_cli::SwitchRequest, String>,
}

/// Which Plugin pane has focus. `List`: the checks + profiles selector (↑↓ moves,
/// ⏎ descends, `f` fixes). `Detail`: the selected row's readout (↑↓ scrolls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginFocus {
    List,
    Detail,
}

/// A one-key fix offered on the selected row. `WireMcpServers` writes the manual
/// entry (a [`ConfirmAction`]); `RepairDivergence` re-raises the existing
/// divergence resolver for the named (active) profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginFix {
    WireMcpServers,
    RepairDivergence(String),
    /// Relink a `missing` active-profile credential link to its own stored creds.
    RelinkCredentials(String),
    /// Append the keybinding + sidebar row to herdr's config (the config half of
    /// `tollgate herdr install`). `PathBuf` = the resolved config file.
    HealHerdrConfig(std::path::PathBuf),
    /// Install the tollgate plugin through agentgear (user scope, embedded tree).
    /// Replaces the copy-paste `/plugin` hint the row used to show.
    InstallPlugin,
    Completions {
        shell: String,
        stale: bool,
    },
}

/// UI-thread-only state for the Plugin tab. Recomputed synchronously on tab focus
/// and on `r`; there is no background thread (all reads are local FS/`PATH`;
/// `claude --version` is one cached subprocess gated by [`PluginState::cc_version`]).
#[derive(Debug)]
pub(crate) struct PluginState {
    pub(crate) focus: PluginFocus,
    /// Cursor over the integration checks (`0..checks.len()`).
    pub(crate) cursor: usize,
    pub(crate) selector: ListView,
    pub(crate) form_offset: std::cell::Cell<usize>,
    pub(crate) detail_scroll: u16,
    pub(crate) diagnostic_focus: bool,
    pub(crate) displayed_scroll: std::cell::Cell<u16>,
    pub(crate) detail_viewport: std::cell::Cell<u16>,
    /// Max valid `detail_scroll` from the last render (`&App` interior mutability,
    /// clamped by the key handler — same pattern as `StatusState`).
    pub(crate) detail_max_scroll: std::cell::Cell<u16>,
    /// True while a `claude --version` probe is in flight (title spinner). The
    /// probe is synchronous, so this is mostly belt-and-suspenders for the spec.
    pub(crate) fetching: bool,
    pub(crate) error: Option<String>,
    pub(crate) checks: Vec<Check>,
    /// Cached `claude --version`: `None` = unprobed, `Some(None)` = probed and
    /// missing/unparseable, `Some(Some(v))` = the version string. Probed only
    /// on an explicit `r` — construction and a tab switch never spawn the
    /// subprocess, so the first paint never blocks on it.
    pub(crate) cc_version: Option<Option<String>>,
    /// Cached `tollgate mcp` discovery handshake: `None` = unprobed. Re-probed only
    /// on `r` (heavier than the others — it boots the real server), never on a tab
    /// switch or the per-tick refresh.
    pub(crate) mcp_boot: Option<crate::plugin_probe::McpProbe>,
    /// Cached herdr probe: `None` = unprobed, `Some(None)` = herdr does not
    /// resolve (no row), `Some(Some(p))` = the probe. Probed at construction in
    /// herdr mode (`HERDR_ENV=1` proves herdr is present) and re-probed on `r`;
    /// it spawns three subprocesses, so a tab switch and the per-tick refresh
    /// reuse the cached value.
    pub(crate) herdr: Option<Option<crate::herdr::HerdrProbe>>,
    /// The `tollgate mcp` job store as of the last refresh, newest first: what the
    /// delegates pane draws. Re-read on the same cadence as the checks, because
    /// the server writing it is a DIFFERENT process, so there is nothing to
    /// subscribe to and no event to wait for. Read-only: the TUI never writes
    /// this store, and never sweeps it.
    ///
    /// The cost is one readdir plus a parse per file. No count cap bounds that
    /// from here: retention is the two TTLs (`jobs::DONE_TTL_MS` /
    /// `jobs::RUNNING_TTL_MS`), applied only by the startup sweep inside a
    /// `tollgate mcp` process, which the TUI neither runs nor depends on having
    /// run, and the count spans both record spellings. What the directory
    /// really holds is whatever the last such startup expired, plus every job
    /// written since. Measured against the 1 s tick with 256 records over
    /// ~5 MB of envelopes, best of 5 warm: **~3 ms release, ~20 ms debug**.
    /// Written as a bound rather than to two figures on purpose — two
    /// independent runs on this box landed 2.2 ms and 3.2 ms release, neither on
    /// a quiet machine, so a point figure would read as stale to the next person
    /// who re-derives it and misses by half. What binds is the order: single-
    /// digit ms in release, roughly ten times that in debug.
    pub(crate) delegates: Vec<crate::mcp::jobs::StoredJob>,
    /// Cursor over the herdr detail's options rows ([`HERDR_OPTIONS`]). The
    /// herdr detail is the one detail pane that takes per-row focus: while it
    /// is descended, ↑↓ walks these rows instead of scrolling the prose.
    pub(crate) herdr_options_cursor: usize,
    /// Typed editor for the `tag refresh` stepper — the Config-tab
    /// refresh-interval editor's shape. `None` = not editing.
    pub(crate) herdr_tag_draft: Option<SeededInput>,
    /// The herdr config verdict from the last recompute (a cheap file re-read
    /// that rides the per-tick refresh), cached so the render and the key
    /// handler never re-read the file per frame. `None` = unreadable or no
    /// config path. The options section reads it to decide whether the
    /// `delegate row text` row can write.
    pub(crate) herdr_config: Option<crate::herdr::ConfigStatus>,
    pub(crate) plugin_health: Option<Health>,
    pub(crate) herdr_health: Option<Health>,
}

/// Selector index the `herdr` check occupies once it renders: `about`,
/// `mcp servers`, `plugin`, `herdr`, `runtime`. The landing row in herdr mode;
/// when the construction probe does not resolve herdr, the same index rests
/// on `runtime`, the last row — the cursor clamp in
/// `recompute_plugin_checks` keeps it valid either way.
const HERDR_SELECTOR_ROW: usize = 3;

impl Default for PluginState {
    fn default() -> Self {
        Self {
            plugin_health: None,
            herdr_health: None,
            focus: PluginFocus::List,
            cursor: 0,
            selector: ListView::default(),
            form_offset: std::cell::Cell::new(0),
            detail_scroll: 0,
            diagnostic_focus: false,
            displayed_scroll: std::cell::Cell::new(0),
            detail_viewport: std::cell::Cell::new(0),
            detail_max_scroll: std::cell::Cell::new(0),
            fetching: false,
            error: None,
            checks: Vec::new(),
            cc_version: None,
            mcp_boot: None,
            herdr: None,
            delegates: Vec::new(),
            herdr_options_cursor: 0,
            herdr_tag_draft: None,
            herdr_config: None,
        }
    }
}

impl PluginState {
    /// Total selectable rows (the integration checks).
    pub(crate) fn row_count(&self) -> usize {
        self.checks.len()
    }

    /// The check under the cursor.
    pub(crate) fn selected_check(&self) -> Option<&Check> {
        self.checks.get(self.cursor)
    }

    /// The fix offered by the row under the cursor, if any.
    pub(crate) fn selected_fix(&self) -> Option<&PluginFix> {
        if self.focus == PluginFocus::Detail
            && self.diagnostic_focus
            && self
                .selected_check()
                .is_some_and(|check| check.label == "herdr")
        {
            return None;
        }
        self.selected_check().and_then(|check| check.fix.as_ref())
    }
}

/// Whether `c` on the Overview cycles the harness filter. Off in tollgate for
/// now: the Overview shows codex and other providers as monitor rows, not
/// harness rows, so a filter step only hides the Claude accounts. With it off
/// the filter stays `All`, `c` is unbound there and the footer omits it.
pub(crate) const HARNESS_FILTER_KEY: bool = false;

/// One Hermes profile as the Overview renders it: read-only (a Hermes home is
/// the account, switched by relaunch), from the roster, the liveness markers
/// and the usage cache the daemon writes. Never a Hermes, python or sqlite3
/// run: the TUI reads files only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HermesRow {
    pub(crate) name: String,
    pub(crate) provider: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) live: bool,
    /// Month-to-date spend, as a short money string (`$0.42`).
    pub(crate) estimate: Option<String>,
}

/// Read the Hermes roster into the [`App::hermes_rows`] snapshot.
pub(crate) fn hermes_rows() -> Vec<HermesRow> {
    let Ok(state) = crate::hermes::profiles::HermesState::load() else {
        return Vec::new();
    };
    let now_secs = crate::usage::now_epoch_secs();
    let month = crate::usage::hermes_local::rfc3339_z(
        crate::usage::hermes_local::month_start_secs(now_secs),
    );
    state
        .profiles()
        .iter()
        .map(|p| HermesRow {
            name: p.name.clone(),
            provider: p.provider.as_str(),
            mode: p.mode.as_str(),
            live: crate::runtime::has_live_session(&ProfileName::from(p.name.as_str())),
            estimate: crate::usage::hermes_local::load(&p.name)
                .filter(|c| c.read_at_ms > 0 && c.period_start == month)
                .map(|c| crate::usage::derive::format_money(&c.total_cost(), "USD")),
        })
        .collect()
}

/// M-SWITCH, the refusal a Hermes row gives Enter and `s` (hermes spec §2.2).
pub(crate) fn hermes_switch_toast(name: &str) -> String {
    crate::hermes::m_switch(name)
}

/// Why an Overview read-only row refuses switch / edit / delete / reorder, as
/// the one-line hint the refusal toasts.
pub(crate) fn read_only_hint(obs: &crate::usage::observation::AccountObservation) -> &'static str {
    match obs.origin {
        crate::usage::observation::Origin::Upstream => UPSTREAM_ACCOUNT_HINT,
        crate::usage::observation::Origin::HermesProfile => {
            "hermes — switch by relaunch: tollgate start <name>"
        }
        _ => "monitor — edit with tollgate monitor",
    }
}

/// How often the TUI re-reads the monitoring / upstream observations.
pub(crate) const USAGE_EXTRAS_POLL_MS: u64 = 2_000;

/// Re-read the `monitor:` / `upstream:` observations from their caches (the
/// collect hooks never fetch), at most every [`USAGE_EXTRAS_POLL_MS`].
fn poll_usage_extras(app: &mut App) {
    use crate::usage::collect::{MONITOR_SOURCES, UPSTREAM_SOURCES};
    if MONITOR_SOURCES.is_empty() && UPSTREAM_SOURCES.is_empty() {
        return;
    }
    let now = std::time::Instant::now();
    let dirty = app.monitor_refresh_dirty.swap(false, Ordering::AcqRel);
    if !dirty
        && app.usage_extras_polled.is_some_and(|t| {
            now.duration_since(t) < std::time::Duration::from_millis(USAGE_EXTRAS_POLL_MS)
        })
    {
        return;
    }
    #[cfg(not(test))]
    if app.usage_extras_polled.is_none() {
        let now_ms = crate::usage::now_ms();
        crate::usage::monitor::grok::ensure_official_grok_config();
        crate::usage::monitor::antigravity::ensure_official_antigravity_config(now_ms);
        crate::usage::monitor::codex_native::ensure_official_codex_config(now_ms);
    }
    app.usage_extras_polled = Some(now);
    let config = (*app.config()).clone();
    let codex = crate::codex_profiles::CodexState::load().unwrap_or_default();
    let ctx = crate::usage::collect::CollectCtx {
        config: Some(&config),
        codex: &codex,
        now_ms: crate::usage::now_ms(),
        interval_ms: config.state.refresh_interval_ms,
        guest_mode: app.guest_mode,
        include_disabled: false,
    };
    let extras = usage_extras_from(&ctx, MONITOR_SOURCES, UPSTREAM_SOURCES);
    set_usage_extras(app, extras);
    #[cfg(not(test))]
    let _ = app.refresh_due_monitors();
}

/// Canonical provider display name for a local `profiles.toml` profile.
pub(crate) fn profile_provider_name(p: &Profile) -> &'static str {
    if p.usage_cache_is_third_party() {
        crate::usage::observation::SourceId::from_provider(p.provider).display_name()
    } else {
        crate::usage::observation::SourceId::AnthropicOauth.display_name()
    }
}

/// Group indices into `extras` by `o.provider` in first-seen provider order,
/// preserving relative order within each provider.
pub(crate) fn group_extra_indices_by_provider(
    indices: Vec<usize>,
    extras: &[crate::usage::observation::AccountObservation],
) -> Vec<usize> {
    let mut providers: Vec<&str> = Vec::new();
    for &i in &indices {
        let p = extras[i].provider.as_str();
        if !providers.iter().any(|&seen| seen.eq_ignore_ascii_case(p)) {
            providers.push(p);
        }
    }
    let mut out = Vec::with_capacity(indices.len());
    for p in providers {
        out.extend(
            indices
                .iter()
                .copied()
                .filter(|&i| extras[i].provider.eq_ignore_ascii_case(p)),
        );
    }
    out
}

/// The extras the Usage rail lists: the collector's hook observations
/// ([`crate::usage::collect::hook_observations`]), first id wins, grouped by
/// provider (if the last local profile shares a provider with any extra, that
/// provider leads so the group stays contiguous on the Usage rail).
pub(crate) fn usage_extras_from(
    ctx: &crate::usage::collect::CollectCtx<'_>,
    monitors: &[crate::usage::collect::SourceHook],
    upstream: &[crate::usage::collect::SourceHook],
) -> Vec<crate::usage::observation::AccountObservation> {
    let raw = crate::usage::collect::hook_observations(ctx, monitors, upstream);
    let mut providers: Vec<String> = Vec::new();
    if let Some(last_prof) = ctx.config.and_then(|c| c.profiles.last()) {
        let last_p = profile_provider_name(last_prof);
        if raw.iter().any(|o| o.provider.eq_ignore_ascii_case(last_p)) {
            providers.push(last_p.to_string());
        }
    }
    for o in &raw {
        if !providers
            .iter()
            .any(|seen| seen.eq_ignore_ascii_case(&o.provider))
        {
            providers.push(o.provider.clone());
        }
    }
    let mut out = Vec::with_capacity(raw.len());
    for p in &providers {
        for o in &raw {
            if o.provider.eq_ignore_ascii_case(p) {
                out.push(o.clone());
            }
        }
    }
    out
}

/// Replace the extras, keeping the selection on the same account id when it
/// is still listed, else falling back to the profile rail.
pub(crate) fn set_usage_extras(
    app: &mut App,
    extras: Vec<crate::usage::observation::AccountObservation>,
) {
    let selected = app
        .usage_extra_cursor
        .and_then(|i| app.usage_extras.get(i))
        .map(|o| o.id.clone());
    app.usage_extras = extras;
    app.usage_extra_cursor =
        selected.and_then(|id| app.usage_extras.iter().position(|o| o.id == id));
}

/// Live palette reload: a changed Omarchy theme repaints on the next frame;
/// a malformed one keeps the last palette and toasts once.
fn poll_palette(app: &mut App) {
    let Some(watch) = app.palette_watch.as_mut() else {
        return;
    };
    if let super::theme::PaletteChange::Malformed(why) = watch.tick() {
        app.toast(
            ToastKind::Warning,
            format!("omarchy theme colours unreadable, keeping the last palette\n{why}"),
        );
    }
}

/// Overview `m`: open the move-a-session modal for the selected account, over
/// every live claude session. Codex sessions have no in-session executor, and
/// a row whose session is gone (awaiting GC) is left out by the same liveness
/// probe the live tally reads. Nothing to move toasts instead of opening.
fn open_move_session(app: &mut App) {
    let Some(MainItemKind::Profile(idx)) = app.current_main_item() else {
        return;
    };
    let Some(target) = app.config().profiles.get(idx).map(|p| p.name.to_string()) else {
        return;
    };
    let sessions = move_candidates();
    if sessions.is_empty() {
        app.toast(ToastKind::Info, "no live claude session to move");
        return;
    }
    app.modals.push(Modal::MoveSession(MoveSessionForm {
        scroll: Default::default(),
        viewport: Default::default(),
        starts: Default::default(),
        block_ends: Default::default(),
        total: Default::default(),
        target,
        sessions,
        cursor: 0,
    }));
}

/// The live claude sessions, oldest first, each with the member it is served
/// by and where it is in a switch.
pub(crate) fn move_candidates() -> Vec<MoveCandidate> {
    let mut rows: Vec<crate::live_sessions::LiveSession> = crate::live_sessions::list()
        .into_iter()
        .filter(|row| row.harness == Harness::Claude)
        .filter(|row| {
            let probe =
                ProfileName::from(row.current_member.as_deref().unwrap_or(&row.start_profile));
            crate::runtime::session_row_is_live(&probe, row.isolated, &row.session_id)
        })
        .collect();
    rows.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    rows.iter()
        .map(|row| {
            let ack = crate::live_sessions::read_helper_ack(&row.session_id);
            let view = crate::hot_swap::SwapView::of(row, ack.as_ref());
            MoveCandidate {
                sid: row.session_id.clone(),
                now_on: view
                    .served_member()
                    .unwrap_or(&row.start_profile)
                    .to_string(),
                executor: view.executor.wire().unwrap_or("none"),
                state: view.state,
            }
        })
        .collect()
}

/// Keys on the `m` modal: ↑↓ pick, ⏎ hands the pick to the session request
/// core on a worker (it polls up to five seconds for an api-key commit), esc/q
/// close. The TUI never relaunches: a session that cannot hot-swap gets the
/// terminal command in a toast.
fn handle_move_session_key(app: &mut App, key: KeyEvent) {
    let Some(Modal::MoveSession(form)) = app.modals.last_mut() else {
        return;
    };
    let last = form.sessions.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.modals.pop();
        }
        KeyCode::Up => {
            form.cursor = if form.cursor == 0 {
                last
            } else {
                form.cursor - 1
            }
        }
        KeyCode::Down => {
            form.cursor = if form.cursor >= last {
                0
            } else {
                form.cursor + 1
            }
        }
        KeyCode::PageUp | KeyCode::PageDown => {
            let starts = form.starts.borrow();
            let ends = form.block_ends.borrow();
            form.cursor = if key.code == KeyCode::PageDown {
                page_down_cursor_by_rows(
                    form.cursor,
                    &starts,
                    &ends,
                    form.viewport.get(),
                    form.scroll.get(),
                    form.total.get(),
                )
            } else {
                page_up_cursor_by_rows(
                    form.cursor,
                    &starts,
                    &ends,
                    &[],
                    form.viewport.get(),
                    form.scroll.get(),
                    form.total.get(),
                )
            };
        }
        KeyCode::Enter => {
            let picked = form.sessions.get(form.cursor.min(last)).cloned();
            let target = form.target.clone();
            app.modals.pop();
            let Some(picked) = picked else {
                return;
            };
            // No "already on" shortcut here: `now_on` is the SERVED member,
            // and a session committed elsewhere but not yet served must still
            // be movable back. The request core decides (and withdraws a
            // standing intent on a switch back).
            let tx = app.session_move_tx.clone();
            let sid = picked.sid;
            spawn_worker(move || {
                let outcome = crate::sessions_cli::request_session_switch(
                    &sid,
                    &target,
                    crate::sessions_cli::Surface::Tui,
                )
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(SessionMoveResult {
                    sid,
                    target,
                    outcome,
                });
            });
        }
        _ => {}
    }
}

/// Toast each session-move outcome in the three shapes the hot-swap spec
/// names: swapping, stays on (with the reason), or the relaunch command.
fn drain_session_moves(app: &mut App) {
    use crate::sessions_cli::RequestOutcome;
    while let Ok(SessionMoveResult {
        sid,
        target,
        outcome,
    }) = app.session_move_rx.try_recv()
    {
        let relaunch =
            format!("session {sid}: relaunch with 'tollgate switch {sid} {target} --relaunch'");
        match outcome {
            Ok(request) => match request.outcome {
                RequestOutcome::Committed(_) | RequestOutcome::Requested => app.toast(
                    ToastKind::Success,
                    format!("session {sid}: swapping… onto {target}"),
                ),
                RequestOutcome::IntentRecorded => app.toast(
                    ToastKind::Success,
                    format!("session {sid}: moving onto {target} at its next request"),
                ),
                RequestOutcome::AlreadyOn => app.toast(
                    ToastKind::Info,
                    format!("session {sid} is already on {target}"),
                ),
                RequestOutcome::Refused(code) => app.toast(
                    ToastKind::Warning,
                    format!(
                        "session {sid} stays on {}: {}",
                        request.current,
                        crate::hot_swap::reason_text(&code, None)
                    ),
                ),
                RequestOutcome::RelaunchRequired(_) => app.toast(ToastKind::Warning, relaunch),
            },
            Err(e) => app.toast(ToastKind::Danger, format!("session {sid}: {e}")),
        }
    }
}

/// Step the Overview cursor by `delta` over the profiles, then the read-only
/// rows in display order ([`App::overview_extras`]), wrapping. With no extras
/// this is the plain profile step, unchanged.
fn step_overview_cursor(app: &mut App, delta: i32) {
    let count = app.main_items().len();
    let order = if app.harness_filter.shows_claude() {
        app.overview_extras()
    } else {
        Vec::new()
    };
    let len = count + order.len();
    if len == 0 {
        return;
    }
    let at = match app
        .overview_selected_extra()
        .and_then(|i| order.iter().position(|&o| o == i))
    {
        Some(pos) => count + pos,
        None => app.overview_cursor.min(count.saturating_sub(1)),
    };
    let next = (at as i32 + delta).rem_euclid(len as i32) as usize;
    if next < count {
        app.move_overview_cursor(next);
        app.usage_extra_cursor = None;
        app.sync_overview_profile_selection();
    } else {
        app.usage_extra_cursor = Some(order[next - count]);
    }
}

/// Toast why the Overview's selected read-only row refuses an account action.
/// A no-op off such a row.
fn refuse_read_only(app: &mut App) {
    let Some(hint) = app
        .overview_selected_extra()
        .and_then(|i| app.usage_extras.get(i))
        .map(read_only_hint)
    else {
        return;
    };
    app.toast(ToastKind::Info, hint);
}

/// Plugin tab keymap. List focus: ↑↓ moves the cursor (wrapping over both
/// groups), ⏎ descends to the detail pane, `f` applies the selected row's fix.
/// Detail focus: the herdr detail walks its focusable options rows, every
/// other detail scrolls (clamped by the render pass); `f` still fixes.
fn handle_plugin_key(app: &mut App, key: KeyEvent) {
    match app.plugin.focus {
        PluginFocus::List => {
            let len = app.plugin.row_count();
            match key.code {
                KeyCode::Up if len > 0 => {
                    app.plugin.cursor = (app.plugin.cursor + len - 1) % len;
                    app.plugin.detail_scroll = 0;
                    app.plugin.diagnostic_focus = false;
                }
                KeyCode::Down if len > 0 => {
                    app.plugin.cursor = (app.plugin.cursor + 1) % len;
                    app.plugin.detail_scroll = 0;
                    app.plugin.diagnostic_focus = false;
                }
                KeyCode::PageUp if len > 0 => {
                    app.plugin.cursor = selector_page_up(app.plugin.cursor, &app.plugin.selector);
                    app.plugin.detail_scroll = 0;
                    app.plugin.diagnostic_focus = false;
                }
                KeyCode::PageDown if len > 0 => {
                    app.plugin.cursor = selector_page_down(app.plugin.cursor, &app.plugin.selector);
                    app.plugin.detail_scroll = 0;
                    app.plugin.diagnostic_focus = false;
                }
                KeyCode::Enter if len > 0 => {
                    app.plugin.focus = PluginFocus::Detail;
                    app.plugin.detail_scroll = 0;
                    app.plugin.diagnostic_focus = false;
                }
                KeyCode::Char('f') => apply_plugin_fix(app),
                _ => {}
            }
        }
        PluginFocus::Detail => {
            if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                let max = app.plugin.detail_max_scroll.get();
                let viewport = app.plugin.detail_viewport.get();
                let origin = if app.plugin.diagnostic_focus {
                    app.plugin.detail_scroll.min(max)
                } else {
                    app.plugin.displayed_scroll.get().min(max)
                };
                app.plugin.diagnostic_focus = true;
                app.plugin.detail_scroll = if key.code == KeyCode::PageDown {
                    page_down_scroll(origin as usize, max as usize, viewport as usize) as u16
                } else {
                    page_up_scroll(origin as usize, max as usize, viewport as usize) as u16
                };
            } else if app
                .plugin
                .selected_check()
                .is_some_and(|c| c.label == "herdr")
            {
                handle_herdr_options_key(app, key);
            } else {
                match key.code {
                    KeyCode::Up => {
                        // Clamp before stepping — see the Status detail pane's ↑ arm.
                        let max = app.plugin.detail_max_scroll.get();
                        app.plugin.detail_scroll =
                            app.plugin.detail_scroll.min(max).saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let max = app.plugin.detail_max_scroll.get();
                        app.plugin.detail_scroll =
                            app.plugin.detail_scroll.saturating_add(1).min(max);
                    }
                    KeyCode::PageUp => {
                        let max = app.plugin.detail_max_scroll.get();
                        app.plugin.detail_scroll = page_up_scroll(
                            app.plugin.detail_scroll as usize,
                            max as usize,
                            app.plugin.detail_viewport.get() as usize,
                        ) as u16;
                    }
                    KeyCode::PageDown => {
                        let max = app.plugin.detail_max_scroll.get();
                        app.plugin.detail_scroll = page_down_scroll(
                            app.plugin.detail_scroll as usize,
                            max as usize,
                            app.plugin.detail_viewport.get() as usize,
                        ) as u16;
                    }
                    KeyCode::Char('f') => apply_plugin_fix(app),
                    _ => {}
                }
            }
        }
    }
}

/// Apply the selected row's fix. `WireMcpServers` opens a confirm modal; a
/// diverged active profile re-raises the existing 3-way divergence resolver.
fn apply_plugin_fix(app: &mut App) {
    let Some(fix) = app.plugin.selected_fix().cloned() else {
        return;
    };
    match fix {
        PluginFix::Completions { shell, stale } => {
            if let Some(change) = crate::plugin_host::self_exe_change() {
                app.toast(
                    ToastKind::Danger,
                    own_binary_completions_refusal(change, stale),
                );
                return;
            }
            app.open_modal(Modal::Confirm(ConfirmState {
                message: format!("{} {shell} completions?", if stale { "update" } else { "install" }),
                detail: Some(match shell.as_str() {
                    "bash" => "writes ~/.tollgate/completions/tollgate.bash and adds a source line to ~/.bashrc if it has none.",
                    "zsh" => "writes ~/.tollgate/completions/tollgate.zsh and adds a source line to ~/.zshrc if it has none.",
                    "fish" => "writes ~/.config/fish/completions/tollgate.fish.",
                    _ => "writes this binary's shell completion script and keeps its source line current.",
                }.to_string()),
                choice: false, scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::Completions { shell, stale },
            }));
        }

        PluginFix::WireMcpServers => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: "wire tollgate into claude code's mcpServers?".to_string(),
                detail: Some(
                    "writes the tollgate entry into ~/.claude.json; other fields are preserved."
                        .to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::WireMcpServers,
            }));
        }
        PluginFix::RepairDivergence(name) => {
            app.disarm_quit();
            open_divergence_modal(app, &name);
        }
        PluginFix::RelinkCredentials(name) => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: format!("relink ~/.claude credentials to '{name}'?"),
                detail: Some(
                    "re-points .credentials.json at the account's own stored tokens; spends nothing.".to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::RelinkCredentials(name),
            }));
        }
        PluginFix::HealHerdrConfig(path) => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: "add the keybinding and sidebar row to herdr's config?".to_string(),
                detail: Some(
                    concat!(
                        "writes them into herdr's config.toml and validates the result with ",
                        cmd_lit!("herdr config check"),
                        "."
                    )
                    .to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::HealHerdrConfig(path),
            }));
        }
        PluginFix::InstallPlugin => {
            app.disarm_quit();
            app.modals.push(Modal::Confirm(ConfirmState {
                message: "install the tollgate plugin into claude code?".to_string(),
                detail: Some(
                    "runs claude's own plugin installer at user scope and turns the plugin on; your other plugins and settings are untouched."
                        .to_string(),
                ),
                choice: false,
                scroll: ConfirmScroll::default(),
                on_confirm: ConfirmAction::InstallPlugin,
            }));
        }
    }
}

/// Compare plugin manifest versions numerically; unknown versions prove no drift.
fn plugin_version_older(installed: &str, current: &str) -> bool {
    let parse = |version: &str| -> Option<(u64, u64, u64)> {
        let mut parts = version.trim_start_matches('v').split('.');
        let parsed = (
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(parsed)
    };
    match (parse(installed), parse(current)) {
        (Some(installed), Some(current)) => installed < current,
        _ => false,
    }
}

/// Recompute the Plugin tab's integration checks; the last (`runtime`) folds every
/// profile into one summary. Every read is a local FS/`PATH` check; `claude
/// --version` runs only when `refresh_version` is set or the cached result is
/// absent. Synchronous — no background thread.
fn recompute_plugin_checks(app: &mut App, refresh_version: bool) {
    use crate::plugin_probe as probe;

    app.plugin.error = None;

    // CC version is cached; only `r` probes. Construction and a tab switch
    // leave it unprobed rather than spawning `claude --version` synchronously
    // — construction must not block the first paint. Skipped under test so
    // the suite never spawns the real `claude` binary.
    if refresh_version {
        app.plugin.fetching = true;
        app.plugin.cc_version = Some(if cfg!(test) {
            None
        } else {
            probe::cc_version()
        });
        app.plugin.fetching = false;
    }

    let mut checks: Vec<Check> = Vec::with_capacity(4);

    // about — tollgate's data dir + PATH resolution (CC spawns `tollgate mcp` by
    // name, so resolution is load-bearing) and the Claude Code version. Combined
    // health: tollgate missing is danger (server can't start), CC missing is warn.
    let tollgate_path = probe::on_path(crate::identity::NAME);
    let mut about_detail = vec![format!(
        "data: {}",
        crate::profile::tollgate_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "\u{2014}".to_string())
    )];
    match &tollgate_path {
        Some(path) => about_detail.push(format!("path: {}", path.display())),
        None => {
            about_detail.push("path: not on PATH".to_string());
            about_detail.push(
                "claude code spawns tollgate mcp by name, so the server won't start".to_string(),
            );
            about_detail.push("install tollgate so its bin directory is on PATH".to_string());
        }
    }
    match &app.plugin.cc_version {
        Some(Some(version)) => about_detail.push(format!("claude: {version}")),
        Some(None) => {
            about_detail.push("claude: not found".to_string());
            about_detail.push("claude --version failed or claude is not on PATH".to_string());
            about_detail.push("install claude code so the claude binary resolves".to_string());
        }
        // Unprobed is not missing: the probe is `r`-gated, so before the first
        // `r` the row names the key that fills the line in instead of claiming
        // a binary is absent.
        None => about_detail.push(format!("claude: press {} to probe", prose::key("r"))),
    }
    checks.push(Check {
        label: "about",
        health: if tollgate_path.is_none() {
            Health::Danger
        } else if matches!(app.plugin.cc_version, Some(None)) {
            // Probed and missing is the only version verdict that warns; an
            // unprobed version is not evidence of anything.
            Health::Warn
        } else {
            Health::Ok
        },
        detail: about_detail,
        fix: None,
    });

    // `tollgate mcp` boot self-probe — `r`-gated only (heavier than the other reads:
    // it spawns the real server). Cleared when tollgate no longer resolves so a stale
    // "boots" can't linger. Skipped under test so the suite never boots the server.
    if refresh_version {
        app.plugin.fetching = true;
        app.plugin.mcp_boot = if tollgate_path.is_some() {
            Some(if cfg!(test) {
                probe::McpProbe::Ok
            } else {
                probe::mcp_boots()
            })
        } else {
            None
        };
        app.plugin.fetching = false;
    }
    let mcp_boot = app.plugin.mcp_boot.clone();

    // herdr probe — three subprocesses, so it is `r`-gated like `mcp_boot`.
    // Skipped under test rather than set to a fixed value, so a test that
    // injected a probe keeps it.
    if refresh_version && !cfg!(test) {
        app.plugin.herdr = Some(crate::herdr::probe());
    }

    // "global" == active in every project: a CC `user`-scope plugin install. A
    // `local`/`project` install (or a `./.mcp.json`) binds tollgate to one repo.
    let records = probe::installed_records();
    let installed = !records.is_empty();
    let plugin_global = records.iter().any(|r| r.scope.as_deref() == Some("user"));

    // mcpServers wiring — a plugin install OR a manual `mcpServers.tollgate` entry.
    // Globally wired = a `user`-scope plugin or the `~/.claude.json` entry; a
    // project-scope plugin or a `./.mcp.json` wires this repo only, so it warns and
    // offers the same global write fix as a missing wiring does.
    let wiring = probe::manual_mcp_wiring();
    let wired = installed || wiring != probe::McpWiring::None;
    let manual_global = wiring == probe::McpWiring::GlobalConfig;
    // A manual `~/.claude.json` entry whose command/args no longer match the
    // canonical launch line reads as wired but won't start the current server.
    // Only the operative manual entry matters — a `user`-scope plugin install
    // supersedes it, so drift under one is moot.
    let drifted = manual_global && !plugin_global && probe::global_entry_drifted() == Some(true);
    let globally_wired = plugin_global || (manual_global && !drifted);
    let project_only = wired && !globally_wired && !drifted;
    let source = if plugin_global {
        "source: plugin install (user)"
    } else if manual_global {
        "source: ~/.claude.json (manual)"
    } else if installed {
        "source: plugin install (project)"
    } else if wiring == probe::McpWiring::ProjectFile {
        "source: ./.mcp.json (manual)"
    } else {
        "source: none"
    };
    let mut mcp_detail = vec![
        format!("present: {}", if wired { "yes" } else { "no" }),
        source.to_string(),
    ];
    match &mcp_boot {
        Some(probe::McpProbe::Ok) => mcp_detail.push("server: boots".to_string()),
        Some(probe::McpProbe::Failed(reason)) => {
            mcp_detail.push(format!("server: failed ({reason})"));
        }
        None => {}
    }
    let needs_wire = !globally_wired || drifted;
    if needs_wire {
        mcp_detail.push(String::new());
        if drifted {
            mcp_detail.push("entry doesn't match the current launch line".to_string());
        } else if project_only {
            mcp_detail.push("wired for this project only, not global".to_string());
        }
        mcp_detail.push("[f] wire mcpServers into ~/.claude.json".to_string());
    }
    let boot_failed = matches!(mcp_boot, Some(probe::McpProbe::Failed(_)));
    checks.push(Check {
        label: "mcp servers",
        health: if boot_failed {
            Health::Danger
        } else if needs_wire {
            Health::Warn
        } else {
            Health::Ok
        },
        detail: mcp_detail,
        fix: needs_wire.then_some(PluginFix::WireMcpServers),
    });

    // plugin install record — installed-only verdict (CC exposes no clean per-scope
    // "enabled" boolean, so v1 reports presence + scope, not enabled/disabled).
    let completions = crate::completions::detected_installed_script();
    let mut plugin_check = plugin_install_check(completions);
    if let Some(change) = app.daemon_binary_change {
        plugin_check
            .detail
            .push(daemon_binary_card_line(change).to_string());
        plugin_check.health = worst(plugin_check.health, Health::Warn);
    }
    if let Some(change) = crate::plugin_host::self_exe_change() {
        plugin_check
            .detail
            .push(own_binary_install_refusal(change).to_string());
        plugin_check.health = worst(plugin_check.health, Health::Warn);
    }
    // Native Plugin retains separate MCP/about rows and install/completion health.
    checks.push(plugin_check);

    // herdr — the installed herdr's tollgate plugin + keybinding/sidebar config.
    // The probe is cached (`r`-gated, three subprocesses); the config read is a
    // cheap `fs::read_to_string` that rides the tick, using the path the probe
    // already resolved. No row when herdr does not resolve or was never probed.
    if let Some(Some(probe)) = &app.plugin.herdr {
        let config = probe
            .config_path
            .as_deref()
            .map(crate::herdr::read_config)
            .and_then(Result::ok)
            .map(|text| crate::herdr::config_status(&text));
        checks.push(herdr_check(probe, config.as_ref()));
        // The options section reads this verdict (writable vs inert) off the
        // cache — recompute already paid for the file read, so neither the
        // render nor the key handler re-reads per frame.
        app.plugin.herdr_config = config;
    } else {
        // No herdr row renders without a probe, so its verdict must not
        // outlive the probe (a stale writable verdict would arm the
        // delegate-row confirm against a file that no longer resolves).
        app.plugin.herdr_config = None;
    }

    // runtime — fold every profile's live sessions / credential link / token
    // freshness into one summary row. Snapshot the names under the config lock,
    // then drop it before the FS reads (`classify_credentials_link`) so no lock
    // is held across I/O.
    struct Snap {
        name: ProfileName,
        active: bool,
        expires_at: Option<i64>,
    }
    let snaps: Vec<Snap> = {
        let cfg = app.config();
        cfg.profiles
            .iter()
            .map(|p| Snap {
                name: p.name.clone(),
                active: cfg.is_active(&p.name),
                expires_at: p.access_token_expires_at(),
            })
            .collect()
    };

    let now_secs = (crate::usage::now_ms() / 1000) as i64;
    let total = snaps.len();
    let mut live_sessions: usize = 0;
    let mut live_profiles: usize = 0;
    let mut live_names: Vec<String> = Vec::new();
    let mut rate_limited_names: Vec<String> = Vec::new();
    // The active profile's link readout plus the one fix it can offer. Divergence
    // and missing-link are meaningful only for the active profile — its creds are
    // the ones linked into ~/.claude — so non-active profiles only contribute
    // their live-session and rate-limit signal.
    let mut active_name: Option<String> = None;
    let mut active_link = "\u{2014}";
    let mut active_expires = "\u{2014}".to_string();
    let mut active_fix: Option<PluginFix> = None;
    let mut active_bad = false; // diverged / missing / unknown link

    // `r` means re-probe everything, so it re-collects rather than rendering the
    // age the tick left. Sited next to the read, not at the top: the version
    // probe above spawns `claude`, and a tally taken before that is already as
    // stale as the subprocess is slow.
    if refresh_version {
        app.last_live_sessions_refresh = None;
        poll_live_sessions(app);
    }

    // Otherwise the tick's fleet tally, never a second sweep: this row and the
    // Overview's `live` column answer one question, and two independent reads of
    // it can disagree inside a frame.
    let tally = &app.live_sessions;
    for snap in snaps {
        let instances = tally.member(&snap.name).sessions;
        live_sessions += instances;
        if instances > 0 {
            live_profiles += 1;
            live_names.push(if instances > 1 {
                format!("{} · {instances}", snap.name)
            } else {
                snap.name.to_string()
            });
        }
        // Observed delegate throughput (MCP `delegate`); a recent rate-limit on any
        // exercised model warns even when the credential link is healthy.
        let throughput = crate::throughput::summary(&snap.name, now_secs);
        if throughput.iter().any(|t| t.rate_limited_recent) {
            rate_limited_names.push(snap.name.to_string());
        }

        if !snap.active {
            continue;
        }
        active_name = Some(snap.name.to_string());

        // A classify error (broken symlink mid-read, perms) must not read as
        // healthy: surface it as a warn with an `unknown` link label rather than
        // silently dropping to idle/ok.
        let link_result = classify_credentials_link(&snap.name);
        let link = link_result.as_ref().ok().copied();
        let link_err = link_result.is_err();
        let diverged = matches!(link, Some(LinkState::Diverged));
        let missing = matches!(link, Some(LinkState::Missing));
        // A `missing` link is repairable only when the profile still holds stored
        // creds to relink to; with none it needs a fresh login, not a relink.
        let stored_creds = crate::claude::install_source_path(&snap.name)
            .map(|p| p.exists())
            .unwrap_or(false);

        active_link = if link_err {
            "unknown"
        } else {
            match link {
                Some(LinkState::LinkedTo) => "linked",
                Some(LinkState::Diverged) => "diverged",
                Some(LinkState::Missing) => "missing",
                None => "\u{2014}",
            }
        };
        active_bad = link_err || diverged || missing;
        active_fix = if diverged {
            Some(PluginFix::RepairDivergence(snap.name.to_string()))
        } else if missing && stored_creds {
            Some(PluginFix::RelinkCredentials(snap.name.to_string()))
        } else {
            None
        };
        // Access-token freshness as a relative span; `—` when no OAuth expiry is
        // known (third-party / api-key profiles).
        active_expires = match snap.expires_at {
            Some(ms) => {
                let secs = ms / 1000 - (crate::usage::now_ms() / 1000) as i64;
                if secs <= 0 {
                    "expired".to_string()
                } else {
                    crate::usage::humanize_duration(secs)
                }
            }
            None => "\u{2014}".to_string(),
        };
    }

    // Health: a bad active link (diverged/missing/unknown) or any recent delegate
    // rate-limit warns; an active `linked` creds link or any live session is ok;
    // otherwise the fleet is idle (neutral, not green).
    let runtime_health = if active_bad || !rate_limited_names.is_empty() {
        Health::Warn
    } else if active_link == "linked" || live_sessions > 0 {
        Health::Ok
    } else {
        Health::Idle
    };

    let link_line = match &active_name {
        Some(_) if active_expires != "\u{2014}" => format!("{active_link} · {active_expires}"),
        Some(_) => active_link.to_string(),
        None => "\u{2014}".to_string(),
    };
    // Runtime health only — config (type / model / overrides) lives on the Setup
    // tab. This row answers "how many live sessions, is the active credential link
    // healthy, and how fresh is its token?".
    let mut runtime_detail = vec![format!("accounts: {total}")];
    // A zero is hidden rather than printed, matching the Overview cell and the
    // Fallback card — the row says nothing when there is nothing to say. `live`
    // is the one noun every session-counting surface uses (the Overview column
    // header, the Fallback card's key), so the figure reads the same everywhere.
    if live_sessions > 0 {
        runtime_detail.push(format!(
            "live: {live_sessions} across {live_profiles} account{}",
            crate::format::plural(live_profiles)
        ));
    }
    // Name each account carrying a live session as an indented sub-line, so the
    // spread is concrete rather than just a tally.
    for name in &live_names {
        runtime_detail.push(format!("  {name}"));
    }
    runtime_detail.push(format!(
        "active: {}",
        active_name.as_deref().unwrap_or("\u{2014}")
    ));
    runtime_detail.push(format!("link: {link_line}"));
    if !rate_limited_names.is_empty() {
        // "rate-limited" sits in the value so `value_tone` warns on it (the key is
        // a plain label).
        runtime_detail.push(format!(
            "delegate: rate-limited ({})",
            rate_limited_names.join(", ")
        ));
    }
    match &active_fix {
        Some(PluginFix::RepairDivergence(_)) => {
            runtime_detail.push(String::new());
            runtime_detail.push("[f] repair credentials".to_string());
        }
        Some(PluginFix::RelinkCredentials(_)) => {
            runtime_detail.push(String::new());
            runtime_detail.push("[f] relink credentials".to_string());
        }
        // The other fixes are produced on their own check rows (wire / herdr /
        // install), never on this one; `None` is a healthy or inactive link.
        // Named, so a new `PluginFix` variant fails the build instead of a
        // silent no-op.
        Some(
            PluginFix::WireMcpServers
            | PluginFix::HealHerdrConfig(_)
            | PluginFix::InstallPlugin
            | PluginFix::Completions { .. },
        ) => {}
        None => {}
    }
    checks.push(Check {
        label: "runtime",
        health: runtime_health,
        detail: runtime_detail,
        fix: active_fix,
    });

    let plugin_health = checks
        .iter()
        .find(|c| c.label == "plugin")
        .map(|c| c.health);
    let herdr_health = checks.iter().find(|c| c.label == "herdr").map(|c| c.health);
    for (previous, current) in [
        (app.plugin.plugin_health, plugin_health),
        (app.plugin.herdr_health, herdr_health),
    ] {
        if previous.is_some()
            && previous != current
            && let Some(kind) = current.and_then(health_activity)
        {
            app.set_tab_activity(Tab::Plugin, kind);
        }
    }
    app.plugin.plugin_health = plugin_health;
    app.plugin.herdr_health = herdr_health;
    app.plugin.checks = checks;

    // The delegates pane's data. Read here rather than on its own timer so it
    // rides the cadence this tab already documents (tab focus, `r`, and the 1 s
    // tick while focused) — a `tollgate mcp` run is a different process, so a
    // watcher would buy freshness this tab has never promised.
    //
    // `list_banded`, the same call `tollgate jobs` and `monitor`'s listing make,
    // so the pane's row order is not a second derivation of one. It arrives
    // banded and the renderer sorts nothing.
    app.plugin.delegates = crate::mcp::jobs::list_banded(crate::usage::now_ms());

    // Keep the cursor in range after the check set changes.
    let max = app.plugin.row_count().saturating_sub(1);
    if app.plugin.cursor > max {
        app.plugin.cursor = max;
    }
}

/// Read the focused account identity once without holding the config lock.
fn start_verify_login(app: &mut App) {
    if app.verify_login_inflight.is_some() {
        return;
    }
    let Some((name, true, _)) = focused_account(app) else {
        return;
    };
    let tx = app.verify_login_tx.clone();
    app.verify_login_inflight = Some(name.clone());
    app.toast(ToastKind::Info, format!("verifying '{name}'"));
    spawn_worker(move || {
        // spawn_worker's outer catch cannot send an outcome; this catch always
        // re-arms the action even if the probe itself panics.
        let outcome = catch_unwind(AssertUnwindSafe(|| crate::usage::verify_login(&name)))
            .unwrap_or(crate::usage::VerifyOutcome::Network);
        let _ = tx.send((name, outcome));
    });
}

/// Identity details stay on the UI thread and only in this explicit toast.
fn drain_verify_login(app: &mut App) {
    use crate::usage::{ReconcileOutcome, VerifyOutcome};
    while let Ok((name, outcome)) = app.verify_login_rx.try_recv() {
        app.verify_login_inflight = None;
        let (kind, body) = match outcome {
            VerifyOutcome::Verified {
                identity,
                reconcile,
            } => {
                let account: String = identity.account_uuid.chars().take(8).collect();
                let email = identity
                    .email
                    .as_deref()
                    .and_then(crate::profile_cache::sanitize_identity_detail);
                let organization_uuid = identity
                    .organization_uuid
                    .as_deref()
                    .and_then(crate::profile_cache::sanitize_identity_detail);
                let details = email.as_ref().map_or_else(
                    || format!("acct {account}"),
                    |email| format!("{email} · acct {account}"),
                );
                let previous = match &reconcile {
                    ReconcileOutcome::Replaced {
                        previous: Some(previous),
                    } if previous != &identity.account_uuid => Some(previous),
                    _ => None,
                };
                let was = previous.map_or_else(String::new, |old| {
                    format!(" (was {})", old.chars().take(8).collect::<String>())
                });
                let duplicate = {
                    let cfg = app.config();
                    crate::profile_cache::same_account_as(&cfg, &name).map(|other| {
                        let other_has_login = cfg.find(&other)
                            .is_some_and(|p| p.credentials.is_some());
                        let other_org = match crate::profile_cache::login_identity(&other) {
                            crate::profile_cache::LoginIdentity::Anchored {
                                details: Some(details), ..
                            } => details.organization_uuid.as_deref()
                                .and_then(crate::profile_cache::sanitize_identity_detail),
                            _ => None,
                        };
                        let reason = if !other_has_login {
                            format!("'{other}' is logged out but kept its old anchor · log it in or delete it")
                        } else {
                            match (organization_uuid.as_ref(), other_org.as_ref()) {
                                (Some(org), Some(other_org)) if org != other_org =>
                                    "different organization · quotas may differ; auto-switch still skips one".to_string(),
                                (None, _) | (_, None) =>
                                    "same Anthropic account · likely one quota · re-login one with the intended account".to_string(),
                                _ => "both draw on one quota · re-login one with the intended account".to_string(),
                            }
                        };
                        format!("'{name}' is the same Anthropic account as '{other}'\n{reason}{was}")
                    })
                };
                match reconcile {
                    ReconcileOutcome::WriteFailed => (
                        ToastKind::Warning,
                        format!(
                            "could not save the identity of '{name}'\n{details} · see tollgate.log"
                        ),
                    ),
                    ReconcileOutcome::NoUuid => (
                        ToastKind::Warning,
                        format!("could not verify '{name}'\nprofile identity unreadable"),
                    ),
                    _ if duplicate.is_some() => (ToastKind::Warning, duplicate.unwrap_or_default()),
                    _ if previous.is_some() => (
                        ToastKind::Warning,
                        format!("'{name}' is now a different account\n{details}{was}"),
                    ),
                    _ => (ToastKind::Success, format!("verified '{name}'\n{details}")),
                }
            }
            VerifyOutcome::StoreMoved => (
                ToastKind::Info,
                format!("'{name}' changed login during verify · nothing saved"),
            ),
            failure => {
                let reason = match failure {
                    VerifyOutcome::NoStoredLogin => "no stored login".to_string(),
                    VerifyOutcome::Expired => "access token expired · refresh it on Usage (r), then verify".to_string(),
                    VerifyOutcome::Refused => "stored access token refused (401) · refresh it on Usage (r); re-login if that fails".to_string(),
                    VerifyOutcome::NoProfileScope => "stored access token has no profile scope (403)".to_string(),
                    VerifyOutcome::RateLimited(retry_after) => retry_after.map_or_else(
                        || "rate limited (429) · try again later".to_string(),
                        |delay| {
                            let seconds = delay.as_secs()
                                .saturating_add(u64::from(delay.subsec_nanos() != 0));
                            format!("rate limited (429) · try again in ~{seconds}s")
                        },
                    ),
                    VerifyOutcome::Network => "profile request failed · try again".to_string(),
                    VerifyOutcome::Unreadable => "profile identity unreadable".to_string(),
                    VerifyOutcome::Verified { .. } | VerifyOutcome::StoreMoved => unreachable!(),
                };
                (
                    ToastKind::Warning,
                    format!("could not verify '{name}'\n{reason}"),
                )
            }
        };
        app.toast(kind, body);
    }
}

fn poll_plugin_refresh(app: &mut App) {
    if !app.modals.is_empty() || app.last_plugin_refresh.elapsed() < Duration::from_secs(1) {
        return;
    }
    app.last_plugin_refresh = Instant::now();
    recompute_plugin_checks(app, false);
}

fn toast_duplicate_login(app: &mut App, name: &ProfileName) {
    let warning = crate::profile_cache::duplicate_login_warning(&app.config(), name);
    if let Some(warning) = warning {
        app.toast(ToastKind::Warning, warning);
    }
}

/// Refuse operations on the upstream-owned global slots before spending a login token.
fn guest_refused(app: &mut App) -> bool {
    if crate::identity::upstream_active() {
        app.toast(ToastKind::Danger, crate::identity::GUEST_REFUSAL);
        true
    } else {
        false
    }
}
#[cfg(test)]
#[path = "../../tests/inline/tui_guest.rs"]
mod guest_tests;

#[cfg(test)]
fn dispatch_action_menu_action(app: &mut App, action: ActionMenuAction) {
    let identity = focused_account(app).map(|(name, _, _)| RowIdentity::Profile(name));
    dispatch_menu_pick(app, action, identity);
}

fn poll_form_predicates(app: &mut App) {
    if app
        .last_form_predicates_refresh
        .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
    {
        return;
    }
    app.last_form_predicates_refresh = Some(Instant::now());
    app.refresh_form_predicates();
}
fn stderr_display_lines(text: &str) -> Vec<String> {
    strip_ansi_csi(text).lines().map(escape_control).collect()
}

const UPSTREAM_ACCOUNT_HINT: &str = "read-only — managed by clauth; import to manage it here";

#[cfg(test)]
thread_local! {
    static TEST_MONITOR_HTTP: std::cell::RefCell<
        Option<Arc<dyn crate::usage::monitor::source::MonitorHttp + Send + Sync>>,
    > = const { std::cell::RefCell::new(None) };
    static DELETE_WORKER_PANICS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_BARE_ITEM_SOURCE: std::cell::Cell<Option<BareItemSource>> = const { std::cell::Cell::new(None) };
}
