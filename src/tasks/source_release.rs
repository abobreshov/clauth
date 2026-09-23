//! Stop-first journal. Scope-empty is not native source fencing, resource
//! reconciliation, successor readiness, or a task ownership transfer.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReleaseState {
    StopRequested,
    ScopeEmpty,
    Parked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReleaseFailure {
    StopUnproven,
    WorkspaceChanged,
    WorkspaceUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceRelease {
    execution_id: String,
    /// Digest of the immutable local launch/scope binding, excluding exit and
    /// finalization fields. A retry must not rebind even the same execution ID.
    binding_digest: String,
    requested_generation: u64,
    updated_generation: u64,
    state: ReleaseState,
    recorded_scope_empty: bool,
    failure: Option<ReleaseFailure>,
}

impl TaskRecord {
    pub(crate) fn source_scope_released(&self, handoff_id: &str) -> bool {
        self.handoffs.iter().any(|proposal| {
            proposal.handoff_id == handoff_id
                && proposal.source_release.as_ref().is_some_and(|release| {
                    release.state == ReleaseState::ScopeEmpty && release.recorded_scope_empty
                })
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) enum ReleaseOutcome {
    ScopeEmpty,
    StopUnproven,
    WorkspaceChanged,
    WorkspaceUnavailable,
}

/// The execution caller holds the execution lock first and retains this task
/// lock through shutdown and result publication. A crash releases the locks,
/// but the persisted intent continues to exclude ordinary source mutations.
#[cfg(target_os = "linux")]
pub(crate) struct ReleaseGuard {
    record: TaskRecord,
    dir: PathBuf,
    index: usize,
    _lock: File,
}

#[cfg(target_os = "linux")]
pub(crate) fn begin_source_release(
    id: &str,
    actor: &str,
    generation: u64,
    handoff_id: &str,
    execution_id: &str,
    binding_digest: &str,
) -> Result<ReleaseGuard> {
    validate_id(execution_id)?;
    if !valid_key(handoff_id) || !valid_digest(binding_digest) {
        bail!("invalid source release identity");
    }
    let dir = task_dir(id)?;
    let lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    let index = record
        .handoffs
        .iter()
        .position(|p| p.handoff_id == handoff_id && p.state == ProposalState::Proposed)
        .ok_or_else(|| {
            anyhow::anyhow!("source release requires the exact active handoff proposal")
        })?;
    if actor != record.owner.native_session_id {
        bail!("source release actor does not match the registered source");
    }
    if let Some(prior) = &record.handoffs[index].source_release {
        // Exact retries can retain their original revision or use a freshly
        // read one. Intermediate/replaced intent identities are never selected.
        if prior.execution_id != execution_id
            || prior.binding_digest != binding_digest
            || (generation != prior.requested_generation - 1 && generation != record.generation)
        {
            bail!("source release retry differs from its immutable intent");
        }
    } else {
        guard(&record, actor, generation)?;
        // A saved classification has to match this binding, and the live probe
        // has to still say the source scope can stop without taking adopted work.
        super::disposition::recheck(&record, handoff_id, binding_digest)?;
        if !checkpoint_matches_workspace(&record)? {
            bail!(
                "workspace differs from the checkpoint; compact and capture before releasing the source"
            );
        }
        // Leave room for an outcome even if the history is nearly full.
        if record.history.len() > MAX_HISTORY - 2 {
            bail!("task history lacks room for source release and outcome");
        }
        advance(&mut record)?;
        record.handoffs[index].source_release = Some(SourceRelease {
            execution_id: execution_id.into(),
            binding_digest: binding_digest.into(),
            requested_generation: record.generation,
            updated_generation: record.generation,
            state: ReleaseState::StopRequested,
            recorded_scope_empty: false,
            failure: None,
        });
        record.history.push(event(&record, "source_stop_requested"));
        super::validate(&record, &dir)?;
        atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    }
    Ok(ReleaseGuard {
        record,
        dir,
        index,
        _lock: lock,
    })
}

#[cfg(target_os = "linux")]
fn checkpoint_matches_workspace(record: &TaskRecord) -> Result<bool> {
    let snapshot = record
        .latest_checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.workspace_snapshot.as_ref())
        .ok_or_else(|| anyhow::anyhow!("source release lacks a captured checkpoint"))?;
    Ok(workspace::capture(&record.workspace)? == *snapshot)
}

#[cfg(target_os = "linux")]
impl ReleaseGuard {
    pub(crate) fn workspace_matches(&self) -> Result<bool> {
        checkpoint_matches_workspace(&self.record)
    }

    pub(crate) fn finish(mut self, outcome: ReleaseOutcome) -> Result<TaskRecord> {
        let (state, empty, failure, kind) = match outcome {
            ReleaseOutcome::ScopeEmpty => {
                (ReleaseState::ScopeEmpty, true, None, "source_scope_empty")
            }
            ReleaseOutcome::StopUnproven => (
                ReleaseState::Parked,
                false,
                Some(ReleaseFailure::StopUnproven),
                "source_release_parked",
            ),
            ReleaseOutcome::WorkspaceChanged => (
                ReleaseState::Parked,
                true,
                Some(ReleaseFailure::WorkspaceChanged),
                "source_release_parked",
            ),
            ReleaseOutcome::WorkspaceUnavailable => (
                ReleaseState::Parked,
                true,
                Some(ReleaseFailure::WorkspaceUnavailable),
                "source_release_parked",
            ),
        };
        let previous = self.record.handoffs[self.index]
            .source_release
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("source release intent is missing"))?;
        if (
            previous.state,
            previous.recorded_scope_empty,
            previous.failure,
        ) == (state, empty, failure)
        {
            return Ok(self.record);
        }
        advance(&mut self.record)?;
        let release = self.record.handoffs[self.index]
            .source_release
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("source release intent is missing"))?;
        release.state = state;
        release.recorded_scope_empty = empty;
        release.failure = failure;
        release.updated_generation = self.record.generation;
        self.record.history.push(event(&self.record, kind));
        super::validate(&self.record, &self.dir)?;
        atomic_json(&self.dir, "task.json", &self.record, MAX_RECORD_BYTES, true)?;
        Ok(self.record)
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn validate(record: &TaskRecord) -> Result<()> {
    let mut expected_events = BTreeSet::new();
    for proposal in &record.handoffs {
        let Some(release) = &proposal.source_release else {
            continue;
        };
        validate_id(&release.execution_id)?;
        let plan_slots = u64::from(proposal.disposition_plan.is_some());
        let Some(expected_request) = proposal.proposed_generation.checked_add(1 + plan_slots)
        else {
            bail!("source release identity or revisions are invalid");
        };
        if !matches!(
            proposal.state,
            ProposalState::Proposed | ProposalState::Committed
        ) || !valid_digest(&release.binding_digest)
            || release.requested_generation != expected_request
            || release.updated_generation > record.generation
            || release.requested_generation > release.updated_generation
        {
            bail!("source release identity or revisions are invalid");
        }
        if record.resources.is_empty() {
            if proposal.disposition_plan.is_some() {
                bail!("source release identity or revisions are invalid");
            }
        } else if proposal.disposition_plan.as_ref().is_none_or(|plan| {
            !plan.stop_safe
                || plan.binding_digest != release.binding_digest
                || plan.resource_revision != record.resource_revision
                || plan.classified_generation + 1 != release.requested_generation
        }) {
            bail!("source release identity or revisions are invalid");
        }
        for generation in release.requested_generation..=release.updated_generation {
            let kind = record
                .history
                .get((generation - 1) as usize)
                .map(|e| e.kind.as_str());
            let valid = if generation == release.requested_generation {
                kind == Some("source_stop_requested")
            } else {
                matches!(kind, Some("source_scope_empty" | "source_release_parked"))
            };
            if !valid || !expected_events.insert(generation) {
                bail!("source release history is inconsistent");
            }
        }
        let expected_kind = match (release.state, release.recorded_scope_empty, release.failure) {
            (ReleaseState::StopRequested, false, None)
                if release.requested_generation == release.updated_generation =>
            {
                "source_stop_requested"
            }
            (ReleaseState::ScopeEmpty, true, None) => "source_scope_empty",
            (ReleaseState::Parked, false, Some(ReleaseFailure::StopUnproven))
            | (
                ReleaseState::Parked,
                true,
                Some(ReleaseFailure::WorkspaceChanged | ReleaseFailure::WorkspaceUnavailable),
            ) => "source_release_parked",
            _ => bail!("source release outcome is inconsistent"),
        };
        if record
            .history
            .get((release.updated_generation - 1) as usize)
            .map(|event| event.kind.as_str())
            != Some(expected_kind)
        {
            bail!("source release outcome disagrees with history");
        }
    }
    let actual_events: BTreeSet<_> = record
        .history
        .iter()
        .filter(|e| {
            matches!(
                e.kind.as_str(),
                "source_stop_requested" | "source_scope_empty" | "source_release_parked"
            )
        })
        .map(|e| e.generation)
        .collect();
    if actual_events != expected_events {
        bail!("source release events lack a corresponding intent");
    }
    Ok(())
}
