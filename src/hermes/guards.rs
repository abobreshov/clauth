//! The launch guards (spec §4.3) and the child env scrub.
//!
//! Every guard is a pure function over what it was handed: paths, a
//! projection, a parsed `auth.json`. The orchestration in [`super`] decides
//! the order and which locks are held (G1–G6 and G15 before any lock, P
//! before any lock, G2a and G7–G13 inside the RotationGuard). A warning never
//! passes a refusal: each guard returns `Err` on the first refusal it finds,
//! and warnings are collected separately.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::home::HermesPaths;
use super::pool::PoolAuthView;
use super::profiles::HermesProfile;
use super::projector::{EnvKey, ProjectionV1};

/// A guard refusal: one line starting `tollgate: hermes '<name>': `,
/// printed bare (no `Error:` chain) with exit 1 (spec §2.1 exit contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal(pub(crate) String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

pub(crate) fn refuse(line: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refusal(line.into()))
}

fn prefix(name: &str) -> String {
    format!("tollgate: hermes '{name}': ")
}

// ── the scrub ─────────────────────────────────────────────────────────────────

/// The exact names of `SCRUB` (spec §4.3), apart from the prefixes, the
/// registry list and the plugin scan.
pub(crate) const SCRUB_EXACT: &[&str] = &[
    "HERMES_HOME",
    "HERMES_SHARED_AUTH_DIR",
    "HERMES_INFERENCE_PROVIDER",
    "HERMES_MODEL",
    // Hermes sets it beside HERMES_MODEL for its own relaunches
    // (`main.py:2227-2228`); an inherited one would outrank the roster's model.
    "HERMES_INFERENCE_MODEL",
    "HERMES_S6_SUPERVISED_CHILD",
    "HERMES_PORTAL_BASE_URL",
    // Flips Hermes' managed-scope and auth-store behaviour (managed_scope.py:41-49).
    "PYTEST_CURRENT_TEST",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENROUTER_API_KEY",
    "OPENROUTER_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OLLAMA_API_KEY",
    "OLLAMA_BASE_URL",
    // Set when `tollgate start <hermes>` runs from a Claude Code Bash tool.
    "CLAUDE_CONFIG_DIR",
    // The pool's `gh_cli` source (credential_sources.py:10).
    "GH_TOKEN",
    "GH_CONFIG_DIR",
    "GITHUB_TOKEN",
    // These would point the child back into the real home past the HOME redirect.
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    // Hermes reads and refreshes Codex tokens at `$CODEX_HOME/auth.json`
    // (`hermes_cli/auth.py`), writing back past the HOME redirect: any
    // inherited value (a user's, upstream's per-profile codex home) goes.
    "CODEX_HOME",
];

/// Every var with one of these prefixes is scrubbed.
pub(crate) const SCRUB_PREFIXES: &[&str] = &["NOUS_", "ANTHROPIC_"];

/// Every `api_key_env_vars` and `base_url_env_var` of Hermes 0.19.0's
/// provider registry (`hermes_cli/auth.py:177-445`), pinned. Never read by
/// importing Hermes code (D-H16).
pub(crate) const HERMES_REGISTRY_ENV_KEYS: &[&str] = &[
    "ALIBABA_CODING_PLAN_API_KEY",
    "ALIBABA_CODING_PLAN_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_TOKEN",
    "ARCEEAI_API_KEY",
    "ARCEE_BASE_URL",
    "AZURE_FOUNDRY_API_KEY",
    "AZURE_FOUNDRY_BASE_URL",
    "BEDROCK_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "COPILOT_ACP_BASE_URL",
    "COPILOT_API_BASE_URL",
    "COPILOT_GITHUB_TOKEN",
    "DASHSCOPE_API_KEY",
    "DASHSCOPE_BASE_URL",
    "DEEPSEEK_API_KEY",
    "DEEPSEEK_BASE_URL",
    "GEMINI_API_KEY",
    "GEMINI_BASE_URL",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GLM_API_KEY",
    "GLM_BASE_URL",
    "GMI_API_KEY",
    "GMI_BASE_URL",
    "GOOGLE_API_KEY",
    "HF_BASE_URL",
    "HF_TOKEN",
    "KILOCODE_API_KEY",
    "KILOCODE_BASE_URL",
    "KIMI_API_KEY",
    "KIMI_BASE_URL",
    "KIMI_CN_API_KEY",
    "KIMI_CODING_API_KEY",
    "LM_API_KEY",
    "LM_BASE_URL",
    "MINIMAX_API_KEY",
    "MINIMAX_BASE_URL",
    "MINIMAX_CN_API_KEY",
    "MINIMAX_CN_BASE_URL",
    "NVIDIA_API_KEY",
    "NVIDIA_BASE_URL",
    "OLLAMA_API_KEY",
    "OLLAMA_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENCODE_GO_API_KEY",
    "OPENCODE_GO_BASE_URL",
    "OPENCODE_ZEN_API_KEY",
    "OPENCODE_ZEN_BASE_URL",
    "STEPFUN_API_KEY",
    "STEPFUN_BASE_URL",
    "TOKENHUB_API_KEY",
    "TOKENHUB_BASE_URL",
    "XAI_API_KEY",
    "XAI_BASE_URL",
    "XIAOMI_API_KEY",
    "XIAOMI_BASE_URL",
    "ZAI_API_KEY",
    "Z_AI_API_KEY",
];

/// The 15 auxiliary task keys of 0.19.0's default config
/// (`hermes_cli/config.py:1621-1800`), pinned. `new` pins each one to the
/// profile's provider, and G10a refuses an unpinned one.
pub(crate) const HERMES_AUX_TASKS: &[&str] = &[
    "vision",
    "web_extract",
    "compression",
    "skills_hub",
    "approval",
    "mcp",
    "title_generation",
    "memory_query_rewrite",
    "tts_audio_tags",
    "triage_specifier",
    "kanban_decomposer",
    "profile_describer",
    "goal_judge",
    "curator",
    "monitor",
];

/// Whether `key` is in the static half of `SCRUB` (exact names, prefixes,
/// the pinned registry list).
pub(crate) fn in_static_scrub(key: &str) -> bool {
    SCRUB_EXACT.contains(&key)
        || HERMES_REGISTRY_ENV_KEYS.contains(&key)
        || SCRUB_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Remove the static half of `SCRUB` from `command`'s inherited env: the
/// exact names and the registry list by name, the prefixes by scanning this
/// process's environment.
pub(crate) fn scrub_static(command: &mut std::process::Command) {
    for key in SCRUB_EXACT.iter().chain(HERMES_REGISTRY_ENV_KEYS) {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|k| SCRUB_PREFIXES.iter().any(|p| k.starts_with(p)))
        {
            command.env_remove(&key);
        }
    }
}

/// The runtime plugin scan: every quoted name inside `env_vars=(…)` in the
/// provider plugins, taken by regex and never executed (D-H16).
pub(crate) fn plugin_env_vars(roots: &[PathBuf]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let (Ok(block), Ok(name)) = (
        regex::Regex::new(r"env_vars\s*=\s*\(([^)]*)\)"),
        regex::Regex::new(r#"["']([A-Za-z_][A-Za-z0-9_]*)["']"#),
    ) else {
        return out;
    };
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let init = entry.path().join("__init__.py");
            let Ok(meta) = init.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.len() > 1024 * 1024 {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&init) else {
                continue;
            };
            for m in block.captures_iter(&text) {
                for n in name.captures_iter(&m[1]) {
                    out.insert(n[1].to_string());
                }
            }
        }
    }
    out
}

/// The two plugin roots of the scan: the install's and the home's.
pub(crate) fn plugin_roots(hsp: &Path, home: &Path) -> Vec<PathBuf> {
    vec![
        hsp.join("plugins").join("model-providers"),
        home.join("plugins").join("model-providers"),
    ]
}

// ── anthropic ─────────────────────────────────────────────────────────────────

/// Whether a provider string normalises to anthropic: Hermes' aliases
/// (`hermes_cli/providers.py:291-292`) plus the `claude_code` pool source.
pub(crate) fn is_anthropic(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "anthropic" | "claude" | "claude-code" | "claude_code"
    )
}

fn is_anthropic_host(host: Option<&str>) -> bool {
    host.is_some_and(|h| {
        let h = h.trim_end_matches('.').to_ascii_lowercase();
        h == "api.anthropic.com" || h.ends_with(".anthropic.com")
    })
}

fn m_anthropic(name: &str, route: &str) -> anyhow::Error {
    refuse(format!(
        "{}{route} routes to anthropic, which makes Hermes read and rewrite \
         ~/.claude/.credentials.json outside tollgate and clauth; remove it (refused on every \
         route in v1)",
        prefix(name)
    ))
}

// ── G1–G6 ─────────────────────────────────────────────────────────────────────

pub(crate) const M_NAME: &str = "a Hermes profile cannot be named 'profiles': Hermes treats a \
                                 home whose parent dir is named profiles as one profile of a \
                                 shared root";

/// G1 shape: the literal home's parent is not `profiles`; the home exists,
/// is a real dir owned by this uid; a looser mode is tightened to 0700.
pub(crate) fn g1_shape(name: &str, paths: &HermesPaths) -> Result<()> {
    if paths
        .home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == "profiles")
    {
        return Err(refuse(format!("{}{M_NAME}", prefix(name))));
    }
    let meta = match paths.home.symlink_metadata() {
        Ok(meta) => meta,
        Err(_) => {
            return Err(refuse(format!(
                "{}home missing; delete and recreate the profile",
                prefix(name)
            )));
        }
    };
    if meta.file_type().is_symlink() || !meta.is_dir() || !super::home::owned_by_me(&meta) {
        return Err(refuse(format!(
            "{}{} is a symlink, not a directory, or not owned by you; delete and recreate the \
             profile",
            prefix(name),
            paths.home.display()
        )));
    }
    super::home::tighten_700(&paths.home, &meta);
    Ok(())
}

/// G2 containment, computed against the CHILD home: Hermes' default root is
/// `Path.home()/.hermes`, and `Path.home()` is the child home. `operator_home`
/// is the belt-and-braces second test.
pub(crate) fn g2_containment(name: &str, paths: &HermesPaths, operator_home: &Path) -> Result<()> {
    let child_root = paths.child_home.join(".hermes");
    if child_root.symlink_metadata().is_ok() {
        return Err(refuse(format!(
            "{}{} exists; Hermes would take it as its default root and read its active_profile \
             — remove it (tollgate never creates it)",
            prefix(name),
            child_root.display()
        )));
    }
    let home = std::fs::canonicalize(&paths.home).unwrap_or_else(|_| paths.home.clone());
    for root in [child_root, operator_home.join(".hermes")] {
        if let Ok(root) = std::fs::canonicalize(&root)
            && home.starts_with(&root)
        {
            return Err(refuse(format!(
                "{}the home resolves under {}, Hermes' default root; a tollgate home must sit \
                 outside it",
                prefix(name),
                root.display()
            )));
        }
    }
    Ok(())
}

/// G3: Hermes' own `active_profile` rule (`main.py:607-613`): empty or
/// `default` passes, anything else or a symlink refuses (M-ACTIVE).
pub(crate) fn g3_active_profile(name: &str, paths: &HermesPaths) -> Result<()> {
    let file = paths.home.join("active_profile");
    let Ok(meta) = file.symlink_metadata() else {
        return Ok(());
    };
    let value = if meta.file_type().is_symlink() {
        "<symlink>".to_string()
    } else {
        let raw = std::fs::read(&file).unwrap_or_default();
        let text = String::from_utf8_lossy(&raw[..raw.len().min(256)])
            .trim()
            .to_string();
        if text.is_empty() || text == "default" {
            return Ok(());
        }
        text
    };
    Err(refuse(format!(
        "{}{}/active_profile names '{value}', which would move Hermes into {}/profiles/{value} \
         and out of tollgate's isolation; remove the file (tollgate never writes it)",
        prefix(name),
        paths.home.display(),
        paths.home.display()
    )))
}

/// G4: `<home>/profiles`, of any type, refuses (M-PROFILES).
pub(crate) fn g4_profiles(name: &str, paths: &HermesPaths) -> Result<()> {
    if paths.home.join("profiles").symlink_metadata().is_ok() {
        return Err(refuse(format!(
            "{}{}/profiles exists; Hermes sub-profiles inside a tollgate home are not supported \
             — remove it",
            prefix(name),
            paths.home.display()
        )));
    }
    Ok(())
}

/// G5, the `start` pass-through: scan the WHOLE vector, `--` included,
/// because Hermes scans for `-p` anywhere (`main.py:526-560`).
///
/// Hermes' parser is argparse with its default `allow_abbrev`, so it takes a
/// unique prefix of a long option (`--prov anthropic` is `--provider`), and a
/// short option's value glued on (`-manthropic:x` is `-m anthropic:x`). Both
/// spellings are read here as the option they expand to.
pub(crate) fn g5_argv(name: &str, provider: &str, args: &[String]) -> Result<()> {
    // `--prov` / `--prov=x` against `--provider`: a prefix of the long option,
    // `--` plus at least one letter.
    let long_prefix_of = |arg: &str, full: &str| {
        let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
        flag.len() > 2 && flag.starts_with("--") && full.starts_with(flag)
    };
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        let a = arg.as_str();
        if a == "-p"
            || (a.starts_with("-p") && !a.starts_with("--"))
            || (long_prefix_of(a, "--profile") && !long_prefix_of(a, "--provider"))
        {
            return Err(refuse(format!(
                "{}'{a}' selects a Hermes profile and would leave this home; drop it (the \
                 tollgate profile is the account)",
                prefix(name)
            )));
        }
        if long_prefix_of(a, "--provider") {
            return Err(refuse(format!(
                "{}'--provider' is fixed by the profile ({provider}); create another profile \
                 for another provider",
                prefix(name)
            )));
        }
        let model = if a == "-m" || (long_prefix_of(a, "--model") && !a.contains('=')) {
            it.peek().map(|v| v.as_str())
        } else if long_prefix_of(a, "--model") {
            a.split_once('=').map(|(_, v)| v)
        } else {
            // A glued short value, `-mX`. argparse reads `-m=x` as the value
            // `=x`; Hermes' own alias split sees what follows it either way.
            a.strip_prefix("-m")
                .filter(|v| !v.is_empty())
                .map(|glued| glued.trim_start_matches('='))
        };
        if let Some(model) = model
            && let Some((alias, _)) = model.split_once(':')
            && is_anthropic(alias)
        {
            return Err(m_anthropic(name, "-m"));
        }
    }
    Ok(())
}

/// G5 on the roster's own model: `start` passes it as `-m`, so an anthropic
/// alias saved by `hermes new --model` would bypass the argv scan.
pub(crate) fn g5_model(name: &str, provider: &str, model: Option<&str>) -> Result<()> {
    match model {
        Some(m) => g5_argv(name, provider, &["-m".to_string(), m.to_string()]),
        None => Ok(()),
    }
}

/// G6: `$HSP/.env` is loaded into every session (`main.py:654`) and can
/// refill scrubbed keys.
pub(crate) fn g6_hsp_env(name: &str, hsp: &Path) -> Result<()> {
    if hsp.join(".env").symlink_metadata().is_ok() {
        return Err(refuse(format!(
            "{}{}/.env exists; Hermes loads it into every session and it can refill scrubbed \
             keys — move it away",
            prefix(name),
            hsp.display()
        )));
    }
    Ok(())
}

// ── G14 ───────────────────────────────────────────────────────────────────────

/// G14 liveness: a live tollgate marker (M-LIVE), then a live
/// `gateway.pid` (M-BUSY).
pub(crate) fn g14_liveness(name: &str, paths: &HermesPaths) -> Result<()> {
    if crate::runtime::has_live_session(&crate::profile::ProfileName::from(name)) {
        return Err(m_live_for(name, paths));
    }
    if let Some(pid) = gateway_pid(&paths.home)
        && pid_alive(pid)
    {
        return Err(m_busy(name, "a Hermes gateway"));
    }
    Ok(())
}

pub(crate) fn m_live(name: &str, sid: &str) -> anyhow::Error {
    refuse(format!(
        "{}a Hermes session is already running on this home (session {sid}); Hermes homes take \
         one process at a time — start another account home instead",
        prefix(name)
    ))
}

/// M-LIVE naming the live marker's session, when one can be found.
pub(crate) fn m_live_for(name: &str, paths: &HermesPaths) -> anyhow::Error {
    let sid = live_marker_sid(&paths.profile).unwrap_or_else(|| "unknown".to_string());
    m_live(name, &sid)
}

pub(crate) fn m_busy(name: &str, what: &str) -> anyhow::Error {
    refuse(format!(
        "{}the home is busy ({what}); try again when it finishes",
        prefix(name)
    ))
}

/// The sid of the first held marker under `sessions-<sid>/`.
fn live_marker_sid(profile: &Path) -> Option<String> {
    let entries = std::fs::read_dir(profile).ok()?;
    for entry in entries.flatten() {
        let dir = entry.file_name().to_string_lossy().into_owned();
        let Some(sid) = dir.strip_prefix("sessions-") else {
            continue;
        };
        if crate::runtime::live_sessions_at(&entry.path()).is_some_and(|n| n > 0) {
            return Some(sid.to_string());
        }
    }
    None
}

/// `gateway.pid`: a bare int, a JSON int, or `{"pid": n}`
/// (`gateway/status.py:541-567`).
fn gateway_pid(home: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(home.join("gateway.pid")).ok()?;
    let raw = raw.trim();
    if let Ok(pid) = raw.parse::<i32>() {
        return Some(pid);
    }
    match serde_json::from_str::<serde_json::Value>(raw).ok()? {
        serde_json::Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
        serde_json::Value::Object(o) => o.get("pid")?.as_i64().and_then(|n| i32::try_from(n).ok()),
        _ => None,
    }
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 only probes; `pid` is a plain integer.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    false
}

// ── G8 managed scope ─────────────────────────────────────────────────────────

/// Hermes' own resolution (`managed_scope.py:52-71`): a non-empty
/// `$HERMES_MANAGED_DIR` decides alone (that dir if it is a dir, else no
/// scope); otherwise `default` when it is a dir. Pure, so the test injects
/// both inputs.
pub(crate) fn managed_dir_from(env_value: Option<OsString>, default: &Path) -> Option<PathBuf> {
    if let Some(v) = env_value.filter(|v| !v.to_string_lossy().trim().is_empty()) {
        let p = PathBuf::from(v.to_string_lossy().trim());
        return p.is_dir().then_some(p);
    }
    default.is_dir().then(|| default.to_path_buf())
}

/// The managed dir for this process: `$HERMES_MANAGED_DIR`, else
/// `/etc/hermes` (or the test override, `MANAGED_DIR_OVERRIDE`).
pub(crate) fn managed_dir() -> Option<PathBuf> {
    let default = managed_default();
    managed_dir_from(std::env::var_os("HERMES_MANAGED_DIR"), &default)
}

fn managed_default() -> PathBuf {
    #[cfg(test)]
    if let Some(path) = MANAGED_DIR_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return path;
    }
    PathBuf::from("/etc/hermes")
}

/// Test-only override of the `/etc/hermes` default, so the real system dir is
/// never consulted from a test. Serialized by `HOME_TEST_LOCK`.
#[cfg(test)]
pub(crate) static MANAGED_DIR_OVERRIDE: std::sync::Mutex<Option<PathBuf>> =
    std::sync::Mutex::new(None);

/// The config keys a managed scope may not set: each outranks the profile's
/// binding.
const MANAGED_FORBIDDEN_TOP_KEYS: &[&str] = &[
    "model",
    "provider",
    "providers",
    "custom_providers",
    "auxiliary",
    "delegation",
    "fallback_providers",
    "fallback_model",
    "credential_pool_strategies",
    "secrets",
];

fn is_scrubbed_or_anthropic(key: &str, dynamic: &BTreeSet<String>) -> bool {
    in_static_scrub(key) || dynamic.contains(key)
}

// ── the projection audit: G7–G10a, G12 ───────────────────────────────────────

/// What the projection audit hands back when nothing refuses.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AuditNotes {
    /// Warnings to print (W-MANAGED).
    pub(crate) warnings: Vec<String>,
}

/// G7, G8, G9, G10 and G10a over one projection, in that order, stopping at
/// the first refusal (G11 reads `auth.json`, and G12 is [`g12_env`]). `dynamic_scrub` is the plugin-scan half of `SCRUB`,
/// and `entry` names the resolved entrypoint in the M-AUX fix.
pub(crate) fn audit_projection(
    name: &str,
    home: &Path,
    profile: &HermesProfile,
    projection: &ProjectionV1,
    managed_dir: Option<&Path>,
    dynamic_scrub: &BTreeSet<String>,
    entry: &Path,
) -> Result<AuditNotes> {
    let mut notes = AuditNotes::default();
    let p = prefix(name);
    let env = &projection.env_keys;

    // G7: `.op.env` may carry only the 1Password bootstrap token.
    let foreign: Vec<&str> = env
        .op_env
        .iter()
        .map(|k| k.key.as_str())
        .filter(|k| *k != "OP_SERVICE_ACCOUNT_TOKEN")
        .collect();
    if !foreign.is_empty() {
        return Err(refuse(format!(
            "{p}{}/.op.env sets {}; only OP_SERVICE_ACCOUNT_TOKEN is allowed there",
            home.display(),
            foreign.join(", ")
        )));
    }

    // G8: the managed scope.
    if let Some(dir) = managed_dir {
        let m_managed = |what: String| {
            refuse(format!(
                "{p}the managed Hermes scope {} sets {what}, which outranks this profile's \
                 binding; ask whoever manages this machine, tollgate cannot override it",
                dir.display()
            ))
        };
        if let Some(k) = env.managed.iter().find(|k| {
            is_scrubbed_or_anthropic(&k.key, dynamic_scrub) || k.key.starts_with("ANTHROPIC_")
        }) {
            return Err(m_managed(k.key.clone()));
        }
        if let Some(k) = projection
            .managed_config_top_keys
            .iter()
            .find(|k| MANAGED_FORBIDDEN_TOP_KEYS.contains(&k.as_str()))
        {
            return Err(m_managed(k.clone()));
        }
        notes.warnings.push(format!(
            "tollgate: note — managed Hermes scope {} applies to this session",
            dir.display()
        ));
    }

    // G9: secret sources.
    for (source, view) in &projection.config.secrets {
        if !view.enabled {
            continue;
        }
        let m_secrets = |why: String| {
            refuse(format!(
                "{p}secrets source '{source}' {why}; disable it in {}/config.yaml or map only \
                 names this profile does not bind",
                home.display()
            ))
        };
        if source != "onepassword" {
            return Err(m_secrets(if source == "bitwarden" {
                "is a bulk source that can set any variable".to_string()
            } else {
                "is not one tollgate can audit".to_string()
            }));
        }
        if let Some(target) = view.targets.iter().find(|t| {
            is_scrubbed_or_anthropic(t, dynamic_scrub)
                && profile.key_env.as_deref() != Some(t.as_str())
        }) {
            return Err(m_secrets(format!("maps {target}, which tollgate scrubs")));
        }
    }

    // G10: anthropic on a named config route, first hit wins.
    let c = &projection.config;
    if c.model_provider.as_deref().is_some_and(is_anthropic) {
        return Err(m_anthropic(name, "model.provider"));
    }
    if is_anthropic_host(c.model_base_url_host.as_deref()) {
        return Err(m_anthropic(name, "model.base_url"));
    }
    for fp in &c.fallback_providers {
        if is_anthropic(fp) || fp.split_once(':').is_some_and(|(a, _)| is_anthropic(a)) {
            return Err(m_anthropic(name, "fallback_providers"));
        }
    }
    for fm in &c.fallback_model {
        if is_anthropic(fm) || fm.split_once(':').is_some_and(|(a, _)| is_anthropic(a)) {
            return Err(m_anthropic(name, "fallback_model"));
        }
    }
    for (task, route) in &c.auxiliary {
        if route.provider.as_deref().is_some_and(is_anthropic) {
            return Err(m_anthropic(name, &format!("auxiliary.{task}.provider")));
        }
        if is_anthropic_host(route.base_url_host.as_deref()) {
            return Err(m_anthropic(name, &format!("auxiliary.{task}.base_url")));
        }
    }
    if c.delegation.provider.as_deref().is_some_and(is_anthropic) {
        return Err(m_anthropic(name, "delegation.provider"));
    }
    if is_anthropic_host(c.delegation.base_url_host.as_deref()) {
        return Err(m_anthropic(name, "delegation.base_url"));
    }
    for pv in &c.providers {
        let key = pv.key.clone().unwrap_or_default();
        if is_anthropic(&key) || pv.name.as_deref().is_some_and(is_anthropic) {
            return Err(m_anthropic(name, &format!("providers.{key}")));
        }
        if is_anthropic_host(pv.base_url_host.as_deref()) {
            return Err(m_anthropic(name, &format!("providers.{key}.base_url")));
        }
    }
    for (i, cp) in c.custom_providers.iter().enumerate() {
        if cp.name.as_deref().is_some_and(is_anthropic)
            || is_anthropic_host(cp.base_url_host.as_deref())
        {
            return Err(m_anthropic(name, &format!("custom_providers[{i}]")));
        }
    }

    // G10a, defence in depth: every auxiliary task pinned to a real provider.
    let mut tasks: Vec<&str> = HERMES_AUX_TASKS.to_vec();
    for task in c.auxiliary.keys() {
        if !tasks.contains(&task.as_str()) {
            tasks.push(task);
        }
    }
    for task in tasks {
        let value = c
            .auxiliary
            .get(task)
            .and_then(|r| r.provider.as_deref())
            .map(str::trim);
        let shown = match value {
            None => "unset",
            Some("") => "empty",
            Some(v) if v.eq_ignore_ascii_case("auto") => "auto",
            Some(_) => continue,
        };
        let child_home = home
            .parent()
            .unwrap_or(home)
            .join(super::home::CHILD_HOME_DIR);
        return Err(refuse(format!(
            "{p}auxiliary.{task}.provider is {shown}; Hermes' auto chain can fall through to \
             Anthropic — run HOME={} HERMES_HOME={} {} config set auxiliary.{task}.provider {} or recreate \
             the profile",
            super::shell_quote(&child_home.display().to_string()),
            super::shell_quote(&home.display().to_string()),
            super::shell_quote(&entry.display().to_string()),
            profile.provider
        )));
    }
    Ok(notes)
}

/// G12: anthropic in the env layers. A non-blank Anthropic key or the Claude
/// Code token in `.op.env` or the managed `.env` refuses, and so does any
/// `ANTHROPIC_*` (blank or not) or a non-blank `CLAUDE_CODE_OAUTH_TOKEN` in
/// the home `.env`.
pub(crate) fn g12_env(name: &str, projection: &ProjectionV1) -> Result<()> {
    let env = &projection.env_keys;
    let named = |k: &EnvKey| {
        k.nonblank
            && matches!(
                k.key.as_str(),
                "ANTHROPIC_API_KEY" | "ANTHROPIC_TOKEN" | "CLAUDE_CODE_OAUTH_TOKEN"
            )
    };
    if env.op_env.iter().any(named) {
        return Err(m_anthropic(name, ".op.env"));
    }
    if env.managed.iter().any(named) {
        return Err(m_anthropic(name, "the managed .env"));
    }
    if env.home.iter().any(|k| {
        k.key.starts_with("ANTHROPIC_") || (k.key == "CLAUDE_CODE_OAUTH_TOKEN" && k.nonblank)
    }) {
        return Err(m_anthropic(name, ".env"));
    }
    Ok(())
}

/// The dotenv key names of the home `.env`, as the projector parsed them:
/// the cross-check set of the `.env` writer (§4.2 step 2).
pub(crate) fn home_env_key_set(projection: &ProjectionV1) -> BTreeSet<String> {
    projection
        .env_keys
        .home
        .iter()
        .map(|k| k.key.clone())
        .collect()
}

// ── G11 ───────────────────────────────────────────────────────────────────────

/// G11 over a parsed `auth.json` and the home: `active_provider`, a
/// `providers` key, a `credential_pool` key, and Hermes' own PKCE store.
pub(crate) fn g11_auth(name: &str, home: &Path, view: Option<&PoolAuthView>) -> Result<()> {
    if let Some(view) = view {
        if view.active_provider.as_deref().is_some_and(is_anthropic) {
            return Err(m_anthropic(name, "auth.json active_provider"));
        }
        if let Some(k) = view.providers.keys().find(|k| is_anthropic(k)) {
            return Err(m_anthropic(name, &format!("auth.json providers.{k}")));
        }
        if let Some(k) = view.credential_pool.keys().find(|k| is_anthropic(k)) {
            return Err(m_anthropic(name, &format!("auth.json credential_pool.{k}")));
        }
    }
    if home
        .join(".anthropic_oauth.json")
        .symlink_metadata()
        .is_ok()
    {
        return Err(m_anthropic(name, ".anthropic_oauth.json"));
    }
    Ok(())
}

/// G11's read, refusing an unreadable or torn file.
pub(crate) fn g11_read_and_check(name: &str, home: &Path) -> Result<Option<PoolAuthView>> {
    let view = super::pool::read_auth_view(home).map_err(|_| {
        refuse(format!(
            "{}cannot audit auth.json; retry when Hermes is not writing it",
            prefix(name)
        ))
    })?;
    g11_auth(name, home, view.as_ref())?;
    Ok(view)
}

// ── G13 ───────────────────────────────────────────────────────────────────────

/// G13 for `auth add <provider>` (the env half runs in the launch audit).
pub(crate) fn g13_auth_add(
    name: &str,
    profile: &HermesProfile,
    provider: &str,
    view: Option<&PoolAuthView>,
    env_has_key: bool,
) -> Result<()> {
    use super::profiles::Mode;
    let p = prefix(name);
    if provider != profile.provider.as_str() {
        return match profile.mode {
            Mode::Account => Err(refuse(format!(
                "{p}an account is one provider; this home is {}",
                profile.provider
            ))),
            Mode::Pool => Err(refuse(format!(
                "{p}this pool home is {}; create another profile for another provider",
                profile.provider
            ))),
        };
    }
    if profile.mode == Mode::Account {
        let pooled = view.is_some_and(|v| !v.entries(provider).is_empty());
        if env_has_key || pooled {
            bail!(refuse(format!(
                "{p}account homes hold one account; create another with 'tollgate hermes new', \
                 or use a pool home"
            )));
        }
    }
    Ok(())
}

/// `auth add` refuses an anthropic alias before G1 (G13, last bullet).
pub(crate) fn refuse_anthropic_provider(name: &str, provider: &str) -> Result<()> {
    if is_anthropic(provider) {
        return Err(m_anthropic(name, "auth add"));
    }
    Ok(())
}

/// An error from the `.env` audit or writer, as the one-line refusal naming
/// the profile (the writer's own texts carry no prefix).
pub(crate) fn as_refusal(name: &str, e: anyhow::Error) -> anyhow::Error {
    if e.downcast_ref::<Refusal>().is_some() {
        return e;
    }
    let text = format!("{e:#}");
    if text.starts_with("tollgate: hermes '") {
        return refuse(text);
    }
    refuse(format!("{}{text}", prefix(name)))
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_guards.rs"]
mod tests;
