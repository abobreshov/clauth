//! `import clauth` part 1: the inventory, the dry-run report and the M-1
//! refusals (spec §3.4, §4.1, tests 1–13 and 51a–51c).

use super::test_common::*;

fn report_of(s: &txn::Survey) -> txn::Report {
    txn::report(s, "dry_run")
}

fn entry_lines(r: &txn::Report) -> Vec<String> {
    r.entries
        .iter()
        .map(|e| format!("{} {}", e.action, e.src))
        .collect()
}

/// Test 1. The dry-run, text and JSON, leaves every byte, inode, mode and
/// link of the home as it was, and creates nothing — not even a lock file
/// (the reference tree has no `clauthd.lock`, and `~/.tollgate/.lock` does
/// not exist yet).
#[test]
fn dry_run_changes_no_byte_and_creates_no_lock_file() {
    let env = Env::new();
    env.tree.reference().codex_roster(&[], None);
    env.with_upstream_bin();
    let before = env.snapshot();
    super::cmd_dry_run(&Options::default(), false).expect("text dry-run is clean");
    super::cmd_dry_run(&Options::default(), true).expect("json dry-run is clean");
    let after = env.snapshot();
    assert_eq!(before, after, "the dry-run changed the tree");
    assert!(!env.p(".clauth/clauthd.lock").exists());
    assert!(!env.p(".tollgate/.lock").exists());
    assert!(!env.p(".tollgate/import-journal.json").exists());
}

/// Test 2. The reference tree (the owner machine's shape) classifies
/// exactly as spec §3.4 says, every entry and every action, `clauth.log`
/// skipped; the slots, the roster and the step count come out with it.
#[test]
fn dry_run_classifies_the_reference_inventory() {
    let env = Env::new();
    env.tree.reference();
    let s = survey(&Options::default());
    assert!(
        s.blockers.is_empty(),
        "reference is clean: {:?}",
        s.blockers
    );
    let r = report_of(&s);
    let mut want = Vec::new();
    for p in ["leadtone", "personal", "scifoo"] {
        for (action, f) in [
            ("copy", "account_id.json"),
            ("copy-0600", "config.toml"),
            ("move", "credentials.json"),
            ("copy", "profile_fetched.json"),
            ("copy", "usage_cache.json"),
            ("copy", "usage_history.jsonl"),
        ] {
            want.push(format!("{action} profiles/{p}/{f}"));
        }
    }
    for (action, f) in [
        ("skip", ".completions_installed"),
        ("never", ".lock"),
        ("skip", "ai_pricelog_v4_price_cache.json"),
        ("skip", "clauth.log"),
        ("skip", "completions"),
        ("copy", "conversations"),
        ("skip", "live_bare"),
        ("skip", "mcp_live"),
        ("merge", "profiles.toml"),
        ("never", "rotation-locks"),
        ("merge", "session_profiles.json"),
        ("skip", "status.json"),
        ("skip", "status_cache.json"),
        ("never", "usage-fetch.lock"),
    ] {
        want.push(format!("{action} {f}"));
    }
    assert_eq!(entry_lines(&r), want);
    let creds = r
        .entries
        .iter()
        .find(|e| e.src == "profiles/personal/credentials.json")
        .expect("personal store");
    assert_eq!(
        creds.dst.as_deref(),
        Some("profiles/personal/credentials.json")
    );
    assert!(creds.secret && creds.carrier);
    assert_eq!(r.live_slots.claude.state, "symlink");
    assert_eq!(r.live_slots.claude.profile.as_deref(), Some("personal"));
    assert_eq!(r.live_slots.claude.verdict, "relink");
    assert_eq!(r.live_slots.codex.state, "regular");
    assert_eq!(r.live_slots.codex.verdict, "untouched");
    assert_eq!(r.roster.claude, ["leadtone", "personal", "scifoo"]);
    assert_eq!(r.roster.active.as_deref(), Some("personal"));
    // mkdir profiles/, then per profile mkdir + move + 5 copies, the tree
    // copy, the session merge, the roster merge, F2 and the tombstone.
    assert_eq!(r.journal.steps_planned, 1 + 3 * 7 + 1 + 1 + 1 + 1 + 1);
    assert_eq!(r.journal.state, "none");
    assert!(r.ok);
    let json = serde_json::to_value(&r).expect("report serializes");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "import clauth");
    assert_eq!(json["mode"], "dry_run");
}

/// Test 3. Anything the table does not name refuses, at the top level and
/// inside a profile: it may carry a credential.
#[test]
fn an_unknown_top_level_or_profile_entry_is_refused() {
    let env = Env::new();
    env.tree.roster(&["a"], Some("a")).oauth("a");
    env.tree
        .unknown_entry("weird.bin")
        .unknown_entry("profiles/a/notes.txt");
    let s = survey(&Options::default());
    let unknown: Vec<_> = s
        .blockers
        .iter()
        .filter(|b| b.code == "unknown_entry")
        .map(|b| b.path.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        unknown,
        ["~/.clauth/profiles/a/notes.txt", "~/.clauth/weird.bin"]
    );
    assert!(s.blockers[0].message.contains(
        "is not in the import inventory; it may carry a credential, so nothing is imported"
    ));
}

/// Test 4. A crashed rotation's staged chain and a crashed write's staging
/// file each refuse with the remedy that lets upstream adopt it.
#[test]
fn a_pending_rotation_or_stray_temp_is_refused_with_its_remedy() {
    let env = Env::new();
    env.tree
        .roster(&["a"], Some("a"))
        .oauth("a")
        .pending("a")
        .stray_temp("a");
    env.tree.write(".status.json.tmp.99.1", "{}");
    let s = survey(&Options::default());
    let c = codes(&s);
    assert!(c.contains(&"pending_rotation".to_string()), "{c:?}");
    assert_eq!(c.iter().filter(|x| *x == "stray_temp").count(), 2, "{c:?}");
    let pending = s
        .blockers
        .iter()
        .find(|b| b.code == "pending_rotation")
        .expect("pending");
    assert_eq!(
        pending.message,
        "~/.clauth/profiles/a/credentials.json.pending is a crashed rotation's staged chain; run 'clauth list' once so upstream adopts it, then retry"
    );
}

/// Test 5. A chain with a second hard link, or a store that is a symlink,
/// has no single inode to move: refused.
#[test]
fn a_hardlinked_or_symlinked_carrier_is_refused() {
    let env = Env::new();
    env.tree
        .roster(&["a", "b"], Some("a"))
        .oauth("a")
        .oauth("b")
        .hardlink("a");
    let b = env.tree.dir("b").join("credentials.json");
    std::fs::rename(&b, env.p("b-real.json")).expect("move b");
    std::os::unix::fs::symlink(env.p("b-real.json"), &b).expect("symlink b");
    let s = survey(&Options::default());
    let c = codes(&s);
    assert!(c.contains(&"hardlinked_carrier".to_string()), "{c:?}");
    assert!(c.contains(&"symlinked_source".to_string()), "{c:?}");
}

/// Test 6. A codex home holding an `auth.json` at any depth is a second
/// carrier: refused. Without one it is copied.
#[test]
fn codex_home_holding_auth_json_is_refused() {
    let env = Env::new();
    env.tree.codex_roster(&["cx"], Some("cx")).codex("cx");
    env.tree
        .write("profiles/cx/codex-home/config.toml", "model = \"x\"\n");
    let clean = survey(&Options::default());
    assert!(!codes(&clean).contains(&"codex_home_carrier".to_string()));
    assert!(entry_lines(&report_of(&clean)).contains(&"copy profiles/cx/codex-home".to_string()));
    env.tree
        .write("profiles/cx/codex-home/deep/auth.json", "{}");
    let s = survey(&Options::default());
    assert!(
        codes(&s).contains(&"codex_home_carrier".to_string()),
        "{:?}",
        codes(&s)
    );
}

/// Test 7. A stale per-session tree is skipped, but a regular-file chain
/// copy inside it refuses; a symlink there is fine.
#[test]
fn a_carrier_copy_in_a_stale_runtime_tree_is_refused_and_a_symlink_there_is_not() {
    let env = Env::new();
    env.tree
        .roster(&["a"], Some("a"))
        .oauth("a")
        .runtime_tree("a", false);
    let s = survey(&Options::default());
    assert!(
        !codes(&s).contains(&"stale_runtime_carrier".to_string()),
        "{:?}",
        codes(&s)
    );
    assert!(entry_lines(&report_of(&s)).contains(&"skip profiles/a/runtime-abc".to_string()));
    std::fs::remove_dir_all(env.tree.dir("a").join("runtime-abc")).expect("rm");
    env.tree.runtime_tree("a", true);
    let s = survey(&Options::default());
    assert!(
        codes(&s).contains(&"stale_runtime_carrier".to_string()),
        "{:?}",
        codes(&s)
    );
}

/// Test 8. A name tollgate's claude roster, its codex roster or a bare
/// profile dir already holds refuses, whatever the case.
#[test]
fn a_name_colliding_with_either_roster_or_a_profile_dir_is_refused_case_insensitively() {
    let env = Env::new();
    env.tree
        .roster(&["personal", "leadtone", "scifoo"], Some("personal"))
        .oauth("personal")
        .oauth("leadtone")
        .oauth("scifoo");
    std::fs::write(
        env.p(".tollgate/profiles.toml"),
        "profiles = [\"Personal\"]\n",
    )
    .expect("claude roster");
    std::fs::write(
        env.p(".tollgate/codex-profiles.toml"),
        "profiles = [\"LEADTONE\"]\n",
    )
    .expect("codex roster");
    std::fs::create_dir_all(env.p(".tollgate/profiles/SciFoo")).expect("bare dir");
    let s = survey(&Options::default());
    let msgs: Vec<_> = s
        .blockers
        .iter()
        .filter(|b| b.code == "name_collision" || b.code == "destination_exists")
        .map(|b| (b.code.clone(), b.message.clone()))
        .collect();
    assert!(msgs.contains(&(
        "name_collision".to_string(),
        "a tollgate codex profile named 'leadtone' already exists; pass --rename leadtone=<new>".to_string()
    )), "{msgs:?}");
    assert!(msgs.contains(&(
        "name_collision".to_string(),
        "a tollgate claude profile named 'personal' already exists; pass --rename personal=<new>".to_string()
    )), "{msgs:?}");
    assert!(
        msgs.contains(&(
            "destination_exists".to_string(),
            "a tollgate claude profile named 'scifoo' already exists; pass --rename scifoo=<new>"
                .to_string()
        )),
        "{msgs:?}"
    );
    // --rename clears each.
    let opts = Options {
        renames: [("personal", "p2"), ("leadtone", "l2"), ("scifoo", "s2")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        adopt_live: false,
    };
    let s = survey(&opts);
    assert!(
        !s.blockers
            .iter()
            .any(|b| b.code == "name_collision" || b.code == "destination_exists"),
        "{:?}",
        s.blockers
    );
}

/// Test 51c. The uniqueness check is `actions::validate_profile_name`'s own,
/// so the message names the harness whose roster holds the name — and a
/// rename to a name some roster holds refuses the same way.
#[test]
fn a_name_held_by_any_roster_refuses_through_validate_profile_name() {
    let env = Env::new();
    env.tree.codex_roster(&["cx"], Some("cx")).codex("cx");
    env.tree.roster(&["a"], Some("a")).oauth("a");
    std::fs::write(
        env.p(".tollgate/profiles.toml"),
        "profiles = [\"cx\", \"taken\"]\n",
    )
    .expect("roster");
    let s = survey(&Options::default());
    let b = s
        .blockers
        .iter()
        .find(|b| b.code == "name_collision")
        .expect("collision");
    assert_eq!(
        b.message,
        "a tollgate claude profile named 'cx' already exists; pass --rename cx=<new>"
    );
    let opts = Options {
        renames: [
            ("cx".to_string(), "fresh".to_string()),
            ("a".to_string(), "TAKEN".to_string()),
        ]
        .into_iter()
        .collect(),
        adopt_live: false,
    };
    let s = survey(&opts);
    let c: Vec<_> = s.blockers.iter().map(|b| b.message.clone()).collect();
    assert_eq!(
        c,
        ["a tollgate claude profile named 'TAKEN' already exists; pass --rename a=<new>"]
    );
    let bad = Options {
        renames: [("nobody".to_string(), "x".to_string())]
            .into_iter()
            .collect(),
        adopt_live: false,
    };
    assert!(codes(&survey(&bad)).contains(&"rename_unknown".to_string()));
}

/// Test 10 (M-1 half). A source whose filesystem differs from its
/// destination's refuses before anything moves; nothing is copied.
#[test]
fn cross_device_source_and_destination_are_refused_without_a_copy() {
    let env = Env::new();
    env.tree.roster(&["a"], Some("a")).oauth("a");
    let parent = env.tree.dir("a");
    env.seams.set(|s| s.foreign_dev = vec![parent]);
    let s = survey(&Options::default());
    let cross: Vec<_> = s
        .blockers
        .iter()
        .filter(|b| b.code == "cross_device")
        .collect();
    assert!(!cross.is_empty(), "{:?}", s.blockers);
    assert!(
        cross[0]
            .message
            .ends_with("are on different filesystems; a credential is never copied")
    );
    let e = run(&Options::default()).expect_err("refused");
    assert!(blocked_codes(&e).contains(&"cross_device".to_string()));
    assert!(
        !env.p(".tollgate/profiles/a").exists(),
        "nothing was copied"
    );
}

/// Test 11. No fixture token reaches the text report, the JSON report, the
/// journal or the backups — dry-run, run and rollback.
#[test]
fn import_report_never_echoes_a_fixture_secret() {
    let env = Env::new();
    env.tree
        .reference()
        .api_key("apikey")
        .static_token("leadtone");
    env.tree.codex_roster(&["cx"], None).codex("cx");
    env.tree
        .write("gateway.toml", "admin_key = \"FIXTURE-KEY-gw\"\n");
    std::fs::write(
        env.p(".claude/settings.json"),
        "{\"env\":{\"K\":\"FIXTURE-KEY-env\"}}",
    )
    .expect("settings");
    let s = survey(&Options::default());
    let r = report_of(&s);
    assert_no_fixture_secret("text report", &txn::render_text(&r, true));
    assert_no_fixture_secret("json report", &serde_json::to_string(&r).expect("json"));
    let committed = run(&Options::default()).expect("commits");
    assert_no_fixture_secret("commit line", &txn::commit_message(&committed));
    assert_no_fixture_secret(
        "journal",
        &std::fs::read_to_string(env.paths().journal()).expect("journal"),
    );
    assert_no_fixture_secret("backups", &read_tree_text(&env.paths().backup_dir()));
    let rolled = rollback(&Options::default()).expect("rolls back");
    assert_no_fixture_secret("rollback warnings", &format!("{:?}", rolled.warnings));
    assert_no_fixture_secret(
        "journal after rollback",
        &std::fs::read_to_string(env.paths().journal()).expect("journal"),
    );
    let r = txn::report(&survey(&Options::default()), "rollback");
    assert_no_fixture_secret("rollback report", &serde_json::to_string(&r).expect("json"));
}

/// Test 12. The dry-run probes every fence file read-only: one held
/// elsewhere reads `held` and blocks, a free one `free`, a missing one
/// `absent` (and stays missing).
#[test]
fn dry_run_reports_held_free_and_absent_locks() {
    let env = Env::new();
    env.tree.reference();
    let _held = LockHolder::hold(&env.p(".clauth/usage-fetch.lock"));
    let s = survey(&Options::default());
    let state = |p: &str| {
        s.locks
            .iter()
            .find(|l| l.path == p)
            .map(|l| l.state)
            .unwrap_or_else(|| panic!("no lock row {p}: {:?}", s.locks))
    };
    assert_eq!(state("~/.clauth/usage-fetch.lock"), "held");
    assert_eq!(state("~/.clauth/.lock"), "free");
    assert_eq!(state("~/.clauth/clauthd.lock"), "absent");
    assert_eq!(state("~/.clauth/rotation-locks/personal.lock"), "absent");
    assert_eq!(state("~/.tollgate/.lock"), "absent");
    let b = s
        .blockers
        .iter()
        .find(|b| b.code == "lock_held")
        .expect("blocker");
    assert_eq!(
        b.message,
        "~/.clauth/usage-fetch.lock is held by another process; stop it first"
    );
    assert!(!env.p(".clauth/clauthd.lock").exists());
}

/// Test 13. A completed import, a home without `~/.clauth`, and any
/// platform but Linux all refuse at M-1.
#[test]
fn already_complete_no_upstream_and_non_linux_are_refused() {
    let env = Env::new();
    env.tree.reference();
    std::fs::write(env.paths().journal(), "{\"state\":\"complete\"}").expect("journal");
    assert!(codes(&survey(&Options::default())).contains(&"already_imported".to_string()));
    std::fs::write(env.paths().journal(), "{\"state\":\"in_progress\"}").expect("journal");
    let s = survey(&Options::default());
    assert!(codes(&s).contains(&"journal_pending".to_string()));
    assert_eq!(exit_of(txn::blocked_outcome(&s, true)), 4);
    std::fs::remove_file(env.paths().journal()).expect("rm journal");
    env.seams.set(|s| s.not_linux = true);
    assert_eq!(
        codes(&survey(&Options::default())),
        ["unsupported_platform"]
    );
    env.seams.set(|s| s.not_linux = false);
    std::fs::remove_dir_all(env.p(".clauth")).expect("rm clauth");
    let s = survey(&Options::default());
    assert_eq!(codes(&s), ["upstream_absent"]);
    assert_eq!(exit_of(txn::blocked_outcome(&s, true)), 3);
}

/// Test 51a. A tollgate that is not the first `tollgate` on `PATH` (a
/// `cargo run` build) refuses; the installed one passes.
#[test]
fn a_dev_build_exe_is_refused_and_the_installed_path_passes() {
    let env = Env::new();
    env.tree.reference();
    let dev = env.p("repo/target/debug/tollgate");
    std::fs::create_dir_all(dev.parent().expect("dir")).expect("mkdir");
    std::fs::write(&dev, "dev").expect("dev build");
    let bin = env.p("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::write(bin.join("tollgate"), "installed").expect("installed");
    let (d, b) = (dev.clone(), bin.clone());
    env.seams.set(|s| {
        s.current_exe = Some(d);
        s.path_dirs = vec![b];
    });
    let s = survey(&Options::default());
    let blocker = s
        .blockers
        .iter()
        .find(|x| x.code == "dev_build_exe")
        .expect("dev build refused");
    assert!(
        blocker
            .message
            .contains("run the installed tollgate so the rewritten helper points at it")
    );
    let installed = bin.join("tollgate");
    env.seams.set(|s| s.current_exe = Some(installed));
    assert!(!codes(&survey(&Options::default())).contains(&"dev_build_exe".to_string()));
}

/// Test 51b. An upstream build off `PATH` (`<src>/target/*/clauth`, `<src>`
/// read from the cargo install record) is listed as a warning, and never
/// executed: its sentinel stays absent.
#[test]
fn offpath_upstream_builds_are_listed_as_warnings_without_exec() {
    let env = Env::new();
    env.tree.reference();
    let src = env.p("Work/clauth");
    let debug = src.join("target/debug");
    std::fs::create_dir_all(&debug).expect("target");
    crate::testutil::write_shim(
        &debug,
        "clauth",
        &format!("touch {}/RAN-OFFPATH", env.h().display()),
    );
    std::fs::create_dir_all(env.p(".cargo")).expect("cargo");
    std::fs::write(
        env.p(".cargo/.crates2.json"),
        serde_json::json!({"installs": {format!("clauth 0.16.0 (path+file://{})", src.display()): {}}}).to_string(),
    )
    .expect("record");
    let s = survey(&Options::default());
    let w = s
        .warnings
        .iter()
        .find(|w| w.code == "upstream_offpath_build")
        .expect("listed");
    assert_eq!(
        w.message,
        "upstream clauth build ~/Work/clauth/target/debug/clauth is not on PATH and is not retired; do not run it (or 'cargo run' on the mommy branch) until R2's start-time reconcile ships"
    );
    assert!(
        s.blockers.is_empty(),
        "a warning, never a blocker: {:?}",
        s.blockers
    );
    assert!(!env.p("RAN-OFFPATH").exists(), "the build was executed");
}

/// `claude::install_source_in` is `install_source_path` on a directory: the
/// sidecar when it holds a genuine long-lived login (a mint or a rolling
/// bearer), else `credentials.json` — a mis-filled sidecar (a refresh token)
/// never becomes the install source.
#[test]
fn install_source_in_follows_the_content_aware_rule() {
    let env = Env::new();
    env.tree
        .oauth("o")
        .rolling("r")
        .static_token("s")
        .oauth("m");
    std::fs::write(
        env.tree.dir("m").join("session-token.json"),
        fixture_oauth_body("m", 1),
    )
    .expect("misfill");
    for (p, want) in [
        ("o", "credentials.json"),
        ("r", "session-token.json"),
        ("s", "session-token.json"),
        ("m", "credentials.json"),
    ] {
        let got = crate::claude::install_source_in(&env.tree.dir(p));
        assert_eq!(got.file_name().and_then(|n| n.to_str()), Some(want), "{p}");
    }
}

/// Import spec §6 lane 3 / Hermes spec §6.2: the M2 uniqueness check spans
/// the Hermes roster too, through the same `validate_profile_name`. An
/// upstream claude or codex profile whose name `hermes-profiles.toml` holds
/// (any case) refuses naming the Hermes roster, `--rename` clears it, and an
/// unreadable Hermes roster blocks the whole check.
#[test]
fn import_refuses_a_name_held_by_a_hermes_profile() {
    let env = Env::new();
    env.tree.roster(&["herm"], Some("herm")).oauth("herm");
    env.tree.codex_roster(&["hx"], Some("hx")).codex("hx");
    crate::testutil::write_hermes_roster(&["Herm", "HX"]);
    let s = survey(&Options::default());
    let mut msgs: Vec<_> = s
        .blockers
        .iter()
        .filter(|b| b.code == "name_collision")
        .map(|b| b.message.clone())
        .collect();
    msgs.sort();
    assert_eq!(
        msgs,
        [
            "a tollgate hermes profile named 'herm' already exists; pass --rename herm=<new>",
            "a tollgate hermes profile named 'hx' already exists; pass --rename hx=<new>",
        ],
        "{:?}",
        s.blockers
    );

    let opts = Options {
        renames: [("herm", "herm-claude"), ("hx", "hx-codex")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        adopt_live: false,
    };
    let s = survey(&opts);
    assert!(
        !s.blockers
            .iter()
            .any(|b| b.code == "name_collision" || b.code == "destination_exists"),
        "{:?}",
        s.blockers
    );

    std::fs::write(
        crate::hermes::profiles::hermes_state_path().unwrap(),
        "schema_version = [\n",
    )
    .unwrap();
    let s = survey(&opts);
    assert!(
        s.blockers
            .iter()
            .any(|b| b.code == "tollgate_roster_unreadable"
                && b.message
                    .starts_with("~/.tollgate/hermes-profiles.toml cannot be read")),
        "{:?}",
        s.blockers
    );
}

/// Review lens credentials #2. A `clauth` on `PATH` that resolves to this
/// tollgate (a compatibility alias, by symlink or hard link) or to a file not
/// named `clauth` is never an F1 target: the survey refuses with
/// `upstream_binary_is_tollgate` and plans no `retire_bin`, so tollgate never
/// renames itself and writes the shim over its own name.
#[test]
fn a_clauth_alias_to_tollgate_is_never_retired() {
    let env = Env::new();
    env.tree.reference();
    let bin = env.p("bin");
    let tollgate = env.tree.tollgate_bin();
    let before = std::fs::read(&tollgate).expect("tollgate");
    std::os::unix::fs::symlink(&tollgate, bin.join("clauth")).expect("alias");
    let (b, t) = (bin.clone(), tollgate.clone());
    env.seams.set(|s| {
        s.path_dirs = vec![b];
        s.current_exe = Some(t);
    });
    let s = survey(&Options::default());
    assert!(
        codes(&s).contains(&"upstream_binary_is_tollgate".to_string()),
        "{:?}",
        codes(&s)
    );
    assert!(s.bins.is_empty(), "the alias is no F1 target");
    assert!(
        !s.plan
            .iter()
            .flat_map(|p| &p.entries)
            .any(|e| e.op == Op::RetireBin)
    );
    assert!(run(&Options::default()).is_err(), "the import refuses");
    assert_eq!(std::fs::read(&tollgate).expect("tollgate"), before);
    assert!(
        !bin.join(format!("clauth-{}.retired", txn::UPSTREAM_VERSION))
            .exists()
    );

    // A hard link to this tollgate and a differently named target refuse too.
    std::fs::remove_file(bin.join("clauth")).expect("rm alias");
    std::fs::hard_link(&tollgate, bin.join("clauth")).expect("hard link");
    assert!(
        codes(&survey(&Options::default())).contains(&"upstream_binary_is_tollgate".to_string())
    );
    std::fs::remove_file(bin.join("clauth")).expect("rm link");
    let other = bin.join("clauth-dev");
    std::fs::write(&other, "#!/bin/sh\n").expect("other");
    std::os::unix::fs::symlink(&other, bin.join("clauth")).expect("alias");
    assert!(
        codes(&survey(&Options::default())).contains(&"upstream_binary_is_tollgate".to_string())
    );
}

/// Review lens credentials #9. A symlinked slot whose store is refused as
/// `cross_device` is not also reported as `live_link_foreign` (whose
/// "relink it with clauth first" is the wrong remedy).
#[test]
fn a_cross_device_store_does_not_also_call_its_symlinked_slot_foreign() {
    let env = Env::new();
    env.tree
        .roster(&["a"], Some("a"))
        .oauth("a")
        .live_symlink("a", "credentials.json");
    let parent = env.tree.dir("a");
    env.seams.set(|s| s.foreign_dev = vec![parent]);
    let s = survey(&Options::default());
    assert!(
        codes(&s).contains(&"cross_device".to_string()),
        "{:?}",
        codes(&s)
    );
    assert!(
        !codes(&s).contains(&"live_link_foreign".to_string()),
        "{:?}",
        codes(&s)
    );
    assert_eq!(s.claude.verdict, "refuse");
}
