//! `import clauth` part 1: the transaction — moves, copies, merges, the
//! commit, crash replay and the automatic reversal (spec §4.1, §4.4, §4.5,
//! §4.7; tests 9, 10, 22, 24–28, 38–44, 51d, 51e).

use std::path::Path;

use super::test_common::*;

fn store(env: &Env, p: &str, f: &str) -> std::path::PathBuf {
    env.p(&format!(".clauth/profiles/{p}/{f}"))
}

fn tstore(env: &Env, p: &str, f: &str) -> std::path::PathBuf {
    env.p(&format!(".tollgate/profiles/{p}/{f}"))
}

/// The seq of the `n`-th (0-based) carrier step in the plan the dry-run
/// survey computes.
fn carrier_seq(n: usize) -> u64 {
    let s = survey(&Options::default());
    s.plan
        .expect("plan")
        .entries
        .iter()
        .filter(|e| e.op.is_carrier_step())
        .nth(n)
        .expect("carrier step")
        .seq
}

/// The crash-loop fixture: the reference tree, a quarantine and MCP logins
/// on the active profile, a codex profile whose store the codex slot links,
/// and an upstream binary on the pinned `PATH`.
fn crash_fixture(env: &Env) {
    env.tree
        .reference()
        .quarantine("personal")
        .mcp_logins("personal");
    env.tree
        .codex_roster(&["cx"], Some("cx"))
        .codex("cx")
        .codex_symlink("cx");
    env.with_upstream_bin();
}

/// Test 24. OAuth, rolling and static profiles move by rename: every store
/// keeps its inode and link count 1, and tollgate's install source for each
/// names the same file upstream's did.
#[test]
fn oauth_rolling_and_static_sets_move_by_rename_and_keep_their_install_source() {
    let env = Env::new();
    env.tree
        .roster(&["o", "r", "s"], Some("o"))
        .oauth("o")
        .rolling("r")
        .static_token("s");
    let mut inodes = Vec::new();
    for (p, files) in [
        ("o", &["credentials.json"][..]),
        ("r", &["credentials.json", "session-token.json"][..]),
        ("s", &["credentials.json", "session-token.json"][..]),
    ] {
        for f in files {
            inodes.push((p, *f, ino(&store(&env, p, f))));
        }
    }
    run_ok();
    for (p, f, before) in inodes {
        let dst = tstore(&env, p, f);
        assert_eq!(ino(&dst), before, "{p}/{f} was not moved by rename");
        assert_eq!(nlink(&dst), 1);
        assert_eq!(mode(&dst), 0o600);
        assert!(!store(&env, p, f).exists(), "{p}/{f} left upstream");
    }
    for (p, want) in [
        ("o", "credentials.json"),
        ("r", "session-token.json"),
        ("s", "session-token.json"),
    ] {
        let got = crate::claude::install_source_in(&env.p(&format!(".tollgate/profiles/{p}")));
        assert_eq!(got.file_name().and_then(|n| n.to_str()), Some(want), "{p}");
    }
}

fn files_containing(root: &Path, needle: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (rel, node) in &TreeSnapshot::of(root).0 {
        if node.kind == "file"
            && std::fs::read(root.join(rel))
                .map(|b| String::from_utf8_lossy(&b).contains(needle))
                .unwrap_or(false)
        {
            out.push(rel.clone());
        }
    }
    out
}

/// Test 25. A quarantine dir and parked MCP logins move whole, by rename;
/// no carrier-shaped file is copied anywhere.
#[test]
fn quarantine_and_mcp_logins_move_and_nothing_carrier_shaped_is_copied() {
    let env = Env::new();
    env.tree
        .reference()
        .quarantine("personal")
        .mcp_logins("personal");
    let q = ino(&store(&env, "personal", "quarantine"));
    let m = ino(&store(&env, "personal", "mcp-logins.json"));
    let before_rt = files_containing(env.h(), "FIXTURE-RT-personal-1");
    run_ok();
    assert_eq!(ino(&tstore(&env, "personal", "quarantine")), q);
    assert_eq!(mode(&tstore(&env, "personal", "quarantine")), 0o700);
    assert_eq!(ino(&tstore(&env, "personal", "mcp-logins.json")), m);
    let after_rt = files_containing(env.h(), "FIXTURE-RT-personal-1");
    assert_eq!(
        before_rt.len(),
        after_rt.len(),
        "a chain was copied: {before_rt:?} -> {after_rt:?}"
    );
    assert_eq!(after_rt, [".tollgate/profiles/personal/credentials.json"]);
    assert_eq!(
        files_containing(env.h(), "FIXTURE-MCPL-personal"),
        [".tollgate/profiles/personal/mcp-logins.json"]
    );
}

/// Test 26. A codex store, its belt and its quarantine verdict move by
/// rename; no copy of the chain appears anywhere the import wrote.
#[test]
fn a_codex_store_lkg_and_quarantine_move_and_each_chain_has_one_inode() {
    let env = Env::new();
    env.tree.codex_roster(&["cx"], Some("cx")).codex("cx");
    env.tree
        .write("profiles/cx/codex-home/config.toml", "model = \"x\"\n");
    let inodes: Vec<_> = ["auth.json", "auth.lkg.json", "auth.quarantine.json"]
        .iter()
        .map(|f| (*f, ino(&store(&env, "cx", f))))
        .collect();
    let before = files_containing(env.h(), "FIXTURE-CODEX-RT-cx").len();
    run_ok();
    for (f, i) in inodes {
        assert_eq!(ino(&tstore(&env, "cx", f)), i, "{f}");
        assert_eq!(nlink(&tstore(&env, "cx", f)), 1, "{f}");
    }
    let after = files_containing(env.h(), "FIXTURE-CODEX-RT-cx");
    assert_eq!(after.len(), before, "{after:?}");
    assert!(after.iter().all(|p| !p.contains("codex-home")), "{after:?}");
    assert!(tstore(&env, "cx", "codex-home/config.toml").exists());
}

/// Test 27. A secret-bearing `config.toml` lands 0600, byte-identical, and
/// its journal entry records its size and nothing derived from its bytes.
#[test]
fn config_toml_copies_are_0600_and_the_journal_records_only_their_size() {
    let env = Env::new();
    env.tree.roster(&["k"], None).api_key("k");
    let src = store(&env, "k", "config.toml");
    let bytes = std::fs::read(&src).expect("src");
    run_ok();
    let dst = tstore(&env, "k", "config.toml");
    assert_eq!(std::fs::read(&dst).expect("dst"), bytes);
    assert_eq!(mode(&dst), 0o600);
    let j = env.journal();
    let e = j
        .main
        .iter()
        .find(|e| e.op == Op::CopySecret)
        .expect("copy_secret entry");
    assert!(e.secret);
    assert_eq!(e.after.size, Some(bytes.len() as u64));
    assert_eq!(e.after.sha256, None);
    assert_no_fixture_secret(
        "journal",
        &std::fs::read_to_string(env.paths().journal()).expect("journal"),
    );
}

/// Test 28. Every upstream file the import COPIES stays exactly as it was
/// (same inode, same bytes).
#[test]
fn upstream_originals_of_copied_entries_stay_byte_identical() {
    let env = Env::new();
    env.tree.reference().api_key("k");
    let before = env.snapshot();
    let s = survey(&Options::default());
    let copied: Vec<String> = s
        .inv
        .items
        .iter()
        .filter(|i| {
            use super::inventory::Action::*;
            matches!(
                i.action,
                Copy | CopySecret | CopyTree | CopyTreeSecret | MergeJson
            )
        })
        .map(|i| format!(".clauth/{}", i.rel))
        .collect();
    assert!(copied.len() > 10, "{copied:?}");
    run_ok();
    let after = env.snapshot();
    for rel in copied {
        for (path, node) in before
            .0
            .iter()
            .filter(|(k, _)| **k == rel || k.starts_with(&format!("{rel}/")))
        {
            assert_eq!(after.get(path), Some(node), "{path} changed");
        }
    }
}

/// Test 38. The claude roster merge appends upstream's names to tollgate's,
/// extends the chain, keeps every key tollgate sets, takes the ones it
/// leaves unset, and never imports `[serve]`/`[update]`/`[local_api]`.
#[test]
fn roster_merge_appends_names_keeps_set_tollgate_keys_and_takes_unset_upstream_keys() {
    let env = Env::new();
    env.tree
        .roster(&["a", "b"], Some("a"))
        .oauth("a")
        .oauth("b");
    let mut up = std::fs::read_to_string(env.p(".clauth/profiles.toml")).expect("roster");
    up = format!(
        "home_tab = \"usage\"\nauth_broken = [\"b\"]\n{up}\n[local_api]\nlisten = \"127.0.0.1:1\"\n"
    );
    std::fs::write(env.p(".clauth/profiles.toml"), up).expect("write");
    std::fs::write(
        env.p(".tollgate/profiles.toml"),
        "# mine\nprofiles = [\"guest\"]\ntheme = \"full\"\n\n[update]\nauto_update = false\n",
    )
    .expect("tollgate roster");
    run_ok();
    let merged = std::fs::read_to_string(env.p(".tollgate/profiles.toml")).expect("merged");
    let doc: toml::Value = toml::from_str(&merged).expect("parses");
    let arr = |k: &str| -> Vec<String> {
        doc.get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(arr("profiles"), ["guest", "a", "b"]);
    assert_eq!(arr("fallback_chain"), ["a", "b"]);
    assert_eq!(arr("auth_broken"), ["b"]);
    assert_eq!(doc["active_profile"].as_str(), Some("a"));
    assert_eq!(
        doc["theme"].as_str(),
        Some("full"),
        "tollgate's own key is kept"
    );
    assert_eq!(
        doc["home_tab"].as_str(),
        Some("usage"),
        "an unset key comes from upstream"
    );
    assert_eq!(doc["update"]["auto_update"].as_bool(), Some(false));
    assert!(doc.get("serve").is_none(), "[serve] is never imported");
    assert!(
        doc.get("local_api").is_none(),
        "[local_api] is never imported"
    );
    assert!(
        merged.starts_with("# mine"),
        "tollgate's formatting survives"
    );
}

/// Test 39. Session owners merge as a union; one session two profiles own
/// becomes `contested`.
#[test]
fn session_owners_merge_and_conflicts_become_contested() {
    let env = Env::new();
    env.tree.reference();
    env.tree.write(
        "session_profiles.json",
        "{\"sessions\":{\"s-1\":{\"known\":\"personal\"},\"s-2\":\"contested\",\"s-3\":{\"known\":\"leadtone\"}}}",
    );
    std::fs::write(
        env.p(".tollgate/session_profiles.json"),
        "{\"sessions\":{\"s-1\":{\"known\":\"guest\"},\"s-4\":{\"known\":\"guest\"}}}",
    )
    .expect("tollgate owners");
    run_ok();
    let v: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.p(".tollgate/session_profiles.json")).expect("read"),
    )
    .expect("json");
    let s = &v["sessions"];
    assert_eq!(s["s-1"], "contested");
    assert_eq!(s["s-2"], "contested");
    assert_eq!(s["s-3"]["known"], "leadtone");
    assert_eq!(s["s-4"]["known"], "guest");
}

/// Test 40. `complete` is the very last durable write, and with it guest
/// mode ends.
#[test]
fn commit_writes_complete_last_and_upstream_active_turns_false() {
    let env = Env::new();
    env.tree.reference();
    env.seams.set(|s| s.op_log = Some(Vec::new()));
    assert!(crate::identity::upstream_active(), "guest mode before");
    run_ok();
    assert!(!crate::identity::upstream_active(), "guest mode after");
    let log = env.seams.op_log();
    let last = log.last().expect("log");
    assert!(last.starts_with("journal state=complete"), "{log:?}");
    assert_eq!(
        log.iter()
            .filter(|l| l.starts_with("journal state=complete"))
            .count(),
        1
    );
    let j = env.journal();
    assert!(j.completed_at.is_some());
    assert!(j.main.iter().all(|e| e.status == Status::Done));
    assert_eq!(
        crate::identity::import_state(),
        crate::identity::ImportState::Complete
    );
}

/// Test 41. After the commit: upstream's rosters name no active profile,
/// its binary is the shim (the original kept, renamed, never run), the
/// tombstone is written and the claude slot links into tollgate's store.
#[test]
fn after_commit_upstream_has_no_active_profile_its_binary_is_the_shim_and_the_slot_links_into_tollgate()
 {
    let env = Env::new();
    env.tree.reference();
    let bin = env.with_upstream_bin();
    let original = std::fs::read(&bin).expect("bin");
    let bin_ino = ino(&bin);
    run_ok();
    let up = std::fs::read_to_string(env.p(".clauth/profiles.toml")).expect("roster");
    assert!(!up.contains("active_profile"), "{up}");
    assert!(up.contains("personal"), "F2 only drops the active marker");
    assert_eq!(std::fs::read_to_string(&bin).expect("shim"), txn::SHIM);
    let retired = env.p("bin/clauth-0.16.0.retired");
    assert_eq!(std::fs::read(&retired).expect("retired"), original);
    assert_eq!(ino(&retired), bin_ino, "renamed, not copied");
    assert!(
        !env.p("RAN-CLAUTH").exists(),
        "the upstream binary was executed"
    );
    let tomb = std::fs::read_to_string(env.p(".clauth/MIGRATED")).expect("tombstone");
    assert!(
        tomb.starts_with("migrated to tollgate ")
            && tomb.contains("undo: tollgate import rollback")
    );
    assert_eq!(
        std::fs::read_link(env.p(".claude/.credentials.json")).expect("link"),
        tstore(&env, "personal", "credentials.json")
    );
    let roster =
        std::fs::read_to_string(env.p(".tollgate/profiles.toml")).expect("tollgate roster");
    assert!(roster.contains("active_profile = \"personal\""), "{roster}");
}

/// Test 22. A Claude Code session that appears before a carrier move makes
/// the engine reverse everything it did: exit 1, journal `aborted`, the
/// tree as it was (carriers by inode).
#[test]
fn a_process_appearing_before_a_carrier_move_reverses_the_transaction() {
    let env = Env::new();
    env.tree.reference();
    env.with_upstream_bin();
    let before = rollback_view(&env.snapshot());
    let at = carrier_seq(1);
    env.seams.set(|s| {
        s.before_step = Some(Box::new(move |seq| {
            if seq == at {
                seams::with(|st| st.procs = vec![FakeProc::new(701, &["claude"])]);
            }
        }));
    });
    let e = run(&Options::default()).expect_err("reversed");
    assert!(format!("{e:#}").contains("process_alive"), "{e:#}");
    assert_eq!(exit_of(Err(e)), 1);
    let j = env.journal();
    assert_eq!(j.state, "aborted");
    assert!(j.main.iter().any(|e| e.status == Status::Undone));
    assert_same_tree(&rollback_view(&env.snapshot()), &before, "tree");
    assert!(crate::identity::upstream_active(), "guest mode stays on");
}

/// Test 10 (run half). An `EXDEV` rename fails the step with no copy
/// fallback: the engine reverses, exit 1, naming the cross-device refusal,
/// and no copy of the chain exists.
#[test]
fn an_exdev_rename_reverses_without_a_copy() {
    let env = Env::new();
    env.tree.reference();
    let before = rollback_view(&env.snapshot());
    let src = store(&env, "personal", "credentials.json");
    env.seams.set(|s| s.exdev = vec![src]);
    let e = run(&Options::default()).expect_err("reversed");
    let text = format!("{e:#}");
    assert!(
        text.contains("cross_device") && text.contains("a credential is never copied"),
        "{text}"
    );
    assert_eq!(exit_of(Err(e)), 1);
    assert!(!tstore(&env, "personal", "credentials.json").exists());
    assert_same_tree(&rollback_view(&env.snapshot()), &before, "tree");
    assert_eq!(env.journal().state, "aborted");
}

/// Test 51e. A reversal once M5 started exits 1; a refusal at M4 (the tree
/// changed between the check and the fence) exits 3 and writes no journal.
#[test]
fn a_reversal_after_m5_started_exits_1_and_an_m4_refusal_exits_3() {
    let env = Env::new();
    env.tree.reference();
    let changed = env.p(".clauth/conversations/late.json");
    env.seams.set(|s| {
        s.after_confirm = Some(Box::new(move || {
            std::fs::write(&changed, "{}").expect("late write");
        }));
    });
    let e = run(&Options::default()).expect_err("M4 refuses");
    assert_eq!(blocked_codes(&e), ["inventory_changed"]);
    assert_eq!(exit_of(Err(e)), 3);
    assert!(!env.paths().journal().exists());
    let src = store(&env, "scifoo", "credentials.json");
    env.seams.set(|s| s.exdev = vec![src]);
    let e = run(&Options::default()).expect_err("reversed");
    assert_eq!(exit_of(Err(e)), 1);
}

/// Test 9. `--rename` maps a name everywhere the import writes it — the
/// profile dir, the roster, the chain, the active marker, the session
/// owners, the slot — and a rollback maps every one back.
#[test]
fn rename_maps_a_name_in_dirs_rosters_chain_session_owners_and_rollback_maps_it_back() {
    let env = Env::new();
    env.tree.reference();
    let before = rollback_view(&env.snapshot());
    let creds = ino(&store(&env, "personal", "credentials.json"));
    let opts = Options {
        renames: [("personal".to_string(), "me".to_string())]
            .into_iter()
            .collect(),
        adopt_live: false,
    };
    run(&opts).expect("commits");
    assert_eq!(ino(&tstore(&env, "me", "credentials.json")), creds);
    assert!(!env.p(".tollgate/profiles/personal").exists());
    let roster: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.p(".tollgate/profiles.toml")).expect("roster"))
            .expect("toml");
    assert_eq!(roster["active_profile"].as_str(), Some("me"));
    let names: Vec<_> = roster["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(names, ["leadtone", "me", "scifoo"]);
    let chain: Vec<_> = roster["fallback_chain"]
        .as_array()
        .expect("chain")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(chain, ["leadtone", "me", "scifoo"]);
    let owners: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.p(".tollgate/session_profiles.json")).expect("owners"),
    )
    .expect("json");
    assert_eq!(owners["sessions"]["s-1"]["known"], "me");
    assert_eq!(
        std::fs::read_link(env.p(".claude/.credentials.json")).expect("slot"),
        tstore(&env, "me", "credentials.json")
    );
    rollback(&Options::default()).expect("rolls back");
    assert_eq!(ino(&store(&env, "personal", "credentials.json")), creds);
    assert_same_tree(&rollback_view(&env.snapshot()), &before, "tree");
}

/// Test 51d. The guest transcript store is copied into `~/.claude/projects`
/// (a name already there wins), and a rollback deletes only what the copy
/// created.
#[test]
fn the_guest_store_is_copied_into_the_global_store_and_rollback_deletes_only_created() {
    let env = Env::new();
    env.tree.reference();
    let guest = env.p(".tollgate/guest-claude/projects/-work");
    std::fs::create_dir_all(&guest).expect("guest");
    std::fs::write(guest.join("a.jsonl"), "guest-a").expect("a");
    std::fs::write(guest.join("b.jsonl"), "guest-b").expect("b");
    std::fs::create_dir_all(env.p(".tollgate/guest-claude/projects/-other")).expect("other");
    std::fs::write(
        env.p(".tollgate/guest-claude/projects/-other/c.jsonl"),
        "guest-c",
    )
    .expect("c");
    let global = env.p(".claude/projects/-work");
    std::fs::create_dir_all(&global).expect("global");
    std::fs::write(global.join("a.jsonl"), "operator-a").expect("a");
    std::fs::write(global.join("own.jsonl"), "operator-own").expect("own");
    run_ok();
    assert_eq!(
        std::fs::read_to_string(global.join("a.jsonl")).expect("a"),
        "operator-a"
    );
    assert_eq!(
        std::fs::read_to_string(global.join("b.jsonl")).expect("b"),
        "guest-b"
    );
    assert_eq!(
        std::fs::read_to_string(env.p(".claude/projects/-other/c.jsonl")).expect("c"),
        "guest-c"
    );
    rollback(&Options::default()).expect("rolls back");
    assert!(!global.join("b.jsonl").exists());
    assert!(!env.p(".claude/projects/-other").exists());
    assert_eq!(
        std::fs::read_to_string(global.join("a.jsonl")).expect("a"),
        "operator-a"
    );
    assert_eq!(
        std::fs::read_to_string(global.join("own.jsonl")).expect("own"),
        "operator-own"
    );
    assert_eq!(
        std::fs::read_to_string(guest.join("b.jsonl")).expect("guest b"),
        "guest-b"
    );
}

/// No leftover relink temp anywhere in `snap`.
fn assert_no_temp(snap: &TreeSnapshot, label: &str) {
    let temps: Vec<_> = snap
        .0
        .keys()
        .filter(|k| k.contains(".tollgate-import.") || k.contains("tollgate-shim"))
        .collect();
    assert!(temps.is_empty(), "{label}: a temp survived: {temps:?}");
}

/// The comparable view of a finished import: bookkeeping dropped, the
/// tombstone's timestamp ignored, inodes zeroed (two sandboxes).
fn final_view(snap: &TreeSnapshot) -> TreeSnapshot {
    let mut v = snap.without(is_bookkeeping).content();
    if let Some(node) = v.0.get_mut(".clauth/MIGRATED") {
        node.sha256 = None;
    }
    v
}

/// Test 42. A crash before or after every single step, then either a resume
/// (which must reach exactly the tree an uninterrupted import leaves, every
/// carrier on its original inode) or a rollback (which must restore the
/// original tree, inodes included). No relink temp survives either way.
#[test]
fn a_crash_at_every_step_resumes_to_the_uninterrupted_tree_or_rolls_back_to_the_original() {
    let (want, steps) = {
        let env = Env::new();
        crash_fixture(&env);
        run_ok();
        (final_view(&env.snapshot()), env.journal().main.len() as u64)
    };
    assert!(
        steps > 30,
        "the fixture exercises a real plan ({steps} steps)"
    );
    let carriers = [
        ("personal", "credentials.json"),
        ("personal", "quarantine"),
        ("personal", "mcp-logins.json"),
        ("leadtone", "credentials.json"),
        ("cx", "auth.json"),
    ];
    for seq in 1..=steps {
        for point in [CrashPoint::BeforeOp, CrashPoint::AfterOp] {
            let label = format!("step {seq} {point:?}");
            // Resume forward.
            {
                let env = Env::new();
                crash_fixture(&env);
                let inodes: Vec<_> = carriers
                    .iter()
                    .map(|(p, f)| ino(&store(&env, p, f)))
                    .collect();
                env.seams.set(|s| s.crash = Some((seq, point)));
                let e = run(&Options::default()).expect_err("crashes");
                assert!(
                    e.downcast_ref::<txn::SimulatedCrash>().is_some(),
                    "{label}: {e:#}"
                );
                assert_eq!(env.journal().state, "in_progress", "{label}");
                env.seams.set(|s| s.crash = None);
                txn::resume().unwrap_or_else(|e| panic!("{label}: resume: {e:#}"));
                let snap = env.snapshot();
                assert_no_temp(&snap, &label);
                assert_same_tree(&final_view(&snap), &want, &format!("{label}: resume"));
                for ((p, f), i) in carriers.iter().zip(inodes) {
                    assert_eq!(
                        ino(&tstore(&env, p, f)),
                        i,
                        "{label}: {p}/{f} not the original inode"
                    );
                }
            }
            // Roll back.
            {
                let env = Env::new();
                crash_fixture(&env);
                let original = rollback_view(&env.snapshot());
                env.seams.set(|s| s.crash = Some((seq, point)));
                run(&Options::default()).expect_err("crashes");
                env.seams.set(|s| s.crash = None);
                rollback(&Options::default())
                    .unwrap_or_else(|e| panic!("{label}: rollback: {e:#}"));
                let snap = env.snapshot();
                assert_no_temp(&snap, &label);
                assert_same_tree(
                    &rollback_view(&snap),
                    &original,
                    &format!("{label}: rollback"),
                );
                assert_eq!(env.journal().state, "rolled_back", "{label}");
            }
        }
    }
}

/// Test 43. A journal that disagrees with disk — a planned move whose
/// source and destination are both gone — stops a resume and a rollback
/// with exit 4 and changes nothing more.
#[test]
fn a_journal_disagreeing_with_disk_stops_with_exit_4() {
    let env = Env::new();
    env.tree.reference();
    let at = carrier_seq(1);
    env.seams
        .set(|s| s.crash = Some((at, CrashPoint::BeforeOp)));
    run(&Options::default()).expect_err("crashes");
    env.seams.set(|s| s.crash = None);
    let victim = env
        .journal()
        .main
        .iter()
        .find(|e| e.seq == at)
        .and_then(|e| e.src.clone())
        .expect("src");
    std::fs::rename(&victim, env.p("elsewhere.json")).expect("tamper");
    let before = rollback_view(&env.snapshot());
    let e = txn::resume().expect_err("stops");
    let a = e.downcast_ref::<ImportNeedsAttention>().expect("attention");
    assert_eq!(a.step, Some(at));
    assert_eq!(exit_of(Err(e)), 4);
    assert_same_tree(&rollback_view(&env.snapshot()), &before, "tree");
    let e = rollback(&Options::default()).expect_err("stops");
    assert_eq!(exit_of(Err(e)), 4);
}

/// Test 44. Write-ahead: every op runs only after a durable journal write
/// that lists it `planned`, and the next journal write after it marks it
/// `done`.
#[test]
fn every_journal_write_is_fsynced_before_its_op() {
    let env = Env::new();
    env.tree.reference();
    env.with_upstream_bin();
    env.seams.set(|s| s.op_log = Some(Vec::new()));
    run_ok();
    let log = env.seams.op_log();
    let mut ops = 0;
    for (i, event) in log.iter().enumerate() {
        let Some(seq) = event.strip_prefix("op ") else {
            continue;
        };
        ops += 1;
        let prior_write = log[..i]
            .iter()
            .rev()
            .find(|l| l.starts_with("journal "))
            .expect("a write before the op");
        assert!(
            prior_write.contains(&format!("{seq}:Planned")),
            "op {seq} before its planned write: {prior_write}"
        );
        let next_write = log[i + 1..]
            .iter()
            .find(|l| l.starts_with("journal "))
            .expect("a write after the op");
        assert!(
            next_write.contains(&format!("{seq}:Done")),
            "op {seq} not marked done: {next_write}"
        );
    }
    assert_eq!(ops, env.journal().main.len());
    assert!(
        log[0].starts_with("journal state=in_progress"),
        "{:?}",
        log.first()
    );
}

/// Test 42, the regular-slot shapes: a Same slot captured onto a
/// credentials profile and a Same slot relinked onto a session-token
/// profile, crashed before and after every step and resumed, reach exactly
/// the uninterrupted tree (the live inode is the store in the first, the
/// sidecar keeps its inode in the second) and leave no temp. A rollback of
/// either is covered by the rollback suite: past the capture it restores a
/// link, not the regular copy, by design (I4, I22).
#[test]
fn a_crash_at_every_step_of_a_regular_slot_import_resumes_to_the_uninterrupted_tree() {
    fn capture_fixture(env: &Env) {
        env.tree
            .roster(&["a", "b"], Some("a"))
            .oauth("a")
            .oauth("b")
            .live_regular_same("a");
    }
    fn static_fixture(env: &Env) {
        env.tree.roster(&["s"], Some("s")).static_token("s");
        let live = serde_json::json!({"claudeAiOauth": {
            "accessToken": fixture_access("s", 9),
            "refreshToken": "FIXTURE-RT-live-copy",
        }});
        std::fs::write(env.p(".claude/.credentials.json"), live.to_string()).expect("slot");
    }
    for (name, fixture, store) in [
        (
            "capture",
            capture_fixture as fn(&Env),
            ".tollgate/profiles/a/credentials.json",
        ),
        (
            "relink-discard",
            static_fixture as fn(&Env),
            ".tollgate/profiles/s/session-token.json",
        ),
    ] {
        let (want, steps) = {
            let env = Env::new();
            fixture(&env);
            run_ok();
            (final_view(&env.snapshot()), env.journal().main.len() as u64)
        };
        for seq in 1..=steps {
            for point in [CrashPoint::BeforeOp, CrashPoint::AfterOp] {
                let label = format!("{name} step {seq} {point:?}");
                let env = Env::new();
                fixture(&env);
                let keep = if name == "capture" {
                    ino(&env.p(".claude/.credentials.json"))
                } else {
                    ino(&env.p(".clauth/profiles/s/session-token.json"))
                };
                env.seams.set(|s| s.crash = Some((seq, point)));
                run(&Options::default()).expect_err("crashes");
                env.seams.set(|s| s.crash = None);
                txn::resume().unwrap_or_else(|e| panic!("{label}: resume: {e:#}"));
                let snap = env.snapshot();
                assert_no_temp(&snap, &label);
                assert_same_tree(&final_view(&snap), &want, &label);
                assert_eq!(ino(&env.p(store)), keep, "{label}");
            }
        }
    }
}
