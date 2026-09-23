//! Local Linux cgroup containment and bounded host session observations.
//! Remote/shared jobs and task-ownership transfer remain outside this supervisor.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::{ExecutionMode, agy_session, codex_session, grok_session, native_session};

const MAX_RECORD: usize = 131_072;
const MAX_OUTPUT: usize = 16_384;
const START_TIMEOUT: Duration = Duration::from_secs(8);
const WORKER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionReport {
    pub(crate) schema: u32,
    pub(crate) task_id: String,
    pub(crate) execution_id: String,
    pub(crate) unit: String,
    pub(crate) state: String,
    pub(crate) recorded_scope_empty: bool,
    pub(crate) handoff_ready: bool,
    pub(crate) native_identity_verified: bool,
    pub(crate) launch_never_started: bool,
    pub(crate) finalized: bool,
    pub(crate) execution_mode: ExecutionMode,
    pub(crate) native_session_observation: Option<NativeSessionObservation>,
    pub(crate) limitations: Vec<String>,
    pub(crate) exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    task_id: String,
    execution_id: String,
    unit: String,
    description: String,
    workspace: PathBuf,
    program: PathBuf,
    args: Vec<String>,
    #[serde(default)]
    execution_mode: ExecutionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_session_observation: Option<NativeSessionObservation>,
    boot_id: String,
    launcher_pid: u32,
    launcher_start: String,
    created_at_ms: u64,
    binding: Option<Binding>,
    gate_open: bool,
    never_started: bool,
    finalized: bool,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionObservation {
    pub(crate) response: native_session::SessionObservation,
    pub(crate) native_start: String,
    pub(crate) observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    invocation_id: String,
    cgroup: String,
    cgroup_inode: u64,
}

fn report(record: &Record, state: &str) -> ExecutionReport {
    ExecutionReport {
        schema: 1, task_id: record.task_id.clone(), execution_id: record.execution_id.clone(),
        unit: record.unit.clone(), state: state.into(), recorded_scope_empty: state == "local_empty",
        handoff_ready: false, native_identity_verified: false, launch_never_started: record.never_started, finalized: record.finalized, exit_code: record.exit_code,
        execution_mode: record.execution_mode, native_session_observation: record.native_session_observation.clone(),
        limitations: vec![
            "Native session observations, if present, do not verify the account or exclusive task ownership.".into(),
            "Native init can report a requested session before restoration succeeds; an init observation is not resume confirmation.".into(),
            "Task ownership has not transferred and no graceful native checkpoint is proven.".into(),
            "Jobs launched through the user bus, shared services, Herdr or remote hosts can escape this scope.".into(),
            "Other workspace writers and independently managed resources are not fenced.".into(),
            "An empty recorded scope does not prove escaped descendants have stopped.".into(),
        ],
    }
}

pub(crate) fn run(
    task_id: &str,
    actor: &str,
    generation: u64,
    program: &Path,
    args: &[String],
    mode: ExecutionMode,
) -> Result<ExecutionReport> {
    #[cfg(target_os = "linux")]
    {
        run_linux(task_id, actor, generation, program, args, mode)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (task_id, actor, generation, program, args, mode);
        bail!("local execution supervision requires Linux user systemd and cgroup v2")
    }
}

pub(crate) fn inspect(task_id: &str) -> Result<ExecutionReport> {
    #[cfg(target_os = "linux")]
    {
        let dir = task_directory(task_id)?;
        let _lock = execution_lock(&dir)?;
        let record = read_record(&dir, task_id)?;
        match observe(&record) {
            Ok(state) => Ok(report(&record, state)),
            Err(_) => Ok(report(&record, "unknown")),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = task_id;
        bail!("local execution supervision is unavailable on this platform")
    }
}

pub(crate) fn stop(
    task_id: &str,
    actor: &str,
    generation: u64,
    expected_execution_id: &str,
) -> Result<ExecutionReport> {
    #[cfg(target_os = "linux")]
    {
        let dir = task_directory(task_id)?;
        let _lock = execution_lock(&dir)?;
        let (_task, _task_guard) = super::store::lock_current_actor(task_id, actor, generation)?;
        let record = read_record(&dir, task_id)?;
        verify_expected_execution(&record, expected_execution_id)?;
        stop_record(&record)?;
        Ok(report(&record, "local_empty"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (task_id, actor, generation, expected_execution_id);
        bail!("local execution supervision is unavailable on this platform")
    }
}

/// Caller owns execution then task lock. Never reacquire those locks here.
fn stop_record(record: &Record) -> Result<()> {
    // observe checks the boot, invocation, description and cgroup before
    // this exact unit name can be passed to a destructive control command.
    let state = observe(record)?;
    if state == "local_empty" {
        return Ok(());
    }
    if state != "active" {
        bail!("execution identity or containment is unproven; refusing to stop any unit");
    }
    if command_output(
        "systemctl",
        &["--user", "--no-ask-password", "stop", "--", &record.unit],
        Duration::from_secs(6),
        false,
    )?
    .is_none()
    {
        bail!("systemd could not stop the bound execution scope");
    }
    let started = Instant::now();
    loop {
        if observe(record)? == "local_empty" {
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(4) {
            bail!("execution scope has not been proven empty after stop");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn release_handoff_source(
    task_id: &str,
    actor: &str,
    generation: u64,
    handoff_id: &str,
    execution_id: &str,
) -> Result<super::store::TaskRecord> {
    use super::store::{ReleaseOutcome, begin_source_release};
    let dir = task_directory(task_id)?;
    let _execution_lock = execution_lock(&dir)?;
    let record = read_record(&dir, task_id)?;
    verify_expected_execution(&record, execution_id)?;
    if record.execution_mode != ExecutionMode::Direct || !record.gate_open || record.never_started {
        bail!(
            "source release requires a directly managed execution, not a native-session observer or unstarted launch"
        );
    }
    let binding = record
        .binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("source release lacks a bound execution scope"))?;
    verify_external_coordinator(&verified_coordinator_cgroup()?, &binding.cgroup)?;
    let digest = release_binding_digest(&record)?;
    let task = super::store::show(task_id)?;
    if !task.resources.is_empty() {
        let Some((cgroup, plan_digest)) = super::store::classified_binding(&task, handoff_id)
        else {
            bail!(
                "source release with declared resources requires a verified resource classification"
            );
        };
        if cgroup != binding.cgroup || plan_digest != digest {
            bail!("resource classification does not match the bound source scope");
        }
    }
    let intent = begin_source_release(
        task_id,
        actor,
        generation,
        handoff_id,
        execution_id,
        &digest,
    )?;
    // Errors after the durable intent park this same transition. Never clear
    // the source gate or launch/restart anything, even if shutdown was partial.
    let outcome = if stop_record(&record).is_err() {
        ReleaseOutcome::StopUnproven
    } else {
        match intent.workspace_matches() {
            Ok(true) => ReleaseOutcome::ScopeEmpty,
            Ok(false) => ReleaseOutcome::WorkspaceChanged,
            Err(_) => ReleaseOutcome::WorkspaceUnavailable,
        }
    };
    intent.finish(outcome)
}

fn verify_external_coordinator(own: &str, source: &str) -> Result<()> {
    // Prefix comparisons need a component boundary: scope-other is not a
    // descendant of scope. Fail closed on malformed or ambiguous paths.
    cgroup_path(own)?;
    cgroup_path(source)?;
    if own == source
        || own
            .strip_prefix(source)
            .is_some_and(|tail| tail.starts_with('/'))
    {
        bail!("source release must run from an external coordinator outside the source scope");
    }
    Ok(())
}

fn verified_coordinator_cgroup() -> Result<String> {
    let pid = std::process::id();
    // Establish that procfs has the caller's PID view (including NSpid and
    // namespace metadata checks), rather than interpreting a foreign PID.
    super::resources::process_identity(pid)?;
    let own = own_cgroup()?;
    // proc cgroup paths are relative to the reader's cgroup namespace, whereas
    // the saved binding came from the service manager. Prove that the textual
    // caller path resolves to its actual group in the SAME cgroupfs tree used
    // to verify/stop the source. A rebased path must not masquerade as external.
    let group = cgroup_path(&own)?;
    let kind = bounded_read(&group.join("cgroup.type"), MAX_OUTPUT, false)?;
    let members = bounded_read(&group.join("cgroup.procs"), MAX_OUTPUT, false)?;
    verify_cgroup_membership(&kind, &members, pid)?;
    if own_cgroup()? != own || bounded_read(&group.join("cgroup.type"), MAX_OUTPUT, false)? != kind
    {
        bail!("coordinator cgroup changed during its membership check");
    }
    Ok(own)
}

fn verify_cgroup_membership(kind: &[u8], bytes: &[u8], pid: u32) -> Result<()> {
    // A domain-threaded group's cgroup.procs includes its threaded subtree,
    // which cannot prove this exact group is the caller's own group.
    if kind != b"domain\n" {
        bail!("coordinator requires an unambiguous non-threaded domain cgroup");
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("coordinator cgroup membership is unavailable"))?;
    let mut found = false;
    for line in text.lines() {
        if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
            bail!("coordinator cgroup membership is ambiguous");
        }
        let member = line
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("coordinator cgroup membership is ambiguous"))?;
        if member == 0 {
            bail!("coordinator cgroup membership uses an unavailable PID view");
        }
        found |= member == pid;
    }
    if pid == 0 || !found {
        bail!("coordinator cgroup path does not prove caller membership; refusing source release");
    }
    Ok(())
}

fn release_binding_digest(record: &Record) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&(
        (
            &record.task_id,
            &record.execution_id,
            &record.unit,
            &record.description,
        ),
        (
            &record.workspace,
            &record.program,
            &record.args,
            record.execution_mode,
            &record.source_session_id,
        ),
        (
            &record.boot_id,
            record.launcher_pid,
            &record.launcher_start,
            record.created_at_ms,
        ),
        (&record.binding, record.gate_open, record.never_started),
    ))
    .map_err(|_| anyhow::anyhow!("cannot encode source release binding"))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub(crate) fn worker(task_id: &str, execution_id: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        valid_id(task_id)?;
        valid_id(execution_id)?;
        let dir = task_path(task_id)?;
        let started = Instant::now();
        loop {
            let record = read_record(&dir, task_id)?;
            if record.execution_id != execution_id {
                bail!("execution worker identity no longer matches");
            }
            if record.never_started || record.finalized {
                bail!("execution worker is not pending native launch");
            }
            if record.boot_id != boot_id()?
                || process_start(record.launcher_pid).ok().as_deref()
                    != Some(record.launcher_start.as_str())
            {
                bail!("execution launcher disappeared before the worker gate");
            }
            if record.gate_open {
                if now_ms().saturating_sub(record.created_at_ms) > WORKER_TIMEOUT.as_millis() as u64
                {
                    bail!("execution worker gate is expired");
                }
                let binding = record
                    .binding
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("execution gate lacks a verified binding"))?;
                if own_cgroup()? != binding.cgroup
                    || cgroup_inode(&binding.cgroup)? != Some(binding.cgroup_inode)
                {
                    bail!("execution worker is outside the bound cgroup");
                }
                let unit = query_unit(&record.unit)?;
                verify_unit(&record, &unit)?;
                if record.execution_mode != ExecutionMode::Direct {
                    let session = record.source_session_id.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("native session binding lacks a source identity")
                    })?;
                    let bind = match record.execution_mode {
                        ExecutionMode::GrokSessionBinding => grok_session::bind_existing,
                        ExecutionMode::AgySessionBinding => agy_session::bind_existing,
                        ExecutionMode::CodexSessionBinding => codex_session::bind_existing,
                        ExecutionMode::Direct => unreachable!(),
                    };
                    return bind(
                        &record.program,
                        &record.args,
                        &record.workspace,
                        session,
                        |observation| {
                            persist_native_observation(task_id, execution_id, observation)
                        },
                    );
                }
                let _error = native_command(&record).exec();
                bail!("cannot execute the configured native program");
            }
            if started.elapsed() >= WORKER_TIMEOUT {
                bail!("execution worker gate expired before native launch");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (task_id, execution_id);
        bail!("execution workers are unavailable on this platform")
    }
}

#[cfg(target_os = "linux")]
fn run_linux(
    task_id: &str,
    actor: &str,
    generation: u64,
    program: &Path,
    args: &[String],
    mode: ExecutionMode,
) -> Result<ExecutionReport> {
    let dir = task_directory(task_id)?;
    let lock = execution_lock(&dir)?;
    let (task, task_guard) = super::store::lock_current_actor(task_id, actor, generation)?;
    check_program(program, args)?;
    if !program.is_file() {
        bail!("configured native program is unavailable");
    }
    if fs::symlink_metadata(dir.join("execution.json")).is_ok() {
        let prior = read_record(&dir, task_id)?;
        if observe(&prior)? != "local_empty" {
            bail!("a previous execution is active or unproven; inspect it before starting another");
        }
        verify_finalization(&prior, !prior.finalized && launcher_matches(&prior)?)?;
        archive_record(&dir, &prior)?;
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
        bail!("execution supervision requires cgroup v2");
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| anyhow::anyhow!("cannot generate execution identity"))?;
    let execution_id = hex::encode(random);
    let unit = unit_name(&execution_id)?;
    let mut record = Record {
        schema: 1,
        task_id: task_id.into(),
        description: description(&execution_id),
        execution_id,
        unit,
        workspace: task.workspace,
        program: program.to_path_buf(),
        args: args.to_vec(),
        execution_mode: mode,
        source_session_id: (mode != ExecutionMode::Direct).then_some(task.owner.native_session_id),
        native_session_observation: None,
        boot_id: boot_id()?,
        launcher_pid: std::process::id(),
        launcher_start: process_start(std::process::id())?,
        created_at_ms: now_ms(),
        binding: None,
        gate_open: false,
        never_started: false,
        finalized: false,
        exit_code: None,
    };
    write_record(&dir, &record)?;
    let exe = std::env::current_exe()
        .map_err(|_| anyhow::anyhow!("cannot locate the execution worker"))?;
    let launched = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--no-ask-password",
            "--expand-environment=no",
        ])
        .arg(format!("--unit={}", record.unit))
        .arg(format!("--description={}", record.description))
        .args([
            "--property=KillMode=control-group",
            "--property=SendSIGKILL=yes",
            "--property=TimeoutStopSec=2s",
        ])
        .arg(exe)
        .args(["tasks", "__scope-worker", task_id, &record.execution_id])
        .current_dir(&record.workspace)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    let mut child = match launched {
        Ok(child) => child,
        Err(_) => {
            // spawn() did not create an executing systemd-run process. This
            // terminal proof is narrower than a startup timeout or child exit.
            record.never_started = true;
            record.finalized = true;
            write_record(&dir, &record)?;
            bail!("cannot start systemd execution scope; launch never started");
        }
    };
    let started = Instant::now();
    loop {
        if let Ok(unit) = query_unit(&record.unit)
            && unit.load_state == "loaded"
            && unit.active_state == "active"
            && unit.id == record.unit
            && unit.description == record.description
            && valid_hex(&unit.invocation_id, 32)
            && valid_cgroup(&unit.cgroup, &record.unit).is_ok()
            && let Some(inode) = cgroup_inode(&unit.cgroup)?
        {
            record.binding = Some(Binding {
                invocation_id: unit.invocation_id,
                cgroup: unit.cgroup,
                cgroup_inode: inode,
            });
            // The durable binding and gate become visible atomically. Worker
            // independently checks membership before it can exec user argv.
            record.gate_open = true;
            write_record(&dir, &record)?;
            break;
        }
        if child
            .try_wait()
            .map_err(|_| anyhow::anyhow!("cannot inspect execution launcher"))?
            .is_some()
        {
            bail!(
                "execution scope ended before its identity was bound; native launch was not authorized"
            );
        }
        if started.elapsed() >= START_TIMEOUT {
            bail!("execution scope could not be bound before the gate deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    drop(task_guard);
    drop(lock);
    let status = child
        .wait()
        .map_err(|_| anyhow::anyhow!("cannot wait for execution launcher"))?;
    let _lock = finalization_lock(&dir)?;
    let mut current = read_record(&dir, task_id)?;
    if current.execution_id != record.execution_id {
        bail!("execution record changed while the native program ran");
    }
    current.exit_code = status.code();
    current.finalized = true;
    write_record(&dir, &current)?;
    match observe(&current) {
        Ok(state) => Ok(report(&current, state)),
        Err(_) => Ok(report(&current, "unknown")),
    }
}

fn native_command(record: &Record) -> Command {
    let mut command = Command::new(&record.program);
    command
        .args(&record.args)
        .env("CLAUTH_TASK_ID", &record.task_id)
        .env("CLAUTH_EXECUTION_ID", &record.execution_id)
        .current_dir(&record.workspace)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

fn persist_native_observation(
    task_id: &str,
    execution_id: &str,
    observation: native_session::SessionObservation,
) -> Result<()> {
    let dir = task_path(task_id)?;
    let _lock = finalization_lock(&dir)?;
    let mut record = read_record(&dir, task_id)?;
    verify_expected_execution(&record, execution_id)?;
    if record.execution_mode == ExecutionMode::Direct
        || !record.gate_open
        || record.finalized
        || record.never_started
        || record.native_session_observation.is_some()
        || record.boot_id != boot_id()?
        || record.source_session_id.as_deref() != Some(observation.native_session_id.as_str())
    {
        bail!("native session observation does not match the pending execution");
    }
    let binding = record
        .binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("native observation lacks a bound local scope"))?;
    verify_unit(&record, &query_unit(&record.unit)?)?;
    if own_cgroup()? != binding.cgroup || process_cgroup(observation.native_pid)? != binding.cgroup
    {
        bail!("native session observation is outside its recorded scope");
    }
    let native_start = process_start(observation.native_pid)?;
    record.native_session_observation = Some(NativeSessionObservation {
        response: observation,
        native_start,
        observed_at_ms: now_ms(),
    });
    write_record(&dir, &record)
}

#[derive(Debug)]
struct Unit {
    id: String,
    load_state: String,
    active_state: String,
    invocation_id: String,
    cgroup: String,
    description: String,
}

fn parse_unit(bytes: &[u8]) -> Result<Unit> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("systemd returned invalid execution metadata"))?;
    let mut values = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("systemd returned invalid execution metadata"))?;
        if values.insert(key, value).is_some() {
            bail!("systemd returned duplicate execution metadata");
        }
    }
    let get = |name| {
        values
            .get(name)
            .map(|value| (*value).to_owned())
            .ok_or_else(|| anyhow::anyhow!("systemd execution metadata is incomplete"))
    };
    Ok(Unit {
        id: get("Id")?,
        load_state: get("LoadState")?,
        active_state: get("ActiveState")?,
        invocation_id: get("InvocationID")?,
        cgroup: get("ControlGroup")?,
        description: get("Description")?,
    })
}

fn query_unit(unit: &str) -> Result<Unit> {
    let output = command_output(
        "systemctl",
        &[
            "--user",
            "--no-pager",
            "show",
            "--property=Id,LoadState,ActiveState,InvocationID,ControlGroup,Description",
            "--",
            unit,
        ],
        Duration::from_secs(2),
        true,
    )?
    .ok_or_else(|| anyhow::anyhow!("systemd execution metadata is unavailable"))?;
    parse_unit(&output)
}

fn verify_unit(record: &Record, unit: &Unit) -> Result<()> {
    let binding = record
        .binding
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("execution has no bound systemd identity"))?;
    if !unit_identity_matches(record, unit) || unit.cgroup != binding.cgroup {
        bail!("systemd scope identity differs from the persisted binding");
    }
    if cgroup_inode(&binding.cgroup)? != Some(binding.cgroup_inode) {
        bail!("systemd cgroup identity differs from the persisted binding");
    }
    Ok(())
}

fn unit_identity_matches(record: &Record, unit: &Unit) -> bool {
    record.binding.as_ref().is_some_and(|binding| {
        unit.load_state == "loaded"
            && unit.id == record.unit
            && unit.description == record.description
            && unit.invocation_id == binding.invocation_id
    })
}

fn observe(record: &Record) -> Result<&'static str> {
    if record.never_started {
        validate_record(record, &record.task_id)?;
        return Ok("local_empty");
    }
    if boot_id()? != record.boot_id {
        bail!("execution belongs to a different host boot; control is refused");
    }
    let binding = match &record.binding {
        Some(binding) => binding,
        None => return Ok("unknown"),
    };
    let unit = query_unit(&record.unit)?;
    if unit.load_state == "not-found" {
        return match cgroup_inode(&binding.cgroup)? {
            None => Ok("local_empty"),
            Some(inode) if inode == binding.cgroup_inode && !populated(&binding.cgroup)? => {
                Ok("local_empty")
            }
            _ => Ok("unknown"),
        };
    }
    if unit_identity_matches(record, &unit)
        && matches!(unit.active_state.as_str(), "inactive" | "failed")
        && unit.cgroup.is_empty()
    {
        return match cgroup_inode(&binding.cgroup)? {
            None => Ok("local_empty"),
            Some(inode) if inode == binding.cgroup_inode && !populated(&binding.cgroup)? => {
                Ok("local_empty")
            }
            _ => Ok("unknown"),
        };
    }
    verify_unit(record, &unit)?;
    if populated(&binding.cgroup)? {
        Ok("active")
    } else {
        Ok("local_empty")
    }
}

fn populated(cgroup: &str) -> Result<bool> {
    let bytes = bounded_read(
        &cgroup_path(cgroup)?.join("cgroup.events"),
        MAX_OUTPUT,
        false,
    )?;
    parse_populated(&bytes)
}

fn parse_populated(bytes: &[u8]) -> Result<bool> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("invalid cgroup population metadata"))?;
    let values: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("populated "))
        .collect();
    match values.as_slice() {
        ["0"] => Ok(false),
        ["1"] => Ok(true),
        _ => bail!("cgroup population is unproven"),
    }
}

fn cgroup_path(cgroup: &str) -> Result<PathBuf> {
    let relative = cgroup
        .strip_prefix('/')
        .ok_or_else(|| anyhow::anyhow!("invalid cgroup path"))?;
    if relative.is_empty()
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || relative.chars().any(char::is_control)
    {
        bail!("invalid cgroup path");
    }
    Ok(Path::new("/sys/fs/cgroup").join(relative))
}

fn valid_cgroup(cgroup: &str, unit: &str) -> Result<()> {
    let _ = cgroup_path(cgroup)?;
    if cgroup.rsplit('/').next() != Some(unit) {
        bail!("cgroup is not the expected execution scope");
    }
    Ok(())
}

fn cgroup_inode(cgroup: &str) -> Result<Option<u64>> {
    let path = cgroup_path(cgroup)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Ok(Some(metadata.ino()))
            }
            #[cfg(not(unix))]
            {
                let _ = metadata;
                bail!("cgroup identity is unavailable")
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        _ => bail!("cannot verify cgroup identity"),
    }
}

fn own_cgroup() -> Result<String> {
    read_process_cgroup(Path::new("/proc/self/cgroup"))
}

fn process_cgroup(pid: u32) -> Result<String> {
    read_process_cgroup(&PathBuf::from(format!("/proc/{pid}/cgroup")))
}

fn read_process_cgroup(path: &Path) -> Result<String> {
    let bytes = bounded_read(path, MAX_OUTPUT, false)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("cannot parse worker cgroup identity"))?;
    let paths: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .collect();
    match paths.as_slice() {
        [path] => Ok((*path).into()),
        _ => bail!("worker requires an unambiguous cgroup v2 membership"),
    }
}

fn process_start(pid: u32) -> Result<String> {
    let bytes = bounded_read(
        &PathBuf::from(format!("/proc/{pid}/stat")),
        MAX_OUTPUT,
        false,
    )?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("cannot parse launcher identity"))?;
    // comm may contain whitespace and parentheses; fields after its final ')'
    // start at field 3, so index 19 is Linux starttime (field 22).
    let tail = text
        .rsplit_once(')')
        .ok_or_else(|| anyhow::anyhow!("cannot parse launcher identity"))?
        .1;
    let start = tail
        .split_whitespace()
        .nth(19)
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| anyhow::anyhow!("cannot parse launcher start identity"))?;
    Ok(start.into())
}

fn launcher_matches(record: &Record) -> Result<bool> {
    match process_start(record.launcher_pid) {
        Ok(start) => Ok(start == record.launcher_start),
        Err(error) => match fs::symlink_metadata(format!("/proc/{}/stat", record.launcher_pid)) {
            Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(false),
            _ => Err(error),
        },
    }
}

fn boot_id() -> Result<String> {
    let bytes = bounded_read(Path::new("/proc/sys/kernel/random/boot_id"), 128, false)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("host boot identity is unavailable"))?
        .trim();
    if !valid_boot(text) {
        bail!("host boot identity is invalid");
    }
    Ok(text.into())
}

fn valid_boot(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
            }
        })
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_id(id: &str) -> Result<()> {
    if !valid_hex(id, 32) {
        bail!("invalid task or execution ID");
    }
    Ok(())
}
fn unit_name(id: &str) -> Result<String> {
    valid_id(id)?;
    Ok(format!("clauth-task-{id}.scope"))
}
fn description(id: &str) -> String {
    format!("clauth execution {id}")
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn check_program(program: &Path, args: &[String]) -> Result<()> {
    if !program.is_absolute()
        || program.as_os_str().as_encoded_bytes().contains(&0)
        || args.len() > 256
        || args.iter().map(String::len).sum::<usize>() > MAX_RECORD / 2
        || args
            .iter()
            .any(|arg| arg.len() > 16_384 || arg.contains('\0'))
    {
        bail!("execution command is invalid or exceeds the size limit");
    }
    Ok(())
}

fn task_directory(task_id: &str) -> Result<PathBuf> {
    valid_id(task_id)?;
    let _ = super::store::show(task_id)?;
    task_path(task_id)
}

fn task_path(task_id: &str) -> Result<PathBuf> {
    valid_id(task_id)?;
    let root = crate::profile::clauth_dir()
        .map_err(|_| anyhow::anyhow!("cannot locate execution storage"))?;
    let tasks = root.join("tasks");
    let dir = tasks.join(task_id);
    for path in [&root, &tasks, &dir] {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| anyhow::anyhow!("execution directory is unavailable"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("execution directory is invalid");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path != &root && metadata.permissions().mode() & 0o077 != 0 {
                bail!("execution directory must have private permissions");
            }
        }
    }
    Ok(dir)
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}

fn execution_lock(dir: &Path) -> Result<File> {
    let path = dir.join("execution.lock");
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        bail!("execution lock is invalid");
    }
    let file = options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| anyhow::anyhow!("cannot open execution lock"))?;
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("cannot inspect execution lock"))?;
    if !metadata.is_file() {
        bail!("execution lock is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("execution lock must have private permissions");
        }
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(anyhow::Error::new(ExecutionBusy)),
        Err(_) => bail!("cannot acquire execution lock"),
    }
}

#[derive(Debug)]
struct ExecutionBusy;
impl std::fmt::Display for ExecutionBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("execution is busy; retry after the current operation")
    }
}
impl std::error::Error for ExecutionBusy {}

fn finalization_lock(dir: &Path) -> Result<File> {
    let started = Instant::now();
    loop {
        match execution_lock(dir) {
            Ok(lock) => return Ok(lock),
            Err(error)
                if error.downcast_ref::<ExecutionBusy>().is_some()
                    && started.elapsed() < Duration::from_secs(12) =>
            {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(error) => return Err(error),
        }
    }
}

fn bounded_read(path: &Path, max: usize, private: bool) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("execution metadata is unavailable"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("execution metadata must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if private && metadata.permissions().mode() & 0o077 != 0 {
            bail!("execution record must have private permissions");
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    let file = options()
        .read(true)
        .open(path)
        .map_err(|_| anyhow::anyhow!("cannot read execution metadata"))?;
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot read execution metadata"))?;
    if bytes.len() > max {
        bail!("execution metadata exceeds its size limit");
    }
    Ok(bytes)
}

fn read_record(dir: &Path, task_id: &str) -> Result<Record> {
    let record: Record = serde_json::from_slice(&bounded_read(
        &dir.join("execution.json"),
        MAX_RECORD,
        true,
    )?)
    .map_err(|_| anyhow::anyhow!("execution record has invalid or unsupported JSON"))?;
    validate_record(&record, task_id)?;
    Ok(record)
}

fn validate_record(record: &Record, task_id: &str) -> Result<()> {
    valid_id(task_id)?;
    valid_id(&record.execution_id)?;
    if record.schema != 1
        || record.task_id != task_id
        || record.unit != unit_name(&record.execution_id)?
        || record.description != description(&record.execution_id)
        || !record.workspace.is_absolute()
        || !valid_boot(&record.boot_id)
        || record.launcher_pid == 0
        || record.launcher_start.is_empty()
        || !record.launcher_start.bytes().all(|b| b.is_ascii_digit())
        || (record.gate_open && record.binding.is_none())
        || (record.never_started
            && (record.gate_open
                || record.binding.is_some()
                || record.exit_code.is_some()
                || !record.finalized))
    {
        bail!("execution record has invalid identity or schema");
    }
    check_program(&record.program, &record.args)?;
    match record.execution_mode {
        ExecutionMode::Direct
            if record.source_session_id.is_some()
                || record.native_session_observation.is_some() =>
        {
            bail!("direct execution cannot claim a native protocol observation");
        }
        ExecutionMode::GrokSessionBinding
        | ExecutionMode::AgySessionBinding
        | ExecutionMode::CodexSessionBinding
            if !record
                .source_session_id
                .as_deref()
                .is_some_and(valid_native_text) =>
        {
            bail!("native binding requires a bounded source session identity");
        }
        _ => {}
    }
    if let Some(observed) = &record.native_session_observation
        && (!record.gate_open
            || record.never_started
            || record.source_session_id.as_deref()
                != Some(observed.response.native_session_id.as_str())
            || observed.response.native_pid == 0
            || match record.execution_mode {
                ExecutionMode::Direct => true,
                ExecutionMode::GrokSessionBinding => {
                    observed.response.request_id != Some(2)
                        || observed.response.requested_model.is_some()
                        || observed.response.codex.is_some()
                }
                ExecutionMode::AgySessionBinding => {
                    observed.response.request_id.is_some()
                        || observed.response.configured_model.is_some()
                        || observed.response.codex.is_some()
                }
                ExecutionMode::CodexSessionBinding => {
                    observed.response.request_id != Some(2)
                        || observed.response.configured_model.is_none()
                        || !observed.response.codex.as_ref().is_some_and(|metadata| {
                            valid_native_text(&metadata.session_tree_id)
                                && valid_native_text(&metadata.model_provider)
                        })
                }
            }
            || observed
                .response
                .configured_model
                .as_deref()
                .is_some_and(|model| !valid_native_text(model))
            || observed
                .response
                .requested_model
                .as_deref()
                .is_some_and(|model| !valid_native_text(model))
            || observed.native_start.is_empty()
            || !observed.native_start.bytes().all(|b| b.is_ascii_digit())
            || observed.observed_at_ms < record.created_at_ms)
    {
        bail!("native session observation has invalid identity or scope");
    }
    if let Some(binding) = &record.binding {
        if !valid_hex(&binding.invocation_id, 32) || binding.cgroup_inode == 0 {
            bail!("execution binding is invalid");
        }
        valid_cgroup(&binding.cgroup, &record.unit)?;
    }
    Ok(())
}

fn valid_native_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn verify_expected_execution(record: &Record, expected_execution_id: &str) -> Result<()> {
    valid_id(expected_execution_id)?;
    if record.execution_id != expected_execution_id {
        bail!("stale execution ID; inspect the current execution before requesting stop");
    }
    Ok(())
}

fn verify_finalization(record: &Record, launcher_alive: bool) -> Result<()> {
    if !record.finalized && launcher_alive {
        bail!("previous execution is awaiting launcher finalization; retry after it completes");
    }
    Ok(())
}

fn write_record(dir: &Path, record: &Record) -> Result<()> {
    validate_record(record, &record.task_id)?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|_| anyhow::anyhow!("cannot encode execution record"))?;
    if bytes.len() > MAX_RECORD {
        bail!("execution record exceeds its size limit");
    }
    let target = dir.join("execution.json");
    if let Ok(metadata) = fs::symlink_metadata(&target)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        bail!("execution record target is invalid");
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|_| anyhow::anyhow!("cannot stage execution record"))?;
    temp.write_all(&bytes)
        .map_err(|_| anyhow::anyhow!("cannot write execution record"))?;
    temp.as_file()
        .sync_all()
        .map_err(|_| anyhow::anyhow!("cannot sync execution record"))?;
    temp.persist(target)
        .map_err(|_| anyhow::anyhow!("cannot commit execution record"))?;
    #[cfg(unix)]
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|_| anyhow::anyhow!("cannot sync execution directory"))?;
    Ok(())
}

fn archive_record(dir: &Path, record: &Record) -> Result<()> {
    validate_record(record, &record.task_id)?;
    let target = dir.join(format!("execution-{}.json", record.execution_id));
    if fs::symlink_metadata(&target).is_ok() {
        let previous: Record = serde_json::from_slice(&bounded_read(&target, MAX_RECORD, true)?)
            .map_err(|_| anyhow::anyhow!("execution archive is invalid"))?;
        if previous != *record {
            bail!("immutable execution archive conflicts with the current record");
        }
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|_| anyhow::anyhow!("cannot encode execution archive"))?;
    if bytes.len() > MAX_RECORD {
        bail!("execution archive exceeds its size limit");
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|_| anyhow::anyhow!("cannot stage execution archive"))?;
    temp.write_all(&bytes)
        .map_err(|_| anyhow::anyhow!("cannot write execution archive"))?;
    temp.as_file()
        .sync_all()
        .map_err(|_| anyhow::anyhow!("cannot sync execution archive"))?;
    temp.persist_noclobber(target)
        .map_err(|_| anyhow::anyhow!("cannot commit immutable execution archive"))?;
    #[cfg(unix)]
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|_| anyhow::anyhow!("cannot sync execution archive directory"))?;
    Ok(())
}

fn command_output(
    program: &str,
    args: &[&str],
    timeout: Duration,
    allow_failure_output: bool,
) -> Result<Option<Vec<u8>>> {
    let mut child = Command::new(program)
        .args(args)
        .env("SYSTEMD_PAGER", "cat")
        .env("SYSTEMD_COLORS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| anyhow::anyhow!("systemd control is unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("systemd metadata output is unavailable"))?;
    let (sender, receiver) = std::sync::mpsc::channel();
    // A grandchild can retain stdout after the control process exits. Detach
    // this bounded reader and enforce the receive deadline; joining it would
    // silently make the command timeout unbounded.
    let _reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_OUTPUT as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let start = Instant::now();
    let mut output = None;
    let status = loop {
        if output.is_none() {
            match receiver.try_recv() {
                Ok(Ok(bytes)) if bytes.len() <= MAX_OUTPUT => output = Some(bytes),
                Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("systemd metadata output failed or exceeded its limit");
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("systemd control command timed out or failed");
            }
        }
    };
    let bytes = match output {
        Some(bytes) => bytes,
        None => receiver
            .recv_timeout(timeout.saturating_sub(start.elapsed()))
            .map_err(|_| anyhow::anyhow!("systemd metadata output failed or timed out"))?
            .map_err(|_| anyhow::anyhow!("systemd metadata output failed"))?,
    };
    if bytes.len() > MAX_OUTPUT {
        bail!("systemd metadata output exceeded its limit");
    }
    Ok((status.success() || (allow_failure_output && !bytes.is_empty())).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::HomeSandbox;

    #[test]
    fn source_release_requires_an_outside_coordinator_with_component_boundaries() {
        let source = "/user.slice/source.scope";
        assert!(verify_external_coordinator(source, source).is_err());
        assert!(verify_external_coordinator("/user.slice/source.scope/child", source).is_err());
        assert!(verify_external_coordinator("/user.slice/source.scope-other", source).is_ok());
        assert!(verify_external_coordinator("/user.slice/coordinator.service", source).is_ok());
        assert!(verify_external_coordinator("ambiguous", source).is_err());
        assert!(verify_external_coordinator("/user.slice/../source.scope", source).is_err());
    }

    #[test]
    fn source_release_requires_actual_cgroup_membership_in_the_caller_pid_view() {
        assert!(verify_cgroup_membership(b"domain\n", b"7\n42\n99\n", 42).is_ok());
        for bytes in [
            b"7\n99\n".as_slice(),
            b"",
            b"0\n42\n",
            b"42 junk\n",
            b"+42\n",
            b"4294967296\n",
            b"\xff",
        ] {
            assert!(verify_cgroup_membership(b"domain\n", bytes, 42).is_err());
        }
        assert!(verify_cgroup_membership(b"domain\n", b"42\n", 0).is_err());
        for kind in [
            b"domain threaded\n".as_slice(),
            b"threaded\n",
            b"domain invalid\n",
            b"",
        ] {
            assert!(verify_cgroup_membership(kind, b"42\n", 42).is_err());
        }
    }

    #[test]
    fn source_release_binding_excludes_finalization_but_detects_replacement() {
        let original = record();
        let digest = release_binding_digest(&original).unwrap();
        let mut finalized = original.clone();
        finalized.finalized = true;
        finalized.exit_code = Some(0);
        assert_eq!(release_binding_digest(&finalized).unwrap(), digest);
        for case in 0..7 {
            let mut changed = original.clone();
            match case {
                0 => changed.launcher_start.push('1'),
                1 => changed.boot_id = "00000000-0000-0000-0000-000000000000".into(),
                2 => changed.execution_id = "0".repeat(32),
                3 => changed.workspace = "/other".into(),
                4 => changed.args.push("different".into()),
                5 => {
                    changed.binding = Some(Binding {
                        invocation_id: "1".repeat(32),
                        cgroup: "/other".into(),
                        cgroup_inode: 1,
                    })
                }
                _ => changed.created_at_ms += 1,
            }
            assert_ne!(
                release_binding_digest(&changed).unwrap(),
                digest,
                "case {case}"
            );
        }
    }

    fn record() -> Record {
        let execution_id = "abcdefabcdefabcdefabcdefabcdefab".to_owned();
        Record {
            schema: 1,
            task_id: "12345678123456781234567812345678".into(),
            unit: unit_name(&execution_id).unwrap(),
            description: description(&execution_id),
            execution_id,
            workspace: "/tmp".into(),
            program: "/bin/example".into(),
            args: vec!["literal $HOME".into(), "a b".into(), "$(no-shell)".into()],
            execution_mode: ExecutionMode::Direct,
            source_session_id: None,
            native_session_observation: None,
            boot_id: "12345678-1234-1234-1234-123456789abc".into(),
            launcher_pid: 123,
            launcher_start: "12345".into(),
            created_at_ms: 1,
            binding: None,
            gate_open: false,
            never_started: false,
            finalized: false,
            exit_code: None,
        }
    }

    #[test]
    fn names_paths_and_gate_identity_fail_closed() {
        let good = record();
        assert!(validate_record(&good, &good.task_id).is_ok());
        for bad in [
            "../other",
            "",
            "--help",
            "ABCDEFABCDEFABCDEFABCDEFABCDEFABCD",
        ] {
            assert!(unit_name(bad).is_err());
        }
        for path in ["/", "/../user.slice", "/a//b", "relative", "/a/./b"] {
            assert!(cgroup_path(path).is_err());
        }
        let mut bad = good.clone();
        bad.gate_open = true;
        assert!(validate_record(&bad, &good.task_id).is_err());
        bad = good.clone();
        bad.unit = "foreign.scope".into();
        assert!(validate_record(&bad, &good.task_id).is_err());
        bad = good.clone();
        bad.binding = Some(Binding {
            invocation_id: "bad".into(),
            cgroup: format!("/user.slice/{}", good.unit),
            cgroup_inode: 1,
        });
        assert!(validate_record(&bad, &good.task_id).is_err());
    }

    #[test]
    fn argv_is_preserved_without_shell_expansion() {
        let record = record();
        let command = native_command(&record);
        assert_eq!(command.get_program(), record.program);
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>(),
            record.args.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(command.get_current_dir(), Some(record.workspace.as_path()));
    }

    #[test]
    fn unit_and_population_parsing_require_unambiguous_evidence() {
        let unit = parse_unit(b"Id=example.scope\nLoadState=loaded\nActiveState=active\nInvocationID=1234\nControlGroup=/user.slice/example.scope\nDescription=example\n").unwrap();
        assert_eq!(unit.id, "example.scope");
        assert!(parse_unit(b"Id=a\nId=b\n").is_err());
        assert!(parse_unit(b"LoadState=not-found\n").is_err());
        assert!(!parse_populated(b"populated 0\nfrozen 0\n").unwrap());
        assert!(parse_populated(b"populated 1\n").unwrap());
        for bytes in [
            b"populated 2\n".as_slice(),
            b"populated 0\npopulated 1\n",
            b"frozen 0\n",
        ] {
            assert!(parse_populated(bytes).is_err());
        }
    }

    #[test]
    fn reports_never_claim_full_handoff_or_native_ownership() {
        for state in ["starting", "active", "unknown", "local_empty"] {
            let report = report(&record(), state);
            assert!(!report.handoff_ready);
            assert!(!report.native_identity_verified);
            assert_eq!(report.recorded_scope_empty, state == "local_empty");
            assert!(report.limitations.iter().any(|line| line.contains("Herdr")));
            assert!(
                report
                    .limitations
                    .iter()
                    .any(|line| line.contains("do not verify the account"))
            );
        }
    }

    #[test]
    fn old_direct_execution_records_need_no_native_observation_fields() {
        let mut encoded = serde_json::to_value(record()).unwrap();
        let object = encoded.as_object_mut().unwrap();
        object.remove("execution_mode");
        object.remove("source_session_id");
        object.remove("native_session_observation");
        let decoded: Record = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.execution_mode, ExecutionMode::Direct);
        assert!(decoded.native_session_observation.is_none());
        assert!(validate_record(&decoded, &decoded.task_id).is_ok());
    }

    #[test]
    fn observed_session_is_bound_to_source_and_gated_execution_without_ownership_claim() {
        let mut record = record();
        record.execution_mode = ExecutionMode::GrokSessionBinding;
        record.source_session_id = Some("native-session".into());
        record.binding = Some(Binding {
            invocation_id: "12345678123456781234567812345678".into(),
            cgroup: format!("/user.slice/{}", record.unit),
            cgroup_inode: 1,
        });
        record.gate_open = true;
        record.native_session_observation = Some(NativeSessionObservation {
            response: native_session::SessionObservation {
                native_session_id: "native-session".into(),
                configured_model: Some("grok-test".into()),
                requested_model: None,
                native_pid: 456,
                request_id: Some(2),
                codex: None,
            },
            native_start: "23456".into(),
            observed_at_ms: 2,
        });
        assert!(validate_record(&record, &record.task_id).is_ok());
        let observed = report(&record, "local_empty");
        assert!(observed.native_session_observation.is_some());
        assert!(!observed.native_identity_verified && !observed.handoff_ready);
        let mut agy = record.clone();
        agy.execution_mode = ExecutionMode::AgySessionBinding;
        let response = &mut agy.native_session_observation.as_mut().unwrap().response;
        response.configured_model = None;
        response.requested_model = Some("agy-fixture".into());
        response.request_id = None;
        assert!(validate_record(&agy, &agy.task_id).is_ok());
        let mut codex = record.clone();
        codex.execution_mode = ExecutionMode::CodexSessionBinding;
        codex
            .native_session_observation
            .as_mut()
            .unwrap()
            .response
            .codex = Some(native_session::CodexSessionMetadata {
            session_tree_id: "different-tree-id".into(),
            model_provider: "openai".into(),
        });
        // A requested alias may resolve to a different configured model name.
        codex
            .native_session_observation
            .as_mut()
            .unwrap()
            .response
            .requested_model = Some("requested-alias".into());
        assert!(validate_record(&codex, &codex.task_id).is_ok());
        for bad in 0..5 {
            let mut changed = codex.clone();
            let response = &mut changed
                .native_session_observation
                .as_mut()
                .unwrap()
                .response;
            match bad {
                0 => response.codex = None,
                1 => response.codex.as_mut().unwrap().session_tree_id = String::new(),
                2 => response.codex.as_mut().unwrap().model_provider = "unsafe\nprovider".into(),
                3 => response.configured_model = None,
                _ => response.requested_model = Some("unsafe\nmodel".into()),
            }
            assert!(validate_record(&changed, &changed.task_id).is_err());
        }
        for bad in 0..3 {
            let mut changed = agy.clone();
            let response = &mut changed
                .native_session_observation
                .as_mut()
                .unwrap()
                .response;
            match bad {
                0 => response.configured_model = Some("unverified".into()),
                1 => response.request_id = Some(2),
                _ => response.requested_model = Some("unsafe\nmodel".into()),
            }
            assert!(validate_record(&changed, &changed.task_id).is_err());
        }
        for bad in 0..4 {
            let mut changed = record.clone();
            match bad {
                0 => changed.source_session_id = Some("different-session".into()),
                1 => changed.execution_mode = ExecutionMode::Direct,
                2 => changed.gate_open = false,
                _ => {
                    changed
                        .native_session_observation
                        .as_mut()
                        .unwrap()
                        .response
                        .request_id = Some(1)
                }
            }
            assert!(validate_record(&changed, &changed.task_id).is_err());
        }
    }

    #[test]
    fn durable_private_record_is_bounded_and_schema_checked() {
        let home = HomeSandbox::new();
        let dir = home.home();
        let record = record();
        write_record(dir, &record).unwrap();
        assert_eq!(read_record(dir, &record.task_id).unwrap().args, record.args);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("execution.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let mut corrupt = record.clone();
        corrupt.schema = 2;
        assert!(write_record(dir, &corrupt).is_err());
        let mut file = options()
            .write(true)
            .truncate(true)
            .open(dir.join("execution.json"))
            .unwrap();
        file.write_all(&vec![b'x'; MAX_RECORD + 1]).unwrap();
        assert!(
            read_record(dir, &record.task_id)
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
    }

    #[test]
    fn execution_lock_is_nonblocking_and_separate_from_task_state() {
        let home = HomeSandbox::new();
        let dir = home.home();
        let _held = execution_lock(dir).unwrap();
        assert!(
            execution_lock(dir)
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        assert!(!dir.join(".lock").exists());
    }

    #[test]
    fn foreign_invocation_description_and_unit_never_match() {
        let mut record = record();
        record.binding = Some(Binding {
            invocation_id: "12345678123456781234567812345678".into(),
            cgroup: format!("/user.slice/{}", record.unit),
            cgroup_inode: 1,
        });
        let unit = || Unit {
            id: record.unit.clone(),
            load_state: "loaded".into(),
            active_state: "failed".into(),
            invocation_id: record.binding.as_ref().unwrap().invocation_id.clone(),
            cgroup: String::new(),
            description: record.description.clone(),
        };
        assert!(unit_identity_matches(&record, &unit()));
        let mut foreign = unit();
        foreign.invocation_id = "abcdefabcdefabcdefabcdefabcdefab".into();
        assert!(!unit_identity_matches(&record, &foreign));
        foreign = unit();
        foreign.description = "foreign".into();
        assert!(!unit_identity_matches(&record, &foreign));
        foreign = unit();
        foreign.id = "foreign.scope".into();
        assert!(!unit_identity_matches(&record, &foreign));
        foreign = unit();
        foreign.load_state = "not-found".into();
        assert!(!unit_identity_matches(&record, &foreign));
    }

    #[test]
    fn immutable_archives_preserve_argv_and_reject_replacement() {
        let home = HomeSandbox::new();
        let record = record();
        archive_record(home.home(), &record).unwrap();
        archive_record(home.home(), &record).unwrap();
        let mut changed = record.clone();
        changed.args.push("different".into());
        assert!(archive_record(home.home(), &changed).is_err());
        let bytes = bounded_read(
            &home
                .home()
                .join(format!("execution-{}.json", record.execution_id)),
            MAX_RECORD,
            true,
        )
        .unwrap();
        assert_eq!(serde_json::from_slice::<Record>(&bytes).unwrap(), record);
        assert!(check_program(Path::new("/bin/example"), &["x".repeat(MAX_RECORD)]).is_err());
        assert!(check_program(Path::new("/bin/example"), &["nul\0value".into()]).is_err());
    }

    #[test]
    fn stale_stop_cannot_target_a_newer_execution() {
        let current = record();
        assert!(verify_expected_execution(&current, &current.execution_id).is_ok());
        assert!(verify_expected_execution(&current, "12345678123456781234567812345678").is_err());
        assert!(verify_expected_execution(&current, "../scope").is_err());
    }

    #[test]
    fn never_started_is_terminal_only_without_any_bound_or_open_gate() {
        let mut never = record();
        never.never_started = true;
        never.finalized = true;
        assert_eq!(observe(&never).unwrap(), "local_empty");
        assert!(report(&never, "local_empty").launch_never_started);
        let mut invalid = never.clone();
        invalid.gate_open = true;
        assert!(validate_record(&invalid, &invalid.task_id).is_err());
        invalid = never.clone();
        invalid.finalized = false;
        assert!(validate_record(&invalid, &invalid.task_id).is_err());
        invalid = never.clone();
        invalid.binding = Some(Binding {
            invocation_id: "12345678123456781234567812345678".into(),
            cgroup: format!("/user.slice/{}", never.unit),
            cgroup_inode: 1,
        });
        assert!(validate_record(&invalid, &invalid.task_id).is_err());
        invalid = never;
        invalid.exit_code = Some(0);
        assert!(validate_record(&invalid, &invalid.task_id).is_err());
    }

    #[test]
    fn unfinalized_live_launcher_blocks_replacement_even_if_scope_was_empty() {
        let mut record = record();
        assert!(verify_finalization(&record, true).is_err());
        assert!(verify_finalization(&record, false).is_ok());
        record.finalized = true;
        // A signalled child legitimately has no numeric exit code.
        assert_eq!(record.exit_code, None);
        assert!(verify_finalization(&record, true).is_ok());
    }

    #[test]
    fn finalization_retries_a_short_inspection_lock() {
        let home = HomeSandbox::new();
        let held = execution_lock(home.home()).unwrap();
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        let final_lock = finalization_lock(home.home()).unwrap();
        releasing.join().unwrap();
        drop(final_lock);
    }

    #[test]
    fn retained_stdout_cannot_extend_the_control_command_deadline() {
        // A short-lived test grandchild keeps the pipe open after its parent
        // exits. No systemd scope or native provider session is created.
        let result = command_output(
            "/bin/sh",
            &["-c", "sleep 0.3 &"],
            Duration::from_millis(30),
            false,
        );
        assert!(result.is_err());
    }
}
