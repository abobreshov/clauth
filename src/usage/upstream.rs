//! The upstream read-only view (plan v3.1 §4.0 guest mode): while upstream
//! clauth owns `~/.claude`, the owner's real Claude (and codex) accounts live
//! in upstream's roster, not tollgate's. This projects upstream's own
//! non-secret status feed, `~/.clauth/status.json` (the daemon's published
//! schema-2 body, the same shape `src/daemon/status_json.rs` writes), into
//! `upstream:<name>` observations: origin Upstream, auth ReadOnly, label
//! `<name> (clauth)`.
//!
//! **Nothing else under `~/.clauth` is ever read** — no config, no
//! credentials, no per-profile cache — and nothing is written. The reader is
//! tolerant: unknown fields are ignored, a malformed profile entry is skipped,
//! and an unreadable or future-schema feed yields no accounts.

use serde::Deserialize;

use super::collect::CollectCtx;
use super::fetch::{LABEL_5H, LABEL_7D, ScopedWindow, UsageInfo, UsageWindow};
use super::observation::{
    AccountObservation, AuthKind, Failure, FailureKind, Freshness, Origin, SourceId, Timestamp,
    account_id,
};
use super::project::usage_windows;

/// The feed's name under upstream's data dir.
pub(crate) const UPSTREAM_STATUS_FILE: &str = "status.json";
/// The newest feed schema this reader understands (upstream's
/// `status_json::SCHEMA_VERSION`). Schema 1 differs only in spelling
/// `auth_status` `expired` as `expiring`.
pub(crate) const MAX_UPSTREAM_SCHEMA: u64 = 2;
/// Largest feed read.
const MAX_FEED_BYTES: u64 = 4 * 1024 * 1024;
/// Longest upstream profile name projected.
const MAX_NAME_LEN: usize = 64;

/// The collector hook ([`crate::usage::collect::UPSTREAM_SOURCES`]). Only in
/// guest mode: once upstream is imported its accounts are tollgate's own.
pub(crate) fn upstream_observations(ctx: &CollectCtx<'_>) -> Vec<AccountObservation> {
    if !ctx.guest_mode {
        return Vec::new();
    }
    let Ok(home) = crate::profile::home_dir() else {
        return Vec::new();
    };
    let path = home
        .join(crate::identity::UPSTREAM_DATA_DIR_NAME)
        .join(UPSTREAM_STATUS_FILE);
    let Some(bytes) = read_capped(&path) else {
        return Vec::new();
    };
    project_status(&bytes, ctx.now_secs(), |at| ctx.freshness_of(at))
}

fn read_capped(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FEED_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_FEED_BYTES).then_some(bytes)
}

#[derive(Deserialize)]
struct Feed {
    #[serde(default)]
    schema: Option<u64>,
    #[serde(default)]
    profiles: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct FeedProfile {
    name: String,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    auth_status: Option<String>,
    #[serde(default)]
    fetch_status: Option<String>,
    #[serde(default)]
    stale: bool,
    #[serde(default)]
    fetched_at: Option<String>,
    #[serde(default)]
    windows: Vec<FeedWindow>,
    #[serde(default)]
    third_party: Option<FeedThirdParty>,
}

#[derive(Deserialize)]
struct FeedWindow {
    label: String,
    utilization_pct: f64,
    #[serde(default)]
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct FeedThirdParty {
    available: bool,
}

/// Project a `status.json` body at `now_secs`. `freshness` judges the age of
/// a profile's `fetched_at` (epoch ms). Pure.
pub(crate) fn project_status(
    bytes: &[u8],
    now_secs: i64,
    freshness: impl Fn(Option<u64>) -> Freshness,
) -> Vec<AccountObservation> {
    let Ok(feed) = serde_json::from_slice::<Feed>(bytes) else {
        return Vec::new();
    };
    if feed
        .schema
        .is_none_or(|s| s == 0 || s > MAX_UPSTREAM_SCHEMA)
    {
        return Vec::new();
    }
    feed.profiles
        .into_iter()
        .filter_map(|v| serde_json::from_value::<FeedProfile>(v).ok())
        .filter(|p| valid_name(&p.name))
        .map(|p| project_profile(p, now_secs, &freshness))
        .collect()
}

fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= MAX_NAME_LEN
        && !name.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn project_profile(
    p: FeedProfile,
    now_secs: i64,
    freshness: &impl Fn(Option<u64>) -> Freshness,
) -> AccountObservation {
    let mut obs = AccountObservation::new(
        account_id(Origin::Upstream, &p.name),
        SourceId::UpstreamClauth,
        AuthKind::ReadOnly,
        Origin::Upstream,
        format!("{} (clauth)", p.name),
    );
    let codex = p.harness.as_deref() == Some("codex");
    obs.provider = provider_display(p.provider.as_deref(), codex);
    obs.active = p.active;
    obs.endpoint = p.base_url.clone();
    obs.plan = p.tier.clone();
    obs.windows = usage_windows(&feed_usage(&p.windows), now_secs);

    let fetched_ms = p
        .fetched_at
        .as_deref()
        .and_then(Timestamp::parse)
        .and_then(|t| u64::try_from(t.secs()).ok())
        .map(|s| s.saturating_mul(1000));
    obs.observed_at = fetched_ms.map(Timestamp::from_ms);
    obs.freshness = match (fetched_ms, p.stale) {
        (Some(at), false) => freshness(Some(at)),
        (Some(at), true) => Freshness::Stale {
            since: Some(Timestamp::from_ms(at)),
        },
        (None, _) if !obs.windows.is_empty() => Freshness::Stale { since: None },
        (None, _) => Freshness::NotFetched,
    };
    obs.failure = feed_failure(&p);
    obs
}

/// The feed's windows as the [`UsageInfo`] they were published from:
/// `5h` → session, `7d` → weekly, `7d <model>` → a per-model weekly window.
/// Any other label is dropped, as upstream drops it.
fn feed_usage(windows: &[FeedWindow]) -> UsageInfo {
    let mut usage = UsageInfo::default();
    for w in windows.iter().filter(|w| w.utilization_pct.is_finite()) {
        let window = UsageWindow {
            utilization: w.utilization_pct,
            resets_at: w.resets_at.clone(),
        };
        if w.label == LABEL_5H {
            usage.five_hour = Some(window);
        } else if w.label == LABEL_7D {
            usage.seven_day = Some(window);
        } else if w.label.starts_with(&format!("{LABEL_7D} ")) {
            usage.weekly_scoped.push(ScopedWindow {
                label: w.label.clone(),
                window,
            });
        }
    }
    usage
}

fn provider_display(raw: Option<&str>, codex: bool) -> String {
    match raw {
        Some(p) if p.eq_ignore_ascii_case("anthropic") => "Anthropic".to_string(),
        Some(p) if p.eq_ignore_ascii_case("openai") => "OpenAI".to_string(),
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ if codex => "OpenAI".to_string(),
        _ => "Anthropic".to_string(),
    }
}

/// The feed's dead-credential and availability verdicts, most actionable
/// first. The hint names upstream's own command: tollgate cannot fix an
/// account it only reads.
fn feed_failure(p: &FeedProfile) -> Option<Failure> {
    let name = &p.name;
    if p.auth_status.as_deref() == Some("broken") {
        return Some(Failure::new(
            FailureKind::AuthRequired,
            &format!("login expired; run `clauth login {name}`"),
        ));
    }
    match p.fetch_status.as_deref() {
        Some("AuthExpired") => {
            return Some(Failure::new(
                FailureKind::AuthRequired,
                "credential rejected (reported by clauth)",
            ));
        }
        Some("RateLimited") => {
            return Some(Failure::new(
                FailureKind::RateLimited,
                "rate limited (reported by clauth)",
            ));
        }
        _ => {}
    }
    if p.third_party.as_ref().is_some_and(|t| !t.available) {
        return Some(Failure::new(
            FailureKind::QuotaExhausted,
            crate::providers::LOW_BALANCE,
        ));
    }
    None
}

#[cfg(test)]
#[path = "../../tests/inline/usage_upstream.rs"]
mod tests;
