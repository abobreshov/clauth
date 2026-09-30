//! `import clauth` part 1: the live slots (spec §4.6; tests 29–37).

use super::test_common::*;

fn slot(env: &Env) -> std::path::PathBuf {
    env.p(".claude/.credentials.json")
}

fn tstore(env: &Env, p: &str, f: &str) -> std::path::PathBuf {
    env.p(&format!(".tollgate/profiles/{p}/{f}"))
}

fn regular_slot(env: &Env, body: &serde_json::Value) {
    let _ = std::fs::remove_file(slot(env));
    std::fs::write(slot(env), body.to_string()).expect("slot");
}

fn files_with(env: &Env, needle: &str) -> Vec<String> {
    env.snapshot()
        .0
        .iter()
        .filter(|(rel, n)| {
            n.kind == "file"
                && std::fs::read(env.p(rel))
                    .map(|b| String::from_utf8_lossy(&b).contains(needle))
                    .unwrap_or(false)
        })
        .map(|(rel, _)| rel.clone())
        .collect()
}

/// Test 29. A slot linked into an upstream store is repointed in the same
/// journal entry that moves the store (`move_relink`), not a later one.
#[test]
fn a_symlinked_live_slot_is_repointed_in_the_move_step() {
    let env = Env::new();
    env.tree.reference();
    run_ok();
    assert_eq!(
        std::fs::read_link(slot(&env)).expect("link"),
        tstore(&env, "personal", "credentials.json")
    );
    let j = env.journal();
    let relinks: Vec<_> = j.main.iter().filter(|e| e.op == Op::MoveRelink).collect();
    assert_eq!(relinks.len(), 1);
    assert_eq!(relinks[0].prior.link.as_deref(), Some(slot(&env).as_path()));
    assert!(
        relinks[0].after.temp.is_some(),
        "the temp path is journaled"
    );
    assert!(!j.main.iter().any(|e| e.op == Op::Capture));
    assert!(
        !env.snapshot()
            .0
            .keys()
            .any(|k| k.contains("tollgate-import"))
    );
}

/// Test 30. A regular slot holding the active profile's own login becomes
/// the store (its inode, with its newer `mcpOAuth`), then a link to it.
#[test]
fn a_same_regular_live_slot_becomes_the_store_then_a_link() {
    let env = Env::new();
    env.tree.roster(&["a"], Some("a")).oauth("a");
    env.tree.live_regular_same("a");
    let mut live: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(slot(&env)).expect("slot")).expect("body");
    live["mcpOAuth"]["srv"]["accessToken"] = "FIXTURE-MCP-newer".into();
    regular_slot(&env, &live);
    let live_ino = ino(&slot(&env));
    let s = survey(&Options::default());
    assert_eq!((s.claude.state, s.claude.verdict), ("regular", "capture"));
    run_ok();
    let store = tstore(&env, "a", "credentials.json");
    assert_eq!(ino(&store), live_ino, "the live inode became the store");
    assert_eq!(mode(&store), 0o600);
    assert!(
        std::fs::read_to_string(&store)
            .expect("store")
            .contains("FIXTURE-MCP-newer")
    );
    assert_eq!(std::fs::read_link(slot(&env)).expect("link"), store);
    assert_eq!(
        files_with(&env, "FIXTURE-RT-a-1"),
        [".tollgate/profiles/a/credentials.json"]
    );
}

/// Test 30a. On a long-lived session-token profile a Same regular slot is
/// relinked, never captured: the sidecar keeps its inode and stays the
/// install source, the live copy moves to the profile's quarantine (review
/// lens credentials #4), and M8's assert passes.
#[test]
fn a_same_regular_slot_on_a_session_token_profile_is_relinked_not_captured() {
    let env = Env::new();
    env.tree.roster(&["a"], Some("a")).static_token("a");
    let live = serde_json::json!({"claudeAiOauth": {
        "accessToken": fixture_access("a", 9),
        "refreshToken": "FIXTURE-RT-live-copy",
    }});
    regular_slot(&env, &live);
    let sidecar = ino(&env.p(".clauth/profiles/a/session-token.json"));
    let s = survey(&Options::default());
    assert_eq!((s.claude.state, s.claude.verdict), ("regular", "relink"));
    run_ok();
    let dst = tstore(&env, "a", "session-token.json");
    assert_eq!(ino(&dst), sidecar);
    assert_eq!(std::fs::read_link(slot(&env)).expect("link"), dst);
    let source = crate::claude::install_source_in(&env.p(".tollgate/profiles/a"));
    assert_eq!(source, dst, "the install source is unchanged");
    assert_eq!(
        files_with(&env, "FIXTURE-RT-live-copy"),
        [".tollgate/profiles/a/quarantine/credentials.json.live"],
        "the live copy is kept only in the profile's quarantine"
    );
    assert!(!env.journal().main.iter().any(|e| e.op == Op::Capture));
    assert_eq!(env.journal().state, "complete");
}

/// Test 31. A regular slot holding some other login refuses unless
/// `--adopt-live`, which captures it as the profile (the stored chain is
/// kept in the profile's quarantine). On a session-token profile it refuses
/// either way.
#[test]
fn a_diverged_regular_live_slot_refuses_without_adopt_live_and_is_captured_with_it() {
    let env = Env::new();
    env.tree
        .roster(&["a"], Some("a"))
        .oauth("a")
        .live_regular_diverged("a");
    let s = survey(&Options::default());
    let b = s
        .blockers
        .iter()
        .find(|b| b.code == "claude_live_diverged")
        .expect("refused");
    assert_eq!(
        b.message,
        "~/.claude/.credentials.json holds a login that differs from profile 'a'; pass --adopt-live to import it as 'a' (the stored chain is kept in its quarantine/)"
    );
    let live_ino = ino(&slot(&env));
    let adopt = Options {
        adopt_live: true,
        ..Options::default()
    };
    run(&adopt).expect("captured");
    let store = tstore(&env, "a", "credentials.json");
    assert_eq!(ino(&store), live_ino);
    assert!(
        std::fs::read_to_string(&store)
            .expect("store")
            .contains(&fixture_refresh("a", 5))
    );
    assert_eq!(
        files_with(&env, &fixture_refresh("a", 1)),
        [".tollgate/profiles/a/quarantine/credentials.json.superseded"],
        "the stored chain is kept only in the profile's quarantine"
    );

    drop(env);
    let env = Env::new();
    env.tree
        .roster(&["s"], Some("s"))
        .static_token("s")
        .live_regular_diverged("s");
    assert!(codes(&survey(&adopt)).contains(&"live_diverged_on_static_token".to_string()));
}

/// Test 32. With no upstream active profile, a regular slot whose refresh
/// token equals a store's is that store's detached duplicate: relinked to
/// it, one inode left.
#[test]
fn a_detached_duplicate_with_no_active_profile_is_relinked_to_its_store() {
    let env = Env::new();
    env.tree.roster(&["a", "b"], None).oauth("a").oauth("b");
    std::fs::copy(env.p(".clauth/profiles/b/credentials.json"), slot(&env)).expect("copy");
    let s = survey(&Options::default());
    assert_eq!(s.claude.profile.as_deref(), Some("b"));
    run_ok();
    assert_eq!(
        std::fs::read_link(slot(&env)).expect("link"),
        tstore(&env, "b", "credentials.json")
    );
    assert_eq!(
        files_with(&env, &fixture_refresh("b", 1)),
        [".tollgate/profiles/b/credentials.json"]
    );
    let roster = std::fs::read_to_string(env.p(".tollgate/profiles.toml")).expect("roster");
    assert!(roster.contains("active_profile = \"b\""), "{roster}");
}

/// Test 33. An independent login, or no slot at all, is left exactly as it
/// is.
#[test]
fn an_independent_or_missing_live_slot_is_untouched() {
    let env = Env::new();
    env.tree.roster(&["a"], None).oauth("a");
    regular_slot(
        &env,
        &serde_json::json!({"claudeAiOauth": {"accessToken": "own", "refreshToken": "own-rt"}}),
    );
    let before = env.snapshot().get(".claude/.credentials.json").cloned();
    assert_eq!(survey(&Options::default()).claude.verdict, "untouched");
    run_ok();
    assert_eq!(
        env.snapshot().get(".claude/.credentials.json").cloned(),
        before
    );

    drop(env);
    let env = Env::new();
    env.tree.roster(&["a"], Some("a")).oauth("a");
    run_ok();
    assert!(
        std::fs::symlink_metadata(slot(&env)).is_err(),
        "a missing slot stays missing"
    );
}

/// Test 34. A slot linking anywhere the import does not move — a
/// non-carrier upstream file, or outside both trees — refuses.
#[test]
fn a_live_link_to_a_non_carrier_is_refused() {
    let env = Env::new();
    env.tree
        .roster(&["a"], Some("a"))
        .oauth("a")
        .live_symlink("a", "config.toml");
    let s = survey(&Options::default());
    let b = s
        .blockers
        .iter()
        .find(|b| b.code == "live_link_foreign")
        .expect("refused");
    assert!(
        b.message
            .contains("~/.clauth/profiles/a/config.toml, which the import does not move")
    );
    let _ = std::fs::remove_file(slot(&env));
    std::os::unix::fs::symlink(env.p("elsewhere.json"), slot(&env)).expect("link");
    assert!(codes(&survey(&Options::default())).contains(&"live_link_foreign".to_string()));
}

/// Test 35. A codex slot linked into a codex store moves and repoints in
/// one step, while the fence holds that profile's upstream rotation lock.
#[test]
fn a_codex_symlink_slot_moves_and_repoints_under_its_rotation_lock() {
    let env = Env::new();
    env.tree
        .codex_roster(&["cx"], Some("cx"))
        .codex("cx")
        .codex_symlink("cx");
    let store = ino(&env.p(".clauth/profiles/cx/auth.json"));
    let s = survey(&Options::default());
    assert_eq!(s.codex.verdict, "relink");
    let relink_seq = s
        .plan
        .expect("plan")
        .entries
        .iter()
        .find(|e| e.op == Op::MoveRelink)
        .expect("relink")
        .seq;
    let lock = env.p(".clauth/rotation-locks/cx.lock");
    let (tx, rx) = std::sync::mpsc::channel();
    env.seams.set(|s| {
        s.before_step = Some(Box::new(move |seq| {
            if seq == relink_seq {
                let held = std::fs::File::open(&lock)
                    .map(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
                    .unwrap_or(false);
                let _ = tx.send(held);
            }
        }));
    });
    run_ok();
    assert_eq!(
        rx.try_recv(),
        Ok(true),
        "the rotation lock was not held at the relink"
    );
    let dst = tstore(&env, "cx", "auth.json");
    assert_eq!(ino(&dst), store);
    assert_eq!(
        std::fs::read_link(env.p(".codex/auth.json")).expect("codex link"),
        dst
    );
    let codex =
        std::fs::read_to_string(env.p(".tollgate/codex-profiles.toml")).expect("codex roster");
    assert!(codex.contains("active_profile = \"cx\""), "{codex}");
}

/// Test 36. A regular `~/.codex/auth.json` carrying a store's chain is a
/// second carrier: refused.
#[test]
fn a_codex_regular_copy_of_a_store_chain_is_refused() {
    let env = Env::new();
    env.tree
        .codex_roster(&["cx"], Some("cx"))
        .codex("cx")
        .codex_regular_copy("cx");
    let s = survey(&Options::default());
    let b = s
        .blockers
        .iter()
        .find(|b| b.code == "codex_second_carrier")
        .expect("refused");
    assert_eq!(
        b.message,
        "~/.codex/auth.json is a copy of profile 'cx''s chain; relink it with 'clauth' first"
    );
}

/// Test 37. An independent codex login (this machine's case) is untouched.
#[test]
fn an_independent_codex_login_is_untouched() {
    let env = Env::new();
    env.tree
        .codex_roster(&["cx"], None)
        .codex("cx")
        .codex_independent();
    let before = env.snapshot().get(".codex/auth.json").cloned();
    assert_eq!(survey(&Options::default()).codex.verdict, "untouched");
    run_ok();
    assert_eq!(env.snapshot().get(".codex/auth.json").cloned(), before);
}

/// A diverged slot adopted onto a profile that stores no login at all (an
/// api-key profile): the live inode becomes its store, and a rollback puts
/// that same inode back as the regular slot it was.
#[test]
fn an_adopted_slot_with_no_store_to_replace_rolls_back_to_the_regular_slot() {
    let env = Env::new();
    env.tree
        .roster(&["k"], Some("k"))
        .api_key("k")
        .live_regular_diverged("k");
    let live = ino(&slot(&env));
    let bytes = std::fs::read(slot(&env)).expect("slot");
    let adopt = Options {
        adopt_live: true,
        ..Options::default()
    };
    run(&adopt).expect("captured");
    assert_eq!(ino(&tstore(&env, "k", "credentials.json")), live);
    rollback(&Options::default()).expect("rolls back");
    let meta = std::fs::symlink_metadata(slot(&env)).expect("slot");
    assert!(
        meta.file_type().is_file(),
        "the slot is the regular file again"
    );
    assert_eq!(ino(&slot(&env)), live);
    assert_eq!(std::fs::read(slot(&env)).expect("slot"), bytes);
    assert!(!env.p(".tollgate/profiles/k").exists());
}

/// Review lens credentials #1. A regular slot holding ANOTHER stored
/// profile's login is never adopted onto the active one (that would leave
/// the chain in two stores), not even under `--adopt-live`; a slot older
/// than the store it would supersede is refused under `--adopt-live` too.
#[test]
fn adopt_live_never_moves_another_profiles_login_or_a_spent_one() {
    let env = Env::new();
    env.tree
        .roster(&["a", "b"], Some("a"))
        .oauth("a")
        .oauth("b");
    let b_body = std::fs::read_to_string(env.p(".clauth/profiles/b/credentials.json")).expect("b");
    std::fs::write(slot(&env), &b_body).expect("slot holds b");
    let adopt = Options {
        adopt_live: true,
        ..Options::default()
    };
    for opts in [Options::default(), adopt.clone()] {
        let s = survey(&opts);
        assert!(
            codes(&s).contains(&"live_is_other_profile".to_string()),
            "{:?}",
            codes(&s)
        );
        assert!(!codes(&s).contains(&"claude_live_diverged".to_string()));
    }
    let before = env.snapshot();
    assert!(run(&adopt).is_err(), "the import refuses");
    assert_eq!(
        env.snapshot().without(is_bookkeeping).0,
        before.without(is_bookkeeping).0
    );
    assert_eq!(
        files_with(&env, &fixture_refresh("b", 1)).len(),
        2,
        "untouched"
    );

    drop(env);
    let env = Env::new();
    env.tree.roster(&["a"], Some("a")).oauth("a");
    let mut spent: serde_json::Value =
        serde_json::from_str(&fixture_oauth_body("a", 5)).expect("body");
    spent["claudeAiOauth"]["expiresAt"] = 1_000_i64.into();
    regular_slot(&env, &spent);
    assert!(codes(&survey(&adopt)).contains(&"live_older_than_store".to_string()));
    assert!(codes(&survey(&Options::default())).contains(&"claude_live_diverged".to_string()));
}

/// Review lens credentials #1 and #6. `--adopt-live` keeps the store it
/// supersedes in the profile's `quarantine/` (never unlinked, journaled),
/// and a rollback puts both back: the slot as the regular file it was, the
/// store at its path, both on their own inodes and byte for byte — even
/// when the slot's `mcpOAuth` differs from the store's.
#[test]
fn an_adopted_slot_keeps_the_superseded_store_and_rolls_back_byte_identical() {
    let env = Env::new();
    env.tree
        .roster(&["a"], Some("a"))
        .oauth("a")
        .live_regular_diverged("a");
    let mut live: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(slot(&env)).expect("slot")).expect("body");
    live["mcpOAuth"]["srv"]["accessToken"] = "FIXTURE-MCP-slot-only".into();
    regular_slot(&env, &live);
    let store = env.p(".clauth/profiles/a/credentials.json");
    let (slot_ino, slot_bytes) = (ino(&slot(&env)), std::fs::read(slot(&env)).expect("slot"));
    let (store_ino, store_bytes) = (ino(&store), std::fs::read(&store).expect("store"));
    let adopt = Options {
        adopt_live: true,
        ..Options::default()
    };
    run(&adopt).expect("captured");
    let q = tstore(&env, "a", "quarantine/credentials.json.superseded");
    assert_eq!(ino(&q), store_ino, "the superseded store is kept");
    assert_eq!(mode(q.parent().expect("dir")), 0o700);
    assert_eq!(ino(&tstore(&env, "a", "credentials.json")), slot_ino);
    let capture = env
        .journal()
        .main
        .into_iter()
        .find(|e| e.op == Op::Capture)
        .expect("capture");
    assert_eq!(capture.after.quarantine.as_deref(), Some(q.as_path()));
    rollback(&Options::default()).expect("rolls back");
    let meta = std::fs::symlink_metadata(slot(&env)).expect("slot");
    assert!(meta.file_type().is_file(), "the slot is regular again");
    assert_eq!(ino(&slot(&env)), slot_ino);
    assert_eq!(std::fs::read(slot(&env)).expect("slot"), slot_bytes);
    assert_eq!(ino(&store), store_ino);
    assert_eq!(std::fs::read(&store).expect("store"), store_bytes);
    assert!(!env.p(".tollgate/profiles/a").exists());
}

/// Review lens credentials #4. On a session-token profile the Same regular
/// slot's copy is kept in the profile's quarantine (its `mcpOAuth` and
/// refresh token exist nowhere else), and a rollback puts that inode back
/// as the regular slot.
#[test]
fn a_relinked_session_token_slot_is_quarantined_and_restored_by_rollback() {
    let env = Env::new();
    env.tree.roster(&["s"], Some("s")).static_token("s");
    let live = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": fixture_access("s", 9),
            "refreshToken": "FIXTURE-RT-live-only",
        },
        "mcpOAuth": {"srv": {"accessToken": "FIXTURE-MCP-LIVE-ONLY"}}
    });
    regular_slot(&env, &live);
    let (slot_ino, slot_bytes) = (ino(&slot(&env)), std::fs::read(slot(&env)).expect("slot"));
    run_ok();
    let q = tstore(&env, "s", "quarantine/credentials.json.live");
    assert_eq!(ino(&q), slot_ino, "the live copy is kept, not unlinked");
    assert_eq!(files_with(&env, "FIXTURE-MCP-LIVE-ONLY").len(), 1);
    assert_eq!(
        std::fs::read_link(slot(&env)).expect("link"),
        tstore(&env, "s", "session-token.json")
    );
    rollback(&Options::default()).expect("rolls back");
    let meta = std::fs::symlink_metadata(slot(&env)).expect("slot");
    assert!(meta.file_type().is_file(), "the slot is regular again");
    assert_eq!(ino(&slot(&env)), slot_ino);
    assert_eq!(std::fs::read(slot(&env)).expect("slot"), slot_bytes);
}
