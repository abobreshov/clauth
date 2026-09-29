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
    assert_eq!(CATALOG.len(), 15);

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
