//! Preset store: the shipped table, the on-disk round-trip, and the two
//! refusals that keep a built-in's slot the binary's alone.

use super::{
    delete_preset, is_builtin, list_presets, load_preset, preset_exists, presets_dir, save_preset,
};
use crate::profile::ModelSettings;

fn models(default: &str) -> ModelSettings {
    ModelSettings {
        default: Some(default.to_string()),
        ..ModelSettings::default()
    }
}

/// The shipped names in menu order, so a table change lands in one place.
const BUILTIN_NAMES: [&str; 9] = [
    "DeepSeek",
    "Z.ai",
    "OpenRouter",
    "MiniMax",
    "Qwen-TokenPlan-Intl",
    "Qwen-TokenPlan-CN",
    "Qwen-CodingPlan-Intl",
    "Qwen-CodingPlan-CN",
    "Ollama-Cloud",
];

fn names(presets: &[super::Preset]) -> Vec<&str> {
    presets.iter().map(|p| p.name.as_str()).collect()
}

/// With nothing on disk the list is exactly the built-ins, in menu order, each
/// carrying the endpoint + base model that makes it worth picking.
#[test]
fn the_builtins_ship_with_their_endpoint_and_base_model() {
    let _home = crate::testutil::HomeSandbox::new();

    let listed = list_presets();
    assert_eq!(names(&listed), BUILTIN_NAMES);
    assert!(listed.iter().all(|p| p.builtin));

    let deepseek = &listed[0];
    assert_eq!(
        deepseek.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic")
    );
    assert_eq!(deepseek.models.default.as_deref(), Some("deepseek-chat"));
    // Only the base model: the per-tier rows stay free for the operator, and
    // `ANTHROPIC_MODEL`'s stand-in (the top-level `model` setting) already
    // covers every alias no override pins.
    assert_eq!(deepseek.models.opus, None);
    assert_eq!(deepseek.models.subagent, None);

    let zai = &listed[1];
    assert_eq!(
        zai.base_url.as_deref(),
        Some("https://api.z.ai/api/anthropic")
    );
    assert_eq!(zai.models.default.as_deref(), Some("glm-5.2"));

    let openrouter = &listed[2];
    assert_eq!(
        openrouter.base_url.as_deref(),
        Some("https://openrouter.ai/api")
    );
    assert_eq!(
        openrouter.models.default.as_deref(),
        Some("openrouter/auto")
    );
    assert_eq!(openrouter.models.opus, None);

    let minimax = &listed[3];
    assert_eq!(
        minimax.base_url.as_deref(),
        Some("https://api.minimax.io/anthropic")
    );
    assert_eq!(minimax.models.default.as_deref(), Some("MiniMax-M3"));
    assert_eq!(minimax.models.opus, None);
}

/// Alibaba's endpoints answer `400 "Model not exist."` for any Claude model id,
/// so the alias an uncovered tier resolves to is a hard failure there rather
/// than a degraded route. Every alibaba preset therefore pins each alias AND the
/// subagent row, not just `models.default` — the deliberate deviation from the
/// presets above.
#[test]
fn an_alibaba_preset_pins_every_alias_and_the_subagent_row() {
    let _home = crate::testutil::HomeSandbox::new();

    for (name, base_url, model) in [
        (
            "Qwen-TokenPlan-Intl",
            "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
            "qwen3.8-max",
        ),
        (
            "Qwen-TokenPlan-CN",
            "https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic",
            "qwen3.8-max",
        ),
        (
            "Qwen-CodingPlan-Intl",
            "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic",
            "qwen3-coder-plus",
        ),
        (
            "Qwen-CodingPlan-CN",
            "https://coding.dashscope.aliyuncs.com/apps/anthropic",
            "qwen3-coder-plus",
        ),
    ] {
        let p = load_preset(name).unwrap_or_else(|| panic!("{name} ships in the binary"));
        assert_eq!(p.base_url.as_deref(), Some(base_url), "{name} endpoint");
        let m = &p.models;
        for (field, got) in [
            ("default", &m.default),
            ("opus", &m.opus),
            ("sonnet", &m.sonnet),
            ("haiku", &m.haiku),
            ("fable", &m.fable),
            ("subagent", &m.subagent),
        ] {
            assert_eq!(
                got.as_deref(),
                Some(model),
                "{name} leaves `{field}` free to resolve to a Claude id the endpoint rejects",
            );
        }
    }
}

#[test]
fn a_saved_preset_round_trips_and_joins_the_list_after_the_builtins() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut m = models("my-model");
    m.fable = Some("claude-fable-5".to_string());
    save_preset(
        "mine",
        &Some("https://api.example/anthropic".to_string()),
        &m,
    )
    .expect("save");

    let loaded = load_preset("mine").expect("the saved preset loads back");
    assert_eq!(loaded.name, "mine");
    assert_eq!(
        loaded.base_url.as_deref(),
        Some("https://api.example/anthropic")
    );
    assert_eq!(loaded.models, m, "every tier survives the json round-trip");
    assert!(!loaded.builtin);

    assert_eq!(
        names(&list_presets()),
        [BUILTIN_NAMES.as_slice(), ["mine"].as_slice()].concat(),
        "built-ins keep the head of the list",
    );
}

/// The whole preset tree lives under `~/.tollgate`, so it is bound by the
/// owner-only invariant — dir 0o700, file 0o600, both
/// born that way rather than repaired later.
#[cfg(unix)]
#[test]
fn preset_file_and_dir_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("mine", &None, &models("m")).expect("save");

    let dir = presets_dir().expect("presets dir");
    let dir_mode = std::fs::metadata(&dir)
        .expect("dir metadata")
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700, "presets dir is owner-only");

    let file_mode = std::fs::metadata(dir.join("mine.json"))
        .expect("file metadata")
        .permissions()
        .mode();
    assert_eq!(file_mode & 0o777, 0o600, "a preset file is owner-only");
}

/// A built-in's name is the binary's, whatever the spelling: a case-folding
/// filesystem would otherwise let `deepseek.json` take the slot on one host and
/// not another.
#[test]
fn a_builtin_name_refuses_both_writes_whatever_its_case() {
    let _home = crate::testutil::HomeSandbox::new();

    assert!(is_builtin("DeepSeek") && is_builtin("deepseek") && is_builtin("Z.AI"));

    let err = save_preset("deepseek", &None, &models("m")).expect_err("built-in name refuses");
    assert!(
        err.to_string().contains("built-in"),
        "the refusal names why: {err}",
    );
    assert!(
        delete_preset("DeepSeek").is_err(),
        "a built-in cannot be deleted either",
    );
    assert!(!preset_exists("deepseek"), "the refused save wrote no file",);
}

/// The name is the file NAME, so anything that could climb out of the presets
/// dir is refused before it ever reaches a `join`.
#[test]
fn a_name_that_is_not_a_bare_filename_is_refused() {
    let _home = crate::testutil::HomeSandbox::new();

    for bad in ["../escape", "a/b", "", ".hidden"] {
        assert!(
            save_preset(bad, &None, &models("m")).is_err(),
            "'{bad}' must not reach a path join",
        );
    }
}

#[test]
fn delete_drops_the_file_and_then_reports_the_miss() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("mine", &None, &models("m")).expect("save");
    assert!(preset_exists("mine"));
    delete_preset("mine").expect("delete");
    assert!(!preset_exists("mine"));
    assert!(load_preset("mine").is_none());
    assert!(
        delete_preset("mine").is_err(),
        "deleting what is already gone is an error, not a silent success",
    );
}

/// One unparseable file must not hide the rest of the store from the picker.
#[test]
fn an_unparseable_preset_is_skipped_not_fatal() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("good", &None, &models("m")).expect("save");
    let dir = presets_dir().expect("presets dir");
    std::fs::write(dir.join("broken.json"), "{ not json").expect("write broken preset");

    assert_eq!(
        names(&list_presets()),
        [BUILTIN_NAMES.as_slice(), ["good"].as_slice()].concat(),
    );
}

/// `save_preset` refuses a built-in's name, so a file in that slot can only be
/// hand-written — and it must not shadow the binary's copy. Both surfaces read
/// the built-in: the list shows one `DeepSeek` carrying the shipped endpoint,
/// and a load by that name never reaches the file.
#[test]
fn a_hand_written_file_cannot_shadow_a_builtins_slot() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("anchor", &None, &models("m")).expect("seed the dir");
    let dir = presets_dir().expect("presets dir");
    std::fs::write(
        dir.join("DeepSeek.json"),
        r#"{"base_url":"https://evil.test","models":{"default":"pwned"}}"#,
    )
    .expect("hand-write into the built-in's slot");

    let listed = list_presets();
    assert_eq!(
        listed.iter().filter(|p| p.name == "DeepSeek").count(),
        1,
        "the shadow file does not add a second entry",
    );
    let deepseek = listed
        .iter()
        .find(|p| p.name == "DeepSeek")
        .expect("built-in listed");
    assert_eq!(
        deepseek.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "the list reads the binary's copy, not the file",
    );
    assert_eq!(
        load_preset("DeepSeek").expect("loads").base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "so does a load by name",
    );
}

/// Ollama Cloud: the root endpoint (CC's `/v1/messages` lands on
/// `https://ollama.com/v1/messages`), no model pinned (the operator picks from
/// `GET /api/tags`), and an env allowlist of telemetry switches that never
/// carries a credential — the key reaches CC through the `apiKeyHelper` only,
/// which is what keeps a same-provider hot swap possible.
#[test]
fn the_ollama_cloud_preset_pins_the_root_endpoint_and_no_model() {
    let _home = crate::testutil::HomeSandbox::new();

    let p = load_preset("ollama-cloud").expect("ships in the binary, any case");
    assert!(p.builtin);
    assert_eq!(p.base_url.as_deref(), Some("https://ollama.com"));
    assert_eq!(
        crate::providers::Provider::from_base_url(p.base_url.as_deref().unwrap()),
        Some(crate::providers::Provider::OllamaCloud),
        "the preset's endpoint is the one the usage integration claims"
    );
    assert_eq!(
        p.models,
        ModelSettings::default(),
        "every slot is the operator's"
    );

    let env: Vec<(&str, &str)> = p
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        env,
        [
            ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
            ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
            ("DISABLE_ERROR_REPORTING", "1"),
        ]
    );
    for preset in list_presets() {
        for key in preset.env.keys() {
            assert!(
                !key.starts_with("ANTHROPIC_"),
                "{} carries {key}: a preset never routes or authenticates via env",
                preset.name
            );
        }
    }
}

/// A custom preset carries no env, whatever its file says.
#[test]
fn a_custom_preset_carries_no_env() {
    let _home = crate::testutil::HomeSandbox::new();

    save_preset("mine", &None, &models("m")).expect("save");
    let dir = presets_dir().expect("presets dir");
    std::fs::write(
        dir.join("sneaky.json"),
        r#"{"models":{},"env":{"ANTHROPIC_API_KEY":"leak"}}"#,
    )
    .expect("hand-write a preset with env");
    assert!(load_preset("sneaky").expect("loads").env.is_empty());
    assert!(
        list_presets()
            .iter()
            .filter(|p| !p.builtin)
            .all(|p| p.env.is_empty())
    );
}

/// Applying a built-in's env to a saved account adds only the keys the account
/// lacks: an operator's own value wins, nothing is removed, and the account's
/// api key and provider follow the endpoint as for any preset.
#[test]
fn a_preset_env_fills_gaps_and_never_overrides_the_account() {
    let _home = crate::testutil::HomeSandbox::new();
    let name = crate::profile::ProfileName::from("oll");
    let mut profile = crate::profile::Profile::new(
        "oll".to_string(),
        None,
        Some("test-key-not-real".to_string()),
    );
    profile
        .env
        .insert("DISABLE_ERROR_REPORTING".to_string(), "0".to_string());
    profile.env.insert("MY_OWN".to_string(), "kept".to_string());
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![profile],
    };

    let preset = load_preset("Ollama-Cloud").expect("built-in");
    crate::actions::edit_profile_preset_with_env(
        &mut config,
        &name,
        preset.base_url.clone(),
        preset.models.clone(),
        &preset.env,
    )
    .expect("applied");

    let p = config.find(&name).expect("still there");
    assert_eq!(p.base_url.as_deref(), Some("https://ollama.com"));
    assert_eq!(p.provider, Some(crate::providers::Provider::OllamaCloud));
    assert_eq!(p.api_key.as_deref(), Some("test-key-not-real"));
    assert_eq!(p.env["DISABLE_ERROR_REPORTING"], "0", "the operator's wins");
    assert_eq!(p.env["MY_OWN"], "kept");
    assert_eq!(p.env["CLAUDE_CODE_ATTRIBUTION_HEADER"], "0");
    assert_eq!(p.env["CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY"], "1");
    assert!(
        !p.env.contains_key("ANTHROPIC_AUTH_TOKEN") && !p.env.contains_key("ANTHROPIC_API_KEY"),
        "the key reaches CC through the apiKeyHelper only"
    );
}
