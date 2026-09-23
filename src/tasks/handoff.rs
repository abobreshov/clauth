//! Durable handoff journal. Stop-first launch happens only after the source
//! scope is empty. The task lock and one atomic record publish each transition.

use super::*;

#[path = "continuation.rs"]
mod continuation;
#[path = "disposition.rs"]
mod disposition;
#[path = "source_release.rs"]
mod source_release;
pub(super) use continuation::resume_loaded;
pub(crate) use continuation::{continue_handoff, recover_handoff, successor_worker};
pub(super) use disposition::TransferState;
pub(crate) use disposition::{classify_resources, reconcile_resources};
#[cfg(target_os = "linux")]
pub(crate) use source_release::{ReleaseOutcome, begin_source_release};

pub(super) fn refresh_readiness(record: &mut TaskRecord) {
    super::assign_readiness(record);
}

pub(crate) fn classified_binding<'a>(
    record: &'a TaskRecord,
    handoff_id: &str,
) -> Option<(&'a str, &'a str)> {
    record
        .handoffs
        .iter()
        .find(|proposal| proposal.handoff_id == handoff_id)
        .and_then(|proposal| proposal.disposition_plan.as_ref())
        .map(|plan| (plan.source_cgroup.as_str(), plan.binding_digest.as_str()))
}

pub(crate) fn handoff_destination<'a>(
    record: &'a TaskRecord,
    handoff_id: &str,
) -> Option<(&'a str, &'a str)> {
    record
        .handoffs
        .iter()
        .find(|proposal| {
            proposal.handoff_id == handoff_id && proposal.state == ProposalState::Proposed
        })
        .map(|proposal| {
            (
                proposal.destination_tool.as_str(),
                proposal.destination_model.as_str(),
            )
        })
}

const MAX_PROPOSALS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProposalState {
    Proposed,
    Cancelled,
    Superseded,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffProposal {
    handoff_id: String,
    source_generation: u64,
    proposed_generation: u64,
    source_epoch: u64,
    source: ExecutionIdentity,
    destination_tool: String,
    destination_model: String,
    checkpoint_generation: u64,
    checkpoint_digest: String,
    resource_revision: u64,
    policy_digest: String,
    state: ProposalState,
    closed_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_release: Option<source_release::SourceRelease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disposition_plan: Option<disposition::DispositionPlan>,
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn policy_digest(record: &TaskRecord) -> Result<String> {
    let policy = record
        .sharing_policy
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("handoff proposal requires a recorded sharing policy"))?;
    let bytes =
        serde_json::to_vec(policy).map_err(|_| anyhow::anyhow!("cannot encode sharing policy"))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub(super) fn sharing_allowed(record: &TaskRecord, tool: &str, model: &str) -> bool {
    record.sharing_policy.as_ref().is_some_and(|policy| {
        policy.share_checkpoint
            && policy.share_workspace
            && (record.resources.is_empty() || policy.share_resource_metadata)
            && policy.destinations.iter().any(|rule| {
                rule.tool == tool && rule.models.iter().any(|name| name == "*" || name == model)
            })
    })
}

pub(crate) fn propose_handoff(
    id: &str,
    actor: &str,
    expected_generation: u64,
    request_id: &str,
    tool: &str,
    model: &str,
) -> Result<TaskRecord> {
    if !valid_key(request_id) {
        bail!("invalid handoff request ID");
    }
    check_tool(tool)?;
    check_text(model, true)?;
    if model == "*" {
        bail!("handoff requires an actual destination model");
    }
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    // A retry may use the original generation after the first call committed.
    // It must never re-open a cancelled/superseded proposal or retarget it.
    if let Some(prior) = record.handoffs.iter().find(|p| p.handoff_id == request_id) {
        if actor != record.owner.native_session_id
            || prior.source.native_session_id != actor
            || prior.source_generation != expected_generation
            || prior.destination_tool != tool
            || prior.destination_model != model
        {
            bail!("handoff request ID already belongs to a different request");
        }
        return Ok(record);
    }
    guard(&record, actor, expected_generation)?;
    if record.handoffs.len() >= MAX_PROPOSALS {
        bail!("handoff proposal history is full");
    }
    if record
        .handoffs
        .iter()
        .any(|p| p.state == ProposalState::Proposed)
    {
        bail!("a handoff proposal is already active; cancel it or update its source checkpoint");
    }
    if record
        .handoffs
        .iter()
        .any(|p| p.state == ProposalState::Committed)
    {
        bail!("a committed handoff exists; a further hop is not enabled");
    }
    if !sharing_allowed(&record, tool, model) {
        bail!("destination or required data sharing is not allowed by recorded policy");
    }
    let checkpoint = record
        .latest_checkpoint
        .as_ref()
        .filter(|c| c.resource_revision == record.resource_revision)
        .ok_or_else(|| {
            anyhow::anyhow!("handoff proposal requires a checkpoint covering current resources")
        })?;
    let snapshot = checkpoint.workspace_snapshot.as_ref().ok_or_else(|| {
        anyhow::anyhow!("handoff proposal requires a captured workspace checkpoint")
    })?;
    if workspace::capture(&record.workspace)? != *snapshot {
        bail!("workspace differs from the checkpoint; compact and capture again");
    }
    let proposal = HandoffProposal {
        handoff_id: request_id.into(),
        source_generation: record.generation,
        proposed_generation: record
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("task generation exhausted"))?,
        source_epoch: record.ownership_epoch,
        source: record.owner.clone(),
        destination_tool: tool.into(),
        destination_model: model.into(),
        checkpoint_generation: checkpoint.generation,
        checkpoint_digest: checkpoint.digest.clone(),
        resource_revision: record.resource_revision,
        policy_digest: policy_digest(&record)?,
        state: ProposalState::Proposed,
        closed_generation: None,
        source_release: None,
        disposition_plan: None,
    };
    advance(&mut record)?;
    record.handoffs.push(proposal);
    record.history.push(event(&record, "handoff_proposed"));
    validate(&record, &dir)?;
    atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    Ok(record)
}

pub(crate) fn cancel_handoff(
    id: &str,
    actor: &str,
    expected_generation: u64,
    handoff_id: &str,
) -> Result<TaskRecord> {
    if !valid_key(handoff_id) {
        bail!("invalid handoff ID");
    }
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    guard(&record, actor, expected_generation)?;
    let index = record
        .handoffs
        .iter()
        .position(|p| p.handoff_id == handoff_id && p.state == ProposalState::Proposed)
        .ok_or_else(|| anyhow::anyhow!("handoff proposal is not active"))?;
    advance(&mut record)?;
    record.handoffs[index].state = ProposalState::Cancelled;
    record.handoffs[index].closed_generation = Some(record.generation);
    record.history.push(event(&record, "handoff_cancelled"));
    validate(&record, &dir)?;
    atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    Ok(record)
}

pub(super) fn supersede(record: &mut TaskRecord) {
    for proposal in &mut record.handoffs {
        if proposal.state == ProposalState::Proposed {
            proposal.state = ProposalState::Superseded;
            proposal.closed_generation = Some(record.generation);
        }
    }
}

pub(super) fn guard_source_mutation(record: &TaskRecord) -> Result<()> {
    if record.ownership_epoch == 1 && record.handoffs.iter().any(|p| p.source_release.is_some()) {
        bail!(
            "source release is pending or completed; managed source mutation/relaunch is blocked; reconcile the exact handoff instead"
        );
    }
    Ok(())
}

pub(crate) fn source_launch_allowed(record: &TaskRecord) -> bool {
    record.ownership_epoch == 1
        && record
            .handoffs
            .iter()
            .all(|proposal| proposal.source_release.is_none())
}

pub(super) fn validate(record: &TaskRecord, dir: &Path) -> Result<()> {
    if record.handoffs.len() > MAX_PROPOSALS {
        bail!("handoff proposal history exceeds limit");
    }
    let mut ids = BTreeSet::new();
    let mut starts = BTreeSet::new();
    let mut cancellations = BTreeSet::new();
    let mut active = 0;
    let mut previous_end = 0;
    for p in &record.handoffs {
        check_tool(&p.destination_tool)?;
        check_text(&p.destination_model, true)?;
        let same_owner = p.source == record.owner && p.source_epoch == record.ownership_epoch;
        if !valid_key(&p.handoff_id)
            || !ids.insert(&p.handoff_id)
            || (p.state != ProposalState::Committed && !same_owner)
            || p.destination_model == "*"
            || p.source_generation == 0
            || p.source_generation.checked_add(1) != Some(p.proposed_generation)
            || p.proposed_generation <= previous_end
            || p.proposed_generation > record.generation
            || p.checkpoint_generation < 2
            || p.checkpoint_generation > p.source_generation
            || p.resource_revision > record.resource_revision
            || p.policy_digest.len() != 64
            || !p.policy_digest.bytes().all(|b| b.is_ascii_hexdigit())
            || record
                .history
                .get((p.proposed_generation - 1) as usize)
                .map(|e| e.kind.as_str())
                != Some("handoff_proposed")
        {
            bail!("handoff proposal identity or revision is invalid");
        }
        starts.insert(p.proposed_generation);
        let checkpoint: CheckpointRecord = read_json(
            &dir.join(checkpoint_name(p.checkpoint_generation)),
            MAX_CHECKPOINT_BYTES,
        )?;
        check_checkpoint(&checkpoint.payload)?;
        if let Some(snapshot) = &checkpoint.workspace_snapshot {
            workspace::validate(snapshot)?;
            if snapshot.root != record.workspace {
                bail!("handoff checkpoint workspace identity is invalid");
            }
        }
        if checkpoint.generation != p.checkpoint_generation
            || checkpoint.digest != p.checkpoint_digest
            || digest(&checkpoint)? != p.checkpoint_digest
            || checkpoint.resource_revision != p.resource_revision
            || checkpoint.workspace_snapshot.is_none()
            || unique(&checkpoint.payload.constraints)? != unique(&record.constraints)?
            || unique(&checkpoint.payload.resource_ids)?
                != record
                    .resources
                    .iter()
                    .take(p.resource_revision as usize)
                    .map(|r| r.id.as_str())
                    .collect::<BTreeSet<_>>()
            || record.history[(p.checkpoint_generation - 1) as usize].kind != "checkpointed"
            || record.history.iter().any(|e| {
                e.generation > p.checkpoint_generation
                    && e.generation < p.proposed_generation
                    && e.kind == "checkpointed"
            })
            || record
                .history
                .iter()
                .filter(|e| e.kind == "resource_declared" && e.generation < p.proposed_generation)
                .count() as u64
                != p.resource_revision
        {
            bail!("handoff proposal checkpoint does not match its immutable record");
        }
        let end = match (p.state, p.closed_generation) {
            (ProposalState::Proposed, None) => {
                active += 1;
                if record.latest_checkpoint.as_ref() != Some(&checkpoint)
                    || record.resource_revision != p.resource_revision
                    || policy_digest(record)? != p.policy_digest
                    || !sharing_allowed(record, &p.destination_tool, &p.destination_model)
                    || record.history.iter().any(|e| {
                        e.generation > p.proposed_generation
                            && matches!(
                                e.kind.as_str(),
                                "checkpointed" | "resource_declared" | "policy_updated"
                            )
                    })
                {
                    bail!("active handoff proposal no longer matches task state");
                }
                record.generation
            }
            (ProposalState::Committed, Some(end)) => {
                if end <= p.proposed_generation || end > record.generation {
                    bail!("handoff proposal close revision is invalid");
                }
                if record.history[(end - 1) as usize].kind != "ownership_committed"
                    || record.ownership_epoch != p.source_epoch + 1
                    || record.continued_from.as_ref() != Some(&p.source)
                    || record.handed_off_to.as_ref() != Some(&record.owner)
                    || p.source == record.owner
                {
                    bail!("committed handoff ownership is invalid");
                }
                end
            }
            (ProposalState::Cancelled | ProposalState::Superseded, Some(end)) => {
                if end <= p.proposed_generation || end > record.generation {
                    bail!("handoff proposal close revision is invalid");
                }
                if record.history.iter().any(|e| {
                    e.generation > p.proposed_generation
                        && e.generation < end
                        && matches!(
                            e.kind.as_str(),
                            "checkpointed" | "resource_declared" | "policy_updated"
                        )
                }) {
                    bail!("handoff proposal crossed an earlier source revision change");
                }
                let kind = record.history[(end - 1) as usize].kind.as_str();
                if p.state == ProposalState::Cancelled {
                    if kind != "handoff_cancelled" || !cancellations.insert(end) {
                        bail!("handoff cancellation history is invalid");
                    }
                } else if !matches!(
                    kind,
                    "checkpointed" | "resource_declared" | "policy_updated"
                ) {
                    bail!("handoff supersession history is invalid");
                }
                end
            }
            _ => bail!("handoff proposal state is invalid"),
        };
        previous_end = end;
    }
    if active > 1
        || record
            .history
            .iter()
            .filter(|e| e.kind == "handoff_proposed")
            .map(|e| e.generation)
            .collect::<BTreeSet<_>>()
            != starts
        || record
            .history
            .iter()
            .filter(|e| e.kind == "handoff_cancelled")
            .map(|e| e.generation)
            .collect::<BTreeSet<_>>()
            != cancellations
    {
        bail!("handoff proposal history is inconsistent");
    }
    source_release::validate(record)?;
    disposition::validate_plans(record)?;
    continuation::validate(record, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::HomeSandbox;

    fn ready(home: &HomeSandbox) -> TaskRecord {
        let root = home.home().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("work.txt"), "unfinished").unwrap();
        let task = register(NewTask {
            objective: "Continue with another model".into(),
            workspace: root,
            constraints: vec!["one".into(), "two".into()],
            source: ExecutionIdentity {
                tool: "codex".into(),
                model: None,
                native_session_id: "source".into(),
                account_ref: None,
            },
        })
        .unwrap();
        let task = checkpoint_capturing_workspace(
            &task.task_id,
            "source",
            task.generation,
            CheckpointInput {
                brief: "Compact state".into(),
                completed: vec![],
                remaining_plan: vec!["Finish".into()],
                decisions: vec![],
                uncertainties: vec![],
                next_action: "Review".into(),
                constraints: vec!["two".into(), "one".into()],
                resource_ids: vec![],
            },
        )
        .unwrap();
        set_policy(
            &task.task_id,
            "source",
            task.generation,
            SharingPolicy {
                destinations: vec![DestinationRule {
                    tool: "grok".into(),
                    models: vec!["model".into()],
                }],
                share_checkpoint: true,
                share_workspace: true,
                share_resource_metadata: false,
            },
        )
        .unwrap()
    }

    #[test]
    fn malformed_proposal_metadata_is_rejected_without_panicking() {
        let home = HomeSandbox::new();
        let task = ready(&home);
        let task = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "attempt",
            "grok",
            "model",
        )
        .unwrap();
        let dir = task_dir(&task.task_id).unwrap();
        for case in 0..9 {
            let mut changed = task.clone();
            match case {
                0 => changed.handoffs[0].proposed_generation = 0,
                1 => changed.handoffs[0].checkpoint_generation = u64::MAX,
                2 => changed.handoffs[0].checkpoint_digest = "0".repeat(64),
                3 => changed.handoffs[0].policy_digest = "0".repeat(64),
                4 => changed.handoffs[0].source.native_session_id = "other".into(),
                5 => changed.handoffs[0].source_epoch += 1,
                6 => changed.handoffs[0].closed_generation = Some(u64::MAX),
                7 => changed.handoffs.push(changed.handoffs[0].clone()),
                _ => changed.handoffs.clear(),
            }
            atomic_json(&dir, "task.json", &changed, MAX_RECORD_BYTES, true).unwrap();
            assert!(show(&task.task_id).is_err(), "case {case}");
        }
        atomic_json(&dir, "task.json", &task, MAX_RECORD_BYTES, true).unwrap();
        assert_eq!(show(&task.task_id).unwrap(), task);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_release_intent_survives_coordinator_loss_and_freezes_source() {
        let home = HomeSandbox::new();
        let task = ready(&home);
        let task = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "stop-first",
            "grok",
            "model",
        )
        .unwrap();
        let id = &task.task_id;
        let execution = "1".repeat(32);
        let binding = "2".repeat(64);
        let intent = begin_source_release(
            id,
            "source",
            task.generation,
            "stop-first",
            &execution,
            &binding,
        )
        .unwrap();
        drop(intent); // coordinator loss after persistence, before shutdown
        let pending = show(id).unwrap();
        assert_eq!(pending.generation, task.generation + 1);
        assert_eq!(
            pending.history.last().unwrap().kind,
            "source_stop_requested"
        );
        assert!(check_current_actor(id, "source", pending.generation).is_err());
        assert!(cancel_handoff(id, "source", pending.generation, "stop-first").is_err());
        assert!(
            propose_handoff(id, "source", pending.generation, "another", "grok", "model").is_err()
        );
        assert!(
            set_policy(
                id,
                "source",
                pending.generation,
                pending.sharing_policy.clone().unwrap()
            )
            .is_err()
        );
        assert!(
            checkpoint(
                id,
                "source",
                pending.generation,
                pending.latest_checkpoint.clone().unwrap().payload
            )
            .is_err()
        );
        assert!(
            begin_source_release(
                id,
                "other",
                task.generation,
                "stop-first",
                &execution,
                &binding
            )
            .is_err()
        );
        assert!(
            begin_source_release(
                id,
                "source",
                task.generation,
                "stop-first",
                &"3".repeat(32),
                &binding
            )
            .is_err()
        );
        assert!(
            begin_source_release(
                id,
                "source",
                task.generation,
                "stop-first",
                &execution,
                &"4".repeat(64)
            )
            .is_err()
        );
        assert_eq!(show(id).unwrap(), pending);
        let parked = begin_source_release(
            id,
            "source",
            task.generation,
            "stop-first",
            &execution,
            &binding,
        )
        .unwrap()
        .finish(ReleaseOutcome::StopUnproven)
        .unwrap();
        assert_eq!(parked.history.last().unwrap().kind, "source_release_parked");
        let duplicate = begin_source_release(
            id,
            "source",
            task.generation,
            "stop-first",
            &execution,
            &binding,
        )
        .unwrap()
        .finish(ReleaseOutcome::StopUnproven)
        .unwrap();
        assert_eq!(duplicate, parked);
        let stopped = begin_source_release(
            id,
            "source",
            parked.generation,
            "stop-first",
            &execution,
            &binding,
        )
        .unwrap()
        .finish(ReleaseOutcome::ScopeEmpty)
        .unwrap();
        assert_eq!(stopped.history.last().unwrap().kind, "source_scope_empty");
        assert_eq!(stopped.owner, task.owner);
        assert_eq!(stopped.ownership_epoch, 1);
        assert!(!stopped.readiness.handoff_ready);
        assert!(check_current_actor(id, "source", stopped.generation).is_err());
        assert_eq!(show(id).unwrap(), stopped);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_release_rechecks_workspace_and_rejects_corrupt_outcomes() {
        let home = HomeSandbox::new();
        let task = ready(&home);
        let task = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "stop-first",
            "grok",
            "model",
        )
        .unwrap();
        let id = &task.task_id;
        let execution = "1".repeat(32);
        let binding = "2".repeat(64);
        fs::write(task.workspace.join("work.txt"), "changed before release").unwrap();
        assert!(
            begin_source_release(
                id,
                "source",
                task.generation,
                "stop-first",
                &execution,
                &binding
            )
            .is_err()
        );
        assert_eq!(show(id).unwrap(), task);
        fs::write(task.workspace.join("work.txt"), "unfinished").unwrap();
        let intent = begin_source_release(
            id,
            "source",
            task.generation,
            "stop-first",
            &execution,
            &binding,
        )
        .unwrap();
        fs::write(task.workspace.join("work.txt"), "changed during shutdown").unwrap();
        assert!(!intent.workspace_matches().unwrap());
        let parked = intent.finish(ReleaseOutcome::WorkspaceChanged).unwrap();
        let value = serde_json::to_value(&parked).unwrap();
        assert_eq!(
            value["handoffs"][0]["source_release"]["recorded_scope_empty"],
            true
        );
        assert_eq!(
            value["handoffs"][0]["source_release"]["failure"],
            "workspace_changed"
        );
        let dir = task_dir(id).unwrap();
        for case in 0..7 {
            let mut changed = value.clone();
            let release = &mut changed["handoffs"][0]["source_release"];
            match case {
                0 => release["state"] = serde_json::json!("scope_empty"),
                1 => release["recorded_scope_empty"] = serde_json::json!(false),
                2 => release["requested_generation"] = serde_json::json!(0),
                3 => release["updated_generation"] = serde_json::json!(u64::MAX),
                4 => release["binding_digest"] = serde_json::json!("not-a-digest"),
                5 => changed["handoffs"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("source_release")
                    .map(|_| ())
                    .unwrap(),
                _ => {
                    changed["history"]
                        .as_array_mut()
                        .unwrap()
                        .last_mut()
                        .unwrap()["kind"] = serde_json::json!("source_scope_empty")
                }
            }
            atomic_json(&dir, "task.json", &changed, MAX_RECORD_BYTES, true).unwrap();
            assert!(show(id).is_err(), "corrupt case {case}");
        }
        atomic_json(&dir, "task.json", &parked, MAX_RECORD_BYTES, true).unwrap();
        assert_eq!(show(id).unwrap(), parked);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_release_refuses_declared_resources_before_freezing_task() {
        let home = HomeSandbox::new();
        let task = ready(&home);
        let task = register_resource(
            &task.task_id,
            "source",
            task.generation,
            ResourceInput {
                id: "keep-job".into(),
                kind: "external".into(),
                native_identity: BTreeMap::from([("uri".into(), "fixture://job".into())]),
                purpose: "Preserve independently managed work".into(),
                ownership: "task".into(),
                disposition: "adopt".into(),
                may_write: true,
                reconnect: None,
                cleanup: None,
            },
        )
        .unwrap();
        let mut brief = task.latest_checkpoint.as_ref().unwrap().payload.clone();
        brief.resource_ids.push("keep-job".into());
        let task = checkpoint_capturing_workspace(&task.task_id, "source", task.generation, brief)
            .unwrap();
        let mut policy = task.sharing_policy.clone().unwrap();
        policy.share_resource_metadata = true;
        let task = set_policy(&task.task_id, "source", task.generation, policy).unwrap();
        let task = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "stop-first",
            "grok",
            "model",
        )
        .unwrap();
        assert!(
            begin_source_release(
                &task.task_id,
                "source",
                task.generation,
                "stop-first",
                &"1".repeat(32),
                &"2".repeat(64)
            )
            .is_err()
        );
        assert_eq!(show(&task.task_id).unwrap(), task);
        assert!(check_current_actor(&task.task_id, "source", task.generation).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_release_atomic_write_failures_preserve_recoverable_intent() {
        use std::os::unix::fs::PermissionsExt;
        let home = HomeSandbox::new();
        let task = ready(&home);
        let task = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "stop-first",
            "grok",
            "model",
        )
        .unwrap();
        let dir = task_dir(&task.task_id).unwrap();
        let begin = || {
            begin_source_release(
                &task.task_id,
                "source",
                task.generation,
                "stop-first",
                &"1".repeat(32),
                &"2".repeat(64),
            )
        };
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let failed = begin();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err());
        assert_eq!(show(&task.task_id).unwrap(), task);
        let intent = begin().unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let failed = intent.finish(ReleaseOutcome::ScopeEmpty);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err());
        let pending = show(&task.task_id).unwrap();
        assert_eq!(
            pending.history.last().unwrap().kind,
            "source_stop_requested"
        );
        assert!(check_current_actor(&task.task_id, "source", pending.generation).is_err());
        let recovered = begin().unwrap().finish(ReleaseOutcome::ScopeEmpty).unwrap();
        assert!(recovered.source_scope_released("stop-first"));
        assert_eq!(show(&task.task_id).unwrap(), recovered);
    }

    #[cfg(unix)]
    #[test]
    fn failed_atomic_proposal_write_leaves_original_generation_retryable() {
        use std::os::unix::fs::PermissionsExt;
        let home = HomeSandbox::new();
        let task = ready(&home);
        let dir = task_dir(&task.task_id).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let attempted = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "retry-key",
            "grok",
            "model",
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(attempted.is_err());
        assert_eq!(show(&task.task_id).unwrap(), task);
        let committed = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "retry-key",
            "grok",
            "model",
        )
        .unwrap();
        assert_eq!(committed.generation, task.generation + 1);
        assert_eq!(
            propose_handoff(
                &task.task_id,
                "source",
                task.generation,
                "retry-key",
                "grok",
                "model"
            )
            .unwrap(),
            committed
        );
    }

    #[test]
    fn supersession_must_close_at_the_first_source_change() {
        let home = HomeSandbox::new();
        let task = ready(&home);
        let task = propose_handoff(
            &task.task_id,
            "source",
            task.generation,
            "attempt",
            "grok",
            "model",
        )
        .unwrap();
        let policy = task.sharing_policy.clone().unwrap();
        let changed = set_policy(&task.task_id, "source", task.generation, policy.clone()).unwrap();
        let mut changed_again =
            set_policy(&task.task_id, "source", changed.generation, policy).unwrap();
        assert_eq!(
            changed_again.handoffs[0].closed_generation,
            Some(changed.generation)
        );
        let dir = task_dir(&task.task_id).unwrap();
        changed_again.handoffs[0].closed_generation = Some(changed_again.generation);
        atomic_json(&dir, "task.json", &changed_again, MAX_RECORD_BYTES, true).unwrap();
        assert!(show(&task.task_id).is_err());
        changed_again.handoffs[0].state = ProposalState::Cancelled;
        changed_again.history.last_mut().unwrap().kind = "handoff_cancelled".into();
        atomic_json(&dir, "task.json", &changed_again, MAX_RECORD_BYTES, true).unwrap();
        assert!(show(&task.task_id).is_err());
    }
}
