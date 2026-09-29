//! Offline discovery. No credential is displayed and no CLI is executed.
use super::{
    cli::preset,
    config::{self, MonitorConfig},
};
use crate::out::{errln, outln};
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::Path;

#[derive(Debug, Serialize)]
pub(crate) struct DetectRow {
    found: String,
    preset: String,
    flags: Vec<String>,
    reason: String,
    state: String,
    #[serde(skip)]
    config: Option<MonitorConfig>,
}
fn proposal(
    found: String,
    name: &str,
    flags: Vec<String>,
    reason: String,
    m: MonitorConfig,
    existing: &[MonitorConfig],
) -> DetectRow {
    let matched = existing
        .iter()
        .find(|old| old.fingerprint() == m.fingerprint());
    DetectRow {
        found,
        preset: name.into(),
        flags,
        reason: matched.map_or(reason, |old| format!("already monitored as '{}'", old.id)),
        state: if matched.is_some() {
            "monitored"
        } else {
            "proposed"
        }
        .into(),
        config: matched.is_none().then_some(m),
    }
}
fn skipped(found: String, reason: &str) -> DetectRow {
    DetectRow {
        found,
        preset: String::new(),
        flags: vec![],
        reason: reason.into(),
        state: "skipped".into(),
        config: None,
    }
}
fn read_shape(path: &Path) -> Option<Value> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 1_048_576 {
        return None;
    }
    let shape = serde_json::from_slice::<SafeShape>(&bytes)
        .ok()
        .map(|shape| shape.0);
    use zeroize::Zeroize;
    bytes.zeroize();
    shape
}
/// Parse field spellings and permitted expiry metadata. Unknown values,
/// including refresh tokens, are consumed with IgnoredAny, never allocated.
struct SafeShape(Value);
impl<'de> serde::Deserialize<'de> for SafeShape {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct ShapeVisitor;
        impl<'de> serde::de::Visitor<'de> for ShapeVisitor {
            type Value = SafeShape;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an auth object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut fields = serde_json::Map::new();
                while let Some(name) = map.next_key::<String>()? {
                    let official = name.split_once("::").is_some_and(|(issuer, _)| {
                        matches!(
                            issuer,
                            "https://auth.x.ai" | "https://accounts.x.ai/sign-in"
                        )
                    });
                    let value =
                        if official || matches!(name.as_str(), "providers" | "nous" | "tokens") {
                            map.next_value::<SafeShape>()?.0
                        } else if matches!(name.as_str(), "key" | "access_token") {
                            let token = map.next_value::<Option<super::source::Secret>>()?;
                            if token.as_ref().is_some_and(|t| {
                                crate::codex_auth::jwt_exp_ms(t.expose()).is_some()
                            }) {
                                Value::String("jwt".into())
                            } else {
                                Value::Null
                            }
                        } else if matches!(name.as_str(), "expires_at" | "expiry_date") {
                            map.next_value::<ExpiryShape>()?.0
                        } else {
                            map.next_value::<serde::de::IgnoredAny>()?;
                            Value::Null
                        };
                    fields.insert(name, value);
                }
                Ok(SafeShape(Value::Object(fields)))
            }
        }
        deserializer.deserialize_map(ShapeVisitor)
    }
}
struct ExpiryShape(Value);
impl<'de> serde::Deserialize<'de> for ExpiryShape {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct ExpiryVisitor;
        impl<'de> serde::de::Visitor<'de> for ExpiryVisitor {
            type Value = ExpiryShape;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an expiry scalar")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(ExpiryShape(
                    if chrono::DateTime::parse_from_rfc3339(v).is_ok() {
                        Value::String("rfc3339".into())
                    } else {
                        Value::Null
                    },
                ))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(ExpiryShape(Value::Number(v.into())))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(ExpiryShape(Value::Number(v.into())))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(ExpiryShape(Value::Null))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(ExpiryShape(Value::Null))
            }
        }
        deserializer.deserialize_any(ExpiryVisitor)
    }
}
/// Fixture-friendly discovery; environment names only, no values.
#[cfg(test)]
pub(crate) fn discover(
    home: &Path,
    names: &BTreeSet<String>,
    existing: &[MonitorConfig],
    explain: bool,
) -> Vec<DetectRow> {
    discover_in(home, names, existing, explain, None, None)
}
fn discover_in(
    home: &Path,
    names: &BTreeSet<String>,
    existing: &[MonitorConfig],
    explain: bool,
    codex_override: Option<std::path::PathBuf>,
    hermes_override: Option<std::path::PathBuf>,
) -> Vec<DetectRow> {
    let mut rows = Vec::new();
    let grok = home.join(".grok/auth.json");
    if grok.exists() {
        if let Some(Value::Object(entries)) = read_shape(&grok) {
            let official: Vec<_> = entries
                .iter()
                .filter(|(name, _)| {
                    name.split_once("::").is_some_and(|(issuer, _)| {
                        matches!(
                            issuer,
                            "https://auth.x.ai" | "https://accounts.x.ai/sign-in"
                        )
                    })
                })
                .collect();
            for (name, entry) in &official {
                let Ok(mut m) = preset("grok") else { continue };
                let mut flags = Vec::new();
                if official.len() > 1 {
                    m.auth_entry = Some((*name).clone());
                    flags.extend(["--auth-entry".into(), (*name).clone()]);
                }
                let mut reason = "official CLI login".to_string();
                if explain {
                    reason = format!(
                        "fields: {}; expiry: {}; entries: {}",
                        fields(entry),
                        expiry(entry),
                        official.len()
                    );
                }
                rows.push(proposal(
                    "~/.grok/auth.json".into(),
                    "grok",
                    flags,
                    reason,
                    m,
                    existing,
                ));
            }
            if official.is_empty() {
                rows.push(skipped(
                    "~/.grok/auth.json".into(),
                    "no official Grok login",
                ));
            }
        } else {
            rows.push(skipped(
                "~/.grok/auth.json".into(),
                "auth.json is unreadable or being rewritten",
            ));
        }
    }
    let codex_home = codex_override.unwrap_or_else(|| home.join(".codex"));
    let codex = codex_home.join("auth.json");
    if let Ok(meta) = std::fs::symlink_metadata(&codex) {
        if meta.file_type().is_symlink() {
            rows.push(skipped(
                codex.display().to_string(),
                "managed by a profile store (symlink)",
            ));
        } else if meta.is_file()
            && let Ok(mut m) = preset("codex-native")
        {
            let mut flags = vec![];
            if codex_home != home.join(".codex") {
                m.tool_home = Some(codex_home.display().to_string());
                flags = vec!["--tool-home".into(), codex_home.display().to_string()];
            }
            let reason = if explain {
                read_shape(&codex).map_or_else(
                    || "unreadable auth shape".into(),
                    |v| format!("fields: {}; expiry: jwt; entries: 1", fields(&v)),
                )
            } else {
                "regular CLI login file".into()
            };
            rows.push(proposal(
                codex.display().to_string(),
                "codex-native",
                flags,
                reason,
                m,
                existing,
            ));
        }
    }
    let mut hermes_homes = BTreeSet::from([home.join(".hermes")]);
    if let Some(path) = hermes_override {
        hermes_homes.insert(path);
    }
    if let Ok(entries) = std::fs::read_dir(home.join(".hermes/profiles")) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                hermes_homes.insert(entry.path());
            }
        }
    }
    for dir in hermes_homes {
        if !dir.exists() {
            continue;
        }
        if let Some(v) = read_shape(&dir.join("auth.json"))
            && v.pointer("/providers/nous").is_some()
        {
            if let Ok(mut m) = preset("nous") {
                let path = if dir == home.join(".hermes") {
                    "~/.hermes".into()
                } else {
                    dir.display().to_string()
                };
                m.hermes_home = Some(path.clone());
                let reason = if explain {
                    format!(
                        "fields: {}; expiry: {}; entries: 1",
                        fields(&v["providers"]["nous"]),
                        expiry(&v["providers"]["nous"])
                    )
                } else {
                    "Hermes Nous login".into()
                };
                rows.push(proposal(
                    dir.display().to_string(),
                    "nous",
                    vec!["--hermes-home".into(), path],
                    reason,
                    m,
                    existing,
                ));
            }
        } else {
            rows.push(skipped(
                dir.display().to_string(),
                "Hermes has no Nous login (run hermes, then /login)",
            ));
        }
    }
    for (key, name) in [
        ("OPENROUTER_API_KEY", "openrouter"),
        ("NOUS_API_KEY", "nous-key"),
        ("OPENAI_API_KEY", "openai"),
        ("GEMINI_API_KEY", "google-ai"),
        ("GOOGLE_API_KEY", "google-ai"),
    ] {
        if !names.contains(key) {
            continue;
        }
        if let Ok(mut m) = preset(name) {
            m.api_key_env = Some(key.into());
            let mut flags = vec![];
            if key == "GOOGLE_API_KEY" {
                flags.extend(["--api-key-env".into(), key.into()]);
            }
            if name == "openai" && names.contains("OPENAI_ADMIN_KEY") {
                m.billing_key_env = Some("OPENAI_ADMIN_KEY".into());
                flags.extend(["--admin-key-env".into(), "OPENAI_ADMIN_KEY".into()]);
            }
            rows.push(proposal(
                format!("${key}"),
                name,
                flags,
                "key name present (value hidden)".into(),
                m,
                existing,
            ));
        }
    }
    rows
}
fn fields(v: &Value) -> String {
    v.as_object().map_or_else(
        || "non-object".into(),
        |o| o.keys().map(String::as_str).collect::<Vec<_>>().join(", "),
    )
}
fn expiry(v: &Value) -> &'static str {
    match v.get("expires_at").or_else(|| v.get("expiry_date")) {
        Some(Value::String(s)) if s == "rfc3339" => "rfc3339",
        Some(Value::Number(n)) if n.as_i64().is_some_and(|n| n > 10_000_000_000) => "epoch_ms",
        Some(Value::Number(_)) => "epoch_s",
        _ if v
            .get("key")
            .or_else(|| v.get("access_token"))
            .and_then(Value::as_str)
            .is_some_and(|s| s == "jwt") =>
        {
            "jwt"
        }
        _ => "absent",
    }
}
pub(crate) fn run(json: bool, explain: bool, apply: bool, yes: bool) -> Result<()> {
    let home = crate::profile::home_dir()?;
    let names = crate::secrets::stored_names()
        .into_iter()
        .chain(
            std::env::vars()
                .filter(|(_, v)| !v.trim().is_empty())
                .map(|(n, _)| n),
        )
        .collect();
    let existing = config::load()?;
    let mut rows = discover_in(
        &home,
        &names,
        &existing,
        explain,
        if cfg!(test) {
            None
        } else {
            std::env::var_os("CODEX_HOME").map(Into::into)
        },
        if cfg!(test) {
            None
        } else {
            std::env::var_os("HERMES_HOME").map(Into::into)
        },
    );
    let agy_found = home.join(".local/bin/agy").is_file()
        || std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|dir| dir.join("agy").is_file()));
    if agy_found {
        match super::antigravity::keyring_metadata() {
            Ok(metadata) => {
                if metadata.unlocked == 1 && metadata.locked == 0 {
                    rows.push(proposal(
                        "agy keyring item".into(),
                        "antigravity",
                        vec![],
                        if explain { "one unlocked Secret Service item; blob shape unavailable (metadata only; secret not read)".into() } else { "one unlocked Secret Service item; secret not read".into() },
                        preset("antigravity")?,
                        &existing,
                    ));
                } else {
                    rows.push(skipped(
                        "agy keyring".into(),
                        &format!(
                            "{} unlocked, {} locked items; secret not read",
                            metadata.unlocked, metadata.locked
                        ),
                    ));
                }
            }
            Err(_) => rows.push(skipped(
                "agy".into(),
                "Secret Service unavailable; no secret read",
            )),
        }
    }
    if json {
        outln!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        for row in &rows {
            if row.state == "proposed" {
                outln!(
                    "{}  →  tollgate monitor add {} {}   ({})",
                    row.found,
                    row.preset,
                    row.flags
                        .iter()
                        .map(|f| shell_word(f))
                        .collect::<Vec<_>>()
                        .join(" "),
                    row.reason
                );
            } else {
                outln!(
                    "{}  →  {}{}",
                    row.found,
                    if row.state == "skipped" {
                        "skipped: "
                    } else {
                        ""
                    },
                    row.reason
                );
            }
        }
        if crate::identity::upstream_active() {
            outln!("Claude accounts: via upstream clauth (import to manage)");
        }
    }
    if apply {
        if !yes {
            if !std::io::stdin().is_terminal() {
                bail!("tollgate: monitor detect --apply requires --yes off a terminal");
            }
            errln!("Add all proposed monitors? [y/N]");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y") {
                return Ok(());
            }
        }
        for row in rows {
            if let Some(mut m) = row.config {
                let base = m.id.clone();
                let mut n = 2;
                while config::load()?.iter().any(|old| old.id == m.id) {
                    m.id = format!("{base}-{n}");
                    n += 1;
                }
                config::add(&m)?;
            }
        }
    }
    Ok(())
}
fn shell_word(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_/.~".contains(&b))
    {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_detect.rs"]
mod tests;
