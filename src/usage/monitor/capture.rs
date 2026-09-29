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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) struct CaptureHttp<'a> {
    inner: &'a dyn MonitorHttp,
    sink: Arc<CaptureSink>,
}
struct CaptureSink {
    dir: Option<PathBuf>,
    sequence: Mutex<(String, usize)>,
}
impl<'a> CaptureHttp<'a> {
    pub(crate) fn new(inner: &'a dyn MonitorHttp, dir: Option<&Path>) -> Result<Self> {
        if let Some(dir) = dir {
            if crate::identity::upstream_active() {
                let home = crate::profile::home_dir()?;
                let candidate = if dir.is_absolute() {
                    dir.to_path_buf()
                } else {
                    std::env::current_dir()?.join(dir)
                };
                let root = home.join(".tollgate");
                anyhow::ensure!(
                    candidate.starts_with(&root)
                        && !candidate
                            .components()
                            .any(|part| matches!(part, std::path::Component::ParentDir)),
                    "guest mode: capture directory must be inside ~/.tollgate/"
                );
                // A lexical descendant must not escape through an existing link.
                let mut prefix = home;
                for part in candidate.strip_prefix(&prefix)?.components() {
                    prefix.push(part);
                    if let Ok(meta) = std::fs::symlink_metadata(&prefix) {
                        anyhow::ensure!(
                            !meta.file_type().is_symlink(),
                            "guest mode: capture directory must not traverse a symlink"
                        );
                    }
                }
            }
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
            sink: Arc::new(CaptureSink {
                dir: dir.map(Path::to_path_buf),
                sequence: Mutex::new(("monitor".into(), 0)),
            }),
        })
    }
    pub(crate) fn set_id(&self, id: &str) {
        if let Ok(mut seq) = self.sink.sequence.lock() {
            *seq = (id.into(), 0);
        }
    }
    fn record(&self, reply: &HttpReply) -> Result<()> {
        self.sink.record(reply)
    }
    fn captured_provider<T>(
        &self,
        fetch: impl FnOnce() -> Result<T, ThirdPartyError>,
    ) -> Result<T, ThirdPartyError> {
        if self.sink.dir.is_none() {
            return fetch();
        }
        let sink = Arc::clone(&self.sink);
        let failed = Arc::new(AtomicBool::new(false));
        let observer_failed = Arc::clone(&failed);
        let observer = Arc::new(move |reply: &crate::usage::keyed_http::Reply| {
            let Some(body) = &reply.body else { return };
            if sink
                .record(&HttpReply {
                    status: reply.status,
                    body: body.clone(),
                    headers: reply.headers.clone(),
                    retry_after_secs: reply.retry_after.map(|delay| delay.as_secs()),
                })
                .is_err()
            {
                observer_failed.store(true, Ordering::Relaxed);
            }
        });
        let result = crate::usage::keyed_http::with_response_observer(observer, fetch);
        if failed.load(Ordering::Relaxed) {
            Err(ThirdPartyError::Parse)
        } else {
            result
        }
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
impl CaptureSink {
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
        let headers: Vec<(String, String)> = reply
            .headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                crate::usage::keyed_http::response_header_allowed(&name)
                    .then(|| (name, value.chars().take(256).collect()))
            })
            .take(64)
            .collect();
        let dump = serde_json::json!({"status":reply.status,"headers":headers,"body":body});
        crate::profile::atomic_write_600(
            &dir.join(format!("{}-{}.shape.json", seq.0, seq.1)),
            &serde_json::to_vec_pretty(&dump)?,
        )
        .context("write response shape")
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
        if self.sink.dir.is_none() {
            return self.inner.codex_usage(token, account, fedramp, now);
        }
        let failed = std::cell::Cell::new(false);
        let result = self.inner.codex_usage_captured(
            token,
            account,
            fedramp,
            now,
            &|status, body, headers| {
                if self
                    .record(&HttpReply {
                        status,
                        body: body.into(),
                        headers: headers.to_vec(),
                        retry_after_secs: None,
                    })
                    .is_err()
                {
                    failed.set(true);
                }
            },
        );
        if failed.get() {
            Err(FetchError::Parse)
        } else {
            result
        }
    }

    fn third_party(
        &self,
        target: &ThirdPartyTarget,
        key: &Secret,
    ) -> Result<ThirdPartyStats, ThirdPartyError> {
        self.captured_provider(|| self.inner.third_party(target, key))
    }
    fn openrouter_wallet(&self, key: &Secret) -> Result<ThirdPartyStats, ThirdPartyError> {
        self.captured_provider(|| self.inner.openrouter_wallet(key))
    }
}
#[cfg(test)]
#[path = "../../../tests/inline/usage_monitor_capture.rs"]
mod tests;
