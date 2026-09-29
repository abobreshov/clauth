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
