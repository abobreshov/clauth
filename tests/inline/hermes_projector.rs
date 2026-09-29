#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The projector fails closed (spec §4.3 P, test 25): a crash, garbage, a
//! hang, and any output that does not match `ProjectionV1` exactly.

use super::*;
use crate::hermes::testkit::{Fixture, passing_projection};
use crate::testutil::HomeSandbox;

fn run(fx: &Fixture, home: &Path) -> Result<ProjectionV1> {
    run_projector_with(
        std::process::Command::new(&fx.python),
        home,
        None,
        Duration::from_secs(2),
    )
}

#[test]
fn projector_failure_refuses_closed() {
    let sb = HomeSandbox::new();
    let fx = Fixture::new(&sb.home().join("fx"));
    let home = sb.home().join("h");
    std::fs::create_dir_all(&home).unwrap();

    // The passing fixture parses, and the stub saw `-I -B -c <code> home ""`.
    let p = run(&fx, &home).expect("the passing projection parses");
    assert_eq!(p.config.model_provider.as_deref(), Some("openrouter"));
    let call = fx.calls().pop().unwrap();
    assert_eq!(call.argv, ["projector", home.to_str().unwrap(), ""]);

    // Exit 1.
    fx.set_ctl("projector.exit", "1");
    let err = run(&fx, &home).unwrap_err();
    assert!(
        err.to_string().contains("the projector exited 1"),
        "{err:#}"
    );
    fx.set_ctl("projector.exit", "0");

    // Garbage.
    std::fs::write(fx.ctl.join("projection.json"), "not json {").unwrap();
    assert!(run(&fx, &home).is_err());

    // A hang past the timeout is killed and refused.
    fx.set_projection(&passing_projection("openrouter"));
    fx.set_ctl("projector.sleep", "5");
    let t0 = std::time::Instant::now();
    let err = run(&fx, &home).unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err:#}");
    assert!(t0.elapsed() < Duration::from_secs(4));
    std::fs::remove_file(fx.ctl.join("projector.sleep")).unwrap();

    // The strict schema: a missing key, an unknown key, a wrong type.
    let mut missing = passing_projection("openrouter");
    missing["config"]
        .as_object_mut()
        .unwrap()
        .remove("delegation");
    let mut nullable_missing = passing_projection("openrouter");
    nullable_missing["config"]
        .as_object_mut()
        .unwrap()
        .remove("model_provider");
    let mut unknown = passing_projection("openrouter");
    unknown["config"]["extra_route"] = serde_json::json!("anthropic");
    let mut unknown_top = passing_projection("openrouter");
    unknown_top["v2"] = serde_json::json!(true);
    let mut wrong = passing_projection("openrouter");
    wrong["env_keys"]["home"] = serde_json::json!([{"key": "A", "nonblank": "yes"}]);
    for (what, v) in [
        ("missing key", missing),
        ("missing nullable key", nullable_missing),
        ("unknown key", unknown),
        ("unknown top-level key", unknown_top),
        ("wrong type", wrong),
    ] {
        fx.set_projection(&v);
        let err = run(&fx, &home).expect_err(what);
        assert!(
            format!("{err:#}").contains("does not match the projection schema"),
            "{what}: {err:#}"
        );
    }
}

/// A failure message never carries stderr past the exception class: a YAML
/// error's tail quotes the offending line, which may be an `api_key`.
#[test]
fn projector_errors_never_echo_config_values() {
    let sb = HomeSandbox::new();
    let fx = Fixture::new(&sb.home().join("fx"));
    std::fs::write(
        &fx.python,
        "#!/bin/sh\necho 'yaml.scanner.ScannerError: api_key: sk-secret-value' >&2\nexit 1\n",
    )
    .unwrap();
    let err = run(&fx, sb.home()).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("yaml.scanner.ScannerError"), "{text}");
    assert!(!text.contains("sk-secret-value"), "{text}");
}

#[test]
fn the_embedded_projector_names_hermes_own_parsers() {
    assert!(PROJECTOR.contains("from utils import fast_safe_load"));
    assert!(PROJECTOR.contains("from dotenv import dotenv_values"));
    // Hosts only, never a path or a value.
    assert!(PROJECTOR.contains(".hostname"));
    assert!(!PROJECTOR.contains("api_key"));
}
