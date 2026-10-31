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

/// One entry of the shipped table.
struct Builtin {
    name: &'static str,
    base_url: &'static str,
    /// Model ids this built-in shipped in an earlier release. A row holding one
    /// of these (exact string equality) is offered the current bind for that
    /// row; any other stored id stays.
    former_ids: &'static [&'static str],
    binds: Binds,
    /// Env switches, each one on [`PRESET_ENV_ALLOWLIST`].
    env: &'static [(&'static str, &'static str)],
}

/// How a built-in fills the account's model block.
enum Binds {
    /// Leave the moving Ollama catalogue for the operator to choose from.
    Unbound,
    /// One id per Claude tier. `default` becomes the `opus` alias, so the main
    /// model follows the opus bind and a model release touches tier rows only;
    /// `subagent` stays unset, so a subagent's own tier pick resolves through
    /// that tier's bind. A `[1m]` suffix makes Claude Code size the context
    /// window at 1M, so it rides only an id whose endpoint takes that much.
    Tiers {
        opus: &'static str,
        sonnet: &'static str,
        haiku: &'static str,
        fable: &'static str,
    },
}

impl Binds {
    fn models(&self) -> ModelSettings {
        let id = |s: &str| Some(s.to_string());
        match *self {
            Binds::Unbound => ModelSettings::default(),
            Binds::Tiers {
                opus,
                sonnet,
                haiku,
                fable,
            } => ModelSettings {
                default: id("opus"),
                opus: id(opus),
                sonnet: id(sonnet),
                haiku: id(haiku),
                fable: id(fable),
                subagent: None,
            },
        }
    }
}

/// The built-ins, in menu order.
const BUILTINS: &[Builtin] = &[
    Builtin {
        env: &[],
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/anthropic",
        former_ids: &["deepseek-chat"],
        binds: Binds::Tiers {
            opus: "deepseek-v4-pro[1m]",
            sonnet: "deepseek-flash[1m]",
            haiku: "deepseek-flash[1m]",
            fable: "deepseek-v4-pro[1m]",
        },
    },
    Builtin {
        env: &[],
        name: "Z.ai",
        base_url: "https://api.z.ai/api/anthropic",
        former_ids: &["glm-5.2"],
        binds: Binds::Tiers {
            opus: "glm-5.3[1m]",
            sonnet: "glm-5.3-flash[1m]",
            haiku: "glm-5.3-flash[1m]",
            fable: "glm-5.3[1m]",
        },
    },
    Builtin {
        env: &[("CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK", "1")],
        name: "OpenRouter",
        // OpenRouter's own Claude Code guide pins this exact base URL, so CC's
        // `/v1/messages` lands on `/api/v1/messages` (openrouter.ai docs,
        // "Connect Claude to OpenRouter").
        base_url: "https://openrouter.ai/api",
        former_ids: &["openrouter/auto"],
        // OpenRouter serves the real Claude models, so each tier binds its own
        // family's moving `-latest` alias instead of a pro/flash pair; the same
        // guide marks only the fable, opus and sonnet aliases `[1m]`-capable.
        binds: Binds::Tiers {
            opus: "~anthropic/claude-opus-latest[1m]",
            sonnet: "~anthropic/claude-sonnet-latest[1m]",
            haiku: "~anthropic/claude-haiku-latest",
            fable: "~anthropic/claude-fable-latest[1m]",
        },
    },
    Builtin {
        env: &[],
        name: "MiniMax",
        // MiniMax fronts the Anthropic-shaped surface at `/anthropic`, so CC's
        // `/v1/messages` lands on `/anthropic/v1/messages` (platform.minimax.io,
        // "Claude Code" integration guide). The China-region host is a separate
        // account and endpoint; only the international one ships, matching what
        // `providers::minimax` claims.
        base_url: "https://api.minimax.io/anthropic",
        former_ids: &["MiniMax-M3"],
        // The fast model is sold on the M Plan alone: a pay-as-you-go key
        // fails on sonnet and haiku until those two rows are re-pointed.
        binds: Binds::Tiers {
            opus: "MiniMax-M3[1m]",
            sonnet: "MiniMax-M3.1-Flash-Preview[1m]",
            haiku: "MiniMax-M3.1-Flash-Preview[1m]",
            fable: "MiniMax-M3[1m]",
        },
    },
    Builtin {
        env: &[],
        name: "Qwen",
        // Qwen Cloud serves Token Plan for every region from one host, which
        // answered `400 "Model not exist."` to `claude-sonnet-5-5` (measured
        // 2026-10-13), so every tier binds a Qwen id.
        base_url: "https://token-plan.maas.qwencloudapi.com/apps/anthropic",
        former_ids: &["qwen3.8-max", "qwen3-coder-plus"],
        binds: Binds::Tiers {
            opus: "qwen3.8-max[1m]",
            sonnet: "qwen3.8-flash[1m]",
            haiku: "qwen3.8-flash[1m]",
            fable: "qwen3.8-max[1m]",
        },
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
        former_ids: &[],
        binds: Binds::Unbound,
        // What `ollama launch claude` sets (ollama cmd/launch/claude.go),
        // minus the context-window knob, which depends on the model picked.
        env: &[
            ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
            ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
            ("DISABLE_ERROR_REPORTING", "1"),
        ],
    },
];

fn builtins() -> Vec<Preset> {
    BUILTINS
        .iter()
        .map(|b| Preset {
            name: b.name.to_string(),
            base_url: Some(b.base_url.to_string()),
            models: b.binds.models(),
            env: allowlisted_env(
                b.env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            builtin: true,
        })
        .collect()
}

/// Whether `name` collides with a built-in, case-insensitive: the built-in owns
/// its name, and a case-folding filesystem would read `qwen.json` as that name
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
/// profile-name charset ([`crate::actions::is_name_char`], no leading `.`), which is what keeps
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
        // Apply and delete reach the file through `preset_path`, which trims the
        // name and refuses one outside the preset charset; a row they could
        // never open, or would open as another file, is skipped like an
        // unparseable one.
        if crate::actions::validate_name_chars(name).ok() != Some(name) {
            continue;
        }
        // A file whose stem matches a built-in's name still lists as the user's
        // own saved preset: the built-in keeps its name, the file keeps its
        // stem, and neither replaces the other.
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
    load_saved_preset(trimmed)
}

/// The disk half of [`load_preset`]: reads only the file, never the built-ins.
/// A built-in's name with no file on disk is `None`, so this is how the saved
/// `qwen` beside the built-in `Qwen` is told apart from it.
pub(crate) fn load_saved_preset(name: &str) -> Option<Preset> {
    let trimmed = name.trim();
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
/// built-in name outright — the built-in owns that name, and a fresh save under
/// it would add a second row beside the shipped one, never replace it.
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
    let path = preset_path(trimmed)?;
    if path.exists() {
        std::fs::remove_file(&path)?;
        return Ok(());
    }
    // No file at the name's path: it is either a built-in (whose copy ships in
    // the binary and can't be deleted) or unknown.
    if is_builtin(trimmed) {
        bail!("'{trimmed}' is a built-in preset and cannot be deleted");
    }
    bail!("no preset named '{trimmed}'");
}

// ── Re-bind offer engine ──────────────────────────────────────────────────────

/// The model rows the offer walks, in render order (`default` is the account's
/// `model` setting; `subagent` the `CLAUDE_CODE_SUBAGENT_MODEL` row).
pub(crate) const MODEL_ROWS: [&str; 6] =
    ["default", "opus", "sonnet", "haiku", "fable", "subagent"];

/// The `(unset)` marker for a row with no stored id, on both the old and the
/// new side.
const UNSET: &str = "(unset)";

/// One row a built-in's bind fills: `None` renders `(unset)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowMove {
    pub(crate) row: &'static str,
    pub(crate) old: Option<String>,
    pub(crate) new: Option<String>,
}

impl RowMove {
    pub(crate) fn old_display(&self) -> &str {
        self.old.as_deref().unwrap_or(UNSET)
    }

    pub(crate) fn new_display(&self) -> &str {
        self.new.as_deref().unwrap_or(UNSET)
    }
}

/// Every row one account is offered, in [`MODEL_ROWS`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountOffers {
    pub(crate) account: String,
    pub(crate) rows: Vec<RowMove>,
}

/// Accounts whose change sets are identical share one block: the shared rows
/// once, then one toggle line per account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RebindBlock {
    pub(crate) rows: Vec<RowMove>,
    pub(crate) accounts: Vec<String>,
}

/// `a` and `b` name the same endpoint: the crate's shared host predicate
/// (`providers::url_matches_host`), case-insensitive on scheme + host and
/// forgiving a trailing slash. It also admits a longer path under `b` (e.g.
/// `…/anthropic/v1`), which still points at that built-in's endpoint.
pub(crate) fn base_url_matches(a: &str, b: &str) -> bool {
    crate::providers::url_matches_host(a, b)
}

/// The per-account offer, in roster order: every account whose `base_url`
/// matches a built-in, with one [`RowMove`] per row that is empty or still
/// holds a former id of that built-in. A row holding any other id is untouched,
/// and a row already equal to its new value is no change.
pub(crate) fn rebind_offers(config: &crate::profile::AppConfig) -> Vec<AccountOffers> {
    let mut out = Vec::new();
    for profile in &config.profiles {
        let Some(base) = profile.base_url.as_deref() else {
            continue;
        };
        let Some(builtin) = BUILTINS.iter().find(|b| base_url_matches(base, b.base_url)) else {
            continue;
        };
        let binds = builtin.binds.models();
        let mut rows = Vec::new();
        for row in MODEL_ROWS {
            let old = profile.models.field(row).cloned();
            let new = binds.field(row).cloned();
            if old == new {
                continue;
            }
            match &old {
                None => {
                    if new.is_some() {
                        rows.push(RowMove {
                            row,
                            old: None,
                            new,
                        });
                    }
                }
                Some(old_id) => {
                    if builtin.former_ids.contains(&old_id.as_str()) {
                        rows.push(RowMove {
                            row,
                            old: Some(old_id.clone()),
                            new,
                        });
                    }
                }
            }
        }
        if !rows.is_empty() {
            out.push(AccountOffers {
                account: profile.name.to_string(),
                rows,
            });
        }
    }
    out
}

/// Group offers whose row change sets are identical, preserving first-seen
/// order for both the blocks and the accounts inside them.
pub(crate) fn group_offers(offers: Vec<AccountOffers>) -> Vec<RebindBlock> {
    let mut blocks: Vec<RebindBlock> = Vec::new();
    for offer in offers {
        if let Some(block) = blocks.iter_mut().find(|b| b.rows == offer.rows) {
            block.accounts.push(offer.account);
        } else {
            blocks.push(RebindBlock {
                rows: offer.rows,
                accounts: vec![offer.account],
            });
        }
    }
    blocks
}

/// SHA-256 (hex) of the shipped bind table: each built-in's name, base_url,
/// resolved models and former ids. Changes whenever the table's names,
/// endpoints, binds or former ids change (their order included), so a stored
/// digest stands for "this release's binds were already offered" — a release
/// that only adds a former id reopens the confirm for an account that
/// dismissed it.
pub(crate) fn builtin_binds_digest() -> String {
    digest_of_builtins(BUILTINS)
}

/// The digest over a specific table — split out so a test can pin that two
/// literal tables differ without recomputing the shipped one.
fn digest_of_builtins(builtins: &[Builtin]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for builtin in builtins {
        hasher.update(builtin.name.as_bytes());
        hasher.update([0u8]);
        hasher.update(builtin.base_url.as_bytes());
        hasher.update([0u8]);
        let binds = builtin.binds.models();
        for row in MODEL_ROWS {
            hasher.update(row.as_bytes());
            hasher.update([0u8]);
            if let Some(value) = binds.field(row) {
                hasher.update(value.as_bytes());
            }
            hasher.update([0u8]);
        }
        // The `0` after each id keeps `["ab", "c"]` and `["a", "bc"]` apart; the
        // count prefix keeps a list's last id from reading as the start of the
        // next built-in's name.
        hasher.update((builtin.former_ids.len() as u64).to_le_bytes());
        for id in builtin.former_ids {
            hasher.update(id.as_bytes());
            hasher.update([0u8]);
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `N row(s) on M account(s)`, singular-aware — the one phrase the TUI toast
/// and the CLI's two count lines share.
pub(crate) fn rows_on_accounts(rows: usize, accounts: usize) -> String {
    let row_word = if rows == 1 { "row" } else { "rows" };
    let acct_word = if accounts == 1 { "account" } else { "accounts" };
    format!("{rows} {row_word} on {accounts} {acct_word}")
}

#[cfg(test)]
#[path = "../tests/inline/presets.rs"]
mod tests;

/// What applying a preset does with a set field the preset leaves unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leftover {
    Keep,
    Clear,
}

/// An endpoint + model block as the Setup rows list it: label and value, in
/// row order.
fn block_rows<'a>(
    base_url: Option<&'a str>,
    m: &'a ModelSettings,
) -> [(&'static str, Option<&'a str>); 7] {
    [
        ("base url", base_url),
        ("model", m.default.as_deref()),
        ("fable", m.fable.as_deref()),
        ("opus", m.opus.as_deref()),
        ("sonnet", m.sonnet.as_deref()),
        ("haiku", m.haiku.as_deref()),
        ("subagent", m.subagent.as_deref()),
    ]
}

impl Preset {
    /// Over an account's endpoint + model block: the fields whose value this
    /// preset's own replaces, and the fields holding a value it leaves unset,
    /// each in Setup row order.
    pub(crate) fn apply_diff(
        &self,
        base_url: Option<&str>,
        models: &ModelSettings,
    ) -> (Vec<&'static str>, Vec<&'static str>) {
        let mut replaces = Vec::new();
        let mut leaves = Vec::new();
        let ours = block_rows(self.base_url.as_deref(), &self.models);
        for ((label, mine), (_, theirs)) in ours.into_iter().zip(block_rows(base_url, models)) {
            match (mine, theirs) {
                (Some(mine), Some(theirs)) if mine != theirs => replaces.push(label),
                (None, Some(_)) => leaves.push(label),
                _ => {}
            }
        }
        (replaces, leaves)
    }

    /// The endpoint + model block applying this preset leaves: every field it
    /// sets, and every field it leaves unset kept or cleared per `leftover`.
    pub(crate) fn applied_to(
        &self,
        base_url: &Option<String>,
        models: &ModelSettings,
        leftover: Leftover,
    ) -> (Option<String>, ModelSettings) {
        let pick = |mine: &Option<String>, theirs: &Option<String>| match (mine, leftover) {
            (Some(_), _) => mine.clone(),
            (None, Leftover::Keep) => theirs.clone(),
            (None, Leftover::Clear) => None,
        };
        let m = &self.models;
        (
            pick(&self.base_url, base_url),
            ModelSettings {
                default: pick(&m.default, &models.default),
                opus: pick(&m.opus, &models.opus),
                sonnet: pick(&m.sonnet, &models.sonnet),
                haiku: pick(&m.haiku, &models.haiku),
                fable: pick(&m.fable, &models.fable),
                subagent: pick(&m.subagent, &models.subagent),
            },
        )
    }
}
