//! `import clauth` part 1: the reverse replay (spec §4.10; tests 45–51).

use super::test_common::*;

fn store(env: &Env, p: &str, f: &str) -> std::path::PathBuf {
    env.p(&format!(".clauth/profiles/{p}/{f}"))
}

fn tstore(env: &Env, p: &str, f: &str) -> std::path::PathBuf {
    env.p(&format!(".tollgate/profiles/{p}/{f}"))
}

fn slot(env: &Env) -> std::path::PathBuf {
    env.p(".claude/.credentials.json")
}

/// Test 45. A full import then rollback puts every carrier back at its
/// path on its own inode, and the whole tree as it was; guest mode is back.
#[test]
fn rollback_restores_carriers_to_their_paths_and_inodes() {
    let env = Env::new();
    env.tree.reference().quarantine("personal").api_key("k");
    env.tree
        .codex_roster(&["cx"], Some("cx"))
        .codex("cx")
        .codex_symlink("cx");
    env.with_upstream_bin();
    let original = rollback_view(&env.snapshot());
    let inodes: Vec<_> = [
        ("personal", "credentials.json"),
        ("personal", "quarantine"),
        ("cx", "auth.json"),
    ]
    .iter()
    .map(|(p, f)| (store(&env, p, f), ino(&store(&env, p, f))))
    .collect();
    run_ok();
    assert!(!crate::identity::upstream_active());
    rollback(&Options::default()).expect("rolls back");
    for (path, i) in inodes {
        assert_eq!(ino(&path), i, "{}", path.display());
    }
    assert_same_tree(&rollback_view(&env.snapshot()), &original, "rollback");
    assert_eq!(env.journal().state, "rolled_back");
    assert!(
        env.journal()
            .main
            .iter()
            .all(|e| e.status == Status::Undone)
    );
    assert!(crate::identity::upstream_active(), "guest mode is back on");
}

/// Test 46. After tollgate rotated a store (a new inode), the rollback moves
/// the CURRENT store back, never the journaled one.
#[test]
fn rollback_after_a_tollgate_rotation_moves_the_current_store_back() {
    let env = Env::new();
    env.tree.reference();
    run_ok();
    let dst = tstore(&env, "personal", "credentials.json");
    let staged = env.p(".tollgate/profiles/personal/.credentials.json.new");
    std::fs::write(&staged, fixture_oauth_body("personal", 2)).expect("rotated");
    std::fs::rename(&staged, &dst).expect("rotate in place");
    let rotated = ino(&dst);
    rollback(&Options::default()).expect("rolls back");
    let back = store(&env, "personal", "credentials.json");
    assert_eq!(ino(&back), rotated);
    assert!(
        std::fs::read_to_string(&back)
            .expect("store")
            .contains(&fixture_refresh("personal", 2))
    );
    assert_eq!(std::fs::read_link(slot(&env)).expect("slot"), back);
}

/// Test 47. A slot tollgate detached into a regular file after the import
/// (its TUI exit, I18) comes back as a link to the restored store — never a
/// copy; a diverged one refuses unless `--adopt-live`.
#[test]
fn rollback_relinks_a_detached_live_slot_as_a_link_never_a_copy() {
    let env = Env::new();
    env.tree.reference();
    run_ok();
    std::fs::remove_file(slot(&env)).expect("unlink");
    std::fs::copy(tstore(&env, "personal", "credentials.json"), slot(&env)).expect("detach");
    rollback(&Options::default()).expect("rolls back");
    let meta = std::fs::symlink_metadata(slot(&env)).expect("slot");
    assert!(meta.file_type().is_symlink(), "the slot is a link again");
    assert_eq!(
        std::fs::read_link(slot(&env)).expect("link"),
        store(&env, "personal", "credentials.json")
    );

    drop(env);
    let env = Env::new();
    env.tree.reference();
    run_ok();
    std::fs::remove_file(slot(&env)).expect("unlink");
    std::fs::write(slot(&env), fixture_oauth_body("personal", 8)).expect("relogin");
    let e = rollback(&Options::default()).expect_err("diverged refuses");
    assert_eq!(blocked_codes(&e), ["live_slot_diverged"]);
    let live = ino(&slot(&env));
    let adopt = Options {
        adopt_live: true,
        ..Options::default()
    };
    rollback(&adopt).expect("adopts");
    let back = store(&env, "personal", "credentials.json");
    assert_eq!(ino(&back), live, "the live login became the store");
    assert_eq!(std::fs::read_link(slot(&env)).expect("link"), back);
}

/// Test 48. A rollback refuses while tollgate, Claude Code or the clauth
/// shim runs, and while codex runs once codex was imported.
#[test]
fn rollback_refuses_while_tollgate_claude_or_codex_lives() {
    let env = Env::new();
    env.tree.reference();
    run_ok();
    for (argv, code) in [
        (&["claude"][..], "process_alive"),
        (&["tollgate", "daemon"][..], "tollgate_process_alive"),
        (&["clauth", "list"][..], "process_alive"),
    ] {
        env.procs(vec![proc_row(801, argv)]);
        let e = rollback(&Options::default()).expect_err("refused");
        assert_eq!(blocked_codes(&e), [code], "{argv:?}");
        assert_eq!(exit_of(Err(e)), 3);
    }
    env.procs(vec![proc_row(802, &["codex"])]);
    rollback(&Options::default()).expect("codex was not imported, so it may run");

    drop(env);
    let env = Env::new();
    env.tree.reference();
    env.tree.codex_roster(&["cx"], None).codex("cx");
    run_ok();
    env.procs(vec![proc_row(803, &["codex"])]);
    let e = rollback(&Options::default()).expect_err("codex imported");
    assert_eq!(blocked_codes(&e), ["process_alive"]);
}

/// Test 49. The stores go home before the upstream binary comes back (the
/// reinstall-order rule); a vanished retired binary leaves the shim and
/// prints the manual step.
#[test]
fn rollback_restores_stores_before_the_upstream_binary() {
    let env = Env::new();
    env.tree.reference();
    let bin = env.with_upstream_bin();
    let original = std::fs::read(&bin).expect("bin");
    run_ok();
    env.seams.set(|s| s.op_log = Some(Vec::new()));
    rollback(&Options::default()).expect("rolls back");
    let log = env.seams.op_log();
    let undo: Vec<_> = log.iter().filter(|l| l.starts_with("undo ")).collect();
    let bin_at = undo
        .iter()
        .position(|l| l.ends_with("RetireBin"))
        .expect("bin undone");
    let last_store = undo
        .iter()
        .rposition(|l| l.ends_with(" Move") || l.ends_with("MoveRelink"))
        .expect("stores");
    assert!(
        last_store < bin_at,
        "a store came back after the binary: {undo:?}"
    );
    assert_eq!(bin_at, undo.len() - 1, "the binary is the last step");
    assert_eq!(std::fs::read(&bin).expect("bin"), original);
    assert!(!env.p("bin/clauth-0.16.0.retired").exists());

    drop(env);
    let env = Env::new();
    env.tree.reference();
    let bin = env.with_upstream_bin();
    run_ok();
    std::fs::remove_file(env.p("bin/clauth-0.16.0.retired")).expect("lose it");
    let rolled = rollback(&Options::default()).expect("rolls back");
    assert_eq!(
        std::fs::read_to_string(&bin).expect("shim"),
        txn::SHIM,
        "the shim stays"
    );
    let w = rolled
        .warnings
        .iter()
        .find(|w| w.code == "retired_binary_missing")
        .expect("warned");
    assert!(
        w.message
            .contains("worktree add <tmp> v0.16.0 && cargo install --path <tmp> --locked"),
        "{}",
        w.message
    );
}

/// Test 50. Profiles created after the import stay in tollgate, only the
/// imported names leave its roster, and upstream's roster comes back
/// byte-identical — tollgate's roster is never written into `~/.clauth`.
#[test]
fn rollback_keeps_post_import_profiles_and_never_writes_the_fork_roster_into_clauth() {
    let env = Env::new();
    env.tree.reference();
    let upstream = std::fs::read(env.p(".clauth/profiles.toml")).expect("upstream roster");
    run_ok();
    let roster = env.p(".tollgate/profiles.toml");
    let text = std::fs::read_to_string(&roster).expect("roster");
    let text = text.replace("\"scifoo\"]", "\"scifoo\", \"fresh\"]");
    std::fs::write(&roster, text).expect("post-import profile");
    std::fs::create_dir_all(tstore(&env, "fresh", "")).expect("fresh dir");
    std::fs::write(
        tstore(&env, "fresh", "credentials.json"),
        fixture_oauth_body("fresh", 1),
    )
    .expect("fresh store");
    let rolled = rollback(&Options::default()).expect("rolls back");
    assert!(rolled.warnings.iter().any(|w| w.code == "semantic_undo"));
    let after: toml::Value =
        toml::from_str(&std::fs::read_to_string(&roster).expect("roster")).expect("toml");
    let names: Vec<_> = after["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(names, ["fresh"]);
    assert!(
        after.get("active_profile").is_none(),
        "the imported active marker left"
    );
    assert!(tstore(&env, "fresh", "credentials.json").exists());
    assert_eq!(
        std::fs::read(env.p(".clauth/profiles.toml")).expect("upstream"),
        upstream
    );
    assert!(!env.p(".clauth/profiles/fresh").exists());
}

/// Test 51. A slot on a profile created after the import refuses the
/// rollback, naming the way out.
#[test]
fn rollback_refuses_a_live_slot_on_a_post_import_profile() {
    let env = Env::new();
    env.tree.reference();
    run_ok();
    std::fs::create_dir_all(tstore(&env, "fresh", "")).expect("fresh dir");
    std::fs::write(
        tstore(&env, "fresh", "credentials.json"),
        fixture_oauth_body("fresh", 1),
    )
    .expect("store");
    std::fs::remove_file(slot(&env)).expect("unlink");
    std::os::unix::fs::symlink(tstore(&env, "fresh", "credentials.json"), slot(&env))
        .expect("switch");
    let e = rollback(&Options::default()).expect_err("refused");
    let b = e.downcast_ref::<ImportBlocked>().expect("blocked");
    assert_eq!(b.blockers[0].code, "live_slot_on_new_profile");
    assert!(
        b.blockers[0]
            .message
            .contains("run 'tollgate switch <imported profile>' first")
    );
    assert_eq!(env.journal().state, "complete", "nothing was reversed");
}
