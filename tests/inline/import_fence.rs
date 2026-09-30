//! `import clauth` part 1: the fence, the process and marker scan, and the
//! rechecks around the prompt (spec §4.2, §4.3; tests 14–21, 20a–20d, 23).

use std::time::{Duration, Instant};

use super::fence::{self, Fence};
use super::test_common::*;

fn names() -> Vec<String> {
    ["leadtone", "personal", "scifoo"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Test 14. Each of the fence's nine items, held by another holder, makes
/// the fence refuse within three seconds naming that file, release what it
/// took, and change nothing but the lock files it may create (I17).
#[test]
fn each_fence_lock_held_elsewhere_refuses_within_three_seconds_and_changes_nothing() {
    let env = Env::new();
    env.tree.reference();
    let paths = env.paths();
    let mut items = fence::item_paths(&paths, &names());
    items.push(fence::state_lock_path(&paths));
    assert_eq!(
        items.len(),
        6 + 3 + 1 + 1,
        "items 1–6, three rotation locks, 8 and 9"
    );
    for item in items {
        let before = env.snapshot().without(is_bookkeeping);
        let holder = LockHolder::hold(&item);
        let started = Instant::now();
        let refused = Fence::acquire(&paths, &names())
            .err()
            .unwrap_or_else(|| panic!("fence took {} while it was held", item.display()));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{}",
            item.display()
        );
        assert_eq!(refused.code, "lock_held");
        assert_eq!(refused.path.as_deref(), Some(paths.tilde(&item).as_str()));
        drop(holder);
        assert_eq!(
            env.snapshot().without(is_bookkeeping),
            before,
            "{}",
            item.display()
        );
        // Everything it took was released: a fresh acquire succeeds.
        drop(Fence::acquire(&paths, &names()).expect("free again"));
    }
}

/// Test 15. A parked upstream standby holds `clauthd-standby.lock`: the
/// dry-run blocks on it and the fence refuses it.
#[test]
fn a_parked_upstream_standby_refuses() {
    let env = Env::new();
    env.tree.reference();
    let _standby = LockHolder::hold(&env.p(".clauth/clauthd-standby.lock"));
    let s = survey(&Options::default());
    let b = s
        .blockers
        .iter()
        .find(|b| b.code == "lock_held")
        .expect("blocked");
    assert_eq!(b.path.as_deref(), Some("~/.clauth/clauthd-standby.lock"));
    assert!(Fence::acquire(&env.paths(), &names()).is_err());
}

/// Test 16. A Claude Code session (native or node-hosted) and every
/// upstream clauth process — its MCP server included — refuse, each named.
#[test]
fn claude_node_claude_and_clauth_mcp_processes_refuse_and_are_named() {
    let env = Env::new();
    env.tree.reference();
    env.procs(vec![
        proc_row(101, &["claude"]),
        proc_row(
            102,
            &[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            ],
        ),
        proc_row(103, &["/home/u/.cargo/bin/clauth", "mcp"]),
        proc_row(104, &["clauth-0.16.0.retired", "daemon"]),
        proc_row(105, &["bash"]),
    ]);
    let s = survey(&Options::default());
    let msgs: Vec<_> = s
        .blockers
        .iter()
        .filter(|b| b.code == "process_alive")
        .map(|b| b.message.clone())
        .collect();
    let tail = "is running; close every Claude Code session and every clauth process first";
    assert_eq!(
        msgs,
        [
            format!("claude (pid 101) {tail}"),
            format!("claude (pid 102) {tail}"),
            format!("clauth (pid 103) {tail}"),
            format!("clauth-0.16.0.retired (pid 104) {tail}"),
        ]
    );
    assert_eq!(s.procs.len(), 4, "bash is not listed");
}

/// Test 17. A codex process only warns while no codex chain is in scope,
/// and blocks once one is.
#[test]
fn a_codex_process_blocks_only_when_codex_carriers_are_in_scope() {
    let env = Env::new();
    env.tree.reference();
    env.procs(vec![proc_row(201, &["codex"])]);
    let s = survey(&Options::default());
    assert!(
        !codes(&s).contains(&"process_alive".to_string()),
        "{:?}",
        s.blockers
    );
    assert!(warning_codes(&s).contains(&"codex_process".to_string()));
    env.tree.codex_roster(&["cx"], None).codex("cx");
    let s = survey(&Options::default());
    assert!(
        codes(&s).contains(&"process_alive".to_string()),
        "{:?}",
        s.blockers
    );
}

/// Test 18. Any other tollgate process — the daemon, an MCP server, a TUI —
/// refuses, named with its role.
#[test]
fn another_tollgate_process_refuses() {
    let env = Env::new();
    env.tree.reference();
    env.procs(vec![
        proc_row(301, &["tollgate", "daemon"]),
        proc_row(302, &["/usr/local/bin/tollgate", "mcp"]),
        proc_row(303, &["tollgate"]),
        proc_row(304, &["tollgate", "api", "serve"]),
    ]);
    let s = survey(&Options::default());
    let msgs: Vec<_> = s
        .blockers
        .iter()
        .filter(|b| b.code == "tollgate_process_alive")
        .map(|b| b.message.clone())
        .collect();
    assert_eq!(
        msgs,
        [
            "another tollgate process (pid 301, daemon) is running; stop it first",
            "another tollgate process (pid 302, mcp) is running; stop it first",
            "another tollgate process (pid 303, tui) is running; stop it first",
            "another tollgate process (pid 304, api) is running; stop it first",
        ]
    );
}

/// Test 20a. A `tollgate start` supervisor — a Hermes session included —
/// blocks the import (spec §4.3, the Hermes spec §6.2).
#[test]
fn a_tollgate_hermes_supervisor_blocks() {
    let env = Env::new();
    env.tree.reference();
    env.procs(vec![proc_row(401, &["tollgate", "start", "hermes-work"])]);
    let s = survey(&Options::default());
    let b = s
        .blockers
        .iter()
        .find(|b| b.code == "tollgate_process_alive")
        .expect("blocked");
    assert_eq!(
        b.message,
        "another tollgate process (pid 401, start) is running; stop it first"
    );
    assert_eq!(b.pid, Some(401));
}

/// Test 19. A live-session row whose pid lives refuses, upstream's and
/// tollgate's; a stale row only warns.
#[test]
fn a_live_session_row_with_a_live_pid_refuses_and_a_stale_one_warns() {
    let env = Env::new();
    env.tree.reference();
    let me = std::process::id();
    let gone = 4_000_000_000_u32;
    env.tree
        .write("live_sessions/up-live.json", &format!("{{\"pid\":{me}}}"));
    env.tree.write(
        "live_sessions/up-stale.json",
        &format!("{{\"pid\":{gone}}}"),
    );
    std::fs::create_dir_all(env.p(".tollgate/live_sessions")).expect("dir");
    std::fs::write(
        env.p(".tollgate/live_sessions/s-live.json"),
        format!("{{\"pid\":{me},\"start_profile\":\"guest\"}}"),
    )
    .expect("row");
    std::fs::write(
        env.p(".tollgate/live_sessions/s-stale.json"),
        format!("{{\"pid\":{gone},\"start_profile\":\"guest\"}}"),
    )
    .expect("row");
    let s = survey(&Options::default());
    let c = codes(&s);
    assert!(c.contains(&"live_session_row".to_string()), "{c:?}");
    let t = s
        .blockers
        .iter()
        .find(|b| b.code == "tollgate_live_session")
        .expect("tollgate row");
    assert_eq!(
        t.message,
        "a tollgate session (s-live, profile 'guest') is live; exit it first (Hermes sessions included)"
    );
    let w = warning_codes(&s);
    assert!(w.contains(&"stale_session_row".to_string()), "{w:?}");
    assert!(
        w.contains(&"stale_tollgate_session_row".to_string()),
        "{w:?}"
    );
    assert_eq!(
        c.iter().filter(|x| x.contains("session")).count(),
        2,
        "{c:?}"
    );
}

/// Test 20. A held session marker refuses: upstream's bare and per-profile
/// markers, and every tollgate `sessions-*` marker, a Hermes one included.
#[test]
fn a_held_session_marker_refuses() {
    let env = Env::new();
    env.tree.reference();
    let bare = env.tree.write("live_bare/m1", "");
    let upstream_session = env.tree.write("profiles/personal/sessions-abc/pid", "");
    let hermes = env.p(".tollgate/profiles/hermes-work/sessions-hermes-7/marker");
    std::fs::create_dir_all(hermes.parent().expect("dir")).expect("mkdir");
    std::fs::write(&hermes, "").expect("marker");
    let free = env.tree.write("mcp_live/free", "");
    let s = survey(&Options::default());
    assert!(
        !codes(&s).iter().any(|c| c.contains("marker")),
        "unheld markers are fine"
    );
    let _a = LockHolder::hold(&bare);
    let _b = LockHolder::hold(&upstream_session);
    let _c = LockHolder::hold(&hermes);
    let s = survey(&Options::default());
    let held: Vec<_> = s
        .blockers
        .iter()
        .filter(|b| b.code == "session_marker_held")
        .map(|b| b.path.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        held,
        [
            "~/.clauth/live_bare/m1",
            "~/.clauth/profiles/personal/sessions-abc/pid"
        ]
    );
    let t = s
        .blockers
        .iter()
        .find(|b| b.code == "tollgate_live_session")
        .expect("hermes marker");
    assert_eq!(
        t.message,
        "a tollgate session (marker, profile 'hermes-work') is live; exit it first (Hermes sessions included)"
    );
    drop(free);
}

/// Test 20b. The read-only tollgate subcommands the herdr scripts and status
/// bars run only warn — at M1 and in a per-carrier rescan — and the import
/// still commits.
#[test]
fn exempt_read_only_tollgate_runs_warn_and_never_abort() {
    let env = Env::new();
    env.tree.reference();
    let exempt = vec![
        proc_row(501, &["tollgate", "herdr", "tag", "--pane", "x"]),
        proc_row(502, &["tollgate", "usage", "--waybar"]),
        proc_row(503, &["tollgate", "__complete", "sw"]),
        proc_row(504, &["tollgate", "which"]),
        proc_row(505, &["tollgate", "status", "--json"]),
        proc_row(506, &["tollgate", "list"]),
        proc_row(507, &["tollgate", "import", "status"]),
    ];
    env.procs(exempt.clone());
    let s = survey(&Options::default());
    assert!(s.blockers.is_empty(), "{:?}", s.blockers);
    assert_eq!(
        warning_codes(&s)
            .iter()
            .filter(|c| *c == "tollgate_readonly_run")
            .count(),
        7
    );
    // The same runs stay alive at M1 and at every per-carrier rescan.
    env.seams.set(|s| {
        s.before_step = Some(Box::new(move |_| {
            let table = exempt.clone();
            seams::with(|st| st.procs = table);
        }));
    });
    let committed = run(&Options::default()).expect("the import commits through exempt runs");
    assert!(
        committed
            .warnings
            .iter()
            .any(|w| w.code == "tollgate_readonly_run"),
        "{:?}",
        committed.warnings
    );
    assert_eq!(env.journal().state, "complete");
}

/// Test 20c. A fence file held at M-1 is a blocker there: the run refuses
/// with exit 3 and changes nothing.
#[test]
fn a_held_fence_file_at_m_minus_1_is_a_blocker() {
    let env = Env::new();
    env.tree.reference();
    for rel in [
        ".clauth/clauthd.lock",
        ".tollgate/tollgated.lock",
        ".clauth/.lock",
    ] {
        let before = env.snapshot();
        let holder = LockHolder::hold(&env.p(rel));
        let mid = env.snapshot();
        let e = run(&Options::default()).expect_err("refused");
        assert_eq!(blocked_codes(&e), ["lock_held"], "{rel}");
        assert_eq!(exit_of(Err(e)), 3);
        assert_eq!(env.snapshot(), mid, "{rel}: the refusal changed the tree");
        drop(holder);
        let _ = before;
    }
}

/// Test 20d. A Claude Code session started while the prompt was open is
/// caught by the post-confirmation recheck: exit 3, nothing written.
#[test]
fn the_post_confirmation_recheck_refuses_a_session_started_during_the_prompt() {
    let env = Env::new();
    env.tree.reference();
    let before = env.snapshot();
    env.seams.set(|s| {
        s.after_confirm = Some(Box::new(|| {
            seams::with(|st| st.procs = vec![FakeProc::new(601, &["claude"])]);
        }));
    });
    let e = run(&Options::default()).expect_err("recheck refuses");
    assert_eq!(blocked_codes(&e), ["process_alive"]);
    assert_eq!(exit_of(Err(e)), 3);
    assert_eq!(
        env.snapshot(),
        before,
        "nothing may be written before the fence"
    );
}

/// Test 21. The whole run enters `ImportFence` once, then `State` for item
/// 9, and no rank below `State` after it — never `Rotation`, `Config` or
/// `ProfileTtl` (spec §4.2).
#[cfg(debug_assertions)]
#[test]
fn the_fence_holds_rank_import_fence_and_enters_no_rank_below_state_after_item_9() {
    use crate::lockorder::{rank, record_entries, value_of};
    let env = Env::new();
    env.tree.reference();
    env.with_upstream_bin();
    let (result, entered) = record_entries(|| run(&Options::default()));
    result.expect("commits");
    let fence = value_of::<rank::ImportFence>();
    let state = value_of::<rank::State>();
    assert_eq!(
        entered.iter().filter(|r| **r == fence).count(),
        1,
        "{entered:?}"
    );
    let at = entered
        .iter()
        .position(|r| *r == state)
        .expect("state entered");
    assert!(
        entered.iter().position(|r| *r == fence) < Some(at),
        "{entered:?}"
    );
    assert!(
        entered[at..].iter().all(|r| *r >= state),
        "a rank below State after item 9: {entered:?}"
    );
    for forbidden in [
        value_of::<rank::Rotation>(),
        value_of::<rank::Config>(),
        value_of::<rank::ProfileTtl>(),
    ] {
        assert!(
            !entered.contains(&forbidden),
            "rank {forbidden} entered: {entered:?}"
        );
    }
}

/// Test 23. While the transaction is paused mid-M5, upstream's state lock,
/// its daemon lock, tollgate's daemon and state locks are all taken, and a
/// tollgate refresher reads upstream as active.
#[test]
fn a_paused_transaction_blocks_upstream_state_and_daemon_lock_takers() {
    let env = Env::new();
    env.tree.reference();
    let (paused_tx, paused_rx) = std::sync::mpsc::channel::<u64>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = std::sync::Mutex::new(go_rx);
    env.seams.set(|s| {
        s.before_step = Some(Box::new(move |seq| {
            if seq == 3 {
                let _ = paused_tx.send(seq);
                let _ = go_rx.lock().map(|rx| rx.recv());
            }
        }));
    });
    let worker = std::thread::spawn(|| run(&Options::default()).map(|_| ()));
    let seq = paused_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("paused");
    assert_eq!(seq, 3);
    let busy = |rel: &str| {
        let f = std::fs::File::open(env.p(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock))
    };
    for rel in [
        ".clauth/.lock",
        ".clauth/clauthd.lock",
        ".clauth/usage-fetch.lock",
        ".clauth/rotation-locks/personal.lock",
        ".tollgate/tollgated.lock",
        ".tollgate/usage-fetch.lock",
        ".tollgate/.lock",
    ] {
        assert!(busy(rel), "{rel} is not held while paused");
    }
    assert!(crate::daemon::upstream_refresher_active());
    go_tx.send(()).expect("resume");
    worker.join().expect("worker").expect("commits");
    assert!(!busy(".clauth/.lock"), "released after commit");
    assert!(!crate::daemon::upstream_refresher_active());
}

/// The `/proc` reader behind test 16: numeric entries of this uid, argv
/// from `cmdline` (NUL-split), the `exe` link, the caller's own pid skipped,
/// and nothing else read (a stub `environ` holding a token never matters).
#[test]
fn the_proc_scan_reads_cmdline_and_exe_only_and_skips_itself() {
    let env = Env::new();
    let root = env.p("proc");
    for (pid, argv, exe) in [
        (
            "100",
            &b"claude\0--resume\0x\0"[..],
            Some("/opt/claude/bin/claude"),
        ),
        ("200", b"/usr/bin/bash\0", None),
        ("300", b"tollgate\0mcp\0", None),
    ] {
        let dir = root.join(pid);
        std::fs::create_dir_all(&dir).expect("pid dir");
        std::fs::write(dir.join("cmdline"), argv).expect("cmdline");
        std::fs::write(dir.join("environ"), "TOKEN=FIXTURE-RT-env").expect("environ");
        if let Some(exe) = exe {
            std::os::unix::fs::symlink(exe, dir.join("exe")).expect("exe");
        }
    }
    std::fs::create_dir_all(root.join("self")).expect("self");
    let uid = super::fsops::current_uid(env.h());
    let seen = super::procs::scan_root(&root, 300, uid);
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].argv, ["claude", "--resume", "x"]);
    assert_eq!(
        seen[0].exe.as_deref(),
        Some(std::path::Path::new("/opt/claude/bin/claude"))
    );
    assert_eq!(seen[1].pid, 200);
    assert!(
        super::procs::scan_root(&root, 1, uid + 1).is_empty(),
        "another uid's processes are not ours"
    );
}

/// A process is upstream clauth by its `exe` as well as its name: a
/// renamed wrapper whose exe is the upstream binary blocks.
#[test]
fn a_process_whose_exe_is_the_upstream_binary_is_clauth() {
    use super::procs::{Role, Scope, classify};
    let bin = std::path::PathBuf::from("/home/u/.cargo/bin/clauth");
    let scope = Scope {
        codex_in_scope: false,
        upstream_bins: vec![bin.clone()],
        self_exe: None,
    };
    let mut p = FakeProc::new(9, &["cl", "herdr", "tag"]);
    assert_eq!(classify(&p, &scope), None);
    p.exe = Some(bin);
    assert_eq!(classify(&p, &scope).map(|(r, _)| r), Some(Role::Clauth));
    let bun = FakeProc::new(10, &["bun", "/x/node_modules/@openai/codex/bin/codex.js"]);
    assert_eq!(classify(&bun, &scope).map(|(r, _)| r), Some(Role::Codex));
}

/// Review lens concurrency #9. A presence probe that takes a fence item's
/// flock for an instant (as `daemon_health` does at 1 Hz) does not refuse
/// the import: the try-locked items are retried before `lock_held`.
#[test]
fn a_momentary_probe_on_a_fence_item_does_not_refuse_the_import() {
    let env = Env::new();
    env.tree.reference();
    let paths = env.paths();
    let item = fence::item_paths(&paths, &names())[0].clone();
    let holder = LockHolder::hold(&item);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        drop(holder);
    });
    let fence = Fence::acquire(&paths, &names());
    release.join().expect("release");
    assert!(fence.is_ok(), "a probe held for 120 ms must not refuse");
}
