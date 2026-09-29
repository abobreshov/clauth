//! The projector (spec §4.3 P): Hermes' own interpreter reads `config.yaml`,
//! the home `.env`, `.op.env` and the managed scope with Hermes' own parsers,
//! and prints names and hosts only.
//!
//! Parser parity is the point (D-H2). `utils.fast_safe_load` is the exact
//! loader Hermes uses for `config.yaml`, and python-dotenv is the one it uses
//! for `.env`. A YAML anchor, alias or `<<:` merge therefore resolves here
//! exactly as it does when Hermes starts, so a route hidden behind a merge
//! key still reaches the guards. A Rust YAML crate would be a second parser
//! with its own disagreements.
//!
//! The output never carries a value: no `api_key`, no URL path, no env value,
//! only key names, provider strings, hostnames and whether an env value is
//! blank. It is checked against [`ProjectionV1`], a strict schema: every key
//! is required and an unknown key refuses, so a projector that silently drops
//! a route cannot pass the guards. Every failure refuses the launch (fail
//! closed).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// How long one projector run may take before it is killed and the launch
/// refused.
pub(crate) const PROJECTOR_TIMEOUT: Duration = Duration::from_secs(10);

/// Run as `<venv>/bin/python -I -B -c PROJECTOR <home> <managed_dir|"">`.
pub(crate) const PROJECTOR: &str = r#"
import json, os, sys
from urllib.parse import urlsplit
from utils import fast_safe_load
from dotenv import dotenv_values

home, managed = sys.argv[1], sys.argv[2]

def die(code, why):
    sys.stderr.write(why + "\n")
    sys.exit(code)

def load_yaml(path):
    try:
        with open(path, encoding="utf-8") as f:
            data = fast_safe_load(f)
    except FileNotFoundError:
        return {}
    if data is None:
        return {}
    if not isinstance(data, dict):
        die(3, path + " is not a mapping")
    return data

def s(v):
    if v is None:
        return None
    return v if isinstance(v, str) else str(v)

def host(v):
    if not isinstance(v, str) or not v.strip():
        return None
    try:
        return urlsplit(v.strip()).hostname
    except ValueError:
        return "<unparseable>"

def env_keys(path):
    if not os.path.exists(path):
        return []
    vals = dotenv_values(path)
    return [{"key": k, "nonblank": bool(v and v.strip())} for k, v in vals.items()]

def route(d):
    if not isinstance(d, dict):
        return {"provider": None, "base_url_host": None}
    return {"provider": s(d.get("provider")), "base_url_host": host(d.get("base_url"))}

cfg = load_yaml(os.path.join(home, "config.yaml"))
model = cfg.get("model")
if isinstance(model, dict):
    model_provider, model_name, model_host = s(model.get("provider")), s(model.get("default")), host(model.get("base_url"))
else:
    model_provider, model_name, model_host = None, s(model), None

providers = []
for k, v in (cfg.get("providers") or {}).items() if isinstance(cfg.get("providers"), dict) else []:
    v = v if isinstance(v, dict) else {}
    providers.append({"key": s(k), "name": s(v.get("name")), "base_url_host": host(v.get("base_url") or v.get("api"))})

custom = []
for v in cfg.get("custom_providers") or [] if isinstance(cfg.get("custom_providers"), list) else []:
    v = v if isinstance(v, dict) else {}
    custom.append({"name": s(v.get("name")), "base_url_host": host(v.get("base_url"))})

def provider_list(raw):
    if raw is None:
        return []
    items = raw if isinstance(raw, list) else [raw]
    out = []
    for item in items:
        p = s(item.get("provider")) if isinstance(item, dict) else s(item)
        if p is not None:
            out.append(p)
    return out

aux = {}
if isinstance(cfg.get("auxiliary"), dict):
    for task, v in cfg["auxiliary"].items():
        aux[s(task)] = route(v)

secrets = {}
if isinstance(cfg.get("secrets"), dict):
    for src, v in cfg["secrets"].items():
        if isinstance(v, dict):
            env = v.get("env")
            secrets[s(src)] = {"enabled": bool(v.get("enabled")),
                               "targets": sorted(s(k) for k in env) if isinstance(env, dict) else []}

plugins = cfg.get("plugins")
enabled = plugins.get("enabled") if isinstance(plugins, dict) else None
strategies = cfg.get("credential_pool_strategies")

managed_top = []
managed_env = []
if managed:
    managed_top = sorted(s(k) for k in load_yaml(os.path.join(managed, "config.yaml")))
    managed_env = env_keys(os.path.join(managed, ".env"))

print(json.dumps({
    "config": {
        "model_provider": model_provider,
        "model": model_name,
        "model_base_url_host": model_host,
        "providers": providers,
        "custom_providers": custom,
        "fallback_providers": provider_list(cfg.get("fallback_providers")),
        "fallback_model": provider_list(cfg.get("fallback_model")),
        "auxiliary": aux,
        "delegation": route(cfg.get("delegation")),
        "credential_pool_strategies": {s(k): s(v) for k, v in strategies.items()} if isinstance(strategies, dict) else {},
        "secrets": secrets,
        "plugins_enabled": [s(p) for p in enabled] if isinstance(enabled, list) else [],
    },
    "managed_config_top_keys": managed_top,
    "env_keys": {
        "home": env_keys(os.path.join(home, ".env")),
        "op_env": env_keys(os.path.join(home, ".op.env")),
        "managed": managed_env,
    },
}))
"#;

/// A present-but-nullable field: `Option<T>` that must still be SPELLED in
/// the JSON. serde's derive treats a missing `Option` as `None`; routing it
/// through `deserialize_with` makes absence an error, which is what "every
/// key required" means for [`ProjectionV1`].
fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

/// The projector's output, schema v1. `deny_unknown_fields` on every struct,
/// every key required, lists possibly empty.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectionV1 {
    pub(crate) config: ConfigView,
    pub(crate) managed_config_top_keys: Vec<String>,
    pub(crate) env_keys: EnvKeys,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigView {
    #[serde(deserialize_with = "nullable")]
    pub(crate) model_provider: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) model: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) model_base_url_host: Option<String>,
    pub(crate) providers: Vec<ProviderView>,
    pub(crate) custom_providers: Vec<CustomProviderView>,
    pub(crate) fallback_providers: Vec<String>,
    pub(crate) fallback_model: Vec<String>,
    pub(crate) auxiliary: BTreeMap<String, RouteView>,
    pub(crate) delegation: RouteView,
    pub(crate) credential_pool_strategies: BTreeMap<String, Option<String>>,
    pub(crate) secrets: BTreeMap<String, SecretSourceView>,
    pub(crate) plugins_enabled: Vec<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderView {
    #[serde(deserialize_with = "nullable")]
    pub(crate) key: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) name: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) base_url_host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomProviderView {
    #[serde(deserialize_with = "nullable")]
    pub(crate) name: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) base_url_host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RouteView {
    #[serde(deserialize_with = "nullable")]
    pub(crate) provider: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) base_url_host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretSourceView {
    pub(crate) enabled: bool,
    pub(crate) targets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvKeys {
    pub(crate) home: Vec<EnvKey>,
    pub(crate) op_env: Vec<EnvKey>,
    pub(crate) managed: Vec<EnvKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvKey {
    pub(crate) key: String,
    pub(crate) nonblank: bool,
}

/// Parse the projector's stdout against the strict schema.
pub(crate) fn parse_projection(stdout: &[u8]) -> Result<ProjectionV1> {
    serde_json::from_slice(stdout).context("output does not match the projection schema")
}

/// One projector run: `python -I -B -c PROJECTOR <home> <managed>`, with
/// `command` already carrying the child env of §4.4 step 4, stdin null, and a
/// [`PROJECTOR_TIMEOUT`] kill. Every failure is an `Err` naming why; the
/// caller renders it as `cannot audit {home}/config.yaml ({why})`.
pub(crate) fn run_projector(
    command: std::process::Command,
    home: &Path,
    managed_dir: Option<&Path>,
) -> Result<ProjectionV1> {
    run_projector_with(command, home, managed_dir, PROJECTOR_TIMEOUT)
}

fn run_projector_with(
    mut command: std::process::Command,
    home: &Path,
    managed_dir: Option<&Path>,
    timeout: Duration,
) -> Result<ProjectionV1> {
    command
        .arg("-I")
        .arg("-B")
        .arg("-c")
        .arg(PROJECTOR)
        .arg(home)
        .arg(managed_dir.map(Path::as_os_str).unwrap_or_default());
    let out = super::run_bounded(command, timeout, "the projector")?;
    if !out.status.success() {
        // Only the head of the last stderr line (a traceback's exception class,
        // or the projector's own `die` text): a YAML error's tail quotes the
        // offending line of config.yaml, which may be an `api_key`.
        let why = String::from_utf8_lossy(&out.stderr);
        let last = why
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .split(": ")
            .next()
            .unwrap_or("");
        bail!(
            "the projector exited {}{}",
            out.status
                .code()
                .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
            if last.is_empty() {
                String::new()
            } else {
                format!(": {}", last.chars().take(160).collect::<String>())
            }
        );
    }
    parse_projection(&out.stdout)
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_projector.rs"]
mod tests;
