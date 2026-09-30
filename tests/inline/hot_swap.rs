//! Executor B's pure halves: the transport key, the launch class, the S1 gate,
//! the executor choice and the requested → committed → served view.

use super::*;
use crate::live_sessions::LiveSession;
use crate::testutil::{HomeSandbox, api_key_profile, live_row};

fn gate_pass(versions: &[&str]) -> Gate {
    Gate {
        result: GateResult::Pass,
        claude_code: versions.iter().map(|v| (*v).to_string()).collect(),
        g_real_endpoints: None,
    }
}

/// Every check passing for an OpenRouter api-key profile on real links.
fn passing<'a>(profile: &'a Profile, gate: &'a GateStatus) -> ChoiceFacts<'a> {
    ChoiceFacts {
        profile,
        policy: HotSwapPolicy::Allowed,
        kill_switch: false,
        isolated: false,
        real_links: true,
        has_oauth_store: false,
        inherited_cloud_env: false,
        gateway_policy: false,
        gate: Some(gate),
    }
}

fn relaunch(code: &str) -> Executor {
    Executor::RelaunchOnly {
        reason: code.to_string(),
    }
}

// 1
#[test]
fn transport_key_drops_default_port_trailing_slash_and_case() {
    assert_eq!(
        transport_key("HTTPS://OpenRouter.ai:443/api/").as_deref(),
        Some("https://openrouter.ai/api")
    );
    assert_eq!(
        transport_key("http://Example.COM:80/").as_deref(),
        Some("http://example.com")
    );
    // A non-default port stays; so does the path's case.
    assert_eq!(
        transport_key("https://api.example.com:8443/V1").as_deref(),
        Some("https://api.example.com:8443/V1")
    );
}

// 2
#[test]
fn transport_key_keeps_query_drops_fragment_and_refuses_userinfo() {
    assert_eq!(
        transport_key("https://h.example/api/?a=B&c#frag").as_deref(),
        Some("https://h.example/api?a=B&c")
    );
    assert_eq!(transport_key("https://user:pw@h.example/api"), None);
    assert_eq!(transport_key("https://token@h.example"), None);
    assert_eq!(transport_key("h.example/api"), None, "no scheme separator");
    assert_eq!(transport_key("https://"), None, "no authority");
}

// 3
#[test]
fn an_openrouter_helper_profile_on_real_links_chooses_executor_b() {
    let profile = api_key_profile("or-main", "https://openrouter.ai/api", "sk-or-v1-abc");
    let gate = gate_status_of(&gate_pass(&["2.1.283"]), Some("2.1.283 (Claude Code)"));
    assert_eq!(gate, GateStatus::Open);
    let (executor, class) = choose(&passing(&profile, &gate));
    assert_eq!(executor, Executor::ApiKey);
    let class = class.expect("a B launch records its class");
    assert_eq!(class.endpoint, "https://openrouter.ai/api");
    assert_eq!(class.link_mode, "real");
    assert_eq!(class.provider.as_deref(), Some("openrouter"));
    assert_eq!(class.workspace_id, None);
    assert_eq!(class.version, 1);
}

/// An OAuth account is executor A whatever else holds.
#[test]
fn an_oauth_profile_is_executor_a() {
    let profile = crate::profile::Profile::new("oa".to_string(), None, None);
    let gate = GateStatus::Open;
    assert_eq!(choose(&passing(&profile, &gate)), (Executor::Oauth, None));
    // An endpoint with no usable key is not API-key shaped either.
    let keyless = api_key_profile("kl", "https://openrouter.ai/api", "   ");
    assert_eq!(choose(&passing(&keyless, &gate)).0, Executor::Oauth);
}

// 4
#[test]
fn each_relaunch_only_reason_is_recorded_in_check_order() {
    // Start with every check failing, then clear them one at a time in the
    // spec's order: each step must report exactly the next code.
    let mut profile = api_key_profile("ord", "http://127.0.0.1:3001", "sk-ord");
    profile
        .env
        .insert("ANTHROPIC_AUTH_TOKEN".to_string(), "tok".to_string());
    profile
        .env
        .insert("CLAUDE_CODE_USE_BEDROCK".to_string(), "1".to_string());
    let pending = GateStatus::Pending;
    fn facts<'a>(p: &'a Profile, pending: &'a GateStatus) -> ChoiceFacts<'a> {
        ChoiceFacts {
            profile: p,
            policy: HotSwapPolicy::Never,
            kill_switch: true,
            isolated: true,
            real_links: false,
            has_oauth_store: true,
            inherited_cloud_env: true,
            gateway_policy: true,
            gate: Some(pending),
        }
    }
    let mut f = facts(&profile, &pending);
    assert_eq!(choose(&f).0, relaunch("delegate"));
    f.policy = HotSwapPolicy::Allowed;
    assert_eq!(choose(&f).0, relaunch("kill_switch"));
    f.kill_switch = false;
    assert_eq!(choose(&f).0, relaunch("isolated"));
    f.isolated = false;
    assert_eq!(choose(&f).0, relaunch("fake_links"));
    f.real_links = true;
    assert_eq!(choose(&f).0, relaunch("hybrid_oauth_store"));
    f.has_oauth_store = false;
    assert_eq!(choose(&f).0, relaunch("loopback_endpoint"));

    // `no_endpoint` and `endpoint_userinfo` sit before the loopback check.
    let mut env_only = profile.clone();
    env_only.env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://u:p@h.example".to_string(),
    );
    let f2 = ChoiceFacts {
        profile: &env_only,
        ..facts(&env_only, &pending)
    };
    let f2 = ChoiceFacts {
        policy: HotSwapPolicy::Allowed,
        kill_switch: false,
        isolated: false,
        real_links: true,
        has_oauth_store: false,
        ..f2
    };
    assert_eq!(choose(&f2).0, relaunch("endpoint_userinfo"));
    let mut blank = profile.clone();
    blank
        .env
        .insert("ANTHROPIC_BASE_URL".to_string(), "  ".to_string());
    blank.base_url = Some("  ".to_string());
    let f3 = ChoiceFacts {
        profile: &blank,
        policy: HotSwapPolicy::Allowed,
        kill_switch: false,
        isolated: false,
        real_links: true,
        has_oauth_store: false,
        ..facts(&blank, &pending)
    };
    assert_eq!(choose(&f3).0, relaunch("no_endpoint"));

    let mut profile = profile.clone();
    profile.base_url = Some("https://openrouter.ai/api".to_string());
    let mut f = ChoiceFacts {
        profile: &profile,
        policy: HotSwapPolicy::Allowed,
        kill_switch: false,
        isolated: false,
        real_links: true,
        has_oauth_store: false,
        ..facts(&profile, &pending)
    };
    assert_eq!(choose(&f).0, relaunch("auth_env"));
    let mut no_auth = profile.clone();
    no_auth.env.remove("ANTHROPIC_AUTH_TOKEN");
    f.profile = &no_auth;
    assert_eq!(
        choose(&f).0,
        relaunch("cloud_env"),
        "profile env selects bedrock"
    );
    let mut no_cloud = no_auth.clone();
    no_cloud.env.remove("CLAUDE_CODE_USE_BEDROCK");
    f.profile = &no_cloud;
    assert_eq!(
        choose(&f).0,
        relaunch("cloud_env"),
        "the inherited env does"
    );
    f.inherited_cloud_env = false;
    assert_eq!(choose(&f).0, relaunch("gateway_policy"));
    f.gateway_policy = false;
    assert_eq!(choose(&f).0, relaunch("s1_gate_pending"));
    let unknown = GateStatus::VersionUnknown;
    f.gate = Some(&unknown);
    assert_eq!(choose(&f).0, relaunch("cc_version_unknown"));
    f.gate = None;
    assert_eq!(
        choose(&f).0,
        relaunch("cc_version_unknown"),
        "a gate nobody computed is not a pass"
    );
    let unlisted = GateStatus::VersionNotListed("2.1.300".to_string());
    f.gate = Some(&unlisted);
    assert_eq!(choose(&f).0, relaunch("s1_gate_version"));
    let open = GateStatus::Open;
    f.gate = Some(&open);
    assert_eq!(choose(&f).0, Executor::ApiKey);
}

// 5
#[test]
fn loopback_endpoints_are_relaunch_only() {
    let gate = GateStatus::Open;
    for url in [
        "http://127.0.0.1:3001",
        "http://localhost:11434",
        "http://[::1]:8080/v1",
        "http://127.8.9.10",
    ] {
        let profile = api_key_profile("lo", url, "sk-lo");
        assert_eq!(
            choose(&passing(&profile, &gate)).0,
            relaunch("loopback_endpoint"),
            "{url}"
        );
    }
    assert!(!is_loopback_endpoint("https://localhost.example.com"));
    assert!(!is_loopback_endpoint("https://128.0.0.1"));
}

// 6
#[test]
fn a_blank_api_key_env_is_not_auth_env_and_leaves_the_env_hash_alone() {
    let mut profile = api_key_profile("blank", "https://openrouter.ai/api", "sk-b");
    let before = env_sha256(&profile.env);
    profile
        .env
        .insert("ANTHROPIC_API_KEY".to_string(), "   ".to_string());
    assert_eq!(env_sha256(&profile.env), before);
    let gate = GateStatus::Open;
    assert_eq!(choose(&passing(&profile, &gate)).0, Executor::ApiKey);
    profile
        .env
        .insert("ANTHROPIC_API_KEY".to_string(), "sk-real".to_string());
    assert_eq!(choose(&passing(&profile, &gate)).0, relaunch("auth_env"));
}

// 7
#[test]
fn the_env_hash_ignores_the_endpoint_key_only() {
    let mut env = BTreeMap::new();
    let empty = env_sha256(&env);
    env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        "https://a.example".to_string(),
    );
    assert_eq!(env_sha256(&env), empty);
    env.insert("HTTPS_PROXY".to_string(), "http://proxy".to_string());
    assert_ne!(env_sha256(&env), empty);
    assert_eq!(env_sha256(&env).len(), 64);
}

// 8
#[test]
fn class_matches_rejects_each_axis_and_tolerates_a_missing_workspace() {
    let launch_profile = api_key_profile("a", "https://openrouter.ai/api", "sk-a");
    let class = LaunchClass::of(&launch_profile, true).expect("class");
    assert_eq!(class.workspace_id, None);
    let same = api_key_profile("b", "HTTPS://openrouter.ai:443/api/", "sk-b");
    assert_eq!(class_matches(&class, &same), Ok(()));

    let other_host = api_key_profile("b", "https://api.deepseek.com/anthropic", "sk-b");
    assert_eq!(
        class_matches(&class, &other_host),
        Err("class_differs:endpoint")
    );
    let mut models = same.clone();
    models.models.opus = Some("x/opus".to_string());
    assert_eq!(class_matches(&class, &models), Err("class_differs:models"));
    let mut env = same.clone();
    env.env.insert("FOO".to_string(), "1".to_string());
    assert_eq!(class_matches(&class, &env), Err("class_differs:env"));
    let mut auth = same.clone();
    auth.env
        .insert("ANTHROPIC_AUTH_TOKEN".to_string(), "t".to_string());
    assert_eq!(class_matches(&class, &auth), Err("class_differs:auth_env"));
    let keyless = api_key_profile("b", "https://openrouter.ai/api", "");
    assert_eq!(
        class_matches(&class, &keyless),
        Err("class_differs:no_api_key")
    );
    // A class carrying a workspace still matches a target that has none.
    let mut scoped = class.clone();
    scoped.workspace_id = Some("ws-1".to_string());
    assert_eq!(class_matches(&scoped, &same), Ok(()));
}

// 9
#[test]
fn a_pending_or_malformed_gate_block_keeps_b_off() {
    let v = Some("2.1.283 (Claude Code)");
    assert_eq!(
        gate_status_of(&parse_gate("no block here\nS1 RESULT: PASS\n"), v),
        GateStatus::Pending
    );
    let pending = "<!-- tollgate:s1-gate\nresult = \"PENDING\"\nclaude_code = [\"2.1.283\"]\n-->";
    assert_eq!(gate_status_of(&parse_gate(pending), v), GateStatus::Pending);
    let fail = "<!-- tollgate:s1-gate\nresult = \"FAIL\"\nclaude_code = [\"2.1.283\"]\n-->";
    assert_eq!(gate_status_of(&parse_gate(fail), v), GateStatus::Pending);
    let malformed = "<!-- tollgate:s1-gate\nresult = PASS\n-->";
    assert_eq!(parse_gate(malformed).result, GateResult::Pending);
    let unterminated = "<!-- tollgate:s1-gate\nresult = \"PASS\"\nclaude_code = [\"2.1.283\"]\n";
    assert_eq!(parse_gate(unterminated).result, GateResult::Pending);
}

// 10
#[test]
fn a_pass_gate_enables_only_listed_versions() {
    let gate = gate_pass(&["2.1.283"]);
    assert_eq!(
        gate_status_of(&gate, Some("2.1.283 (Claude Code)")),
        GateStatus::Open
    );
    assert_eq!(gate_status_of(&gate, Some("2.1.283")), GateStatus::Open);
    assert_eq!(
        gate_status_of(&gate, Some("2.1.284 (Claude Code)")),
        GateStatus::VersionNotListed("2.1.284".to_string())
    );
    assert_eq!(gate_status_of(&gate, None), GateStatus::VersionUnknown);
    assert_eq!(
        gate_status_of(&gate, Some("   ")),
        GateStatus::VersionUnknown
    );
    assert!(
        reason_text("s1_gate_version", Some("2.1.284 (Claude Code)")).contains("2.1.284"),
        "the refusal names the version to add"
    );
}

// 11
#[test]
fn the_committed_spike_doc_block_parses() {
    let gate = parse_gate(spike_doc());
    assert_eq!(gate.result, GateResult::Pass);
    assert!(gate.claude_code.iter().any(|v| v == "2.1.283"));
    assert_eq!(gate.g_real_endpoints.as_deref(), Some("not_run"));
    // Everything above the block is the committed evidence of 60e4aee5, byte
    // for byte: its SHA-256, taken with `git show 60e4aee5:<doc> | sha256sum`.
    let start = spike_doc()
        .find(GATE_OPEN)
        .expect("the doc carries the gate block");
    let prose = spike_doc()[..start]
        .strip_suffix('\n')
        .expect("the block is appended after a blank line");
    use sha2::Digest as _;
    let digest: String = sha2::Sha256::digest(prose.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        digest, "ecb0ad4449f6267d283be2c07f57f10069f030f05c21dcaad2e5a688c28b40c1",
        "the prose above the gate block must stay byte-identical to 60e4aee5"
    );
    assert!(prose.ends_with("S1 RESULT: PASS\n"));
    // Nothing follows the block.
    assert!(spike_doc().trim_end().ends_with("-->"));
}

// 12
#[cfg(unix)]
#[test]
fn the_cc_version_cache_is_reused_only_on_equal_path_mtime_and_len() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    let bin = home.home().join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\necho 2.1.283\n").expect("fake claude");
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = std::ffi::OsString::from(format!("{}:/usr/bin:/bin", bin.display()));
    let _path = crate::testutil::EnvPin::new(&home, &[("PATH", Some(path.as_os_str()))]);
    set_cc_probe(Some(Box::new(|| Some("2.1.283 (Claude Code)".to_string()))));

    assert_eq!(
        cached_cc_version().as_deref(),
        Some("2.1.283 (Claude Code)")
    );
    assert_eq!(cc_probe_runs(), 1);
    let cache: CcVersionCache = serde_json::from_slice(
        &std::fs::read(cc_version_cache_path().expect("path")).expect("cache written"),
    )
    .expect("cache parses");
    assert_eq!(cache.path, claude);
    assert_eq!(
        cached_cc_version().as_deref(),
        Some("2.1.283 (Claude Code)")
    );
    assert_eq!(
        cc_probe_runs(),
        1,
        "an equal (path, mtime, len) reuses the cache"
    );

    // A new mtime re-probes.
    crate::testutil::set_mtime(
        &claude,
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
    cached_cc_version();
    assert_eq!(cc_probe_runs(), 2);
    // A new length re-probes.
    let mtime = std::fs::metadata(&claude)
        .and_then(|m| m.modified())
        .expect("mtime");
    std::fs::write(&claude, "#!/bin/sh\necho 2.1.283 # longer\n").expect("rewrite");
    crate::testutil::set_mtime(&claude, mtime);
    cached_cc_version();
    assert_eq!(cc_probe_runs(), 3);
    // Only the unchanged stat reuses it.
    cached_cc_version();
    assert_eq!(cc_probe_runs(), 3);
    set_cc_probe(None);
}

// 13
#[test]
fn tollgate_hot_swap_off_forces_relaunch_only() {
    let home = HomeSandbox::new();
    for (value, on) in [
        ("off", true),
        ("OFF", true),
        (" off ", true),
        ("on", false),
        ("", false),
    ] {
        let _pin = crate::testutil::EnvPin::new(
            &home,
            &[(KILL_SWITCH_ENV, Some(std::ffi::OsStr::new(value)))],
        );
        assert_eq!(kill_switch_on(), on, "{value:?}");
    }
    let profile = api_key_profile("ks", "https://openrouter.ai/api", "sk-ks");
    let gate = GateStatus::Open;
    let facts = ChoiceFacts {
        kill_switch: true,
        ..passing(&profile, &gate)
    };
    assert_eq!(choose(&facts).0, relaunch("kill_switch"));
    assert_eq!(reason_text("kill_switch", None), "TOLLGATE_HOT_SWAP=off");
}

/// A B row committed to `member` at `generation` (`committed_at` = `at`).
fn b_row(start: &str, member: &str, generation: u64, at: u64) -> LiveSession {
    let mut row = live_row("4242-0", start).with_executor(Executor::ApiKey, None);
    row.current_member = Some(member.to_string());
    row.key_generation = Some(generation);
    row.committed_at = (generation > 0).then_some(at);
    row
}

fn ack(generation: u64, member: &str, served_at: u64) -> HelperAck {
    HelperAck {
        version: 1,
        generation,
        member: Some(member.to_string()),
        served_at_ms: Some(served_at),
        last_failure: None,
        launch_class: None,
    }
}

// 14
#[test]
fn swap_view_states_follow_generation_and_recorded_helper_runs() {
    // Generation 0 with no ack is served: the launch member is what the
    // session was built with.
    let fresh = b_row("a", "a", 0, 0);
    let view = SwapView::of(&fresh, None);
    assert_eq!(view.state, SwapState::Served);
    assert_eq!(view.served_member(), Some("a"));

    // Requested: an intent the session has not committed.
    let mut requested = fresh.clone();
    requested.intended_member = Some("b".to_string());
    let view = SwapView::of(&requested, None);
    assert_eq!(view.state, SwapState::Requested);
    assert_eq!(view.requested_member.as_deref(), Some("b"));

    // Committed, and the helper has run since: swapping, not idle.
    let committed = b_row("a", "b", 1, 1_000);
    let old = ack(0, "a", 1_500);
    let view = SwapView::of(&committed, Some(&old));
    assert_eq!(view.state, SwapState::Swapping);
    assert!(!view.idle);
    assert_eq!(view.served_member(), Some("a"), "attribution stays served");
    assert_eq!(
        view.committed.as_ref().map(|p| p.member.as_str()),
        Some("b")
    );

    // Committed, no helper run since the commit: idle.
    let before = ack(0, "a", 500);
    let view = SwapView::of(&committed, Some(&before));
    assert_eq!(view.state, SwapState::Swapping);
    assert!(view.idle);

    // Served once the ack reaches the committed generation.
    let served = ack(1, "b", 1_200);
    let view = SwapView::of(&committed, Some(&served));
    assert_eq!(view.state, SwapState::Served);
    assert_eq!(view.served_member(), Some("b"));

    // Stalled: the helper ran for this generation and failed.
    let mut failed = before.clone();
    failed.last_failure = Some(HelperFailure {
        generation: 1,
        code: "no_key".to_string(),
        at_ms: 1_100,
    });
    let view = SwapView::of(&committed, Some(&failed));
    assert_eq!(view.state, SwapState::Stalled);
    assert_eq!(view.stall_code.as_deref(), Some("no_key"));

    // An executor A row is served = committed, never stalled.
    let mut a = live_row("4243-0", "x");
    a.current_member = Some("y".to_string());
    let view = SwapView::of(&a, Some(&failed));
    assert_eq!(view.state, SwapState::Served);
    assert_eq!(view.served_member(), Some("y"));
}

// 14a
#[test]
fn an_idle_committed_session_never_reports_stalled() {
    // The commit an hour ago and no helper run since: `SwapView::of` has no
    // clock input at all, so no amount of elapsed time turns this stalled.
    let hour_ago = crate::usage::now_ms() - 3_600_000;
    let committed = b_row("a", "b", 3, hour_ago);
    let stale_ack = ack(2, "a", hour_ago - 10);
    for ack in [None, Some(&stale_ack)] {
        let view = SwapView::of(&committed, ack);
        assert_eq!(view.state, SwapState::Swapping);
        assert!(view.idle, "no helper run since the commit");
        assert_eq!(view.stall_code, None);
    }
}

// 14b
#[test]
fn a_failed_helper_run_marks_stalled_with_its_code() {
    let committed = b_row("a", "b", 2, 1_000);
    let mut ack = ack(1, "a", 900);
    ack.last_failure = Some(HelperFailure {
        generation: 2,
        code: "config_unreadable".to_string(),
        at_ms: 1_050,
    });
    let view = SwapView::of(&committed, Some(&ack));
    assert_eq!(view.state, SwapState::Stalled);
    assert_eq!(view.stall_code.as_deref(), Some("config_unreadable"));
    assert!(!view.idle);
    // A failure for an OLDER generation does not stall this commit.
    let mut older = ack.clone();
    older.last_failure.as_mut().expect("failure").generation = 1;
    assert_eq!(
        SwapView::of(&committed, Some(&older)).state,
        SwapState::Swapping
    );
}

#[test]
fn a_generation_zero_failure_only_ack_is_stalled_everywhere() {
    let row = b_row("a", "a", 0, 0);
    let failed = HelperAck {
        version: 1,
        generation: 0,
        member: None,
        served_at_ms: None,
        last_failure: Some(HelperFailure {
            generation: 0,
            code: "class_differs:endpoint".to_string(),
            at_ms: 1,
        }),
        launch_class: None,
    };
    let view = SwapView::of(&row, Some(&failed));
    assert_eq!(view.state, SwapState::Stalled);
    assert_eq!(view.stall_code.as_deref(), Some("class_differs:endpoint"));
    assert!(!view.idle);
    assert_eq!(view.served_member(), None);
    assert_eq!(
        LiveSessionView::of(&row, Some(&failed)).state,
        SwapState::Stalled
    );
}

// 14c
#[test]
fn a_hermes_or_codex_row_has_no_executor() {
    // A legacy claude row (no `executor` field) reads executor A.
    let legacy: LiveSession = serde_json::from_str(
        r#"{"session_id":"1-0","start_profile":"p","pid":1,"started_at":0,"cwd":null,"isolated":false}"#,
    )
    .expect("a legacy row parses");
    assert_eq!(legacy.executor, None);
    assert_eq!(legacy.executor(), Executor::Oauth);
    assert_eq!(LiveSessionView::of(&legacy, None).executor, Some("oauth"));

    let mut codex = live_row("2-0", "cx");
    codex.harness = crate::harness::Harness::Codex;
    assert_eq!(codex.executor(), Executor::None);
    let view = LiveSessionView::of(&codex, None);
    assert_eq!(view.executor, None);
    assert_eq!(view.committed, None);
    assert_eq!(view.served, None);
    let json = serde_json::to_value(&view).expect("serialises");
    assert!(json["executor"].is_null());
    assert!(json["committed"].is_null());
    assert!(json["served"].is_null());
    // (Hermes rows arrive with the Hermes lane's harness; they take the same
    // `harness`-derived `None`.)
}

/// The gate override replaces the compiled block for its guard's life.
#[test]
fn the_gate_override_replaces_the_compiled_block() {
    let home = HomeSandbox::new();
    assert_eq!(current_gate().result, GateResult::Pass);
    {
        let _gate = S1GateOverride::new(&home, Gate::pending());
        assert_eq!(current_gate().result, GateResult::Pending);
    }
    assert_eq!(current_gate().result, GateResult::Pass);
}

/// The drift check reads the runtime settings' endpoint and model env.
#[test]
fn runtime_settings_drift_names_the_axis() {
    let home = HomeSandbox::new();
    let profile = api_key_profile("d", "https://openrouter.ai/api", "sk-d");
    let class = LaunchClass::of(&profile, true).expect("class");
    let path = home.home().join("settings.json");
    assert_eq!(
        runtime_settings_drift(&path, &class),
        Some("class_differs:endpoint"),
        "an unreadable file refuses"
    );
    std::fs::write(
        &path,
        r#"{"env":{"ANTHROPIC_BASE_URL":"https://openrouter.ai/api/"}}"#,
    )
    .expect("write");
    assert_eq!(runtime_settings_drift(&path, &class), None);
    std::fs::write(
        &path,
        r#"{"env":{"ANTHROPIC_BASE_URL":"https://openrouter.ai/api","ANTHROPIC_DEFAULT_OPUS_MODEL":"x"}}"#,
    )
    .expect("write");
    assert_eq!(
        runtime_settings_drift(&path, &class),
        Some("class_differs:models")
    );
    std::fs::write(
        &path,
        r#"{"env":{"ANTHROPIC_BASE_URL":"https://other.example"}}"#,
    )
    .expect("write");
    assert_eq!(
        runtime_settings_drift(&path, &class),
        Some("class_differs:endpoint")
    );
}

/// A `forceLogin*` key in the base settings or Claude Code's managed file puts
/// the account behind a login policy the helper cannot satisfy.
#[test]
fn a_force_login_setting_in_the_base_or_managed_file_is_gateway_policy() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).expect(".claude");
    assert!(!gateway_policy_in_force(&claude), "no settings, no policy");
    std::fs::write(claude.join("settings.json"), br#"{"theme":"dark"}"#).expect("base");
    assert!(!gateway_policy_in_force(&claude));
    std::fs::write(
        claude.join("settings.json"),
        br#"{"forceLoginOrgUUID":"org-1"}"#,
    )
    .expect("base");
    assert!(gateway_policy_in_force(&claude));
    std::fs::write(claude.join("settings.json"), br#"{}"#).expect("base");
    let managed = home.home().join("managed-settings.json");
    std::fs::write(&managed, br#"{"forceLoginMethod":"console"}"#).expect("managed");
    set_managed_settings_override(Some(managed));
    let in_force = gateway_policy_in_force(&claude);
    set_managed_settings_override(None);
    assert!(in_force, "the managed file counts too");
}

#[test]
fn unreadable_or_invalid_settings_fail_closed_for_gateway_policy() {
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(&claude).expect("claude dir");
    std::fs::write(claude.join("settings.json"), b"{").expect("invalid base");
    assert!(gateway_policy_in_force(&claude));
    std::fs::remove_file(claude.join("settings.json")).expect("remove base");
    let managed = home.home().join("managed-settings.json");
    std::fs::write(&managed, b"{").expect("invalid managed");
    set_managed_settings_override(Some(managed.clone()));
    assert!(gateway_policy_in_force(&claude));
    std::fs::remove_file(&managed).expect("remove managed");
    std::fs::create_dir(&managed).expect("unreadable managed");
    assert!(gateway_policy_in_force(&claude));
    set_managed_settings_override(None);
}

/// The commit's settings touch moves a regular file's mtime and refuses a
/// symlink rather than follow it: the target (the operator's
/// `~/.claude/settings.json`, say) keeps its mtime.
#[cfg(unix)]
#[test]
fn the_settings_touch_refuses_a_symlink() {
    let home = HomeSandbox::new();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let real = home.home().join("settings.json");
    std::fs::write(&real, b"{}").expect("write");
    crate::testutil::set_mtime(&real, old);
    touch_settings(&real).expect("a regular file is touched");
    let moved = std::fs::metadata(&real)
        .and_then(|m| m.modified())
        .expect("mtime");
    assert!(moved > old + std::time::Duration::from_secs(3000));

    let operator = home.home().join("operator-settings.json");
    std::fs::write(&operator, b"{}").expect("write");
    crate::testutil::set_mtime(&operator, old);
    let link = home.home().join("runtime-settings.json");
    std::os::unix::fs::symlink(&operator, &link).expect("symlink");
    assert!(touch_settings(&link).is_err(), "a symlink is refused");
    assert_eq!(
        std::fs::metadata(&operator)
            .and_then(|m| m.modified())
            .expect("mtime"),
        old,
        "the link's target is untouched"
    );
}
