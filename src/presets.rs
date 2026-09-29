//! Named endpoint + model templates a Setup-tab account can be stamped from.
//!
//! A preset carries what makes an account talk to a given provider: its
//! `base_url`, its [`ModelSettings`], and a few NON-SECRET env switches from
//! [`PRESET_ENV_ALLOWLIST`]. Credentials, any other env, and every fallback
//! knob stay out — those are per-account, and a template that carried them
//! would silently move an api key between accounts. The allowlist is applied
//! on every load, so a hand-edited preset file cannot smuggle
//! `ANTHROPIC_AUTH_TOKEN` (or anything else) onto an account either: api keys
//! reach Claude Code only through the `apiKeyHelper`.
//!
//! The built-ins ship in the binary; the rest live one JSON file per preset
//! under `~/.tollgate/presets/`. The file NAME is the preset name, so it goes
//! through [`crate::actions::validate_profile_name`] (the same charset that
//! bounds a profile directory) before it ever reaches a path.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::profile::{ModelSettings, atomic_write_600, read_json_file, tollgate_dir};

/// A named `base_url` + [`ModelSettings`] template.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) base_url: Option<String>,
    pub(crate) models: ModelSettings,
    /// Env switches stamped onto the account, already filtered through
    /// [`PRESET_ENV_ALLOWLIST`].
    pub(crate) env: BTreeMap<String, String>,
    /// Ships in the binary: never written, never deleted, never overwritten.
    pub(crate) builtin: bool,
}

/// The only env keys a preset may carry: behaviour switches with no secret
/// in them. `CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK` also stops the helper key
/// being sent to api.anthropic.com for the fast-mode org check on a gateway
/// endpoint; gateway model discovery lets `/model` list the gateway's ids
/// (code.claude.com/docs/en/llm-gateway-connect).
/// The last three are what `ollama launch claude` sets for the Ollama Cloud
/// preset (telemetry and survey switches, ollama cmd/launch/claude.go).
pub(crate) const PRESET_ENV_ALLOWLIST: &[&str] = &[
    "CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
    "CLAUDE_CODE_ATTRIBUTION_HEADER",
    "CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY",
    "DISABLE_ERROR_REPORTING",
];

/// `env` with every key outside [`PRESET_ENV_ALLOWLIST`] dropped.
pub(crate) fn allowlisted_env(env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.into_iter()
        .filter(|(k, _)| PRESET_ENV_ALLOWLIST.contains(&k.as_str()))
        .collect()
}

/// On-disk shape of `~/.tollgate/presets/<name>.json`. The name is the file stem,
/// so it is deliberately absent from the body — one spelling, no way for the two
/// to disagree.
#[derive(Debug, Serialize, Deserialize)]
struct PresetFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(default)]
    models: ModelSettings,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
}

/// Per-tier model pins of a shipped preset, beyond what `pin_every_tier`
/// copies from `model`.
struct TierPins {
    opus: Option<&'static str>,
    sonnet: Option<&'static str>,
    haiku: Option<&'static str>,
    fable: Option<&'static str>,
    subagent: Option<&'static str>,
}

const NO_PINS: TierPins = TierPins {
    opus: None,
    sonnet: None,
    haiku: None,
    fable: None,
    subagent: None,
};

/// One entry of the shipped table.
struct Builtin {
    name: &'static str,
    base_url: &'static str,
    /// `models.default` (CC's top-level `model`); `None` leaves it unset.
    model: Option<&'static str>,
    /// Whether `model` is written to every alias and the subagent row, or to
    /// `models.default` alone.
    ///
    /// `default` alone is the lighter template: it is CC's top-level `model`
    /// setting, the fallback every alias resolves through when no per-tier
    /// override covers it, so it leaves the tier rows free for the operator.
    /// That only holds while the endpoint tolerates whatever an uncovered alias
    /// resolves to. Alibaba's does not: `POST /apps/anthropic/v1/messages` with
    /// `claude-sonnet-4-5` or `claude-haiku-4-5-20251001` answers
    /// `400 InvalidParameter "Model not exist."` (measured 2026-08-11 against
    /// `token-plan.ap-southeast-1`), so an alias left unpinned is a hard failure
    /// rather than a degraded route. Alibaba's own `bl config agent --agent
    /// claude-code` writes all of them for the same reason.
    pin_every_tier: bool,
    /// Explicit per-tier pins; a pin here wins over `pin_every_tier`.
    tiers: TierPins,
    /// Env switches, each one on [`PRESET_ENV_ALLOWLIST`].
    env: &'static [(&'static str, &'static str)],
}

/// The built-ins, in menu order.
const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/anthropic",
        model: Some("deepseek-chat"),
        pin_every_tier: false,
        tiers: NO_PINS,
        env: &[],
    },
    Builtin {
        name: "Z.ai",
        base_url: "https://api.z.ai/api/anthropic",
        model: Some("glm-5.2"),
        pin_every_tier: false,
        tiers: NO_PINS,
        env: &[],
    },
    Builtin {
        name: "OpenRouter",
        // OpenRouter's own Claude Code guide pins this exact base URL, so CC's
        // `/v1/messages` lands on `/api/v1/messages` (openrouter.ai docs,
        // "Connect Claude to OpenRouter").
        base_url: "https://openrouter.ai/api",
        // Preset v2 (plan v3.1 §4.7): no `openrouter/auto` default, which can
        // route to non-Anthropic models while the guide says Claude Code is
        // only guaranteed to work with the Anthropic first-party provider.
        // Each tier pins OpenRouter's `~anthropic/...-latest` alias instead
        // (`[1m]` = the 1M-context variant). The key reaches CC only through
        // the `apiKeyHelper`, never `ANTHROPIC_AUTH_TOKEN`.
        model: None,
        pin_every_tier: false,
        tiers: TierPins {
            opus: Some("~anthropic/claude-opus-latest[1m]"),
            sonnet: Some("~anthropic/claude-sonnet-latest[1m]"),
            haiku: Some("~anthropic/claude-haiku-latest"),
            fable: None,
            subagent: Some("~anthropic/claude-opus-latest[1m]"),
        },
        env: &[("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK", "1")],
    },
    Builtin {
        name: "MiniMax",
        // MiniMax fronts the Anthropic-shaped surface at `/anthropic`, so CC's
        // `/v1/messages` lands on `/anthropic/v1/messages` (platform.minimax.io,
        // "Claude Code" integration guide). The China-region host is a separate
        // account and endpoint; only the international one ships, matching what
        // `providers::minimax` claims.
        base_url: "https://api.minimax.io/anthropic",
        model: Some("MiniMax-M3"),
        pin_every_tier: false,
        tiers: NO_PINS,
        env: &[],
    },
    Builtin {
        name: "Qwen-TokenPlan-Intl",
        base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic",
        model: Some("qwen3.8-max"),
        pin_every_tier: true,
        tiers: NO_PINS,
        env: &[],
    },
    Builtin {
        name: "Qwen-TokenPlan-CN",
        base_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic",
        model: Some("qwen3.8-max"),
        pin_every_tier: true,
        tiers: NO_PINS,
        env: &[],
    },
    Builtin {
        name: "Qwen-CodingPlan-Intl",
        base_url: "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic",
        model: Some("qwen3-coder-plus"),
        pin_every_tier: true,
        tiers: NO_PINS,
        env: &[],
    },
    Builtin {
        name: "Qwen-CodingPlan-CN",
        base_url: "https://coding.dashscope.aliyuncs.com/apps/anthropic",
        model: Some("qwen3-coder-plus"),
        pin_every_tier: true,
        tiers: NO_PINS,
        env: &[],
    },
    Builtin {
        name: "Ollama-Cloud",
        // Ollama Cloud serves an Anthropic-compatible `/v1/messages` at the
        // ROOT, so CC's `/v1/messages` lands on `https://ollama.com/v1/messages`
        // (docs.ollama.com/integrations/claude-code). The inference key (minted
        // at ollama.com/settings/keys) is Bearer-only and reaches CC solely via
        // the `apiKeyHelper`; an `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`
        // env entry would pin one key into the session and break a hot swap to
        // another Ollama account.
        base_url: "https://ollama.com",
        // No model pinned: the catalogue moves too fast to ship ids. The live
        // list is the unauthenticated `GET https://ollama.com/api/tags` (or
        // `/v1/models`); the operator picks opus / sonnet / haiku / fable /
        // subagent from it (Setup tab, or `tollgate login <p> --base-url
        // https://ollama.com --api-key` then `--model <id>` / `[models]` in the
        // profile's config.toml).
        model: None,
        // What `ollama launch claude` sets (ollama cmd/launch/claude.go),
        // minus the context-window knob, which depends on the model picked.
        env: &[
            ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
            ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
            ("DISABLE_ERROR_REPORTING", "1"),
        ],
        pin_every_tier: false,
        tiers: NO_PINS,
    },
];

fn builtins() -> Vec<Preset> {
    BUILTINS
        .iter()
        .map(|b| {
            let model = || b.model.map(str::to_string);
            let tier = |pin: Option<&str>| {
                pin.map(str::to_string)
                    .or_else(|| b.pin_every_tier.then(model).flatten())
            };
            Preset {
                name: b.name.to_string(),
                base_url: Some(b.base_url.to_string()),
                models: ModelSettings {
                    default: model(),
                    opus: tier(b.tiers.opus),
                    sonnet: tier(b.tiers.sonnet),
                    haiku: tier(b.tiers.haiku),
                    fable: tier(b.tiers.fable),
                    subagent: tier(b.tiers.subagent),
                },
                env: allowlisted_env(
                    b.env
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                builtin: true,
            }
        })
        .collect()
}

/// Whether `name` collides with a built-in. Case-insensitive: a
/// case-folding filesystem would let `deepseek.json` shadow the built-in's slot
/// on one host and not another, so the refusal can't depend on spelling.
pub(crate) fn is_builtin(name: &str) -> bool {
    BUILTINS
        .iter()
        .any(|b| b.name.eq_ignore_ascii_case(name.trim()))
}

fn presets_dir() -> Result<std::path::PathBuf> {
    Ok(tollgate_dir()?.join("presets"))
}

/// The name's own path, refusing anything that isn't a bare filename. Reuses the
/// profile-name charset (`[A-Za-z0-9-_.@+]`, no leading `.`), which is what keeps
/// a separator or a `..` out of the join.
fn preset_path(name: &str) -> Result<std::path::PathBuf> {
    let trimmed = crate::actions::validate_name_chars(name)?;
    Ok(presets_dir()?.join(format!("{trimmed}.json")))
}

/// Built-ins first, then the on-disk ones sorted by name. A file that won't
/// parse is skipped rather than failing the whole list — one hand-edited preset
/// must not hide the others from the picker.
pub(crate) fn list_presets() -> Vec<Preset> {
    let mut out = builtins();
    let Ok(dir) = presets_dir() else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut custom: Vec<Preset> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // A built-in's slot is never readable from disk: the binary's copy is
        // the only definition, so a stray file with that stem stays invisible
        // instead of quietly shadowing it.
        if is_builtin(name) {
            continue;
        }
        let Ok(file) = read_json_file::<PresetFile>(&path) else {
            continue;
        };
        custom.push(Preset {
            name: name.to_string(),
            base_url: file.base_url,
            models: file.models,
            env: allowlisted_env(file.env),
            builtin: false,
        });
    }
    custom.sort_by_key(|a| a.name.to_lowercase());
    out.extend(custom);
    out
}

/// Built-ins first, then disk. `None` when neither carries the name.
pub(crate) fn load_preset(name: &str) -> Option<Preset> {
    let trimmed = name.trim();
    if let Some(p) = builtins()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(trimmed))
    {
        return Some(p);
    }
    let path = preset_path(trimmed).ok()?;
    let file = read_json_file::<PresetFile>(&path).ok()?;
    Some(Preset {
        name: trimmed.to_string(),
        base_url: file.base_url,
        models: file.models,
        env: allowlisted_env(file.env),
        builtin: false,
    })
}

/// Whether a custom preset already occupies `name`. Callers confirm before
/// [`save_preset`] overwrites it.
pub(crate) fn preset_exists(name: &str) -> bool {
    preset_path(name).is_ok_and(|p| p.exists())
}

/// Write `name` to disk, replacing any custom preset already there. Refuses a
/// built-in name outright — the binary's copy is the definition, so a file in
/// that slot would be written and then never read.
pub(crate) fn save_preset(
    name: &str,
    base_url: &Option<String>,
    models: &ModelSettings,
) -> Result<()> {
    let trimmed = name.trim();
    if is_builtin(trimmed) {
        bail!("'{trimmed}' is a built-in preset and cannot be overwritten");
    }
    let path = preset_path(trimmed)?;
    let body = serde_json::to_string_pretty(&PresetFile {
        base_url: base_url.clone(),
        models: models.clone(),
        // A saved preset copies an account's endpoint + models only: its env
        // may hold anything, so none of it is written.
        env: BTreeMap::new(),
    })?;
    // `atomic_write_600` creates a missing parent 0o700 itself, so the dir and
    // the file are both born owner-only.
    atomic_write_600(&path, format!("{body}\n"))?;
    Ok(())
}

pub(crate) fn delete_preset(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if is_builtin(trimmed) {
        bail!("'{trimmed}' is a built-in preset and cannot be deleted");
    }
    let path = preset_path(trimmed)?;
    if !path.exists() {
        bail!("no preset named '{trimmed}'");
    }
    std::fs::remove_file(&path)?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/presets.rs"]
mod tests;
