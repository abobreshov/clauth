#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Guest mode's additive writes: tollgate's own keys in the shared Claude Code
//! files move, upstream clauth's never do. Every fixture stages a fake upstream
//! install (`~/.clauth`, its lock, its entries in each shared file) inside a
//! `HomeSandbox`, and every "upstream is untouched" claim parses the file
//! before and after and compares everything except tollgate's keys.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::testutil::HomeSandbox;

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(map) => map,
        other => panic!("fixture is not an object: {other}"),
    }
}

fn read(path: &Path) -> Map<String, Value> {
    read_object(path).unwrap().expect("file exists")
}

const UPSTREAM_SETTINGS: &str = r#"{"env":{"UPSTREAM":"1"},"apiKeyHelper":"clauth __api-key personal","enabledPlugins":{"clauth@clauth":true,"other@market":false},"theme":"dark"}"#;
const UPSTREAM_CLAUDE_JSON: &str = r#"{"oauthAccount":{"accountUuid":"upstream-uuid"},"mcpServers":{"clauth":{"command":"clauth","args":["mcp"]}},"numStartups":3}"#;
const UPSTREAM_INSTALLED: &str = r#"{"version":2,"plugins":{"clauth@clauth":[{"scope":"user","version":"0.16.0","installPath":"/x/cache/clauth/clauth/0.16.0"}]}}"#;
const UPSTREAM_MARKETPLACES: &str =
    r#"{"clauth":{"source":{"source":"github","repo":"uwuclxdy/clauth"}}}"#;

/// Upstream's install as guest mode sees it: `~/.clauth` with its state lock,
/// and upstream's entries in every shared Claude Code file.
fn stage_upstream(home: &Path) {
    let clauth = home.join(crate::identity::UPSTREAM_DATA_DIR_NAME);
    std::fs::create_dir_all(clauth.join("profiles")).unwrap();
    std::fs::write(clauth.join(UPSTREAM_LOCK_FILE), "").unwrap();
    let claude = home.join(".claude");
    std::fs::create_dir_all(claude.join("plugins")).unwrap();
    std::fs::write(claude.join("settings.json"), UPSTREAM_SETTINGS).unwrap();
    std::fs::write(
        claude.join("plugins").join("installed_plugins.json"),
        UPSTREAM_INSTALLED,
    )
    .unwrap();
    std::fs::write(
        claude.join("plugins").join("known_marketplaces.json"),
        UPSTREAM_MARKETPLACES,
    )
    .unwrap();
    std::fs::write(home.join(".claude.json"), UPSTREAM_CLAUDE_JSON).unwrap();
    assert!(crate::identity::upstream_active(), "the fixture is a guest");
}

/// Upstream's lock held by "upstream" — a second open file description, so
/// the flock conflicts even inside this process.
fn hold_upstream_lock(home: &Path) -> std::fs::File {
    let file = std::fs::File::open(
        home.join(crate::identity::UPSTREAM_DATA_DIR_NAME)
            .join(UPSTREAM_LOCK_FILE),
    )
    .unwrap();
    file.lock().unwrap();
    file
}

fn shared_files(home: &Path) -> Vec<(PathBuf, &'static [OwnedKey])> {
    let claude = home.join(".claude");
    vec![
        (claude.join("settings.json"), SETTINGS_KEYS),
        (
            claude.join("plugins").join("installed_plugins.json"),
            INSTALLED_PLUGINS_KEYS,
        ),
        (
            claude.join("plugins").join("known_marketplaces.json"),
            KNOWN_MARKETPLACES_KEYS,
        ),
        (home.join(".claude.json"), CLAUDE_JSON_KEYS),
    ]
}

/// Each shared file's foreign view (everything but tollgate's keys).
fn foreign_views(home: &Path) -> Vec<Map<String, Value>> {
    shared_files(home)
        .iter()
        .map(|(path, owned)| foreign_view(&read(path), owned))
        .collect()
}

// ── the pure owned-keys rules ─────────────────────────────────────────────

#[test]
fn foreign_view_drops_only_the_owned_keys_and_their_emptied_parents() {
    let doc = obj(json!({
        "mcpServers": {"tollgate": {"command": "tollgate"}},
        "enabledPlugins": {"tollgate@tollgate": true, "clauth@clauth": true},
        "tollgate": 1,
        "env": {}
    }));
    let owned = [
        OwnedKey::Entry("mcpServers", "tollgate"),
        OwnedKey::Entry("enabledPlugins", "tollgate@tollgate"),
        OwnedKey::Top("tollgate"),
    ];
    assert_eq!(
        Value::Object(foreign_view(&doc, &owned)),
        json!({"enabledPlugins": {"clauth@clauth": true}, "env": {}}),
        "an emptied owned parent goes; a foreign empty object stays"
    );
}

#[test]
fn foreign_changes_names_every_foreign_key_an_edit_moves() {
    let before = obj(json!({"mcpServers": {"clauth": 1}, "numStartups": 3, "gone": true}));
    let after = obj(json!({
        "mcpServers": {"clauth": 2, "tollgate": {}, "new": 1},
        "numStartups": 3,
        "added": 1
    }));
    assert_eq!(
        foreign_changes(&before, &after, CLAUDE_JSON_KEYS),
        vec!["mcpServers.clauth", "mcpServers.new", "gone", "added"],
    );
    let only_owned =
        obj(json!({"mcpServers": {"clauth": 1, "tollgate": {}}, "numStartups": 3, "gone": true}));
    assert!(foreign_changes(&before, &only_owned, CLAUDE_JSON_KEYS).is_empty());
    // A non-object parent replaced by a map holding only the owned entry is a
    // foreign change: the old value was not tollgate's.
    let scalar = obj(json!({"mcpServers": "oops"}));
    let replaced = obj(json!({"mcpServers": {"tollgate": {}}}));
    assert_eq!(
        foreign_changes(&scalar, &replaced, CLAUDE_JSON_KEYS),
        vec!["mcpServers"]
    );
}

#[test]
fn rebase_keeps_the_childs_owned_keys_and_additions_and_undoes_the_rest() {
    let before = obj(json!({
        "version": 2,
        "plugins": {"clauth@clauth": [1], "tollgate@tollgate": ["old"]}
    }));
    // A child that rewrote the file from scratch: upstream's row and the
    // version are gone, a new foreign key appeared.
    let after = obj(json!({"plugins": {"tollgate@tollgate": ["new"]}, "extra": true}));
    let lost = lost_foreign_keys(&before, &after, INSTALLED_PLUGINS_KEYS);
    assert_eq!(lost, vec!["version", "plugins.clauth@clauth"]);
    let rebased = rebase(&before, &after, INSTALLED_PLUGINS_KEYS);
    assert_eq!(
        Value::Object(rebased.clone()),
        json!({
            "version": 2,
            "plugins": {"clauth@clauth": [1], "tollgate@tollgate": ["new"]},
            "extra": true
        })
    );
    assert!(lost_foreign_keys(&before, &rebased, INSTALLED_PLUGINS_KEYS).is_empty());
    // The child removing tollgate's own row is honoured.
    let removed = obj(json!({"version": 2, "plugins": {"clauth@clauth": [1]}}));
    assert_eq!(
        Value::Object(rebase(&before, &removed, INSTALLED_PLUGINS_KEYS)),
        json!({"version": 2, "plugins": {"clauth@clauth": [1]}})
    );
    // A top-level owned key follows the child both ways.
    let m_before = obj(json!({"clauth": 1}));
    let m_after = obj(json!({"tollgate": 2}));
    assert_eq!(
        Value::Object(rebase(&m_before, &m_after, KNOWN_MARKETPLACES_KEYS)),
        json!({"clauth": 1, "tollgate": 2})
    );
}

/// Claude Code 2.1.283's `plugin marketplace add --scope user` also declares
/// the marketplace in `settings.json`'s `extraKnownMarketplaces`, where
/// upstream's `clauth` entry already sits. tollgate's entry there is its own:
/// the guard keeps it and still puts back any upstream entry the child moved.
#[test]
fn rebase_keeps_tollgates_marketplace_declaration_beside_upstreams() {
    let before = obj(json!({
        "enabledPlugins": {"clauth@clauth": true},
        "extraKnownMarketplaces": {"clauth": {"source": {"source": "directory", "path": "/u"}}},
        "model": "opus"
    }));
    let tollgate_mkt = json!({"source": {"source": "directory", "path": "/t"}});
    let after = obj(json!({
        "enabledPlugins": {"clauth@clauth": true, "tollgate@tollgate": true},
        "extraKnownMarketplaces": {
            "clauth": {"source": {"source": "directory", "path": "/u"}},
            "tollgate": tollgate_mkt.clone()
        },
        "model": "opus"
    }));
    assert!(
        lost_foreign_keys(&before, &after, SETTINGS_KEYS).is_empty(),
        "adding tollgate's own declaration moves no upstream key"
    );
    assert_eq!(
        Value::Object(rebase(&before, &after, SETTINGS_KEYS)),
        Value::Object(after.clone())
    );

    // A child that also dropped upstream's declaration: that entry comes
    // back, tollgate's stays.
    let hostile = obj(json!({
        "enabledPlugins": {"clauth@clauth": true, "tollgate@tollgate": true},
        "extraKnownMarketplaces": {"tollgate": tollgate_mkt.clone()},
        "model": "opus"
    }));
    assert_eq!(
        lost_foreign_keys(&before, &hostile, SETTINGS_KEYS),
        vec!["extraKnownMarketplaces.clauth"]
    );
    let rebased = rebase(&before, &hostile, SETTINGS_KEYS);
    assert_eq!(
        rebased["extraKnownMarketplaces"],
        json!({
            "clauth": {"source": {"source": "directory", "path": "/u"}},
            "tollgate": tollgate_mkt
        })
    );
    assert!(lost_foreign_keys(&before, &rebased, SETTINGS_KEYS).is_empty());
}

// ── the writer ───────────────────────────────────────────────────────────

#[test]
fn additive_write_adds_updates_and_removes_only_owned_keys() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let path = home.home().join(".claude.json");
    let before = foreign_views(home.home());

    let wrote = guest_additive_write(&path, CLAUDE_JSON_KEYS, |root| {
        root["mcpServers"]["tollgate"] = json!({"command": "tollgate"});
        Ok(())
    })
    .unwrap();
    assert!(wrote);
    assert_eq!(
        read(&path)["mcpServers"]["tollgate"],
        json!({"command": "tollgate"})
    );

    // The same edit again changes nothing and writes nothing.
    let bytes = std::fs::read(&path).unwrap();
    let wrote = guest_additive_write(&path, CLAUDE_JSON_KEYS, |root| {
        root["mcpServers"]["tollgate"] = json!({"command": "tollgate"});
        Ok(())
    })
    .unwrap();
    assert!(!wrote);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    // Removal of tollgate's own entry is additive too.
    guest_additive_write(&path, CLAUDE_JSON_KEYS, |root| {
        root["mcpServers"]
            .as_object_mut()
            .unwrap()
            .shift_remove("tollgate");
        Ok(())
    })
    .unwrap();
    assert!(read(&path)["mcpServers"].get("tollgate").is_none());
    assert_eq!(
        foreign_views(home.home()),
        before,
        "upstream's keys never moved"
    );
}

#[test]
fn additive_write_refuses_an_edit_that_touches_a_foreign_key() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let path = home.home().join(".claude.json");
    let bytes = std::fs::read(&path).unwrap();
    for (what, edit) in [
        (
            "oauthAccount",
            Box::new(|root: &mut Map<String, Value>| {
                root.shift_remove("oauthAccount");
            }) as Box<dyn Fn(&mut Map<String, Value>)>,
        ),
        (
            "mcpServers.clauth",
            Box::new(|root: &mut Map<String, Value>| {
                root["mcpServers"]["clauth"] = json!({"command": "tollgate"});
            }),
        ),
    ] {
        let err = guest_additive_write(&path, CLAUDE_JSON_KEYS, |root| {
            edit(root);
            Ok(())
        })
        .unwrap_err();
        assert!(
            err.to_string().contains(what),
            "the refusal names {what}: {err}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "nothing written");
    }
}

#[test]
fn additive_write_waits_a_bounded_time_on_upstreams_lock_then_fails_clearly() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let path = home.home().join(".claude.json");
    let bytes = std::fs::read(&path).unwrap();
    let _held = hold_upstream_lock(home.home());
    let _fast = upstream_lock_timeout_for_test(Duration::from_millis(150));

    let started = std::time::Instant::now();
    let err = guest_additive_write(&path, CLAUDE_JSON_KEYS, |root| {
        root["mcpServers"]["tollgate"] = json!({});
        Ok(())
    })
    .unwrap_err();
    let waited = started.elapsed();
    assert!(
        err.downcast_ref::<UpstreamLockTimeout>().is_some(),
        "a held lock is a typed timeout: {err:#}"
    );
    assert!(err.to_string().contains("~/.clauth/.lock"), "{err}");
    assert!(
        waited >= Duration::from_millis(150) && waited < Duration::from_secs(3),
        "the wait is bounded by the timeout, got {waited:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "nothing written");
}

#[test]
fn upstream_lock_is_never_created_and_is_a_no_op_outside_guest_mode() {
    let home = HomeSandbox::new();
    let lock = home
        .home()
        .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
        .join(UPSTREAM_LOCK_FILE);
    drop(upstream_lock().expect("not a guest: nothing to take"));
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    assert!(crate::identity::upstream_active());
    drop(upstream_lock().expect("an absent lock has no writer to wait for"));
    assert!(!lock.exists(), "upstream's lock file is never created");
}

#[cfg(unix)]
#[test]
fn atomic_replace_keeps_the_mode_and_writes_through_a_symlink() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    let real = home.home().join("real.json");
    std::fs::write(&real, "{}").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    let link = home.home().join("link.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    atomic_replace(&link, b"{\"a\":1}").unwrap();
    assert!(
        link.symlink_metadata().unwrap().file_type().is_symlink(),
        "the link survives"
    );
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "{\"a\":1}");
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640
    );

    let dangling = home.home().join("dangling.json");
    std::os::unix::fs::symlink(home.home().join("nowhere"), &dangling).unwrap();
    assert!(
        atomic_replace(&dangling, b"{}").is_err(),
        "a dangling link is refused"
    );
}

// ── the wired call sites ─────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn guest_wire_adds_mcp_servers_tollgate_and_keeps_upstreams_keys_and_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let path = home.home().join(".claude.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = foreign_views(home.home());

    crate::plugin_probe::wire_mcp_server().expect("guest mode wires tollgate's own entry");

    let after = read(&path);
    assert_eq!(
        after["mcpServers"]["tollgate"],
        json!({"type": "stdio", "command": "tollgate", "args": ["mcp"]})
    );
    assert_eq!(
        foreign_views(home.home()),
        before,
        "upstream's keys never moved"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "the file keeps its mode"
    );
}

#[test]
fn guest_wire_refuses_to_replace_a_non_object_mcp_servers() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let path = home.home().join(".claude.json");
    std::fs::write(&path, r#"{"mcpServers":"hand-edited"}"#).unwrap();
    let err = crate::plugin_probe::wire_mcp_server().unwrap_err();
    assert!(err.to_string().contains("mcpServers"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"{"mcpServers":"hand-edited"}"#
    );
}

#[test]
fn guest_wire_waits_on_upstreams_lock() {
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let path = home.home().join(".claude.json");
    let bytes = std::fs::read(&path).unwrap();
    let _held = hold_upstream_lock(home.home());
    let _fast = upstream_lock_timeout_for_test(Duration::from_millis(100));
    let err = crate::plugin_probe::wire_mcp_server().unwrap_err();
    assert!(
        err.downcast_ref::<UpstreamLockTimeout>().is_some(),
        "{err:#}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

/// The Plugin tab's install in guest mode, through the fake `claude` in its
/// worst case: a CLI that rewrites `settings.json` and both registries from
/// scratch. The install lands, tollgate's rows are there, and every upstream
/// key is back.
#[cfg(unix)]
#[test]
fn guest_plugin_install_adds_tollgates_rows_and_restores_upstreams() {
    use crate::testutil::{ConfigDirSandbox, EnvPin, FakeClaude};
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let claude = home.home().join(".claude");
    let _config = ConfigDirSandbox::new(&home, &claude);
    let fake = FakeClaude::new(&home);
    let _clobber = EnvPin::new(&home, &[("CLAUDE_SHIM_CLOBBER", Some("1".as_ref()))]);
    let before = foreign_views(home.home());

    let outcome = crate::plugin_host::install().expect("guest mode installs tollgate's plugin");
    assert!(
        matches!(outcome, agentgear::Outcome::Installed),
        "{outcome}"
    );
    assert!(
        fake.log()
            .lines()
            .any(|l| l == "plugin install tollgate@tollgate --scope user"),
        "the CLI install ran: {}",
        fake.log()
    );

    assert_eq!(
        foreign_views(home.home()),
        before,
        "every upstream key survives the CLI's rewrite"
    );
    let installed = read(&claude.join("plugins").join("installed_plugins.json"));
    assert!(installed["plugins"]["tollgate@tollgate"].is_array());
    assert_eq!(installed["version"], json!(2));
    assert_eq!(
        read(&claude.join("settings.json"))["enabledPlugins"]["tollgate@tollgate"],
        json!(true)
    );
    assert!(read(&claude.join("plugins").join("known_marketplaces.json")).contains_key("tollgate"));
}

#[cfg(unix)]
#[test]
fn guest_plugin_install_holds_upstreams_lock_and_runs_nothing_while_it_is_held() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude};
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let claude = home.home().join(".claude");
    let _config = ConfigDirSandbox::new(&home, &claude);
    let fake = FakeClaude::new(&home);
    let _held = hold_upstream_lock(home.home());
    let _fast = upstream_lock_timeout_for_test(Duration::from_millis(100));

    let err = crate::plugin_host::install().unwrap_err();
    assert!(
        err.downcast_ref::<UpstreamLockTimeout>().is_some(),
        "{err:#}"
    );
    assert!(fake.log().is_empty(), "no `claude` ran: {}", fake.log());
}

#[cfg(unix)]
#[test]
fn guest_plugin_install_refuses_a_session_config_dir() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude};
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let runtime = home
        .home()
        .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
        .join("profiles/personal/runtime-1-0");
    std::fs::create_dir_all(&runtime).unwrap();
    let _config = ConfigDirSandbox::new(&home, &runtime);
    let fake = FakeClaude::new(&home);

    let err = crate::plugin_host::install().unwrap_err();
    assert!(err.to_string().contains("CLAUDE_CONFIG_DIR"), "{err}");
    assert!(fake.log().is_empty(), "no `claude` ran: {}", fake.log());
}

/// Outside guest mode the guard is a pass-through: no snapshot, no restore,
/// the child's writes stand exactly as it made them.
#[cfg(unix)]
#[test]
fn non_guest_install_is_unguarded() {
    use crate::testutil::{ConfigDirSandbox, EnvPin, FakeClaude};
    let home = HomeSandbox::new();
    let claude = home.home().join(".claude");
    std::fs::create_dir_all(claude.join("plugins")).unwrap();
    std::fs::write(claude.join("settings.json"), UPSTREAM_SETTINGS).unwrap();
    let _config = ConfigDirSandbox::new(&home, &claude);
    let _fake = FakeClaude::new(&home);
    let _clobber = EnvPin::new(&home, &[("CLAUDE_SHIM_CLOBBER", Some("1".as_ref()))]);
    assert!(!crate::identity::upstream_active());

    crate::plugin_host::install().expect("install");
    assert_eq!(
        Value::Object(read(&claude.join("settings.json"))),
        json!({"enabledPlugins": {"tollgate@tollgate": true}}),
        "no guard outside guest mode"
    );
}

// ── the hooks ────────────────────────────────────────────────────────────

#[test]
fn hooks_stand_down_only_in_a_foreign_session_in_guest_mode() {
    use crate::testutil::ConfigDirSandbox;
    let home = HomeSandbox::new();
    let own = home
        .home()
        .join(crate::identity::DATA_DIR_NAME)
        .join("profiles/work/runtime-1-0");
    let upstream = home
        .home()
        .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
        .join("profiles/personal/runtime-1-0");
    std::fs::create_dir_all(&own).unwrap();

    assert!(
        !crate::identity::hook_stands_down(),
        "not a guest: every session is ours"
    );
    std::fs::create_dir_all(&upstream).unwrap();
    assert!(crate::identity::upstream_active());
    assert!(
        crate::identity::hook_stands_down(),
        "a bare `claude` in guest mode is upstream's session"
    );
    {
        let _dir = ConfigDirSandbox::new(&home, &upstream);
        assert!(crate::identity::hook_stands_down(), "upstream's runtime");
        assert_eq!(
            crate::identity::plugin_target(),
            crate::identity::PluginTarget::Foreign(upstream.clone())
        );
    }
    {
        let _dir = ConfigDirSandbox::new(&home, &own);
        assert!(!crate::identity::hook_stands_down(), "a tollgate runtime");
        assert!(crate::identity::in_own_session());
        assert_eq!(
            crate::identity::plugin_target(),
            crate::identity::PluginTarget::OwnRuntime(own.clone())
        );
    }
    {
        let dotdot = home
            .home()
            .join(crate::identity::DATA_DIR_NAME)
            .join("..")
            .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
            .join("profiles/personal/runtime-1-0");
        let _dir = ConfigDirSandbox::new(&home, &dotdot);
        assert!(
            crate::identity::hook_stands_down(),
            "a `..` out of ~/.tollgate is not a tollgate runtime"
        );
    }
    {
        let _dir = ConfigDirSandbox::new(&home, &home.home().join(".claude"));
        assert_eq!(
            crate::identity::plugin_target(),
            crate::identity::PluginTarget::Home
        );
    }
}

/// The `self-heal` hook in an upstream session: nothing runs, nothing moves.
/// The fake `claude` is staged anyway, so a broken gate would show up in its
/// log instead of reaching the operator's real CLI.
#[cfg(unix)]
#[test]
fn self_heal_hook_is_a_no_op_in_an_upstream_session() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude};
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let upstream = home
        .home()
        .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
        .join("profiles/personal/runtime-1-0");
    std::fs::create_dir_all(&upstream).unwrap();
    let _dir = ConfigDirSandbox::new(&home, &upstream);
    let fake = FakeClaude::new(&home);
    let before: Vec<_> = shared_files(home.home())
        .iter()
        .map(|(p, _)| std::fs::read(p).unwrap())
        .collect();

    crate::plugin_host::self_heal().expect("a silent no-op");
    assert!(crate::plugin_host::self_heal_line().unwrap().is_none());
    crate::plugin_host::heal_detached();
    crate::plugin_host::preflight();

    assert!(fake.log().is_empty(), "no `claude` ran: {}", fake.log());
    let after: Vec<_> = shared_files(home.home())
        .iter()
        .map(|(p, _)| std::fs::read(p).unwrap())
        .collect();
    assert_eq!(after, before, "nothing written");
}

/// The same hook in a tollgate guest session heals: the registration the
/// session's own config dir holds converges through the guarded CLI.
#[cfg(unix)]
#[test]
fn self_heal_hook_runs_in_a_tollgate_guest_session() {
    use crate::testutil::{ConfigDirSandbox, FakeClaude};
    let home = HomeSandbox::new();
    stage_upstream(home.home());
    let own = home
        .home()
        .join(crate::identity::DATA_DIR_NAME)
        .join("profiles/work/runtime-1-0");
    std::fs::create_dir_all(own.join("plugins")).unwrap();
    let _dir = ConfigDirSandbox::new(&home, &own);
    let fake = FakeClaude::new(&home);

    crate::plugin_host::self_heal().expect("heal");
    assert!(
        fake.log().lines().any(|l| l == "plugin list --json"),
        "the heal probed the registration: {}",
        fake.log()
    );
}
