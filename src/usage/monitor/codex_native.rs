//! Native Codex login read-only. Never enters the refresh scheduler.
use super::source::{MonitorHttp, MonitorTarget, Reading, Secret, UsageSource};
use crate::usage::{
    fetch::FetchError,
    observation::{
        AccountObservation, AuthKind, Failure, FailureKind, Origin, SourceId, Timestamp,
    },
};
use serde::Deserialize;
use std::{io::Read, path::Path};
pub(crate) struct CodexNativeSource;
#[derive(Deserialize, Debug)]
struct Auth {
    tokens: Tokens,
    #[serde(default)]
    chatgpt_account_is_fedramp: bool,
}
#[derive(Deserialize, Debug)]
struct Tokens {
    access_token: Secret,
    account_id: Option<String>,
}
#[expect(
    unsafe_code,
    reason = "geteuid reads the effective owner for the borrowed store check"
)]
fn read_auth(home: &Path, now: i64) -> Result<Auth, Failure> {
    let path = home.join("auth.json");
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|_| Failure::new(FailureKind::AuthRequired, "no codex login; run codex"))?;
    if meta.file_type().is_symlink() {
        let dest = std::fs::read_link(&path).unwrap_or_default();
        let text = dest.to_string_lossy();
        let message = if let Some(rest) = text.split("/.tollgate/profiles/").nth(1) {
            format!(
                "~/.codex/auth.json is profile '{}'s store; it is watched as codex:{}",
                rest.split('/').next().unwrap_or("?"),
                rest.split('/').next().unwrap_or("?")
            )
        } else if let Some(rest) = text.split("/.clauth/profiles/").nth(1) {
            format!(
                "~/.codex/auth.json is upstream clauth's '{}'; see upstream:{}",
                rest.split('/').next().unwrap_or("?"),
                rest.split('/').next().unwrap_or("?")
            )
        } else {
            "codex's auth.json is a symlink; refusing to borrow it".into()
        };
        return Err(Failure::new(FailureKind::Unavailable, &message));
    }
    if !meta.is_file() || meta.len() >= 1024 * 1024 {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "codex's auth.json is not a regular private store",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "codex's auth.json has a foreign owner",
            ));
        }
    }
    // O_NOFOLLOW also closes a symlink substitution between lstat and open.
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|_| Failure::new(FailureKind::Unavailable, "could not read codex's auth.json"))?;
    let opened = file.metadata().map_err(|_| {
        Failure::new(
            FailureKind::Unavailable,
            "could not inspect codex's auth.json",
        )
    })?;
    if !opened.is_file() || opened.len() >= 1024 * 1024 {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "codex's auth.json is not a regular store",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.uid() != unsafe { libc::geteuid() } {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "codex's auth.json has a foreign owner",
            ));
        }
    }
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::new(FailureKind::Unavailable, "could not read codex's auth.json"))?;
    if bytes.len() >= 1024 * 1024 {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "codex's auth.json is too large",
        ));
    }
    let auth: Auth = serde_json::from_slice(&bytes).map_err(|_| {
        Failure::new(
            FailureKind::Unavailable,
            "codex's auth.json is being rewritten; keeping the last reading",
        )
    })?;
    if crate::codex_auth::jwt_exp_ms(auth.tokens.access_token.expose())
        .is_none_or(|exp| exp / 1000 <= now.saturating_add(60))
    {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "codex's token expired; run codex",
        ));
    }
    Ok(auth)
}
impl UsageSource for CodexNativeSource {
    fn source_id(&self, _: &MonitorTarget) -> SourceId {
        SourceId::Codex
    }
    fn auth_kind(&self, _: &MonitorTarget) -> AuthKind {
        AuthKind::NativeLogin
    }
    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
        let auth = read_auth(&target.cfg.tool_home_in(&target.home), target.now_secs)?;
        let usage = http
            .codex_usage(
                &auth.tokens.access_token,
                auth.tokens.account_id.as_deref(),
                auth.chatgpt_account_is_fedramp,
                target.now_secs,
            )
            .map_err(|e| match e {
                FetchError::Status(401 | 403) => Failure::new(
                    FailureKind::AuthRequired,
                    "codex's login was rejected; run codex",
                ),
                FetchError::Status(429) => {
                    Failure::new(FailureKind::RateLimited, "codex rate limited")
                }
                FetchError::RateLimited { retry_after, .. } => {
                    let mut f = Failure::new(FailureKind::RateLimited, "codex rate limited");
                    f.retry_after = retry_after.map(|d| {
                        Timestamp::from_secs(target.now_secs.saturating_add(d.as_secs() as i64))
                    });
                    f
                }
                _ => Failure::new(FailureKind::Unavailable, "codex usage unavailable"),
            })?;
        let mut obs = AccountObservation::new(
            "native".into(),
            SourceId::Codex,
            AuthKind::NativeLogin,
            Origin::Monitor,
            "Codex",
        );
        crate::usage::project::apply_codex_usage(&mut obs, &usage, target.now_secs);
        Ok(Reading {
            plan: obs.plan,
            windows: obs.windows,
            money: obs.money,
            verdict: obs.failure,
            ..Reading::default()
        })
    }
}
pub(crate) fn has_recovered_login(
    cfg: &super::config::MonitorConfig,
    failure_msg: &str,
    checked_at_ms: u64,
    now_secs: i64,
) -> bool {
    let Ok(home) = crate::profile::home_dir() else {
        return false;
    };
    let tool_home = cfg.tool_home_in(&home);
    if read_auth(&tool_home, now_secs).is_err() {
        return false;
    }
    failure_msg != "codex's login was rejected; run codex"
        || super::cache::file_modified_after(&tool_home.join("auth.json"), checked_at_ms)
}

/// The monitor row an official login gets when nobody has added one.
/// Same non-secret fields as `tollgate monitor add codex-native`.
fn official_monitor() -> super::config::MonitorConfig {
    super::config::MonitorConfig {
        id: "codex-native".into(),
        kind: super::config::MonitorKind::CodexNative,
        provider: None,
        label: Some("Codex (~/.codex login)".into()),
        api_key_env: None,
        billing_key_env: None,
        hermes_home: None,
        tool_home: Some("~/.codex".into()),
        auth_entry: None,
        via: None,
        probe_model: None,
        probe: false,
        budget_usd_month: None,
        alert_pct: None,
        ttl_secs: Some(300),
        enabled: true,
    }
}

fn single_official_login(home: &Path, now_secs: i64) -> bool {
    read_auth(&home.join(".codex"), now_secs).is_ok()
}

pub(crate) fn ensure_official_codex_config(now_ms: u64) {
    let Ok(home) = crate::profile::home_dir() else {
        return;
    };
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    if !single_official_login(&home, now_secs) {
        return;
    }
    let Ok(monitors) = super::config::load() else {
        return;
    };
    if monitors
        .iter()
        .any(|m| m.kind == super::config::MonitorKind::CodexNative)
    {
        return;
    }
    let cfg = official_monitor();
    let _ = super::config::add(&cfg);
}

fn fill_if_empty(cfg: &super::config::MonitorConfig, http: &dyn MonitorHttp, now_ms: u64) {
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    if let Some(cache) = super::cache::load(&cfg.id)
        && cache.matches(cfg)
        && (cache.reading.is_some() || cache.checked_at_ms.is_some())
    {
        let checked_ms = cache.checked_at_ms.unwrap_or(0);
        let recovered = cache.failure.as_ref().is_some_and(|f| {
            f.kind == FailureKind::AuthRequired
                && has_recovered_login(cfg, &f.message, checked_ms, now_secs)
        });
        if !recovered {
            return;
        }
    }
    let deps = super::cache::RefreshDeps {
        http,
        notifier: None,
        env: &super::source::process_env,
        now_ms,
    };
    let _ = super::cache::refresh_one(cfg, &deps, true);
}

/// Make one official regular-file `~/.codex/auth.json` login visible to the
/// normal monitor collector.
///
/// Writes only tollgate's own monitor row and cache, and only when that row
/// is missing or its cache has never been filled. Does not refresh, lock, or
/// rewrite the Codex auth store. A missing, expired, or symlinked login
/// leaves the report unchanged.
pub(crate) fn ensure_official_codex(http: &dyn MonitorHttp, now_ms: u64) {
    let Ok(home) = crate::profile::home_dir() else {
        return;
    };
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    if !single_official_login(&home, now_secs) {
        return;
    }
    let Ok(monitors) = super::config::load() else {
        return;
    };
    if let Some(existing) = monitors
        .iter()
        .find(|m| m.kind == super::config::MonitorKind::CodexNative)
    {
        if existing.enabled {
            fill_if_empty(existing, http, now_ms);
        }
        return;
    }
    let cfg = official_monitor();
    if super::config::add(&cfg).is_err() {
        if let Ok(monitors) = super::config::load()
            && let Some(existing) = monitors
                .iter()
                .find(|m| m.kind == super::config::MonitorKind::CodexNative && m.enabled)
        {
            fill_if_empty(existing, http, now_ms);
        }
        return;
    }
    fill_if_empty(&cfg, http, now_ms);
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_codex_native.rs"]
mod tests;
