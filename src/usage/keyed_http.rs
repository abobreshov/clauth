//! The one transport every key-bearing provider read goes through: the
//! OpenRouter, Ollama Cloud, DeepSeek, Z.ai, MiniMax and generic usage GETs,
//! and the monitors' bearer reads (plan v3.1 §4.2).
//!
//! The policy is the monitor agent's, applied to every read that carries an
//! API key or a borrowed token:
//!
//! - **No redirect is followed.** A 3xx is refused as no answer at all, so a
//!   key-bearing request never lands anywhere but the origin it was built
//!   for (ureq already drops `Authorization` on a redirect; not following
//!   one at all also keeps the reply from being some other host's body).
//! - **Bodies are capped at [`MAX_BODY_BYTES`]** (2 MiB). A larger body is
//!   unreadable, never buffered whole.
//! - **Every call has a finite end-to-end deadline** ([`CALL_DEADLINE`]).
//!   ureq's `timeout_recv_response` is re-armed per header byte and the
//!   body has no bound of its own, so without `timeout_global` a server that
//!   sends its headers and then stalls would hold the caller (the
//!   sequential monitor poll, the usage scheduler) for as long as the socket
//!   lives.

use std::sync::LazyLock;
use std::time::Duration;

/// Largest response body a key-bearing read accepts (plan §4.2: 2 MiB).
pub(crate) const MAX_BODY_BYTES: u64 = 2 * 1024 * 1024;

/// End-to-end bound on one key-bearing call: connect, headers and body.
pub(crate) const CALL_DEADLINE: Duration = Duration::from_secs(20);

/// Connect budget (TLS handshake included).
const CONNECT_SECS: u64 = 4;
/// Header-receive budget (re-armed per byte by ureq; [`CALL_DEADLINE`] is the
/// real bound).
const RECV_HEADERS_SECS: u64 = 8;

/// The agent policy with `deadline` as its end-to-end bound. Split out so a
/// test can prove the stall bound without waiting out [`CALL_DEADLINE`].
pub(crate) fn build_agent(deadline: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(CONNECT_SECS).min(deadline)))
        .timeout_recv_response(Some(Duration::from_secs(RECV_HEADERS_SECS).min(deadline)))
        .timeout_recv_body(Some(deadline))
        .timeout_global(Some(deadline))
        // Statuses on the `Ok` side: callers read 401 / 429 / 403 off it.
        .http_status_as_error(false)
        // Never follow a redirect with a key.
        .max_redirects(0)
        .build()
        .into()
}

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| build_agent(CALL_DEADLINE));

/// The shared key-bearing agent.
pub(crate) fn agent() -> &'static ureq::Agent {
    &AGENT
}

/// One answer to a key-bearing GET.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    /// `retry-after`, parsed and clamped ([`crate::usage::parse_retry_after`]).
    pub(crate) retry_after: Option<Duration>,
    /// The body, or `None` when it could not be read inside the cap and the
    /// deadline.
    pub(crate) body: Option<String>,
}

/// `GET url` with `Authorization: Bearer <bearer>` over [`agent`]. `None` when
/// no usable answer arrived: a transport failure, a deadline, or a redirect
/// (refused, never followed).
pub(crate) fn get_bearer(url: &str, bearer: &str) -> Option<Reply> {
    get_bearer_with(agent(), url, bearer)
}

/// [`get_bearer`] over a caller-chosen agent (tests: a short deadline).
pub(crate) fn get_bearer_with(agent: &ureq::Agent, url: &str, bearer: &str) -> Option<Reply> {
    let mut response = agent
        .get(url)
        .header("Authorization", &format!("Bearer {bearer}"))
        .header("Accept", "application/json")
        .call()
        .ok()?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return None;
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::usage::parse_retry_after);
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_string()
        .ok();
    Some(Reply {
        status,
        retry_after,
        body,
    })
}

#[cfg(test)]
#[path = "../../tests/inline/usage_keyed_http.rs"]
mod tests;
