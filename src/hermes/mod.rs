//! Hermes Agent as a third harness (spec `docs/specs/hermes-harness.md`).
//!
//! A Hermes profile is a whole Hermes home, `~/.tollgate/profiles/<name>/
//! hermes-home`, started with `HERMES_HOME` set to it and with `HOME` set to
//! the profile's child home ([`home`]). The redirect is what keeps the
//! Hermes tollgate launches away from `~/.hermes`, `~/.claude`, `~/.clauth`
//! and `~/.codex`: Hermes resolves `Path.home()` from `HOME`, so its default
//! root, its Claude Code credential reader and its `gh` pool source all land
//! in a directory holding three allowlisted links and nothing else. Spike
//! S7(f) verified that against the real 0.19.0
//! (`docs/spikes/s7f-hermes-home.md`), and [`resolve`] compiles its verdict
//! in as the start gate.
//!
//! tollgate never owns a Hermes credential. Hermes is the only writer of
//! `auth.json` and `config.yaml`. tollgate writes one line of the home
//! `.env` (the bound key, [`env_file`]) and runs Hermes' own
//! `config set` / `auth` for everything else.
//!
//! Lock discipline (spec §4): every verb that writes takes the profile's
//! RotationGuard (rank 100) first and the state lock (500) inside it, never
//! the other way round. It never holds either across a prompt or a child
//! process: the key prompt, the projector, `mise where`, `config set` and
//! the session itself all run with neither held. Each of those passes through
//! [`unlocked_point`], which asserts that in debug builds and records it for
//! the tests.

pub(crate) mod env_file;
pub(crate) mod guards;
pub(crate) mod home;
pub(crate) mod pool;
pub(crate) mod profiles;
pub(crate) mod projector;
pub(crate) mod resolve;
pub(crate) mod show;

#[cfg(test)]
#[path = "../../tests/inline/hermes_testkit.rs"]
pub(crate) mod testkit;

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::out::{errln, outln};
use crate::profile::ProfileName;
use crate::runtime::RotationGuard;

use guards::refuse;
use home::HermesPaths;
use profiles::{Auth, HermesProfile, HermesState, Mode, Provider};
use projector::ProjectionV1;
use resolve::Install;

/// How long a Hermes verb waits for the profile's RotationGuard (spec §4).
pub(crate) const ROTATION_WAIT: Duration = Duration::from_secs(25);

/// How long each `config set` of the auxiliary pinning may take (§4.1 step 7).
const CONFIG_SET_TIMEOUT: Duration = Duration::from_secs(10);

/// M-SWITCH (spec §2.1): the one text every surface gives a Hermes name
/// asked to switch — the bare-name CLI switch (a usage error), the TUI's
/// Hermes rows and the MCP `switch_profile` refusal. Hermes switches by
/// relaunch.
pub(crate) fn m_switch(name: &str) -> String {
    format!("'{name}' is a Hermes profile; Hermes switches by relaunch: 'tollgate start {name}'")
}

/// Every Hermes verb refuses on Windows (spec §1).
pub(crate) fn refuse_on_windows() -> Result<()> {
    if cfg!(windows) {
        bail!("Hermes profiles are supported on Linux and macOS only");
    }
    Ok(())
}

// ── the unlocked-spawn discipline ────────────────────────────────────────────

#[cfg(test)]
thread_local! {
    /// Every [`unlocked_point`] this thread passed: `(what, a tollgate rank
    /// was held)`. The test half of the rule; debug builds also assert it.
    pub(crate) static UNLOCKED_POINTS: std::cell::RefCell<Vec<(String, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Mark a point that must run with neither the RotationGuard nor the state
/// lock held: a prompt, or a child process about to spawn.
pub(crate) fn unlocked_point(what: &str) {
    let held = crate::lockorder::holds::<crate::lockorder::rank::Rotation>()
        || crate::lockorder::holds::<crate::lockorder::rank::State>();
    // `holds` is always true in release, where the rank stack is compiled out.
    #[cfg(debug_assertions)]
    debug_assert!(!held, "{what} ran under a tollgate lock");
    #[cfg(test)]
    UNLOCKED_POINTS.with(|p| p.borrow_mut().push((what.to_string(), held)));
    let _ = (what, held);
}

/// Run `command` with stdin null and stdout/stderr captured, killing it after
/// `timeout`. The one runner for every short-lived child Hermes needs (the
/// projector, `mise where`, `config set`).
pub(crate) fn run_bounded(
    mut command: Command,
    timeout: Duration,
    what: &str,
) -> Result<std::process::Output> {
    unlocked_point(what);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run {what}"))?;
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_t = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let err_t = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{what} timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

// ── the child env ────────────────────────────────────────────────────────────

/// The active CLAUDE profile's custom env keys, scrubbed from every Hermes
/// child like any spawn's: they describe a claude account.
pub(crate) fn active_claude_env_keys() -> Vec<String> {
    let Ok(config) = crate::profile::load_config() else {
        return Vec::new();
    };
    config
        .state
        .active_profile
        .as_deref()
        .map(ProfileName::from)
        .and_then(|n| config.find(&n))
        .map(|p| p.env.keys().cloned().collect())
        .unwrap_or_default()
}

/// A command for `program` with the child env of §4.4 step 4: the static
/// `SCRUB` and the active profile's keys through the engine, the plugin scan,
/// then `HERMES_HOME`, `HERMES_SHARED_AUTH_DIR` and `HOME` = the child home.
/// `HERMES_MANAGED_DIR` passes through untouched.
pub(crate) fn child_command(
    program: &Path,
    paths: &HermesPaths,
    dynamic_scrub: &BTreeSet<String>,
    active_env_keys: &[String],
) -> Command {
    use crate::harness::HarnessEngine as _;
    let engine = crate::harness::HermesEngine;
    let mut command = Command::new(program);
    engine.scrub_env(&mut command, active_env_keys);
    for key in dynamic_scrub {
        command.env_remove(key);
    }
    command
        .env(engine.home_env_key(), &paths.home)
        .env("HERMES_SHARED_AUTH_DIR", &paths.shared)
        .env("HOME", &paths.child_home);
    command
}

// ── preflight and the in-guard audit ─────────────────────────────────────────

/// `(ino, len, mtime_ns)` of one audited file; `None` when absent.
pub(crate) type FileStat = Option<(u64, u64, i128)>;

fn stat_file(path: &Path) -> FileStat {
    let meta = path.symlink_metadata().ok()?;
    #[cfg(unix)]
    let ino = {
        use std::os::unix::fs::MetadataExt;
        meta.ino()
    };
    #[cfg(not(unix))]
    let ino = 0;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i128);
    Some((ino, meta.len(), mtime))
}

/// The four files the projector and G11 read (§4.4 step 2).
fn audited_stats(home: &Path) -> [FileStat; 4] {
    ["config.yaml", ".env", ".op.env", "auth.json"].map(|f| stat_file(&home.join(f)))
}

/// Everything the lock-free half of a launch produced (§4.4 step 2).
pub(crate) struct Launch {
    pub(crate) name: String,
    pub(crate) profile: HermesProfile,
    pub(crate) paths: HermesPaths,
    pub(crate) install: Install,
    pub(crate) projection: ProjectionV1,
    pub(crate) managed_dir: Option<PathBuf>,
    pub(crate) dynamic_scrub: BTreeSet<String>,
    pub(crate) warnings: Vec<String>,
    stats: [FileStat; 4],
}

/// Which verb the preflight runs for: `start` scans argv (G5) and needs the
/// S7(f) gate; `auth` does neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb<'a> {
    Start(&'a [String]),
    Auth,
}

/// G1–G4 over a profile's paths.
fn shape_guards(name: &str, paths: &HermesPaths) -> Result<()> {
    guards::g1_shape(name, paths)?;
    guards::g2_containment(name, paths, &crate::profile::home_dir()?)?;
    guards::g3_active_profile(name, paths)?;
    guards::g4_profiles(name, paths)
}

/// The resolver input for this roster.
fn resolve_env() -> Result<resolve::ResolveEnv> {
    let settings = HermesState::load()?.settings();
    Ok(resolve_env_with(settings.bin))
}

fn resolve_env_with(bin: Option<PathBuf>) -> resolve::ResolveEnv {
    #[cfg(test)]
    {
        // Tests never resolve an install from the real environment: only the
        // roster's `[settings] bin` (a fixture) or the sandbox home.
        let home = crate::profile::home_dir().unwrap_or_default();
        resolve::ResolveEnv {
            settings_bin: bin,
            mise_data_dir: home.join(".local/share/mise"),
            pipx_home: home.join(".local/pipx"),
            path: None,
            tollgate_dir: crate::profile::tollgate_dir().unwrap_or_default(),
        }
    }
    #[cfg(not(test))]
    resolve::ResolveEnv::from_process(bin)
}

/// Resolve the install, as a refusal naming `name`.
pub(crate) fn resolve_install(name: &str) -> Result<Install> {
    resolve::resolve_entrypoint(&resolve_env()?).map_err(|e| refuse(e.message(name)))
}

/// G1–G6 (argv included for `start`), the entrypoint: what `--explain` runs
/// before it prints the pick line.
pub(crate) fn preflight_explain(
    name: &str,
    profile: &HermesProfile,
    args: &[String],
) -> Result<()> {
    let paths = HermesPaths::for_name(name)?;
    shape_guards(name, &paths)?;
    guards::g5_argv(name, profile.provider.as_str(), args)?;
    let install = resolve_install(name)?;
    guards::g6_hsp_env(name, &install.hsp)
}

/// The lock-free half of `start` and `auth`: G1–G6, G14, G15, then the
/// projector. The four audited files are stat'ed before P reads them, and the
/// guard re-stats them ([`audit_in_guard`]).
pub(crate) fn preflight(name: &str, profile: &HermesProfile, verb: Verb<'_>) -> Result<Launch> {
    let paths = HermesPaths::for_name(name)?;
    shape_guards(name, &paths)?;
    if let Verb::Start(args) = verb {
        guards::g5_argv(name, profile.provider.as_str(), args)?;
    }
    let install = resolve_install(name)?;
    guards::g6_hsp_env(name, &install.hsp)?;
    guards::g14_liveness(name, &paths)?;

    let mut warnings = Vec::new();
    let policy = HermesState::load()?
        .settings()
        .version_policy
        .unwrap_or_default();
    match resolve::version_verdict(name, &install.version, policy) {
        resolve::VersionVerdict::Ok => {}
        resolve::VersionVerdict::Warn(line) => warnings.push(line),
        resolve::VersionVerdict::Refuse(line) => return Err(refuse(line)),
    }
    if matches!(verb, Verb::Start(_))
        && let Some(line) = resolve::s7f_gate_refusal(name, &install.version)
    {
        return Err(refuse(line));
    }

    let managed_dir = guards::managed_dir();
    let dynamic_scrub = guards::plugin_env_vars(&guards::plugin_roots(&install.hsp, &paths.home));
    let stats = audited_stats(&paths.home);
    let command = child_command(
        &install.python,
        &paths,
        &dynamic_scrub,
        &active_claude_env_keys(),
    );
    let projection = projector::run_projector(command, &paths.home, managed_dir.as_deref())
        .map_err(|e| {
            refuse(format!(
                "tollgate: hermes '{name}': cannot audit {}/config.yaml ({e:#})",
                paths.home.display()
            ))
        })?;
    Ok(Launch {
        name: name.to_string(),
        profile: profile.clone(),
        paths,
        install,
        projection,
        managed_dir,
        dynamic_scrub,
        warnings,
        stats,
    })
}

/// The in-guard half (§4.4 step 2): re-stat the four files (M-CHANGED),
/// then G2a, G7–G13 and the `.env` launch normalisation, none of which
/// spawn. Returns the warnings to print (W-MANAGED, the re-attribution note).
pub(crate) fn audit_in_guard(launch: &Launch, rotation: &RotationGuard) -> Result<Vec<String>> {
    let name = launch.name.as_str();
    let paths = &launch.paths;
    if audited_stats(&paths.home) != launch.stats {
        return Err(refuse(format!(
            "tollgate: hermes '{name}': home changed during audit; retry"
        )));
    }
    g2a_child_home(name, paths, rotation)?;
    let mut notes = guards::audit_projection(
        name,
        &paths.home,
        &launch.profile,
        &launch.projection,
        launch.managed_dir.as_deref(),
        &launch.dynamic_scrub,
        &launch.install.entry,
    )?
    .warnings;
    guards::g11_read_and_check(name, &paths.home)?;
    guards::g12_env(name, &launch.projection)?;
    if let Some(note) = g13_env_audit(launch, Some(&guards::home_env_key_set(&launch.projection)))?
    {
        notes.push(note);
    }
    Ok(notes)
}

/// G2a, backfilling a child home a profile made before the rule lacks.
fn g2a_child_home(name: &str, paths: &HermesPaths, _rotation: &RotationGuard) -> Result<()> {
    match home::audit_child_home(&paths.child_home)? {
        home::ChildHomeVerdict::Ok => Ok(()),
        home::ChildHomeVerdict::Missing => home::build_child_home(&paths.child_home),
        home::ChildHomeVerdict::Foreign(entry) => Err(m_child_home(name, paths, &entry)),
    }
}

pub(crate) fn m_child_home(name: &str, paths: &HermesPaths, entry: &str) -> anyhow::Error {
    refuse(format!(
        "tollgate: hermes '{name}': {} holds '{entry}', which tollgate did not put there; remove \
         it (the child home carries only .gitconfig, .config/git and .ssh links)",
        paths.child_home.display()
    ))
}

/// G13's env half: an env-mode home must hold its key, and a fingerprint that
/// moved is re-attributed in the roster (the file is never rewritten).
fn g13_env_audit(
    launch: &Launch,
    projector_keys: Option<&BTreeSet<String>>,
) -> Result<Option<String>> {
    let profile = &launch.profile;
    if profile.auth != Auth::Env {
        return Ok(None);
    }
    let var = profile
        .key_env
        .clone()
        .unwrap_or_else(|| profile.provider.key_env().to_string());
    let audit = env_file::audit_key(&launch.paths, &launch.name, &var, projector_keys)
        .map_err(|e| guards::as_refusal(&launch.name, e))?;
    if profile.key_fingerprint.as_deref() == Some(audit.fingerprint.as_str()) {
        return Ok(None);
    }
    HermesState::update(|state| {
        state.set_fingerprint(&launch.name, &audit.fingerprint);
        Ok(())
    })?;
    Ok(Some(format!(
        "tollgate: note — {var} changed outside tollgate; re-attributed"
    )))
}

/// The roster entry for `name`, or a not-found error.
pub(crate) fn find_profile(name: &str) -> Result<HermesProfile> {
    HermesState::load()?
        .find(name)
        .cloned()
        .with_context(|| format!("Hermes profile '{name}' not found"))
}

// ── `hermes new` ─────────────────────────────────────────────────────────────

/// `tollgate hermes new`'s inputs, after flag parsing.
#[derive(Debug, Clone)]
pub(crate) struct NewOpts {
    pub(crate) name: String,
    pub(crate) provider: Provider,
    pub(crate) model: Option<String>,
    pub(crate) pool: bool,
    pub(crate) env_key: bool,
    pub(crate) no_key: bool,
}

/// What `new` resolved the flags to (§4.1 step 1).
fn new_binding(opts: &NewOpts) -> Result<(Mode, Auth, Option<&'static str>)> {
    if opts.pool {
        return Ok((Mode::Pool, Auth::Pool, None));
    }
    if opts.env_key && opts.provider != Provider::Nous {
        bail!(
            "--env-key is for nous only; {} always binds its key in .env",
            opts.provider
        );
    }
    let auth = match opts.provider {
        Provider::Nous if !opts.env_key => Auth::Oauth,
        _ => Auth::Env,
    };
    if opts.no_key && auth != Auth::Env {
        bail!("--no-key is for env-mode homes; this one logs in with 'tollgate hermes auth'");
    }
    let key_env = (auth == Auth::Env).then(|| opts.provider.key_env());
    Ok((Mode::Account, auth, key_env))
}

/// §4.1 step 2: the charset, then the reserved name.
pub(crate) fn validate_new_name(name: &str) -> Result<String> {
    let trimmed = crate::actions::validate_name_chars(name)?.to_string();
    if trimmed.eq_ignore_ascii_case("profiles") {
        bail!("{}", guards::M_NAME);
    }
    Ok(trimmed)
}

/// `hermes new`, with the key source injected: `read_key` runs before any
/// lock and only for an env-mode home that takes a key now.
pub(crate) fn new_profile(
    opts: &NewOpts,
    read_key: &mut dyn FnMut(Provider) -> Result<String>,
) -> Result<()> {
    refuse_on_windows()?;
    let (mode, auth, key_env) = new_binding(opts)?;
    let name = validate_new_name(&opts.name)?;

    // Step 3: the key, before any lock.
    let key = match key_env {
        Some(_) if !opts.no_key => {
            unlocked_point("the key prompt");
            Some(env_file::validate_key(&read_key(opts.provider)?)?)
        }
        _ => None,
    };

    let paths = HermesPaths::for_name(&name)?;
    // Step 4–5: RotationGuard, then the state lock.
    let rotation =
        RotationGuard::acquire_with_timeout(&ProfileName::from(name.as_str()), ROTATION_WAIT)?;
    HermesState::update(|state| {
        crate::actions::validate_profile_name(&name, crate::harness::Harness::Hermes, None)?;
        if !home::is_adoptable_leftover(&paths.profile)? {
            bail!(
                "profiles/{name} exists and is not a leftover Hermes home; remove it or pick \
                 another name"
            );
        }
        home::build_layout(&paths)?;
        let fingerprint = match (&key, key_env) {
            (Some(key), Some(var)) => {
                env_file::write_key(&paths, &name, var, key, None, &rotation)?;
                Some(env_file::fingerprint(key))
            }
            _ => None,
        };
        state.add_profile(HermesProfile {
            name: name.clone(),
            provider: opts.provider,
            model: opts.model.clone(),
            mode,
            auth,
            key_env: key_env.map(str::to_string),
            key_fingerprint: fingerprint,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        });
        Ok(())
    })?;
    drop(rotation);

    outln!(
        "tollgate: created Hermes profile '{name}' ({}, {} home, auth {})",
        opts.provider,
        mode.as_str(),
        auth.as_str()
    );
    match (auth, key.is_some()) {
        (Auth::Oauth, _) => outln!("next: tollgate hermes auth {name} add nous --type oauth"),
        (Auth::Pool, _) => outln!(
            "next: tollgate hermes auth {name} add {} --type api-key --label <account>",
            opts.provider
        ),
        (Auth::Env, false) => outln!("next: tollgate hermes key {name}"),
        (Auth::Env, true) => outln!("next: tollgate start {name}"),
    }

    // Step 7: pin the auxiliary providers, no lock held.
    pin_auxiliary(&name, &paths, opts.provider);
    // Step 8 (H2h): herdr's Hermes integration for this home.
    herdr_integration(&paths);
    Ok(())
}

/// How long `herdr integration install hermes` may take.
const HERDR_INTEGRATION_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
/// The herdr the tests stage (a recording stub); `None` means no herdr, so a
/// test never runs the operator's real one against its real config.
pub(crate) static HERDR_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// The herdr `new` runs, when one is installed.
fn herdr_for_integration() -> Option<PathBuf> {
    #[cfg(test)]
    {
        HERDR_OVERRIDE.lock().ok().and_then(|g| g.clone())
    }
    #[cfg(not(test))]
    {
        crate::herdr::resolved_bin()
    }
}

/// §4.1 step 8 (H2h): outside guest mode, and when herdr is installed, run
/// `HERMES_HOME=<home> herdr integration install hermes` once, so herdr
/// recognises this home's Hermes panes; a failure is a warning. In guest mode
/// the command is printed instead (D-H11: whether it touches herdr's own
/// config, which upstream clauth's plugin shares, is unconfirmed). No lock is
/// held, stdin is null, and the run is bounded.
fn herdr_integration(paths: &HermesPaths) {
    let text = format!(
        "HERMES_HOME={} herdr integration install hermes",
        paths.home.display()
    );
    if crate::identity::upstream_active() {
        outln!(
            "tollgate: guest mode leaves herdr's config alone; to have herdr tag this home's panes, run: {text}"
        );
        return;
    }
    let Some(bin) = herdr_for_integration() else {
        return;
    };
    let mut command = crate::providers::billing_key::helper_command(bin);
    crate::herdr::strip_session_env(&mut command);
    command
        .arg("integration")
        .arg("install")
        .arg("hermes")
        .env("HERMES_HOME", &paths.home);
    match run_bounded(
        command,
        HERDR_INTEGRATION_TIMEOUT,
        "herdr integration install",
    ) {
        Ok(out) if out.status.success() => {}
        _ => errln!("tollgate: warning — `{text}` did not succeed; run it by hand"),
    }
}

/// §4.1 step 7: `<hermes> config set auxiliary.<task>.provider <provider>`
/// for every pinned task, with the child env, stdin null, 10 s each. A
/// failure prints the command to finish by hand; the next `start` refuses
/// (M-AUX) until it is done.
fn pin_auxiliary(name: &str, paths: &HermesPaths, provider: Provider) {
    let hint = |entry: &str, task: &str| {
        errln!(
            "tollgate: finish by hand: {entry} config set auxiliary.{task}.provider {provider} \
             (with HERMES_HOME={} HOME={})",
            paths.home.display(),
            paths.child_home.display()
        );
    };
    let install = match resolve_install(name) {
        Ok(install) => install,
        Err(e) => {
            errln!("{e}");
            errln!(
                "tollgate: the auxiliary providers are not pinned yet; 'tollgate start {name}' \
                 refuses until they are"
            );
            for task in guards::HERMES_AUX_TASKS {
                hint("<hermes>", task);
            }
            return;
        }
    };
    let dynamic = guards::plugin_env_vars(&guards::plugin_roots(&install.hsp, &paths.home));
    let active = active_claude_env_keys();
    for task in guards::HERMES_AUX_TASKS {
        let mut command = child_command(&install.entry, paths, &dynamic, &active);
        command
            .arg("config")
            .arg("set")
            .arg(format!("auxiliary.{task}.provider"))
            .arg(provider.as_str());
        let ok = run_bounded(command, CONFIG_SET_TIMEOUT, "hermes config set")
            .is_ok_and(|out| out.status.success());
        if !ok {
            hint(&install.entry.display().to_string(), task);
        }
    }
}

/// Read a key from the terminal (input hidden) or from one stdin line.
pub(crate) fn read_key_interactive(stdin: bool, provider: Provider) -> Result<String> {
    if stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("failed to read the key from stdin")?;
        return Ok(line);
    }
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        bail!("no terminal to prompt for the key; pass --stdin, or --no-key to set it later");
    }
    rpassword::prompt_password(format!(
        "Paste your {} API key (input hidden): ",
        provider.display_name()
    ))
    .context("failed to read the key")
}

// ── `hermes key` ─────────────────────────────────────────────────────────────

/// `hermes key`: read the key without locks, then RotationGuard; G1–G4 and
/// G14; then the §4.2 writer and the roster fingerprint.
pub(crate) fn set_key(
    name: &str,
    read_key: &mut dyn FnMut(Provider) -> Result<String>,
) -> Result<()> {
    refuse_on_windows()?;
    let profile = find_profile(name)?;
    if profile.mode == Mode::Pool || profile.auth != Auth::Env {
        return Err(refuse(format!(
            "tollgate: hermes '{name}': this is a{} home; its credentials are Hermes' own \
             (use 'tollgate hermes auth {name} add ...')",
            if profile.mode == Mode::Pool {
                " pool"
            } else {
                "n OAuth"
            }
        )));
    }
    let var = profile
        .key_env
        .clone()
        .unwrap_or_else(|| profile.provider.key_env().to_string());
    unlocked_point("the key prompt");
    let key = env_file::validate_key(&read_key(profile.provider)?)?;
    let paths = HermesPaths::for_name(name)?;
    let projector_keys = projector_keys_for(name, &paths);

    let rotation = RotationGuard::acquire_with_timeout(&ProfileName::from(name), ROTATION_WAIT)?;
    shape_guards(name, &paths)?;
    guards::g14_liveness(name, &paths)?;
    let written = env_file::write_key(&paths, name, &var, &key, projector_keys.as_ref(), &rotation)
        .map_err(|e| guards::as_refusal(name, e))?;
    let fingerprint = env_file::fingerprint(&key);
    HermesState::update(|state| {
        if !state.holds(name) {
            bail!("Hermes profile '{name}' not found");
        }
        state.set_fingerprint(name, &fingerprint);
        Ok(())
    })?;
    drop(rotation);
    match written {
        env_file::Written::Written => outln!("tollgate: set {var} for Hermes profile '{name}'"),
        env_file::Written::Unchanged => {
            outln!("tollgate: {var} for Hermes profile '{name}' is already that key")
        }
    }
    Ok(())
}

/// The projector's view of the home `.env` for the writer's cross-check,
/// when there is a file to cross-check and Hermes resolves; `None` otherwise
/// (the writer then relies on its own multiline scan).
fn projector_keys_for(name: &str, paths: &HermesPaths) -> Option<BTreeSet<String>> {
    let env = paths.env_file();
    if env.metadata().map_or(true, |m| m.len() == 0) {
        return None;
    }
    let install = resolve_install(name).ok()?;
    let dynamic = guards::plugin_env_vars(&guards::plugin_roots(&install.hsp, &paths.home));
    let command = child_command(&install.python, paths, &dynamic, &active_claude_env_keys());
    let projection =
        projector::run_projector(command, &paths.home, guards::managed_dir().as_deref()).ok()?;
    Some(guards::home_env_key_set(&projection))
}

// ── `hermes auth` ────────────────────────────────────────────────────────────

/// One `hermes auth` hand-off, already in Hermes' argv form after `auth`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthAction {
    Add {
        provider: String,
        auth_type: String,
        label: Option<String>,
        no_browser: bool,
        timeout: Option<u64>,
    },
    Remove {
        provider: String,
        target: String,
    },
    Reset {
        provider: String,
    },
}

impl AuthAction {
    fn provider(&self) -> &str {
        match self {
            AuthAction::Add { provider, .. }
            | AuthAction::Remove { provider, .. }
            | AuthAction::Reset { provider } => provider,
        }
    }

    /// The args after `auth`. Never `--api-key`, `--portal-url`,
    /// `--inference-url`, `--client-id`, `--scope`, `--insecure` or
    /// `--ca-bundle`: Hermes prompts for the key itself.
    pub(crate) fn argv(&self) -> Vec<String> {
        match self {
            AuthAction::Add {
                provider,
                auth_type,
                label,
                no_browser,
                timeout,
            } => {
                let t = if auth_type == "api_key" {
                    "api-key"
                } else {
                    auth_type
                };
                let mut v = vec!["add".into(), provider.clone(), "--type".into(), t.into()];
                if let Some(label) = label {
                    v.extend(["--label".into(), label.clone()]);
                }
                if *no_browser {
                    v.push("--no-browser".into());
                }
                if let Some(t) = timeout {
                    v.extend(["--timeout".into(), t.to_string()]);
                }
                v
            }
            AuthAction::Remove { provider, target } => {
                vec!["remove".into(), provider.clone(), target.clone()]
            }
            AuthAction::Reset { provider } => vec!["reset".into(), provider.clone()],
        }
    }
}

/// `hermes auth` (§4.8): the anthropic alias refusal before G1; the lock-free
/// preflight; RotationGuard with the in-guard audit and G13; a marker with no
/// row; then the hand-off with the terminal inherited and no lock held.
/// Returns the child's exit code.
pub(crate) fn run_auth(name: &str, action: &AuthAction) -> Result<i32> {
    refuse_on_windows()?;
    guards::refuse_anthropic_provider(name, action.provider())?;
    let profile = find_profile(name)?;
    let launch = preflight(name, &profile, Verb::Auth)?;
    for w in &launch.warnings {
        errln!("{w}");
    }
    let rotation = RotationGuard::acquire_with_timeout(&ProfileName::from(name), ROTATION_WAIT)?;
    let notes = audit_in_guard_for_auth(&launch, action, &rotation)?;
    let marker = crate::runtime::HermesMarker::claim(name, false, &rotation, || {
        guards::m_busy(name, "another tollgate command holds it")
    })?;
    drop(rotation);
    for n in notes {
        errln!("{n}");
    }
    let mut command = child_command(
        &launch.install.entry,
        &launch.paths,
        &launch.dynamic_scrub,
        &active_claude_env_keys(),
    );
    command.arg("auth").args(action.argv());
    unlocked_point("hermes auth");
    let status = command
        .status()
        .with_context(|| format!("failed to run {}", launch.install.entry.display()))?;
    drop(marker);
    Ok(exit_code_of(status))
}

/// `auth`'s in-guard audit: [`audit_in_guard`] with the env half of G13 only
/// where it applies, plus G13's `auth add` checks.
fn audit_in_guard_for_auth(
    launch: &Launch,
    action: &AuthAction,
    rotation: &RotationGuard,
) -> Result<Vec<String>> {
    let notes = audit_in_guard(launch, rotation)?;
    if let AuthAction::Add { provider, .. } = action {
        let view = pool::read_auth_view(&launch.paths.home).ok().flatten();
        let env_has_key = launch.profile.auth == Auth::Env;
        guards::g13_auth_add(
            &launch.name,
            &launch.profile,
            provider,
            view.as_ref(),
            env_has_key,
        )?;
    }
    Ok(notes)
}

/// A child's exit status as a process exit code: its code, or 128+signal.
pub(crate) fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map_or(1, |s| 128 + s))
    }
    #[cfg(not(unix))]
    status.code().unwrap_or(1)
}

// ── the post-session evidence check ──────────────────────────────────────────

/// How long the post-session `sqlite3` read may take.
const SQLITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The warning the post-session check prints (§4.4 step 6.2).
pub(crate) const ANTHROPIC_SESSION_WARNING: &str = "tollgate: WARNING — this Hermes session \
     called Anthropic (the in-session /model picker); it had no Claude credentials to use, but \
     check the session";

/// §4.4 step 6.2: whether `<home>/state.db` holds a `session_model_usage`
/// row billed to anthropic whose `last_seen` is at or after `run_start_secs`.
/// Read with `sqlite3 -readonly -json`, never Hermes. Evidence, not proof:
/// auxiliary calls need not land in that table (G10a). `false` whenever the
/// read is impossible (no `sqlite3`, no db, an unknown schema).
pub(crate) fn post_session_anthropic_rows(
    home: &Path,
    run_start_secs: i64,
    path: Option<&std::ffi::OsStr>,
) -> bool {
    let db = home.join("state.db");
    let Some(sqlite) = resolve::which_on(path, "sqlite3") else {
        return false;
    };
    if !db.is_file() {
        return false;
    }
    // A helper, like herdr or notify-send: no monitoring or billing key rides
    // into it.
    let mut command = crate::providers::billing_key::helper_command(sqlite);
    command
        .arg("-readonly")
        .arg("-json")
        .arg("-cmd")
        .arg(".timeout 2000")
        .arg(&db)
        .arg(format!(
            "SELECT billing_provider FROM session_model_usage \
             WHERE COALESCE(last_seen, first_seen, 0) >= {run_start_secs}"
        ));
    let Ok(out) = run_bounded(command, SQLITE_TIMEOUT, "sqlite3") else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return false;
    }
    let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(text.trim()) else {
        return false;
    };
    rows.iter().any(|r| {
        r.get("billing_provider")
            .and_then(serde_json::Value::as_str)
            .is_some_and(guards::is_anthropic)
    })
}

// ── `hermes list` ────────────────────────────────────────────────────────────

/// `hermes list`: the roster with provider, mode, live state and the
/// month-to-date estimate ([`show::list`]). Reads files and runs `sqlite3`
/// only.
pub(crate) fn list(json: bool) -> Result<()> {
    show::list(json)
}

// ── delete ───────────────────────────────────────────────────────────────────

/// The delete body (§4.8), after the caller's confirm prompt: RotationGuard,
/// then under the state lock re-check membership, refuse a live session
/// without `force`, remove `profiles/<name>/` and the roster entry.
/// `remove_dir_all` unlinks the child home's links without following them.
/// Returns whether a live session was overridden.
pub(crate) fn delete_profile(name: &str, force: bool) -> Result<bool> {
    refuse_on_windows()?;
    let owned = ProfileName::from(name);
    let _rotation = RotationGuard::acquire_with_timeout(&owned, ROTATION_WAIT)?;
    let paths = HermesPaths::for_name(name)?;
    HermesState::update(|state| {
        if !state.holds(name) {
            bail!("Hermes profile '{name}' not found");
        }
        let live = crate::runtime::has_live_session(&owned);
        if live && !force {
            bail!("'{name}' has a live session, pass --force to remove it anyway");
        }
        if paths.profile.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(&paths.profile)
                .with_context(|| format!("failed to remove profile directory for '{name}'"))?;
        }
        state.remove_profile(name);
        Ok(live)
    })
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_mod.rs"]
mod tests;
