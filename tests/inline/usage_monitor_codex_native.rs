#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
#[test]
fn codex_native_expired_jwt_is_auth_required() {
    let home = crate::testutil::HomeSandbox::new();
    std::fs::write(home.home().join("auth.json"),r#"{"tokens":{"access_token":"bad","account_id":"test","refresh_token":"REFRESH-CANARY","id_token":"ID-CANARY"}}"#).unwrap();
    assert_eq!(
        read_auth(home.home(), 100).unwrap_err().kind,
        FailureKind::AuthRequired
    );
}
#[cfg(unix)]
#[test]
fn codex_native_refuses_store_symlinks() {
    let home = crate::testutil::HomeSandbox::new();
    std::os::unix::fs::symlink(
        home.home().join(".tollgate/profiles/test/auth.json"),
        home.home().join("auth.json"),
    )
    .unwrap();
    let f = read_auth(home.home(), 100).unwrap_err();
    assert_eq!(f.kind, FailureKind::Unavailable);
    assert!(f.message.contains("codex:test"));
}
fn jwt(exp: i64) -> String {
    use base64::Engine;
    format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::json!({"exp":exp}).to_string())
    )
}
#[test]
fn codex_native_holds_only_access_account_and_fedramp() {
    let home = crate::testutil::HomeSandbox::new();
    let value = serde_json::json!({"tokens":{"access_token":jwt(2000000000),"account_id":"test","refresh_token":"REFRESH-CANARY","id_token":"ID-CANARY"},"chatgpt_account_is_fedramp":true});
    std::fs::write(home.home().join("auth.json"), value.to_string()).unwrap();
    let auth = read_auth(home.home(), 100).unwrap();
    let debug = format!("{auth:?}");
    assert!(!debug.contains("REFRESH-CANARY"));
    assert!(!debug.contains("ID-CANARY"));
    assert!(auth.chatgpt_account_is_fedramp);
    assert_eq!(auth.tokens.account_id.as_deref(), Some("test"));
}
#[test]
fn codex_native_401_does_not_refresh() {
    let home = crate::testutil::HomeSandbox::new();
    let tool = home.home().join(".codex");
    std::fs::create_dir(&tool).unwrap();
    let path = tool.join("auth.json");
    let raw = serde_json::json!({"tokens":{"access_token":jwt(2000000000),"account_id":"test"}})
        .to_string();
    std::fs::write(&path, &raw).unwrap();
    let cfg: super::super::config::MonitorConfig =
        toml::from_str("id='native'\nkind='codex_native'\n").unwrap();
    let target = super::super::source::resolve_target(&cfg, home.home(), 100, &|_| None);
    let mut http = super::super::source::FakeHttp::offline();
    http.codex_reply = Box::new(|| Err(FetchError::Status(401)));
    let failure = CodexNativeSource.fetch(&target, &http).unwrap_err();
    assert_eq!(failure.kind, FailureKind::AuthRequired);
    assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    assert_eq!(http.calls.lock().unwrap().len(), 1);
}
// Slice-1 review: native and profile legs share the exact same projection.
#[test]
fn codex_native_windows_equal_the_profile_leg_projection() {
    let home = crate::testutil::HomeSandbox::new();
    let tool = home.home().join(".codex");
    std::fs::create_dir(&tool).unwrap();
    std::fs::write(
        tool.join("auth.json"),
        serde_json::json!({"tokens":{"access_token":jwt(2000000000),"account_id":"fixture"}})
            .to_string(),
    )
    .unwrap();
    let now = 1900000000;
    let usage=crate::usage::map_codex_usage(r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_at":2000000000},"secondary_window":{"used_percent":75,"limit_window_seconds":604800,"reset_at":2000000001}}}"#,now).unwrap();
    let mut profile = AccountObservation::new(
        "codex:fixture".into(),
        SourceId::Codex,
        AuthKind::Subscription,
        Origin::Profile,
        "Fixture",
    );
    crate::usage::project::apply_codex_usage(&mut profile, &usage, now);
    let cfg = super::super::config::MonitorConfig::new(
        "native",
        super::super::config::MonitorKind::CodexNative,
    );
    let target = super::super::source::resolve_target(&cfg, home.home(), now, &|_| None);
    let mut http = super::super::source::FakeHttp::offline();
    http.codex_reply = Box::new(move || Ok(usage.clone()));
    let native = CodexNativeSource.fetch(&target, &http).unwrap();
    assert_eq!(native.windows, profile.windows);
    assert_eq!(native.windows.len(), 2);
    assert_eq!(native.plan, profile.plan);
}

#[test]
fn codex_usage_adopts_one_official_login_and_skips_symlinks() {
    let home = crate::testutil::HomeSandbox::new();
    let tool = home.home().join(".codex");
    std::fs::create_dir(&tool).unwrap();
    let path = tool.join("auth.json");
    let now = crate::usage::now_epoch_secs();
    let exp = now + 86_400;
    let usage = crate::usage::map_codex_usage(
        &serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": 18000,
                    "reset_at": exp
                },
                "secondary_window": {
                    "used_percent": 75,
                    "limit_window_seconds": 604800,
                    "reset_at": exp + 1
                }
            }
        })
        .to_string(),
        now,
    )
    .unwrap();
    let mut http = super::super::source::FakeHttp::offline();
    http.codex_reply = Box::new(move || Ok(usage.clone()));
    let opts = crate::usage::collect::CollectOpts::default();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(home.home().join(".tollgate/profiles/test/auth.json"), &path)
            .unwrap();
        let accounts = crate::usage::report::accounts(&opts, &http);
        assert!(accounts.iter().all(|a| a.id != "monitor:codex-native"));
        assert!(http.calls().is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    let raw = serde_json::json!({
        "tokens": {
            "access_token": jwt(exp),
            "account_id": "fixture",
            "refresh_token": "REFRESH-CANARY"
        }
    })
    .to_string();
    std::fs::write(&path, &raw).unwrap();
    let first = crate::usage::report::accounts(&opts, &http);
    let account = first
        .iter()
        .find(|a| a.id == "monitor:codex-native")
        .expect("expected monitor:codex-native observation");
    assert_eq!(account.plan.as_deref(), Some("plus"));
    assert_eq!(account.windows.len(), 2);
    assert_eq!(account.windows[0].used_pct, Some(25.0));
    assert_eq!(account.windows[1].used_pct, Some(75.0));
    let calls = http.calls().len();
    assert_eq!(calls, 1);

    let second = crate::usage::report::accounts(&opts, &http);
    assert!(second.iter().any(|a| a.id == "monitor:codex-native"));
    assert_eq!(
        http.calls().len(),
        calls,
        "a second usage read must use the cache"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    let monitors = super::super::config::load().unwrap();
    let codex_monitors: Vec<_> = monitors
        .iter()
        .filter(|m| m.kind == super::super::config::MonitorKind::CodexNative)
        .collect();
    assert_eq!(codex_monitors.len(), 1);
    assert_eq!(codex_monitors[0].id, "codex-native");
    assert_eq!(codex_monitors[0].tool_home.as_deref(), Some("~/.codex"));
    assert_eq!(codex_monitors[0].ttl_secs, Some(300));
}
