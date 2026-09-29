//! Gather every [`AccountObservation`] tollgate can produce, from caches only.
//!
//! [`collect`] reads the same on-disk caches `tollgate list` and
//! `tollgate status --json` read — the per-profile `usage_cache.json` /
//! `third_party_cache.json` — and NEVER fetches. It writes nothing at all: the
//! roster comes through [`crate::profile::load_config_read_only`], which
//! creates no directory, tightens no mode and adopts no staged rotation, so a
//! poll of the local agent API leaves the data dir byte-identical. It never
//! touches `~/.claude` or `~/.clauth`.
//!
//! Order of the result: claude profiles (config order), codex profiles (roster
//! order), then every [`MONITOR_SOURCES`] hook in slice order, then every
//! [`UPSTREAM_SOURCES`] hook. Ids are unique: a later observation whose id an
//! earlier one already holds is dropped.
//!
//! ## Extension points
//!
//! A new source that has no `profiles.toml` / `codex-profiles.toml` profile
//! (a monitoring-only key, Hermes local state, upstream clauth's caches) is
//! one `fn(&CollectCtx) -> Vec<AccountObservation>` appended to one of the two
//! slices below. The hook must read caches only (no network; the scheduler
//! owns fetching), must not write, and must build ids with
//! [`super::observation::account_id`] under its origin (`monitor:` /
//! `upstream:`).

use super::observation::{
    AccountObservation, AuthKind, Failure, FailureKind, Freshness, Origin, SourceId, Timestamp,
    account_id,
};
use super::project::{apply_codex_usage, apply_oauth_usage, apply_third_party};
use crate::codex_profiles::CodexState;
use crate::profile::{AppConfig, Profile};
use crate::profile_cache::{
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, profile_cache_mtime_ms,
};
use crate::profile_json::{OauthAge, oauth_age, stale_after_ms};
use crate::providers::ThirdPartyStats;
use crate::usage::UsageInfo;

/// What a source hook sees.
pub(crate) struct CollectCtx<'a> {
    /// The loaded `profiles.toml` config; `None` when it could not be read.
    pub(crate) config: Option<&'a AppConfig>,
    /// The loaded codex roster (empty when absent or unreadable).
    pub(crate) codex: &'a CodexState,
    /// The clock every projection judges at, epoch ms.
    pub(crate) now_ms: u64,
    /// The refresh cadence staleness is judged against, ms.
    pub(crate) interval_ms: u64,
    /// Upstream clauth owns `~/.claude` (identity::upstream_active).
    pub(crate) guest_mode: bool,
    /// The caller asked for disabled profiles too.
    pub(crate) include_disabled: bool,
}

impl CollectCtx<'_> {
    /// [`Self::now_ms`] in epoch seconds.
    pub(crate) fn now_secs(&self) -> i64 {
        i64::try_from(self.now_ms / 1000).unwrap_or(i64::MAX)
    }

    /// Freshness of figures read at `observed_ms` against this context's
    /// clock and cadence (`None` = nothing read).
    pub(crate) fn freshness_of(&self, observed_ms: Option<u64>) -> Freshness {
        self.freshness_at_cadence(observed_ms, self.interval_ms)
    }

    /// [`Self::freshness_of`] against a source's own cadence (a monitor's
    /// `ttl_secs`) instead of the profile refresh interval.
    pub(crate) fn freshness_at_cadence(
        &self,
        observed_ms: Option<u64>,
        interval_ms: u64,
    ) -> Freshness {
        match observed_ms {
            None => Freshness::NotFetched,
            Some(at) => match self.now_ms.checked_sub(at) {
                Some(age) if age <= stale_after_ms(interval_ms) => Freshness::Fresh,
                // A future stamp proves the clock moved, not that it is fresh.
                _ => Freshness::Stale {
                    since: Some(Timestamp::from_ms(at)),
                },
            },
        }
    }
}

/// A source hook: produce observations from caches, given the context.
pub(crate) type SourceHook = fn(&CollectCtx<'_>) -> Vec<AccountObservation>;

/// Monitoring-only sources (`monitor:<id>` accounts: an unbound OpenRouter
/// management key, an Ollama monitor key, Hermes local state, …).
///
/// **Feature agents: append your hook here**, one line per source, e.g.
/// `crate::usage::ollama::monitor_observations,`. Keep the slice in a stable
/// order — it is the output order.
pub(crate) static MONITOR_SOURCES: &[SourceHook] = &[
    crate::usage::monitor::monitor_observations,
    // append monitor hooks here
];

/// Upstream-clauth sources (`upstream:<profile>` accounts read from
/// `~/.clauth` caches, read-only; relevant while `ctx.guest_mode`).
///
/// **Feature agents: append your hook here.**
pub(crate) static UPSTREAM_SOURCES: &[SourceHook] = &[
    crate::usage::upstream::upstream_observations,
    // append upstream hooks here
];

/// What [`collect`] gathers.
#[derive(Debug, Clone, Default)]
pub(crate) struct CollectOpts {
    /// Also include user-disabled claude profiles (the active one is always
    /// included).
    pub(crate) include_disabled: bool,
    /// Keep only the account whose id or label equals this.
    pub(crate) account: Option<String>,
    /// Keep only accounts whose source ([`SourceId::as_str`]) or provider
    /// display name equals this, case-insensitively.
    pub(crate) provider: Option<String>,
}

/// Every observation tollgate can make right now, from caches. Never fails: an
/// unreadable config yields no claude profiles, an unreadable codex roster no
/// codex profiles.
pub(crate) fn collect(opts: &CollectOpts) -> Vec<AccountObservation> {
    let config = crate::profile::load_config_read_only().ok();
    let codex = CodexState::load().unwrap_or_default();
    let interval_ms = config
        .as_ref()
        .map(|c| c.state.refresh_interval_ms)
        .unwrap_or(crate::profile::AppState::default().refresh_interval_ms);
    let ctx = CollectCtx {
        config: config.as_ref(),
        codex: &codex,
        now_ms: crate::usage::now_ms(),
        interval_ms,
        guest_mode: crate::identity::upstream_active(),
        include_disabled: opts.include_disabled,
    };
    collect_with(&ctx, opts, MONITOR_SOURCES, UPSTREAM_SOURCES)
}

/// [`collect`] over an explicit context and hook set — what tests drive.
pub(crate) fn collect_with(
    ctx: &CollectCtx<'_>,
    opts: &CollectOpts,
    monitors: &[SourceHook],
    upstream: &[SourceHook],
) -> Vec<AccountObservation> {
    let mut all: Vec<AccountObservation> = Vec::new();
    if let Some(config) = ctx.config {
        all.extend(
            config
                .profiles
                .iter()
                .filter(|p| ctx.include_disabled || !p.is_disabled() || config.is_active(&p.name))
                .map(|p| observe_profile(ctx, config, p)),
        );
    }
    all.extend(
        ctx.codex
            .profiles()
            .iter()
            .map(|name| observe_codex(ctx, name.as_str())),
    );
    all.extend(hook_observations(ctx, monitors, upstream));
    let mut seen = std::collections::HashSet::new();
    all.retain(|o| seen.insert(o.id.clone()));
    all.retain(|o| matches_filters(o, opts));
    all
}

/// Every hook's observations (`monitor:` then `upstream:`), first id wins:
/// the part of [`collect_with`] a surface that lists profiles itself (the
/// TUI's Usage rail) reads on its own, so both see the same accounts.
pub(crate) fn hook_observations(
    ctx: &CollectCtx<'_>,
    monitors: &[SourceHook],
    upstream: &[SourceHook],
) -> Vec<AccountObservation> {
    let mut seen = std::collections::HashSet::new();
    monitors
        .iter()
        .chain(upstream)
        .flat_map(|hook| hook(ctx))
        .filter(|o| seen.insert(o.id.clone()))
        .collect()
}

fn matches_filters(o: &AccountObservation, opts: &CollectOpts) -> bool {
    let account_ok = opts
        .account
        .as_deref()
        .is_none_or(|a| o.id == a || o.label == a);
    let provider_ok = opts.provider.as_deref().is_none_or(|p| {
        o.source.as_str().eq_ignore_ascii_case(p) || o.provider.eq_ignore_ascii_case(p)
    });
    account_ok && provider_ok
}

/// One `profiles.toml` profile, OAuth or api-key, off its own cache.
pub(crate) fn observe_profile(
    ctx: &CollectCtx<'_>,
    config: &AppConfig,
    p: &Profile,
) -> AccountObservation {
    let name = &p.name;
    let third_party = p.usage_cache_is_third_party();
    let source = if third_party {
        SourceId::from_provider(p.provider)
    } else {
        SourceId::AnthropicOauth
    };
    let auth = match (third_party, p.credentials.is_some()) {
        (false, _) => AuthKind::Subscription,
        (true, true) => AuthKind::Hybrid,
        (true, false) => AuthKind::ApiKey,
    };
    let mut obs = AccountObservation::new(
        account_id(Origin::Profile, name.as_str()),
        source,
        auth,
        Origin::Profile,
        name.as_str(),
    );
    obs.active = config.is_active(name);
    obs.disabled = p.is_disabled();
    obs.endpoint = p.base_url.clone();
    obs.plan = crate::profile_json::tier_label(p);
    obs.failure = profile_failure(config, p);
    let now_secs = ctx.now_secs();

    if third_party {
        let mtime = profile_cache_mtime_ms(name, THIRD_PARTY_CACHE_FILE);
        let stats = load_profile_cache::<ThirdPartyStats>(name, THIRD_PARTY_CACHE_FILE);
        if let Some(stats) = &stats {
            apply_third_party(&mut obs, stats, now_secs);
            obs.observed_at = mtime.map(Timestamp::from_ms);
            obs.freshness = ctx.freshness_of(mtime);
        }
        crate::providers::ollama_cloud::refine_observation(&mut obs, stats.as_ref());
        return obs;
    }

    let usage = load_profile_cache::<UsageInfo>(name, USAGE_CACHE_FILE);
    match oauth_age(usage.as_ref(), ctx.now_ms) {
        OauthAge::Absent => {}
        OauthAge::Undated => obs.freshness = Freshness::Stale { since: None },
        OauthAge::Dated(_) => {
            let at = usage.as_ref().and_then(|u| u.fetched_at);
            obs.observed_at = at.map(Timestamp::from_ms);
            obs.freshness = ctx.freshness_of(at);
        }
    }
    if let Some(usage) = &usage {
        apply_oauth_usage(&mut obs, usage, now_secs);
    }
    obs
}

/// The dead-credential verdicts a profile carries on disk, most actionable
/// first: the chain quarantined its OAuth pair, or its usage credential was
/// rejected (the api key, or Alibaba's console session).
fn profile_failure(config: &AppConfig, p: &Profile) -> Option<Failure> {
    let name = &p.name;
    if config.is_auth_broken(name) {
        return Some(Failure::new(
            FailureKind::AuthRequired,
            &format!("login expired; run `tollgate login {name}`"),
        ));
    }
    let rejected = crate::usage::profile_credential_fingerprint(p)
        .is_some_and(|fp| crate::profile_cache::auth_expired_matches(name, fp));
    if !rejected {
        return None;
    }
    Some(
        if p.console.is_some() || p.provider == Some(crate::providers::Provider::Alibaba) {
            Failure::new(
                FailureKind::ConsoleExpired,
                &format!("console login needed; run `tollgate login {name}`"),
            )
        } else {
            Failure::new(FailureKind::AuthRequired, "api key rejected")
        },
    )
}

/// One codex profile off its `usage_cache.json`.
pub(crate) fn observe_codex(ctx: &CollectCtx<'_>, name: &str) -> AccountObservation {
    let mut obs = AccountObservation::new(
        account_id(Origin::CodexProfile, name),
        SourceId::Codex,
        AuthKind::Subscription,
        Origin::CodexProfile,
        name,
    );
    obs.active = ctx
        .codex
        .active_profile()
        .is_some_and(|a| a.as_str() == name);
    let typed = crate::profile::ProfileName::from(name);
    let mtime = profile_cache_mtime_ms(&typed, USAGE_CACHE_FILE);
    let usage = load_profile_cache::<UsageInfo>(&typed, USAGE_CACHE_FILE);
    obs.plan = crate::codex_auth::plan_label(
        name,
        usage
            .as_ref()
            .and_then(|u| u.plan.as_ref())
            .and_then(|p| p.codex_plan.as_deref()),
    );
    if crate::codex_auth::read_quarantine(name).is_some() {
        obs.failure = Some(Failure::new(
            FailureKind::AuthRequired,
            &format!("codex login expired; run `tollgate login {name}`"),
        ));
    }
    if let Some(usage) = &usage {
        apply_codex_usage(&mut obs, usage, ctx.now_secs());
        obs.observed_at = mtime.map(Timestamp::from_ms);
        obs.freshness = ctx.freshness_of(mtime);
    }
    obs
}

#[cfg(test)]
#[path = "../../tests/inline/usage_collect.rs"]
mod tests;
