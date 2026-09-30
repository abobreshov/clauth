#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::process::{Command, Stdio};

/// Integration tests cannot access the binary crate's testutil module. This
/// sandbox gives each child a temporary home without mutating the parent env.
struct HomeSandbox(tempfile::TempDir);
impl HomeSandbox {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tollgate"));
        command
            .env_clear()
            .env("HOME", self.0.path())
            .stdin(Stdio::null());
        command
    }
}
#[test]
fn monitor_add_prints_note_when_key_missing_from_environment_and_store() {
    let home = HomeSandbox::new();
    let output = home
        .command()
        .args([
            "monitor",
            "add",
            "missing-key",
            "--kind",
            "openrouter",
            "--api-key-env",
            "MISSING_TEST_KEY",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.lines().any(|line|line=="note: $MISSING_TEST_KEY is not set in the environment or the store; run 'tollgate secret set MISSING_TEST_KEY'"),"{text}");
}
#[test]
fn monitor_add_does_not_print_missing_note_when_key_is_stored() {
    use std::os::unix::fs::PermissionsExt;
    let home = HomeSandbox::new();
    let dir = home.0.path().join(".tollgate");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = dir.join("secrets.env");
    std::fs::write(&store, "STORED_TEST_KEY=STORE-CANARY\n").unwrap();
    std::fs::set_permissions(store, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = home
        .command()
        .args([
            "monitor",
            "add",
            "stored-key",
            "--kind",
            "openrouter",
            "--api-key-env",
            "STORED_TEST_KEY",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        !text.contains("not set in the environment or the store"),
        "{text}"
    );
    assert!(!text.contains("STORE-CANARY"));
}
