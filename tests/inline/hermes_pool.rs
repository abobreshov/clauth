#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The whitelist `auth.json` view: secret-bearing fields never land in a
//! tollgate value.

use super::*;
use crate::testutil::HomeSandbox;

const SECRETS_BEARING: &str = r#"{
  "version": 1,
  "active_provider": "openrouter",
  "providers": {"nous": {"access_token": "SENTINEL-ACCESS", "refresh_token": "SENTINEL-REFRESH", "agent_key": "SENTINEL-AGENT"}},
  "credential_pool": {"openrouter": [
    {"id": "e1", "label": "work", "source": "env:OPENROUTER_API_KEY", "auth_type": "api_key",
     "priority": 0, "last_status": "ok", "request_count": 3, "secret_fingerprint": "sha256:1b43f854967b5ae2",
     "api_key": "SENTINEL-APIKEY", "access_token": "SENTINEL-ACCESS2"}
  ]},
  "future_top_level": {"x": 1}
}"#;

#[test]
fn the_view_holds_no_secret_field() {
    let sb = HomeSandbox::new();
    std::fs::write(sb.home().join("auth.json"), SECRETS_BEARING).unwrap();
    let view = read_auth_view(sb.home()).unwrap().expect("present");
    assert!(view.version_known());
    assert_eq!(view.active_provider.as_deref(), Some("openrouter"));
    assert_eq!(view.providers.keys().collect::<Vec<_>>(), ["nous"]);
    let e = &view.entries("openrouter")[0];
    assert_eq!(
        e.secret_fingerprint.as_deref(),
        Some("sha256:1b43f854967b5ae2")
    );
    assert_eq!(e.request_count, Some(3));
    let debug = format!("{view:?}");
    assert!(!debug.contains("SENTINEL"), "{debug}");
    assert!(view.entries("nous").is_empty());
}

#[test]
fn absent_passes_and_a_symlink_or_torn_file_errs() {
    let sb = HomeSandbox::new();
    assert!(read_auth_view(sb.home()).unwrap().is_none());
    std::fs::write(sb.home().join("real.json"), "{}").unwrap();
    std::os::unix::fs::symlink(sb.home().join("real.json"), sb.home().join("auth.json")).unwrap();
    assert!(read_auth_view(sb.home()).is_err());
    std::fs::remove_file(sb.home().join("auth.json")).unwrap();
    std::fs::write(sb.home().join("auth.json"), "{\"version\":").unwrap();
    assert!(read_auth_view(sb.home()).is_err());
}
