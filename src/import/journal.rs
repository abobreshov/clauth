//! The import journal, `~/.tollgate/import-journal.json` (spec §3.2).
//!
//! Write-ahead: every entry is written `planned` durably before its op runs,
//! and `done` durably after the op and the `fsync` of every directory it
//! touched. The top-level `state` is the guest-mode contract
//! (`identity::import_completed` reads `"complete"`). No entry ever holds a
//! credential byte: a secret copy records its size only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{Options, Paths, seams};
use crate::identity::ImportState;

pub(crate) const SCHEMA_VERSION: u32 = 1;

/// A journal op (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Op {
    Mkdir,
    Move,
    MoveRelink,
    Capture,
    Copy,
    CopySecret,
    CopyTree,
    MergeRoster,
    MergeJson,
    RewriteToml,
    RetireBin,
    Write,
}

impl Op {
    /// Whether the op moves a chain carrier: a process rescan runs first.
    pub(crate) fn is_carrier_step(self) -> bool {
        matches!(self, Op::Move | Op::MoveRelink | Op::Capture)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Planned,
    Done,
    Undone,
    Skipped,
}

/// A regular live slot a `move_relink` discards (its inode and digest, so a
/// replay can tell it apart).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveRegular {
    pub(crate) ino: u64,
    pub(crate) sha256: String,
}

/// The facts an entry records before (`prior`) or after (`after`) its op.
/// Every field is optional; each op fills the ones spec §3.2 names for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Facts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ino: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dev: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) nlink: Option<u64>,
    /// The live slot a relink or capture repoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) link: Option<PathBuf>,
    /// Where that slot pointed before (the upstream store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) link_target: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) live_regular: Option<LiveRegular>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) live_ino: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) store_ino: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
    /// The temp symlink a relink renames over the slot (replay removes a
    /// leftover in either direction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) temp: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) created: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) added_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) backup: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prior_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retired: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bin_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) shim_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exists: Option<bool>,
}

/// One journaled step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) seq: u64,
    pub(crate) op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) src: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dst: Option<PathBuf>,
    pub(crate) secret: bool,
    #[serde(default)]
    pub(crate) prior: Facts,
    #[serde(default)]
    pub(crate) after: Facts,
    pub(crate) status: Status,
}

/// An upstream binary F1 retires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BinRecord {
    pub(crate) path: PathBuf,
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) sha256: String,
}

/// An imported profile: its upstream and tollgate names, harness, and the
/// install source recorded before its store moved (the M8 assert).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProfileRecord {
    pub(crate) name: String,
    pub(crate) dst: String,
    pub(crate) harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) install_source: Option<String>,
}

/// The journal document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub(crate) schema_version: u32,
    pub(crate) state: String,
    pub(crate) tool_version: String,
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
    pub(crate) completed_at: Option<String>,
    pub(crate) uid: u32,
    pub(crate) source: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) source_dev: u64,
    pub(crate) options: Options,
    #[serde(default)]
    pub(crate) upstream_bins: Vec<BinRecord>,
    /// Additive to spec §3.2: the imported profiles, so a rollback maps
    /// names back and a resume re-runs M8's install-source assert.
    #[serde(default)]
    pub(crate) profiles: Vec<ProfileRecord>,
    #[serde(default)]
    pub(crate) pre: Vec<Entry>,
    #[serde(default)]
    pub(crate) main: Vec<Entry>,
    #[serde(default)]
    pub(crate) retire: Vec<Entry>,
    pub(crate) rollback_from: Option<String>,
}

impl Journal {
    pub(crate) fn state(&self) -> ImportState {
        ImportState::from_journal(&self.state).unwrap_or(ImportState::Unreadable)
    }

    pub(crate) fn set_state(&mut self, state: ImportState) {
        self.state = state.as_str().to_string();
    }

    /// Load the journal; `None` when there is none.
    pub(crate) fn load(paths: &Paths) -> Result<Option<Self>> {
        let path = paths.journal();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        let journal: Journal = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} does not parse", path.display()))?;
        Ok(Some(journal))
    }

    /// The durable write (`write_durable_600`): temp, `fsync`, rename,
    /// `fsync(dir)`.
    pub(crate) fn write(&mut self, paths: &Paths) -> Result<()> {
        self.updated_at = super::now_rfc3339();
        let bytes = serde_json::to_vec_pretty(self)?;
        crate::profile::mkdir_700(&paths.target)?;
        crate::profile::write_durable_600(&paths.journal(), &bytes)
            .context("failed to write the import journal")?;
        let summary = self
            .main
            .iter()
            .map(|e| format!("{}:{:?}", e.seq, e.status))
            .collect::<Vec<_>>()
            .join(",");
        let state = self.state.clone();
        seams::log(|| format!("journal state={state} [{summary}]"));
        Ok(())
    }

    /// Move a terminal journal (`aborted`, `rolled_back`) aside so a new
    /// import starts clean: `import-journal.<unix_ms>.json`.
    pub(crate) fn archive(paths: &Paths) -> Result<Option<PathBuf>> {
        let path = paths.journal();
        if !path.exists() {
            return Ok(None);
        }
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let dest = paths.target.join(format!("import-journal.{ms}.json"));
        std::fs::rename(&path, &dest)
            .with_context(|| format!("failed to archive {}", path.display()))?;
        crate::profile::sync_dir(&paths.target)?;
        Ok(Some(dest))
    }
}

/// `tollgate import status`'s document.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusReport {
    pub(crate) schema_version: u32,
    pub(crate) state: &'static str,
    pub(crate) journal: String,
    pub(crate) started_at: Option<String>,
    pub(crate) completed_at: Option<String>,
    pub(crate) steps: BTreeMap<&'static str, BTreeMap<&'static str, usize>>,
    pub(crate) interrupted_at: Option<u64>,
    pub(crate) next: String,
}

fn counts(entries: &[Entry]) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::from([("planned", 0), ("done", 0), ("undone", 0), ("skipped", 0)]);
    for e in entries {
        let key = match e.status {
            Status::Planned => "planned",
            Status::Done => "done",
            Status::Undone => "undone",
            Status::Skipped => "skipped",
        };
        *out.entry(key).or_default() += 1;
    }
    out
}

/// The status of the journal under `paths`, read without any lock.
pub(crate) fn status(paths: &Paths) -> StatusReport {
    let journal_path = paths.tilde(&paths.journal());
    let state = crate::identity::import_state_at(&paths.journal());
    let loaded = Journal::load(paths).ok().flatten();
    let mut steps = BTreeMap::new();
    let mut interrupted_at = None;
    if let Some(j) = &loaded {
        steps.insert("pre", counts(&j.pre));
        steps.insert("main", counts(&j.main));
        steps.insert("retire", counts(&j.retire));
        if state.is_interrupted() {
            interrupted_at = j
                .main
                .iter()
                .find(|e| e.status == Status::Planned)
                .map(|e| e.seq);
        }
    }
    let next = match state {
        ImportState::None => "tollgate import clauth --dry-run".to_string(),
        ImportState::Pre | ImportState::InProgress | ImportState::RollingBack => {
            "tollgate import clauth --resume, or tollgate import rollback".to_string()
        }
        ImportState::Complete => "tollgate import retire".to_string(),
        ImportState::RolledBack | ImportState::Aborted => {
            "tollgate import clauth --dry-run (a new import archives this journal)".to_string()
        }
        ImportState::Unreadable => format!("inspect {journal_path} by hand"),
    };
    StatusReport {
        schema_version: SCHEMA_VERSION,
        state: state.as_str(),
        journal: journal_path,
        started_at: loaded.as_ref().map(|j| j.started_at.clone()),
        completed_at: loaded.as_ref().and_then(|j| j.completed_at.clone()),
        steps,
        interrupted_at,
        next,
    }
}

/// The text form of [`status`].
pub(crate) fn render_status(s: &StatusReport) -> String {
    let mut out = format!("tollgate import: {}", s.state);
    if let Some(at) = &s.started_at {
        out.push_str(&format!("\n  started    {at}"));
    }
    if let Some(at) = &s.completed_at {
        out.push_str(&format!("\n  completed  {at}"));
    }
    for (section, c) in &s.steps {
        let total: usize = c.values().sum();
        if total == 0 {
            continue;
        }
        out.push_str(&format!(
            "\n  {section:<9}  {} done, {} planned, {} undone, {} skipped",
            c["done"], c["planned"], c["undone"], c["skipped"]
        ));
    }
    if let Some(seq) = s.interrupted_at {
        out.push_str(&format!("\n  interrupted at step {seq}"));
    }
    out.push_str(&format!("\n  next: {}", s.next));
    out
}

/// A backup file under `import-backup/` for step `seq`.
pub(crate) fn backup_path(paths: &Paths, seq: u64, name: &str) -> PathBuf {
    paths.backup_dir().join(format!("{seq}-{name}"))
}

/// Write a non-secret byte backup (0600, dir 0700).
pub(crate) fn write_backup(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        crate::profile::mkdir_700(dir)?;
    }
    crate::profile::write_durable_600(path, bytes)
        .with_context(|| format!("failed to write backup {}", path.display()))
}
