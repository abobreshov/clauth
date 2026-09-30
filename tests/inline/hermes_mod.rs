#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Hermes verbs end to end against the stub venv (spec §7, part 1): `new`,
//! `key`, `auth`, `start` and `delete`, the child env, the lock discipline and
//! guest mode. Every child process is the fixture's shell stub.

use super::*;
use crate::hermes::testkit::{
    Fixture, NoManagedScope, TEST_KEY, TEST_KEY_FINGERPRINT, new_openrouter, tree_digest,
};
use crate::testutil::HomeSandbox;

fn fixture(sb: &HomeSandbox) -> Fixture {
    let fx = Fixture::new(&sb.home().join("fx"));
    fx.install_as_settings_bin();
    fx
}

fn opts(name: &str, provider: Provider) -> NewOpts {
    NewOpts {
        name: name.to_string(),
        provider,
        model: None,
        pool: false,
        env_key: false,
        no_key: false,
    }
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn points() -> Vec<(String, bool)> {
    UNLOCKED_POINTS.with(|p| p.borrow().clone())
}

/// Test 4: one namespace across claude, codex and Hermes, case-insensitively,
/// through the one validator every creation site calls.
#[test]
fn names_are_unique_across_three_rosters() {
    use crate::actions::validate_profile_name;
    use crate::harness::Harness;
    let _sb = HomeSandbox::new();
    crate::testutil::register_names(&["work"]);
    crate::testutil::write_codex_roster(&["cx"]);
    HermesState::update(|s| {
        s.add_profile(HermesProfile {
            name: "hm".into(),
            provider: Provider::Nous,
            model: None,
            mode: Mode::Account,
            auth: Auth::Oauth,
            key_env: None,
            key_fingerprint: None,
            created_at: "t".into(),
        });
        Ok(())
    })
    .unwrap();

    for (name, harness, holder) in [
        ("WORK", Harness::Hermes, "claude"),
        ("Cx", Harness::Hermes, "codex"),
        ("HM", Harness::Claude, "hermes"),
        ("hM", Harness::Codex, "hermes"),
    ] {
        let err = validate_profile_name(name, harness, None).unwrap_err();
        assert!(
            err.to_string().contains(&format!("is a {holder} profile")),
            "{name} on {harness}: {err}"
        );
    }
    let err = validate_profile_name("HM", Harness::Hermes, None).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    validate_profile_name("fresh", Harness::Hermes, None).unwrap();
}

/// Test 5.
#[test]
fn a_hermes_profile_cannot_be_named_profiles() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    for name in ["profiles", "Profiles", "PROFILES"] {
        let err = new_profile(&opts(name, Provider::Nous), &mut |_| unreachable!()).unwrap_err();
        assert_eq!(err.to_string(), guards::M_NAME, "{name}");
    }
    assert!(HermesState::load().unwrap().profiles().is_empty());
}

/// Test 6: the home, the `.env` at 0600 holding the key line, and the
/// roster's fingerprint in Hermes' format, equal to a Python-computed vector.
#[test]
fn new_creates_home_env_0600_and_fingerprint_in_hermes_format() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");

    let paths = HermesPaths::for_name("or-main").unwrap();
    for d in [
        &paths.profile,
        &paths.home,
        &paths.shared,
        &paths.child_home,
    ] {
        assert!(d.is_dir(), "{}", d.display());
        assert_eq!(mode(d), 0o700);
    }
    let env = paths.env_file();
    assert_eq!(mode(&env), 0o600);
    let text = std::fs::read_to_string(&env).unwrap();
    assert!(
        text.contains(&format!("\nOPENROUTER_API_KEY={TEST_KEY}\n")),
        "{text}"
    );

    let entry = HermesState::load()
        .unwrap()
        .find("or-main")
        .cloned()
        .unwrap();
    assert_eq!(entry.provider, Provider::Openrouter);
    assert_eq!((entry.mode, entry.auth), (Mode::Account, Auth::Env));
    assert_eq!(entry.key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    assert_eq!(entry.key_fingerprint.as_deref(), Some(TEST_KEY_FINGERPRINT));
    let roster = std::fs::read_to_string(profiles::hermes_state_path().unwrap()).unwrap();
    assert!(
        !roster.contains(TEST_KEY),
        "the key value never reaches the roster"
    );
}

/// Test 7.
#[test]
fn new_adopts_a_crashed_leftover_and_refuses_a_foreign_dir() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    let leftover = HermesPaths::for_name("or-main").unwrap();
    std::fs::create_dir_all(leftover.home.join("shared")).unwrap();
    std::fs::create_dir_all(leftover.profile.join("sessions-1-0")).unwrap();
    std::fs::write(leftover.env_file(), "OPENROUTER_API_KEY=stale\n").unwrap();
    new_openrouter("or-main");
    let text = std::fs::read_to_string(leftover.env_file()).unwrap();
    assert!(text.contains(&format!("OPENROUTER_API_KEY={TEST_KEY}")) && !text.contains("stale"));

    let foreign = HermesPaths::for_name("other").unwrap();
    std::fs::create_dir_all(&foreign.profile).unwrap();
    std::fs::write(foreign.profile.join("config.toml"), "").unwrap();
    let err = new_profile(&opts("other", Provider::Openrouter), &mut |_| {
        Ok(TEST_KEY.into())
    })
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "profiles/other exists and is not a leftover Hermes home; remove it or pick another name"
    );
    assert!(!HermesState::load().unwrap().holds("other"));
}

#[test]
fn new_refuses_unsafe_homes_and_install_env_before_hermes_runs() {
    use crate::hermes::testkit::passing_projection;
    for hazard in ["anthropic_key", "claude_token", "child_claude", "hsp_env"] {
        let sb = HomeSandbox::new();
        let _scope = NoManagedScope::new(&sb);
        let fx = fixture(&sb);
        let paths = HermesPaths::for_name("or-main").unwrap();
        std::fs::create_dir_all(&paths.home).unwrap();
        match hazard {
            "anthropic_key" | "claude_token" => {
                let key = if hazard == "anthropic_key" {
                    "ANTHROPIC_API_KEY"
                } else {
                    "CLAUDE_CODE_OAUTH_TOKEN"
                };
                std::fs::write(paths.env_file(), format!("{key}=sentinel\n")).unwrap();
                let mut projection = passing_projection("openrouter");
                projection["env_keys"]["home"] =
                    serde_json::json!([{"key": key, "nonblank": true}]);
                fx.set_projection(&projection);
            }
            "child_claude" => {
                std::fs::create_dir_all(sb.home().join(".claude")).unwrap();
                std::fs::create_dir_all(&paths.child_home).unwrap();
                std::os::unix::fs::symlink(
                    sb.home().join(".claude"),
                    paths.child_home.join(".claude"),
                )
                .unwrap();
            }
            "hsp_env" => {
                std::fs::write(fx.hsp.join(".env"), "ANTHROPIC_API_KEY=sentinel\n").unwrap()
            }
            _ => unreachable!(),
        }
        let err = new_profile(&opts("or-main", Provider::Openrouter), &mut |_| {
            Ok(TEST_KEY.into())
        })
        .unwrap_err();
        let why = err.to_string();
        match hazard {
            "anthropic_key" | "claude_token" => {
                assert!(why.contains(".env routes to anthropic"), "{hazard}: {why}");
            }
            "child_claude" => {
                assert!(why.contains("holds '.claude'"), "{hazard}: {why}");
                assert!(fx.calls().is_empty(), "projector ran before G2a");
            }
            "hsp_env" => assert!(why.contains("/.env exists; Hermes loads it"), "{why}"),
            _ => unreachable!(),
        }
        assert!(fx.hermes_calls().is_empty(), "{hazard}: Hermes ran");
    }
}

#[test]
fn explain_checks_the_child_home() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();
    std::os::unix::fs::symlink(sb.home(), paths.child_home.join(".claude")).unwrap();
    let profile = find_profile("or-main").unwrap();
    let err = preflight_explain("or-main", &profile, &[]).unwrap_err();
    assert!(err.to_string().contains(".claude"), "{err}");
}

#[test]
fn explain_names_a_missing_child_home_without_a_fake_entry() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();
    std::fs::remove_dir_all(&paths.child_home).unwrap();
    let profile = find_profile("or-main").unwrap();
    let err = preflight_explain("or-main", &profile, &[]).unwrap_err();
    assert!(err.to_string().contains("child home is missing"), "{err}");
    assert!(!err.to_string().contains("holds '.'"), "{err}");
}

#[test]
fn unresolved_install_prints_complete_auxiliary_pin_hints() {
    const CHILD: &str = "TOLLGATE_PIN_HINT_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let sb = HomeSandbox::new();
        new_profile(&opts("or-main", Provider::Openrouter), &mut |_| {
            Ok(TEST_KEY.into())
        })
        .unwrap();
        let _ = sb;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hermes::tests::unresolved_install_prints_complete_auxiliary_pin_hints",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", "/usr/bin:/bin")
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("tollgate: finish by hand: HOME=").count(),
        guards::HERMES_AUX_TASKS.len(),
        "{stderr}"
    );
    assert!(stderr.contains(" HERMES_HOME="), "{stderr}");
    assert!(
        stderr.contains(" 'hermes' config set auxiliary.vision.provider openrouter"),
        "{stderr}"
    );
}

#[test]
fn auxiliary_pin_hint_is_a_complete_child_home_command() {
    let sb = HomeSandbox::new();
    let paths = HermesPaths::for_name("or-main").unwrap();
    let hint = pin_auxiliary_hint(&paths, "/venv/bin/hermes", "vision", Provider::Openrouter);
    assert!(hint.starts_with("HOME="), "{hint}");
    assert!(hint.contains(" HERMES_HOME="), "{hint}");
    assert!(
        hint.contains(" '/venv/bin/hermes' config set auxiliary.vision.provider openrouter"),
        "{hint}"
    );
    let _ = sb;
}

/// Test 8: the key prompt runs with neither tollgate lock held.
#[test]
fn new_reads_the_key_before_taking_any_lock() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    let mut prompted = false;
    new_profile(&opts("or-main", Provider::Openrouter), &mut |p| {
        assert_eq!(p, Provider::Openrouter);
        assert!(!crate::lockorder::holds::<crate::lockorder::rank::Rotation>());
        assert!(!crate::lockorder::holds::<crate::lockorder::rank::State>());
        prompted = true;
        Ok(TEST_KEY.into())
    })
    .unwrap();
    assert!(prompted);
    assert!(points().contains(&("the key prompt".to_string(), false)));
    // An OAuth home never prompts.
    new_profile(&opts("nous-a", Provider::Nous), &mut |_| unreachable!()).unwrap();
}

/// Test 21a: every Hermes child runs with `HOME` = the child home, which holds
/// only the allowlisted links; a planted `.claude` refuses the next start.
#[test]
fn the_child_home_has_no_claude_codex_qwen_or_gh_entries() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    let h = sb.home();
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    std::fs::write(h.join(".claude/.credentials.json"), "{}").unwrap();
    std::fs::write(h.join(".claude.json"), "{}").unwrap();
    for d in [".codex", ".qwen", ".config/gh", ".hermes", ".ssh"] {
        std::fs::create_dir_all(h.join(d)).unwrap();
    }
    std::fs::write(h.join(".gitconfig"), "").unwrap();
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();

    let calls = fx.calls();
    assert!(!calls.is_empty());
    for c in &calls {
        assert_eq!(
            c.env.get("HOME").map(PathBuf::from),
            Some(paths.child_home.clone())
        );
    }
    let mut entries: Vec<String> = std::fs::read_dir(&paths.child_home)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, [".gitconfig", ".ssh"]);

    std::fs::create_dir(paths.child_home.join(".claude")).unwrap();
    let err = crate::start::run_hermes("or-main", &[]).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "tollgate: hermes 'or-main': {} holds '.claude', which tollgate did not put there; \
             remove it (the child home carries only .gitconfig, .config/git and .ssh links)",
            paths.child_home.display()
        )
    );
    assert!(
        fx.hermes_calls()
            .iter()
            .all(|c| c.argv.first().is_some_and(|a| a == "config"))
    );
}

/// Test 21b: the redirect cannot be routed around. The child env drops the
/// XDG dirs, `CLAUDE_CONFIG_DIR` and the gh tokens, and pins `HOME`.
#[test]
fn the_child_env_scrubs_xdg_claude_config_dir_and_gh_tokens() {
    let _sb = HomeSandbox::new();
    let paths = HermesPaths::for_name("or-main").unwrap();
    let command = child_command(
        Path::new("/x/hermes"),
        &paths,
        &BTreeSet::new(),
        &["MY_CUSTOM".into()],
    );
    let env = crate::testutil::env_overrides(&command);
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "CLAUDE_CONFIG_DIR",
        "GH_TOKEN",
        "GH_CONFIG_DIR",
        "GITHUB_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "PYTEST_CURRENT_TEST",
        "HERMES_INFERENCE_PROVIDER",
        "HERMES_MODEL",
        "HERMES_INFERENCE_MODEL",
        // Review lens credentials #8: Hermes refreshes `$CODEX_HOME/auth.json`.
        "CODEX_HOME",
        "MY_CUSTOM",
    ] {
        assert_eq!(env.get(key), Some(&None), "{key} must be scrubbed");
    }
    assert_eq!(
        env.get("HOME"),
        Some(&Some(paths.child_home.display().to_string()))
    );
    assert_eq!(
        env.get("HERMES_HOME"),
        Some(&Some(paths.home.display().to_string()))
    );
    assert_eq!(
        env.get("HERMES_SHARED_AUTH_DIR"),
        Some(&Some(paths.shared.display().to_string()))
    );
    for prefixed in [
        "NOUS_PORTAL_URL",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
    ] {
        assert!(guards::in_static_scrub(prefixed), "{prefixed}");
    }
}

/// Test 21e: `new` pins every auxiliary task to the provider, one `config set`
/// each, with no lock held.
#[test]
fn new_pins_every_auxiliary_task() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    let got: Vec<Vec<String>> = fx.hermes_calls().into_iter().map(|c| c.argv).collect();
    let want: Vec<Vec<String>> = guards::HERMES_AUX_TASKS
        .iter()
        .map(|t| {
            vec![
                "config".into(),
                "set".into(),
                format!("auxiliary.{t}.provider"),
                "openrouter".into(),
            ]
        })
        .collect();
    assert_eq!(got, want);
    assert_eq!(guards::HERMES_AUX_TASKS.len(), 15);
    let sets: Vec<_> = points()
        .into_iter()
        .filter(|(w, _)| w == "hermes config set")
        .collect();
    assert_eq!(sets.len(), 15);
    assert!(sets.iter().all(|(_, held)| !held));
}

/// Test 25a: a `config.yaml` rewritten between the projector and the guard
/// refuses (M-CHANGED).
#[test]
fn audit_refuses_when_the_home_changes_between_projector_and_guard() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();
    fx.set_ctl(
        "projector.after",
        &format!(
            "echo 'display: {{}}' >> '{}/config.yaml'\n",
            paths.home.display()
        ),
    );
    fx.clear_rec();
    let err = crate::start::run_hermes("or-main", &[]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "tollgate: hermes 'or-main': home changed during audit; retry"
    );
    assert!(fx.hermes_calls().is_empty(), "Hermes never started");
}

/// Test 25b: no child process and no prompt runs under a tollgate lock, across
/// `new`, `auth` and `start`.
#[test]
fn no_child_process_runs_under_a_tollgate_lock() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");
    let mut pool = opts("pool-a", Provider::Openrouter);
    pool.pool = true;
    new_profile(&pool, &mut |_| unreachable!()).unwrap();
    let action = AuthAction::Add {
        provider: "openrouter".into(),
        auth_type: "api_key".into(),
        label: Some("work".into()),
        no_browser: false,
        timeout: None,
    };
    assert_eq!(run_auth("pool-a", &action).unwrap(), 0);
    assert_eq!(show::pool_strategy("pool-a", "round_robin").unwrap(), 0);
    crate::start::run_hermes("or-main", &["chat".into()]).unwrap();

    let seen = points();
    for what in [
        "the key prompt",
        "hermes config set",
        "the projector",
        "hermes auth",
        "hermes config set (strategy)",
        "the Hermes session",
    ] {
        assert!(
            seen.iter().any(|(w, _)| w == what),
            "{what} was never marked: {seen:?}"
        );
    }
    assert!(seen.iter().all(|(_, held)| !held), "{seen:?}");
}

/// Test 26: every pinned registry var and every plugin-scan var is removed
/// from the spawned env; `HERMES_MANAGED_DIR` passes through untouched.
#[test]
fn scrub_covers_registry_and_plugin_scan_and_passes_managed_dir() {
    let sb = HomeSandbox::new();
    let fx = Fixture::new(&sb.home().join("fx"));
    let paths = HermesPaths::for_name("or-main").unwrap();
    let dynamic = guards::plugin_env_vars(&guards::plugin_roots(&fx.hsp, &paths.home));
    assert!(dynamic.contains("TG_PLUGIN_ONLY_KEY"), "{dynamic:?}");
    let command = child_command(&fx.entry, &paths, &dynamic, &[]);
    let env = crate::testutil::env_overrides(&command);
    for key in guards::HERMES_REGISTRY_ENV_KEYS
        .iter()
        .copied()
        .chain(["TG_PLUGIN_ONLY_KEY"])
    {
        assert_eq!(env.get(key), Some(&None), "{key}");
    }
    assert!(
        !env.contains_key("HERMES_MANAGED_DIR"),
        "passed through, never altered"
    );
}

/// Test 27: the recorded argv and env of a real (stub) start: `--provider`
/// and `-m` from the roster ahead of the user args, `HERMES_HOME`,
/// `HERMES_SHARED_AUTH_DIR` and `HOME` pinned, the caller's cwd kept.
#[test]
fn spawn_command_pins_home_shared_dir_provider_and_model_before_user_args() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    let mut o = opts("or-main", Provider::Openrouter);
    o.model = Some("anthropic/claude-sonnet-4.5".into());
    new_profile(&o, &mut |_| Ok(TEST_KEY.into())).unwrap();
    fx.clear_rec();
    let args: Vec<String> = ["chat", "-q", "hi there"].map(String::from).to_vec();
    crate::start::run_hermes("or-main", &args).unwrap();
    let calls = fx.hermes_calls();
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert_eq!(
        c.argv,
        [
            "--provider",
            "openrouter",
            "-m",
            "anthropic/claude-sonnet-4.5",
            "chat",
            "-q",
            "hi there"
        ]
    );
    let paths = HermesPaths::for_name("or-main").unwrap();
    assert_eq!(
        c.env.get("HERMES_HOME").map(PathBuf::from),
        Some(paths.home.clone())
    );
    assert_eq!(
        c.env.get("HERMES_SHARED_AUTH_DIR").map(PathBuf::from),
        Some(paths.shared)
    );
    assert_eq!(c.env.get("HOME").map(PathBuf::from), Some(paths.child_home));
    assert_eq!(
        std::fs::canonicalize(&c.cwd).unwrap(),
        std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap()
    );
}

/// Test 28a: the fork seam. With a supervisor pid that is not the parent's,
/// the child `_exit(1)`s before exec; with the real one it runs.
#[cfg(target_os = "linux")]
#[test]
fn pdeathsig_child_exits_when_the_parent_is_already_gone() {
    let mut orphan = Command::new("/bin/sh");
    orphan.arg("-c").arg("exit 0");
    crate::start::install_pdeathsig(&mut orphan, 1);
    assert_eq!(orphan.status().unwrap().code(), Some(1));
    let mut child = Command::new("/bin/sh");
    child.arg("-c").arg("exit 0");
    crate::start::install_pdeathsig(&mut child, std::process::id() as libc::pid_t);
    assert_eq!(child.status().unwrap().code(), Some(0));
}

/// Test 31: a start registers a `harness = hermes` row and holds its marker
/// for the run (the stub sees both), GC leaves a live pair alone, and the
/// teardown removes both.
#[test]
fn start_registers_hermes_row_and_marker_then_tears_down() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    fx.clear_rec();
    crate::start::run_hermes("or-main", &[]).unwrap();
    let c = fx.hermes_calls().pop().unwrap();
    let marker_dir = c
        .profile_dir
        .iter()
        .find(|e| e.starts_with("sessions-"))
        .expect("the marker dir exists during the run")
        .clone();
    assert_eq!(c.rows.len(), 1, "{:?}", c.rows);
    let row: crate::live_sessions::LiveSession = serde_json::from_str(&c.rows[0]).unwrap();
    assert_eq!(row.harness, crate::harness::Harness::Hermes);
    assert_eq!(row.start_profile, "or-main");
    assert_eq!(row.launch_store, None);
    assert!(!row.follows_chain && !row.isolated);
    assert_eq!(format!("sessions-{}", row.session_id), marker_dir);

    // After the run: no marker dir, no row, not live.
    let paths = HermesPaths::for_name("or-main").unwrap();
    assert!(!paths.profile.join(&marker_dir).exists());
    assert!(crate::live_sessions::list().is_empty());
    assert!(!crate::runtime::has_live_session(&ProfileName::from(
        "or-main"
    )));

    // GC while a marker is held keeps the pair; the drop removes both.
    let guard =
        RotationGuard::acquire_with_timeout(&ProfileName::from("or-main"), ROTATION_WAIT).unwrap();
    let marker =
        crate::runtime::HermesMarker::claim("or-main", true, &guard, || unreachable!()).unwrap();
    drop(guard);
    let sid = marker.session_id().to_string();
    crate::runtime::gc_stale_runtimes();
    assert!(
        crate::live_sessions::get(&sid).is_some(),
        "GC must keep a live Hermes row"
    );
    assert!(crate::runtime::has_live_session(&ProfileName::from(
        "or-main"
    )));
    assert!(crate::runtime::session_row_is_live(
        &ProfileName::from("or-main"),
        false,
        &sid
    ));
    drop(marker);
    assert!(crate::live_sessions::get(&sid).is_none());
    assert!(!crate::runtime::has_live_session(&ProfileName::from(
        "or-main"
    )));
}

/// Test 32: a second start on a live home refuses (M-LIVE naming the
/// session), and so does a live `gateway.pid` (M-BUSY).
#[test]
fn second_start_on_same_profile_refuses_live() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    fx.clear_rec();
    let guard =
        RotationGuard::acquire_with_timeout(&ProfileName::from("or-main"), ROTATION_WAIT).unwrap();
    let marker =
        crate::runtime::HermesMarker::claim("or-main", true, &guard, || unreachable!()).unwrap();
    drop(guard);
    let err = crate::start::run_hermes("or-main", &[]).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "tollgate: hermes 'or-main': a Hermes session is already running on this home \
             (session {}); Hermes homes take one process at a time — start another account home \
             instead",
            marker.session_id()
        )
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
    drop(marker);
    assert!(fx.hermes_calls().is_empty());
}

#[test]
fn gateway_pid_live_refuses() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();
    for body in [
        std::process::id().to_string(),
        format!("{{\"pid\": {}, \"kind\": \"gateway\"}}", std::process::id()),
    ] {
        std::fs::write(paths.home.join("gateway.pid"), &body).unwrap();
        let err = crate::start::run_hermes("or-main", &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "tollgate: hermes 'or-main': the home is busy (a Hermes gateway); try again when it \
             finishes"
        );
    }
    // A dead pid is not busy.
    std::fs::write(paths.home.join("gateway.pid"), "999999999").unwrap();
    fx.clear_rec();
    crate::start::run_hermes("or-main", &[]).unwrap();
    assert_eq!(fx.hermes_calls().len(), 1);
}

/// Test 34: the post-session evidence check against a real `state.db`.
#[test]
#[ignore = "needs sqlite3"]
fn post_session_check_flags_anthropic_billing_rows() {
    let sb = HomeSandbox::new();
    let home = sb.home().join("h");
    std::fs::create_dir_all(&home).unwrap();
    let db = home.join("state.db");
    let path = std::env::var_os("PATH");
    let sqlite = resolve::which_on(path.as_deref(), "sqlite3").expect("sqlite3 on PATH");
    let ok = Command::new(sqlite)
        .arg(&db)
        .arg(
            "CREATE TABLE session_model_usage (session_id TEXT, billing_provider TEXT, model TEXT, \
             first_seen REAL, last_seen REAL); \
             INSERT INTO session_model_usage VALUES ('s1','openrouter','m',100,200); \
             INSERT INTO session_model_usage VALUES ('s1','anthropic','claude',100,150);",
        )
        .status()
        .unwrap();
    assert!(ok.success());
    assert!(post_session_anthropic_rows(&home, 120, path.as_deref()));
    assert!(
        !post_session_anthropic_rows(&home, 160, path.as_deref()),
        "billed before the run"
    );
    assert!(
        !post_session_anthropic_rows(&home, 120, None),
        "no sqlite3, no verdict"
    );
}

#[test]
fn post_session_check_refuses_symlinked_state_db() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let sb = HomeSandbox::new();
    let home = sb.home().join("h");
    let bin = sb.home().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(home.join("real.db"), "sentinel").unwrap();
    symlink(home.join("real.db"), home.join("state.db")).unwrap();
    let sqlite = bin.join("sqlite3");
    let invocation = home.join("sqlite-was-run");
    std::fs::write(
        &sqlite,
        format!(
            "#!/bin/sh\nprintf called > '{}'\necho '[{{\"billing_provider\":\"anthropic\",\"last_seen\":123}}]'\n",
            invocation.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&sqlite, std::fs::Permissions::from_mode(0o755)).unwrap();
    let warning = post_session_state_db_skip_warning(&home).expect("symlink warning");
    assert!(
        warning.contains("state.db") && warning.contains("symlink"),
        "{warning}"
    );
    assert!(!post_session_anthropic_rows(
        &home,
        100,
        Some(bin.as_os_str())
    ));
    assert!(!invocation.exists(), "sqlite3 followed the symlink");
}

/// `hermes key` rewrites the one line and the roster fingerprint, and
/// refuses on an OAuth or pool home.
#[test]
fn key_rewrites_the_bound_line_and_the_fingerprint() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    let mut o = opts("or-main", Provider::Openrouter);
    o.no_key = true;
    new_profile(&o, &mut |_| unreachable!()).unwrap();
    assert_eq!(
        HermesState::load()
            .unwrap()
            .find("or-main")
            .unwrap()
            .key_fingerprint,
        None
    );
    // A start before the key is set refuses M-NOKEY.
    let err = crate::start::run_hermes("or-main", &[]).unwrap_err();
    assert!(
        err.to_string()
            .ends_with("run 'tollgate hermes key or-main'"),
        "{err}"
    );

    set_key("or-main", &mut |_| Ok(format!("  {TEST_KEY}\n"))).unwrap();
    assert_eq!(
        HermesState::load()
            .unwrap()
            .find("or-main")
            .unwrap()
            .key_fingerprint
            .as_deref(),
        Some(TEST_KEY_FINGERPRINT)
    );
    new_profile(&opts("nous-a", Provider::Nous), &mut |_| unreachable!()).unwrap();
    let err = set_key("nous-a", &mut |_| unreachable!()).unwrap_err();
    assert!(err.to_string().contains("OAuth home"), "{err}");

    // An edit outside tollgate is re-attributed at the next start.
    let paths = HermesPaths::for_name("or-main").unwrap();
    std::fs::write(paths.env_file(), "OPENROUTER_API_KEY=edited-by-hand\n").unwrap();
    crate::start::run_hermes("or-main", &[]).unwrap();
    assert_eq!(
        HermesState::load()
            .unwrap()
            .find("or-main")
            .unwrap()
            .key_fingerprint
            .as_deref(),
        Some(env_file::fingerprint("edited-by-hand").as_str())
    );
}

/// `hermes auth` hands off Hermes' own argv, with no secret flag, and only the
/// profile's provider.
#[test]
fn auth_hands_off_to_hermes_auth_with_the_terminal_and_no_secret_flags() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_profile(&opts("nous-a", Provider::Nous), &mut |_| unreachable!()).unwrap();
    fx.clear_rec();
    let add = AuthAction::Add {
        provider: "nous".into(),
        auth_type: "oauth".into(),
        label: None,
        no_browser: true,
        timeout: Some(90),
    };
    assert_eq!(run_auth("nous-a", &add).unwrap(), 0);
    let c = fx.hermes_calls().pop().unwrap();
    assert_eq!(
        c.argv,
        [
            "auth",
            "add",
            "nous",
            "--type",
            "oauth",
            "--no-browser",
            "--timeout",
            "90"
        ]
    );
    assert!(
        c.rows.is_empty(),
        "auth claims a marker with no registry row"
    );
    assert!(
        c.profile_dir.iter().any(|e| e.starts_with("sessions-")),
        "…but holds the marker"
    );
    fx.set_ctl("hermes.exit", "3");
    assert_eq!(
        run_auth(
            "nous-a",
            &AuthAction::Reset {
                provider: "nous".into()
            }
        )
        .unwrap(),
        3
    );

    let wrong = AuthAction::Add {
        provider: "openrouter".into(),
        auth_type: "api-key".into(),
        label: None,
        no_browser: false,
        timeout: None,
    };
    let err = run_auth("nous-a", &wrong).unwrap_err();
    assert!(
        err.to_string()
            .contains("an account is one provider; this home is nous"),
        "{err}"
    );
    let anthropic = AuthAction::Add {
        provider: "anthropic".into(),
        auth_type: "api-key".into(),
        label: None,
        no_browser: false,
        timeout: None,
    };
    let err = run_auth("nous-a", &anthropic).unwrap_err();
    assert!(err.to_string().contains("routes to anthropic"), "{err}");
}

/// Test 37: guest mode (upstream clauth present) allows every Hermes verb,
/// raises no GUEST_REFUSAL, and leaves the operator trees byte-identical.
#[test]
fn guest_mode_allows_hermes_verbs_and_operator_trees_stay_byte_identical() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    let h = sb.home();
    std::fs::create_dir_all(h.join(".clauth/profiles/a")).unwrap();
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    std::fs::write(h.join(".claude/.credentials.json"), "{\"fake\":true}").unwrap();
    std::fs::write(h.join(".claude.json"), "{\"fake\":true}").unwrap();
    std::fs::create_dir_all(h.join(".codex")).unwrap();
    std::fs::write(h.join(".codex/auth.json"), "{}").unwrap();
    std::fs::create_dir_all(h.join(".hermes")).unwrap();
    std::fs::write(h.join(".hermes/config.yaml"), "model: x\n").unwrap();
    std::fs::create_dir_all(h.join(".config/herdr")).unwrap();
    std::fs::write(h.join(".config/herdr/config.toml"), "[x]\n").unwrap();
    assert!(crate::identity::upstream_active(), "guest mode is on");

    let trees = [".clauth", ".claude", ".codex", ".hermes", ".config/herdr"];
    let digest = || {
        let mut all: Vec<String> = trees.iter().flat_map(|t| tree_digest(&h.join(t))).collect();
        all.push(hex::encode(std::fs::read(h.join(".claude.json")).unwrap()));
        all
    };
    let before = digest();

    new_openrouter("or-main");
    set_key("or-main", &mut |_| Ok("sk-second".into())).unwrap();
    let mut pool = opts("pool-a", Provider::Openrouter);
    pool.pool = true;
    new_profile(&pool, &mut |_| unreachable!()).unwrap();
    let add = AuthAction::Add {
        provider: "openrouter".into(),
        auth_type: "api-key".into(),
        label: None,
        no_browser: false,
        timeout: None,
    };
    assert_eq!(run_auth("pool-a", &add).unwrap(), 0);
    crate::start::run_hermes("or-main", &[]).unwrap();
    assert!(!delete_profile("or-main", false).unwrap());
    assert!(!delete_profile("pool-a", false).unwrap());

    assert_eq!(digest(), before, "no operator tree changed");
    assert!(HermesState::load().unwrap().profiles().is_empty());
}

/// Test 36's body half: the delete removes the home, and the child home's
/// link targets survive (`remove_dir_all` never follows a link).
#[test]
fn delete_removes_the_home_and_keeps_the_link_targets() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    std::fs::create_dir_all(sb.home().join(".ssh")).unwrap();
    std::fs::write(sb.home().join(".ssh/id_ed25519"), "k").unwrap();
    std::fs::write(sb.home().join(".gitconfig"), "g").unwrap();
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();
    assert!(paths.child_home.join(".ssh").symlink_metadata().is_ok());

    // A live session refuses without --force.
    let guard =
        RotationGuard::acquire_with_timeout(&ProfileName::from("or-main"), ROTATION_WAIT).unwrap();
    let marker =
        crate::runtime::HermesMarker::claim("or-main", true, &guard, || unreachable!()).unwrap();
    drop(guard);
    let err = delete_profile("or-main", false).unwrap_err();
    assert!(
        err.to_string().contains("has a live session, pass --force"),
        "{err}"
    );
    assert!(
        delete_profile("or-main", true).unwrap(),
        "forced over a live session"
    );
    drop(marker);

    assert!(!paths.profile.exists());
    assert_eq!(
        std::fs::read_to_string(sb.home().join(".ssh/id_ed25519")).unwrap(),
        "k"
    );
    assert_eq!(
        std::fs::read_to_string(sb.home().join(".gitconfig")).unwrap(),
        "g"
    );
    assert!(!HermesState::load().unwrap().holds("or-main"));
}

/// The S7(f) gate refuses a start on a series the spike never passed.
#[test]
fn start_refuses_a_hermes_series_the_s7f_spike_has_not_passed() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = Fixture::with_version(&sb.home().join("fx"), "0.20.0");
    fx.install_as_settings_bin();
    new_openrouter("or-main");
    fx.clear_rec();
    let err = crate::start::run_hermes("or-main", &[]).unwrap_err();
    assert_eq!(
        err.to_string(),
        "tollgate: hermes 'or-main': the S7(f) HOME-redirect spike has not passed for Hermes 0.20.0"
    );
    assert!(fx.hermes_calls().is_empty());
}

#[test]
fn list_is_the_roster_and_reads_files_only() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    fx.clear_rec();
    list(true).unwrap();
    list(false).unwrap();
    assert!(
        fx.calls().is_empty(),
        "list never runs Hermes or its interpreter"
    );
}

/// A dir of recording stubs (`herdr`, `mise`, `hermes`, `python3`): each
/// appends `<name> <argv> HERMES_HOME=<v>` to `calls.log` beside it.
fn recording_stubs(sb: &HomeSandbox, names: &[&str]) -> PathBuf {
    let bin = sb.home().join("rec-bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in names {
        let path = bin.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"{name} $* HERMES_HOME=$HERMES_HOME\" >> '{}/calls.log'\nexit 0\n",
                bin.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn recorded(bin: &Path) -> Vec<String> {
    std::fs::read_to_string(bin.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Test 53 (H2h, §4.1 step 8): outside guest mode `new` runs `herdr
/// integration install hermes` once with the new home as `HERMES_HOME`; in
/// guest mode the herdr stub records nothing and the command is printed
/// instead.
#[test]
fn herdr_integration_install_skipped_in_guest_mode() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    let bin = recording_stubs(&sb, &["herdr"]);
    *HERDR_OVERRIDE.lock().unwrap() = Some(bin.join("herdr"));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            *HERDR_OVERRIDE.lock().unwrap() = None;
        }
    }
    let _reset = Reset;

    new_openrouter("or-main");
    let home = HermesPaths::for_name("or-main").unwrap().home;
    assert_eq!(
        recorded(&bin),
        [format!(
            "herdr integration install hermes HERMES_HOME={}",
            home.display()
        )],
        "one integration install, for this home"
    );

    std::fs::create_dir_all(sb.home().join(".clauth/profiles/a")).unwrap();
    assert!(crate::identity::upstream_active(), "guest mode is on");
    std::fs::remove_file(bin.join("calls.log")).unwrap();
    new_openrouter("or-guest");
    assert!(recorded(&bin).is_empty(), "guest mode runs no herdr");
}

/// Test 54: the daemon tick, `collect`, `hermes list`, `hermes show` without
/// `--check` and `herdr tag` never execute Hermes, its interpreter or `mise`:
/// the fixture's stub (the only Hermes and python there is) and the `mise` /
/// `hermes` / `python3` stubs on PATH record nothing. Only `sqlite3` may run.
#[test]
fn daemon_never_executes_hermes_python_or_mise() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    let bin = recording_stubs(&sb, &["mise", "hermes", "python3", "python"]);
    let path = std::ffi::OsString::from(format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    ));
    let _pin = crate::testutil::EnvPin::new(&sb, &[("PATH", Some(path.as_os_str()))]);
    fx.clear_rec();

    crate::usage::hermes_local::reset_scan_gate_for_test();
    crate::usage::hermes_local::refresh_detached();
    crate::testutil::join_background_tasks();
    assert!(
        crate::usage::hermes_local::load("or-main").is_some(),
        "the daemon leg wrote the usage cache"
    );
    let accounts = crate::usage::collect::collect(&crate::usage::collect::CollectOpts::default());
    assert!(accounts.iter().any(|o| o.id == "hermes:or-main"));
    list(true).unwrap();
    list(false).unwrap();
    show::show("or-main", true, false).unwrap();
    crate::herdr::tag::run(Some("or-main"), Some("hermes"), None, None).unwrap();
    let own = HermesPaths::for_name("or-main").unwrap().home;
    crate::herdr::tag::run(None, Some("hermes"), None, Some(&own)).unwrap();

    assert!(
        fx.calls().is_empty(),
        "no Hermes or interpreter run: {:?}",
        fx.calls()
    );
    assert!(
        recorded(&bin).is_empty(),
        "no mise, hermes or python: {:?}",
        recorded(&bin)
    );
}

/// Review lens credentials #7. An anthropic alias as the roster's own model
/// (`hermes new --model anthropic:…`) is refused like the same value in the
/// argv, and a roster that already holds one is refused at `start`'s
/// preflight: `start` would pass it as `-m`, past the argv scan.
#[test]
fn an_anthropic_alias_as_the_roster_model_is_refused_on_every_route() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    let mut o = opts("nm", Provider::Nous);
    o.model = Some("anthropic:claude-opus-4".to_string());
    let err = new_profile(&o, &mut |_| unreachable!()).unwrap_err();
    assert!(err.to_string().contains("-m"), "{err}");
    assert!(HermesState::load().unwrap().profiles().is_empty());
    // A roster written before the guard (or by hand) holds one.
    new_profile(&opts("nm", Provider::Nous), &mut |_| unreachable!()).unwrap();
    let mut profile = HermesState::load().unwrap().profiles()[0].clone();
    profile.model = Some("anthropic:claude-opus-4".to_string());
    let err = preflight_explain("nm", &profile, &[]).unwrap_err();
    assert!(err.to_string().contains("-m"), "{err}");
}
