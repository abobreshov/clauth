#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Hermes read surfaces (spec §7 part 2, tests 39–41): the whitelist pool
//! view against a secrets-bearing `auth.json`, env-mode attribution by
//! fingerprint, the strategy writer, and `show --check`. Every child process
//! is the fixture's shell stub.

use super::*;
use crate::hermes::profiles::Provider;
use crate::hermes::testkit::{Fixture, NoManagedScope, TEST_KEY_FINGERPRINT, new_openrouter};
use crate::testutil::HomeSandbox;
use std::process::Command;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hermes");

fn fixture(sb: &HomeSandbox) -> Fixture {
    let fx = Fixture::new(&sb.home().join("fx"));
    fx.install_as_settings_bin();
    fx
}

fn plant_auth(name: &str, file: &str) -> std::path::PathBuf {
    let paths = HermesPaths::for_name(name).unwrap();
    let dst = paths.home.join("auth.json");
    std::fs::copy(format!("{FIXTURES}/auth/{file}"), &dst).unwrap();
    dst
}

fn new_pool(name: &str) {
    let opts = super::super::NewOpts {
        name: name.to_string(),
        provider: Provider::Openrouter,
        model: None,
        pool: true,
        env_key: false,
        no_key: false,
    };
    super::super::new_profile(&opts, &mut |_| unreachable!()).unwrap();
}

/// No sqlite3 anywhere: these tests read no `state.db`.
fn no_path(sb: &HomeSandbox) -> std::ffi::OsString {
    let empty = sb.home().join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    empty.into_os_string()
}

/// Test 39: the pool view's structs hold no secret field, so the sentinels of
/// `secrets-bearing.json` reach no surface: the parsed view, `show` (text and
/// `--json`, `--check` included), the log lines, the `usage --json` envelope
/// the local API and the MCP `usage` tool serve, and the herdr tag.
#[test]
fn pool_view_holds_no_secret_fields_and_sentinels_never_reach_output() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");
    let auth = plant_auth("or-main", "secrets-bearing.json");
    assert!(
        std::fs::read_to_string(&auth).unwrap().contains("SENTINEL"),
        "the fixture carries its sentinels"
    );
    let path = no_path(&sb);
    let logs = crate::logline::LogLines::new();
    let _capture = logs.capture_here();

    let view = super::super::pool::read_auth_view(&HermesPaths::for_name("or-main").unwrap().home)
        .unwrap()
        .unwrap();
    let mut surfaces: Vec<(String, String)> = vec![("the parsed view".into(), format!("{view:?}"))];
    let shown = show_out("or-main", true, Some(&path)).unwrap();
    assert!(!check_refused(&shown), "{:?}", shown.check);
    surfaces.push(("show --json".into(), serde_json::to_string(&shown).unwrap()));
    surfaces.push(("show".into(), render_show(&shown)));
    let pool = shown.pool.as_ref().expect("the pool view renders");
    assert_eq!(pool.entries.len(), 2);
    assert_eq!(pool.entries[0].label.as_deref(), Some("work"));
    assert_eq!(pool.entries[0].fingerprint_tail.as_deref(), Some("cdef"));
    assert_eq!(
        pool.entries[1].reset_at.as_deref(),
        Some(crate::usage::epoch_secs_to_iso(1_790_000_000).as_str())
    );
    surfaces.push((
        "hermes list".into(),
        serde_json::to_string(&list_rows(Some(&path)).unwrap()).unwrap(),
    ));
    let report = crate::local_api::routes::usage_report(&crate::usage::collect::CollectOpts {
        include_disabled: true,
        ..Default::default()
    });
    surfaces.push((
        "usage --json / MCP usage".into(),
        serde_json::to_string(&report).unwrap(),
    ));
    let accounts = crate::usage::collect::collect(&crate::usage::collect::CollectOpts::default());
    let tag = crate::herdr::tag::resolve_tag(
        &accounts,
        Some("or-main"),
        "hermes",
        None,
        crate::usage::now_epoch_secs(),
    )
    .unwrap();
    surfaces.push((
        "herdr tag".into(),
        crate::herdr::tag::tag_lines(&tag).join("\n"),
    ));
    surfaces.push(("the log lines".into(), logs.snapshot().join("\n")));

    for (what, text) in &surfaces {
        assert!(
            !text.contains("SENTINEL"),
            "{what} carries a secret:\n{text}"
        );
    }
    // The fingerprint is Hermes' own public digest, but no surface prints it
    // whole: only its last four digits.
    for (what, text) in &surfaces[1..] {
        assert!(
            !text.contains("0123456789abcdef"),
            "{what} prints a whole fingerprint:\n{text}"
        );
    }
    assert!(
        surfaces[1].1.contains("\"fingerprint_tail\":\"cdef\""),
        "the JSON keeps only the fingerprint's tail"
    );
}

/// Test 40: env-mode attribution marks the one `env:<key_env>` entry whose
/// fingerprint is the roster's: not a same-source entry with another key, not
/// a manual one.
#[test]
fn pool_view_marks_env_entry_by_fingerprint() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");
    plant_auth("or-main", "pool-env-openrouter.json");
    let profile = super::super::find_profile("or-main").unwrap();
    assert_eq!(
        profile.key_fingerprint.as_deref(),
        Some(TEST_KEY_FINGERPRINT)
    );
    let view = super::super::pool::read_auth_view(&HermesPaths::for_name("or-main").unwrap().home)
        .unwrap()
        .unwrap();
    let pool = pool_out(&view, &profile);
    let marked: Vec<(&str, bool)> = pool
        .entries
        .iter()
        .map(|e| (e.id.as_deref().unwrap(), e.tollgate_key))
        .collect();
    assert_eq!(marked, [("env1", true), ("m1", false), ("m2", false)]);
    let lines = pool_lines(&pool);
    assert_eq!(
        lines[0],
        "#1 OPENROUTER_API_KEY  api_key/env:OPENROUTER_API_KEY  ok  req 5  prio 0  fp …5ae2  \
         (the key tollgate bound)"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("tollgate bound"))
            .count(),
        1
    );

    // An OAuth or pool home never claims an entry, whatever it holds.
    let mut pool_profile = profile.clone();
    pool_profile.auth = crate::hermes::profiles::Auth::Pool;
    pool_profile.mode = Mode::Pool;
    assert!(
        pool_out(&view, &pool_profile)
            .entries
            .iter()
            .all(|e| !e.tollgate_key)
    );
    let _ = sb;
}

/// Test 41: the strategy writer runs `config set
/// credential_pool_strategies.<provider> <s>` with the child env, holding a
/// marker (no row) and no tollgate lock across the child; it refuses a live
/// home and an account home, and never touches `auth.json`.
#[test]
fn strategy_writer_runs_hermes_config_set_idle_only_and_never_writes_auth_json() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_pool("pool-a");
    let auth = plant_auth("pool-a", "secrets-bearing.json");
    let before = std::fs::read(&auth).unwrap();
    let paths = HermesPaths::for_name("pool-a").unwrap();
    fx.clear_rec();
    super::super::UNLOCKED_POINTS.with(|p| p.borrow_mut().clear());

    assert_eq!(pool_strategy("pool-a", "round_robin").unwrap(), 0);
    let calls = fx.hermes_calls();
    assert_eq!(calls.len(), 1, "one config set");
    let call = &calls[0];
    assert_eq!(
        call.argv,
        [
            "config",
            "set",
            "credential_pool_strategies.openrouter",
            "round_robin"
        ]
    );
    assert_eq!(
        call.env.get("HERMES_HOME").map(String::as_str),
        paths.home.to_str()
    );
    assert_eq!(
        call.env.get("HOME").map(String::as_str),
        paths.child_home.to_str()
    );
    assert!(
        call.profile_dir.iter().any(|e| e.starts_with("sessions-")),
        "a marker holds the home across the child: {:?}",
        call.profile_dir
    );
    assert!(
        call.live_rows.is_empty(),
        "the writer registers no session row"
    );
    assert!(
        !std::fs::read_dir(&paths.profile)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("sessions-")),
        "the marker is gone after"
    );
    let points = super::super::UNLOCKED_POINTS.with(|p| p.borrow().clone());
    assert!(
        points
            .iter()
            .any(|(w, held)| w == "hermes config set (strategy)" && !held),
        "{points:?}"
    );
    assert_eq!(std::fs::read(&auth).unwrap(), before, "auth.json untouched");

    // A live home refuses, and nothing runs.
    let guard =
        RotationGuard::acquire_with_timeout(&ProfileName::from("pool-a"), ROTATION_WAIT).unwrap();
    let marker =
        crate::runtime::HermesMarker::claim("pool-a", true, &guard, || unreachable!()).unwrap();
    drop(guard);
    fx.clear_rec();
    let err = pool_strategy("pool-a", "fill_first").unwrap_err();
    assert!(
        err.to_string()
            .contains("a Hermes session is already running on this home"),
        "{err}"
    );
    assert!(fx.hermes_calls().is_empty());
    drop(marker);

    // An account home has one credential: no strategy.
    new_openrouter("or-main");
    let err = pool_strategy("or-main", "random").unwrap_err();
    assert!(err.to_string().contains("this is an account home"), "{err}");
    assert!(pool_strategy("pool-a", "sideways").is_err());
    assert_eq!(std::fs::read(&auth).unwrap(), before);
}

#[test]
fn strategy_refuses_unsafe_homes_and_install_env_before_hermes_runs() {
    use crate::hermes::testkit::passing_projection;
    for hazard in ["anthropic_key", "claude_token", "child_claude", "hsp_env"] {
        let sb = HomeSandbox::new();
        let _scope = NoManagedScope::new(&sb);
        let fx = fixture(&sb);
        new_pool("pool-a");
        let paths = HermesPaths::for_name("pool-a").unwrap();
        fx.clear_rec();
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
        let err = pool_strategy("pool-a", "round_robin").unwrap_err();
        let why = err.to_string();
        match hazard {
            "anthropic_key" | "claude_token" => {
                assert!(why.contains(".env routes to anthropic"), "{hazard}: {why}");
            }
            "child_claude" => assert!(why.contains("holds '.claude'"), "{why}"),
            "hsp_env" => assert!(why.contains("/.env exists; Hermes loads it"), "{why}"),
            _ => unreachable!(),
        }
        assert!(fx.hermes_calls().is_empty(), "{hazard}: Hermes ran");
    }
}

/// `show --check` reports every guard in launch order and stops at the first
/// refusal; `show` without it runs nothing but file reads.
#[test]
fn show_check_reports_each_guard_and_stops_at_the_first_refusal() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_openrouter("or-main");
    let path = no_path(&sb);

    fx.clear_rec();
    let plain = show_out("or-main", false, Some(&path)).unwrap();
    assert!(plain.check.is_none());
    assert!(fx.calls().is_empty(), "show without --check runs nothing");
    assert_eq!(plain.provider, "openrouter");
    assert_eq!(plain.key_env.as_deref(), Some("OPENROUTER_API_KEY"));

    let checked = show_out("or-main", true, Some(&path)).unwrap();
    let guards: Vec<&str> = checked
        .check
        .as_ref()
        .unwrap()
        .iter()
        .map(|c| c.guard)
        .collect();
    assert_eq!(
        guards,
        [
            "G1 shape",
            "G2 containment",
            "G2a child home",
            "G3 active_profile",
            "G4 profiles",
            "entrypoint",
            "G6 site .env",
            "G14 liveness",
            "G15 version",
            "S7(f) gate",
            "P projector",
            "G7–G10a config",
            "G11 auth.json",
            "G12 env",
            "G13 binding",
        ]
    );
    assert!(!check_refused(&checked));
    assert!(render_show(&checked).contains("G13 binding        ok"));

    let paths = HermesPaths::for_name("or-main").unwrap();
    std::fs::create_dir_all(paths.home.join("profiles")).unwrap();
    let refused = show_out("or-main", true, Some(&path)).unwrap();
    let last = refused.check.as_ref().unwrap().last().unwrap().clone();
    assert_eq!(last.guard, "G4 profiles");
    assert!(
        matches!(&last.verdict, Verdict::Refused { text } if text.contains("Hermes sub-profiles")),
        "{last:?}"
    );
    assert!(check_refused(&refused));
    assert!(render_show(&refused).contains("G4 profiles        REFUSED"));
    std::fs::remove_dir_all(paths.home.join("profiles")).unwrap();

    // A key edited outside tollgate is a note, not a repair: the roster keeps
    // its fingerprint until a launch re-attributes it.
    std::fs::write(paths.env_file(), "OPENROUTER_API_KEY=sk-other\n").unwrap();
    let noted = show_out("or-main", true, Some(&path)).unwrap();
    let g13 = noted.check.as_ref().unwrap().last().unwrap().clone();
    assert!(
        matches!(&g13.verdict, Verdict::Note { text } if text.contains("changed outside tollgate")),
        "{g13:?}"
    );
    assert_eq!(
        super::super::find_profile("or-main")
            .unwrap()
            .key_fingerprint
            .as_deref(),
        Some(TEST_KEY_FINGERPRINT)
    );
}

/// `hermes show` lists the five latest sessions off `state.db`, newest first.
#[test]
#[ignore = "needs sqlite3"]
fn show_lists_the_latest_sessions_from_sqlite() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();
    let path = std::env::var_os("PATH");
    let sqlite = resolve::which_on(path.as_deref(), "sqlite3").expect("sqlite3");
    let status = Command::new(sqlite)
        .arg(paths.home.join("state.db"))
        .stdin(std::fs::File::open(format!("{FIXTURES}/state-v22.sql")).unwrap())
        .status()
        .unwrap();
    assert!(status.success());
    let s = show_out("or-main", false, path.as_deref()).unwrap();
    let ids: Vec<&str> = s.sessions.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(ids, ["s-sep2", "s-sep1", "s-aug"]);
    assert_eq!(s.sessions[0].title.as_deref(), Some("write the docs"));
    assert!(render_show(&s).contains("-- --resume <id>"));
}

/// The plain list table carries the estimate or the reason there is none,
/// its columns padded to line up (review lens guest-ux #14).
#[test]
fn list_renders_the_estimate_or_the_usage_error() {
    let rows = vec![
        ListRow {
            name: "a".into(),
            provider: "openrouter",
            mode: "account",
            auth: "env",
            model: Some("m".into()),
            live: true,
            estimate_usd: Some("0.4212".into()),
            usage_error: None,
        },
        ListRow {
            name: "b".into(),
            provider: "nous",
            mode: "pool",
            auth: "pool",
            model: None,
            live: false,
            estimate_usd: None,
            usage_error: Some("sqlite3_missing".into()),
        },
    ];
    assert_eq!(
        render_list(&rows),
        "● a  openrouter  account home  auth env  model m  $0.42 this month\n  b  nous        \
         pool home     auth pool  usage: sqlite3_missing\n"
    );
    assert!(render_list(&[]).starts_with("no Hermes profiles"));
}

/// A torn `auth.json` (Hermes mid-write) is a pool line saying so, never an
/// error or a partial view; and a label carrying control characters reaches
/// the terminal without them.
#[test]
fn show_survives_a_torn_auth_json_and_strips_control_characters() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let _fx = fixture(&sb);
    new_openrouter("or-main");
    let paths = HermesPaths::for_name("or-main").unwrap();
    std::fs::write(
        paths.home.join("auth.json"),
        "{\"version\": 1, \"credential_pool\": {",
    )
    .unwrap();
    let s = show_out("or-main", false, Some(&no_path(&sb))).unwrap();
    assert!(s.pool.is_none());
    assert!(
        s.pool_error
            .as_deref()
            .is_some_and(|e| e.contains("retry when Hermes is not writing it")),
        "{s:?}"
    );
    assert!(render_show(&s).contains("pool        auth.json cannot be read now"));

    std::fs::write(
        paths.home.join("auth.json"),
        r#"{"version":1,"credential_pool":{"openrouter":[{"label":"evil\u001b[2Jlabel","source":"manual","auth_type":"api_key","last_status":"ok\u0007"}]}}"#,
    )
    .unwrap();
    let s = show_out("or-main", false, Some(&no_path(&sb))).unwrap();
    let lines = pool_lines(s.pool.as_ref().unwrap());
    assert_eq!(
        lines[0],
        "#1 evil[2Jlabel  api_key/manual  ok  req -  prio -"
    );
    assert!(
        !render_show(&s)
            .chars()
            .any(|c| c == '\u{1b}' || c == '\u{7}')
    );
}

/// A `config set` that fails returns Hermes' exit code and still drops the
/// marker, so the home is not left busy.
#[test]
fn strategy_writer_returns_the_childs_code_and_releases_the_home() {
    let sb = HomeSandbox::new();
    let _scope = NoManagedScope::new(&sb);
    let fx = fixture(&sb);
    new_pool("pool-a");
    fx.set_ctl("hermes.exit", "3");
    assert_eq!(pool_strategy("pool-a", "least_used").unwrap(), 3);
    assert!(!crate::runtime::has_live_session(&ProfileName::from(
        "pool-a"
    )));
    fx.set_ctl("hermes.exit", "0");
    assert_eq!(pool_strategy("pool-a", "least_used").unwrap(), 0);
}
