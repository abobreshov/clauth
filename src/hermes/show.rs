//! The Hermes read surfaces (spec §2.1, §4.7, part 2): `hermes list` with the
//! month-to-date estimate, `hermes show` with the pool view and the latest
//! sessions, `show --check` with every guard's verdict, and the pool strategy
//! writer.
//!
//! Every value these print comes from a whitelist: the roster, the usage
//! cache, [`super::pool::PoolAuthView`] (which has no field that could hold a
//! secret) and `sqlite3 -readonly` reads of `state.db`. `list` and `show`
//! without `--check` never run Hermes, its interpreter or `mise`: they read
//! files and run `sqlite3` only. `--check` runs the same entrypoint
//! resolution and projector a launch runs, with no lock held.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Serialize;

use super::guards::{self, refuse};
use super::home::{self, HermesPaths};
use super::pool::{PoolAuthView, PoolEntryView};
use super::profiles::{Auth, HermesProfile, HermesState, Mode};
use super::{ROTATION_WAIT, resolve};
use crate::out::{errln, out, outln};
use crate::profile::ProfileName;
use crate::runtime::RotationGuard;
use crate::usage::hermes_local::{self, HermesUsageCache};

/// How many sessions `show` lists (H-4: the ids feed `-- --resume <id>`).
const RECENT_SESSIONS: usize = 5;
const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long each `config set` of the strategy writer may take.
const CONFIG_SET_TIMEOUT: Duration = Duration::from_secs(10);
/// A `state.db-wal` touched this recently with no tollgate marker held means
/// a Hermes tollgate did not start is running on the home (§5).
const WAL_ACTIVE: Duration = Duration::from_secs(5);

// ── the estimate ─────────────────────────────────────────────────────────────

/// The month-to-date estimate of a cache, `None` for another month's cache or
/// one with no successful read.
fn month_estimate(cache: &HermesUsageCache, now_secs: i64) -> Option<String> {
    (cache.read_at_ms > 0
        && cache.period_start == hermes_local::rfc3339_z(hermes_local::month_start_secs(now_secs)))
    .then(|| cache.total_cost().to_string())
}

/// The estimate as a short money string (`$0.01`).
fn money(amount: &str) -> String {
    crate::usage::observation::Amount::parse(amount).map_or_else(
        || format!("${amount}"),
        |a| crate::usage::derive::format_money(&a, "USD"),
    )
}

fn is_live(name: &str) -> bool {
    crate::runtime::has_live_session(&ProfileName::from(name))
}

// ── `hermes list` ────────────────────────────────────────────────────────────

/// One `hermes list` row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ListRow {
    pub(crate) name: String,
    pub(crate) provider: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) auth: &'static str,
    pub(crate) model: Option<String>,
    pub(crate) live: bool,
    /// Month-to-date USD, a decimal string.
    pub(crate) estimate_usd: Option<String>,
    /// Why the usage cache has no figures (`sqlite3_missing`, …).
    pub(crate) usage_error: Option<String>,
}

/// The rows, after refreshing each profile's usage cache (`sqlite3` only).
pub(crate) fn list_rows(path: Option<&OsStr>) -> Result<Vec<ListRow>> {
    let state = HermesState::load()?;
    let now_ms = crate::usage::now_ms();
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    Ok(state
        .profiles()
        .iter()
        .map(|p| {
            let cache = hermes_local::refresh_with(&p.name, now_ms, path, None)
                .ok()
                .or_else(|| hermes_local::load(&p.name));
            ListRow {
                name: p.name.clone(),
                provider: p.provider.as_str(),
                mode: p.mode.as_str(),
                auth: p.auth.as_str(),
                model: p.model.clone(),
                live: is_live(&p.name),
                estimate_usd: cache.as_ref().and_then(|c| month_estimate(c, now_secs)),
                usage_error: cache
                    .as_ref()
                    .and_then(|c| c.error)
                    .map(|e| serde_json::to_value(e).ok())
                    .and_then(|v| v.and_then(|v| v.as_str().map(str::to_string))),
            }
        })
        .collect())
}

/// `hermes list [--json]`.
pub(crate) fn list(json: bool) -> Result<()> {
    let path = std::env::var_os("PATH");
    let rows = list_rows(path.as_deref())?;
    if json {
        outln!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    out!("{}", render_list(&rows));
    Ok(())
}

/// The plain `hermes list` table.
pub(crate) fn render_list(rows: &[ListRow]) -> String {
    if rows.is_empty() {
        return "no Hermes profiles; create one with 'tollgate hermes new <name>'\n".to_string();
    }
    let mut out = String::new();
    for r in rows {
        let estimate = match (&r.estimate_usd, &r.usage_error) {
            (Some(a), _) => format!("  {} this month", money(a)),
            (None, Some(e)) => format!("  usage: {e}"),
            (None, None) => String::new(),
        };
        out.push_str(&format!(
            "{}{}  {}  {} home  auth {}{}{}\n",
            if r.live { "● " } else { "  " },
            r.name,
            r.provider,
            r.mode,
            r.auth,
            r.model
                .as_deref()
                .map(|m| format!("  model {m}"))
                .unwrap_or_default(),
            estimate
        ));
    }
    out
}

// ── the pool view (§4.7) ─────────────────────────────────────────────────────

/// One pool entry as tollgate renders it: whitelisted fields only, and the
/// fingerprint cut to its last four hex digits.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PoolEntryOut {
    pub(crate) index: usize,
    pub(crate) id: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) auth_type: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) reset_at: Option<String>,
    pub(crate) request_count: Option<u64>,
    pub(crate) priority: Option<String>,
    pub(crate) fingerprint_tail: Option<String>,
    /// The `env:<key_env>` entry whose fingerprint is the roster's: the key
    /// tollgate bound in the home `.env`.
    pub(crate) tollgate_key: bool,
}

/// The pool view of one provider.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PoolOut {
    pub(crate) provider: String,
    /// `auth.json` is a version this binary was not written against.
    pub(crate) best_effort: bool,
    pub(crate) active_provider: Option<String>,
    pub(crate) entries: Vec<PoolEntryOut>,
}

/// A scalar `auth.json` value as display text: a string as is, an epoch
/// number as an RFC 3339 stamp, anything else dropped.
fn scalar(v: Option<&serde_json::Value>, epoch: bool) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) if epoch => n
            .as_f64()
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| crate::usage::epoch_secs_to_iso(f as i64)),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The last four hex digits of a `sha256:<hex>` fingerprint.
fn fingerprint_tail(fp: &str) -> Option<String> {
    let hex = fp.strip_prefix("sha256:").unwrap_or(fp);
    let tail: String = hex
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    (tail.len() == 4 && tail.chars().all(|c| c.is_ascii_hexdigit())).then_some(tail)
}

fn entry_out(index: usize, e: &PoolEntryView, profile: &HermesProfile) -> PoolEntryOut {
    let tollgate_key = profile.auth == Auth::Env
        && profile
            .key_env
            .as_deref()
            .is_some_and(|var| e.source.as_deref() == Some(format!("env:{var}").as_str()))
        && e.secret_fingerprint.is_some()
        && e.secret_fingerprint == profile.key_fingerprint;
    PoolEntryOut {
        index,
        id: e.id.clone(),
        label: e.label.clone(),
        auth_type: e.auth_type.clone(),
        source: e.source.clone(),
        status: e.last_status.clone(),
        reset_at: scalar(e.last_error_reset_at.as_ref(), true),
        request_count: e.request_count,
        priority: scalar(e.priority.as_ref(), false),
        fingerprint_tail: e.secret_fingerprint.as_deref().and_then(fingerprint_tail),
        tollgate_key,
    }
}

/// The pool view of `profile`'s provider in `view`.
pub(crate) fn pool_out(view: &PoolAuthView, profile: &HermesProfile) -> PoolOut {
    PoolOut {
        provider: profile.provider.as_str().to_string(),
        best_effort: !view.version_known(),
        active_provider: view.active_provider.clone(),
        entries: view
            .entries(profile.provider.as_str())
            .iter()
            .enumerate()
            .map(|(i, e)| entry_out(i + 1, e, profile))
            .collect(),
    }
}

/// `#<n> <label>  <auth_type>/<source>  <status>[ until <reset>]  req <count>
/// prio <p>  fp …<last 4 hex>`, one line per entry.
pub(crate) fn pool_lines(pool: &PoolOut) -> Vec<String> {
    let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
    pool.entries
        .iter()
        .map(|e| {
            let mut line = format!(
                "#{} {}  {}/{}  {}",
                e.index,
                dash(&e.label),
                dash(&e.auth_type),
                dash(&e.source),
                dash(&e.status)
            );
            if let Some(reset) = &e.reset_at {
                line.push_str(&format!(" until {reset}"));
            }
            line.push_str(&format!(
                "  req {}  prio {}",
                e.request_count
                    .map_or_else(|| "-".to_string(), |n| n.to_string()),
                dash(&e.priority)
            ));
            if let Some(tail) = &e.fingerprint_tail {
                line.push_str(&format!("  fp …{tail}"));
            }
            if e.tollgate_key {
                line.push_str("  (the key tollgate bound)");
            }
            line
        })
        .collect()
}

// ── the latest sessions ──────────────────────────────────────────────────────

/// One `sessions` row of `state.db`.
#[derive(Debug, Clone, Serialize, PartialEq, serde::Deserialize)]
pub(crate) struct SessionSummary {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) started_at: Option<f64>,
    #[serde(default)]
    pub(crate) billing_provider: Option<String>,
}

/// The latest sessions of `<home>/state.db`, newest first, read with
/// `sqlite3 -readonly -json`. Empty when there is no `sqlite3`, no db, or an
/// unknown schema.
pub(crate) fn recent_sessions(home: &Path, path: Option<&OsStr>) -> Vec<SessionSummary> {
    let db = home.join("state.db");
    if !db.symlink_metadata().is_ok_and(|m| m.is_file()) {
        return Vec::new();
    }
    let Some(sqlite) = resolve::which_on(path, "sqlite3") else {
        return Vec::new();
    };
    let mut command = Command::new(sqlite);
    command
        .arg("-readonly")
        .arg("-json")
        .arg("-cmd")
        .arg(".timeout 2000")
        .arg(&db)
        .arg(format!(
            "SELECT id, title, started_at, billing_provider FROM sessions \
             ORDER BY started_at DESC LIMIT {RECENT_SESSIONS}"
        ));
    let Ok(out) = super::run_bounded(command, SQLITE_TIMEOUT, "sqlite3") else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str(text.trim()).unwrap_or_default()
}

// ── `show --check` ───────────────────────────────────────────────────────────

/// One guard's verdict.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub(crate) enum Verdict {
    Pass,
    /// Passes, with a warning or a note.
    Note {
        text: String,
    },
    Refused {
        text: String,
    },
}

/// `(guard, verdict)` in guard order.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct CheckLine {
    pub(crate) guard: &'static str,
    #[serde(flatten)]
    pub(crate) verdict: Verdict,
}

fn line(guard: &'static str, r: Result<Option<String>>) -> CheckLine {
    CheckLine {
        guard,
        verdict: match r {
            Ok(None) => Verdict::Pass,
            Ok(Some(text)) => Verdict::Note { text },
            Err(e) => Verdict::Refused {
                text: format!("{e:#}"),
            },
        },
    }
}

/// The full launch audit as a read-only report (spec §2.1 `show --check`):
/// every guard in launch order, stopping at the first refusal. It runs the
/// entrypoint resolution and the projector, with no lock held, and writes
/// nothing: a missing child home or a moved key fingerprint is reported, not
/// repaired.
pub(crate) fn check_report(name: &str, profile: &HermesProfile) -> Result<Vec<CheckLine>> {
    let paths = HermesPaths::for_name(name)?;
    let operator = crate::profile::home_dir()?;
    let mut lines: Vec<CheckLine> = Vec::new();
    macro_rules! step {
        ($guard:expr, $body:expr) => {{
            let l = line($guard, $body);
            let stop = matches!(l.verdict, Verdict::Refused { .. });
            lines.push(l);
            if stop {
                return Ok(lines);
            }
        }};
    }
    step!("G1 shape", guards::g1_shape(name, &paths).map(|()| None));
    step!(
        "G2 containment",
        guards::g2_containment(name, &paths, &operator).map(|()| None)
    );
    step!(
        "G2a child home",
        match home::audit_child_home(&paths.child_home) {
            Ok(home::ChildHomeVerdict::Ok) => Ok(None),
            Ok(home::ChildHomeVerdict::Missing) =>
                Ok(Some("missing; the next start creates it".to_string())),
            Ok(home::ChildHomeVerdict::Foreign(entry)) =>
                Err(super::m_child_home(name, &paths, &entry)),
            Err(e) => Err(e),
        }
    );
    step!(
        "G3 active_profile",
        guards::g3_active_profile(name, &paths).map(|()| None)
    );
    step!(
        "G4 profiles",
        guards::g4_profiles(name, &paths).map(|()| None)
    );
    let install = match super::resolve_install(name) {
        Ok(install) => install,
        Err(e) => {
            step!("entrypoint", Err(e));
            unreachable!("a refused step returns");
        }
    };
    step!(
        "entrypoint",
        Ok(Some(format!(
            "{} (Hermes {})",
            install.entry.display(),
            install.version
        )))
    );
    step!(
        "G6 site .env",
        guards::g6_hsp_env(name, &install.hsp).map(|()| None)
    );
    step!(
        "G14 liveness",
        guards::g14_liveness(name, &paths).map(|()| wal_note(&paths.home))
    );
    let policy = HermesState::load()?
        .settings()
        .version_policy
        .unwrap_or_default();
    step!(
        "G15 version",
        match resolve::version_verdict(name, &install.version, policy) {
            resolve::VersionVerdict::Ok => Ok(None),
            resolve::VersionVerdict::Warn(w) => Ok(Some(w)),
            resolve::VersionVerdict::Refuse(r) => Err(refuse(r)),
        }
    );
    step!(
        "S7(f) gate",
        match resolve::s7f_gate_refusal(name, &install.version) {
            None => Ok(None),
            Some(r) => Err(refuse(r)),
        }
    );
    let managed_dir = guards::managed_dir();
    let dynamic = guards::plugin_env_vars(&guards::plugin_roots(&install.hsp, &paths.home));
    let command = super::child_command(
        &install.python,
        &paths,
        &dynamic,
        &super::active_claude_env_keys(),
    );
    let projection =
        match super::projector::run_projector(command, &paths.home, managed_dir.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                step!(
                    "P projector",
                    Err(refuse(format!(
                        "tollgate: hermes '{name}': cannot audit {}/config.yaml ({e:#})",
                        paths.home.display()
                    )))
                );
                unreachable!("a refused step returns");
            }
        };
    step!("P projector", Ok(None));
    step!(
        "G7–G10a config",
        guards::audit_projection(
            name,
            &paths.home,
            profile,
            &projection,
            managed_dir.as_deref(),
            &dynamic,
            &install.entry,
        )
        .map(|notes| (!notes.warnings.is_empty()).then(|| notes.warnings.join("; ")))
    );
    step!(
        "G11 auth.json",
        guards::g11_read_and_check(name, &paths.home).map(|_| None)
    );
    step!("G12 env", guards::g12_env(name, &projection).map(|()| None));
    step!(
        "G13 binding",
        g13_report(name, profile, &paths, &projection)
    );
    Ok(lines)
}

/// G13's env half as a report: the key must be there; a moved fingerprint is
/// a note (the next launch re-attributes it).
fn g13_report(
    name: &str,
    profile: &HermesProfile,
    paths: &HermesPaths,
    projection: &super::projector::ProjectionV1,
) -> Result<Option<String>> {
    if profile.auth != Auth::Env {
        return Ok(None);
    }
    let var = profile
        .key_env
        .clone()
        .unwrap_or_else(|| profile.provider.key_env().to_string());
    let keys = guards::home_env_key_set(projection);
    let audit = super::env_file::audit_key(paths, name, &var, Some(&keys))
        .map_err(|e| guards::as_refusal(name, e))?;
    Ok(
        (profile.key_fingerprint.as_deref() != Some(audit.fingerprint.as_str()))
            .then(|| format!("{var} changed outside tollgate; the next start re-attributes it")),
    )
}

/// §5: a `state.db-wal` written in the last few seconds with no tollgate
/// marker held is a Hermes tollgate did not start.
fn wal_note(home: &Path) -> Option<String> {
    let age = home
        .join("state.db-wal")
        .metadata()
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    (age < WAL_ACTIVE).then(|| {
        "state.db-wal was written seconds ago and no tollgate session holds the home: a Hermes \
         tollgate did not start may be running on it"
            .to_string()
    })
}

// ── `hermes show` ────────────────────────────────────────────────────────────

/// Everything `show` prints, as one value (the `--json` form).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShowOut {
    pub(crate) name: String,
    pub(crate) provider: &'static str,
    pub(crate) model: Option<String>,
    pub(crate) mode: &'static str,
    pub(crate) auth: &'static str,
    pub(crate) key_env: Option<String>,
    pub(crate) created_at: String,
    pub(crate) home: String,
    pub(crate) child_home: String,
    pub(crate) live: bool,
    pub(crate) estimate_usd: Option<String>,
    pub(crate) usage_error: Option<String>,
    /// `null` when the home has no `auth.json`, or it cannot be read.
    pub(crate) pool: Option<PoolOut>,
    pub(crate) pool_error: Option<String>,
    pub(crate) sessions: Vec<SessionSummary>,
    /// Only with `--check`.
    pub(crate) check: Option<Vec<CheckLine>>,
}

/// Build `show`'s value. `check` runs the audit; `path` finds `sqlite3`.
pub(crate) fn show_out(name: &str, check: bool, path: Option<&OsStr>) -> Result<ShowOut> {
    let profile = super::find_profile(name)?;
    let paths = HermesPaths::for_name(name)?;
    let now_ms = crate::usage::now_ms();
    let now_secs = i64::try_from(now_ms / 1000).unwrap_or(i64::MAX);
    let cache = hermes_local::refresh_with(name, now_ms, path, None)
        .ok()
        .or_else(|| hermes_local::load(name));
    let (pool, pool_error) = match super::pool::read_auth_view(&paths.home) {
        Ok(Some(view)) => (Some(pool_out(&view, &profile)), None),
        Ok(None) => (None, None),
        Err(_) => (
            None,
            Some("auth.json cannot be read now; retry when Hermes is not writing it".to_string()),
        ),
    };
    Ok(ShowOut {
        name: profile.name.clone(),
        provider: profile.provider.as_str(),
        model: profile.model.clone(),
        mode: profile.mode.as_str(),
        auth: profile.auth.as_str(),
        key_env: profile.key_env.clone(),
        created_at: profile.created_at.clone(),
        home: paths.home.display().to_string(),
        child_home: paths.child_home.display().to_string(),
        live: is_live(name),
        estimate_usd: cache.as_ref().and_then(|c| month_estimate(c, now_secs)),
        usage_error: cache
            .as_ref()
            .and_then(|c| c.error)
            .map(|e| e.message().to_string()),
        pool,
        pool_error,
        sessions: recent_sessions(&paths.home, path),
        check: if check {
            Some(check_report(name, &profile)?)
        } else {
            None
        },
    })
}

/// The plain `show` text.
pub(crate) fn render_show(s: &ShowOut) -> String {
    let mut out = String::new();
    let mut push = |l: String| {
        out.push_str(&l);
        out.push('\n');
    };
    push(format!(
        "{}{}  {} · {} home · auth {}",
        if s.live { "● " } else { "" },
        s.name,
        s.provider,
        s.mode,
        s.auth
    ));
    if let Some(m) = &s.model {
        push(format!("  model       {m}"));
    }
    if let Some(k) = &s.key_env {
        push(format!("  key         {k} in the home .env"));
    }
    push(format!("  home        {}", s.home));
    push(format!("  child HOME  {}", s.child_home));
    match (&s.estimate_usd, &s.usage_error) {
        (Some(a), _) => push(format!(
            "  this month  {} (hermes state.db: billed where known, else estimated)",
            money(a)
        )),
        (None, Some(e)) => push(format!("  this month  - ({e})")),
        (None, None) => push("  this month  -".to_string()),
    }
    match (&s.pool, &s.pool_error) {
        (Some(pool), _) => {
            push(format!(
                "  pool        {}{}",
                pool.provider,
                if pool.best_effort {
                    " (auth.json version unknown; best effort)"
                } else {
                    ""
                }
            ));
            if pool.entries.is_empty() {
                push("    (no entries)".to_string());
            }
            for l in pool_lines(pool) {
                push(format!("    {l}"));
            }
        }
        (None, Some(e)) => push(format!("  pool        {e}")),
        (None, None) => push("  pool        (no auth.json yet)".to_string()),
    }
    if !s.sessions.is_empty() {
        push("  sessions (resume with 'tollgate start <name> -- --resume <id>'):".to_string());
        for x in &s.sessions {
            push(format!(
                "    {}  {}{}",
                x.id,
                x.started_at.filter(|f| f.is_finite()).map_or_else(
                    || "-".to_string(),
                    |f| crate::usage::epoch_secs_to_iso(f as i64)
                ),
                x.title
                    .as_deref()
                    .map(|t| format!(
                        "  {}",
                        t.chars().filter(|c| !c.is_control()).collect::<String>()
                    ))
                    .unwrap_or_default()
            ));
        }
    }
    if let Some(check) = &s.check {
        push("  check:".to_string());
        for c in check {
            match &c.verdict {
                Verdict::Pass => push(format!("    {:<18} ok", c.guard)),
                Verdict::Note { text } => push(format!("    {:<18} ok — {text}", c.guard)),
                Verdict::Refused { text } => push(format!("    {:<18} REFUSED — {text}", c.guard)),
            }
        }
    }
    out
}

/// Whether a `--check` report refused.
pub(crate) fn check_refused(s: &ShowOut) -> bool {
    s.check.as_ref().is_some_and(|c| {
        c.iter()
            .any(|l| matches!(l.verdict, Verdict::Refused { .. }))
    })
}

/// `hermes show <name> [--json] [--check]`: exit 1 when `--check` refused.
pub(crate) fn show(name: &str, json: bool, check: bool) -> Result<i32> {
    super::refuse_on_windows()?;
    let path = std::env::var_os("PATH");
    let s = show_out(name, check, path.as_deref())?;
    if json {
        outln!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        out!("{}", render_show(&s));
    }
    Ok(if check_refused(&s) { 1 } else { 0 })
}

// ── `hermes pool <name> strategy <s>` ────────────────────────────────────────

/// The pool strategies Hermes knows (`agent/credential_pool.py`).
pub(crate) const STRATEGIES: &[&str] = &["fill_first", "round_robin", "random", "least_used"];

/// The strategy writer (§4.7): the entrypoint resolved lock-free; then the
/// RotationGuard with G1–G4 and G14; a marker claimed with no row; both locks
/// released; `<hermes> config set credential_pool_strategies.<provider> <s>`
/// with the child env; the marker dropped. tollgate never writes
/// `config.yaml` or `auth.json` itself. Returns the child's code.
pub(crate) fn pool_strategy(name: &str, strategy: &str) -> Result<i32> {
    super::refuse_on_windows()?;
    if !STRATEGIES.contains(&strategy) {
        bail!(
            "unknown pool strategy '{strategy}'; expected one of {}",
            STRATEGIES.join(", ")
        );
    }
    let profile = super::find_profile(name)?;
    if profile.mode != Mode::Pool {
        return Err(refuse(format!(
            "tollgate: hermes '{name}': this is an account home (one credential); a pool \
             strategy applies to a pool home"
        )));
    }
    let paths = HermesPaths::for_name(name)?;
    let install = super::resolve_install(name)?;
    let dynamic = guards::plugin_env_vars(&guards::plugin_roots(&install.hsp, &paths.home));
    let active = super::active_claude_env_keys();

    let rotation = RotationGuard::acquire_with_timeout(&ProfileName::from(name), ROTATION_WAIT)?;
    super::shape_guards(name, &paths)?;
    guards::g14_liveness(name, &paths)?;
    let marker = crate::runtime::HermesMarker::claim(name, false, &rotation, || {
        guards::m_busy(name, "another tollgate command holds it")
    })?;
    drop(rotation);

    let mut command = super::child_command(&install.entry, &paths, &dynamic, &active);
    command
        .arg("config")
        .arg("set")
        .arg(format!(
            "credential_pool_strategies.{}",
            profile.provider.as_str()
        ))
        .arg(strategy);
    let result = super::run_bounded(command, CONFIG_SET_TIMEOUT, "hermes config set (strategy)");
    drop(marker);
    let out = result?;
    let code = super::exit_code_of(out.status);
    if code == 0 {
        outln!(
            "tollgate: Hermes profile '{name}' now draws its {} pool {strategy}",
            profile.provider
        );
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        errln!(
            "tollgate: hermes '{name}': '{} config set' exited {code}{}",
            install.entry.display(),
            if last.is_empty() {
                String::new()
            } else {
                format!(": {}", crate::usage::observation::sanitize_message(last))
            }
        );
    }
    Ok(code)
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_show.rs"]
mod tests;
