//! Provider-neutral observations; percentages never imply comparable work across providers.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    Codex,
    Grok,
    Antigravity,
}

impl ProviderKind {
    pub(crate) fn tool(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Antigravity => "agy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationState {
    Fresh,
    Stale,
    NotFetched,
    AuthRequired,
    RateLimited,
    Unavailable,
    InvalidResponse,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct QuotaBucket {
    pub(crate) id: String,
    pub(crate) label: String,
    /// shared, model, or product; no assumed separate budget for each model.
    pub(crate) scope: String,
    #[serde(default)]
    pub(crate) models: Vec<String>,
    pub(crate) used_percent: Option<f64>,
    pub(crate) remaining_percent: Option<f64>,
    pub(crate) resets_at: Option<String>,
    pub(crate) window_seconds: Option<u64>,
    /// Explicit provider verdict, preserved even when no percentage is available.
    pub(crate) exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct CreditBalance {
    pub(crate) label: String,
    pub(crate) remaining: f64,
    pub(crate) unit: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct UsageAttribution {
    pub(crate) label: String,
    pub(crate) used_percent: f64,
    pub(crate) bucket_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct ProviderData {
    pub(crate) plan: Option<String>,
    pub(crate) subscription_status: Option<String>,
    #[serde(default)]
    pub(crate) buckets: Vec<QuotaBucket>,
    #[serde(default)]
    pub(crate) credits: Vec<CreditBalance>,
    #[serde(default)]
    pub(crate) attribution: Vec<UsageAttribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub(crate) struct ProviderReport {
    pub(crate) id: String,
    pub(crate) provider: ProviderKind,
    pub(crate) tool: String,
    pub(crate) model: Option<String>,
    pub(crate) state: ObservationState,
    pub(crate) observed_at_ms: Option<u64>,
    pub(crate) checked_at_ms: Option<u64>,
    /// File-backed credentials are rechecked by cached reads; native keyrings
    /// are checked only by the worker, never synchronously during rendering.
    #[serde(default)]
    pub(crate) identity_checked_at_observation_only: bool,
    /// A failed refresh retains the prior data, but never labels it fresh.
    pub(crate) data: ProviderData,
    pub(crate) message: Option<String>,
    /// Account-wide advisory, not selected-model eligibility or automatic routing.
    pub(crate) warning: bool,
    /// The user added this login on Overview. Monitor targets from
    /// `providers init` stay off that list. Not part of the providers JSON.
    #[serde(skip)]
    #[schema(ignore)]
    pub(crate) listed: bool,
    /// Where this reading's next refresh stands, for the TUI status rows.
    /// Not part of the providers JSON.
    #[serde(skip)]
    #[schema(ignore)]
    pub(crate) refresh: RefreshState,
}

/// The refresh bookkeeping a render needs to show the same status rows as a
/// Claude account: a spinner while a refresh is queued or running, the retry
/// ordinal after failures, and the countdown to the next check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RefreshState {
    /// Consecutive failed checks. Zero after a successful read.
    pub(crate) failures: u32,
    /// Epoch ms of the next scheduled check. `None` when no check has run yet,
    /// so the next scheduler pass is due at once.
    pub(crate) next_check_ms: Option<u64>,
    /// A manual refresh was requested in this process and has not started.
    pub(crate) queued: bool,
    /// This process is fetching the target now.
    pub(crate) refreshing: bool,
}

#[derive(Debug)]
pub(crate) struct ProviderError {
    pub(crate) state: ObservationState,
    pub(crate) message: &'static str,
}

impl ProviderError {
    pub(crate) fn auth(message: &'static str) -> Self {
        Self {
            state: ObservationState::AuthRequired,
            message,
        }
    }

    pub(crate) fn invalid() -> Self {
        Self {
            state: ObservationState::InvalidResponse,
            message: "provider returned an unrecognized usage response",
        }
    }

    pub(crate) fn network() -> Self {
        Self {
            state: ObservationState::Unavailable,
            message: "provider usage request failed",
        }
    }

    pub(crate) fn http(status: u16) -> Self {
        match status {
            401 | 403 => Self::auth("sign in again with the provider's official tool"),
            429 => Self {
                state: ObservationState::RateLimited,
                message: "usage endpoint rate limited; retrying after backoff",
            },
            _ => Self::network(),
        }
    }
}

/// Invalid/absent values remain unknown, including out-of-range server values.
pub(crate) fn percent(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
}
