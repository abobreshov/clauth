#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::testutil::HomeSandbox;
#[test]
fn capture_writes_0600_shape_files_with_strings_masked() {
    let _home = HomeSandbox::new();
    let dir = crate::profile::home_dir().unwrap().join("capture");
    let http = super::super::source::LiveHttp;
    let capture = CaptureHttp::new(&http, Some(&dir)).unwrap();
    capture.set_id("grok");
    capture.record(&HttpReply{status:200,body:r#"{"token":"SECRET-CANARY","creditUsagePercent":42,"type":"WEEKLY","nested":["CANARY"]}"#.into(),headers:vec![],retry_after_secs:None}).unwrap();
    let path = dir.join("grok-1.shape.json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("CANARY"));
    assert!(text.contains("WEEKLY"));
    assert!(text.contains("42"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
#[test]
fn capture_masks_nested_objects_under_enum_keys() {
    let result = shape(
        serde_json::json!({"displayName":{"credential":"SECRET-CANARY"},"array":["CANARY",9],"type":"WEEKLY"}),
    );
    let dump = result.to_string();
    assert!(!dump.contains("CANARY"));
    assert!(dump.contains("WEEKLY"));
    assert_eq!(result["array"][1], 9);
}
#[cfg(unix)]
#[test]
fn capture_refuses_symlink_directory() {
    let _home = HomeSandbox::new();
    let home = crate::profile::home_dir().unwrap();
    std::fs::create_dir(home.join("target")).unwrap();
    std::os::unix::fs::symlink(home.join("target"), home.join("capture")).unwrap();
    assert!(
        CaptureHttp::new(&super::super::source::LiveHttp, Some(&home.join("capture"))).is_err()
    );
}

#[test]
fn codex_capture_masks_raw_fields_before_mapping_even_when_login_is_rejected() {
    use super::super::source::FakeHttp;
    let home = HomeSandbox::new();
    let dir = home.home().join("capture");
    let http = FakeHttp {
        codex_raw_reply: Some(HttpReply {
            status: 401,
            body: r#"{"unknown_numeric_unit":13.75,"error":{"message":"SECRET-CANARY","code":"rejected"}}"#.into(),
            headers: vec![("set-cookie".into(), "SECRET-CANARY".into()), ("X-Ratelimit-Remaining-Tokens".into(), "7".into())],
            retry_after_secs: None,
        }),
        codex_reply: Box::new(|| Err(FetchError::Status(401))),
        ..FakeHttp::offline()
    };
    let capture = CaptureHttp::new(&http, Some(&dir)).unwrap();
    capture.set_id("codex-native");
    assert!(matches!(
        capture.codex_usage(&Secret::new("TOKEN-CANARY"), None, false, 1000),
        Err(FetchError::Status(401))
    ));
    let dump = std::fs::read_to_string(dir.join("codex-native-1.shape.json")).unwrap();
    assert!(!dump.contains("CANARY"));
    let value: Value = serde_json::from_str(&dump).unwrap();
    assert_eq!(value["status"], 401);
    assert_eq!(value["body"]["unknown_numeric_unit"], 13.75);
    assert_eq!(
        value["headers"],
        serde_json::json!([["x-ratelimit-remaining-tokens", "7"]])
    );
}

#[test]
fn legacy_provider_and_wallet_capture_each_raw_reply_before_normalization() {
    use super::super::source::FakeHttp;
    let home = HomeSandbox::new();
    let dir = home.home().join("capture");
    let http = FakeHttp::stats(|| {
        assert!(crate::usage::keyed_http::response_observer_active());
        for status in [200, 403] {
            crate::usage::keyed_http::observe_response(&crate::usage::keyed_http::Reply {
                status,
                body: Some(r#"{"raw_numeric_units":123.45,"credential":"SECRET-CANARY"}"#.into()),
                headers: vec![("set-cookie".into(), "SECRET-CANARY".into())],
                retry_after: None,
            });
        }
        Ok(ThirdPartyStats {
            is_available: true,
            rows: vec![],
            bars: vec![],
            plan: None,
            endpoint: None,
            best_effort: false,
            observed: None,
        })
    });
    let capture = CaptureHttp::new(&http, Some(&dir)).unwrap();
    capture.set_id("legacy");
    let target = ThirdPartyTarget::Generic {
        base_url: "https://fixture.invalid".into(),
    };
    capture
        .third_party(&target, &Secret::new("KEY-CANARY"))
        .unwrap();
    capture
        .openrouter_wallet(&Secret::new("KEY-CANARY"))
        .unwrap();
    assert!(!crate::usage::keyed_http::response_observer_active());
    for n in 1..=4 {
        let dump = std::fs::read_to_string(dir.join(format!("legacy-{n}.shape.json"))).unwrap();
        assert!(!dump.contains("CANARY"));
        let shape: Value = serde_json::from_str(&dump).unwrap();
        assert_eq!(shape["body"]["raw_numeric_units"], 123.45);
        assert_eq!(shape["status"], if n % 2 == 1 { 200 } else { 403 });
        assert_eq!(shape["headers"], serde_json::json!([]));
    }
}

#[test]
fn legacy_capture_write_failure_is_reported_and_scope_is_removed() {
    use super::super::source::FakeHttp;
    let home = HomeSandbox::new();
    let dir = home.home().join("capture");
    let http = FakeHttp::stats(|| {
        crate::usage::keyed_http::observe_response(&crate::usage::keyed_http::Reply {
            status: 200,
            body: Some("{}".into()),
            headers: vec![],
            retry_after: None,
        });
        Err(ThirdPartyError::Status)
    });
    let capture = CaptureHttp::new(&http, Some(&dir)).unwrap();
    std::fs::remove_dir(&dir).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();
    assert!(matches!(
        capture.openrouter_wallet(&Secret::new("KEY-CANARY")),
        Err(ThirdPartyError::Parse)
    ));
    assert!(!crate::usage::keyed_http::response_observer_active());
}

#[test]
fn guest_capture_refuses_operator_paths_before_touching_them() {
    let home = HomeSandbox::new();
    std::fs::create_dir(home.home().join(".clauth")).unwrap();
    assert!(crate::identity::upstream_active());
    let http = super::super::source::FakeHttp::offline();
    let operator = home.home().join(".clauth/capture");
    assert!(CaptureHttp::new(&http, Some(&operator)).is_err());
    assert!(!operator.exists());
    assert!(
        CaptureHttp::new(
            &http,
            Some(&home.home().join(".tollgate/../.clauth/capture"))
        )
        .is_err()
    );
    assert!(CaptureHttp::new(&http, Some(&home.home().join(".tollgate/capture"))).is_ok());
}

#[cfg(unix)]
#[test]
fn guest_capture_refuses_a_link_inside_tollgate_pointing_into_upstream() {
    let home = HomeSandbox::new();
    std::fs::create_dir(home.home().join(".clauth")).unwrap();
    std::fs::create_dir(home.home().join(".tollgate")).unwrap();
    std::os::unix::fs::symlink(
        home.home().join(".clauth"),
        home.home().join(".tollgate/outside"),
    )
    .unwrap();
    let http = super::super::source::FakeHttp::offline();
    assert!(CaptureHttp::new(&http, Some(&home.home().join(".tollgate/outside/capture"))).is_err());
    assert!(!home.home().join(".clauth/capture").exists());
}
