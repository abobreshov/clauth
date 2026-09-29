#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The profile layout and the child home (spec §3, G2a).

use super::*;
use crate::testutil::HomeSandbox;

#[test]
fn the_layout_is_a_sibling_child_home_with_links_only_to_existing_targets() {
    let sb = HomeSandbox::new();
    std::fs::write(sb.home().join(".gitconfig"), "[user]\n").unwrap();
    std::fs::create_dir_all(sb.home().join(".ssh")).unwrap();
    let paths = HermesPaths::for_name("or-main").unwrap();
    assert_eq!(paths.home, paths.profile.join("hermes-home"));
    assert_eq!(paths.child_home, paths.profile.join("child-home"));
    assert!(
        !paths.child_home.starts_with(&paths.home),
        "never inside the Hermes root (D-H18)"
    );
    build_layout(&paths).unwrap();
    assert!(paths.shared.is_dir());
    assert_eq!(
        std::fs::read_link(paths.child_home.join(".gitconfig")).unwrap(),
        sb.home().join(".gitconfig")
    );
    assert_eq!(
        std::fs::read_link(paths.child_home.join(".ssh")).unwrap(),
        sb.home().join(".ssh")
    );
    assert!(
        paths.child_home.join(".config").symlink_metadata().is_err(),
        "no ~/.config/git, so no .config at all"
    );
    assert_eq!(
        audit_child_home(&paths.child_home).unwrap(),
        ChildHomeVerdict::Ok
    );
    // Idempotent, and it backfills a link whose target appeared since.
    std::fs::create_dir_all(sb.home().join(".config/git")).unwrap();
    build_layout(&paths).unwrap();
    assert_eq!(
        std::fs::read_link(paths.child_home.join(".config/git")).unwrap(),
        sb.home().join(".config/git")
    );
    assert_eq!(
        audit_child_home(&paths.child_home).unwrap(),
        ChildHomeVerdict::Ok
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for d in [
            &paths.profile,
            &paths.home,
            &paths.shared,
            &paths.child_home,
        ] {
            assert_eq!(
                std::fs::metadata(d).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}

#[test]
fn the_child_home_audit_names_the_first_foreign_entry() {
    let sb = HomeSandbox::new();
    std::fs::create_dir_all(sb.home().join(".config/git")).unwrap();
    let paths = HermesPaths::for_name("or-main").unwrap();
    assert_eq!(
        audit_child_home(&paths.child_home).unwrap(),
        ChildHomeVerdict::Missing
    );
    build_layout(&paths).unwrap();

    std::fs::create_dir(paths.child_home.join(".claude")).unwrap();
    assert_eq!(
        audit_child_home(&paths.child_home).unwrap(),
        ChildHomeVerdict::Foreign(".claude".into())
    );
    std::fs::remove_dir(paths.child_home.join(".claude")).unwrap();

    std::fs::create_dir(paths.child_home.join(".config/gh")).unwrap();
    assert_eq!(
        audit_child_home(&paths.child_home).unwrap(),
        ChildHomeVerdict::Foreign(".config/gh".into())
    );
    std::fs::remove_dir(paths.child_home.join(".config/gh")).unwrap();

    // A link that points somewhere else is foreign too.
    std::os::unix::fs::symlink("/etc", paths.child_home.join(".ssh")).unwrap();
    assert_eq!(
        audit_child_home(&paths.child_home).unwrap(),
        ChildHomeVerdict::Foreign(".ssh".into())
    );
}

#[test]
fn only_a_crashed_new_leftover_is_adoptable() {
    let sb = HomeSandbox::new();
    let dir = sb.home().join("p");
    assert!(is_adoptable_leftover(&dir).unwrap(), "absent is adoptable");
    std::fs::create_dir_all(dir.join("hermes-home")).unwrap();
    std::fs::create_dir_all(dir.join("child-home")).unwrap();
    std::fs::create_dir_all(dir.join("sessions-1-0")).unwrap();
    assert!(is_adoptable_leftover(&dir).unwrap());
    std::fs::write(dir.join("credentials.json"), "{}").unwrap();
    assert!(!is_adoptable_leftover(&dir).unwrap());
}

/// Test 38: the perms sweep stops at the Hermes home and the child home. An
/// exec-bit file in `hermes-home/plugins` keeps its mode, and nothing is
/// chmod'ed through the child home's links.
#[test]
fn perms_sweep_stops_at_hermes_home_threshold() {
    use std::os::unix::fs::PermissionsExt;
    let sb = HomeSandbox::new();
    let ssh = sb.home().join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(ssh.join("config"), "Host x\n").unwrap();
    std::fs::set_permissions(ssh.join("config"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let paths = HermesPaths::for_name("or-main").unwrap();
    build_layout(&paths).unwrap();
    let tool = paths.home.join("plugins/tool/run.sh");
    std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&paths.home, std::fs::Permissions::from_mode(0o755)).unwrap();

    crate::profile::enforce_tollgate_perms(&crate::profile::tollgate_dir().unwrap());
    assert_eq!(
        std::fs::metadata(&tool).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(&paths.home).unwrap().permissions().mode() & 0o777,
        0o700,
        "the node itself keeps the invariant"
    );
    assert_eq!(
        std::fs::metadata(ssh.join("config"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644,
        "the operator's ~/.ssh is never touched through the link"
    );
    assert!(
        crate::testutil::owner_only_violations(&crate::profile::tollgate_dir().unwrap()).is_empty(),
        "the test twin stops at the same thresholds"
    );
    assert!(is_perms_threshold(&paths.home) && is_perms_threshold(&paths.child_home));
    assert!(!is_perms_threshold(&sb.home().join("hermes-home")));
}
