//! Generic per-profile disk-cache IO.
//!
//! Both the OAuth usage layer (`usage/fetch.rs`) and the third-party provider
//! layer (`providers/mod.rs`) persist one JSON file per profile under the same
//! per-profile dir, with the same atomic-write + None-on-error semantics. This
//! module owns that shared IO once; the two layers only differ in their cache
//! filename and the concrete type.

use std::any::TypeId;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::profile::{AccountId, ProfileName};

/// Filename of the OAuth usage cache, relative to the per-profile dir.
pub(crate) const USAGE_CACHE_FILE: &str = "usage_cache.json";
/// Filename of the third-party provider cache, relative to the per-profile dir.
pub(crate) const THIRD_PARTY_CACHE_FILE: &str = "third_party_cache.json";

/// The account uuid this profile's login last authenticated as (a bare JSON
/// string). Derived data, backfilled on login and on every successful mirror
/// adoption — the identity anchor that lets an unattended adopt refuse a live
/// login belonging to a DIFFERENT account (`oauth::try_adopt_live_rotation`).
pub(crate) const ACCOUNT_ID_CACHE_FILE: &str = "account_id.json";

/// Local-only details observed by the login's existing `/profile` read.
pub(crate) const ACCOUNT_IDENTITY_FILE: &str = "account_identity.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AccountIdentity {
    pub(crate) account_uuid: AccountId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) organization_type: Option<String>,
    pub(crate) verified_at_ms: u64,
}

impl AccountIdentity {
    #[cfg(test)]
    pub(crate) fn uuid_only(account_uuid: AccountId, verified_at_ms: u64) -> Self {
        Self {
            account_uuid,
            email: None,
            organization_uuid: None,
            organization_name: None,
            organization_type: None,
            verified_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoginIdentity {
    Pending,
    Unanchored,
    Anchored {
        account: AccountId,
        details: Option<AccountIdentity>,
    },
}

/// Bind details to the current anchor so a torn two-file write cannot display
/// another login's email. In-memory test profiles never resolve the real home.
pub(crate) fn login_identity(name: &ProfileName) -> LoginIdentity {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return LoginIdentity::Unanchored;
    }
    if identity_pending(name) {
        return LoginIdentity::Pending;
    }
    let Some(account) = account_anchor(name) else {
        return LoginIdentity::Unanchored;
    };
    let details = load_profile_cache::<AccountIdentity>(name, ACCOUNT_IDENTITY_FILE)
        .filter(|identity| identity.account_uuid == account);
    LoginIdentity::Anchored { account, details }
}

/// A login was installed without a resolved identity. Persisted before the
/// credential write so cross-process walks never use the displaced anchor.
pub(crate) const IDENTITY_PENDING_FILE: &str = "identity_pending.json";

pub(crate) fn begin_login_install(name: &ProfileName) -> anyhow::Result<()> {
    let path = crate::profile::profile_subpath(name, IDENTITY_PENDING_FILE)?;
    crate::profile::atomic_write_600(&path, b"true")?;
    Ok(())
}

pub(crate) fn identity_pending(name: &ProfileName) -> bool {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return false;
    }
    profile_cache_path(name, IDENTITY_PENDING_FILE).is_some_and(|path| path.exists())
}

/// Only newly unproven installs are blocked: legacy profiles without anchors
/// keep their existing behavior. An unknown active login also cannot walk to
/// another OAuth account it might duplicate, until its normal probe resolves.
pub(crate) fn unverified_walk_member(
    config: &crate::profile::AppConfig,
    name: &ProfileName,
    active: Option<&ProfileName>,
) -> Option<ProfileName> {
    let profile = config.find(name)?;
    if !profile.is_oauth() {
        return None;
    }
    if identity_pending(name) {
        return Some(name.clone());
    }
    active
        .filter(|active| {
            config
                .find(active)
                .is_some_and(crate::profile::Profile::is_oauth)
                && identity_pending(active)
        })
        .cloned()
}

/// Read a usable identity anchor. In-memory-only test profiles have no home
/// or persisted anchor; never resolve the operator's home for those fixtures.
fn account_anchor(name: &ProfileName) -> Option<crate::profile::AccountId> {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return None;
    }
    if identity_pending(name) {
        return None;
    }
    load_profile_cache::<crate::profile::AccountId>(name, ACCOUNT_ID_CACHE_FILE)
        .filter(|id| !id.trim().is_empty())
}

/// Another roster profile holding the same resolved Anthropic account. Reads
/// the anchors the login and hourly `/profile` legs already derive; no HTTP.
pub(crate) fn same_account_as(
    config: &crate::profile::AppConfig,
    name: &ProfileName,
) -> Option<ProfileName> {
    let account = account_anchor(name)?;
    config.profiles.iter().find_map(|p| {
        (p.name != *name && account_anchor(&p.name).as_ref() == Some(&account))
            .then(|| p.name.clone())
    })
}

/// A later duplicate is not a walk candidate. Also exclude the earlier member
/// when walking FROM its duplicate, including a session on a non-global member:
/// either direction would switch credentials without moving quota.
pub(crate) fn duplicate_walk_member(
    config: &crate::profile::AppConfig,
    name: &ProfileName,
    active: Option<&ProfileName>,
) -> Option<ProfileName> {
    let account = account_anchor(name)?;
    if let Some(active) = active
        && active != name
        && account_anchor(active).as_ref() == Some(&account)
    {
        return Some(active.clone());
    }
    config
        .state
        .fallback_chain
        .iter()
        .take_while(|n| *n != name)
        .find(|n| config.find(n).is_some() && account_anchor(n).as_ref() == Some(&account))
        .cloned()
}

/// The login warning is shared by the CLI commit and TUI toast paths.
pub(crate) fn duplicate_login_warning(
    config: &crate::profile::AppConfig,
    name: &ProfileName,
) -> Option<String> {
    duplicate_login_warning_for_account(config, name, &account_anchor(name)?)
}

/// A completed login can still be waiting in a TUI draft or confirmation. Its
/// proven uuid is enough to warn before the new anchor is committed.
pub(crate) fn duplicate_login_warning_for_account(
    config: &crate::profile::AppConfig,
    name: &ProfileName,
    account: &crate::profile::AccountId,
) -> Option<String> {
    config.profiles.iter().find_map(|p| {
        (p.name != *name && account_anchor(&p.name).as_ref() == Some(account))
            .then(|| p.name.clone())
    }).map(|other| format!(
        "WARNING: '{name}' is the same Anthropic account as '{other}'; the browser was signed in as the other account. These profiles share quota; duplicate fallback members are skipped."
    ))
}

/// The last adopt refusal announced for this profile, as one of the reason
/// keys `oauth::try_adopt_live_rotation` refuses with (a bare JSON string).
/// The refusal is a STANDING state while the live slot stays unadoptable, and
/// the rotation legs that reach it run in DIFFERENT processes (the daemon
/// scheduler and the TUI's in-process scheduler), so the once-per-state
/// announcement is recorded on disk where every process reads it instead of
/// in either process's memory.
pub(crate) const ADOPT_REFUSAL_FILE: &str = "adopt_refusal.json";

/// Epoch-ms of this profile's last `/profile` fetch attempt (a bare JSON number).
/// Derived data: the durable half of `usage::fetch`'s once-per-hour-per-profile
/// TTL clock, so a relaunch reuses the cached plan instead of re-pulling
/// `/profile` for every profile at once.
pub(crate) const PROFILE_FETCHED_CACHE_FILE: &str = "profile_fetched.json";

/// Per-profile kick-429 block (`usage::scheduler::KickBlock`): written by the
/// fetching instance so a standdown TUI can mirror the judgment and a restart
/// doesn't forget a live block mid-outage; removed the moment a kick lands.
pub(crate) const KICK_BLOCK_CACHE_FILE: &str = "kick_block.json";

/// The last third-party fetch for this profile died on a credential that can
/// never self-heal ([`crate::usage::FetchStatus::AuthExpired`]), recorded
/// against the fingerprint of the credential that produced it.
///
/// It exists for the surfaces with no scheduler in the process — `tollgate list`
/// and `tollgate status --json` — which otherwise derive freshness from the usage
/// cache's mtime alone and so report a warm cache behind a dead session as
/// `Fresh`: a live measurement, over a credential that will never come back.
/// The refusal splitter `oauth::third_party_dead_chain_copy` is a third reader:
/// a matched verdict demotes the dead-chain sentence to the keyless one.
///
/// This is NOT a freshness claim and cannot go stale in the dangerous
/// direction: it is a terminal verdict BOUND to one credential, so a re-login
/// changes the fingerprint and the record stops applying on its own — the same
/// invalidation rule the scheduler's in-memory suppression uses.
pub(crate) const THIRD_PARTY_AUTH_FILE: &str = "third_party_auth.json";

/// Body of [`THIRD_PARTY_AUTH_FILE`]. A struct rather than a bare number so a
/// second field can join without a format break.
#[derive(Debug, Serialize, Deserialize)]
struct AuthVerdict {
    /// `ThirdPartyEntry::credential_fingerprint` of the credential that failed.
    credential: u64,
}

/// Record that `name`'s usage credential is dead. Best-effort like every other
/// per-profile cache: a record that never lands leaves the reader on the
/// mtime derivation, which is the pre-record answer rather than a worse one.
pub(crate) fn write_auth_expired(name: &ProfileName, credential: u64) {
    write_profile_cache(name, THIRD_PARTY_AUTH_FILE, &AuthVerdict { credential });
}

/// Drop the record — any outcome other than `AuthExpired` proves the verdict no
/// longer holds. Cheap and unconditional: the file is usually absent.
pub(crate) fn clear_auth_expired(name: &ProfileName) {
    remove_profile_cache(name, THIRD_PARTY_AUTH_FILE);
}

/// Whether `name` carries a dead-credential verdict for the credential it holds
/// RIGHT NOW. A record for any other fingerprint is inert — that is what makes
/// this safe to persist.
pub(crate) fn auth_expired_matches(name: &ProfileName, credential: u64) -> bool {
    load_profile_cache::<AuthVerdict>(name, THIRD_PARTY_AUTH_FILE)
        .is_some_and(|v| v.credential == credential)
}

/// The one credential-store mtime bump tollgate makes with NO bytes behind it
/// ([`TouchReceipt`]). Sits beside the store it describes, so
/// [`effective_write_time`] resolves it from the store's own path.
/// Per-profile parked MCP-server logins (`claude::park_mcp_logins_from_store`),
/// written only while a profile stores no credentials file of its own to hold
/// them. Carries none of the account's own credential material: the blocks
/// inside are minted per (MCP server, endpoint) and belong to no Claude account,
/// which is why a profile that stops storing a login still has to keep them.
pub(crate) const MCP_LOGINS_FILE: &str = "mcp-logins.json";

pub(crate) const TOUCH_RECEIPT_FILE: &str = "touch-receipt.json";

/// A store mtime tollgate moved without writing the store.
///
/// The per-session swap executor must move the mtime of the store it repoints to
/// — Claude Code re-reads credentials only when that value changes, so an
/// mtime-preserving repoint is a silent no-op — but every OTHER reader of that
/// mtime is asking when the bytes were last WRITTEN, and answers a bare stamp as
/// "just now". This is what separates the two.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TouchReceipt {
    /// File name of the stamped store. A profile can hold both a
    /// `credentials.json` and a `session-token.json` and only ever one of them
    /// is stamped, so a receipt for one must not resolve the other.
    store: String,
    /// The mtime the stamp actually landed on, read back rather than predicted —
    /// a coarse filesystem truncates the value asked for. A store whose mtime has
    /// moved off this has been genuinely written since, which retires the
    /// receipt.
    stamped: SystemTime,
    /// The mtime the stamp displaced: when the store's bytes were last written.
    prev: Option<SystemTime>,
}

/// Record that `name`'s `store` carries a bare stamp. Best-effort like every
/// other per-profile cache — a receipt that never lands leaves the readers on the
/// raw mtime, which is the pre-receipt answer rather than a worse one.
pub(crate) fn write_touch_receipt(
    name: &ProfileName,
    store: &Path,
    stamped: SystemTime,
    prev: Option<SystemTime>,
) {
    let Some(file) = store.file_name().and_then(|f| f.to_str()) else {
        return;
    };
    debug_assert!(
        crate::profile::profile_dir(name).ok().as_deref() == store.parent(),
        "the receipt is read back from the store's own directory, so it has to be written there",
    );
    write_profile_cache(
        name,
        TOUCH_RECEIPT_FILE,
        &TouchReceipt {
            store: file.to_string(),
            stamped,
            prev,
        },
    );
}

/// When `store`'s bytes were last written: its mtime, unless a [`TouchReceipt`]
/// beside it identifies that exact mtime as a bare stamp — in which case the
/// value that stamp displaced.
///
/// Every decision that compares store mtimes to answer "which side was written
/// last" resolves through here. A receipt that is absent, unreadable,
/// unparseable, for the other store, or retired by a later write yields the raw
/// mtime, so a lost or stale receipt costs exactly the pre-receipt answer.
pub(crate) fn effective_write_time(store: &Path) -> Option<SystemTime> {
    // Returns before the receipt read when the store is absent: nothing can have
    // displaced a write that never happened, and a profile with no sidecar is the
    // common case on the reload fingerprint's per-profile walk.
    let mtime = std::fs::metadata(store)
        .ok()
        .and_then(|m| m.modified().ok())?;
    let Some(file) = store.file_name().and_then(|f| f.to_str()) else {
        return Some(mtime);
    };
    std::fs::read_to_string(store.with_file_name(TOUCH_RECEIPT_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<TouchReceipt>(&text).ok())
        .filter(|receipt| receipt.store == file && mtime == receipt.stamped)
        .map_or(Some(mtime), |receipt| receipt.prev)
}

/// Resolve `<profile_dir>/<file>` for `name`. `None` only when the per-profile
/// dir itself can't be resolved (matches the prior per-layer `cache_path`).
pub(crate) fn profile_cache_path(name: &ProfileName, file: &str) -> Option<PathBuf> {
    // `profile_dir` (override-aware) rather than raw `dirs::home_dir`, so tests
    // never touch the real `~/.tollgate`.
    crate::profile::profile_dir(name).ok().map(|p| p.join(file))
}

/// The cache file each usage-cache payload type is written to, so a call site
/// pairing the wrong constant with the wrong type fails the read instead of
/// deserializing a foreign file into an all-default value. An all-`serde(default)`
/// payload (e.g. [`crate::usage::UsageInfo`]) parses ANY JSON object into a
/// valid-looking all-default answer, so the constant↔type mismatch is invisible
/// to a parse and no test can catch it — the defect a two-part fix once shipped
/// half-swallowed by. Payloads with required fields fail a mismatched read on
/// their own (`ThirdPartyStats`'s `is_available`/`rows`), and `u64` /
/// `serde_json::Value` parse anything by design; neither needs an entry.
///
/// A `TypeId` table rather than a trait bound: a `CachePayload` trait could not
/// be implemented here for the one payload private to another module
/// (`throughput::ThroughputStore`), and the bound would force an edit in its
/// home. The table keeps the guard beside the constants it protects.
fn expected_cache_file<T: 'static>() -> Option<&'static str> {
    match TypeId::of::<T>() {
        t if t == TypeId::of::<AccountIdentity>() => Some(ACCOUNT_IDENTITY_FILE),
        t if t == TypeId::of::<crate::usage::UsageInfo>() => Some(USAGE_CACHE_FILE),
        t if t == TypeId::of::<crate::providers::ThirdPartyStats>() => Some(THIRD_PARTY_CACHE_FILE),
        _ => None,
    }
}

/// Read + deserialize `<profile_dir>/<file>`. `None` on missing file or any
/// read/parse error — the caller treats both as "no cache" (matches the prior
/// per-layer loaders exactly) — and `None` again when `file` is not the cache
/// this payload type is written to, so a wrong constant cannot read an
/// all-default value out of a foreign file (see [`expected_cache_file`]).
pub(crate) fn load_profile_cache<T: DeserializeOwned + 'static>(
    name: &ProfileName,
    file: &str,
) -> Option<T> {
    if let Some(expected) = expected_cache_file::<T>()
        && expected != file
    {
        return None;
    }
    profile_cache_path(name, file).and_then(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        serde_json::from_str::<T>(&text).ok()
    })
}

/// Atomically write `value` to `<profile_dir>/<file>`. Failures are swallowed
/// (cache is best-effort): a missing parent is created at 0o700, the file at
/// 0o600, via a tmp + rename so a torn write reads as no cache rather than a
/// parse failure.
///
/// Skips names the on-disk record no longer carries: the fetch legs hold a
/// stale in-memory config for up to a tick, and the parent creation above
/// would re-create a deleted profile's directory. Fail-closed — an unreadable
/// record skips the write too. The tick-driven callers (usage fetch,
/// scheduler) retry on the next tick; one-shot writers degrade to their
/// documented safe answers instead (a lost touch receipt yields the raw
/// mtime, an unseeded anchor reads as unanchored). Cost: one read + TOML
/// parse of `profiles.toml` per write — the record is small, and the same
/// read already backs the oauth adopt gate and the daemon's switch-target
/// existence check.
///
/// "The record" means EITHER roster. The gate exists to skip names that are no
/// longer accounts, and a codex profile is an account — it simply lives in
/// `codex-profiles.toml`, which `is_configured` cannot see (it reads
/// `profiles.toml` alone, by design). Checking only that one silently dropped
/// every codex usage reading, so the cache the published feed and the Overview
/// resolve BY NAME stayed empty forever while the fetch itself succeeded.
pub(crate) fn write_profile_cache<T: Serialize>(name: &ProfileName, file: &str, value: &T) {
    let _ = write_profile_cache_reported(name, file, value);
}

/// [`write_profile_cache`] that reports whether the file write actually landed,
/// so a caller whose cache is load-bearing (the identity anchor) can log a
/// failure instead of taking the default silent swallow. `false` covers every
/// skip (off-roster, unresolvable path, serialization) and a failed write alike.
pub(crate) fn write_profile_cache_reported<T: Serialize>(
    name: &ProfileName,
    file: &str,
    value: &T,
) -> bool {
    let configured = crate::profile::is_configured(name).unwrap_or(false)
        || crate::codex_profiles::CodexState::load().is_ok_and(|s| s.holds(name.as_str()));
    if !configured {
        return false;
    }
    let Some(path) = profile_cache_path(name, file) else {
        return false;
    };
    let Ok(json) = serde_json::to_string(value) else {
        return false;
    };
    crate::profile::atomic_write_600(&path, json.as_bytes()).is_ok()
}

/// Delete `<profile_dir>/<file>`. Best-effort: an already-absent file is the
/// intended post-state, and every other removal error is logged (never surfaced
/// as a failure) — a stale cache is worse than the log line that names it.
pub(crate) fn remove_profile_cache(name: &ProfileName, file: &str) {
    let Some(path) = profile_cache_path(name, file) else {
        return;
    };
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        crate::logline::logline!("tollgate: {name}: could not remove its {file} cache: {e}");
    }
}

/// Epoch-ms of `<profile_dir>/<file>`'s last write, or `None` when it's absent.
pub(crate) fn profile_cache_mtime_ms(name: &ProfileName, file: &str) -> Option<u64> {
    let modified = std::fs::metadata(profile_cache_path(name, file)?)
        .ok()?
        .modified()
        .ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

#[cfg(test)]
#[path = "../tests/inline/profile_cache.rs"]
mod tests;
