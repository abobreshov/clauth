//! Resource disposition before source stop, and live rechecks after it.
//! Reconnect and cleanup strings are never executed. Herdr has no verified
//! pane-close or ownership compare-and-swap, so a live Herdr pane cannot be
//! released or given exclusive control.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanOutcome {
    /// Identity survived outside the source scope. Not exclusive control.
    AdoptedReference,
    Observed,
    Gone,
    ReleasedAbsent,
    /// Process identity is inside the source scope; scope stop is the cleanup.
    ReleasedWithSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlanEntry {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) disposition: String,
    pub(super) outcome: PlanOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DispositionPlan {
    pub(super) binding_digest: String,
    pub(super) source_cgroup: String,
    pub(super) resource_revision: u64,
    pub(super) classified_generation: u64,
    pub(super) stop_safe: bool,
    pub(super) entries: Vec<PlanEntry>,
}

pub(crate) fn classify_resources(
    id: &str,
    actor: &str,
    generation: u64,
    handoff_id: &str,
    source_cgroup: &str,
    binding_digest: &str,
) -> Result<TaskRecord> {
    if !valid_key(handoff_id) || !valid_digest(binding_digest) || !valid_cgroup(source_cgroup) {
        bail!("invalid resource classification identity");
    }
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    let index = record
        .handoffs
        .iter()
        .position(|proposal| {
            proposal.handoff_id == handoff_id && proposal.state == ProposalState::Proposed
        })
        .ok_or_else(|| anyhow::anyhow!("resource classification requires the active handoff"))?;
    if record.resources.is_empty() {
        bail!("resource classification requires declared resources");
    }
    if let Some(plan) = &record.handoffs[index].disposition_plan {
        if plan.binding_digest != binding_digest
            || plan.source_cgroup != source_cgroup
            || plan.resource_revision != record.resource_revision
            || plan.entries != decide(&record, source_cgroup)?
        {
            bail!("resource classification differs from the saved plan");
        }
        if generation != record.generation && generation + 1 != plan.classified_generation {
            bail!("stale task generation; read the task again before retrying");
        }
        return Ok(record);
    }
    guard(&record, actor, generation)?;
    let entries = decide(&record, source_cgroup)?;
    if record.history.len() + 1 > MAX_HISTORY {
        bail!("task history lacks room for resource classification");
    }
    advance(&mut record)?;
    record.handoffs[index].disposition_plan = Some(DispositionPlan {
        binding_digest: binding_digest.into(),
        source_cgroup: source_cgroup.into(),
        resource_revision: record.resource_revision,
        classified_generation: record.generation,
        stop_safe: true,
        entries,
    });
    record.history.push(event(&record, "resources_classified"));
    super::validate(&record, &dir)?;
    atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    Ok(record)
}

pub(super) fn recheck(record: &TaskRecord, handoff_id: &str, binding_digest: &str) -> Result<()> {
    if record.resources.is_empty() {
        return Ok(());
    }
    let plan = record
        .handoffs
        .iter()
        .find(|proposal| {
            proposal.handoff_id == handoff_id && proposal.state == ProposalState::Proposed
        })
        .and_then(|proposal| proposal.disposition_plan.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "source release with declared resources requires a verified resource classification"
            )
        })?;
    if !plan.stop_safe
        || plan.binding_digest != binding_digest
        || plan.resource_revision != record.resource_revision
        || plan.entries != decide(record, &plan.source_cgroup)?
    {
        bail!("resource classification is stale or does not match this source scope; not stopping");
    }
    Ok(())
}

pub(super) fn validate_plans(record: &TaskRecord) -> Result<()> {
    let mut events = BTreeSet::new();
    for proposal in &record.handoffs {
        let Some(plan) = &proposal.disposition_plan else {
            continue;
        };
        if !plan.stop_safe
            || !valid_digest(&plan.binding_digest)
            || !valid_cgroup(&plan.source_cgroup)
            || plan.classified_generation <= proposal.proposed_generation
            || plan.classified_generation > record.generation
            || plan.resource_revision > record.resource_revision
            || plan.entries.len() != plan.resource_revision as usize
            || record
                .history
                .get((plan.classified_generation - 1) as usize)
                .map(|event| event.kind.as_str())
                != Some("resources_classified")
            || !events.insert(plan.classified_generation)
        {
            bail!("resource classification history is invalid");
        }
        let covered: BTreeSet<_> = record
            .resources
            .iter()
            .take(plan.resource_revision as usize)
            .map(|resource| resource.id.as_str())
            .collect();
        let planned: BTreeSet<_> = plan.entries.iter().map(|entry| entry.id.as_str()).collect();
        if planned != covered || plan.entries.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            bail!("resource classification does not cover the declared resources");
        }
        for entry in &plan.entries {
            let Some(resource) = record
                .resources
                .iter()
                .find(|resource| resource.id == entry.id)
            else {
                bail!("resource classification names an unknown resource");
            };
            if entry.kind != resource.kind || entry.disposition != resource.disposition {
                bail!("resource classification does not match the declaration");
            }
            if !matches!(
                entry.outcome,
                PlanOutcome::AdoptedReference
                    | PlanOutcome::Observed
                    | PlanOutcome::Gone
                    | PlanOutcome::ReleasedAbsent
                    | PlanOutcome::ReleasedWithSource
            ) {
                bail!("resource classification outcome is invalid");
            }
        }
        if proposal.state == ProposalState::Proposed
            && (plan.resource_revision != record.resource_revision
                || plan.entries.len() != record.resources.len())
        {
            bail!("active resource classification does not match the current registry");
        }
    }
    let actual: BTreeSet<_> = record
        .history
        .iter()
        .filter(|event| event.kind == "resources_classified")
        .map(|event| event.generation)
        .collect();
    if actual != events {
        bail!("resource classification events lack a saved plan");
    }
    Ok(())
}

fn decide(record: &TaskRecord, source_cgroup: &str) -> Result<Vec<PlanEntry>> {
    if !valid_cgroup(source_cgroup) {
        bail!("source cgroup identity is invalid");
    }
    let mut entries = Vec::new();
    for resource in &record.resources {
        entries.push(PlanEntry {
            id: resource.id.clone(),
            kind: resource.kind.clone(),
            disposition: resource.disposition.clone(),
            outcome: decide_one(resource, source_cgroup)?,
        });
    }
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(entries)
}

fn decide_one(resource: &ResourceInput, source_cgroup: &str) -> Result<PlanOutcome> {
    if resource.disposition == "block" {
        bail!("disposition block parks the handoff before the source is stopped");
    }
    if !matches!(resource.kind.as_str(), "process" | "herdr_pane") {
        bail!("resource kind has no verified disposition adapter");
    }
    if resource.ownership == "shared" && resource.disposition != "observe" {
        bail!("shared resources can only be observed");
    }
    let live = live_resource(resource)?;
    let outcome = match (resource.disposition.as_str(), live) {
        ("release", Live::Missing) => PlanOutcome::ReleasedAbsent,
        ("release", Live::Matched { cgroup, .. })
            if resource.kind == "process" && inside(&cgroup, source_cgroup) =>
        {
            PlanOutcome::ReleasedWithSource
        }
        ("release", _) => {
            bail!(
                "live release has no verified cleanup and is not inside the source scope; refusing to stop"
            )
        }
        ("observe", Live::Missing) if !resource.may_write => PlanOutcome::Gone,
        (
            "observe",
            Live::Matched {
                cgroup,
                parent_cgroup,
            },
        ) if !resource.may_write
            && !inside(&cgroup, source_cgroup)
            && parent_cgroup
                .as_deref()
                .is_some_and(|parent| !inside(parent, source_cgroup)) =>
        {
            PlanOutcome::Observed
        }
        (
            "adopt",
            Live::Matched {
                cgroup,
                parent_cgroup,
            },
        ) if resource.ownership == "task"
            && !resource.may_write
            && !inside(&cgroup, source_cgroup)
            && parent_cgroup
                .as_deref()
                .is_some_and(|parent| !inside(parent, source_cgroup)) =>
        {
            PlanOutcome::AdoptedReference
        }
        ("observe" | "adopt", _) => {
            bail!(
                "resource is an unfenced writer, not independently reachable, or not outside the source scope"
            )
        }
        _ => bail!("unsupported resource disposition"),
    };
    let _ = resource.reconnect.as_deref();
    let _ = resource.cleanup.as_deref();
    Ok(outcome)
}

enum Live {
    Missing,
    Matched {
        cgroup: String,
        parent_cgroup: Option<String>,
    },
}

fn live_resource(resource: &ResourceInput) -> Result<Live> {
    let (state, _reason) =
        crate::tasks::resources::declared_identity(&resource.kind, &resource.native_identity);
    if state == crate::tasks::resources::State::Missing {
        return Ok(Live::Missing);
    }
    if state != crate::tasks::resources::State::Matched {
        bail!(match state {
            crate::tasks::resources::State::Mismatch => {
                "resource identity changed or its pid was reused"
            }
            crate::tasks::resources::State::Unsupported => {
                "resource identity has no verified adapter for this disposition"
            }
            _ => "resource identity could not be re-observed",
        });
    }
    let pid = resource_pid(resource)?;
    let cgroup = process_cgroup(pid)?;
    let parent_cgroup = process_parent(pid)
        .ok()
        .and_then(|parent| process_cgroup(parent).ok());
    Ok(Live::Matched {
        cgroup,
        parent_cgroup,
    })
}

fn resource_pid(resource: &ResourceInput) -> Result<u32> {
    let key = if resource.kind == "herdr_pane" {
        "pane_pid"
    } else {
        "pid"
    };
    resource
        .native_identity
        .get(key)
        .and_then(|value| value.parse().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| anyhow::anyhow!("resource is missing a verified process id"))
}

fn inside(cgroup: &str, source: &str) -> bool {
    cgroup == source
        || cgroup
            .strip_prefix(source)
            .is_some_and(|tail| tail.starts_with('/'))
}

pub(crate) fn process_cgroup(pid: u32) -> Result<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map_err(|_| anyhow::anyhow!("resource cgroup could not be read"))?;
    let paths: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .filter(|path| valid_cgroup(path))
        .collect();
    match paths.as_slice() {
        [path] => Ok((*path).to_owned()),
        _ => bail!("resource cgroup membership is ambiguous"),
    }
}

fn process_parent(pid: u32) -> Result<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|_| anyhow::anyhow!("resource parent could not be read"))?;
    let parent = text
        .lines()
        .find_map(|line| line.strip_prefix("PPid:\t"))
        .and_then(|value| value.parse().ok())
        .filter(|parent: &u32| *parent > 0)
        .ok_or_else(|| anyhow::anyhow!("resource parent is unavailable"))?;
    Ok(parent)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn valid_cgroup(value: &str) -> bool {
    let Some(relative) = value.strip_prefix('/') else {
        return false;
    };
    !relative.is_empty()
        && relative.len() <= 4096
        && !relative.chars().any(char::is_control)
        && !relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

/// Post-stop probe. A failed writer or a failed adopt parks; it does not restart the source.
pub(super) fn reconcile_entries(
    record: &TaskRecord,
    plan: &DispositionPlan,
) -> Result<Vec<PlanEntry>> {
    let mut entries = Vec::new();
    for saved in &plan.entries {
        let resource = record
            .resources
            .iter()
            .find(|resource| resource.id == saved.id)
            .ok_or_else(|| anyhow::anyhow!("reconciled resource disappeared from the registry"))?;
        let live = live_resource(resource);
        let outcome = match (saved.outcome, live) {
            (PlanOutcome::ReleasedAbsent | PlanOutcome::ReleasedWithSource, Ok(Live::Missing)) => {
                PlanOutcome::ReleasedAbsent
            }
            (PlanOutcome::Gone, Ok(Live::Missing)) => PlanOutcome::Gone,
            (
                PlanOutcome::Observed,
                Ok(Live::Matched {
                    cgroup,
                    parent_cgroup,
                }),
            ) if !inside(&cgroup, &plan.source_cgroup)
                && parent_cgroup
                    .as_deref()
                    .is_some_and(|parent| !inside(parent, &plan.source_cgroup)) =>
            {
                PlanOutcome::Observed
            }
            (PlanOutcome::Observed, Ok(Live::Missing)) => PlanOutcome::Gone,
            (
                PlanOutcome::AdoptedReference,
                Ok(Live::Matched {
                    cgroup,
                    parent_cgroup,
                }),
            ) if !inside(&cgroup, &plan.source_cgroup)
                && parent_cgroup
                    .as_deref()
                    .is_some_and(|parent| !inside(parent, &plan.source_cgroup)) =>
            {
                PlanOutcome::AdoptedReference
            }
            _ => bail!(
                "resource reconnect or release probe failed; handoff is parked and the source stays stopped"
            ),
        };
        entries.push(PlanEntry {
            id: saved.id.clone(),
            kind: saved.kind.clone(),
            disposition: saved.disposition.clone(),
            outcome,
        });
    }
    Ok(entries)
}

pub(crate) fn reconcile_resources(
    id: &str,
    actor: &str,
    generation: u64,
    handoff_id: &str,
) -> Result<TaskRecord> {
    if !valid_key(handoff_id) {
        bail!("invalid handoff ID");
    }
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    if actor != record.owner.native_session_id || record.ownership_epoch != 1 {
        bail!("resource reconciliation actor does not match the stopped source");
    }
    if !record.source_scope_released(handoff_id) {
        bail!("resource reconciliation requires the exact source scope to be empty");
    }
    let plan = record
        .handoffs
        .iter()
        .find(|proposal| proposal.handoff_id == handoff_id)
        .and_then(|proposal| proposal.disposition_plan.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("resource reconciliation requires the saved classification")
        })?;
    if let Some(transfer) = &record.transfer {
        if transfer.handoff_id != handoff_id {
            bail!("another handoff already has reconciliation state");
        }
        if transfer.phase == TransferPhase::Reconciled {
            if generation != record.generation && generation + 1 != transfer.generation {
                bail!("stale task generation; read the task again before retrying");
            }
            let entries = reconcile_entries(&record, &plan)?;
            if transfer.reconciliation.as_ref().map(|item| &item.entries) != Some(&entries) {
                bail!("reconciled resources changed; refusing to launch");
            }
            return Ok(record);
        }
        if !matches!(
            transfer.phase,
            TransferPhase::ReconcileParked | TransferPhase::Parked
        ) {
            bail!("resource reconciliation is already past the probe");
        }
    }
    if generation != record.generation {
        bail!("stale task generation; read the task again before retrying");
    }
    let entries = match reconcile_entries(&record, &plan) {
        Ok(entries) => entries,
        Err(error) => {
            park_reconciliation(&mut record, &dir, handoff_id, &plan)?;
            return Err(error);
        }
    };
    advance(&mut record)?;
    record.transfer = Some(TransferState {
        handoff_id: handoff_id.into(),
        phase: TransferPhase::Reconciled,
        generation: record.generation,
        reconciliation: Some(Reconciliation {
            source_cgroup: plan.source_cgroup,
            entries,
            exclusive_control: false,
        }),
        successor: None,
        delivery_digest: None,
        receipts_digest: None,
        successor_session_id: None,
    });
    record.history.push(event(&record, "resources_reconciled"));
    super::validate(&record, &dir)?;
    atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    Ok(record)
}

fn park_reconciliation(
    record: &mut TaskRecord,
    dir: &Path,
    handoff_id: &str,
    plan: &DispositionPlan,
) -> Result<()> {
    if record
        .transfer
        .as_ref()
        .is_some_and(|transfer| transfer.phase == TransferPhase::ReconcileParked)
        && record.history.last().map(|event| event.kind.as_str()) == Some("resources_parked")
    {
        return Ok(());
    }
    advance(record)?;
    record.transfer = Some(TransferState {
        handoff_id: handoff_id.into(),
        phase: TransferPhase::ReconcileParked,
        generation: record.generation,
        reconciliation: Some(Reconciliation {
            source_cgroup: plan.source_cgroup.clone(),
            entries: plan.entries.clone(),
            exclusive_control: false,
        }),
        successor: None,
        delivery_digest: None,
        receipts_digest: None,
        successor_session_id: None,
    });
    record.history.push(event(record, "resources_parked"));
    super::validate(record, dir)?;
    atomic_json(dir, "task.json", record, MAX_RECORD_BYTES, true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TransferPhase {
    Reconciled,
    ReconcileParked,
    SuccessorPrepared,
    Delivered,
    ReceiptsPending,
    Confirmed,
    Parked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reconciliation {
    pub(super) source_cgroup: String,
    pub(super) entries: Vec<PlanEntry>,
    /// Herdr and process probes prove identity, not a lease that blocks other prompts.
    pub(super) exclusive_control: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SuccessorRecord {
    pub(super) tool: String,
    pub(super) model: String,
    pub(super) program: String,
    pub(super) args: Vec<String>,
    pub(super) pid: u32,
    pub(super) start_identity: String,
    pub(super) session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransferState {
    pub(super) handoff_id: String,
    pub(super) phase: TransferPhase,
    pub(super) generation: u64,
    pub(super) reconciliation: Option<Reconciliation>,
    pub(super) successor: Option<SuccessorRecord>,
    pub(super) successor_session_id: Option<String>,
    pub(super) delivery_digest: Option<String>,
    pub(super) receipts_digest: Option<String>,
}
