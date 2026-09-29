//! `tollgate import` against the real binary (spec import-clauth.md §7,
//! part 1): the dry-run is provably read-only, the part-1 CLI refuses a real
//! run and a rollback with exit 2, and the status read works on a fresh home.
//!
//! Every run points `HOME` at a tempdir fixture and pins `PATH` to the
//! built binary's own directory plus the system dirs, so no real home, no
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

fn stub_path() -> String {
    format!("{}:/usr/bin:/bin", bin_dir().display())
}

/// `tollgate` with `HOME` at `home`, a stub `PATH`, and nothing inherited
/// that names another config dir.
fn tollgate(home: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home)
        .env("PATH", stub_path())
        .env("TOLLGATE_NO_API", "1")
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
        .arg("--ro-bind")
        .arg(home)
        .arg(home)
        .arg("--setenv")
        .arg("HOME")
        .arg(home)
        .args(["--setenv", "PATH"])
        .arg(stub_path())
        .args(["--setenv", "TOLLGATE_NO_API", "1"])
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

/// Part 1 has no real run and no rollback: both answer the one sentence and
/// exit 2, changing nothing.
#[test]
fn a_real_run_and_a_rollback_exit_2_in_this_build() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let before = snapshot(home.path());
    for args in [
        &["import", "clauth"][..],
        &["import", "clauth", "--yes"][..],
        &["import", "rollback", "--yes"][..],
    ] {
        let out = tollgate(home.path()).args(args).output().unwrap();
        let (_, stderr) = text(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(
            stderr.contains("tollgate import clauth: only --dry-run is available in this build"),
            "{stderr}"
        );
    }
    assert_eq!(snapshot(home.path()), before);
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
