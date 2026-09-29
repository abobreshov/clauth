#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `tollgate usage`: the `--json` envelope pinned byte-for-byte against a
//! golden (`tests/fixtures/usage_report_golden.json`) — the schema agents read,
//! so any key rename, reorder or type change reds here — plus the plain line.

use super::*;
use crate::usage::observation::{
    Amount, AuthKind, Failure, FailureKind, LocalEstimate, ModelCount, MoneyKind, MoneyMeter,
    MoneyScope, Origin, Period, PeriodKind, QuotaWindow, ScopeOrigin, SourceId, Timestamp,
    WindowScope, account_id,
};

const GOLDEN: &str = include_str!("../fixtures/usage_report_golden.json");

/// 2026-09-29T10:00:00Z.
const NOW: i64 = 1_790_676_000;

/// One observation that sets every field and every enum shape at least once.
fn full_observation() -> AccountObservation {
    let mut o = AccountObservation::new(
        account_id(Origin::Profile, "or-main"),
        SourceId::OpenRouter,
        AuthKind::ApiKey,
        Origin::Profile,
        "or-main",
    );
    o.plan = Some("pay-as-you-go".to_string());
    o.active = true;
    o.endpoint = Some("https://openrouter.ai/api".to_string());
    o.freshness = Freshness::Stale {
        since: Some(Timestamp(NOW - 7_200)),
    };
    o.failure = Some(Failure {
        kind: FailureKind::RateLimited,
        message: "slow down".to_string(),
        retry_after: Some(Timestamp(NOW + 60)),
    });
    let mut session = QuotaWindow::new("session", "5h", WindowScope::Shared);
    session.used_pct = Some(82.5);
    session.resets_at = Some(Timestamp(NOW + 11_100));
    session.window_secs = Some(18_000);
    session.chain_eligible = true;
    let mut free = QuotaWindow::new("free_daily", "free requests", WindowScope::Account);
    free.used_pct = None;
    free.used = Some(12.0);
    free.limit = Some(50.0);
    free.breakdown = vec![ModelCount {
        model: "glm-5".to_string(),
        requests: 12,
    }];
    let model = QuotaWindow::new(
        "weekly:opus",
        "7d opus",
        WindowScope::Model {
            models: vec!["opus".to_string()],
        },
    );
    o.windows = vec![session, free, model];
    let wallet = MoneyMeter::new(
        "wallet",
        "Balance",
        MoneyKind::Balance,
        Amount::parse("-0.000123").unwrap(),
        "USD",
        MoneyScope::Profile,
    );
    let mut cap = MoneyMeter::new(
        "key_limit",
        "Key cap",
        MoneyKind::Limit,
        Amount::parse("9.00").unwrap(),
        "usd",
        MoneyScope::Key,
    );
    cap.limit = Some(Amount::parse("50").unwrap());
    cap.scope_id = Some("org-1".to_string());
    cap.scope_origin = ScopeOrigin::MonitoringCredential { bound: true };
    cap.additive = false;
    cap.period = Some(Period {
        kind: PeriodKind::Monthly,
        start: Some(Timestamp(NOW - 86_400)),
        end: Some(Timestamp(NOW + 86_400)),
        derived: true,
    });
    o.money = vec![wallet, cap];
    o.estimate = Some(LocalEstimate {
        amount: Amount::parse("1.5").unwrap(),
        currency: "USD".to_string(),
        period: Some(Period::of(PeriodKind::Daily)),
        basis: "token ledger × ai-pricelog".to_string(),
    });
    o.banked_resets = Some(0);
    o.best_effort = true;
    o.observed_at = Some(Timestamp(NOW - 7_200));
    o.checked_at = Some(Timestamp(NOW - 60));
    o
}

fn bare(origin: Origin, name: &str, source: SourceId) -> AccountObservation {
    AccountObservation::new(
        account_id(origin, name),
        source,
        AuthKind::Subscription,
        origin,
        name,
    )
}

#[test]
fn usage_json_matches_the_golden_schema_byte_for_byte() {
    let report = UsageReport::new(
        vec![
            full_observation(),
            bare(Origin::CodexProfile, "cx", SourceId::Codex),
        ],
        NOW,
        true,
    );
    let got = serde_json::to_string_pretty(&report).unwrap();
    assert_eq!(
        got,
        GOLDEN.trim_end(),
        "the usage --json schema changed; if deliberate, bump SCHEMA_VERSION when it breaks readers and update the golden:\n{got}"
    );
    // And it reads back losslessly.
    let back: UsageReport = serde_json::from_str(&got).unwrap();
    assert_eq!(back, report);
}

#[test]
fn the_envelope_carries_the_schema_version_and_an_rfc3339_stamp() {
    let report = UsageReport::new(Vec::new(), NOW, false);
    let v = serde_json::to_value(&report).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["generated_at"], "2026-09-29T10:00:00+00:00");
    assert_eq!(v["guest_mode"], false);
    assert_eq!(v["accounts"], serde_json::json!([]));
}

#[test]
fn plain_line_names_the_account_its_figures_and_its_flags() {
    let mut o = full_observation();
    o.windows[0].resets_at = None;
    let line = plain_line(&o, NOW);
    assert!(
        line.starts_with("* claude:or-main  OpenRouter  pay-as-you-go  "),
        "{line}"
    );
    assert!(line.contains("5h 82.5% HIGH (—)"), "{line}");
    assert!(line.contains("free requests ? (—)"), "{line}");
    assert!(line.contains("Balance -$0.00 CRITICAL"), "{line}");
    assert!(line.contains("  critical  "), "{line}");
    assert!(line.ends_with("(stale)  (slow down)"), "{line}");

    let cold = bare(Origin::CodexProfile, "cx", SourceId::Codex);
    assert_eq!(plain_line(&cold, NOW), "  codex:cx  OpenAI  (not fetched)");
}

#[test]
fn plain_line_shows_the_pace_of_a_dated_window() {
    let mut o = bare(Origin::Profile, "w", SourceId::AnthropicOauth);
    let mut w = QuotaWindow::new("session", "5h", WindowScope::Shared);
    w.used_pct = Some(62.0);
    w.resets_at = Some(Timestamp(NOW + 9_000));
    w.window_secs = Some(18_000);
    o.windows.push(w);
    o.freshness = Freshness::Fresh;
    let line = plain_line(&o, NOW);
    assert!(line.contains("5h 62% ↑ 12 pts ahead (2h 30m ("), "{line}");
    assert!(line.ends_with("  mid"), "{line}");
}

#[test]
fn key_health_and_note_render_on_cards() {
    use crate::usage::observation::{KeyHealth, KeyHealthState};
    let mut o = bare(Origin::Monitor, "google-ai", SourceId::GoogleAi);
    o.freshness = Freshness::Fresh;
    o.key_health = Some(KeyHealth {
        state: KeyHealthState::Valid,
        checked_at: Timestamp(NOW - 180),
    });
    o.note = Some("spend and quota not available for API keys".into());
    let ctx = crate::usage::cards::CardCtx {
        width: 100,
        now_secs: NOW,
        offset_secs: 0,
        guest_mode: false,
    };
    let lines: Vec<_> = crate::usage::cards::account_body(&o, &ctx)
        .iter()
        .map(|l| crate::usage::cards::plain_text(l))
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("key valid · 3m ago")),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains(o.note.as_ref().unwrap())));
    assert!(plain_line(&o, NOW).contains("key valid · 3m ago"));
    let json = serde_json::to_value(&o).unwrap();
    assert_eq!(json["key_health"]["state"], "valid");
    assert_eq!(
        json["key_health"]["checked_at"],
        Timestamp(NOW - 180).to_rfc3339()
    );
}
