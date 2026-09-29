#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::testutil::{EnvPin, HomeSandbox};
use clap::Parser;

#[test]
fn secret_set_takes_no_value_positional() {
    assert!(
        crate::cli::Cli::try_parse_from(["tollgate", "secret", "set", "TEST_KEY", "CANARY"])
            .is_err()
    );
}
#[test]
fn secret_name_with_equals_is_a_usage_error() {
    assert!(
        validate_name("KEY=value")
            .unwrap_err()
            .downcast_ref::<crate::UsageError>()
            .is_some()
    );
}
#[test]
fn secret_names_follow_monitor_env_name_rules() {
    for name in [
        "PATH",
        "TOLLGATE_KEY",
        "ANTHROPIC_API_KEY",
        "2KEY",
        "BAD-NAME",
    ] {
        assert!(validate_name(name).is_err(), "{name}");
    }
    assert!(validate_name("OPENAI_API_KEY").is_ok());
}
#[test]
fn values_with_whitespace_or_control_chars_are_refused() {
    for value in ["", "a b", "a\nb", "a\0b", "a\tb"] {
        assert!(validate_value(value).is_err());
    }
    assert!(validate_value(&"a".repeat(4097)).is_err());
}
#[test]
fn secret_file_is_0600_atomic_and_locked() {
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("TEST_KEY".into(), Secret::new("CANARY"));
        Ok(())
    })
    .unwrap();
    let dir = crate::profile::tollgate_dir().unwrap();
    assert!(dir.join(".secrets.lock").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let before = std::fs::metadata(dir.join("secrets.env")).unwrap();
        assert_eq!(before.mode() & 0o777, 0o600);
        edit(|v| {
            v.insert("OTHER_KEY".into(), Secret::new("OTHER"));
            Ok(())
        })
        .unwrap();
        assert_ne!(
            before.ino(),
            std::fs::metadata(dir.join("secrets.env")).unwrap().ino()
        );
    }
    assert_eq!(load().unwrap()["TEST_KEY"].expose(), "CANARY");
}
#[test]
fn secret_bad_line_loads_nothing() {
    let _home = HomeSandbox::new();
    let dir = crate::profile::tollgate_dir().unwrap();
    crate::profile::mkdir_700(&dir).unwrap();
    crate::profile::atomic_write_600(&dir.join("secrets.env"), "TEST_KEY=CANARY\nbad line\n")
        .unwrap();
    assert!(load().is_err());
    assert!(stored_names().is_empty());
}
#[test]
#[cfg(unix)]
fn secret_load_refuses_group_readable_symlink_and_hardlink() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("TEST_KEY".into(), Secret::new("CANARY"));
        Ok(())
    })
    .unwrap();
    let dir = crate::profile::tollgate_dir().unwrap();
    let path = dir.join("secrets.env");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(load().is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&path, dir.join("link")).unwrap();
    assert!(load().is_err());
    std::fs::remove_file(dir.join("link")).unwrap();
    std::fs::rename(&path, dir.join("real")).unwrap();
    symlink(dir.join("real"), &path).unwrap();
    assert!(load().is_err());
}
#[test]
fn env_wins_over_store_by_default() {
    let _home = HomeSandbox::new();
    let _env = EnvPin::new(
        &_home,
        &[
            ("LANE4_TEST_KEY", Some(std::ffi::OsStr::new("ENV-CANARY"))),
            ("TOLLGATE_PREFER_STORE", None),
        ],
    );
    configure_prefer_store(false);
    edit(|v| {
        v.insert("LANE4_TEST_KEY".into(), Secret::new("STORE-CANARY"));
        Ok(())
    })
    .unwrap();
    assert_eq!(resolve("LANE4_TEST_KEY").as_deref(), Some("ENV-CANARY"));
    configure_prefer_store(true);
    assert_eq!(resolve("LANE4_TEST_KEY").as_deref(), Some("STORE-CANARY"));
    configure_prefer_store(false);
}
#[test]
fn stored_names_join_all_scrub_lists() {
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("LANE4_TEST_KEY".into(), Secret::new("STORE-CANARY"));
        Ok(())
    })
    .unwrap();
    assert!(
        crate::providers::billing_key::referenced_env_vars().contains(&"LANE4_TEST_KEY".to_owned())
    );
    assert!(
        crate::providers::billing_key::monitoring_only_env_vars()
            .contains(&"LANE4_TEST_KEY".to_owned())
    );
    let cmd = crate::providers::billing_key::helper_command("env");
    assert!(
        cmd.get_envs()
            .any(|(name, value)| name == "LANE4_TEST_KEY" && value.is_none())
    );
}
#[test]
fn the_lookup_reloads_a_changed_store() {
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("LANE4_CHANGED_KEY".into(), Secret::new("FIRST"));
        Ok(())
    })
    .unwrap();
    assert_eq!(resolve("LANE4_CHANGED_KEY").as_deref(), Some("FIRST"));
    edit(|v| {
        v.insert("LANE4_CHANGED_KEY".into(), Secret::new("SECOND"));
        Ok(())
    })
    .unwrap();
    assert_eq!(resolve("LANE4_CHANGED_KEY").as_deref(), Some("SECOND"));
}
#[test]
fn the_confirmation_vendor_prefix_contains_no_key_specific_characters() {
    assert_eq!(prefix("sk-proj-CANARY"), "sk-proj-");
    assert_eq!(prefix("unknown-CANARY"), "");
}

#[test]
fn secret_set_non_tty_needs_stdin() {
    if std::io::stdin().is_terminal() {
        return;
    }
    let error = dispatch(SecretCommand::Set {
        name: "TEST_KEY".into(),
        stdin: false,
        force: false,
    })
    .unwrap_err();
    assert!(error.downcast_ref::<crate::UsageError>().is_some());
    assert_eq!(
        error.to_string(),
        "tollgate: no terminal to prompt on; pipe the value with --stdin"
    );
}
#[test]
fn prefer_store_env_lets_store_win() {
    let home = HomeSandbox::new();
    let _env = EnvPin::new(
        &home,
        &[
            ("LANE4_TEST_KEY", Some(std::ffi::OsStr::new("ENV-CANARY"))),
            ("TOLLGATE_PREFER_STORE", Some(std::ffi::OsStr::new("1"))),
        ],
    );
    edit(|v| {
        v.insert("LANE4_TEST_KEY".into(), Secret::new("STORE-CANARY"));
        Ok(())
    })
    .unwrap();
    assert_eq!(resolve("LANE4_TEST_KEY").as_deref(), Some("STORE-CANARY"));
}
#[test]
#[cfg(unix)]
fn secret_load_refuses_a_group_writable_directory() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("TEST_KEY".into(), Secret::new("CANARY"));
        Ok(())
    })
    .unwrap();
    let dir = crate::profile::tollgate_dir().unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(load().is_err());
}
#[test]
#[cfg(unix)]
fn stored_values_never_enter_a_child_environment() {
    let home = HomeSandbox::new();
    let _env = EnvPin::new(
        &home,
        &[("LANE4_CHILD_KEY", Some(std::ffi::OsStr::new("ENV-CANARY")))],
    );
    edit(|v| {
        v.insert("LANE4_CHILD_KEY".into(), Secret::new("STORE-CANARY"));
        Ok(())
    })
    .unwrap();
    let output = crate::providers::billing_key::helper_command("env")
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(!output.contains("LANE4_CHILD_KEY="));
    assert!(!output.contains("STORE-CANARY"));
}

#[test]
#[cfg(unix)]
fn secret_load_refuses_foreign_owner_metadata() {
    use std::os::unix::fs::MetadataExt;
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("TEST_KEY".into(), Secret::new("CANARY"));
        Ok(())
    })
    .unwrap();
    let meta =
        std::fs::metadata(crate::profile::tollgate_dir().unwrap().join("secrets.env")).unwrap();
    assert!(
        check_metadata_for_uid(&meta, false, meta.uid().wrapping_add(1))
            .unwrap_err()
            .to_string()
            .contains("owner")
    );
}

#[test]
fn secret_list_prints_names_only() {
    let home = HomeSandbox::new();
    let _env = EnvPin::new(&home, &[("LANE4_LIST_KEY", None)]);
    edit(|v| {
        v.insert("LANE4_LIST_KEY".into(), Secret::new("STORE-CANARY"));
        Ok(())
    })
    .unwrap();
    assert_eq!(
        list_output(false).unwrap(),
        "LANE4_LIST_KEY  store  used by: nothing"
    );
    assert_eq!(list_output(true).unwrap(), "[\"LANE4_LIST_KEY\"]");
}
#[test]
fn secret_rm_names_referencing_monitors() {
    let home = HomeSandbox::new();
    let _env = EnvPin::new(&home, &[("LANE4_MONITOR_KEY", None)]);
    let mut monitor = crate::usage::monitor::config::MonitorConfig::new(
        "openai",
        crate::usage::monitor::config::MonitorKind::Openai,
    );
    monitor.api_key_env = Some("LANE4_MONITOR_KEY".into());
    crate::usage::monitor::config::add(&monitor).unwrap();
    edit(|v| {
        v.insert("LANE4_MONITOR_KEY".into(), Secret::new("STORE-CANARY"));
        Ok(())
    })
    .unwrap();
    assert_eq!(users("LANE4_MONITOR_KEY"), ["openai"]);
    assert!(list_output(false).unwrap().contains("used by: openai"));
    dispatch(SecretCommand::Rm {
        name: "LANE4_MONITOR_KEY".into(),
        yes: true,
    })
    .unwrap();
    assert!(load().unwrap().is_empty());
}
#[test]
fn the_scrub_name_scan_does_not_allocate_credentials() {
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("TEST_KEY".into(), Secret::new("STORE-CANARY"));
        Ok(())
    })
    .unwrap();
    let entries = load_entries(false).unwrap();
    assert_eq!(entries["TEST_KEY"].expose(), "");
    assert_eq!(stored_names(), ["TEST_KEY"]);
}

#[test]
fn lazy_cache_reloads_external_changes_on_poll_scan() {
    let home = HomeSandbox::new();
    let _env = EnvPin::new(&home, &[("LANE4_CACHE_KEY", None)]);
    edit(|v| {
        v.insert("LANE4_CACHE_KEY".into(), Secret::new("FIRST"));
        Ok(())
    })
    .unwrap();
    assert_eq!(resolve("LANE4_CACHE_KEY").as_deref(), Some("FIRST"));
    let path = crate::profile::tollgate_dir().unwrap().join("secrets.env");
    crate::profile::atomic_write_600(&path, "LANE4_CACHE_KEY=SECOND\n").unwrap();
    assert_eq!(
        resolve("LANE4_CACHE_KEY").as_deref(),
        Some("FIRST"),
        "lookups reuse loaded values until the scan"
    );
    reload_if_changed();
    assert_eq!(resolve("LANE4_CACHE_KEY").as_deref(), Some("SECOND"));
    std::fs::remove_file(path).unwrap();
    reload_if_changed();
    assert!(resolve("LANE4_CACHE_KEY").is_none());
}
#[test]
fn cache_is_isolated_by_sandbox_home() {
    {
        let _home = HomeSandbox::new();
        edit(|v| {
            v.insert("LANE4_ISOLATED_KEY".into(), Secret::new("CANARY"));
            Ok(())
        })
        .unwrap();
        assert_eq!(resolve("LANE4_ISOLATED_KEY").as_deref(), Some("CANARY"));
    }
    let _home = HomeSandbox::new();
    assert!(resolve("LANE4_ISOLATED_KEY").is_none());
}
#[test]
#[cfg(unix)]
fn poll_reload_drops_cached_values_after_permissions_become_unsafe() {
    use std::os::unix::fs::PermissionsExt;
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("LANE4_PRIVATE_KEY".into(), Secret::new("CANARY"));
        Ok(())
    })
    .unwrap();
    assert_eq!(resolve("LANE4_PRIVATE_KEY").as_deref(), Some("CANARY"));
    let path = crate::profile::tollgate_dir().unwrap().join("secrets.env");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    reload_if_changed();
    assert!(resolve("LANE4_PRIVATE_KEY").is_none());
}

#[test]
#[cfg(unix)]
fn opened_store_directory_cannot_be_redirected_by_a_symlink_swap() {
    use std::os::unix::fs::symlink;
    let home = HomeSandbox::new();
    edit(|v| {
        v.insert("TEST_KEY".into(), Secret::new("ORIGINAL"));
        Ok(())
    })
    .unwrap();
    let dir = crate::profile::tollgate_dir().unwrap();
    let pinned = open_directory(&dir).unwrap().unwrap();
    let foreign = home.home().join("foreign");
    crate::profile::mkdir_700(&foreign).unwrap();
    crate::profile::atomic_write_600(&foreign.join("secrets.env"), "TEST_KEY=FOREIGN\n").unwrap();
    std::fs::rename(&dir, home.home().join("original")).unwrap();
    symlink(&foreign, &dir).unwrap();
    assert!(load().is_err());
    let values = parse_store(
        open_at(&pinned, c"secrets.env", libc::O_RDONLY).unwrap(),
        true,
    )
    .unwrap();
    assert_eq!(values["TEST_KEY"].expose(), "ORIGINAL");
}
#[test]
#[cfg(unix)]
fn secret_writes_refuse_symlink_and_hardlink_locks() {
    use std::os::unix::fs::symlink;
    let _home = HomeSandbox::new();
    edit(|v| {
        v.insert("TEST_KEY".into(), Secret::new("CANARY"));
        Ok(())
    })
    .unwrap();
    let dir = crate::profile::tollgate_dir().unwrap();
    let lock = dir.join(".secrets.lock");
    let target = dir.join("target");
    crate::profile::atomic_write_600(&target, "UNCHANGED").unwrap();
    std::fs::remove_file(&lock).unwrap();
    symlink(&target, &lock).unwrap();
    assert!(edit(|_| Ok(())).is_err());
    std::fs::remove_file(&lock).unwrap();
    std::fs::hard_link(&target, &lock).unwrap();
    assert!(edit(|_| Ok(())).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "UNCHANGED");
}
