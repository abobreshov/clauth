//! Provider subscriptions as one table per provider.
//!
//! Claude is the active clauth account, not a `providers.toml` target. Native
//! reports stay grouped by provider. Each quota window, credit, and product
//! observation is its own row. Reset cells are local calendar dates
//! ([`crate::format::local_stamp`]), never the stored ISO-8601 or epoch text.

use super::super::{app::App, theme};
use super::format::{NO_DATA, fixed};
use super::panes::{draw_scrollbar, section_box};
use crate::provider_monitor::types::{ObservationState, ProviderKind, ProviderReport};
use crate::provider_monitor::{ClaudeReading, remaining_percent};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, layout::Rect};

/// Inner width of this pane at an 80-column terminal: border plus one pad each side.
const CONTENT_WIDTH: usize = 76;
const WINDOW_W: usize = 26;
const REMAINING_W: usize = 17;
const STATUS_W: usize = 9;
const RESET_W: usize = 19;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = section_box("provider subscriptions", true, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = table_lines(app);
    let viewport = inner.height as usize;
    let total = lines.len();
    let max_scroll = total.saturating_sub(viewport).min(u16::MAX as usize) as u16;
    let scroll = app.provider_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .scroll((scroll, 0)),
        inner,
    );
    draw_scrollbar(frame, inner, total, scroll as usize, viewport);
}

fn table_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(error) = &app.provider_error {
        let error = plain(error);
        if !error.is_empty() {
            lines.push(styled(fit(&error), theme::danger()));
            lines.push(Line::from(""));
        }
    }
    push_claude(&mut lines, &claude_reading(app));
    if app.provider_reports.is_empty() {
        lines.push(Line::from(""));
        lines.push(dim_line(
            "Run `clauth providers init` to configure Codex, Grok and agy.",
        ));
        lines.push(dim_line(
            "Launch: clauth providers start <target> [--herdr]",
        ));
        return lines;
    }
    for (kind, reports) in grouped_reports(&app.provider_reports) {
        lines.push(Line::from(""));
        lines.push(group_heading(match kind {
            crate::provider_monitor::types::ProviderKind::Codex => "codex",
            crate::provider_monitor::types::ProviderKind::Grok => "grok",
            crate::provider_monitor::types::ProviderKind::Antigravity => "antigravity",
        }));
        for report in reports {
            push_report(&mut lines, report);
        }
    }
    lines
}

/// Live usage when this session has it. A profile with no in-memory usage
/// falls back to the on-disk cache. Neither path switches the account.
fn claude_reading(app: &App) -> ClaudeReading {
    let snap = {
        let cfg = app.config();
        let Some(name) = cfg.state.active_profile.as_ref().cloned() else {
            return ClaudeReading {
                account: None,
                plan: None,
                usage: None,
            };
        };
        let Some(profile) = cfg.find(&name) else {
            return ClaudeReading {
                account: Some(name.to_string()),
                plan: None,
                usage: None,
            };
        };
        let tier = crate::format::account_tier(profile).and_then(|tier| tier.short_label());
        let plan = tier.or_else(|| {
            profile
                .provider
                .map(|provider| provider.display_name().to_string())
        });
        (name.to_string(), plan, profile.usage.clone())
    };
    match snap.2 {
        Some(usage) => ClaudeReading {
            account: Some(snap.0),
            plan: snap.1,
            usage: Some(usage),
        },
        None => crate::provider_monitor::current_claude_reading(),
    }
}

fn push_claude(lines: &mut Vec<Line<'static>>, reading: &ClaudeReading) {
    lines.push(group_heading("claude"));
    let Some(account) = reading.account.as_deref().filter(|name| !name.is_empty()) else {
        lines.push(dim_line("no active account"));
        return;
    };
    let state = match &reading.usage {
        None => "not fetched",
        Some(usage) if usage.fetched_at.is_some() => "fresh",
        Some(_) => "stale",
    };
    lines.push(styled(
        target_line(
            account,
            reading.plan.as_deref().unwrap_or("plan unknown"),
            state,
            "",
        ),
        theme::base(),
    ));
    let Some(usage) = reading.usage.as_ref() else {
        lines.push(note("Quota unavailable (not unlimited)"));
        return;
    };
    if usage.plan.as_ref().is_some_and(|plan| plan.is_canceled()) {
        lines.push(note("subscription canceled"));
    }
    let windows: Vec<_> = usage
        .windows()
        .into_iter()
        .map(|(label, window)| {
            let (remaining, exhausted) = remaining_percent(window.utilization);
            QuotaRow {
                label: label.to_string(),
                remaining,
                exhausted,
                reset: window.resets_at.clone(),
            }
        })
        .collect();
    if windows.is_empty() {
        lines.push(note("Quota unavailable (not unlimited)"));
        return;
    }
    lines.push(dim_line(&columns(
        "window",
        "remaining",
        "status",
        "resets",
    )));
    for row in windows {
        lines.push(styled(quota_line(&row), theme::base()));
    }
}

fn push_report(lines: &mut Vec<Line<'static>>, report: &ProviderReport) {
    let extra = if report.warning {
        "account quota warning"
    } else {
        ""
    };
    let style = if report.warning {
        theme::warning()
    } else {
        theme::base()
    };
    lines.push(styled(
        target_line(
            &report.id,
            report.data.plan.as_deref().unwrap_or("plan unknown"),
            state_label(report.state),
            extra,
        ),
        style,
    ));
    if report.identity_checked_at_observation_only {
        lines.push(note(
            "Login identity checked at observation only (native keyring)",
        ));
    }
    if let Some(message) = &report.message {
        let message = plain(message);
        if !message.is_empty() {
            lines.push(note(&message));
        }
    }
    if report.data.buckets.is_empty() {
        lines.push(note("Quota unavailable (not unlimited)"));
    }
    let mut rows = Vec::new();
    for bucket in &report.data.buckets {
        rows.push(QuotaRow {
            label: bucket.label.clone(),
            remaining: bucket.remaining_percent,
            exhausted: bucket.exhausted,
            reset: bucket.resets_at.clone(),
        });
    }
    for attribution in &report.data.attribution {
        let (remaining, exhausted) = remaining_percent(attribution.used_percent);
        let reset = report
            .data
            .buckets
            .iter()
            .find(|bucket| bucket.id == attribution.bucket_id)
            .and_then(|bucket| bucket.resets_at.clone());
        rows.push(QuotaRow {
            label: attribution.label.clone(),
            remaining,
            exhausted,
            reset,
        });
    }
    if !rows.is_empty() || !report.data.credits.is_empty() {
        lines.push(dim_line(&columns(
            "window",
            "remaining",
            "status",
            "resets",
        )));
    }
    for row in rows {
        lines.push(styled(quota_line(&row), theme::base()));
    }
    for credit in &report.data.credits {
        let remaining = if credit.remaining.is_finite() {
            format!(
                "{} {}",
                crate::format::format_amount(credit.remaining),
                plain(&credit.unit)
            )
        } else {
            "remaining unknown".to_string()
        };
        lines.push(styled(
            columns(&credit.label, remaining.trim(), "", NO_DATA),
            theme::base(),
        ));
    }
    if let Some(at) = report.observed_at_ms
        && let Ok(secs) = i64::try_from(at / 1000)
        && let Some(stamp) = crate::format::local_stamp(secs)
    {
        lines.push(dim_line(&columns("last reading", "", "", &stamp)));
    }
}

struct QuotaRow {
    label: String,
    remaining: Option<f64>,
    exhausted: bool,
    reset: Option<String>,
}

fn quota_line(row: &QuotaRow) -> String {
    let exhausted = row.exhausted || row.remaining.is_some_and(|p| p.is_finite() && p <= 0.0);
    columns(
        &row.label,
        &remaining_text(row.remaining),
        if exhausted { "exhausted" } else { "" },
        &reset_stamp(row.reset.as_deref()),
    )
}

fn grouped_reports(reports: &[ProviderReport]) -> Vec<(ProviderKind, Vec<&ProviderReport>)> {
    [
        ProviderKind::Codex,
        ProviderKind::Grok,
        ProviderKind::Antigravity,
    ]
    .into_iter()
    .filter_map(|kind| {
        let rows: Vec<_> = reports
            .iter()
            .filter(|report| report.provider == kind)
            .collect();
        (!rows.is_empty()).then_some((kind, rows))
    })
    .collect()
}

fn state_label(state: ObservationState) -> &'static str {
    match state {
        ObservationState::Fresh => "fresh",
        ObservationState::Stale => "stale",
        ObservationState::NotFetched => "not fetched",
        ObservationState::AuthRequired => "sign in",
        ObservationState::RateLimited => "rate limited",
        ObservationState::Unavailable => "unavailable",
        ObservationState::InvalidResponse => "bad response",
    }
}

fn remaining_text(remaining: Option<f64>) -> String {
    match remaining {
        Some(percent) if percent.is_finite() => format!("{percent:.1}% remaining"),
        _ => "remaining unknown".to_string(),
    }
}

/// A known instant as `YYYY-MM-DD HH:MM:SS` in the operator's local zone.
/// Anything that is not an instant — missing, blank, or unparseable — is
/// [`NO_DATA`], never the raw stored text and never a guessed clock.
fn reset_stamp(raw: Option<&str>) -> String {
    let Some(raw) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return NO_DATA.to_string();
    };
    let epoch = crate::usage::iso_to_epoch_secs(raw).or_else(|| numeric_epoch(raw));
    epoch
        .and_then(crate::format::local_stamp)
        .unwrap_or_else(|| NO_DATA.to_string())
}

/// Epoch seconds, or milliseconds when the magnitude is past year ~2286 in seconds.
fn numeric_epoch(raw: &str) -> Option<i64> {
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let n: u64 = raw.parse().ok()?;
    if n > 10_000_000_000 {
        i64::try_from(n / 1000).ok()
    } else {
        i64::try_from(n).ok()
    }
}

fn columns(window: &str, remaining: &str, status: &str, reset: &str) -> String {
    format!(
        "  {} {} {} {}",
        fixed(&plain(window), WINDOW_W),
        fixed(&plain(remaining), REMAINING_W),
        fixed(&plain(status), STATUS_W),
        fixed(&plain(reset), RESET_W),
    )
}

fn target_line(id: &str, plan: &str, state: &str, extra: &str) -> String {
    let prefix = format!(
        "  {} {} {}",
        fixed(&plain(id), 16),
        fixed(&plain(plan), 14),
        fixed(&plain(state), 12),
    );
    let extra = plain(extra);
    if extra.is_empty() {
        return prefix;
    }
    let spare = CONTENT_WIDTH.saturating_sub(prefix.chars().count() + 1);
    if spare == 0 {
        return fit(&prefix);
    }
    format!("{prefix} {}", fixed(&extra, spare))
}

fn note(text: &str) -> Line<'static> {
    dim_line(&format!("  {}", plain(text)))
}

fn dim_line(text: &str) -> Line<'static> {
    styled(fit(text), theme::dim())
}

fn group_heading(title: &str) -> Line<'static> {
    let (mark, style) = super::panes::group_mark(title);
    Line::from(vec![
        Span::styled(mark, style),
        Span::styled(title.to_string(), theme::label()),
    ])
}

fn styled(text: String, style: Style) -> Line<'static> {
    Line::styled(text, style)
}

fn fit(text: &str) -> String {
    let text = plain(text);
    if text.chars().count() <= CONTENT_WIDTH {
        text
    } else {
        fixed(&text, CONTENT_WIDTH)
    }
}

fn plain(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::profile::{AppConfig, AppState, ProfileName};
    use crate::provider_monitor::types::{
        CreditBalance, ObservationState, ProviderData, ProviderKind, QuotaBucket, UsageAttribution,
    };
    use crate::tui::app::App;
    use crate::usage::{PlanInfo, PlanTier, ScopedWindow, UsageInfo, UsageWindow};
    use chrono::{DateTime, Local, NaiveDateTime};

    #[test]
    fn providers_tab_shows_the_active_claude_account() {
        let _home = crate::testutil::HomeSandbox::new();
        let name = ProfileName::from("beta");
        let mut profile = crate::testutil::blank_profile(&name);
        profile.usage = Some(UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Max(Some(20)),
                ..PlanInfo::default()
            }),
            five_hour: Some(UsageWindow {
                utilization: 20.0,
                resets_at: Some("2026-09-22T01:00:00Z".into()),
            }),
            seven_day: Some(UsageWindow {
                utilization: 70.0,
                resets_at: Some("2026-09-28T00:00:00Z".into()),
            }),
            fetched_at: Some(1),
            ..UsageInfo::default()
        });
        let app = App::new(AppConfig {
            state: AppState {
                active_profile: Some(name),
                profiles: vec![profile.name.clone()],
                ..AppState::default()
            },
            profiles: vec![profile],
        });
        let text = render(&app, 80, 24);
        assert!(
            text.contains("beta") && text.contains("claude") && text.contains("Max 20x"),
            "{text}"
        );
        assert!(text.contains("80.0% remaining"), "{text}");
        assert!(text.contains("30.0% remaining"), "{text}");
        assert!(
            text.contains("clauth providers init"),
            "empty native list keeps the init hint\n{text}"
        );
        assert!(
            !text.contains("2026-09-22T01:00:00Z"),
            "reset cell is not raw ISO\n{text}"
        );
    }

    #[test]
    fn providers_tab_names_a_missing_claude_account_and_the_init_hint() {
        let _home = crate::testutil::HomeSandbox::new();
        let app = App::new(AppConfig {
            state: AppState::default(),
            profiles: Vec::new(),
        });
        let text = render(&app, 80, 24);
        assert!(text.contains("claude"), "{text}");
        assert!(text.contains("no active account"), "{text}");
        assert!(text.contains("clauth providers init"), "{text}");
        assert!(stamps(&text).is_empty(), "{text}");
    }

    #[test]
    fn providers_tab_groups_by_provider_and_shows_readable_resets() {
        let home = crate::testutil::HomeSandbox::new();
        let name = ProfileName::from("beta");
        let mut profile = crate::testutil::blank_profile(&name);
        profile.usage = Some(UsageInfo {
            plan: Some(PlanInfo {
                tier: PlanTier::Max(Some(20)),
                ..PlanInfo::default()
            }),
            five_hour: Some(UsageWindow {
                utilization: 20.0,
                resets_at: Some("2026-09-22T01:00:00Z".into()),
            }),
            seven_day: Some(UsageWindow {
                utilization: 70.0,
                resets_at: Some("2026-09-28T00:00:00Z".into()),
            }),
            weekly_scoped: vec![ScopedWindow {
                label: "7d opus".into(),
                window: UsageWindow {
                    utilization: 10.0,
                    resets_at: None,
                },
            }],
            fetched_at: Some(1),
            ..UsageInfo::default()
        });
        let grok_ms = DateTime::parse_from_rfc3339("2026-11-02T08:09:10Z")
            .unwrap()
            .timestamp_millis();
        let observed_ms = 1_780_000_000_000_u64;
        let mut reports = vec![
            native(
                "grok-desk",
                ProviderKind::Grok,
                "Super",
                false,
                vec![QuotaBucket {
                    id: "shared".into(),
                    label: "shared weekly".into(),
                    scope: "shared".into(),
                    remaining_percent: Some(40.0),
                    resets_at: Some(grok_ms.to_string()),
                    ..QuotaBucket::default()
                }],
                vec![],
                vec![UsageAttribution {
                    label: "GrokBuild".into(),
                    used_percent: 20.0,
                    bucket_id: "shared".into(),
                }],
            ),
            native(
                "codex-b",
                ProviderKind::Codex,
                "Pro\u{1b}[2J",
                true,
                vec![],
                vec![],
                vec![],
            ),
            native(
                "codex-a",
                ProviderKind::Codex,
                "Plus",
                false,
                vec![
                    QuotaBucket {
                        id: "5h".into(),
                        label: "session 5h".into(),
                        remaining_percent: Some(0.0),
                        exhausted: true,
                        resets_at: Some("2026-10-01T15:30:45Z".into()),
                        ..QuotaBucket::default()
                    },
                    QuotaBucket {
                        id: "week".into(),
                        label: "weekly pool".into(),
                        scope: "shared".into(),
                        remaining_percent: None,
                        resets_at: None,
                        ..QuotaBucket::default()
                    },
                ],
                vec![CreditBalance {
                    label: "limit resets".into(),
                    remaining: 2.0,
                    unit: "resets".into(),
                }],
                vec![],
            ),
        ];
        reports[0].observed_at_ms = Some(observed_ms);
        reports[0].identity_checked_at_observation_only = true;
        reports[1].message = Some("account\u{7} note".into());
        let mut app = App::new(AppConfig {
            state: AppState {
                active_profile: Some(name.clone()),
                profiles: vec![profile.name.clone()],
                ..AppState::default()
            },
            profiles: vec![profile],
        });
        app.provider_reports = reports;
        app.provider_error = Some("provider endpoint down".into());

        let text = render(&app, 80, 60);
        let again = render(&app, 80, 60);
        save_evidence(&text);
        assert_eq!(text, again, "two draws of the same state differ");
        assert!(
            text.lines().all(|line| !line.chars().any(char::is_control)),
            "{text}"
        );
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text}"
        );
        assert!(!text.contains('·'), "table is not the prose dump\n{text}");
        assert!(text.contains("provider endpoint down"), "{text}");
        assert!(text.contains("Max 20x"), "{text}");
        assert!(text.contains("account quota warning"), "{text}");
        assert!(text.contains("Quota unavailable (not unlimited)"), "{text}");
        assert!(text.contains("account note"), "{text}");
        assert!(text.contains("native keyring"), "{text}");
        assert!(!text.contains("clauth providers init"), "{text}");

        let rows: Vec<&str> = text.lines().collect();
        let beta = text.find("beta").unwrap();
        let codex_b = text.find("codex-b").unwrap();
        let codex_a = text.find("codex-a").unwrap();
        let grok = text.find("grok-desk").unwrap();
        assert!(
            beta < codex_b && codex_b < codex_a && codex_a < grok,
            "{text}"
        );
        assert!(!text[codex_b..codex_a].contains("grok"), "{text}");
        // Group headings carry the same marks as the account lists.
        assert!(
            rows.iter().any(|row| inner_text(row) == "✳ claude"),
            "{text}"
        );
        assert!(
            rows.iter().any(|row| inner_text(row) == "▣ codex"),
            "{text}"
        );
        assert!(rows.iter().any(|row| inner_text(row) == "✶ grok"), "{text}");

        let five = window_row(&rows, "5h");
        let seven = window_row(&rows, "7d");
        let opus = window_row(&rows, "7d opus");
        let spent = window_row(&rows, "session 5h");
        let pool = window_row(&rows, "weekly pool");
        let shared = window_row(&rows, "shared weekly");
        let model = window_row(&rows, "GrokBuild");
        assert!(five.contains("80.0% remaining"), "{five}");
        assert!(seven.contains("30.0% remaining"), "{seven}");
        assert!(opus.contains("90.0% remaining"), "{opus}");
        assert_ne!(five, seven);
        assert_ne!(seven, opus);
        assert!(stamps(opus).is_empty(), "{opus}");
        assert!(opus.contains(NO_DATA), "{opus}");
        assert!(
            spent.contains("0.0% remaining") && spent.contains("exhausted"),
            "{spent}"
        );
        assert!(pool.contains("remaining unknown"), "{pool}");
        assert!(stamps(pool).is_empty() && pool.contains(NO_DATA), "{pool}");
        assert_ne!(shared, model);
        assert!(model.contains("80.0% remaining"), "{model}");
        let credit = rows
            .iter()
            .find(|row| row.contains("limit resets"))
            .expect("credit row");
        assert!(credit.contains("2 resets"), "{credit}");
        assert!(stamps(credit).is_empty(), "{credit}");

        let isos = [
            "2026-09-22T01:00:00Z",
            "2026-09-28T00:00:00Z",
            "2026-10-01T15:30:45Z",
            "2026-11-02T08:09:10Z",
        ];
        for iso in isos {
            assert!(!text.contains(iso), "{iso} still raw\n{text}");
            let want = local_instant(iso);
            assert!(
                stamps(&text).contains(&want),
                "missing {want} for {iso}\n{text}"
            );
        }
        let observed = DateTime::from_timestamp(i64::try_from(observed_ms / 1000).unwrap(), 0)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        assert!(stamps(&text).contains(&observed), "{text}");
        assert!(!text.contains(&grok_ms.to_string()), "{text}");
        assert!(!text.contains(&observed_ms.to_string()), "{text}");
        let found = stamps(&text);
        let expected = [
            local_instant(isos[0]),
            local_instant(isos[1]),
            local_instant(isos[2]),
            local_instant(isos[3]),
            observed,
        ];
        for stamp in &found {
            assert!(expected.contains(stamp), "unexpected {stamp}\n{text}");
        }
        let mut cols = Vec::new();
        for row in &rows {
            if let Some((idx, _)) = next_stamp(row) {
                assert!(idx + 19 <= 80, "stamp cut at column {idx} in {row}");
                cols.push(idx);
            }
        }
        assert!(cols.len() >= 4, "{text}");
        assert!(cols.iter().all(|col| *col == cols[0]), "{cols:?}");

        app.provider_scroll = 0;
        let top = render(&app, 80, 12);
        app.provider_scroll = u16::MAX;
        let bottom = render(&app, 80, 12);
        assert!(top.contains("beta") && !top.contains("grok-desk"), "{top}");
        assert!(
            bottom.contains("grok-desk") && !bottom.contains("beta"),
            "{bottom}"
        );
        assert_eq!(
            app.config()
                .state
                .active_profile
                .as_ref()
                .map(ProfileName::as_str),
            Some("beta")
        );
        assert!(!home.home().join(".clauth/providers.toml").exists());
    }

    fn native(
        id: &str,
        provider: ProviderKind,
        plan: &str,
        warning: bool,
        buckets: Vec<QuotaBucket>,
        credits: Vec<CreditBalance>,
        attribution: Vec<UsageAttribution>,
    ) -> ProviderReport {
        ProviderReport {
            id: id.into(),
            provider,
            tool: provider.tool().into(),
            model: None,
            state: ObservationState::Fresh,
            observed_at_ms: None,
            checked_at_ms: None,
            identity_checked_at_observation_only: false,
            data: ProviderData {
                plan: Some(plan.into()),
                buckets,
                credits,
                attribution,
                ..ProviderData::default()
            },
            message: None,
            warning,
            listed: false,
            refresh: Default::default(),
        }
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
            .expect("terminal");
        term.draw(|frame| draw(frame, frame.area(), app))
            .expect("draw");
        crate::testutil::buffer_rows(term.backend().buffer()).join("\n")
    }

    fn save_evidence(text: &str) {
        let Ok(dir) = std::env::var("CLAUTH_GOAL_SCRATCH") else {
            return;
        };
        if dir.is_empty() {
            return;
        }
        std::fs::write(std::path::Path::new(&dir).join("providers-table.txt"), text)
            .expect("write providers table");
    }

    fn inner_text(row: &str) -> String {
        let chars: Vec<char> = row.chars().collect();
        if chars.len() < 4 {
            return String::new();
        }
        chars[2..chars.len() - 2]
            .iter()
            .collect::<String>()
            .trim()
            .to_string()
    }

    fn window_row<'a>(rows: &'a [&str], label: &str) -> &'a str {
        rows.iter()
            .copied()
            .find(|row| window_label(row) == label)
            .unwrap_or_else(|| panic!("missing window {label}"))
    }

    fn window_label(row: &str) -> String {
        let chars: Vec<char> = row.chars().collect();
        if chars.len() < 4 {
            return String::new();
        }
        chars[2..chars.len() - 2]
            .iter()
            .skip(2)
            .take(WINDOW_W)
            .collect::<String>()
            .trim()
            .to_string()
    }

    fn local_instant(iso: &str) -> NaiveDateTime {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Local)
            .naive_local()
    }

    fn stamps(text: &str) -> Vec<NaiveDateTime> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some((idx, dt)) = next_stamp(rest) {
            out.push(dt);
            let chars: Vec<(usize, char)> = rest.char_indices().collect();
            let byte = chars
                .get(idx + 19)
                .map(|(byte, _)| *byte)
                .unwrap_or(rest.len());
            rest = &rest[byte..];
        }
        out
    }

    /// Char index of the next `YYYY-MM-DD HH:MM:SS` and the instant it names.
    fn next_stamp(text: &str) -> Option<(usize, NaiveDateTime)> {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() < 19 {
            return None;
        }
        (0..=chars.len() - 19).find_map(|start| {
            let stamp: String = chars[start..start + 19].iter().collect();
            let bytes = stamp.as_bytes();
            let shaped = bytes.len() == 19
                && bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes[10] == b' '
                && bytes[13] == b':'
                && bytes[16] == b':'
                && bytes
                    .iter()
                    .enumerate()
                    .all(|(i, byte)| matches!(i, 4 | 7 | 10 | 13 | 16) || byte.is_ascii_digit());
            shaped
                .then(|| NaiveDateTime::parse_from_str(&stamp, "%Y-%m-%d %H:%M:%S").ok())
                .flatten()
                .map(|dt| (start, dt))
        })
    }
}
