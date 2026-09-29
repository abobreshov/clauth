//! Borrow agy's Secret Service login without activating or unlocking a wallet.
use super::{
    config::MonitorKind,
    source::{HttpReply, MonitorHttp, MonitorTarget, Reading, Secret, UsageSource},
};
use crate::usage::{
    keyed_http::{Auth, Method, Request},
    observation::{AuthKind, Failure, FailureKind, QuotaWindow, SourceId, Timestamp, WindowScope},
};
use serde::Deserialize;
use serde_json::Value;
use zeroize::Zeroizing;
pub(crate) struct AntigravitySource;
#[derive(Debug)]
pub(crate) struct KeyringMetadata {
    pub(crate) unlocked: usize,
    pub(crate) locked: usize,
}
pub(crate) trait KeyringProbe {
    fn has_owner(&self) -> Result<bool, Failure>;
    fn metadata(&self) -> Result<KeyringMetadata, Failure>;
    fn secret(&self) -> Result<Zeroizing<Vec<u8>>, Failure>;
}
pub(crate) struct LiveKeyring;
fn unavailable() -> Failure {
    Failure::new(
        FailureKind::Unavailable,
        "no Secret Service running; tollgate will not start one",
    )
}
// Plain sessions keep the implementation to zbus alone. The local session bus is
// the trust boundary: no CLI, subprocess, Unlock, Prompt, or service activation.
// Resolve and pin its unique owner on every connection, so losing the owner after
// NameHasOwner can never activate a replacement wallet.
#[cfg(all(target_os = "linux", not(test)))]
struct ServiceConnection {
    bus: zbus::blocking::Connection,
    owner: String,
}
#[cfg(all(target_os = "linux", not(test)))]
impl ServiceConnection {
    fn connect() -> Result<Self, Failure> {
        let bus = zbus::blocking::Connection::session().map_err(|_| unavailable())?;
        let dbus = zbus::blocking::Proxy::new(
            &bus,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .map_err(|_| unavailable())?;
        let present: bool = dbus
            .call("NameHasOwner", &("org.freedesktop.secrets",))
            .map_err(|_| unavailable())?;
        if !present {
            return Err(unavailable());
        }
        let owner: String = dbus
            .call("GetNameOwner", &("org.freedesktop.secrets",))
            .map_err(|_| unavailable())?;
        drop(dbus);
        Ok(Self { bus, owner })
    }
    fn proxy<'a>(
        &'a self,
        path: &'a str,
        interface: &'a str,
    ) -> Result<zbus::blocking::Proxy<'a>, Failure> {
        zbus::blocking::Proxy::new(&self.bus, self.owner.as_str(), path, interface)
            .map_err(|_| unavailable())
    }
    fn search(
        &self,
    ) -> Result<
        (
            Vec<zbus::zvariant::OwnedObjectPath>,
            Vec<zbus::zvariant::OwnedObjectPath>,
        ),
        Failure,
    > {
        let proxy = self.proxy("/org/freedesktop/secrets", "org.freedesktop.Secret.Service")?;
        proxy
            .call_with_flags(
                "SearchItems",
                zbus::proxy::MethodFlags::NoAutoStart.into(),
                &(std::collections::HashMap::from([
                    ("service", "gemini"),
                    ("username", "antigravity"),
                ]),),
            )
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)
    }
}
impl KeyringProbe for LiveKeyring {
    fn has_owner(&self) -> Result<bool, Failure> {
        #[cfg(all(target_os = "linux", not(test)))]
        {
            let bus = zbus::blocking::Connection::session().map_err(|_| unavailable())?;
            let proxy = zbus::blocking::Proxy::new(
                &bus,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
            )
            .map_err(|_| unavailable())?;
            proxy
                .call("NameHasOwner", &("org.freedesktop.secrets",))
                .map_err(|_| unavailable())
        }
        #[cfg(any(not(target_os = "linux"), test))]
        {
            Err(unavailable())
        }
    }
    fn metadata(&self) -> Result<KeyringMetadata, Failure> {
        #[cfg(all(target_os = "linux", not(test)))]
        {
            let service = ServiceConnection::connect()?;
            let (unlocked, locked) = service.search()?;
            Ok(KeyringMetadata {
                unlocked: unlocked.len(),
                locked: locked.len(),
            })
        }
        #[cfg(any(not(target_os = "linux"), test))]
        {
            Err(unavailable())
        }
    }
    fn secret(&self) -> Result<Zeroizing<Vec<u8>>, Failure> {
        #[cfg(all(target_os = "linux", not(test)))]
        {
            let service = ServiceConnection::connect()?;
            let (unlocked, locked) = service.search()?;
            check_items(&KeyringMetadata {
                unlocked: unlocked.len(),
                locked: locked.len(),
            })?;
            let proxy =
                service.proxy("/org/freedesktop/secrets", "org.freedesktop.Secret.Service")?;
            let (_output, session): (zbus::zvariant::OwnedValue, zbus::zvariant::OwnedObjectPath) =
                proxy
                    .call_with_flags(
                        "OpenSession",
                        zbus::proxy::MethodFlags::NoAutoStart.into(),
                        &("plain", zbus::zvariant::Value::from("")),
                    )
                    .map_err(|_| unavailable())?
                    .ok_or_else(unavailable)?;
            let result = (|| {
                let item = service.proxy(unlocked[0].as_str(), "org.freedesktop.Secret.Item")?;
                let secret: (zbus::zvariant::OwnedObjectPath, Vec<u8>, Vec<u8>, String) = item
                    .call_with_flags(
                        "GetSecret",
                        zbus::proxy::MethodFlags::NoAutoStart.into(),
                        &(&session,),
                    )
                    .map_err(|_| unavailable())?
                    .ok_or_else(unavailable)?;
                let bytes = Zeroizing::new(secret.2);
                if bytes.len() > 1024 * 1024 {
                    return Err(Failure::new(
                        FailureKind::Unavailable,
                        "agy login blob is too large",
                    ));
                }
                Ok(bytes)
            })();
            // Closing a session cannot display a prompt, including on an error.
            if let Ok(proxy) = service.proxy(session.as_str(), "org.freedesktop.Secret.Session") {
                let _: Result<Option<()>, _> = proxy.call_with_flags(
                    "Close",
                    zbus::proxy::MethodFlags::NoAutoStart.into(),
                    &(),
                );
            }
            result
        }
        #[cfg(any(not(target_os = "linux"), test))]
        {
            Err(unavailable())
        }
    }
}
pub(crate) fn keyring_metadata() -> Result<KeyringMetadata, Failure> {
    if !LiveKeyring.has_owner()? {
        return Err(unavailable());
    }
    LiveKeyring.metadata()
}
fn check_items(m: &KeyringMetadata) -> Result<(), Failure> {
    if m.unlocked + m.locked > 1 {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "several agy logins in the keyring",
        ));
    }
    if m.locked > 0 {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "keyring locked; unlock your session (tollgate never unlocks it)",
        ));
    }
    if m.unlocked == 0 {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "no agy login in the keyring; open agy",
        ));
    }
    Ok(())
}
#[derive(Deserialize)]
struct Blob {
    token: Option<Token>,
    #[serde(flatten)]
    flat: Token,
}
#[derive(Deserialize)]
struct Token {
    access_token: Option<Secret>,
    #[serde(
        default,
        alias = "expires_at",
        alias = "expiresAt",
        deserialize_with = "super::grok::present_expiry"
    )]
    expiry: Option<Value>,
}
pub(crate) fn parse_blob(bytes: &[u8], now: i64) -> Result<Secret, Failure> {
    if bytes.len() > 1024 * 1024 {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "agy login blob is too large",
        ));
    }
    use base64::Engine;
    let decoded = if let Some(data) = bytes.strip_prefix(b"go-keyring-base64:") {
        Some(Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| {
                    Failure::new(FailureKind::AuthRequired, "agy login blob is unreadable")
                })?,
        ))
    } else {
        None
    };
    let blob: Blob =
        serde_json::from_slice(decoded.as_deref().map(|v| v.as_slice()).unwrap_or(bytes))
            .map_err(|_| Failure::new(FailureKind::AuthRequired, "agy login blob is unreadable"))?;
    let token = blob.token.unwrap_or(blob.flat);
    let access = token
        .access_token
        .filter(|t| !t.expose().is_empty() && !t.expose().chars().any(char::is_whitespace))
        .ok_or_else(|| Failure::new(FailureKind::AuthRequired, "agy access token is missing"))?;
    let expiry = match token.expiry.as_ref() {
        Some(value) => super::nous::value_timestamp(value).map(|t| t.secs()),
        None => crate::codex_auth::jwt_exp_ms(access.expose()).map(|ms| ms / 1000),
    };
    if expiry.is_none_or(|exp| exp <= now.saturating_add(60)) {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "agy's token expired; open agy for a moment",
        ));
    }
    Ok(access)
}
pub(crate) fn read_token(probe: &dyn KeyringProbe, now: i64) -> Result<Secret, Failure> {
    if !probe.has_owner()? {
        return Err(unavailable());
    }
    check_items(&probe.metadata()?)?;
    parse_blob(&probe.secret()?, now)
}
fn status(reply: &HttpReply, now: i64) -> Result<(), Failure> {
    match reply.status {
        200..=299 => Ok(()),
        403 if reply.body.contains("SUBSCRIPTION_REQUIRED") => Err(Failure::new(
            FailureKind::SubscriptionInactive,
            "free plan: no quota summary",
        )),
        401 | 403 => Err(Failure::new(
            FailureKind::AuthRequired,
            "open agy and sign in",
        )),
        429 => {
            let delay = reply
                .headers
                .iter()
                .find(|(k, _)| k == "retry-after")
                .and_then(|(_, v)| crate::usage::parse_retry_after_at(v, now))
                .map(|d| d.as_secs())
                .unwrap_or(900)
                .max(900);
            let mut f = Failure::new(FailureKind::RateLimited, "agy rate limited");
            f.retry_after = Some(Timestamp::from_secs(now.saturating_add(delay as i64)));
            Err(f)
        }
        _ => Err(Failure::new(
            FailureKind::Unavailable,
            "agy quota summary unavailable",
        )),
    }
}
#[cfg(target_os = "linux")]
pub(crate) fn fetch_with(
    target: &MonitorTarget,
    http: &dyn MonitorHttp,
    probe: &dyn KeyringProbe,
) -> Result<Reading, Failure> {
    if target.cfg.via.as_deref() == Some("cli") {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "agy print mode is disabled until the owner records gate AGY-CLI",
        ));
    }
    let token = read_token(probe, target.now_secs)?;
    let send = |host: &str, path: &str, agent: &'static str| {
        let url = format!("https://{host}.googleapis.com/v1internal:{path}");
        http.send(
            MonitorKind::Antigravity,
            &Request {
                method: Method::Post,
                url: &url,
                auth: Auth::Bearer(&token),
                extra: &[("User-Agent", agent)],
                json_body: Some(b"{}"),
            },
        )
    };
    let mut host = "daily-cloudcode-pa";
    let first = send(host, "retrieveUserQuotaSummary", "antigravity");
    let quota = if first
        .as_ref()
        .map_or(true, |r| r.status == 404 || r.status >= 500)
    {
        host = "cloudcode-pa";
        send(host, "retrieveUserQuotaSummary", "antigravity")?
    } else {
        first?
    };
    status(&quota, target.now_secs)?;
    let quota: Value = serde_json::from_str(&quota.body)
        .map_err(|_| Failure::new(FailureKind::Unavailable, "agy quota response unrecognized"))?;
    let mut reading = map_summary(&quota)?;
    let previous = target.previous.as_ref();
    reading.plan = previous.and_then(|p| p.plan.clone());
    reading.plan_checked_at = previous.and_then(|p| p.plan_checked_at);
    if reading
        .plan_checked_at
        .is_none_or(|at| target.now_secs.saturating_sub(at) >= 86400)
    {
        let plan = send(host, "loadCodeAssist", "agy")?;
        status(&plan, target.now_secs)?;
        let plan: Value = serde_json::from_str(&plan.body).map_err(|_| {
            Failure::new(FailureKind::Unavailable, "agy plan response unrecognized")
        })?;
        let plan = plan.get("response").unwrap_or(&plan);
        reading.plan = plan["paidTier"]["name"]
            .as_str()
            .or_else(|| plan["currentTier"]["name"].as_str())
            .map(str::to_string);
        reading.plan_checked_at = Some(target.now_secs);
    }
    Ok(reading)
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn fetch_with(
    _: &MonitorTarget,
    _: &dyn MonitorHttp,
    _: &dyn KeyringProbe,
) -> Result<Reading, Failure> {
    Err(Failure::new(FailureKind::Unavailable, "Linux only in 0.1"))
}
impl UsageSource for AntigravitySource {
    fn source_id(&self, _: &MonitorTarget) -> SourceId {
        SourceId::Antigravity
    }
    fn auth_kind(&self, _: &MonitorTarget) -> AuthKind {
        AuthKind::NativeLogin
    }
    fn fetch(&self, target: &MonitorTarget, http: &dyn MonitorHttp) -> Result<Reading, Failure> {
        fetch_with(target, http, &LiveKeyring)
    }
}
pub(crate) fn map_summary(value: &Value) -> Result<Reading, Failure> {
    let value = value.get("response").unwrap_or(value);
    let groups = value["groups"]
        .as_array()
        .filter(|g| !g.is_empty())
        .ok_or_else(|| {
            Failure::new(
                FailureKind::Unavailable,
                "no quota summary is available for this account",
            )
        })?;
    let mut reading = Reading::default();
    for group in groups {
        let label = group["displayName"].as_str().unwrap_or("Antigravity");
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            let Some(id) = bucket["bucketId"].as_str() else {
                continue;
            };
            let (duration, secs) = match bucket["window"].as_str() {
                Some("5h") => ("5h", Some(18000)),
                Some("weekly" | "7d") => ("7d", Some(604800)),
                _ => ("quota", None),
            };
            let mut window = QuotaWindow::new(
                format!("agy.{id}"),
                format!("{duration} {label}"),
                WindowScope::Model {
                    models: vec![label.into()],
                },
            );
            window.window_secs = secs;
            window.used_pct = bucket["remainingFraction"]
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .map(|v| (1. - v) * 100.);
            window.exhausted = window.used_pct.is_some_and(|v| v >= 100.);
            window.resets_at = super::nous::value_timestamp(&bucket["resetTime"]);
            reading.windows.push(window);
        }
    }
    if reading.windows.is_empty() {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "no quota summary is available for this account",
        ));
    }
    Ok(reading)
}
#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_antigravity.rs"]
mod tests;
