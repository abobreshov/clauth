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
