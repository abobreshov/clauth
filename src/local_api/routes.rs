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
use crate::usage::collect::{CollectOpts, collect};
use crate::usage::observation::{
    AccountObservation, AuthKind, SCHEMA_VERSION, SourceId, sanitize_message,
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

/// Answer one request. The token is checked before anything else on TCP, so
/// an unauthenticated caller learns nothing about which paths exist; the unix
/// door is trusted by its filesystem permissions.
pub(crate) fn handle(ctx: &Ctx, req: &Request, door: Door) -> Response {
    if door == Door::Tcp && !super::token_matches(&ctx.token_path, req.bearer.as_deref()) {
        return Response::unauthorized();
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
}

/// `GET /v1/accounts`.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct AccountsBody {
    schema_version: u32,
    /// `AccountObservation` objects (see `tollgate usage --json`), redacted.
    #[schema(value_type = Vec<Object>)]
    accounts: Vec<AccountObservation>,
}

/// `GET /v1/accounts/{id}`.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct AccountBody {
    schema_version: u32,
    /// One `AccountObservation`, redacted.
    #[schema(value_type = Object)]
    account: AccountObservation,
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
        Some(account) => Response::serialize(
            200,
            &AccountBody {
                schema_version: SCHEMA_VERSION,
                account,
            },
        ),
        None => Response::error(404, "account_not_found"),
    }
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
        (status = 200, description = "the `~/.tollgate/status.json` feed (built on the spot when no daemon has published one)", body = crate::daemon::StatusBody),
        (status = 401, description = R401, body = ErrorBody),
        (status = 405, description = R405, body = ErrorBody),
        (status = 503, description = "no feed on disk and the config does not load (`status_unavailable`)", body = ErrorBody)
    ),
    security(("bearer" = []))
)]
fn status(ctx: &Ctx) -> Response {
    // The daemon's published feed, passed through untouched (a torn file does
    // not parse and falls through to the rebuild).
    if let Some((body, etag)) = crate::daemon::api::routes::read_feed_tagged(&ctx.status_path) {
        return Response::raw_json_tagged(200, body, etag);
    }
    let Ok(config) = crate::profile::load_config() else {
        return Response::error(503, "status_unavailable");
    };
    let body = crate::daemon::build_status(&config, config.state.refresh_interval_ms, None, false);
    Response::serialize(200, &body)
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
