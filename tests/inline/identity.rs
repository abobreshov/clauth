use super::*;

#[test]
fn the_fork_never_shares_a_global_name_with_upstream() {
    assert_ne!(NAME, UPSTREAM_NAME);
    assert_ne!(DATA_DIR_NAME, UPSTREAM_DATA_DIR_NAME);
    assert_ne!(HERDR_PLUGIN_ID, UPSTREAM_HERDR_PLUGIN_ID);
    assert_ne!(CC_PLUGIN, UPSTREAM_CC_PLUGIN);
    assert_ne!(DEFAULT_LISTEN, "0.0.0.0:8443", "upstream's port");
    assert!(!ENV_PREFIX.starts_with("CLAUTH"));
    assert_ne!(
        API_KEY_HELPER_SUBCMD, "__api-key",
        "upstream's helper token"
    );
    assert!(
        !RELEASE_TAG_PREFIX.starts_with('v'),
        "upstream's tags are bare `v*`"
    );
}

#[test]
fn the_herdr_names_derive_from_the_plugin_id() {
    assert_eq!(HERDR_OPEN_ACTION, format!("{HERDR_PLUGIN_ID}.open"));
    assert_eq!(HERDR_TOKEN, format!("${HERDR_PLUGIN_ID}"));
    assert_eq!(
        HERDR_DELEGATE_TOKEN_KEY,
        format!("{HERDR_PLUGIN_ID}_delegate")
    );
    assert_eq!(HERDR_DELEGATE_TOKEN, format!("${HERDR_DELEGATE_TOKEN_KEY}"));
    assert_eq!(HERDR_CONFIG_MARKER, format!("# {NAME} herdr plugin"));
    assert_eq!(REPO_GIT_URL, format!("https://github.com/{REPO_SLUG}.git"));
    assert_eq!(RELEASE_TAG_PREFIX, format!("{NAME}-v"));
    assert_eq!(API_KEY_HELPER_SUBCMD, format!("__{NAME}-api-key"));
}

#[test]
fn the_identity_agrees_with_the_manifest() {
    assert_eq!(NAME, env!("CARGO_PKG_NAME"));
    assert_eq!(DATA_DIR_NAME, format!(".{NAME}"));
    assert_eq!(ENV_PREFIX, format!("{}_", NAME.to_ascii_uppercase()));
    assert_eq!(CC_PLUGIN, format!("{NAME}@{NAME}"));
    assert!(HERDR_GITHUB_SOURCE.starts_with(&format!("{REPO_SLUG}/")));
}

#[test]
fn the_repo_slug_splits_into_owner_and_name() {
    assert_eq!(repo_owner(), "abobreshov");
    assert_eq!(repo_name(), "clauth");
    assert_eq!(format!("{}/{}", repo_owner(), repo_name()), REPO_SLUG);
}

/// I4: guest mode refuses mutations of the DEFAULT `Claude Code-credentials`
/// Keychain item (upstream clauth's, like `~/.claude/.credentials.json`) and
/// leaves a per-session namespaced item writable. Pinned here because the
/// Keychain module that asks it compiles on macOS only.
#[test]
fn guest_mode_refuses_only_the_default_keychain_item() {
    let home = crate::testutil::HomeSandbox::new();
    let default = crate::claude::CLAUDE_KEYCHAIN_SERVICE;
    let namespaced =
        crate::claude::namespaced_keychain_service(std::path::Path::new("/tmp/session-runtime"));

    assert!(
        !guest_refuses_keychain_service(default),
        "no upstream install: the default item is ours to write"
    );

    std::fs::create_dir_all(home.home().join(UPSTREAM_DATA_DIR_NAME)).expect("stage ~/.clauth");
    assert!(
        guest_refuses_keychain_service(default),
        "guest mode: the default item is upstream's"
    );
    assert!(
        !guest_refuses_keychain_service(&namespaced),
        "a per-session namespaced item stays writable in guest mode"
    );
    assert!(!guest_refuses_keychain_service("some-other-service"));
}
