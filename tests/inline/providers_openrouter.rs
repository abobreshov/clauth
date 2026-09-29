#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Inline tests for the OpenRouter provider (v2): base-URL matching, raw-number
//! parsing, the `/key`-first fetch order over a recording transport, per-meter
//! degradation of `/credits`, the management-key path allowlist, the typed
//! meters, and the v1-compatible display rows. Every response is a fixture
//! under `tests/fixtures/openrouter/`; the recorder answers from a script and
//! [`LiveHttp`] panics under `cfg(test)`, so nothing here can reach the
//! network.

use std::cell::RefCell;
use std::collections::VecDeque;

use super::*;

use crate::providers::Provider;
use crate::usage::observation::{
    AccountObservation, AuthKind, MoneyKind, MoneyScope, Origin, PeriodKind, ScopeOrigin, SourceId,
    WindowScope,
};

const KEY_PAID: &str = include_str!("../fixtures/openrouter/key_paid.json");
const KEY_FREE: &str = include_str!("../fixtures/openrouter/key_free.json");
const KEY_LIMITED: &str = include_str!("../fixtures/openrouter/key_limited.json");
const KEY_SUBCENT: &str = include_str!("../fixtures/openrouter/key_subcent.json");
const CREDITS_OVERDRAWN: &str = include_str!("../fixtures/openrouter/credits_overdrawn.json");
const CREDITS_FUNDED: &str = include_str!("../fixtures/openrouter/credits_funded.json");
const CREDITS_403: &str = include_str!("../fixtures/openrouter/credits_403.json");

/// Placeholder credentials. Neither is a real key; both must stay out of
/// every output.
const INFERENCE: &str = "sk-or-v1-test-inference-not-a-real-key-0000000000";
const MANAGEMENT: &str = "sk-or-v1-test-management-not-a-real-key-111111111";

const KEY_URL: &str = "https://openrouter.ai/api/v1/key";
const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";

/// 2026-09-30T12:00:00Z, a Wednesday.
fn now() -> i64 {
    chrono::NaiveDate::from_ymd_opt(2026, 9, 30)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

fn ts(y: i32, m: u32, d: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

/// Scripted transport: answers in order and records `(url, bearer)` per call.
struct Recorder {
    replies: RefCell<VecDeque<HttpReply>>,
    calls: RefCell<Vec<(String, String)>>,
}

impl Recorder {
    fn new(replies: Vec<HttpReply>) -> Self {
        Self {
            replies: RefCell::new(replies.into()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn urls(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|(u, _)| u.clone()).collect()
    }
}

impl OpenRouterHttp for Recorder {
    fn get(&self, url: &str, bearer: &str) -> HttpReply {
        self.calls
            .borrow_mut()
            .push((url.to_string(), bearer.to_string()));
        self.replies
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| panic!("unscripted request to {url}"))
    }
}

fn body(s: &str) -> HttpReply {
    HttpReply::Body(s.to_string())
}

fn status(code: u16) -> HttpReply {
    HttpReply::Status {
        code,
        retry_after: None,
    }
}

fn fetch_with(key: HttpReply, credits: HttpReply, billing: Option<&str>) -> OpenRouterUsage {
    let http = Recorder::new(vec![key, credits]);
    fetch_openrouter_usage(INFERENCE, billing, &http).expect("fetch")
}

fn meter<'a>(m: &'a ObservedMeters, id: &str) -> Option<&'a MoneyMeter> {
    m.money.iter().find(|x| x.meter_id == id)
}

fn amt(s: &str) -> Amount {
    Amount::parse(s).unwrap()
}

// ── Provider::from_base_url dispatch ───────────────────────────────────────────
//
// Asserted through the dispatch, not the module fn: the module fn passing while
// the `from_base_url` arm is missing would silently route an OpenRouter profile
// through the generic scanner, and a mutation that drops the arm must red here.

#[test]
fn from_base_url_dispatches_openrouter() {
    for url in [
        "https://openrouter.ai",
        "https://openrouter.ai/api",
        "https://openrouter.ai/api/v1",
        "https://openrouter.ai/api/v1/chat/completions",
        // Hosts are case-insensitive (RFC 3986).
        "https://OPENROUTER.AI/api",
        // An explicit port is still the provider.
        "https://openrouter.ai:443/api",
    ] {
        assert_eq!(
            Provider::from_base_url(url),
            Some(Provider::OpenRouter),
            "{url}"
        );
    }
}

#[test]
fn from_base_url_rejects_host_extension_and_userinfo() {
    // A bare prefix match would claim these and send the profile's API key to
    // the real provider endpoint.
    assert_eq!(
        Provider::from_base_url("https://openrouter.ai.evil.tld"),
        None
    );
    // Everything before an `@` is userinfo, so this host is `evil.tld`.
    assert_eq!(
        Provider::from_base_url("https://openrouter.ai:443@evil.tld"),
        None
    );
    assert_eq!(Provider::from_base_url("http://openrouter.ai"), None);
    assert_eq!(Provider::from_base_url("https://api.anthropic.com"), None);
}

// ── raw numbers ────────────────────────────────────────────────────────────────

#[test]
fn json_numbers_become_exact_decimals() {
    for (raw, want) in [
        ("0.000001536", Some("0.000001536")),
        ("162.870592992", Some("162.870592992")),
        // An f64 would print this as 0.0004.
        ("0.000400000000000001", Some("0.000400000000000001")),
        ("1.5e-7", Some("0.00000015")),
        ("1E3", Some("1000")),
        ("2.5e+2", Some("250")),
        ("-3", Some("-3")),
        ("0", Some("0")),
        ("\"2.50\"", Some("2.50")),
        ("null", None),
        ("true", None),
        ("\"12 USD\"", None),
        ("1e999", None),
    ] {
        let got = json_number_amount(raw);
        assert_eq!(got.as_ref().map(Amount::as_str), want, "{raw}");
    }
}

#[test]
fn exact_subtraction_keeps_sub_cent_and_sign() {
    for (a, b, want) in [
        ("600.815", "601.014979078", "-0.199979078"),
        ("100", "40.123456789", "59.876543211"),
        ("0.1", "0.3", "-0.2"),
        ("5", "5.00", "0.00"),
        ("-1.5", "2", "-3.5"),
    ] {
        assert_eq!(amount_sub(&amt(a), &amt(b)).as_str(), want, "{a} - {b}");
    }
}

#[test]
fn the_key_body_parses_every_modelled_field_from_raw_json() {
    let key: KeyEnvelope = serde_json::from_str(KEY_LIMITED).unwrap();
    let k = key.data;
    assert_eq!(k.limit.as_ref().unwrap().0.as_str(), "25");
    assert_eq!(k.limit_remaining.as_ref().unwrap().0.as_str(), "3.5");
    assert_eq!(k.limit_reset.as_deref(), Some("monthly"));
    assert_eq!(k.usage.as_ref().unwrap().0.as_str(), "71.25");
    assert_eq!(k.usage_daily.as_ref().unwrap().0.as_str(), "1.2");
    assert_eq!(k.usage_weekly.as_ref().unwrap().0.as_str(), "6.75");
    assert_eq!(k.usage_monthly.as_ref().unwrap().0.as_str(), "21.5");
    assert_eq!(k.workspace_id.as_deref(), Some("ws_acme_research"));
    assert_eq!(k.organization_id.as_deref(), Some("org_acme_0042"));
    assert_eq!(k.creator_user_id.as_deref(), Some("user_member_0007"));
    assert_eq!(k.expires_at.as_deref(), Some("2026-12-31T23:59:59Z"));
    assert_eq!(
        k.free_model_daily_requests,
        Some(FreeDaily {
            used: Some(50.0),
            limit: Some(50.0),
            remaining: Some(0.0),
        })
    );
    // The org wins over the member for "which account owns this key".
    assert_eq!(k.owner_id(), Some("org_acme_0042"));
}

#[test]
fn a_captured_paid_key_keeps_its_sub_cent_figures_exact() {
    let key: KeyEnvelope = serde_json::from_str(KEY_PAID).unwrap();
    let k = key.data;
    assert_eq!(k.usage_daily.as_ref().unwrap().0.as_str(), "0.000001536");
    assert_eq!(k.usage_monthly.as_ref().unwrap().0.as_str(), "0.003722806");
    assert_eq!(k.byok_usage.as_ref().unwrap().0.as_str(), "9.49797775");
    assert_eq!(k.limit, None, "null cap");
    assert_eq!(k.owner_id(), None, "the captured body predates owner ids");
}

#[test]
fn bodies_missing_required_parts_fail_to_parse() {
    // The wallet needs both numbers; a degraded body never invents one.
    assert!(serde_json::from_str::<CreditsEnvelope>(r#"{"data":{}}"#).is_err());
    assert!(
        serde_json::from_str::<CreditsEnvelope>(
            r#"{"data":{"total_credits":"lots","total_usage":1}}"#
        )
        .is_err()
    );
    // `data` is required: an error envelope never reads as usable usage.
    assert!(serde_json::from_str::<KeyEnvelope>("{}").is_err());
    assert!(serde_json::from_str::<KeyEnvelope>(CREDITS_403).is_err());
}

// ── fetch order and degradation ────────────────────────────────────────────────

#[test]
fn key_is_fetched_before_credits_with_the_inference_key() {
    let http = Recorder::new(vec![body(KEY_PAID), body(CREDITS_FUNDED)]);
    fetch_openrouter_usage(INFERENCE, None, &http).unwrap();
    let calls = http.calls.borrow();
    assert_eq!(
        *calls,
        vec![
            (KEY_URL.to_string(), INFERENCE.to_string()),
            (CREDITS_URL.to_string(), INFERENCE.to_string()),
        ]
    );
}

#[test]
fn a_management_key_reaches_credits_and_nothing_else() {
    let http = Recorder::new(vec![body(KEY_LIMITED), body(CREDITS_FUNDED)]);
    let usage = fetch_openrouter_usage(INFERENCE, Some(MANAGEMENT), &http).unwrap();
    let calls = http.calls.borrow();
    assert_eq!(
        *calls,
        vec![
            (KEY_URL.to_string(), INFERENCE.to_string()),
            (CREDITS_URL.to_string(), MANAGEMENT.to_string()),
        ],
        "the auth probe runs on the inference key; the management key reads only the wallet"
    );
    assert_eq!(
        usage.wallet.as_ref().unwrap().read_with,
        WalletCredential::Management
    );
}

#[test]
fn a_blank_management_key_is_no_management_key() {
    let http = Recorder::new(vec![body(KEY_PAID), body(CREDITS_FUNDED)]);
    fetch_openrouter_usage(INFERENCE, Some("   "), &http).unwrap();
    assert_eq!(http.calls.borrow()[1].1, INFERENCE);
}

#[test]
fn the_management_key_path_allowlist_is_credits_only() {
    assert!(billing_key_may_reach(CREDITS_PATH));
    for path in [KEY_PATH, "/api/v1/keys", "/api/v1/chat/completions", ""] {
        assert!(!billing_key_may_reach(path), "{path}");
    }
    // The guard refuses before sending: nothing reaches the transport.
    let http = Recorder::new(vec![]);
    let reply = guarded_get(&http, KEY_PATH, MANAGEMENT, WalletCredential::Management);
    assert_eq!(reply, status(403));
    assert!(http.calls.borrow().is_empty());
}

#[test]
fn credits_403_drops_only_the_wallet_meter() {
    let usage = fetch_with(
        body(KEY_LIMITED),
        HttpReply::Status {
            code: 403,
            retry_after: None,
        },
        None,
    );
    assert_eq!(usage.wallet, None);
    assert_eq!(usage.notes.len(), 1);
    assert!(
        usage.notes[0].contains("management key") && usage.notes[0].contains("403"),
        "{:?}",
        usage.notes
    );
    let st = stats(&usage, now());
    let observed = st.observed.as_ref().unwrap();
    assert!(meter(observed, METER_WALLET).is_none());
    for id in [
        "spend.daily",
        "spend.weekly",
        "spend.monthly",
        "spend.lifetime",
        METER_KEY_LIMIT,
    ] {
        assert!(meter(observed, id).is_some(), "{id} survives a 403 wallet");
    }
    assert_eq!(observed.notes, usage.notes);
    // No wallet is no verdict: the account is not called unfunded.
    assert!(st.is_available);
    assert!(
        !st.rows
            .iter()
            .any(|r| crate::providers::is_balance_row(&r.label)),
        "no balance row is invented"
    );
    assert!(
        st.rows
            .iter()
            .any(|r| r.kind == StatRowKind::Faint && r.value.contains("403")),
        "the note renders"
    );
}

#[test]
fn every_credits_failure_degrades_and_none_marks_the_key_dead() {
    for (reply, billing, needle) in [
        (status(404), None, "404"),
        (status(401), None, "401"),
        (status(401), Some(MANAGEMENT), "expired"),
        (status(403), Some(MANAGEMENT), "management key refused"),
        (status(429), None, "rate limited"),
        (status(500), None, "500"),
        (HttpReply::Network, None, "network"),
        (body("<html>oops</html>"), None, "unreadable"),
        (body(CREDITS_403), None, "unreadable"),
    ] {
        let usage = fetch_with(body(KEY_PAID), reply.clone(), billing);
        assert_eq!(usage.wallet, None, "{reply:?}");
        assert!(
            usage.notes.iter().any(|n| n.contains(needle)),
            "{reply:?}: {:?}",
            usage.notes
        );
    }
}

#[test]
fn key_failures_fail_the_fetch_before_credits_is_tried() {
    type IsExpected = fn(&ThirdPartyError) -> bool;
    let cases: Vec<(HttpReply, IsExpected)> = vec![
        (status(401), |e| matches!(e, ThirdPartyError::AuthExpired)),
        (
            HttpReply::Status {
                code: 429,
                retry_after: Some(Duration::from_secs(30)),
            },
            |e| {
                matches!(
                    e,
                    ThirdPartyError::RateLimited {
                        retry_after: Some(d)
                    } if *d == Duration::from_secs(30)
                )
            },
        ),
        (status(403), |e| matches!(e, ThirdPartyError::Status)),
        (status(503), |e| matches!(e, ThirdPartyError::Status)),
        (HttpReply::Network, |e| {
            matches!(e, ThirdPartyError::Network)
        }),
        (body("not json"), |e| matches!(e, ThirdPartyError::Parse)),
        (body(CREDITS_403), |e| matches!(e, ThirdPartyError::Parse)),
    ];
    for (reply, is_expected) in cases {
        let http = Recorder::new(vec![reply.clone()]);
        let err = fetch_openrouter_usage(INFERENCE, Some(MANAGEMENT), &http)
            .expect_err("a /key failure fails the fetch");
        assert!(is_expected(&err), "{reply:?} → {err:?}");
        assert_eq!(http.urls(), vec![KEY_URL.to_string()], "{reply:?}");
    }
}

#[test]
#[should_panic(expected = "real network call attempted in a test")]
fn the_live_transport_refuses_to_run_under_test() {
    let _ = LiveHttp.get(KEY_URL, INFERENCE);
}

// ── meters ─────────────────────────────────────────────────────────────────────

#[test]
fn a_negative_balance_is_an_exact_signed_wallet() {
    let usage = fetch_with(body(KEY_PAID), body(CREDITS_OVERDRAWN), None);
    let m = observed_meters(&usage, now());
    let wallet = meter(&m, METER_WALLET).unwrap();
    assert_eq!(wallet.kind, MoneyKind::Balance);
    assert_eq!(wallet.amount.as_str(), "-0.199979078");
    assert!(wallet.amount.is_negative());
    assert_eq!(wallet.limit, Some(amt("600.815")));
    assert_eq!(wallet.currency, "USD");
    assert_eq!(wallet.scope, MoneyScope::Organization);
    assert_eq!(wallet.scope_origin, ScopeOrigin::Provider);
    assert_eq!(wallet.scope_id, None, "the captured key names no owner");
}

#[test]
fn a_funded_org_wallet_carries_the_owner_for_de_dup() {
    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_FUNDED), None);
    let m = observed_meters(&usage, now());
    let wallet = meter(&m, METER_WALLET).unwrap();
    assert_eq!(wallet.amount.as_str(), "59.876543211");
    assert_eq!(wallet.scope_id.as_deref(), Some("org_acme_0042"));
    assert_eq!(wallet.scope_origin, ScopeOrigin::Provider);
}

#[test]
fn a_management_key_wallet_is_never_bound_or_de_duplicated() {
    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_FUNDED), Some(MANAGEMENT));
    let m = observed_meters(&usage, now());
    let wallet = meter(&m, METER_WALLET).unwrap();
    assert_eq!(wallet.scope_id, None);
    assert_eq!(
        wallet.scope_origin,
        ScopeOrigin::MonitoringCredential { bound: false }
    );
}

#[test]
fn spend_rows_are_three_key_scoped_periods_plus_lifetime() {
    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_FUNDED), None);
    let m = observed_meters(&usage, now());
    let day = meter(&m, "spend.daily").unwrap();
    assert_eq!(day.amount.as_str(), "1.2");
    assert_eq!(day.kind, MoneyKind::Spend);
    assert_eq!(day.scope, MoneyScope::Key);
    let p = day.period.unwrap();
    assert_eq!(p.kind, PeriodKind::Daily);
    assert_eq!(p.start.unwrap().secs(), ts(2026, 9, 30));
    assert_eq!(p.end.unwrap().secs(), ts(2026, 10, 1));
    assert!(p.derived);

    let week = meter(&m, "spend.weekly").unwrap();
    assert_eq!(week.amount.as_str(), "6.75");
    let p = week.period.unwrap();
    // Monday–Sunday UTC: 2026-09-30 is a Wednesday.
    assert_eq!(p.start.unwrap().secs(), ts(2026, 9, 28));
    assert_eq!(p.end.unwrap().secs(), ts(2026, 10, 5));

    let month = meter(&m, "spend.monthly").unwrap();
    assert_eq!(month.amount.as_str(), "21.5");
    let p = month.period.unwrap();
    assert_eq!(p.start.unwrap().secs(), ts(2026, 9, 1));
    assert_eq!(p.end.unwrap().secs(), ts(2026, 10, 1));

    let life = meter(&m, "spend.lifetime").unwrap();
    assert_eq!(life.amount.as_str(), "71.25");
    assert_eq!(life.scope, MoneyScope::Key);
    assert_eq!(life.period.unwrap().kind, PeriodKind::Lifetime);
}

#[test]
fn a_december_month_ends_in_january() {
    let p = utc_period(PeriodKind::Monthly, ts(2026, 12, 15) + 3600);
    assert_eq!(p.start.unwrap().secs(), ts(2026, 12, 1));
    assert_eq!(p.end.unwrap().secs(), ts(2027, 1, 1));
}

#[test]
fn a_key_cap_is_a_limit_meter_left_under_cap() {
    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_FUNDED), None);
    let m = observed_meters(&usage, now());
    let cap = meter(&m, METER_KEY_LIMIT).unwrap();
    assert_eq!(cap.kind, MoneyKind::Limit);
    assert_eq!(cap.amount.as_str(), "3.5");
    assert_eq!(cap.limit, Some(amt("25")));
    assert_eq!(cap.scope, MoneyScope::Key);
    assert_eq!(cap.period.unwrap().kind, PeriodKind::Monthly);

    // A null cap is no meter at all.
    let usage = fetch_with(body(KEY_PAID), body(CREDITS_FUNDED), None);
    assert!(meter(&observed_meters(&usage, now()), METER_KEY_LIMIT).is_none());
}

#[test]
fn sub_cent_figures_stay_exact_in_meters_and_round_in_rows() {
    let usage = fetch_with(body(KEY_SUBCENT), body(CREDITS_FUNDED), None);
    let st = stats(&usage, now());
    let m = st.observed.as_ref().unwrap();
    assert_eq!(
        meter(m, "spend.daily").unwrap().amount.as_str(),
        "0.00000015"
    );
    assert_eq!(
        meter(m, "spend.monthly").unwrap().amount.as_str(),
        "0.000400000000000001"
    );
    let cap = meter(m, METER_KEY_LIMIT).unwrap();
    assert_eq!(cap.amount.as_str(), "0.00999985");
    assert_eq!(cap.period.unwrap().kind, PeriodKind::Daily);
    assert_eq!(
        meter(m, METER_WALLET).unwrap().scope_id.as_deref(),
        Some("user_tiny_0003"),
        "no org: the minting user owns the key"
    );
    let row = |label: &str| {
        st.rows
            .iter()
            .find(|r| r.label == label)
            .map(|r| r.value.clone())
    };
    assert_eq!(row("today").as_deref(), Some("0.00 USD"));
    assert_eq!(row("key limit left").as_deref(), Some("0.01 USD"));
}

#[test]
fn byok_spend_is_non_additive_and_only_when_used() {
    let usage = fetch_with(body(KEY_PAID), body(CREDITS_FUNDED), None);
    let m = observed_meters(&usage, now());
    let life = meter(&m, "byok.lifetime").unwrap();
    assert_eq!(life.amount.as_str(), "9.49797775");
    assert!(!life.additive);
    assert!(meter(&m, "byok.daily").is_some_and(|d| d.amount.is_zero()));

    let usage = fetch_with(body(KEY_FREE), body(CREDITS_FUNDED), None);
    let m = observed_meters(&usage, now());
    assert!(
        !m.money.iter().any(|x| x.meter_id.starts_with("byok.")),
        "an account with no BYOK spend gets no BYOK rows"
    );
}

#[test]
fn free_model_daily_requests_is_an_account_window_outside_the_chain() {
    let usage = fetch_with(body(KEY_FREE), body(CREDITS_FUNDED), None);
    let st = stats(&usage, now());
    let w = &st.observed.as_ref().unwrap().windows[0];
    assert_eq!(w.id, WINDOW_FREE_DAILY);
    assert_eq!(w.scope, WindowScope::Account);
    assert!(!w.chain_eligible);
    assert_eq!(w.used, Some(12.0));
    assert_eq!(w.limit, Some(50.0));
    assert_eq!(w.used_pct, Some(24.0));
    assert!(!w.exhausted);
    assert_eq!(w.window_secs, Some(86_400));
    assert_eq!(w.resets_at.unwrap().secs(), ts(2026, 10, 1), "UTC midnight");

    // The Usage tab's bar, and nothing the chain reads.
    assert_eq!(st.bars.len(), 1);
    assert_eq!(st.bars[0].label, FREE_DAILY_LABEL);
    assert_eq!(st.bars[0].pct, 24.0);
    assert_eq!(st.bars[0].used, Some(12.0));
    assert_eq!(st.bars[0].total, Some(50.0));
    assert!(st.to_usage_info().is_none(), "chain behaviour unchanged");

    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_FUNDED), None);
    let w = &observed_meters(&usage, now()).windows[0];
    assert!(w.exhausted, "50 of 50 used");
}

#[test]
fn a_key_without_the_free_counter_publishes_no_window() {
    let usage = fetch_with(body(KEY_PAID), body(CREDITS_FUNDED), None);
    let st = stats(&usage, now());
    assert!(st.observed.as_ref().unwrap().windows.is_empty());
    assert!(st.bars.is_empty());
}

// ── display rows (v1-compatible) ───────────────────────────────────────────────

#[test]
fn stats_builds_the_v1_wallet_rows_from_exact_amounts() {
    let usage = fetch_with(body(KEY_PAID), body(CREDITS_OVERDRAWN), None);
    let st = stats(&usage, now());
    // The live account is overdrawn: the rows render, but the reachability
    // dot must read red and the refusal rides beside the figures.
    assert!(!st.is_available);
    let last = st.rows.last().expect("refusal row");
    assert_eq!(last.kind, StatRowKind::Danger);
    assert_eq!(last.value, crate::providers::LOW_BALANCE);
    // Heading + 3 wallet rows + 3 period rows, plus that refusal.
    assert_eq!(st.rows.len(), 8);
    assert_eq!(st.rows[0].kind, StatRowKind::Heading);
    assert_eq!(st.rows[0].label, "credits");
    // The literal, not the constant: this row's label is a cross-module
    // contract (the MCP roster's wallet rank matches on it).
    assert_eq!(st.rows[1].label, "api balance");
    assert_eq!(st.rows[1].value, "-0.20 USD");
    assert_eq!(st.rows[1].kind, StatRowKind::Danger);
    let labels: Vec<&str> = st.rows[2..7].iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        ["used", "purchased", "today", "this week", "this month"]
    );
    assert_eq!(st.rows[2].value, "601.01 USD");
    assert_eq!(st.rows[3].value, "600.82 USD");
    assert_eq!(st.rows[4].value, "0.00 USD");
}

#[test]
fn remaining_danger_boundary_tracks_the_rendered_value() {
    // Anything under half a cent (an overdrawn account included) renders as
    // `0.00 USD` or worse, so it must read Danger and unfunded.
    for (total, used, expect_kind, expect_value, expect_available) in [
        ("100", "100", StatRowKind::Danger, "0.00 USD", false),
        ("100", "99.999", StatRowKind::Danger, "0.00 USD", false),
        ("100", "100.001", StatRowKind::Danger, "-0.00 USD", false),
        ("100", "100.2", StatRowKind::Danger, "-0.20 USD", false),
        ("100", "99.995", StatRowKind::Body, "0.01 USD", true),
        ("100", "99.99", StatRowKind::Body, "0.01 USD", true),
    ] {
        let usage = OpenRouterUsage {
            key: KeySnapshot::default(),
            wallet: Some(WalletRead {
                total_credits: amt(total),
                total_usage: amt(used),
                read_with: WalletCredential::Inference,
            }),
            notes: Vec::new(),
        };
        let st = stats(&usage, now());
        assert_eq!(st.rows[1].kind, expect_kind, "{total} - {used}");
        assert_eq!(st.rows[1].value, expect_value, "{total} - {used}");
        assert_eq!(st.is_available, expect_available, "{total} - {used}");
    }
}

#[test]
fn key_cap_and_free_tier_rows_append_when_present() {
    let usage = fetch_with(body(KEY_FREE), body(CREDITS_FUNDED), None);
    let st = stats(&usage, now());
    assert!(
        st.rows
            .iter()
            .any(|r| r.label == "free tier" && r.kind == StatRowKind::Faint)
    );
    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_FUNDED), None);
    let st = stats(&usage, now());
    let rows: Vec<(&str, &str)> = st
        .rows
        .iter()
        .map(|r| (r.label.as_str(), r.value.as_str()))
        .collect();
    assert!(rows.contains(&("key limit", "25.00 USD")));
    assert!(rows.contains(&("key limit left", "3.50 USD")));
    assert!(rows.contains(&("api balance", "59.88 USD")));
}

// ── cache + projection ─────────────────────────────────────────────────────────

#[test]
fn the_cache_round_trips_and_the_observation_reads_the_raw_meters() {
    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_OVERDRAWN), None);
    let st = stats(&usage, now());
    let json = serde_json::to_string(&st).unwrap();
    let back: ThirdPartyStats = serde_json::from_str(&json).unwrap();
    assert_eq!(back.observed, st.observed);

    let mut obs = AccountObservation::new(
        "claude:or".to_string(),
        SourceId::OpenRouter,
        AuthKind::ApiKey,
        Origin::Profile,
        "or",
    );
    crate::usage::project::apply_third_party(&mut obs, &back, now());
    assert_eq!(
        obs.meter(METER_WALLET).unwrap().amount.as_str(),
        "-0.199979078"
    );
    assert_eq!(
        obs.meter("spend.lifetime").unwrap().scope,
        MoneyScope::Key,
        "the raw meters win over re-parsing the rounded rows"
    );
    assert_eq!(obs.window(WINDOW_FREE_DAILY).unwrap().used_pct, Some(100.0));
    // The unfunded verdict still lands.
    assert!(obs.failure.is_some());

    // Past its reset the free window is the previous day's reading: dropped.
    let mut later = obs.clone();
    crate::usage::project::apply_third_party(&mut later, &back, ts(2026, 10, 1) + 1);
    assert!(later.window(WINDOW_FREE_DAILY).is_none());
}

#[test]
fn no_credential_reaches_the_cache_or_the_notes() {
    for credits in [body(CREDITS_FUNDED), status(403), status(401)] {
        let usage = fetch_with(body(KEY_LIMITED), credits, Some(MANAGEMENT));
        let json = serde_json::to_string(&stats(&usage, now())).unwrap();
        assert!(!json.contains(INFERENCE));
        assert!(!json.contains(MANAGEMENT));
        assert!(!json.contains("test-management"));
    }
}

/// The monitoring leg with only a management key: one `GET /api/v1/credits`
/// with it, never `/api/v1/key`; the wallet is an unbound monitoring meter and
/// nothing key-scoped is invented.
#[test]
fn a_wallet_only_fetch_sends_the_management_key_to_credits_alone() {
    let http = Recorder::new(vec![body(CREDITS_FUNDED)]);
    let usage = fetch_openrouter_wallet(MANAGEMENT, &http).expect("wallet");
    assert_eq!(http.urls(), [CREDITS_URL]);
    assert_eq!(http.calls.borrow()[0].1, MANAGEMENT);
    let m = observed_meters(&usage, now());
    assert_eq!(m.money.len(), 1, "the wallet alone: {:?}", m.money);
    let wallet = meter(&m, METER_WALLET).unwrap();
    assert_eq!(
        wallet.scope_origin,
        ScopeOrigin::MonitoringCredential { bound: false }
    );
    assert!(m.windows.is_empty());
}

/// On the wallet-only leg a `/credits` failure is the whole fetch, and a blank
/// key sends nothing.
#[test]
fn a_wallet_only_fetch_maps_every_failure() {
    for (reply, want) in [
        (status(401), ThirdPartyError::AuthExpired),
        (status(403), ThirdPartyError::Status),
        (status(500), ThirdPartyError::Status),
        (HttpReply::Network, ThirdPartyError::Network),
        (body("{not json"), ThirdPartyError::Parse),
    ] {
        let http = Recorder::new(vec![reply]);
        let err = fetch_openrouter_wallet(MANAGEMENT, &http).unwrap_err();
        assert_eq!(std::mem::discriminant(&err), std::mem::discriminant(&want));
    }
    let http = Recorder::new(vec![status(429)]);
    assert!(matches!(
        fetch_openrouter_wallet(MANAGEMENT, &http),
        Err(ThirdPartyError::RateLimited { .. })
    ));
    let http = Recorder::new(Vec::new());
    assert!(matches!(
        fetch_openrouter_wallet("  ", &http),
        Err(ThirdPartyError::AuthExpired)
    ));
    assert!(http.urls().is_empty());
}

// ── an unbound management wallet (plan §4.7 (a)) ───────────────────────────────

/// A management key from an UNRELATED account: the inference key belongs to
/// `org_acme_0042` and is healthy, while the management key's own wallet is
/// overdrawn. That wallet is published as its own unbound meter and must not
/// decide the inference account's availability: no "balance too low", no
/// projected `QuotaExhausted`, and no balance row the roster ranks on.
#[test]
fn an_unbound_management_wallet_never_drives_the_inference_account() {
    let usage = fetch_with(body(KEY_LIMITED), body(CREDITS_OVERDRAWN), Some(MANAGEMENT));
    assert_eq!(
        usage.wallet.as_ref().map(|w| w.read_with),
        Some(WalletCredential::Management)
    );
    let st = stats(&usage, now());
    assert!(
        st.is_available,
        "an unbound wallet is no verdict on this key"
    );
    assert!(
        !st.rows
            .iter()
            .any(|r| r.value == crate::providers::LOW_BALANCE),
        "{:?}",
        st.rows
    );
    assert!(
        crate::providers::funded_wallets(&st.rows).is_empty()
            && !st
                .rows
                .iter()
                .any(|r| crate::providers::is_balance_row(&r.label)),
        "the unbound wallet never ranks as this account's balance: {:?}",
        st.rows
    );
    // The figure still renders, under its own label.
    let row = st
        .rows
        .iter()
        .find(|r| r.label == UNBOUND_WALLET_ROW_LABEL)
        .expect("the monitoring wallet row");
    assert_eq!(row.value, "-0.20 USD");

    // Its meter is separate and unbound.
    let observed = st.observed.as_ref().unwrap();
    let wallet = meter(observed, METER_WALLET).unwrap();
    assert_eq!(
        wallet.scope_origin,
        ScopeOrigin::MonitoringCredential { bound: false }
    );
    assert_eq!(wallet.scope_id, None);

    // Projected, the inference account is not exhausted.
    let mut obs = AccountObservation::new(
        "claude:or".to_string(),
        SourceId::OpenRouter,
        AuthKind::ApiKey,
        Origin::Profile,
        "or",
    );
    crate::usage::project::apply_third_party(&mut obs, &st, now());
    assert_eq!(obs.failure, None, "{:?}", obs.failure);
    assert!(
        obs.meter(METER_WALLET).is_some(),
        "the meter is still published"
    );

    // The same overdrawn wallet read by the inference key itself IS this
    // account's: unfunded, as before.
    let own = fetch_with(body(KEY_LIMITED), body(CREDITS_OVERDRAWN), None);
    assert!(!stats(&own, now()).is_available);
}

// ── the /credits backoff ────────────────────────────────────────────────────────

fn rate_limited(secs: u64) -> HttpReply {
    HttpReply::Status {
        code: 429,
        retry_after: Some(Duration::from_secs(secs)),
    }
}

/// A `/credits` 429 keeps the key meters and holds the wallet credential for
/// its `Retry-After`: the next fetch inside the hold reads `/key` alone, and
/// one after it reads `/credits` again.
#[test]
fn a_credits_429_holds_the_wallet_for_its_retry_after_and_keeps_the_key() {
    let holds = WalletHolds::default();
    let t0: u64 = 1_000_000;
    let retry = 10 * 60; // past the floor, inside the cap
    let http = Recorder::new(vec![body(KEY_LIMITED), rate_limited(retry)]);
    let usage =
        fetch_openrouter_usage_held(INFERENCE, Some(MANAGEMENT), &http, &holds, t0).unwrap();
    assert_eq!(usage.wallet, None);
    assert!(usage.key.limit.is_some(), "the key meters survive");
    assert!(
        usage.notes.iter().any(|n| n.contains("429")),
        "{:?}",
        usage.notes
    );
    assert!(holds.remaining(MANAGEMENT, t0).is_some());

    // Inside the hold: `/key` only, and the note says the wallet is held.
    let inside = t0 + 60_000;
    let http = Recorder::new(vec![body(KEY_LIMITED)]);
    let usage =
        fetch_openrouter_usage_held(INFERENCE, Some(MANAGEMENT), &http, &holds, inside).unwrap();
    assert_eq!(http.urls(), [KEY_URL], "no /credits while held");
    assert!(
        usage.notes.iter().any(|n| n.contains("held")),
        "{:?}",
        usage.notes
    );
    let st = stats(&usage, now());
    assert!(st.is_available);
    assert!(meter(st.observed.as_ref().unwrap(), "spend.daily").is_some());

    // A different wallet credential is not held.
    let http = Recorder::new(vec![body(KEY_LIMITED), body(CREDITS_FUNDED)]);
    fetch_openrouter_usage_held(INFERENCE, None, &http, &holds, inside).unwrap();
    assert_eq!(http.urls(), [KEY_URL, CREDITS_URL]);

    // Past the Retry-After: `/credits` again.
    let after = t0 + retry * 1000 + 1;
    let http = Recorder::new(vec![body(KEY_LIMITED), body(CREDITS_FUNDED)]);
    let usage =
        fetch_openrouter_usage_held(INFERENCE, Some(MANAGEMENT), &http, &holds, after).unwrap();
    assert_eq!(http.urls(), [KEY_URL, CREDITS_URL]);
    assert!(usage.wallet.is_some());
}

/// A 429 with no (or a tiny) `Retry-After` still holds for the floor.
#[test]
fn a_credits_429_without_retry_after_holds_for_the_floor() {
    let holds = WalletHolds::default();
    let http = Recorder::new(vec![body(KEY_LIMITED), status(429)]);
    fetch_openrouter_usage_held(INFERENCE, None, &http, &holds, 0).unwrap();
    assert_eq!(holds.remaining(INFERENCE, 0), Some(WALLET_HOLD_FLOOR));
    holds.hold(MANAGEMENT, 0, Some(Duration::from_secs(1)));
    assert_eq!(holds.remaining(MANAGEMENT, 0), Some(WALLET_HOLD_FLOOR));
    let floor_ms = u64::try_from(WALLET_HOLD_FLOOR.as_millis()).unwrap();
    assert_eq!(holds.remaining(MANAGEMENT, floor_ms), None);
}

/// The wallet-only leg shares the hold: a held key sends nothing and answers
/// `RateLimited` with the time left.
#[test]
fn the_wallet_only_leg_honours_the_hold() {
    let holds = WalletHolds::default();
    let http = Recorder::new(vec![rate_limited(600)]);
    let err = fetch_openrouter_wallet_held(MANAGEMENT, &http, &holds, 0).unwrap_err();
    assert!(matches!(err, ThirdPartyError::RateLimited { .. }));
    let http = Recorder::new(Vec::new());
    let err = fetch_openrouter_wallet_held(MANAGEMENT, &http, &holds, 300_000).unwrap_err();
    assert!(
        matches!(err, ThirdPartyError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(300)),
        "{err:?}"
    );
    assert!(http.urls().is_empty(), "nothing is sent while held");
}

// ── the persisted /credits hold (across processes) ─────────────────────────────

/// Every hold file under `~/.tollgate/holds/` (the `.json` deadlines, not
/// their `.lock` siblings).
fn hold_files() -> Vec<std::path::PathBuf> {
    let dir = crate::profile::tollgate_dir().unwrap().join("holds");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect()
}

/// The deadline persisted for `bearer`, read raw (no expiry, no clamp).
fn persisted_raw(bearer: &str) -> Option<u64> {
    WalletHolds::read_until(&WalletHolds::hold_path(&WalletHolds::fingerprint(bearer)).unwrap())
}

/// A `/credits` 429 is written to disk (0600, under a 0700 dir, no key in
/// name or body), and a second process — a fresh store with an empty map —
/// reads `/key` alone inside the hold, through the scheduler's own entry;
/// past the `Retry-After` it reads `/credits` again and the file is gone.
#[test]
fn a_persisted_credits_hold_binds_a_second_process_until_it_expires() {
    let _home = crate::testutil::HomeSandbox::new();
    let t0: u64 = 1_000_000;
    let retry = 10 * 60;
    let daemon = WalletHolds::persistent();
    let http = Recorder::new(vec![body(KEY_LIMITED), rate_limited(retry)]);
    fetch_stats_with(INFERENCE, None, &http, &daemon, t0).unwrap();
    assert_eq!(http.urls(), [KEY_URL, CREDITS_URL]);

    let files = hold_files();
    assert_eq!(files.len(), 1, "{files:?}");
    let raw = std::fs::read_to_string(&files[0]).unwrap();
    for text in [raw.as_str(), &files[0].to_string_lossy()] {
        assert!(!text.contains(INFERENCE), "the key reaches disk: {text}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&files[0]), 0o600);
        assert_eq!(mode(&files[0].with_extension("lock")), 0o600);
        assert_eq!(mode(files[0].parent().unwrap()), 0o700);
    }

    // Another process: nothing in memory, the hold read from disk.
    let cli = WalletHolds::persistent();
    let inside = t0 + 60_000;
    assert_eq!(
        cli.remaining(INFERENCE, inside),
        Some(Duration::from_secs(retry - 60))
    );
    let http = Recorder::new(vec![body(KEY_LIMITED)]);
    let st = fetch_stats_with(INFERENCE, None, &http, &cli, inside).unwrap();
    assert_eq!(http.urls(), [KEY_URL], "no /credits inside the hold");
    assert!(st.is_available, "the key meters still refresh");

    // A memory-only store (the tests' default) never sees the file.
    assert_eq!(WalletHolds::default().remaining(INFERENCE, inside), None);

    // Past the Retry-After: `/credits` again, and the expired file is removed.
    let after = t0 + retry * 1000 + 1;
    let later = WalletHolds::persistent();
    let http = Recorder::new(vec![body(KEY_LIMITED), body(CREDITS_FUNDED)]);
    fetch_stats_with(INFERENCE, None, &http, &later, after).unwrap();
    assert_eq!(http.urls(), [KEY_URL, CREDITS_URL]);
    assert!(hold_files().is_empty(), "{:?}", hold_files());
}

/// The wallet-only leg reads the persisted hold too: a second process sends
/// nothing and answers `RateLimited` with the time left.
#[test]
fn the_wallet_only_leg_honours_a_persisted_hold() {
    let _home = crate::testutil::HomeSandbox::new();
    let http = Recorder::new(vec![rate_limited(600)]);
    let err = wallet_stats_with(MANAGEMENT, &http, &WalletHolds::persistent(), 0).unwrap_err();
    assert!(matches!(err, ThirdPartyError::RateLimited { .. }));

    let http = Recorder::new(Vec::new());
    let err =
        wallet_stats_with(MANAGEMENT, &http, &WalletHolds::persistent(), 300_000).unwrap_err();
    assert!(
        matches!(err, ThirdPartyError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(300)),
        "{err:?}"
    );
    assert!(http.urls().is_empty(), "nothing is sent while held");
}

/// A persisted deadline past the retry cap (a skewed clock, a hand edit) is
/// honoured for the cap at most; a torn or foreign file reads as no hold.
#[test]
fn a_persisted_hold_is_clamped_and_a_foreign_file_is_ignored() {
    let _home = crate::testutil::HomeSandbox::new();
    let holds = WalletHolds::persistent();
    holds.hold(INFERENCE, 0, Some(Duration::from_secs(600)));
    let path = hold_files().pop().unwrap();
    std::fs::write(&path, r#"{"version":1,"until_ms":18446744073709551615}"#).unwrap();
    let fresh = WalletHolds::persistent();
    assert_eq!(fresh.remaining(INFERENCE, 0), Some(WalletHolds::cap()));
    let cap_ms = u64::try_from(WalletHolds::cap().as_millis()).unwrap();
    assert_eq!(persisted_raw(INFERENCE), Some(cap_ms));

    std::fs::write(&path, "{not json").unwrap();
    assert_eq!(WalletHolds::persistent().remaining(INFERENCE, 0), None);
}

#[test]
fn a_skewed_persisted_hold_is_rewritten_and_credits_resume_after_the_cap() {
    let _home = crate::testutil::HomeSandbox::new();
    let now_ms = 1_000_000;
    WalletHolds::persistent().hold(INFERENCE, now_ms, Some(Duration::from_secs(600)));
    let path = hold_files().pop().unwrap();
    let skewed = now_ms + 30 * 24 * 60 * 60 * 1000;
    std::fs::write(&path, format!(r#"{{"version":1,"until_ms":{skewed}}}"#)).unwrap();

    let cap_ms = u64::try_from(WalletHolds::cap().as_millis()).unwrap();
    let reader = WalletHolds::persistent();
    assert_eq!(
        reader.remaining(INFERENCE, now_ms),
        Some(WalletHolds::cap())
    );
    assert_eq!(persisted_raw(INFERENCE), Some(now_ms + cap_ms));

    let http = Recorder::new(vec![body(KEY_LIMITED)]);
    fetch_stats_with(INFERENCE, None, &http, &reader, now_ms + cap_ms - 1).unwrap();
    assert_eq!(http.urls(), [KEY_URL]);

    let http = Recorder::new(vec![body(KEY_LIMITED), body(CREDITS_FUNDED)]);
    fetch_stats_with(
        INFERENCE,
        None,
        &http,
        &WalletHolds::persistent(),
        now_ms + cap_ms + 1,
    )
    .unwrap();
    assert_eq!(http.urls(), [KEY_URL, CREDITS_URL]);
    assert_eq!(persisted_raw(INFERENCE), None);
}

#[test]
fn reading_an_in_cap_hold_preserves_its_bytes() {
    let _home = crate::testutil::HomeSandbox::new();
    let now_ms = 1_000_000;
    let until_ms = now_ms + 600_000;
    WalletHolds::persistent().hold(INFERENCE, now_ms, Some(Duration::from_secs(600)));
    let path = hold_files().pop().unwrap();
    let bytes = format!("{{\n  \"version\": 1, \"until_ms\": {until_ms}\n}}\n");
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(
        WalletHolds::persistent().remaining(INFERENCE, now_ms),
        Some(Duration::from_secs(600))
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes.as_bytes());
}

#[test]
fn a_skewed_read_preserves_an_in_cap_renewal_under_the_lock() {
    let _home = crate::testutil::HomeSandbox::new();
    let now_ms = 1_000_000;
    let cap_ms = u64::try_from(WalletHolds::cap().as_millis()).unwrap();
    let cap_until = now_ms + cap_ms;
    WalletHolds::persistent().hold(INFERENCE, now_ms, Some(Duration::from_secs(600)));
    let path = hold_files().pop().unwrap();
    let skewed = now_ms + 30 * 24 * 60 * 60 * 1000;
    std::fs::write(&path, format!(r#"{{"version":1,"until_ms":{skewed}}}"#)).unwrap();
    let seen_until = WalletHolds::read_until(&path).unwrap();
    let lock = WalletHolds::lock_hold(&path).unwrap();
    let renewed_until = now_ms + cap_ms / 2;
    let renewed = format!("{{\n  \"version\": 1, \"until_ms\": {renewed_until}\n}}\n");

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| WalletHolds::clamp_skewed(&path, seen_until, cap_until));
        crate::profile::atomic_write_600(&path, renewed.as_bytes()).unwrap();
        drop(lock);
        assert_eq!(reader.join().unwrap(), renewed_until);
    });
    assert_eq!(std::fs::read(&path).unwrap(), renewed.as_bytes());
}

#[test]
fn a_missing_hold_after_the_unlocked_read_keeps_the_seen_deadline() {
    let _home = crate::testutil::HomeSandbox::new();
    let now_ms = 1_000_000;
    let cap_ms = u64::try_from(WalletHolds::cap().as_millis()).unwrap();
    WalletHolds::persistent().hold(INFERENCE, now_ms, Some(Duration::from_secs(600)));
    let path = hold_files().pop().unwrap();
    let skewed = now_ms + 30 * 24 * 60 * 60 * 1000;
    std::fs::write(&path, format!(r#"{{"version":1,"until_ms":{skewed}}}"#)).unwrap();
    let seen_until = WalletHolds::read_until(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(
        WalletHolds::clamp_skewed(&path, seen_until, now_ms + cap_ms),
        seen_until
    );
}

#[cfg(unix)]
#[test]
fn a_failed_clamp_write_keeps_the_disk_deadline() {
    use std::os::unix::fs::PermissionsExt;

    let _home = crate::testutil::HomeSandbox::new();
    let now_ms = 1_000_000;
    let cap_ms = u64::try_from(WalletHolds::cap().as_millis()).unwrap();
    WalletHolds::persistent().hold(INFERENCE, now_ms, Some(Duration::from_secs(600)));
    let path = hold_files().pop().unwrap();
    let skewed = now_ms + 30 * 24 * 60 * 60 * 1000;
    std::fs::write(&path, format!(r#"{{"version":1,"until_ms":{skewed}}}"#)).unwrap();
    let dir = path.parent().unwrap();
    let permissions = std::fs::metadata(dir).unwrap().permissions();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = WalletHolds::clamp_skewed(&path, skewed, now_ms + cap_ms);
    std::fs::set_permissions(dir, permissions).unwrap();

    assert_eq!(result, skewed);
    assert_eq!(persisted_raw(INFERENCE), Some(skewed));
}

/// Two stores sharing one wallet key (two monitors, or a monitor and the
/// profile scheduler, each under its own flock) never let a shorter 429 cut
/// a longer persisted hold short: the later deadline stays on disk.
#[test]
fn a_shorter_persisted_hold_never_replaces_a_longer_one() {
    let _home = crate::testutil::HomeSandbox::new();
    let long = WalletHolds::persistent();
    long.hold(MANAGEMENT, 0, Some(Duration::from_secs(900)));
    assert_eq!(persisted_raw(MANAGEMENT), Some(900_000));

    // Another process, a minute later, sees a floor-length 429.
    let short = WalletHolds::persistent();
    short.hold(MANAGEMENT, 60_000, None);
    assert_eq!(
        persisted_raw(MANAGEMENT),
        Some(900_000),
        "the longer hold stays"
    );
    assert_eq!(
        WalletHolds::persistent().remaining(MANAGEMENT, 400_000),
        Some(Duration::from_secs(500)),
        "a third process still waits out the longer Retry-After"
    );

    // Within one process the in-memory hold is never shortened either.
    let one = WalletHolds::default();
    one.hold(INFERENCE, 0, Some(Duration::from_secs(900)));
    one.hold(INFERENCE, 0, None);
    assert_eq!(one.remaining(INFERENCE, 0), Some(Duration::from_secs(900)));

    // A later deadline still extends it.
    short.hold(MANAGEMENT, 800_000, None);
    assert_eq!(persisted_raw(MANAGEMENT), Some(1_100_000));
}

/// The interleaving the per-credential flock closes: a reader sees an
/// expired deadline, another process renews the hold before the reader's
/// removal runs, and the removal — a compare-and-remove under the flock —
/// leaves the renewed hold in place. A removal whose read is still current
/// does remove the file.
#[test]
fn a_stale_expired_read_cannot_delete_a_newer_hold() {
    let _home = crate::testutil::HomeSandbox::new();
    let fp = WalletHolds::fingerprint(MANAGEMENT);
    WalletHolds::persistent().hold(MANAGEMENT, 0, None);
    let floor_ms = u64::try_from(WALLET_HOLD_FLOOR.as_millis()).unwrap();
    let now = floor_ms + 1;

    // The reader's unlocked read: expired.
    let seen = persisted_raw(MANAGEMENT).unwrap();
    assert!(seen <= now);

    // Before its removal, another process takes a fresh 429.
    WalletHolds::persistent().hold(MANAGEMENT, now, Some(Duration::from_secs(600)));
    let renewed = now + 600_000;
    assert_eq!(persisted_raw(MANAGEMENT), Some(renewed));

    // The reader's removal of what it saw: refused, the new hold survives.
    assert!(!WalletHolds::remove_if_expired(&fp, seen, now));
    assert_eq!(persisted_raw(MANAGEMENT), Some(renewed));
    assert_eq!(
        WalletHolds::persistent().remaining(MANAGEMENT, now),
        Some(Duration::from_secs(600))
    );

    // A removal still matching the file, once expired, removes it; the lock
    // file stays for the next contender.
    assert!(
        !WalletHolds::remove_if_expired(&fp, renewed, now),
        "not yet expired"
    );
    assert!(WalletHolds::remove_if_expired(&fp, renewed, renewed));
    assert!(hold_files().is_empty(), "{:?}", hold_files());
    let lock = WalletHolds::hold_path(&fp).unwrap().with_extension("lock");
    assert!(lock.exists());
}

/// The same interleaving seen from the reader: its unlocked read finds the
/// expired deadline, a renewal lands while it waits on the flock, and its
/// refused removal reads the renewed hold back instead of reporting none.
/// The test holds the flock itself so the renewal lands in that window; a
/// reader that reads only after the renewal sees it directly, so the result
/// is the same either way.
#[test]
fn a_reader_whose_removal_is_refused_honours_the_renewed_hold() {
    let _home = crate::testutil::HomeSandbox::new();
    let fp = WalletHolds::fingerprint(MANAGEMENT);
    WalletHolds::persistent().hold(MANAGEMENT, 0, None);
    let floor_ms = u64::try_from(WALLET_HOLD_FLOOR.as_millis()).unwrap();
    let now = floor_ms + 1;
    let path = WalletHolds::hold_path(&fp).unwrap();

    let lock = crate::profile::open_state_file(&path.with_extension("lock")).unwrap();
    lock.lock().unwrap();
    let left = std::thread::scope(|scope| {
        let reader = scope.spawn(|| WalletHolds::persistent().remaining(MANAGEMENT, now));
        std::thread::sleep(Duration::from_millis(200));
        let renewed = serde_json::to_vec(&PersistedWalletHold {
            version: WALLET_HOLD_VERSION,
            until_ms: now + 600_000,
        })
        .unwrap();
        crate::profile::atomic_write_600(&path, renewed).unwrap();
        lock.unlock().unwrap();
        reader.join().unwrap()
    });
    assert_eq!(left, Some(Duration::from_secs(600)));
    assert_eq!(persisted_raw(MANAGEMENT), Some(now + 600_000));
}

/// A skewed persisted deadline past the cap is rewritten to the clamped value
/// by the next hold write, not kept as it stands.
#[test]
fn a_hold_write_rewrites_a_skewed_deadline_to_the_cap() {
    let _home = crate::testutil::HomeSandbox::new();
    let holds = WalletHolds::persistent();
    holds.hold(MANAGEMENT, 0, Some(Duration::from_secs(600)));
    let path = hold_files().pop().unwrap();
    std::fs::write(&path, r#"{"version":1,"until_ms":18446744073709551615}"#).unwrap();
    WalletHolds::persistent().hold(MANAGEMENT, 0, None);
    let cap_ms = u64::try_from(WalletHolds::cap().as_millis()).unwrap();
    assert_eq!(persisted_raw(MANAGEMENT), Some(cap_ms));
}

/// Threads standing in for processes (each its own store, its own open of
/// the flock) hammer `hold` and `remaining` on one credential, some reading
/// at a time when the shorter holds have already expired: the file ends at
/// the longest deadline taken, and a fresh store honours it.
#[test]
fn concurrent_holds_on_one_credential_end_at_the_longest_deadline() {
    let _home = crate::testutil::HomeSandbox::new();
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 40;
    let reader_now = 400_000;
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            scope.spawn(move || {
                let store = WalletHolds::persistent();
                for r in 0..ROUNDS {
                    // 300 s .. 619 s; the longest (900 s) is the one extra thread.
                    let secs = 300 + (t * ROUNDS + r) % 600;
                    store.hold(MANAGEMENT, 0, Some(Duration::from_secs(secs)));
                    let _ = WalletHolds::persistent().remaining(MANAGEMENT, reader_now);
                }
            });
        }
        scope.spawn(|| {
            WalletHolds::persistent().hold(MANAGEMENT, 0, Some(Duration::from_secs(900)));
        });
    });
    assert_eq!(persisted_raw(MANAGEMENT), Some(900_000));
    assert_eq!(
        WalletHolds::persistent().remaining(MANAGEMENT, reader_now),
        Some(Duration::from_secs(500))
    );
    assert_eq!(hold_files().len(), 1, "{:?}", hold_files());
}
