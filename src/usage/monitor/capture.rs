//! Response structure capture, never requests or credentials.
use super::config::MonitorKind;
use super::source::{HttpReply, MonitorHttp, Secret};
use crate::providers::{ThirdPartyError, ThirdPartyStats, ThirdPartyTarget};
use crate::usage::fetch::{FetchError, UsageInfo};
use crate::usage::keyed_http::Request;
use crate::usage::observation::Failure;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) struct CaptureHttp<'a> {
    inner: &'a dyn MonitorHttp,
    dir: Option<PathBuf>,
    sequence: Mutex<(String, usize)>,
}
impl<'a> CaptureHttp<'a> {
    pub(crate) fn new(inner: &'a dyn MonitorHttp, dir: Option<&Path>) -> Result<Self> {
        if let Some(dir) = dir {
            if let Ok(meta) = std::fs::symlink_metadata(dir) {
                anyhow::ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "capture directory must be a regular directory"
                );
            }
            crate::profile::mkdir_700(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(Self {
            inner,
            dir: dir.map(Path::to_path_buf),
            sequence: Mutex::new(("monitor".into(), 0)),
        })
    }
    pub(crate) fn set_id(&self, id: &str) {
        if let Ok(mut seq) = self.sequence.lock() {
            *seq = (id.into(), 0);
        }
    }
    fn record(&self, reply: &HttpReply) -> Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let body = serde_json::from_str::<Value>(&reply.body)
            .map(shape)
            .unwrap_or_else(|_| Value::String(format!("<str:{}>", reply.body.len())));
        let mut seq = self
            .sequence
            .lock()
            .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?;
        seq.1 += 1;
        let dump = serde_json::json!({"status":reply.status,"headers":reply.headers,"body":body});
        crate::profile::atomic_write_600(
            &dir.join(format!("{}-{}.shape.json", seq.0, seq.1)),
            &serde_json::to_vec_pretty(&dump)?,
        )
        .context("write response shape")
    }
    fn captured(&self, result: Result<HttpReply, Failure>) -> Result<HttpReply, Failure> {
        let reply = result?;
        self.record(&reply).map_err(|_| {
            Failure::new(
                crate::usage::observation::FailureKind::Unavailable,
                "could not write response shape",
            )
        })?;
        Ok(reply)
    }
}
pub(crate) fn shape(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("<str:{}>", s.len())),
        Value::Array(values) => Value::Array(values.into_iter().map(shape).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(k, v)| {
                    let keep = matches!(
                        k.as_str(),
                        "type"
                            | "product"
                            | "bucketId"
                            | "window"
                            | "displayName"
                            | "plan_type"
                            | "code"
                            | "reason"
                            | "subscriptionTier"
                    );
                    (k, if keep && v.is_string() { v } else { shape(v) })
                })
                .collect(),
        ),
        other => other,
    }
}
impl MonitorHttp for CaptureHttp<'_> {
    fn get_bearer(&self, url: &str, token: &Secret) -> Result<HttpReply, Failure> {
        self.captured(self.inner.get_bearer(url, token))
    }
    fn send(&self, kind: MonitorKind, req: &Request<'_>) -> Result<HttpReply, Failure> {
        self.captured(self.inner.send(kind, req))
    }
    fn codex_usage(
        &self,
        token: &Secret,
        account: Option<&str>,
        fedramp: bool,
        now: i64,
    ) -> Result<UsageInfo, FetchError> {
        let usage = self.inner.codex_usage(token, account, fedramp, now)?;
        if self.dir.is_some() {
            let body = serde_json::to_string(&usage).map_err(|_| FetchError::Parse)?;
            self.record(&HttpReply {
                status: 200,
                body,
                headers: vec![],
                retry_after_secs: None,
            })
            .map_err(|_| FetchError::Parse)?;
        }
        Ok(usage)
    }
    fn third_party(
        &self,
        target: &ThirdPartyTarget,
        key: &Secret,
    ) -> Result<ThirdPartyStats, ThirdPartyError> {
        self.inner.third_party(target, key)
    }
    fn openrouter_wallet(&self, key: &Secret) -> Result<ThirdPartyStats, ThirdPartyError> {
        self.inner.openrouter_wallet(key)
    }
}
#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_capture.rs"]
mod tests;
