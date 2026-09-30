use super::*;
use std::sync::mpsc::channel;

use crate::testutil::HomeSandbox;

/// A row for a session that never ran, with every field pinned so the tests
/// assert exact values rather than "something was written".
fn row(session_id: &str, profile: &str) -> LiveSession {
    LiveSession {
        session_id: session_id.to_string(),
        start_profile: profile.to_string(),
        harness: crate::harness::Harness::Claude,
        pid: 4242,
        started_at: 1_700_000_000_000,
        cwd: Some(PathBuf::from("/w/proj")),
        isolated: false,
        follows_chain: false,
        intended_member: None,
        chain_cursor: None,
        current_member: None,
        last_swap_at: None,
        launch_store: None,
        executor: None,
        launch_class: None,
        key_generation: None,
        committed_at: None,
        swap_refusal: None,
        relaunch_capable: false,
        relaunched_from: None,
        intended_at: None,
    }
}

/// A row written by a tollgate that predates the opt-in field must not read as
/// opted IN on upgrade — the decision leg would then move EVERY live session off
/// the account it launched on.
#[test]
fn a_row_predating_the_opt_in_key_deserializes_as_not_following_the_chain() {
    let pre_upgrade = br#"{"session_id":"4242-0","start_profile":"work","pid":4242,
        "started_at":1700000000000,"cwd":"/w/proj","isolated":false}"#;

    let row: LiveSession = serde_json::from_slice(pre_upgrade).expect("parse a pre-upgrade row");

    assert!(
        !row.follows_chain,
        "a row with no `follows_chain` key must default to opted OUT"
    );
    assert_eq!(
        row.harness,
        crate::harness::Harness::Claude,
        "a row with no `harness` key predates the axis, so it is a claude row"
    );
}

/// The tag survives the registry round-trip and spells itself lowercase, the
/// stable on-disk form a mixed-version window reads back.
#[test]
fn a_codex_row_round_trips_its_harness_tag() {
    let _home = HomeSandbox::new();
    let mut written = row("4242-0", "cx");
    written.harness = crate::harness::Harness::Codex;

    register(&written).expect("register");

    let listed = list().pop().expect("one row");
    assert_eq!(listed, written, "the tag must survive the round-trip");
    let raw = std::fs::read_to_string(
        crate::profile::tollgate_dir()
            .expect("tollgate dir")
            .join("live_sessions")
            .join("4242-0.json"),
    )
    .expect("read row file");
    assert!(
        raw.contains("\"harness\":\"codex\""),
        "the on-disk spelling is lowercase: {raw}"
    );
}

#[test]
fn register_then_list_returns_the_row() {
    let _home = HomeSandbox::new();
    let written = row("4242-0", "work");

    register(&written).expect("register");

    assert_eq!(list(), vec![written], "list must round-trip the exact row");
}

#[test]
fn each_writers_update_preserves_the_others_fields() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");

    update_as_daemon("4242-0", |d| {
        d.set_intended_member("kerry");
        d.set_chain_cursor(2);
    })
    .expect("daemon update");
    update_as_session("4242-0", |s| {
        s.set_current_member("work");
        s.set_last_swap_at(1_700_000_009_000);
    })
    .expect("session update");

    let after = list().pop().expect("one row");
    assert_eq!(after.intended_member.as_deref(), Some("kerry"));
    assert_eq!(after.chain_cursor, Some(2));
    assert_eq!(after.current_member.as_deref(), Some("work"));
    assert_eq!(after.last_swap_at, Some(1_700_000_009_000));

    // ...and the other direction: a daemon write after a session write must not
    // drop what the session put there.
    update_as_daemon("4242-0", |d| d.set_intended_member("filip")).expect("second daemon update");

    let after = list().pop().expect("one row");
    assert_eq!(after.intended_member.as_deref(), Some("filip"));
    assert_eq!(
        after.current_member.as_deref(),
        Some("work"),
        "the daemon's write clobbered the session's field"
    );
    assert_eq!(after.last_swap_at, Some(1_700_000_009_000));
    assert_eq!(after.chain_cursor, Some(2));
}

/// A delegate's row is registered by the `tollgate mcp` that spawns it (that
/// process's `std::process::id()` is what register reads) and re-keyed onto the
/// delegate child right after spawn. `set_pid` is the mutator behind the
/// re-key, and it must move nothing else — the daemon's decision fields least
/// of all.
#[test]
fn set_pid_rekeys_the_row_and_touches_nothing_else() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");
    update_as_daemon("4242-0", |d| d.set_intended_member("kerry")).expect("daemon update");

    update_as_session("4242-0", |s| s.set_pid(7777)).expect("session update");

    let after = list().pop().expect("one row");
    assert_eq!(after.pid, 7777, "the row names the re-keyed process");
    assert_eq!(
        after.session_id, "4242-0",
        "the id is not part of the re-key"
    );
    assert_eq!(
        after.start_profile, "work",
        "the launch member is untouched"
    );
    assert_eq!(
        after.intended_member.as_deref(),
        Some("kerry"),
        "the daemon's field survives the re-key"
    );
    assert_eq!(after.current_member, None);
    assert_eq!(after.cwd.as_deref(), Some(Path::new("/w/proj")));
}

/// THE LOST-UPDATE TEST. The load has to happen INSIDE the state lock, not just
/// the store: a row read before a swap and written after silently reverts
/// whatever the other writer put there in between. Thread A parks inside its
/// closure while holding the lock; B contends. `with_state_lock` serializes them,
/// so B reloads A's stored row and the file must end up carrying BOTH writes.
#[test]
fn a_concurrent_daemon_write_is_not_lost_under_a_parked_session_write() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");

    let (inside_tx, inside_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();

    let session_writer = std::thread::spawn(move || {
        update_as_session("4242-0", |s| {
            inside_tx.send(()).expect("signal inside");
            release_rx.recv().expect("await release");
            s.set_current_member("work");
        })
    });
    inside_rx.recv().expect("A reached its closure");

    let daemon_writer = std::thread::spawn(|| {
        update_as_daemon("4242-0", |d| {
            d.set_intended_member("kerry");
            d.set_chain_cursor(7);
        })
    });
    // B is contending for the lock (or about to be); let it get there before A
    // stores, so a load taken outside the lock would read the pre-A row.
    std::thread::sleep(std::time::Duration::from_millis(300));
    release_tx.send(()).expect("release A");

    session_writer
        .join()
        .expect("session thread panicked")
        .expect("session update");
    daemon_writer
        .join()
        .expect("daemon thread panicked")
        .expect("daemon update");

    let after = list().pop().expect("one row");
    assert_eq!(
        after.current_member.as_deref(),
        Some("work"),
        "the session's write was lost — the daemon stored a row it loaded before it"
    );
    assert_eq!(
        after.intended_member.as_deref(),
        Some("kerry"),
        "the daemon's write was lost — the session stored a row it loaded before it"
    );
    assert_eq!(after.chain_cursor, Some(7));
}

#[test]
fn unregister_removes_the_row_and_is_idempotent() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "work")).expect("register");
    register(&row("4242-1", "kerry")).expect("register sibling");

    unregister("4242-0").expect("unregister");

    let left: Vec<String> = list().into_iter().map(|r| r.session_id).collect();
    assert_eq!(left, vec!["4242-1".to_string()]);

    unregister("4242-0").expect("a row already gone is not an error");
}

#[test]
fn an_update_of_a_missing_row_names_the_id() {
    let _home = HomeSandbox::new();

    let err = update_as_daemon("4242-9", |d| d.set_chain_cursor(1))
        .expect_err("a missing row must not silently no-op");

    assert!(
        format!("{err:#}").contains("4242-9"),
        "the error must name the id, got: {err:#}"
    );
}

/// A path join must never take a separator or a `..` from an id read back off
/// disk or handed in by a later phase's decision leg.
#[test]
fn a_malformed_session_id_is_refused() {
    let _home = HomeSandbox::new();

    for bad in ["../escape", "4242-0/x", "", "isolated", "4242"] {
        assert!(
            unregister(bad).is_err(),
            "{bad:?} must be refused as a session id"
        );
    }
}

// ── live tally ───────────────────────────────────────────────────────────────

/// `current_member` is written only by a session's FIRST swap, so a session that
/// never moved — every pinned one, and every opted-in one before it swaps — is
/// running as the account it launched on. Reading `current_member` alone leaves
/// the overwhelmingly common case attributed to nobody.
#[test]
fn a_session_that_never_swapped_counts_on_the_account_it_launched_on() {
    let tally = LiveTally::from_live_rows([row("4242-0", "work")]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1
    );
}

/// The other direction: once a session has swapped, the launch account is a
/// place nothing authenticates as, and counting it there is the exact defect
/// that made the Plugin tab report one child as two.
#[test]
fn a_swapped_session_counts_on_its_current_member_and_not_its_launch_one() {
    let mut swapped = row("4242-0", "work");
    swapped.current_member = Some("spare".to_string());

    let tally = LiveTally::from_live_rows([swapped]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("spare"))
            .sessions,
        1
    );
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        0
    );
}

/// `follows_chain` is what separates a session the chain can move from a pinned
/// one — both hold the account's marker and burn its window, so both are
/// counted, and only the follower earns the `⇄` the render layers put on it.
#[test]
fn only_opted_in_sessions_count_as_following_the_chain() {
    let pinned = row("4242-0", "work");
    let mut follower = row("4242-1", "work");
    follower.follows_chain = true;

    let tally = LiveTally::from_live_rows([pinned, follower]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        2
    );
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .following,
        1
    );
}

/// The card names when a session last landed here, so the newest swap onto an
/// account wins over an older one; a session that never swapped contributes no
/// stamp at all, which is also what tells the render layer no pickup lag applies.
#[test]
fn the_newest_swap_onto_an_account_is_the_one_reported() {
    let mut old = row("4242-0", "work");
    old.current_member = Some("spare".to_string());
    old.last_swap_at = Some(1_700_000_010_000);
    let mut new = row("4242-1", "work");
    new.current_member = Some("spare".to_string());
    new.last_swap_at = Some(1_700_000_020_000);
    let never = row("4242-2", "work");

    // Newest FIRST: `list()` reads rows back in readdir order, so a tally that
    // simply kept the last row it saw would agree with this fixture in ascending
    // order and disagree with production at random.
    let tally = LiveTally::from_live_rows([new, old, never]);

    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("spare"))
            .last_swap_at,
        Some(1_700_000_020_000)
    );
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .last_swap_at,
        None
    );
}

/// An account hosting nothing reports zeroes, never a missing-key panic — the
/// render layers ask about every configured account, most of which host none.
#[test]
fn an_account_with_no_sessions_tallies_empty() {
    let tally = LiveTally::from_live_rows([row("4242-0", "work")]);

    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("idle")),
        MemberSessions::default()
    );
}

/// Row GC runs from `gc_stale_runtimes` at daemon STARTUP, not per tick, so a
/// SIGKILLed session's row sits on disk for the whole daemon run. A tally that
/// trusted the file would keep showing a session that is gone.
#[test]
fn collect_drops_a_row_whose_session_is_no_longer_running() {
    let _home = HomeSandbox::new();

    let live = row("4242-0", "work");
    let dead = row("4242-1", "work");
    register(&live).expect("register the live row");
    register(&dead).expect("register the dead row");
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from(live.start_profile.clone()),
        false,
        "4242-0",
    )
    .expect("hold the live session's marker");

    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work"))
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "only the row whose marker is still held is a live session"
    );
}

/// After `tollgate delete <launch_profile> --force`, the launch profile's marker
/// dir is gone — but the session keeps running on `current_member` and holds
/// that member's marker. The probe must look at `current_member`, not
/// `start_profile`, to find it.
#[test]
fn a_swapped_session_counts_on_current_member_after_its_launch_marker_is_removed() {
    let _home = HomeSandbox::new();

    let swapped = LiveSession {
        start_profile: "work".to_string(),
        current_member: Some("spare".to_string()),
        ..row("4242-0", "work")
    };
    register(&swapped).expect("register the swapped row");
    // Hold the marker for `current_member` ("spare"), not `start_profile`
    // ("work"). The procedure `tollgate delete work --force` removed the
    // work markers, so only the spare marker exists.
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("spare"),
        false,
        "4242-0",
    )
    .expect("hold the spare member's marker");

    let config = config_with(vec![], "work");
    assert_eq!(
        LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("spare"))
            .sessions,
        1,
        "the swapped session must count on current_member when its \
         start_profile marker is absent"
    );
    assert_eq!(
        LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        0,
        "the swapped session must not count on the launch account"
    );
}

/// Mirror: a session that never swapped has no `current_member`, so the probe
/// falls back to `start_profile` — the only marker dir that exists.
#[test]
fn a_session_that_never_swapped_probes_start_profile_in_collect() {
    let _home = HomeSandbox::new();

    let never = LiveSession {
        current_member: None,
        ..row("4242-0", "work")
    };
    register(&never).expect("register the row");
    // Only the start_profile marker exists.
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("work"),
        false,
        "4242-0",
    )
    .expect("hold the start_profile marker");

    assert_eq!(
        LiveTally::collect(&config_with(vec![], "work"))
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "a session that never swapped must count on the launch account"
    );
}

// ── bare `claude` sessions ───────────────────────────────────────────────────

fn oauth_profile(name: &str, refresh: &str) -> crate::profile::Profile {
    let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
    profile.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: format!("at-{name}"),
            refresh_token: Some(refresh.to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    profile
}

fn config_with(profiles: Vec<crate::profile::Profile>, active: &str) -> AppConfig {
    AppConfig {
        state: crate::profile::AppState {
            active_profile: Some(active.into()),
            profiles: profiles.iter().map(|p| p.name.clone()).collect(),
            ..Default::default()
        },
        profiles,
    }
}

/// What the credential link the bare `claude` reads actually holds.
fn write_linked_credentials(refresh: &str) {
    let dir = crate::profile::claude_dir().expect("claude dir");
    std::fs::create_dir_all(&dir).expect("mkdir .claude");
    std::fs::write(
        dir.join(".credentials.json"),
        format!(r#"{{"claudeAiOauth":{{"accessToken":"at","refreshToken":"{refresh}"}}}}"#),
    )
    .expect("write linked credentials");
}

/// A bare `claude` — started without `tollgate start` — burns the same account
/// window a supervised session does, and the daemon's global auto-switch really
/// does move it (it repoints the very link the session re-reads), so it counts as
/// following the chain too.
#[test]
fn a_held_bare_marker_counts_on_the_account_the_credential_link_resolves_to() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![oauth_profile("work", "rt-work")], "work");
    write_linked_credentials("rt-work");

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    assert_eq!(
        LiveTally::collect(&config).member(&crate::profile::ProfileName::from("work")),
        MemberSessions {
            sessions: 1,
            following: 1,
            last_swap_at: None,
            swapping: 0,
        },
    );
}

/// The link is the fact; `active_profile` is a wish. Under a divergence the bare
/// `claude` authenticates as whatever the link resolves to, so that is where its
/// window is spent and where it must be counted.
#[test]
fn bare_attribution_follows_the_credential_link_not_the_active_profile() {
    let _home = HomeSandbox::new();
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            oauth_profile("spare", "rt-spare"),
        ],
        "work",
    );
    write_linked_credentials("rt-spare");

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("spare"))
            .sessions,
        1
    );
    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("work")),
        MemberSessions::default(),
        "the account the link does NOT resolve to hosts nothing"
    );
}

/// The fd closing IS the release, which is what makes this survive SIGKILL: a
/// bare session runs no tollgate code and has no teardown path to unregister from.
#[test]
fn releasing_a_bare_marker_stops_it_counting() {
    let _home = HomeSandbox::new();
    let config = config_with(vec![oauth_profile("work", "rt-work")], "work");
    write_linked_credentials("rt-work");

    let bare = crate::runtime::register_bare_session().expect("hold a bare marker");
    assert_eq!(
        LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "positive control: the marker counts while it is held"
    );

    drop(bare);

    // The liveness probe is fail-ALIVE (any `try_lock` I/O error reads as alive),
    // so one transient error under a parallel suite can inflate a single reading;
    // only a persistently-live reading is a regression. Same hardening as
    // `runtime`'s `has_live_session_true_when_any_session_alive`.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let settled = loop {
        if LiveTally::collect(&config)
            .member(&crate::profile::ProfileName::from("work"))
            .sessions
            == 0
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(settled, "a released bare marker must stop counting");
}

/// The tally is read by a TUI that may itself be running inside a `tollgate start`
/// session, where `CLAUDE_CONFIG_DIR` names its own runtime tree. That env
/// describes the READER, so letting it reach the attribution claims every bare
/// `claude` on the box for the reader's profile. Pinning the resolver in
/// isolation is not enough — this pins which one the fold calls.
#[test]
fn bare_attribution_ignores_the_readers_own_config_dir() {
    let home = HomeSandbox::new();
    let config = config_with(
        vec![
            oauth_profile("work", "rt-work"),
            oauth_profile("spare", "rt-spare"),
        ],
        "work",
    );
    write_linked_credentials("rt-work");
    let reader_runtime = home
        .home()
        .join(".tollgate")
        .join("profiles")
        .join("spare")
        .join("runtime-4242-0");
    let _config_dir = crate::testutil::ConfigDirSandbox::new(&home, &reader_runtime);

    let _bare = crate::runtime::register_bare_session().expect("hold a bare marker");

    let tally = LiveTally::collect(&config);
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("work"))
            .sessions,
        1,
        "the bare session belongs to the account the global link resolves to"
    );
    assert_eq!(
        tally.member(&crate::profile::ProfileName::from("spare")),
        MemberSessions::default(),
        "the READER's own runtime profile hosts nothing"
    );
}

// ── executor B fields (hot-swap spec part 1) ─────────────────────────────────

// 15
#[test]
fn a_row_predating_the_fields_reads_as_oauth_generation_zero() {
    let pre = br#"{"session_id":"4242-0","start_profile":"work","pid":4242,
        "started_at":1700000000000,"cwd":"/w/proj","isolated":false}"#;
    let row: LiveSession = serde_json::from_slice(pre).expect("parse a pre-hot-swap row");
    assert_eq!(row.executor, None);
    assert_eq!(row.executor(), crate::hot_swap::Executor::Oauth);
    assert_eq!(row.key_generation, None);
    assert_eq!(row.key_generation.unwrap_or(0), 0);
    assert_eq!(row.launch_class, None);
    assert_eq!(row.swap_refusal, None);
    assert!(!row.relaunch_capable);
    assert_eq!(row.relaunched_from, None);
    // And the fields do not appear on a row that has none of them, so a row
    // written now reads byte-compatibly by an older tollgate.
    let bytes = serde_json::to_string(&row).expect("serialise");
    for key in [
        "executor",
        "launch_class",
        "key_generation",
        "committed_at",
        "swap_refusal",
        "relaunch_capable",
        "relaunched_from",
    ] {
        assert!(!bytes.contains(key), "{key} must be omitted when unset");
    }
}

// 16
#[test]
fn bump_key_generation_is_monotonic_across_fresh_loads() {
    let _home = HomeSandbox::new();
    let written = row("4242-0", "a").with_executor(crate::hot_swap::Executor::ApiKey, None);
    assert_eq!(written.key_generation, Some(0));
    assert_eq!(written.current_member.as_deref(), Some("a"));
    register(&written).expect("register");
    let mut seen = Vec::new();
    for _ in 0..3 {
        update_as_session("4242-0", |f| seen.push(f.bump_key_generation())).expect("bump");
    }
    assert_eq!(seen, vec![1, 2, 3]);
    assert_eq!(get("4242-0").expect("row").key_generation, Some(3));
}

// 17
#[test]
fn a_daemon_write_preserves_every_session_owned_hot_swap_field() {
    let _home = HomeSandbox::new();
    let class = crate::hot_swap::LaunchClass::of(
        &crate::testutil::api_key_profile("a", "https://openrouter.ai/api", "sk-a"),
        true,
    );
    let written = row("4242-0", "a")
        .with_executor(crate::hot_swap::Executor::ApiKey, class)
        .with_relaunch(true, Some("4000-1".to_string()));
    register(&written).expect("register");
    update_as_session("4242-0", |f| {
        f.set_current_member("b");
        f.bump_key_generation();
        f.set_committed_at(1_234);
        f.set_swap_refusal(SwapRefusal {
            member: "c".to_string(),
            code: "class_differs:endpoint".to_string(),
            text: "a different endpoint".to_string(),
            at_ms: 1_235,
        });
    })
    .expect("session write");
    let before = get("4242-0").expect("row");
    update_as_daemon("4242-0", |f| f.set_intended_member("d")).expect("daemon write");
    let after = get("4242-0").expect("row");
    assert_eq!(after.intended_member.as_deref(), Some("d"));
    assert_eq!(
        LiveSession {
            intended_member: None,
            ..after
        },
        before,
        "the daemon's write carried every session-owned field through"
    );
}

// 18
#[test]
fn list_reads_only_session_id_json_stems() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "a")).expect("register");
    let dir = crate::profile::tollgate_dir()
        .expect("tollgate dir")
        .join("live_sessions");
    // Sidecars and strays that WOULD parse as a row if read: `list` must skip
    // them by name, not by a parse failure.
    let parsable = serde_json::to_vec(&row("4242-1", "b")).expect("row bytes");
    for name in [
        "4242-0.helper",
        "4242-0.helper.lock",
        "4242-0.relaunch",
        "4242-0.relaunch.taken",
        "4242-0.relaunch.cancel",
        "4242-0.relaunch.result",
        "x.json",
        "4242-0.json.tmp",
    ] {
        std::fs::write(dir.join(name), &parsable).expect("write sidecar");
    }
    let rows = list();
    assert_eq!(rows.len(), 1, "only 4242-0.json is a row");
    assert_eq!(rows[0].session_id, "4242-0");
}

/// Teardown's sidecar sweep keeps `.relaunch.taken` and — review lens
/// concurrency #10 — `.relaunch.result` (the CLI's answer) on the relaunch
/// path only.
#[test]
fn remove_sidecars_keeps_relaunch_taken_only_when_asked() {
    let _home = HomeSandbox::new();
    register(&row("4242-0", "a")).expect("register");
    let paths: Vec<PathBuf> = [
        "helper",
        "helper.lock",
        "relaunch",
        "relaunch.lock",
        "relaunch.taken",
        "relaunch.result",
    ]
    .iter()
    .map(|s| sidecar_path("4242-0", s).expect("path"))
    .collect();
    for p in &paths {
        std::fs::write(p, b"{}").expect("write");
    }
    remove_sidecars("4242-0", KeepSidecars::RelaunchTaken);
    let left: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
    assert_eq!(left, vec![false, false, false, false, true, true]);
    remove_sidecars("4242-0", KeepSidecars::Nothing);
    assert!(paths.iter().all(|p| !p.exists()));
    assert!(get("4242-0").is_some(), "the row itself is `unregister`'s");
}

#[test]
fn relaunch_staging_names_are_visible_to_sidecar_gc() {
    let _home = HomeSandbox::new();
    let dir = crate::profile::tollgate_dir()
        .expect("home")
        .join("live_sessions");
    std::fs::create_dir_all(&dir).expect("dir");
    let stage = dir.join(".4242-0.relaunch.99.request-id");
    std::fs::write(&stage, b"staged").expect("stage");
    let found = list_sidecars();
    assert!(
        found
            .iter()
            .any(|s| s.path == stage && s.session_id == "4242-0")
    );
}

/// The tally counts a committed-not-served B session on the SERVED member and
/// as swapping.
#[test]
fn the_tally_counts_a_swapping_session_on_its_served_member() {
    let _home = HomeSandbox::new();
    let mut committed = row("4242-0", "a").with_executor(crate::hot_swap::Executor::ApiKey, None);
    committed.current_member = Some("b".to_string());
    committed.key_generation = Some(1);
    committed.committed_at = Some(1_000);
    let tally = LiveTally::of([committed.clone()]);
    let a = tally.member(&crate::profile::ProfileName::from("a"));
    assert_eq!((a.sessions, a.swapping), (1, 1));
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("b"))
            .sessions,
        0
    );
    crate::hot_swap::write_ack_for_test(
        "4242-0",
        &crate::hot_swap::HelperAck {
            version: 1,
            generation: 1,
            member: Some("b".to_string()),
            served_at_ms: Some(1_100),
            last_failure: None,
            launch_class: None,
        },
    );
    let tally = LiveTally::of([committed]);
    let b = tally.member(&crate::profile::ProfileName::from("b"));
    assert_eq!((b.sessions, b.swapping), (1, 0));
}

#[test]
fn a_generation_zero_helper_failure_counts_as_swapping() {
    let _home = HomeSandbox::new();
    let row = row("4242-0", "a").with_executor(crate::hot_swap::Executor::ApiKey, None);
    crate::hot_swap::write_ack_for_test(
        "4242-0",
        &crate::hot_swap::HelperAck {
            version: 1,
            generation: 0,
            member: None,
            served_at_ms: None,
            last_failure: Some(crate::hot_swap::HelperFailure {
                generation: 0,
                code: "no_key".into(),
                at_ms: 1,
            }),
            launch_class: None,
        },
    );
    let tally = LiveTally::of([row]);
    assert_eq!(
        tally
            .member(&crate::profile::ProfileName::from("a"))
            .swapping,
        1
    );
}
