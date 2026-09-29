#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The MCP `usage` tool: read-only, listed with its input schema, and
//! answering the local API's redacted `usage --json` envelope with the same
//! filters. Caches under a `HomeSandbox` only.

use super::*;

use crate::profile::{AppState, Profile, save_app_state, save_profile};
use crate::testutil::HomeSandbox;

fn seed() {
    save_profile(&Profile::new("solo".to_string(), None, None)).expect("save solo");
    save_profile(&Profile::new(
        "vendor".to_string(),
        Some("https://user:hunter2hunter2@api.deepseek.com/anthropic?key=sk-q".to_string()),
        Some("sk-test-not-a-real-key-0123456789".to_string()),
    ))
    .expect("save vendor");
    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "vendor".into()],
        ..Default::default()
    })
    .expect("save state");
}

fn call(args: UsageArgs) -> CallToolResult {
    let server = TollgateServer::new();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(async { server.usage(Parameters(args)).await })
        .expect("usage returns a tool result")
}

fn payload(result: &CallToolResult) -> serde_json::Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("a text block");
    serde_json::from_str(&text).expect("the block is JSON")
}

#[test]
fn the_usage_tool_is_listed_read_only_with_its_filters() {
    let server = TollgateServer::new();
    let tool = server
        .tool_router
        .list_all()
        .into_iter()
        .find(|t| t.name == "usage")
        .expect("`usage` is registered");
    let annotations = tool.annotations.as_ref().expect("annotations");
    assert_eq!(annotations.read_only_hint, Some(true));
    let schema = serde_json::to_value(&*tool.input_schema).unwrap();
    let props = schema["properties"].as_object().expect("properties");
    let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["account", "all", "provider"]);
    assert!(
        schema
            .get("required")
            .is_none_or(|r| r.as_array().is_none_or(Vec::is_empty)),
        "every filter is optional: {schema}"
    );
}

#[test]
fn the_usage_tool_returns_the_redacted_envelope() {
    let _home = HomeSandbox::new();
    seed();
    let result = call(UsageArgs::default());
    assert_ne!(result.is_error, Some(true));
    let body = payload(&result);
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["guest_mode"], false);
    let ids: Vec<&str> = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["claude:solo", "claude:vendor"]);
    let text = body.to_string();
    for secret in ["hunter2", "sk-test", "sk-q", "user:"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert_eq!(
        body["accounts"][1]["endpoint"],
        "https://api.deepseek.com/anthropic"
    );
    // Parses back as the same envelope `tollgate usage --json` prints.
    let _: crate::usage::report::UsageReport = serde_json::from_value(body).unwrap();
}

#[test]
fn the_usage_tool_filters_by_account_and_provider() {
    let _home = HomeSandbox::new();
    seed();
    let one = payload(&call(UsageArgs {
        account: Some("claude:vendor".into()),
        ..UsageArgs::default()
    }));
    assert_eq!(one["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(one["accounts"][0]["label"], "vendor");

    let none = payload(&call(UsageArgs {
        provider: Some("codex".into()),
        ..UsageArgs::default()
    }));
    assert!(none["accounts"].as_array().unwrap().is_empty());

    // An empty filter is no filter.
    let all = payload(&call(UsageArgs {
        account: Some(String::new()),
        provider: Some(String::new()),
        all: Some(true),
    }));
    assert_eq!(all["accounts"].as_array().unwrap().len(), 2);
}
