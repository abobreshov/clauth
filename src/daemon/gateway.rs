//! The managed shunt gateway's supervisor: the one `shunt run` child the
//! active daemon runs on the engine in [`crate::gateway`], its `/health` read,
//! its restarts, and the `gateway` slot `status.json` and
//! `GET /api/v1/gateway` publish.
//!
//! The gateway runs iff the record exists, its config exists and `disabled`
//! is off, never inside this process, and only under the active daemon:
//! [`start`] takes the singleton's [`DaemonLock`], which a parked standby does
//! not hold. clauth signals only a process it spawned: its own [`Child`], or
//! the orphan of a daemon that died hard, matched by the pid AND the process
//! start time recorded in `~/.clauth/gateway-child.json`.
//!
//! Threads: the supervisor's rounds run on the `clauth-gateway` thread, paced
//! to at most one probe per [`SUPERVISE_POLL`], so a `/health` probe (up to
//! [`crate::gateway::HEALTH_PROBE_TIMEOUT`]) never stalls the daemon's tick;
//! the tick and the REST API read only the slot a round publishes. Each probe
//! runs on a short helper thread and its answer and a shutdown share one
//! channel, so a shutdown preempts a probe instead of waiting it out.
//!
//! The gateway's stdout and stderr go to `~/.clauth/gateway.log`, never the
//! daemon's own stderr: shunt logs every request at `info`, which would flood
//! `daemon.log`'s size cap.

use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::log_rotate::{LOG_KEEP_BYTES, LOG_MAX_BYTES, rotate_log_if_large};
use super::probe::{DaemonLock, REPLACE_WAIT, terminate_pid};
use crate::gateway::{
    GatewayEnv, GatewayRecord, HEALTH_PROBE_TIMEOUT, Health, NotToml, VERSION_FLOOR,
    check_version_floor, gateway_bind, gateway_cwd, gateway_env, gateway_shutdown_timeout,
    probe_health,
};
use crate::lockorder::{RankedMutex, rank};
use crate::logline::logline;
use crate::profile::{atomic_write_600, clauth_dir, open_append_600};
use crate::usage::{epoch_secs_to_iso, now_ms};

/// The run loop's round length: after a round it waits out the rest of this
/// since the round's tick, so a `starting` gateway is probed at most once a
/// second, a state change shows within one feed write, and an exit is noticed
/// within one.
const SUPERVISE_POLL: Duration = Duration::from_secs(1);

/// How often a running gateway, a foreign answerer on its port, or a start
/// that could not spawn is looked at again. A healthy probe is a loopback
/// round trip of milliseconds; 5 s shows an outage within five feed writes
/// without probing on every tick.
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

/// How long a spawned gateway may stay silent before it reads `unhealthy`.
/// shunt 0.49.1 answered its first `/health` 8 to 28 ms after spawn (measured
/// 2026-09-27, 10 runs, median 8 ms), so 10 s is past any start a loaded box
/// stretches that to.
const STARTUP_GRACE: Duration = Duration::from_secs(10);

/// The first restart's delay, doubled per consecutive crash up to
/// [`BACKOFF_CAP`]: one daemon tick, about 35 times shunt's slowest measured
/// start, so a gateway dying at startup is respawned at most once a second
/// before the doubling spaces it out.
const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The longest restart delay: a crash-looping gateway (a config shunt refuses
/// at load) costs one spawn a minute, and a fixed config is picked up within
/// one.
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// A gateway that stayed healthy this long restarts from [`BACKOFF_BASE`]: its
/// crash is already spaced as far as the capped backoff would space it, so the
/// reset can never produce a tighter loop than the cap.
const BACKOFF_RESET_AFTER: Duration = BACKOFF_CAP;

/// shunt's fixed wait for already-started blocking work after its drain
/// (`BLOCKING_SHUTDOWN_GRACE`, shunt `main.rs`).
const SHUNT_BLOCKING_GRACE: Duration = Duration::from_secs(5);

/// Past shunt's drain and its blocking grace: the process exit and the
/// telemetry flush shunt runs after the drain.
const STOP_MARGIN: Duration = Duration::from_secs(5);

/// How long a daemon exiting on a signal waits for its gateway before it exits
/// anyway. `--replace` escalates to SIGKILL once [`REPLACE_WAIT`] passes with
/// the singleton flock still held, so the stop gets that window less 1 s for
/// the release to be seen at `--replace`'s 50 ms poll. A `/health` probe does
/// not eat into it: a shutdown preempts a probe, so the budget bounds only the
/// child's exit. A gateway still draining past it is left running with its
/// stop deadline in the child marker; shunt bounds its own drain, and the next
/// daemon start waits out the rest of the deadline and kills it there.
pub(crate) const DAEMON_STOP_BUDGET: Duration = REPLACE_WAIT.saturating_sub(Duration::from_secs(1));

/// How often a stop waiting inside [`DAEMON_STOP_BUDGET`] polls the child.
const STOP_POLL: Duration = Duration::from_millis(50);

const CHILD_MARKER_FILE: &str = "gateway-child.json";

const LOG_FILE: &str = "gateway.log";

// ── the slot ────────────────────────────────────────────────────────────────

/// The managed gateway's state, a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GatewayState {
    /// No gateway record: nothing is adopted.
    Absent,
    /// The record's `disabled` flag is on.
    Disabled,
    /// The adopted config is gone from disk.
    NoConfig,
    /// The record names a YAML config, which clauth cannot edit in place.
    YamlRefused,
    /// The record, its env file or its bind cannot be read into a spawn, or
    /// the gateway's log cannot be opened; `reason` says which.
    Misconfigured,
    /// The shunt binary is not there; `binary` names what was looked up.
    BinaryMissing,
    /// Something clauth did not spawn answers on the bind port, so none is
    /// started beside it; re-probed until it leaves.
    Foreign,
    /// Spawned, or about to be, with no `/health` answer yet.
    Starting,
    Healthy,
    /// Running, but `/health` does not answer; never killed for it.
    Unhealthy,
    /// It reported a version below the floor, so clauth stopped it and holds
    /// off until the record or the binary changes.
    BelowFloor,
    /// It exited; the next spawn waits out the backoff.
    Restarting,
    /// clauth asked a gateway it spawned to stop and waits for it to exit.
    Stopping,
    /// Built without the supervisor: the single-shot `clauth status --json`,
    /// a feed republished while no daemon runs, or a live daemon whose
    /// supervisor thread failed to spawn (so it published the record-only
    /// slot once and never steps).
    Unobserved,
}

/// What answered `/health` on the bind port of a [`GatewayState::Foreign`]
/// gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Answerer {
    /// A shunt-shaped answer; `version` carries what it reported.
    Shunt,
    /// An HTTP answer that is not shunt's `/health`.
    NotShunt,
    /// A listener that took the connection and never answered.
    NoAnswer,
}

/// How a gateway process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct ExitReport {
    /// The exit code, `null` for a process a signal ended.
    #[schema(required = true)]
    pub(crate) code: Option<i32>,
    /// The signal that ended it, `null` for an exit code (and always on
    /// Windows).
    #[schema(required = true)]
    pub(crate) signal: Option<i32>,
}

impl From<ExitStatus> for ExitReport {
    fn from(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt as _;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }
}

impl std::fmt::Display for ExitReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(f, "exit {code}"),
            (None, Some(signal)) => write!(f, "signal {signal}"),
            (None, None) => f.write_str("no exit status"),
        }
    }
}

/// The `gateway` slot: `status.json`'s top-level object and the body of
/// `GET /api/v1/gateway`. Paths, a port, a pid, versions and states only:
/// never an env value, a token or a file's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct GatewaySlot {
    pub(crate) state: GatewayState,
    /// The adopted config, `null` with no record.
    #[schema(required = true)]
    pub(crate) config: Option<String>,
    /// The binary the gateway runs as: the record's path, else `shunt`,
    /// looked up on `PATH`.
    #[schema(required = true)]
    pub(crate) binary: Option<String>,
    /// The port the gateway binds, or the port a foreign answerer holds.
    #[schema(required = true)]
    pub(crate) port: Option<u16>,
    /// The pid of the gateway clauth spawned, while one runs.
    #[schema(required = true)]
    pub(crate) pid: Option<u32>,
    /// The version `/health` reported: clauth's gateway's, or a shunt-shaped
    /// foreign answerer's.
    #[schema(required = true)]
    pub(crate) version: Option<String>,
    /// What answered on the port of a `foreign` gateway.
    #[schema(required = true)]
    pub(crate) answerer: Option<Answerer>,
    /// The oldest shunt clauth supervises.
    pub(crate) floor: String,
    /// Restarts after an exit clauth did not ask for, since the daemon
    /// started.
    pub(crate) restarts: u32,
    /// How the last gateway process ended, `null` before any has.
    #[schema(required = true)]
    pub(crate) last_exit: Option<ExitReport>,
    /// Why a `misconfigured` gateway cannot start.
    #[schema(required = true)]
    pub(crate) reason: Option<String>,
    /// ISO-8601 UTC stamp of when `state` last changed; `null` without a
    /// supervisor.
    #[schema(required = true)]
    pub(crate) since: Option<String>,
}

impl GatewaySlot {
    /// `state` with every other field empty.
    fn of(state: GatewayState) -> Self {
        Self {
            state,
            config: None,
            binary: None,
            port: None,
            pid: None,
            version: None,
            answerer: None,
            floor: VERSION_FLOOR.to_string(),
            restarts: 0,
            last_exit: None,
            reason: None,
            since: None,
        }
    }

    /// `state` naming `record`'s config and binary.
    fn for_record(state: GatewayState, record: &GatewayRecord) -> Self {
        Self {
            config: Some(record.config().display().to_string()),
            binary: Some(record.shunt_binary().display().to_string()),
            ..Self::of(state)
        }
    }
}

/// The slot the supervisor publishes, `None` until its first publish.
pub(crate) type GatewayHandle = Arc<RankedMutex<Option<GatewaySlot>, rank::GatewayPublished>>;

pub(crate) fn new_handle() -> GatewayHandle {
    Arc::new(RankedMutex::new(None))
}

/// What the supervisor last published.
pub(crate) fn published(handle: &GatewayHandle) -> Option<GatewaySlot> {
    match handle.lock() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// The slot a body publishes: the supervisor's, else what the record alone
/// says.
pub(crate) fn slot_or_record(published: Option<&GatewaySlot>) -> GatewaySlot {
    published.cloned().unwrap_or_else(unsupervised_slot)
}

/// The slot with no supervisor to ask: the record's own verdict, or
/// `unobserved` for a record the gateway would run on.
pub(crate) fn unsupervised_slot() -> GatewaySlot {
    match read_intent() {
        Intent::Idle(slot) => slot,
        Intent::Run(record) => GatewaySlot::for_record(GatewayState::Unobserved, &record),
    }
}

// ── the record's verdict ────────────────────────────────────────────────────

enum Intent {
    /// Run nothing, or the record cannot be read; the slot says which.
    Idle(GatewaySlot),
    Run(GatewayRecord),
}

fn read_intent() -> Intent {
    let record = match GatewayRecord::load() {
        Ok(Some(record)) => record,
        Ok(None) => return Intent::Idle(GatewaySlot::of(GatewayState::Absent)),
        Err(e) => {
            if let Some(yaml) = e.chain().find_map(|cause| cause.downcast_ref::<NotToml>()) {
                return Intent::Idle(GatewaySlot {
                    config: Some(yaml.path.display().to_string()),
                    ..GatewaySlot::of(GatewayState::YamlRefused)
                });
            }
            return Intent::Idle(GatewaySlot {
                reason: Some(format!("{e:#}")),
                ..GatewaySlot::of(GatewayState::Misconfigured)
            });
        }
    };
    if record.disabled {
        return Intent::Idle(GatewaySlot::for_record(GatewayState::Disabled, &record));
    }
    match record.config().try_exists() {
        Ok(true) => Intent::Run(record),
        Ok(false) => Intent::Idle(GatewaySlot::for_record(GatewayState::NoConfig, &record)),
        Err(e) => Intent::Idle(GatewaySlot {
            reason: Some(format!("cannot inspect {}: {e}", record.config().display())),
            ..GatewaySlot::for_record(GatewayState::Misconfigured, &record)
        }),
    }
}

/// What a below-floor hold waits to see change: the record, and the binary it
/// resolves to with that file's mtime.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    record: GatewayRecord,
    binary: Option<PathBuf>,
    mtime: Option<SystemTime>,
}

impl Identity {
    fn of(record: &GatewayRecord) -> Self {
        let binary = match &record.binary {
            Some(path) => Some(path.clone()),
            None => crate::plugin_probe::on_path("shunt"),
        };
        let mtime = binary.as_deref().and_then(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        });
        Self {
            record: record.clone(),
            binary,
            mtime,
        }
    }
}

// ── the supervisor ──────────────────────────────────────────────────────────

/// One instant as the supervisor reads it: the monotonic clock for its own
/// deadlines, the wall clock for the slot's stamp and for the child marker's
/// deadline, which another daemon reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tick {
    pub(crate) at: Instant,
    pub(crate) wall_ms: u64,
}

impl Tick {
    pub(crate) fn now() -> Self {
        Self {
            at: Instant::now(),
            wall_ms: now_ms(),
        }
    }

    /// `self` moved `by` on both clocks.
    #[cfg(all(test, unix))]
    pub(crate) fn after(self, by: Duration) -> Self {
        Self {
            at: self.at + by,
            wall_ms: self.wall_ms + by.as_millis() as u64,
        }
    }
}

/// The gateway clauth spawned.
struct Running {
    child: Child,
    pid: u32,
    /// The process start time recorded beside the pid.
    start: Option<String>,
    port: u16,
    probe: SocketAddr,
    spawned: Instant,
    healthy_since: Option<Instant>,
    next_probe: Instant,
    version: Option<String>,
    stop_bound: Duration,
    identity: Identity,
    stop: Option<Stop>,
}

impl Running {
    fn slot(&self, state: GatewayState) -> GatewaySlot {
        GatewaySlot {
            port: Some(self.port),
            pid: Some(self.pid),
            version: self.version.clone(),
            ..GatewaySlot::for_record(state, &self.identity.record)
        }
    }

    fn marker(&self, stop_deadline_ms: Option<u64>) -> ChildMarker {
        ChildMarker {
            pid: self.pid,
            start: self.start.clone(),
            stop_bound_secs: self.stop_bound.as_secs(),
            stop_deadline_ms,
        }
    }
}

/// A SIGTERM sent: when to kill, and what the gateway's exit leads to.
struct Stop {
    deadline: Instant,
    killed: bool,
    then: AfterStop,
}

enum AfterStop {
    /// The record says run nothing; publish its slot.
    Idle(GatewaySlot),
    /// It reported this version below the floor; hold until something
    /// changes.
    BelowFloor(String),
    /// The record's spawn inputs changed; spawn it afresh on them at once.
    Respawn,
    /// The daemon is exiting.
    Shutdown,
}

/// The one-time check for a gateway a previous daemon left running.
enum Orphan {
    Unchecked,
    Stopping {
        pid: u32,
        start: String,
        deadline_ms: u64,
        killed: bool,
    },
    Done,
}

/// The supervision state machine, stepped by the `clauth-gateway` thread (or
/// by a test, with an injected [`Tick`]).
pub(crate) struct Supervisor {
    handle: GatewayHandle,
    slot: GatewaySlot,
    child: Option<Running>,
    orphan: Orphan,
    restarts: u32,
    last_exit: Option<ExitReport>,
    /// Consecutive crashes, the backoff's exponent.
    crashes: u32,
    restart_at: Option<Instant>,
    /// When a start that did not spawn (foreign, binary missing,
    /// misconfigured) is tried again.
    retry_at: Option<Instant>,
    /// The identity a below-floor gateway was refused under.
    held: Option<Identity>,
    /// The env file and its skipped line numbers last logged (S-2), so a
    /// repeated skip of the same file's lines does not log again; a spawn
    /// that skips nothing clears it.
    last_skipped: Option<(PathBuf, Vec<usize>)>,
}

/// The probe a [`Supervisor::tend`] round still wants before it is complete.
enum StepWants {
    /// The running child's `/health` at this address.
    Child { tick: Tick, addr: SocketAddr },
    /// The port's `/health` before a spawn, with everything the spawn needs
    /// once the port reads silent.
    Foreign(Box<ForeignWants>),
}

/// What a pre-spawn probe carries into the spawn that follows a silent read.
struct ForeignWants {
    tick: Tick,
    record: GatewayRecord,
    env: GatewayEnv,
    port: u16,
    log: File,
    addr: SocketAddr,
    identity: Identity,
}

impl StepWants {
    fn addr(&self) -> SocketAddr {
        match self {
            StepWants::Child { addr, .. } => *addr,
            StepWants::Foreign(wants) => wants.addr,
        }
    }
}

impl Supervisor {
    /// A supervisor over `handle`, which it publishes the record's slot into
    /// at once, before any probe or spawn.
    pub(crate) fn new(handle: GatewayHandle) -> Self {
        let slot = match read_intent() {
            Intent::Idle(slot) => slot,
            Intent::Run(record) => GatewaySlot::for_record(GatewayState::Starting, &record),
        };
        let supervisor = Self {
            handle,
            slot,
            child: None,
            orphan: Orphan::Unchecked,
            restarts: 0,
            last_exit: None,
            crashes: 0,
            restart_at: None,
            retry_at: None,
            held: None,
            last_skipped: None,
        };
        supervisor.publish();
        supervisor
    }

    /// One round up to its first probe: cap the gateway's log, read the
    /// record, tend the gateway. A gateway that exits in this round is
    /// followed by the idle round at once, so a stop asked for by a record
    /// change spawns the fresh gateway in the same step. Returns the probe
    /// the round still wants ([`run`] performs it on a helper thread so a
    /// shutdown can preempt it; [`step`] performs it inline), or `None` when
    /// the round is complete barring the publish.
    fn tend(&mut self, tick: Tick) -> Option<StepWants> {
        trim_log();
        let intent = read_intent();
        if self.child.is_some() {
            let wants = self.tend_child(tick, &intent);
            if wants.is_none() && self.child.is_none() {
                return self.tend_idle(tick, intent);
            }
            return wants;
        }
        self.tend_idle(tick, intent)
    }

    /// One round with every probe performed inline: the synchronous form the
    /// tests drive and the fallback for a probe thread that cannot spawn.
    #[cfg(test)]
    pub(crate) fn step(&mut self, tick: Tick) {
        if let Some(want) = self.tend(tick) {
            let addr = want.addr();
            let answer = run_probe(addr);
            self.apply(want, answer);
        }
        self.publish();
    }

    /// Apply a probe's answer to the round that asked for it.
    fn apply(&mut self, want: StepWants, answer: Result<Health>) {
        match want {
            StepWants::Child { tick, addr } => self.apply_child_probe(tick, addr, answer),
            StepWants::Foreign(wants) => self.apply_foreign_probe(*wants, answer),
        }
    }

    /// Stop the gateway for a daemon that is exiting: SIGTERM (unless a stop
    /// is already underway, since a second SIGTERM makes shunt skip its
    /// drain), then wait until `deadline`. A gateway still running then is
    /// left draining, its deadline in the child marker for the next start.
    pub(crate) fn shutdown(&mut self, deadline: Instant) {
        // The probe seam holds or panics the supervisor thread here, so
        // `stop`'s missed-budget and panic paths are reachable in a test.
        #[cfg(all(test, unix))]
        let _seam = probe_seam::hold();
        self.begin_stop(AfterStop::Shutdown, Tick::now());
        let Some(running) = self.child.as_mut() else {
            return;
        };
        loop {
            match running.child.try_wait() {
                Ok(Some(status)) => {
                    let report = ExitReport::from(status);
                    logline!(
                        "clauth daemon: the shunt gateway (pid {}) stopped ({report})",
                        running.pid
                    );
                    remove_marker();
                    self.child = None;
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    logline!(
                        "clauth daemon: cannot wait on the shunt gateway (pid {}): {e}",
                        running.pid
                    );
                    return;
                }
            }
            let now = Instant::now();
            if let Some(stop) = running.stop.as_mut()
                && now >= stop.deadline
                && !stop.killed
            {
                let _ = running.child.kill();
                stop.killed = true;
            }
            if now >= deadline {
                logline!(
                    "clauth daemon: the shunt gateway (pid {}) is still stopping; the next daemon start finishes the stop",
                    running.pid
                );
                return;
            }
            std::thread::sleep(STOP_POLL);
        }
    }

    fn tend_child(&mut self, tick: Tick, intent: &Intent) -> Option<StepWants> {
        let running = self.child.as_mut()?;
        match running.child.try_wait() {
            Ok(Some(status)) => {
                self.exited(status, tick);
                return None;
            }
            Ok(None) => {}
            Err(e) => logline!(
                "clauth daemon: cannot wait on the shunt gateway (pid {}): {e}",
                running.pid
            ),
        }
        if let Some(stop) = running.stop.as_mut() {
            if tick.at >= stop.deadline && !stop.killed {
                logline!(
                    "clauth daemon: the shunt gateway (pid {}) did not exit within {}s of SIGTERM; killing it",
                    running.pid,
                    running.stop_bound.as_secs()
                );
                let _ = running.child.kill();
                stop.killed = true;
            }
            return None;
        }
        match intent {
            Intent::Idle(slot)
                if matches!(
                    slot.state,
                    GatewayState::Absent | GatewayState::Disabled | GatewayState::NoConfig
                ) =>
            {
                self.begin_stop(AfterStop::Idle(slot.clone()), tick);
                return None;
            }
            // An unreadable or YAML record keeps a running gateway: it was
            // started under a record that read, and that record's intent is
            // the last one clauth knows.
            Intent::Idle(_) => {}
            Intent::Run(record)
                if spawn_inputs(record) != spawn_inputs(&running.identity.record) =>
            {
                self.begin_stop(AfterStop::Respawn, tick);
                return None;
            }
            Intent::Run(_) => {}
        }
        if tick.at >= running.next_probe {
            Some(StepWants::Child {
                tick,
                addr: running.probe,
            })
        } else {
            None
        }
    }

    fn apply_child_probe(&mut self, tick: Tick, addr: SocketAddr, answer: Result<Health>) {
        let Some(running) = self.child.as_mut() else {
            return;
        };
        if running.probe != addr {
            // The gateway changed while the probe ran; drop the stale answer.
            return;
        }
        if let Ok(Health::Shunt { version }) = answer {
            running.version = Some(version.clone());
            running.next_probe = tick.at + HEALTH_INTERVAL;
            if let Err(refusal) = check_version_floor(&version) {
                logline!(
                    "clauth daemon: {refusal}; stopping the gateway it started (pid {})",
                    running.pid
                );
                return self.begin_stop(AfterStop::BelowFloor(refusal.read), tick);
            }
            running.healthy_since.get_or_insert(tick.at);
            let slot = running.slot(GatewayState::Healthy);
            return self.set(slot, tick);
        }
        let state = if running.healthy_since.is_none() && tick.at < running.spawned + STARTUP_GRACE
        {
            running.next_probe = tick.at;
            GatewayState::Starting
        } else {
            running.next_probe = tick.at + HEALTH_INTERVAL;
            GatewayState::Unhealthy
        };
        let slot = running.slot(state);
        self.set(slot, tick);
    }

    fn begin_stop(&mut self, then: AfterStop, tick: Tick) {
        let Some(running) = self.child.as_mut() else {
            return;
        };
        if running.stop.is_some() {
            return;
        }
        let why = match &then {
            AfterStop::Idle(slot) => format!("the gateway is now {:?}", slot.state),
            AfterStop::BelowFloor(read) => format!("it reported shunt {read}"),
            AfterStop::Respawn => "its config, binary or env file changed".to_string(),
            AfterStop::Shutdown => "the daemon is exiting".to_string(),
        };
        logline!(
            "clauth daemon: stopping the shunt gateway (pid {}): {why}",
            running.pid
        );
        terminate_pid(running.pid, false);
        let deadline_ms = tick.wall_ms + running.stop_bound.as_millis() as u64;
        if let Err(e) = write_marker(&running.marker(Some(deadline_ms))) {
            logline!("clauth daemon: failed to record the gateway's stop deadline: {e:#}");
        }
        running.stop = Some(Stop {
            deadline: tick.at + running.stop_bound,
            killed: false,
            then,
        });
        let slot = running.slot(GatewayState::Stopping);
        self.set(slot, tick);
    }

    fn exited(&mut self, status: ExitStatus, tick: Tick) {
        let Some(running) = self.child.take() else {
            return;
        };
        remove_marker();
        let report = ExitReport::from(status);
        self.last_exit = Some(report);
        let port = Some(running.port);
        match running.stop.map(|stop| stop.then) {
            Some(AfterStop::Idle(slot)) => {
                logline!(
                    "clauth daemon: the shunt gateway (pid {}) stopped ({report})",
                    running.pid
                );
                self.set(slot, tick);
            }
            Some(AfterStop::BelowFloor(read)) => {
                let slot = GatewaySlot {
                    port,
                    version: Some(read),
                    ..GatewaySlot::for_record(GatewayState::BelowFloor, &running.identity.record)
                };
                self.held = Some(running.identity);
                self.set(slot, tick);
            }
            Some(AfterStop::Respawn) => {
                logline!(
                    "clauth daemon: the shunt gateway (pid {}) stopped ({report}); starting it on the changed record",
                    running.pid
                );
                // The backoff paces crashes of one set of spawn inputs; the
                // new ones owe none of it.
                self.crashes = 0;
            }
            Some(AfterStop::Shutdown) => {}
            None => {
                if running.healthy_since.is_some_and(|since| {
                    tick.at.saturating_duration_since(since) >= BACKOFF_RESET_AFTER
                }) {
                    self.crashes = 0;
                }
                let delay = backoff(self.crashes);
                self.crashes = self.crashes.saturating_add(1);
                self.restarts = self.restarts.saturating_add(1);
                self.restart_at = Some(tick.at + delay);
                logline!(
                    "clauth daemon: the shunt gateway (pid {}) exited ({report}); restarting in {}s",
                    running.pid,
                    delay.as_secs()
                );
                let slot = GatewaySlot {
                    port,
                    ..GatewaySlot::for_record(GatewayState::Restarting, &running.identity.record)
                };
                self.set(slot, tick);
            }
        }
    }

    fn tend_idle(&mut self, tick: Tick, intent: Intent) -> Option<StepWants> {
        if !self.reclaim(tick) {
            return None;
        }
        let record = match intent {
            Intent::Idle(slot) => {
                self.restart_at = None;
                self.retry_at = None;
                self.held = None;
                self.set(slot, tick);
                return None;
            }
            Intent::Run(record) => record,
        };
        let identity = Identity::of(&record);
        if let Some(held) = &self.held {
            if *held == identity {
                return None;
            }
            self.held = None;
        }
        if self.restart_at.is_some_and(|at| tick.at < at)
            || self.retry_at.is_some_and(|at| tick.at < at)
        {
            return None;
        }
        self.restart_at = None;
        self.retry_at = Some(tick.at + HEALTH_INTERVAL);
        let (env, probe, port, log) = match prepare(&record) {
            Ok(prepared) => prepared,
            Err(e) => {
                let reason = format!("{e:#}");
                if self.slot.reason.as_deref() != Some(reason.as_str()) {
                    logline!("clauth daemon: cannot start the shunt gateway: {reason}");
                }
                let slot = GatewaySlot {
                    reason: Some(reason),
                    ..GatewaySlot::for_record(GatewayState::Misconfigured, &record)
                };
                self.set(slot, tick);
                return None;
            }
        };
        Some(StepWants::Foreign(Box::new(ForeignWants {
            tick,
            record,
            env,
            port,
            log,
            addr: probe,
            identity,
        })))
    }

    fn apply_foreign_probe(&mut self, wants: ForeignWants, answer: Result<Health>) {
        let ForeignWants {
            tick,
            record,
            env,
            port,
            log,
            addr,
            identity,
        } = wants;
        let foreign = match answer {
            Ok(Health::Silent(_)) => None,
            Ok(Health::Shunt { version }) => Some((Answerer::Shunt, Some(version))),
            Ok(Health::NotShunt { .. }) => Some((Answerer::NotShunt, None)),
            Err(_) => Some((Answerer::NoAnswer, None)),
        };
        if let Some((answerer, version)) = foreign {
            if self.slot.state != GatewayState::Foreign {
                logline!(
                    "clauth daemon: port {port} already answers /health ({}); not starting the managed gateway beside it",
                    version.as_deref().map_or_else(
                        || format!("{answerer:?}"),
                        |version| format!("shunt {version:?}")
                    )
                );
            }
            let slot = GatewaySlot {
                port: Some(port),
                version,
                answerer: Some(answerer),
                ..GatewaySlot::for_record(GatewayState::Foreign, &record)
            };
            return self.set(slot, tick);
        }
        let stop_bound = gateway_shutdown_timeout(&record, &env)
            .saturating_add(SHUNT_BLOCKING_GRACE)
            .saturating_add(STOP_MARGIN);
        self.log_skipped(&record, &env);
        let child = match spawn(&record, &env, log) {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if self.slot.state != GatewayState::BinaryMissing {
                    logline!(
                        "clauth daemon: cannot start the shunt gateway: {} not found; install shunt or point the gateway at its binary",
                        record.shunt_binary().display()
                    );
                }
                let slot = GatewaySlot {
                    port: Some(port),
                    ..GatewaySlot::for_record(GatewayState::BinaryMissing, &record)
                };
                self.set(slot, tick);
                return;
            }
            Err(e) => {
                let reason = format!("cannot run {}: {e}", record.shunt_binary().display());
                if self.slot.reason.as_deref() != Some(reason.as_str()) {
                    logline!("clauth daemon: cannot start the shunt gateway: {reason}");
                }
                let slot = GatewaySlot {
                    port: Some(port),
                    reason: Some(reason),
                    ..GatewaySlot::for_record(GatewayState::Misconfigured, &record)
                };
                self.set(slot, tick);
                return;
            }
        };
        self.retry_at = None;
        let pid = child.id();
        let running = Running {
            child,
            pid,
            start: process_start_time(pid),
            port,
            probe: addr,
            spawned: tick.at,
            healthy_since: None,
            next_probe: tick.at,
            version: None,
            stop_bound,
            identity,
            stop: None,
        };
        if let Err(e) = write_marker(&running.marker(None)) {
            logline!("clauth daemon: failed to record the gateway's pid: {e:#}");
        }
        logline!("clauth daemon: started the shunt gateway (pid {pid}) on port {port}");
        let slot = running.slot(GatewayState::Starting);
        self.child = Some(running);
        self.set(slot, tick);
    }

    /// Say once what the env file skipped, before the spawn that runs without
    /// those assignments: the file's path and the line numbers, never a line's
    /// text (a line may hold a secret).
    fn log_skipped(&mut self, record: &GatewayRecord, env: &GatewayEnv) {
        let skipped = env.skipped_lines();
        let Some(env_file) = record.env_file.as_deref() else {
            // No env file, so nothing is skipped; a spawn that skips nothing
            // clears the memo.
            self.last_skipped = None;
            return;
        };
        if skipped.is_empty() {
            self.last_skipped = None;
            return;
        }
        let memo = (env_file.to_path_buf(), skipped.to_vec());
        if self.last_skipped.as_ref() == Some(&memo) {
            return;
        }
        let numbers = skipped
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        logline!(
            "clauth daemon: the gateway's env file {} assigns nothing on line(s) {numbers} (systemd skips such lines too); the gateway runs without them",
            env_file.display()
        );
        self.last_skipped = Some(memo);
    }

    /// Stop the gateway a previous daemon left running, once, before this
    /// supervisor spawns its own. `true` once nothing is left to stop.
    fn reclaim(&mut self, tick: Tick) -> bool {
        match &mut self.orphan {
            Orphan::Done => true,
            Orphan::Unchecked => {
                let Some(left) = stop_left_behind(tick.wall_ms) else {
                    self.orphan = Orphan::Done;
                    return true;
                };
                self.orphan = Orphan::Stopping {
                    pid: left.pid,
                    start: left.start,
                    deadline_ms: left.deadline_ms,
                    killed: false,
                };
                self.set(
                    GatewaySlot {
                        pid: Some(left.pid),
                        ..GatewaySlot::of(GatewayState::Stopping)
                    },
                    tick,
                );
                false
            }
            Orphan::Stopping {
                pid,
                start,
                deadline_ms,
                killed,
            } => {
                if process_start_time(*pid).as_ref() != Some(start) {
                    logline!(
                        "clauth daemon: the shunt gateway a previous daemon left running (pid {pid}) is gone"
                    );
                    remove_marker();
                    self.orphan = Orphan::Done;
                    return true;
                }
                if tick.wall_ms >= *deadline_ms && !*killed {
                    logline!(
                        "clauth daemon: the shunt gateway a previous daemon left running (pid {pid}) outlived its stop deadline; killing it"
                    );
                    terminate_pid(*pid, true);
                    *killed = true;
                }
                false
            }
        }
    }
}

/// A gateway a previous daemon left running, with its stop asked for.
struct LeftBehind {
    pid: u32,
    start: String,
    deadline_ms: u64,
}

/// Ask the gateway the child marker names to stop, at most once, and say
/// when that stop runs out; `None` when no live gateway is left (an unreadable
/// or stale marker is dropped). Only a live process started when the marker
/// says is the gateway a daemon spawned; any other pid, recycled or not, is
/// never signalled.
fn stop_left_behind(now_ms: u64) -> Option<LeftBehind> {
    let marker = match read_marker() {
        Ok(Some(marker)) => marker,
        Ok(None) => return None,
        Err(e) => {
            logline!("clauth daemon: ignoring an unreadable gateway child marker: {e:#}");
            remove_marker();
            return None;
        }
    };
    let start = match marker.start.clone() {
        Some(start) if process_start_time(marker.pid).as_ref() == Some(&start) => start,
        _ => {
            remove_marker();
            return None;
        }
    };
    let deadline_ms = match marker.stop_deadline_ms {
        // A stop already asked for: a second SIGTERM makes shunt skip its
        // drain, so this only waits out the deadline.
        Some(deadline_ms) => deadline_ms,
        None => {
            logline!(
                "clauth daemon: stopping the shunt gateway a previous daemon left running (pid {})",
                marker.pid
            );
            terminate_pid(marker.pid, false);
            let deadline_ms = now_ms.saturating_add(marker.stop_bound_secs.saturating_mul(1000));
            let stopping = ChildMarker {
                stop_deadline_ms: Some(deadline_ms),
                ..marker.clone()
            };
            if let Err(e) = write_marker(&stopping) {
                logline!("clauth daemon: failed to record the gateway's stop deadline: {e:#}");
            }
            deadline_ms
        }
    };
    Some(LeftBehind {
        pid: marker.pid,
        start,
        deadline_ms,
    })
}

/// The TUI's `stop daemon`, once no daemon is left: stop the gateway the
/// stopped daemon could not (on Windows it ends by `taskkill /F` with no
/// chance to; on unix a SIGKILLed one never did) instead of leaving it to a
/// next daemon start that may never come. The stop is recorded in the marker
/// like the orphan rule's, so a later daemon still finishes a drain that
/// outlives its deadline.
pub(crate) fn stop_left_behind_gateway() {
    let _ = stop_left_behind(now_ms());
}

impl Supervisor {
    /// Replace the slot, keeping `since` while the state stays the same and
    /// carrying the supervisor's restart count and last exit.
    fn set(&mut self, slot: GatewaySlot, tick: Tick) {
        let since = if slot.state == self.slot.state && self.slot.since.is_some() {
            self.slot.since.clone()
        } else {
            Some(epoch_secs_to_iso((tick.wall_ms / 1000) as i64))
        };
        self.slot = GatewaySlot {
            restarts: self.restarts,
            last_exit: self.last_exit,
            since,
            ..slot
        };
    }

    fn publish(&self) {
        match self.handle.lock() {
            Ok(mut slot) => *slot = Some(self.slot.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(self.slot.clone()),
        }
    }

    /// Hand the child over without stopping it, the way a daemon that dies
    /// hard leaves it, for a test to pose the orphan a later daemon meets.
    #[cfg(all(test, unix))]
    pub(crate) fn abandon(&mut self) -> Option<Child> {
        self.child.take().map(|running| running.child)
    }

    /// Kill and reap the child, for a test's teardown.
    #[cfg(all(test, unix))]
    pub(crate) fn kill_for_test(&mut self) {
        if let Some(mut running) = self.child.take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}

/// The env, the probe address, the port and the log a spawn of `record`
/// uses.
fn prepare(record: &GatewayRecord) -> Result<(GatewayEnv, SocketAddr, u16, File)> {
    let env = gateway_env(record)?;
    let bind = gateway_bind(record, &env)?;
    let log = log_path()?;
    let log = open_append_600(&log).with_context(|| format!("failed to open {}", log.display()))?;
    Ok((env, bind.probe, bind.configured.port(), log))
}

/// What a running gateway's record may change under it and have it spawned
/// afresh on: its config, its binary and its env file, by value.
fn spawn_inputs(record: &GatewayRecord) -> (&Path, Option<&Path>, Option<&Path>) {
    (
        record.config(),
        record.binary.as_deref(),
        record.env_file.as_deref(),
    )
}

/// The delay before the restart after `crashes` consecutive crashes.
fn backoff(crashes: u32) -> Duration {
    BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(crashes))
        .min(BACKOFF_CAP)
}

/// `<binary> run --config <config>` in the config's dir under the gateway's
/// env, the pair `shunt check` runs with, its stdout and stderr appended to
/// `log`. On unix the child leads its own process group, so a Ctrl-C at the
/// daemon's terminal never reaches it beside the one SIGTERM the daemon
/// sends: shunt reads a second signal as "skip the drain".
fn spawn(record: &GatewayRecord, env: &GatewayEnv, log: File) -> std::io::Result<Child> {
    let cwd = gateway_cwd(record).map_err(std::io::Error::other)?;
    let mut command = Command::new(record.shunt_binary());
    command
        .arg("run")
        .arg("--config")
        .arg(record.config())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    env.apply(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn()
}

// ── the gateway's log ───────────────────────────────────────────────────────

fn log_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(LOG_FILE))
}

/// Hold `~/.clauth/gateway.log` under `daemon.log`'s size cap, once a step:
/// the file takes shunt's per-request lines, so it passes the cap by at most
/// one step's output. The trim rewrites the file in place, which the
/// gateway's append-mode fds follow to the new end.
fn trim_log() {
    if let Ok(path) = log_path() {
        let _ = rotate_log_if_large(&path, LOG_MAX_BYTES, LOG_KEEP_BYTES);
    }
}

// ── the child marker ────────────────────────────────────────────────────────

/// `~/.clauth/gateway-child.json`: the gateway a daemon spawned, so the next
/// daemon can tell its own orphan from a stranger on the same pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChildMarker {
    pub(crate) pid: u32,
    /// The process start time ([`process_start_time`]); `None` where it could
    /// not be read, which no later check ever matches.
    pub(crate) start: Option<String>,
    /// The stop bound the gateway was spawned with.
    pub(crate) stop_bound_secs: u64,
    /// When a stop already asked for runs out, epoch ms.
    pub(crate) stop_deadline_ms: Option<u64>,
}

fn marker_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(CHILD_MARKER_FILE))
}

pub(crate) fn write_marker(marker: &ChildMarker) -> Result<()> {
    let path = marker_path()?;
    atomic_write_600(&path, serde_json::to_vec(marker)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

pub(crate) fn read_marker() -> Result<Option<ChildMarker>> {
    let path = marker_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn remove_marker() {
    let Ok(path) = marker_path() else { return };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => logline!("clauth daemon: failed to remove {}: {e}", path.display()),
    }
}

/// When `pid` started, as an opaque token two reads of one live process
/// agree on; `None` for a pid that is gone, a zombie, or unreadable.
///
/// Linux: field 22 of `/proc/<pid>/stat`, clock ticks since boot.
#[cfg(target_os = "linux")]
pub(crate) fn process_start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) is parenthesized and may hold spaces or
    // parens itself, so the fields are counted from its last `)`.
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        return None;
    }
    fields.nth(18).map(str::to_string)
}

/// macOS and the BSDs: `ps -o lstart`, the start time to the second.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn process_start_time(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let (stat, lstart) = text.trim().split_once(char::is_whitespace)?;
    if stat.starts_with('Z') {
        return None;
    }
    Some(lstart.trim().to_string())
}

/// Windows: the .NET process start time in UTC ticks, through the PowerShell
/// the TLS listener's FQDN lookup already runs.
#[cfg(windows)]
pub(crate) fn process_start_time(pid: u32) -> Option<String> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Process -Id {pid} -ErrorAction Stop).StartTime.ToUniversalTime().Ticks"),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let ticks = String::from_utf8(output.stdout).ok()?;
    let ticks = ticks.trim();
    (output.status.success() && !ticks.is_empty() && ticks.bytes().all(|b| b.is_ascii_digit()))
        .then(|| ticks.to_string())
}

// ── the thread ──────────────────────────────────────────────────────────────

/// The `clauth-gateway` thread and the way to end it.
pub(crate) struct SupervisorThread {
    commands: Option<Sender<RunMessage>>,
    thread: Option<JoinHandle<()>>,
}

/// What the supervisor thread's inbox carries: a probe's answer, or a
/// shutdown request. One channel, so a shutdown lands beside a probe's answer
/// and preempts it.
enum RunMessage {
    Probe(Result<Health>),
    Shutdown(ShutdownReq),
}

/// Stop the gateway by `deadline`, then answer on `done` and end the thread.
struct ShutdownReq {
    deadline: Instant,
    done: Sender<()>,
}

/// Run one `/health` probe, blocking on the test seam first when it is armed,
/// so a test can hold the supervisor inside a probe and prove a shutdown
/// preempts it.
fn run_probe(addr: SocketAddr) -> Result<Health> {
    #[cfg(all(test, unix))]
    probe_seam::count_probe();
    #[cfg(all(test, unix))]
    let _seam = probe_seam::hold();
    probe_health(addr)
}

/// Start supervising under the active daemon's singleton. The record's slot
/// is published before this returns; the first probe and spawn run on the
/// thread.
pub(crate) fn start(handle: GatewayHandle, _singleton: &DaemonLock) -> Result<SupervisorThread> {
    let supervisor = Supervisor::new(handle);
    let (commands, inbox) = mpsc::channel::<RunMessage>();
    let probe_sender = commands.clone();
    let thread = std::thread::Builder::new()
        .name("clauth-gateway".into())
        .spawn(move || run(supervisor, &inbox, probe_sender))
        .context("failed to spawn the gateway supervisor thread")?;
    Ok(SupervisorThread {
        commands: Some(commands),
        thread: Some(thread),
    })
}

/// The supervisor's rounds. The thread ends on a `Shutdown` alone: `probe`,
/// its own sender, keeps the inbox open for the thread's whole life, so the
/// `Disconnected` arms never run and only keep each match exhaustive.
fn run(mut supervisor: Supervisor, inbox: &Receiver<RunMessage>, probe: Sender<RunMessage>) {
    loop {
        let tick = Tick::now();
        let Some(want) = supervisor.tend(tick) else {
            supervisor.publish();
            match inbox.recv_timeout(SUPERVISE_POLL) {
                Ok(RunMessage::Shutdown(req)) => {
                    supervisor.shutdown(req.deadline);
                    let _ = req.done.send(());
                    return;
                }
                Ok(RunMessage::Probe(_)) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    supervisor.shutdown(Instant::now() + DAEMON_STOP_BUDGET);
                    return;
                }
            }
            continue;
        };
        // Run the probe on a short helper thread, so a shutdown landing while
        // it blocks preempts it instead of waiting the probe out. The helper
        // self-bounds at `HEALTH_PROBE_TIMEOUT`; on preemption it is left to
        // finish and its answer is dropped with the channel.
        let addr = want.addr();
        if let Err(e) = std::thread::Builder::new()
            .name("clauth-gateway-probe".into())
            .spawn({
                let sender = probe.clone();
                move || {
                    let _ = sender.send(RunMessage::Probe(run_probe(addr)));
                }
            })
        {
            // No thread left: probe inline so the state machine still advances.
            logline!("clauth daemon: cannot spawn the gateway probe thread ({e}); probing inline");
            supervisor.apply(want, run_probe(addr));
            supervisor.publish();
            continue;
        }
        // Wait for the probe's answer or a shutdown. The probe is bounded, so
        // this never waits longer than the probe plus one poll.
        let probe_deadline = Instant::now() + HEALTH_PROBE_TIMEOUT + STOP_POLL;
        loop {
            match inbox.recv_timeout(STOP_POLL) {
                Ok(RunMessage::Probe(answer)) => {
                    // A shutdown queued behind the answer still wins: no spawn
                    // may start once a stop was asked for.
                    match inbox.try_recv() {
                        Ok(RunMessage::Shutdown(req)) => {
                            supervisor.shutdown(req.deadline);
                            let _ = req.done.send(());
                            return;
                        }
                        Ok(RunMessage::Probe(_)) => {}
                        Err(_) => {}
                    }
                    supervisor.apply(want, answer);
                    supervisor.publish();
                    break;
                }
                Ok(RunMessage::Shutdown(req)) => {
                    supervisor.shutdown(req.deadline);
                    let _ = req.done.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= probe_deadline {
                        // A panicking helper never answered; move on, bounded.
                        supervisor.apply(
                            want,
                            Err(anyhow::anyhow!("the gateway probe did not answer")),
                        );
                        supervisor.publish();
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    supervisor.shutdown(Instant::now() + DAEMON_STOP_BUDGET);
                    return;
                }
            }
        }
        // Pace the round: a `starting` gateway is due again at once, so wait
        // out the rest of SUPERVISE_POLL since the round's tick before the
        // next probe; a shutdown still preempts, and a stray late answer is
        // dropped.
        if drain_until(&mut supervisor, inbox, tick.at + SUPERVISE_POLL) {
            return;
        }
    }
}

/// Wait on `inbox` until `deadline`, ending the thread on a shutdown request;
/// a stray probe answer is dropped. `true` ends the thread. The inbox never
/// disconnects here (`run` holds a sender), so its arm only keeps the match
/// exhaustive.
fn drain_until(
    supervisor: &mut Supervisor,
    inbox: &Receiver<RunMessage>,
    deadline: Instant,
) -> bool {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        match inbox.recv_timeout(deadline - now) {
            Ok(RunMessage::Shutdown(req)) => {
                supervisor.shutdown(req.deadline);
                let _ = req.done.send(());
                return true;
            }
            Ok(RunMessage::Probe(_)) => {}
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => {
                supervisor.shutdown(Instant::now() + DAEMON_STOP_BUDGET);
                return true;
            }
        }
    }
}

/// How a [`SupervisorThread`] stop ended.
enum StopOutcome {
    /// The thread answered (or had already ended) and was joined.
    Stopped,
    /// The thread ended in a panic.
    Panicked,
    /// The thread did not answer within the budget plus one poll: it is
    /// stuck, and is left running.
    MissedBudget,
}

impl StopOutcome {
    fn of_join(thread: JoinHandle<()>) -> Self {
        if thread.join().is_ok() {
            Self::Stopped
        } else {
            Self::Panicked
        }
    }
}

impl SupervisorThread {
    /// Stop the gateway within `budget` and end the thread. `false` when the
    /// thread did not answer within `budget` plus one poll (it is stuck) or
    /// ended in a panic; a gateway then left running is named by its child
    /// marker, the next daemon start finishes the stop, and the cause is
    /// logged.
    #[cfg(unix)]
    pub(crate) fn shutdown(mut self, budget: Duration) -> bool {
        match self.stop(budget) {
            StopOutcome::Stopped => true,
            StopOutcome::Panicked => {
                logline!(
                    "clauth daemon: the shunt gateway supervisor panicked; the next daemon start finishes the stop"
                );
                false
            }
            StopOutcome::MissedBudget => {
                logline!(
                    "clauth daemon: the shunt gateway did not stop within the {} s signal budget; the next daemon start finishes the stop",
                    budget.as_secs()
                );
                false
            }
        }
    }

    fn stop(&mut self, budget: Duration) -> StopOutcome {
        let (Some(commands), Some(thread)) = (self.commands.take(), self.thread.take()) else {
            return StopOutcome::Stopped;
        };
        let deadline = Instant::now() + budget;
        let (done, answered) = mpsc::channel();
        if commands
            .send(RunMessage::Shutdown(ShutdownReq { deadline, done }))
            .is_err()
        {
            return StopOutcome::of_join(thread);
        }
        // The thread preempts whatever it is doing (a probe included) to
        // answer, so its reply lands well inside the budget; the extra poll is
        // margin for the delivery. A dropped `done` is a thread that unwound
        // before answering, so the join returns at once.
        match answered.recv_timeout(budget.saturating_add(STOP_POLL)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => StopOutcome::of_join(thread),
            Err(RecvTimeoutError::Timeout) => StopOutcome::MissedBudget,
        }
    }
}

impl Drop for SupervisorThread {
    fn drop(&mut self) {
        self.stop(DAEMON_STOP_BUDGET);
    }
}

/// Whether `signal`'s disposition in this process is `SIG_IGN`: a daemon
/// started under `nohup` or a non-interactive shell's `&` inherited that, and
/// must keep surviving it rather than have the watcher turn it into a death.
#[cfg(unix)]
#[allow(unsafe_code)]
fn inherited_ignored(signal: libc::c_int) -> bool {
    // SAFETY: `signal` is one of `SIGTERM`/`SIGINT`/`SIGHUP`; a null `act`
    // asks the kernel to fill `old` with the current disposition and install
    // nothing. `old` is an owned, zeroed `sigaction`.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
        // Unreadable: treat as catchable, so the gateway still stops.
        return false;
    }
    old.sa_sigaction == libc::SIG_IGN
}

/// Stop the gateway before the daemon dies of SIGTERM, SIGINT or SIGHUP, then
/// die of that same signal, so a supervisor reads the exit it always read.
/// Returns the supervisor back when no watcher could take it; the caller
/// keeps it alive for the process's life either way.
#[cfg(unix)]
pub(crate) fn stop_on_signal(supervisor: SupervisorThread) -> Option<SupervisorThread> {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    // Watch only the signals this daemon did not inherit as ignored (nohup, a
    // non-interactive shell's `&`): signal-hook installs its handler over an
    // inherited SIG_IGN without chaining it, so watching an ignored signal
    // would turn one the daemon used to survive into a death.
    let watched: Vec<libc::c_int> = [SIGTERM, SIGINT, SIGHUP]
        .into_iter()
        .filter(|signal| !inherited_ignored(*signal))
        .collect();
    if watched.is_empty() {
        return Some(supervisor);
    }
    let mut signals = match signal_hook::iterator::Signals::new(watched) {
        Ok(signals) => signals,
        Err(e) => {
            logline!(
                "clauth daemon: cannot catch the stop signals ({e}); the shunt gateway outlives this daemon, and the next start stops it"
            );
            return Some(supervisor);
        }
    };
    let spawned = std::thread::Builder::new()
        .name("clauth-daemon-sig".into())
        .spawn(move || {
            let Some(signal) = signals.forever().next() else {
                return;
            };
            supervisor.shutdown(DAEMON_STOP_BUDGET);
            if let Err(e) = signal_hook::low_level::emulate_default_handler(signal) {
                logline!("clauth daemon: cannot re-raise signal {signal}: {e}");
            }
            std::process::exit(128 + signal);
        });
    if let Err(e) = spawned {
        logline!(
            "clauth daemon: failed to spawn the signal watcher ({e}); the shunt gateway was stopped"
        );
    }
    None
}

/// Windows delivers no SIGTERM to a console process: the daemon ends by
/// `taskkill /F`, the gateway outlives it, and the next start meets it as an
/// orphan and stops it there ([`Supervisor::reclaim`]), or the TUI's
/// `stop daemon` does once no daemon is left ([`stop_left_behind_gateway`]).
#[cfg(not(unix))]
pub(crate) fn stop_on_signal(supervisor: SupervisorThread) -> Option<SupervisorThread> {
    Some(supervisor)
}

/// A test-only stand-in for a wedged `/health` answerer: when armed, the next
/// probe or `Supervisor::shutdown` to reach it, whichever comes first, blocks
/// until released, so a test can hold the supervisor inside a probe and prove
/// a shutdown preempts it rather than waiting the probe out, or hold a
/// shutdown past its budget. Armed to panic, it panics that thread instead.
#[cfg(all(test, unix))]
#[expect(
    clippy::expect_used,
    reason = "the seam installs its channels together in `arm`, so a missing one is a test bug"
)]
mod probe_seam {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    static STATE: OnceLock<Mutex<State>> = OnceLock::new();

    /// How many probes this process has run, counted by the probe hook, for
    /// the run-loop pacing test to bound.
    static PROBES: AtomicUsize = AtomicUsize::new(0);

    /// Start counting probes from zero for the next test.
    pub fn reset_probes() {
        PROBES.store(0, Ordering::Relaxed);
    }

    /// The probes counted since the last reset.
    pub fn probes() -> usize {
        PROBES.load(Ordering::Relaxed)
    }

    /// Count one probe, from the probe hook.
    pub fn count_probe() {
        PROBES.fetch_add(1, Ordering::Relaxed);
    }

    struct State {
        armed: AtomicBool,
        entered: Option<Sender<()>>,
        release: Option<Receiver<()>>,
        finished: Option<Sender<()>>,
    }

    fn state() -> &'static Mutex<State> {
        STATE.get_or_init(|| {
            Mutex::new(State {
                armed: AtomicBool::new(false),
                entered: None,
                release: None,
                finished: None,
            })
        })
    }

    /// Set by [`arm_panic`]: the next probe or supervisor shutdown to reach
    /// the seam panics there.
    static PANIC_NEXT: AtomicBool = AtomicBool::new(false);

    /// Arm the seam to panic the next probe or `Supervisor::shutdown` to
    /// reach it, whichever comes first. The returned guard disarms it on
    /// drop, so an arm nothing consumed never reaches another test.
    pub fn arm_panic() -> PanicArm {
        PANIC_NEXT.store(true, Ordering::SeqCst);
        PanicArm
    }

    pub struct PanicArm;

    impl Drop for PanicArm {
        fn drop(&mut self) {
            PANIC_NEXT.store(false, Ordering::SeqCst);
        }
    }

    /// Arm the seam for the next probe or `Supervisor::shutdown` to reach it,
    /// whichever comes first, and return a handle that reports when it is
    /// entered and releases it.
    pub fn arm() -> Handle {
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (finished_tx, finished_rx) = channel();
        let mut st = state().lock().expect("the probe seam is not poisoned");
        assert!(
            !st.armed.load(Ordering::SeqCst),
            "a probe seam is already armed"
        );
        st.armed.store(true, Ordering::SeqCst);
        st.entered = Some(entered_tx);
        st.release = Some(release_rx);
        st.finished = Some(finished_tx);
        Handle {
            entered: entered_rx,
            release: Some(release_tx),
            finished: finished_rx,
        }
    }

    /// Panic the calling thread when the seam is armed to panic; hold it when
    /// the seam is armed: report entry, wait for the release, and return a
    /// guard whose drop reports the held work finished.
    pub fn hold() -> Option<Guard> {
        assert!(
            !PANIC_NEXT.swap(false, Ordering::SeqCst),
            "the probe seam panics the thread it holds, as armed"
        );
        let (entered, release, finished) = {
            let mut st = state().lock().expect("the probe seam is not poisoned");
            if !st.armed.swap(false, Ordering::SeqCst) {
                return None;
            }
            (
                st.entered
                    .take()
                    .expect("an armed seam has an entered channel"),
                st.release
                    .take()
                    .expect("an armed seam has a release channel"),
                st.finished
                    .take()
                    .expect("an armed seam has a finished channel"),
            )
        };
        let _ = entered.send(());
        let _ = release.recv();
        Some(Guard {
            finished: Some(finished),
        })
    }

    pub struct Guard {
        finished: Option<Sender<()>>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(finished) = self.finished.take() {
                let _ = finished.send(());
            }
        }
    }

    pub struct Handle {
        entered: Receiver<()>,
        release: Option<Sender<()>>,
        finished: Receiver<()>,
    }

    impl Handle {
        /// Wait until the next probe or shutdown has entered the seam and
        /// blocked.
        pub fn wait_entered(&self) {
            self.entered
                .recv_timeout(Duration::from_secs(10))
                .expect("the probe enters the seam");
        }

        /// Release the held probe or shutdown and wait for it to finish, up
        /// to a probe's own timeout plus a second, so no thread it held
        /// outlives the sandbox.
        pub fn release(mut self) {
            let _ = self.release.take().expect("released once").send(());
            self.finished
                .recv_timeout(crate::gateway::HEALTH_PROBE_TIMEOUT + Duration::from_secs(1))
                .expect("the released probe finishes within its bound");
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            // A test that panicked before releasing still lets the held probe
            // finish, so the probe thread ends before the sandbox drops.
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/inline/daemon_gateway.rs"]
mod tests;
