//! `import clauth` part 2: the global edits (spec §4.8; tests 52–57). G1,
//! G3 and G4 are `main` entries inside the fence; G2 is the one `pre` edit,
//! run before any lock, with its config restored inside the fence and its
//! plugin reinstalled only after the fence is released.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::test_common::*;
use crate::testutil::{EnvPin, FakeHerdr};

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json")
}

/// A fake herdr listing upstream's plugin, herdr's config with upstream's
/// block, and a G2-capable upstream `clauth` as the only `PATH` dir. The pin
/// keeps `herdr::config_path` on the sandbox's file whatever the operator's
/// own environment says.
fn g2_setup(env: &Env) -> (FakeHerdr, PathBuf, EnvPin<'_>) {
    let config = env.tree.herdr_config();
    let herdr = env.tree.fake_herdr();
    let bin = env.tree.g2_upstream_bin(&config);
    let hb = herdr.bin.clone();
    env.seams.set(|s| {
        s.herdr_bin = Some(hb);
        s.path_dirs = vec![bin];
    });
    let pin = EnvPin::new(
        &env.home,
        &[
            ("HERDR_CONFIG_PATH", Some(config.as_os_str())),
            ("OWNER_API_KEY", Some("FIXTURE-KEY-env".as_ref())),
        ],
    );
    (herdr, config, pin)
}

/// The seq of the planned `main` entry for global edit `id`.
fn edit_seq(id: &str) -> u64 {
    survey(&Options::default())
        .plan
        .expect("plan")
        .entries
        .iter()
        .find(|e| e.after.step.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no {id} entry"))
        .seq
}

fn busy(path: &Path) -> bool {
    let f = std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Test 52. G1 is an M7 entry: while it runs, the `ImportFence` rank is on
/// this thread's rank stack and upstream's `~/.clauth/.lock` (fence item 8)
/// is held, so no upstream writer can lose-update `settings.json`. The
/// plugin is off afterwards, every other key as it was, and a rollback puts
/// the file back byte for byte.
#[test]
fn g1_turns_the_upstream_plugin_off_inside_the_fence_and_rollback_restores_it() {
    let env = Env::new();
    env.tree.reference();
    let settings = env.tree.write_json(
        &env.p(".claude/settings.json"),
        &json!({"theme": "dark", "enabledPlugins": {"clauth@clauth": true, "other@m": true}}),
    );
    let before = std::fs::read(&settings).expect("read");
    let g1 = edit_seq("G1");
    let seen = Arc::new(Mutex::new(None));
    let (seen2, lock) = (Arc::clone(&seen), env.p(".clauth/.lock"));
    env.seams.set(|s| {
        s.before_step = Some(Box::new(move |seq| {
            if seq == g1 {
                let fence = crate::lockorder::holds::<crate::lockorder::rank::ImportFence>();
                *seen2.lock().expect("seen") = Some((fence, busy(&lock)));
            }
        }));
    });
    run_ok();
    assert_eq!(
        *seen.lock().expect("seen"),
        Some((true, true)),
        "G1 ran outside the fence (rank ImportFence, item 8 held)"
    );
    let after = read_json(&settings);
    assert_eq!(after["enabledPlugins"]["clauth@clauth"], false);
    assert_eq!(after["enabledPlugins"]["other@m"], true);
    assert_eq!(after["theme"], "dark");
    let j = env.journal();
    let e = j
        .main
        .iter()
        .find(|e| e.after.step.as_deref() == Some("G1"))
        .expect("G1 journaled");
    assert_eq!(e.op, Op::RewriteJson);
    assert_eq!(
        e.prior.pointer.as_deref(),
        Some("/enabledPlugins/clauth@clauth")
    );
    assert_eq!(e.prior.prior_value.as_deref(), Some("true"));
    assert_eq!(e.after.new_value.as_deref(), Some("false"));
    assert_eq!(e.status, Status::Done);
    rollback(&Options::default()).expect("rolls back");
    assert_eq!(std::fs::read(&settings).expect("read"), before);
}

/// Test 53. G2 runs upstream's own `clauth herdr uninstall --yes` before the
/// fence is taken, with the env cleared down to the allowlist plus
/// upstream's three opt-outs (a secret in this process's env never reaches
/// it), after backing up herdr's config and journaling upstream's plugin
/// record. The `pre` entry records the argv, the env NAMES and both digests.
#[test]
fn g2_runs_upstream_herdr_uninstall_scrubbed_before_any_lock_with_a_config_backup_and_the_plugin_record()
 {
    let env = Env::new();
    env.tree.reference();
    let (herdr, config, _pin) = g2_setup(&env);
    let config_before = std::fs::read(&config).expect("config");
    env.seams.set(|s| s.op_log = Some(Vec::new()));
    run_ok();
    let log = env.seams.op_log();
    let g2 = log.iter().position(|l| l == "g2 exec").expect("g2 ran");
    let fence = log
        .iter()
        .position(|l| l == "fence acquired")
        .expect("fence taken");
    assert!(g2 < fence, "G2 must run before any lock: {log:?}");
    let g2log = std::fs::read_to_string(env.p("g2.log")).expect("g2 log");
    assert!(g2log.contains("argv=herdr uninstall --yes"), "{g2log}");
    for key in [
        "CLAUTH_NO_UPDATE",
        "CLAUTH_NO_COMPLETIONS",
        "CLAUTH_NO_API",
        "HERDR_BIN_PATH",
    ] {
        assert!(g2log.contains(key), "{key} not passed: {g2log}");
    }
    assert!(
        !g2log.contains("OWNER_API_KEY"),
        "an env secret leaked: {g2log}"
    );
    assert!(!herdr.installed(), "upstream's plugin is uninstalled");
    let text = std::fs::read_to_string(&config).expect("config");
    assert!(!text.contains("# clauth herdr plugin"), "{text}");
    let j = env.journal();
    assert_eq!(j.pre.len(), 1, "pre holds only G2");
    let e = &j.pre[0];
    assert_eq!(e.op, Op::Exec);
    assert_eq!(e.status, Status::Done);
    let rec = e.prior.plugin.as_ref().expect("plugin record");
    assert_eq!(rec.kind.as_deref(), Some("github"));
    assert_eq!(rec.owner.as_deref(), Some("uwuclxdy"));
    assert_eq!(rec.repo.as_deref(), Some("clauth"));
    assert_eq!(rec.resolved_commit.as_deref(), Some("abc123"));
    assert!(rec.managed_path.is_some());
    let backup = e.prior.backup.as_ref().expect("backup");
    assert_eq!(std::fs::read(backup).expect("backup"), config_before);
    assert_eq!(mode(backup), 0o600);
    assert_eq!(
        e.after.argv.as_deref().map(|a| a[1..].join(" ")),
        Some("herdr uninstall --yes".to_string())
    );
    let keys = e.after.env_keys.clone().unwrap_or_default();
    assert!(keys.iter().any(|k| k == "CLAUTH_NO_API"), "{keys:?}");
    assert!(!keys.iter().any(|k| k == "OWNER_API_KEY"), "{keys:?}");
    assert!(e.prior.sha256.is_some() && e.after.sha256.is_some());
    assert_ne!(e.prior.sha256, e.after.sha256);
    assert_no_fixture_secret(
        "journal",
        &std::fs::read_to_string(env.paths().journal()).expect("journal"),
    );
}

/// Make M4 refuse after G2 ran: the upstream fixture's G2 hook drops an
/// unknown file into `~/.clauth`, which M4's inventory refuses.
fn refuse_at_m4(env: &Env) {
    std::fs::write(
        env.p("g2-extra.sh"),
        format!("echo x > '{}'\n", env.p(".clauth/mystery.bin").display()),
    )
    .expect("extra");
}

/// Test 53a. A refusal after G2 restores herdr's config inside the fence and
/// reinstalls upstream's plugin at the RECORDED commit only after the fence
/// is released; offline, the reinstall fails and the exact command, plus
/// upstream's own fallback, is printed. Nothing is spawned while the fence
/// is held.
#[test]
fn g2_undo_reinstalls_at_the_recorded_commit_after_the_fence_and_prints_the_command_on_failure() {
    let env = Env::new();
    env.tree.reference();
    let (herdr, config, _pin) = g2_setup(&env);
    let config_before = std::fs::read(&config).expect("config");
    refuse_at_m4(&env);
    env.seams.set(|s| s.op_log = Some(Vec::new()));
    let e = run(&Options::default()).expect_err("M4 refuses");
    assert_eq!(blocked_codes(&e), ["unknown_entry"]);
    assert_eq!(std::fs::read(&config).expect("config"), config_before);
    assert!(herdr.installed(), "reinstalled");
    assert!(
        herdr
            .log()
            .lines()
            .any(|l| l == "plugin install uwuclxdy/clauth/herdr-plugin --ref abc123 --yes"),
        "{}",
        herdr.log()
    );
    let log = env.seams.op_log();
    let acquired = log
        .iter()
        .position(|l| l == "fence acquired")
        .expect("taken");
    let released = log
        .iter()
        .position(|l| l == "fence released")
        .expect("released");
    let reinstall = log
        .iter()
        .position(|l| l.starts_with("herdr reinstall"))
        .expect("reinstall");
    assert!(acquired < released && released < reinstall, "{log:?}");

    // Offline: the reinstall fails, and the command is printed.
    std::fs::remove_file(env.p(".clauth/mystery.bin")).expect("clean");
    std::fs::write(herdr.state.join("offline"), "").expect("offline");
    env.seams.set(|s| s.op_log = Some(Vec::new()));
    let e = run(&Options::default()).expect_err("M4 refuses again");
    assert_eq!(exit_of(Err(e)), 3);
    assert!(!herdr.installed(), "offline: nothing reinstalled");
    let printed: Vec<String> = env
        .seams
        .op_log()
        .into_iter()
        .filter_map(|l| l.strip_prefix("print ").map(str::to_string))
        .collect();
    assert!(
        printed.iter().any(|l| l
            .contains("run: herdr plugin install uwuclxdy/clauth/herdr-plugin --ref abc123 --yes")),
        "{printed:?}"
    );
    assert!(
        printed
            .iter()
            .any(|l| l.contains("clauth herdr install --yes --no-config")),
        "{printed:?}"
    );
    assert_eq!(std::fs::read(&config).expect("config"), config_before);
}

/// Test 54. G3 rebuilds upstream's `apiKeyHelper` as tollgate's for the
/// same (renamed) profile and renames each allowed upstream MCP tool; the
/// foreign rule and `env` stay put, and a rollback reverses both edits byte
/// for byte. The `env` secret never reaches the journal or the report.
#[test]
fn g3_rewrites_the_upstream_helper_and_permissions_allow_and_rollback_reverses_both() {
    let env = Env::new();
    env.tree.reference().helper_in_settings("personal");
    let bin = env.with_upstream_bin();
    let exe = env.tree.tollgate_bin();
    let exe2 = exe.clone();
    env.seams.set(|s| s.current_exe = Some(exe2));
    let _ = bin;
    let settings = env.p(".claude/settings.json");
    let before = std::fs::read(&settings).expect("read");
    let opts = Options {
        renames: [("personal".to_string(), "me".to_string())].into(),
        ..Options::default()
    };
    let report = txn::report(&survey(&opts), "dry_run");
    assert!(
        report.global_edits.iter().any(|g| g.id == "G3"),
        "{:?}",
        report.global_edits
    );
    assert_no_fixture_secret(
        "report",
        &serde_json::to_string(&report).expect("report json"),
    );
    run(&opts).expect("commits");
    let after = read_json(&settings);
    let want = crate::claude::build_api_key_helper_command(
        &exe,
        crate::claude::HelperForm::Profile,
        &crate::profile::ProfileName::from("me".to_string()),
    );
    assert_eq!(after["apiKeyHelper"], Value::String(want));
    assert_eq!(
        after["permissions"]["allow"],
        json!([
            "Bash(ls:*)",
            "mcp__plugin_tollgate_tollgate__profiles",
            "mcp__plugin_tollgate_tollgate__delegate"
        ])
    );
    assert_eq!(after["env"]["OWNER_TOKEN"], "FIXTURE-KEY-env");
    assert_no_fixture_secret(
        "journal",
        &std::fs::read_to_string(env.paths().journal()).expect("journal"),
    );
    assert_no_fixture_secret("backups", &read_tree_text(&env.paths().backup_dir()));
    rollback(&opts).expect("rolls back");
    assert_eq!(std::fs::read(&settings).expect("read"), before);
}

/// Test 55. G4 re-points each installPath under `~/.clauth/profiles/`: to
/// the tollgate copy of content the import brings over, else to the
/// `~/.claude/plugins` twin, else it stays and a warning names it. A
/// rollback puts every spelling back.
#[test]
fn g4_remaps_registry_paths_to_tollgate_then_the_twin_then_warns() {
    let env = Env::new();
    env.tree.reference();
    // (a) content the import copies: a claude profile's `codex-home/`.
    let copied = env
        .tree
        .dir("personal")
        .join("codex-home/plugins/cache/m/p/1");
    std::fs::create_dir_all(&copied).expect("copied");
    std::fs::write(copied.join("plugin.json"), "{}").expect("file");
    // (b) a skipped runtime tree whose `~/.claude/plugins` twin exists.
    let runtime = env
        .tree
        .dir("leadtone")
        .join("runtime-abc/plugins/cache/t/q/2");
    std::fs::create_dir_all(&runtime).expect("runtime");
    let twin = env.p(".claude/plugins/cache/t/q/2");
    std::fs::create_dir_all(&twin).expect("twin");
    // (c) a runtime path with no twin at all.
    let gone = env
        .tree
        .dir("scifoo")
        .join("runtime-xyz/plugins/cache/g/o/3");
    env.tree.registry_path(&[&copied, &runtime, &gone]);
    let registry = env.p(".claude/plugins/installed_plugins.json");
    let before = std::fs::read(&registry).expect("read");
    let s = survey(&Options::default());
    assert!(
        warning_codes(&s).iter().any(|c| c == "registry_path_kept"),
        "{:?}",
        warning_codes(&s)
    );
    run_ok();
    let after = read_json(&registry);
    let path_of = |i: usize| {
        after["plugins"][format!("other@market{i}")][0]["installPath"]
            .as_str()
            .expect("path")
            .to_string()
    };
    assert_eq!(
        path_of(0),
        env.p(".tollgate/profiles/personal/codex-home/plugins/cache/m/p/1")
            .display()
            .to_string()
    );
    assert!(Path::new(&path_of(0)).is_dir(), "the copy exists");
    assert_eq!(path_of(1), twin.display().to_string());
    assert_eq!(path_of(2), gone.display().to_string(), "kept");
    rollback(&Options::default()).expect("rolls back");
    assert_eq!(std::fs::read(&registry).expect("read"), before);
}

/// Test 56. A refusal after M0 (here M4's revalidation) reverses `pre` and
/// records `aborted`: `pre` held only G2, now undone; `settings.json` was
/// never touched (G1 is an M7 edit); exit 3.
#[test]
fn a_refusal_after_m0_undoes_pre_and_records_aborted() {
    let env = Env::new();
    env.tree.reference().helper_in_settings("personal");
    let (_herdr, config, _pin) = g2_setup(&env);
    let settings = env.p(".claude/settings.json");
    let (settings_before, config_before) = (
        std::fs::read(&settings).expect("settings"),
        std::fs::read(&config).expect("config"),
    );
    refuse_at_m4(&env);
    let e = run(&Options::default()).expect_err("M4 refuses");
    assert_eq!(exit_of(Err(e)), 3);
    let j = env.journal();
    assert_eq!(j.state, "aborted");
    assert_eq!(j.pre.len(), 1);
    assert_eq!(j.pre[0].op, Op::Exec);
    assert_eq!(j.pre[0].status, Status::Undone);
    assert!(j.main.is_empty());
    assert_eq!(std::fs::read(&settings).expect("settings"), settings_before);
    assert_eq!(std::fs::read(&config).expect("config"), config_before);
    assert!(crate::identity::upstream_active(), "guest mode stays on");
}

/// Test 57. An M-1 refusal (dry-run or real run) writes nothing anywhere:
/// no journal, no global file, and upstream's binary is never run.
#[test]
fn an_m_minus_1_refusal_touches_no_global_file() {
    let env = Env::new();
    env.tree
        .reference()
        .helper_in_settings("personal")
        .claude_json()
        .known_marketplaces()
        .bashrc()
        .unknown_entry("mystery.bin");
    let (herdr, _config, _pin) = g2_setup(&env);
    let fake_log = |rel: &str| rel.starts_with("fakeherdr/");
    let before = env.snapshot().without(fake_log);
    let s = survey(&Options::default());
    assert!(codes(&s).contains(&"unknown_entry".to_string()));
    let e = run(&Options::default()).expect_err("M-1 refuses");
    assert_eq!(exit_of(Err(e)), 3);
    assert_same_tree(&env.snapshot().without(fake_log), &before, "home");
    assert!(!env.p("g2.log").exists(), "upstream's binary ran");
    assert!(herdr.installed());
}

/// Self-review: G2's own failure paths. A non-zero exit and a child that
/// outlives its deadline both refuse with exit 3, put herdr's config back
/// and record the journal `aborted`; nothing moved.
#[test]
fn a_failing_or_hung_g2_refuses_with_exit_3_and_restores_the_config() {
    for hang in [false, true] {
        let env = Env::new();
        env.tree.reference();
        let (_herdr, config, _pin) = g2_setup(&env);
        let before = std::fs::read(&config).expect("config");
        if hang {
            // Strip the block, then hang past the deadline.
            std::fs::write(env.p("g2-extra.sh"), "sleep 5\n").expect("extra");
            env.seams
                .set(|s| s.g2_deadline = Some(std::time::Duration::from_millis(300)));
        } else {
            std::fs::write(env.p("g2-fail"), "").expect("fail");
        }
        let store = env.p(".clauth/profiles/personal/credentials.json");
        let e = run(&Options::default()).expect_err("G2 refuses");
        assert_eq!(blocked_codes(&e), ["g2_failed"], "hang={hang}");
        let text = format!("{e:#}");
        assert!(
            text.contains(if hang {
                "did not finish within"
            } else {
                "exited Some(3)"
            }),
            "{text}"
        );
        assert_eq!(exit_of(Err(e)), 3);
        assert_eq!(
            std::fs::read(&config).expect("config"),
            before,
            "hang={hang}"
        );
        assert!(store.exists(), "no store moved");
        let j = env.journal();
        assert_eq!(j.state, "aborted");
        assert_eq!(j.pre[0].status, Status::Undone);
    }
}

/// Self-review: a crash inside M0. Before G2 ran, `--resume` runs it once
/// and commits; after it ran but before `done` was written, `--resume`
/// settles it without running upstream's uninstall again; after it was
/// marked done, `import rollback` undoes `pre` alone (config bytes back,
/// the plugin reinstalled at its commit) and the journal is `rolled_back`.
#[test]
fn a_crash_in_pre_resumes_to_the_commit_or_rolls_back_pre() {
    let crash_run = |env: &Env| {
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run(&Options::default());
        }));
        assert!(out.is_err(), "the simulated crash fired");
        assert_eq!(env.journal().state, "pre");
    };
    let g2_runs = |env: &Env| {
        std::fs::read_to_string(env.p("g2.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("argv="))
            .count()
    };

    // Before G2 ran: resume runs it and commits.
    let env = Env::new();
    env.tree.reference();
    let (_herdr, _config, _pin) = g2_setup(&env);
    env.seams.set(|s| {
        s.before_step = Some(Box::new(|seq| {
            if seq == 0 {
                panic!("simulated crash before G2");
            }
        }));
    });
    crash_run(&env);
    assert_eq!(env.journal().pre[0].status, Status::Planned);
    env.seams.set(|s| s.before_step = None);
    txn::resume().expect("resume commits");
    assert_eq!(g2_runs(&env), 1);
    assert_eq!(env.journal().state, "complete");
    drop((_herdr, _config, _pin));
    drop(env);

    // After G2 ran, before `done`: resume does not run it again.
    let env = Env::new();
    env.tree.reference();
    let (herdr, _config, _pin) = g2_setup(&env);
    env.seams.set(|s| {
        s.before_step = Some(Box::new(|seq| {
            if seq == 0 {
                panic!("simulated crash before G2 was marked");
            }
        }));
    });
    crash_run(&env);
    let status = std::process::Command::new(env.p("bin/clauth"))
        .args(["herdr", "uninstall", "--yes"])
        .env("HERDR_BIN_PATH", &herdr.bin)
        .status()
        .expect("the child a crash interrupted");
    assert!(status.success());
    env.seams.set(|s| s.before_step = None);
    txn::resume().expect("resume commits");
    assert_eq!(g2_runs(&env), 1, "G2 ran twice");
    assert_eq!(env.journal().pre[0].status, Status::Done);
    drop((herdr, _config, _pin));
    drop(env);

    // G2 done, crash before the fence: rollback undoes pre alone.
    let env = Env::new();
    env.tree.reference();
    let (herdr, config, _pin) = g2_setup(&env);
    let before = std::fs::read(&config).expect("config");
    env.seams.set(|s| {
        s.before_fence = Some(Box::new(|| panic!("simulated crash before the fence")));
    });
    crash_run(&env);
    assert!(!herdr.installed());
    assert!(
        super::journal::interrupted_warning(&env.paths())
            .is_some_and(|w| w.contains("interrupted at step")),
    );
    rollback(&Options::default()).expect("rolls back pre");
    assert_eq!(std::fs::read(&config).expect("config"), before);
    assert!(herdr.installed(), "reinstalled after the fence");
    let j = env.journal();
    assert_eq!(j.state, "rolled_back");
    assert_eq!(j.pre[0].status, Status::Undone);
    assert!(env.p(".clauth/profiles/personal/credentials.json").exists());
}

/// Self-review: with every global edit planned, the whole run still enters
/// `ImportFence` once and no rank below `State` after the state flock (the
/// edits are raw read-modify-writes, never a guest-gated or config-ranked
/// helper).
#[cfg(debug_assertions)]
#[test]
fn the_global_edits_enter_no_rank_below_state() {
    use crate::lockorder::{rank, record_entries, value_of};
    let env = Env::new();
    env.tree
        .reference()
        .helper_in_settings("personal")
        .registry_path(&[]);
    let (_herdr, _config, _pin) = g2_setup(&env);
    let exe = env.tree.tollgate_bin();
    env.seams.set(|s| s.current_exe = Some(exe));
    let (result, entered) = record_entries(|| run(&Options::default()));
    result.expect("commits");
    let j = env.journal();
    for step in ["G1", "G3"] {
        assert!(
            j.main.iter().any(|e| e.after.step.as_deref() == Some(step)),
            "{step} not run"
        );
    }
    let state = value_of::<rank::State>();
    let at = entered.iter().position(|r| *r == state).expect("state");
    assert!(
        entered[at..].iter().all(|r| *r >= state),
        "a rank below State after item 9: {entered:?}"
    );
    assert_eq!(
        entered
            .iter()
            .filter(|r| **r == value_of::<rank::ImportFence>())
            .count(),
        1
    );
}

/// Self-review: the process scan names a process by its program and
/// subcommand only; an argument value on its command line (a key passed on
/// argv) never reaches a finding or the report.
#[test]
fn the_process_scan_never_echoes_an_argument_value() {
    let env = Env::new();
    env.tree.reference();
    env.procs(vec![
        proc_row(
            701,
            &["tollgate", "usage", "--account", "sk-ant-oat01-FIXTURE-p-1"],
        ),
        proc_row(
            702,
            &["tollgate", "login", "n", "--api-key", "FIXTURE-KEY-argv"],
        ),
        proc_row(703, &["claude", "--api-key", "FIXTURE-KEY-claude"]),
    ]);
    let s = survey(&Options::default());
    let report = serde_json::to_string(&txn::report(&s, "dry_run")).expect("json");
    assert_no_fixture_secret("report", &report);
    assert!(codes(&s).contains(&"tollgate_process_alive".to_string()));
    assert!(codes(&s).contains(&"process_alive".to_string()));
}

/// Review lens guest-ux #1. The dry run spawns no herdr (the real one creates
/// its plugin dirs and `.plugins.lock` when asked for its config dir or its
/// plugin list): G2 is still planned from herdr's config read in place, and
/// without upstream's marked block it is named "planned at run time".
#[test]
fn the_dry_run_plans_g2_without_spawning_herdr() {
    let env = Env::new();
    env.tree.reference();
    let (herdr, config, _pin) = g2_setup(&env);
    let report = txn::report(&survey(&Options::default()), "dry_run");
    let g2 = report
        .global_edits
        .iter()
        .find(|g| g.id == "G2")
        .expect("G2 planned");
    assert!(g2.change.contains("herdr uninstall --yes"), "{}", g2.change);
    assert_eq!(herdr.log(), "", "the dry run ran herdr");

    // No marked block: only herdr's plugin list could tell, so the row says
    // it is decided at run time, and herdr still never runs.
    std::fs::write(&config, "theme = \"dark\"\n").expect("unmarked");
    let report = txn::report(&survey(&Options::default()), "dry_run");
    let g2 = report
        .global_edits
        .iter()
        .find(|g| g.id == "G2")
        .expect("G2 row");
    assert!(
        g2.change.starts_with("planned at run time"),
        "{}",
        g2.change
    );
    assert_eq!(herdr.log(), "", "the dry run ran herdr");

    // The real run does ask herdr.
    let s = txn::survey(&Options::default(), txn::Mode::Run).expect("survey");
    assert!(s.g2.is_some());
    assert!(herdr.log().contains("plugin list"), "{}", herdr.log());
}
