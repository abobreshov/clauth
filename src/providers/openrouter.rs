//! OpenRouter provider, v2 (plan v3.1 §4.7, P4-OR).
//!
//! **Fetch order.** `GET /api/v1/key` runs first. It is the auth probe: a 401
//! there is the only thing that marks the inference key dead
//! ([`ThirdPartyError::AuthExpired`]). It carries the per-key figures: `usage`
//! (since minting), `usage_daily` / `usage_weekly` / `usage_monthly`, the
//! key's own cap (`limit`, `limit_remaining`, `limit_reset`), `byok_usage*`,
//! `is_free_tier`, `expires_at`, the owner ids (`workspace_id`,
//! `organization_id`, `creator_user_id`) and `free_model_daily_requests`.
//!
//! `GET /api/v1/credits` runs second. It is the wallet: `total_credits`
//! purchased vs `total_usage` spent, whose difference goes NEGATIVE once the
//! account is overdrawn (the state an inference call answers with `402 ...
//! can only afford 0`, measured 2026-08-17). OpenRouter documents it as
//! management-key only, although it answered regular keys on 2026-08-17. So
//! it degrades PER METER: any failure there (403 / 404 for a regular key, 401
//! for an expired management key, a 5xx, a network fault, an unreadable body)
//! drops the wallet meter alone, with a note, and never fails the fetch or
//! marks the inference key dead (plan §4.2).
//!
//! **Money is parsed from the raw JSON numbers**, never from a `"%.2f USD"`
//! rendering: each figure is the decimal text the server sent, kept exact in
//! an [`Amount`] (sub-cent usage like `0.000001536` survives, and so does a
//! negative wallet). The display rows are rounded from those amounts; the
//! typed meters ride in [`ThirdPartyStats::observed`].
//!
//! **Optional management key.** A profile may name an env var in its
//! `config.toml` (`billing_key_env = "OPENROUTER_MGMT_KEY"`, see
//! [`super::billing_key`]). Only the NAME is stored. The value is read from the
//! process env at fetch time, is sent to `GET /api/v1/credits` and nowhere
//! else ([`billing_key_may_reach`]), is never written, never printed, and is
//! scrubbed from every child session's env.
//!
//! Wire shapes per <https://openrouter.ai/docs/api/api-reference/api-keys/get-current-key>
//! and <https://openrouter.ai/docs/api/api-reference/credits/get-remaining-credits>.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use super::{
    DEEPSEEK_BALANCE_ROW_LABEL, ObservedMeters, StatRow, StatRowKind, ThirdPartyError,
    ThirdPartyStats, UsageBar, url_matches_host,
};
use crate::usage::observation::{
    Amount, MoneyKind, MoneyMeter, MoneyScope, Period, PeriodKind, QuotaWindow, ScopeOrigin,
    Timestamp, WindowScope, sanitize_message,
};

pub(super) const DISPLAY_NAME: &str = "OpenRouter";

pub(super) const ORIGIN: &str = "https://openrouter.ai";

pub(crate) const CREDITS_PATH: &str = "/api/v1/credits";
pub(crate) const KEY_PATH: &str = "/api/v1/key";

/// Where an operator mints the api key this provider authenticates with, as
/// published by <https://openrouter.ai/docs/quickstart> ("Your first request").
pub(super) const CONSOLE_URL: &str = "https://openrouter.ai/settings/keys";

/// Meter / window ids this source publishes (plan §4.7 mapping).
pub(crate) const METER_WALLET: &str = "wallet";
pub(crate) const METER_KEY_LIMIT: &str = "key_limit";
pub(crate) const WINDOW_FREE_DAILY: &str = "free_daily";

/// Label of the free-model daily request bar on the Usage tab. Not `5h` /
/// `7d`, so [`ThirdPartyStats::to_usage_info`] never folds it into the chain.
pub(crate) const FREE_DAILY_LABEL: &str = "free/day";

/// Row label of a wallet read with a management key. Deliberately NOT a
/// balance-row label ([`crate::providers::is_balance_row`]): an unbound
/// wallet is not this account's balance, so no rank or balance column may
/// read it as one.
pub(crate) const UNBOUND_WALLET_ROW_LABEL: &str = "monitoring wallet";

const DAY_SECS: i64 = 86_400;

pub(super) fn matches_base_url(url: &str) -> bool {
    url_matches_host(url, ORIGIN)
}

// ── HTTP seam ───────────────────────────────────────────────────────────────────

/// What one GET came back with. Status codes arrive as data so the caller can
/// degrade one meter on a 403 instead of failing the fetch.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HttpReply {
    /// A 2xx body (a 3xx is never followed and reads as [`Self::Network`]).
    Body(String),
    /// A >= 400 status. `retry_after` is the `retry-after` header in
    /// delta-seconds form, when present.
    Status {
        code: u16,
        retry_after: Option<Duration>,
    },
    /// The request never produced a status (DNS, TLS, timeout, body read).
    Network,
}

/// The GET transport the fetch runs on. [`LiveHttp`] is the real one; tests
/// hand in a recorder so no test can reach the network.
pub(crate) trait OpenRouterHttp {
    /// `GET url` with `Authorization: Bearer <bearer>`.
    fn get(&self, url: &str, bearer: &str) -> HttpReply;
}

/// The key-bearing transport ([`crate::usage::keyed_http`]).
pub(crate) struct LiveHttp;

impl OpenRouterHttp for LiveHttp {
    fn get(&self, url: &str, bearer: &str) -> HttpReply {
        if cfg!(test) {
            // Tests drive a recorder. Reaching here means a test would have
            // sent a real request with whatever key it held.
            panic!("openrouter: real network call attempted in a test ({url})");
        }
        // The key-bearing transport: no redirect followed (a 3xx is no
        // answer), a 2 MiB body cap, and an end-to-end deadline.
        let Some(reply) = crate::usage::keyed_http::get_bearer(url, bearer) else {
            return HttpReply::Network;
        };
        if reply.status >= 400 {
            return HttpReply::Status {
                code: reply.status,
                retry_after: reply.retry_after,
            };
        }
        match reply.body {
            Some(body) => HttpReply::Body(body),
            None => HttpReply::Network,
        }
    }
}

/// The management (billing) key's path allowlist: `GET /api/v1/credits` only.
/// A management key can create and delete keys, so it is never sent anywhere
/// it does not need to go (plan §4.2, per-credential path allowlist).
pub(crate) fn billing_key_may_reach(path: &str) -> bool {
    path == CREDITS_PATH
}

// ── Fetch ───────────────────────────────────────────────────────────────────────

/// Which credential read the wallet. The wallet is attributed to it (plan §4.7
/// (a)): the inference key's own owner, or a separate management key whose
/// owner tollgate has not proven to be the same account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalletCredential {
    Inference,
    Management,
}

/// `/api/v1/credits`, exact.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WalletRead {
    pub(crate) total_credits: Amount,
    pub(crate) total_usage: Amount,
    pub(crate) read_with: WalletCredential,
}

impl WalletRead {
    /// `total_credits − total_usage`, exact; negative when overdrawn.
    pub(crate) fn remaining(&self) -> Amount {
        amount_sub(&self.total_credits, &self.total_usage)
    }
}

/// `free_model_daily_requests`: requests to `:free` models today (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub(crate) struct FreeDaily {
    #[serde(default)]
    pub(crate) used: Option<f64>,
    #[serde(default)]
    pub(crate) limit: Option<f64>,
    #[serde(default)]
    pub(crate) remaining: Option<f64>,
}

/// `/api/v1/key` `data`, with every money figure as its exact decimal. Every
/// field is optional: a body missing one drops that meter, never the fetch.
/// `data` itself is required ([`KeyEnvelope`]), so an error envelope never
/// reads as usable usage. `label` (a masked copy of the key) and the
/// deprecated `rate_limit` object are deliberately not modelled.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct KeySnapshot {
    /// The key's own spending cap; `None` = no cap of its own. Says nothing
    /// about the wallet, which a null-cap key can still overdraw.
    #[serde(default)]
    pub(crate) limit: Option<RawAmount>,
    /// What is left under `limit`; `None` with it.
    #[serde(default)]
    pub(crate) limit_remaining: Option<RawAmount>,
    /// `daily` / `weekly` / `monthly`, or `None` for a cap that never resets.
    #[serde(default)]
    pub(crate) limit_reset: Option<String>,
    /// Spend on this key since it was minted.
    #[serde(default)]
    pub(crate) usage: Option<RawAmount>,
    /// Spend on this key in the current UTC day.
    #[serde(default)]
    pub(crate) usage_daily: Option<RawAmount>,
    /// Spend on this key in the current UTC week (Monday–Sunday).
    #[serde(default)]
    pub(crate) usage_weekly: Option<RawAmount>,
    /// Spend on this key in the current UTC month.
    #[serde(default)]
    pub(crate) usage_monthly: Option<RawAmount>,
    #[serde(default)]
    pub(crate) byok_usage: Option<RawAmount>,
    #[serde(default)]
    pub(crate) byok_usage_daily: Option<RawAmount>,
    #[serde(default)]
    pub(crate) byok_usage_weekly: Option<RawAmount>,
    #[serde(default)]
    pub(crate) byok_usage_monthly: Option<RawAmount>,
    /// Whether the account has never bought credits.
    #[serde(default)]
    pub(crate) is_free_tier: bool,
    #[serde(default)]
    pub(crate) is_management_key: bool,
    /// RFC 3339, or `None` for a key that never expires.
    #[serde(default)]
    pub(crate) expires_at: Option<String>,
    #[serde(default)]
    pub(crate) workspace_id: Option<String>,
    #[serde(default)]
    pub(crate) organization_id: Option<String>,
    #[serde(default)]
    pub(crate) creator_user_id: Option<String>,
    #[serde(default)]
    pub(crate) free_model_daily_requests: Option<FreeDaily>,
}

impl KeySnapshot {
    /// The account that owns this key: the organization when the key is an
    /// org key, else the user who minted it (plan §4.4 "same account").
    pub(crate) fn owner_id(&self) -> Option<&str> {
        self.organization_id
            .as_deref()
            .or(self.creator_user_id.as_deref())
            .filter(|s| !s.trim().is_empty())
    }
}

/// Everything one OpenRouter fetch learned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenRouterUsage {
    pub(crate) key: KeySnapshot,
    /// `None` when `/credits` could not be read; `notes` says why.
    pub(crate) wallet: Option<WalletRead>,
    /// Sanitised, human-readable degradations (a dropped wallet, an expiring
    /// key). Never carries a credential.
    pub(crate) notes: Vec<String>,
}

/// [`fetch_openrouter_usage_held`] without a backoff (a fresh
/// [`WalletHolds`]): the tests' entry.
#[cfg(test)]
pub(crate) fn fetch_openrouter_usage(
    inference_key: &str,
    billing_key: Option<&str>,
    http: &dyn OpenRouterHttp,
) -> Result<OpenRouterUsage, ThirdPartyError> {
    fetch_openrouter_usage_held(inference_key, billing_key, http, &WalletHolds::default(), 0)
}

/// Fetch one OpenRouter account: `GET /api/v1/key` with `inference_key`, then
/// `GET /api/v1/credits` with `billing_key` when one is given, else with the
/// inference key.
///
/// Errors only on the `/key` leg: a 401 is [`ThirdPartyError::AuthExpired`],
/// a 429 [`ThirdPartyError::RateLimited`], any other status
/// [`ThirdPartyError::Status`], no status [`ThirdPartyError::Network`], and an
/// unreadable body [`ThirdPartyError::Parse`]. Every `/credits` outcome is an
/// `Ok`: a failure there leaves `wallet = None` plus a note.
///
/// The `/credits` leg runs under a per-credential backoff: a wallet
/// credential `holds` has on hold is not sent, and a `/credits` 429 puts it
/// on hold until its `Retry-After` (at least [`WALLET_HOLD_FLOOR`]). The key
/// leg is never held here, so its meters keep refreshing while the wallet
/// waits.
pub(crate) fn fetch_openrouter_usage_held(
    inference_key: &str,
    billing_key: Option<&str>,
    http: &dyn OpenRouterHttp,
    holds: &WalletHolds,
    now_ms: u64,
) -> Result<OpenRouterUsage, ThirdPartyError> {
    let key = match http.get(&format!("{ORIGIN}{KEY_PATH}"), inference_key) {
        HttpReply::Body(body) => {
            serde_json::from_str::<KeyEnvelope>(&body)
                .map_err(|_| ThirdPartyError::Parse)?
                .data
        }
        HttpReply::Status { code: 401, .. } => return Err(ThirdPartyError::AuthExpired),
        HttpReply::Status {
            code: 429,
            retry_after,
        } => return Err(ThirdPartyError::RateLimited { retry_after }),
        HttpReply::Status { .. } => return Err(ThirdPartyError::Status),
        HttpReply::Network => return Err(ThirdPartyError::Network),
    };

    let mut notes = Vec::new();
    if key.is_management_key {
        notes.push("this is a management key; it cannot run inference".to_string());
    }
    let billing_key = billing_key.map(str::trim).filter(|k| !k.is_empty());
    let (bearer, read_with) = match billing_key {
        Some(k) => (k, WalletCredential::Management),
        None => (inference_key, WalletCredential::Inference),
    };
    let reply = match holds.remaining(bearer, now_ms) {
        Some(left) => {
            notes.push(format!(
                "wallet unavailable: rate limited (429); /credits held for {}s",
                left.as_secs().max(1)
            ));
            None
        }
        None => Some(guarded_get(http, CREDITS_PATH, bearer, read_with)),
    };
    if let Some(HttpReply::Status {
        code: 429,
        retry_after,
    }) = &reply
    {
        holds.hold(bearer, now_ms, *retry_after);
    }
    let wallet = match reply {
        None => None,
        Some(HttpReply::Body(body)) => match serde_json::from_str::<CreditsEnvelope>(&body) {
            Ok(env) => Some(WalletRead {
                total_credits: env.data.total_credits.0,
                total_usage: env.data.total_usage.0,
                read_with,
            }),
            Err(_) => {
                notes.push("wallet unavailable: unreadable /credits response".to_string());
                None
            }
        },
        Some(HttpReply::Status { code, .. }) => {
            notes.push(wallet_status_note(code, read_with));
            None
        }
        Some(HttpReply::Network) => {
            notes.push("wallet unavailable: network error".to_string());
            None
        }
    };
    let notes = notes.iter().map(|n| sanitize_message(n)).collect();
    Ok(OpenRouterUsage { key, wallet, notes })
}

/// [`fetch_openrouter_wallet_held`] without a backoff: the tests' entry.
#[cfg(test)]
pub(crate) fn fetch_openrouter_wallet(
    billing_key: &str,
    http: &dyn OpenRouterHttp,
) -> Result<OpenRouterUsage, ThirdPartyError> {
    fetch_openrouter_wallet_held(billing_key, http, &WalletHolds::default(), 0)
}

/// Read one OpenRouter wallet with a management key alone: `GET
/// /api/v1/credits` and nothing else (the key never reaches `/api/v1/key`).
/// The monitoring leg for a monitor configured with only `billing_key_env`.
///
/// Unlike the `/credits` leg of [`fetch_openrouter_usage_held`], a failure
/// here is the whole fetch: 401 is [`ThirdPartyError::AuthExpired`], 429
/// [`ThirdPartyError::RateLimited`], any other status
/// [`ThirdPartyError::Status`], no status [`ThirdPartyError::Network`], and an
/// unreadable body [`ThirdPartyError::Parse`]. The wallet is read with
/// [`WalletCredential::Management`], so it projects as an unbound monitoring
/// meter; the key snapshot is empty (no key spend, no cap, no free window).
///
/// Under the same `/credits` backoff: a held key sends nothing and answers
/// [`ThirdPartyError::RateLimited`] with the time left; a 429 puts the key on
/// hold.
pub(crate) fn fetch_openrouter_wallet_held(
    billing_key: &str,
    http: &dyn OpenRouterHttp,
    holds: &WalletHolds,
    now_ms: u64,
) -> Result<OpenRouterUsage, ThirdPartyError> {
    let billing_key = billing_key.trim();
    if billing_key.is_empty() {
        return Err(ThirdPartyError::AuthExpired);
    }
    if let Some(left) = holds.remaining(billing_key, now_ms) {
        return Err(ThirdPartyError::RateLimited {
            retry_after: Some(left),
        });
    }
    let read_with = WalletCredential::Management;
    let reply = guarded_get(http, CREDITS_PATH, billing_key, read_with);
    if let HttpReply::Status {
        code: 429,
        retry_after,
    } = &reply
    {
        holds.hold(billing_key, now_ms, *retry_after);
    }
    match reply {
        HttpReply::Body(body) => {
            let env = serde_json::from_str::<CreditsEnvelope>(&body)
                .map_err(|_| ThirdPartyError::Parse)?;
            Ok(OpenRouterUsage {
                key: KeySnapshot::default(),
                wallet: Some(WalletRead {
                    total_credits: env.data.total_credits.0,
                    total_usage: env.data.total_usage.0,
                    read_with,
                }),
                notes: Vec::new(),
            })
        }
        HttpReply::Status { code: 401, .. } => Err(ThirdPartyError::AuthExpired),
        HttpReply::Status {
            code: 429,
            retry_after,
        } => Err(ThirdPartyError::RateLimited { retry_after }),
        HttpReply::Status { .. } => Err(ThirdPartyError::Status),
        HttpReply::Network => Err(ThirdPartyError::Network),
    }
}

/// [`fetch_openrouter_wallet`] over [`LiveHttp`], projected to
/// [`ThirdPartyStats`] (the monitor source's live leg).
pub(crate) fn fetch_wallet_stats(billing_key: &str) -> Result<ThirdPartyStats, ThirdPartyError> {
    wallet_stats_with(billing_key, &LiveHttp, &WALLET_HOLDS, wall_clock_ms())
}

/// [`fetch_wallet_stats`] over any transport and hold store: the live leg
/// and the tests share it.
pub(crate) fn wallet_stats_with(
    billing_key: &str,
    http: &dyn OpenRouterHttp,
    holds: &WalletHolds,
    now_ms: u64,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    let usage = fetch_openrouter_wallet_held(billing_key, http, holds, now_ms)?;
    Ok(stats(&usage, crate::usage::now_epoch_secs()))
}

/// Shortest `/credits` hold after a 429, when the answer names no
/// `Retry-After` (or a shorter one): the monitor poll's own 429 floor.
pub(crate) const WALLET_HOLD_FLOOR: Duration = Duration::from_secs(5 * 60);

/// A per-credential `/credits` backoff (plan §4.2: a 429 is honoured). The
/// wallet leg of a fetch fails soft (a note, the key meters kept), so the
/// fetch-level hold never sees its 429 — a monitor's partial success even
/// clears its own cache hold — and a successful `/key` read would otherwise
/// re-send `/credits` on every poll. Keyed by a SHA-256 of the credential,
/// never the credential.
///
/// The live store is [`WalletHolds::persistent`]: besides the in-memory map,
/// each hold is written to `~/.tollgate/holds/openrouter-credits-<hex>.json`
/// (atomic, 0600), and every read consults that file, so the daemon, a
/// forced `tollgate monitor refresh` and any other process back off one
/// wallet together. Monitor locks and the profile fetch lease are keyed by
/// monitor / profile, not by credential, so two of them can share one wallet
/// key: every hold write and every expiry removal therefore runs under the
/// credential's own flock (`openrouter-credits-<hex>.lock` beside the hold,
/// 0600, never removed). A write keeps the later of the persisted deadline and
/// the new one, so a shorter 429 never cuts a longer hold short, and an
/// expired file is removed only while it still holds the expired deadline
/// that was read, so a stale reader never unlinks a hold renewed meanwhile.
#[derive(Debug, Default)]
pub(crate) struct WalletHolds {
    map: Mutex<HashMap<[u8; 32], u64>>,
    /// Mirror every hold to `~/.tollgate/holds/` and honour the ones there.
    persist: bool,
}

/// Bumped when the persisted hold's shape changes; another version reads as
/// no hold.
const WALLET_HOLD_VERSION: u32 = 1;
/// Largest persisted hold file read.
const MAX_WALLET_HOLD_BYTES: u64 = 4096;

/// One persisted `/credits` hold: nothing but the deadline.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedWalletHold {
    version: u32,
    until_ms: u64,
}

impl WalletHolds {
    /// The store the live fetches share: in memory and on disk.
    pub(crate) fn persistent() -> Self {
        Self {
            map: Mutex::default(),
            persist: true,
        }
    }

    /// Domain-separated, so the name a hold file carries is not a bare
    /// SHA-256 of the key anything else might also publish.
    fn fingerprint(bearer: &str) -> [u8; 32] {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"tollgate/openrouter-credits-hold\0");
        hasher.update(bearer.trim().as_bytes());
        hasher.finalize().into()
    }

    /// The longest hold ever honoured: the scheduler's retry cap, never less
    /// than the floor. A persisted deadline further out (a skewed clock, a
    /// hand edit) is clamped to it.
    fn cap() -> Duration {
        Duration::from_millis(crate::usage::MAX_RETRY_AFTER_MS).max(WALLET_HOLD_FLOOR)
    }

    /// `~/.tollgate/holds/openrouter-credits-<hex>.json` for `fingerprint`.
    fn hold_path(fingerprint: &[u8; 32]) -> Option<std::path::PathBuf> {
        use std::fmt::Write as _;
        let mut hex = String::with_capacity(64);
        for b in fingerprint {
            let _ = write!(hex, "{b:02x}");
        }
        let dir = crate::profile::tollgate_dir().ok()?.join("holds");
        Some(dir.join(format!("openrouter-credits-{hex}.json")))
    }

    /// Take `fingerprint`'s hold flock (`<hold>.lock`, created 0600 and never
    /// removed, so every process contends on one inode), creating the 0700
    /// `holds/` dir first. Blocks: the section it guards is one small read
    /// and one small write, and nothing is locked inside it. `None` when the
    /// dir or the lock cannot be had.
    fn lock_hold(hold: &std::path::Path) -> Option<std::fs::File> {
        crate::profile::mkdir_700(hold.parent()?).ok()?;
        let lock = crate::profile::open_state_file(&hold.with_extension("lock")).ok()?;
        lock.lock().ok()?;
        Some(lock)
    }

    /// The deadline the hold file at `path` carries, expired or not. A
    /// missing, oversized, torn or foreign-version file reads as none.
    fn read_until(path: &std::path::Path) -> Option<u64> {
        use std::io::Read as _;
        let file = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::new();
        file.take(MAX_WALLET_HOLD_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_WALLET_HOLD_BYTES {
            return None;
        }
        serde_json::from_slice::<PersistedWalletHold>(&bytes)
            .ok()
            .filter(|h| h.version == WALLET_HOLD_VERSION)
            .map(|h| h.until_ms)
    }

    /// The persisted deadline for `fingerprint`, when one is in force at
    /// `now_ms`. A skewed deadline is rewritten under its flock; an expired
    /// file is removed (see [`Self::remove_if_expired`]). When a concurrent
    /// write changes either deadline after the unlocked read, the current
    /// value is checked under the lock before changing the file.
    fn persisted_until(fingerprint: &[u8; 32], now_ms: u64) -> Option<u64> {
        let path = Self::hold_path(fingerprint)?;
        let until = Self::read_until(&path)?;
        let cap_ms = u64::try_from(Self::cap().as_millis()).unwrap_or(u64::MAX);
        let cap_until = now_ms.saturating_add(cap_ms);
        let until = if until > cap_until {
            Self::clamp_skewed(&path, until, cap_until)
        } else {
            until
        };
        if until > now_ms {
            return Some(until);
        }
        if Self::remove_if_expired(fingerprint, until, now_ms) {
            return None;
        }
        Self::read_until(&path).filter(|&t| t > now_ms)
    }

    fn clamp_skewed(path: &std::path::Path, seen_until: u64, cap_until: u64) -> u64 {
        let Some(_lock) = Self::lock_hold(path) else {
            return seen_until;
        };
        let Some(current) = Self::read_until(path) else {
            return seen_until;
        };
        if current <= cap_until {
            return current;
        }
        let hold = PersistedWalletHold {
            version: WALLET_HOLD_VERSION,
            until_ms: cap_until,
        };
        let written = serde_json::to_vec(&hold)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::profile::atomic_write_600(path, bytes));
        if let Err(error) = written {
            crate::logline::logline!(
                "tollgate openrouter: failed to clamp wallet hold {}: {error}",
                path.display()
            );
            return current;
        }
        cap_until
    }

    /// Remove `fingerprint`'s hold file, under its flock, only when it still
    /// carries `seen_until` — the expired deadline an unlocked read saw — and
    /// that is expired at `now_ms`. A hold another process wrote after that
    /// read survives. Returns whether the file was removed.
    fn remove_if_expired(fingerprint: &[u8; 32], seen_until: u64, now_ms: u64) -> bool {
        let Some(path) = Self::hold_path(fingerprint) else {
            return false;
        };
        let Some(_lock) = Self::lock_hold(&path) else {
            return false;
        };
        seen_until <= now_ms
            && Self::read_until(&path) == Some(seen_until)
            && std::fs::remove_file(&path).is_ok()
    }

    /// Persist `until_ms` as `fingerprint`'s hold, keeping the later of it and
    /// the deadline already on disk (clamped to the cap from `now_ms`, so a
    /// skewed file is not carried forward unbounded). Read and write run
    /// under the credential's flock. Best effort: the in-memory hold still
    /// covers this process when the write fails.
    fn persist_until(fingerprint: &[u8; 32], until_ms: u64, now_ms: u64) {
        let Some(path) = Self::hold_path(fingerprint) else {
            return;
        };
        let Some(_lock) = Self::lock_hold(&path) else {
            return;
        };
        let cap_ms = u64::try_from(Self::cap().as_millis()).unwrap_or(u64::MAX);
        let raw = Self::read_until(&path);
        let existing = raw.map(|t| t.min(now_ms.saturating_add(cap_ms)));
        let hold = PersistedWalletHold {
            version: WALLET_HOLD_VERSION,
            until_ms: existing.map_or(until_ms, |t| t.max(until_ms)),
        };
        // Compared with the RAW deadline, so a skewed file past the cap is
        // rewritten to the clamped value rather than kept as it stands.
        if raw == Some(hold.until_ms) {
            return;
        }
        if let Ok(bytes) = serde_json::to_vec(&hold) {
            let _ = crate::profile::atomic_write_600(&path, bytes);
        }
    }

    /// How much longer `bearer`'s `/credits` read is held, if at all: the
    /// later of this process's hold and the persisted one.
    pub(crate) fn remaining(&self, bearer: &str, now_ms: u64) -> Option<Duration> {
        let fingerprint = Self::fingerprint(bearer);
        let in_memory = self
            .map
            .lock()
            .ok()
            .and_then(|map| map.get(&fingerprint).copied());
        let on_disk = self
            .persist
            .then(|| Self::persisted_until(&fingerprint, now_ms))
            .flatten();
        let until = in_memory.into_iter().chain(on_disk).max()?;
        (until > now_ms).then(|| Duration::from_millis(until - now_ms).min(Self::cap()))
    }

    /// Hold `bearer`'s `/credits` read for `retry_after`, at least
    /// [`WALLET_HOLD_FLOOR`] (and at most the scheduler's retry cap), in
    /// memory and, for the persistent store, on disk. Never shortens a hold
    /// already in force.
    pub(crate) fn hold(&self, bearer: &str, now_ms: u64, retry_after: Option<Duration>) {
        let wait = retry_after
            .unwrap_or(WALLET_HOLD_FLOOR)
            .max(WALLET_HOLD_FLOOR)
            .min(Self::cap());
        let until = now_ms.saturating_add(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX));
        let fingerprint = Self::fingerprint(bearer);
        if let Ok(mut map) = self.map.lock() {
            map.retain(|_, t| *t > now_ms);
            let held = map.entry(fingerprint).or_insert(until);
            *held = (*held).max(until);
        }
        if self.persist {
            Self::persist_until(&fingerprint, until, now_ms);
        }
    }
}

/// The wallet holds every live fetch shares — persisted, so a profile and a
/// monitor reading the same wallet with the same key back off together
/// across processes as well as within one.
static WALLET_HOLDS: LazyLock<WalletHolds> = LazyLock::new(WalletHolds::persistent);

fn wall_clock_ms() -> u64 {
    crate::usage::now_ms()
}

/// `GET ORIGIN+path`, refusing to send a management key to any path outside
/// [`billing_key_may_reach`]. A refusal reads as a 403 so the one meter
/// degrades and nothing is sent.
fn guarded_get(
    http: &dyn OpenRouterHttp,
    path: &str,
    bearer: &str,
    cred: WalletCredential,
) -> HttpReply {
    if cred == WalletCredential::Management && !billing_key_may_reach(path) {
        return HttpReply::Status {
            code: 403,
            retry_after: None,
        };
    }
    http.get(&format!("{ORIGIN}{path}"), bearer)
}

/// Why the wallet meter is missing, per status and reading credential.
fn wallet_status_note(code: u16, read_with: WalletCredential) -> String {
    match (code, read_with) {
        (401, WalletCredential::Management) => {
            "wallet unavailable: management key rejected or expired (401)".to_string()
        }
        (403 | 404, WalletCredential::Inference) => format!(
            "wallet unavailable: /credits needs a management key ({code}); set billing_key_env"
        ),
        (403 | 404, WalletCredential::Management) => {
            format!("wallet unavailable: management key refused by /credits ({code})")
        }
        (429, _) => "wallet unavailable: rate limited (429)".to_string(),
        (code, _) => format!("wallet unavailable: /credits answered {code}"),
    }
}

/// The scheduler's entry point: fetch over [`LiveHttp`] and project to
/// [`ThirdPartyStats`]. `billing_key_env` is the NAME of the env var holding
/// an optional management key; its value is read here and dropped with the
/// call.
pub(super) fn fetch(
    api_key: &str,
    billing_key_env: Option<&str>,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    fetch_stats_with(
        api_key,
        billing_key_env,
        &LiveHttp,
        &WALLET_HOLDS,
        wall_clock_ms(),
    )
}

/// [`fetch`] over any transport and hold store: the scheduler's live leg and
/// the tests share it.
pub(crate) fn fetch_stats_with(
    api_key: &str,
    billing_key_env: Option<&str>,
    http: &dyn OpenRouterHttp,
    holds: &WalletHolds,
    now_ms: u64,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    let mut pre_notes = Vec::new();
    let billing_key = billing_key_env.and_then(|name| {
        let value = super::billing_key::resolve(name);
        if value.is_none() {
            pre_notes.push(format!(
                "billing_key_env {name} is not set; wallet read with the inference key"
            ));
        }
        value
    });
    let mut usage =
        fetch_openrouter_usage_held(api_key, billing_key.as_deref(), http, holds, now_ms)?;
    drop(billing_key);
    pre_notes.extend(usage.notes);
    usage.notes = pre_notes.iter().map(|n| sanitize_message(n)).collect();
    Ok(stats(&usage, crate::usage::now_epoch_secs()))
}

// ── Projection ──────────────────────────────────────────────────────────────────

/// The typed meters and windows of one fetch (plan §4.7 mapping (a)–(f)).
///
/// - `wallet`: Balance = `total_credits − total_usage` (may be negative),
///   `limit` = `total_credits`, scope Organization. Read with the inference
///   key: `scope_id` = its owner, `ScopeOrigin::Provider`. Read with a
///   management key: owner unproven, so `scope_id = None` and
///   `MonitoringCredential { bound: false }` (never de-duplicated).
/// - `spend.daily` / `spend.weekly` / `spend.monthly` (UTC, Monday weeks) and
///   `spend.lifetime`: Spend, scope Key, three period rows never merged.
/// - `key_limit`: Limit, `amount` = `limit_remaining`, `limit` = `limit`,
///   period from `limit_reset`.
/// - `byok.*`: Spend on the operator's own provider keys, non-additive, only
///   when any BYOK spend exists.
/// - `free_daily` window: `free_model_daily_requests`, account scope, never
///   chain-eligible, resets at the next UTC midnight.
pub(crate) fn observed_meters(usage: &OpenRouterUsage, now_secs: i64) -> ObservedMeters {
    let key = &usage.key;
    let owner = key.owner_id().map(str::to_string);
    let mut money = Vec::new();

    if let Some(w) = &usage.wallet {
        let mut m = MoneyMeter::new(
            METER_WALLET,
            "Balance",
            MoneyKind::Balance,
            w.remaining(),
            "USD",
            MoneyScope::Organization,
        );
        m.limit = Some(w.total_credits.clone());
        match w.read_with {
            WalletCredential::Inference => {
                m.scope_id = owner.clone();
                m.scope_origin = ScopeOrigin::Provider;
            }
            WalletCredential::Management => {
                m.scope_id = None;
                m.scope_origin = ScopeOrigin::MonitoringCredential { bound: false };
            }
        }
        money.push(m);
    }

    for (id, label, figure, kind) in [
        (
            "spend.daily",
            "Spend today",
            &key.usage_daily,
            PeriodKind::Daily,
        ),
        (
            "spend.weekly",
            "Spend this week",
            &key.usage_weekly,
            PeriodKind::Weekly,
        ),
        (
            "spend.monthly",
            "Spend this month",
            &key.usage_monthly,
            PeriodKind::Monthly,
        ),
        (
            "spend.lifetime",
            "Key lifetime spend",
            &key.usage,
            PeriodKind::Lifetime,
        ),
    ] {
        if let Some(a) = figure {
            let mut m = key_spend(id, label, a);
            m.period = Some(utc_period(kind, now_secs));
            money.push(m);
        }
    }

    if let (Some(cap), Some(left)) = (&key.limit, &key.limit_remaining) {
        let mut m = MoneyMeter::new(
            METER_KEY_LIMIT,
            "Key cap",
            MoneyKind::Limit,
            left.0.clone(),
            "USD",
            MoneyScope::Key,
        );
        m.limit = Some(cap.0.clone());
        m.period = limit_reset_kind(key.limit_reset.as_deref()).map(|k| utc_period(k, now_secs));
        money.push(m);
    }

    let any_byok = [
        &key.byok_usage,
        &key.byok_usage_daily,
        &key.byok_usage_weekly,
        &key.byok_usage_monthly,
    ]
    .iter()
    .any(|f| f.as_ref().is_some_and(|a| !a.0.is_zero()));
    if any_byok {
        for (id, label, figure, kind) in [
            (
                "byok.daily",
                "BYOK today",
                &key.byok_usage_daily,
                PeriodKind::Daily,
            ),
            (
                "byok.weekly",
                "BYOK this week",
                &key.byok_usage_weekly,
                PeriodKind::Weekly,
            ),
            (
                "byok.monthly",
                "BYOK this month",
                &key.byok_usage_monthly,
                PeriodKind::Monthly,
            ),
            (
                "byok.lifetime",
                "BYOK lifetime",
                &key.byok_usage,
                PeriodKind::Lifetime,
            ),
        ] {
            if let Some(a) = figure {
                let mut m = key_spend(id, label, a);
                // Billed by the operator's own provider, not this wallet.
                m.additive = false;
                m.period = Some(utc_period(kind, now_secs));
                money.push(m);
            }
        }
    }

    let windows = key
        .free_model_daily_requests
        .and_then(|f| free_daily_window(&f, now_secs))
        .into_iter()
        .collect();

    ObservedMeters {
        money,
        windows,
        notes: usage.notes.clone(),
    }
}

fn key_spend(id: &str, label: &str, a: &RawAmount) -> MoneyMeter {
    MoneyMeter::new(
        id,
        label,
        MoneyKind::Spend,
        a.0.clone(),
        "USD",
        MoneyScope::Key,
    )
}

/// `free_model_daily_requests` as a quota window, or `None` when it carries
/// neither a used count nor a limit.
fn free_daily_window(f: &FreeDaily, now_secs: i64) -> Option<QuotaWindow> {
    let limit = f.limit.filter(|l| l.is_finite() && *l > 0.0);
    let used = f
        .used
        .or_else(|| Some(limit? - f.remaining?))
        .filter(|u| u.is_finite());
    if used.is_none() && limit.is_none() {
        return None;
    }
    let mut w = QuotaWindow::new(WINDOW_FREE_DAILY, FREE_DAILY_LABEL, WindowScope::Account);
    w.used = used;
    w.limit = limit;
    w.used_pct = match (used, limit) {
        (Some(u), Some(l)) => Some(u / l * 100.0),
        _ => None,
    };
    w.exhausted = f.remaining.is_some_and(|r| r <= 0.0) || w.used_pct.is_some_and(|p| p >= 100.0);
    w.resets_at = Some(Timestamp::from_secs(day_start(now_secs) + DAY_SECS));
    w.window_secs = Some(DAY_SECS as u64);
    w.chain_eligible = false;
    Some(w)
}

fn limit_reset_kind(reset: Option<&str>) -> Option<PeriodKind> {
    match reset?.trim().to_ascii_lowercase().as_str() {
        "daily" | "day" => Some(PeriodKind::Daily),
        "weekly" | "week" => Some(PeriodKind::Weekly),
        "monthly" | "month" => Some(PeriodKind::Monthly),
        _ => None,
    }
}

/// The UTC calendar period of `kind` containing `now_secs`, bounds derived by
/// tollgate. `Lifetime` / `Custom` carry no bounds.
fn utc_period(kind: PeriodKind, now_secs: i64) -> Period {
    let bounds = match kind {
        PeriodKind::Daily => {
            let s = day_start(now_secs);
            Some((s, s + DAY_SECS))
        }
        PeriodKind::Weekly => {
            let s = week_start(now_secs);
            Some((s, s + 7 * DAY_SECS))
        }
        PeriodKind::Monthly => month_bounds(now_secs),
        PeriodKind::Lifetime | PeriodKind::Custom => None,
    };
    match bounds {
        Some((start, end)) => Period {
            kind,
            start: Some(Timestamp::from_secs(start)),
            end: Some(Timestamp::from_secs(end)),
            derived: true,
        },
        None => Period::of(kind),
    }
}

fn day_start(now_secs: i64) -> i64 {
    now_secs - now_secs.rem_euclid(DAY_SECS)
}

/// Monday 00:00 UTC of the week containing `now_secs` (1970-01-01 was a
/// Thursday, so day 0 is weekday index 3 counting from Monday).
fn week_start(now_secs: i64) -> i64 {
    let days = now_secs.div_euclid(DAY_SECS);
    (days - (days + 3).rem_euclid(7)) * DAY_SECS
}

fn month_bounds(now_secs: i64) -> Option<(i64, i64)> {
    use chrono::Datelike as _;
    let now = chrono::DateTime::from_timestamp(now_secs, 0)?.date_naive();
    let start = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)?;
    let (ny, nm) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let end = chrono::NaiveDate::from_ymd_opt(ny, nm, 1)?;
    let secs = |d: chrono::NaiveDate| Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    Some((secs(start)?, secs(end)?))
}

/// One fetch → the cache the Usage tab renders and the observation reads.
///
/// Rows keep the v1 labels and order (`credits` heading, `api balance`,
/// `used`, `purchased`, then `today` / `this week` / `this month`, `key limit`
/// / `key limit left`, `free tier`), rounded from the exact amounts. A missing
/// wallet renders a faint note in place of its three rows. An overdrawn
/// wallet (under half a cent, which renders `0.00 USD` or less) marks the
/// stats unfunded, exactly as v1 did; nothing else changes availability, so
/// the chain behaves as before.
pub(crate) fn stats(usage: &OpenRouterUsage, now_secs: i64) -> ThirdPartyStats {
    let key = &usage.key;
    let mut rows = vec![StatRow {
        label: "credits".to_string(),
        value: String::new(),
        kind: StatRowKind::Heading,
    }];
    let mut funded = true;
    match &usage.wallet {
        Some(w) => {
            let remaining = w.remaining();
            // Danger must agree with what the row SAYS: anything under half
            // a cent (an overdrawn account included) renders as `0.00 USD` or
            // worse, so an exact zero test would leave a spent key reading
            // as a healthy one.
            let wallet_funded = remaining >= half_cent();
            // Only a wallet the inference key read itself is this account's
            // own. A management key's wallet is unbound (its owner is not
            // proven to be the inference key's account, plan §4.7 (a)): it is
            // published as its own `MonitoringCredential { bound: false }`
            // meter and never decides whether this account can run a call,
            // nor ranks as its balance.
            let own = w.read_with == WalletCredential::Inference;
            if own {
                funded = wallet_funded;
            }
            // The account's own wallet row shares the DeepSeek balance label
            // on purpose: the MCP roster's balance rank and the overview's
            // balance column single that label out.
            rows.push(StatRow {
                label: if own {
                    DEEPSEEK_BALANCE_ROW_LABEL.to_string()
                } else {
                    UNBOUND_WALLET_ROW_LABEL.to_string()
                },
                value: dollars(&remaining),
                kind: if wallet_funded {
                    StatRowKind::Body
                } else {
                    StatRowKind::Danger
                },
            });
            rows.push(body_row("used", &w.total_usage));
            rows.push(body_row("purchased", &w.total_credits));
        }
        None => rows.push(StatRow {
            label: "wallet".to_string(),
            value: "unavailable".to_string(),
            kind: StatRowKind::Faint,
        }),
    }
    for (label, figure) in [
        ("today", &key.usage_daily),
        ("this week", &key.usage_weekly),
        ("this month", &key.usage_monthly),
    ] {
        if let Some(a) = figure {
            rows.push(body_row(label, &a.0));
        }
    }
    if let Some(cap) = &key.limit {
        rows.push(body_row("key limit", &cap.0));
    }
    if let Some(left) = &key.limit_remaining {
        rows.push(body_row("key limit left", &left.0));
    }
    if key.is_free_tier {
        rows.push(StatRow {
            label: "free tier".to_string(),
            value: String::new(),
            kind: StatRowKind::Faint,
        });
    }
    for note in &usage.notes {
        rows.push(StatRow {
            label: String::new(),
            value: note.clone(),
            kind: StatRowKind::Faint,
        });
    }

    let observed = observed_meters(usage, now_secs);
    let bars = observed
        .windows
        .iter()
        .filter(|w| w.id == WINDOW_FREE_DAILY)
        .filter_map(|w| {
            Some(UsageBar {
                label: FREE_DAILY_LABEL.to_string(),
                pct: w.used_pct?.clamp(0.0, 100.0),
                resets_at: w.resets_at.map(|t| t.to_rfc3339()),
                used: w.used,
                total: w.limit,
            })
        })
        .collect();

    let mut out = if funded {
        ThirdPartyStats::from_rows(rows)
    } else {
        // An overdrawn wallet cannot afford any call, so the daemon's
        // reachability dot must read red. The rows still render, and
        // `unfunded` appends the shared refusal beside them.
        ThirdPartyStats::unfunded(rows)
    };
    out.bars = bars;
    out.observed = Some(Box::new(observed));
    out
}

fn body_row(label: &str, a: &Amount) -> StatRow {
    StatRow {
        label: label.to_string(),
        value: dollars(a),
        kind: StatRowKind::Body,
    }
}

fn half_cent() -> Amount {
    Amount::parse("0.005").unwrap_or_else(Amount::zero)
}

/// `1.5` → `"1.50 USD"`, the `amount currency` shape `parse_balance` reads.
/// Rounded half away from zero from the exact amount. An overdrawn remaining
/// formats negative (`-0.20 USD`), which the funded-wallet selection drops.
fn dollars(a: &Amount) -> String {
    let rounded = a.round_dp(2);
    // `round_dp` drops the sign of a debt that rounds to zero; keep it, as
    // the v1 `{:.2}` rendering did, so a tiny overdraft never reads funded.
    if a.is_negative() && !rounded.is_negative() {
        format!("-{rounded} USD")
    } else {
        format!("{rounded} USD")
    }
}

// ── Exact decimals ──────────────────────────────────────────────────────────────

/// One JSON money figure kept as its exact decimal. Accepts a JSON number in
/// any spelling (`0.000001536`, `1.5e-7`, `-3`) or a decimal string; rejects
/// anything else rather than inventing a figure.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawAmount(pub(crate) Amount);

impl<'de> Deserialize<'de> for RawAmount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw: Box<RawValue> = Deserialize::deserialize(d)?;
        json_number_amount(raw.get())
            .map(RawAmount)
            .ok_or_else(|| serde::de::Error::custom("not a decimal amount"))
    }
}

/// The exact decimal a JSON number token (or decimal string) spells.
pub(crate) fn json_number_amount(text: &str) -> Option<Amount> {
    let text = text.trim();
    let owned;
    let text = if text.starts_with('"') {
        owned = serde_json::from_str::<String>(text).ok()?;
        owned.trim()
    } else {
        text
    };
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(i) => (&text[..i], text[i + 1..].parse::<i32>().ok()?),
        None => (text, 0),
    };
    let m = Amount::parse(mantissa)?;
    // A figure whose exponent would need more than a few hundred digits is no
    // money anyone owes.
    if exponent.unsigned_abs() > 64 {
        return None;
    }
    Some(if exponent < 0 {
        m.scaled_down(exponent.unsigned_abs())
    } else {
        scaled_up(&m, exponent.unsigned_abs())
    })
}

/// `a × 10^places`, exactly (a decimal-point shift right).
fn scaled_up(a: &Amount, places: u32) -> Amount {
    if places == 0 {
        return a.clone();
    }
    let s = a.as_str();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let p = places as usize;
    let frac_padded = format!("{frac:0<p$}");
    let (moved, rest) = frac_padded.split_at(p);
    let raw = if rest.is_empty() {
        format!("{}{int}{moved}", if neg { "-" } else { "" })
    } else {
        format!("{}{int}{moved}.{rest}", if neg { "-" } else { "" })
    };
    Amount::parse(&raw).unwrap_or_else(Amount::zero)
}

/// `a − b`, exact while both fit 38 significant digits (every real wallet);
/// beyond that, through `f64`.
pub(crate) fn amount_sub(a: &Amount, b: &Amount) -> Amount {
    fn split(x: &Amount) -> (bool, String, String) {
        let s = x.as_str();
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (i, f) = body.split_once('.').unwrap_or((body, ""));
        (neg, i.to_string(), f.to_string())
    }
    let (an, ai, af) = split(a);
    let (bn, bi, bf) = split(b);
    let scale = af.len().max(bf.len());
    let to_int = |neg: bool, i: &str, f: &str| -> Option<i128> {
        let digits = format!("{i}{f:0<scale$}");
        let v: i128 = digits.parse().ok()?;
        Some(if neg { -v } else { v })
    };
    let exact = (|| {
        let diff = to_int(an, &ai, &af)?.checked_sub(to_int(bn, &bi, &bf)?)?;
        let neg = diff < 0;
        let digits = diff.unsigned_abs().to_string();
        let raw = if scale == 0 {
            digits
        } else {
            let padded = format!("{digits:0>width$}", width = scale + 1);
            let (i, f) = padded.split_at(padded.len() - scale);
            format!("{i}.{f}")
        };
        Amount::parse(&format!("{}{raw}", if neg { "-" } else { "" }))
    })();
    exact
        .or_else(|| Amount::from_f64(a.to_f64() - b.to_f64()))
        .unwrap_or_else(Amount::zero)
}

// ── Wire types ──────────────────────────────────────────────────────────────────

/// `total_credits` and `total_usage` are required: a body missing either
/// fails the parse (and drops the wallet meter) rather than inventing one.
#[derive(Debug, Clone, Deserialize)]
struct CreditsEnvelope {
    data: CreditsData,
}

#[derive(Debug, Clone, Deserialize)]
struct CreditsData {
    total_credits: RawAmount,
    total_usage: RawAmount,
}

/// `data` is required, so an error envelope never reads as usable usage.
#[derive(Debug, Clone, Deserialize)]
struct KeyEnvelope {
    data: KeySnapshot,
}

#[cfg(test)]
#[path = "../../tests/inline/providers_openrouter.rs"]
mod tests;
