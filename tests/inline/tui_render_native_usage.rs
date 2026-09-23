//! The Codex, Grok and Antigravity Usage pane: what each provider's reading
//! becomes in the Claude layout, and the status rows it names.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ratatui::text::Line;

use super::super::WindowGates;
use super::{
    NativeFetch, NativeStatus, codex_usage, native_status_lines, native_usage_lines, report_usage,
};
use crate::provider_monitor::ProviderReport;
use crate::provider_monitor::types::{
    ObservationState, ProviderData, ProviderKind, QuotaBucket, RefreshState, UsageAttribution,
};
use crate::tui::app::CodexRow;
use crate::tui::render::format::ResetFmt;
use crate::usage::{CodexPoll, CodexPollOutcome, ProfileActivity, UsageWindow};

const NOW: u64 = 1_790_000_000_000;
const INTERVAL: u64 = 90_000;

fn text(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn bucket(id: &str, label: &str, scope: &str, used: f64, window: Option<u64>) -> QuotaBucket {
    QuotaBucket {
        id: id.into(),
        label: label.into(),
        scope: scope.into(),
        used_percent: Some(used),
        remaining_percent: Some(100.0 - used),
        window_seconds: window,
        ..QuotaBucket::default()
    }
}

fn report(provider: ProviderKind, buckets: Vec<QuotaBucket>) -> ProviderReport {
    ProviderReport {
        id: "native".into(),
        provider,
        tool: provider.tool().into(),
        model: None,
        state: ObservationState::Fresh,
        observed_at_ms: Some(NOW),
        checked_at_ms: Some(NOW),
        identity_checked_at_observation_only: false,
        data: ProviderData {
            plan: Some("Plan".into()),
            buckets,
            ..ProviderData::default()
        },
        message: None,
        warning: false,
        listed: true,
        refresh: RefreshState::default(),
    }
}

fn codex_row() -> CodexRow {
    CodexRow {
        name: "cx".into(),
        active: true,
        broken: false,
        plan: Some("prolite".into()),
        five_hour: Some(UsageWindow {
            utilization: 12.0,
            resets_at: None,
        }),
        seven_day: Some(UsageWindow {
            utilization: 37.0,
            resets_at: None,
        }),
        fetched_at: Some(NOW - 10_000),
        limit_reached: None,
        reset_credits: None,
        poll: None,
    }
}

fn status(fetch: NativeFetch) -> NativeStatus {
    NativeStatus {
        activity: ProfileActivity::Idle,
        fetch,
        failures: 0,
        next_ms: None,
        stale: false,
        limit_reached: false,
        hint: None,
    }
}

#[test]
fn antigravity_pools_become_named_5h_and_7d_bars_without_model_rows() {
    let agy = report(
        ProviderKind::Antigravity,
        vec![
            bucket(
                "pool:gemini-weekly",
                "Gemini Models: Weekly Limit Remaining",
                "shared",
                3.0,
                Some(604_800),
            ),
            bucket(
                "pool:gemini-5h",
                "Gemini Models: Five Hour Limit Remaining",
                "shared",
                20.0,
                Some(18_000),
            ),
            bucket(
                "pool:3p-weekly",
                "Claude and GPT models: Weekly Limit Remaining",
                "shared",
                4.0,
                Some(604_800),
            ),
            bucket(
                "pool:3p-5h",
                "Claude and GPT models: Five Hour Limit Remaining",
                "shared",
                40.0,
                Some(18_000),
            ),
            bucket(
                "model:gemini-pro",
                "Gemini 3.1 Pro (High)",
                "model",
                0.0,
                None,
            ),
        ],
    );
    let usage = report_usage(&agy);
    let labels: Vec<&str> = usage.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "5h gemini",
            "5h claude and gpt",
            "7d gemini",
            "7d claude and gpt"
        ]
    );
    assert_eq!(
        crate::usage::window_duration_secs("5h gemini"),
        Some(5 * 3600),
        "a pooled 5h label keeps its pace marker"
    );
}

#[test]
fn a_single_grok_week_is_the_7d_bar_and_its_products_are_a_fact() {
    let mut grok = report(
        ProviderKind::Grok,
        vec![bucket(
            "grok-shared",
            "Grok shared weekly allowance",
            "shared",
            99.0,
            Some(604_800),
        )],
    );
    grok.data.attribution = vec![UsageAttribution {
        label: "GrokBuild".into(),
        used_percent: 99.0,
        bucket_id: "grok-shared".into(),
    }];
    let usage = report_usage(&grok);
    let labels: Vec<&str> = usage.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["7d"]);
    assert_eq!(usage.facts, [("products", "GrokBuild 99%".to_string())]);
}

#[test]
fn a_native_report_maps_to_the_claude_fetch_states() {
    let mut r = report(ProviderKind::Grok, Vec::new());
    r.state = ObservationState::AuthRequired;
    r.message = Some("sign in again with the provider's official tool".into());
    let s = report_usage(&r).status;
    assert_eq!(s.fetch, NativeFetch::SignInNeeded);
    assert_eq!(s.hint.as_deref(), r.message.as_deref());

    r.state = ObservationState::RateLimited;
    r.refresh = RefreshState {
        failures: 3,
        next_check_ms: Some(NOW + 60_000),
        queued: false,
        refreshing: false,
    };
    let s = report_usage(&r).status;
    assert_eq!(s.fetch, NativeFetch::RateLimited);
    assert_eq!((s.failures, s.next_ms), (3, Some(NOW + 60_000)));

    r.state = ObservationState::Stale;
    assert!(report_usage(&r).status.stale);

    r.refresh.queued = true;
    assert_eq!(report_usage(&r).status.activity, ProfileActivity::Queued);
    r.refresh.refreshing = true;
    assert_eq!(report_usage(&r).status.activity, ProfileActivity::Fetching);
}

#[test]
fn an_exhausted_shared_bucket_reads_limit_reached() {
    let mut b = bucket(
        "grok-shared",
        "Grok shared weekly allowance",
        "shared",
        100.0,
        Some(604_800),
    );
    b.exhausted = true;
    assert!(
        report_usage(&report(ProviderKind::Grok, vec![b]))
            .status
            .limit_reached
    );
}

#[test]
fn a_codex_row_reads_its_poll_record_and_cache_age() {
    let mut row = codex_row();
    let usage = codex_usage(&row, INTERVAL, NOW);
    let labels: Vec<&str> = usage.windows.iter().map(|w| w.label.as_str()).collect();
    assert_eq!(labels, ["5h", "7d"]);
    assert_eq!(usage.status.fetch, NativeFetch::Fresh);
    assert!(!usage.status.stale);
    assert_eq!(
        usage.status.next_ms,
        Some(NOW - 10_000 + INTERVAL),
        "without a poll record the cache age paces the countdown"
    );

    row.fetched_at = Some(NOW - 60 * 60 * 1000);
    assert!(codex_usage(&row, INTERVAL, NOW).status.stale);
    row.fetched_at = None;
    assert!(
        codex_usage(&row, INTERVAL, NOW).status.stale,
        "an undated reading is stale"
    );

    row.poll = Some(CodexPoll {
        polled_at: Some(NOW - 5_000),
        outcome: Some(CodexPollOutcome::Unauthorized),
        failures: 2,
        ..CodexPoll::default()
    });
    let s = codex_usage(&row, INTERVAL, NOW).status;
    assert_eq!(s.fetch, NativeFetch::AuthFailing);
    assert_eq!((s.failures, s.next_ms), (2, Some(NOW - 5_000 + INTERVAL)));

    row.poll = Some(CodexPoll {
        fetching: true,
        ..CodexPoll::default()
    });
    assert_eq!(
        codex_usage(&row, INTERVAL, NOW).status.activity,
        ProfileActivity::Fetching
    );

    row.broken = true;
    assert_eq!(
        codex_usage(&row, INTERVAL, NOW).status.fetch,
        NativeFetch::AuthBroken
    );
}

#[test]
fn a_codex_limit_and_its_reset_credits_show() {
    let mut row = codex_row();
    row.limit_reached = Some("primary".into());
    row.reset_credits = Some(1);
    let usage = codex_usage(&row, INTERVAL, NOW);
    assert!(usage.status.limit_reached);
    assert_eq!(usage.facts, [("resets", "1 available".to_string())]);
}

#[test]
fn the_status_rows_speak_the_claude_vocabulary() {
    let now = NOW;
    let mut s = status(NativeFetch::Fresh);
    s.next_ms = Some(now + 42_000);
    assert!(text(&native_status_lines(&s, 0, 80, now)).contains("◌ refresh in 42s"));
    s.next_ms = None;
    assert!(text(&native_status_lines(&s, 0, 80, now)).contains("◌ up to date"));

    let mut s = status(NativeFetch::RateLimited);
    s.failures = 3;
    s.next_ms = Some(now + 30_000);
    let t = text(&native_status_lines(&s, 0, 80, now));
    assert!(t.contains("[ rate limited ]  3rd retry in 30s"), "{t}");

    let mut s = status(NativeFetch::SignInNeeded);
    s.hint = Some("sign in with grok".into());
    s.stale = true;
    let t = text(&native_status_lines(&s, 0, 80, now));
    assert!(
        t.contains("sign-in needed") && t.contains("sign in with grok"),
        "{t}"
    );
    assert!(!t.contains("stale"), "a dead login ends the block\n{t}");

    let mut s = status(NativeFetch::Fresh);
    s.stale = true;
    s.limit_reached = true;
    let t = text(&native_status_lines(&s, 0, 80, now));
    assert!(t.contains("limit reached") && t.contains("stale"), "{t}");

    let mut s = status(NativeFetch::Fresh);
    s.activity = ProfileActivity::Queued;
    assert!(text(&native_status_lines(&s, 0, 80, now)).contains("queued"));
}

#[test]
fn a_pane_with_no_windows_says_why() {
    let usage = report_usage(&report(ProviderKind::Grok, Vec::new()));
    let lines = native_usage_lines(
        &usage,
        80,
        0,
        WindowGates {
            show_estimates: true,
            show_pace: true,
        },
        ResetFmt::default(),
        (NOW / 1000) as i64,
    );
    assert!(
        text(&lines).contains("no usage available"),
        "{}",
        text(&lines)
    );
}
