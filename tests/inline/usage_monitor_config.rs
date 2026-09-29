#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `usage::monitor::config`: `monitors.toml` parse + validation, the secret
//! refusals, and the in-place `add` / `remove` edits (0600, comments kept).

use super::*;
use crate::testutil::HomeSandbox;

const FULL: &str = r#"
# my monitors
[[monitor]]
id = "nous"
kind = "nous"
label = "Nous (Hermes)"
budget_usd_month = "20"
alert_pct = 80

[[monitor]]
id = "or-billing"
kind = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
billing_key_env = "OPENROUTER_MGMT_KEY"
ttl_secs = 300

[[monitor]]
id = "ds"
kind = "provider"
provider = "DeepSeek"
api_key_env = "DEEPSEEK_API_KEY"
enabled = false

[[monitor]]
id = "oc"
kind = "ollama_cloud"
api_key_env = "OLLAMA_API_KEY"
"#;

#[test]
fn parses_every_kind_with_defaults() {
    let m = parse(FULL).unwrap();
    assert_eq!(m.len(), 4);
    let nous = &m[0];
    assert_eq!(nous.kind, MonitorKind::Nous);
    assert_eq!(nous.display_label(), "Nous (Hermes)");
    assert!(nous.enabled, "enabled defaults to true");
    assert_eq!(nous.budget_usd_month, Amount::parse("20"));
    assert_eq!(nous.alert_pct, Some(80.0));
    assert_eq!(nous.ttl_ms(), DEFAULT_TTL_SECS * 1000);
    assert_eq!(
        nous.hermes_home_in(std::path::Path::new("/home/u")),
        std::path::PathBuf::from("/home/u/.hermes"),
        "hermes_home defaults to ~/.hermes"
    );
    assert_eq!(m[1].kind, MonitorKind::OpenRouter);
    assert_eq!(m[1].typed_provider(), Some(Provider::OpenRouter));
    assert_eq!(m[1].ttl_ms(), 300_000);
    assert_eq!(m[2].typed_provider(), Some(Provider::DeepSeek));
    assert!(!m[2].enabled);
    assert_eq!(m[3].kind, MonitorKind::OllamaCloud);
    assert_eq!(m[3].display_label(), "oc", "label falls back to the id");
}

#[test]
fn an_empty_file_is_no_monitors() {
    assert!(parse("").unwrap().is_empty());
    assert!(parse("# nothing yet\n").unwrap().is_empty());
}

#[test]
fn unknown_keys_are_refused_at_both_levels() {
    let top = parse("colour = \"red\"\n").unwrap_err();
    assert!(format!("{top:#}").contains("colour"), "{top:#}");
    let inner = parse("[[monitor]]\nid = \"a\"\nkind = \"nous\"\nlable = \"typo\"\n").unwrap_err();
    assert!(format!("{inner:#}").contains("lable"), "{inner:#}");
}

#[test]
fn a_secret_valued_key_is_refused_without_echoing_it() {
    for key in ["api_key", "token", "billing_key", "secret"] {
        let text = format!(
            "[[monitor]]\nid = \"or\"\nkind = \"openrouter\"\n{key} = \"sk-or-v1-deadbeefcafe0123456789\"\n"
        );
        let err = format!("{:#}", parse(&text).unwrap_err());
        assert!(err.contains("api_key_env"), "{key}: {err}");
        assert!(
            !err.contains("deadbeef"),
            "{key}: the refusal must not echo the value: {err}"
        );
    }
}

fn one(extra: &str, kind: &str) -> anyhow::Result<Vec<MonitorConfig>> {
    parse(&format!(
        "[[monitor]]\nid = \"m\"\nkind = \"{kind}\"\n{extra}"
    ))
}

#[test]
fn validation_names_what_is_wrong() {
    let cases: &[(&str, &str, &str)] = &[
        ("", "provider", "needs provider"),
        (
            "provider = \"Nope\"\napi_key_env = \"K\"",
            "provider",
            "unknown provider",
        ),
        (
            "provider = \"Alibaba\"\napi_key_env = \"K\"",
            "provider",
            "console session",
        ),
        ("provider = \"DeepSeek\"", "nous", "only valid with kind"),
        (
            "hermes_home = \"/h\"\napi_key_env = \"K\"",
            "openrouter",
            "hermes_home",
        ),
        ("hermes_home = \"relative/h\"", "nous", "absolute"),
        ("", "openrouter", "needs api_key_env"),
        ("budget_usd_month = \"0\"", "nous", "above zero"),
        ("budget_usd_month = -5", "nous", "above zero"),
        ("alert_pct = 0", "nous", "alert_pct"),
        ("alert_pct = 150", "nous", "alert_pct"),
        ("ttl_secs = 5", "nous", "ttl_secs"),
        ("api_key_env = \"sk-or-v1-abc\"", "openrouter", "NAME"),
        ("label = \"\"", "nous", "label"),
    ];
    for (extra, kind, want) in cases {
        let err = format!("{:#}", one(extra, kind).unwrap_err());
        assert!(
            err.contains(want),
            "{kind} {extra:?}: want {want:?} in {err}"
        );
    }
}

#[test]
fn ids_are_file_safe_and_unique() {
    for bad in ["", "Upper", "-lead", "a/b", "a.b", "../x", &"x".repeat(49)] {
        assert!(validate_id(bad).is_err(), "{bad:?}");
    }
    for good in ["a", "nous", "or-billing", "ds_2"] {
        validate_id(good).unwrap();
    }
    let dup =
        "[[monitor]]\nid = \"a\"\nkind = \"nous\"\n[[monitor]]\nid = \"a\"\nkind = \"nous\"\n";
    assert!(format!("{:#}", parse(dup).unwrap_err()).contains("duplicate"));
}

#[test]
fn env_names_are_names_never_values() {
    for good in ["OPENROUTER_API_KEY", "NOUS_API_KEY", "_X", "key2"] {
        validate_env_name(good).unwrap();
    }
    for bad in [
        "sk-or-v1-0123456789abcdef",
        "has space",
        "1LEAD",
        "",
        "abcDEF0123ghiJKL4567mnoPQR89",
        &"A".repeat(65),
    ] {
        assert!(validate_env_name(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn providers_parse_by_variant_or_display_name() {
    assert_eq!(parse_provider("deepseek"), Some(Provider::DeepSeek));
    assert_eq!(parse_provider("Z.ai"), Some(Provider::Zai));
    assert_eq!(parse_provider("zai"), Some(Provider::Zai));
    assert_eq!(parse_provider("MINIMAX"), Some(Provider::MiniMax));
    assert_eq!(parse_provider("OpenRouter"), Some(Provider::OpenRouter));
    assert_eq!(parse_provider("anthropic"), None);
}

#[test]
fn the_fingerprint_moves_with_the_target_not_the_label() {
    let a = MonitorConfig::new("m", MonitorKind::Nous);
    let mut b = a.clone();
    b.label = Some("renamed".into());
    b.budget_usd_month = Amount::parse("5");
    assert_eq!(a.fingerprint(), b.fingerprint());
    b.hermes_home = Some("/other".into());
    assert_ne!(a.fingerprint(), b.fingerprint());
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn load_is_empty_without_a_file() {
    let _home = HomeSandbox::new();
    assert!(load().unwrap().is_empty());
    assert!(
        !monitors_path().unwrap().exists(),
        "load never creates the file"
    );
}

#[test]
fn add_creates_the_file_owner_only_and_round_trips() {
    let _home = HomeSandbox::new();
    let mut m = MonitorConfig::new("or", MonitorKind::OpenRouter);
    m.api_key_env = Some("OPENROUTER_API_KEY".into());
    m.budget_usd_month = Amount::parse("12.50");
    m.alert_pct = Some(75.0);
    m.ttl_secs = Some(120);
    m.enabled = false;
    add(&m).unwrap();
    #[cfg(unix)]
    assert_eq!(mode(&monitors_path().unwrap()), 0o600);
    let loaded = load().unwrap();
    assert_eq!(loaded, vec![m.clone()]);
    let text = std::fs::read_to_string(monitors_path().unwrap()).unwrap();
    assert!(text.contains("budget_usd_month = \"12.50\""), "{text}");

    let err = format!("{:#}", add(&m).unwrap_err());
    assert!(err.contains("already exists"), "{err}");
}

#[test]
fn add_and_remove_keep_the_operators_comments() {
    let _home = HomeSandbox::new();
    let path = monitors_path().unwrap();
    crate::profile::mkdir_700(path.parent().unwrap()).unwrap();
    std::fs::write(&path, FULL).unwrap();
    let mut m = MonitorConfig::new("extra", MonitorKind::Nous);
    m.hermes_home = Some("~/hermes-work".into());
    add(&m).unwrap();
    assert_eq!(load().unwrap().len(), 5);
    assert!(remove("ds").unwrap());
    assert!(!remove("ds").unwrap(), "a second remove finds nothing");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# my monitors"), "{text}");
    let ids: Vec<String> = load().unwrap().into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["nous", "or-billing", "oc", "extra"]);
}

#[test]
fn removing_the_last_monitor_leaves_a_loadable_file() {
    let _home = HomeSandbox::new();
    add(&MonitorConfig::new("only", MonitorKind::Nous)).unwrap();
    assert!(remove("only").unwrap());
    assert!(load().unwrap().is_empty());
}

#[test]
fn an_invalid_monitor_is_never_written() {
    let _home = HomeSandbox::new();
    let m = MonitorConfig::new("or", MonitorKind::OpenRouter); // no key env
    assert!(add(&m).is_err());
    assert!(!monitors_path().unwrap().exists());
}
