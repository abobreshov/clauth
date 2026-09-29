//! Entrypoint resolution and the version read (spec §4.5, G15), plus the
//! compiled S7(f) gate (spec §8).
//!
//! Re-run at every user-initiated launch and never cached: `mise up`
//! reinstalls into a new version dir, and a cached path would go stale
//! (D-H4). The daemon, `list`, and every read-only surface never call this.
//!
//! The one hard rule: tollgate never executes the Omarchy shim
//! (`~/.local/bin/hermes`, a bash script that can install software). Every
//! candidate must be a Python entrypoint whose shebang names the interpreter
//! of its own venv. Anything else is rejected by reading its first line, never
//! by running it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

/// A command path that cannot spawn, for [`crate::harness::HermesEngine`]'s
/// `command()` when resolution fails. It must never fall back to a bare
/// `hermes` PATH lookup.
pub(crate) const UNRESOLVED_ENTRYPOINT: &str = "/nonexistent/tollgate-hermes-unresolved";

/// The series tollgate's guards were verified against (W-VERSION, D-H5).
pub(crate) const VERIFIED_SERIES: (u64, u64) = (0, 19);

/// How long the `mise where` fallback may run.
const MISE_WHERE_TIMEOUT: Duration = Duration::from_secs(5);

/// The spike doc, compiled in: its machine block is the S7(f) gate.
const S7F_DOC: &str = include_str!("../../docs/spikes/s7f-hermes-home.md");

/// Where the resolver looks. Built from the process environment in
/// production. Tests build it directly, so no test reads or sets a process
/// env var.
#[derive(Debug, Clone)]
pub(crate) struct ResolveEnv {
    /// `[settings] bin` from the roster.
    pub(crate) settings_bin: Option<PathBuf>,
    /// `$MISE_DATA_DIR`, default `~/.local/share/mise`.
    pub(crate) mise_data_dir: PathBuf,
    /// `$PIPX_HOME`, default `~/.local/pipx`.
    pub(crate) pipx_home: PathBuf,
    /// `$PATH`, for the `mise` fallback and the last-resort `which hermes`.
    pub(crate) path: Option<OsString>,
    /// `~/.tollgate`: the cwd of `mise where`, so no project `.mise.toml`
    /// loads its env or hooks (D-H19).
    pub(crate) tollgate_dir: PathBuf,
}

impl ResolveEnv {
    pub(crate) fn from_process(settings_bin: Option<PathBuf>) -> Self {
        let home = crate::profile::home_dir().unwrap_or_default();
        let env_dir = |key: &str, default: &str| {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
        };
        Self {
            settings_bin,
            mise_data_dir: env_dir("MISE_DATA_DIR", ".local/share/mise"),
            pipx_home: env_dir("PIPX_HOME", ".local/pipx"),
            path: std::env::var_os("PATH"),
            tollgate_dir: crate::profile::tollgate_dir().unwrap_or_default(),
        }
    }
}

/// A resolved Hermes install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Install {
    /// The pip-generated entrypoint script.
    pub(crate) entry: PathBuf,
    /// The interpreter its shebang names (runs the projector).
    pub(crate) python: PathBuf,
    /// `$HSP`: the single site-packages holding `hermes_cli/main.py`.
    pub(crate) hsp: PathBuf,
    /// `Version:` from the single `hermes_agent-*.dist-info/METADATA`.
    pub(crate) version: String,
}

/// Why resolution failed; each maps to one spec message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// M-BIN: nothing usable at any step; `tried` lists what was looked at.
    NotFound { tried: Vec<String> },
    /// M-SHIM: a candidate is a launcher script, not the Python entrypoint.
    Shim { path: PathBuf },
}

impl ResolveError {
    /// The one-line refusal for profile `name`.
    pub(crate) fn message(&self, name: &str) -> String {
        match self {
            ResolveError::NotFound { tried } => format!(
                "tollgate: hermes '{name}': cannot find the Hermes install (tried {}); install \
                 it, or set [settings] bin in ~/.tollgate/hermes-profiles.toml",
                tried.join(", ")
            ),
            ResolveError::Shim { path } => format!(
                "tollgate: hermes '{name}': {} is a launcher script, not the Hermes \
                 entrypoint; tollgate will not run it (it can install software)",
                path.display()
            ),
        }
    }
}

/// Resolve the entrypoint in the §4.5 order. The first candidate that exists
/// decides: it passes the checks, or the resolution refuses. A shim found
/// early never lets a later candidate through, because a shim on the path
/// means the operator's `hermes` is not what tollgate would run.
pub(crate) fn resolve_entrypoint(env: &ResolveEnv) -> Result<Install, ResolveError> {
    let mut tried = Vec::new();

    // 1. The override.
    if let Some(bin) = &env.settings_bin {
        tried.push(format!("[settings] bin {}", bin.display()));
        return check_candidate(bin).map_err(|e| e.unwrap_or(ResolveError::NotFound { tried }));
    }

    // 2. The mise install glob, read without running mise; `mise where` only
    //    when the glob matches nothing.
    let installs = env.mise_data_dir.join("installs").join("pipx-hermes-agent");
    tried.push(format!(
        "{}",
        installs.join("*/hermes-agent/bin/hermes").display()
    ));
    if let Some(entry) = highest_mise_install(&installs) {
        return check_candidate(&entry).map_err(|e| e.unwrap_or(ResolveError::NotFound { tried }));
    }
    if let Some(dir) = mise_where(env) {
        let entry = dir.join("hermes-agent").join("bin").join("hermes");
        tried.push(format!("mise where → {}", entry.display()));
        if entry.exists() {
            return check_candidate(&entry)
                .map_err(|e| e.unwrap_or(ResolveError::NotFound { tried }));
        }
    }

    // 3. pipx.
    let pipx = env
        .pipx_home
        .join("venvs")
        .join("hermes-agent")
        .join("bin")
        .join("hermes");
    tried.push(pipx.display().to_string());
    if pipx.exists() {
        return check_candidate(&pipx).map_err(|e| e.unwrap_or(ResolveError::NotFound { tried }));
    }

    // 4. PATH. The Omarchy shim lives here, and the shebang check turns it away.
    tried.push("hermes on PATH".to_string());
    if let Some(found) = which_on(env.path.as_deref(), "hermes") {
        return check_candidate(&found).map_err(|e| e.unwrap_or(ResolveError::NotFound { tried }));
    }
    Err(ResolveError::NotFound { tried })
}

/// `major.minor.patch`, digits only; anything else (`latest`, `0.19`, a
/// pre-release) is not an install dir the glob picks.
fn semver(raw: &str) -> Option<(u64, u64, u64)> {
    let mut it = raw.split('.');
    let v = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(v)
}

fn highest_mise_install(installs: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(installs).ok()?;
    entries
        .flatten()
        .filter_map(|e| {
            let v = semver(&e.file_name().to_string_lossy())?;
            let entry = e.path().join("hermes-agent").join("bin").join("hermes");
            entry.exists().then_some((v, entry))
        })
        .max_by_key(|(v, _)| *v)
        .map(|(_, entry)| entry)
}

/// `mise where pipx:hermes-agent`, from `~/.tollgate`, stdin null, a 5 s
/// timeout, stdout's first line. `None` on any failure.
fn mise_where(env: &ResolveEnv) -> Option<PathBuf> {
    let mise = which_on(env.path.as_deref(), "mise")?;
    let mut command = std::process::Command::new(mise);
    command
        .arg("where")
        .arg("pipx:hermes-agent")
        .current_dir(&env.tollgate_dir);
    if let Some(path) = &env.path {
        command.env("PATH", path);
    }
    let out = super::run_bounded(command, MISE_WHERE_TIMEOUT, "mise where").ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim();
    (!first.is_empty()).then(|| PathBuf::from(first))
}

/// The first `name` on `path` that is a file.
pub(crate) fn which_on(path: Option<&std::ffi::OsStr>, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// The candidate checks: a regular file (or a link to one) whose first line
/// is `#!<venv>/bin/python…` for its OWN venv, then `$HSP` and the version.
/// `Err(Some(_))` is a definite refusal (a shim, a broken venv); `Err(None)`
/// means "not a usable file", which the caller reports as not found.
fn check_candidate(entry: &Path) -> Result<Install, Option<ResolveError>> {
    let meta = std::fs::metadata(entry).map_err(|_| None)?;
    if !meta.is_file() {
        return Err(None);
    }
    let shim = || {
        Some(ResolveError::Shim {
            path: entry.to_path_buf(),
        })
    };
    let first = first_line(entry).ok_or_else(shim)?;
    let python = first
        .strip_prefix("#!")
        .map(str::trim)
        .map(PathBuf::from)
        .ok_or_else(shim)?;
    let is_python = python.is_absolute()
        && python.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("bin"))
        && python
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("python"));
    if !is_python {
        return Err(shim());
    }
    // "In the same venv": the interpreter's venv is the entrypoint's venv,
    // compared canonically (a PATH link to the script, or a mise `latest`
    // dir, resolves to the venv its shebang was written for).
    let real_entry = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.to_path_buf());
    let entry_venv = real_entry
        .parent()
        .and_then(Path::parent)
        .ok_or_else(shim)?;
    let python_venv = python.parent().and_then(Path::parent).ok_or_else(shim)?;
    let same = match (
        std::fs::canonicalize(entry_venv),
        std::fs::canonicalize(python_venv),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => entry_venv == python_venv,
    };
    if !same {
        return Err(shim());
    }
    let venv = python_venv.to_path_buf();
    let not_found = || {
        Some(ResolveError::NotFound {
            tried: vec![format!(
                "{} (no single hermes_cli site-packages)",
                venv.display()
            )],
        })
    };
    let hsp = single(
        glob_dirs(&venv.join("lib"), "python3.")
            .into_iter()
            .map(|lib| lib.join("site-packages"))
            .filter(|sp| sp.join("hermes_cli").join("main.py").is_file()),
    )
    .ok_or_else(not_found)?;
    let metadata = single(
        glob_dirs(&hsp, "hermes_agent-")
            .into_iter()
            .filter(|d| {
                d.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".dist-info"))
            })
            .map(|d| d.join("METADATA"))
            .filter(|m| m.is_file()),
    )
    .ok_or_else(not_found)?;
    let version = read_version(&metadata).ok_or_else(not_found)?;
    Ok(Install {
        entry: entry.to_path_buf(),
        python,
        hsp,
        version,
    })
}

fn first_line(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut buf = [0u8; 512];
    let mut file = std::fs::File::open(path).ok()?;
    let n = file.read(&mut buf).ok()?;
    let head = &buf[..n];
    let end = head.iter().position(|&b| b == b'\n')?;
    String::from_utf8(head[..end].to_vec()).ok()
}

fn glob_dirs(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

fn single<T>(mut it: impl Iterator<Item = T>) -> Option<T> {
    let first = it.next()?;
    it.next().is_none().then_some(first)
}

fn read_version(metadata: &Path) -> Option<String> {
    let text = std::fs::read_to_string(metadata).ok()?;
    text.lines()
        .take_while(|l| !l.is_empty())
        .find_map(|l| l.strip_prefix("Version:"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `(major, minor)` of a version string, if it has them.
fn series(version: &str) -> Option<(u64, u64)> {
    let mut it = version.split('.');
    let major = it.next()?.parse().ok()?;
    let minor_raw = it.next()?;
    let digits: String = minor_raw.chars().take_while(char::is_ascii_digit).collect();
    Some((major, digits.parse().ok()?))
}

/// Whether `version` is in the series the guards were verified against.
pub(crate) fn in_verified_series(version: &str) -> bool {
    series(version) == Some(VERIFIED_SERIES)
}

/// The G15 verdict for an installed version under a policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VersionVerdict {
    Ok,
    /// W-VERSION, printed to stderr; the launch proceeds.
    Warn(String),
    Refuse(String),
}

pub(crate) fn version_verdict(
    name: &str,
    version: &str,
    policy: super::profiles::VersionPolicy,
) -> VersionVerdict {
    if in_verified_series(version) {
        return VersionVerdict::Ok;
    }
    match policy {
        super::profiles::VersionPolicy::Warn => VersionVerdict::Warn(format!(
            "tollgate: note — Hermes {version} is installed; tollgate's guards were verified \
             against 0.19.x"
        )),
        super::profiles::VersionPolicy::Refuse => VersionVerdict::Refuse(format!(
            "tollgate: hermes '{name}': Hermes {version} is installed and version_policy is \
             \"refuse\"; tollgate's guards were verified against 0.19.x"
        )),
    }
}

/// The S7(f) machine block, parsed out of a spike doc.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct S7fGate {
    pub(crate) result: String,
    pub(crate) hermes: Vec<String>,
    pub(crate) commit: String,
}

/// The fenced ```toml block whose first line is `# s7f-gate`.
pub(crate) fn parse_s7f_gate(doc: &str) -> Option<S7fGate> {
    let start = doc.find("```toml\n# s7f-gate\n")? + "```toml\n".len();
    let body = &doc[start..];
    let end = body.find("\n```")?;
    toml::from_str(&body[..end]).ok()
}

/// Whether the compiled S7(f) block passes `version`: `result = "pass"` and a
/// listed version in the same `major.minor` series (the same 0.19.x band the
/// version gate uses; the spike is re-run when the series moves).
pub(crate) fn s7f_gate_passes(doc: &str, version: &str) -> bool {
    let Some(gate) = parse_s7f_gate(doc) else {
        return false;
    };
    gate.result == "pass"
        && series(version).is_some_and(|s| gate.hermes.iter().any(|v| series(v) == Some(s)))
}

/// The start refusal while S7(f) has not passed for the installed version.
pub(crate) fn s7f_gate_refusal(name: &str, version: &str) -> Option<String> {
    (!s7f_gate_passes(S7F_DOC, version)).then(|| {
        format!(
            "tollgate: hermes '{name}': the S7(f) HOME-redirect spike has not passed for \
             Hermes {version}"
        )
    })
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_resolve.rs"]
mod tests;
