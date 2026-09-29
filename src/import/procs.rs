//! Process and marker revalidation (spec §4.3, the plan's M1).
//!
//! A Linux `/proc` scan of this uid's processes, excluding this one, reading
//! each `cmdline` and `exe` link and NOTHING else (never `environ`). Every
//! Claude Code session and every upstream clauth process blocks; a codex
//! process blocks only while a codex carrier is in scope; every other
//! tollgate process blocks except the short-lived read-only subcommands the
//! herdr scripts and status bars run, which only warn. Session markers and
//! live-session rows, upstream's and tollgate's, block while their holder
//! lives.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Finding, Paths, fsops, seams};

/// One process as the scan saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FakeProc {
    pub(crate) pid: u32,
    pub(crate) argv: Vec<String>,
    pub(crate) exe: Option<PathBuf>,
}

impl FakeProc {
    /// A test's process row: `pid` running `argv`, no exe link.
    #[cfg(test)]
    pub(crate) fn new(pid: u32, argv: &[&str]) -> Self {
        Self {
            pid,
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            exe: None,
        }
    }
}

/// The role a process plays for the import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Claude,
    Codex,
    Clauth,
    Tollgate,
}

/// A report row: `{pid, name, role}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProcRow {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) role: Role,
}

/// What decides which roles block.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    /// A codex carrier or a codex live-slot case (i)/(ii) is in scope.
    pub(crate) codex_in_scope: bool,
    /// Upstream binaries by canonical path (an `exe` equal to one is clauth).
    pub(crate) upstream_bins: Vec<PathBuf>,
    /// This binary (an `exe` equal to it is tollgate).
    pub(crate) self_exe: Option<PathBuf>,
}

/// The scan's verdict.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scan {
    pub(crate) blockers: Vec<Finding>,
    pub(crate) warnings: Vec<Finding>,
    pub(crate) rows: Vec<ProcRow>,
}

/// The processes of this uid other than this one: the test table when a
/// test posed one, else `/proc`.
pub(crate) fn processes(home: &Path) -> Vec<FakeProc> {
    if let Some(table) = seams::fake_procs() {
        return table;
    }
    scan_root(
        Path::new("/proc"),
        std::process::id(),
        fsops::current_uid(home),
    )
}

/// Scan a `/proc`-shaped tree at `root`: every numeric entry owned by `uid`,
/// other than `self_pid`, with its argv and exe link. Unreadable entries (a
/// process that exited mid-scan, a kernel thread) are skipped.
pub(crate) fn scan_root(root: &Path, self_pid: u32, uid: u32) -> Vec<FakeProc> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let dir = entry.path();
        if fsops::lmeta(&dir).is_none_or(|m| m.uid != uid) {
            continue;
        }
        let Ok(raw) = std::fs::read(dir.join("cmdline")) else {
            continue;
        };
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if argv.is_empty() {
            continue;
        }
        let exe = std::fs::read_link(dir.join("exe"))
            .ok()
            .map(|p| crate::platform::installed_exe_path(&p));
        out.push(FakeProc { pid, argv, exe });
    }
    out.sort_by_key(|p| p.pid);
    out
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// The role of `p`, with the argv index its subcommand starts at, or `None`
/// for a process the import does not care about.
pub(crate) fn classify(p: &FakeProc, scope: &Scope) -> Option<(Role, usize)> {
    let argv0 = p.argv.first()?;
    let (prog, rest) = if matches!(basename(argv0), "node" | "bun" | "deno") {
        (p.argv.get(1).map_or("", String::as_str), 2)
    } else {
        (argv0.as_str(), 1)
    };
    let base = basename(prog);
    let exe_is = |bins: &[PathBuf]| {
        p.exe
            .as_ref()
            .is_some_and(|exe| bins.iter().any(|b| b == exe))
    };
    if base == "claude" || prog.contains("/claude-code/") {
        return Some((Role::Claude, rest));
    }
    if base == "codex" || prog.contains("/@openai/codex/") {
        return Some((Role::Codex, rest));
    }
    if base == crate::identity::UPSTREAM_NAME
        || (base.starts_with("clauth-") && base.ends_with(".retired"))
        || exe_is(&scope.upstream_bins)
    {
        return Some((Role::Clauth, rest));
    }
    let self_exe: Vec<PathBuf> = scope.self_exe.iter().cloned().collect();
    if base == crate::identity::NAME || exe_is(&self_exe) {
        return Some((Role::Tollgate, rest));
    }
    None
}

/// Whether a tollgate process's arguments are one of the read-only
/// subcommands that take no lock the fence does not already hold.
pub(crate) fn is_exempt(args: &[String]) -> bool {
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    matches!(
        words.as_slice(),
        ["herdr", "tag", ..]
            | ["usage", ..]
            | ["__complete", ..]
            | ["which", ..]
            | ["status", ..]
            | ["list", ..]
            | ["import", "status", ..]
    )
}

/// The M1 process scan. When the only matches are exempt read-only tollgate
/// runs it rescans (up to three more times) so they can exit, and then
/// warns: it never aborts on them.
pub(crate) fn check(paths: &Paths, scope: &Scope) -> Scan {
    let mut attempt = 0;
    loop {
        let scan = scan_once(paths, scope);
        let exempt_only = scan.blockers.is_empty()
            && scan
                .warnings
                .iter()
                .any(|w| w.code == "tollgate_readonly_run");
        if exempt_only && attempt < 3 {
            attempt += 1;
            std::thread::sleep(seams::exempt_retry_delay());
            continue;
        }
        return scan;
    }
}

fn scan_once(paths: &Paths, scope: &Scope) -> Scan {
    let mut scan = Scan::default();
    for p in processes(&paths.home) {
        let Some((role, rest)) = classify(&p, scope) else {
            continue;
        };
        let name = p
            .argv
            .get(rest.saturating_sub(1))
            .map(|s| basename(s))
            .filter(|b| !b.is_empty() && !is_script(b))
            .map_or_else(|| role_label(role).to_string(), str::to_string);
        let args = p.argv.get(rest..).unwrap_or(&[]);
        scan.rows.push(ProcRow {
            pid: p.pid,
            name: name.clone(),
            role,
        });
        match role {
            Role::Claude | Role::Clauth => scan.blockers.push(process_alive(&name, p.pid)),
            Role::Codex if scope.codex_in_scope => {
                scan.blockers.push(process_alive(&name, p.pid));
            }
            Role::Codex => scan.warnings.push(
                Finding::new(
                    "codex_process",
                    format!(
                        "{name} (pid {}) is running; no codex store is imported, so it may stay",
                        p.pid
                    ),
                )
                .with_pid(p.pid),
            ),
            Role::Tollgate if is_exempt(args) => scan.warnings.push(
                Finding::new(
                    "tollgate_readonly_run",
                    format!(
                        "a read-only tollgate run (pid {}, {}) is alive; it waits on the fence and exits",
                        p.pid,
                        // The subcommand words only: an argument's value
                        // never reaches a report.
                        exempt_words(args)
                    ),
                )
                .with_pid(p.pid),
            ),
            Role::Tollgate => {
                let role_label = args
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_else(|| "tui".to_string());
                scan.blockers.push(
                    Finding::new(
                        "tollgate_process_alive",
                        format!(
                            "another tollgate process (pid {}, {role_label}) is running; stop it first",
                            p.pid
                        ),
                    )
                    .with_pid(p.pid),
                );
            }
        }
    }
    scan
}

/// An exempt run's subcommand words (`herdr tag`, `import status`, `usage`),
/// never its other arguments.
fn exempt_words(args: &[String]) -> String {
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let n = match words.first() {
        Some(&"herdr" | &"import") => 2,
        _ => 1,
    };
    words.into_iter().take(n).collect::<Vec<_>>().join(" ")
}

/// A script file a runtime hosts (`cli.js`): the process is named for its
/// role instead.
fn is_script(base: &str) -> bool {
    [".js", ".mjs", ".cjs", ".ts"]
        .iter()
        .any(|ext| base.ends_with(ext))
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::Claude => "claude",
        Role::Codex => "codex",
        Role::Clauth => "clauth",
        Role::Tollgate => "tollgate",
    }
}

fn process_alive(name: &str, pid: u32) -> Finding {
    Finding::new(
        "process_alive",
        format!(
            "{name} (pid {pid}) is running; close every Claude Code session and every clauth process first"
        ),
    )
    .with_pid(pid)
}

/// Whether `pid` names a running process (`/proc/<pid>` exists).
pub(crate) fn pid_alive(pid: u32) -> bool {
    pid != 0 && Path::new("/proc").join(pid.to_string()).exists()
}

/// Whether another process holds `path`'s flock: opened read-only (never
/// created), probed with a shared try-lock, released at once.
pub(crate) fn held(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    matches!(
        file.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    )
}

fn files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| fsops::lmeta(p).is_some_and(|m| m.is_file))
        .collect();
    out.sort();
    out
}

fn dirs_named(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
        .filter(|p| fsops::lmeta(p).is_some_and(|m| m.is_dir))
        .collect();
    out.sort();
    out
}

fn row_pid(path: &Path) -> Option<(u32, Option<String>)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let pid = u32::try_from(v.get("pid")?.as_u64()?).ok()?;
    let profile = v
        .get("start_profile")
        .and_then(|p| p.as_str())
        .map(str::to_string);
    Some((pid, profile))
}

/// Session markers and live-session rows, upstream's and tollgate's.
pub(crate) fn markers(paths: &Paths) -> (Vec<Finding>, Vec<Finding>) {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();
    // Upstream: bare and MCP markers, and every profile's sessions dirs.
    let mut upstream_markers = Vec::new();
    for dir in ["live_bare", "mcp_live"] {
        upstream_markers.extend(files_in(&paths.source.join(dir)));
    }
    for profile in dirs_named(&paths.source.join("profiles"), "") {
        for sessions in dirs_named(&profile, "sessions") {
            upstream_markers.extend(files_in(&sessions));
        }
    }
    for marker in upstream_markers {
        if held(&marker) {
            let shown = paths.tilde(&marker);
            blockers.push(
                Finding::new(
                    "session_marker_held",
                    format!(
                        "{shown} is held by a live clauth session; close every Claude Code session and every clauth process first"
                    ),
                )
                .with_path(shown),
            );
        }
    }
    for row in files_in(&paths.source.join("live_sessions")) {
        let Some((pid, _)) = row_pid(&row) else {
            continue;
        };
        let shown = paths.tilde(&row);
        if pid_alive(pid) {
            blockers.push(
                Finding::new(
                    "live_session_row",
                    format!("{shown} names a live clauth session (pid {pid}); exit it first"),
                )
                .with_pid(pid)
                .with_path(shown),
            );
        } else {
            warnings.push(
                Finding::new(
                    "stale_session_row",
                    format!("{shown} names pid {pid}, which is gone; it is not imported"),
                )
                .with_pid(pid)
                .with_path(shown),
            );
        }
    }
    // tollgate: live-session rows and held session markers, every harness.
    for row in files_in(&paths.target.join("live_sessions")) {
        let Some((pid, profile)) = row_pid(&row) else {
            continue;
        };
        let sid = row
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let profile = profile.unwrap_or_default();
        if pid_alive(pid) {
            blockers.push(tollgate_live(&sid, &profile).with_pid(pid));
        } else {
            warnings.push(
                Finding::new(
                    "stale_tollgate_session_row",
                    format!("{} names pid {pid}, which is gone", paths.tilde(&row)),
                )
                .with_pid(pid),
            );
        }
    }
    for profile in dirs_named(&paths.target.join("profiles"), "") {
        let name = profile
            .file_name()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        for sessions in dirs_named(&profile, "sessions") {
            for marker in files_in(&sessions) {
                if held(&marker) {
                    let sid = marker
                        .file_name()
                        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                    blockers.push(tollgate_live(&sid, &name).with_path(paths.tilde(&marker)));
                }
            }
        }
    }
    (blockers, warnings)
}

fn tollgate_live(sid: &str, profile: &str) -> Finding {
    Finding::new(
        "tollgate_live_session",
        format!(
            "a tollgate session ({sid}, profile '{profile}') is live; exit it first (Hermes sessions included)"
        ),
    )
}
