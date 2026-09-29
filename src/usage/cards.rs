//! The metric-card layout (plan v3.1 §4.5), built only from
//! [`AccountObservation`]s and [`super::derive`], and sink-agnostic: every
//! line is a list of [`Seg`]s carrying a semantic [`Ink`], which the CLI turns
//! into ANSI (`usage::pretty`) and the TUI into ratatui spans
//! (`tui::render::cards`). One layout, so the two surfaces cannot drift.
//!
//! An account renders as a header line, its status lines (stale, failure),
//! then one card per quota window and money meter:
//!
//! ```text
//! work · Anthropic · Max 20x · ● active
//!   Session · 5h                                 resets in 3h 05m (13:53)
//!   ████████████████│░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░         42% ↑
//!   40% elapsed · 2 pts ahead
//!   Balance                                                   $13.67 left
//!   Spend                       today $0.00 · week $4.08 · month $4.46
//! ```

use super::derive::{
    Severity, SeverityBasis, countdown, format_money, hhmm_at_offset, meter_severity,
    severity_from_used_pct, window_pace, window_severity,
};
use super::observation::{
    AccountObservation, Amount, FailureKind, Freshness, MoneyKind, MoneyMeter, Origin, PeriodKind,
    QuotaWindow, ScopeOrigin, WINDOW_MONTH, WINDOW_SESSION, WINDOW_WEEKLY,
    WINDOW_WEEKLY_MODEL_PREFIX,
};

/// A semantic colour; the sink maps it through the palette
/// (`tui::theme::ink_color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ink {
    Text,
    Dim,
    Faint,
    Accent,
    /// The active-account marker (accent_2).
    Active,
    /// A bar's empty track and rules.
    Track,
    Warning,
    Danger,
    /// A severity rung's colour.
    Sev(Severity),
}

/// One run of text in one ink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seg {
    pub(crate) text: String,
    pub(crate) ink: Ink,
    pub(crate) bold: bool,
}

impl Seg {
    pub(crate) fn new(text: impl Into<String>, ink: Ink) -> Self {
        Self {
            text: text.into(),
            ink,
            bold: false,
        }
    }

    pub(crate) fn bold(text: impl Into<String>, ink: Ink) -> Self {
        Self {
            text: text.into(),
            ink,
            bold: true,
        }
    }
}

/// One rendered line.
pub(crate) type CardLine = Vec<Seg>;

/// What a layout pass needs besides the observations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CardCtx {
    /// Columns available; clamped to [`MIN_WIDTH`]..=[`MAX_WIDTH`].
    pub(crate) width: usize,
    /// The clock countdowns and ages judge at.
    pub(crate) now_secs: i64,
    /// The local zone's UTC offset for the `(HH:MM)` stamps.
    pub(crate) offset_secs: i32,
    /// Upstream clauth owns `~/.claude` (plan §4.0).
    pub(crate) guest_mode: bool,
}

/// Narrowest layout; below it lines simply overflow.
pub(crate) const MIN_WIDTH: usize = 40;
/// Widest layout; a wider terminal leaves the rest blank.
pub(crate) const MAX_WIDTH: usize = 160;
/// Left indent of every card row.
const INDENT: usize = 2;
/// The value column right of a bar: fits `100% ↑ CRITICAL`.
const VALUE_W: usize = 15;

impl CardCtx {
    fn w(&self) -> usize {
        self.width.clamp(MIN_WIDTH, MAX_WIDTH)
    }
}

/// Display width of a segment list (every glyph used here is one cell).
pub(crate) fn line_width(line: &[Seg]) -> usize {
    line.iter().map(|s| s.text.chars().count()).sum()
}

/// The line as plain text, trailing blanks trimmed.
pub(crate) fn plain_text(line: &[Seg]) -> String {
    let s: String = line.iter().map(|s| s.text.as_str()).collect();
    s.trim_end().to_string()
}

/// `left` at the indent, `right` flush to `width`, at least one blank between.
fn lr(left: Vec<Seg>, right: Vec<Seg>, width: usize) -> CardLine {
    let used = INDENT + line_width(&left) + line_width(&right);
    let gap = width.saturating_sub(used).max(1);
    let mut out = vec![Seg::new(" ".repeat(INDENT), Ink::Text)];
    out.extend(left);
    out.push(Seg::new(" ".repeat(gap), Ink::Text));
    out.extend(right);
    out
}

/// An indented line.
fn indented(segs: Vec<Seg>) -> CardLine {
    let mut out = vec![Seg::new(" ".repeat(INDENT), Ink::Text)];
    out.extend(segs);
    out
}

/// A whole percent: `42%`. Rounded; the exact figure is in `--json`.
fn pct_text(pct: f64) -> String {
    format!("{:.0}%", pct)
}

/// Bar cells: `used_pct` filled (`█`, `fill` ink) over the `░` track, with the
/// elapsed marker `│` at `marker_pct` when known. Width is exact.
pub(crate) fn bar_segs(
    used_pct: Option<f64>,
    marker_pct: Option<f64>,
    width: usize,
    fill: Ink,
) -> Vec<Seg> {
    if width == 0 {
        return Vec::new();
    }
    let frac = used_pct.map_or(0.0, |p| (p / 100.0).clamp(0.0, 1.0));
    let filled = ((frac * width as f64).round() as usize).min(width);
    let marker = marker_pct
        .map(|p| (((p / 100.0).clamp(0.0, 1.0) * width as f64).floor() as usize).min(width - 1));
    let mut segs: Vec<Seg> = Vec::new();
    let mut push = |text: &str, ink: Ink| match segs.last_mut() {
        Some(last) if last.ink == ink && !last.bold => last.text.push_str(text),
        _ => segs.push(Seg::new(text, ink)),
    };
    for col in 0..width {
        if marker == Some(col) {
            push("│", Ink::Text);
        } else if col < filled {
            push("█", fill);
        } else {
            push("░", Ink::Track);
        }
    }
    segs
}

/// A bar row: the bar across the card, then `value` right-aligned in the
/// value column.
fn bar_row(
    used_pct: Option<f64>,
    marker_pct: Option<f64>,
    fill: Ink,
    value: Vec<Seg>,
    width: usize,
) -> CardLine {
    let bar_w = width.saturating_sub(INDENT + 1 + VALUE_W).max(8);
    let pad = VALUE_W.saturating_sub(line_width(&value));
    let mut line = indented(bar_segs(used_pct, marker_pct, bar_w, fill));
    line.push(Seg::new(" ".repeat(1 + pad), Ink::Text));
    line.extend(value);
    line
}

/// The ink a severity paints (`Dim` when ungraded).
fn sev_ink(sev: Option<Severity>) -> Ink {
    sev.map_or(Ink::Dim, Ink::Sev)
}

/// ` HIGH` / ` LOW` / ` CRITICAL` for the upper two rungs — colour never
/// carries severity alone (plan §3.3).
fn sev_word(sev: Option<Severity>, basis: SeverityBasis) -> Option<Seg> {
    sev.filter(|s| *s >= Severity::High)
        .map(|s| Seg::bold(format!(" {}", s.word(basis)), Ink::Sev(s)))
}

/// A window's human label: `Session · 5h`, `Weekly · 7d`, `Weekly · 7d opus`,
/// `Monthly · 30d`, else the source's own label.
pub(crate) fn window_title(w: &QuotaWindow) -> String {
    if w.id == WINDOW_SESSION {
        format!("Session · {}", w.label)
    } else if w.id == WINDOW_WEEKLY || w.id.starts_with(WINDOW_WEEKLY_MODEL_PREFIX) {
        format!("Weekly · {}", w.label)
    } else if w.id == WINDOW_MONTH {
        format!("Monthly · {}", w.label)
    } else {
        w.label.clone()
    }
}

/// `resets in 3h 05m (13:53)`, `resets now`, or `no reset time`.
fn reset_segs(w: &QuotaWindow, ctx: &CardCtx) -> Vec<Seg> {
    let Some(at) = w.resets_at else {
        return vec![Seg::new("no reset time", Ink::Faint)];
    };
    let left = countdown(Some(at.secs() - ctx.now_secs));
    if left == "now" {
        return vec![Seg::new("resets now", Ink::Dim)];
    }
    vec![
        Seg::new("resets in ", Ink::Faint),
        Seg::new(left, Ink::Dim),
        Seg::new(
            format!(" ({})", hhmm_at_offset(at, ctx.offset_secs)),
            Ink::Faint,
        ),
    ]
}

/// A figure in a provider unit: `1200`, `12.5`.
fn unit_text(n: f64) -> String {
    crate::format::format_amount(n)
}

/// The three-row metric card of one quota window.
pub(crate) fn window_card(w: &QuotaWindow, ctx: &CardCtx) -> Vec<CardLine> {
    let width = ctx.w();
    let sev = window_severity(w);
    let pace = window_pace(w, ctx.now_secs);
    let mut value: Vec<Seg> = vec![match w.used_pct {
        Some(p) => Seg::bold(pct_text(p), sev_ink(sev)),
        None => Seg::new("—", Ink::Faint),
    }];
    if let Some(p) = pace {
        value.push(Seg::new(format!(" {}", p.glyph()), Ink::Dim));
    }
    value.extend(sev_word(sev, SeverityBasis::Usage));
    let marker = w
        .resets_at
        .zip(w.window_secs)
        .map(|(at, len)| super::derive::elapsed_pct(at, len, ctx.now_secs));
    let used_for_bar = if w.exhausted {
        Some(w.used_pct.unwrap_or(100.0).max(100.0))
    } else {
        w.used_pct
    };
    let mut rows = vec![
        lr(
            vec![Seg::bold(window_title(w), Ink::Text)],
            reset_segs(w, ctx),
            width,
        ),
        bar_row(used_for_bar, marker, sev_ink(sev), value, width),
    ];
    let mut foot: Vec<Seg> = Vec::new();
    let sep = |foot: &mut Vec<Seg>| {
        if !foot.is_empty() {
            foot.push(Seg::new(" · ", Ink::Faint));
        }
    };
    if w.exhausted {
        foot.push(Seg::bold("exhausted", Ink::Danger));
    }
    if let Some(p) = pace {
        sep(&mut foot);
        foot.push(Seg::new(
            format!("{:.0}% elapsed · {}", p.elapsed_pct.floor(), p.label()),
            Ink::Faint,
        ));
    }
    if let (Some(used), Some(limit)) = (w.used, w.limit) {
        sep(&mut foot);
        foot.push(Seg::new(
            format!("{} of {} used", unit_text(used), unit_text(limit)),
            Ink::Faint,
        ));
    } else if let Some(used) = w.used {
        sep(&mut foot);
        foot.push(Seg::new(format!("{} used", unit_text(used)), Ink::Faint));
    }
    if !foot.is_empty() {
        rows.push(indented(foot));
    }
    rows
}

/// The ids folded into the one `Spend  today · week · month` row.
const SPEND_GROUP: [&str; 4] = [
    "spend.daily",
    "spend.weekly",
    "spend.monthly",
    "spend.lifetime",
];

fn period_word(kind: PeriodKind) -> &'static str {
    match kind {
        PeriodKind::Daily => "today",
        PeriodKind::Weekly => "week",
        PeriodKind::Monthly => "month",
        PeriodKind::Lifetime => "all time",
        PeriodKind::Custom => "period",
    }
}

fn period_adverb(m: &MoneyMeter) -> Option<&'static str> {
    m.period.map(|p| match p.kind {
        PeriodKind::Daily => "daily",
        PeriodKind::Weekly => "weekly",
        PeriodKind::Monthly => "monthly",
        PeriodKind::Lifetime => "lifetime",
        PeriodKind::Custom => "per period",
    })
}

/// `Spend      today $0.00 · week $4.08 · month $4.46`.
fn spend_group_row(meters: &[&MoneyMeter], width: usize) -> CardLine {
    let mut right: Vec<Seg> = Vec::new();
    for m in meters {
        if !right.is_empty() {
            right.push(Seg::new(" · ", Ink::Faint));
        }
        let word = m.period.map_or("", |p| period_word(p.kind));
        right.push(Seg::new(format!("{word} "), Ink::Faint));
        right.push(Seg::bold(format_money(&m.amount, &m.currency), Ink::Text));
    }
    lr(vec![Seg::bold("Spend", Ink::Text)], right, width)
}

/// Used share of a cap, percent, from what is left: `(cap − left) / cap`.
fn used_share(left: &Amount, cap: &Amount) -> Option<f64> {
    let c = cap.to_f64();
    (c > 0.0).then(|| (c - left.to_f64()) / c * 100.0)
}

/// The cards of one money meter (the spend group is handled by the caller).
fn meter_card(m: &MoneyMeter, width: usize) -> Vec<CardLine> {
    let label = vec![Seg::bold(m.label.clone(), Ink::Text)];
    let money = |a: &Amount| format_money(a, &m.currency);
    match m.kind {
        MoneyKind::Balance => {
            let graded = meter_severity(m);
            let sev = graded.map(|(s, _)| s);
            if !m.additive {
                // A derived part (granted, topped up): informational, never summed.
                return vec![lr(
                    vec![Seg::new(m.label.clone(), Ink::Dim)],
                    vec![Seg::new(money(&m.amount), Ink::Dim)],
                    width,
                )];
            }
            let mut right = vec![
                Seg::bold(money(&m.amount), sev.map_or(Ink::Text, Ink::Sev)),
                Seg::new(" left", Ink::Dim),
            ];
            right.extend(sev_word(sev, SeverityBasis::Balance));
            let mut rows = vec![lr(label, right, width)];
            if let Some(cap) = &m.limit
                && let Some(used) = used_share(&m.amount, cap)
            {
                let value = vec![
                    Seg::bold(pct_text(used), sev_ink(sev)),
                    Seg::new(" used", Ink::Dim),
                ];
                rows.push(bar_row(Some(used), None, sev_ink(sev), value, width));
                rows.push(indented(vec![Seg::new(
                    format!("of {}", money(cap)),
                    Ink::Faint,
                )]));
            }
            rows
        }
        MoneyKind::Limit | MoneyKind::Budget => {
            let sev = meter_severity(m).map(|(s, _)| s);
            let mut right = vec![
                Seg::bold(money(&m.amount), Ink::Text),
                Seg::new(" left", Ink::Dim),
            ];
            if let Some(cap) = &m.limit {
                right.push(Seg::new(" of ", Ink::Dim));
                right.push(Seg::new(money(cap), Ink::Text));
            }
            let mut rows = vec![lr(label, right, width)];
            let used = m.limit.as_ref().and_then(|cap| used_share(&m.amount, cap));
            let used = if m.amount <= Amount::zero() {
                Some(used.unwrap_or(100.0).max(100.0))
            } else {
                used
            };
            if used.is_some() {
                let mut value = vec![
                    Seg::bold(pct_text(used.unwrap_or(0.0)), sev_ink(sev)),
                    Seg::new(" used", Ink::Dim),
                ];
                value.extend(sev_word(sev, SeverityBasis::Usage));
                rows.push(bar_row(used, None, sev_ink(sev), value, width));
            }
            let origin = match (m.kind, m.scope_origin) {
                (MoneyKind::Budget, _) => "your budget",
                (_, ScopeOrigin::MonitoringCredential { .. }) => "limit via monitoring key",
                _ => "provider limit",
            };
            let mut foot = origin.to_string();
            if let Some(adv) = period_adverb(m) {
                foot.push_str(&format!(" · {adv}"));
            }
            rows.push(indented(vec![Seg::new(foot, Ink::Faint)]));
            rows
        }
        MoneyKind::Spend => {
            let period = period_adverb(m)
                .map(|a| format!(" {a}"))
                .unwrap_or_default();
            match &m.limit {
                Some(cap) if m.period.is_none_or(|p| p.kind != PeriodKind::Lifetime) => {
                    let used =
                        (cap.to_f64() > 0.0).then(|| m.amount.to_f64() / cap.to_f64() * 100.0);
                    let ink = sev_ink(used.map(severity_from_used_pct));
                    let right = vec![
                        Seg::bold(money(&m.amount), Ink::Text),
                        Seg::new(" of ", Ink::Dim),
                        Seg::new(money(cap), Ink::Text),
                        Seg::new(period, Ink::Faint),
                    ];
                    let mut rows = vec![lr(label, right, width)];
                    if let Some(u) = used {
                        let value = vec![Seg::bold(pct_text(u), ink), Seg::new(" used", Ink::Dim)];
                        rows.push(bar_row(Some(u), None, ink, value, width));
                    }
                    rows
                }
                _ => vec![lr(
                    label,
                    vec![
                        Seg::bold(money(&m.amount), Ink::Text),
                        Seg::new(" spent", Ink::Dim),
                        Seg::new(period, Ink::Faint),
                    ],
                    width,
                )],
            }
        }
    }
}

/// Every money card of an account, in source order, with the
/// daily / weekly / monthly / lifetime spend meters folded into one row where
/// the first of them stood.
pub(crate) fn money_cards(obs: &AccountObservation, ctx: &CardCtx) -> Vec<CardLine> {
    let width = ctx.w();
    let group: Vec<&MoneyMeter> = obs
        .money
        .iter()
        .filter(|m| SPEND_GROUP.contains(&m.meter_id.as_str()))
        .collect();
    let mut rows = Vec::new();
    let mut grouped = false;
    for m in &obs.money {
        if SPEND_GROUP.contains(&m.meter_id.as_str()) {
            if !grouped {
                rows.push(spend_group_row(&group, width));
                grouped = true;
            }
            continue;
        }
        rows.extend(meter_card(m, width));
    }
    rows
}

/// What to do about a failure, in a few words.
pub(crate) fn failure_hint(obs: &AccountObservation) -> Option<String> {
    let f = obs.failure.as_ref()?;
    Some(match f.kind {
        FailureKind::AuthRequired => match obs.origin {
            Origin::Profile => format!("run `tollgate login {}`", obs.label),
            Origin::CodexProfile => format!("run `tollgate login {} --codex`", obs.label),
            Origin::Monitor => "replace the monitoring key".to_string(),
            Origin::Upstream => "log in again with clauth".to_string(),
        },
        FailureKind::RateLimited => "backing off, retries on its own".to_string(),
        FailureKind::QuotaExhausted => "wait for the reset or top up".to_string(),
        FailureKind::Unavailable => "retries on the next poll".to_string(),
        FailureKind::InvalidResponse => {
            "the provider changed its reply; update tollgate".to_string()
        }
        // A bare login on an Alibaba profile opens its console.
        FailureKind::ConsoleExpired => format!("run `tollgate login {}`", obs.label),
        FailureKind::SubscriptionInactive => "renew the subscription".to_string(),
    })
}

/// `name · provider · plan · ● active [guest]` plus the origin markers.
pub(crate) fn header_line(obs: &AccountObservation, ctx: &CardCtx) -> CardLine {
    let dot = || Seg::new(" · ", Ink::Faint);
    let name_ink = if obs.active { Ink::Active } else { Ink::Text };
    let mut line = vec![Seg::bold(obs.label.clone(), name_ink), dot()];
    line.push(Seg::new(obs.provider.clone(), Ink::Dim));
    if let Some(plan) = &obs.plan {
        line.push(dot());
        line.push(Seg::new(plan.clone(), Ink::Dim));
    }
    if obs.active {
        line.push(dot());
        line.push(Seg::new("● active", Ink::Active));
        if ctx.guest_mode && obs.origin == Origin::Profile {
            line.push(Seg::new(" [guest]", Ink::Faint));
        }
    }
    if obs.disabled {
        line.push(dot());
        line.push(Seg::new("disabled", Ink::Faint));
    }
    match obs.origin {
        Origin::Upstream => line.push(Seg::new(" (clauth)", Ink::Faint)),
        Origin::Monitor => line.push(Seg::new(" (monitor)", Ink::Faint)),
        Origin::Profile | Origin::CodexProfile => {}
    }
    if obs.best_effort {
        line.push(dot());
        line.push(Seg::new("best effort", Ink::Faint));
    }
    line
}

/// `12m ago`, `just now`.
fn ago(secs: i64) -> String {
    match countdown(Some(secs)).as_str() {
        "now" => "just now".to_string(),
        c => format!("{c} ago"),
    }
}

/// The stale / not-fetched line and the failure line, when they apply.
pub(crate) fn status_lines(obs: &AccountObservation, ctx: &CardCtx) -> Vec<CardLine> {
    let mut rows = Vec::new();
    if let Some(health) = &obs.key_health {
        let ink = match health.state {
            super::observation::KeyHealthState::Valid => Ink::Text,
            super::observation::KeyHealthState::Unknown => Ink::Dim,
            _ => Ink::Danger,
        };
        rows.push(indented(vec![Seg::new(
            format!(
                "key {} · {}",
                health.state.display_name(),
                ago(ctx.now_secs.saturating_sub(health.checked_at.secs()).max(0))
            ),
            ink,
        )]));
    }
    if let Some(note) = &obs.note {
        rows.push(indented(vec![Seg::new(
            super::observation::sanitize_message(note),
            Ink::Dim,
        )]));
    }
    match obs.freshness {
        Freshness::Fresh => {}
        Freshness::Stale { since } => {
            let text = match since.or(obs.observed_at) {
                Some(t) => format!("⏸ updated {}", ago(ctx.now_secs - t.secs())),
                None => "⏸ stale".to_string(),
            };
            rows.push(indented(vec![Seg::new(text, Ink::Warning)]));
        }
        Freshness::NotFetched => {
            rows.push(indented(vec![Seg::new("⏸ not fetched yet", Ink::Faint)]));
        }
    }
    if let Some(f) = &obs.failure {
        let mut line = vec![Seg::new(format!("⚠ {}", f.message), Ink::Danger)];
        if let Some(at) = f.retry_after.filter(|t| t.secs() > ctx.now_secs) {
            line.push(Seg::new(
                format!(" · retry in {}", countdown(Some(at.secs() - ctx.now_secs))),
                Ink::Dim,
            ));
        }
        if let Some(hint) = failure_hint(obs) {
            line.push(Seg::new(format!(" · {hint}"), Ink::Dim));
        }
        rows.push(indented(line));
    }
    rows
}

/// One account: header, status, then every window card and money card.
pub(crate) fn account_lines(obs: &AccountObservation, ctx: &CardCtx) -> Vec<CardLine> {
    let mut rows = vec![header_line(obs, ctx)];
    rows.extend(account_body(obs, ctx));
    rows
}

/// [`account_lines`] without the header — the TUI puts the name in its box.
pub(crate) fn account_body(obs: &AccountObservation, ctx: &CardCtx) -> Vec<CardLine> {
    let mut rows = status_lines(obs, ctx);
    for w in &obs.windows {
        rows.extend(window_card(w, ctx));
    }
    rows.extend(money_cards(obs, ctx));
    if let Some(n) = obs.banked_resets.filter(|n| *n > 0) {
        let unit = if n == 1 { "reset" } else { "resets" };
        rows.push(indented(vec![Seg::new(
            format!("{n} banked {unit}"),
            Ink::Faint,
        )]));
    }
    if obs.windows.is_empty()
        && obs.money.is_empty()
        && obs.failure.is_none()
        && obs.freshness != Freshness::NotFetched
    {
        rows.push(indented(vec![Seg::new("no figures published", Ink::Faint)]));
    }
    rows
}

/// `── Anthropic ───────…` across the width.
fn group_heading(provider: &str, width: usize) -> CardLine {
    let lead = "── ";
    let used = lead.chars().count() + provider.chars().count() + 1;
    vec![
        Seg::new(lead, Ink::Track),
        Seg::bold(provider, Ink::Accent),
        Seg::new(
            format!(" {}", "─".repeat(width.saturating_sub(used))),
            Ink::Track,
        ),
    ]
}

/// The whole report: a title line, then the accounts grouped by provider
/// (groups in order of first appearance, accounts in collect order).
pub(crate) fn report_lines(accounts: &[AccountObservation], ctx: &CardCtx) -> Vec<CardLine> {
    let width = ctx.w();
    let n = accounts.len();
    let mut title = vec![
        Seg::bold("tollgate usage", Ink::Text),
        Seg::new(
            format!(" · {n} account{}", if n == 1 { "" } else { "s" }),
            Ink::Dim,
        ),
    ];
    if ctx.guest_mode {
        title.push(Seg::new(
            " · guest mode (clauth owns ~/.claude)",
            Ink::Warning,
        ));
    }
    let mut rows = vec![title];
    if accounts.is_empty() {
        rows.push(Vec::new());
        rows.push(vec![Seg::new(
            "no accounts yet. add one with `tollgate login <name>`.",
            Ink::Dim,
        )]);
        return rows;
    }
    let mut providers: Vec<&str> = Vec::new();
    for o in accounts {
        if !providers.contains(&o.provider.as_str()) {
            providers.push(&o.provider);
        }
    }
    for provider in providers {
        rows.push(Vec::new());
        rows.push(group_heading(provider, width));
        let mut first = true;
        for o in accounts.iter().filter(|o| o.provider == provider) {
            if !first {
                rows.push(Vec::new());
            }
            first = false;
            rows.extend(account_lines(o, ctx));
        }
    }
    rows
}

#[cfg(test)]
#[path = "../../tests/inline/usage_cards.rs"]
mod tests;
