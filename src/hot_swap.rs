//! Same-provider hot swap for API-key accounts (executor B) and the pieces
//! every surface shares about it.
//!
//! A `tollgate start` session on an API-key account authenticates through
//! Claude Code's `apiKeyHelper`. Written in its session form
//! (`<exe> __tollgate-api-key --session <sid>`), the helper resolves the
//! member the session's registry row names on every run, so moving a session
//! to another account of the SAME transport class is only a row write: the
//! next helper run prints the new account's key. That is executor B. Executor
//! A (OAuth credential-link repoint) lives in [`crate::runtime::SessionSwap`]
//! and is untouched.
//!
//! B is chosen once, at spawn ([`choose`]), and only when the
//! `S1-APIKEYHELPER` gate is open for the installed Claude Code version: the
//! spike doc compiled in below carries a machine block that names the exact
//! versions the harness passed on ([`current_gate`]). Everything a spawn can
//! detect that would make the helper's output not the whole story (an OAuth
//! store beside the key, an env token, a local endpoint, a cloud provider, a
//! gateway login policy) registers the session `relaunch_only` instead, and a
//! switch there goes through `tollgate switch <sid> <p> --relaunch`.
//!
//! The three states every surface reports ([`SwapView`]) are keyed on
//! RECORDED helper runs, never on elapsed time: the spike (S1(a)) showed the
//! TTL refresh is lazy with no timer, so an idle session never runs the helper
//! and a clock-driven "stalled" would fire on every idle session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::live_sessions::LiveSession;
use crate::profile::{ModelSettings, Profile};

/// The TTL a B session's Claude Code re-runs its helper after, on the child
/// env only. A backstop: the commit's `settings.json` touch already makes the
/// next request run the helper synchronously (S1(c)).
pub(crate) const HELPER_TTL_MS: u64 = 30_000;

/// Claude Code's env key for [`HELPER_TTL_MS`].
pub(crate) const TTL_ENV_KEY: &str = "CLAUDE_CODE_API_KEY_HELPER_TTL_MS";

/// `TOLLGATE_HOT_SWAP=off` registers every API-key session relaunch-only. There
/// is no force-on: the gate block is the only way B turns on.
pub(crate) const KILL_SWITCH_ENV: &str = "TOLLGATE_HOT_SWAP";

/// The committed spike evidence. Its machine block is the gate.
const SPIKE_DOC: &str = include_str!("../docs/spikes/s1-apikeyhelper.md");

/// How a live session changes account without a restart. Recorded once, at
/// registration; a row without it derives one from its harness
/// ([`LiveSession::executor`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Executor {
    /// Executor A: the OAuth credential link repoint.
    Oauth,
    /// Executor B: only the key helper's output changes.
    ApiKey,
    /// Neither executor may move this session; `reason` is a code from
    /// [`reason_text`].
    RelaunchOnly { reason: String },
    /// A codex or Hermes row: no in-session executor at all.
    None,
}

impl Executor {
    /// The wire spelling surfaces report (`null` for [`Executor::None`]).
    pub(crate) fn wire(&self) -> Option<&'static str> {
        match self {
            Self::Oauth => Some("oauth"),
            Self::ApiKey => Some("api_key"),
            Self::RelaunchOnly { .. } => Some("relaunch_only"),
            Self::None => None,
        }
    }
}

/// Whether an `acquire` may choose executor B at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HotSwapPolicy {
    /// `tollgate start`.
    Allowed,
    /// The MCP delegate and every other acquirer: never B.
    Never,
}

// ── transport key ────────────────────────────────────────────────────────────

/// A parsed endpoint: what [`transport_key`] normalises and what the loopback
/// test reads the host of.
struct Endpoint {
    scheme: String,
    host: String,
    port: Option<String>,
    path: String,
    query: Option<String>,
}

fn parse_endpoint(url: &str) -> Option<Endpoint> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (inner, after) = v6.split_once(']')?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?.to_string()),
        };
        (format!("[{}]", inner.to_ascii_lowercase()), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_ascii_lowercase(), Some(p.to_string())),
            None => (authority.to_ascii_lowercase(), None),
        }
    };
    if host.is_empty() || host == "[]" {
        return None;
    }
    let tail = tail.split('#').next().unwrap_or_default();
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (tail, None),
    };
    Some(Endpoint {
        scheme: scheme.to_ascii_lowercase(),
        host,
        port: port.filter(|p| !p.is_empty()),
        path: path.trim_end_matches('/').to_string(),
        query,
    })
}

/// The transport identity of an endpoint URL: scheme and host lower-cased, the
/// default port (`:443` https, `:80` http) dropped, a trailing `/` dropped from
/// the path, the query kept verbatim and the fragment dropped. `None` when the
/// authority carries userinfo or there is no `://`, which is never executor B.
pub(crate) fn transport_key(url: &str) -> Option<String> {
    let e = parse_endpoint(url)?;
    let port = match (e.scheme.as_str(), e.port.as_deref()) {
        ("https", Some("443")) | ("http", Some("80")) | (_, None) => String::new(),
        (_, Some(p)) => format!(":{p}"),
    };
    let query = e.query.map(|q| format!("?{q}")).unwrap_or_default();
    Some(format!("{}://{}{port}{}{query}", e.scheme, e.host, e.path))
}

/// Whether the endpoint names this machine: `localhost`, `127.0.0.0/8` or
/// `::1`. Covers the shunt (`127.0.0.1:3001`) and the Ollama daemon.
pub(crate) fn is_loopback_endpoint(url: &str) -> bool {
    let Some(e) = parse_endpoint(url) else {
        return false;
    };
    if e.host == "localhost" {
        return true;
    }
    if let Some(inner) = e.host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return inner
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|ip| ip.is_loopback());
    }
    e.host
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback())
}

// ── launch class ─────────────────────────────────────────────────────────────

/// The transport class a B session launched with. Two members of one class are
/// interchangeable to a running Claude Code: only the key its helper prints
/// differs. Persisted in the row; carries no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaunchClass {
    pub(crate) version: u32,
    pub(crate) endpoint: String,
    pub(crate) models: ModelSettings,
    pub(crate) env_sha256: String,
    pub(crate) link_mode: String,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// OpenRouter workspace, `None` until P4-OR binds keys to one. A `None` on
    /// either side never refuses.
    #[serde(default)]
    pub(crate) workspace_id: Option<String>,
}

/// SHA-256 over the profile's custom env minus the endpoint key and minus a
/// blank api-key entry (blank is no auth source, so it cannot split a class).
pub(crate) fn env_sha256(env: &BTreeMap<String, String>) -> String {
    use sha2::Digest as _;
    let filtered: BTreeMap<&str, &str> = env
        .iter()
        .filter(|(k, v)| {
            k.as_str() != "ANTHROPIC_BASE_URL"
                && !(k.as_str() == "ANTHROPIC_API_KEY" && v.trim().is_empty())
        })
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let bytes = serde_json::to_vec(&filtered).unwrap_or_default();
    let digest = sha2::Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn provider_label(profile: &Profile) -> Option<String> {
    let value = serde_json::to_value(profile.provider?).ok()?;
    Some(value.as_str()?.to_ascii_lowercase())
}

impl LaunchClass {
    /// The class `profile` launches in on a host of this link mode, or `None`
    /// when its endpoint has no transport key (none, or userinfo in the URL).
    pub(crate) fn of(profile: &Profile, real_links: bool) -> Option<Self> {
        Some(Self {
            version: 1,
            endpoint: transport_key(profile.routing_endpoint()?)?,
            models: profile.models.clone(),
            env_sha256: env_sha256(&profile.env),
            link_mode: if real_links { "real" } else { "fake" }.to_string(),
            provider: provider_label(profile),
            workspace_id: None,
        })
    }
}

/// Whether `env` carries a non-blank auth token or api key: Claude Code would
/// send that instead of the helper's output.
fn has_auth_env(env: &BTreeMap<String, String>) -> bool {
    env.iter().any(|(k, v)| {
        matches!(k.as_str(), "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY") && !v.trim().is_empty()
    })
}

/// Config-level class check of `target` against a launch class: the first
/// axis that differs, as a `class_differs:*` (or `class_differs`-family)
/// refusal code. Pure; the executor adds the disk-side checks.
pub(crate) fn class_matches(launch: &LaunchClass, target: &Profile) -> Result<(), &'static str> {
    if !crate::claude::has_usable_api_key(target) {
        return Err("class_differs:no_api_key");
    }
    if has_auth_env(&target.env) {
        return Err("class_differs:auth_env");
    }
    let endpoint = target.routing_endpoint().and_then(transport_key);
    if endpoint.as_deref() != Some(launch.endpoint.as_str()) {
        return Err("class_differs:endpoint");
    }
    if target.models != launch.models {
        return Err("class_differs:models");
    }
    if env_sha256(&target.env) != launch.env_sha256 {
        return Err("class_differs:env");
    }
    // `workspace_id` is always `None` today; a `Some` on both sides that
    // differs is the only refusal.
    Ok(())
}

/// Executor B's check of a target member, config and disk: a read-only load
/// (never a repair), enabled, [`class_matches`], and no OAuth store beside
/// the key. Shared by the executor and `tollgate switch`'s pre-check.
pub(crate) fn check_target(
    name: &crate::profile::ProfileName,
    class: &LaunchClass,
) -> Result<(), &'static str> {
    let Ok(profile) = crate::profile::load_profile_read_only(name) else {
        return Err("not_configured");
    };
    if profile.is_disabled() {
        return Err("disabled");
    }
    class_matches(class, &profile)?;
    if crate::claude::credential_fingerprint(name)
        .iter()
        .any(Option::is_some)
    {
        return Err("class_differs:oauth_store");
    }
    Ok(())
}

// ── the S1 gate ──────────────────────────────────────────────────────────────

/// The machine block's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateResult {
    Pass,
    Fail,
    Pending,
}

/// The parsed `tollgate:s1-gate` block. `g_real_endpoints` and
/// `gateway_precondition` are recorded for the owner and never read by the B
/// decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Gate {
    pub(crate) result: GateResult,
    pub(crate) claude_code: Vec<String>,
    pub(crate) g_real_endpoints: Option<String>,
}

impl Gate {
    fn pending() -> Self {
        Self {
            result: GateResult::Pending,
            claude_code: Vec::new(),
            g_real_endpoints: None,
        }
    }
}

#[derive(Deserialize)]
struct GateBlock {
    result: String,
    #[serde(default)]
    claude_code: Vec<String>,
    #[serde(default)]
    g_real_endpoints: Option<String>,
}

const GATE_OPEN: &str = "<!-- tollgate:s1-gate";

/// Parse the gate block out of a spike doc. Missing or malformed reads as
/// PENDING, never as PASS.
pub(crate) fn parse_gate(doc: &str) -> Gate {
    let Some(start) = doc.find(GATE_OPEN) else {
        return Gate::pending();
    };
    let body = &doc[start + GATE_OPEN.len()..];
    let Some(end) = body.find("-->") else {
        return Gate::pending();
    };
    let Ok(block) = toml::from_str::<GateBlock>(&body[..end]) else {
        return Gate::pending();
    };
    let result = match block.result.as_str() {
        "PASS" => GateResult::Pass,
        "FAIL" => GateResult::Fail,
        _ => GateResult::Pending,
    };
    Gate {
        result,
        claude_code: block.claude_code,
        g_real_endpoints: block.g_real_endpoints,
    }
}

/// The compiled spike doc, for the test that pins it byte for byte.
#[cfg(test)]
pub(crate) fn spike_doc() -> &'static str {
    SPIKE_DOC
}

#[cfg(test)]
static GATE_OVERRIDE: std::sync::Mutex<Option<Gate>> = std::sync::Mutex::new(None);

/// Test-only replacement of the parsed gate block for the guard's life.
/// Borrows the home sandbox whose lock serializes it.
#[cfg(test)]
pub(crate) struct S1GateOverride<'a>(std::marker::PhantomData<&'a crate::testutil::HomeSandbox>);

#[cfg(test)]
impl<'a> S1GateOverride<'a> {
    pub(crate) fn new(_home: &'a crate::testutil::HomeSandbox, gate: Gate) -> Self {
        *GATE_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = Some(gate);
        Self(std::marker::PhantomData)
    }

    /// A PASS block listing exactly `versions`.
    pub(crate) fn pass(home: &'a crate::testutil::HomeSandbox, versions: &[&str]) -> Self {
        Self::new(
            home,
            Gate {
                result: GateResult::Pass,
                claude_code: versions.iter().map(|v| (*v).to_string()).collect(),
                g_real_endpoints: None,
            },
        )
    }
}

#[cfg(test)]
impl Drop for S1GateOverride<'_> {
    fn drop(&mut self) {
        *GATE_OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The gate in force: the compiled block, or a test's override.
pub(crate) fn current_gate() -> Gate {
    #[cfg(test)]
    if let Some(gate) = GATE_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return gate;
    }
    parse_gate(SPIKE_DOC)
}

/// What the gate says for the installed Claude Code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateStatus {
    Open,
    VersionUnknown,
    Pending,
    /// The block passed, but not on this version (its first token).
    VersionNotListed(String),
}

impl GateStatus {
    fn refusal(&self) -> Option<&'static str> {
        match self {
            Self::Open => None,
            Self::VersionUnknown => Some("cc_version_unknown"),
            Self::Pending => Some("s1_gate_pending"),
            Self::VersionNotListed(_) => Some("s1_gate_version"),
        }
    }
}

/// The first whitespace token of `claude --version` (`2.1.283 (Claude Code)`).
pub(crate) fn version_token(version: &str) -> Option<&str> {
    version.split_whitespace().next()
}

/// The gate's answer for `version` under `gate`.
pub(crate) fn gate_status_of(gate: &Gate, version: Option<&str>) -> GateStatus {
    let Some(token) = version.and_then(version_token) else {
        return GateStatus::VersionUnknown;
    };
    if gate.result != GateResult::Pass {
        return GateStatus::Pending;
    }
    if gate.claude_code.iter().any(|v| v == token) {
        GateStatus::Open
    } else {
        GateStatus::VersionNotListed(token.to_string())
    }
}

/// Whether `TOLLGATE_HOT_SWAP=off` is set.
pub(crate) fn kill_switch_on() -> bool {
    std::env::var(KILL_SWITCH_ENV).is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"))
}

// ── the Claude Code version cache ────────────────────────────────────────────

/// `~/.tollgate/cc-version.json`: `claude --version` keyed on the resolved
/// binary's `(path, mtime_ns, len)`, so a start runs the probe once per
/// install rather than once per launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CcVersionCache {
    pub(crate) version: u32,
    pub(crate) path: PathBuf,
    pub(crate) mtime_ns: u128,
    pub(crate) len: u64,
    pub(crate) cc_version: String,
}

pub(crate) fn cc_version_cache_path() -> Result<PathBuf> {
    Ok(crate::profile::tollgate_dir()?.join("cc-version.json"))
}

/// The `claude` a spawn would run, resolved on `PATH`.
fn resolve_claude() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        which::which("claude").ok()
    }
    #[cfg(not(windows))]
    {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("claude"))
            .find(|candidate| candidate.is_file())
    }
}

fn stat_key(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime, meta.len()))
}

/// A stand-in for `claude --version`.
#[cfg(test)]
pub(crate) type CcProbe = Box<dyn Fn() -> Option<String>>;

#[cfg(test)]
thread_local! {
    /// Test seam for the one `claude --version` run: tests never run a real
    /// `claude`. `None` answers "unknown".
    static CC_PROBE: std::cell::RefCell<Option<CcProbe>> =
        const { std::cell::RefCell::new(None) };
    static CC_PROBE_RUNS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn set_cc_probe(probe: Option<CcProbe>) {
    CC_PROBE.with(|p| *p.borrow_mut() = probe);
    CC_PROBE_RUNS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn cc_probe_runs() -> u32 {
    CC_PROBE_RUNS.with(std::cell::Cell::get)
}

fn run_cc_probe() -> Option<String> {
    #[cfg(test)]
    {
        CC_PROBE_RUNS.with(|c| c.set(c.get() + 1));
        CC_PROBE.with(|p| p.borrow().as_ref().and_then(|probe| probe()))
    }
    #[cfg(not(test))]
    {
        crate::plugin_probe::cc_version()
    }
}

/// The installed Claude Code version: the cache on an equal `(path, mtime_ns,
/// len)`, else one `claude --version` and a cache rewrite. `None` when no
/// `claude` resolves or the probe fails.
pub(crate) fn cached_cc_version() -> Option<String> {
    let path = resolve_claude()?;
    let (mtime_ns, len) = stat_key(&path)?;
    let cache = cc_version_cache_path().ok()?;
    if let Some(hit) = read_cc_cache(&cache)
        && hit.version == 1
        && hit.path == path
        && hit.mtime_ns == mtime_ns
        && hit.len == len
    {
        return Some(hit.cc_version);
    }
    let version = run_cc_probe()?;
    let record = CcVersionCache {
        version: 1,
        path,
        mtime_ns,
        len,
        cc_version: version.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&record)
        && let Err(e) = crate::profile::atomic_write_600(&cache, bytes)
    {
        crate::logline::logline!("tollgate: writing {} failed: {e}", cache.display());
    }
    Some(version)
}

/// The cached version without probing or stat-ing, for rendering a refusal.
pub(crate) fn cached_cc_version_lockfree() -> Option<String> {
    read_cc_cache(&cc_version_cache_path().ok()?).map(|c| c.cc_version)
}

fn read_cc_cache(path: &Path) -> Option<CcVersionCache> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The gate status for this start: kill switch aside, the S1 gate for the
/// installed version. Stats `claude` and may run `claude --version` once, so
/// it is computed outside every lock.
pub(crate) fn gate_status() -> GateStatus {
    gate_status_of(&current_gate(), cached_cc_version().as_deref())
}

// ── executor choice ──────────────────────────────────────────────────────────

/// Everything [`choose`] reads, gathered by the caller so the choice is pure.
pub(crate) struct ChoiceFacts<'a> {
    pub(crate) profile: &'a Profile,
    pub(crate) policy: HotSwapPolicy,
    pub(crate) kill_switch: bool,
    pub(crate) isolated: bool,
    pub(crate) real_links: bool,
    /// Any of `credential_fingerprint`'s three files exists.
    pub(crate) has_oauth_store: bool,
    /// `CLAUDE_CODE_USE_BEDROCK|VERTEX|FOUNDRY` non-empty in the env the child
    /// inherits.
    pub(crate) inherited_cloud_env: bool,
    /// A `forceLogin*` key in the base or managed settings.
    pub(crate) gateway_policy: bool,
    /// `None` when the caller never computed it (not an API-key `Allowed`
    /// launch at gather time).
    pub(crate) gate: Option<&'a GateStatus>,
}

const CLOUD_ENV_KEYS: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Whether `env` selects a cloud provider.
pub(crate) fn selects_cloud(env: &BTreeMap<String, String>) -> bool {
    CLOUD_ENV_KEYS
        .iter()
        .any(|k| env.get(*k).is_some_and(|v| !v.trim().is_empty()))
}

/// Whether this process's env selects a cloud provider (a child inherits it).
pub(crate) fn inherited_cloud_env() -> bool {
    CLOUD_ENV_KEYS
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// The executor a spawn registers, and its launch class. An OAuth account, and
/// anything not API-key shaped, is executor A exactly as today. An API-key
/// account is B only when every check passes; the FIRST failing one is the
/// recorded reason, in the order the spec fixes.
pub(crate) fn choose(facts: &ChoiceFacts<'_>) -> (Executor, Option<LaunchClass>) {
    let profile = facts.profile;
    if profile.is_oauth() || !crate::claude::has_usable_api_key(profile) {
        return (Executor::Oauth, None);
    }
    let class = LaunchClass::of(profile, facts.real_links);
    let endpoint = profile.routing_endpoint();
    let refuse = |code: &str| {
        (
            Executor::RelaunchOnly {
                reason: code.to_string(),
            },
            class.clone(),
        )
    };
    if facts.policy == HotSwapPolicy::Never {
        return refuse("delegate");
    }
    if facts.kill_switch {
        return refuse("kill_switch");
    }
    if facts.isolated {
        return refuse("isolated");
    }
    if !facts.real_links {
        return refuse("fake_links");
    }
    if facts.has_oauth_store {
        return refuse("hybrid_oauth_store");
    }
    let Some(endpoint) = endpoint.map(str::trim).filter(|e| !e.is_empty()) else {
        return refuse("no_endpoint");
    };
    if transport_key(endpoint).is_none() {
        return refuse("endpoint_userinfo");
    }
    if is_loopback_endpoint(endpoint) {
        return refuse("loopback_endpoint");
    }
    if has_auth_env(&profile.env) {
        return refuse("auth_env");
    }
    if selects_cloud(&profile.env) || facts.inherited_cloud_env {
        return refuse("cloud_env");
    }
    if facts.gateway_policy {
        return refuse("gateway_policy");
    }
    let gate = facts.gate.cloned().unwrap_or(GateStatus::VersionUnknown);
    if let Some(code) = gate.refusal() {
        return refuse(code);
    }
    (Executor::ApiKey, class)
}

const GATEWAY_KEYS: &[&str] = &[
    "forceLoginMethod",
    "forceLoginOrgUUID",
    "forceLoginGatewayUrl",
];

/// Whether a settings file carries a `forceLogin*` key.
pub(crate) fn settings_force_login(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    GATEWAY_KEYS.iter().any(|k| value.get(*k).is_some())
}

#[cfg(test)]
static MANAGED_SETTINGS_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_managed_settings_override(path: Option<PathBuf>) {
    *MANAGED_SETTINGS_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = path;
}

/// Claude Code's managed settings file for this OS. Tests read none unless
/// they pose one.
fn managed_settings_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        MANAGED_SETTINGS_OVERRIDE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    #[cfg(all(not(test), target_os = "macos"))]
    {
        Some(PathBuf::from(
            "/Library/Application Support/ClaudeCode/managed-settings.json",
        ))
    }
    #[cfg(all(not(test), windows))]
    {
        Some(PathBuf::from(
            r"C:\ProgramData\ClaudeCode\managed-settings.json",
        ))
    }
    #[cfg(all(not(test), not(target_os = "macos"), not(windows)))]
    {
        Some(PathBuf::from("/etc/claude-code/managed-settings.json"))
    }
}

/// A `forceLogin*` setting in the base `settings.json` or the managed file.
pub(crate) fn gateway_policy_in_force(claude_home: &Path) -> bool {
    settings_force_login(&claude_home.join("settings.json"))
        || managed_settings_path().is_some_and(|p| settings_force_login(&p))
}

// ── reason texts ─────────────────────────────────────────────────────────────

/// The human text for a relaunch-only or refusal code. `cc_version` feeds the
/// `s1_gate_version` line.
pub(crate) fn reason_text(code: &str, cc_version: Option<&str>) -> String {
    let fixed = match code {
        "s1_gate_pending" => "the S1 key-helper spike has not passed",
        "s1_gate_version" => {
            let v = cc_version.and_then(version_token).unwrap_or("this version");
            return format!(
                "the S1 spike has not passed for Claude Code {v}; re-run \
                 tools/spikes/s1/run.sh and add {v} to the gate block"
            );
        }
        "cc_version_unknown" => "the installed Claude Code version could not be read",
        "kill_switch" => "TOLLGATE_HOT_SWAP=off",
        "registry" => "its registry row could not be written",
        "fake_links" => "this host copies runtime trees",
        "isolated" => "an isolated session",
        "delegate" => "a delegate run",
        "hybrid_oauth_store" => "the account also stores an OAuth login",
        "auth_env" | "class_differs:auth_env" => "its env sets an auth token",
        "cloud_env" => "its env selects a cloud provider",
        "loopback_endpoint" => "a local gateway or daemon endpoint",
        "endpoint_userinfo" => "an endpoint with credentials in the URL",
        "no_endpoint" => "no custom endpoint",
        "gateway_policy" => "a forceLogin* setting is in effect",
        "class_differs:endpoint" => "a different endpoint",
        "class_differs:models" => "different model routing",
        "class_differs:env" => "different custom env",
        "class_differs:oauth_store" => "it also stores an OAuth login",
        "class_differs:no_api_key" => "it has no usable api key",
        "class_differs:workspace" => "a different OpenRouter workspace",
        "class_differs:harness" => "another harness",
        "disabled" => "it is disabled",
        "not_configured" => "it is not configured",
        "marker_held" => "its liveness marker is held by another process",
        "shutting_down" => "the session is shutting down",
        other => return other.to_string(),
    };
    fixed.to_string()
}

// ── helper ack ───────────────────────────────────────────────────────────────

/// A failed helper run, as the ack records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HelperFailure {
    pub(crate) generation: u64,
    pub(crate) code: String,
    pub(crate) at_ms: u64,
}

/// `live_sessions/<sid>.helper`: the last successful helper run (never
/// regresses) and the newest failed one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HelperAck {
    pub(crate) version: u32,
    pub(crate) generation: u64,
    #[serde(default)]
    pub(crate) member: Option<String>,
    #[serde(default)]
    pub(crate) served_at_ms: Option<u64>,
    #[serde(default)]
    pub(crate) last_failure: Option<HelperFailure>,
}

/// Parse an ack. Torn, foreign (not a regular file) or other-version bytes
/// read as no ack.
pub(crate) fn read_helper_ack_at(path: &Path) -> Option<HelperAck> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let ack: HelperAck = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (ack.version == 1).then_some(ack)
}

// ── the view every surface reads ─────────────────────────────────────────────

/// Where a session is in a switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SwapState {
    /// A switch is asked for and not committed yet.
    Requested,
    /// Committed; the helper has not served it.
    Swapping,
    /// The helper ran for this commit and failed.
    Stalled,
    /// The helper serves the committed member (or nothing is in flight).
    Served,
}

/// One member at one key generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub(crate) struct SwapPoint {
    pub(crate) member: String,
    pub(crate) generation: u64,
    pub(crate) at_ms: Option<u64>,
}

/// Requested → committed → served for one row, the single function every
/// surface uses. Keyed on recorded helper runs, never on elapsed time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwapView {
    pub(crate) executor: Executor,
    pub(crate) requested_member: Option<String>,
    pub(crate) committed: Option<SwapPoint>,
    pub(crate) served: Option<SwapPoint>,
    pub(crate) state: SwapState,
    /// `swapping` with no helper run recorded since the commit.
    pub(crate) idle: bool,
    /// The helper's failure code while `stalled`.
    pub(crate) stall_code: Option<String>,
}

impl SwapView {
    pub(crate) fn of(row: &LiveSession, ack: Option<&HelperAck>) -> Self {
        let executor = row.executor();
        let current = row
            .current_member
            .clone()
            .unwrap_or_else(|| row.start_profile.clone());
        let requested = row
            .intended_member
            .clone()
            .filter(|intended| *intended != current);
        match executor {
            Executor::None => Self {
                executor,
                requested_member: None,
                committed: None,
                served: None,
                state: SwapState::Served,
                idle: false,
                stall_code: None,
            },
            Executor::ApiKey => Self::api_key(row, ack, current, requested),
            Executor::Oauth | Executor::RelaunchOnly { .. } => {
                let point = SwapPoint {
                    member: current,
                    generation: row.key_generation.unwrap_or(0),
                    at_ms: row.last_swap_at,
                };
                Self {
                    executor,
                    state: if requested.is_some() {
                        SwapState::Requested
                    } else {
                        SwapState::Served
                    },
                    requested_member: requested,
                    committed: Some(point.clone()),
                    served: Some(point),
                    idle: false,
                    stall_code: None,
                }
            }
        }
    }

    fn api_key(
        row: &LiveSession,
        ack: Option<&HelperAck>,
        current: String,
        requested: Option<String>,
    ) -> Self {
        let committed = SwapPoint {
            member: current,
            generation: row.key_generation.unwrap_or(0),
            at_ms: row.committed_at,
        };
        // No successful ack: the launch member at generation 0 is what the
        // session was built to serve (and, at generation 0, is committed).
        let served = ack
            .and_then(|a| {
                Some(SwapPoint {
                    member: a.member.clone()?,
                    generation: a.generation,
                    at_ms: a.served_at_ms,
                })
            })
            .unwrap_or_else(|| SwapPoint {
                member: row.start_profile.clone(),
                generation: 0,
                at_ms: None,
            });
        let failure = ack.and_then(|a| a.last_failure.as_ref());
        let (state, stall_code) = if requested.is_some() {
            (SwapState::Requested, None)
        } else if served.generation >= committed.generation {
            (SwapState::Served, None)
        } else if let Some(f) = failure.filter(|f| f.generation >= committed.generation) {
            (SwapState::Stalled, Some(f.code.clone()))
        } else {
            (SwapState::Swapping, None)
        };
        let since = committed.at_ms.unwrap_or(0);
        let ran_since = ack.is_some_and(|a| {
            a.served_at_ms.is_some_and(|t| t >= since)
                || a.last_failure.as_ref().is_some_and(|f| f.at_ms >= since)
        });
        Self {
            executor: Executor::ApiKey,
            requested_member: requested,
            idle: state == SwapState::Swapping && !ran_since,
            committed: Some(committed),
            served: Some(served),
            state,
            stall_code,
        }
    }

    /// The member the session's requests authenticate as: attribution reads
    /// this, never the committed member (until the helper serves, Claude Code
    /// still sends the previous key, S1(d)).
    pub(crate) fn served_member(&self) -> Option<&str> {
        self.served.as_ref().map(|p| p.member.as_str())
    }
}

/// The member a live row is attributed to on every surface: the served
/// member for a B row (read off its ack), `current_member` else the launch
/// profile for everything else.
pub(crate) fn attributed_member(row: &LiveSession) -> String {
    if row.executor() == Executor::ApiKey {
        let ack = crate::live_sessions::read_helper_ack(&row.session_id);
        if let Some(member) = SwapView::of(row, ack.as_ref()).served_member() {
            return member.to_string();
        }
    }
    row.current_member
        .clone()
        .unwrap_or_else(|| row.start_profile.clone())
}

/// The serialisable per-session view the read surfaces render. Carries no
/// claude argv (there is none on the row to carry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub(crate) struct LiveSessionView {
    pub(crate) session_id: String,
    /// `claude`, `codex` or `hermes`.
    pub(crate) harness: String,
    pub(crate) start_profile: String,
    /// `oauth`, `api_key` or `relaunch_only`; `null` for a codex row.
    #[schema(required = true, value_type = Option<String>)]
    pub(crate) executor: Option<&'static str>,
    /// The code a `relaunch_only` row registered with.
    #[schema(required = true)]
    pub(crate) relaunch_reason: Option<String>,
    /// A switch asked for and not committed yet.
    #[schema(required = true)]
    pub(crate) requested_member: Option<String>,
    /// What the session committed to; `null` for a codex row.
    #[schema(required = true)]
    pub(crate) committed: Option<SwapPoint>,
    /// What the session's requests authenticate as; `null` for a codex row.
    #[schema(required = true)]
    pub(crate) served: Option<SwapPoint>,
    pub(crate) state: SwapState,
    /// Present (`true`) only on a `swapping` view with no helper run recorded
    /// since the commit.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) idle: bool,
}

impl LiveSessionView {
    pub(crate) fn of(row: &LiveSession, ack: Option<&HelperAck>) -> Self {
        let view = SwapView::of(row, ack);
        Self {
            session_id: row.session_id.clone(),
            harness: row.harness.to_string(),
            start_profile: row.start_profile.clone(),
            executor: view.executor.wire(),
            relaunch_reason: match &view.executor {
                Executor::RelaunchOnly { reason } => Some(reason.clone()),
                _ => None,
            },
            requested_member: view.requested_member,
            committed: view.committed,
            served: view.served,
            state: view.state,
            idle: view.idle,
        }
    }

    /// Whether the view names `member` as committed or served: the local
    /// API's per-account filter.
    pub(crate) fn involves(&self, member: &str) -> bool {
        [&self.committed, &self.served]
            .into_iter()
            .flatten()
            .any(|point| point.member == member)
    }
}

/// Every running session's view, oldest first, for the read-only surfaces.
///
/// Lock-free: each row and ack sidecar is one rename-atomic read, and a row
/// whose supervisor pid is gone (a crashed session awaiting GC) is left out by
/// a signal-0 probe, which neither writes nor takes a lock. No `load_config`.
pub(crate) fn live_session_views() -> Vec<LiveSessionView> {
    let mut rows: Vec<LiveSession> = crate::live_sessions::list()
        .into_iter()
        .filter(|row| supervisor_alive(row.pid))
        .collect();
    rows.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    rows.iter()
        .map(|row| {
            let ack = (row.executor() == Executor::ApiKey)
                .then(|| crate::live_sessions::read_helper_ack(&row.session_id))
                .flatten();
            LiveSessionView::of(row, ack.as_ref())
        })
        .collect()
}

/// `kill(pid, 0)` on unix; every row counts elsewhere (no probe to ask).
fn supervisor_alive(pid: u32) -> bool {
    if cfg!(unix) {
        crate::runtime::namespaced_keychain_ledger::pid_alive(pid)
    } else {
        true
    }
}

// ── the session helper ───────────────────────────────────────────────────────

/// How long the helper tries its ack lock before skipping the record.
const ACK_LOCK_TRIES: u32 = 10;
const ACK_LOCK_SLEEP: std::time::Duration = std::time::Duration::from_millis(20);

/// Which member and generation a session helper run serves, from the row
/// (rename-atomic, lock-free), else the last successful ack, else the start
/// profile encoded in `CLAUDE_CONFIG_DIR`.
///
/// Only an executor-B row moves the key off its launch profile: executor B is
/// the only writer that commits an api-key member. Any other row serves its
/// launch profile at generation 0, so a `current_member` another executor
/// wrote (an OAuth swap's member) never reaches this helper's stdout.
///
/// An executor-B row also hands back its launch class: the helper re-checks
/// the member against it before printing, since the class was checked only
/// when the switch committed and the member's endpoint can change since.
fn helper_target(sid: &str) -> Result<(String, u64, Option<LaunchClass>), &'static str> {
    if let Some(row) = crate::live_sessions::get(sid) {
        if row.executor() != Executor::ApiKey {
            return Ok((row.start_profile, 0, None));
        }
        let class = row.launch_class.clone();
        let member = row.current_member.unwrap_or(row.start_profile);
        return Ok((member, row.key_generation.unwrap_or(0), class));
    }
    if let Some(ack) = crate::live_sessions::read_helper_ack(sid)
        && let Some(member) = ack.member
    {
        return Ok((member, ack.generation, None));
    }
    start_profile_of_config_dir(sid)
        .map(|start| (start, 0, None))
        .ok_or("no_row")
}

/// The `<start>` of a `CLAUDE_CONFIG_DIR` that is exactly
/// `<tollgate_dir>/profiles/<start>/runtime-<sid>` for this `sid`.
fn start_profile_of_config_dir(sid: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("CLAUDE_CONFIG_DIR")?);
    let name = dir.file_name()?.to_str()?;
    if name.strip_prefix("runtime-")? != sid {
        return None;
    }
    let profile_dir = dir.parent()?;
    let start = profile_dir.file_name()?.to_str()?;
    let root = crate::profile::tollgate_dir().ok()?.join("profiles");
    if profile_dir.parent()? != root || !crate::claude::is_profile_name_token(start) {
        return None;
    }
    Some(start.to_string())
}

/// The slice of a member's `config.toml` the helper reads: the key, and
/// what [`class_matches`] compares.
#[derive(Deserialize)]
struct HelperConfig {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    models: crate::profile::ModelSettings,
}

#[cfg(test)]
thread_local! {
    /// Counts `config.toml` reads by the session helper, for the pin that it
    /// never goes through `load_profile`.
    pub(crate) static HELPER_CONFIG_READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The member's key from its `config.toml`, one read, no `load_profile`.
/// With the session's launch `class`, the same bytes must still match it
/// ([`class_matches`]): a member whose endpoint, models or env changed since
/// the commit would hand its key to the wrong provider, so the run fails
/// with that code and the session shows as stalled instead.
fn helper_key(member: &str, class: Option<&LaunchClass>) -> Result<String, &'static str> {
    #[cfg(test)]
    HELPER_CONFIG_READS.with(|c| c.set(c.get() + 1));
    let path = crate::profile::profile_dir(&crate::profile::ProfileName::from(member))
        .map_err(|_| "config_unreadable")?
        .join("config.toml");
    let raw = std::fs::read_to_string(&path).map_err(|_| "config_unreadable")?;
    let parsed: HelperConfig = toml::from_str(&raw).map_err(|_| "config_unreadable")?;
    if let Some(class) = class {
        let mut profile = Profile::new(
            member.to_string(),
            parsed.base_url.clone(),
            parsed.api_key.clone(),
        );
        profile.env = parsed.env.clone();
        profile.models = parsed.models.clone();
        class_matches(class, &profile)?;
    }
    let key = parsed.api_key.unwrap_or_default();
    let key = key.trim();
    if key.is_empty() {
        return Err("no_key");
    }
    crate::claude::validate_api_key(key).map_err(|_| "invalid_key")?;
    Ok(key.to_string())
}

/// What one helper run did, for the ack.
enum RunOutcome {
    Served { member: String, generation: u64 },
    Failed { generation: u64, code: &'static str },
}

/// `tollgate __tollgate-api-key --session <sid>`: print the committed
/// member's key and record the run. No `load_profile`, no state flock, no
/// network. `Err` = exit 1 with nothing on stdout.
pub(crate) fn run_session_helper(sid: &str, out: &mut dyn std::io::Write) -> Result<()> {
    if !crate::runtime::is_session_id(sid) {
        anyhow::bail!("not a session id: {sid:?}");
    }
    let outcome = match helper_target(sid) {
        Err(code) => RunOutcome::Failed {
            generation: 0,
            code,
        },
        Ok((member, generation, class)) => match helper_key(&member, class.as_ref()) {
            Err(code) => RunOutcome::Failed { generation, code },
            Ok(key) => {
                let printed = out.write_all(key.as_bytes()).and_then(|()| out.flush());
                match printed {
                    Ok(()) => RunOutcome::Served { member, generation },
                    Err(_) => RunOutcome::Failed {
                        generation,
                        code: "stdout_write",
                    },
                }
            }
        },
    };
    record_run(sid, &outcome);
    match outcome {
        RunOutcome::Served { .. } => Ok(()),
        RunOutcome::Failed { code, .. } => {
            anyhow::bail!("session '{sid}' key helper failed ({code})")
        }
    }
}

/// Record a run in `<sid>.helper` under `<sid>.helper.lock` (rank
/// `HelperAck`, a leaf). The lock is tried for about 200 ms, then the record
/// is skipped: the next run records.
fn record_run(sid: &str, outcome: &RunOutcome) {
    let (Ok(ack_path), Ok(lock_path)) = (
        crate::live_sessions::helper_ack_path(sid),
        crate::live_sessions::helper_lock_path(sid),
    ) else {
        return;
    };
    let Some(dir) = ack_path.parent() else {
        return;
    };
    if crate::profile::mkdir_700(dir).is_err() {
        return;
    }
    let _rank = crate::lockorder::RankGuard::enter::<crate::lockorder::rank::HelperAck>();
    let Ok(lock) = crate::profile::open_state_file(&lock_path) else {
        return;
    };
    let mut locked = false;
    for _ in 0..ACK_LOCK_TRIES {
        if lock.try_lock().is_ok() {
            locked = true;
            break;
        }
        std::thread::sleep(ACK_LOCK_SLEEP);
    }
    if !locked {
        return;
    }
    let existing = read_helper_ack_at(&ack_path);
    let now = crate::usage::now_ms();
    let next = match outcome {
        RunOutcome::Served { member, generation } => {
            let generation = *generation;
            let current = existing.as_ref().is_some_and(|a| {
                a.generation >= generation
                    && a.member.is_some()
                    && a.last_failure
                        .as_ref()
                        .is_none_or(|f| f.generation > generation)
            });
            if current {
                None
            } else {
                let mut ack = existing.clone().unwrap_or(HelperAck {
                    version: 1,
                    generation: 0,
                    member: None,
                    served_at_ms: None,
                    last_failure: None,
                });
                // The success fields never regress: an N-1 run finishing
                // after N leaves N's standing.
                if !(ack.member.is_some() && ack.generation > generation) {
                    ack.generation = generation;
                    ack.member = Some(member.clone());
                    ack.served_at_ms = Some(now);
                }
                if ack
                    .last_failure
                    .as_ref()
                    .is_some_and(|f| f.generation <= generation)
                {
                    ack.last_failure = None;
                }
                Some(ack)
            }
        }
        RunOutcome::Failed { generation, code } => {
            let mut ack = existing.clone().unwrap_or(HelperAck {
                version: 1,
                generation: 0,
                member: None,
                served_at_ms: None,
                last_failure: None,
            });
            let newer_standing = ack
                .last_failure
                .as_ref()
                .is_some_and(|f| f.generation > *generation);
            if newer_standing {
                None
            } else {
                ack.last_failure = Some(HelperFailure {
                    generation: *generation,
                    code: (*code).to_string(),
                    at_ms: now,
                });
                Some(ack)
            }
        }
    };
    if let Some(ack) = next {
        write_ack(&ack_path, &ack);
    }
    let _ = lock.unlock();
}

/// Replace the ack by rename of `<sid>.helper.tmp.<pid>` (0600).
fn write_ack(path: &Path, ack: &HelperAck) {
    let Ok(bytes) = serde_json::to_vec(ack) else {
        return;
    };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let written = {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&tmp).and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(&bytes)
        })
    };
    if written.and_then(|()| std::fs::rename(&tmp, path)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The ack's path for a test fixture.
#[cfg(test)]
pub(crate) fn write_ack_for_test(sid: &str, ack: &HelperAck) {
    let path = crate::live_sessions::helper_ack_path(sid).expect("ack path");
    if let Some(dir) = path.parent() {
        crate::profile::mkdir_700(dir).expect("registry dir");
    }
    write_ack(&path, ack);
}

/// Read a session's runtime `settings.json` env and compare the transport it
/// carries with the launch class: Claude Code hot-reloads that env (S1(c)),
/// so a drift means the session no longer runs the class it launched with.
pub(crate) fn runtime_settings_drift(settings: &Path, class: &LaunchClass) -> Option<&'static str> {
    let Ok(bytes) = std::fs::read(settings) else {
        return Some("class_differs:endpoint");
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Some("class_differs:endpoint");
    };
    let env = value.get("env");
    let get = |k: &str| env.and_then(|e| e.get(k)).and_then(|v| v.as_str());
    let endpoint = get("ANTHROPIC_BASE_URL").and_then(transport_key);
    if endpoint.as_deref() != Some(class.endpoint.as_str()) {
        return Some("class_differs:endpoint");
    }
    let models = [
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", &class.models.opus),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", &class.models.sonnet),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &class.models.haiku),
        ("ANTHROPIC_DEFAULT_FABLE_MODEL", &class.models.fable),
        ("CLAUDE_CODE_SUBAGENT_MODEL", &class.models.subagent),
    ];
    if models.iter().any(|(k, want)| get(k) != want.as_deref()) {
        return Some("class_differs:models");
    }
    None
}

/// Move a runtime `settings.json`'s mtime without writing a byte: any change
/// to that file drops Claude Code's cached key (S1(c)), so the next request
/// runs the helper before it is sent.
///
/// The runtime copy is always a regular file tollgate wrote. A symlink there
/// is refused rather than followed, so the touch can never reach the file it
/// points at (the operator's `~/.claude/settings.json`, say).
pub(crate) fn touch_settings(path: &Path) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    anyhow::ensure!(
        !std::fs::symlink_metadata(path)
            .with_context(|| format!("failed to stat {}", path.display()))?
            .file_type()
            .is_symlink(),
        "{} is a symlink",
        path.display()
    );
    let file = opts
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.set_modified(std::time::SystemTime::now())
        .with_context(|| format!("failed to touch {}", path.display()))
}

#[cfg(test)]
#[path = "../tests/inline/hot_swap.rs"]
mod tests;
