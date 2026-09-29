#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The home `.env` writer (spec §4.2): one managed line, every other line the
//! user's, idempotent, and refusing any file it cannot prove it understands.

use super::*;
use crate::testutil::HomeSandbox;

fn setup(name: &str) -> (HermesPaths, crate::runtime::RotationGuard) {
    let paths = HermesPaths::for_name(name).unwrap();
    crate::profile::mkdir_700(&paths.home).unwrap();
    let guard = crate::runtime::RotationGuard::acquire_with_timeout(
        &crate::profile::ProfileName::from(name),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    (paths, guard)
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Test 9: the managed line leads, every foreign line survives byte-for-byte
/// in order, an earlier binding of the var is dropped, and a second run with
/// the same key leaves the bytes and the mtime alone.
#[test]
fn env_writer_keeps_foreign_lines_and_is_idempotent() {
    let _home = HomeSandbox::new();
    let (paths, guard) = setup("or-main");
    let env = paths.env_file();
    std::fs::write(
        &env,
        "# my notes\nexport FOO=bar\nOPENROUTER_API_KEY=old-key\nTERMINAL_ENV=local # keep\n",
    )
    .unwrap();

    let first = write_key(
        &paths,
        "or-main",
        "OPENROUTER_API_KEY",
        "sk-new",
        None,
        &guard,
    )
    .unwrap();
    assert_eq!(first, Written::Written);
    let text = std::fs::read_to_string(&env).unwrap();
    assert_eq!(
        text,
        "# tollgate: OPENROUTER_API_KEY is managed by 'tollgate hermes key or-main'; other lines \
         are yours\nOPENROUTER_API_KEY=sk-new\n# my notes\nexport FOO=bar\nTERMINAL_ENV=local # keep\n"
    );
    assert_eq!(mode(&env), 0o600);
    assert!(!text.contains('\r') && !text.contains('\u{feff}'));

    let mtime = std::fs::metadata(&env).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = write_key(
        &paths,
        "or-main",
        "OPENROUTER_API_KEY",
        "sk-new",
        None,
        &guard,
    )
    .unwrap();
    assert_eq!(second, Written::Unchanged);
    assert_eq!(std::fs::read_to_string(&env).unwrap(), text);
    assert_eq!(std::fs::metadata(&env).unwrap().modified().unwrap(), mtime);

    // A stale temp from a crashed writer is removed by the next real write.
    std::fs::write(paths.home.join(".env.tollgate-99-0"), "junk").unwrap();
    write_key(
        &paths,
        "or-main",
        "OPENROUTER_API_KEY",
        "sk-newer",
        None,
        &guard,
    )
    .unwrap();
    assert!(!paths.home.join(".env.tollgate-99-0").exists());
    assert_eq!(
        audit_key(&paths, "or-main", "OPENROUTER_API_KEY", None)
            .unwrap()
            .fingerprint,
        fingerprint("sk-newer")
    );
}

/// Test 10: a symlinked `.env` refuses; a value opening a quote it does not
/// close refuses; and a key-name set that disagrees with the projector's
/// dotenv parse refuses, all without touching the file.
#[test]
fn env_writer_refuses_symlink_and_multiline_disagreement() {
    let home = HomeSandbox::new();
    let (paths, guard) = setup("or-main");
    let env = paths.env_file();

    let target = home.home().join("elsewhere.env");
    std::fs::write(&target, "A=1\n").unwrap();
    std::os::unix::fs::symlink(&target, &env).unwrap();
    let err = write_key(&paths, "or-main", "OPENROUTER_API_KEY", "k", None, &guard).unwrap_err();
    assert!(
        err.to_string()
            .contains("not a regular file tollgate can rewrite"),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "A=1\n");
    std::fs::remove_file(&env).unwrap();

    std::fs::write(&env, "CERT=\"-----BEGIN\nMIIB\n-----END\"\n").unwrap();
    let err = write_key(&paths, "or-main", "OPENROUTER_API_KEY", "k", None, &guard).unwrap_err();
    assert!(err.to_string().contains("(multiline values)"), "{err}");

    // Line-wise it looks fine, but the projector's parse disagrees.
    std::fs::write(&env, "A=1\nB=2\n").unwrap();
    let theirs: BTreeSet<String> = ["A".to_string()].into();
    let err = write_key(
        &paths,
        "or-main",
        "OPENROUTER_API_KEY",
        "k",
        Some(&theirs),
        &guard,
    )
    .unwrap_err();
    assert!(err.to_string().contains("(multiline values)"), "{err}");
    assert_eq!(std::fs::read_to_string(&env).unwrap(), "A=1\nB=2\n");

    // Agreement passes.
    let theirs: BTreeSet<String> = ["A".to_string(), "B".to_string()].into();
    write_key(
        &paths,
        "or-main",
        "OPENROUTER_API_KEY",
        "k",
        Some(&theirs),
        &guard,
    )
    .unwrap();
}

/// Test 11: a held session marker on the profile makes the writer refuse.
#[test]
fn env_writer_never_writes_under_a_live_marker() {
    let _home = HomeSandbox::new();
    let (paths, guard) = setup("or-main");
    let _marker = crate::runtime::hold_session_row_marker(
        &crate::profile::ProfileName::from("or-main"),
        false,
        "4242-0",
    )
    .unwrap();
    let err = write_key(&paths, "or-main", "OPENROUTER_API_KEY", "k", None, &guard).unwrap_err();
    assert!(err.to_string().contains("live session"), "{err}");
    assert!(!paths.env_file().exists());
}

#[test]
fn keys_are_refused_when_dotenv_would_read_them_differently() {
    assert_eq!(validate_key("  sk-ok_1.2~  \n").unwrap(), "sk-ok_1.2~");
    for bad in ["", "   ", "a b", "a#b", "a'b", "a\"b", "a\\b", "ключ"] {
        assert!(validate_key(bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn the_fingerprint_is_hermes_format_and_matches_python() {
    // "sha256:" + hashlib.sha256(b"sk-or-v1-tollgate-test-key").hexdigest()[:16]
    assert_eq!(
        fingerprint("sk-or-v1-tollgate-test-key"),
        "sha256:1b43f854967b5ae2"
    );
}

#[test]
fn the_audit_needs_a_nonblank_bound_key() {
    let _home = HomeSandbox::new();
    let (paths, _guard) = setup("or-main");
    let err = audit_key(&paths, "or-main", "OPENROUTER_API_KEY", None).unwrap_err();
    assert!(err.to_string().contains("no OPENROUTER_API_KEY"), "{err}");
    std::fs::write(paths.env_file(), "OPENROUTER_API_KEY=\n").unwrap();
    let err = audit_key(&paths, "or-main", "OPENROUTER_API_KEY", None).unwrap_err();
    assert!(
        err.to_string()
            .contains("run 'tollgate hermes key or-main'"),
        "{err}"
    );
    std::fs::write(
        paths.env_file(),
        "OPENROUTER_API_KEY=a\nexport OPENROUTER_API_KEY='b'\n",
    )
    .unwrap();
    assert_eq!(
        audit_key(&paths, "or-main", "OPENROUTER_API_KEY", None)
            .unwrap()
            .fingerprint,
        fingerprint("b"),
        "the last assignment wins, as in python-dotenv"
    );
}

#[test]
fn the_key_name_scan_matches_the_spec_regex() {
    assert_eq!(line_key("FOO=1"), Some("FOO"));
    assert_eq!(line_key("  export  FOO_2 = 1"), Some("FOO_2"));
    assert_eq!(line_key("exportFOO=1"), Some("exportFOO"));
    assert_eq!(line_key("# FOO=1"), None);
    assert_eq!(line_key("1FOO=1"), None);
    assert_eq!(line_key("FOO"), None);
}
