#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The local agent API's router, redaction and catalog, at the seam: a
//! constructed `Request` into `handle`, no socket. The socket-level behavior
//! lives in `local_api.rs`.

use super::*;

use crate::daemon::api::http::WsHeaders;
use crate::testutil::HomeSandbox;
use crate::usage::observation::{
    Failure, FailureKind, MoneyKind, MoneyMeter, MoneyScope, Origin, QuotaWindow, WindowScope,
    account_id,
};

fn req(method: &str, path: &str, query: &str, bearer: Option<&str>) -> Request {
    Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        bearer: bearer.map(str::to_string),
        host: Some("127.0.0.1:8454".to_string()),
        if_none_match: None,
        body: Vec::new(),
        keep_alive: false,
        ws: WsHeaders::default(),
    }
}

fn ctx() -> Ctx {
    let dir = crate::profile::tollgate_dir().unwrap();
    Ctx {
        token_path: dir.join(super::super::TOKEN_FILE),
        status_path: dir.join("status.json"),
    }
}

fn body(resp: &Response) -> serde_json::Value {
    serde_json::from_slice(&resp.body).unwrap()
}

#[test]
fn the_route_table_is_exact() {
    assert_eq!(route_of("/v1/health"), Some(Route::Health));
    assert_eq!(route_of("/v1/accounts"), Some(Route::Accounts));
    assert_eq!(route_of("/v1/usage"), Some(Route::Usage));
    assert_eq!(route_of("/v1/providers"), Some(Route::Providers));
    assert_eq!(route_of("/v1/status"), Some(Route::Status));
    assert_eq!(route_of("/v1/openapi.json"), Some(Route::OpenApi));
    assert_eq!(
        route_of("/v1/accounts/claude:work"),
        Some(Route::Account("claude:work".into()))
    );
    assert_eq!(
        route_of("/v1/accounts/codex%3Amain"),
        Some(Route::Account("codex:main".into()))
    );
    for miss in [
        "/",
        "/v1",
        "/v1/",
        "/v1/health/",
        "/v1/accounts/",
        "/v1/accounts/a/b",
        "/v1/accounts/%zz",
        "/api/v1/health",
        "/v1/HEALTH",
        "/v2/health",
    ] {
        assert_eq!(route_of(miss), None, "{miss}");
    }
}

#[test]
fn tcp_is_authenticated_before_routing_and_the_socket_is_trusted() {
    let _home = HomeSandbox::new();
    let token = super::super::ensure_token().unwrap();
    let ctx = ctx();

    let denied = handle(&ctx, &req("GET", "/v1/health", "", None), Door::Tcp);
    assert_eq!(denied.status, 401);
    assert!(denied.challenge);
    assert_eq!(
        handle(&ctx, &req("DELETE", "/v1/nope", "", None), Door::Tcp).status,
        401
    );

    let ok = handle(&ctx, &req("GET", "/v1/health", "", Some(&token)), Door::Tcp);
    assert_eq!(ok.status, 200);
    let unix = handle(&ctx, &req("GET", "/v1/health", "", None), Door::Unix);
    assert_eq!(unix.status, 200);
    // Any presented bearer is ignored on the socket rather than rejected.
    let unix = handle(
        &ctx,
        &req("GET", "/v1/health", "", Some("junk")),
        Door::Unix,
    );
    assert_eq!(unix.status, 200);
}

#[test]
fn every_route_is_get_only() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    for path in [
        "/v1/health",
        "/v1/accounts",
        "/v1/accounts/claude:x",
        "/v1/usage",
        "/v1/providers",
        "/v1/status",
        "/v1/openapi.json",
    ] {
        for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS", "get"] {
            let resp = handle(&ctx, &req(method, path, "", None), Door::Unix);
            assert_eq!(resp.status, 405, "{method} {path}");
            assert_eq!(body(&resp)["error"], "method_not_allowed");
        }
    }
}

#[test]
fn query_filters_reach_the_collector() {
    let _home = HomeSandbox::new();
    crate::profile::save_profile(&crate::profile::Profile::new("a".into(), None, None)).unwrap();
    crate::profile::save_profile(&crate::profile::Profile::new("b".into(), None, None)).unwrap();
    crate::testutil::register_names(&["a", "b"]);
    let ctx = ctx();
    let ids = |query: &str| -> Vec<String> {
        body(&handle(
            &ctx,
            &req("GET", "/v1/accounts", query, None),
            Door::Unix,
        ))["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(""), ["claude:a", "claude:b"]);
    assert_eq!(ids("account=b"), ["claude:b"]);
    assert_eq!(ids("account=claude%3Ab"), ["claude:b"]);
    assert_eq!(ids("provider=anthropic_oauth"), ["claude:a", "claude:b"]);
    assert!(ids("provider=openrouter").is_empty());
    // An undecodable value matches nothing rather than everything.
    assert!(ids("account=%zz").is_empty());
    // An empty value is no filter.
    assert_eq!(ids("account="), ["claude:a", "claude:b"]);
}

#[test]
fn endpoint_redaction_keeps_the_host_and_path_only() {
    for (raw, want) in [
        (
            "https://api.deepseek.com/anthropic",
            "https://api.deepseek.com/anthropic",
        ),
        (
            "https://user:pass@api.example.com/v1",
            "https://api.example.com/v1",
        ),
        (
            "https://api.example.com/v1?key=sk-abc&x=1#top",
            "https://api.example.com/v1",
        ),
        (
            "https://gw.example.com/keys/sk-or-v1-0123456789abcdef/v1",
            "https://gw.example.com/keys/[redacted]/v1",
        ),
        (
            "https://gw.example.com/a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7/v1",
            "https://gw.example.com/[redacted]/v1",
        ),
        ("http://127.0.0.1:11434", "http://127.0.0.1:11434"),
        ("http://127.0.0.1:11434/", "http://127.0.0.1:11434/"),
        ("api.example.com/v1?k=v", "api.example.com/v1"),
        ("", ""),
    ] {
        assert_eq!(redact_endpoint(raw), want, "{raw}");
    }
}

fn leaky_observation() -> AccountObservation {
    let mut obs = AccountObservation::new(
        account_id(Origin::Profile, "vendor"),
        SourceId::OpenRouter,
        AuthKind::ApiKey,
        Origin::Profile,
        "vendor",
    );
    obs.plan = Some("plan sk-or-v1-0123456789abcdefghij".into());
    obs.endpoint = Some("https://k:sk-secret-0123456789ab@openrouter.ai/api?x=y".into());
    // Built without `Failure::new`, the way a careless producer would.
    obs.failure = Some(Failure {
        kind: FailureKind::AuthRequired,
        message: "401 Bearer eyJhbGciOiJIUzI1NiJ9abcdefgh rejected".into(),
        retry_after: None,
    });
    let mut window = QuotaWindow::new("session", "5h", WindowScope::Shared);
    window.label = "5h sk-live-0123456789abcdef".into();
    obs.windows.push(window);
    let mut meter = MoneyMeter::new(
        "key_limit",
        "key sk-or-v1-abcdefghijklmnop",
        MoneyKind::Limit,
        crate::usage::observation::Amount::parse("1.50").unwrap(),
        "USD",
        MoneyScope::Key,
    );
    meter.scope_id = Some("sk-or-v1-abcdefghijklmnop0123".into());
    obs.money.push(meter);
    obs
}

#[test]
fn redaction_masks_every_free_text_field_and_keeps_the_handles() {
    let mut obs = leaky_observation();
    redact(&mut obs);
    let json = serde_json::to_string(&obs).unwrap();
    for secret in ["sk-or", "sk-secret", "sk-live", "eyJ", "k:", "x=y"] {
        assert!(!json.contains(secret), "{secret} leaked: {json}");
    }
    assert_eq!(obs.id, "claude:vendor");
    assert_eq!(obs.label, "vendor");
    assert_eq!(obs.endpoint.as_deref(), Some("https://openrouter.ai/api"));
    assert_eq!(obs.plan.as_deref(), Some("plan [redacted]"));
    assert_eq!(
        obs.failure.unwrap().message,
        "401 Bearer [redacted] rejected"
    );
    assert_eq!(obs.windows[0].label, "5h [redacted]");
    assert_eq!(obs.money[0].label, "key [redacted]");
    assert_eq!(obs.money[0].scope_id.as_deref(), Some("[redacted]"));
    // Numbers are untouched.
    assert_eq!(obs.money[0].amount.as_str(), "1.50");
}

#[test]
fn the_catalog_lists_every_source_once_with_its_auth_kinds() {
    let mut seen = std::collections::HashSet::new();
    for source in CATALOG {
        assert!(seen.insert(source.as_str()), "{} twice", source.as_str());
        assert!(!auth_kinds(*source).is_empty());
        // The catalog's spelling is the observation's serde spelling.
        assert_eq!(
            serde_json::to_value(source).unwrap(),
            serde_json::json!(source.as_str())
        );
    }
    assert_eq!(CATALOG.len(), 17);

    let mut obs = leaky_observation();
    obs.source = SourceId::DeepSeek;
    let rows = provider_catalog(&[obs.clone(), obs]);
    let ds = rows.iter().find(|r| r.source == "deepseek").unwrap();
    assert!(ds.configured);
    assert_eq!(ds.accounts, 2);
    assert_eq!(ds.display_name, "DeepSeek");
    assert_eq!(ds.auth_kinds, ["api_key", "hybrid", "read_only"]);
    assert!(
        rows.iter()
            .filter(|r| r.source != "deepseek")
            .all(|r| !r.configured)
    );
}

#[test]
fn a_missing_account_is_a_404_and_an_exact_id_wins() {
    let _home = HomeSandbox::new();
    // A profile literally named `claude:b`'s look-alike: `b` resolves by name.
    crate::profile::save_profile(&crate::profile::Profile::new("b".into(), None, None)).unwrap();
    crate::testutil::register_names(&["b"]);
    let ctx = ctx();
    let hit = handle(
        &ctx,
        &req("GET", "/v1/accounts/claude:b", "", None),
        Door::Unix,
    );
    assert_eq!(hit.status, 200);
    assert_eq!(body(&hit)["account"]["id"], "claude:b");
    assert_eq!(body(&hit)["schema_version"], 1);
    let miss = handle(
        &ctx,
        &req("GET", "/v1/accounts/codex:b", "", None),
        Door::Unix,
    );
    assert_eq!(miss.status, 404);
    assert_eq!(body(&miss)["error"], "account_not_found");
}

#[test]
fn a_torn_status_feed_is_rebuilt_not_served() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    std::fs::create_dir_all(ctx.status_path.parent().unwrap()).unwrap();
    std::fs::write(&ctx.status_path, b"{\"profiles\": [").unwrap();
    let resp = handle(&ctx, &req("GET", "/v1/status", "", None), Door::Unix);
    assert_eq!(resp.status, 200);
    assert!(body(&resp).get("profiles").is_some());
}

#[test]
fn the_openapi_document_matches_the_route_table() {
    let doc: serde_json::Value =
        serde_json::from_slice(&openapi_document_bytes().unwrap()).unwrap();
    assert_eq!(doc["info"]["title"], "tollgate local agent API");
    let paths = doc["paths"].as_object().unwrap();
    assert_eq!(paths.len(), 7, "{:?}", paths.keys().collect::<Vec<_>>());
    for (path, ops) in paths {
        let concrete = path.replace("{id}", "claude:x");
        assert!(
            route_of(&concrete).is_some(),
            "{path} documented but not routed"
        );
        let ops = ops.as_object().unwrap();
        assert_eq!(
            ops.keys().collect::<Vec<_>>(),
            ["get"],
            "{path} is GET-only"
        );
        assert!(
            ops["get"]["responses"].get("401").is_some(),
            "{path} names its 401"
        );
    }
    assert_eq!(
        doc["components"]["securitySchemes"]["bearer"]["scheme"],
        "bearer"
    );
}

/// A profile whose endpoint carries a credential (userinfo, a `?key=`) as the
/// collector reads it, saved the way `tollgate login --base-url` saves it.
fn seed_leaky_profile() {
    let p = crate::profile::Profile::new(
        "ep".to_string(),
        Some(
            "https://user:pw-deadbeef@api.deepseek.com/anthropic?key=sk-deadbeefcafe0123456789"
                .to_string(),
        ),
        Some("sk-placeholder-not-a-key".to_string()),
    );
    crate::profile::save_profile(&p).unwrap();
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec![crate::profile::ProfileName::from("ep")],
        ..crate::profile::AppState::default()
    })
    .unwrap();
}

/// `tollgate usage --json` and `GET /v1/usage` share this envelope, so an
/// endpoint's userinfo and query reach neither.
#[test]
fn the_usage_envelope_redacts_a_profiles_endpoint() {
    let _home = HomeSandbox::new();
    seed_leaky_profile();
    let report = usage_report(&CollectOpts::default());
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains("api.deepseek.com/anthropic"), "{json}");
    assert!(!json.contains("deadbeef"), "{json}");
}

/// `/v1/status` redacts `profiles[].base_url` too, whether it rebuilds the
/// feed or re-serialises the daemon's; a feed with nothing to redact
/// re-serialises to the same bytes (key order is preserved), tagged.
#[test]
fn the_status_route_redacts_profile_endpoints() {
    let _home = HomeSandbox::new();
    seed_leaky_profile();
    let ctx = ctx();
    let rebuilt = handle(&ctx, &req("GET", "/v1/status", "", None), Door::Unix);
    assert_eq!(rebuilt.status, 200);
    let text = String::from_utf8(rebuilt.body.clone()).unwrap();
    assert!(text.contains("api.deepseek.com/anthropic"), "{text}");
    assert!(!text.contains("deadbeef"), "rebuilt: {text}");

    std::fs::create_dir_all(ctx.status_path.parent().unwrap()).unwrap();
    std::fs::write(
        &ctx.status_path,
        text.replace(
            "api.deepseek.com/anthropic",
            "u:pw-deadbeef@api.deepseek.com/anthropic",
        ),
    )
    .unwrap();
    let served = handle(&ctx, &req("GET", "/v1/status", "", None), Door::Unix);
    let served_text = String::from_utf8(served.body.clone()).unwrap();
    assert!(
        !served_text.contains("deadbeef"),
        "passed through: {served_text}"
    );
    assert_eq!(
        served.etag.as_deref(),
        Some(crate::daemon::api::routes::etag_for(&served.body).as_str()),
        "a rewritten feed is tagged as what is served, not as the file"
    );

    std::fs::write(&ctx.status_path, text.as_bytes()).unwrap();
    let clean = handle(&ctx, &req("GET", "/v1/status", "", None), Door::Unix);
    assert_eq!(
        clean.body,
        text.as_bytes(),
        "a clean feed passes through untouched"
    );
    assert!(clean.etag.is_some());
}

/// Nothing on disk reaches an agent unredacted: a published feed whose only
/// credential-shaped values sit OUTSIDE `profiles[].base_url` (a credential
/// key, a `Bearer` in free text, a key in an error, a URL's query in an
/// unexpected field, a token-shaped path segment) is parsed, masked and
/// re-serialised, never passed through. Handles and paths survive.
#[test]
fn the_status_route_never_passes_the_feed_through_raw() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    std::fs::create_dir_all(ctx.status_path.parent().unwrap()).unwrap();
    let long_name = "a-very-long-profile-name-for-the-work-account";
    let feed = serde_json::json!({
        "schema": 1,
        "active_profile": long_name,
        "codex_fallback_chain": [long_name],
        "gateway": {
            "config": "/home/u/.tollgate/gateway/config.yaml",
            "reason": "exited: Authorization: Bearer abcdefabcdef leaked",
        },
        "profiles": [{
            "name": long_name,
            "base_url": "https://api.example.com/v1",
            "api_key": "short",
            "nested": { "refresh-token": "rt-plain", "extra_secret": "xyz" },
            "fetch_status": "failed: sk-ant-api03-abcdefghijklmnopqrstuv rejected",
            "note": "see https://api.example.com/x?key=querysecret",
            "store": "/home/u/.tollgate/profiles/0123456789abcdef0123456789abcdef0123/x.json",
            "fetched_at": "2026-09-29T12:00:00.000Z",
        }],
    });
    let raw = serde_json::to_vec(&feed).unwrap();
    std::fs::write(&ctx.status_path, &raw).unwrap();

    let resp = handle(&ctx, &req("GET", "/v1/status", "", None), Door::Unix);
    assert_eq!(resp.status, 200);
    assert_ne!(resp.body, raw, "the file's bytes are not what is served");
    let text = String::from_utf8(resp.body.clone()).unwrap();
    for secret in [
        "abcdefabcdef",
        "short",
        "rt-plain",
        "xyz",
        "sk-ant-api03",
        "querysecret",
        "0123456789abcdef0123456789abcdef0123",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let body = body(&resp);
    assert_eq!(body["active_profile"], long_name, "handles are kept");
    assert_eq!(body["codex_fallback_chain"][0], long_name);
    assert_eq!(body["profiles"][0]["name"], long_name);
    assert_eq!(
        body["gateway"]["config"], "/home/u/.tollgate/gateway/config.yaml",
        "an ordinary path is kept"
    );
    assert_eq!(
        body["profiles"][0]["base_url"],
        "https://api.example.com/v1"
    );
    assert_eq!(
        body["profiles"][0]["fetched_at"],
        "2026-09-29T12:00:00.000Z"
    );
    assert_eq!(body["profiles"][0]["api_key"], "[redacted]");
    assert_eq!(
        body["profiles"][0]["store"],
        "/home/u/.tollgate/profiles/[redacted]/x.json"
    );
    assert_eq!(
        resp.etag.as_deref(),
        Some(crate::daemon::api::routes::etag_for(&resp.body).as_str())
    );
}

/// The redactor has no false positives on what tollgate itself publishes: a
/// rebuilt status body (long profile names included) comes out unchanged.
#[test]
fn the_status_redactor_leaves_a_clean_body_alone() {
    let _home = HomeSandbox::new();
    let long_name = "a-very-long-profile-name-for-the-work-account";
    crate::profile::save_profile(&crate::profile::Profile::new(
        long_name.to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-placeholder-not-a-key".to_string()),
    ))
    .unwrap();
    crate::profile::save_profile(&crate::profile::Profile::new(
        "solo".to_string(),
        None,
        None,
    ))
    .unwrap();
    crate::profile::save_app_state(&crate::profile::AppState {
        active_profile: Some(long_name.into()),
        profiles: vec![long_name.into(), "solo".into()],
        ..crate::profile::AppState::default()
    })
    .unwrap();
    let config = crate::profile::load_config_read_only().unwrap();
    let built = crate::daemon::build_status(&config, config.state.refresh_interval_ms, None, false);
    let mut value = serde_json::to_value(&built).unwrap();
    let before = value.clone();
    assert!(!redact_status(&mut value), "{value:#}");
    assert_eq!(value, before);
}

#[test]
fn only_loopback_hosts_pass() {
    for ok in [
        "localhost",
        "localhost:8454",
        "LocalHost:1",
        "127.0.0.1",
        "127.0.0.1:8454",
        "127.1.2.3:80",
        "[::1]",
        "[::1]:8454",
        " localhost:8454 ",
    ] {
        assert!(loopback_host(ok), "{ok}");
    }
    for bad in [
        "",
        "evil.example",
        "evil.example:8454",
        "localhost.evil.example",
        "127.0.0.1.nip.io",
        "10.0.0.1:8454",
        "0.0.0.0:8454",
        "::1",
        "[::1",
        "[::1]x",
        "[::2]:8454",
        "[127.0.0.1]",
        "localhost:",
        "localhost:99999",
        "localhost:80:80",
        "localhost:+80",
        "user@localhost",
    ] {
        assert!(!loopback_host(bad), "{bad}");
    }
}

/// On TCP the Host is checked before the token: a rebound page with a stolen
/// token still gets 421, a caller with no Host a 400, and the socket door
/// ignores the Host entirely.
#[test]
fn tcp_checks_the_host_before_the_token() {
    let _home = HomeSandbox::new();
    let token = super::super::ensure_token().unwrap();
    let ctx = ctx();
    let with = |host: Option<&str>, bearer: Option<&str>| Request {
        host: host.map(str::to_string),
        ..req("GET", "/v1/health", "", bearer)
    };
    let evil = handle(
        &ctx,
        &with(Some("evil.example:8454"), Some(&token)),
        Door::Tcp,
    );
    assert_eq!(evil.status, 421);
    assert_eq!(body(&evil)["error"], "misdirected_request");
    let evil_no_token = handle(&ctx, &with(Some("evil.example"), None), Door::Tcp);
    assert_eq!(evil_no_token.status, 421);
    let missing = handle(&ctx, &with(None, Some(&token)), Door::Tcp);
    assert_eq!(missing.status, 400);
    assert_eq!(body(&missing)["error"], "host_required");
    let ok = handle(&ctx, &with(Some("localhost:8454"), Some(&token)), Door::Tcp);
    assert_eq!(ok.status, 200);
    let unix = handle(&ctx, &with(Some("evil.example"), None), Door::Unix);
    assert_eq!(unix.status, 200);
    let unix = handle(&ctx, &with(None, None), Door::Unix);
    assert_eq!(unix.status, 200);
}

// ── live sessions (hot-swap spec §2.4) ───────────────────────────────────────

/// Two running sessions, pids this process's own so the signal-0 probe reads
/// them live: an api-key session committed from `or-main` to `or-alt` whose
/// helper still serves `or-main`, and a plain OAuth session on `solo`. A third
/// row names a pid that cannot run (a crashed session awaiting GC).
fn seed_live_sessions() {
    for name in ["or-main", "or-alt", "solo"] {
        crate::profile::save_profile(&crate::profile::Profile::new(name.into(), None, None))
            .unwrap();
    }
    crate::testutil::register_names(&["or-main", "or-alt", "solo"]);
    let launch = crate::testutil::api_key_profile("or-main", "https://openrouter.ai/api", "k");
    let mut b = crate::testutil::live_row("4242-0", "or-main").with_executor(
        crate::hot_swap::Executor::ApiKey,
        crate::hot_swap::LaunchClass::of(&launch, true),
    );
    b.pid = std::process::id();
    b.started_at = 1;
    b.current_member = Some("or-alt".into());
    b.key_generation = Some(2);
    b.committed_at = Some(1_759_140_000_000);
    crate::live_sessions::register(&b).unwrap();
    crate::hot_swap::write_ack_for_test(
        "4242-0",
        &crate::hot_swap::HelperAck {
            version: 1,
            generation: 1,
            member: Some("or-main".into()),
            served_at_ms: Some(1_759_139_990_000),
            last_failure: None,
            launch_class: None,
        },
    );
    let mut a = crate::testutil::live_row("4242-1", "solo");
    a.pid = std::process::id();
    a.started_at = 2;
    crate::live_sessions::register(&a).unwrap();
    let mut dead = crate::testutil::live_row("4242-2", "solo");
    dead.pid = u32::MAX - 1;
    crate::live_sessions::register(&dead).unwrap();
}

// 61
#[test]
fn accounts_list_live_sessions_committed_and_served() {
    let _home = HomeSandbox::new();
    seed_live_sessions();
    let resp = handle(&ctx(), &req("GET", "/v1/accounts", "", None), Door::Unix);
    assert_eq!(resp.status, 200);
    let sessions = body(&resp)["live_sessions"].clone();
    assert_eq!(
        sessions,
        serde_json::json!([
            {
                "session_id": "4242-0",
                "harness": "claude",
                "start_profile": "or-main",
                "executor": "api_key",
                "relaunch_reason": null,
                "requested_member": null,
                "committed": {"member": "or-alt", "generation": 2, "at_ms": 1_759_140_000_000_u64},
                "served": {"member": "or-main", "generation": 1, "at_ms": 1_759_139_990_000_u64},
                "state": "swapping",
                "idle": true,
            },
            {
                "session_id": "4242-1",
                "harness": "claude",
                "start_profile": "solo",
                "executor": "oauth",
                "relaunch_reason": null,
                "requested_member": null,
                "committed": {"member": "solo", "generation": 0, "at_ms": null},
                "served": {"member": "solo", "generation": 0, "at_ms": null},
                "state": "served",
            },
        ]),
        "the dead row is left out; the swapping session has run no helper since its commit, so \
         it is idle, and only a swapping view carries the flag"
    );
    assert_eq!(body(&resp)["schema_version"], 1, "additive: no schema bump");
}

#[test]
fn accounts_list_reports_generation_zero_helper_failure_as_stalled() {
    let _home = HomeSandbox::new();
    seed_live_sessions();
    let mut row = crate::live_sessions::get("4242-0").expect("row");
    row.current_member = Some("or-main".into());
    row.key_generation = Some(0);
    row.committed_at = None;
    crate::live_sessions::register(&row).expect("row");
    crate::hot_swap::write_ack_for_test(
        "4242-0",
        &crate::hot_swap::HelperAck {
            version: 1,
            generation: 0,
            member: None,
            served_at_ms: None,
            last_failure: Some(crate::hot_swap::HelperFailure {
                generation: 0,
                code: "no_key".into(),
                at_ms: 1,
            }),
            launch_class: row.launch_class.clone(),
        },
    );
    let resp = handle(&ctx(), &req("GET", "/v1/accounts", "", None), Door::Unix);
    let sessions = &body(&resp)["live_sessions"];
    assert_eq!(sessions[0]["state"], "stalled");
    assert_eq!(sessions[0]["served"], serde_json::Value::Null);
    assert!(sessions[0].get("idle").is_none());
}

// 62
#[test]
fn account_by_id_filters_live_sessions() {
    let _home = HomeSandbox::new();
    seed_live_sessions();
    let ids = |account: &str| -> Vec<String> {
        let resp = handle(
            &ctx(),
            &req("GET", &format!("/v1/accounts/{account}"), "", None),
            Door::Unix,
        );
        assert_eq!(resp.status, 200, "{account}");
        body(&resp)["live_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["session_id"].as_str().unwrap().to_string())
            .collect()
    };
    // Committed on one account, served on the other: both list it.
    assert_eq!(ids("claude:or-alt"), ["4242-0"]);
    assert_eq!(ids("claude:or-main"), ["4242-0"]);
    assert_eq!(ids("claude:solo"), ["4242-1"]);
}

/// The document names the new field on both bodies, with its view schema.
#[test]
fn the_openapi_document_describes_live_sessions() {
    let doc: serde_json::Value =
        serde_json::from_slice(&openapi_document_bytes().unwrap()).unwrap();
    let schemas = &doc["components"]["schemas"];
    for body in ["AccountsBody", "AccountBody"] {
        assert_eq!(
            schemas[body]["properties"]["live_sessions"]["items"]["$ref"],
            "#/components/schemas/LiveSessionView",
            "{body}"
        );
    }
    let view = &schemas["LiveSessionView"]["properties"];
    for field in [
        "session_id",
        "harness",
        "start_profile",
        "executor",
        "relaunch_reason",
        "requested_member",
        "committed",
        "served",
        "state",
        "idle",
    ] {
        assert!(view.get(field).is_some(), "LiveSessionView.{field}");
    }
    assert_eq!(
        schemas["SwapState"]["enum"],
        serde_json::json!(["requested", "swapping", "stalled", "served"])
    );
}

/// Review lens guest-ux #8. `/v1/accounts/hermes:<n>` lists a live Hermes
/// session of that profile, as `/v1/accounts` does.
#[test]
fn account_by_id_lists_a_live_hermes_session() {
    let _home = HomeSandbox::new();
    crate::testutil::write_hermes_roster(&["hm-a"]);
    let mut row = crate::testutil::live_row("4242-5", "hm-a");
    row.harness = crate::harness::Harness::Hermes;
    row.follows_chain = false;
    row.pid = std::process::id();
    crate::live_sessions::register(&row).unwrap();
    let resp = handle(
        &ctx(),
        &req("GET", "/v1/accounts/hermes:hm-a", "", None),
        Door::Unix,
    );
    assert_eq!(resp.status, 200, "{:?}", body(&resp));
    let ids: Vec<String> = body(&resp)["live_sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["session_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["4242-5"]);
}

#[test]
fn providers_list_lane4_sources_and_native_auth() {
    assert_eq!(
        auth_kinds(SourceId::OpenaiApi),
        &[AuthKind::ApiKey, AuthKind::ReadOnly]
    );
    assert_eq!(auth_kinds(SourceId::GoogleAi), &[AuthKind::ApiKey]);
    for s in [SourceId::Codex, SourceId::Grok, SourceId::Antigravity] {
        assert_eq!(
            auth_kinds(s),
            &[AuthKind::Subscription, AuthKind::NativeLogin]
        );
    }
}

#[test]
fn openapi_links_accounts_to_optional_health_note_and_attribution() {
    let doc: serde_json::Value =
        serde_json::from_slice(&openapi_document_bytes().unwrap()).unwrap();
    let schemas = &doc["components"]["schemas"];
    assert_eq!(
        schemas["AccountsBody"]["properties"]["accounts"]["items"]["$ref"],
        "#/components/schemas/AccountObservation"
    );
    let observation = &schemas["AccountObservation"];
    assert!(observation["properties"].get("key_health").is_some());
    assert!(observation["properties"].get("note").is_some());
    let required = observation["required"].as_array().unwrap();
    assert!(
        !required
            .iter()
            .any(|name| name == "key_health" || name == "note")
    );
    assert_eq!(schemas["Timestamp"]["type"], "string");
    assert_eq!(schemas["Timestamp"]["format"], "date-time");
    assert!(
        schemas["QuotaWindow"]["properties"]
            .get("attribution")
            .is_some()
    );
    assert_eq!(schemas["Amount"]["type"], "string");
}
