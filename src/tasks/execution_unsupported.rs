//! Fail closed on platforms without the verified Linux containment adapter.

use std::path::Path;

use anyhow::{Result, bail};
use serde::Serialize;

// No successful report is produced on these platforms. The field is the
// minimal shared return contract read by the CLI after a successful run.
#[derive(Serialize)]
pub(crate) struct ExecutionReport {
    pub(crate) exit_code: Option<i32>,
}

pub(crate) fn run(
    _task_id: &str,
    _actor: &str,
    _generation: u64,
    _program: &Path,
    _args: &[String],
    _mode: super::ExecutionMode,
) -> Result<ExecutionReport> {
    unavailable()
}

pub(crate) fn inspect(_task_id: &str) -> Result<ExecutionReport> {
    unavailable()
}

pub(crate) fn stop(
    _task_id: &str,
    _actor: &str,
    _generation: u64,
    _execution_id: &str,
) -> Result<ExecutionReport> {
    unavailable()
}

pub(crate) fn worker(_task_id: &str, _execution_id: &str) -> Result<()> {
    unavailable()
}

pub(crate) fn release_handoff_source(
    _task_id: &str,
    _actor: &str,
    _generation: u64,
    _handoff_id: &str,
    _execution_id: &str,
) -> Result<super::store::TaskRecord> {
    unavailable()
}

fn unavailable<T>() -> Result<T> {
    bail!(
        "local execution supervision requires Linux user systemd and cgroup v2; no uncontrolled fallback was launched"
    )
}
