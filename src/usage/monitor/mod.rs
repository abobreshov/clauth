//! Monitors: usage sources that are NOT Claude Code or codex profiles (plan
//! v3.1 §4.2, §4.3, §4.7 Nous, §4.8) — a Nous Portal account read through
//! Hermes' login, a billing-only OpenRouter key, another provider's key.
//!
//! - [`config`]: `~/.tollgate/monitors.toml` (no secrets; env var names only).
//! - [`source`]: the [`source::UsageSource`] trait, its registry, and the HTTP
//!   seam with the monitoring allowlist.
//! - [`nous`]: the Nous source (Hermes' unexpired access token, read-only).
//! - [`cache`]: `~/.tollgate/monitors/<id>.json` — TTL, 429 hold, 7-day stale
//!   retention, single-flight flock.
//! - [`observe`]: config + cache → `monitor:<id>` observations, budgets, and
//!   the collector hook.
//! - [`alert`]: notify-send on a severity or `alert_pct` crossing.
//! - [`poll`]: the daemon tick's detached poll.
//! - [`cli`]: `tollgate monitor list|add|remove|refresh`.

pub(crate) mod alert;
pub(crate) mod cache;
pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod nous;
pub(crate) mod observe;
pub(crate) mod poll;
pub(crate) mod source;

pub(crate) use observe::monitor_observations;
pub(crate) use poll::poll_detached;
