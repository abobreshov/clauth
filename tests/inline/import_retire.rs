//! `import clauth` part 2: the journal status read, the retire checklist,
//! the guest store after an import, and the full reference round trip
//! (spec §4.10, §4.11, §6; tests 60, 62, 63, 65, 67).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::retire::{self, Step};
use super::test_common::*;
use crate::testutil::{ConfigDirSandbox, EnvPin, FakeClaude, FakeHerdr};

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json")
}

/// The shared files an import and its retire edit, as bytes.
fn global_bytes(env: &Env) -> Vec<(String, Option<Vec<u8>>)> {
    [
        ".claude/settings.json",
        ".claude/plugins/known_marketplaces.json",
        ".claude/plugins/installed_plugins.json",
        ".claude.json",
        ".bashrc",
    ]
    .iter()
    .map(|rel| (rel.to_string(), std::fs::read(env.p(rel)).ok()))
    .collect()
}

/// Every global file upstream's wiring lives in.
fn globals(env: &Env) {
    env.tree
        .helper_in_settings("personal")
        .claude_json()
        .known_marketplaces()
        .bashrc()
        .registry_path(&[]);
}

/// Pose the R3 hook: record each `(install, no_config)` call. A herdr must
/// resolve for R3 to run at all, so a stand-in path is pinned.
fn herdr_hook(env: &Env) -> Arc<Mutex<Vec<(bool, bool)>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let stand_in = env.p("no-such-herdr");
    env.seams.set(|s| {
        s.herdr_bin = Some(stand_in);
        s.herdr_plugin = Some(Box::new(move |install, no_config| {
            seen.lock().expect("calls").push((install, no_config));
            Ok(())
        }));
    });
    calls
}

/// Test 60. `import status` reads every journal state without a lock, names
/// the step an interrupted import stopped at, and says what to run next.
#[test]
fn import_status_reports_every_journal_state() {
    let env = Env::new();
    let paths = env.paths();
    let status = super::journal::status(&paths);
    assert_eq!(status.state, "none");
    assert_eq!(status.next, "tollgate import clauth --dry-run");
    for (state, interrupted, next) in [
        ("pre", true, "--resume"),
        ("in_progress", true, "--resume"),
        ("rolling_back", true, "rollback"),
        ("complete", false, "tollgate import retire"),
        ("rolled_back", false, "archives this journal"),
        ("aborted", false, "archives this journal"),
    ] {
        let doc = serde_json::json!({
            "schema_version": 1, "state": state, "tool_version": "0", "started_at": "t",
            "updated_at": "t", "completed_at": null, "uid": 0, "source": "/s", "target": "/t",
            "source_dev": 0, "options": {},
            "pre": [{"seq": 0, "op": "exec", "secret": false, "status": "done"}],
            "main": [
                {"seq": 1, "op": "mkdir", "secret": false, "status": "done"},
                {"seq": 2, "op": "mkdir", "secret": false, "status": "planned"}
            ],
            "retire": [], "rollback_from": null
        });
        std::fs::write(paths.journal(), doc.to_string()).expect("journal");
        let s = super::journal::status(&paths);
        assert_eq!(s.state, state);
        assert!(s.next.contains(next), "{state}: {}", s.next);
        assert_eq!(s.interrupted_at.is_some(), interrupted, "{state}");
        if state == "in_progress" || state == "pre" {
            assert_eq!(s.interrupted_at, Some(2), "{state}");
        }
        if state == "rolling_back" {
            assert_eq!(s.interrupted_at, Some(1), "the last step not yet undone");
        }
        let warning = super::journal::interrupted_warning(&paths);
        assert_eq!(warning.is_some(), interrupted, "{state}");
        let text = super::journal::render_status(&s);
        assert!(
            text.starts_with(&format!("tollgate import: {state}")),
            "{text}"
        );
    }
    std::fs::write(paths.journal(), "{").expect("torn");
    assert_eq!(super::journal::status(&paths).state, "unreadable");
}

/// Test 62. Each retire step journals what it did and reverses: R1's JSON
/// removals come back byte for byte, R2 uninstalls the plugin it installed,
/// R3 uninstalls with the config choice it installed with, R4 puts the
/// completion line (and its comment) back. A second retire is a no-op.
#[test]
fn retire_steps_are_journaled_and_each_reverses() {
    let env = Env::new();
    env.tree.reference();
    globals(&env);
    let claude = env.p(".claude");
    let _config = ConfigDirSandbox::new(&env.home, &claude);
    let fake = FakeClaude::new(&env.home);
    run_ok();
    let calls = herdr_hook(&env);
    let before = global_bytes(&env);
    let done = retire::retire(&[], true).expect("retires");
    assert_eq!(done.lines.len(), 4, "{:?}", done.lines);
    let settings = read_json(&env.p(".claude/settings.json"));
    assert!(settings["enabledPlugins"].get("clauth@clauth").is_none());
    assert!(settings["extraKnownMarketplaces"].get("clauth").is_none());
    assert_eq!(settings["enabledPlugins"]["other@market"], true);
    assert!(
        read_json(&env.p(".claude/plugins/known_marketplaces.json"))
            .get("clauth")
            .is_none()
    );
    let cj = read_json(&env.p(".claude.json"));
    assert!(cj["mcpServers"].get("clauth").is_none());
    assert!(cj["mcpServers"].get("other").is_some());
    assert!(
        fake.log()
            .lines()
            .any(|l| l == "plugin install tollgate@tollgate --scope user"),
        "{}",
        fake.log()
    );
    assert_eq!(*calls.lock().expect("calls"), [(true, true)]);
    let rc = std::fs::read_to_string(env.p(".bashrc")).expect("rc");
    assert!(rc.contains("# tollgate completions\nsource \""), "{rc}");
    assert!(rc.contains(".tollgate/completions/tollgate.bash\""), "{rc}");
    assert!(!rc.contains("clauth"), "{rc}");
    assert!(rc.starts_with("export EDITOR=vi\n") && rc.ends_with("alias ll='ls -l'\n"));
    let j = env.journal();
    for step in ["r1", "r2", "r3", "r4"] {
        assert!(
            j.retire
                .iter()
                .any(|e| e.after.step.as_deref() == Some(step) && e.status == Status::Done),
            "{step} not journaled done"
        );
    }
    assert_eq!(
        j.retire
            .iter()
            .filter(|e| e.after.step.as_deref() == Some("r1"))
            .count(),
        5,
        "one removal per upstream key"
    );
    assert_no_fixture_secret(
        "journal",
        &std::fs::read_to_string(env.paths().journal()).expect("journal"),
    );
    let again = retire::retire(&[], true).expect("idempotent");
    assert!(
        again.lines.iter().all(|l| l.ends_with("already done")),
        "{:?}",
        again.lines
    );

    let mut j = env.journal();
    let mut warnings = Vec::new();
    retire::undo_all(&mut j, &env.paths(), &mut warnings).expect("undoes");
    assert!(j.retire.iter().all(|e| e.status != Status::Done));
    assert!(
        fake.log()
            .lines()
            .any(|l| l.starts_with("plugin uninstall tollgate@tollgate")),
        "{}",
        fake.log()
    );
    assert_eq!(calls.lock().expect("calls").last(), Some(&(false, true)));
    let after = global_bytes(&env);
    for ((rel, want), (_, have)) in before.iter().zip(&after) {
        if rel.ends_with("installed_plugins.json") {
            // The fake `claude plugin install` rewrote the registry from
            // scratch; R1's own removal there is covered by test 63.
            continue;
        }
        assert_eq!(have, want, "{rel} is not restored");
    }
}

/// Test 63. A rollback after retire undoes the retire section first (before
/// its fence is taken), then the import: every global file is back as it
/// was before the import.
#[test]
fn rollback_after_retire_reverses_retire_first() {
    let env = Env::new();
    env.tree.reference();
    globals(&env);
    let before = global_bytes(&env);
    run_ok();
    let _calls = herdr_hook(&env);
    retire::retire(&[Step::R1, Step::R3, Step::R4], true).expect("retires");
    env.seams.set(|s| s.op_log = Some(Vec::new()));
    rollback(&Options::default()).expect("rolls back");
    let log = env.seams.op_log();
    let last_retire_undo = log
        .iter()
        .rposition(|l| l.starts_with("undo retire"))
        .expect("retire undone");
    let fence = log
        .iter()
        .position(|l| l == "fence acquired")
        .expect("fence taken");
    let first_main_undo = log
        .iter()
        .position(|l| l.starts_with("undo ") && !l.starts_with("undo retire"))
        .expect("main undone");
    assert!(
        last_retire_undo < fence && fence < first_main_undo,
        "{log:?}"
    );
    assert_eq!(global_bytes(&env), before);
    assert_eq!(env.journal().state, "rolled_back");
}

/// Test 65. A guest-mode conversation stays listed and resumable after the
/// import turns guest mode off: the M5.2 copy puts it in `~/.claude/projects`,
/// where `sessions` lists it once and `--resume` finds it.
#[test]
fn a_guest_conversation_is_listed_and_resumable_after_import() {
    let env = Env::new();
    env.tree.reference();
    let guest = env.p(".tollgate/guest-claude/projects/-w");
    std::fs::create_dir_all(&guest).expect("guest store");
    std::fs::write(
        guest.join("sess-guest-1.jsonl"),
        "{\"type\":\"user\",\"cwd\":\"/w\",\"sessionId\":\"sess-guest-1\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .expect("transcript");
    let listed = |id: &str| {
        crate::sessions::walk()
            .into_iter()
            .filter(|w| w.id == id)
            .collect::<Vec<_>>()
    };
    assert!(crate::identity::upstream_active());
    assert_eq!(listed("sess-guest-1").len(), 1, "listed in guest mode");
    run_ok();
    assert!(!crate::identity::upstream_active(), "guest mode is off");
    let rows = listed("sess-guest-1");
    assert_eq!(rows.len(), 1, "listed exactly once");
    assert!(
        rows[0].path.starts_with(env.p(".claude/projects")),
        "{}",
        rows[0].path.display()
    );
    let found = crate::sessions::find_session("sess-guest-1").expect("resumable");
    assert!(found.path.starts_with(env.p(".claude/projects")));
    assert_eq!(found.workspace(), Some(PathBuf::from("/w")));
}

/// The part of a home a full import-then-rollback promises back: the
/// import's bookkeeping, the fakes' own state and logs excluded, and the
/// inode ignored on the files rewritten through a temp and a rename (the
/// upstream rosters, `settings.json`, the registry, herdr's config).
fn round_trip_view(snap: &TreeSnapshot) -> TreeSnapshot {
    let mut v = rollback_view(snap).without(|rel| rel.starts_with("fakeherdr") || rel == "g2.log");
    for rel in [
        ".claude/settings.json",
        ".claude/plugins/installed_plugins.json",
        ".config/herdr/config.toml",
    ] {
        if let Some(node) = v.0.get_mut(rel) {
            node.ino = 0;
        }
    }
    v
}

fn g2_env(env: &Env) -> (FakeHerdr, EnvPin<'_>) {
    let config = env.tree.herdr_config();
    let herdr = env.tree.fake_herdr();
    let bin = env.tree.g2_upstream_bin(&config);
    let exe = env.tree.tollgate_bin();
    let hb = herdr.bin.clone();
    env.seams.set(|s| {
        s.herdr_bin = Some(hb);
        s.path_dirs = vec![bin];
        s.current_exe = Some(exe);
    });
    let pin = EnvPin::new(
        &env.home,
        &[("HERDR_CONFIG_PATH", Some(config.as_os_str()))],
    );
    (herdr, pin)
}

/// Test 67. The reference machine with every global file upstream wires,
/// imported (G1–G4 all planned, both slot shapes) and rolled back: every
/// file tollgate restores is byte-identical, `~/.claude`, `~/.claude.json`,
/// `~/.codex`, herdr's `config.toml`, the registry and `.bashrc` included.
/// herdr's own plugin state is the documented residual; the reinstall argv
/// names the recorded commit. A regular-file slot comes back as a link to
/// its restored store (spec I4/I22), so only that one path differs there.
#[test]
fn a_full_reference_import_then_rollback_leaves_every_file_tollgate_restores_byte_identical() {
    for regular in [false, true] {
        let env = Env::new();
        env.tree.reference();
        globals(&env);
        if regular {
            env.tree.live_regular_same("personal");
        }
        let runtime = env
            .tree
            .dir("leadtone")
            .join("runtime-abc/plugins/cache/t/q/2");
        std::fs::create_dir_all(&runtime).expect("runtime");
        std::fs::create_dir_all(env.p(".claude/plugins/cache/t/q/2")).expect("twin");
        env.tree.registry_path(&[&runtime]);
        let (herdr, _pin) = g2_env(&env);
        let label = if regular {
            "regular slot"
        } else {
            "symlink slot"
        };
        let report = txn::report(&survey(&Options::default()), "dry_run");
        let ids: Vec<&str> = report.global_edits.iter().map(|g| g.id.as_str()).collect();
        for id in ["G1", "G2", "G3", "G4"] {
            assert!(ids.contains(&id), "{label}: {id} not planned: {ids:?}");
        }
        let before = round_trip_view(&env.snapshot());
        run_ok();
        assert!(!crate::identity::upstream_active(), "{label}");
        rollback(&Options::default()).expect("rolls back");
        let mut after = round_trip_view(&env.snapshot());
        let mut want = before.clone();
        if regular {
            let slot = ".claude/.credentials.json";
            let node = after.0.remove(slot).expect("slot");
            assert_eq!(node.kind, "symlink", "{label}: {node:?}");
            want.0.remove(slot);
            // The capture made the live inode the store (I4): the store is
            // back byte for byte, on the inode the slot had.
            let store = ".clauth/profiles/personal/credentials.json";
            for view in [&mut after, &mut want] {
                if let Some(node) = view.0.get_mut(store) {
                    node.ino = 0;
                }
            }
        }
        assert_same_tree(&after, &want, label);
        assert!(
            herdr
                .log()
                .lines()
                .any(|l| l == "plugin install uwuclxdy/clauth/herdr-plugin --ref abc123 --yes"),
            "{label}: {}",
            herdr.log()
        );
        assert!(
            crate::identity::upstream_active(),
            "{label}: guest mode is back"
        );
    }
}
