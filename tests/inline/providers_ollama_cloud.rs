//! Inline tests for the Ollama Cloud provider: base-URL dispatch (ollama.com
//! and the local daemon), both `/api/usage` body shapes, the tolerant parse,
//! failure classification, the one shared fetch path over a canned transport,
//! and the observation refinement end to end through `usage::collect`.
//!
//! Every body here is hand-built in the shape ai-usagebar live-captured
//! (2026-09-09 legacy, 2026-09-16 monthly); no number is an account's. No test
//! can reach the network: the fetch path only ever sees [`Canned`], which
//! records what it was asked and never opens a socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::time::Duration;

use super::*;
use crate::providers::{Provider, ThirdPartyTarget, fetch_third_party_usage};
use crate::usage::observation::{AccountObservation, Origin, account_id};

/// Legacy shape: session + weekly with per-model rows and a 4-week spend.
const LEGACY: &str = r#"{
  "activity": {
    "cost": "0.00000",
    "period": {
      "type": "last_4_weeks",
      "starting_at": "2026-08-17T00:00:00Z",
      "ending_at": "2026-09-09T18:28:57.120401373Z"
    },
    "models": []
  },
  "limits": {
    "session": {
      "usage": 0.819,
      "models": [
        {"name": "kimi-k3", "request_count": 180},
        {"name": "gpt-oss:120b", "request_count": 2}
      ]
    },
    "weekly": {
      "usage": 0.23,
      "models": [
        {"name": "kimi-k3", "request_count": 180},
        {"name": "minimax-m3", "request_count": 554}
      ]
    }
  }
}"#;

/// New-pricing shape: one monthly pool.
const MONTHLY: &str = r#"{
  "activity": {
    "cost": "4.12345",
    "period": {
      "type": "last_4_weeks",
      "starting_at": "2026-08-24T00:00:00Z",
      "ending_at": "2026-09-16T08:55:34.663902649Z"
    }
  },
  "limits": {
    "monthly": {
      "usage": 0.003,
      "models": [
        {"name": "gpt-oss:120b", "request_count": 100},
        {"name": "gpt-oss:20b", "request_count": 2}
      ]
    }
  }
}"#;

fn bar<'a>(s: &'a ThirdPartyStats, label: &str) -> Option<&'a UsageBar> {
    s.bars.iter().find(|b| b.label == label)
}

fn row<'a>(s: &'a ThirdPartyStats, label: &str) -> Option<&'a StatRow> {
    s.rows.iter().find(|r| r.label == label)
}

fn reply(status: u16, body: &str) -> HttpReply {
    HttpReply {
        status,
        retry_after: None,
        body: body.to_string(),
    }
}

/// A transport that answers one canned reply and records the request.
struct Canned {
    reply: Result<HttpReply, ()>,
    seen: RefCell<Vec<(String, String)>>,
}

impl Canned {
    fn new(reply: HttpReply) -> Self {
        Self {
            reply: Ok(reply),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl UsageHttp for Canned {
    fn get(&self, url: &str, bearer: &str) -> Result<HttpReply, ThirdPartyError> {
        self.seen
            .borrow_mut()
            .push((url.to_string(), bearer.to_string()));
        self.reply.clone().map_err(|()| ThirdPartyError::Network)
    }
}

/// A transport that must never be called.
struct NoCall;

impl UsageHttp for NoCall {
    fn get(&self, url: &str, _bearer: &str) -> Result<HttpReply, ThirdPartyError> {
        panic!("no request may be sent, got GET {url}");
    }
}

// ── dispatch ───────────────────────────────────────────────────────────────────

#[test]
fn from_base_url_dispatches_ollama_cloud() {
    for url in [
        "https://ollama.com",
        "https://ollama.com/",
        "https://ollama.com/v1",
        "https://OLLAMA.COM",
        "https://ollama.com:443",
    ] {
        assert_eq!(
            Provider::from_base_url(url),
            Some(Provider::OllamaCloud),
            "{url}"
        );
    }
    for url in [
        "https://ollama.com.evil.tld",
        "https://ollama.com:443@evil.tld",
        "https://notollama.com",
        "http://ollama.com",
    ] {
        assert_ne!(
            Provider::from_base_url(url),
            Some(Provider::OllamaCloud),
            "{url} must not receive an Ollama key"
        );
    }
}

/// The daemon is typed, not handed to the generic scanner — the scanner would
/// probe `/api/usage`-like paths on a listener that signs with its own key.
#[test]
fn the_local_daemon_is_detected_as_the_ollama_daemon() {
    for url in [
        "http://127.0.0.1:11434",
        "http://localhost:11434",
        "http://localhost:11434/",
        "http://LOCALHOST:11434/v1",
    ] {
        assert_eq!(
            Provider::from_base_url(url),
            Some(Provider::OllamaDaemon),
            "{url}"
        );
    }
    for url in [
        "http://127.0.0.1:11435",
        "http://localhost:114340",
        "http://127.0.0.1",
        "https://localhost:11434",
    ] {
        assert_eq!(Provider::from_base_url(url), None, "{url} stays generic");
    }
}

#[test]
fn provider_metadata_for_both_transports() {
    let cloud = Provider::OllamaCloud;
    assert_eq!(cloud.display_name(), "Ollama Cloud");
    assert!(cloud.publishes_windows());
    assert_eq!(cloud.store_source(), None);
    assert_eq!(
        cloud.console_url("https://ollama.com"),
        Some("https://ollama.com/settings/keys")
    );
    assert_eq!(
        cloud.console_url("https://api.deepseek.com"),
        None,
        "a mismatched endpoint opens no page"
    );
    let target = ThirdPartyTarget::Known {
        provider: cloud,
        console: None,
    };
    assert_eq!(target.throttle_key(), "https://ollama.com");

    let daemon = Provider::OllamaDaemon;
    assert_eq!(daemon.display_name(), "Ollama daemon");
    assert!(!daemon.publishes_windows());
    assert_eq!(
        daemon.console_url("http://localhost:11434"),
        Some("https://ollama.com/settings/keys")
    );
    assert_eq!(daemon.console_url("https://ollama.com"), None);
}

// ── legacy shape ───────────────────────────────────────────────────────────────

#[test]
fn the_legacy_shape_maps_session_and_weekly_to_the_chain_windows() {
    let s = parse_usage(LEGACY).unwrap();
    assert!(s.is_available);
    assert!(!s.best_effort);
    assert_eq!(s.plan, None, "the plan label comes from config");

    let session = bar(&s, "5h").unwrap();
    assert!((session.pct - 81.9).abs() < 1e-9);
    assert_eq!(
        session.resets_at, None,
        "the API publishes no reset instant"
    );
    let weekly = bar(&s, "7d").unwrap();
    assert!((weekly.pct - 23.0).abs() < 1e-9);
    assert!(bar(&s, "month").is_none());

    let info = s.to_usage_info().expect("5h / 7d reach the chain");
    assert!((info.five_hour.unwrap().utilization - 81.9).abs() < 1e-9);
    assert!((info.seven_day.unwrap().utilization - 23.0).abs() < 1e-9);

    // Per-model request blocks, in window order.
    let labels: Vec<(&str, &str)> = s
        .rows
        .iter()
        .map(|r| (r.label.as_str(), r.value.as_str()))
        .collect();
    let at = |l: &str| labels.iter().position(|(x, _)| *x == l).unwrap();
    assert!(at("5h requests") < at("7d requests"));
    assert_eq!(labels[at("5h requests") + 1], ("kimi-k3", "180 requests"));
    assert_eq!(
        labels[at("7d requests") + 2],
        ("minimax-m3", "554 requests")
    );

    let spend = row(&s, "spend (last 4 weeks)").unwrap();
    assert_eq!(spend.value, "0.00000 USD", "the cost string is kept exact");
    assert_eq!(
        row(&s, "spend period").unwrap().value,
        "2026-08-17T00:00:00+00:00 to 2026-09-09T18:28:57+00:00"
    );
    assert_eq!(
        row(&s, "resets").unwrap().value,
        "no reset time from API",
        "the missing reset instant is said, not hidden"
    );
    // No money row reads as a wallet: the route publishes no balance.
    assert!(crate::providers::balance_wallets(&s.rows).is_empty());
}

// ── monthly shape ──────────────────────────────────────────────────────────────

#[test]
fn the_monthly_shape_maps_to_a_month_bar_the_chain_never_reads() {
    let s = parse_usage(MONTHLY).unwrap();
    assert!(bar(&s, "5h").is_none() && bar(&s, "7d").is_none());
    let month = bar(&s, "month").unwrap();
    assert!((month.pct - 0.3).abs() < 1e-9);
    assert!(
        s.to_usage_info().is_none(),
        "a calendar pool is not a rolling chain window"
    );
    assert_eq!(
        row(&s, "spend (last 4 weeks)").unwrap().value,
        "4.12345 USD",
        "five decimals survive"
    );
    assert!(row(&s, INCLUDED_USED_UP).is_none());
}

#[test]
fn usage_past_one_is_unclamped_and_a_spent_pool_says_included_credits_used_up() {
    let body = r#"{"limits":{"monthly":{"usage":1.2}}}"#;
    let s = parse_usage(body).unwrap();
    assert!((bar(&s, "month").unwrap().pct - 120.0).abs() < 1e-9);
    let note = row(&s, INCLUDED_USED_UP).unwrap();
    assert!(note.value.contains("balance unknown"), "{}", note.value);
    assert_eq!(note.kind, StatRowKind::Body, "HIGH, not a danger verdict");
    assert!(
        s.is_available,
        "past the pool is not proof the account stops"
    );

    let s = parse_usage(r#"{"limits":{"session":{"usage":1.5}}}"#).unwrap();
    assert!((bar(&s, "5h").unwrap().pct - 150.0).abs() < 1e-9);
    assert!(
        (s.to_usage_info().unwrap().five_hour.unwrap().utilization - 100.0).abs() < 1e-9,
        "only the chain's copy is clamped"
    );
}

// ── tolerant parse ─────────────────────────────────────────────────────────────

#[test]
fn a_missing_usage_is_not_invented_as_zero() {
    let s = parse_usage(r#"{"limits":{"session":{"models":[]},"weekly":{"usage":0.1}}}"#).unwrap();
    assert!(bar(&s, "5h").is_none(), "no bar for an unknown figure");
    assert_eq!(row(&s, "5h").unwrap().value, NOT_REPORTED);
    assert!(bar(&s, "7d").is_some());
    let s = parse_usage(r#"{"limits":{"session":{"usage":"high"}}}"#).unwrap();
    assert_eq!(row(&s, "5h").unwrap().value, NOT_REPORTED);
}

#[test]
fn one_bad_model_row_is_skipped_not_fatal() {
    let body = r#"{"limits":{"weekly":{"usage":0.5,"models":[
        {"request_count": 7},
        {"name": "", "request_count": 1},
        {"name": "neg", "request_count": -3},
        {"name": "str", "request_count": "9"},
        {"name": "nocount"},
        "not-an-object",
        {"name": "good", "request_count": 4}
    ]}}}"#;
    let s = parse_usage(body).unwrap();
    let models: Vec<&str> = s
        .rows
        .iter()
        .filter(|r| r.value.ends_with(" requests"))
        .map(|r| r.label.as_str())
        .collect();
    assert_eq!(models, ["good"]);
    assert!(bar(&s, "7d").is_some(), "the window survives its bad rows");
}

#[test]
fn empty_limits_and_missing_activity_parse_to_no_figures() {
    let s = parse_usage(r#"{"limits":{}}"#).unwrap();
    assert!(s.bars.is_empty() && s.rows.is_empty());

    let s = parse_usage(r#"{"limits":{"session":{"usage":0.2}}}"#).unwrap();
    assert!(row(&s, "spend").is_none() && row(&s, "spend period").is_none());

    // A cost that is not a plain decimal is dropped, never guessed.
    let s = parse_usage(r#"{"activity":{"cost":"$1.00"}}"#).unwrap();
    assert!(s.rows.is_empty());
    // A cost with no period still renders, under the bare label.
    let s = parse_usage(r#"{"activity":{"cost":"1.50"}}"#).unwrap();
    assert_eq!(row(&s, "spend").unwrap().value, "1.50 USD");
    assert!(row(&s, "spend period").is_none());
}

#[test]
fn an_unknown_shape_is_a_parse_failure() {
    for body in [
        "",
        "<html>sign in</html>",
        "[]",
        "{}",
        r#"{"error":"something"}"#,
        r#"{"limits":5}"#,
        r#"{"limits":[]}"#,
    ] {
        assert!(
            matches!(parse_usage(body), Err(ThirdPartyError::Parse)),
            "{body:?}"
        );
    }
    // A stray scalar under one window drops that window only.
    let s = parse_usage(r#"{"limits":{"session":7,"weekly":{"usage":0.4}}}"#).unwrap();
    assert!(bar(&s, "5h").is_none() && bar(&s, "7d").is_some());
}

// ── failures ───────────────────────────────────────────────────────────────────

#[test]
fn statuses_classify_into_typed_failures() {
    assert!(matches!(
        classify(&reply(401, r#"{"error":"invalid credentials"}"#)),
        Err(ThirdPartyError::AuthExpired)
    ));

    let mut quota = reply(
        429,
        r#"{"error":"you've reached your session usage limit, please wait or upgrade to continue"}"#,
    );
    quota.retry_after = Some(Duration::from_secs(90));
    match classify(&quota) {
        Err(ThirdPartyError::QuotaExhausted { retry_after }) => {
            assert_eq!(retry_after, Some(Duration::from_secs(90)));
        }
        other => panic!("expected QuotaExhausted, got {other:?}"),
    }
    assert!(matches!(
        classify(&reply(429, r#"{"error":"weekly usage limit reached"}"#)),
        Err(ThirdPartyError::QuotaExhausted { .. })
    ));

    let mut throttle = reply(429, r#"{"error":"too many requests"}"#);
    throttle.retry_after = Some(Duration::from_secs(5));
    match classify(&throttle) {
        Err(ThirdPartyError::RateLimited { retry_after }) => {
            assert_eq!(retry_after, Some(Duration::from_secs(5)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert!(matches!(
        classify(&reply(429, "")),
        Err(ThirdPartyError::RateLimited { .. })
    ));

    for status in [403, 404, 500, 502, 302] {
        assert!(
            matches!(classify(&reply(status, "{}")), Err(ThirdPartyError::Status)),
            "{status}"
        );
    }
    assert!(matches!(
        classify(&reply(200, r#"{"unexpected":true}"#)),
        Err(ThirdPartyError::Parse)
    ));
    assert!(classify(&reply(200, LEGACY)).is_ok());
}

// ── the shared fetch path ──────────────────────────────────────────────────────

#[test]
fn the_fetch_sends_one_bearer_get_to_the_usage_route() {
    let http = Canned::new(reply(200, MONTHLY));
    let s = fetch_ollama_cloud_usage("  test-key-not-real  ", &http).unwrap();
    assert!(bar(&s, "month").is_some());
    assert_eq!(
        http.seen.borrow().as_slice(),
        [(
            "https://ollama.com/api/usage".to_string(),
            "test-key-not-real".to_string()
        )],
        "one request, to the fixed origin, with the trimmed key"
    );
}

#[test]
fn a_blank_key_never_reaches_the_wire() {
    for key in ["", "   "] {
        assert!(matches!(
            fetch_ollama_cloud_usage(key, &NoCall),
            Err(ThirdPartyError::AuthExpired)
        ));
    }
}

#[test]
fn a_transport_failure_is_network_and_errors_pass_through() {
    let http = Canned {
        reply: Err(()),
        seen: RefCell::new(Vec::new()),
    };
    assert!(matches!(
        fetch_ollama_cloud_usage("k", &http),
        Err(ThirdPartyError::Network)
    ));
    let http = Canned::new(reply(401, r#"{"error":"invalid credentials"}"#));
    assert!(matches!(
        fetch_ollama_cloud_usage("k", &http),
        Err(ThirdPartyError::AuthExpired)
    ));
}

/// The daemon arm answers without a request (the target's own fetch runs with
/// no api key at all, since the daemon signs with its own).
#[test]
fn the_daemon_arm_says_usage_needs_a_key_without_a_request() {
    let target = ThirdPartyTarget::Known {
        provider: Provider::OllamaDaemon,
        console: None,
    };
    let s = fetch_third_party_usage(&target, "", None).unwrap();
    assert!(s.bars.is_empty());
    assert_eq!(s.rows.len(), 1);
    assert_eq!(s.rows[0].value, DAEMON_NEEDS_KEY);
    assert!(s.to_usage_info().is_none());
}

#[test]
fn a_keyless_daemon_profile_is_still_scheduled_for_its_note() {
    let p = crate::profile::Profile::new(
        "local".to_string(),
        Some("http://localhost:11434".to_string()),
        None,
    );
    assert_eq!(p.provider, Some(Provider::OllamaDaemon));
    assert!(crate::usage::third_party_credentialed(&p));
    let keyless_cloud = crate::profile::Profile::new(
        "cloud".to_string(),
        Some("https://ollama.com".to_string()),
        None,
    );
    assert!(
        !crate::usage::third_party_credentialed(&keyless_cloud),
        "ollama.com needs its key"
    );
}

// ── observation ────────────────────────────────────────────────────────────────

fn cloud_obs(stats: &ThirdPartyStats) -> AccountObservation {
    let mut obs = AccountObservation::new(
        account_id(Origin::Profile, "oll"),
        SourceId::OllamaCloud,
        AuthKind::ApiKey,
        Origin::Profile,
        "oll",
    );
    crate::usage::project::apply_third_party(&mut obs, stats, 1_790_000_000);
    refine_observation(&mut obs, Some(stats));
    obs
}

#[test]
fn legacy_windows_project_with_breakdown_and_exact_spend() {
    let obs = cloud_obs(&parse_usage(LEGACY).unwrap());
    let ids: Vec<&str> = obs.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, [WINDOW_SESSION, WINDOW_WEEKLY]);

    let session = obs.window(WINDOW_SESSION).unwrap();
    assert_eq!(session.label, "5h");
    assert_eq!(session.window_secs, Some(18_000));
    assert!(session.chain_eligible);
    assert_eq!(session.resets_at, None);
    assert!(!session.exhausted);
    assert_eq!(
        session.breakdown,
        [
            ModelCount {
                model: "kimi-k3".to_string(),
                requests: 180
            },
            ModelCount {
                model: "gpt-oss:120b".to_string(),
                requests: 2
            },
        ]
    );
    let weekly = obs.window(WINDOW_WEEKLY).unwrap();
    assert_eq!(weekly.window_secs, Some(604_800));
    assert!(weekly.chain_eligible);
    assert_eq!(weekly.breakdown.len(), 2);

    assert_eq!(obs.money.len(), 1, "one spend meter, never a balance");
    let m = obs.meter("spend.period").unwrap();
    assert_eq!(m.kind, MoneyKind::Spend);
    assert_eq!(m.amount.as_str(), "0.00000");
    assert_eq!(m.currency, "USD");
    assert_eq!(m.label, "Spend (last 4 weeks)");
    let period = m.period.unwrap();
    assert_eq!(period.kind, PeriodKind::Custom);
    assert_eq!(period.start, Timestamp::parse("2026-08-17T00:00:00Z"));
    assert_eq!(period.end, Timestamp::parse("2026-09-09T18:28:57Z"));
    assert!(!period.derived);
}

#[test]
fn a_spent_month_is_high_not_exhausted_and_never_chain_eligible() {
    let obs = cloud_obs(&parse_usage(r#"{"limits":{"monthly":{"usage":1.2}}}"#).unwrap());
    let month = obs.window(WINDOW_MONTH).unwrap();
    assert_eq!(month.label, "month");
    assert!((month.used_pct.unwrap() - 120.0).abs() < 1e-9, "unclamped");
    assert!(!month.exhausted, "extra use may draw on purchased credits");
    assert!(!month.chain_eligible);
    assert_eq!(month.window_secs, None);

    // Session / weekly at 100 % ARE exhausted.
    let obs = cloud_obs(&parse_usage(r#"{"limits":{"session":{"usage":1.0}}}"#).unwrap());
    assert!(obs.window(WINDOW_SESSION).unwrap().exhausted);
}

#[test]
fn a_window_without_usage_projects_with_no_percent() {
    let obs = cloud_obs(
        &parse_usage(r#"{"limits":{"session":{"models":[]},"weekly":{"usage":0.1}}}"#).unwrap(),
    );
    let session = obs.window(WINDOW_SESSION).unwrap();
    assert_eq!(session.used_pct, None);
    assert!(!session.exhausted);
    assert_eq!(session.window_secs, Some(18_000));
    assert_eq!(
        obs.windows
            .iter()
            .map(|w| w.id.as_str())
            .collect::<Vec<_>>(),
        [WINDOW_SESSION, WINDOW_WEEKLY],
        "session keeps its place ahead of weekly"
    );
}

#[test]
fn refinement_leaves_other_sources_alone() {
    let stats = parse_usage(LEGACY).unwrap();
    let mut obs = AccountObservation::new(
        account_id(Origin::Profile, "z"),
        SourceId::Zai,
        AuthKind::ApiKey,
        Origin::Profile,
        "z",
    );
    crate::usage::project::apply_third_party(&mut obs, &stats, 1_790_000_000);
    let before = obs.clone();
    refine_observation(&mut obs, Some(&stats));
    assert_eq!(obs, before);
}

// ── through collect, off a real cache ──────────────────────────────────────────

mod collect {
    use super::*;
    use crate::codex_profiles::CodexState;
    use crate::profile::{AppConfig, AppState, Profile, ProfileName};
    use crate::profile_cache::{THIRD_PARTY_CACHE_FILE, write_profile_cache};
    use crate::testutil::HomeSandbox;
    use crate::usage::collect::{CollectCtx, CollectOpts, collect_with};

    fn run(profiles: Vec<Profile>) -> Vec<AccountObservation> {
        let mut config = AppConfig {
            state: AppState::default(),
            profiles,
        };
        config.state.active_profile = Some(config.profiles[0].name.clone());
        let codex = CodexState::default();
        let ctx = CollectCtx {
            config: Some(&config),
            codex: &codex,
            now_ms: crate::usage::now_ms(),
            interval_ms: config.state.refresh_interval_ms,
            guest_mode: false,
            include_disabled: false,
        };
        collect_with(&ctx, &CollectOpts::default(), &[], &[])
    }

    #[test]
    fn an_ollama_cloud_profile_projects_its_cache() {
        let _home = HomeSandbox::new();
        crate::testutil::register_names(&["oll"]);
        write_profile_cache(
            &ProfileName::from("oll"),
            THIRD_PARTY_CACHE_FILE,
            &parse_usage(LEGACY).unwrap(),
        );
        let got = run(vec![Profile::new(
            "oll".to_string(),
            Some("https://ollama.com".to_string()),
            Some("test-key-not-real".to_string()),
        )]);
        let o = &got[0];
        assert_eq!(o.id, "claude:oll");
        assert_eq!(o.source, SourceId::OllamaCloud);
        assert_eq!(o.provider, "Ollama Cloud");
        assert_eq!(o.auth, AuthKind::ApiKey);
        assert_eq!(o.freshness, Freshness::Fresh);
        assert_eq!(
            o.window(WINDOW_SESSION).unwrap().breakdown[0].model,
            "kimi-k3"
        );
        assert!(o.meter("spend.period").is_some());
        assert!(o.failure.is_none());
    }

    #[test]
    fn a_daemon_profile_needs_a_key_and_is_never_fetched() {
        let _home = HomeSandbox::new();
        crate::testutil::register_names(&["local"]);
        // Even a cache on disk (the scheduler's note) yields no figures.
        write_profile_cache(
            &ProfileName::from("local"),
            THIRD_PARTY_CACHE_FILE,
            &daemon_stats(),
        );
        let got = run(vec![Profile::new(
            "local".to_string(),
            Some("http://127.0.0.1:11434".to_string()),
            None,
        )]);
        let o = &got[0];
        assert_eq!(o.source, SourceId::Ollama);
        assert_eq!(o.auth, AuthKind::NativeLogin);
        assert_eq!(o.freshness, Freshness::NotFetched);
        assert!(o.windows.is_empty() && o.money.is_empty());
        let f = o.failure.as_ref().unwrap();
        assert_eq!(f.kind, FailureKind::AuthRequired);
        assert!(f.message.contains("ollama.com API key"), "{}", f.message);
        assert!(
            f.message.contains("ollama.com/settings/keys"),
            "{}",
            f.message
        );
    }
}
