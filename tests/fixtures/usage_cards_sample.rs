// Shared, hand-built observations for the metric-card goldens
// (`tests/inline/usage_cards.rs`), the CLI sinks (`usage_pretty.rs`) and the
// TUI card render (`tui_render_cards.rs`). Pulled in with `include!`, so it is
// plain items, no module. Times are relative to `SAMPLE_NOW`.

/// 2026-09-29T10:00:00Z.
const SAMPLE_NOW: i64 = 1_790_676_000;

/// The zone the goldens stamp `(HH:MM)` in: UTC+2.
#[allow(dead_code)] // not every includer stamps a local time
const SAMPLE_OFFSET: i32 = 7_200;

fn sample_window(
    id: &str,
    label: &str,
    used: Option<f64>,
    resets_in: Option<i64>,
    len: Option<u64>,
) -> crate::usage::observation::QuotaWindow {
    let mut w = crate::usage::observation::QuotaWindow::new(
        id,
        label,
        crate::usage::observation::WindowScope::Shared,
    );
    w.used_pct = used;
    w.resets_at = resets_in.map(|s| crate::usage::observation::Timestamp(SAMPLE_NOW + s));
    w.window_secs = len;
    w
}

fn sample_meter(
    id: &str,
    label: &str,
    kind: crate::usage::observation::MoneyKind,
    amount: &str,
    limit: Option<&str>,
    period: Option<crate::usage::observation::PeriodKind>,
) -> crate::usage::observation::MoneyMeter {
    let mut m = crate::usage::observation::MoneyMeter::new(
        id,
        label,
        kind,
        crate::usage::observation::Amount::parse(amount).expect("amount"),
        "USD",
        crate::usage::observation::MoneyScope::Key,
    );
    m.limit = limit.map(|l| crate::usage::observation::Amount::parse(l).expect("limit"));
    m.period = period.map(crate::usage::observation::Period::of);
    m
}

/// Four accounts over three providers plus an upstream one: an active Anthropic
/// subscription with a session, a weekly and an exhausted model window; a stale
/// OpenRouter key with a wallet, spend and a key cap; an Ollama Cloud monitor
/// with a failure and no reset time; an upstream clauth account.
fn sample_accounts() -> Vec<crate::usage::observation::AccountObservation> {
    use crate::usage::observation::{
        AccountObservation, AuthKind, Failure, FailureKind, Freshness, MoneyKind, Origin,
        PeriodKind, SourceId, Timestamp, account_id,
    };

    let mut work = AccountObservation::new(
        account_id(Origin::Profile, "work"),
        SourceId::AnthropicOauth,
        AuthKind::Subscription,
        Origin::Profile,
        "work",
    );
    work.plan = Some("Max 20x".to_string());
    work.active = true;
    work.freshness = Freshness::Fresh;
    let mut opus = sample_window("weekly:opus", "7d opus", Some(100.0), Some(86_400 + 3_600), Some(604_800));
    opus.exhausted = true;
    work.windows = vec![
        // 3h 05m left of 5h: 38% elapsed, 42% used → on pace.
        sample_window("session", "5h", Some(42.0), Some(11_100), Some(18_000)),
        // 4d 1h left of 7d: 42% elapsed, 81% used → 38 pts ahead, HIGH.
        sample_window("weekly", "7d", Some(81.0), Some(4 * 86_400 + 3_600), Some(604_800)),
        opus,
    ];
    work.money = vec![sample_meter(
        "spend.extra",
        "Extra usage",
        MoneyKind::Spend,
        "12.5",
        Some("50"),
        Some(PeriodKind::Monthly),
    )];

    let mut or = AccountObservation::new(
        account_id(Origin::Profile, "or-main"),
        SourceId::OpenRouter,
        AuthKind::ApiKey,
        Origin::Profile,
        "or-main",
    );
    or.plan = Some("pay-as-you-go".to_string());
    or.freshness = Freshness::Stale {
        since: Some(Timestamp(SAMPLE_NOW - 720)),
    };
    or.money = vec![
        sample_meter("wallet", "Credit balance", MoneyKind::Balance, "13.67", None, None),
        sample_meter("spend.daily", "Spend today", MoneyKind::Spend, "0", None, Some(PeriodKind::Daily)),
        sample_meter("spend.weekly", "Spend this week", MoneyKind::Spend, "4.08", None, Some(PeriodKind::Weekly)),
        sample_meter("spend.monthly", "Spend this month", MoneyKind::Spend, "4.46", None, Some(PeriodKind::Monthly)),
        sample_meter("key_limit", "Key cap", MoneyKind::Limit, "9.00", Some("50"), Some(PeriodKind::Monthly)),
    ];

    let mut oll = AccountObservation::new(
        account_id(Origin::Monitor, "oll-main"),
        SourceId::OllamaCloud,
        AuthKind::ReadOnly,
        Origin::Monitor,
        "oll-main",
    );
    oll.plan = Some("pro".to_string());
    oll.freshness = Freshness::Fresh;
    oll.failure = Some(Failure::new(FailureKind::Unavailable, "ollama.com did not answer"));
    oll.windows = vec![sample_window("session", "5h", Some(93.0), None, None)];
    oll.money = vec![sample_meter(
        "wallet",
        "Credit balance",
        MoneyKind::Balance,
        "0.40",
        None,
        None,
    )];

    let mut up = AccountObservation::new(
        account_id(Origin::Upstream, "personal"),
        SourceId::UpstreamClauth,
        AuthKind::ReadOnly,
        Origin::Upstream,
        "personal",
    );
    up.provider = "Anthropic".to_string();
    up.active = true;
    up.freshness = Freshness::Fresh;
    up.windows = vec![sample_window("session", "5h", Some(29.0), Some(4_320), Some(18_000))];

    vec![work, or, oll, up]
}
