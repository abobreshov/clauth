#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The local agent API over real sockets: a loopback `TcpListener` on an
//! ephemeral port and the unix socket, both under a `HomeSandbox`, so the token
//! file, the socket node and every cache the handlers read live in a tempdir.
//! Caches only: nothing here reaches the network beyond 127.0.0.1.

use super::*;

use std::io::{Read, Write};

use crate::profile::{AppState, Profile, save_app_state, save_profile};
use crate::testutil::HomeSandbox;

/// A server on `127.0.0.1:0` (and the socket when `unix`).
fn serve(unix: bool) -> Server {
    start(StartOpts {
        listen: Some("127.0.0.1:0".parse().unwrap()),
        unix_socket: unix,
        status_path: crate::profile::tollgate_dir().unwrap().join("status.json"),
    })
    .expect("start the local API")
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.body))
    }
}

/// Send `raw` and read to EOF (every request here says `Connection: close`).
fn exchange<S: Read + Write>(mut stream: S, raw: &str) -> Reply {
    stream.write_all(raw.as_bytes()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("a complete response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("a status line");
    Reply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

fn request(method: &str, path: &str, bearer: Option<&str>) -> String {
    let auth = bearer
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Connection: close\r\n\r\n")
}

fn tcp(server: &Server, method: &str, path: &str, bearer: Option<&str>) -> Reply {
    let stream = TcpStream::connect(server.tcp_addr().unwrap()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    exchange(stream, &request(method, path, bearer))
}

fn token() -> String {
    ensure_token().unwrap()
}

#[test]
fn tcp_without_a_token_is_401_with_a_bearer_challenge() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let reply = tcp(&server, "GET", "/v1/health", None);
    assert_eq!(reply.status, 401);
    assert!(
        reply.head.contains("WWW-Authenticate: Bearer"),
        "{}",
        reply.head
    );
    assert_eq!(reply.json()["error"], "unauthorized");
}

#[test]
fn tcp_with_a_wrong_token_is_401() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let wrong = "0".repeat(64);
    assert_ne!(token(), wrong);
    assert_eq!(tcp(&server, "GET", "/v1/health", Some(&wrong)).status, 401);
    // A prefix of the real token is not the token.
    assert_eq!(
        tcp(&server, "GET", "/v1/health", Some(&token()[..32])).status,
        401
    );
}

#[test]
fn tcp_with_the_token_serves_health() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let reply = tcp(&server, "GET", "/v1/health", Some(&token()));
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.head.contains("Content-Type: application/json"));
    assert!(reply.head.contains("Cache-Control: no-store"));
    // Not for browsers: no CORS header is ever sent.
    assert!(!reply.head.to_ascii_lowercase().contains("access-control-"));
    let body = reply.json();
    assert_eq!(body["ok"], true);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["guest_mode"], false);
}

#[test]
fn a_non_get_method_is_405() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    for method in ["POST", "PUT", "DELETE", "HEAD", "PATCH"] {
        let reply = tcp(&server, method, "/v1/usage", Some(&token()));
        assert_eq!(reply.status, 405, "{method}");
        if method != "HEAD" {
            assert_eq!(reply.json()["error"], "method_not_allowed");
        }
    }
}

#[test]
fn an_unknown_path_is_a_json_404_but_only_after_auth() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let reply = tcp(&server, "GET", "/v1/nope", Some(&token()));
    assert_eq!(reply.status, 404);
    assert_eq!(
        reply.json(),
        serde_json::json!({"ok": false, "error": "not_found"})
    );
    // Without the token the answer is 401 whatever the path, so an
    // unauthenticated caller cannot map the routes.
    assert_eq!(tcp(&server, "GET", "/v1/nope", None).status, 401);
    assert_eq!(tcp(&server, "GET", "/api/v1/health", None).status, 401);
}

#[test]
fn an_oversized_head_is_refused_with_431() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let stream = TcpStream::connect(server.tcp_addr().unwrap()).unwrap();
    // Exactly the 8 KiB cap of an unfinished head: the server consumes every
    // byte before refusing, so its close is orderly (unread bytes would make
    // the kernel reset the connection over the answer).
    let prefix = "GET /v1/health HTTP/1.1\r\nX-Pad: ";
    let raw = format!("{prefix}{}", "a".repeat(8 * 1024 - prefix.len()));
    assert_eq!(exchange(stream, &raw).status, 431);
}

#[test]
fn a_success_keeps_the_connection_for_the_next_request() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let token = token();
    let mut stream = TcpStream::connect(server.tcp_addr().unwrap()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let pipelined = format!(
        "GET /v1/health HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n{}",
        request("GET", "/v1/providers", Some(&token))
    );
    stream.write_all(pipelined.as_bytes()).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 2, "{text}");
    assert!(text.contains("Connection: keep-alive"));
}

#[test]
fn non_loopback_binds_are_refused() {
    for bad in [
        "0.0.0.0:8454",
        "192.168.1.10:8454",
        "[::]:8454",
        "10.0.0.1:1",
    ] {
        let err = parse_listen(bad).expect_err(bad).to_string();
        assert!(err.contains("only a loopback address"), "{bad}: {err}");
    }
    for bad in ["example.com:8454", "127.0.0.1", "localhost:notaport", ""] {
        assert!(parse_listen(bad).is_err(), "{bad} must not parse");
    }
    assert_eq!(parse_listen("127.0.0.1:8454").unwrap().port(), 8454);
    assert_eq!(parse_listen(" [::1]:9 ").unwrap().to_string(), "[::1]:9");
    assert_eq!(
        parse_listen("localhost:7").unwrap().to_string(),
        "127.0.0.1:7"
    );
    assert_eq!(
        parse_listen("127.8.9.10:1").unwrap().to_string(),
        "127.8.9.10:1"
    );

    // `start` re-checks, so no caller can hand it a wildcard.
    let _home = HomeSandbox::new();
    let err = start(StartOpts {
        listen: Some("0.0.0.0:0".parse().unwrap()),
        unix_socket: false,
        status_path: PathBuf::from("/nonexistent"),
    })
    .err()
    .expect("a wildcard bind is refused")
    .to_string();
    assert!(err.contains("only a loopback address"), "{err}");
}

#[test]
fn a_taken_port_is_an_error_not_a_panic() {
    let _home = HomeSandbox::new();
    let first = serve(false);
    let err = start(StartOpts {
        listen: first.tcp_addr(),
        unix_socket: false,
        status_path: PathBuf::from("/nonexistent"),
    })
    .err()
    .expect("the second bind fails")
    .to_string();
    assert!(err.contains("failed to bind the local API"), "{err}");
}

#[test]
fn the_token_is_generated_once_owner_only_and_well_formed() {
    let _home = HomeSandbox::new();
    let path = token_path().unwrap();
    assert!(!path.exists());
    let first = ensure_token().unwrap();
    assert!(well_formed(&first), "{first}");
    assert_eq!(
        ensure_token().unwrap(),
        first,
        "a second call reuses the file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    // No temp file is left behind.
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    assert!(token_matches(&path, Some(&first)));
    assert!(!token_matches(&path, Some(&first.to_uppercase())));
    assert!(!token_matches(&path, None));
    assert!(!token_matches(&path, Some("")));
}

#[test]
fn a_malformed_token_file_is_replaced_and_never_matches() {
    let _home = HomeSandbox::new();
    let path = token_path().unwrap();
    crate::profile::mkdir_700(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "short\n").unwrap();
    // A malformed file authorizes nothing, not even its own contents.
    assert!(!token_matches(&path, Some("short")));
    let token = ensure_token().unwrap();
    assert!(well_formed(&token));
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), token);
}

#[test]
fn deleting_the_token_file_rotates_it_without_a_restart() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let old = token();
    std::fs::remove_file(token_path().unwrap()).unwrap();
    let new = token();
    assert_ne!(old, new);
    assert_eq!(tcp(&server, "GET", "/v1/health", Some(&old)).status, 401);
    assert_eq!(tcp(&server, "GET", "/v1/health", Some(&new)).status, 200);
}

#[cfg(unix)]
#[test]
fn the_unix_socket_answers_without_a_token_and_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

    let _home = HomeSandbox::new();
    let server = serve(true);
    let path = server.socket().expect("a socket").to_path_buf();
    assert_eq!(path, socket_path().unwrap());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let reply = exchange(
        UnixStream::connect(&path).unwrap(),
        &request("GET", "/v1/health", None),
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["ok"], true);
    // The same routing rules apply on this door.
    let reply = exchange(
        UnixStream::connect(&path).unwrap(),
        &request("POST", "/v1/health", None),
    );
    assert_eq!(reply.status, 405);
    let reply = exchange(
        UnixStream::connect(&path).unwrap(),
        &request("GET", "/v2", None),
    );
    assert_eq!(reply.status, 404);

    drop(server);
    assert!(!path.exists(), "dropping the server removes its socket");
}

#[cfg(unix)]
#[test]
fn signal_cleanup_removes_the_bound_socket() {
    let _home = HomeSandbox::new();
    let server = serve(true);
    let path = server.socket().unwrap().to_path_buf();
    server.cleanup_socket();
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn dropping_a_server_leaves_a_rebound_foreign_socket_alone() {
    use std::os::unix::net::{UnixListener, UnixStream};

    let _home = HomeSandbox::new();
    let server = serve(true);
    let path = server.socket().unwrap().to_path_buf();
    std::fs::remove_file(&path).unwrap();
    let foreign = UnixListener::bind(&path).unwrap();
    drop(server);
    assert!(path.exists());
    assert!(UnixStream::connect(&path).is_ok());
    drop(foreign);
}

#[cfg(unix)]
#[test]
fn a_live_socket_is_never_stolen_but_a_stale_one_is_replaced() {
    let _home = HomeSandbox::new();
    let live = serve(true);
    let err = start(StartOpts {
        listen: None,
        unix_socket: true,
        status_path: PathBuf::from("/nonexistent"),
    })
    .err()
    .expect("a second server on the live socket is refused")
    .to_string();
    assert!(err.contains("already answers"), "{err}");
    assert!(live.socket().unwrap().exists());
    drop(live);

    // A node nothing listens on (a killed daemon's) is replaced.
    let path = socket_path().unwrap();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let fresh = serve(true);
    assert_eq!(fresh.socket(), Some(path.as_path()));
}

#[cfg(unix)]
#[test]
fn bind_unix_replaces_a_stale_socket_node_after_listener_drops() {
    use std::os::unix::net::UnixStream;

    let _home = HomeSandbox::new();
    let path = socket_path().unwrap();
    let (stale, _) = bind_unix(&path).unwrap();
    drop(stale);
    assert!(path.exists());

    let (rebound, _) = bind_unix(&path).unwrap();
    assert!(UnixStream::connect(&path).is_ok());
    drop(rebound);
    std::fs::remove_file(path).unwrap();
}

/// One OAuth profile, and one api-key profile whose endpoint carries every
/// credential shape a URL can: userinfo, a key-shaped path segment, a query.
fn seed_profiles_with_a_leaky_endpoint() {
    save_profile(&Profile::new("solo".to_string(), None, None)).unwrap();
    save_profile(&Profile::new(
        "vendor".to_string(),
        Some(
            "https://me:hunter2hunter2@api.example.com/v1/sk-live-abcdefghijklmnop0123/anthropic\
             ?key=sk-query-secret-value#frag"
                .to_string(),
        ),
        Some("sk-test-not-a-real-key-0123456789".to_string()),
    ))
    .unwrap();
    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "vendor".into()],
        ..Default::default()
    })
    .unwrap();
}

#[test]
fn accounts_over_tcp_carry_no_credential() {
    let _home = HomeSandbox::new();
    seed_profiles_with_a_leaky_endpoint();
    let server = serve(false);
    let reply = tcp(&server, "GET", "/v1/accounts", Some(&token()));
    assert_eq!(reply.status, 200, "{}", reply.body);
    for secret in ["hunter2", "sk-live", "sk-query", "sk-test", "frag", "me:"] {
        assert!(
            !reply.body.contains(secret),
            "{secret} leaked: {}",
            reply.body
        );
    }
    let body = reply.json();
    assert_eq!(body["schema_version"], 1);
    let ids: Vec<&str> = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["claude:solo", "claude:vendor"]);
    assert_eq!(
        body["accounts"][1]["endpoint"],
        "https://api.example.com/v1/[redacted]/anthropic"
    );

    // The same for one account, looked up by its id (colon percent-encoded
    // or not) or by its name.
    for path in [
        "/v1/accounts/claude:vendor",
        "/v1/accounts/claude%3Avendor",
        "/v1/accounts/vendor",
    ] {
        let reply = tcp(&server, "GET", path, Some(&token()));
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        assert_eq!(reply.json()["account"]["id"], "claude:vendor", "{path}");
        assert!(!reply.body.contains("hunter2"));
    }
    let reply = tcp(&server, "GET", "/v1/accounts/claude:ghost", Some(&token()));
    assert_eq!(reply.status, 404);
    assert_eq!(reply.json()["error"], "account_not_found");
}

#[test]
fn usage_over_tcp_is_the_usage_json_envelope_and_filters() {
    let _home = HomeSandbox::new();
    seed_profiles_with_a_leaky_endpoint();
    let server = serve(false);
    let reply = tcp(&server, "GET", "/v1/usage", Some(&token()));
    assert_eq!(reply.status, 200);
    let report: crate::usage::report::UsageReport =
        serde_json::from_str(&reply.body).expect("the body is a UsageReport");
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.accounts.len(), 2);
    assert!(!report.guest_mode);
    // Key order matches `tollgate usage --json`.
    let keys: Vec<String> = reply.json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        ["schema_version", "generated_at", "guest_mode", "accounts"]
    );

    let one = tcp(&server, "GET", "/v1/usage?account=solo", Some(&token())).json();
    assert_eq!(one["accounts"].as_array().unwrap().len(), 1);
    let none = tcp(&server, "GET", "/v1/usage?provider=codex", Some(&token())).json();
    assert!(none["accounts"].as_array().unwrap().is_empty());
}

#[test]
fn providers_mark_what_is_configured() {
    let _home = HomeSandbox::new();
    seed_profiles_with_a_leaky_endpoint();
    let server = serve(false);
    let body = tcp(&server, "GET", "/v1/providers", Some(&token())).json();
    let rows = body["providers"].as_array().unwrap();
    assert_eq!(rows.len(), routes::CATALOG.len());
    let row = |source: &str| rows.iter().find(|r| r["source"] == source).unwrap().clone();
    assert_eq!(row("anthropic_oauth")["configured"], true);
    assert_eq!(row("anthropic_oauth")["accounts"], 1);
    assert_eq!(row("anthropic_oauth")["display_name"], "Anthropic");
    assert_eq!(row("codex")["configured"], false);
    assert_eq!(row("openrouter")["auth_kinds"][0], "api_key");
}

#[test]
fn status_passes_the_published_feed_through() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    // No feed yet: built on the spot from config.
    let built = tcp(&server, "GET", "/v1/status", Some(&token()));
    assert_eq!(built.status, 200, "{}", built.body);
    assert!(built.json().get("profiles").is_some(), "{}", built.body);

    let feed = crate::profile::tollgate_dir().unwrap().join("status.json");
    std::fs::write(&feed, br#"{"schema":1,"marker":"published"}"#).unwrap();
    let passed = tcp(&server, "GET", "/v1/status", Some(&token()));
    assert_eq!(passed.status, 200);
    // The feed's bytes, parsed and re-serialised, plus the import block
    // (import spec §2.5).
    assert_eq!(
        passed.body,
        r#"{"schema":1,"marker":"published","import":{"state":"none","completed_at":null}}"#
    );
    assert!(passed.head.contains("ETag: \""));
}

#[test]
fn openapi_is_served_and_names_every_route() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let body = tcp(&server, "GET", "/v1/openapi.json", Some(&token())).json();
    let paths = body["paths"].as_object().unwrap();
    for path in [
        "/v1/health",
        "/v1/accounts",
        "/v1/accounts/{id}",
        "/v1/usage",
        "/v1/providers",
        "/v1/status",
        "/v1/openapi.json",
    ] {
        assert!(paths.contains_key(path), "{path} undocumented");
    }
}

#[test]
fn the_settings_round_trip_through_profiles_toml() {
    let defaults: AppState = toml::from_str("profiles = []").unwrap();
    assert_eq!(defaults.local_api, LocalApiSettings::default());
    assert!(defaults.local_api.enabled);
    assert_eq!(defaults.local_api.listen, DEFAULT_LISTEN);
    // At its default the table is not written.
    assert!(!toml::to_string(&defaults).unwrap().contains("local_api"));

    let custom: AppState = toml::from_str(
        "profiles = []\nlocal_api = { enabled = false, listen = \"127.0.0.1:9999\" }\n",
    )
    .unwrap();
    assert!(!custom.local_api.enabled);
    assert_eq!(custom.local_api.listen, "127.0.0.1:9999");
    let written = toml::to_string(&custom).unwrap();
    let back: AppState = toml::from_str(&written).unwrap();
    assert_eq!(back.local_api, custom.local_api);

    // A partial table fills from the defaults, and a bad address still loads
    // (it is refused when the listener starts, not on every config read).
    let partial: AppState =
        toml::from_str("profiles = []\n[local_api]\nlisten = \"0.0.0.0:1\"\n").unwrap();
    assert!(partial.local_api.enabled);
    assert!(parse_listen(&partial.local_api.listen).is_err());
}

#[test]
fn the_daemon_skips_the_api_when_disabled() {
    let off = LocalApiSettings {
        enabled: false,
        ..LocalApiSettings::default()
    };
    assert_eq!(
        daemon_skip_reason(&off).as_deref(),
        Some("local_api.enabled is false")
    );
}

#[test]
fn url_text_names_both_doors_and_the_token_file() {
    let text = url_text(
        "127.0.0.1:8454".parse().unwrap(),
        Path::new("/h/.tollgate/api-token"),
        Path::new("/h/.tollgate/api.sock"),
        true,
    );
    assert!(text.starts_with("http://127.0.0.1:8454\n"), "{text}");
    assert!(text.contains(
        "curl -s -H \"Authorization: Bearer $(cat /h/.tollgate/api-token)\" \
         http://127.0.0.1:8454/v1/usage"
    ));
    #[cfg(unix)]
    assert!(text.contains("curl -s --unix-socket /h/.tollgate/api.sock http://localhost/v1/usage"));
    assert!(!text.contains("enabled is false"));
    assert!(
        url_text(
            "127.0.0.1:1".parse().unwrap(),
            Path::new("t"),
            Path::new("s"),
            false
        )
        .contains("local_api.enabled is false")
    );
}

#[cfg(unix)]
#[test]
fn a_socket_that_cannot_be_bound_leaves_tcp_serving() {
    let _home = HomeSandbox::new();
    // Something else already answers on the socket path.
    let path = socket_path().unwrap();
    crate::profile::mkdir_700(path.parent().unwrap()).unwrap();
    let squatter = std::os::unix::net::UnixListener::bind(&path).unwrap();

    let server = serve(true);
    assert_eq!(server.socket(), None);
    let err = server
        .socket_error()
        .expect("the socket failure is reported");
    assert!(err.contains("already answers"), "{err}");
    assert_eq!(
        tcp(&server, "GET", "/v1/health", Some(&token())).status,
        200
    );

    // Dropping the server leaves the squatter's node alone.
    drop(server);
    assert!(path.exists());
    drop(squatter);
}

// ── Read-only: a poll changes nothing on disk ─────────────────────────────────

/// Every node under `root`: relative path, kind, mode, mtime and a digest of
/// its bytes (a link's target), sorted. Two equal digests mean nothing was
/// created, removed, rewritten, retouched or chmodded in between.
#[cfg(unix)]
fn tree_digest(root: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let meta = path.symlink_metadata().unwrap();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let content = if meta.file_type().is_symlink() {
                format!("-> {}", std::fs::read_link(&path).unwrap().display())
            } else if meta.is_file() {
                hex::encode(sha2::Sha256::digest(std::fs::read(&path).unwrap()))
            } else {
                String::new()
            };
            out.push(format!(
                "{rel} mode={:o} mtime={}.{} {content}",
                meta.mode(),
                meta.mtime(),
                meta.mtime_nsec()
            ));
            if meta.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// The repairs `load_config` makes on every entry point, all set up at once: a
/// staged rotation sidecar newer than its commit (adopted = `credentials.json`
/// rewritten and the sidecar deleted), a `config.toml` whose canonical form
/// differs (rewritten), and a file with group/other bits (chmodded).
#[cfg(unix)]
fn seed_everything_a_repairing_load_would_touch() -> crate::profile::ProfileName {
    use std::os::unix::fs::PermissionsExt;
    let name = crate::profile::ProfileName::from("solo");
    let pair = |access: &str| crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: access.to_string(),
            refresh_token: Some(format!("{access}-refresh")),
            expires_at: None,
            scopes: None,
            subscription_type: None,
            ..crate::profile::OAuthToken::default_extra()
        }),
    };
    let mut solo = Profile::new("solo".to_string(), None, None);
    solo.credentials = Some(pair("committed"));
    save_profile(&solo).unwrap();
    crate::profile::stage_rotated_credentials(&name, &pair("staged")).unwrap();
    let cred = crate::profile::profile_subpath(&name, "credentials.json").unwrap();
    let pending = crate::profile::profile_subpath(&name, "credentials.json.pending").unwrap();
    let now = std::time::SystemTime::now();
    crate::testutil::set_mtime(&cred, now - Duration::from_secs(60));
    crate::testutil::set_mtime(&pending, now);

    save_profile(&Profile::new("drift".to_string(), None, None)).unwrap();
    let drift = crate::profile::profile_subpath(&"drift".into(), "config.toml").unwrap();
    std::fs::write(&drift, "preferred_days = [\"Saturday\"]\n").unwrap();

    save_app_state(&AppState {
        active_profile: Some("solo".into()),
        profiles: vec!["solo".into(), "drift".into()],
        ..Default::default()
    })
    .unwrap();
    let state = crate::profile::tollgate_dir()
        .unwrap()
        .join("profiles.toml");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
    name
}

/// Every `GET /v1/*` over TCP leaves the sandbox home byte-identical, down to
/// modes and mtimes, even with a pending rotation sidecar, a drifted
/// `config.toml` and a loose mode on disk: the collector and the status
/// rebuild load through `load_config_read_only`, never `load_config`. The
/// staged pair is still what the read-only load describes, so both loads agree
/// on the account.
#[cfg(unix)]
#[test]
fn every_get_leaves_the_home_byte_identical_even_with_a_pending_sidecar() {
    let home = HomeSandbox::new();
    let name = seed_everything_a_repairing_load_would_touch();
    let server = serve(false);
    let token = token();
    let before = tree_digest(home.home());

    for path in [
        "/v1/health",
        "/v1/accounts",
        "/v1/accounts?all=1",
        "/v1/accounts/claude:solo",
        "/v1/usage",
        "/v1/providers",
        "/v1/status",
        "/v1/openapi.json",
    ] {
        let reply = tcp(&server, "GET", path, Some(&token));
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
    }
    assert_eq!(
        tree_digest(home.home()),
        before,
        "a GET changed something under the home"
    );

    let read_only = crate::profile::load_config_read_only().unwrap();
    let refresh = |config: &crate::profile::AppConfig| {
        config
            .find(&name)
            .and_then(|p| p.credentials.clone())
            .and_then(|c| c.claude_ai_oauth.clone())
            .and_then(|o| o.refresh_token.clone())
    };
    assert_eq!(
        refresh(&read_only).as_deref(),
        Some("staged-refresh"),
        "the read-only load sees the pair a repairing load would adopt"
    );
    assert_eq!(
        tree_digest(home.home()),
        before,
        "and wrote nothing doing it"
    );
    let repaired = crate::profile::load_config().unwrap();
    assert_eq!(refresh(&repaired), refresh(&read_only), "both loads agree");
    assert_ne!(
        tree_digest(home.home()),
        before,
        "the repairing load did repair: the fixture exercises every repair"
    );
}

/// Hot-swap spec test 63, extending the I3 pin above: with a live api-key
/// session mid swap and every sidecar it can leave beside its row (the helper
/// ack and its lock, a pending relaunch request and a claimed one), every
/// `GET` still leaves the home byte-identical, down to modes and mtimes. The
/// `live_sessions` field reads the row and the ack and nothing else: no
/// flock, no marker probe, no rename, no `load_config`.
#[cfg(unix)]
#[test]
fn every_get_leaves_the_home_byte_identical_with_hot_swap_sidecars() {
    let home = HomeSandbox::new();
    seed_everything_a_repairing_load_would_touch();
    let launch = crate::testutil::api_key_profile("solo", "https://openrouter.ai/api", "k");
    let mut row = crate::testutil::live_row("4242-0", "solo").with_executor(
        crate::hot_swap::Executor::ApiKey,
        crate::hot_swap::LaunchClass::of(&launch, true),
    );
    row.pid = std::process::id();
    row.current_member = Some("drift".into());
    row.key_generation = Some(1);
    row.committed_at = Some(crate::usage::now_ms());
    crate::live_sessions::register(&row).unwrap();
    crate::hot_swap::write_ack_for_test(
        "4242-0",
        &crate::hot_swap::HelperAck {
            version: 1,
            generation: 0,
            member: Some("solo".into()),
            served_at_ms: Some(1),
            last_failure: None,
        },
    );
    let dir = crate::profile::tollgate_dir()
        .unwrap()
        .join("live_sessions");
    std::fs::write(dir.join("4242-0.helper.lock"), b"").unwrap();
    std::fs::write(dir.join("4242-0.relaunch"), b"{\"version\":1}").unwrap();
    std::fs::write(dir.join("4242-0.relaunch.taken"), b"{\"version\":1}").unwrap();

    let server = serve(false);
    let token = token();
    let before = tree_digest(home.home());
    for path in [
        "/v1/accounts",
        "/v1/accounts?all=1",
        "/v1/accounts/claude:solo",
        "/v1/accounts/claude:drift",
        "/v1/usage",
        "/v1/status",
    ] {
        let reply = tcp(&server, "GET", path, Some(&token));
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        if path.starts_with("/v1/accounts") {
            let sessions = reply.json()["live_sessions"].clone();
            assert_eq!(sessions[0]["session_id"], "4242-0", "{path}: {sessions}");
            assert_eq!(sessions[0]["state"], "swapping", "{path}");
            assert_eq!(sessions[0]["served"]["member"], "solo", "{path}");
        }
    }
    assert_eq!(
        tree_digest(home.home()),
        before,
        "a GET changed something under the home"
    );
}

// ── Host: the DNS-rebinding guard ─────────────────────────────────────────────

fn with_host(host: Option<&str>, bearer: &str) -> String {
    let host = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
    format!(
        "GET /v1/health HTTP/1.1\r\n{host}Authorization: Bearer {bearer}\r\nConnection: close\r\n\r\n"
    )
}

fn tcp_raw(server: &Server, raw: &str) -> Reply {
    let stream = TcpStream::connect(server.tcp_addr().unwrap()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    exchange(stream, raw)
}

/// A page on `evil.example` whose name was rebound to 127.0.0.1 reaches the
/// port, but its requests carry `Host: evil.example`: refused 421 even with
/// the right token. A missing Host on TCP is a 400; every loopback spelling,
/// with or without the port, is served.
#[test]
fn tcp_refuses_a_non_loopback_host_even_with_the_token() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let token = token();
    let port = server.tcp_addr().unwrap().port();

    for host in [
        "evil.example".to_string(),
        format!("evil.example:{port}"),
        format!("localhost.evil.example:{port}"),
        format!("127.0.0.1.nip.io:{port}"),
        "[::2]".to_string(),
        format!("127.0.0.1:{port}x"),
        String::new(),
    ] {
        let reply = tcp_raw(&server, &with_host(Some(&host), &token));
        assert_eq!(reply.status, 421, "Host {host:?}: {}", reply.body);
        assert_eq!(reply.json()["error"], "misdirected_request", "{host:?}");
        assert!(reply.head.starts_with("HTTP/1.1 421 Misdirected Request"));
    }

    let missing = tcp_raw(&server, &with_host(None, &token));
    assert_eq!(missing.status, 400, "{}", missing.body);
    assert_eq!(missing.json()["error"], "host_required");

    let twice = tcp_raw(
        &server,
        &format!(
            "GET /v1/health HTTP/1.1\r\nHost: localhost\r\nHost: evil.example\r\n\
             Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(twice.status, 400, "two Host headers are malformed");

    for host in [
        "localhost".to_string(),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        "127.0.0.1".to_string(),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        "[::1]".to_string(),
    ] {
        let reply = tcp_raw(&server, &with_host(Some(&host), &token));
        assert_eq!(reply.status, 200, "Host {host:?}: {}", reply.body);
    }
}

/// The unix door is not a browser's: any Host, or none, is served.
#[cfg(unix)]
#[test]
fn the_unix_socket_takes_any_host_or_none() {
    use std::os::unix::net::UnixStream;
    let _home = HomeSandbox::new();
    let server = serve(true);
    let path = server.socket().unwrap().to_path_buf();
    for host in [Some("evil.example"), None] {
        let reply = exchange(UnixStream::connect(&path).unwrap(), &with_host(host, "x"));
        assert_eq!(reply.status, 200, "{host:?}: {}", reply.body);
    }
}

// ── The socket's directory ────────────────────────────────────────────────────

/// A data dir left group/other-readable is tightened to 0700 before the bind,
/// so no other user can traverse to the node in the window between `bind` and
/// its `chmod`.
#[cfg(unix)]
#[test]
fn a_loose_data_dir_is_tightened_before_the_socket_is_bound() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    let dir = crate::profile::tollgate_dir().unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = serve(true);
    assert!(server.socket().is_some(), "{:?}", server.socket_error());
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

/// A symlinked data dir puts the socket wherever the link points: refused.
/// With TCP up the refusal is the socket error and TCP still serves; alone it
/// is the start's error.
#[cfg(unix)]
#[test]
fn a_symlinked_data_dir_is_refused_for_the_socket() {
    let home = HomeSandbox::new();
    let elsewhere = home.home().join("elsewhere");
    crate::profile::mkdir_700(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, crate::profile::tollgate_dir().unwrap()).unwrap();

    let server = serve(true);
    assert_eq!(server.socket(), None);
    let err = server.socket_error().expect("the socket is refused");
    assert!(err.contains("symlink"), "{err}");
    assert!(!elsewhere.join(SOCKET_FILE).exists());
    assert_eq!(
        tcp(&server, "GET", "/v1/health", Some(&token())).status,
        200
    );
    drop(server);

    let err = start(StartOpts {
        listen: None,
        unix_socket: true,
        status_path: PathBuf::from("/nonexistent"),
    })
    .err()
    .expect("socket-only start fails")
    .to_string();
    assert!(err.contains("symlink"), "{err}");
}

/// A directory another uid owns is refused, and left exactly as it was.
/// Needs a directory this user does not own: `/` when not running as root.
#[cfg(unix)]
#[test]
fn a_data_dir_owned_by_another_user_is_refused() {
    use std::os::unix::fs::MetadataExt;
    let root = Path::new("/");
    let meta = root.symlink_metadata().unwrap();
    if effective_uid() == 0 || meta.uid() == effective_uid() {
        return; // no foreign-owned directory to test against
    }
    let err = secure_socket_dir(root).unwrap_err().to_string();
    assert!(err.contains("owned by uid"), "{err}");
    assert_eq!(root.symlink_metadata().unwrap().mode(), meta.mode());
}

/// Test 64 (import spec §2.5): `GET /v1/health` and `GET /v1/status` carry
/// the import journal's state and completion instant, read without a lock,
/// and the OpenAPI document names the block.
#[test]
fn health_and_status_routes_carry_the_import_state() {
    let _home = HomeSandbox::new();
    let server = serve(false);
    let health = tcp(&server, "GET", "/v1/health", Some(&token())).json();
    assert_eq!(health["import"]["state"], "none");
    assert!(health["import"]["completed_at"].is_null());
    let journal = crate::profile::tollgate_dir()
        .unwrap()
        .join(crate::identity::IMPORT_JOURNAL_FILE);
    for (state, done) in [
        ("pre", None),
        ("in_progress", None),
        ("complete", Some("2026-09-29T12:00:00Z")),
        ("rolling_back", None),
        ("rolled_back", None),
        ("aborted", None),
    ] {
        std::fs::write(
            &journal,
            serde_json::json!({"state": state, "completed_at": done}).to_string(),
        )
        .unwrap();
        let health = tcp(&server, "GET", "/v1/health", Some(&token())).json();
        assert_eq!(health["import"]["state"], state);
        assert_eq!(health["import"]["completed_at"].as_str(), done, "{state}");
        let status = tcp(&server, "GET", "/v1/status", Some(&token())).json();
        assert_eq!(status["import"]["state"], state, "{status}");
    }
    let doc = tcp(&server, "GET", "/v1/openapi.json", Some(&token())).json();
    let schemas = &doc["components"]["schemas"];
    assert!(
        schemas["HealthBody"]["properties"].get("import").is_some(),
        "{}",
        schemas["HealthBody"]
    );
    let block = &schemas["ImportBlock"]["properties"];
    assert!(block.get("state").is_some() && block.get("completed_at").is_some());
}
