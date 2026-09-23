//! Read-only, point-in-time resource observations. Never execute declaration
//! text, infer cleanup authority, or treat a matching PID as a write fence.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::Serialize;

use super::store;

#[cfg(target_os = "linux")]
mod herdr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    #[cfg(target_os = "linux")]
    Matched,
    #[cfg(target_os = "linux")]
    Mismatch,
    #[cfg(target_os = "linux")]
    Missing,
    #[cfg(target_os = "linux")]
    Unavailable,
    Unsupported,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResourceObservation {
    schema: u32,
    task_id: String,
    generation: u64,
    resource_revision: u64,
    resource_id: String,
    kind: String,
    disposition: String,
    observed_at_ms: u64,
    identity_state: State,
    reason: &'static str,
    control_verified: bool,
    lifecycle_independence_verified: bool,
    handoff_ready: bool,
    limitations: Vec<&'static str>,
}

pub(crate) fn inspect(task: &str, resource: &str) -> Result<ResourceObservation> {
    let record = store::show(task)?;
    let declaration = record
        .resources
        .iter()
        .find(|entry| entry.id == resource)
        .ok_or_else(|| anyhow::anyhow!("resource is not registered for this task"))?;
    let (state, reason) = match declaration.kind.as_str() {
        "process" => inspect_process(&declaration.native_identity),
        "herdr_pane" => inspect_herdr(&declaration.native_identity),
        _ => (
            State::Unsupported,
            "resource kind has no verified identity adapter",
        ),
    };
    Ok(ResourceObservation {
        schema: 1,
        task_id: record.task_id,
        generation: record.generation,
        resource_revision: record.resource_revision,
        resource_id: declaration.id.clone(),
        kind: declaration.kind.clone(),
        disposition: declaration.disposition.clone(),
        observed_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()
            .map_err(|_| anyhow::anyhow!("observation timestamp overflow"))?,
        identity_state: state,
        reason,
        control_verified: false,
        lifecycle_independence_verified: false,
        handoff_ready: false,
        limitations: vec![
            "Point-in-time observation only; neither resource liveness nor the task revision is locked after this read.",
            "Matching identity does not prove exclusive control, successful reconnection, or survival after source shutdown.",
            "Process start time has tick resolution and survives exec; matching fields are not an unforgeable process-lifetime token.",
            "Missing means absent, hidden or no longer live in this process view; it does not prove termination or that descendants stopped.",
            "No resource was adopted or released; reconnect and cleanup descriptions are never executed.",
            "Filesystem/procfs reads are synchronous; the Herdr socket deadline cannot interrupt a stalled filesystem.",
        ],
    })
}

pub(crate) fn process_identity(pid: u32) -> Result<BTreeMap<String, String>> {
    if pid == 0 {
        bail!("process identity requires a positive PID");
    }
    observe_process(pid)
        .map_err(|_| anyhow::anyhow!("cannot observe a stable live local process identity"))
}

pub(crate) fn herdr_identity(
    socket: &std::path::Path,
    pane: &str,
) -> Result<BTreeMap<String, String>> {
    #[cfg(target_os = "linux")]
    {
        herdr::capture(socket, pane).map_err(|_| {
            anyhow::anyhow!("cannot observe a stable explicit local Herdr pane identity")
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket, pane);
        bail!("local Herdr resource identity observations require Linux");
    }
}

pub(crate) fn declared_identity(
    kind: &str,
    expected: &BTreeMap<String, String>,
) -> (State, &'static str) {
    match kind {
        "process" => inspect_process(expected),
        "herdr_pane" => inspect_herdr(expected),
        _ => (
            State::Unsupported,
            "resource kind has no verified identity adapter",
        ),
    }
}

fn inspect_herdr(expected: &BTreeMap<String, String>) -> (State, &'static str) {
    #[cfg(target_os = "linux")]
    {
        herdr::inspect(expected)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = expected;
        (
            State::Unsupported,
            "local Herdr resource observations require Linux",
        )
    }
}

#[cfg(not(target_os = "linux"))]
fn inspect_process(_: &BTreeMap<String, String>) -> (State, &'static str) {
    (
        State::Unsupported,
        "process observations require Linux procfs",
    )
}

#[cfg(target_os = "linux")]
fn inspect_process(expected: &BTreeMap<String, String>) -> (State, &'static str) {
    let Some(pid) = expected
        .get("pid")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
    else {
        return (
            State::Unavailable,
            "declared process identity is incomplete",
        );
    };
    // Free-form legacy host/start descriptions remain valid declarations, but
    // cannot be promoted into verified local process observations.
    let Some(host) = expected.get("host").filter(|v| valid_host(v)) else {
        return (
            State::Unsupported,
            "host identity is not a supported Linux process view",
        );
    };
    let Some(start) = expected.get("start_identity").filter(|v| decimal(v)) else {
        return (
            State::Unsupported,
            "start identity is not Linux starttime ticks",
        );
    };
    match process_host() {
        Ok(current) if current != *host => {
            return (
                State::Mismatch,
                "host boot, PID view or time namespace differs",
            );
        }
        Err(_) => {
            return (
                State::Unavailable,
                "local process view could not be observed",
            );
        }
        _ => {}
    }
    match observe_process(pid) {
        Ok(observed)
            if observed.get("host") == Some(host)
                && observed.get("start_identity") == Some(start) =>
        {
            (
                State::Matched,
                "local process identity fields matched at observation time",
            )
        }
        Ok(_) => (
            State::Mismatch,
            "process identity changed or PID was reused",
        ),
        Err(State::Missing) => (
            State::Missing,
            "process is not visible or is no longer live in this PID view",
        ),
        Err(_) => (
            State::Unavailable,
            "stable local process identity could not be observed",
        ),
    }
}

#[cfg(target_os = "linux")]
fn decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u64>().is_ok()
}

#[cfg(target_os = "linux")]
fn valid_host(value: &str) -> bool {
    let fields: Vec<_> = value.split(':').collect();
    if fields.len() != 6
        || fields[0] != "linux-v1"
        || fields[2..].iter().any(|field| !decimal(field))
    {
        return false;
    }
    let boot = fields[1];
    boot.len() == 36
        && boot.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

#[cfg(target_os = "linux")]
fn process_host() -> std::result::Result<String, State> {
    use std::os::unix::fs::MetadataExt;
    // NSpid is ordered from the procfs mount's PID view to the caller's own.
    // Exactly one ID equal to getpid proves those numeric views agree.
    // /proc/1/ns may be masked even when the caller's namespace is readable.
    verify_pid_view(
        &bounded_read("/proc/self/status", 16_384)?,
        std::process::id(),
    )?;
    let boot = bounded_read("/proc/sys/kernel/random/boot_id", 128)?;
    let boot = std::str::from_utf8(&boot)
        .map_err(|_| State::Unavailable)?
        .trim();
    let namespace = namespace_metadata("pid")?;
    // Linux adjusts field 22 by the reader's time-namespace offset. A different
    // time view must not be compared as though it shared the same clock.
    let time_namespace = namespace_metadata("time")?;
    let host = format!(
        "linux-v1:{boot}:{}:{}:{}:{}",
        namespace.dev(),
        namespace.ino(),
        time_namespace.dev(),
        time_namespace.ino()
    );
    if !valid_host(&host) {
        return Err(State::Unavailable);
    }
    Ok(host)
}

#[cfg(target_os = "linux")]
fn namespace_metadata(kind: &str) -> std::result::Result<std::fs::Metadata, State> {
    use std::os::unix::fs::MetadataExt;
    let path = format!("/proc/self/ns/{kind}");
    let target = std::fs::read_link(&path).map_err(|_| State::Unavailable)?;
    let metadata = std::fs::metadata(&path).map_err(|_| State::Unavailable)?;
    if !metadata.is_file()
        || target.to_str() != Some(format!("{kind}:[{}]", metadata.ino()).as_str())
    {
        return Err(State::Unavailable);
    }
    Ok(metadata)
}

#[cfg(target_os = "linux")]
fn verify_pid_view(bytes: &[u8], pid: u32) -> std::result::Result<(), State> {
    // Other status fields may contain non-UTF-8 comm; inspect only NSpid.
    // Duplicated, absent or multi-level values cannot establish this view.
    let mut entries = bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"NSpid:"));
    let value = entries.next().ok_or(State::Unavailable)?;
    if entries.next().is_some() {
        return Err(State::Unavailable);
    }
    let value = std::str::from_utf8(value).map_err(|_| State::Unavailable)?;
    let mut numbers = value.split_whitespace();
    if pid == 0
        || numbers.next().and_then(|v| v.parse::<u32>().ok()) != Some(pid)
        || numbers.next().is_some()
    {
        return Err(State::Unavailable);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn bounded_read(path: &str, maximum: usize) -> std::result::Result<Vec<u8>, State> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            State::Missing
        } else {
            State::Unavailable
        }
    })?;
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| State::Unavailable)?;
    if bytes.len() > maximum {
        return Err(State::Unavailable);
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn parse_stat(bytes: &[u8], pid: u32) -> std::result::Result<String, State> {
    // comm is arbitrary bytes and may include spaces, ')' and newlines. Only
    // parse the ASCII fields after its final ')'; never echo comm/argv.
    let end = bytes
        .iter()
        .rposition(|b| *b == b')')
        .ok_or(State::Unavailable)?;
    let first = bytes
        .iter()
        .position(|b| *b == b' ')
        .ok_or(State::Unavailable)?;
    if bytes.get(first + 1) != Some(&b'(')
        || std::str::from_utf8(&bytes[..first])
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            != Some(pid)
    {
        return Err(State::Unavailable);
    }
    let tail = std::str::from_utf8(&bytes[end + 1..]).map_err(|_| State::Unavailable)?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let state = fields.first().copied().ok_or(State::Unavailable)?;
    if matches!(state, "Z" | "X" | "x") {
        return Err(State::Missing);
    }
    if !matches!(state, "R" | "S" | "D" | "T" | "t" | "K" | "W" | "P" | "I") {
        return Err(State::Unavailable);
    }
    fields
        .get(19)
        .filter(|v| decimal(v))
        .map(|v| (*v).into())
        .ok_or(State::Unavailable)
}

#[cfg(target_os = "linux")]
fn observe_process(pid: u32) -> std::result::Result<BTreeMap<String, String>, State> {
    if pid == 0 {
        return Err(State::Unavailable);
    }
    let host = process_host()?;
    let path = format!("/proc/{pid}/stat");
    let start = parse_stat(&bounded_read(&path, 8192)?, pid)?;
    if parse_stat(&bounded_read(&path, 8192)?, pid)? != start || process_host()? != host {
        return Err(State::Unavailable);
    }
    Ok(BTreeMap::from([
        ("pid".into(), pid.to_string()),
        ("start_identity".into(), start),
        ("host".into(), host),
    ]))
}

#[cfg(not(target_os = "linux"))]
fn observe_process(_: u32) -> std::result::Result<BTreeMap<String, String>, State> {
    Err(State::Unsupported)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn stat(comm: &[u8], state: &str, start: &str) -> Vec<u8> {
        let mut result = b"123 (".to_vec();
        result.extend_from_slice(comm);
        result.extend_from_slice(
            format!(") {state} {} {start} 0\n", vec!["0"; 18].join(" ")).as_bytes(),
        );
        result
    }

    #[test]
    fn parser_handles_non_utf8_names_and_rejects_wrong_pid_or_dead_process() {
        assert_eq!(
            parse_stat(&stat(b"name )\n\xff", "S", "987"), 123).unwrap(),
            "987"
        );
        assert_eq!(
            parse_stat(&stat(b"name", "S", "987"), 124),
            Err(State::Unavailable)
        );
        assert_eq!(
            parse_stat(&stat(b"name", "Z", "987"), 123),
            Err(State::Missing)
        );
        for input in [b"".as_slice(), b"123 bad", b"123 () S", b"123 () secret"] {
            assert_eq!(parse_stat(input, 123), Err(State::Unavailable));
        }
        assert_eq!(
            parse_stat(&stat(b"name", "S", "-1"), 123),
            Err(State::Unavailable)
        );
    }

    #[test]
    fn live_identity_is_matched_but_wrong_start_boot_or_legacy_host_are_not() {
        let observed = process_identity(std::process::id()).unwrap();
        assert_eq!(inspect_process(&observed).0, State::Matched);
        let mut wrong = observed.clone();
        wrong.insert("start_identity".into(), "18446744073709551615".into());
        assert_eq!(inspect_process(&wrong).0, State::Mismatch);
        wrong = observed.clone();
        wrong.insert(
            "host".into(),
            "linux-v1:00000000-0000-0000-0000-000000000000:0:1:0:1".into(),
        );
        assert_eq!(inspect_process(&wrong).0, State::Mismatch);
        wrong.insert("host".into(), "local".into());
        assert_eq!(inspect_process(&wrong).0, State::Unsupported);
        assert!(process_identity(0).is_err());
    }

    #[test]
    fn numeric_proc_view_must_be_the_callers_only_pid_namespace() {
        assert_eq!(
            verify_pid_view(b"Name:\t\xff\nNSpid:\t123\nPid:\t123\n", 123),
            Ok(())
        );
        for (input, pid) in [
            (b"NSpid:\t123 1\n".as_slice(), 1),
            (b"NSpid:\t123 1\n", 123),
            (b"NSpid:\t124\n", 123),
            (b"NSpid:\t123\nNSpid:\t123\n", 123),
            (b"Pid:\t123\n", 123),
            (b"NSpid:\t\n", 123),
            (b"NSpid:\t0\n", 0),
        ] {
            assert_eq!(verify_pid_view(input, pid), Err(State::Unavailable));
        }
    }

    #[test]
    fn host_encoding_rejects_unknown_versions_missing_time_view_and_bad_fields() {
        let boot = "01234567-89ab-cdef-0123-456789abcdef";
        assert!(valid_host(&format!("linux-v1:{boot}:4:5:6:7")));
        for value in [
            format!("linux:{boot}:4:5"),
            format!("linux-v2:{boot}:4:5:6:7"),
            format!("linux-v1:{boot}:4:5:6"),
            format!("linux-v1:{boot}:4:5:6:7:8"),
            format!("linux-v1:{boot}:4:5:6:-7"),
            format!("linux-v1:{boot}:4:5:6:18446744073709551616"),
            "linux-v1:not-a-boot-id:4:5:6:7".into(),
        ] {
            assert!(!valid_host(&value));
        }
        let mut observed = process_identity(std::process::id()).unwrap();
        let host = observed["host"].clone();
        let (before_time_inode, _) = host.rsplit_once(':').unwrap();
        observed.insert(
            "host".into(),
            format!("{before_time_inode}:18446744073709551615"),
        );
        assert_eq!(inspect_process(&observed).0, State::Mismatch);
    }
}
