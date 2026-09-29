#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The `hermes_projector_real` tier (hermes spec §7): the REAL [`PROJECTOR`]
//! against real PyYAML and python-dotenv, at the versions Hermes 0.19.0 pins
//! (PyYAML 6.0.3, python-dotenv 1.2.2), plus the vendored
//! `utils.fast_safe_load` (`tests/fixtures/hermes/projector-real/vendor`).
//! This is the only place the security-critical projector meets real parsers;
//! tests 22–26 drive it through the stub.
//!
//! Off by default twice over: the module needs the `hermes-projector-real`
//! feature, and each test is `#[ignore]`d because `--all-features` (the main
//! CI legs, the local clippy gate) turns the feature on where no such venv
//! exists. Its own CI job builds the venv from cached wheels and runs
//! `cargo nextest run --features hermes-projector-real --run-ignored all -E
//! 'test(/projector_real/)'` with `TOLLGATE_PROJECTOR_REAL_PYTHON` naming the
//! venv's interpreter. Nothing here runs Hermes, mise or the network.
//!
//! The interpreter runs under `-I`, which ignores `PYTHONPATH`, so a two-line
//! wrapper puts the vendored `utils.py` first on `sys.path` and then executes
//! the projector's text unchanged, with the argv the launch passes.
//! `TOLLGATE_PROJECTOR_REAL_BLESS=1` rewrites each `expected.json` (review
//! the diff: an expectation is a claim about Hermes' parser, not a snapshot).

use std::path::{Path, PathBuf};

use super::*;
use crate::hermes::profiles::{Auth, HermesProfile, Mode, Provider};

const CASES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/hermes/projector-real"
);

fn python() -> PathBuf {
    PathBuf::from(std::env::var_os("TOLLGATE_PROJECTOR_REAL_PYTHON").expect(
        "TOLLGATE_PROJECTOR_REAL_PYTHON names a python with PyYAML 6.0.3 and python-dotenv 1.2.2",
    ))
}

/// The `-I`-safe wrapper: `$4` is the projector text, `$5` / `$6` its argv.
fn wrapper(dir: &Path) -> PathBuf {
    let path = dir.join("python-wrapper");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec '{py}' -I -B -c 'import sys; sys.path.insert(0, sys.argv[1]); \
             src = sys.argv[2]; sys.argv = [sys.argv[0]] + sys.argv[3:]; \
             exec(compile(src, \"<projector>\", \"exec\"), {{\"__name__\": \"__main__\"}})' \
             '{vendor}' \"$4\" \"$5\" \"$6\"\n",
            py = python().display(),
            vendor = Path::new(CASES).join("vendor").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn cases() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(CASES)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("verdict.txt").is_file())
        .collect();
    v.sort();
    assert!(v.len() >= 6, "the tier found only {} fixtures", v.len());
    v
}

/// A home holding the case's `config.yaml`, `.env` and `.op.env`.
fn home_for(case: &Path, root: &Path) -> PathBuf {
    let home = root.join("profiles").join("or-main").join("hermes-home");
    std::fs::create_dir_all(&home).unwrap();
    for f in ["config.yaml", ".env", ".op.env"] {
        if case.join(f).is_file() {
            std::fs::copy(case.join(f), home.join(f)).unwrap();
        }
    }
    home
}

fn project(case: &Path, root: &Path) -> ProjectionV1 {
    let home = home_for(case, root);
    run_projector(std::process::Command::new(wrapper(root)), &home, None)
        .unwrap_or_else(|e| panic!("{}: {e:#}", case.display()))
}

/// Every fixture's real projection equals its `expected.json` exactly, and
/// no value (a key, a URL path or query) reaches the output.
#[test]
#[ignore = "needs the hermes_projector_real venv (TOLLGATE_PROJECTOR_REAL_PYTHON)"]
fn projector_real_matches_every_expected_projection() {
    let bless = std::env::var_os("TOLLGATE_PROJECTOR_REAL_BLESS").is_some();
    for case in cases() {
        let root = tempfile::tempdir().unwrap();
        let home = home_for(&case, root.path());
        let mut command = std::process::Command::new(wrapper(root.path()));
        command
            .arg("-I")
            .arg("-B")
            .arg("-c")
            .arg(PROJECTOR)
            .arg(&home)
            .arg("");
        let raw = command.output().unwrap();
        assert!(
            raw.status.success(),
            "{}: {}",
            case.display(),
            String::from_utf8_lossy(&raw.stderr)
        );
        let text = String::from_utf8(raw.stdout).unwrap();
        for leak in [
            "never-printed",
            "fixture-not-a-key",
            "op://",
            "/v1/messages",
            "/chat",
        ] {
            assert!(
                !text.contains(leak),
                "{}: {leak} leaked: {text}",
                case.display()
            );
        }
        let got = parse_projection(text.as_bytes()).unwrap();
        let expected_path = case.join("expected.json");
        if bless {
            let pretty: serde_json::Value = serde_json::from_str(&text).unwrap();
            std::fs::write(
                &expected_path,
                serde_json::to_string_pretty(&pretty).unwrap() + "\n",
            )
            .unwrap();
            continue;
        }
        let want = parse_projection(&std::fs::read(&expected_path).unwrap()).unwrap();
        assert_eq!(got, want, "{}", case.display());
        assert_eq!(got, project(&case, root.path()), "run_projector agrees");
    }
}

/// G7–G12 over each real projection give the case's `verdict.txt`: `pass`,
/// or `refused: <text the refusal contains>`.
#[test]
#[ignore = "needs the hermes_projector_real venv (TOLLGATE_PROJECTOR_REAL_PYTHON)"]
fn projector_real_guard_verdicts() {
    let profile = HermesProfile {
        name: "or-main".into(),
        provider: Provider::Openrouter,
        model: None,
        mode: Mode::Account,
        auth: Auth::Env,
        key_env: Some("OPENROUTER_API_KEY".into()),
        key_fingerprint: None,
        created_at: "2026-09-29T00:00:00Z".into(),
    };
    for case in cases() {
        let root = tempfile::tempdir().unwrap();
        let projection = project(&case, root.path());
        let home = root.path().join("profiles/or-main/hermes-home");
        let verdict = crate::hermes::guards::audit_projection(
            "or-main",
            &home,
            &profile,
            &projection,
            None,
            &std::collections::BTreeSet::new(),
            Path::new("/venv/bin/hermes"),
        )
        .and_then(|_| crate::hermes::guards::g12_env("or-main", &projection));
        let want = std::fs::read_to_string(case.join("verdict.txt")).unwrap();
        let want = want.trim();
        match (want.strip_prefix("refused: "), verdict) {
            (None, Ok(())) => assert_eq!(want, "pass", "{}", case.display()),
            (Some(text), Err(e)) => assert!(
                e.to_string().contains(text),
                "{}: want {text:?}, got {e}",
                case.display()
            ),
            (None, Err(e)) => panic!("{}: want pass, got {e}", case.display()),
            (Some(text), Ok(())) => panic!("{}: want {text:?}, got pass", case.display()),
        }
    }
}
