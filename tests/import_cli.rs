//! `tollgate import` against the real binary (spec import-clauth.md §7):
//! the dry-run is provably read-only (its global edits planned), a changing
//! command on a non-interactive stdin without `--yes` exits 2 and changes
//! nothing, every exit code of §2.2 is what the binary returns, a real run
//! and its rollback round-trip inside a sandbox, an interrupted journal is
//! named on every command, and the status read works on a fresh home.
//!
//! Every run points `HOME` at a tempdir fixture and pins `PATH` to the
//! built binary alone plus the system dirs, so no real home, no
//! real `clauth` and no real `claude` is ever in reach. The read-only proof
//! runs the dry-run inside bubblewrap with the WHOLE filesystem bound
//! read-only, the operator's real home masked by an empty tmpfs, and its own
//! pid namespace (so no live Claude Code session of the operator's shows up
//! in the scan); a machine without a usable `bwrap` skips that one test, and
//! the byte-and-metadata snapshot test below still runs everywhere.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use sha2::Digest as _;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tollgate"))
}

fn bin_dir() -> PathBuf {
    bin().parent().expect("bin dir").to_path_buf()
}

/// A directory that holds only a `tollgate` link to the built binary, so the
/// run sees itself as the installed binary on `PATH`. The build directory
/// itself stays off `PATH`: a stale `clauth` build artifact can sit beside
/// `tollgate` there, and the survey would resolve it as upstream's binary.
fn path_dir() -> PathBuf {
    let dir = bin_dir().join("import-cli-path");
    std::fs::create_dir_all(&dir).unwrap();
    match std::os::unix::fs::symlink(bin(), dir.join("tollgate")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => panic!("linking tollgate into {}: {e}", dir.display()),
    }
    dir
}

fn stub_path() -> String {
    format!("{}:/usr/bin:/bin", path_dir().display())
}

/// `tollgate` with `HOME` at `home`, a stub `PATH`, and nothing inherited
/// that names another config dir.
fn tollgate(home: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home)
        .env("PATH", stub_path())
        .env("TOLLGATE_NO_API", "1")
        .env("HERDR_BIN_PATH", home.join("no-herdr"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("HERDR_CONFIG_PATH")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn oauth(p: &str) -> String {
    format!(
        "{{\"claudeAiOauth\":{{\"accessToken\":\"sk-ant-oat01-FIXTURE-{p}-1\",\"refreshToken\":\"FIXTURE-RT-{p}-1\",\"subscriptionType\":\"max\"}}}}"
    )
}

/// A small upstream tree of the owner machine's shape.
fn fixture(home: &Path) {
    let src = home.join(".clauth");
    for p in ["leadtone", "personal"] {
        let dir = src.join("profiles").join(p);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("credentials.json"), oauth(p)).unwrap();
        std::fs::write(dir.join("account_id.json"), format!("\"acct-{p}\"")).unwrap();
        std::fs::write(dir.join("config.toml"), "name = \"x\"\n").unwrap();
    }
    std::fs::write(
        src.join("profiles.toml"),
        "active_profile = \"personal\"\nprofiles = [\"leadtone\", \"personal\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(src.join("conversations")).unwrap();
    std::fs::write(src.join("conversations/c1.json"), "{}").unwrap();
    std::fs::write(src.join("clauth.log"), "log\n").unwrap();
    std::fs::write(src.join(".lock"), "").unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    // Upstream's plugin on, so the dry-run plans G1 (a global edit it must
    // still not write).
    std::fs::write(
        home.join(".claude/settings.json"),
        "{\n  \"enabledPlugins\": {\n    \"clauth@clauth\": true\n  }\n}\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        src.join("profiles/personal/credentials.json"),
        home.join(".claude/.credentials.json"),
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".tollgate")).unwrap();
}

/// Every path under `root`: type, inode, mode, size, mtime, link count, link
/// target and content digest. Any write, create, chmod, rename or touch shows.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let ft = meta.file_type();
            let sha = if ft.is_file() {
                hex::encode(sha2::Sha256::digest(std::fs::read(&path).unwrap()))
            } else {
                String::new()
            };
            let link = std::fs::read_link(&path)
                .map(|l| l.display().to_string())
                .unwrap_or_default();
            out.insert(
                path.strip_prefix(root).unwrap().display().to_string(),
                format!(
                    "{:?} ino={} mode={:o} size={} mtime={}.{} nlink={} link={link} sha={sha}",
                    ft,
                    meta.ino(),
                    meta.mode(),
                    meta.size(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                    meta.nlink()
                ),
            );
            if ft.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The bubblewrap command a read-only dry-run runs under, or `None` when
/// this machine cannot run one.
fn bwrap(home: &Path) -> Option<Command> {
    sandbox(home, false)
}

/// [`bwrap`] with the fixture home bound read-WRITE (everything else stays
/// read-only and the operator's home stays an empty tmpfs): a real run's
/// sandbox, its own pid namespace so no live session of the operator's is in
/// its process scan.
fn sandbox(home: &Path, writable: bool) -> Option<Command> {
    let probe = Command::new("bwrap")
        .args([
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !probe.is_ok_and(|s| s.success()) {
        return None;
    }
    let real_home = std::env::var_os("HOME").map(PathBuf::from)?;
    let mut cmd = Command::new("bwrap");
    cmd.args(["--unshare-pid", "--unshare-net", "--die-with-parent"])
        .args(["--ro-bind", "/", "/"])
        .args(["--dev", "/dev", "--proc", "/proc"])
        // The operator's real home is an empty tmpfs in here: ~/.claude,
        // ~/.clauth, ~/.codex, ~/.hermes, herdr's config — none exists.
        .arg("--tmpfs")
        .arg(&real_home)
        // Only what the run needs comes back, read-only.
        .arg("--ro-bind")
        .arg(bin_dir())
        .arg(bin_dir())
        .arg(if writable { "--bind" } else { "--ro-bind" })
        .arg(home)
        .arg(home)
        .arg("--setenv")
        .arg("HOME")
        .arg(home)
        .args(["--setenv", "PATH"])
        .arg(stub_path())
        .args(["--setenv", "TOLLGATE_NO_API", "1"])
        // No herdr in reach (a path-like HERDR_BIN_PATH that does not exist
        // resolves to none), so G2 is skipped rather than run.
        .args(["--setenv", "HERDR_BIN_PATH"])
        .arg(home.join("no-herdr"))
        .args([
            "--unsetenv",
            "CLAUDE_CONFIG_DIR",
            "--unsetenv",
            "CODEX_HOME",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Some(cmd)
}

/// The dry-run inside a read-only mount namespace: the fixture home is
/// bound read-only (a control write there fails, proving it), and the
/// dry-run still succeeds and reports the tree — so it wrote nothing, not
/// even a lock file. The byte-and-metadata snapshot is unchanged too.
#[test]
fn dry_run_succeeds_on_a_read_only_bind_and_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let Some(mut control) = bwrap(home.path()) else {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    };
    let before = snapshot(home.path());
    let probe = home.path().join("probe");
    let denied = control
        .arg("/bin/sh")
        .arg("-c")
        .arg(format!("echo x > {}", probe.display()))
        .output()
        .unwrap();
    assert!(
        !denied.status.success(),
        "the bind is not read-only: {:?}",
        text(&denied)
    );
    let out = bwrap(home.path())
        .unwrap()
        .arg(bin())
        .args(["import", "clauth", "--dry-run", "--json"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("one JSON document on stdout");
    assert_eq!(report["mode"], "dry_run");
    assert_eq!(report["ok"], true, "{stdout}");
    assert_eq!(report["live_slots"]["claude"]["verdict"], "relink");
    assert!(report["journal"]["steps_planned"].as_u64().unwrap() > 5);
    assert!(!stderr.contains("Read-only file system"), "{stderr}");
    assert!(
        !stdout.contains("FIXTURE"),
        "a fixture token reached the report"
    );
    let text_run = bwrap(home.path())
        .unwrap()
        .arg(bin())
        .args(["import", "clauth", "--dry-run"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&text_run);
    assert_eq!(
        text_run.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.starts_with("tollgate import clauth --dry-run: nothing was changed"),
        "{stdout}"
    );
    assert_eq!(snapshot(home.path()), before);
}

/// Every global edit in reach of the read-only dry-run: an upstream `clauth`
/// on `PATH` that leaves a sentinel if anything ever runs it, and a fake
/// herdr that lists upstream's plugin and prints its config dir, with
/// upstream's marked block in herdr's config. Paths are baked in.
fn global_fixture(home: &Path) -> (PathBuf, PathBuf) {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let sentinel = home.join("RAN-CLAUTH");
    write_script(
        &bin.join("clauth"),
        &format!("touch '{}'\nexit 0\n", sentinel.display()),
    );
    let herdr_dir = home.join("fakeherdr");
    std::fs::create_dir_all(&herdr_dir).unwrap();
    let herdr = herdr_dir.join("herdr");
    let cfg = home.join(".config/herdr/plugins/config");
    write_script(
        &herdr,
        &format!(
            r#"mkdir -p '{root}/plugins/config/tollgate' && touch '{root}/.plugins.lock'
case "$1 $2" in
  "plugin list") echo '{{"id":"cli:plugin","result":{{"plugins":[{{"plugin_id":"clauth","enabled":true,"source":{{"kind":"github","owner":"uwuclxdy","repo":"clauth","resolved_commit":"abc123"}}}}],"type":"plugin_list"}}}}' ;;
  "plugin config-dir") echo '{}'/"$3" ;;
esac
exit 0
"#,
            cfg.display(),
            root = home.join(".config/herdr").display()
        ),
    );
    std::fs::create_dir_all(home.join(".config/herdr")).unwrap();
    std::fs::write(
        home.join(".config/herdr/config.toml"),
        "# clauth herdr plugin\n[[keys.bind]]\nkey = \"prefix+c\"\n",
    )
    .unwrap();
    (herdr, sentinel)
}

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The read-only proof with G1–G4 all in reach: the dry-run plans upstream's
/// herdr uninstall (G2) and the plugin-off edit (G1) from herdr's config read
/// in place (it never spawns herdr), and writes nothing and runs no upstream
/// binary.
#[test]
fn dry_run_plans_the_global_edits_on_a_read_only_bind_and_runs_nothing() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let (herdr, sentinel) = global_fixture(home.path());
    let Some(mut cmd) = bwrap(home.path()) else {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    };
    let before = snapshot(home.path());
    let out = cmd
        .args(["--setenv", "PATH"])
        .arg(format!(
            "{}:{}:/usr/bin:/bin",
            path_dir().display(),
            home.path().join("bin").display()
        ))
        .args(["--setenv", "HERDR_BIN_PATH"])
        .arg(&herdr)
        .arg(bin())
        .args(["import", "clauth", "--dry-run", "--json"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let ids: Vec<&str> = report["global_edits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|g| g["id"].as_str())
        .collect();
    assert!(ids.contains(&"G1") && ids.contains(&"G2"), "{ids:?}");
    assert!(!sentinel.exists(), "the dry-run ran upstream's binary");
    assert_eq!(snapshot(home.path()), before);
}

/// Review lens guest-ux #1: on a WRITABLE bind, with a herdr that creates
/// its plugin dirs and `.plugins.lock` whenever it runs (as the real one
/// does), the dry-run still leaves the home unchanged: it spawns no herdr.
#[test]
fn dry_run_on_a_writable_home_spawns_no_herdr_and_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let (herdr, sentinel) = global_fixture(home.path());
    let Some(mut cmd) = sandbox(home.path(), true) else {
        eprintln!("skipped: bubblewrap is not usable here");
        return;
    };
    let before = snapshot(home.path());
    let out = cmd
        .args(["--setenv", "PATH"])
        .arg(format!(
            "{}:{}:/usr/bin:/bin",
            path_dir().display(),
            home.path().join("bin").display()
        ))
        .args(["--setenv", "HERDR_BIN_PATH"])
        .arg(&herdr)
        .arg(bin())
        .args(["import", "clauth", "--dry-run", "--json"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        report["global_edits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == "G2"),
        "{stdout}"
    );
    assert!(!sentinel.exists(), "the dry-run ran upstream's binary");
    assert!(
        !home.path().join(".config/herdr/.plugins.lock").exists(),
        "the dry-run ran herdr"
    );
    assert_eq!(snapshot(home.path()), before);
}

/// Without a sandbox, the dry-run on a writable home still leaves every
/// byte, inode, mode, size and mtime as it was. (Outside a pid namespace the
/// scan may see live sessions and block, exit 3; either way nothing moves.)
#[test]
fn dry_run_leaves_a_full_byte_and_metadata_snapshot_unchanged() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let before = snapshot(home.path());
    for args in [
        &["import", "clauth", "--dry-run"][..],
        &["import", "clauth", "--dry-run", "--json"][..],
    ] {
        let out = tollgate(home.path()).args(args).output().unwrap();
        let code = out.status.code();
        assert!(
            matches!(code, Some(0 | 3)),
            "{args:?}: {code:?} {:?}",
            text(&out)
        );
        assert_eq!(snapshot(home.path()), before, "{args:?} changed the home");
    }
}

/// Test 58. A real run, a rollback and a retire on a non-interactive stdin
/// without `--yes` refuse before reading anything: exit 2, the one
/// sentence, nothing changed.
#[test]
fn non_tty_without_yes_exits_2_and_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let before = snapshot(home.path());
    for args in [
        &["import", "clauth"][..],
        &["import", "clauth", "--json"][..],
        &["import", "rollback"][..],
        &["import", "retire"][..],
    ] {
        let out = tollgate(home.path()).args(args).output().unwrap();
        let (stdout, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(
            stderr.contains(
                "tollgate import clauth: refusing to change files without --yes on a non-interactive stdin"
            ),
            "{args:?}: {stderr}"
        );
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
    }
    assert_eq!(snapshot(home.path()), before);
}

/// An interrupted journal (M5 stopped at step 3) as a crash leaves it.
fn interrupted_journal(home: &Path) {
    std::fs::write(
        home.join(".tollgate/import-journal.json"),
        r#"{"schema_version":1,"state":"in_progress","tool_version":"0","started_at":"t","updated_at":"t","completed_at":null,"uid":0,"source":"/s","target":"/t","source_dev":0,"options":{},"main":[{"seq":1,"op":"mkdir","secret":false,"status":"done"},{"seq":2,"op":"mkdir","secret":false,"status":"done"},{"seq":3,"op":"mkdir","secret":false,"status":"planned"}],"rollback_from":null}"#,
    )
    .unwrap();
}

/// Test 59. The exit codes of §2.2, off the binary: 2 for a usage error, 3
/// for a refusal before anything changed, 4 for a journal that needs a
/// person, 0 for a committed import and a finished rollback, and 1 for a
/// failure once stores moved, after the automatic reversal. The 0 and 1
/// cases run inside the sandbox (their process scan must not see the
/// operator's own sessions); a machine without bubblewrap checks the rest.
#[test]
fn exit_codes_match_the_table() {
    // 2: a clap error.
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let out = tollgate(home.path())
        .args(["import", "clauth", "--bogus"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    // 3: a refusal at M-1 (dry-run and real run alike).
    std::fs::write(home.path().join(".clauth/mystery.bin"), "?").unwrap();
    for args in [
        &["import", "clauth", "--dry-run"][..],
        &["import", "clauth", "--yes"][..],
    ] {
        let out = tollgate(home.path()).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{args:?}: {:?}", text(&out));
    }
    std::fs::remove_file(home.path().join(".clauth/mystery.bin")).unwrap();
    // 4: an interrupted journal, for the dry-run and the real run.
    interrupted_journal(home.path());
    for args in [
        &["import", "clauth", "--dry-run"][..],
        &["import", "clauth", "--yes"][..],
    ] {
        let out = tollgate(home.path()).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(4), "{args:?}: {:?}", text(&out));
    }
    // 2: `--resume` with nothing to resume.
    std::fs::remove_file(home.path().join(".tollgate/import-journal.json")).unwrap();
    let out = tollgate(home.path())
        .args(["import", "clauth", "--resume"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{:?}", text(&out));

    // 0 and 1 run a real transaction: only inside the sandbox.
    if sandbox(home.path(), true).is_none() {
        eprintln!("skipped the 0/1 cases: bubblewrap is not usable here");
        return;
    }
    // 1: a copy fails once the stores moved (the conversations destination
    // is read-only), so every step is reversed and the journal is aborted.
    let blocked_dst = home.path().join(".tollgate/conversations");
    std::fs::create_dir_all(&blocked_dst).unwrap();
    std::fs::write(blocked_dst.join("keep.json"), "{}").unwrap();
    set_mode(&blocked_dst, 0o500);
    let before = snapshot(home.path());
    let out = sandbox(home.path(), true)
        .unwrap()
        .arg(bin())
        .args(["import", "clauth", "--yes"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    set_mode(&blocked_dst, 0o700);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stderr.contains("every step was reversed"), "{stderr}");
    let journal: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.path().join(".tollgate/import-journal.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(journal["state"], "aborted");
    let carriers = |snap: &BTreeMap<String, String>| {
        snap.iter()
            // Files only: a directory's mtime moves with every rename in it.
            .filter(|(k, v)| k.starts_with(".clauth/profiles/") && v.contains("is_file: true"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        carriers(&snapshot(home.path())),
        carriers(&before),
        "every store is back, inode, mode and bytes"
    );
    std::fs::remove_dir_all(&blocked_dst).unwrap();

    // 0: the import commits, and its rollback finishes.
    let out = sandbox(home.path(), true)
        .unwrap()
        .arg(bin())
        .args(["import", "clauth", "--yes", "--json"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(report["mode"], "run");
    assert_eq!(report["journal"]["state"], "complete");
    assert!(
        report["global_edits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == "G1"),
        "{stdout}"
    );
    assert!(
        stderr.contains("tollgate: imported 2 claude and 0 codex profiles from ~/.clauth; guest mode is off. Next: tollgate import retire"),
        "{stderr}"
    );
    assert!(!stdout.contains("FIXTURE") && !stderr.contains("FIXTURE"));
    let link = std::fs::read_link(home.path().join(".claude/.credentials.json")).unwrap();
    assert!(
        link.starts_with(home.path().join(".tollgate/profiles/personal")),
        "{}",
        link.display()
    );
    let out = sandbox(home.path(), true)
        .unwrap()
        .arg(bin())
        .args(["import", "rollback", "--yes"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", text(&out));
    let link = std::fs::read_link(home.path().join(".claude/.credentials.json")).unwrap();
    assert_eq!(
        link,
        home.path()
            .join(".clauth/profiles/personal/credentials.json")
    );
    let out = tollgate(home.path())
        .args(["import", "status", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "rolled_back");
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Test 61. While a journal is interrupted, every command names it on
/// stderr (spec §2.3), and shell completion's hidden helper stays quiet.
#[test]
fn an_interrupted_journal_warns_on_every_command() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    interrupted_journal(home.path());
    let line = "tollgate: an import of clauth was interrupted at step 3; run 'tollgate import clauth --resume' or 'tollgate import rollback'";
    for args in [
        &["import", "status"][..],
        &["import", "status", "--json"][..],
        &["import", "clauth", "--dry-run"][..],
        &["list"][..],
        &["which"][..],
        &["completions", "bash"][..],
        &["status", "--json"][..],
    ] {
        let out = tollgate(home.path()).args(args).output().unwrap();
        let (_, stderr) = text(&out);
        assert!(stderr.contains(line), "{args:?}: {stderr}");
    }
    let out = tollgate(home.path()).args(["__complete"]).output().unwrap();
    let (_, stderr) = text(&out);
    assert!(!stderr.contains("interrupted"), "{stderr}");
    // Review lens guest-ux #10: the key helper's stderr lands in Claude Code.
    let out = tollgate(home.path())
        .args(["__tollgate-api-key", "nobody"])
        .output()
        .unwrap();
    let (_, stderr) = text(&out);
    assert!(!stderr.contains("interrupted"), "{stderr}");
}

/// A blocked dry-run exits 3 and prints each blocker once, in the report.
#[test]
fn a_blocked_dry_run_exits_3() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    std::fs::write(home.path().join(".clauth/mystery.bin"), "?").unwrap();
    let out = tollgate(home.path())
        .args(["import", "clauth", "--dry-run"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(3), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(
            "  blocked  unknown_entry: ~/.clauth/mystery.bin is not in the import inventory"
        ),
        "{stdout}"
    );
    assert!(!stderr.contains("unknown_entry"), "printed twice: {stderr}");
}

/// `import status` on a home that never imported reads `none` and takes no
/// lock (it creates nothing).
#[test]
fn import_status_reads_none_and_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let before = snapshot(home.path());
    let out = tollgate(home.path())
        .args(["import", "status", "--json"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["state"], "none");
    assert_eq!(v["next"], "tollgate import clauth --dry-run");
    assert_eq!(snapshot(home.path()), before);
}
