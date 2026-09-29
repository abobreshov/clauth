#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Entrypoint resolution (spec §4.5), the version gate (G15) and the compiled
//! S7(f) gate.

use super::*;
use crate::hermes::profiles::VersionPolicy;
use crate::hermes::testkit::Fixture;
use crate::testutil::HomeSandbox;

fn env(root: &Path) -> ResolveEnv {
    ResolveEnv {
        settings_bin: None,
        mise_data_dir: root.join("mise"),
        pipx_home: root.join("pipx"),
        path: None,
        tollgate_dir: root.join(".tollgate"),
    }
}

fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `mise` stub on a PATH dir: records its cwd and whether stdin was at EOF,
/// then prints `answer` (a dir holding `hermes-agent/`).
fn mise_stub(bin: &Path, rec: &Path, answer: &Path) {
    write_exec(
        &bin.join("mise"),
        &format!(
            "#!/bin/sh\npwd > '{rec}'\nif read -r _line; then echo stdin=data >> '{rec}'; else echo stdin=eof >> '{rec}'; fi\necho \"$@\" >> '{rec}'\necho '{answer}'\n",
            rec = rec.display(),
            answer = answer.display()
        ),
    );
}

/// Test 28: the override wins; then the mise glob (highest semver, mise NOT
/// run); then `mise where` from `~/.tollgate` with stdin null; then pipx; then
/// PATH.
#[test]
fn resolver_prefers_override_then_mise_glob_then_mise_where_then_pipx_then_path() {
    let sb = HomeSandbox::new();
    let root = sb.home();
    let mut e = env(root);
    std::fs::create_dir_all(&e.tollgate_dir).unwrap();
    let bin = root.join("bin");
    let rec = root.join("mise.rec");
    let where_dir = root.join("where-install");
    let where_fx = Fixture::in_venv(
        &root.join("fx-where"),
        &where_dir.join("hermes-agent"),
        "0.19.2",
    );
    mise_stub(&bin, &rec, &where_dir);
    e.path = Some(bin.clone().into_os_string());

    // Nothing but `mise where`: mise runs, from ~/.tollgate, stdin at EOF.
    let got = resolve_entrypoint(&e).unwrap();
    assert_eq!(got.entry, where_fx.entry);
    assert_eq!(got.version, "0.19.2");
    let log = std::fs::read_to_string(&rec).unwrap();
    let mut lines = log.lines();
    assert_eq!(
        std::fs::canonicalize(lines.next().unwrap()).unwrap(),
        std::fs::canonicalize(&e.tollgate_dir).unwrap(),
        "mise where runs in ~/.tollgate, so no project .mise.toml loads"
    );
    assert_eq!(lines.next(), Some("stdin=eof"));
    assert_eq!(lines.next(), Some("where pipx:hermes-agent"));
    std::fs::remove_file(&rec).unwrap();

    // The mise glob: the highest semver dir wins, `latest` and `0.19` are not
    // install dirs, and mise itself is not run.
    let installs = e.mise_data_dir.join("installs/pipx-hermes-agent");
    let old = Fixture::in_venv(
        &root.join("fx-old"),
        &installs.join("0.9.9/hermes-agent"),
        "0.9.9",
    );
    let new = Fixture::in_venv(
        &root.join("fx-new"),
        &installs.join("0.19.0/hermes-agent"),
        "0.19.0",
    );
    std::os::unix::fs::symlink(installs.join("0.9.9"), installs.join("latest")).unwrap();
    std::os::unix::fs::symlink(installs.join("0.9.9"), installs.join("0.99")).unwrap();
    let got = resolve_entrypoint(&e).unwrap();
    assert_eq!(got.entry, new.entry);
    assert_eq!(got.python, new.python);
    assert_eq!(got.hsp, new.hsp);
    assert!(!rec.exists(), "the glob matched, so mise must not run");
    let _ = old;

    // The override beats everything.
    let over = Fixture::new(&root.join("fx-over"));
    e.settings_bin = Some(over.entry.clone());
    assert_eq!(resolve_entrypoint(&e).unwrap().entry, over.entry);
    e.settings_bin = None;

    // pipx, then PATH, once mise has nothing.
    std::fs::remove_dir_all(&e.mise_data_dir).unwrap();
    std::fs::remove_file(bin.join("mise")).unwrap();
    let pipx = Fixture::in_venv(
        &root.join("fx-pipx"),
        &e.pipx_home.join("venvs/hermes-agent"),
        "0.19.1",
    );
    assert_eq!(resolve_entrypoint(&e).unwrap().entry, pipx.entry);
    std::fs::remove_dir_all(&e.pipx_home).unwrap();
    let on_path = Fixture::new(&root.join("fx-path"));
    std::os::unix::fs::symlink(&on_path.entry, bin.join("hermes")).unwrap();
    let got = resolve_entrypoint(&e).unwrap();
    assert_eq!(
        got.entry,
        bin.join("hermes"),
        "a link to a real entrypoint is accepted"
    );
    assert_eq!(got.python, on_path.python);

    std::fs::remove_file(bin.join("hermes")).unwrap();
    let ResolveError::NotFound { tried } = resolve_entrypoint(&e).unwrap_err() else {
        panic!("nothing left must be NotFound");
    };
    assert_eq!(tried.len(), 3, "{tried:?}");
    let msg = ResolveError::NotFound { tried }.message("or-main");
    assert!(msg.starts_with("tollgate: hermes 'or-main': cannot find the Hermes install (tried "));
    assert!(msg.ends_with("or set [settings] bin in ~/.tollgate/hermes-profiles.toml"));
    assert!(
        !msg.contains("run hermes"),
        "never point at the self-installing shim"
    );
}

/// Test 29: the Omarchy shim is turned away by its first line and never run.
#[test]
fn resolver_rejects_the_omarchy_shim_and_never_executes_it() {
    let sb = HomeSandbox::new();
    let root = sb.home();
    let mut e = env(root);
    let bin = root.join("local-bin");
    let sentinel = root.join("shim-ran");
    let shim = include_str!("../fixtures/hermes/omarchy-shim.sh")
        .replace("__SENTINEL__", &sentinel.display().to_string());
    write_exec(&bin.join("hermes"), &shim);
    e.path = Some(bin.clone().into_os_string());

    let err = resolve_entrypoint(&e).unwrap_err();
    assert_eq!(
        err,
        ResolveError::Shim {
            path: bin.join("hermes")
        }
    );
    assert_eq!(
        err.message("or-main"),
        format!(
            "tollgate: hermes 'or-main': {} is a launcher script, not the Hermes entrypoint; \
             tollgate will not run it (it can install software)",
            bin.join("hermes").display()
        )
    );
    // The same verdict as the override.
    e.settings_bin = Some(bin.join("hermes"));
    assert!(matches!(
        resolve_entrypoint(&e),
        Err(ResolveError::Shim { .. })
    ));
    // `#!/usr/bin/env python3` is not a venv entrypoint either.
    write_exec(&bin.join("hermes"), "#!/usr/bin/env python3\nprint('x')\n");
    assert!(matches!(
        resolve_entrypoint(&e),
        Err(ResolveError::Shim { .. })
    ));
    // A shebang naming ANOTHER venv's python is refused.
    let other = Fixture::new(&root.join("fx-other"));
    write_exec(
        &bin.join("hermes"),
        &format!(
            "#!{}\nfrom hermes_cli.main import main\n",
            other.python.display()
        ),
    );
    assert!(matches!(
        resolve_entrypoint(&e),
        Err(ResolveError::Shim { .. })
    ));
    assert!(!sentinel.exists(), "the shim must never have been executed");
}

/// Test 30: `Version:` from METADATA; outside 0.19.x warns (W-VERSION) or,
/// with `version_policy = "refuse"`, refuses.
#[test]
fn version_read_from_metadata_warns_or_refuses_per_policy() {
    let sb = HomeSandbox::new();
    let fx = Fixture::with_version(&sb.home().join("fx"), "0.20.1");
    let mut e = env(sb.home());
    e.settings_bin = Some(fx.entry.clone());
    let got = resolve_entrypoint(&e).unwrap();
    assert_eq!(got.version, "0.20.1");

    assert_eq!(
        version_verdict("or-main", "0.19.7", VersionPolicy::Refuse),
        VersionVerdict::Ok
    );
    assert_eq!(
        version_verdict("or-main", "0.20.1", VersionPolicy::Warn),
        VersionVerdict::Warn(
            "tollgate: note — Hermes 0.20.1 is installed; tollgate's guards were verified against \
             0.19.x"
                .into()
        )
    );
    let VersionVerdict::Refuse(line) = version_verdict("or-main", "0.20.1", VersionPolicy::Refuse)
    else {
        panic!("refuse policy must refuse");
    };
    assert!(
        line.starts_with("tollgate: hermes 'or-main': Hermes 0.20.1"),
        "{line}"
    );
    assert!(!in_verified_series("0.1.9") && !in_verified_series("1.19.0"));
    assert!(in_verified_series("0.19.12"));
}

/// Zero or several site-packages / METADATA refuse with M-BIN.
#[test]
fn a_venv_without_a_single_hermes_install_is_not_found() {
    let sb = HomeSandbox::new();
    let fx = Fixture::new(&sb.home().join("fx"));
    let mut e = env(sb.home());
    e.settings_bin = Some(fx.entry.clone());
    std::fs::create_dir_all(fx.hsp.join("hermes_agent-0.18.0.dist-info")).unwrap();
    std::fs::write(
        fx.hsp.join("hermes_agent-0.18.0.dist-info/METADATA"),
        "Version: 0.18.0\n",
    )
    .unwrap();
    assert!(matches!(
        resolve_entrypoint(&e),
        Err(ResolveError::NotFound { .. })
    ));
}

/// The compiled spike doc carries a passing block for 0.19.0, and the gate
/// matches by series.
#[test]
fn the_s7f_gate_is_compiled_from_the_spike_doc() {
    let gate = parse_s7f_gate(S7F_DOC).expect("the spike doc has its machine block");
    assert_eq!(gate.result, "pass");
    assert_eq!(gate.hermes, ["0.19.0"]);
    assert!(!gate.commit.is_empty());
    assert!(s7f_gate_refusal("or-main", "0.19.0").is_none());
    assert!(s7f_gate_refusal("or-main", "0.19.4").is_none());
    assert_eq!(
        s7f_gate_refusal("or-main", "0.20.0").as_deref(),
        Some(
            "tollgate: hermes 'or-main': the S7(f) HOME-redirect spike has not passed for Hermes \
             0.20.0"
        )
    );
    let failed =
        "x\n```toml\n# s7f-gate\nresult = \"fail\"\nhermes = [\"0.19.0\"]\ncommit = \"c\"\n```\n";
    assert!(!s7f_gate_passes(failed, "0.19.0"));
    assert!(!s7f_gate_passes("no block here", "0.19.0"));
}
