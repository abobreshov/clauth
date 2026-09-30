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
#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_codex_native.rs"]
mod tests;
