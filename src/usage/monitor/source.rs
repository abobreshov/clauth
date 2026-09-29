//! The [`UsageSource`] trait, its registry, and the HTTP seam every monitor
//! fetch goes through (plan v3.1 §4.2).
//!
//! A source turns one resolved [`MonitorTarget`] into a [`Reading`] (the
//! figures) or a [`Failure`] (why not). It never writes: the cache layer
//! ([`super::cache`]) owns persistence, TTL, backoff and single-flight.
//!
//! All network access goes through [`MonitorHttp`], so tests inject a fake and
//! any real request from a test build panics ([`LiveHttp`]). The live bearer
//! GET enforces the monitoring allowlist before sending: only the Nous Portal
//! account and billing reads may carry a borrowed Hermes token, and no
//! redirect is ever followed.

use std::fmt;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::config::{MonitorConfig, MonitorKind};
use crate::providers::{Provider, ThirdPartyError, ThirdPartyStats, ThirdPartyTarget};
use crate::usage::observation::{
    AccountObservation, AuthKind, Failure, FailureKind, MoneyMeter, Origin, QuotaWindow,
    ScopeOrigin, SourceId, Timestamp, account_id,
};

// ── secrets ────────────────────────────────────────────────────────────────────

/// A credential held in memory for one fetch. No `Serialize`, and `Debug`
/// prints nothing of it, so it can never reach a cache, a log or a panic
/// message.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value, for the one header that sends it.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

// ── target ─────────────────────────────────────────────────────────────────────

/// A monitor resolved for one fetch: config plus the credential read from the
/// environment right now.
#[derive(Debug)]
pub(crate) struct MonitorTarget {
    pub(crate) provider: Option<Provider>,
    /// `billing_key_env`'s value when set and present, else `api_key_env`'s.
    pub(crate) key: Option<Secret>,
    /// The NAME of the variable [`Self::key`] was (or would have been) read
    /// from, for messages.
    pub(crate) key_env: Option<String>,
    /// [`Self::key`] is a monitoring-only credential (`billing_key_env`).
    pub(crate) monitoring_key: bool,
    /// `api_key_env`'s value whatever [`Self::key`] resolved to. OpenRouter
    /// needs both credentials apart: the inference key reads `/api/v1/key`,
    /// and a management key may only ever reach `/api/v1/credits`.
    pub(crate) api_key: Option<Secret>,
    /// `api_key_env` is configured (set or not): what the account's auth
    /// kind is judged by, so an observation read without the environment
    /// agrees with the fetch.
    pub(crate) api_key_configured: bool,
    /// The NAME of `billing_key_env` when that variable is set. Handed to the
    /// OpenRouter fetch, which reads the value itself for the one `/credits`
    /// call ([`crate::providers::billing_key`]).
    pub(crate) billing_key_env: Option<String>,
    /// Nous only: the Hermes home.
    pub(crate) hermes_home: PathBuf,
    /// The clock the fetch judges expiry and lapsed windows at.
    pub(crate) now_secs: i64,
}

/// Reads one env var by NAME; `None` when unset or blank.
pub(crate) type EnvReader<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process environment, the production [`EnvReader`].
pub(crate) fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Resolve `cfg` against the environment and `home`.
pub(crate) fn resolve_target(
    cfg: &MonitorConfig,
    home: &std::path::Path,
    now_secs: i64,
    env: EnvReader<'_>,
) -> MonitorTarget {
    let billing = cfg
        .billing_key_env
        .as_deref()
        .and_then(|n| env(n).map(|v| (n, v)));
    let billing_key_env = billing.as_ref().map(|(n, _)| (*n).to_string());
    let api_key = cfg.api_key_env.as_deref().and_then(env).map(Secret::new);
    let (key, key_env, monitoring_key) = match billing {
        Some((name, value)) => (Some(Secret::new(value)), Some(name.to_string()), true),
        None => {
            let name = cfg
                .api_key_env
                .as_deref()
                .or(cfg.billing_key_env.as_deref());
            (
                cfg.api_key_env.as_deref().and_then(env).map(Secret::new),
                name.map(str::to_string),
                false,
            )
        }
    };
    MonitorTarget {
        provider: cfg.typed_provider(),
        key,
        key_env,
        monitoring_key,
        api_key,
        api_key_configured: cfg.api_key_env.is_some(),
        billing_key_env,
        hermes_home: cfg.hermes_home_in(home),
        now_secs,
    }
}

impl MonitorTarget {
    /// OpenRouter with an inference key: the fetch runs on it (and the
    /// management key, if any, reads the wallet alone).
    fn openrouter_inference(&self) -> Option<&Secret> {
        (self.provider == Some(Provider::OpenRouter))
            .then_some(self.api_key.as_ref())
            .flatten()
    }

    /// The account reads as monitoring-only: a billing key, and not an
    /// OpenRouter monitor whose inference key runs the fetch.
    fn reads_read_only(&self) -> bool {
        self.monitoring_key
            && !(self.provider == Some(Provider::OpenRouter) && self.api_key_configured)
    }
}

// ── reading ────────────────────────────────────────────────────────────────────

/// What one successful fetch produced: the figures an observation carries,
/// and a verdict that rides with them (a spent balance, a meter the source
/// cannot read).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub(crate) struct Reading {
    pub(crate) plan: Option<String>,
    pub(crate) windows: Vec<QuotaWindow>,
    pub(crate) money: Vec<MoneyMeter>,
    pub(crate) best_effort: bool,
    /// Shown as the observation's failure when the latest attempt had none.
    pub(crate) verdict: Option<Failure>,
}

// ── sources ────────────────────────────────────────────────────────────────────

/// One kind of usage source.
pub(crate) trait UsageSource: Sync {
    /// Which integration the figures come from.
    fn source_id(&self, target: &MonitorTarget) -> SourceId;
    /// How the account authenticates, as far as this source can tell.
    fn auth_kind(&self, target: &MonitorTarget) -> AuthKind;
    /// Fetch the figures. Never writes; every failure is built with
    /// [`Failure::new`] so its message is sanitised.
    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure>;
}

/// The source for a monitor kind.
pub(crate) fn source_for(kind: MonitorKind) -> &'static dyn UsageSource {
    match kind {
        MonitorKind::Nous => &super::nous::NousSource,
        MonitorKind::OllamaCloud | MonitorKind::OpenRouter | MonitorKind::Provider => {
            &ProviderSource
        }
    }
}

/// An observation skeleton for `cfg`, with the source's id and auth kind.
pub(crate) fn skeleton(cfg: &MonitorConfig, target: &MonitorTarget) -> AccountObservation {
    let source = source_for(cfg.kind);
    let mut obs = AccountObservation::new(
        account_id(Origin::Monitor, &cfg.id),
        source.source_id(target),
        source.auth_kind(target),
        Origin::Monitor,
        cfg.display_label(),
    );
    obs.disabled = !cfg.enabled;
    obs
}

/// OpenRouter, Ollama Cloud and every other typed provider: the provider
/// module's own fetch (the same one the profile scheduler runs), keyed by the
/// monitor's key.
///
/// OpenRouter keeps its two credentials apart: with an inference key the
/// fetch reads `/api/v1/key` with it and hands the management key's NAME to
/// the `/credits` leg; with only a management key it reads the wallet alone
/// ([`MonitorHttp::openrouter_wallet`]), never `/api/v1/key`.
pub(crate) struct ProviderSource;

impl UsageSource for ProviderSource {
    fn source_id(&self, target: &MonitorTarget) -> SourceId {
        SourceId::from_provider(target.provider)
    }

    fn auth_kind(&self, target: &MonitorTarget) -> AuthKind {
        if target.reads_read_only() {
            AuthKind::ReadOnly
        } else {
            AuthKind::ApiKey
        }
    }

    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
        let Some(provider) = target.provider else {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "no typed provider configured",
            ));
        };
        let stats = if let Some(inference) = target.openrouter_inference() {
            let tp = ThirdPartyTarget::Known {
                provider,
                console: None,
                billing_key_env: target.billing_key_env.clone(),
            };
            http.third_party(&tp, inference)
        } else if provider == Provider::OpenRouter && target.monitoring_key {
            http.openrouter_wallet(require_key(target)?)
        } else {
            let tp = ThirdPartyTarget::Known {
                provider,
                console: None,
                billing_key_env: None,
            };
            http.third_party(&tp, require_key(target)?)
        }
        .map_err(|e| third_party_failure(e, target))?;
        let mut scratch = AccountObservation::new(
            String::new(),
            SourceId::from_provider(Some(provider)),
            AuthKind::ApiKey,
            Origin::Monitor,
            "",
        );
        crate::usage::project::apply_third_party(&mut scratch, &stats, target.now_secs);
        crate::providers::ollama_cloud::refine_observation(&mut scratch, Some(&stats));
        if target.monitoring_key && target.openrouter_inference().is_none() {
            for m in &mut scratch.money {
                m.scope_origin = ScopeOrigin::MonitoringCredential { bound: false };
            }
        }
        Ok(Reading {
            plan: scratch.plan,
            windows: scratch.windows,
            money: scratch.money,
            best_effort: scratch.best_effort,
            verdict: scratch.failure,
        })
    }
}

/// The key a keyed source needs, or an `AuthRequired` naming the variable.
pub(crate) fn require_key(target: &MonitorTarget) -> Result<&Secret, Failure> {
    target.key.as_ref().ok_or_else(|| {
        Failure::new(
            FailureKind::AuthRequired,
            &match &target.key_env {
                Some(name) => format!("${name} is not set in the environment tollgate runs in"),
                None => "no api_key_env configured".to_string(),
            },
        )
    })
}

/// A provider fetch error as a [`Failure`].
pub(crate) fn third_party_failure(e: ThirdPartyError, target: &MonitorTarget) -> Failure {
    let var = target.key_env.as_deref().unwrap_or("the key");
    match e {
        ThirdPartyError::AuthExpired => Failure::new(
            FailureKind::AuthRequired,
            &format!("the provider rejected the key in ${var}"),
        ),
        ThirdPartyError::RateLimited { retry_after } => {
            let mut f = Failure::new(FailureKind::RateLimited, "rate limited by the provider");
            f.retry_after = retry_at(retry_after, target);
            f
        }
        ThirdPartyError::QuotaExhausted { retry_after } => {
            let mut f = Failure::new(
                FailureKind::QuotaExhausted,
                "the provider reports the usage limit is reached",
            );
            f.retry_after = retry_at(retry_after, target);
            f
        }
        ThirdPartyError::ConsoleExpired => {
            Failure::new(FailureKind::ConsoleExpired, "console session needed")
        }
        ThirdPartyError::Network => {
            Failure::new(FailureKind::Unavailable, "could not reach the provider")
        }
        ThirdPartyError::Status => Failure::new(
            FailureKind::Unavailable,
            "the provider answered with an error status",
        ),
        ThirdPartyError::Parse => Failure::new(
            FailureKind::InvalidResponse,
            "the provider answered in an unknown shape",
        ),
    }
}

fn retry_at(after: Option<Duration>, target: &MonitorTarget) -> Option<Timestamp> {
    after.map(|d| {
        Timestamp::from_secs(
            target
                .now_secs
                .saturating_add(i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
        )
    })
}

// ── HTTP ───────────────────────────────────────────────────────────────────────

/// One HTTP answer.
#[derive(Debug, Clone)]
pub(crate) struct HttpReply {
    pub(crate) status: u16,
    pub(crate) body: String,
    /// `retry-after`, delta-seconds form.
    pub(crate) retry_after_secs: Option<u64>,
}

/// The network seam. Production is [`LiveHttp`]; tests pass a fake.
pub(crate) trait MonitorHttp: Sync {
    /// `GET url` with `Authorization: Bearer <token>`.
    fn get_bearer(&self, url: &str, token: &Secret) -> Result<HttpReply, Failure>;
    /// A typed provider's usage fetch with `key`.
    fn third_party(
        &self,
        target: &ThirdPartyTarget,
        key: &Secret,
    ) -> Result<ThirdPartyStats, ThirdPartyError>;
    /// OpenRouter's wallet alone (`GET /api/v1/credits`), read with a
    /// management key that must never reach any other path.
    fn openrouter_wallet(&self, key: &Secret) -> Result<ThirdPartyStats, ThirdPartyError>;
}

/// The Nous Portal origin the borrowed Hermes token may reach.
pub(crate) const NOUS_PORTAL_ORIGIN: &str = "https://portal.nousresearch.com";

/// Whether a bearer GET to `url` is on the monitoring allowlist (plan §4.2):
/// the Nous Portal's `/api/oauth/account` and `/api/billing/*` reads, and
/// nothing else — the borrowed token is never sent to the inference API or
/// any other origin. Userinfo, ports, `..` and query-smuggled paths are
/// refused.
pub(crate) fn bearer_url_allowed(url: &str) -> bool {
    let Some(path) = url.strip_prefix(NOUS_PORTAL_ORIGIN) else {
        return false;
    };
    if !path.starts_with('/') || path.contains("..") || path.contains(['@', '#', '\\']) {
        return false;
    }
    let path = path.split('?').next().unwrap_or("");
    path == "/api/oauth/account"
        || path
            .strip_prefix("/api/billing/")
            .is_some_and(|rest| !rest.is_empty())
}

/// Largest response body a monitor reads (plan §4.2: 2 MiB).
const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

/// A dedicated agent: status codes on the `Ok` side, and NO redirects, so a
/// bearer can never follow a 30x off the allowlisted origin.
static MONITOR_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(4)))
        .timeout_recv_response(Some(Duration::from_secs(8)))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
});

/// The real network.
pub(crate) struct LiveHttp;

impl MonitorHttp for LiveHttp {
    fn get_bearer(&self, url: &str, token: &Secret) -> Result<HttpReply, Failure> {
        if !bearer_url_allowed(url) {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "refused: that URL is not on the monitoring allowlist",
            ));
        }
        guard_test_network(url);
        let mut response = MONITOR_AGENT
            .get(url)
            .header("Authorization", &format!("Bearer {}", token.expose()))
            .header("Accept", "application/json")
            .call()
            .map_err(|_| Failure::new(FailureKind::Unavailable, "could not reach the source"))?;
        let status = response.status().as_u16();
        let retry_after_secs = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::usage::parse_retry_after)
            .map(|d| d.as_secs());
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|_| Failure::new(FailureKind::Unavailable, "could not read the response"))?;
        Ok(HttpReply {
            status,
            body,
            retry_after_secs,
        })
    }

    fn third_party(
        &self,
        target: &ThirdPartyTarget,
        key: &Secret,
    ) -> Result<ThirdPartyStats, ThirdPartyError> {
        guard_test_network(&target.throttle_key());
        crate::providers::fetch_third_party_usage(target, key.expose(), None)
    }

    fn openrouter_wallet(&self, key: &Secret) -> Result<ThirdPartyStats, ThirdPartyError> {
        guard_test_network("openrouter /api/v1/credits");
        crate::providers::openrouter::fetch_wallet_stats(key.expose())
    }
}

/// Tests are hermetic (plan §3.7): a test build that reaches the real network
/// is a bug in the test, so it fails loudly instead of sending.
fn guard_test_network(what: &str) {
    if cfg!(test) {
        panic!("a test reached the real network ({what}); pass a fake MonitorHttp");
    }
}

/// A scripted [`MonitorHttp`] for tests: records every call (url and the
/// credential it carried, so a test can prove which token was sent) and
/// answers from closures. The default answers panic, so a test that expects
/// no request fails on one.
#[cfg(test)]
pub(crate) struct FakeHttp {
    pub(crate) calls: std::sync::Mutex<Vec<String>>,
    #[allow(clippy::type_complexity)]
    pub(crate) bearer_reply: Box<dyn Fn(&str) -> Result<HttpReply, Failure> + Sync>,
    #[allow(clippy::type_complexity)]
    pub(crate) stats_reply: Box<dyn Fn() -> Result<ThirdPartyStats, ThirdPartyError> + Sync>,
}

#[cfg(test)]
impl FakeHttp {
    /// Panics on any request.
    pub(crate) fn offline() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            bearer_reply: Box::new(|url| panic!("unexpected bearer GET {url}")),
            stats_reply: Box::new(|| panic!("unexpected provider fetch")),
        }
    }

    /// Answers every bearer GET with `status` / `body`.
    pub(crate) fn bearer(status: u16, body: &str) -> Self {
        let body = body.to_string();
        Self {
            bearer_reply: Box::new(move |_| {
                Ok(HttpReply {
                    status,
                    body: body.clone(),
                    retry_after_secs: None,
                })
            }),
            ..Self::offline()
        }
    }

    /// Answers every provider fetch with `reply()`.
    pub(crate) fn stats(
        reply: impl Fn() -> Result<ThirdPartyStats, ThirdPartyError> + Sync + 'static,
    ) -> Self {
        Self {
            stats_reply: Box::new(reply),
            ..Self::offline()
        }
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
impl MonitorHttp for FakeHttp {
    fn get_bearer(&self, url: &str, token: &Secret) -> Result<HttpReply, Failure> {
        if let Ok(mut c) = self.calls.lock() {
            c.push(format!("GET {url} bearer={}", token.expose()));
        }
        (self.bearer_reply)(url)
    }

    fn third_party(
        &self,
        target: &ThirdPartyTarget,
        key: &Secret,
    ) -> Result<ThirdPartyStats, ThirdPartyError> {
        if let Ok(mut c) = self.calls.lock() {
            let billing = match target {
                ThirdPartyTarget::Known {
                    billing_key_env: Some(name),
                    ..
                } => format!(" billing={name}"),
                _ => String::new(),
            };
            c.push(format!(
                "PROVIDER {} key={}{billing}",
                target.throttle_key(),
                key.expose()
            ));
        }
        (self.stats_reply)()
    }

    fn openrouter_wallet(&self, key: &Secret) -> Result<ThirdPartyStats, ThirdPartyError> {
        if let Ok(mut c) = self.calls.lock() {
            c.push(format!("OPENROUTER_WALLET key={}", key.expose()));
        }
        (self.stats_reply)()
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_source.rs"]
mod tests;
