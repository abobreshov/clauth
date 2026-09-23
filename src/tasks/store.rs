//! Durable, explicit checkpoints only. These declarations do not transfer
//! execution ownership, verify writers, or adopt/release any native resource.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::workspace::{self, WorkspaceSnapshot};

#[path = "handoff.rs"]
mod handoff;
pub(crate) use handoff::{
    HandoffProposal, cancel_handoff, classified_binding, classify_resources, continue_handoff,
    handoff_destination, propose_handoff, reconcile_resources, recover_handoff,
    source_launch_allowed, successor_worker,
};
#[cfg(target_os = "linux")]
pub(crate) use handoff::{ReleaseOutcome, begin_source_release};

const MAX_RECORD_BYTES: usize = 1_048_576;
const MAX_CHECKPOINT_BYTES: usize = 262_144;
const MAX_ITEMS: usize = 256;
const MAX_TEXT_BYTES: usize = 16_384;
const MAX_HISTORY: usize = 4096;
const AUTHORITY: &str = "unmanaged_checkpoint_only";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionIdentity {
    pub(crate) tool: String,
    pub(crate) model: Option<String>,
    pub(crate) native_session_id: String,
    pub(crate) account_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewTask {
    pub(crate) objective: String,
    pub(crate) workspace: PathBuf,
    pub(crate) constraints: Vec<String>,
    pub(crate) source: ExecutionIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointInput {
    pub(crate) brief: String,
    pub(crate) completed: Vec<String>,
    pub(crate) remaining_plan: Vec<String>,
    pub(crate) decisions: Vec<String>,
    pub(crate) uncertainties: Vec<String>,
    pub(crate) next_action: String,
    pub(crate) constraints: Vec<String>,
    pub(crate) resource_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceInput {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) native_identity: BTreeMap<String, String>,
    pub(crate) purpose: String,
    pub(crate) ownership: String,
    pub(crate) disposition: String,
    pub(crate) may_write: bool,
    pub(crate) reconnect: Option<String>,
    pub(crate) cleanup: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointRecord {
    pub(crate) generation: u64,
    pub(crate) resource_revision: u64,
    pub(crate) digest: String,
    pub(crate) payload: CheckpointInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_snapshot: Option<WorkspaceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DestinationRule {
    pub(crate) tool: String,
    /// Exact model names, or the sole entry "*" for an explicit all-model rule.
    pub(crate) models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharingPolicy {
    pub(crate) destinations: Vec<DestinationRule>,
    pub(crate) share_checkpoint: bool,
    pub(crate) share_workspace: bool,
    pub(crate) share_resource_metadata: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct HandoffPreflight {
    pub(crate) task_id: String,
    pub(crate) generation: u64,
    pub(crate) destination_tool: String,
    pub(crate) destination_model: String,
    pub(crate) checkpoint_digest: Option<String>,
    pub(crate) workspace_matches_checkpoint: Option<bool>,
    pub(crate) workspace_scope: Option<String>,
    pub(crate) workspace_exclusions: Vec<String>,
    /// A match against local recorded policy, never authenticated user consent.
    pub(crate) sharing_allowed: bool,
    pub(crate) consent_authenticated: bool,
    pub(crate) authority_mode: String,
    pub(crate) checkpoint_covers_resources: bool,
    pub(crate) handoff_ready: bool,
    pub(crate) blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryEvent {
    pub(crate) generation: u64,
    pub(crate) kind: String,
    pub(crate) actor: String,
    pub(crate) recorded_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckpointReadiness {
    pub(crate) handoff_ready: bool,
    pub(crate) workspace_fingerprint_verified: bool,
    pub(crate) native_resources_verified: bool,
    pub(crate) checkpoint_matches_resource_revision: bool,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskRecord {
    pub(crate) schema: u32,
    pub(crate) task_id: String,
    pub(crate) generation: u64,
    pub(crate) ownership_epoch: u64,
    pub(crate) authority_mode: String,
    pub(crate) workspace: PathBuf,
    pub(crate) objective: String,
    pub(crate) constraints: Vec<String>,
    pub(crate) owner: ExecutionIdentity,
    pub(crate) resource_revision: u64,
    pub(crate) resources: Vec<ResourceInput>,
    pub(crate) latest_checkpoint: Option<CheckpointRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sharing_policy: Option<SharingPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) handoffs: Vec<HandoffProposal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) continued_from: Option<ExecutionIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) handed_off_to: Option<ExecutionIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) transfer: Option<handoff::TransferState>,
    pub(crate) history: Vec<HistoryEvent>,
    pub(crate) readiness: CheckpointReadiness,
}

fn confirmed_limitations() -> Vec<String> {
    vec![
        "Stop-first handoff committed after both session-linked receipts were read back.".into(),
        "The ownership epoch advanced only after those receipts, not after launch acknowledgement."
            .into(),
        "Declared resources were reconciled. Undeclared, escaped, or remote writers are not proven absent."
            .into(),
        "Successor pid identity is recorded and is not a cgroup fence around later native tools."
            .into(),
        "Native transcript files were not rewritten. Receipts are session-bound sidecars.".into(),
    ]
}

fn expected_readiness(record: &TaskRecord) -> CheckpointReadiness {
    let current = record
        .latest_checkpoint
        .as_ref()
        .is_some_and(|checkpoint| checkpoint.resource_revision == record.resource_revision);
    let mut readiness = readiness(
        current,
        has_workspace_snapshot(record),
        record.sharing_policy.is_some(),
    );
    if record.ownership_epoch == 2 {
        readiness.handoff_ready = true;
        readiness.limitations = confirmed_limitations();
    }
    readiness
}

pub(super) fn assign_readiness(record: &mut TaskRecord) {
    record.readiness = expected_readiness(record);
}

fn readiness(current: bool, snapshot: bool, policy: bool) -> CheckpointReadiness {
    CheckpointReadiness {
        handoff_ready: false,
        workspace_fingerprint_verified: false,
        native_resources_verified: false,
        checkpoint_matches_resource_revision: current,
        limitations: vec![
            "Checkpoint-only record; no execution ownership has been transferred.".into(),
            if snapshot {
                "Workspace fingerprint is recorded, not a write fence; revalidate before transfer."
            } else {
                "Workspace contents and fingerprint have not been captured or verified."
            }.into(),
            "Native writers and resource adoption/release have not been verified.".into(),
            if policy {
                "Recorded sharing policy must match the destination; it is not authenticated consent."
            } else {
                "Destination and data-sharing policy is absent; this record grants no provider transfer permission."
            }.into(),
            "Workspace is declared scope, not an enforced access boundary.".into(),
        ],
    }
}

pub(crate) fn register(input: NewTask) -> Result<TaskRecord> {
    check_text(&input.objective, true)?;
    check_list(&input.constraints)?;
    unique(&input.constraints)?;
    check_identity(&input.source)?;
    let workspace = input
        .workspace
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("workspace cannot be resolved"))?;
    if !workspace.is_dir() {
        bail!("workspace must be an existing directory");
    }
    let root = task_root()?;
    private_dir(&root)?;
    private_permissions(&root)?;
    for _ in 0..16 {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("cannot generate task identifier"))?;
        let task_id = hex::encode(bytes);
        let dir = root.join(&task_id);
        match create_private_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => bail!("cannot create task directory"),
        }
        sync_dir(&root)?;
        let _lock = lock(&dir)?;
        let record = TaskRecord {
            schema: 1,
            task_id,
            generation: 1,
            ownership_epoch: 1,
            authority_mode: AUTHORITY.into(),
            workspace,
            objective: input.objective,
            constraints: input.constraints,
            owner: input.source,
            resource_revision: 0,
            resources: vec![],
            latest_checkpoint: None,
            sharing_policy: None,
            handoffs: vec![],
            continued_from: None,
            handed_off_to: None,
            transfer: None,
            history: vec![],
            readiness: readiness(false, false, false),
        };
        let mut record = record;
        record.history.push(event(&record, "registered"));
        atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, false)?;
        return Ok(record);
    }
    bail!("cannot allocate a unique task identifier")
}

pub(crate) fn show(id: &str) -> Result<TaskRecord> {
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    read_record(&dir, id)
}

/// Consistency guard for explicit local-process operations. This does not
/// authenticate the caller, bind a native session, or grant handoff authority.
pub(crate) fn check_current_actor(id: &str, actor: &str, generation: u64) -> Result<TaskRecord> {
    let (record, _lock) = lock_current_actor(id, actor, generation)?;
    Ok(record)
}

/// Keep the revision stable through a local execution commit point. Not an
/// authentication boundary; the caller must retain the returned lock.
pub(crate) fn lock_current_actor(
    id: &str,
    actor: &str,
    generation: u64,
) -> Result<(TaskRecord, File)> {
    let dir = task_dir(id)?;
    let lock = lock(&dir)?;
    let record = read_record(&dir, id)?;
    guard(&record, actor, generation)?;
    Ok((record, lock))
}

pub(crate) fn checkpoint(
    id: &str,
    actor: &str,
    expected_generation: u64,
    input: CheckpointInput,
) -> Result<TaskRecord> {
    save_checkpoint(id, actor, expected_generation, input, false)
}

pub(crate) fn checkpoint_capturing_workspace(
    id: &str,
    actor: &str,
    expected_generation: u64,
    input: CheckpointInput,
) -> Result<TaskRecord> {
    save_checkpoint(id, actor, expected_generation, input, true)
}

fn save_checkpoint(
    id: &str,
    actor: &str,
    expected_generation: u64,
    input: CheckpointInput,
    capture_workspace: bool,
) -> Result<TaskRecord> {
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    guard(&record, actor, expected_generation)?;
    check_checkpoint(&input)?;
    if unique(&input.constraints)? != unique(&record.constraints)? {
        bail!("checkpoint must preserve every original constraint exactly");
    }
    let resource_ids: Vec<_> = record.resources.iter().map(|r| r.id.clone()).collect();
    if unique(&input.resource_ids)? != unique(&resource_ids)? {
        bail!("checkpoint resource IDs must exactly cover the resource registry");
    }
    advance(&mut record)?;
    let mut checkpoint = CheckpointRecord {
        generation: record.generation,
        resource_revision: record.resource_revision,
        digest: String::new(),
        payload: input,
        workspace_snapshot: if capture_workspace {
            // Avoid observing our own changing task journal as workspace state.
            if dir.starts_with(&record.workspace) {
                bail!("workspace cannot contain the task storage directory");
            }
            let snapshot = workspace::capture(&record.workspace)?;
            if snapshot.root != record.workspace {
                bail!("workspace identity changed since task registration");
            }
            Some(snapshot)
        } else {
            None
        },
    };
    checkpoint.digest = digest(&checkpoint)?;
    let name = checkpoint_name(checkpoint.generation);
    if dir.join(&name).exists() {
        // An interrupted previous write can leave an immutable checkpoint
        // before task.json commits. Reuse only byte-equivalent logical content.
        let existing: CheckpointRecord = read_json(&dir.join(&name), MAX_CHECKPOINT_BYTES)?;
        if existing != checkpoint {
            bail!("checkpoint generation already exists with different content");
        }
    } else {
        atomic_json(&dir, &name, &checkpoint, MAX_CHECKPOINT_BYTES, false)?;
    }
    record.latest_checkpoint = Some(checkpoint);
    handoff::supersede(&mut record);
    assign_readiness(&mut record);
    record.history.push(event(&record, "checkpointed"));
    atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    Ok(record)
}

pub(crate) fn register_resource(
    id: &str,
    actor: &str,
    expected_generation: u64,
    input: ResourceInput,
) -> Result<TaskRecord> {
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    guard(&record, actor, expected_generation)?;
    check_resource(&input)?;
    if record.resources.len() >= MAX_ITEMS {
        bail!("resource registry is full");
    }
    if record.resources.iter().any(|r| r.id == input.id) {
        bail!("resource ID is already registered");
    }
    advance(&mut record)?;
    record.resource_revision = record
        .resource_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("resource revision exhausted"))?;
    record.resources.push(input);
    handoff::supersede(&mut record);
    assign_readiness(&mut record);
    record.history.push(event(&record, "resource_declared"));
    atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    Ok(record)
}

fn has_workspace_snapshot(record: &TaskRecord) -> bool {
    record
        .latest_checkpoint
        .as_ref()
        .is_some_and(|c| c.workspace_snapshot.is_some())
}

pub(crate) fn set_policy(
    id: &str,
    actor: &str,
    expected_generation: u64,
    policy: SharingPolicy,
) -> Result<TaskRecord> {
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let mut record = read_record(&dir, id)?;
    guard(&record, actor, expected_generation)?;
    check_policy(&policy)?;
    advance(&mut record)?;
    record.sharing_policy = Some(policy);
    handoff::supersede(&mut record);
    assign_readiness(&mut record);
    record.history.push(event(&record, "policy_updated"));
    atomic_json(&dir, "task.json", &record, MAX_RECORD_BYTES, true)?;
    Ok(record)
}

pub(crate) fn check_handoff(id: &str, tool: &str, model: &str) -> Result<HandoffPreflight> {
    check_tool(tool)?;
    check_text(model, true)?;
    if model == "*" {
        bail!("preflight requires the actual destination model, not a wildcard");
    }
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    let record = read_record(&dir, id)?;
    let sharing_allowed = handoff::sharing_allowed(&record, tool, model);
    let mut blockers = vec![
        "Native source release, surviving writers and resource dispositions are not verified."
            .into(),
        "Successor preparation, session receipts and ownership transfer are not implemented."
            .into(),
    ];
    if !sharing_allowed {
        blockers.push(
            "Destination/model or required data sharing is not allowed by the recorded policy."
                .into(),
        );
    }
    let workspace_matches_checkpoint = match record
        .latest_checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.workspace_snapshot.as_ref())
    {
        Some(snapshot) => match workspace::capture(&record.workspace) {
            Ok(current) => {
                let matches = current == *snapshot;
                if !matches {
                    blockers.push(
                        "Workspace differs from the checkpoint; compact and capture again.".into(),
                    );
                }
                Some(matches)
            }
            Err(_) => {
                blockers
                    .push("Workspace could not be safely re-observed; match is unknown.".into());
                None
            }
        },
        None => {
            blockers.push("Checkpoint has no workspace fingerprint.".into());
            None
        }
    };
    if !record.readiness.checkpoint_matches_resource_revision {
        blockers
            .push("Checkpoint is missing or does not cover the current resource revision.".into());
    }
    Ok(HandoffPreflight {
        task_id: record.task_id,
        generation: record.generation,
        destination_tool: tool.into(),
        destination_model: model.into(),
        workspace_scope: record
            .latest_checkpoint
            .as_ref()
            .and_then(|c| c.workspace_snapshot.as_ref())
            .map(|s| s.scope.clone()),
        workspace_exclusions: record
            .latest_checkpoint
            .as_ref()
            .and_then(|c| c.workspace_snapshot.as_ref())
            .map(|s| s.exclusions.clone())
            .unwrap_or_default(),
        checkpoint_digest: record.latest_checkpoint.map(|c| c.digest),
        workspace_matches_checkpoint,
        sharing_allowed,
        consent_authenticated: false,
        authority_mode: AUTHORITY.into(),
        checkpoint_covers_resources: record.readiness.checkpoint_matches_resource_revision,
        handoff_ready: false,
        blockers,
    })
}

fn guard(record: &TaskRecord, actor: &str, generation: u64) -> Result<()> {
    if actor != record.owner.native_session_id {
        bail!("actor does not match the registered source session");
    }
    if generation != record.generation {
        bail!("stale task generation; read the task again before retrying");
    }
    handoff::guard_source_mutation(record)?;
    if record.ownership_epoch > 1 {
        handoff::resume_loaded(record, &task_dir(&record.task_id)?)?;
    }
    Ok(())
}

fn advance(record: &mut TaskRecord) -> Result<()> {
    if record.history.len() >= MAX_HISTORY {
        bail!("task history limit reached");
    }
    record.generation = record
        .generation
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("task generation exhausted"))?;
    Ok(())
}

fn event(record: &TaskRecord, kind: &str) -> HistoryEvent {
    HistoryEvent {
        generation: record.generation,
        kind: kind.into(),
        actor: record.owner.native_session_id.clone(),
        recorded_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    }
}

fn check_text(value: &str, nonempty: bool) -> Result<()> {
    if value.len() > MAX_TEXT_BYTES || value.contains('\0') || (nonempty && value.trim().is_empty())
    {
        bail!("text field is empty, invalid, or exceeds the size limit");
    }
    Ok(())
}

fn check_list(values: &[String]) -> Result<()> {
    if values.len() > MAX_ITEMS {
        bail!("list exceeds the item limit");
    }
    for value in values {
        check_text(value, true)?;
    }
    Ok(())
}

fn unique(values: &[String]) -> Result<BTreeSet<&str>> {
    let set: BTreeSet<_> = values.iter().map(String::as_str).collect();
    if set.len() != values.len() {
        bail!("duplicate identifiers or constraints are not allowed");
    }
    Ok(set)
}

fn check_tool(tool: &str) -> Result<()> {
    if !matches!(tool, "claude" | "codex" | "grok" | "agy") {
        bail!("unsupported tool");
    }
    Ok(())
}

fn check_policy(policy: &SharingPolicy) -> Result<()> {
    if policy.destinations.len() > 4 {
        bail!("sharing policy has too many destination rules");
    }
    let mut tools = BTreeSet::new();
    for rule in &policy.destinations {
        check_tool(&rule.tool)?;
        if !tools.insert(rule.tool.as_str()) {
            bail!("sharing policy has duplicate destination rules");
        }
        check_list(&rule.models)?;
        unique(&rule.models)?;
        if rule.models.is_empty() || (rule.models.len() > 1 && rule.models.iter().any(|m| m == "*"))
        {
            bail!("destination requires exact models or a single explicit wildcard");
        }
    }
    Ok(())
}

fn check_identity(identity: &ExecutionIdentity) -> Result<()> {
    check_tool(&identity.tool)?;
    check_text(&identity.native_session_id, true)?;
    for value in [&identity.model, &identity.account_ref]
        .into_iter()
        .flatten()
    {
        check_text(value, true)?;
    }
    Ok(())
}

fn safe_resource_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn check_resource(resource: &ResourceInput) -> Result<()> {
    if !safe_resource_id(&resource.id) {
        bail!("invalid resource ID");
    }
    check_text(&resource.purpose, true)?;
    if !matches!(resource.ownership.as_str(), "task" | "shared")
        || !matches!(
            resource.disposition.as_str(),
            "adopt" | "observe" | "release" | "block"
        )
    {
        bail!("invalid resource ownership or disposition");
    }
    if resource.ownership == "shared"
        && matches!(resource.disposition.as_str(), "release" | "adopt")
    {
        bail!("shared resources can only be observed or block a future handoff");
    }
    if resource.native_identity.len() > 32 {
        bail!("native identity exceeds the field limit");
    }
    for (key, value) in &resource.native_identity {
        let key = key.to_ascii_lowercase();
        if key.is_empty()
            || key.len() > 128
            || [
                "token",
                "password",
                "secret",
                "authorization",
                "credential",
                "cookie",
                "api_key",
                "apikey",
                "private_key",
                "accesskey",
            ]
            .iter()
            .any(|word| key.contains(word))
        {
            bail!("native identity contains an invalid or credential-like key");
        }
        check_text(value, true)?;
    }
    let keys: &[&str] = match resource.kind.as_str() {
        "herdr_pane" => &["instance", "session", "workspace_id", "tab_id", "pane_id"],
        "process" => &["pid", "start_identity", "host"],
        "external" => &["uri"],
        _ => bail!("unsupported resource kind"),
    };
    if keys
        .iter()
        .any(|key| !resource.native_identity.contains_key(*key))
    {
        bail!("native identity lacks required fields for its resource kind");
    }
    if resource.kind == "process"
        && resource.native_identity["pid"]
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .is_none()
    {
        bail!("process identity requires a positive numeric PID");
    }
    for value in [&resource.reconnect, &resource.cleanup]
        .into_iter()
        .flatten()
    {
        check_text(value, true)?;
    }
    Ok(())
}

fn check_checkpoint(input: &CheckpointInput) -> Result<()> {
    check_text(&input.brief, true)?;
    check_text(&input.next_action, true)?;
    for values in [
        &input.completed,
        &input.remaining_plan,
        &input.decisions,
        &input.uncertainties,
        &input.constraints,
        &input.resource_ids,
    ] {
        check_list(values)?;
    }
    unique(&input.constraints)?;
    unique(&input.resource_ids)?;
    if input.resource_ids.iter().any(|id| !safe_resource_id(id)) {
        bail!("invalid checkpoint resource ID");
    }
    Ok(())
}

fn digest(checkpoint: &CheckpointRecord) -> Result<String> {
    // Preserve verification of existing checkpoint-only records. Captured
    // workspace metadata is part of the immutable digest when present.
    let bytes = if let Some(snapshot) = &checkpoint.workspace_snapshot {
        serde_json::to_vec(&(
            checkpoint.generation,
            checkpoint.resource_revision,
            &checkpoint.payload,
            snapshot,
        ))
    } else {
        serde_json::to_vec(&(
            checkpoint.generation,
            checkpoint.resource_revision,
            &checkpoint.payload,
        ))
    }
    .map_err(|_| anyhow::anyhow!("cannot encode checkpoint"))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn checkpoint_name(generation: u64) -> String {
    format!("checkpoint-{generation}.json")
}

fn read_record(dir: &Path, id: &str) -> Result<TaskRecord> {
    let record: TaskRecord = read_json(&dir.join("task.json"), MAX_RECORD_BYTES)?;
    if record.schema != 1
        || record.task_id != id
        || record.authority_mode != AUTHORITY
        || record.ownership_epoch == 0
        || record.ownership_epoch > 2
        || record.generation == 0
        || !record.workspace.is_absolute()
    {
        bail!("task record has an unsupported schema or invalid identity");
    }
    check_text(&record.objective, true)?;
    check_list(&record.constraints)?;
    unique(&record.constraints)?;
    check_identity(&record.owner)?;
    if let Some(policy) = &record.sharing_policy {
        check_policy(policy)?;
    }
    if record.resources.len() > MAX_ITEMS
        || record.resource_revision != record.resources.len() as u64
        || record.history.len() > MAX_HISTORY
        || record.history.len() as u64 != record.generation
    {
        bail!("task record has invalid revision history");
    }
    let ids: Vec<_> = record.resources.iter().map(|r| r.id.clone()).collect();
    unique(&ids)?;
    for resource in &record.resources {
        check_resource(resource)?;
    }
    let historical_actor = if record.ownership_epoch == 1 {
        record.owner.native_session_id.as_str()
    } else {
        record
            .continued_from
            .as_ref()
            .map(|identity| identity.native_session_id.as_str())
            .unwrap_or("")
    };
    if historical_actor.is_empty() {
        bail!("task record has invalid revision history");
    }
    for (index, event) in record.history.iter().enumerate() {
        if event.generation != index as u64 + 1
            || event.actor != historical_actor
            || (index == 0 && event.kind != "registered")
            || (index > 0
                && !matches!(
                    event.kind.as_str(),
                    "checkpointed"
                        | "resource_declared"
                        | "policy_updated"
                        | "handoff_proposed"
                        | "handoff_cancelled"
                        | "source_stop_requested"
                        | "source_scope_empty"
                        | "source_release_parked"
                        | "resources_classified"
                        | "resources_reconciled"
                        | "resources_parked"
                        | "successor_prepared"
                        | "checkpoint_delivered"
                        | "receipts_pending"
                        | "ownership_committed"
                        | "handoff_parked"
                ))
        {
            bail!("task record has invalid revision history");
        }
    }
    let resource_events = record
        .history
        .iter()
        .filter(|e| e.kind == "resource_declared")
        .count();
    let last_checkpoint_event = record
        .history
        .iter()
        .rev()
        .find(|e| e.kind == "checkpointed");
    if resource_events as u64 != record.resource_revision
        || last_checkpoint_event.map(|e| e.generation)
            != record.latest_checkpoint.as_ref().map(|c| c.generation)
    {
        bail!("task record history disagrees with its resources or checkpoint");
    }
    if record.history.iter().any(|e| e.kind == "policy_updated") != record.sharing_policy.is_some()
    {
        bail!("task record history disagrees with its sharing policy");
    }
    handoff::validate(&record, dir)?;
    if let Some(checkpoint) = &record.latest_checkpoint {
        check_checkpoint(&checkpoint.payload)?;
        if let Some(snapshot) = &checkpoint.workspace_snapshot {
            workspace::validate(snapshot)?;
            if snapshot.root != record.workspace {
                bail!("checkpoint workspace identity differs from task workspace");
            }
        }
        if checkpoint.generation > record.generation
            || checkpoint.generation < 2
            || checkpoint.resource_revision > record.resource_revision
            || digest(checkpoint)? != checkpoint.digest
            || unique(&checkpoint.payload.constraints)? != unique(&record.constraints)?
        {
            bail!("checkpoint metadata or digest verification failed");
        }
        let expected_ids: Vec<_> = record
            .resources
            .iter()
            .take(checkpoint.resource_revision as usize)
            .map(|r| r.id.clone())
            .collect();
        if unique(&checkpoint.payload.resource_ids)? != unique(&expected_ids)? {
            bail!("checkpoint resource coverage verification failed");
        }
        let immutable: CheckpointRecord = read_json(
            &dir.join(checkpoint_name(checkpoint.generation)),
            MAX_CHECKPOINT_BYTES,
        )?;
        if immutable != *checkpoint {
            bail!("immutable checkpoint differs from task record");
        }
    }
    if record.readiness != expected_readiness(&record) {
        bail!("task record contains unsupported readiness claims");
    }
    Ok(record)
}

fn validate_id(id: &str) -> Result<()> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("invalid task ID");
    }
    Ok(())
}

fn task_root() -> Result<PathBuf> {
    let root =
        crate::profile::clauth_dir().map_err(|_| anyhow::anyhow!("cannot locate task storage"))?;
    private_dir(&root)?;
    Ok(root.join("tasks"))
}

fn task_dir(id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    let root = task_root()?;
    existing_dir(&root)?;
    private_permissions(&root)?;
    let dir = root.join(id);
    existing_dir(&dir)?;
    private_permissions(&dir)?;
    Ok(dir)
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn existing_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("task storage directory is unavailable"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("task storage must use real directories");
    }
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    match create_private_dir(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_dir(parent)?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => existing_dir(path)?,
        Err(_) => bail!("cannot create private task storage"),
    }
    Ok(())
}

fn private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| anyhow::anyhow!("cannot inspect task directory permissions"))?;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("task directory must have private permissions");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn file_options() -> OpenOptions {
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

fn validate_file(file: &File) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("cannot inspect task file"))?;
    if !metadata.is_file() {
        bail!("task storage entry is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("task file must have private permissions");
        }
    }
    Ok(())
}

fn lock(dir: &Path) -> Result<File> {
    let path = dir.join(".lock");
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        bail!("task lock must be a regular file, not a link");
    }
    let mut options = file_options();
    let file = options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| anyhow::anyhow!("cannot open task lock"))?;
    validate_file(&file)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("task is busy; retry after the current operation finishes")
        }
        Err(_) => bail!("cannot acquire task lock"),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path, max: usize) -> Result<T> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("task storage file is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("task storage entry must be a regular file, not a link");
    }
    let mut options = file_options();
    let file = options
        .read(true)
        .open(path)
        .map_err(|_| anyhow::anyhow!("task storage file is unavailable"))?;
    validate_file(&file)?;
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot read task storage"))?;
    if bytes.len() > max {
        bail!("task storage file exceeds the size limit");
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("task storage contains invalid or unsupported JSON"))
}

fn atomic_json(
    dir: &Path,
    name: &str,
    value: &impl Serialize,
    max: usize,
    replace: bool,
) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| anyhow::anyhow!("cannot encode task storage"))?;
    if bytes.len() > max {
        bail!("task storage record exceeds the size limit");
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|_| anyhow::anyhow!("cannot stage task storage"))?;
    temp.write_all(&bytes)
        .map_err(|_| anyhow::anyhow!("cannot write task storage"))?;
    temp.as_file()
        .sync_all()
        .map_err(|_| anyhow::anyhow!("cannot sync task storage"))?;
    let target = dir.join(name);
    if let Ok(metadata) = fs::symlink_metadata(&target)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        bail!("task storage target must be a regular file, not a link");
    }
    if replace {
        temp.persist(&target)
            .map_err(|_| anyhow::anyhow!("cannot commit task storage"))?;
    } else {
        temp.persist_noclobber(&target).map_err(|_| {
            anyhow::anyhow!("immutable task storage already exists or cannot be committed")
        })?;
    }
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|_| anyhow::anyhow!("cannot sync task storage directory"))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::HomeSandbox;

    fn new_task(home: &HomeSandbox) -> NewTask {
        NewTask {
            objective: "Implement an example".into(),
            workspace: home.home().to_path_buf(),
            constraints: vec!["Preserve user edits".into()],
            source: ExecutionIdentity {
                tool: "codex".into(),
                model: Some("example-model".into()),
                native_session_id: "source-session".into(),
                account_ref: None,
            },
        }
    }
    fn input() -> CheckpointInput {
        CheckpointInput {
            brief: "Code inspected".into(),
            completed: vec!["Read source".into()],
            remaining_plan: vec!["Implement".into()],
            decisions: vec![],
            uncertainties: vec![],
            next_action: "Implement the requested behavior".into(),
            constraints: vec!["Preserve user edits".into()],
            resource_ids: vec![],
        }
    }
    fn resource() -> ResourceInput {
        ResourceInput {
            id: "tests".into(),
            kind: "herdr_pane".into(),
            native_identity: BTreeMap::from([
                ("instance".into(), "example-instance".into()),
                ("session".into(), "example-session".into()),
                ("workspace_id".into(), "example-workspace".into()),
                ("tab_id".into(), "example-tab".into()),
                ("pane_id".into(), "example-pane".into()),
            ]),
            purpose: "Running tests".into(),
            ownership: "task".into(),
            disposition: "adopt".into(),
            may_write: true,
            reconnect: None,
            cleanup: None,
        }
    }
    fn allow_grok() -> SharingPolicy {
        SharingPolicy {
            destinations: vec![DestinationRule {
                tool: "grok".into(),
                models: vec!["*".into()],
            }],
            share_checkpoint: true,
            share_workspace: true,
            share_resource_metadata: true,
        }
    }
    fn mutate_record(id: &str, change: impl FnOnce(&mut serde_json::Value)) {
        let path = task_dir(id).unwrap().join("task.json");
        let mut value: serde_json::Value = read_json(&path, MAX_RECORD_BYTES).unwrap();
        change(&mut value);
        atomic_json(
            path.parent().unwrap(),
            "task.json",
            &value,
            MAX_RECORD_BYTES,
            true,
        )
        .unwrap();
    }

    #[test]
    fn persistent_roundtrip_has_no_handoff_authority() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        assert_eq!(task.generation, 1);
        assert_eq!(task.ownership_epoch, 1);
        assert_eq!(task.authority_mode, "unmanaged_checkpoint_only");
        assert_eq!(show(&task.task_id).unwrap(), task);
        let saved = checkpoint(&task.task_id, "source-session", 1, input()).unwrap();
        assert_eq!(saved.generation, 2);
        assert_eq!(show(&task.task_id).unwrap(), saved);
        assert!(saved.readiness.checkpoint_matches_resource_revision);
        assert!(!saved.readiness.handoff_ready);
        assert!(!saved.readiness.native_resources_verified);
        assert!(!saved.readiness.workspace_fingerprint_verified);
    }

    #[test]
    fn stale_generation_and_wrong_actor_cannot_mutate() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        assert!(checkpoint(&task.task_id, "another-session", 1, input()).is_err());
        assert!(register_resource(&task.task_id, "source-session", 0, resource()).is_err());
        assert_eq!(show(&task.task_id).unwrap(), task);
        checkpoint(&task.task_id, "source-session", 1, input()).unwrap();
        assert!(checkpoint(&task.task_id, "source-session", 1, input()).is_err());
    }

    #[test]
    fn policy_validation_is_explicit_and_fail_closed() {
        let mut policy = allow_grok();
        assert!(check_policy(&policy).is_ok());
        policy.destinations[0].models.push("specific-model".into());
        assert!(check_policy(&policy).is_err());
        policy = allow_grok();
        policy.destinations[0].models.clear();
        assert!(check_policy(&policy).is_err());
        policy = allow_grok();
        policy.destinations.push(policy.destinations[0].clone());
        assert!(check_policy(&policy).is_err());
        policy = allow_grok();
        policy.destinations[0].tool = "unsupported".into();
        assert!(check_policy(&policy).is_err());
        assert!(
            serde_json::from_value::<SharingPolicy>(serde_json::json!({"destinations":[]}))
                .is_err()
        );
    }

    #[test]
    fn preflight_requires_each_relevant_sharing_scope_and_cannot_grant_authority() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        let saved = set_policy(&task.task_id, "source-session", 1, allow_grok()).unwrap();
        assert_eq!(show(&task.task_id).unwrap(), saved);
        let check = check_handoff(&task.task_id, "grok", "specific-model").unwrap();
        assert!(check.sharing_allowed);
        assert!(!check.handoff_ready);
        assert_eq!(check.workspace_matches_checkpoint, None);
        assert!(check_handoff(&task.task_id, "grok", "*").is_err());
        assert!(
            !check_handoff(&task.task_id, "codex", "specific-model")
                .unwrap()
                .sharing_allowed
        );
        register_resource(&task.task_id, "source-session", 2, resource()).unwrap();
        let mut policy = allow_grok();
        policy.share_resource_metadata = false;
        set_policy(&task.task_id, "source-session", 3, policy.clone()).unwrap();
        assert!(
            !check_handoff(&task.task_id, "grok", "specific-model")
                .unwrap()
                .sharing_allowed
        );
        policy.share_resource_metadata = true;
        policy.share_checkpoint = false;
        set_policy(&task.task_id, "source-session", 4, policy.clone()).unwrap();
        assert!(
            !check_handoff(&task.task_id, "grok", "specific-model")
                .unwrap()
                .sharing_allowed
        );
        policy.share_checkpoint = true;
        policy.share_workspace = false;
        set_policy(&task.task_id, "source-session", 5, policy).unwrap();
        assert!(
            !check_handoff(&task.task_id, "grok", "specific-model")
                .unwrap()
                .sharing_allowed
        );
        assert_eq!(
            show(&task.task_id).unwrap().owner.native_session_id,
            "source-session"
        );
    }

    #[test]
    fn captured_workspace_cannot_include_its_own_mutating_task_journal() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        assert!(
            checkpoint_capturing_workspace(&task.task_id, "source-session", 1, input()).is_err()
        );
        assert_eq!(show(&task.task_id).unwrap(), task);
    }

    #[test]
    fn checkpoint_must_cover_every_constraint_and_declared_resource() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        let mut missing = input();
        missing.constraints.clear();
        assert!(checkpoint(&task.task_id, "source-session", 1, missing).is_err());
        let registered = register_resource(&task.task_id, "source-session", 1, resource()).unwrap();
        assert_eq!(registered.resource_revision, 1);
        assert!(checkpoint(&task.task_id, "source-session", 2, input()).is_err());
        let mut complete = input();
        complete.resource_ids.push("tests".into());
        let saved = checkpoint(&task.task_id, "source-session", 2, complete).unwrap();
        assert_eq!(
            saved.latest_checkpoint.as_ref().unwrap().resource_revision,
            1
        );
        let mut second = resource();
        second.id = "server".into();
        let expanded = register_resource(&task.task_id, "source-session", 3, second).unwrap();
        assert!(!expanded.readiness.checkpoint_matches_resource_revision);
        assert_eq!(show(&task.task_id).unwrap(), expanded);
    }

    #[test]
    fn resource_validation_rejects_shared_release_missing_identity_and_secrets() {
        let mut shared = resource();
        shared.ownership = "shared".into();
        assert!(check_resource(&shared).is_err());
        shared.disposition = "release".into();
        assert!(check_resource(&shared).is_err());
        shared.disposition = "observe".into();
        assert!(check_resource(&shared).is_ok());
        let mut bad = resource();
        bad.native_identity.clear();
        assert!(check_resource(&bad).is_err());
        bad = resource();
        bad.native_identity
            .insert("access_token".into(), "example".into());
        assert!(check_resource(&bad).is_err());
        bad = resource();
        bad.kind = "process".into();
        bad.native_identity = BTreeMap::from([("pid".into(), "42".into())]);
        assert!(check_resource(&bad).is_err());
        bad.native_identity
            .insert("start_identity".into(), "example-start-id".into());
        bad.native_identity
            .insert("host".into(), "example-host".into());
        assert!(check_resource(&bad).is_ok());
    }

    #[test]
    fn tampering_with_immutable_checkpoint_or_cached_payload_fails_closed() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        checkpoint(&task.task_id, "source-session", 1, input()).unwrap();
        mutate_record(&task.task_id, |value| {
            value["latest_checkpoint"]["payload"]["brief"] = "changed".into()
        });
        assert!(show(&task.task_id).is_err());
        let task2 = register(new_task(&home)).unwrap();
        checkpoint(&task2.task_id, "source-session", 1, input()).unwrap();
        let path = task_dir(&task2.task_id).unwrap();
        atomic_json(
            &path,
            "checkpoint-2.json",
            &serde_json::json!({}),
            MAX_CHECKPOINT_BYTES,
            true,
        )
        .unwrap();
        assert!(show(&task2.task_id).is_err());
    }

    #[test]
    fn execution_guard_keeps_task_revision_stable_until_released() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        let (_, held) = lock_current_actor(&task.task_id, "source-session", 1).unwrap();
        let error = checkpoint(&task.task_id, "source-session", 1, input()).unwrap_err();
        assert!(error.to_string().contains("busy"));
        drop(held);
        checkpoint(&task.task_id, "source-session", 1, input()).unwrap();
        assert!(lock_current_actor(&task.task_id, "source-session", 1).is_err());
        assert!(lock_current_actor(&task.task_id, "source-session", 2).is_ok());
    }

    #[test]
    fn busy_lock_does_not_block_and_invalid_ids_cannot_escape_storage() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        let dir = task_dir(&task.task_id).unwrap();
        let _held = lock(&dir).unwrap();
        let error = show(&task.task_id).unwrap_err().to_string();
        assert!(error.contains("busy"));
        for id in [
            "../other",
            "/tmp",
            "",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "0000000000000000000000000000000/",
        ] {
            assert!(show(id).is_err());
        }
    }

    #[test]
    fn future_schema_and_unsupported_readiness_are_rejected() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        mutate_record(&task.task_id, |value| value["schema"] = 2.into());
        assert!(show(&task.task_id).is_err());
        let task2 = register(new_task(&home)).unwrap();
        mutate_record(&task2.task_id, |value| {
            value["readiness"]["handoff_ready"] = true.into()
        });
        assert!(show(&task2.task_id).is_err());
        assert!(
            serde_json::from_value::<CheckpointInput>(
                serde_json::json!({"brief":"x","next_action":"x"})
            )
            .is_err()
        );
        let mut value = serde_json::to_value(input()).unwrap();
        value["unexpected"] = true.into();
        assert!(serde_json::from_value::<CheckpointInput>(value).is_err());
    }

    #[test]
    fn immutable_generation_conflict_cannot_replace_or_advance_the_task() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        let dir = task_dir(&task.task_id).unwrap();
        let mut orphan = CheckpointRecord {
            generation: 2,
            resource_revision: 0,
            digest: String::new(),
            payload: input(),
            workspace_snapshot: None,
        };
        orphan.payload.brief = "Earlier interrupted checkpoint".into();
        orphan.digest = digest(&orphan).unwrap();
        atomic_json(
            &dir,
            "checkpoint-2.json",
            &orphan,
            MAX_CHECKPOINT_BYTES,
            false,
        )
        .unwrap();
        assert!(checkpoint(&task.task_id, "source-session", 1, input()).is_err());
        assert_eq!(show(&task.task_id).unwrap(), task);
        let saved: CheckpointRecord =
            read_json(&dir.join("checkpoint-2.json"), MAX_CHECKPOINT_BYTES).unwrap();
        assert_eq!(saved, orphan);
        let resumed = checkpoint(&task.task_id, "source-session", 1, orphan.payload).unwrap();
        assert_eq!(resumed.generation, 2);
    }

    #[test]
    fn oversized_storage_is_rejected_without_echoing_its_contents() {
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        let path = task_dir(&task.task_id).unwrap().join("task.json");
        let mut file = file_options()
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();
        file.write_all(&vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        let error = show(&task.task_id).unwrap_err().to_string();
        assert!(error.contains("size limit"));
        assert!(error.len() < 100);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_record_and_task_directory_are_rejected() {
        use std::os::unix::fs::symlink;
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        let dir = task_dir(&task.task_id).unwrap();
        fs::rename(dir.join("task.json"), dir.join("original.json")).unwrap();
        symlink(dir.join("original.json"), dir.join("task.json")).unwrap();
        assert!(show(&task.task_id).is_err());
        let other_id = "abcdefabcdefabcdefabcdefabcdefab";
        symlink(home.home(), task_root().unwrap().join(other_id)).unwrap();
        assert!(show(other_id).is_err());
        assert!(!home.home().join(".lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn stored_files_and_task_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = HomeSandbox::new();
        let task = register(new_task(&home)).unwrap();
        checkpoint(&task.task_id, "source-session", 1, input()).unwrap();
        let dir = task_dir(&task.task_id).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for file in ["task.json", "checkpoint-2.json", ".lock"] {
            assert_eq!(
                fs::metadata(dir.join(file)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
