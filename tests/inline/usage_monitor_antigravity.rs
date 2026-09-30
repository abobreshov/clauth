#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use serde_json::json;
struct Fake {
    owner: bool,
    locked: usize,
    unlocked: usize,
    blob: Vec<u8>,
}
impl KeyringProbe for Fake {
    fn has_owner(&self) -> Result<bool, Failure> {
        Ok(self.owner)
    }
    fn metadata(&self) -> Result<KeyringMetadata, Failure> {
        assert!(self.owner);
        Ok(KeyringMetadata {
            locked: self.locked,
            unlocked: self.unlocked,
        })
    }
    fn secret(&self) -> Result<Zeroizing<Vec<u8>>, Failure> {
        assert_eq!(self.locked, 0);
        assert_eq!(self.unlocked, 1);
        Ok(Zeroizing::new(self.blob.clone()))
    }
}
#[test]
fn agy_no_owner_locked_and_multiple_never_read_secret() {
    for (owner, locked, unlocked) in [(false, 0, 0), (true, 1, 0), (true, 0, 2)] {
        let fake = Fake {
            owner,
            locked,
            unlocked,
            blob: vec![],
        };
        assert!(read_token(&fake, 100).is_err());
    }
}
#[test]
fn agy_blob_accepts_nested_flat_and_base64_prefix() {
    use base64::Engine;
    for body in [
        json!({"token":{"access_token":"ACCESS","expiry":2000000000,"refresh_token":"REFRESH-CANARY"}}),
        json!({"access_token":"ACCESS","expiresAt":2000000000000_i64}),
    ] {
        let bytes = body.to_string().into_bytes();
        assert_eq!(parse_blob(&bytes, 100).unwrap().expose(), "ACCESS");
        let encoded = format!(
            "go-keyring-base64:{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        assert_eq!(
            parse_blob(encoded.as_bytes(), 100).unwrap().expose(),
            "ACCESS"
        );
    }
}
#[test]
fn agy_missing_expiry_and_under_60s_fail_closed() {
    for expiry in [Value::Null, json!(160)] {
        assert_eq!(
            parse_blob(
                json!({"access_token":"ACCESS","expiry":expiry})
                    .to_string()
                    .as_bytes(),
                100
            )
            .unwrap_err()
            .kind,
            FailureKind::AuthRequired
        );
    }
}
#[test]
fn agy_accepts_response_envelope_and_bare_groups() {
    let value = json!({"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0.25,"resetTime":2000000000}]}]});
    for body in [value.clone(), json!({"response":value})] {
        let r = map_summary(&body).unwrap();
        assert_eq!(r.windows[0].used_pct, Some(75.));
        assert_eq!(r.windows[0].window_secs, Some(18000));
        assert!(!r.windows[0].chain_eligible);
    }
}
#[test]
fn agy_no_groups_unavailable() {
    assert_eq!(
        map_summary(&json!({"groups":[]})).unwrap_err().kind,
        FailureKind::Unavailable
    );
}
fn target(home: &std::path::Path) -> MonitorTarget {
    let cfg: super::super::config::MonitorConfig =
        toml::from_str("id='agy'\nkind='antigravity'\n").unwrap();
    super::super::source::resolve_target(&cfg, home, 100, &|_| None)
}
#[test]
fn agy_does_not_fall_through_hosts_on_auth_or_rate_limits() {
    let home = crate::testutil::HomeSandbox::new();
    for code in [401, 403, 429] {
        let mut http = super::super::source::FakeHttp::offline();
        http.send_reply = Box::new(move |_, req| {
            assert!(req.url.contains("daily-cloudcode"));
            Ok(HttpReply {
                retry_after_secs: None,
                status: code,
                body: "{}".into(),
                headers: vec![],
            })
        });
        let fake = Fake {
            owner: true,
            locked: 0,
            unlocked: 1,
            blob: json!({"access_token":"ACCESS","expiry":2000000000})
                .to_string()
                .into_bytes(),
        };
        let failure = fetch_with(&target(home.home()), &http, &fake).unwrap_err();
        assert_eq!(http.calls.lock().unwrap().len(), 1);
        if code == 429 {
            assert_eq!(failure.retry_after.unwrap().secs(), 1000);
        }
    }
}
#[test]
fn agy_tries_production_on_404_and_plan_once_per_day() {
    let home = crate::testutil::HomeSandbox::new();
    let mut http = super::super::source::FakeHttp::offline();
    http.send_reply = Box::new(|_, req| {
        let (status, body) = if req.url.contains("daily-cloudcode") {
            (404, json!({}))
        } else if req.url.ends_with("loadCodeAssist") {
            (200, json!({"response":{"paidTier":{"name":"Pro"}}}))
        } else {
            (
                200,
                json!({"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"x","window":"5h","remainingFraction":1}]}]}),
            )
        };
        Ok(HttpReply {
            retry_after_secs: None,
            status,
            body: body.to_string(),
            headers: vec![],
        })
    });
    let fake = Fake {
        owner: true,
        locked: 0,
        unlocked: 1,
        blob: json!({"access_token":"ACCESS","expiry":2000000000})
            .to_string()
            .into_bytes(),
    };
    let mut target = target(home.home());
    let r = fetch_with(&target, &http, &fake).unwrap();
    assert_eq!(r.plan.as_deref(), Some("Pro"));
    assert_eq!(http.calls.lock().unwrap().len(), 3);
    target.previous = Some(r);
    http.calls.lock().unwrap().clear();
    let r = fetch_with(&target, &http, &fake).unwrap();
    assert_eq!(http.calls.lock().unwrap().len(), 2);
    assert_eq!(r.plan.as_deref(), Some("Pro"));
}
#[test]
fn agy_shape_fixture_maps() {
    let body: Value =
        serde_json::from_str(include_str!("../fixtures/monitors/agy-summary.json")).unwrap();
    assert_eq!(map_summary(&body).unwrap().windows[0].used_pct, Some(75.));
}
#[test]
fn agy_malformed_expiry_does_not_fall_back_to_jwt() {
    use base64::Engine;
    let jwt = format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"exp":2000000000}).to_string())
    );
    assert_eq!(
        parse_blob(
            json!({"access_token":jwt,"expiry":"not-a-date"})
                .to_string()
                .as_bytes(),
            100
        )
        .unwrap_err()
        .kind,
        FailureKind::AuthRequired
    );
}
#[test]
fn agy_blob_over_one_mib_is_refused() {
    assert_eq!(
        parse_blob(&vec![b' '; 1024 * 1024 + 1], 100)
            .unwrap_err()
            .kind,
        FailureKind::Unavailable
    );
}
struct FakeCli {
    version: &'static str,
    help: &'static str,
    usage: &'static str,
    calls: std::sync::Mutex<Vec<CliCall>>,
}
impl CliRunner for FakeCli {
    fn run(&self, call: CliCall) -> Result<Zeroizing<String>, Failure> {
        self.calls.lock().unwrap().push(call);
        Ok(Zeroizing::new(
            match call {
                CliCall::Version => self.version,
                CliCall::Help => self.help,
                CliCall::Usage => self.usage,
            }
            .into(),
        ))
    }
}
fn cli_fixture() -> FakeCli {
    FakeCli {
        version: "agy 1.2.13",
        help: r#"{"commands":["/help","/usage"]}"#,
        usage: include_str!("../fixtures/monitors/agy-summary.json"),
        calls: std::sync::Mutex::new(vec![]),
    }
}
#[test]
fn cli_path_needs_1_1_11() {
    let mut runner = cli_fixture();
    runner.version = "agy 1.1.10";
    let failure = fetch_cli(&runner, 100).unwrap_err();
    assert_eq!(failure.kind, FailureKind::Unavailable);
    assert_eq!(failure.message, "agy 1.1.10 has no print-mode /usage");
    assert_eq!(*runner.calls.lock().unwrap(), vec![CliCall::Version]);
}
#[test]
fn cli_path_needs_usage_in_help() {
    let mut runner = cli_fixture();
    runner.help = r#"{"commands":["/help","/login"]}"#;
    assert!(fetch_cli(&runner, 100).is_err());
    assert_eq!(
        *runner.calls.lock().unwrap(),
        vec![CliCall::Version, CliCall::Help]
    );
}
#[test]
fn cli_sign_in_output_is_auth_required() {
    let mut runner = cli_fixture();
    runner.usage = "Please sign in at https://accounts.google.com";
    assert_eq!(
        fetch_cli(&runner, 100).unwrap_err().kind,
        FailureKind::AuthRequired
    );
}
#[test]
fn cli_fixture_maps_without_live_cli() {
    let runner = cli_fixture();
    let r = fetch_cli(&runner, 100).unwrap();
    assert_eq!(r.windows[0].used_pct, Some(75.));
    assert_eq!(
        *runner.calls.lock().unwrap(),
        vec![CliCall::Version, CliCall::Help, CliCall::Usage]
    );
}
#[test]
fn cli_spawn_has_no_display_or_browser() {
    let home = crate::testutil::HomeSandbox::new();
    let command = cli_command(&home.home().join("fixture-agy"), CliCall::Usage);
    let env: Vec<_> = command.get_envs().collect();
    for name in ["DISPLAY", "WAYLAND_DISPLAY", "BROWSER"] {
        assert!(
            env.iter()
                .any(|(key, value)| *key == name && value.is_none())
        );
    }
    assert_eq!(
        command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        vec!["-p", "/usage", "--output-format", "json"]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let program = home.home().join("fixture-agy");
        std::fs::write(
            &program,
            "#!/bin/sh
readlink /proc/self/fd/0
",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = run_cli_command(
            cli_command(&program, CliCall::Usage),
            CliCall::Usage,
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            output.trim(),
            "/dev/null",
            "child stdin must be the null device"
        );
    }
}
#[cfg(target_os = "linux")]
#[test]
#[allow(unsafe_code)]
fn cli_timeout_kills_the_child_and_grandchild() {
    use std::os::unix::fs::PermissionsExt;
    let home = crate::testutil::HomeSandbox::new();
    // Adopt the synthetic grandchild so this test can reap it and assert the
    // PID is actually gone, even on a container whose init never reaps orphans.
    struct RestoreSubreaper(libc::c_int);
    impl Drop for RestoreSubreaper {
        fn drop(&mut self) {
            unsafe {
                libc::prctl(libc::PR_SET_CHILD_SUBREAPER, self.0, 0, 0, 0);
            }
        }
    }
    let mut previous = 0;
    assert_eq!(
        unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut previous, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    let _restore = RestoreSubreaper(previous);
    let binary = home.home().join("fixture-agy");
    let child_pid = home.home().join("child.pid");
    let grandchild_pid = home.home().join("grandchild.pid");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
echo $$ > "$FIXTURE_CHILD_PID"
/bin/sleep 60 &
echo $! > "$FIXTURE_GRANDCHILD_PID"
wait
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = cli_command(&binary, CliCall::Usage);
    command
        .env("FIXTURE_CHILD_PID", &child_pid)
        .env("FIXTURE_GRANDCHILD_PID", &grandchild_pid);
    let start = std::time::Instant::now();
    let failure = run_cli_command(
        command,
        CliCall::Usage,
        std::time::Duration::from_millis(500),
    )
    .unwrap_err();
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
    assert!(failure.message.contains("timed out"));
    let child: libc::pid_t = std::fs::read_to_string(child_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let grandchild: libc::pid_t = std::fs::read_to_string(grandchild_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(child, 0) },
        -1,
        "direct child must be gone"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(grandchild, &mut status, libc::WNOHANG) };
        if reaped == grandchild {
            assert!(libc::WIFSIGNALED(status));
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "grandchild survived process-group kill"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        unsafe { libc::kill(grandchild, 0) },
        -1,
        "grandchild must be gone"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
fn cli_path_stays_disabled_before_owner_gate() {
    let home = crate::testutil::HomeSandbox::new();
    let mut target = target(home.home());
    target.cfg.via = Some("cli".into());
    let failure = fetch_with(
        &target,
        &super::super::source::FakeHttp::offline(),
        &Fake {
            owner: false,
            locked: 0,
            unlocked: 0,
            blob: vec![],
        },
    )
    .unwrap_err();
    assert!(failure.message.contains("AGY-CLI"));
}
#[cfg(unix)]
#[test]
fn cli_nonzero_sign_in_output_is_auth_required() {
    use std::os::unix::fs::PermissionsExt;
    let home = crate::testutil::HomeSandbox::new();
    let binary = home.home().join("fixture-agy");
    std::fs::write(
        &binary,
        "#!/bin/sh\necho 'Please sign in at https://example.invalid/login'\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        run_cli_command(
            cli_command(&binary, CliCall::Usage),
            CliCall::Usage,
            std::time::Duration::from_secs(1)
        )
        .unwrap_err()
        .kind,
        FailureKind::AuthRequired
    );
}
#[test]
fn cli_probes_memoize_for_60_seconds_and_invalidate_on_binary_change() {
    struct MemoCli {
        inner: FakeCli,
        path: std::path::PathBuf,
        len: std::sync::atomic::AtomicU64,
    }
    impl CliRunner for MemoCli {
        fn run(&self, call: CliCall) -> Result<Zeroizing<String>, Failure> {
            self.inner.run(call)
        }
        fn stamp(&self) -> Result<Option<CliBinaryStamp>, Failure> {
            Ok(Some(CliBinaryStamp {
                path: self.path.clone(),
                modified: None,
                len: self.len.load(std::sync::atomic::Ordering::Relaxed),
            }))
        }
    }
    let home = crate::testutil::HomeSandbox::new();
    let runner = MemoCli {
        inner: cli_fixture(),
        path: home.home().join("fixture-agy"),
        len: std::sync::atomic::AtomicU64::new(1),
    };
    fetch_cli(&runner, 100).unwrap();
    fetch_cli(&runner, 101).unwrap();
    assert_eq!(
        *runner.inner.calls.lock().unwrap(),
        vec![
            CliCall::Version,
            CliCall::Help,
            CliCall::Usage,
            CliCall::Usage
        ]
    );
    fetch_cli(&runner, 160).unwrap();
    assert_eq!(runner.inner.calls.lock().unwrap().len(), 7);
    runner.len.store(2, std::sync::atomic::Ordering::Relaxed);
    fetch_cli(&runner, 161).unwrap();
    assert_eq!(runner.inner.calls.lock().unwrap().len(), 10);
}
// Slice-1 review: distinguish undated borrowed credentials from expired ones.
#[test]
fn agy_missing_expiry_has_distinct_message_without_any_http_call() {
    let home = crate::testutil::HomeSandbox::new();
    let target = target(home.home());
    let http = super::super::source::FakeHttp::offline();
    let undated = Fake {
        owner: true,
        locked: 0,
        unlocked: 1,
        blob: json!({"access_token":"ACCESS"}).to_string().into_bytes(),
    };
    let expired = Fake {
        owner: true,
        locked: 0,
        unlocked: 1,
        blob: json!({"access_token":"ACCESS","expiry":160})
            .to_string()
            .into_bytes(),
    };
    let missing = fetch_with(&target, &http, &undated).unwrap_err();
    let expired = fetch_with(&target, &http, &expired).unwrap_err();
    assert_eq!(missing.kind, FailureKind::AuthRequired);
    assert_eq!(expired.kind, FailureKind::AuthRequired);
    assert_eq!(missing.message, "agy's token carries no expiry; open agy");
    assert_eq!(
        expired.message,
        "agy's token expired; open agy for a moment"
    );
    assert!(http.calls().is_empty());
}
#[test]
fn agy_subscription_required_is_subscription_inactive_without_host_fallthrough() {
    let home = crate::testutil::HomeSandbox::new();
    let mut http = super::super::source::FakeHttp::offline();
    http.send_reply = Box::new(|_, _| {
        Ok(HttpReply {
            status: 403,
            body: r#"{"error":{"details":[{"reason":"SUBSCRIPTION_REQUIRED"}]}}"#.into(),
            headers: vec![],
            retry_after_secs: None,
        })
    });
    let probe = Fake {
        owner: true,
        locked: 0,
        unlocked: 1,
        blob: json!({"access_token":"ACCESS","expiry":2000000000})
            .to_string()
            .into_bytes(),
    };
    let failure = fetch_with(&target(home.home()), &http, &probe).unwrap_err();
    assert_eq!(failure.kind, FailureKind::SubscriptionInactive);
    assert_eq!(failure.message, "free plan: no quota summary");
    assert_eq!(http.calls().len(), 1);
}
#[test]
fn agy_falls_through_daily_host_on_5xx_and_transport_error() {
    let home = crate::testutil::HomeSandbox::new();
    for status in [Some(500), Some(503), None] {
        let mut http = super::super::source::FakeHttp::offline();
        http.send_reply = Box::new(move |_, request| {
            if request.url.contains("daily-cloudcode") {
                return match status {
                    Some(status) => Ok(HttpReply {
                        status,
                        body: "{}".into(),
                        headers: vec![],
                        retry_after_secs: None,
                    }),
                    None => Err(Failure::new(
                        FailureKind::Unavailable,
                        "fixture transport error",
                    )),
                };
            }
            let body = if request.url.ends_with("loadCodeAssist") {
                r#"{"paidTier":{"name":"Pro"}}"#
            } else {
                include_str!("../fixtures/monitors/agy-summary.json")
            };
            Ok(HttpReply {
                status: 200,
                body: body.into(),
                headers: vec![],
                retry_after_secs: None,
            })
        });
        let probe = Fake {
            owner: true,
            locked: 0,
            unlocked: 1,
            blob: json!({"access_token":"ACCESS","expiry":2000000000})
                .to_string()
                .into_bytes(),
        };
        let reading = fetch_with(&target(home.home()), &http, &probe).unwrap();
        assert!(!reading.windows.is_empty());
        let calls = http.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains("daily-cloudcode-pa"));
        assert!(
            calls[1].contains(
                "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary"
            )
        );
        assert!(calls[2].contains("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"));
    }
}
// Slice-2 review: only usage prompts authenticate; documentation URLs and
// command listings are ordinary data on both probe and usage responses.
#[test]
fn cli_help_and_usage_documentation_urls_are_not_sign_in_prompts() {
    let mut runner = cli_fixture();
    runner.help =
        r#"{"commands":["/usage","/login"],"documentation":"https://example.invalid/help"}"#;
    runner.usage = r#"{"groups":[{"displayName":"Login telemetry https://example.invalid/model","buckets":[{"bucketId":"fixture","window":"5h","remainingFraction":0.5}]}]}"#;
    assert!(fetch_cli(&runner, 100).is_ok());
    assert!(!sign_in_flow(
        "A /login command is documented at https://example.invalid/help"
    ));
    assert!(sign_in_flow(
        "Please sign in at https://example.invalid/authenticate"
    ));
}
#[cfg(unix)]
#[test]
fn cli_stderr_sign_in_prompt_is_auth_required_only_for_usage() {
    use std::os::unix::fs::PermissionsExt;
    let home = crate::testutil::HomeSandbox::new();
    let program = home.home().join("fixture-agy");
    std::fs::write(&program,"#!/bin/sh\necho 'Please sign in at https://example.invalid/authenticate' >&2\necho 'agy 1.2.13'\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    for call in [CliCall::Version, CliCall::Help] {
        assert!(
            run_cli_command(
                cli_command(&program, call),
                call,
                std::time::Duration::from_secs(1)
            )
            .is_ok()
        );
    }
    let failure = run_cli_command(
        cli_command(&program, CliCall::Usage),
        CliCall::Usage,
        std::time::Duration::from_secs(1),
    )
    .unwrap_err();
    assert_eq!(failure.kind, FailureKind::AuthRequired);
}
#[cfg(unix)]
#[test]
fn cli_stderr_exceeding_shared_output_cap_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let home = crate::testutil::HomeSandbox::new();
    let program = home.home().join("fixture-agy");
    std::fs::write(&program, "#!/bin/sh\nhead -c 1048577 /dev/zero >&2\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failure = run_cli_command(
        cli_command(&program, CliCall::Usage),
        CliCall::Usage,
        std::time::Duration::from_secs(1),
    )
    .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Unavailable);
    assert!(failure.message.contains("exceeds 1 MiB"));
}
#[cfg(unix)]
#[test]
fn cli_bare_official_oauth_url_on_stderr_is_usage_auth_required() {
    use std::os::unix::fs::PermissionsExt;
    let home = crate::testutil::HomeSandbox::new();
    let program = home.home().join("fixture-agy");
    std::fs::write(&program,"#!/bin/sh\necho 'https://accounts.google.com/o/oauth2/v2/auth?client_id=fixture' >&2\necho 'agy 1.2.13'\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        run_cli_command(
            cli_command(&program, CliCall::Help),
            CliCall::Help,
            std::time::Duration::from_secs(1)
        )
        .is_ok()
    );
    assert_eq!(
        run_cli_command(
            cli_command(&program, CliCall::Usage),
            CliCall::Usage,
            std::time::Duration::from_secs(1)
        )
        .unwrap_err()
        .kind,
        FailureKind::AuthRequired
    );
    assert!(!sign_in_flow(
        "https://accounts.google.com/o/oauth2/v2/authentication-documentation"
    ));
    assert!(!sign_in_flow("https://accounts.google.com/help/login"));
}
