//! The local agent API's routes: auth, the route table, the handlers, the
//! redaction every observation passes through, and the OpenAPI document.
//!
//! Every handler reads caches (the collector, `status.json`) and nothing
//! else. Bodies carry `schema_version` ([`SCHEMA_VERSION`], the observation
//! model's) so an agent can refuse a shape newer than it knows.

use std::path::PathBuf;

use serde::Serialize;

use super::Door;
use crate::daemon::api::http::{ErrorBody, Request, Response};
use crate::daemon::api::routes::decode_segment;
use crate::hot_swap::{LiveSessionView, live_session_views};
use crate::usage::collect::{CollectOpts, collect};
use crate::usage::observation::{
    AccountObservation, AuthKind, SCHEMA_VERSION, SourceId, redact_credentials, sanitize_message,
};
use crate::usage::report::UsageReport;

/// What every handler shares.
pub(crate) struct Ctx {
    /// `~/.tollgate/api-token`, re-read per TCP request.
    pub(crate) token_path: PathBuf,
    /// The status feed `/v1/status` passes through.
    pub(crate) status_path: PathBuf,
}

/// The routes. Matching is exact: no trailing slash, no prefix match.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Health,
    Accounts,
    Account(String),
    Usage,
    Providers,
    Status,
    OpenApi,
}

/// The route `path` names, or `None` for an unknown path (404). An account id
/// is one percent-decoded segment (`/v1/accounts/claude%3Awork` and
/// `/v1/accounts/claude:work` are the same account).
pub(crate) fn route_of(path: &str) -> Option<Route> {
    Some(match path {
        "/v1/health" => Route::Health,
        "/v1/accounts" => Route::Accounts,
        "/v1/usage" => Route::Usage,
        "/v1/providers" => Route::Providers,
        "/v1/status" => Route::Status,
        "/v1/openapi.json" => Route::OpenApi,
        _ => {
            let segment = path.strip_prefix("/v1/accounts/")?;
            if segment.is_empty() || segment.contains('/') {
                return None;
            }
            Route::Account(decode_segment(segment)?)
        }
    })
}

/// Whether a `Host` value names this machine's loopback: `localhost` or a
/// loopback IP literal (`127.0.0.1`, any of 127.0.0.0/8, `[::1]`), each with an
/// optional numeric port. Anything else is a name a browser resolved to
/// loopback (DNS rebinding) or a request meant for another server.
pub(crate) fn loopback_host(raw: &str) -> bool {
    let raw = raw.trim();
    let (host, port) = if let Some(rest) = raw.strip_prefix('[') {
        let Some((inside, after)) = rest.split_once(']') else {
            return false;
        };
        let port = match after {
            "" => None,
            _ => match after.strip_prefix(':') {
                Some(port) => Some(port),
                None => return false,
            },
        };
        match inside.parse::<std::net::Ipv6Addr>() {
            Ok(ip) if ip.is_loopback() => return port.is_none_or(valid_port),
            _ => return false,
        }
    } else {
        match raw.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (raw, None),
        }
    };
    let named = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback());
    named && port.is_none_or(valid_port)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok()
}

/// Answer one request. On TCP the `Host` is checked first (a browser tricked by
/// DNS rebinding into calling loopback still sends the attacker's name, so it
/// is refused whatever token it carries), then the token, so an unauthenticated
/// caller learns nothing about which paths exist; the unix door is trusted by
/// its filesystem permissions and may send any `Host` or none.
pub(crate) fn handle(ctx: &Ctx, req: &Request, door: Door) -> Response {
    if door == Door::Tcp {
        match req.host.as_deref() {
            None => return Response::error(400, "host_required"),
            Some(host) if !loopback_host(host) => {
                return Response::error(421, "misdirected_request");
            }
            Some(_) => {}
        }
        if !super::token_matches(&ctx.token_path, req.bearer.as_deref()) {
            return Response::unauthorized();
        }
    }
    let Some(route) = route_of(&req.path) else {
        return Response::error(404, "not_found");
    };
    if req.method != "GET" {
        return Response::error(405, "method_not_allowed");
    }
    match route {
        Route::Health => health(),
        Route::Accounts => accounts(req),
        Route::Account(id) => account(&id),
        Route::Usage => usage(req),
        Route::Providers => providers(),
        Route::Status => status(ctx),
        Route::OpenApi => openapi_document(),
    }
}

// ── Redaction ─────────────────────────────────────────────────────────────────

/// Mask anything credential-shaped an observation could carry. The producers
/// already promise no credential reaches the model (`Failure::new` sanitises,
/// `endpoint` "never carries a credential"); this is the second line, applied
/// on the way out of the process so no future producer can leak through the
/// API by forgetting the first.
///
/// Free text goes through [`sanitize_message`]; the endpoint loses userinfo,
/// query and fragment, and any token-shaped path segment. Ids and profile
/// names are kept: they are the handles an agent looks accounts up by, and a
/// profile name is operator-chosen, never a key.
pub(crate) fn redact(obs: &mut AccountObservation) {
    obs.plan = obs.plan.as_deref().map(sanitize_message);
    obs.endpoint = obs.endpoint.as_deref().map(redact_endpoint);
    if let Some(f) = obs.failure.as_mut() {
        f.message = sanitize_message(&f.message);
    }
    for w in &mut obs.windows {
        w.label = sanitize_message(&w.label);
    }
    for m in &mut obs.money {
        m.label = sanitize_message(&m.label);
        m.scope_id = m.scope_id.as_deref().map(sanitize_message);
    }
    if let Some(e) = obs.estimate.as_mut() {
        e.basis = sanitize_message(&e.basis);
    }
}

/// `scheme://host[:port]/path` with userinfo, query and fragment dropped and
/// every token-shaped path segment replaced by `[redacted]`.
pub(crate) fn redact_endpoint(raw: &str) -> String {
    let raw = raw.split(['?', '#']).next().unwrap_or_default();
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, raw),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let path: Vec<String> = path
        .split('/')
        .map(|segment| {
            if segment.is_empty() || sanitize_message(segment) == segment {
                segment.to_string()
            } else {
                "[redacted]".to_string()
            }
        })
        .collect();
    let path = path.join("/");
    match scheme {
        Some(scheme) => format!("{scheme}://{host}{path}"),
        None => format!("{host}{path}"),
    }
}

/// Every observation the collector makes, redacted. The one read path the
/// HTTP routes and the MCP `usage` tool share.
pub(crate) fn observations(opts: &CollectOpts) -> Vec<AccountObservation> {
    let mut all = collect(opts);
    all.iter_mut().for_each(redact);
    all
}

/// `tollgate usage --json`'s envelope over [`observations`].
pub(crate) fn usage_report(opts: &CollectOpts) -> UsageReport {
    UsageReport::new(
        observations(opts),
        crate::usage::now_epoch_secs(),
        crate::identity::upstream_active(),
    )
}

/// The collector filters from a query string: `all=1`, `account=`,
/// `provider=` (values percent-decoded; an undecodable value matches nothing).
fn opts_from(req: &Request) -> CollectOpts {
    let decoded = |key: &str| {
        req.param(key)
            .filter(|v| !v.is_empty())
            .map(|v| decode_segment(v).unwrap_or_else(|| "\u{0}".to_string()))
    };
    CollectOpts {
        include_disabled: req.flag("all"),
        account: decoded("account"),
        provider: decoded("provider"),
    }
}

// ── Bodies ────────────────────────────────────────────────────────────────────

/// `GET /v1/health`.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct HealthBody {
    ok: bool,
    version: String,
    /// The observation model's schema version (`1`).
    schema_version: u32,
    /// Upstream clauth owns `~/.claude` on this machine (plan §4.0).
    guest_mode: bool,
    /// Where an `import clauth` of upstream's accounts stands.
    import: ImportBlock,
}

/// The `import` block of `GET /v1/health` and `GET /v1/status` (import spec
/// §2.5): the import journal's state, read without a lock.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ImportBlock {
    /// `none`, `pre`, `in_progress`, `complete`, `rolling_back`,
    /// `rolled_back`, `aborted`, or `unreadable` for a journal that does not
    /// parse.
    state: &'static str,
    /// RFC 3339 instant the import committed; `null` until it has.
    completed_at: Option<String>,
}

impl ImportBlock {
    pub(crate) fn current() -> Self {
        let (state, completed_at) = crate::identity::import_summary();
        Self {
            state: state.as_str(),
            completed_at,
        }
    }
}

/// `GET /v1/accounts`.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct AccountsBody {
    schema_version: u32,
    /// `AccountObservation` objects (see `tollgate usage --json`), redacted.
    #[schema(value_type = Vec<Object>)]
    accounts: Vec<AccountObservation>,
    /// Every running `tollgate start` session, with where it is in a switch
    /// (requested, committed, served). Additive under schema version 1.
    live_sessions: Vec<LiveSessionView>,
}

/// `GET /v1/accounts/{id}`.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct AccountBody {
    schema_version: u32,
    /// One `AccountObservation`, redacted.
    #[schema(value_type = Object)]
    account: AccountObservation,
    /// The running sessions whose committed or served member is this account.
    live_sessions: Vec<LiveSessionView>,
}

/// `GET /v1/usage`: byte for byte the `tollgate usage --json` envelope.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct UsageBody {
    schema_version: u32,
    /// RFC 3339 instant the report was assembled.
    generated_at: String,
    guest_mode: bool,
    #[schema(value_type = Vec<Object>)]
    accounts: Vec<AccountObservation>,
}

impl From<UsageReport> for UsageBody {
    fn from(r: UsageReport) -> Self {
        Self {
            schema_version: r.schema_version,
            generated_at: r.generated_at,
            guest_mode: r.guest_mode,
            accounts: r.accounts,
        }
    }
}

/// One row of `GET /v1/providers`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ProviderEntry {
    /// The source id observations carry (`anthropic_oauth`, `openrouter`, …).
    source: &'static str,
    display_name: &'static str,
    /// The `auth` kinds an account of this source can have.
    auth_kinds: Vec<&'static str>,
    /// At least one account of this source exists (disabled ones count).
    configured: bool,
    /// How many.
    accounts: usize,
}

/// `GET /v1/providers`.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct ProvidersBody {
    schema_version: u32,
    providers: Vec<ProviderEntry>,
}

/// Every source, in catalog order.
pub(crate) const CATALOG: &[SourceId] = &[
    SourceId::AnthropicOauth,
    SourceId::Codex,
    SourceId::Ollama,
    SourceId::OllamaCloud,
    SourceId::OpenRouter,
    SourceId::Nous,
    SourceId::Hermes,
    SourceId::DeepSeek,
    SourceId::Zai,
    SourceId::MiniMax,
    SourceId::Alibaba,
    SourceId::Grok,
    SourceId::Antigravity,
    SourceId::Generic,
    SourceId::UpstreamClauth,
];

/// How an account of `source` can authenticate (plan §4.3 / §4.7). The match
/// is exhaustive on purpose: a new source cannot compile without a row here.
pub(crate) fn auth_kinds(source: SourceId) -> &'static [AuthKind] {
    use AuthKind::{ApiKey, Hybrid, NativeLogin, ReadOnly, Subscription};
    match source {
        SourceId::AnthropicOauth | SourceId::Codex | SourceId::Grok | SourceId::Antigravity => {
            &[Subscription]
        }
        SourceId::Ollama | SourceId::Hermes => &[NativeLogin],
        SourceId::OllamaCloud => &[ApiKey, ReadOnly],
        SourceId::OpenRouter => &[ApiKey, Hybrid, ReadOnly],
        // The Nous monitor reads Hermes' own login, or a Nous API key.
        SourceId::Nous => &[NativeLogin, ApiKey],
        // A `provider` monitor on a monitoring-only key reads as ReadOnly.
        SourceId::DeepSeek | SourceId::Zai | SourceId::MiniMax | SourceId::Alibaba => {
            &[ApiKey, Hybrid, ReadOnly]
        }
        SourceId::Generic => &[ApiKey, Hybrid],
        SourceId::UpstreamClauth => &[ReadOnly],
    }
}

fn auth_kind_str(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::Subscription => "subscription",
        AuthKind::ApiKey => "api_key",
        AuthKind::Hybrid => "hybrid",
        AuthKind::NativeLogin => "native_login",
        AuthKind::ReadOnly => "read_only",
    }
}

/// The catalog against what is observed right now.
pub(crate) fn provider_catalog(observed: &[AccountObservation]) -> Vec<ProviderEntry> {
    CATALOG
        .iter()
        .map(|&source| {
            let accounts = observed.iter().filter(|o| o.source == source).count();
            ProviderEntry {
                source: source.as_str(),
                display_name: source.display_name(),
                auth_kinds: auth_kinds(source)
                    .iter()
                    .map(|&k| auth_kind_str(k))
                    .collect(),
                configured: accounts > 0,
                accounts,
            }
        })
        .collect()
}

// ── Handlers ──────────────────────────────────────────────────────────────────

const R401: &str = "TCP without `Authorization: Bearer <~/.tollgate/api-token>` (`unauthorized`)";
const R405: &str = "any method but GET (`method_not_allowed`)";

#[utoipa::path(
    get,
    path = "/v1/health",
    responses(
        (status = 200, description = "the build, the schema version and guest mode", body = HealthBody),
        (status = 401, description = R401, body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn health() -> Response {
    Response::serialize(
        200,
        &HealthBody {
            ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            schema_version: SCHEMA_VERSION,
            guest_mode: crate::identity::upstream_active(),
            import: ImportBlock::current(),
        },
    )
}

#[utoipa::path(
    get,
    path = "/v1/accounts",
    params(
        ("all" = Option<bool>, Query, description = "include disabled profiles (`all=1`)"),
        ("account" = Option<String>, Query, description = "only the account with this id (`claude:work`) or name"),
        ("provider" = Option<String>, Query, description = "only this source (`openrouter`) or provider name")
    ),
    responses(
        (status = 200, description = "every account's observation, from caches", body = AccountsBody),
        (status = 401, description = R401, body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn accounts(req: &Request) -> Response {
    Response::serialize(
        200,
        &AccountsBody {
            schema_version: SCHEMA_VERSION,
            accounts: observations(&opts_from(req)),
            live_sessions: live_session_views(),
        },
    )
}

#[utoipa::path(
    get,
    path = "/v1/accounts/{id}",
    params(("id" = String, Path, description = "an account id (`claude:work`, `codex:main`, `monitor:or`) or profile name; disabled accounts included")),
    responses(
        (status = 200, description = "one account's observation", body = AccountBody),
        (status = 401, description = R401, body = ErrorBody),
        (status = 404, description = "no such account (`account_not_found`)", body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn account(id: &str) -> Response {
    let found = observations(&CollectOpts {
        include_disabled: true,
        account: Some(id.to_string()),
        provider: None,
    });
    // An exact id beats a profile name that happens to equal it.
    let pick = found
        .iter()
        .position(|o| o.id == id)
        .or((!found.is_empty()).then_some(0));
    match pick.and_then(|i| found.into_iter().nth(i)) {
        Some(account) => {
            let live_sessions = account_live_sessions(&account);
            Response::serialize(
                200,
                &AccountBody {
                    schema_version: SCHEMA_VERSION,
                    account,
                    live_sessions,
                },
            )
        }
        None => Response::error(404, "account_not_found"),
    }
}

/// The live sessions an account's body lists: a Claude Code profile's
/// committed or served sessions, a codex or Hermes profile's sessions by
/// launch profile (such a row has neither), and none for a monitor or
/// upstream account.
fn account_live_sessions(account: &AccountObservation) -> Vec<LiveSessionView> {
    use crate::usage::observation::Origin;
    let Some((_, name)) = account.id.split_once(':') else {
        return Vec::new();
    };
    let harness = match account.origin {
        Origin::Profile => crate::harness::Harness::Claude,
        Origin::CodexProfile => crate::harness::Harness::Codex,
        Origin::HermesProfile => crate::harness::Harness::Hermes,
        _ => return Vec::new(),
    };
    live_session_views()
        .into_iter()
        .filter(|view| view.harness == harness.as_str())
        .filter(|view| view.involves(name) || (view.served.is_none() && view.start_profile == name))
        .collect()
}

#[utoipa::path(
    get,
    path = "/v1/usage",
    params(
        ("all" = Option<bool>, Query, description = "include disabled profiles (`all=1`)"),
        ("account" = Option<String>, Query, description = "only the account with this id or name"),
        ("provider" = Option<String>, Query, description = "only this source or provider name")
    ),
    responses(
        (status = 200, description = "the `tollgate usage --json` envelope", body = UsageBody),
        (status = 401, description = R401, body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn usage(req: &Request) -> Response {
    Response::serialize(200, &UsageBody::from(usage_report(&opts_from(req))))
}

#[utoipa::path(
    get,
    path = "/v1/providers",
    responses(
        (status = 200, description = "every source tollgate knows, and whether an account of it exists", body = ProvidersBody),
        (status = 401, description = R401, body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn providers() -> Response {
    let observed = collect(&CollectOpts {
        include_disabled: true,
        ..CollectOpts::default()
    });
    Response::serialize(
        200,
        &ProvidersBody {
            schema_version: SCHEMA_VERSION,
            providers: provider_catalog(&observed),
        },
    )
}

#[utoipa::path(
    get,
    path = "/v1/status",
    responses(
        (status = 200, description = "the `~/.tollgate/status.json` feed, redacted (built on the spot when no daemon has published a parseable one), plus an `import` object shaped like `ImportBlock`", body = crate::daemon::StatusBody),
        (status = 401, description = R401, body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody),
        (status = 503, description = "no parseable feed on disk and the config does not load (`status_unavailable`)", body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn status(ctx: &Ctx) -> Response {
    // The daemon's published feed, parsed; a torn or unparseable file falls
    // through to the rebuild. Never passed through as bytes: whatever is on
    // disk is parsed, redacted and re-serialised, so the file's contents reach
    // an agent only through [`redact_status`].
    let published = std::fs::read(&ctx.status_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let mut value = match published {
        Some(value) => value,
        None => {
            // Read-only: a poll must not create dirs, retighten modes or adopt
            // a staged rotation (`load_config` does all three).
            let Ok(config) = crate::profile::load_config_read_only() else {
                return Response::error(503, "status_unavailable");
            };
            let body =
                crate::daemon::build_status(&config, config.state.refresh_interval_ms, None, false);
            match serde_json::to_value(&body) {
                Ok(value) => value,
                Err(_) => return Response::error(500, "internal"),
            }
        }
    };
    redact_status(&mut value);
    if let (serde_json::Value::Object(map), Ok(block)) =
        (&mut value, serde_json::to_value(ImportBlock::current()))
    {
        map.insert("import".to_string(), block);
    }
    match serde_json::to_vec(&value) {
        // Tagged off the bytes actually served, never the file's.
        Ok(bytes) => {
            let etag = crate::daemon::api::routes::etag_for(&bytes);
            Response::raw_json_tagged(200, bytes, etag)
        }
        Err(_) => Response::error(500, "internal"),
    }
}

/// Keys whose string values are the handles an agent looks accounts up by
/// (profile names, a chain of them). Kept verbatim, as the observation routes
/// keep ids and labels: a profile name is operator-chosen, never a key, and a
/// long one is the same shape a token is.
const STATUS_HANDLE_KEYS: &[&str] = &[
    "name",
    "active_profile",
    "active_codex_profile",
    "pending_switch",
    "fallback_chain",
    "codex_fallback_chain",
];

/// Whether a JSON key names a credential, so its string value is dropped
/// whatever it looks like.
fn credential_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    matches!(
        key.as_str(),
        "api_key"
            | "apikey"
            | "x_api_key"
            | "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "session_token"
            | "bearer"
            | "authorization"
            | "cookie"
            | "password"
            | "secret"
            | "client_secret"
            | "credentials"
    ) || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
        || key.ends_with("_api_key")
}

/// An absolute path with every token-shaped segment masked, the rule
/// [`redact_endpoint`] applies to a URL's path.
fn redact_path(raw: &str) -> String {
    raw.split('/')
        .map(|segment| {
            if segment.is_empty() || redact_credentials(segment) == segment {
                segment.to_string()
            } else {
                "[redacted]".to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Free text from the status feed with every credential-shaped word masked: a
/// URL through [`redact_endpoint`] (userinfo, query, token-shaped segments), a
/// path segment by segment, the word after `Bearer`, and any other word through
/// [`redact_credentials`]. Control characters are dropped and whitespace
/// collapsed, as [`sanitize_message`] does.
fn redact_text(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for word in cleaned.split_whitespace() {
        out.push(if redact_next {
            "[redacted]".to_string()
        } else if word.contains("://") {
            redact_endpoint(word)
        } else if word.starts_with('/') || word.starts_with('~') {
            redact_path(word)
        } else {
            redact_credentials(word)
        });
        redact_next = word.eq_ignore_ascii_case("bearer");
    }
    out.join(" ")
}

fn redact_status_value(value: &mut serde_json::Value, key: Option<&str>) -> bool {
    match value {
        serde_json::Value::String(raw) => {
            let clean = if key.is_some_and(credential_key) {
                "[redacted]".to_string()
            } else if key.is_some_and(|k| STATUS_HANDLE_KEYS.contains(&k)) {
                return false;
            } else {
                redact_text(raw)
            };
            if clean == *raw {
                return false;
            }
            *raw = clean;
            true
        }
        // An array's elements are values of the key that holds it.
        serde_json::Value::Array(items) => items.iter_mut().fold(false, |changed, item| {
            redact_status_value(item, key) | changed
        }),
        serde_json::Value::Object(map) => map.iter_mut().fold(false, |changed, (k, v)| {
            redact_status_value(v, Some(k.as_str())) | changed
        }),
        _ => false,
    }
}

/// Mask everything credential-shaped anywhere in a status body: the value of
/// any credential-named key, and every credential-shaped word of every other
/// string (`profiles[].base_url` included, through [`redact_endpoint`]), bar
/// the [`STATUS_HANDLE_KEYS`] handles. The status feed's producers carry no
/// credential; this is the second line, applied on the way out of the process
/// the way [`redact`] is for observations. `true` when anything changed.
pub(crate) fn redact_status(body: &mut serde_json::Value) -> bool {
    redact_status_value(body, None)
}

/// The local API's OpenAPI document.
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "tollgate local agent API",
        description = "Read-only JSON over loopback HTTP (bearer from ~/.tollgate/api-token) or the unix socket ~/.tollgate/api.sock (no token). See docs/agent-api.md."
    ),
    paths(health, accounts, account, usage, providers, status, openapi_document),
    modifiers(&BearerScheme)
)]
struct LocalApiDoc;

struct BearerScheme;

impl utoipa::Modify for BearerScheme {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                utoipa::openapi::security::SecurityScheme::Http(
                    utoipa::openapi::security::HttpBuilder::new()
                        .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                        .description(Some(
                            "Authorization: Bearer <contents of ~/.tollgate/api-token>; not \
                             needed over the unix socket",
                        ))
                        .build(),
                ),
            );
        }
    }
}

/// The document as pretty JSON bytes.
pub(crate) fn openapi_document_bytes() -> Result<Vec<u8>, String> {
    <LocalApiDoc as utoipa::OpenApi>::openapi()
        .to_pretty_json()
        .map(String::into_bytes)
        .map_err(|e| format!("failed to serialize the local API OpenAPI document: {e}"))
}

#[utoipa::path(
    get,
    path = "/v1/openapi.json",
    responses(
        (status = 200, description = "this API's OpenAPI document", body = serde_json::Value, content_type = "application/json"),
        (status = 401, description = R401, body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn openapi_document() -> Response {
    match openapi_document_bytes() {
        Ok(document) => Response::raw_json(200, document),
        Err(_) => Response::error(500, "internal"),
    }
}

#[cfg(test)]
#[path = "../../tests/inline/local_api_routes.rs"]
mod tests;
