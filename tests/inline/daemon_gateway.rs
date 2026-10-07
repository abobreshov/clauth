#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The gateway supervisor: the slot's closed state set, the record-only slot a
//! body builds without a supervisor, and the state machine stepped by hand
//! with an injected clock over a stub `shunt` in a `HomeSandbox`. The stub is
//! a `/bin/sh` script tied to a loopback `/health` answerer through a named
//! pipe (`tests/support/gateway_stub.rs`), so every stub test is
//! `#[cfg(unix)]`; the serialization and record-only tests run everywhere.

use std::fs;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
use std::time::Duration;

use super::*;
use crate::gateway::GatewayRecord;
use crate::testutil::HomeSandbox;

#[cfg(unix)]
#[path = "../support/gateway_stub.rs"]
mod stub;

/// The injected wall clock every stepped test starts at, and its stamps.
#[cfg(unix)]
const T0_WALL_MS: u64 = 1_790_000_000_000;
const AT_T0: &str = "2026-09-21T14:13:20+00:00";
#[cfg(unix)]
const AT_T1: &str = "2026-09-21T14:13:21+00:00";
#[cfg(unix)]
const AT_T2: &str = "2026-09-21T14:13:22+00:00";
#[cfg(unix)]
const AT_T5: &str = "2026-09-21T14:13:25+00:00";
#[cfg(unix)]
const AT_T10: &str = "2026-09-21T14:13:30+00:00";
#[cfg(unix)]
const AT_T14: &str = "2026-09-21T14:13:34+00:00";
#[cfg(unix)]
const AT_T15: &str = "2026-09-21T14:13:35+00:00";

fn blank(state: GatewayState) -> GatewaySlot {
    GatewaySlot {
        state,
        config: None,
        binary: None,
        port: None,
        pid: None,
        version: None,
        answerer: None,
        floor: "0.48.0".to_string(),
        restarts: 0,
        last_exit: None,
        reason: None,
        since: None,
    }
}

fn shown(path: &std::path::Path) -> Option<String> {
    Some(path.display().to_string())
}

// ── the slot ────────────────────────────────────────────────────────────────

const ALL_STATES: [GatewayState; 14] = [
    GatewayState::Absent,
    GatewayState::Disabled,
    GatewayState::NoConfig,
    GatewayState::YamlRefused,
    GatewayState::Misconfigured,
    GatewayState::BinaryMissing,
    GatewayState::Foreign,
    GatewayState::Starting,
    GatewayState::Healthy,
    GatewayState::Unhealthy,
    GatewayState::BelowFloor,
    GatewayState::Restarting,
    GatewayState::Stopping,
    GatewayState::Unobserved,
];

/// One slot per state, every field fixed, and its exact bytes. The match is
/// wildcard-free, so a new state does not compile until it has a fixture.
fn fixture(state: GatewayState) -> (GatewaySlot, &'static str) {
    let config = Some("/etc/shunt/shunt.toml".to_string());
    let binary = Some("/usr/local/bin/shunt".to_string());
    let since = Some(AT_T0.to_string());
    match state {
        GatewayState::Absent => (
            GatewaySlot {
                since,
                ..blank(state)
            },
            r#"{"state":"absent","config":null,"binary":null,"port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Disabled => (
            GatewaySlot {
                config,
                binary,
                restarts: 2,
                last_exit: Some(ExitReport {
                    code: None,
                    signal: Some(15),
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"disabled","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":2,"last_exit":{"code":null,"signal":15},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::NoConfig => (
            GatewaySlot {
                config,
                binary: Some("shunt".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"no_config","config":"/etc/shunt/shunt.toml","binary":"shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::YamlRefused => (
            GatewaySlot {
                config: Some("/etc/shunt/shunt.yaml".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"yaml_refused","config":"/etc/shunt/shunt.yaml","binary":null,"port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Misconfigured => (
            GatewaySlot {
                config,
                binary,
                reason: Some(
                    "in env file /etc/shunt/tokens.env: line 3 holds a NUL byte; systemd refuses such a file whole, and so does tollgate: remove the byte"
                        .to_string(),
                ),
                since,
                ..blank(state)
            },
            r#"{"state":"misconfigured","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":"in env file /etc/shunt/tokens.env: line 3 holds a NUL byte; systemd refuses such a file whole, and so does tollgate: remove the byte","since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::BinaryMissing => (
            GatewaySlot {
                config,
                binary: Some("/opt/shunt/bin/shunt".to_string()),
                port: Some(3067),
                since,
                ..blank(state)
            },
            r#"{"state":"binary_missing","config":"/etc/shunt/shunt.toml","binary":"/opt/shunt/bin/shunt","port":3067,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Foreign => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                version: Some("0.49.1".to_string()),
                answerer: Some(Answerer::Shunt),
                since,
                ..blank(state)
            },
            r#"{"state":"foreign","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":null,"version":"0.49.1","answerer":"shunt","floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Starting => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                since,
                ..blank(state)
            },
            r#"{"state":"starting","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Healthy => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                version: Some("0.49.1".to_string()),
                restarts: 1,
                last_exit: Some(ExitReport {
                    code: Some(1),
                    signal: None,
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"healthy","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":"0.49.1","answerer":null,"floor":"0.48.0","restarts":1,"last_exit":{"code":1,"signal":null},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Unhealthy => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                version: Some("0.49.1".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"unhealthy","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":"0.49.1","answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::BelowFloor => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                version: Some("0.47.0".to_string()),
                last_exit: Some(ExitReport {
                    code: None,
                    signal: Some(15),
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"below_floor","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":null,"version":"0.47.0","answerer":null,"floor":"0.48.0","restarts":0,"last_exit":{"code":null,"signal":15},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Restarting => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                restarts: 3,
                last_exit: Some(ExitReport {
                    code: None,
                    signal: Some(9),
                }),
                since,
                ..blank(state)
            },
            r#"{"state":"restarting","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":3,"last_exit":{"code":null,"signal":9},"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Stopping => (
            GatewaySlot {
                config,
                binary,
                port: Some(3067),
                pid: Some(4242),
                version: Some("0.49.1".to_string()),
                since,
                ..blank(state)
            },
            r#"{"state":"stopping","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":3067,"pid":4242,"version":"0.49.1","answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":"2026-09-21T14:13:20+00:00"}"#,
        ),
        GatewayState::Unobserved => (
            GatewaySlot {
                config,
                binary,
                ..blank(state)
            },
            r#"{"state":"unobserved","config":"/etc/shunt/shunt.toml","binary":"/usr/local/bin/shunt","port":null,"pid":null,"version":null,"answerer":null,"floor":"0.48.0","restarts":0,"last_exit":null,"reason":null,"since":null}"#,
        ),
    }
}

#[test]
fn every_state_serializes_to_its_fixture() {
    for state in ALL_STATES {
        let (slot, bytes) = fixture(state);
        assert_eq!(
            serde_json::to_string(&slot).expect("serialize"),
            bytes,
            "{state:?}"
        );
    }
    for (answerer, bytes) in [
        (Answerer::Shunt, r#""shunt""#),
        (Answerer::NotShunt, r#""not_shunt""#),
        (Answerer::NoAnswer, r#""no_answer""#),
    ] {
        assert_eq!(
            serde_json::to_string(&answerer).expect("serialize"),
            bytes,
            "{answerer:?}"
        );
    }
}

#[test]
fn the_drain_bound_is_the_env_then_the_config_then_shunts_default() {
    let with = "[server]\nshutdown_timeout_seconds = 45\n";
    for (config, env, secs, case) in [
        (with, Some("7"), 7, "the env outranks the config"),
        (with, None, 45, "the config's value"),
        (
            "[server]\nbind = \"127.0.0.1:3001\"\n",
            None,
            30,
            "shunt's default",
        ),
        ("", None, 30, "an empty config"),
        (
            with,
            Some("soon"),
            3600,
            "an env value tollgate cannot read",
        ),
        (
            "[server]\nshutdown_timeout_seconds = \"soon\"\n",
            None,
            3600,
            "a config value that is not a number",
        ),
        (
            "[server]\nshutdown_timeout_seconds = 7200\n",
            None,
            3600,
            "past shunt's own maximum",
        ),
        ("not = [toml", None, 3600, "a config that does not parse"),
    ] {
        assert_eq!(
            crate::gateway::resolve_shutdown_timeout(config, env),
            Duration::from_secs(secs),
            "{case}"
        );
    }
}

/// With no supervisor (single-shot `status --json`, a daemonless republish),
/// the slot says only what the record says, and never claims a running state.
#[test]
fn without_a_supervisor_the_slot_reads_the_record_alone() {
    let home = HomeSandbox::new();
    assert_eq!(unsupervised_slot(), blank(GatewayState::Absent));

    let dir = home.home().join("etc");
    fs::create_dir_all(&dir).expect("etc");
    let config = dir.join("shunt.toml");
    fs::write(&config, "[server]\n").expect("config");
    let mut record = GatewayRecord::new(config).expect("adoptable");
    let canonical = shown(record.config());
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("save");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: canonical.clone(),
            binary: Some("shunt".to_string()),
            ..blank(GatewayState::Unobserved)
        }
    );

    record.disabled = true;
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("save");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: canonical.clone(),
            binary: Some("shunt".to_string()),
            ..blank(GatewayState::Disabled)
        }
    );

    record.disabled = false;
    GatewayRecord::update(|slot| {
        *slot = Some(record.clone());
        Ok(())
    })
    .expect("save");
    fs::remove_file(record.config()).expect("remove the config");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: canonical,
            binary: Some("shunt".to_string()),
            ..blank(GatewayState::NoConfig)
        }
    );

    let path = crate::gateway::record_path().expect("record path");
    fs::write(&path, "config = \"/etc/shunt/shunt.yaml\"\n").expect("hand-edit");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            config: Some("/etc/shunt/shunt.yaml".to_string()),
            ..blank(GatewayState::YamlRefused)
        }
    );

    fs::write(&path, "config = \"shunt.toml\"\n").expect("hand-edit");
    assert_eq!(
        unsupervised_slot(),
        GatewaySlot {
            reason: Some(format!(
                "invalid gateway record {}: the adopted shunt config must be an absolute path, got shunt.toml",
                path.display()
            )),
            ..blank(GatewayState::Misconfigured)
        }
    );
}

/// The gateway's log stays under the daemon's size cap on the supervisor's
/// own cadence: a step trims an oversized `gateway.log` in place to its last
/// whole lines within 1 MiB.
#[test]
fn a_step_trims_an_oversized_gateway_log_to_its_tail() {
    let _home = HomeSandbox::new();
    let dir = tollgate_dir().expect("dir");
    fs::create_dir_all(&dir).expect("dir");
    let log = dir.join("gateway.log");
    // 52,429 lines of 100 bytes: 5,242,900 bytes, 20 past the 5 MiB cap.
    let text: String = (0..52_429).map(|n| format!("{n:099}\n")).collect();
    fs::write(&log, text).expect("an oversized log");

    Supervisor::new(new_handle()).step(Tick::now());

    // The last 1 MiB starts inside line 41,943, at byte 4,194,324; that
    // partial line goes, so lines 41,944 to 52,428 stay: 10,485 of them.
    let kept = fs::read_to_string(&log).expect("the log");
    assert_eq!(kept.len(), 1_048_500, "trimmed to its tail");
    assert_eq!(
        kept.lines().next(),
        Some(format!("{:099}", 41_944).as_str()),
        "the first whole line of the last 1 MiB"
    );
}

// ── the supervisor, over a stub ─────────────────────────────────────────────

/// A sandbox holding an adopted config bound to a free loopback port with a
/// 2 s drain, an env file, the stub `shunt` and its `/health` answerer, and
/// a record naming all three. Lines the supervisor logs on this thread are
/// captured. Declared before the supervisors a test steps, so they (and the
/// stubs they own) are gone before the answerer's teardown.
#[cfg(unix)]
struct Rig {
    server: stub::HealthServer,
    handle: GatewayHandle,
    lines: crate::logline::LogLines,
    _capture: crate::logline::LogCapture,
    dir: PathBuf,
    etc: PathBuf,
    config: PathBuf,
    binary: PathBuf,
    env_file: PathBuf,
    port: u16,
    home: HomeSandbox,
}

#[cfg(unix)]
impl Rig {
    fn new(version: &str) -> Self {
        let home = HomeSandbox::new();
        let lines = crate::logline::LogLines::new();
        let capture = lines.capture_here();
        fs::create_dir_all(home.home().join("etc")).expect("etc");
        // The record holds the canonical path, so every expectation does too.
        let etc = fs::canonicalize(home.home().join("etc")).expect("canonical etc");
        let port = stub::free_port();
        let config = etc.join("shunt.toml");
        fs::write(
            &config,
            format!("[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n"),
        )
        .expect("config");
        let dir = home.home().join("stub");
        let binary = stub::write_stub(&dir);
        let env_file = home.home().join("tokens.env");
        fs::write(&env_file, "GATEWAY_TEST_SECRET=from-env-file\n").expect("env file");
        let server = stub::HealthServer::start(&dir, port, version);
        let rig = Self {
            server,
            handle: new_handle(),
            lines,
            _capture: capture,
            dir,
            etc,
            config,
            binary,
            env_file,
            port,
            home,
        };
        rig.save(|_| {});
        rig
    }

    fn save(&self, edit: impl FnOnce(&mut GatewayRecord)) {
        let mut record = GatewayRecord::new(self.config.clone()).expect("adoptable");
        record.binary = Some(self.binary.clone());
        record.env_file = Some(self.env_file.clone());
        edit(&mut record);
        GatewayRecord::update(|slot| {
            *slot = Some(record);
            Ok(())
        })
        .expect("save the record");
    }

    /// Rewrite the `shunt` stub to also answer `check`, keeping the run path
    /// [`stub::write_stub`] wrote and the fifo it made: a `run` behaves
    /// exactly as before, while a `check` records one line per run in
    /// `dir/checks` and exits by marker — `check-fail` exits 1 with one
    /// stderr line, `check-slow` sleeps 60 s (for a timeout), else it exits 0.
    fn arm_check_stub(&self) {
        let script = format!(
            "#!/bin/sh\n\
             d='{d}'\n\
             if [ \"$1\" = \"check\" ]; then\n\
             \x20 echo check >> \"$d/checks\"\n\
             \x20 if [ -e \"$d/check-slow\" ]; then echo $$ > \"$d/check-slow-pid\"; exec sleep 60; fi\n\
             \x20 if [ -e \"$d/check-fail\" ]; then echo 'config error: boom' >&2; exit 1; fi\n\
             \x20 exit 0\n\
             fi\n\
             if [ -e \"$d/ignore-term\" ]; then\n\
             \x20 trap 'echo \"term $$\" >> \"$d/signals\"' TERM\n\
             else\n\
             \x20 trap 'echo \"term $$\" >> \"$d/signals\"; trap - TERM; kill -s TERM $$' TERM\n\
             fi\n\
             {{\n\
             \x20 echo \"pid $$\"\n\
             \x20 echo \"bin $0\"\n\
             \x20 for a in \"$@\"; do echo \"arg $a\"; done\n\
             \x20 echo \"cwd $(pwd -P)\"\n\
             \x20 env | grep -e '^SHUNT_' -e '^CODEX_AUTH_FILE=' -e '^CLAUDE_CREDENTIALS=' | LC_ALL=C sort | sed 's/^/env /'\n\
             \x20 echo \"secret ${{GATEWAY_TEST_SECRET-}}\"\n\
             \x20 echo end\n\
             }} >> \"$d/calls\"\n\
             echo \"gateway stub stdout $$\"\n\
             echo \"gateway stub stderr $$\" >&2\n\
             if [ ! -e \"$d/no-health\" ]; then exec 3>\"$d/fifo\"; fi\n\
             i=0\n\
             while [ \"$i\" -lt 120 ]; do\n\
             \x20 sleep 1 3>&- &\n\
             \x20 wait $!\n\
             \x20 i=$((i + 1))\n\
             done\n",
            d = self.dir.display(),
        );
        fs::write(&self.binary, script).expect("arm the check stub");
    }

    /// The number of `check` runs the armed stub recorded.
    fn check_runs(&self) -> usize {
        fs::read_to_string(self.dir.join("checks"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .count()
    }

    fn supervisor(&self) -> Supervised {
        Supervised(Supervisor::new(Arc::clone(&self.handle)))
    }

    fn slot(&self) -> GatewaySlot {
        published(&self.handle).expect("the supervisor publishes a slot")
    }

    /// Every run the stub recorded, read once the record lists each spawn the
    /// supervisor logged on this thread: a stub writes its record after
    /// `spawn` returned, so a read right after a step would miss it.
    fn calls(&self) -> Vec<stub::Invocation> {
        let spawned = self
            .lines
            .snapshot()
            .iter()
            .filter(|line| line.starts_with("tollgate daemon: started the shunt gateway (pid "))
            .count();
        let mut calls = Vec::new();
        stub::wait_until(
            &format!("the stub records the {spawned} runs the supervisor spawned"),
            secs(10),
            || {
                calls = stub::invocations(&self.dir);
                calls.len() >= spawned
            },
        );
        calls
    }

    /// The one run so far, asserted to be the only one.
    fn only_call(&self) -> stub::Invocation {
        let calls = self.calls();
        assert_eq!(calls.len(), 1, "exactly one gateway run: {calls:?}");
        calls.into_iter().next().expect("one run")
    }

    /// `state` naming this rig's config, stub and port.
    fn described(&self, state: GatewayState) -> GatewaySlot {
        GatewaySlot {
            config: shown(&self.config),
            binary: shown(&self.binary),
            port: Some(self.port),
            ..blank(state)
        }
    }

    fn touch(&self, name: &str) {
        fs::write(self.dir.join(name), "").expect("stub switch");
    }
}

/// A supervisor whose gateway is killed and reaped when the test ends, red
/// or green.
#[cfg(unix)]
struct Supervised(Supervisor);

#[cfg(unix)]
impl std::ops::Deref for Supervised {
    type Target = Supervisor;
    fn deref(&self) -> &Supervisor {
        &self.0
    }
}

#[cfg(unix)]
impl std::ops::DerefMut for Supervised {
    fn deref_mut(&mut self) -> &mut Supervisor {
        &mut self.0
    }
}

#[cfg(unix)]
impl Drop for Supervised {
    fn drop(&mut self) {
        self.0.kill_for_test();
    }
}

/// A process the test owns outright, killed and reaped at the end.
#[cfg(unix)]
struct Owned(Child);

#[cfg(unix)]
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn t0() -> Tick {
    Tick {
        at: Instant::now(),
        wall_ms: T0_WALL_MS,
    }
}

#[cfg(unix)]
fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Step at `tick` until the slot reaches `state`: an exit or a probe landing
/// between two steps is waited for, never slept for.
#[cfg(unix)]
fn step_until(rig: &Rig, supervisor: &mut Supervised, tick: Tick, state: GatewayState) {
    stub::wait_until(&format!("the slot reads {state:?}"), secs(10), || {
        supervisor.step(tick);
        rig.slot().state == state
    });
}

#[cfg(unix)]
fn store_env(home: &std::path::Path) -> Vec<String> {
    let root = home.join(".tollgate").join("shunt");
    let accounts = root.join("accounts");
    vec![
        format!(
            "CLAUDE_CREDENTIALS={}",
            root.join("claude-credentials.json").display()
        ),
        format!("CODEX_AUTH_FILE={}", root.join("codex-auth.json").display()),
        format!(
            "SHUNT_ANTIGRAVITY_ACCOUNTS_DIR={}",
            accounts.join("antigravity").display()
        ),
        format!(
            "SHUNT_ANTIGRAVITY_AUTH_FILE={}",
            root.join("antigravity-auth.json").display()
        ),
        format!(
            "SHUNT_CLAUDE_ACCOUNTS_DIR={}",
            accounts.join("claude").display()
        ),
        format!(
            "SHUNT_CODEX_ACCOUNTS_DIR={}",
            accounts.join("codex").display()
        ),
        format!(
            "SHUNT_CURSOR_AUTH_FILE={}",
            root.join("cursor-auth.json").display()
        ),
        format!(
            "SHUNT_KIMI_ACCOUNTS_DIR={}",
            accounts.join("kimi").display()
        ),
        format!(
            "SHUNT_XAI_AUTH_FILE={}",
            root.join("xai-auth.json").display()
        ),
    ]
}

#[cfg(unix)]
#[test]
fn a_first_step_spawns_one_gateway_with_every_store_under_tollgate_and_reads_its_version() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    let calls = rig.calls();
    assert_eq!(calls.len(), 1, "exactly one gateway: {calls:?}");
    let call = &calls[0];
    assert_eq!(
        call.args,
        ["run", "--config", rig.config.to_str().expect("utf-8")]
    );
    assert_eq!(call.cwd, rig.etc, "the adopted config's own dir");
    assert_eq!(call.store_env(), store_env(rig.home.home()));
    assert_eq!(
        call.secret, "from-env-file",
        "the env file reaches the gateway"
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(call.pid),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Starting)
        }
    );
    let marker = read_marker().expect("read").expect("a child marker");
    assert_eq!(
        (marker.pid, marker.stop_bound_secs, marker.stop_deadline_ms),
        (call.pid, 12, None),
        "the drain's 2 s, shunt's 5 s blocking grace and a 5 s margin"
    );
    assert!(
        marker.start.is_some(),
        "the start time rides beside the pid"
    );
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(tollgate_dir().expect("dir").join("gateway-child.json"))
            .expect("marker")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the marker is owner-only");
    }

    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(call.pid),
            version: Some("0.49.1".to_string()),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::Healthy)
        }
    );
    assert_eq!(rig.calls().len(), 1, "a healthy gateway is never respawned");
}

#[cfg(unix)]
#[test]
fn a_killed_gateway_restarts_after_its_backoff() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);

    let first = rig.only_call().pid;
    stub::signal(first, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(1)),
        GatewayState::Restarting,
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            restarts: 1,
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::Restarting)
        }
    );
    rig.server.wait_serving(false);

    supervisor.step(t0.after(Duration::from_millis(1999)));
    assert_eq!(rig.calls().len(), 1, "the 1 s backoff has not run out");
    supervisor.step(t0.after(secs(2)));
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned once the backoff ran out");
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(calls[1].pid),
            restarts: 1,
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Starting)
        }
    );
}

#[cfg(unix)]
#[test]
fn a_shunt_already_answering_on_the_port_blocks_the_spawn_until_it_leaves() {
    let rig = Rig::new("0.49.1");
    let foreign = stub::HttpAnswer::bind(rig.port, 200, r#"{"status":"ok","version":"0.49.1"}"#);
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.49.1".to_string()),
            answerer: Some(Answerer::Shunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    assert_eq!(rig.calls().len(), 0, "nothing spawned beside it");
    supervisor.step(t0.after(Duration::from_millis(4999)));
    assert_eq!(
        foreign.hits(),
        1,
        "re-probed on the 5 s cadence, not every step"
    );

    drop(foreign);
    supervisor.step(t0.after(secs(5)));
    assert_eq!(rig.calls().len(), 1, "foreign is never terminal");
    assert_eq!(rig.slot().state, GatewayState::Starting);
}

#[cfg(unix)]
#[test]
fn something_else_answering_on_the_port_reads_foreign_and_not_shunt() {
    let rig = Rig::new("0.49.1");
    let _foreign = stub::HttpAnswer::bind(rig.port, 404, "not found");
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            answerer: Some(Answerer::NotShunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    assert_eq!(rig.calls().len(), 0);
}

#[cfg(unix)]
#[test]
fn a_gateway_below_the_floor_is_stopped_and_held_until_its_binary_changes() {
    let rig = Rig::new("0.47.0");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let pid = rig.only_call().pid;
    rig.server.wait_serving(true);

    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(1)),
        GatewayState::BelowFloor,
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.47.0".to_string()),
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(15),
            }),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::BelowFloor)
        }
    );
    assert!(
        rig.lines.snapshot().contains(&format!(
            "tollgate daemon: shunt 0.47.0 is older than 0.48.0, the oldest release tollgate supervises; stopping the gateway it started (pid {pid})"
        )),
        "the refusal names both versions: {:?}",
        rig.lines.snapshot()
    );

    supervisor.step(t0.after(secs(600)));
    assert_eq!(rig.calls().len(), 1, "no restart while nothing changed");
    assert_eq!(rig.slot().state, GatewayState::BelowFloor);

    crate::testutil::set_mtime(
        &rig.binary,
        std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    supervisor.step(t0.after(secs(601)));
    assert_eq!(rig.calls().len(), 2, "a changed binary gets a fresh start");
}

#[cfg(unix)]
#[test]
fn disabled_or_a_missing_config_runs_nothing_and_says_which() {
    let rig = Rig::new("0.49.1");
    rig.save(|record| record.disabled = true);
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            port: None,
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Disabled)
        }
    );

    rig.save(|_| {});
    fs::remove_file(&rig.config).expect("remove the config");
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            port: None,
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::NoConfig)
        }
    );
    assert_eq!(rig.calls().len(), 0, "neither runs a gateway");
}

#[cfg(unix)]
#[test]
fn turning_disabled_on_stops_the_running_gateway_with_sigterm() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);

    rig.save(|record| record.disabled = true);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(2)),
        GatewayState::Disabled,
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            port: None,
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(15),
            }),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Disabled)
        }
    );
    assert_eq!(
        rig.calls().len(),
        1,
        "a stop tollgate asked for is no crash"
    );
    assert_eq!(
        read_marker().expect("read"),
        None,
        "the marker goes with it"
    );
}

/// A record whose spawn inputs change under a running gateway restarts it
/// with the normal stop: one SIGTERM to the run on the old binary, one spawn
/// of the new one, and no crash counted. The first stub ignores SIGTERM, so
/// the `terms` pin counts every signal it got and can red on a second one.
#[cfg(unix)]
#[test]
fn a_changed_binary_stops_the_running_gateway_once_and_spawns_the_new_one_once() {
    let rig = Rig::new("0.49.1");
    rig.touch("ignore-term");
    let second = rig.home.home().join("bin").join("shunt");
    fs::create_dir_all(second.parent().expect("bin dir")).expect("bin dir");
    fs::copy(&rig.binary, &second).expect("the second stub");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let first = rig.only_call();

    rig.save(|record| record.binary = Some(second.clone()));
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(first.pid),
            version: Some("0.49.1".to_string()),
            since: Some(AT_T2.to_string()),
            ..rig.described(GatewayState::Stopping)
        }
    );

    // The first stub ignores SIGTERM (records it and lives on), so the
    // supervisor kills it at its 12 s stop bound, then respawns the new one.
    // The virtual clock reaches that bound microseconds after the SIGTERM, so
    // the kill waits for the stub's own record; the respawn waits for the rig's
    // answerer to see the old stub go, or it probes the port as foreign.
    stub::wait_until("the first stub records its SIGTERM", secs(10), || {
        !stub::terms(&rig.dir).is_empty()
    });
    supervisor.step(t0.after(secs(14)));
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(14)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(
        calls
            .iter()
            .map(|call| call.bin.clone())
            .collect::<Vec<_>>(),
        [rig.binary.clone(), second.clone()],
        "the old binary's run, then the new one's"
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            binary: shown(&second),
            pid: Some(calls[1].pid),
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T14.to_string()),
            ..rig.described(GatewayState::Starting)
        },
        "no backoff: the stop was asked for, and the kill is not a second SIGTERM"
    );
    let stop_line = format!(
        "tollgate daemon: stopping the shunt gateway (pid {}): its config, binary or env file changed",
        first.pid
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == stop_line)
            .count(),
        1,
        "the sender side: one stop line, which never coalesces"
    );
    assert_eq!(
        stub::terms(&rig.dir),
        [first.pid],
        "the receiver side: exactly one SIGTERM, not two"
    );
    assert_eq!(
        rig.calls().len(),
        2,
        "the new binary is spawned exactly once"
    );

    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(15)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            binary: shown(&second),
            pid: Some(calls[1].pid),
            version: Some("0.49.1".to_string()),
            last_exit: Some(ExitReport {
                code: None,
                signal: Some(9),
            }),
            since: Some(AT_T15.to_string()),
            ..rig.described(GatewayState::Healthy)
        }
    );
}

/// The stop bound is the drain shunt runs with plus its 5 s blocking grace and
/// a 5 s margin; the env file's 3 s drain outranks the config's 2 s, so the
/// kill lands 13 s after the SIGTERM a stub that ignores it got.
#[cfg(unix)]
#[test]
fn a_gateway_ignoring_sigterm_is_killed_at_its_drain_bound() {
    let rig = Rig::new("0.49.1");
    fs::write(
        &rig.env_file,
        "GATEWAY_TEST_SECRET=from-env-file\nSHUNT_SERVER__SHUTDOWN_TIMEOUT_SECONDS=3\n",
    )
    .expect("env file");
    rig.touch("ignore-term");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    // Its record is written once its SIGTERM trap is set.
    let pid = rig.only_call().pid;

    rig.save(|record| record.disabled = true);
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    // Counted on the supervisor's own kill line: `kill -s 0` still answers
    // for a killed child nobody has reaped yet.
    let kill = format!(
        "tollgate daemon: the shunt gateway (pid {pid}) did not exit within 13s of SIGTERM; killing it"
    );
    let kills = || rig.lines.snapshot().iter().filter(|l| **l == kill).count();
    supervisor.step(t0.after(Duration::from_millis(14_999)));
    assert_eq!(kills(), 0, "SIGTERM ignored, the bound not yet run out");
    assert_eq!(rig.slot().state, GatewayState::Stopping);

    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(15)),
        GatewayState::Disabled,
    );
    assert_eq!(kills(), 1, "killed once, at the bound");
    assert_eq!(
        rig.slot().last_exit,
        Some(ExitReport {
            code: None,
            signal: Some(9),
        })
    );
}

#[cfg(unix)]
#[test]
fn a_gateway_that_never_answers_reads_unhealthy_and_keeps_running() {
    let rig = Rig::new("0.49.1");
    rig.touch("no-health");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let pid = rig.only_call().pid;

    supervisor.step(t0.after(Duration::from_millis(9999)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Starting,
        "inside the 10 s grace"
    );
    supervisor.step(t0.after(secs(10)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(pid),
            since: Some(AT_T10.to_string()),
            ..rig.described(GatewayState::Unhealthy)
        }
    );
    supervisor.step(t0.after(secs(120)));
    // A second step reaps a child the first one killed, which a `kill -s 0`
    // probe cannot tell from a live one.
    supervisor.step(t0.after(secs(120)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Unhealthy,
        "never killed for failing its probes"
    );
    assert!(stub::alive(pid), "never killed for failing its probes");
    assert_eq!(rig.calls().len(), 1);
}

#[cfg(unix)]
#[test]
fn a_missing_binary_is_named_and_picked_up_once_it_exists() {
    let rig = Rig::new("0.49.1");
    let missing = rig.home.home().join("bin").join("shunt");
    rig.save(|record| record.binary = Some(missing.clone()));
    let mut supervisor = rig.supervisor();
    let t0 = t0();

    supervisor.step(t0);
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            binary: shown(&missing),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::BinaryMissing)
        }
    );

    fs::create_dir_all(missing.parent().expect("bin dir")).expect("bin dir");
    fs::copy(&rig.binary, &missing).expect("install the stub");
    supervisor.step(t0.after(Duration::from_millis(4999)));
    assert_eq!(rig.calls().len(), 0, "looked at again on the 5 s cadence");
    supervisor.step(t0.after(secs(5)));
    assert_eq!(rig.calls().len(), 1);
    assert_eq!(rig.slot().since.as_deref(), Some(AT_T5));
}

#[cfg(unix)]
#[test]
fn an_orphan_matching_its_recorded_start_is_stopped_before_a_fresh_spawn() {
    let rig = Rig::new("0.49.1");
    let mut before = rig.supervisor();
    let t0 = t0();
    before.step(t0);
    rig.server.wait_serving(true);
    // The daemon dies hard: its gateway and marker stay behind.
    let mut orphan = Owned(before.abandon().expect("a running gateway"));
    let orphan_pid = orphan.0.id();

    let mut after = rig.supervisor();
    after.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(orphan_pid),
            since: Some(AT_T1.to_string()),
            ..blank(GatewayState::Stopping)
        }
    );
    let status = orphan.0.wait().expect("reap the orphan");
    assert_eq!(
        ExitReport::from(status),
        ExitReport {
            code: None,
            signal: Some(15),
        },
        "SIGTERM, shunt's graceful stop"
    );
    rig.server.wait_serving(false);

    after.step(t0.after(secs(2)));
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "a fresh gateway once the orphan is gone");
    assert_eq!(rig.slot().pid, Some(calls[1].pid));
}

#[cfg(unix)]
#[test]
fn a_recorded_pid_whose_start_differs_is_never_signalled() {
    let rig = Rig::new("0.49.1");
    let mut stranger = Owned(
        Command::new(&rig.binary)
            .args(["run", "--config", "stranger.toml"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the stranger"),
    );
    rig.server.wait_serving(true);
    write_marker(&ChildMarker {
        pid: stranger.0.id(),
        // No process mints this: a real start time is a tick count.
        start: Some("never-a-start-time".to_string()),
        stop_bound_secs: 12,
        stop_deadline_ms: None,
    })
    .expect("marker");
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        stranger.0.try_wait().expect("try_wait"),
        None,
        "a pid with another start time is someone else's"
    );
    assert_eq!(
        read_marker().expect("read"),
        None,
        "the stale marker is dropped"
    );
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.49.1".to_string()),
            answerer: Some(Answerer::Shunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 1, "only the stranger's own run: {calls:?}");
    assert_eq!(calls[0].args, ["run", "--config", "stranger.toml"]);
}

/// A daemon exiting on a signal SIGTERMs its gateway and leaves the deadline
/// in the marker when the gateway outlives its budget. The next daemon never
/// sends a second SIGTERM (shunt reads one as "skip the drain"): it waits the
/// recorded deadline out, then kills.
#[cfg(unix)]
#[test]
fn a_stop_left_running_by_an_exiting_daemon_is_finished_at_its_recorded_deadline() {
    let rig = Rig::new("0.49.1");
    rig.touch("ignore-term");
    let mut before = rig.supervisor();
    before.step(t0());
    // Its record is written once its SIGTERM trap is set.
    rig.only_call();
    before.shutdown(Instant::now());
    let recorded = read_marker().expect("read").expect("a marker");
    let deadline_ms = recorded.stop_deadline_ms.expect("the stop's deadline");
    let mut orphan = Owned(before.abandon().expect("still running"));

    let mut after = rig.supervisor();
    let early = Tick {
        at: Instant::now(),
        wall_ms: deadline_ms - 1,
    };
    after.step(early);
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    assert_eq!(
        read_marker().expect("read"),
        Some(recorded),
        "the recorded stop stands as written"
    );
    assert_eq!(orphan.0.try_wait().expect("try_wait"), None);

    after.step(Tick {
        at: Instant::now(),
        wall_ms: deadline_ms,
    });
    let mut status = None;
    stub::wait_until("the orphan exits at its deadline", secs(10), || {
        status = orphan.0.try_wait().expect("try_wait");
        status.is_some()
    });
    assert_eq!(
        ExitReport::from(status.expect("exited")),
        ExitReport {
            code: None,
            signal: Some(9),
        }
    );
    assert_eq!(
        stub::terms(&rig.dir),
        [orphan.0.id()],
        "the exiting daemon's one SIGTERM, never a second"
    );
    rig.server.wait_serving(false);
    after.step(Tick {
        at: Instant::now(),
        wall_ms: deadline_ms,
    });
    assert_eq!(rig.calls().len(), 2, "a fresh gateway once it is gone");
}

#[cfg(unix)]
#[test]
fn the_supervisor_thread_stops_its_gateway_on_shutdown_and_joins() {
    let rig = Rig::new("0.49.1");
    let dir = tollgate_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes a healthy gateway", secs(15), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Healthy)
    });
    let pid = rig.only_call().pid;

    assert!(thread.shutdown(DAEMON_STOP_BUDGET), "answered and joined");
    assert!(!stub::alive(pid), "the gateway is gone");
    assert_eq!(read_marker().expect("read"), None);
    rig.server.wait_serving(false);
}

/// CX-1 for the slot: neither an env-file value nor the admin token reaches
/// it, in a running state or in the refusal a bad env file gets.
#[cfg(unix)]
#[test]
fn the_slot_never_carries_an_env_value_or_the_admin_token() {
    let env_canary = "tollgate-canary-gateway-env-7f3a";
    let line_canary = "tollgate-canary-gateway-line-7f3a";
    let rig = Rig::new("0.49.1");
    let token = crate::gateway::ensure_admin_token().expect("token");
    fs::write(&rig.env_file, format!("GATEWAY_TEST_SECRET={env_canary}\n")).expect("env file");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    assert_eq!(
        rig.only_call().secret,
        env_canary,
        "the canary did reach the gateway"
    );

    let live = crate::daemon::LiveStores {
        gateway: Arc::clone(&rig.handle),
        ..Default::default()
    };
    let snapshot = live.snapshot();
    let config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    let body = serde_json::to_string(&crate::daemon::build_status(
        &config,
        300_000,
        Some(&snapshot.signals()),
        false,
    ))
    .expect("body");
    assert!(body.contains(r#""state":"healthy""#), "{body}");
    for canary in [env_canary, token.expose()] {
        assert!(!body.contains(canary), "{canary} leaked: {body}");
    }

    supervisor.kill_for_test();
    rig.server.wait_serving(false);
    fs::write(
        &rig.env_file,
        format!("GATEWAY_TEST_SECRET={env_canary}\nLINE={line_canary}\0\n"),
    )
    .expect("env file");
    let mut refused = rig.supervisor();
    refused.step(t0.after(secs(2)));
    let slot = rig.slot();
    assert_eq!(
        slot.reason,
        Some(format!(
            "in env file {}: line 2 holds a NUL byte; systemd refuses such a file whole, and so does tollgate: remove the byte",
            rig.env_file.display()
        ))
    );
    let bytes = serde_json::to_string(&slot).expect("slot");
    for canary in [env_canary, line_canary, token.expose()] {
        assert!(!bytes.contains(canary), "{canary} leaked: {bytes}");
    }
}

/// A shutdown preempts a probe: while the supervisor's child probe blocks on
/// the seam, the shutdown reaches the running gateway as its one SIGTERM and
/// answers inside `DAEMON_STOP_BUDGET`.
#[cfg(unix)]
#[test]
fn a_shutdown_preempts_a_probe_and_stops_the_gateway_inside_the_budget() {
    let rig = Rig::new("0.49.1");
    let dir = tollgate_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes a healthy gateway", secs(15), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Healthy)
    });
    let pid = rig.only_call().pid;

    // Hold the next child probe (they run every 5 s) on the seam, then ask
    // for a shutdown: it must preempt the probe, not wait it out.
    let seam = probe_seam::arm();
    seam.wait_entered();
    assert!(
        thread.shutdown(DAEMON_STOP_BUDGET),
        "answered and joined inside the budget"
    );
    seam.release();
    assert!(!stub::alive(pid), "the gateway is gone");
    assert_eq!(stub::terms(&rig.dir), [pid], "exactly one SIGTERM");
    assert_eq!(read_marker().expect("read"), None);
    rig.server.wait_serving(false);
}

/// A shutdown landing while a stop is already underway sends no second
/// SIGTERM: shunt reads a second signal as "skip the drain".
#[cfg(unix)]
#[test]
fn a_shutdown_during_an_underway_stop_sends_no_second_sigterm() {
    let rig = Rig::new("0.49.1");
    rig.touch("ignore-term");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let pid = rig.only_call().pid;

    rig.save(|record| record.disabled = true);
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    // The stub's trap writes its record a moment after the SIGTERM lands.
    stub::wait_until("the stub records its SIGTERM", secs(10), || {
        !stub::terms(&rig.dir).is_empty()
    });

    supervisor.shutdown(Instant::now());
    assert_eq!(
        stub::terms(&rig.dir),
        [pid],
        "the disable's one SIGTERM, never a second from the shutdown"
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| line.starts_with("tollgate daemon: stopping the shunt gateway (pid "))
            .count(),
        1,
        "one stop line, not two"
    );
}

/// A listener that takes the connection and never answers reads `foreign`
/// with `no_answer`, never a spawn beside a wedged answerer.
#[cfg(unix)]
#[test]
fn a_wedged_listener_on_the_port_reads_foreign_and_no_answer() {
    let rig = Rig::new("0.49.1");
    let _hold = stub::HoldListener::bind(rig.port);
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            answerer: Some(Answerer::NoAnswer),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    assert_eq!(rig.calls().len(), 0, "nothing spawned beside it");
}

/// A foreign answerer's version is any string; it enters `daemon.log` through
/// `{:?}`, so a forged newline cannot write a second log line.
#[cfg(unix)]
#[test]
fn a_foreign_version_with_a_newline_enters_the_log_through_debug() {
    let rig = Rig::new("0.49.1");
    let _foreign = stub::HttpAnswer::bind(
        rig.port,
        200,
        r#"{"status":"ok","version":"0.49.1\nforged"}"#,
    );
    let mut supervisor = rig.supervisor();

    supervisor.step(t0());
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            version: Some("0.49.1\nforged".to_string()),
            answerer: Some(Answerer::Shunt),
            since: Some(AT_T0.to_string()),
            ..rig.described(GatewayState::Foreign)
        }
    );
    let expected = format!(
        "tollgate daemon: port {} already answers /health (shunt {:?}); not starting the managed gateway beside it",
        rig.port, "0.49.1\nforged"
    );
    let snapshot = rig.lines.snapshot();
    let foreign_lines: Vec<&str> = snapshot
        .iter()
        .filter(|line| line.starts_with("tollgate daemon: port "))
        .map(|line| line.as_str())
        .collect();
    assert_eq!(
        foreign_lines,
        vec![expected.as_str()],
        "the version is repr'd, not interpolated raw"
    );
}

/// The env file's skipped lines are named once, by number only, when a spawn
/// runs without them; a repeated set does not log again.
#[cfg(unix)]
#[test]
fn an_env_file_with_skipped_lines_says_which_once() {
    let rig = Rig::new("0.49.1");
    fs::write(
        &rig.env_file,
        "GATEWAY_TEST_SECRET=from-env-file\nexport SKIPPED=1\nNO_EQUALS\n",
    )
    .expect("env file");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let call = rig.only_call();
    assert_eq!(call.secret, "from-env-file", "the good line still loads");
    let line = format!(
        "tollgate daemon: the gateway's env file {} assigns nothing on line(s) 2, 3 (systemd skips such lines too); the gateway runs without them",
        rig.env_file.display()
    );
    assert_eq!(
        rig.lines.snapshot().iter().filter(|l| **l == line).count(),
        1,
        "the skipped lines are named once, by number: {:?}",
        rig.lines.snapshot()
    );

    // A second spawn with the same skipped set does not log again.
    stub::signal(call.pid, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(1)),
        GatewayState::Restarting,
    );
    rig.server.wait_serving(false);
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.calls().len(), 2, "respawned");
    assert_eq!(
        rig.lines.snapshot().iter().filter(|l| **l == line).count(),
        1,
        "the same skipped set is not logged twice"
    );
}

/// The restart backoff doubles (1 s, 2 s, 4 s), caps at a minute, and resets
/// to 1 s after a healthy minute.
#[cfg(unix)]
#[test]
fn the_backoff_doubles_caps_at_a_minute_and_resets_after_a_healthy_minute() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);

    let crash = |rig: &Rig, supervisor: &mut Supervised, tick: Tick| {
        let pid = rig.calls().last().expect("a run").pid;
        stub::signal(pid, "KILL");
        step_until(rig, supervisor, tick, GatewayState::Restarting);
        rig.server.wait_serving(false);
    };

    // crash 0 -> 1 s
    let mut tick = t0.after(secs(1));
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(999)));
    assert_eq!(rig.calls().len(), 1, "no spawn inside the 1 s backoff");
    tick = t0.after(secs(2));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 2, "crash 0 respawns after 1 s");

    // crash 1 -> 2 s
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(1_999)));
    assert_eq!(rig.calls().len(), 2, "no spawn inside the 2 s backoff");
    tick = t0.after(secs(4));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 3, "crash 1 respawns after 2 s");

    // crash 2 -> 4 s
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(3_999)));
    assert_eq!(rig.calls().len(), 3, "no spawn inside the 4 s backoff");
    tick = t0.after(secs(8));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 4, "crash 2 respawns after 4 s");

    // crash 3 -> 8 s, crash 4 -> 16 s, crash 5 -> 32 s
    crash(&rig, &mut supervisor, tick);
    tick = t0.after(secs(16));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 5, "crash 3 respawns after 8 s");

    crash(&rig, &mut supervisor, tick);
    tick = t0.after(secs(32));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 6, "crash 4 respawns after 16 s");

    crash(&rig, &mut supervisor, tick);
    tick = t0.after(secs(64));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 7, "crash 5 respawns after 32 s");

    // crash 6 -> the 60 s cap (the doubling's 64 s is capped)
    crash(&rig, &mut supervisor, tick);
    supervisor.step(tick.after(Duration::from_millis(59_999)));
    assert_eq!(rig.calls().len(), 7, "no spawn inside the 60 s cap");
    tick = t0.after(secs(124));
    supervisor.step(tick);
    assert_eq!(rig.calls().len(), 8, "crash 6 respawns at the 60 s cap");

    // A minute healthy resets it: the next crash waits 1 s again.
    rig.server.wait_serving(true);
    let healthy = tick.after(secs(1));
    supervisor.step(healthy);
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let crash_tick = healthy.after(secs(61));
    crash(&rig, &mut supervisor, crash_tick);
    supervisor.step(crash_tick.after(secs(1)));
    assert_eq!(
        rig.calls().len(),
        9,
        "a healthy minute resets the backoff to 1 s"
    );
}

/// An unreadable or YAML record keeps a running gateway: it was started under
/// a record that read, and that record's intent is the last one tollgate knows.
#[cfg(unix)]
#[test]
fn an_unreadable_record_keeps_a_healthy_gateway_running() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let path = crate::gateway::record_path().expect("record path");
    fs::write(&path, "config = \"/etc/shunt/shunt.yaml\"\n").expect("hand-edit");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot(),
        GatewaySlot {
            pid: Some(pid),
            version: Some("0.49.1".to_string()),
            since: Some(AT_T1.to_string()),
            ..rig.described(GatewayState::Healthy)
        },
        "an unreadable record keeps the running gateway"
    );
    assert!(stub::alive(pid), "never stopped for a bad record");
}

/// The production `run` loop, on a real thread, probes a `starting` gateway at
/// most once per [`SUPERVISE_POLL`]: a stub that never answers is re-probed on
/// the 1 s cadence, never in a spin through its startup grace. Counts probes
/// over a fixed window, a bound the round's pacing meets and a spin breaks by
/// orders of magnitude.
#[cfg(unix)]
#[test]
fn the_run_loop_probes_a_starting_gateway_at_most_once_a_poll() {
    let rig = Rig::new("0.49.1");
    rig.touch("no-health");
    let dir = tollgate_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    // `start` runs the production loop; the handle's `Drop` sends the
    // shutdown and joins the thread on every path, a red one included, so no
    // supervisor outlives this test's sandbox.
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");

    // The child is spawned and the loop is probing it; count probes over a
    // fixed window from here.
    stub::wait_until("the stub records its run", secs(10), || {
        !stub::invocations(&rig.dir).is_empty()
    });
    probe_seam::reset_probes();
    std::thread::sleep(Duration::from_secs(5));
    let probes = probe_seam::probes();
    assert!(thread.shutdown(DAEMON_STOP_BUDGET), "answered and joined");

    // The counted window is the 5 s sleep, overrun only by the sleep's own
    // wake-up lag. One probe per SUPERVISE_POLL fits at most six in it (one
    // at each end); `< 12` leaves that much again for probe helper threads
    // started late on a loaded box, while a spin counts thousands.
    assert!(
        probes < 12,
        "a starting gateway is probed once a poll, not a spin: {probes} probes in 5 s"
    );
}

/// The skipped-lines memo is keyed on the env file's path and cleared by a
/// spawn that skips nothing: an R14 swap to another env file whose skipped
/// numbers equal the last logged set names the new path, and a fixed file
/// that later skips the same lines logs them again.
#[cfg(unix)]
#[test]
fn a_skipped_lines_memo_names_the_env_file_and_clears_on_a_clean_spawn() {
    let skipped = "GATEWAY_TEST_SECRET=from-env-file\nexport SKIPPED=1\nNO_EQUALS\n";
    let rig = Rig::new("0.49.1");
    fs::write(&rig.env_file, skipped).expect("env file");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.only_call();
    let first_line = format!(
        "tollgate daemon: the gateway's env file {} assigns nothing on line(s) 2, 3 (systemd skips such lines too); the gateway runs without them",
        rig.env_file.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == first_line)
            .count(),
        1,
        "the skipped lines are named once: {:?}",
        rig.lines.snapshot()
    );

    // R14: another env file with the same skipped numbers logs again, naming
    // the new path.
    let other = rig.home.home().join("other.env");
    fs::write(&other, skipped).expect("other env file");
    rig.save(|record| record.env_file = Some(other.clone()));
    supervisor.step(t0.after(secs(2)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(3)),
        GatewayState::Starting,
    );
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned on the new env file: {calls:?}");
    let second_line = format!(
        "tollgate daemon: the gateway's env file {} assigns nothing on line(s) 2, 3 (systemd skips such lines too); the gateway runs without them",
        other.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == second_line)
            .count(),
        1,
        "the new path's skip is logged once: {:?}",
        rig.lines.snapshot()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == first_line)
            .count(),
        1,
        "the first path's skip stays logged once"
    );

    // A clean spawn (nothing skipped) clears the memo, so a later skip of the
    // same lines logs again.
    fs::write(&other, "GATEWAY_TEST_SECRET=from-env-file\n").expect("clean env file");
    stub::signal(calls[1].pid, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(4)),
        GatewayState::Restarting,
    );
    rig.server.wait_serving(false);
    supervisor.step(t0.after(secs(5)));
    assert_eq!(rig.calls().len(), 3, "the clean file's spawn");

    fs::write(&other, skipped).expect("re-skip");
    stub::signal(rig.calls()[2].pid, "KILL");
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(6)),
        GatewayState::Restarting,
    );
    rig.server.wait_serving(false);
    supervisor.step(t0.after(secs(8)));
    assert_eq!(rig.calls().len(), 4, "the re-skip's spawn");
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == second_line)
            .count(),
        2,
        "a clean spawn cleared the memo, so the same skip logs again: {:?}",
        rig.lines.snapshot()
    );

    // A spawn with no env file clears the memo too: the same file back, still
    // skipping the same lines, logs them again.
    rig.save(|record| record.env_file = None);
    supervisor.step(t0.after(secs(9)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(10)),
        GatewayState::Starting,
    );
    assert_eq!(rig.calls().len(), 5, "the no-env-file spawn");
    rig.save(|record| record.env_file = Some(other.clone()));
    supervisor.step(t0.after(secs(11)));
    assert_eq!(rig.slot().state, GatewayState::Stopping);
    rig.server.wait_serving(false);
    step_until(
        &rig,
        &mut supervisor,
        t0.after(secs(12)),
        GatewayState::Starting,
    );
    assert_eq!(rig.calls().len(), 6, "the same env file's spawn");
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|l| **l == second_line)
            .count(),
        3,
        "a spawn with no env file cleared the memo, so the same skip logs again: {:?}",
        rig.lines.snapshot()
    );
}

/// The stub teardown signals only a pid still naming its own stub: a recorded
/// pid that names another process of the test's own making is never signalled.
#[cfg(unix)]
#[test]
fn the_stub_teardown_never_signals_a_pid_that_no_longer_names_the_stub() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    let stub_pid = rig.only_call().pid;
    assert!(
        stub::names_stub(stub_pid, &rig.binary),
        "a live stub's command line names it"
    );

    // A process of this test's own making that is not the stub: a sleep.
    let mut sleeper = Owned(
        Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn the sleep"),
    );
    let sleeper_pid = sleeper.0.id();
    assert!(
        !stub::names_stub(sleeper_pid, &rig.binary),
        "`sleep` never names the stub"
    );
    // One whose command line holds the stub's path inside another word: the
    // stub is an argv word, never a substring of the line.
    let lookalike_arg0 = format!("{}.old", rig.binary.display());
    let mut lookalike = Owned(
        Command::new("sleep")
            .arg0(&lookalike_arg0)
            .arg("30")
            .spawn()
            .expect("spawn the lookalike"),
    );
    let lookalike_pid = lookalike.0.id();
    assert!(
        !stub::names_stub(lookalike_pid, &rig.binary),
        "`{lookalike_arg0} 30` never names the stub"
    );
    stub::kill_best_effort(sleeper_pid, &rig.binary);
    stub::kill_best_effort(lookalike_pid, &rig.binary);
    // Give a kill that would land time to land; the foreign pids must survive.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut exited = Vec::new();
    while std::time::Instant::now() < deadline && exited.is_empty() {
        for (name, owned) in [("sleep", &mut sleeper), ("lookalike", &mut lookalike)] {
            if owned.0.try_wait().expect("try_wait").is_some() {
                exited.push(name);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        exited,
        Vec::<&str>::new(),
        "a pid never naming the stub as a word is not signalled"
    );

    // A pid still naming the stub is signalled: the kill closes the stub's
    // pipe, so the health server falls silent (a `kill -s 0` probe would
    // still answer for the zombie, so liveness is not the pin).
    stub::kill_best_effort(stub_pid, &rig.binary);
    rig.server.wait_serving(false);
}

/// A shutdown whose thread never answers within the budget is logged once, by
/// equality: the seam holds the supervisor thread, so `shutdown` misses its
/// budget and names it.
#[cfg(unix)]
#[test]
fn a_shutdown_that_misses_its_budget_is_logged() {
    let rig = Rig::new("0.49.1");
    rig.save(|record| record.disabled = true);
    let dir = tollgate_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes the disabled slot", secs(10), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Disabled)
    });

    let seam = probe_seam::arm();
    assert!(
        !thread.shutdown(DAEMON_STOP_BUDGET),
        "a held supervisor thread misses its budget"
    );
    seam.release();
    let line = format!(
        "tollgate daemon: the shunt gateway did not stop within the {} s signal budget; the next daemon start finishes the stop",
        DAEMON_STOP_BUDGET.as_secs()
    );
    assert_eq!(
        rig.lines.snapshot().iter().filter(|l| **l == line).count(),
        1,
        "the missed budget is logged once: {:?}",
        rig.lines.snapshot()
    );
}

/// A supervisor thread that ends in a panic is logged as a panic, never as a
/// missed budget: the seam panics the supervisor inside its shutdown.
#[cfg(unix)]
#[test]
fn a_supervisor_that_panics_is_logged_as_a_panic() {
    let rig = Rig::new("0.49.1");
    rig.save(|record| record.disabled = true);
    let dir = tollgate_dir().expect("dir");
    let super::super::probe::Claim::Active(singleton) =
        super::super::probe::claim_singleton(&dir, false).expect("claim")
    else {
        panic!("the sandbox holds no daemon");
    };
    let thread = start(Arc::clone(&rig.handle), &singleton).expect("start");
    stub::wait_until("the thread publishes the disabled slot", secs(10), || {
        published(&rig.handle).is_some_and(|slot| slot.state == GatewayState::Disabled)
    });

    let _panic = probe_seam::arm_panic();
    assert!(
        !thread.shutdown(DAEMON_STOP_BUDGET),
        "a panicked supervisor thread answers false"
    );
    assert_eq!(
        rig.lines.snapshot(),
        [
            "tollgate daemon: the shunt gateway supervisor panicked; the next daemon start finishes the stop"
        ],
        "the panic is logged as a panic, and nothing else is"
    );
}

/// Writes `changed` over the rig's config and drives one restart: the child
/// stops (one SIGTERM), a fresh spawn happens, no crash is counted, one check
/// ran, and the stop line names `name` (the changed key paths) by equality.
#[cfg(unix)]
fn expect_restart(
    rig: &Rig,
    supervisor: &mut Supervised,
    t0: Tick,
    first_pid: u32,
    name: &str,
    changed: &str,
) {
    let checks_before = rig.check_runs();
    fs::write(&rig.config, changed).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Stopping,
        "the restart-only edit restarts the child"
    );
    let stop_line = format!(
        "tollgate daemon: stopping the shunt gateway (pid {first_pid}): its config changed {name}"
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == stop_line)
            .count(),
        1,
        "the log names the setting by its key path, never its value"
    );
    // The stub dies on its own SIGTERM: wait for the pipe to fall silent,
    // then step until the reap respawns it (the default-trap restart path,
    // never a kill at the drain bound).
    rig.server.wait_serving(false);
    step_until(rig, supervisor, t0.after(secs(3)), GatewayState::Starting);
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "respawned exactly once: {calls:?}");
    assert_eq!(rig.slot().restarts, 0, "an asked-for respawn is no crash");
    assert_eq!(
        stub::terms(&rig.dir),
        [first_pid],
        "the receiver side: exactly one SIGTERM, not two"
    );
    assert_eq!(
        rig.check_runs(),
        checks_before + 1,
        "the restart is gated on one new check"
    );
}

/// The base `[server]` table every restart-only edit starts from.
#[cfg(unix)]
fn base_config(port: u16) -> String {
    format!("[server]\nbind = \"127.0.0.1:{port}\"\nshutdown_timeout_seconds = 2\n")
}

/// A restart-only case: `(spawn config, changed config)` over a port.
#[cfg(unix)]
type RestartCase = dyn Fn(u16) -> (String, String);

/// `bind = "127.0.0.1:3067"` edited to another host keeps the port, so the
/// same rig port keeps answering `/health` while the bind fact flips.
#[cfg(unix)]
#[test]
fn a_bind_edit_restarts_once_and_names_the_setting() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    expect_restart(&rig, &mut supervisor, t0, pid, "server.bind", &changed);
}

/// Losing `[server.admin]` restarts too: the fact is the key's presence in
/// either direction.
#[cfg(unix)]
#[test]
fn removing_the_admin_table_restarts_once() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    fs::write(&rig.config, with_admin_table(&rig)).expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    expect_restart(
        &rig,
        &mut supervisor,
        t0,
        pid,
        "[server.admin]",
        &base_config(rig.port),
    );
}

/// A model's `display_name` is hot-reloadable: editing it never restarts.
#[cfg(unix)]
#[test]
fn a_model_display_name_edit_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\n[[models]]\nid = \"fable\"\ndisplay_name = \"Fable\"\n",
            rig.port
        ),
    )
    .expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = \"127.0.0.1:{}\"\nshutdown_timeout_seconds = 2\n[[models]]\nid = \"fable\"\ndisplay_name = \"Renamed\"\n",
            rig.port
        ),
    )
    .expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a display_name edit keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a display_name edit");
    assert_eq!(rig.calls().len(), 1, "no respawn");
}

/// A comment added above `bind`, or `bind` moved within `[server]`, compares
/// as the same value: no restart.
#[cfg(unix)]
#[test]
fn a_comment_or_key_move_above_bind_keeps_the_child() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let reordered = format!(
        "# a comment above bind\n[server]\nshutdown_timeout_seconds = 2\nbind = \"127.0.0.1:{}\"\n",
        rig.port
    );
    fs::write(&rig.config, reordered).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a comment and key move keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a comment or key move");
    assert_eq!(rig.calls().len(), 1, "no respawn");
}

/// A refused restart-only edit keeps the child, logs the refusal once, and
/// does not re-run the check while the bytes stay unchanged.
#[cfg(unix)]
#[test]
fn a_refused_bind_edit_keeps_the_child_and_logs_once() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    rig.touch("check-fail");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    fs::write(&rig.config, changed).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "a refused bind edit keeps the child"
    );
    assert!(stub::alive(pid), "never stopped for a refused check");
    assert_eq!(rig.calls().len(), 1, "no respawn on a refused check");
    assert_eq!(rig.check_runs(), 1, "one check for the changed bind");

    let keep_line = format!(
        "tollgate daemon: the shunt gateway's config changed server.bind, but `{} check --config {}` refused it (exit 1); keeping the running gateway until the check passes; run that command to see why",
        rig.binary.display(),
        rig.config.display()
    );
    assert_eq!(
        rig.lines
            .snapshot()
            .iter()
            .filter(|line| **line == keep_line)
            .count(),
        1,
        "the refusal is logged once, naming the setting, never its value"
    );

    supervisor.step(t0.after(secs(3)));
    supervisor.step(t0.after(secs(4)));
    assert_eq!(rig.check_runs(), 1, "an unchanged config is not re-checked");
    assert_eq!(rig.calls().len(), 1, "still no respawn");
    assert!(
        stub::alive(pid),
        "the child stays alive across memoized rounds"
    );
}

/// Every member of the restart-only fact set, in every transition its kind
/// admits. The transitions are generated from the fact table itself: the
/// profiles below are cross-checked against `FACT_SPECS` (length, label,
/// walk, kind) before any transition runs, so a member added, removed,
/// re-pathed, or re-kinded fails this test instead of drifting.
#[cfg(unix)]
#[test]
fn every_restart_only_setting_restarts_once_when_it_changes() {
    let profiles = member_profiles();
    assert_eq!(
        profiles.len(),
        FACT_SPECS.len(),
        "the test profiles drifted from the fact set's length"
    );
    for (profile, spec) in profiles.iter().zip(FACT_SPECS.iter()) {
        assert_eq!(
            profile.path, spec.path,
            "a test profile's label drifted from the fact set: {}",
            spec.path
        );
        assert_eq!(
            profile.keys, spec.keys,
            "a test profile's walk drifted from the fact set: {}",
            spec.path
        );
        assert_eq!(
            kind_name(profile.kind),
            kind_name(spec.kind),
            "a test profile's kind drifted from the fact set: {}",
            spec.path
        );
    }
    for profile in &profiles {
        match profile.kind {
            FactKind::Value => {
                // absent -> present, present -> a different value, and back to
                // absent.
                restart_case(profile, profile.absent, profile.present_a);
                restart_case(
                    profile,
                    profile.present_a,
                    profile.present_b.expect("a Value fact carries present_b"),
                );
                restart_case(profile, profile.present_a, profile.absent);
            }
            FactKind::Present => {
                // absent -> present and back; a content edit inside the present
                // table is a keep, covered by the non-member test.
                restart_case(profile, profile.absent, profile.present_a);
                restart_case(profile, profile.present_a, profile.absent);
            }
            FactKind::NonEmpty => {
                let empty = profile
                    .empty
                    .expect("a NonEmpty fact carries its empty config");
                // absent -> non-empty, [] -> non-empty, and each back.
                restart_case(profile, profile.absent, profile.present_a);
                restart_case(profile, empty, profile.present_a);
                restart_case(profile, profile.present_a, empty);
                restart_case(profile, profile.present_a, profile.absent);
            }
        }
    }
}

/// A restart-only fact's test profile: the fact-table entries it must match,
/// and the config fragments that drive every transition its kind admits.
#[cfg(unix)]
struct MemberProfile {
    /// The log key path, matched against `FactSpec::path`.
    path: &'static str,
    /// The document walk, matched against `FactSpec::keys`.
    keys: &'static [&'static str],
    /// The fact kind, matched against `FactSpec::kind`.
    kind: FactKind,
    /// The config with the member's key absent.
    absent: fn(u16) -> String,
    /// The config with the member present (value A).
    present_a: fn(u16) -> String,
    /// The config with the member present (value B, different): a `Value` fact
    /// only — a `Present` or `NonEmpty` member's content edit is a keep, not a
    /// restart.
    present_b: Option<fn(u16) -> String>,
    /// A `NonEmpty` member's config with the key present but `[]`.
    empty: Option<fn(u16) -> String>,
    /// Env-file line(s) pinning the probe port while the config's `bind` key
    /// is absent, so `server.bind`'s absent <-> present transitions stay
    /// spawnable (the daemon would otherwise probe shunt's default port).
    env_pin: Option<fn(u16) -> String>,
}

/// The fact's kind as a name, so the cross-check compares kinds without a
/// `PartialEq` derive on `FactKind`.
#[cfg(unix)]
fn kind_name(kind: FactKind) -> &'static str {
    match kind {
        FactKind::Value => "Value",
        FactKind::Present => "Present",
        FactKind::NonEmpty => "NonEmpty",
    }
}

/// The test profiles, in the fact set's order.
#[cfg(unix)]
fn member_profiles() -> Vec<MemberProfile> {
    vec![
        MemberProfile {
            path: "server.bind",
            keys: &["server", "bind"],
            kind: FactKind::Value,
            absent: |_| "[server]\nshutdown_timeout_seconds = 2\n".to_string(),
            present_a: base_config,
            present_b: Some(|p| {
                format!("[server]\nbind = \"0.0.0.0:{p}\"\nshutdown_timeout_seconds = 2\n")
            }),
            empty: None,
            env_pin: Some(|p| format!("SHUNT_SERVER__BIND=127.0.0.1:{p}")),
        },
        MemberProfile {
            path: "server.max_concurrent_requests",
            keys: &["server", "max_concurrent_requests"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| format!("{}max_concurrent_requests = 2048\n", base_config(p)),
            present_b: Some(|p| format!("{}max_concurrent_requests = 4096\n", base_config(p))),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.shutdown_timeout_seconds",
            keys: &["server", "shutdown_timeout_seconds"],
            kind: FactKind::Value,
            absent: |p| format!("[server]\nbind = \"127.0.0.1:{p}\"\n"),
            present_a: base_config,
            present_b: Some(|p| {
                format!("[server]\nbind = \"127.0.0.1:{p}\"\nshutdown_timeout_seconds = 3\n")
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.access_control]",
            keys: &["server", "access_control"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.access_control]\nallow_cidrs = [\"10.0.0.0/8\"]\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.access_control]\nallow_cidrs = [\"10.0.0.0/16\"]\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.limits.max_request_header_bytes",
            keys: &["server", "limits", "max_request_header_bytes"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.limits]\nmax_request_header_bytes = 512\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.limits]\nmax_request_header_bytes = 1024\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.limits.max_url_length",
            keys: &["server", "limits", "max_url_length"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| format!("{}[server.limits]\nmax_url_length = 256\n", base_config(p)),
            present_b: Some(|p| {
                format!("{}[server.limits]\nmax_url_length = 512\n", base_config(p))
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.rate_limits]",
            keys: &["server", "rate_limits"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.rate_limits.device_verify]\nmax = 3\nwindow_seconds = 20\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.rate_limits.device_verify]\nmax = 5\nwindow_seconds = 20\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.admin]",
            keys: &["server", "admin"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.admin]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.gateway]",
            keys: &["server", "gateway"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.gateway]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.spend]",
            keys: &["server", "spend"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.spend]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.spend.state_path",
            keys: &["server", "spend", "state_path"],
            kind: FactKind::Value,
            absent: |p| format!("{}[server.spend]\n", base_config(p)),
            present_a: |p| {
                format!(
                    "{}[server.spend]\nstate_path = \"/tmp/spend.json\"\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.spend]\nstate_path = \"/tmp/spend-2.json\"\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.codex_endpoint]",
            keys: &["server", "codex_endpoint"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.codex_endpoint]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.usage]",
            keys: &["server", "usage"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.usage]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.oauth_usage]",
            keys: &["server", "oauth_usage"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.oauth_usage]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[sentry]",
            keys: &["sentry"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[sentry]\ndsn = \"\"\nenvironment = \"dev\"\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[sentry]\ndsn = \"\"\nenvironment = \"prod\"\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[otel]",
            keys: &["otel"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[otel]\nendpoint = \"http://localhost:4318\"\nsample_ratio = 0.5\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[otel]\nendpoint = \"http://localhost:4318\"\nsample_ratio = 1.0\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.pool.usage_refresh_seconds",
            keys: &["server", "pool", "usage_refresh_seconds"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.pool]\nusage_refresh_seconds = 120\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.pool]\nusage_refresh_seconds = 300\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "server.pool.state_path",
            keys: &["server", "pool", "state_path"],
            kind: FactKind::Value,
            absent: base_config,
            present_a: |p| {
                format!(
                    "{}[server.pool]\nstate_path = \"/tmp/pool.json\"\n",
                    base_config(p)
                )
            },
            present_b: Some(|p| {
                format!(
                    "{}[server.pool]\nstate_path = \"/tmp/pool-2.json\"\n",
                    base_config(p)
                )
            }),
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.status]",
            keys: &["server", "status"],
            kind: FactKind::Present,
            absent: base_config,
            present_a: |p| format!("{}[server.status]\n", base_config(p)),
            present_b: None,
            empty: None,
            env_pin: None,
        },
        MemberProfile {
            path: "[server.status].sources",
            keys: &["server", "status", "sources"],
            kind: FactKind::NonEmpty,
            absent: |p| format!("{}[server.status]\n", base_config(p)),
            present_a: |p| {
                format!(
                    "{}[server.status]\n[[server.status.sources]]\nprovider = \"claude\"\nurl = \"https://status.claude.com/api/v2/summary.json\"\n",
                    base_config(p)
                )
            },
            present_b: None,
            empty: Some(|p| format!("{}[server.status]\nsources = []\n", base_config(p))),
            env_pin: None,
        },
        MemberProfile {
            path: "server.status.refresh_seconds",
            keys: &["server", "status", "refresh_seconds"],
            kind: FactKind::Value,
            absent: |p| format!("{}[server.status]\n", base_config(p)),
            present_a: |p| format!("{}[server.status]\nrefresh_seconds = 120\n", base_config(p)),
            present_b: Some(|p| {
                format!("{}[server.status]\nrefresh_seconds = 300\n", base_config(p))
            }),
            empty: None,
            env_pin: None,
        },
    ]
}

/// One restart-only change, driven end to end in a fresh sandbox: spawn on
/// `spawn(p)`, edit to `changed(p)`, and assert exactly one restart gated on
/// one check, the stop line naming the profile's key path by equality.
#[cfg(unix)]
fn restart_case(profile: &MemberProfile, spawn: fn(u16) -> String, changed: fn(u16) -> String) {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    if let Some(pin) = profile.env_pin {
        fs::write(
            &rig.env_file,
            format!("GATEWAY_TEST_SECRET=from-env-file\n{}\n", pin(rig.port)),
        )
        .expect("env pin");
    }
    let spawn_config = spawn(rig.port);
    fs::write(&rig.config, spawn_config).expect("spawn config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "{}: spawns healthy",
        profile.path
    );
    let first = rig.only_call().pid;
    let changed_config = changed(rig.port);
    expect_restart(
        &rig,
        &mut supervisor,
        t0,
        first,
        profile.path,
        &changed_config,
    );
}

/// Representative hot-reloadable non-members: editing them never restarts.
#[cfg(unix)]
#[test]
fn hot_reloadable_settings_never_restart_the_child() {
    let rig = Rig::new("0.49.1");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let base = base_config(rig.port);
    let edits: Vec<(&str, String)> = vec![
        (
            "an [[upstreams]] field",
            format!(
                "{base}[[upstreams]]\nname = \"u\"\nkind = \"anthropic\"\nbase_url = \"https://api.anthropic.com\"\n"
            ),
        ),
        (
            "[[models]]",
            format!("{base}[[models]]\nid = \"fable\"\ndisplay_name = \"Fable\"\n"),
        ),
        (
            "[[routes]]",
            format!("{base}[[routes]]\nmodel = \"fable\"\n"),
        ),
        ("[server.auth]", format!("{base}[server.auth]\n")),
        (
            "[server.pool].hard_threshold",
            format!("{base}[server.pool]\nhard_threshold = 0.9\n"),
        ),
    ];
    for (name, changed) in edits {
        fs::write(&rig.config, changed).expect("config");
        supervisor.step(t0.after(secs(2)));
        assert_eq!(
            rig.slot().state,
            GatewayState::Healthy,
            "{name} keeps the child"
        );
        assert!(stub::alive(pid), "{name} never stops the child");
    }
    assert_eq!(
        rig.calls().len(),
        1,
        "no respawn for any hot-reloadable edit"
    );
}

/// One hot-reloadable edit inside a present restart-only table, driven end to
/// end in a fresh sandbox: spawn on `spawn_config`, edit to `changed_config`,
/// and assert no restart and no check.
#[cfg(unix)]
fn keep_case(name: &str, case: &RestartCase) {
    let rig = Rig::new("0.49.1");
    let (spawn_config, changed_config) = case(rig.port);
    fs::write(&rig.config, spawn_config).expect("spawn config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "{name}: spawns healthy"
    );
    let pid = rig.only_call().pid;

    fs::write(&rig.config, changed_config).expect("config");
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "{name} keeps the child"
    );
    assert!(stub::alive(pid), "{name} never stops the child");
    assert_eq!(rig.calls().len(), 1, "no respawn for {name}");
}

/// A non-member sibling inside a restart-only table hot-reloads: editing it
/// never restarts the child, even while the member table is present.
#[cfg(unix)]
#[test]
fn a_non_member_edit_inside_a_member_table_keeps_the_child() {
    // One keep case per member table: a sibling or content edit that leaves
    // every fact inside it unchanged. `[server.usage]` and `[server.oauth_usage]`
    // have no fields of their own, so an arbitrary key stands in for their
    // content (presence alone is the fact).
    let cases: Vec<(&str, Box<RestartCase>)> = vec![
        (
            "[server] sse_keepalive_seconds beside the bind facts",
            Box::new(|p| {
                (
                    format!("{}sse_keepalive_seconds = 60\n", base_config(p)),
                    format!("{}sse_keepalive_seconds = 61\n", base_config(p)),
                )
            }),
        ),
        (
            "a [[server.admin.write_keys]] edit under a present table",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.admin]\n[[server.admin.write_keys]]\nid = \"tollgate\"\nkey = \"first\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.admin]\n[[server.admin.write_keys]]\nid = \"tollgate\"\nkey = \"second\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.limits] max_request_bytes",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.limits]\nmax_request_bytes = 1000\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.limits]\nmax_request_bytes = 2000\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.spend] blocked_message",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.spend]\nblocked_message = \"nope\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.spend]\nblocked_message = \"denied\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.pool] hard_threshold beside usage_refresh_seconds",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.pool]\nusage_refresh_seconds = 120\nhard_threshold = 0.9\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.pool]\nusage_refresh_seconds = 120\nhard_threshold = 0.95\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.gateway] trust_forwarded_for",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.gateway]\ntrust_forwarded_for = true\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.gateway]\ntrust_forwarded_for = false\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.codex_endpoint] provider",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.codex_endpoint]\nprovider = \"codex\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.codex_endpoint]\nprovider = \"other\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.usage] a field",
            Box::new(|p| {
                (
                    format!("{}[server.usage]\nnote = \"a\"\n", base_config(p)),
                    format!("{}[server.usage]\nnote = \"b\"\n", base_config(p)),
                )
            }),
        ),
        (
            "[server.oauth_usage] a field",
            Box::new(|p| {
                (
                    format!("{}[server.oauth_usage]\nnote = \"a\"\n", base_config(p)),
                    format!("{}[server.oauth_usage]\nnote = \"b\"\n", base_config(p)),
                )
            }),
        ),
        (
            "a [[server.status.sources]] URL edit with the list non-empty",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.status]\n[[server.status.sources]]\nprovider = \"claude\"\nurl = \"https://status.claude.com/api/v2/summary.json\"\n",
                        base_config(p)
                    ),
                    format!(
                        "{}[server.status]\n[[server.status.sources]]\nprovider = \"claude\"\nurl = \"https://status.anthropic.com/api/v2/summary.json\"\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.status] sources absent -> sources = [] (shunt starts no poller either way)",
            Box::new(|p| {
                (
                    format!("{}[server.status]\nrefresh_seconds = 300\n", base_config(p)),
                    format!(
                        "{}[server.status]\nrefresh_seconds = 300\nsources = []\n",
                        base_config(p)
                    ),
                )
            }),
        ),
        (
            "[server.status] sources = [] -> absent",
            Box::new(|p| {
                (
                    format!(
                        "{}[server.status]\nrefresh_seconds = 300\nsources = []\n",
                        base_config(p)
                    ),
                    format!("{}[server.status]\nrefresh_seconds = 300\n", base_config(p)),
                )
            }),
        ),
    ];
    for (name, case) in &cases {
        keep_case(name, case.as_ref());
    }
}

/// Changing two restart-only members at once names both key paths in fact-set
/// order, comma-separated.
#[cfg(unix)]
#[test]
fn a_two_setting_edit_names_both_key_paths_in_order() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n[server.admin]\n",
        rig.port
    );
    expect_restart(
        &rig,
        &mut supervisor,
        t0,
        pid,
        "server.bind, [server.admin]",
        &changed,
    );
}

/// A missing spawn baseline cannot prove that the child loaded the edited
/// config: require a successful check and one respawn before accepting facts.
#[cfg(unix)]
#[test]
fn a_child_whose_spawn_read_failed_checks_and_restarts_on_the_first_readable_round() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    let pid = rig.only_call().pid;
    // Simulate the parent's independent post-spawn read losing an editor race.
    supervisor.child.as_mut().unwrap().spawn_mark = None;
    let changed = format!(
        "[server]\nbind = \"0.0.0.0:{}\"\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    expect_restart(
        &rig,
        &mut supervisor,
        t0,
        pid,
        "unknown spawn-time restart settings",
        &changed,
    );
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(5)));
    assert_eq!(rig.calls().len(), 2, "the new child establishes a baseline");
    assert_eq!(rig.check_runs(), 1);
    assert!(supervisor.child.as_ref().unwrap().spawn_mark.is_some());
}

/// A config whose `[sentry]` subtree holds `nan` compares equal to itself
/// round after round (the mark holds a digest, never the raw value): an
/// unchanged round never restarts.
#[cfg(unix)]
#[test]
fn a_config_holding_nan_in_a_value_fact_does_not_restart_on_an_unchanged_round() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    fs::write(
        &rig.config,
        format!(
            "{}[sentry]\ndsn = \"\"\ntraces_sample_rate = nan\n",
            base_config(rig.port)
        ),
    )
    .expect("config");
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    assert_eq!(rig.slot().state, GatewayState::Healthy);
    let pid = rig.only_call().pid;

    // The config is untouched: a NaN value fact must not read as changed.
    supervisor.step(t0.after(secs(2)));
    assert_eq!(
        rig.slot().state,
        GatewayState::Healthy,
        "an unchanged NaN fact never restarts"
    );
    assert!(
        stub::alive(pid),
        "the child stays alive across unchanged rounds"
    );
    assert_eq!(rig.calls().len(), 1, "no respawn on an unchanged NaN fact");
    assert_eq!(
        rig.check_runs(),
        0,
        "no check runs on an unchanged NaN fact"
    );
}

#[cfg(unix)]
fn with_admin_table(rig: &Rig) -> String {
    format!("{}[server.admin]\n", base_config(rig.port))
}

/// Exercise every restart fact without a loopback listener, independently of
/// the end-to-end stub cases above.
#[cfg(unix)]
#[test]
fn restart_facts_cover_every_transition_and_ignore_hot_edits() {
    let home = HomeSandbox::new();
    let config = home.home().join("shunt.toml");
    let read = |text: String| {
        fs::write(&config, text).unwrap();
        RestartFacts::read(&config).unwrap()
    };
    let profiles = member_profiles();
    assert_eq!(profiles.len(), FACT_SPECS.len());
    for (p, spec) in profiles.iter().zip(FACT_SPECS) {
        assert_eq!(p.path, spec.path);
        assert_eq!(p.keys, spec.keys);
        assert_eq!(kind_name(p.kind), kind_name(spec.kind));
        let absent = read((p.absent)(3067));
        let present = read((p.present_a)(3067));
        assert_eq!(present.changed_since(&absent), [p.path]);
        assert_eq!(absent.changed_since(&present), [p.path]);
        if let Some(other) = p.present_b {
            assert_eq!(read(other(3067)).changed_since(&present), [p.path]);
        }
        if let Some(empty) = p.empty {
            assert_eq!(read(empty(3067)), absent);
        }
    }
    let base = read(
        "[server]\nbind = '127.0.0.1:3067'\nshutdown_timeout_seconds = 2\n[sentry]\nrate = nan\n"
            .into(),
    );
    let hot = read("# reordered and edited hot fields\n[server]\nsse_keepalive_seconds = 60\nshutdown_timeout_seconds = 2\nbind = '127.0.0.1:3067'\n[sentry]\nrate = nan\n[[models]]\nid = 'fable'\n".into());
    assert_eq!(hot, base);
    fs::write(&config, "[server]\nlimits = 'broken'").unwrap();
    assert!(RestartFacts::read(&config).is_err());
    fs::write(&config, "not = [toml").unwrap();
    assert!(RestartFacts::read(&config).is_err());
}

#[cfg(unix)]
#[test]
fn restart_check_memoizes_refusal_rechecks_and_never_logs_stderr() {
    let home = HomeSandbox::new();
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();
    let config = home.home().join("shunt.toml");
    fs::write(&config, "[server]\nbind = '127.0.0.1:3067'\n").unwrap();
    let binary = crate::testutil::write_shim(
        home.home(),
        "shunt",
        r#"
        echo check >> "$(dirname "$0")/checks"
        if [ ! -e "$(dirname "$0")/pass" ]; then echo 'SECRET-ONLY-IN-CHECK-STDERR' >&2; exit 1; fi
    "#,
    );
    let mut record = GatewayRecord::new(config.clone()).unwrap();
    record.binary = Some(binary);
    let change = RestartChange {
        settings: vec!["server.bind"],
    };
    let mut memo = RefusedCheck::default();
    let now = Instant::now();
    let cancel = AtomicBool::new(false);
    assert!(!restart_cleared(&record, &change, &mut memo, now, &cancel));
    assert!(!restart_cleared(
        &record,
        &change,
        &mut memo,
        now + secs(59),
        &cancel
    ));
    assert_eq!(
        fs::read_to_string(home.home().join("checks"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    fs::write(home.home().join("pass"), "").unwrap();
    assert!(restart_cleared(
        &record,
        &change,
        &mut memo,
        now + secs(60),
        &cancel
    ));
    assert_eq!(
        fs::read_to_string(home.home().join("checks"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let logs = lines.snapshot();
    assert_eq!(logs.len(), 1, "one refusal log: {logs:?}");
    assert!(logs[0].contains("config changed server.bind"));
    assert!(!logs[0].contains("SECRET-ONLY-IN-CHECK-STDERR"));
}

#[cfg(unix)]
#[test]
fn restart_check_is_cancelled_before_spawn_and_during_a_wedged_check() {
    let home = HomeSandbox::new();
    let config = home.home().join("shunt.toml");
    fs::write(&config, "[server]\nbind = '127.0.0.1:3067'\n").unwrap();
    let binary = crate::testutil::write_shim(
        home.home(),
        "shunt",
        r#"echo entered > "$(dirname "$0")/entered"; exec sleep 30"#,
    );
    let mut record = GatewayRecord::new(config).unwrap();
    record.binary = Some(binary);
    let cancel = Arc::new(AtomicBool::new(true));
    assert!(crate::gateway::check_adopted_config(&record, &cancel).is_err());
    assert!(!home.home().join("entered").exists());
    cancel.store(false, Ordering::Release);
    let entered = home.home().join("entered");
    let stopper = std::thread::spawn({
        let cancel = Arc::clone(&cancel);
        move || {
            stub::wait_until("our check enters", secs(5), || entered.exists());
            cancel.store(true, Ordering::Release);
        }
    });
    let started = Instant::now();
    assert!(crate::gateway::check_adopted_config(&record, &cancel).is_err());
    stopper.join().unwrap();
    assert!(started.elapsed() < secs(5));
}

#[cfg(unix)]
#[test]
fn a_non_table_edit_after_spawn_logs_once_and_the_repaired_bind_restarts() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    supervisor.step(t0.after(secs(1)));
    let pid = rig.only_call().pid;
    fs::write(
        &rig.config,
        format!(
            "[server]\nbind = '0.0.0.0:{}'\nlimits = 'SECRET-NOT-A-TABLE'\n",
            rig.port
        ),
    )
    .unwrap();
    for seconds in 2..=4 {
        supervisor.step(t0.after(secs(seconds)));
    }
    assert!(stub::alive(pid));
    assert_eq!(rig.calls().len(), 1);
    assert_eq!(rig.check_runs(), 0);
    let refusal = |line: &&String| {
        line.contains("cannot read the shunt gateway's restart settings at server.limits.max_request_header_bytes")
    };
    assert_eq!(rig.lines.snapshot().iter().filter(refusal).count(), 1);
    assert!(
        !rig.lines
            .snapshot()
            .iter()
            .any(|line| line.contains("SECRET-NOT-A-TABLE"))
    );
    let changed = format!(
        "[server]\nbind = '0.0.0.0:{}'\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    expect_restart(
        &rig,
        &mut supervisor,
        t0.after(secs(5)),
        pid,
        "server.bind",
        &changed,
    );
}

#[cfg(unix)]
#[test]
fn a_missing_baseline_never_stops_the_child_until_the_check_passes() {
    let rig = Rig::new("0.49.1");
    rig.arm_check_stub();
    let mut supervisor = rig.supervisor();
    let t0 = t0();
    supervisor.step(t0);
    rig.server.wait_serving(true);
    let pid = rig.only_call().pid;
    supervisor.child.as_mut().unwrap().spawn_mark = None;
    fs::write(rig.dir.join("check-fail"), "").unwrap();
    supervisor.step(t0.after(secs(1)));
    supervisor.step(t0.after(secs(2)));
    assert!(stub::alive(pid));
    assert!(supervisor.child.as_ref().unwrap().spawn_mark.is_none());
    assert_eq!(
        rig.check_runs(),
        1,
        "refusal memo applies to unknown baselines too"
    );
    fs::remove_file(rig.dir.join("check-fail")).unwrap();
    let changed = format!(
        "[server]\nbind = '0.0.0.0:{}'\nshutdown_timeout_seconds = 2\n",
        rig.port
    );
    expect_restart(
        &rig,
        &mut supervisor,
        t0.after(secs(61)),
        pid,
        "unknown spawn-time restart settings",
        &changed,
    );
}
