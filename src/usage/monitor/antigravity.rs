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
#[cfg(test)]
thread_local! { static TEST_KEYRING: std::cell::RefCell<Option<std::rc::Rc<dyn KeyringProbe>>> = const { std::cell::RefCell::new(None) }; }
/// Scope a fake keyring to a real cache/source refresh in this test thread.
#[cfg(test)]
pub(crate) fn with_test_keyring<T>(
    probe: std::rc::Rc<dyn KeyringProbe>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<std::rc::Rc<dyn KeyringProbe>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_KEYRING.with(|p| *p.borrow_mut() = self.0.take());
        }
    }
    let previous = TEST_KEYRING.with(|p| p.replace(Some(probe)));
    let _restore = Restore(previous);
    run()
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
    match expiry {
        Some(exp) if exp > now.saturating_add(60) => Ok(access),
        Some(_) => Err(Failure::new(
            FailureKind::AuthRequired,
            "agy's token expired; open agy for a moment",
        )),
        None => Err(Failure::new(
            FailureKind::AuthRequired,
            "agy's token carries no expiry; open agy",
        )),
    }
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
        if !AGY_CLI_OWNER_GATE_RECORDED {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "agy print mode is disabled until the owner records gate AGY-CLI",
            ));
        }
        return fetch_cli(&LiveCliRunner, target.now_secs);
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
        #[cfg(test)]
        if let Some(probe) = TEST_KEYRING.with(|p| p.borrow().clone()) {
            return fetch_with(target, http, probe.as_ref());
        }
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
// This gate intentionally remains false until the owner records help/usage
// captures and observes that print mode never starts a sign-in browser.
pub(crate) const AGY_CLI_OWNER_GATE_RECORDED: bool = false;
const MAX_CLI_BYTES: u64 = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CliCall {
    Version,
    Help,
    Usage,
}
trait CliRunner {
    fn stamp(&self) -> Result<Option<CliBinaryStamp>, Failure> {
        Ok(None)
    }
    fn run(&self, call: CliCall) -> Result<String, Failure>;
}
struct LiveCliRunner;
fn cli_command(program: &std::path::Path, call: CliCall) -> std::process::Command {
    let mut command = crate::providers::billing_key::helper_command(program);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for name in ["DISPLAY", "WAYLAND_DISPLAY", "BROWSER"] {
        command.env_remove(name);
    }
    match call {
        CliCall::Version => {
            command.arg("--version");
        }
        CliCall::Help => {
            command.args(["-p", "/help", "--output-format", "json"]);
        }
        CliCall::Usage => {
            command.args(["-p", "/usage", "--output-format", "json"]);
        }
    }
    command
}
fn run_cli_command(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Result<String, Failure> {
    use std::io::Read;
    let mut child = command
        .spawn()
        .map_err(|_| Failure::new(FailureKind::Unavailable, "could not run agy print mode"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Failure::new(FailureKind::Unavailable, "agy output unavailable"))?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Zeroizing::new(Vec::new());
        let read = stdout
            .take(MAX_CLI_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(read);
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(Failure::new(
                    FailureKind::Unavailable,
                    "agy print mode timed out",
                ));
            }
        }
    };
    // A killed descendant could keep its stdout descriptor alive. Do not wait
    // for its reader after the deadline; the read remains capped and owns no key.
    let status = status?;
    let bytes = receiver
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        .map_err(|_| Failure::new(FailureKind::Unavailable, "agy output timed out"))?
        .map_err(|_| Failure::new(FailureKind::Unavailable, "agy output unavailable"))?;
    if bytes.len() as u64 > MAX_CLI_BYTES {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "agy output exceeds 1 MiB",
        ));
    }
    let output = String::from_utf8(bytes.to_vec())
        .map_err(|_| Failure::new(FailureKind::Unavailable, "agy output is not UTF-8"))?;
    if sign_in_flow(&output) {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "open agy and sign in",
        ));
    }
    if !status.success() {
        return Err(Failure::new(
            FailureKind::Unavailable,
            "agy print mode failed",
        ));
    }
    Ok(output)
}
#[derive(Clone, PartialEq, Eq)]
struct CliBinaryStamp {
    path: std::path::PathBuf,
    modified: Option<std::time::SystemTime>,
    len: u64,
}
#[cfg(not(test))]
fn cli_binary() -> Result<CliBinaryStamp, Failure> {
    let path = std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("agy"))
                .find(|p| p.is_file())
        })
        .or_else(|| {
            crate::profile::home_dir()
                .ok()
                .map(|p| p.join(".local/bin/agy"))
                .filter(|p| p.is_file())
        })
        .ok_or_else(|| Failure::new(FailureKind::Unavailable, "agy is not installed"))?;
    let meta = std::fs::metadata(&path)
        .map_err(|_| Failure::new(FailureKind::Unavailable, "could not inspect agy"))?;
    Ok(CliBinaryStamp {
        path,
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}
impl CliRunner for LiveCliRunner {
    fn stamp(&self) -> Result<Option<CliBinaryStamp>, Failure> {
        #[cfg(test)]
        {
            panic!("live agy inspection in test");
        }
        #[cfg(not(test))]
        {
            cli_binary().map(Some)
        }
    }
    fn run(&self, call: CliCall) -> Result<String, Failure> {
        #[cfg(test)]
        {
            let _ = call;
            panic!("live agy invocation in test");
        }
        #[cfg(not(test))]
        {
            let stamp = cli_binary()?;
            run_cli_command(
                cli_command(&stamp.path, call),
                std::time::Duration::from_secs(30),
            )
        }
    }
}
fn version_supported(output: &str) -> bool {
    output
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter_map(|s| {
            let mut parts = s.split('.');
            Some((
                parts.next()?.parse::<u64>().ok()?,
                parts.next()?.parse::<u64>().ok()?,
                parts.next()?.parse::<u64>().ok()?,
            ))
        })
        .any(|v| v >= (1, 1, 11))
}
fn contains_usage_command(value: &Value) -> bool {
    match value {
        Value::String(s) => s
            .split_whitespace()
            .any(|word| word.trim_matches(['"', ',', ':', '`']) == "/usage"),
        Value::Array(a) => a.iter().any(contains_usage_command),
        Value::Object(o) => o
            .iter()
            .any(|(k, v)| k == "/usage" || contains_usage_command(v)),
        _ => false,
    }
}
fn sign_in_flow(output: &str) -> bool {
    let lowercase = output.to_ascii_lowercase();
    ["sign in", "sign-in", "login", "https://", "http://"]
        .iter()
        .any(|marker| lowercase.contains(marker))
}
fn fetch_cli(runner: &dyn CliRunner, now: i64) -> Result<Reading, Failure> {
    // Both probes are offline. The production caller remains behind the owner
    // gate; tests exercise this path only with a fixture runner.
    static PROBES: std::sync::Mutex<Vec<(CliBinaryStamp, i64)>> = std::sync::Mutex::new(Vec::new());
    let stamp = runner.stamp()?;
    let memoized = stamp.as_ref().is_some_and(|stamp| {
        PROBES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(old, at)| old == stamp && now.saturating_sub(*at) < 60)
    });
    if !memoized {
        let version = runner.run(CliCall::Version)?;
        if !version_supported(&version) {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "agy has no print-mode /usage (requires 1.1.11)",
            ));
        }
        let help: Value = serde_json::from_str(&runner.run(CliCall::Help)?).map_err(|_| {
            Failure::new(FailureKind::Unavailable, "agy print-mode help unrecognized")
        })?;
        if !contains_usage_command(&help) {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "agy print-mode help does not list /usage",
            ));
        }
        if let Some(stamp) = stamp {
            let mut probes = PROBES.lock().unwrap_or_else(|e| e.into_inner());
            probes.retain(|(old, _)| old.path != stamp.path);
            if probes.len() >= 8 {
                probes.remove(0);
            }
            probes.push((stamp, now));
        }
    }
    let output = runner.run(CliCall::Usage)?;
    if sign_in_flow(&output) {
        return Err(Failure::new(
            FailureKind::AuthRequired,
            "open agy and sign in",
        ));
    }
    let value: Value = serde_json::from_str(&output).map_err(|_| {
        Failure::new(
            FailureKind::Unavailable,
            "agy usage schema awaits owner capture",
        )
    })?;
    let mut reading = map_summary(&value)?;
    let root = value.get("response").unwrap_or(&value);
    reading.plan = root["paidTier"]["name"]
        .as_str()
        .or_else(|| root["currentTier"]["name"].as_str())
        .map(str::to_string);
    reading.plan_checked_at = Some(now);
    Ok(reading)
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_antigravity.rs"]
mod tests;
