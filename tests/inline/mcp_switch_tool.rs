#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

//! Guard coverage for the MCP `switch_profile` tool itself (the
//! `TollgateServer::switch_profile` seam, not the `switch_profile_noninteractive`
//! action it wraps). An unknown or
//! wrong-case profile name must be rejected BEFORE any credential mutation:
//! without the canonical-name guard the raw arg reaches `link_profile_credentials`,
//! which removes the live `~/.claude/.credentials.json` symlink and creates no
//! replacement, leaving the global session credential-less.

use super::*;

use crate::claude::force_link_profile_credentials;
use crate::profile::{
    AppState, ClaudeCredentials, OAuthToken, Profile, claude_dir, read_json_file, save_app_state,
    save_profile,
};
use crate::testutil::HomeSandbox;

/// Seed one cleanly-linked active profile on disk — profile creds, a symlinked
/// live `~/.claude/.credentials.json`, and persisted app state — so the tool's
/// own `load_config` sees a real active session.
fn seed_active_linked() {
    let mut p = Profile::new("active".to_string(), None, None);
    p.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "stored-a".to_string(),
            refresh_token: Some("stored-r".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    save_profile(&p).expect("save profile");
    force_link_profile_credentials(&crate::profile::ProfileName::from("active"))
        .expect("link active");

    let state = AppState {
        active_profile: Some("active".into()),
        profiles: vec!["active".into()],
        ..Default::default()
    };
    save_app_state(&state).expect("save state");
}

/// Drive the async `switch_profile` tool on a current-thread runtime.
fn call_switch(name: &str) -> CallToolResult {
    let server = TollgateServer::new();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        server
            .switch_profile(Parameters(SwitchArgs {
                name: name.to_string(),
                session: None,
            }))
            .await
    })
    .expect("switch_profile returns a tool result, never a transport error")
}

#[test]
fn unknown_target_is_rejected_without_stripping_live_creds() {
    let _home = HomeSandbox::new();
    seed_active_linked();

    let live = claude_dir().expect("claude dir").join(".credentials.json");
    assert!(
        live.symlink_metadata().is_ok(),
        "precondition: live credentials are linked",
    );

    let result = call_switch("ghost");
    assert_eq!(
        result.is_error,
        Some(true),
        "an unknown profile name must be a tool error",
    );
    assert!(
        live.symlink_metadata().is_ok(),
        "the live credentials symlink must survive a failed switch to an unknown name",
    );
}

/// Seed a cleanly-linked active profile plus a second stored `target`, both
/// registered — a non-diverged setup where switching to `target` succeeds.
fn seed_active_plus_target() {
    seed_active_linked();

    let mut target = Profile::new("target".to_string(), None, None);
    target.credentials = Some(ClaudeCredentials {
        claude_ai_oauth: Some(OAuthToken {
            access_token: "target-a".to_string(),
            refresh_token: Some("target-r".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    });
    save_profile(&target).expect("save target");

    let state = AppState {
        active_profile: Some("active".into()),
        profiles: vec!["active".into(), "target".into()],
        ..Default::default()
    };
    save_app_state(&state).expect("save state");
}

/// The reserved running record a switch test seeds a job from, in the shape a
/// real reserve writes. The owner fields default to the legacy ownerless shape;
/// a test that poses an owned job overrides them.
fn switch_running_spec(job_id: &str, profile: &str, started_at: u64) -> jobs::RunningSpec {
    jobs::RunningSpec {
        job_id: job_id.to_string(),
        profile: profile.to_string(),
        started_at,
        recorded_at: started_at,
        timeout_secs: 0,
        endpoint: None,
        provider: None,
        isolated: false,
        idle_secs: None,
        kind: jobs::RecordKind::Collectable,
        owner_pid: 0,
        owner_started_at: 0,
    }
}

/// Row 4's demanded shape: a profile switch with live delegates under this
/// server refuses BEFORE the mutation, naming the jobs and the fix — a switch
/// re-pins the session's account, and the jobs' monitor handles die with this
/// server, parking them beyond the next session's monitor (the DS3→DS5 case).
#[test]
fn a_switch_with_a_live_delegate_refuses_before_the_mutation() {
    let _home = HomeSandbox::new();
    seed_active_plus_target();

    let _marker = jobs::hold_server_marker().expect("hold the server marker");
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: std::process::id(),
        owner_started_at: jobs::server_started_at(),
        ..switch_running_spec("d-switch-live-0", "work", crate::usage::now_ms())
    })
    .unwrap();

    let result = call_switch("target");
    assert_eq!(
        result.is_error,
        Some(true),
        "a live delegate must refuse the switch"
    );
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("switch refusal text");
    assert!(
        text.contains("d-switch-live-0") && text.contains("cancel or collect"),
        "the refusal names the held jobs and the fix: {text}"
    );

    // The refusal runs BEFORE the mutation: the live link still names the
    // original active profile, so nothing was half-switched.
    let live: ClaudeCredentials =
        read_json_file(&claude_dir().expect("claude dir").join(".credentials.json"))
            .expect("read live creds");
    assert_eq!(
        live.refresh_token(),
        Some("stored-r"),
        "a refused switch leaves the live link untouched"
    );
}

/// The guard scopes to THIS server's own live runs: a dead owner's parked job
/// and a foreign live server's job are not this switch's to protect — that
/// server's own monitor still reaches them. A done job is no guard either.
#[test]
fn the_live_jobs_guard_scopes_to_this_servers_own_runs() {
    let _home = HomeSandbox::new();
    let now = crate::usage::now_ms();

    // A parked job (owner dead) is not protected by a refusal: the switch
    // cannot make its fate worse, and nothing here owns it to protect.
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: 42_424,
        ..switch_running_spec("d-switch-parked-0", "work", now)
    })
    .unwrap();
    assert!(
        super::live_jobs_guard(now).is_none(),
        "a dead owner's job is not this switch's to protect"
    );
    jobs::remove("d-switch-parked-0");

    // A foreign live server's job stays reachable through that server.
    let _foreign = jobs::hold_foreign_server_marker_for_test(999_999);
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: 999_999,
        owner_started_at: 1_700_000_000_000,
        ..switch_running_spec("d-switch-foreign-0", "work", now)
    })
    .unwrap();
    assert!(
        super::live_jobs_guard(now).is_none(),
        "a foreign live job is that server's, not this switch's"
    );
    jobs::remove("d-switch-foreign-0");

    // This server's own live run is the one the refusal protects.
    let _mine = jobs::hold_server_marker().expect("hold the server marker");
    jobs::write_running(&jobs::RunningSpec {
        owner_pid: std::process::id(),
        owner_started_at: jobs::server_started_at(),
        ..switch_running_spec("d-switch-mine-0", "work", now)
    })
    .unwrap();
    let guard = super::live_jobs_guard(now).expect("this server's own run guards the switch");
    assert!(
        guard.contains("d-switch-mine-0"),
        "the guard names the held job: {guard}"
    );

    // A done job holds no run to strand.
    jobs::remove("d-switch-mine-0");
    jobs::write_done(
        "d-switch-mine-0",
        "work",
        1,
        None,
        None,
        false,
        serde_json::json!({"is_error": false, "result": "ok"}),
    )
    .unwrap();
    assert!(
        super::live_jobs_guard(now).is_none(),
        "a done job is no guard"
    );
}

#[test]
fn valid_switch_repoints_active_through_the_blocking_task() {
    let _home = HomeSandbox::new();
    seed_active_plus_target();

    // Exercises the `spawn_blocking` wrap end-to-end (the reject test returns
    // before it). A clean switch must succeed and repoint the live link.
    let result = call_switch("target");
    assert_ne!(
        result.is_error,
        Some(true),
        "a clean switch to a known profile must succeed through the spawn_blocking wrap",
    );

    let live: ClaudeCredentials =
        read_json_file(&claude_dir().expect("claude dir").join(".credentials.json"))
            .expect("read live creds");
    assert_eq!(
        live.refresh_token(),
        Some("target-r"),
        "the switch ends with the active link pointing at target's stored creds",
    );

    // The description promises "the reply says which case this session is in":
    // the session-effect note rides the success arm through the same renderer
    // the init block uses. Only the lead is pinned — which variant this
    // process earns depends on the runner's own `CLAUDE_CONFIG_DIR`.
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("switch reply text");
    assert!(
        text.contains("\n\nswitch_profile & this session: "),
        "a successful switch names what it does to THIS session: {text}",
    );
}

// ── `session`: one live session, not the global link (hot-swap spec §2.3) ───

mod session_form {
    use super::*;
    use crate::testutil::{ConfigDirSandbox, api_key_profile, write_api_key_profile};
    use std::time::Duration;

    const OR: &str = "https://openrouter.ai/api";

    /// A live executor-B session of `start` with its marker held, plus the
    /// target's marker (what the real commit stamps, which the liveness probe
    /// reads once `current_member` moves).
    fn b_session(sid: &str, start: &str, target: &str) -> Vec<std::fs::File> {
        let launch = api_key_profile(start, OR, "sk-start");
        write_api_key_profile(&launch);
        write_api_key_profile(&api_key_profile(target, OR, "sk-target"));
        let row = crate::testutil::live_row(sid, start).with_executor(
            crate::hot_swap::Executor::ApiKey,
            crate::hot_swap::LaunchClass::of(&launch, true),
        );
        crate::live_sessions::register(&row).expect("register");
        [start, target]
            .iter()
            .map(|p| {
                crate::runtime::hold_session_row_marker(&ProfileName::from(*p), false, sid)
                    .expect("marker")
            })
            .collect()
    }

    /// Stand in for the session's executor: commit the intent once it lands.
    fn committing_session(sid: &'static str) -> std::thread::JoinHandle<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        std::thread::spawn(move || {
            while std::time::Instant::now() < deadline {
                if let Some(intended) =
                    crate::live_sessions::get(sid).and_then(|r| r.intended_member)
                {
                    crate::live_sessions::update_as_session(sid, |f| {
                        f.set_current_member(intended);
                        f.bump_key_generation();
                        f.set_committed_at(crate::usage::now_ms());
                    })
                    .expect("commit");
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    }

    fn call(session: &str, name: &str) -> CallToolResult {
        let server = TollgateServer::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            server
                .switch_profile(Parameters(SwitchArgs {
                    name: name.to_string(),
                    session: Some(session.to_string()),
                }))
                .await
        })
        .expect("a tool result, never a transport error")
    }

    fn text(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("reply text")
    }

    // 58
    #[test]
    fn switch_profile_session_self_drives_b_and_reports_swapping() {
        let home = HomeSandbox::new();
        let _markers = b_session("4242-0", "mc-a", "mc-b");
        let runtime = crate::profile::tollgate_dir()
            .expect("tollgate dir")
            .join("profiles/mc-a/runtime-4242-0");
        std::fs::create_dir_all(&runtime).expect("runtime dir");
        let _dir = ConfigDirSandbox::new(&home, &runtime);

        let session = committing_session("4242-0");
        let result = call("self", "mc-b");
        session.join().expect("session thread");
        assert_ne!(result.is_error, Some(true), "{}", text(&result));
        assert_eq!(
            text(&result),
            "session `4242-0` committed to `mc-b` (key generation 1); swapping: its requests \
             still authenticate as `mc-a` until Claude Code's next request runs the key helper"
        );
        let row = crate::live_sessions::get("4242-0").expect("row");
        assert_eq!(row.current_member.as_deref(), Some("mc-b"));
        assert!(
            !claude_dir()
                .expect("claude dir")
                .join(".credentials.json")
                .exists(),
            "the session form never touches the global link"
        );

        // The payload behind the prose, once the helper served the commit.
        crate::hot_swap::write_ack_for_test(
            "4242-0",
            &crate::hot_swap::HelperAck {
                version: 1,
                generation: 1,
                member: Some("mc-b".to_string()),
                served_at_ms: Some(crate::usage::now_ms()),
                last_failure: None,
                launch_class: None,
            },
        );
        let payload = session_switch_payload("self", "mc-b");
        assert_eq!(payload["ok"], serde_json::json!(true));
        assert_eq!(payload["session"], serde_json::json!("4242-0"));
        assert_eq!(payload["executor"], serde_json::json!("api_key"));
        assert_eq!(payload["state"], serde_json::json!("served"));
        assert_eq!(payload["committed_member"], serde_json::json!("mc-b"));
        assert_eq!(payload["served_member"], serde_json::json!("mc-b"));
        assert_eq!(payload["key_generation"], serde_json::json!(1));
        assert_eq!(payload["requested_member"], serde_json::Value::Null);
        assert_eq!(payload["reason"], serde_json::Value::Null);

        // A failed helper run for the commit is still `swapping`, with why.
        crate::hot_swap::write_ack_for_test(
            "4242-0",
            &crate::hot_swap::HelperAck {
                version: 1,
                generation: 0,
                member: Some("mc-a".to_string()),
                served_at_ms: Some(1),
                last_failure: Some(crate::hot_swap::HelperFailure {
                    generation: 1,
                    code: "no_key".to_string(),
                    at_ms: crate::usage::now_ms(),
                }),
                launch_class: None,
            },
        );
        let payload = session_switch_payload("4242-0", "mc-b");
        assert_eq!(payload["state"], serde_json::json!("swapping"));
        assert!(
            payload["reason"]
                .as_str()
                .is_some_and(|r| r.contains("no_key")),
            "{payload}"
        );
    }

    /// `"self"` outside a tollgate runtime is refused with why, not guessed.
    #[test]
    fn switch_profile_session_self_outside_a_runtime_is_refused() {
        let home = HomeSandbox::new();
        let _dir = ConfigDirSandbox::new(&home, &home.home().join("elsewhere"));
        let result = call("self", "mc-b");
        assert_eq!(result.is_error, Some(true));
        assert!(
            text(&result).contains("names no tollgate runtime"),
            "{}",
            text(&result)
        );
    }

    // 59
    #[test]
    fn switch_profile_session_on_a_relaunch_only_row_returns_the_command() {
        let _home = HomeSandbox::new();
        write_api_key_profile(&api_key_profile("ro-a", OR, "sk-a"));
        write_api_key_profile(&api_key_profile("ro-b", OR, "sk-b"));
        let row = crate::testutil::live_row("4242-0", "ro-a").with_executor(
            crate::hot_swap::Executor::RelaunchOnly {
                reason: "kill_switch".to_string(),
            },
            None,
        );
        crate::live_sessions::register(&row).expect("register");
        let _marker =
            crate::runtime::hold_session_row_marker(&ProfileName::from("ro-a"), false, "4242-0")
                .expect("marker");

        let payload = session_switch_payload("4242-0", "ro-b");
        assert_eq!(payload["ok"], serde_json::json!(false));
        assert_eq!(payload["state"], serde_json::json!("relaunch_required"));
        assert_eq!(payload["executor"], serde_json::json!("relaunch_only"));
        assert_eq!(
            payload["reason"],
            serde_json::json!("TOLLGATE_HOT_SWAP=off")
        );
        assert_eq!(
            payload["command"],
            serde_json::json!("tollgate switch 4242-0 ro-b --relaunch")
        );
        assert!(
            crate::live_sessions::get("4242-0")
                .expect("row")
                .intended_member
                .is_none(),
            "nothing is written for a relaunch-only row"
        );

        let result = call("4242-0", "ro-b");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            text(&result),
            "session `4242-0` cannot hot-swap (TOLLGATE_HOT_SWAP=off); relaunch in a terminal \
             with `tollgate switch 4242-0 ro-b --relaunch` (this tool never relaunches)"
        );

        // A session that is not running, and a name that is not a session id,
        // are refusals that name why, and write nothing.
        for sid in ["9999-0", "../4242-0"] {
            let payload = session_switch_payload(sid, "ro-b");
            assert_eq!(payload["ok"], serde_json::json!(false), "{sid}");
            assert_eq!(payload["state"], serde_json::json!("refused"), "{sid}");
            assert!(
                payload["reason"]
                    .as_str()
                    .is_some_and(|r| r.starts_with(&format!("no live session '{sid}'"))),
                "{payload}"
            );
            let result = call(sid, "ro-b");
            assert_eq!(result.is_error, Some(true));
            assert!(
                text(&result).starts_with("session switch failed: no live session"),
                "{}",
                text(&result)
            );
        }
    }

    // 60
    #[test]
    fn switch_profile_without_session_is_unchanged() {
        let _home = HomeSandbox::new();
        seed_active_plus_target();
        let result = call_switch("target");
        assert_ne!(result.is_error, Some(true));
        let live: ClaudeCredentials =
            read_json_file(&claude_dir().expect("claude dir").join(".credentials.json"))
                .expect("read live creds");
        assert_eq!(
            live.refresh_token(),
            Some("target-r"),
            "the global relink ran"
        );
        assert!(
            !crate::profile::tollgate_dir()
                .expect("tollgate dir")
                .join("live_sessions")
                .exists(),
            "no session row is read or written by the global form"
        );
        assert!(
            text(&result)
                .starts_with("switched the global active profile from `active` to `target`"),
            "{}",
            text(&result)
        );
    }
}

/// `switch_profile` on a Hermes name (hermes spec §2.2) is a refusal carrying
/// M-SWITCH, the roster's spelling included, and nothing moved: Hermes
/// switches by relaunch, and a Hermes home has no global slot to link.
#[test]
fn a_hermes_name_is_refused_with_the_relaunch_hint() {
    let _home = HomeSandbox::new();
    seed_active_linked();
    let dir = crate::profile::tollgate_dir().unwrap();
    std::fs::write(
        dir.join("hermes-profiles.toml"),
        "schema_version = 1\n[[profiles]]\nname = \"herm\"\nprovider = \"nous\"\n\
         mode = \"account\"\nauth = \"oauth\"\ncreated_at = \"2026-09-29T00:00:00Z\"\n",
    )
    .unwrap();
    let live = claude_dir().expect("claude dir").join(".credentials.json");
    let before = std::fs::read_link(&live).expect("linked");

    let result = call_switch("HERM");
    assert_eq!(result.is_error, Some(true));
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("refusal text");
    assert!(
        text.contains(
            "'herm' is a Hermes profile; Hermes switches by relaunch: 'tollgate start herm'"
        ),
        "{text}"
    );
    assert!(!text.contains("profile not found"), "{text}");
    assert_eq!(std::fs::read_link(&live).expect("still linked"), before);
}
