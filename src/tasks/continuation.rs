//! Stop-first successor launch, checkpoint delivery, receipts, and ownership.
//! A scope-empty source is required first. Nothing here prepares a read-only
//! successor while that source can still write. Native transcript files are
//! not rewritten.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::disposition::{SuccessorRecord, TransferPhase, TransferState};
use super::*;

const PROTOCOL_LIMIT: Duration = Duration::from_secs(20);
const MAX_FRAME: usize = 262_144;
const MAX_DELIVERY: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuccessorFile {
    schema: u32,
    handoff_id: String,
    program: String,
    args: Vec<String>,
    release_worker: bool,
    pid: Option<u32>,
    start_identity: Option<String>,
    phase: String,
    session_id: Option<String>,
}

pub(crate) fn continue_handoff(
    id: &str,
    actor: &str,
    generation: u64,
    handoff_id: &str,
    program: &Path,
) -> Result<TaskRecord> {
    if !valid_key(handoff_id) || !program.is_absolute() || !program.is_file() {
        bail!("successor launch identity is invalid");
    }
    let dir = task_dir(id)?;
    let lock = lock(&dir)?;
    let record = read_record(&dir, id)?;
    if record.ownership_epoch == 2 && record.handed_off_to.is_some() {
        drop(lock);
        return Ok(record);
    }
    if actor != record.owner.native_session_id || record.ownership_epoch != 1 {
        bail!("successor launch actor does not match the stopped source");
    }
    if generation != record.generation {
        bail!("stale task generation; read the task again before retrying");
    }
    if !record.source_scope_released(handoff_id) {
        bail!("successor launch requires the exact source scope to be empty");
    }
    if record.resources.is_empty() {
        if record.transfer.is_some() {
            bail!("successor state already exists for a resource-free handoff");
        }
    } else if record.transfer.as_ref().map(|transfer| &transfer.phase)
        != Some(&TransferPhase::Reconciled)
        || record
            .transfer
            .as_ref()
            .is_some_and(|transfer| transfer.handoff_id != handoff_id)
    {
        bail!("successor launch requires a successful resource reconciliation");
    }
    if !workspace_matches(&record)? {
        park_record(
            &dir,
            &lock,
            id,
            handoff_id,
            "workspace changed before successor launch",
        )?;
        bail!("workspace changed after source stop; successor was not launched");
    }
    // Reject credential-shaped checkpoints before any successor process exists.
    let delivery = delivery_bytes(&record, handoff_id)?;
    let proposal = record
        .handoffs
        .iter()
        .find(|proposal| {
            proposal.handoff_id == handoff_id && proposal.state == ProposalState::Proposed
        })
        .ok_or_else(|| anyhow::anyhow!("successor launch requires the active handoff"))?;
    let tool = proposal.destination_tool.clone();
    let model = proposal.destination_model.clone();
    let args = canonical_args(&tool, &model)?;
    reject_widening(&args)?;
    if fs::symlink_metadata(dir.join("successor.json")).is_ok() {
        recover_locked(&dir, id, handoff_id)?;
        if fs::symlink_metadata(dir.join("successor.json")).is_ok() {
            bail!(
                "successor launch is ambiguous; recover the saved successor instead of starting another"
            );
        }
    }
    let file = SuccessorFile {
        schema: 1,
        handoff_id: handoff_id.into(),
        program: program.display().to_string(),
        args: args.clone(),
        release_worker: false,
        pid: None,
        start_identity: None,
        phase: "intent".into(),
        session_id: None,
    };
    atomic_json(&dir, "successor.json", &file, MAX_RECORD_BYTES, false)?;
    // `cargo test` executes this crate as a harness whose argv is not `clauth`.
    // Production uses the gated worker so the native program cannot start
    // before the successor record is durable. Tests spawn the fixture directly.
    let production_worker = std::env::current_exe()
        .ok()
        .is_some_and(|path| path.file_name().is_some_and(|name| name == "clauth"));
    let mut child = if production_worker {
        let exe = std::env::current_exe()
            .map_err(|_| anyhow::anyhow!("cannot locate successor worker"))?;
        Command::new(exe)
            .args(["tasks", "__successor-worker", id, handoff_id])
            .current_dir(&record.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| anyhow::anyhow!("successor worker could not be started"))?
    } else {
        Command::new(program)
            .args(&args)
            .current_dir(&record.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| anyhow::anyhow!("successor program could not be started"))?
    };
    let pid = child.id();
    let start = process_start(pid)?;
    let mut released = file;
    released.release_worker = true;
    released.pid = Some(pid);
    released.start_identity = Some(start.clone());
    released.phase = "released".into();
    atomic_json(&dir, "successor.json", &released, MAX_RECORD_BYTES, true)?;
    if production_worker
        && !wait_until_exec(pid, program, Instant::now() + Duration::from_secs(10))?
    {
        let _ = child.kill();
        let _ = child.wait();
        park_record(
            &dir,
            &lock,
            id,
            handoff_id,
            "successor worker did not reach the native program",
        )?;
        bail!("successor worker did not exec; no second successor will be started automatically");
    }
    let source_session = record.owner.native_session_id.clone();
    let workspace = record.workspace.clone();
    let digest = hex::encode(Sha256::digest(&delivery));
    let text = std::str::from_utf8(&delivery)
        .map_err(|_| anyhow::anyhow!("checkpoint delivery is not valid text"))?
        .to_owned();
    let session = match speak(&mut child, &tool, &model, &workspace, |session| {
        if session == source_session || !safe_label(session) {
            bail!("successor session identity was rejected");
        }
        released.session_id = Some(session.into());
        released.phase = "identified".into();
        atomic_json(&dir, "successor.json", &released, MAX_RECORD_BYTES, true)?;
        journal_successor(
            &dir,
            id,
            handoff_id,
            SuccessorRecord {
                tool: tool.clone(),
                model: model.clone(),
                program: program.display().to_string(),
                args: args.clone(),
                pid,
                start_identity: start.clone(),
                session_id: session.to_owned(),
            },
        )?;
        atomic_json_bytes(&dir, &format!("delivery-{handoff_id}.json"), &delivery)?;
        if fs::read(dir.join(format!("delivery-{handoff_id}.json")))
            .ok()
            .as_deref()
            != Some(delivery.as_slice())
        {
            bail!("checkpoint delivery file did not read back");
        }
        released.phase = "delivering".into();
        atomic_json(&dir, "successor.json", &released, MAX_RECORD_BYTES, true)?;
        Ok(Some(text.clone()))
    }) {
        Ok(session) => session,
        Err(error) => {
            let prompted = fs::read_to_string(dir.join("successor.json"))
                .ok()
                .is_some_and(|body| body.contains("delivering"));
            if !prompted {
                let _ = child.kill();
                let _ = child.wait();
            }
            park_record(
                &dir,
                &lock,
                id,
                handoff_id,
                "successor delivery did not complete",
            )?;
            return Err(error);
        }
    };
    released.phase = "delivered".into();
    atomic_json(&dir, "successor.json", &released, MAX_RECORD_BYTES, true)?;
    journal_delivery(&dir, id, &digest)?;
    match write_receipts(&dir, &read_record(&dir, id)?, handoff_id, &session) {
        Ok(receipt_digest) => {
            commit_ownership(&dir, id, handoff_id, &session, &receipt_digest)?;
        }
        Err(error) => {
            journal_receipts_pending(&dir, id)?;
            return Err(error);
        }
    }
    drop(child);
    drop(lock);
    show(id)
}

pub(crate) fn recover_handoff(id: &str, handoff_id: &str) -> Result<TaskRecord> {
    if !valid_key(handoff_id) {
        bail!("invalid handoff ID");
    }
    let dir = task_dir(id)?;
    let _lock = lock(&dir)?;
    recover_locked(&dir, id, handoff_id)?;
    show_locked(&dir, id)
}

pub(crate) fn successor_worker(id: &str, handoff_id: &str) -> Result<()> {
    let dir = task_dir(id)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            bail!("successor worker was not released");
        }
        let Ok(file) = read_successor(&dir) else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        if file.handoff_id != handoff_id || file.schema != 1 {
            bail!("successor worker identity does not match");
        }
        if file.release_worker
            && file.pid == Some(std::process::id())
            && file.start_identity.as_deref() == process_start(std::process::id()).ok().as_deref()
        {
            let program = PathBuf::from(&file.program);
            let error = Command::new(program)
                .args(&file.args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .exec();
            let _ = error;
            bail!("successor program could not be exec'd");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn recover_locked(dir: &Path, id: &str, handoff_id: &str) -> Result<()> {
    let record = read_record(dir, id)?;
    if record.ownership_epoch == 2 {
        resume_loaded(&record, dir)?;
        return Ok(());
    }
    let Some(file) = read_successor(dir).ok() else {
        return Ok(());
    };
    if file.handoff_id != handoff_id {
        bail!("saved successor belongs to another handoff");
    }
    let alive = file
        .pid
        .zip(file.start_identity.as_deref())
        .is_some_and(|(pid, start)| process_start(pid).ok().as_deref() == Some(start));
    if file.phase == "intent" && !file.release_worker && !alive {
        let _ = fs::remove_file(dir.join("successor.json"));
        return Ok(());
    }
    if matches!(file.phase.as_str(), "delivered")
        && record.transfer.as_ref().is_some_and(|transfer| {
            transfer.phase == TransferPhase::Delivered && transfer.receipts_digest.is_none()
        })
    {
        bail!(
            "checkpoint was delivered but receipts are not confirmed; refusing to launch another successor"
        );
    }
    if alive || file.release_worker || file.session_id.is_some() {
        bail!(
            "a successor was already started; not launching another. Receipts stay pending until they read back"
        );
    }
    Ok(())
}

pub(crate) fn resume_loaded(record: &TaskRecord, dir: &Path) -> Result<()> {
    if record.ownership_epoch == 1 {
        return Ok(());
    }
    let handoff_id = record
        .handed_off_to
        .as_ref()
        .and_then(|_| {
            record
                .handoffs
                .iter()
                .find(|proposal| proposal.state == ProposalState::Committed)
                .map(|proposal| proposal.handoff_id.clone())
        })
        .ok_or_else(|| anyhow::anyhow!("managed resume lacks a committed handoff"))?;
    let expected = record
        .transfer
        .as_ref()
        .and_then(|transfer| transfer.receipts_digest.clone())
        .ok_or_else(|| anyhow::anyhow!("managed resume lacks a receipt digest"))?;
    let bytes = fs::read(dir.join(format!("resume-guard-{handoff_id}.json")))
        .map_err(|_| anyhow::anyhow!("managed resume could not read the successor receipt"))?;
    if hex::encode(Sha256::digest(bytes)) != expected {
        bail!("managed resume receipt does not match the committed handoff");
    }
    Ok(())
}

fn journal_successor(
    dir: &Path,
    id: &str,
    handoff_id: &str,
    successor: SuccessorRecord,
) -> Result<()> {
    let mut record = read_record(dir, id)?;
    advance(&mut record)?;
    let session = successor.session_id.clone();
    let generation = record.generation;
    match &mut record.transfer {
        Some(transfer) => {
            transfer.phase = TransferPhase::SuccessorPrepared;
            transfer.generation = generation;
            transfer.successor = Some(successor);
            transfer.successor_session_id = Some(session);
        }
        None => {
            record.transfer = Some(TransferState {
                handoff_id: handoff_id.into(),
                phase: TransferPhase::SuccessorPrepared,
                generation,
                reconciliation: None,
                successor: Some(successor),
                successor_session_id: Some(session),
                delivery_digest: None,
                receipts_digest: None,
            });
        }
    }
    record.history.push(event(&record, "successor_prepared"));
    super::validate(&record, dir)?;
    atomic_json(dir, "task.json", &record, MAX_RECORD_BYTES, true)
}

fn journal_delivery(dir: &Path, id: &str, digest: &str) -> Result<()> {
    let mut record = read_record(dir, id)?;
    advance(&mut record)?;
    let transfer = record
        .transfer
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("delivery lacks successor state"))?;
    transfer.phase = TransferPhase::Delivered;
    transfer.generation = record.generation;
    transfer.delivery_digest = Some(digest.into());
    record.history.push(event(&record, "checkpoint_delivered"));
    super::validate(&record, dir)?;
    atomic_json(dir, "task.json", &record, MAX_RECORD_BYTES, true)
}

fn journal_receipts_pending(dir: &Path, id: &str) -> Result<()> {
    let mut record = read_record(dir, id)?;
    if record
        .transfer
        .as_ref()
        .is_some_and(|transfer| transfer.phase == TransferPhase::ReceiptsPending)
    {
        return Ok(());
    }
    advance(&mut record)?;
    let transfer = record
        .transfer
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("receipts lack successor state"))?;
    transfer.phase = TransferPhase::ReceiptsPending;
    transfer.generation = record.generation;
    record.history.push(event(&record, "receipts_pending"));
    super::validate(&record, dir)?;
    atomic_json(dir, "task.json", &record, MAX_RECORD_BYTES, true)
}

fn commit_ownership(
    dir: &Path,
    id: &str,
    handoff_id: &str,
    session: &str,
    receipt_digest: &str,
) -> Result<()> {
    let mut record = read_record(dir, id)?;
    if record.ownership_epoch == 2 {
        return Ok(());
    }
    let proposal = record
        .handoffs
        .iter()
        .position(|proposal| {
            proposal.handoff_id == handoff_id && proposal.state == ProposalState::Proposed
        })
        .ok_or_else(|| anyhow::anyhow!("ownership commit requires the active handoff"))?;
    let destination_tool = record.handoffs[proposal].destination_tool.clone();
    let destination_model = record.handoffs[proposal].destination_model.clone();
    advance(&mut record)?;
    let epoch = record.handoffs[proposal]
        .source_epoch
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("ownership epoch exhausted"))?;
    record.handoffs[proposal].state = ProposalState::Committed;
    record.handoffs[proposal].closed_generation = Some(record.generation);
    let transfer = record
        .transfer
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("ownership commit lacks successor state"))?;
    transfer.phase = TransferPhase::Confirmed;
    transfer.generation = record.generation;
    transfer.receipts_digest = Some(receipt_digest.into());
    transfer.successor_session_id = Some(session.into());
    // Stamp the journal with the source actor, then move ownership.
    record.history.push(event(&record, "ownership_committed"));
    record.continued_from = Some(record.owner.clone());
    record.owner = ExecutionIdentity {
        tool: destination_tool,
        model: Some(destination_model),
        native_session_id: session.into(),
        account_ref: None,
    };
    record.handed_off_to = Some(record.owner.clone());
    record.ownership_epoch = epoch;
    super::refresh_readiness(&mut record);
    super::validate(&record, dir)?;
    atomic_json(dir, "task.json", &record, MAX_RECORD_BYTES, true)
}

fn park_record(dir: &Path, _lock: &File, id: &str, handoff_id: &str, _reason: &str) -> Result<()> {
    let mut record = read_record(dir, id)?;
    if record
        .history
        .last()
        .is_some_and(|event| event.kind == "handoff_parked")
    {
        return Ok(());
    }
    advance(&mut record)?;
    let generation = record.generation;
    match &mut record.transfer {
        Some(transfer) => {
            transfer.phase = TransferPhase::Parked;
            transfer.generation = generation;
        }
        None => {
            record.transfer = Some(TransferState {
                handoff_id: handoff_id.into(),
                phase: TransferPhase::Parked,
                generation,
                reconciliation: None,
                successor: None,
                successor_session_id: None,
                delivery_digest: None,
                receipts_digest: None,
            });
        }
    }
    record.history.push(event(&record, "handoff_parked"));
    super::validate(&record, dir)?;
    atomic_json(dir, "task.json", &record, MAX_RECORD_BYTES, true)
}

fn write_receipts(
    dir: &Path,
    record: &TaskRecord,
    handoff_id: &str,
    session: &str,
) -> Result<String> {
    let checkpoint = record
        .latest_checkpoint
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("receipts require a checkpoint"))?;
    let source = &record.owner.native_session_id;
    let tool = record
        .handoffs
        .iter()
        .find(|proposal| proposal.handoff_id == handoff_id)
        .map(|proposal| proposal.destination_tool.as_str())
        .ok_or_else(|| anyhow::anyhow!("receipts require the handoff destination"))?;
    let source_body = json!({
        "schema": 1,
        "role": "source",
        "task_id": record.task_id,
        "handoff_id": handoff_id,
        "ownership_epoch": 2,
        "checkpoint_digest": checkpoint.digest,
        "source_session_id": source,
        "successor_session_id": session,
        "statement": format!("Handed task {} to {tool} session {session}; I no longer own this task", record.task_id)
    });
    let successor_body = json!({
        "schema": 1,
        "role": "successor",
        "task_id": record.task_id,
        "handoff_id": handoff_id,
        "ownership_epoch": 2,
        "checkpoint_digest": checkpoint.digest,
        "source_session_id": source,
        "successor_session_id": session,
        "statement": format!("I own task {}, continued from session {source}", record.task_id)
    });
    let source_bytes = serde_json::to_vec_pretty(&source_body)
        .map_err(|_| anyhow::anyhow!("cannot encode source receipt"))?;
    let successor_bytes = serde_json::to_vec_pretty(&successor_body)
        .map_err(|_| anyhow::anyhow!("cannot encode successor receipt"))?;
    atomic_json_bytes(
        dir,
        &format!("receipt-source-{handoff_id}.json"),
        &source_bytes,
    )?;
    atomic_json_bytes(
        dir,
        &format!("receipt-successor-{handoff_id}.json"),
        &successor_bytes,
    )?;
    atomic_json_bytes(
        dir,
        &format!("resume-guard-{handoff_id}.json"),
        &successor_bytes,
    )?;
    let read_source = fs::read(dir.join(format!("receipt-source-{handoff_id}.json")))
        .map_err(|_| anyhow::anyhow!("source receipt did not read back"))?;
    let read_successor = fs::read(dir.join(format!("receipt-successor-{handoff_id}.json")))
        .map_err(|_| anyhow::anyhow!("successor receipt did not read back"))?;
    let read_guard = fs::read(dir.join(format!("resume-guard-{handoff_id}.json")))
        .map_err(|_| anyhow::anyhow!("resume guard did not read back"))?;
    if read_source != source_bytes
        || read_successor != successor_bytes
        || read_guard != successor_bytes
    {
        bail!("session-linked receipts did not read back unchanged");
    }
    Ok(hex::encode(Sha256::digest(&successor_bytes)))
}

fn delivery_bytes(record: &TaskRecord, handoff_id: &str) -> Result<Vec<u8>> {
    let checkpoint = record
        .latest_checkpoint
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("delivery requires a checkpoint"))?;
    for text in checkpoint
        .payload
        .brief
        .split('\0')
        .chain(checkpoint.payload.completed.iter().map(String::as_str))
        .chain(checkpoint.payload.remaining_plan.iter().map(String::as_str))
        .chain(checkpoint.payload.decisions.iter().map(String::as_str))
        .chain(checkpoint.payload.uncertainties.iter().map(String::as_str))
        .chain(std::iter::once(checkpoint.payload.next_action.as_str()))
        .chain(checkpoint.payload.constraints.iter().map(String::as_str))
        .chain(std::iter::once(record.objective.as_str()))
    {
        if credential_shaped(text) {
            bail!("checkpoint delivery contains credential-shaped text and was not sent");
        }
    }
    let resources = record
        .transfer
        .as_ref()
        .and_then(|transfer| transfer.reconciliation.as_ref())
        .map(|reconciliation| {
            reconciliation
                .entries
                .iter()
                .map(|entry| {
                    json!({
                        "id": entry.id,
                        "disposition": entry.disposition,
                        "outcome": entry.outcome,
                        "exclusive_control": false
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let body = json!({
        "schema": 1,
        "task_id": record.task_id,
        "handoff_id": handoff_id,
        "checkpoint_generation": checkpoint.generation,
        "checkpoint_digest": checkpoint.digest,
        "objective": record.objective,
        "brief": checkpoint.payload.brief,
        "constraints": checkpoint.payload.constraints,
        "remaining_plan": checkpoint.payload.remaining_plan,
        "next_action": checkpoint.payload.next_action,
        "resources": resources,
        "statement": "Continuation only. Do not restore, merge, or check out a snapshot."
    });
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| anyhow::anyhow!("cannot encode checkpoint delivery"))?;
    if bytes.len() > MAX_DELIVERY {
        bail!("checkpoint delivery exceeds the successor bootstrap limit");
    }
    Ok(bytes)
}

fn credential_shaped(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("begin private")
        || lower.contains("begin rsa")
        || lower.contains("token=")
        || lower.contains("api_key=")
        || lower.contains("apikey=")
        || lower.contains("password=")
        || lower.contains("secret=")
        || text.contains("sk-")
        || text.contains("AKIA")
}

fn canonical_args(tool: &str, model: &str) -> Result<Vec<String>> {
    if !safe_label(model) {
        bail!("destination model is not a supported successor argument");
    }
    let encoded = serde_json::to_string(model)
        .map_err(|_| anyhow::anyhow!("destination model is not a supported successor argument"))?;
    let args = match tool {
        "grok" => vec![
            "agent".into(),
            "--no-leader".into(),
            "--model".into(),
            model.into(),
            "stdio".into(),
        ],
        "agy" => vec![
            "--input-format=stream-json".into(),
            "--output-format=stream-json".into(),
        ],
        "codex" => vec![
            "app-server".into(),
            "--listen".into(),
            "stdio://".into(),
            "--config".into(),
            format!("model={encoded}"),
        ],
        _ => bail!("successor launch supports only Codex, Grok, and agy"),
    };
    Ok(args)
}

fn reject_widening(args: &[String]) -> Result<()> {
    for arg in args {
        let lower = arg.to_ascii_lowercase();
        if lower.contains("restore-code")
            || lower.contains("dangerously")
            || lower.contains("bypass")
            || lower.contains("always-approve")
            || lower.contains("with-fallback")
            || lower.contains("switch_profile")
        {
            bail!("successor arguments would widen permissions or retarget credentials");
        }
    }
    Ok(())
}

fn workspace_matches(record: &TaskRecord) -> Result<bool> {
    let snapshot = record
        .latest_checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.workspace_snapshot.as_ref())
        .ok_or_else(|| anyhow::anyhow!("successor launch lacks a captured checkpoint"))?;
    Ok(workspace::capture(&record.workspace)? == *snapshot)
}

fn speak<F>(
    child: &mut Child,
    tool: &str,
    model: &str,
    workspace: &Path,
    prepare_delivery: F,
) -> Result<String>
where
    F: FnOnce(&str) -> Result<Option<String>>,
{
    let deadline = Instant::now() + PROTOCOL_LIMIT;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("successor stdin is unavailable"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("successor stdout is unavailable"))?;
    let mut reader = BufReader::new(output);
    let session = match tool {
        "grok" => grok_session(&mut input, &mut reader, workspace, deadline)?,
        "agy" => agy_session(&mut reader, workspace, deadline)?,
        "codex" => codex_session(&mut input, &mut reader, workspace, model, deadline)?,
        _ => bail!("successor launch supports only Codex, Grok, and agy"),
    };
    // Persist the session before any checkpoint bytes are written to it.
    let delivery = prepare_delivery(&session)?;
    if let Some(text) = delivery.as_deref() {
        match tool {
            "grok" => {
                write_frame(
                    &mut input,
                    &json!({
                        "jsonrpc":"2.0","id":3,"method":"session/prompt",
                        "params":{"sessionId":session,"prompt":[{"type":"text","text":text}]}
                    }),
                )?;
                let _ = read_result(&mut reader, Some(3), deadline)?;
            }
            "agy" => {
                write_frame(
                    &mut input,
                    &json!({"event":"user","message":{"content":text}}),
                )?;
                // Installed agy 1.2.7 documents a closed stream: init, repeating
                // step_update progress, then one terminal result. The first
                // frame after the user message is progress, not the result.
                let delivery_deadline = Instant::now() + Duration::from_secs(90);
                loop {
                    let frame = read_frame(&mut reader, delivery_deadline)?;
                    let value: Value = serde_json::from_slice(&frame).map_err(|_| {
                        anyhow::anyhow!("invalid successor delivery acknowledgement")
                    })?;
                    if value.get("error").is_some() {
                        bail!("agy delivery was rejected");
                    }
                    match value.get("event").and_then(Value::as_str) {
                        Some("step_update") => continue,
                        Some("result") if value.get("result").is_some() => break,
                        Some("done") => break,
                        _ => bail!("agy delivery acknowledgement was not accepted"),
                    }
                }
            }
            "codex" => {
                write_frame(
                    &mut input,
                    &json!({
                        "id":3,"method":"turn/start",
                        "params":{
                            "threadId":session,
                            "input":[{"type":"text","text":text}],
                            "approvalPolicy":"never",
                            "sandbox":"read-only"
                        }
                    }),
                )?;
                let _ = read_result(&mut reader, Some(3), deadline)?;
            }
            _ => bail!("successor delivery is unsupported"),
        }
    }
    child.stdin = Some(input);
    child.stdout = Some(reader.into_inner());
    Ok(session)
}

fn grok_session(
    input: &mut impl Write,
    reader: &mut impl BufRead,
    workspace: &Path,
    deadline: Instant,
) -> Result<String> {
    write_frame(
        input,
        &json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{
                "protocolVersion":1,
                "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},
                "clientInfo":{"name":"clauth-successor","version":"1"}
            }
        }),
    )?;
    let initialized = read_result(reader, Some(1), deadline)?;
    if initialized.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        bail!("Grok successor does not support the required protocol");
    }
    let cwd = workspace
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("successor workspace is not valid text"))?;
    write_frame(
        input,
        &json!({
            "jsonrpc":"2.0","id":2,"method":"session/new",
            "params":{"cwd":cwd,"mcpServers":[]}
        }),
    )?;
    let created = read_result(reader, Some(2), deadline)?;
    let session = created
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|session| safe_label(session))
        .ok_or_else(|| anyhow::anyhow!("Grok successor did not return a session id"))?;
    Ok(session.into())
}

fn agy_session(reader: &mut impl BufRead, workspace: &Path, deadline: Instant) -> Result<String> {
    let frame = read_frame(reader, deadline)?;
    let value: Value = serde_json::from_slice(&frame)
        .map_err(|_| anyhow::anyhow!("invalid agy successor announcement"))?;
    if value.get("event").and_then(Value::as_str) != Some("init") {
        bail!("agy successor did not announce a new conversation");
    }
    let session = value
        .get("conversation_id")
        .and_then(Value::as_str)
        .filter(|session| safe_label(session))
        .ok_or_else(|| anyhow::anyhow!("agy successor did not return a conversation id"))?;
    let cwd = value
        .pointer("/init/cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("agy successor omitted its workspace"))?;
    if cwd != workspace.to_str().unwrap_or("") {
        bail!("agy successor workspace does not match the task");
    }
    Ok(session.into())
}

fn codex_session(
    input: &mut impl Write,
    reader: &mut impl BufRead,
    workspace: &Path,
    model: &str,
    deadline: Instant,
) -> Result<String> {
    write_frame(
        input,
        &json!({
            "id":1,"method":"initialize",
            "params":{"clientInfo":{"name":"clauth-successor","version":"1"},"capabilities":{}}
        }),
    )?;
    let initialized = read_result(reader, Some(1), deadline)?;
    if !initialized
        .get("userAgent")
        .and_then(Value::as_str)
        .is_some_and(|agent| !agent.is_empty() && agent.len() <= 1024)
    {
        bail!("Codex successor initialization was rejected");
    }
    write_frame(input, &json!({"method":"initialized","params":{}}))?;
    let cwd = workspace
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("successor workspace is not valid text"))?;
    write_frame(
        input,
        &json!({
            "id":2,"method":"thread/start",
            "params":{"cwd":cwd,"model":model,"approvalPolicy":"never","sandbox":"read-only"}
        }),
    )?;
    let created = read_result(reader, Some(2), deadline)?;
    if created.get("approvalPolicy").and_then(Value::as_str) != Some("never")
        || created.pointer("/sandbox/type").and_then(Value::as_str) != Some("readOnly")
        || created.get("cwd").and_then(Value::as_str) != Some(cwd)
    {
        bail!("Codex successor did not confirm the requested thread policy");
    }
    let thread = created
        .get("thread")
        .ok_or_else(|| anyhow::anyhow!("Codex successor did not return a thread"))?;
    let session = thread
        .get("id")
        .and_then(Value::as_str)
        .filter(|session| safe_label(session))
        .ok_or_else(|| anyhow::anyhow!("Codex successor did not return a thread id"))?;
    if thread.get("cwd").and_then(Value::as_str) != Some(cwd) {
        bail!("Codex successor workspace does not match the task");
    }
    Ok(session.into())
}

fn write_frame(input: &mut impl Write, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|_| anyhow::anyhow!("cannot encode successor request"))?;
    bytes.push(b'\n');
    input
        .write_all(&bytes)
        .map_err(|_| anyhow::anyhow!("cannot write to the successor"))?;
    input
        .flush()
        .map_err(|_| anyhow::anyhow!("cannot write to the successor"))
}

fn read_result(reader: &mut impl BufRead, id: Option<u64>, deadline: Instant) -> Result<Value> {
    loop {
        let frame = read_frame(reader, deadline)?;
        let value: Value = serde_json::from_slice(&frame)
            .map_err(|_| anyhow::anyhow!("invalid successor protocol frame"))?;
        if value.get("method").is_some() && value.get("id").is_some() {
            bail!("successor requested a host operation; refusing it");
        }
        if value.get("method").is_some() {
            continue;
        }
        if id.is_some() && value.get("id").and_then(Value::as_u64) != id {
            bail!("successor response identity did not match");
        }
        if value.get("error").is_some() {
            bail!("successor request was rejected");
        }
        return value
            .get("result")
            .cloned()
            .filter(Value::is_object)
            .ok_or_else(|| anyhow::anyhow!("invalid successor protocol result"));
    }
}

fn read_frame(reader: &mut impl BufRead, deadline: Instant) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        if Instant::now() >= deadline {
            bail!("successor protocol timed out");
        }
        let mut byte = [0];
        match reader.read(&mut byte) {
            Ok(0) => bail!("successor closed its output"),
            Ok(_) => {
                frame.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if frame.len() > MAX_FRAME {
                    bail!("successor protocol frame is too large");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => bail!("successor output could not be read"),
        }
    }
    Ok(frame)
}

fn wait_until_exec(pid: u32, program: &Path, deadline: Instant) -> Result<bool> {
    let name = program
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("successor program name is invalid"))?;
    while Instant::now() < deadline {
        if process_start(pid).is_err() {
            return Ok(false);
        }
        if fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .is_some_and(|bytes| String::from_utf8_lossy(&bytes).contains(name))
        {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(false)
}

fn process_start(pid: u32) -> Result<String> {
    let bytes = fs::read(format!("/proc/{pid}/stat"))
        .map_err(|_| anyhow::anyhow!("successor start identity is unavailable"))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("successor start identity is unavailable"))?;
    let start = text
        .rsplit_once(')')
        .and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .filter(|start| !start.is_empty() && start.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| anyhow::anyhow!("successor start identity is unavailable"))?;
    Ok(start.into())
}

fn read_successor(dir: &Path) -> Result<SuccessorFile> {
    let bytes = fs::read(dir.join("successor.json"))
        .map_err(|_| anyhow::anyhow!("successor record is unavailable"))?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("successor record is invalid"))
}

fn atomic_json_bytes(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_DELIVERY && !name.starts_with("receipt") && !name.starts_with("resume") {
        bail!("task storage record exceeds the size limit");
    }
    if bytes.len() > MAX_RECORD_BYTES {
        bail!("task storage record exceeds the size limit");
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|_| anyhow::anyhow!("cannot stage task storage"))?;
    temp.write_all(bytes)
        .map_err(|_| anyhow::anyhow!("cannot write task storage"))?;
    temp.as_file()
        .sync_all()
        .map_err(|_| anyhow::anyhow!("cannot sync task storage"))?;
    temp.persist(dir.join(name))
        .map_err(|_| anyhow::anyhow!("cannot commit task storage"))?;
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|_| anyhow::anyhow!("cannot sync task storage directory"))?;
    Ok(())
}

fn show_locked(dir: &Path, id: &str) -> Result<TaskRecord> {
    read_record(dir, id)
}

fn safe_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

pub(super) fn validate(record: &TaskRecord, dir: &Path) -> Result<()> {
    let Some(transfer) = &record.transfer else {
        if record.ownership_epoch != 1
            || record.continued_from.is_some()
            || record.handed_off_to.is_some()
        {
            bail!("ownership fields require a transfer record");
        }
        if record.history.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                "resources_reconciled"
                    | "resources_parked"
                    | "successor_prepared"
                    | "checkpoint_delivered"
                    | "receipts_pending"
                    | "ownership_committed"
                    | "handoff_parked"
            )
        }) {
            bail!("transfer history lacks a transfer record");
        }
        return Ok(());
    };
    if !valid_key(&transfer.handoff_id)
        || transfer.generation == 0
        || transfer.generation > record.generation
        || record
            .handoffs
            .iter()
            .all(|proposal| proposal.handoff_id != transfer.handoff_id)
    {
        bail!("transfer record does not match a handoff");
    }
    let expected = match transfer.phase {
        TransferPhase::Reconciled => "resources_reconciled",
        TransferPhase::ReconcileParked => "resources_parked",
        TransferPhase::SuccessorPrepared => "successor_prepared",
        TransferPhase::Delivered => "checkpoint_delivered",
        TransferPhase::ReceiptsPending => "receipts_pending",
        TransferPhase::Confirmed => "ownership_committed",
        TransferPhase::Parked => "handoff_parked",
    };
    if record
        .history
        .get((transfer.generation - 1) as usize)
        .map(|event| event.kind.as_str())
        != Some(expected)
    {
        bail!("transfer phase does not match its journal event");
    }
    if transfer
        .reconciliation
        .as_ref()
        .is_some_and(|reconciliation| {
            reconciliation.exclusive_control || !valid_cgroup(&reconciliation.source_cgroup)
        })
    {
        bail!("resource reconciliation claims unsupported exclusive control");
    }
    if transfer.phase == TransferPhase::Confirmed {
        let successor = transfer
            .successor
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("confirmed handoff lacks a successor"))?;
        if record.ownership_epoch != 2
            || record.continued_from.is_none()
            || record.handed_off_to.as_ref() != Some(&record.owner)
            || record.owner.native_session_id != successor.session_id
            || transfer.receipts_digest.is_none()
            || transfer.delivery_digest.is_none()
        {
            bail!("confirmed handoff ownership fields are incomplete");
        }
        resume_loaded(record, dir)?;
    } else if record.ownership_epoch != 1
        || record.continued_from.is_some()
        || record.handed_off_to.is_some()
    {
        bail!("ownership moved before receipts were confirmed");
    }
    Ok(())
}

fn valid_cgroup(value: &str) -> bool {
    super::disposition::valid_cgroup(value)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::testutil::HomeSandbox;
    use std::os::unix::fs::PermissionsExt;

    fn digest() -> String {
        "ab".repeat(32)
    }

    fn scope() -> &'static str {
        "/user.slice/clauth-not-this-scope"
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn task(home: &HomeSandbox, tool: &str) -> TaskRecord {
        let root = home.home().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("work.txt"), "unfinished").unwrap();
        let task = register(NewTask {
            objective: "Finish the portable handoff".into(),
            workspace: root,
            constraints: vec!["keep user edits".into()],
            source: ExecutionIdentity {
                tool: tool.into(),
                model: Some("source-model".into()),
                native_session_id: "source-session".into(),
                account_ref: None,
            },
        })
        .unwrap();
        let task = checkpoint_capturing_workspace(
            &task.task_id,
            "source-session",
            task.generation,
            CheckpointInput {
                brief: "Compact state for the successor".into(),
                completed: vec!["recorded the decision".into()],
                remaining_plan: vec!["run the acceptance check".into()],
                decisions: vec![],
                uncertainties: vec![],
                next_action: "Inspect the current implementation".into(),
                constraints: vec!["keep user edits".into()],
                resource_ids: vec![],
            },
        )
        .unwrap();
        set_policy(
            &task.task_id,
            "source-session",
            task.generation,
            SharingPolicy {
                destinations: vec![
                    DestinationRule {
                        tool: "grok".into(),
                        models: vec!["grok-test".into()],
                    },
                    DestinationRule {
                        tool: "agy".into(),
                        models: vec!["agy-test".into()],
                    },
                    DestinationRule {
                        tool: "codex".into(),
                        models: vec!["codex-test".into()],
                    },
                ],
                share_checkpoint: true,
                share_workspace: true,
                share_resource_metadata: true,
            },
        )
        .unwrap()
    }

    fn release(task: TaskRecord, handoff: &str, destination: &str, model: &str) -> TaskRecord {
        let task = propose_handoff(
            &task.task_id,
            "source-session",
            task.generation,
            handoff,
            destination,
            model,
        )
        .unwrap();
        begin_source_release(
            &task.task_id,
            "source-session",
            task.generation,
            handoff,
            &"cd".repeat(16),
            &digest(),
        )
        .unwrap()
        .finish(ReleaseOutcome::ScopeEmpty)
        .unwrap()
    }

    #[test]
    fn continue_refuses_before_source_stop_and_credential_text() {
        let home = HomeSandbox::new();
        let task = task(&home, "codex");
        let proposed = propose_handoff(
            &task.task_id,
            "source-session",
            task.generation,
            "early",
            "grok",
            "grok-test",
        )
        .unwrap();
        let program = script(home.home(), "fake-grok", "#!/bin/sh\nexit 0\n");
        let error = continue_handoff(
            &proposed.task_id,
            "source-session",
            proposed.generation,
            "early",
            &program,
        );
        assert!(error.is_err());
        assert!(
            !task_dir(&proposed.task_id)
                .unwrap()
                .join("successor.json")
                .exists()
        );
        assert_eq!(show(&proposed.task_id).unwrap().ownership_epoch, 1);

        let mut poisoned = proposed;
        poisoned = checkpoint_capturing_workspace(
            &poisoned.task_id,
            "source-session",
            poisoned.generation,
            CheckpointInput {
                brief: "token=supersecret".into(),
                completed: vec!["recorded the decision".into()],
                remaining_plan: vec!["run the acceptance check".into()],
                decisions: vec![],
                uncertainties: vec![],
                next_action: "Inspect the current implementation".into(),
                constraints: vec!["keep user edits".into()],
                resource_ids: vec![],
            },
        )
        .unwrap();
        poisoned = propose_handoff(
            &poisoned.task_id,
            "source-session",
            poisoned.generation,
            "secret",
            "grok",
            "grok-test",
        )
        .unwrap();
        poisoned = begin_source_release(
            &poisoned.task_id,
            "source-session",
            poisoned.generation,
            "secret",
            &"cd".repeat(16),
            &digest(),
        )
        .unwrap()
        .finish(ReleaseOutcome::ScopeEmpty)
        .unwrap();
        assert!(
            continue_handoff(
                &poisoned.task_id,
                "source-session",
                poisoned.generation,
                "secret",
                &program,
            )
            .is_err()
        );
        assert!(!poisoned.workspace.join("delivered.txt").exists());
        assert_eq!(
            show(&poisoned.task_id).unwrap().owner.native_session_id,
            "source-session"
        );
    }

    #[test]
    fn disposable_codex_to_grok_and_grok_to_agy_commit_after_receipts() {
        {
            let home = HomeSandbox::new();
            let grok = script(
                home.home(),
                "fake-grok",
                r#"#!/usr/bin/env python3
import json, sys
sys.stdout.reconfigure(line_buffering=True)
open("argv.txt","w").write("\n".join(sys.argv))
def read():
    return json.loads(sys.stdin.readline())
assert read()["method"] == "initialize"
print(json.dumps({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}), flush=True)
assert read()["method"] == "session/new"
print(json.dumps({"jsonrpc":"2.0","id":2,"result":{"sessionId":"grok-successor-1"}}), flush=True)
prompt = read()
open("delivered.txt","w").write(prompt["params"]["prompt"][0]["text"])
print(json.dumps({"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}), flush=True)
"#,
            );
            let codex_task = release(task(&home, "codex"), "codex-grok", "grok", "grok-test");
            let continued = continue_handoff(
                &codex_task.task_id,
                "source-session",
                codex_task.generation,
                "codex-grok",
                &grok,
            )
            .unwrap();
            assert_eq!(continued.ownership_epoch, 2);
            assert_eq!(continued.owner.tool, "grok");
            assert_eq!(continued.owner.native_session_id, "grok-successor-1");
            assert_eq!(
                continued.continued_from.unwrap().native_session_id,
                "source-session"
            );
            assert_eq!(continued.handed_off_to.unwrap().tool, "grok");
            assert!(continued.readiness.handoff_ready);
            let argv = fs::read_to_string(continued.workspace.join("argv.txt")).unwrap();
            assert!(argv.contains("--no-leader"));
            assert!(!argv.contains("restore-code"));
            assert!(!argv.contains("dangerously"));
            let delivered = fs::read_to_string(continued.workspace.join("delivered.txt")).unwrap();
            assert!(delivered.contains("Compact state for the successor"));
            assert!(delivered.contains("Do not restore"));
            assert!(
                checkpoint(
                    &continued.task_id,
                    "source-session",
                    continued.generation,
                    continued
                        .latest_checkpoint
                        .as_ref()
                        .unwrap()
                        .payload
                        .clone(),
                )
                .is_err()
            );
            let guard = task_dir(&continued.task_id)
                .unwrap()
                .join("resume-guard-codex-grok.json");
            let saved = fs::read(&guard).unwrap();
            fs::remove_file(&guard).unwrap();
            assert!(
                checkpoint(
                    &continued.task_id,
                    "grok-successor-1",
                    continued.generation,
                    continued
                        .latest_checkpoint
                        .as_ref()
                        .unwrap()
                        .payload
                        .clone(),
                )
                .is_err()
            );
            fs::write(&guard, saved).unwrap();
        }

        let agy_home = HomeSandbox::new();
        let agy_program = script(
            agy_home.home(),
            "fake-agy",
            r#"#!/usr/bin/env python3
import json, os, sys
sys.stdout.reconfigure(line_buffering=True)
open("argv.txt","w").write("\n".join(sys.argv))
print(json.dumps({"event":"init","conversation_id":"agy-successor-1","init":{"cwd":os.getcwd()}}), flush=True)
message = json.loads(sys.stdin.readline())
open("delivered.txt","w").write(message["message"]["content"])
print(json.dumps({"event":"step_update","step_update":{"step_type":"progress"}}), flush=True)
print(json.dumps({"event":"result","result":{"status":"ok"}}), flush=True)
"#,
        );
        let agy_task = release(task(&agy_home, "grok"), "grok-agy", "agy", "agy-test");
        let continued = continue_handoff(
            &agy_task.task_id,
            "source-session",
            agy_task.generation,
            "grok-agy",
            &agy_program,
        )
        .unwrap();
        assert_eq!(continued.owner.tool, "agy");
        assert_eq!(continued.owner.native_session_id, "agy-successor-1");
        assert_eq!(continued.continued_from.unwrap().tool, "grok");
        let argv = fs::read_to_string(continued.workspace.join("argv.txt")).unwrap();
        assert!(argv.contains("--input-format=stream-json"));
        assert!(!argv.contains("--conversation"));
        assert!(
            fs::read_to_string(continued.workspace.join("delivered.txt"))
                .unwrap()
                .contains("run the acceptance check")
        );
        assert!(recover_handoff(&continued.task_id, "grok-agy").is_ok());
    }

    #[test]
    fn disposable_successor_can_be_codex() {
        let home = HomeSandbox::new();
        let program = script(
            home.home(),
            "fake-codex",
            r#"#!/usr/bin/env python3
import json, sys
sys.stdout.reconfigure(line_buffering=True)
open("argv.txt","w").write("\n".join(sys.argv))
def read():
    return json.loads(sys.stdin.readline())
assert read()["method"] == "initialize"
print(json.dumps({"id":1,"result":{"userAgent":"fake-codex"}}), flush=True)
assert read()["method"] == "initialized"
started = read()
assert started["method"] == "thread/start"
assert started["params"]["sandbox"] == "read-only"
assert "restore-code" not in json.dumps(started)
cwd = started["params"]["cwd"]
print(json.dumps({"id":2,"result":{"thread":{"id":"codex-successor-1","cwd":cwd},"cwd":cwd,"approvalPolicy":"never","sandbox":{"type":"readOnly"}}}), flush=True)
turn = read()
assert "restore-code" not in json.dumps(turn)
open("delivered.txt","w").write(turn["params"]["input"][0]["text"])
print(json.dumps({"id":3,"result":{"status":"ok"}}), flush=True)
"#,
        );
        let task = release(task(&home, "grok"), "to-codex", "codex", "codex-test");
        let continued = continue_handoff(
            &task.task_id,
            "source-session",
            task.generation,
            "to-codex",
            &program,
        )
        .unwrap();
        assert_eq!(continued.owner.tool, "codex");
        assert_eq!(continued.owner.native_session_id, "codex-successor-1");
        let argv = fs::read_to_string(continued.workspace.join("argv.txt")).unwrap();
        assert!(argv.contains("app-server"));
        assert!(argv.contains("stdio://"));
        assert!(!argv.contains("restore-code"));
        assert!(
            fs::read_to_string(continued.workspace.join("delivered.txt"))
                .unwrap()
                .contains("Compact state for the successor")
        );
    }

    #[test]
    fn crash_after_successor_start_does_not_launch_another() {
        let home = HomeSandbox::new();
        let task = release(task(&home, "codex"), "crash", "grok", "grok-test");
        let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = sleeper.id();
        let start = process_start(pid).unwrap();
        let dir = task_dir(&task.task_id).unwrap();
        let file = SuccessorFile {
            schema: 1,
            handoff_id: "crash".into(),
            program: "/bin/sleep".into(),
            args: vec!["30".into()],
            release_worker: true,
            pid: Some(pid),
            start_identity: Some(start),
            phase: "identified".into(),
            session_id: Some("maybe-live".into()),
        };
        atomic_json(&dir, "successor.json", &file, MAX_RECORD_BYTES, false).unwrap();
        let program = script(home.home(), "fake-grok", "#!/bin/sh\nexit 7\n");
        assert!(
            continue_handoff(
                &task.task_id,
                "source-session",
                task.generation,
                "crash",
                &program,
            )
            .is_err()
        );
        assert!(recover_handoff(&task.task_id, "crash").is_err());
        assert_eq!(show(&task.task_id).unwrap().ownership_epoch, 1);
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    #[test]
    fn independent_process_is_adopted_and_a_same_scope_writer_blocks() {
        let home = HomeSandbox::new();
        let mut sleeper = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = sleeper.id();
        let identity = crate::tasks::resources::process_identity(pid).unwrap();
        let mut record = task(&home, "codex");
        record = register_resource(
            &record.task_id,
            "source-session",
            record.generation,
            ResourceInput {
                id: "helper".into(),
                kind: "process".into(),
                native_identity: identity.clone(),
                purpose: "independent test helper".into(),
                ownership: "task".into(),
                disposition: "adopt".into(),
                may_write: false,
                reconnect: Some("re-read the process identity".into()),
                cleanup: Some("rm -rf /".into()),
            },
        )
        .unwrap();
        let mut brief = record.latest_checkpoint.as_ref().unwrap().payload.clone();
        brief.resource_ids = vec!["helper".into()];
        record = checkpoint_capturing_workspace(
            &record.task_id,
            "source-session",
            record.generation,
            brief,
        )
        .unwrap();
        record = propose_handoff(
            &record.task_id,
            "source-session",
            record.generation,
            "adopt-helper",
            "grok",
            "grok-test",
        )
        .unwrap();
        let blocked = record.clone();
        let cgroup = super::super::disposition::process_cgroup(pid).unwrap();
        assert!(
            classify_resources(
                &blocked.task_id,
                "source-session",
                blocked.generation,
                "adopt-helper",
                &cgroup,
                &digest(),
            )
            .is_err()
        );
        assert_eq!(show(&blocked.task_id).unwrap(), blocked);
        let classified = classify_resources(
            &record.task_id,
            "source-session",
            record.generation,
            "adopt-helper",
            scope(),
            &digest(),
        )
        .unwrap();
        assert!(
            classified.handoffs[0]
                .disposition_plan
                .as_ref()
                .unwrap()
                .stop_safe
        );
        let released = begin_source_release(
            &classified.task_id,
            "source-session",
            classified.generation,
            "adopt-helper",
            &"cd".repeat(16),
            &digest(),
        )
        .unwrap()
        .finish(ReleaseOutcome::ScopeEmpty)
        .unwrap();
        let reconciled = reconcile_resources(
            &released.task_id,
            "source-session",
            released.generation,
            "adopt-helper",
        )
        .unwrap();
        let reconciliation = reconciled.transfer.unwrap().reconciliation.unwrap();
        assert!(!reconciliation.exclusive_control);
        assert_eq!(
            reconciliation.entries[0].outcome,
            super::disposition::PlanOutcome::AdoptedReference
        );
        assert_eq!(reconciled.resources[0].cleanup.as_deref(), Some("rm -rf /"));
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }
}
