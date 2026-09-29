//! The transaction (spec §4.1): the M-1 precheck ([`survey`]), the report,
//! the plan, and the engine that runs M3–M8 journaled write-ahead, resumes an
//! interrupted journal forward, and reverses automatically on any refusal
//! once a store moved.
//!
//! Every op is driven through three functions over its journal entry:
//! [`probe`] (does disk match the entry's `prior`, its `after`, a known
//! in-between state, or neither), [`apply`] (drive it to `after`,
//! idempotently) and [`revert`] (drive it back). A replay after a crash is
//! then the same loop as a first run: an entry whose disk matches neither
//! stops everything with exit 4.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use super::fence::{Fence, LockRow};
use super::inventory::{Action, Inventory};
use super::journal::{
    self, BinRecord, Entry, Facts, Journal, LiveRegular, Op, ProfileRecord, Status,
};
use super::procs::{self, ProcRow};
use super::roster::{self, Upstream};
use super::seams::{self, CrashPoint};
use super::slots::{self, ClaudePlan, CodexPlan, Slot, SlotRow};
use super::{Finding, ImportBlocked, ImportNeedsAttention, Options, Paths, fsops};
use crate::harness::Harness;
use crate::identity::ImportState;

/// The version string the retired upstream binary is named after.
pub(crate) const UPSTREAM_VERSION: &str = "0.16.0";

/// The shim F1 puts where the upstream binary was.
pub(crate) const SHIM: &str = "#!/bin/sh\necho \"clauth: migrated to tollgate (~/.tollgate); run tollgate, or 'tollgate import rollback'\" >&2\nexit 1\n";

/// Why the survey runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    DryRun,
    Run,
    /// Re-validation under the fence (M4): the lock probes would see the
    /// fence's own locks, so they are skipped.
    InFence,
}

/// Everything the precheck learned.
#[derive(Debug, Clone)]
pub(crate) struct Survey {
    pub(crate) paths: Paths,
    pub(crate) opts: Options,
    pub(crate) upstream: Upstream,
    pub(crate) inv: Inventory,
    pub(crate) claude: Slot<ClaudePlan>,
    pub(crate) codex: Slot<CodexPlan>,
    pub(crate) bins: Vec<BinRecord>,
    pub(crate) blockers: Vec<Finding>,
    pub(crate) warnings: Vec<Finding>,
    pub(crate) locks: Vec<LockRow>,
    pub(crate) procs: Vec<ProcRow>,
    pub(crate) journal_state: ImportState,
    pub(crate) hash: String,
    pub(crate) codex_in_scope: bool,
    pub(crate) plan: Option<Plan>,
}

impl Survey {
    fn empty(paths: Paths, opts: &Options) -> Self {
        fn slot<P>(plan: P) -> Slot<P> {
            Slot {
                state: "missing",
                profile: None,
                verdict: "untouched",
                plan,
            }
        }
        Self {
            paths,
            opts: opts.clone(),
            upstream: Upstream::default(),
            inv: Inventory::default(),
            claude: slot(ClaudePlan::Untouched),
            codex: slot(CodexPlan::Untouched),
            bins: Vec::new(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            locks: Vec::new(),
            procs: Vec::new(),
            journal_state: ImportState::None,
            hash: String::new(),
            codex_in_scope: false,
            plan: None,
        }
    }

    /// Every upstream profile name (claude and codex), for fence item 7.
    pub(crate) fn upstream_names(&self) -> Vec<String> {
        self.inv.profiles.iter().map(|p| p.name.clone()).collect()
    }

    pub(crate) fn scope(&self) -> procs::Scope {
        procs::Scope {
            codex_in_scope: self.codex_in_scope,
            upstream_bins: self.bins.iter().map(|b| b.path.clone()).collect(),
            self_exe: seams::current_exe(),
        }
    }
}

/// The M-1 precheck: platform, upstream presence, journal state, M2 (the
/// inventory, rosters and names, live slots, `dev_build_exe`, the upstream
/// binaries), M1 (processes and markers), the off-`PATH` build scan and —
/// outside the fence — the read-only lock probes. Writes nothing.
pub(crate) fn survey(opts: &Options, mode: Mode) -> Result<Survey> {
    let paths = Paths::resolve()?;
    survey_at(&paths, opts, mode, false)
}

pub(crate) fn survey_at(
    paths: &Paths,
    opts: &Options,
    mode: Mode,
    resuming: bool,
) -> Result<Survey> {
    let mut s = Survey::empty(paths.clone(), opts);
    if !seams::is_linux() {
        s.blockers.push(Finding::new(
            "unsupported_platform",
            "tollgate import clauth runs on Linux only; the macOS Keychain slot is not moved",
        ));
        return Ok(s);
    }
    if !paths.source.is_dir() {
        s.blockers.push(Finding::new(
            "upstream_absent",
            "~/.clauth does not exist; there is nothing to import",
        ));
        return Ok(s);
    }
    s.journal_state = crate::identity::import_state_at(&paths.journal());
    match s.journal_state {
        ImportState::Complete => s.blockers.push(Finding::new(
            "already_imported",
            "clauth was already imported (journal complete); see 'tollgate import status'",
        )),
        ImportState::Pre | ImportState::InProgress | ImportState::RollingBack if !resuming => {
            s.blockers.push(Finding::new(
                "journal_pending",
                "tollgate: an import of clauth was interrupted; run 'tollgate import clauth --resume' or 'tollgate import rollback'",
            ));
        }
        ImportState::Unreadable => s.blockers.push(Finding::new(
            "journal_unreadable",
            format!(
                "{} does not parse; inspect it by hand",
                paths.tilde(&paths.journal())
            ),
        )),
        _ => {}
    }
    // M2: static refusals.
    let (upstream, roster_blockers) = roster::load_upstream(paths);
    s.blockers.extend(roster_blockers);
    let inv = super::inventory::classify(paths, &upstream, opts, resuming);
    s.blockers.extend(inv.blockers.iter().cloned());
    s.warnings.extend(inv.warnings.iter().cloned());
    let names: Vec<(String, Harness)> = inv
        .profiles
        .iter()
        .map(|p| (p.name.clone(), p.harness))
        .collect();
    if !resuming {
        s.blockers.extend(roster::check_names(paths, &names, opts));
    }
    let (claude, cb, cw) = slots::classify_claude(paths, &inv, &upstream, opts);
    let (codex, xb) = slots::classify_codex(paths, &inv);
    s.blockers.extend(cb);
    s.warnings.extend(cw);
    s.blockers.extend(xb);
    s.codex_in_scope = codex.plan != CodexPlan::Untouched
        || codex.verdict == "refuse"
        || inv.items.iter().any(|i| {
            i.action == Action::Move
                && inv
                    .profiles
                    .iter()
                    .any(|p| Some(&p.name) == i.profile.as_ref() && p.harness == Harness::Codex)
        });
    if let Some(b) = dev_build_blocker() {
        s.blockers.push(b);
    }
    let uid = fsops::current_uid(&paths.home);
    let (bins, bb, bw) = upstream_bins(paths, uid);
    s.bins = bins;
    s.blockers.extend(bb);
    s.warnings.extend(bw);
    s.upstream = upstream;
    s.inv = inv;
    s.claude = claude;
    s.codex = codex;
    s.warnings.extend(roster_warnings(&s));
    // M1: processes and markers.
    let scan = procs::check(paths, &s.scope());
    s.blockers.extend(scan.blockers);
    s.warnings.extend(scan.warnings);
    s.procs = scan.rows;
    let (mb, mw) = procs::markers(paths);
    s.blockers.extend(mb);
    s.warnings.extend(mw);
    s.warnings.extend(offpath_builds(paths, uid));
    if mode != Mode::InFence {
        let (locks, lb) = super::fence::probe(paths, &s.upstream_names());
        s.locks = locks;
        s.blockers.extend(lb);
    }
    s.hash = inventory_hash(paths);
    match plan(&s) {
        Ok(p) => s.plan = Some(p),
        Err(e) => s.blockers.push(Finding::new(
            "plan_failed",
            format!("the import cannot be planned: {e:#}"),
        )),
    }
    Ok(s)
}

/// `dev_build_exe` (spec §4.8 G3, a part-1 M2 refusal): this binary must be
/// the first `tollgate` on `PATH`, so the helper a later edit writes never
/// points at a `target/` build.
fn dev_build_blocker() -> Option<Finding> {
    let exe = seams::current_exe()?;
    let on_path = seams::path_dirs()
        .into_iter()
        .map(|d| d.join(crate::identity::NAME))
        .find(|p| p.is_file());
    let same = match &on_path {
        Some(p) => matches!((exe.canonicalize(), p.canonicalize()), (Ok(a), Ok(b)) if a == b),
        None => false,
    };
    (!same).then(|| {
        Finding::new(
            "dev_build_exe",
            format!(
                "this tollgate is {}, not the installed binary on PATH; run the installed tollgate so the rewritten helper points at it",
                exe.display()
            ),
        )
    })
}

/// F1's targets: every user-owned upstream `clauth` on `PATH`, canonical.
fn upstream_bins(paths: &Paths, uid: u32) -> (Vec<BinRecord>, Vec<Finding>, Vec<Finding>) {
    let mut bins: Vec<BinRecord> = Vec::new();
    let mut blockers = Vec::new();
    for dir in seams::path_dirs() {
        let cand = dir.join(crate::identity::UPSTREAM_NAME);
        if fsops::lmeta(&cand).is_none() {
            continue;
        }
        let Ok(canonical) = cand.canonicalize() else {
            continue;
        };
        if bins.iter().any(|b| b.path == canonical) {
            continue;
        }
        let Some(meta) = fsops::lmeta(&canonical).filter(|m| m.is_file) else {
            continue;
        };
        #[cfg(test)]
        assert!(
            canonical.starts_with(
                paths
                    .home
                    .canonicalize()
                    .unwrap_or_else(|_| paths.home.clone())
            ),
            "a test resolved an upstream binary outside its sandbox: {}",
            canonical.display()
        );
        let writable = meta.uid == uid && fsops::owned_writable_dir(fsops::parent(&canonical), uid);
        if !writable {
            blockers.push(
                Finding::new(
                    "upstream_binary_unretirable",
                    format!(
                        "{} is not this user's to rename; remove or relink it, then retry",
                        paths.tilde(&canonical)
                    ),
                )
                .with_path(paths.tilde(&canonical)),
            );
            continue;
        }
        let sha = fsops::sha256_file(&canonical).unwrap_or_default();
        bins.push(BinRecord {
            path: canonical,
            ino: meta.ino,
            size: meta.size,
            sha256: sha,
        });
    }
    let mut warnings = Vec::new();
    if bins.is_empty() && blockers.is_empty() {
        warnings.push(Finding::new(
            "upstream_binary_absent",
            "no upstream clauth binary is on PATH; nothing is retired",
        ));
    }
    (bins, blockers, warnings)
}

/// The off-`PATH` upstream builds (spec §4.9): every uid-owned executable
/// `clauth` under `<src>/target/*/`, where `<src>` is a cargo install
/// record's path source. Metadata only: nothing is executed or read.
fn offpath_builds(paths: &Paths, uid: u32) -> Vec<Finding> {
    let record = paths.home.join(".cargo").join(".crates2.json");
    let Ok(bytes) = std::fs::read(&record) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Some(installs) = doc.get("installs").and_then(|v| v.as_object()) else {
        return out;
    };
    for key in installs.keys() {
        if !key.starts_with("clauth ") {
            continue;
        }
        let Some(src) = key
            .split_once("(path+file://")
            .and_then(|(_, rest)| rest.strip_suffix(')'))
        else {
            continue;
        };
        let target = Path::new(src).join("target");
        let Ok(rd) = std::fs::read_dir(&target) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let cand = dir.join(crate::identity::UPSTREAM_NAME);
            if fsops::lmeta(&cand).is_some_and(|m| m.is_file && m.uid == uid && m.mode & 0o111 != 0)
            {
                let shown = paths.tilde(&cand);
                out.push(
                    Finding::new(
                        "upstream_offpath_build",
                        format!(
                            "upstream clauth build {shown} is not on PATH and is not retired; do not run it (or 'cargo run' on the mommy branch) until R2's start-time reconcile ships"
                        ),
                    )
                    .with_path(shown),
                );
            }
        }
    }
    out
}

/// A digest of the upstream tree's metadata (fence-held lock files
/// excluded) and of both live slots, so M4 can tell the tree it plans from
/// is the tree M-1 inspected.
pub(crate) fn inventory_hash(paths: &Paths) -> String {
    let mut lines = Vec::new();
    walk_hash(&paths.source, Path::new(""), &mut lines, true);
    for slot in [paths.claude_slot(), paths.codex_slot()] {
        lines.push(meta_line("slot", &slot));
    }
    fsops::sha256_hex(lines.join("\n").as_bytes())
}

fn meta_line(rel: &str, path: &Path) -> String {
    match fsops::lmeta(path) {
        Some(m) => format!(
            "{rel}|{}|{}|{}|{}|{}|{:?}",
            m.ino,
            m.size,
            m.mtime_ns,
            m.nlink,
            m.mode,
            fsops::link_target(path)
        ),
        None => format!("{rel}|absent"),
    }
}

fn walk_hash(root: &Path, rel: &Path, lines: &mut Vec<String>, top: bool) {
    let dir = root.join(rel);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut names: Vec<_> = rd.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let n = name.to_string_lossy();
        if top && super::inventory::is_never(&n) {
            continue;
        }
        let child = rel.join(&name);
        let path = root.join(&child);
        lines.push(meta_line(&child.display().to_string(), &path));
        if fsops::lmeta(&path).is_some_and(|m| m.is_dir) {
            walk_hash(root, &child, lines, false);
        }
    }
}

// ── the report ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EntryRow {
    pub(crate) src: String,
    pub(crate) dst: Option<String>,
    pub(crate) action: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) secret: bool,
    pub(crate) carrier: bool,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LiveSlots {
    pub(crate) claude: SlotRow,
    pub(crate) codex: SlotRow,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RosterRow {
    pub(crate) claude: Vec<String>,
    pub(crate) codex: Vec<String>,
    pub(crate) active: Option<String>,
    pub(crate) renames: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JournalRow {
    pub(crate) state: &'static str,
    pub(crate) steps_planned: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HoldRow {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

/// The JSON report (spec §2.4, schema_version 1). `hold` is additive: the
/// file and byte counts of the tree copies the fence is held across.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) schema_version: u32,
    pub(crate) command: &'static str,
    pub(crate) mode: &'static str,
    pub(crate) generated_at: String,
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) ok: bool,
    pub(crate) blockers: Vec<Finding>,
    pub(crate) warnings: Vec<Finding>,
    pub(crate) entries: Vec<EntryRow>,
    pub(crate) live_slots: LiveSlots,
    pub(crate) roster: RosterRow,
    pub(crate) global_edits: Vec<serde_json::Value>,
    pub(crate) locks: Vec<LockRow>,
    pub(crate) processes: Vec<ProcRow>,
    pub(crate) journal: JournalRow,
    pub(crate) hold: HoldRow,
}

/// Build the report for `survey`.
pub(crate) fn report(s: &Survey, mode: &'static str) -> Report {
    let p = &s.paths;
    let entries = s
        .inv
        .items
        .iter()
        .map(|i| EntryRow {
            src: Paths::relative(&p.source, &i.src),
            dst: i.dst.as_ref().map(|d| Paths::relative(&p.target, d)),
            action: i.action.label(),
            kind: i.kind,
            secret: i.secret,
            carrier: i.carrier,
            reason: i.reason.clone(),
        })
        .collect();
    let (steps, hold) = s
        .plan
        .as_ref()
        .map_or((0, HoldRow { files: 0, bytes: 0 }), |pl| {
            (
                pl.entries.len(),
                HoldRow {
                    files: pl.hold_files,
                    bytes: pl.hold_bytes,
                },
            )
        });
    Report {
        schema_version: 1,
        command: "import clauth",
        mode,
        generated_at: super::now_rfc3339(),
        source: p.source.display().to_string(),
        target: p.target.display().to_string(),
        ok: s.blockers.is_empty(),
        blockers: s.blockers.clone(),
        warnings: s.warnings.clone(),
        entries,
        live_slots: LiveSlots {
            claude: s.claude.row(),
            codex: s.codex.row(),
        },
        roster: RosterRow {
            claude: s
                .inv
                .profiles
                .iter()
                .filter(|p| p.harness == Harness::Claude)
                .map(|p| p.name.clone())
                .collect(),
            codex: s
                .inv
                .profiles
                .iter()
                .filter(|p| p.harness == Harness::Codex)
                .map(|p| p.name.clone())
                .collect(),
            active: s.upstream.claude_active.clone(),
            renames: s.opts.renames.clone(),
        },
        global_edits: Vec::new(),
        locks: s.locks.clone(),
        processes: s.procs.clone(),
        journal: JournalRow {
            state: s.journal_state.as_str(),
            steps_planned: steps,
        },
        hold,
    }
}

/// The report as text.
pub(crate) fn render_text(r: &Report, dry_run: bool) -> String {
    let mut out = Vec::new();
    if dry_run {
        out.push("tollgate import clauth --dry-run: nothing was changed".to_string());
    }
    out.push(format!("  from {} to {}", r.source, r.target));
    let list = |v: &[String]| {
        if v.is_empty() {
            "(none)".to_string()
        } else {
            v.join(", ")
        }
    };
    out.push(format!(
        "  profiles  claude {} · codex {} · active {}",
        list(&r.roster.claude),
        list(&r.roster.codex),
        r.roster.active.as_deref().unwrap_or("(none)")
    ));
    for (old, new) in &r.roster.renames {
        out.push(format!("  rename    {old} -> {new}"));
    }
    out.push(format!(
        "  slots     claude {}{} ({}) · codex {}{} ({})",
        r.live_slots.claude.state,
        r.live_slots
            .claude
            .profile
            .as_deref()
            .map_or_else(String::new, |p| format!(" '{p}'")),
        r.live_slots.claude.verdict,
        r.live_slots.codex.state,
        r.live_slots
            .codex
            .profile
            .as_deref()
            .map_or_else(String::new, |p| format!(" '{p}'")),
        r.live_slots.codex.verdict,
    ));
    for e in &r.entries {
        let dst = e
            .dst
            .as_deref()
            .map_or_else(String::new, |d| format!(" -> {d}"));
        out.push(format!("  {:<9} {}{dst}  ({})", e.action, e.src, e.reason));
    }
    for l in &r.locks {
        out.push(format!("  lock      {} {}", l.path, l.state));
    }
    for p in &r.processes {
        out.push(format!(
            "  process   {} (pid {}) {:?}",
            p.name, p.pid, p.role
        ));
    }
    out.push(format!(
        "  journal   {} steps planned; the fence is held across {} copied files ({} bytes)",
        r.journal.steps_planned, r.hold.files, r.hold.bytes
    ));
    for w in &r.warnings {
        out.push(format!("  warning  {}: {}", w.code, w.message));
    }
    for b in &r.blockers {
        out.push(format!("  blocked  {}: {}", b.code, b.message));
    }
    out.join("\n")
}

/// The command's outcome for `survey`'s blockers: Ok when clean, exit 4 for
/// a pending journal, else exit 3.
pub(crate) fn blocked_outcome(s: &Survey, printed: bool) -> Result<()> {
    if s.blockers.is_empty() {
        return Ok(());
    }
    if s.blockers.iter().any(|b| b.code == "journal_pending") {
        return Err(ImportNeedsAttention {
            state: s.journal_state.as_str().to_string(),
            step: None,
            reason: "an import of clauth was interrupted; run 'tollgate import clauth --resume' or 'tollgate import rollback'"
                .to_string(),
        }
        .into());
    }
    Err(ImportBlocked {
        blockers: s.blockers.clone(),
        printed,
    }
    .into())
}

// ── the plan ───────────────────────────────────────────────────────────────

/// The planned `main` section, the byte backups M4 writes before it, and the
/// imported profiles.
#[derive(Debug, Clone, Default)]
pub(crate) struct Plan {
    pub(crate) entries: Vec<Entry>,
    pub(crate) backups: Vec<(PathBuf, Vec<u8>)>,
    pub(crate) profiles: Vec<ProfileRecord>,
    pub(crate) hold_files: u64,
    pub(crate) hold_bytes: u64,
}

struct Planner<'a> {
    s: &'a Survey,
    plan: Plan,
    seq: u64,
}

impl Planner<'_> {
    fn push(
        &mut self,
        op: Op,
        src: Option<PathBuf>,
        dst: Option<PathBuf>,
        secret: bool,
        prior: Facts,
        after: Facts,
    ) -> u64 {
        self.seq += 1;
        self.plan.entries.push(Entry {
            seq: self.seq,
            op,
            src,
            dst,
            secret,
            prior,
            after,
            status: Status::Planned,
        });
        self.seq
    }

    fn next_seq(&self) -> u64 {
        self.seq + 1
    }
}

fn move_prior(meta: Option<&fsops::Meta>) -> Facts {
    Facts {
        ino: meta.map(|m| m.ino),
        dev: meta.map(|m| m.dev),
        mode: meta.map(|m| m.mode),
        nlink: meta.map(|m| m.nlink),
        ..Facts::default()
    }
}

/// Plan every `main` entry (spec §4.1 M5–M8). Pure: reads, never writes.
pub(crate) fn plan(s: &Survey) -> Result<Plan> {
    let p = &s.paths;
    let mut pl = Planner {
        s,
        plan: Plan::default(),
        seq: 0,
    };
    // M5.0: F1, the first main step (I2).
    for bin in &s.bins {
        let dir = fsops::parent(&bin.path);
        let retired = dir.join(format!("clauth-{UPSTREAM_VERSION}.retired"));
        pl.push(
            Op::RetireBin,
            Some(bin.path.clone()),
            Some(retired.clone()),
            false,
            Facts {
                ino: Some(bin.ino),
                mode: fsops::lmeta(&bin.path).map(|m| m.mode),
                ..Facts::default()
            },
            Facts {
                retired: Some(retired),
                bin_sha256: Some(bin.sha256.clone()),
                shim_sha256: Some(fsops::sha256_hex(SHIM.as_bytes())),
                temp: Some(dir.join(format!(".clauth.tollgate-shim.{}", std::process::id()))),
                ..Facts::default()
            },
        );
    }
    // M5.1: per profile in name order.
    let profiles_root = p.target.join("profiles");
    if !profiles_root.exists() {
        pl.push(
            Op::Mkdir,
            None,
            Some(profiles_root),
            false,
            Facts::default(),
            Facts::default(),
        );
    }
    let mut profiles = s.inv.profiles.clone();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    for prof in &profiles {
        let dst_dir = p.tollgate_profile(&prof.dst);
        pl.plan.profiles.push(ProfileRecord {
            name: prof.name.clone(),
            dst: prof.dst.clone(),
            harness: prof.harness.as_str().to_string(),
            install_source: prof.install_source.clone(),
        });
        if !dst_dir.exists() {
            pl.push(
                Op::Mkdir,
                None,
                Some(dst_dir.clone()),
                false,
                Facts::default(),
                Facts::default(),
            );
        }
        let items: Vec<_> = s
            .inv
            .items
            .iter()
            .filter(|i| i.profile.as_deref() == Some(prof.name.as_str()))
            .collect();
        for item in items.iter().filter(|i| i.action == Action::Move) {
            plan_carrier(&mut pl, prof.name.as_str(), item);
        }
        if let ClaudePlan::Capture { profile, live_ino } = &s.claude.plan
            && *profile == prof.name
        {
            let slot = p.claude_slot();
            let store = p.upstream_profile(profile).join("credentials.json");
            pl.push(
                Op::Capture,
                Some(slot.clone()),
                Some(dst_dir.join("credentials.json")),
                true,
                Facts {
                    live_ino: Some(*live_ino),
                    store_ino: fsops::lmeta(&store).map(|m| m.ino),
                    link: Some(slot.clone()),
                    link_target: Some(store),
                    ..Facts::default()
                },
                Facts {
                    temp: Some(slots::temp_for(&slot)),
                    ..Facts::default()
                },
            );
        }
        for item in items {
            plan_copy(&mut pl, item)?;
        }
    }
    // M5.2: top-level copies and merges, then the guest store.
    for item in s.inv.items.iter().filter(|i| i.profile.is_none()) {
        if item.action == Action::MergeJson {
            plan_merge_json(&mut pl, item)?;
        } else {
            plan_copy(&mut pl, item)?;
        }
    }
    let guest = p.target.join("guest-claude").join("projects");
    if fsops::lmeta(&guest).is_some_and(|m| m.is_dir) {
        let dst = p.claude.join("projects");
        let tree = fsops::plan_tree(&guest, &dst)?;
        pl.plan.hold_files += tree.files;
        pl.plan.hold_bytes += tree.bytes;
        if !tree.created.is_empty() {
            pl.push(
                Op::CopyTree,
                Some(guest),
                Some(dst),
                false,
                Facts::default(),
                Facts {
                    created: Some(tree.created),
                    ..Facts::default()
                },
            );
        }
    }
    // M5.3: the rosters.
    let slot_active = match &s.claude.plan {
        ClaudePlan::Relink { profile, .. }
        | ClaudePlan::Capture { profile, .. }
        | ClaudePlan::RelinkDiscard { profile, .. } => Some(s.opts.dst_name(profile)),
        ClaudePlan::Untouched => None,
    };
    let codex_active = match &s.codex.plan {
        CodexPlan::Relink { profile } => Some(s.opts.dst_name(profile)),
        CodexPlan::Untouched => None,
    };
    let claude_names: Vec<String> = profiles
        .iter()
        .filter(|x| x.harness == Harness::Claude)
        .map(|x| x.dst.clone())
        .collect();
    let codex_names: Vec<String> = profiles
        .iter()
        .filter(|x| x.harness == Harness::Codex)
        .map(|x| x.dst.clone())
        .collect();
    plan_roster(
        &mut pl,
        "profiles.toml",
        s.upstream.claude_doc.as_deref(),
        slot_active.as_deref(),
        &claude_names,
    )?;
    if s.upstream.codex_doc.is_some() || !codex_names.is_empty() {
        plan_roster(
            &mut pl,
            "codex-profiles.toml",
            s.upstream.codex_doc.as_deref(),
            codex_active.as_deref(),
            &codex_names,
        )?;
    }
    // M8: F2 on each upstream roster, then the tombstone.
    for (file, doc) in [
        ("profiles.toml", s.upstream.claude_doc.as_deref()),
        ("codex-profiles.toml", s.upstream.codex_doc.as_deref()),
    ] {
        let Some(doc) = doc else { continue };
        let Some(new) = roster::without_active(doc)? else {
            continue;
        };
        let seq = pl.next_seq();
        let prior_backup = journal::backup_path(p, seq, file);
        let new_backup = journal::backup_path(p, seq, &format!("{file}.new"));
        pl.plan
            .backups
            .push((prior_backup.clone(), doc.as_bytes().to_vec()));
        pl.plan
            .backups
            .push((new_backup.clone(), new.as_bytes().to_vec()));
        pl.push(
            Op::RewriteToml,
            None,
            Some(p.source.join(file)),
            false,
            Facts {
                sha256: Some(fsops::sha256_hex(doc.as_bytes())),
                backup: Some(prior_backup),
                mode: fsops::lmeta(&p.source.join(file)).map(|m| m.mode),
                key: Some("active_profile".to_string()),
                prior_value: roster::active_of(doc),
                exists: Some(true),
                ..Facts::default()
            },
            Facts {
                sha256: Some(fsops::sha256_hex(new.as_bytes())),
                backup: Some(new_backup),
                ..Facts::default()
            },
        );
    }
    pl.push(
        Op::Write,
        None,
        Some(p.tombstone()),
        false,
        Facts::default(),
        Facts::default(),
    );
    Ok(pl.plan)
}

fn plan_carrier(pl: &mut Planner<'_>, profile: &str, item: &super::inventory::Item) {
    let s = pl.s;
    let Some(dst) = item.dst.clone() else { return };
    let file = item
        .src
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut prior = move_prior(item.meta.as_ref());
    let mut after = Facts::default();
    let mut op = Op::Move;
    match &s.claude.plan {
        ClaudePlan::Relink {
            profile: p,
            file: f,
        } if p == profile && *f == file => {
            op = Op::MoveRelink;
            let slot = s.paths.claude_slot();
            prior.link_target =
                Some(std::fs::read_link(&slot).unwrap_or_else(|_| item.src.clone()));
            after.temp = Some(slots::temp_for(&slot));
            prior.link = Some(slot);
        }
        ClaudePlan::RelinkDiscard {
            profile: p,
            file: f,
            live_ino,
            live_sha,
        } if p == profile && *f == file => {
            op = Op::MoveRelink;
            let slot = s.paths.claude_slot();
            prior.link_target = Some(item.src.clone());
            prior.live_regular = Some(LiveRegular {
                ino: *live_ino,
                sha256: live_sha.clone(),
            });
            after.temp = Some(slots::temp_for(&slot));
            prior.link = Some(slot);
        }
        _ => {}
    }
    if let CodexPlan::Relink { profile: p } = &s.codex.plan
        && p == profile
        && file == "auth.json"
    {
        op = Op::MoveRelink;
        let slot = s.paths.codex_slot();
        prior.link_target = Some(std::fs::read_link(&slot).unwrap_or_else(|_| item.src.clone()));
        after.temp = Some(slots::temp_for(&slot));
        prior.link = Some(slot);
    }
    pl.push(op, Some(item.src.clone()), Some(dst), true, prior, after);
}

fn plan_copy(pl: &mut Planner<'_>, item: &super::inventory::Item) -> Result<()> {
    let Some(dst) = item.dst.clone() else {
        return Ok(());
    };
    match item.action {
        Action::Copy | Action::CopySecret => {
            let size = item.meta.map(|m| m.size);
            let secret = item.action == Action::CopySecret;
            let sha = (!secret).then(|| fsops::sha256_file(&item.src)).flatten();
            pl.push(
                if secret { Op::CopySecret } else { Op::Copy },
                Some(item.src.clone()),
                Some(dst),
                secret,
                Facts::default(),
                Facts {
                    size,
                    sha256: sha,
                    ..Facts::default()
                },
            );
        }
        Action::CopyTree | Action::CopyTreeSecret => {
            let tree = fsops::plan_tree(&item.src, &dst)?;
            pl.plan.hold_files += tree.files;
            pl.plan.hold_bytes += tree.bytes;
            if !tree.created.is_empty() {
                pl.push(
                    Op::CopyTree,
                    Some(item.src.clone()),
                    Some(dst),
                    item.action == Action::CopyTreeSecret,
                    Facts::default(),
                    Facts {
                        created: Some(tree.created),
                        ..Facts::default()
                    },
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_opt(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Push a merge entry whose new bytes are `new` over tollgate's `dst`.
fn push_merge(
    pl: &mut Planner<'_>,
    op: Op,
    src: Option<PathBuf>,
    dst: PathBuf,
    new: Vec<u8>,
    added: Vec<String>,
    prior_value: Option<String>,
) -> Result<()> {
    let p = &pl.s.paths;
    let name = dst
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let seq = pl.next_seq();
    let prior_bytes = read_opt(&dst)?;
    let mut prior = Facts {
        exists: Some(prior_bytes.is_some()),
        mode: fsops::lmeta(&dst).map(|m| m.mode),
        prior_value,
        ..Facts::default()
    };
    if let Some(bytes) = prior_bytes {
        let backup = journal::backup_path(p, seq, &format!("tollgate-{name}"));
        prior.sha256 = Some(fsops::sha256_hex(&bytes));
        prior.backup = Some(backup.clone());
        pl.plan.backups.push((backup, bytes));
    }
    let new_backup = journal::backup_path(p, seq, &format!("tollgate-{name}.new"));
    let after = Facts {
        sha256: Some(fsops::sha256_hex(&new)),
        backup: Some(new_backup.clone()),
        added_keys: Some(added),
        ..Facts::default()
    };
    pl.plan.backups.push((new_backup, new));
    pl.push(op, src, Some(dst), false, prior, after);
    Ok(())
}

fn plan_roster(
    pl: &mut Planner<'_>,
    file: &str,
    upstream: Option<&str>,
    slot_active: Option<&str>,
    ensure: &[String],
) -> Result<()> {
    let p = &pl.s.paths;
    let dst = p.target.join(file);
    let current = read_opt(&dst)?.map(|b| String::from_utf8_lossy(&b).into_owned());
    let (mut text, mut added, _warnings) = roster::merge(
        current.as_deref(),
        upstream.unwrap_or(""),
        &pl.s.opts,
        slot_active,
    )?;
    // A profile dir upstream's roster does not list still arrives listed.
    let missing: Vec<String> = ensure
        .iter()
        .filter(|n| !text_lists(&text, n))
        .cloned()
        .collect();
    if !missing.is_empty() {
        let extra = format!(
            "profiles = [{}]\n",
            missing
                .iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let (t, a, _) = roster::merge(Some(&text), &extra, &Options::default(), None)?;
        text = t;
        added.extend(a);
    }
    // The merged roster must load in THIS tollgate, or the import would
    // leave it unable to read its own account list.
    if file == "profiles.toml" {
        toml::from_str::<crate::profile::AppState>(&text)
            .context("the merged profiles.toml would not load in tollgate")?;
    } else {
        toml::from_str::<crate::codex_profiles::CodexState>(&text)
            .context("the merged codex-profiles.toml would not load in tollgate")?;
    }
    let prior_active = current.as_deref().and_then(roster::active_of);
    let src = upstream.map(|_| pl.s.paths.source.join(file));
    push_merge(
        pl,
        Op::MergeRoster,
        src,
        dst,
        text.into_bytes(),
        added,
        prior_active,
    )
}

fn text_lists(text: &str, name: &str) -> bool {
    text.parse::<toml_edit::DocumentMut>()
        .ok()
        .is_some_and(|d| {
            d.get("profiles")
                .and_then(|i| i.as_array())
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(name)))
        })
}

fn plan_merge_json(pl: &mut Planner<'_>, item: &super::inventory::Item) -> Result<()> {
    let Some(dst) = item.dst.clone() else {
        return Ok(());
    };
    let upstream = std::fs::read(&item.src)
        .with_context(|| format!("failed to read {}", item.src.display()))?;
    let current = read_opt(&dst)?;
    let (new, added) = roster::merge_sessions(current.as_deref(), &upstream, &pl.s.opts)?;
    push_merge(
        pl,
        Op::MergeJson,
        Some(item.src.clone()),
        dst,
        new,
        added,
        None,
    )
}

/// The roster-merge warnings (e.g. tollgate's own active profile kept), for
/// the report.
pub(crate) fn roster_warnings(s: &Survey) -> Vec<Finding> {
    let dst = s.paths.target.join("profiles.toml");
    let current = std::fs::read_to_string(dst).ok();
    roster::merge(
        current.as_deref(),
        s.upstream.claude_doc.as_deref().unwrap_or(""),
        &s.opts,
        None,
    )
    .map(|(_, _, w)| w)
    .unwrap_or_default()
}

// ── the ops ────────────────────────────────────────────────────────────────

/// Where disk stands relative to an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disk {
    Prior,
    After,
    /// A known state between the two (a relink's store moved, its slot not
    /// yet repointed; a tree half copied): both directions can finish it.
    Partial,
    /// Matches nothing the entry describes: stop, exit 4.
    Neither,
}

fn path_of(p: &Option<PathBuf>) -> Result<&Path> {
    p.as_deref()
        .ok_or_else(|| anyhow!("journal entry is missing a path"))
}

fn store_state(e: &Entry) -> Result<Disk> {
    let src = fsops::lmeta(path_of(&e.src)?);
    let dst = fsops::lmeta(path_of(&e.dst)?);
    let ino = e.prior.ino;
    Ok(match (src, dst) {
        (Some(s), None) if Some(s.ino) == ino => Disk::Prior,
        (None, Some(d)) if Some(d.ino) == ino => Disk::After,
        _ => Disk::Neither,
    })
}

fn points_at(link: &Path, target: &Path) -> bool {
    fsops::resolves_to(link, target)
}

fn regular_ino(path: &Path, ino: u64) -> bool {
    fsops::lmeta(path).is_some_and(|m| m.is_file && m.ino == ino)
}

fn sha_matches(path: &Path, want: Option<&String>) -> bool {
    match (fsops::sha256_file(path), want) {
        (Some(have), Some(want)) => have == *want,
        (None, None) => !path.exists(),
        _ => false,
    }
}

/// Where disk stands for `e`.
pub(crate) fn probe(e: &Entry) -> Result<Disk> {
    Ok(match e.op {
        Op::Mkdir => {
            let dst = path_of(&e.dst)?;
            match fsops::lmeta(dst) {
                Some(m) if m.is_dir => Disk::After,
                None => Disk::Prior,
                Some(_) => Disk::Neither,
            }
        }
        Op::Move => store_state(e)?,
        Op::MoveRelink => {
            let store = store_state(e)?;
            let link = path_of(&e.prior.link)?;
            let dst = path_of(&e.dst)?;
            let orig = path_of(&e.prior.link_target)?;
            match &e.prior.live_regular {
                None => match store {
                    Disk::Prior if points_at(link, orig) => Disk::Prior,
                    Disk::After if points_at(link, dst) => Disk::After,
                    Disk::After if points_at(link, orig) => Disk::Partial,
                    _ => Disk::Neither,
                },
                Some(live) => {
                    let temp = path_of(&e.after.temp)?;
                    let slot_live = regular_ino(link, live.ino);
                    let temp_live = regular_ino(temp, live.ino);
                    match store {
                        Disk::Prior if slot_live && !temp_live => Disk::Prior,
                        Disk::After if points_at(link, dst) && !temp_live => Disk::After,
                        Disk::After if slot_live || temp_live => Disk::Partial,
                        _ => Disk::Neither,
                    }
                }
            }
        }
        Op::Capture => {
            let link = path_of(&e.src)?;
            let dst = path_of(&e.dst)?;
            let live = e.prior.live_ino.unwrap_or(0);
            let dst_ino = fsops::lmeta(dst).map(|m| m.ino);
            let store_ok = match e.prior.store_ino {
                Some(ino) => dst_ino == Some(ino),
                None => dst_ino.is_none(),
            };
            if regular_ino(link, live) && store_ok {
                Disk::Prior
            } else if dst_ino == Some(live) && points_at(link, dst) {
                Disk::After
            } else if dst_ino == Some(live) && !regular_ino(link, live) {
                Disk::Partial
            } else {
                Disk::Neither
            }
        }
        Op::Copy | Op::CopySecret => {
            let dst = path_of(&e.dst)?;
            match fsops::lmeta(dst) {
                None => Disk::Prior,
                Some(m)
                    if Some(m.size) == e.after.size
                        && (e.op == Op::CopySecret
                            || sha_matches(dst, e.after.sha256.as_ref())) =>
                {
                    Disk::After
                }
                Some(_) => Disk::Neither,
            }
        }
        Op::CopyTree => {
            let dst = path_of(&e.dst)?;
            let created = e.after.created.as_deref().unwrap_or(&[]);
            let n = fsops::count_present(dst, created);
            if n == created.len() {
                Disk::After
            } else if n == 0 {
                Disk::Prior
            } else {
                Disk::Partial
            }
        }
        Op::MergeRoster | Op::MergeJson | Op::RewriteToml => {
            let dst = path_of(&e.dst)?;
            if sha_matches(dst, e.after.sha256.as_ref()) {
                Disk::After
            } else if sha_matches(dst, e.prior.sha256.as_ref()) {
                Disk::Prior
            } else {
                Disk::Neither
            }
        }
        Op::RetireBin => {
            let bin = path_of(&e.src)?;
            let retired = path_of(&e.after.retired)?;
            let bin_sha = e.after.bin_sha256.as_ref();
            let shim_sha = e.after.shim_sha256.as_ref();
            let retired_ok = sha_matches(retired, bin_sha);
            if sha_matches(bin, bin_sha) && !retired.exists() {
                Disk::Prior
            } else if retired_ok && sha_matches(bin, shim_sha) {
                Disk::After
            } else if retired_ok && !bin.exists() {
                Disk::Partial
            } else {
                Disk::Neither
            }
        }
        Op::Write => {
            if path_of(&e.dst)?.exists() {
                Disk::After
            } else {
                Disk::Prior
            }
        }
    })
}

/// Move a carrier store `src` → `dst` by rename, after re-checking it is the
/// journaled inode on the same filesystem (spec §4.4). Idempotent.
fn move_store(e: &Entry) -> Result<()> {
    let src = path_of(&e.src)?;
    let dst = path_of(&e.dst)?;
    if let Some(m) = fsops::lmeta(src)
        && fsops::lmeta(dst).is_none()
    {
        anyhow::ensure!(
            Some(m.ino) == e.prior.ino && Some(m.dev) == e.prior.dev,
            "{} is no longer the inode the journal recorded",
            src.display()
        );
        anyhow::ensure!(
            !m.is_file || m.nlink == 1,
            "{} gained a hard link",
            src.display()
        );
        anyhow::ensure!(
            fsops::same_device(fsops::parent(src), fsops::parent(dst)),
            "{} and {} are on different filesystems; a credential is never copied",
            src.display(),
            dst.display()
        );
        fsops::rename(src, dst)?;
    }
    if let Some(m) = fsops::lmeta(dst) {
        fsops::chmod(dst, if m.is_dir { 0o700 } else { 0o600 })?;
    }
    fsops::sync_dirs(&[fsops::parent(src), fsops::parent(dst)])
}

/// Move a store back: whatever inode is at `dst` now (tollgate may have
/// rotated it) returns to `src`, with its journaled mode.
fn unmove_store(e: &Entry, warnings: &mut Vec<Finding>) -> Result<()> {
    let src = path_of(&e.src)?;
    let dst = path_of(&e.dst)?;
    match (fsops::lmeta(src), fsops::lmeta(dst)) {
        (None, Some(_)) => {
            fsops::rename(dst, src)?;
            if let Some(mode) = e.prior.mode {
                fsops::chmod(src, mode)?;
            }
            fsops::sync_dirs(&[fsops::parent(src), fsops::parent(dst)])?;
        }
        (Some(_), Some(_)) => anyhow::bail!(
            "{} exists at both {} and {}; nothing was overwritten",
            src.display(),
            src.display(),
            dst.display()
        ),
        (Some(_), None) => {}
        (None, None) => warnings.push(Finding::new(
            "carrier_missing",
            format!(
                "{} is gone from both places; nothing to move back",
                dst.display()
            ),
        )),
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("failed to create {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<()> {
    anyhow::bail!("{}: symlinks need unix", link.display())
}

/// Drive `e` to its `after` state (from `prior` or a partial state).
pub(crate) fn apply(e: &Entry) -> Result<()> {
    match e.op {
        Op::Mkdir => {
            let dst = path_of(&e.dst)?;
            fsops::mkdir_one_700(dst)?;
            fsops::sync_dirs(&[fsops::parent(dst)])
        }
        Op::Move => move_store(e),
        Op::MoveRelink => {
            move_store(e)?;
            let link = path_of(&e.prior.link)?;
            let dst = path_of(&e.dst)?;
            let temp = path_of(&e.after.temp)?;
            match &e.prior.live_regular {
                None => {
                    if !points_at(link, dst) {
                        fsops::repoint_link(link, dst, temp)?;
                    }
                    fsops::remove_if_present(temp)?;
                }
                Some(live) => {
                    if regular_ino(link, live.ino) {
                        fsops::rename(link, temp)?;
                    }
                    if fsops::lmeta(link).is_none() {
                        symlink(dst, link)?;
                    }
                    if regular_ino(temp, live.ino) {
                        std::fs::remove_file(temp)
                            .with_context(|| format!("failed to remove {}", temp.display()))?;
                    }
                }
            }
            fsops::sync_dirs(&[fsops::parent(link)])
        }
        Op::Capture => {
            let link = path_of(&e.src)?;
            let dst = path_of(&e.dst)?;
            let temp = path_of(&e.after.temp)?;
            if regular_ino(link, e.prior.live_ino.unwrap_or(0)) {
                fsops::rename(link, dst)?;
                fsops::chmod(dst, 0o600)?;
            }
            if !points_at(link, dst) {
                fsops::repoint_link(link, dst, temp)?;
            }
            fsops::remove_if_present(temp)?;
            fsops::sync_dirs(&[fsops::parent(link), fsops::parent(dst)])
        }
        Op::Copy | Op::CopySecret => {
            let src = path_of(&e.src)?;
            let dst = path_of(&e.dst)?;
            if fsops::lmeta(dst).is_none() {
                fsops::copy_file_600(src, dst)?;
            }
            Ok(())
        }
        Op::CopyTree => {
            let src = path_of(&e.src)?;
            let dst = path_of(&e.dst)?;
            fsops::copy_planned(src, dst, e.after.created.as_deref().unwrap_or(&[]))
        }
        Op::MergeRoster | Op::MergeJson | Op::RewriteToml => {
            let dst = path_of(&e.dst)?;
            let backup = path_of(&e.after.backup)?;
            let bytes = std::fs::read(backup)
                .with_context(|| format!("failed to read {}", backup.display()))?;
            crate::profile::write_durable_600(dst, bytes)
                .with_context(|| format!("failed to write {}", dst.display()))?;
            // Upstream's roster keeps the mode it had; tollgate's own files
            // are 0600 like every `~/.tollgate` write.
            if e.op == Op::RewriteToml
                && let Some(mode) = e.prior.mode
            {
                fsops::chmod(dst, mode)?;
            }
            Ok(())
        }
        Op::RetireBin => {
            let bin = path_of(&e.src)?;
            let retired = path_of(&e.after.retired)?;
            let temp = path_of(&e.after.temp)?;
            if sha_matches(bin, e.after.bin_sha256.as_ref()) && !retired.exists() {
                fsops::rename(bin, retired)?;
            }
            if !bin.exists() {
                fsops::remove_if_present(temp)?;
                std::fs::write(temp, SHIM)
                    .with_context(|| format!("failed to write {}", temp.display()))?;
                fsops::chmod(temp, 0o755)?;
                fsops::rename(temp, bin)?;
            }
            fsops::sync_dirs(&[fsops::parent(bin)])
        }
        Op::Write => {
            let dst = path_of(&e.dst)?;
            if !dst.exists() {
                let text = format!(
                    "migrated to tollgate {} at {}; data in ~/.tollgate; undo: tollgate import rollback\n",
                    env!("CARGO_PKG_VERSION"),
                    super::now_rfc3339()
                );
                crate::profile::write_durable_600(dst, text)?;
            }
            Ok(())
        }
    }
}

/// Drive `e` back to its `prior` state as far as that is possible, pushing
/// warnings for what stays (spec §4.10). `paths` names the import's roots.
pub(crate) fn revert(e: &Entry, paths: &Paths, warnings: &mut Vec<Finding>) -> Result<()> {
    match e.op {
        Op::Mkdir => {
            let dst = path_of(&e.dst)?;
            if fsops::lmeta(dst).is_none() {
                return Ok(());
            }
            if std::fs::remove_dir(dst).is_ok() {
                return fsops::sync_dirs(&[fsops::parent(dst)]);
            }
            let is_profile = dst.parent() == Some(paths.target.join("profiles").as_path());
            let carrier = std::fs::read_dir(dst).ok().is_some_and(|rd| {
                rd.flatten()
                    .any(|en| fsops::is_carrier_shaped(&en.file_name().to_string_lossy()))
            });
            if is_profile && !carrier {
                std::fs::remove_dir_all(dst)
                    .with_context(|| format!("failed to remove {}", dst.display()))?;
                fsops::sync_dirs(&[fsops::parent(dst)])?;
            } else {
                warnings.push(
                    Finding::new(
                        "directory_kept",
                        format!(
                            "{} is kept: it holds files written after the import",
                            paths.tilde(dst)
                        ),
                    )
                    .with_path(paths.tilde(dst)),
                );
            }
            Ok(())
        }
        Op::Move => unmove_store(e, warnings),
        Op::MoveRelink => {
            let link = path_of(&e.prior.link)?;
            let orig = path_of(&e.prior.link_target)?;
            let temp = path_of(&e.after.temp)?;
            unmove_store(e, warnings)?;
            let restore_live = e
                .prior
                .live_regular
                .as_ref()
                .is_some_and(|live| regular_ino(temp, live.ino));
            if restore_live {
                // A crash left the discarded live copy at the temp path: put it
                // back byte-identical.
                fsops::rename(temp, link)?;
            } else {
                fsops::repoint_link(link, orig, temp)?;
            }
            fsops::remove_if_present(temp)?;
            fsops::sync_dirs(&[fsops::parent(link)])
        }
        Op::Capture => {
            let link = path_of(&e.src)?;
            let orig = path_of(&e.prior.link_target)?;
            let temp = path_of(&e.after.temp)?;
            let live = e.prior.live_ino.unwrap_or(0);
            if regular_ino(link, live) {
                return Ok(());
            }
            if e.prior.store_ino.is_none() {
                // No upstream store existed to move back into: the captured
                // inode goes back to being the live slot itself.
                let dst = path_of(&e.dst)?;
                if regular_ino(dst, live) {
                    fsops::remove_if_present(temp)?;
                    if fsops::lmeta(link).is_some_and(|m| m.is_symlink) {
                        std::fs::remove_file(link)
                            .with_context(|| format!("failed to remove {}", link.display()))?;
                    }
                    fsops::rename(dst, link)?;
                    return fsops::sync_dirs(&[fsops::parent(link), fsops::parent(dst)]);
                }
            }
            // The captured inode is the store now; the store's move back
            // (the entry before this one) takes it home, and the slot links
            // to where it lands.
            fsops::repoint_link(link, orig, temp)?;
            fsops::remove_if_present(temp)
        }
        Op::Copy | Op::CopySecret => fsops::remove_if_present(path_of(&e.dst)?),
        Op::CopyTree => {
            let dst = path_of(&e.dst)?;
            for kept in fsops::remove_created(dst, e.after.created.as_deref().unwrap_or(&[])) {
                warnings.push(Finding::new(
                    "directory_kept",
                    format!(
                        "{} is kept: it holds files written after the import",
                        paths.tilde(&kept)
                    ),
                ));
            }
            Ok(())
        }
        Op::MergeRoster | Op::MergeJson | Op::RewriteToml => revert_bytes(e, warnings),
        Op::RetireBin => {
            let bin = path_of(&e.src)?;
            let retired = path_of(&e.after.retired)?;
            if !sha_matches(retired, e.after.bin_sha256.as_ref()) {
                if sha_matches(bin, e.after.bin_sha256.as_ref()) {
                    return Ok(());
                }
                warnings.push(Finding::new(
                    "retired_binary_missing",
                    format!(
                        "{} is gone, so the shim stays; reinstall upstream by hand: git -C <src> worktree add <tmp> v{UPSTREAM_VERSION} && cargo install --path <tmp> --locked",
                        paths.tilde(retired)
                    ),
                ));
                return Ok(());
            }
            if sha_matches(bin, e.after.shim_sha256.as_ref()) {
                std::fs::remove_file(bin)
                    .with_context(|| format!("failed to remove {}", bin.display()))?;
            }
            if bin.exists() {
                warnings.push(Finding::new(
                    "binary_replaced",
                    format!(
                        "{} is neither the shim nor upstream's; left both alone",
                        paths.tilde(bin)
                    ),
                ));
                return Ok(());
            }
            fsops::rename(retired, bin)?;
            fsops::sync_dirs(&[fsops::parent(bin)])
        }
        Op::Write => fsops::remove_if_present(path_of(&e.dst)?),
    }
}

/// A byte-restoring revert (rosters, session owners, F2): the prior bytes
/// come back when the file still holds exactly what the import wrote; after
/// a later write the undo is semantic instead.
fn revert_bytes(e: &Entry, warnings: &mut Vec<Finding>) -> Result<()> {
    let dst = path_of(&e.dst)?;
    let current = std::fs::read(dst).ok();
    if sha_matches(dst, e.after.sha256.as_ref()) || current.is_none() {
        match &e.prior.backup {
            Some(backup) => {
                let bytes = std::fs::read(backup)
                    .with_context(|| format!("failed to read {}", backup.display()))?;
                crate::profile::write_durable_600(dst, bytes)?;
                if let Some(mode) = e.prior.mode {
                    fsops::chmod(dst, mode)?;
                }
            }
            None => fsops::remove_if_present(dst)?,
        }
        return Ok(());
    }
    let Some(current) = current else {
        return Ok(());
    };
    let new = match e.op {
        Op::MergeRoster => roster::unmerge(
            &String::from_utf8_lossy(&current),
            e.after.added_keys.as_deref().unwrap_or(&[]),
            e.prior.prior_value.as_deref(),
        )?
        .into_bytes(),
        Op::MergeJson => {
            roster::unmerge_sessions(&current, e.after.added_keys.as_deref().unwrap_or(&[]))?
        }
        Op::RewriteToml => match &e.prior.prior_value {
            Some(prior) => {
                roster::with_active(&String::from_utf8_lossy(&current), prior)?.into_bytes()
            }
            None => return Ok(()),
        },
        _ => return Ok(()),
    };
    warnings.push(Finding::new(
        "semantic_undo",
        format!(
            "{} changed after the import; only the import's own keys were taken back",
            dst.display()
        ),
    ));
    crate::profile::write_durable_600(dst, new)?;
    Ok(())
}

// ── the engine ─────────────────────────────────────────────────────────────

/// Why a forward pass stopped.
#[derive(Debug)]
pub(crate) enum Stop {
    /// A blocking process or marker appeared before a carrier step.
    Refusal(Vec<Finding>),
    /// An op failed (`EXDEV`, a vanished inode, the install-source assert).
    Failed(anyhow::Error),
    /// Disk matches neither side of entry `seq`.
    Disagree(u64),
    /// A test's simulated crash: the process "died" here.
    Crash(u64),
}

/// A simulated crash surfaced by the engine (tests only reach it).
#[derive(Debug)]
pub(crate) struct SimulatedCrash(pub(crate) u64);

impl std::fmt::Display for SimulatedCrash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "simulated crash at step {}", self.0)
    }
}

impl std::error::Error for SimulatedCrash {}

/// The rescan before a carrier step: processes and markers.
fn rescan(paths: &Paths, scope: &procs::Scope) -> Vec<Finding> {
    let scan = procs::check(paths, scope);
    let (mut blockers, _) = procs::markers(paths);
    blockers.splice(0..0, scan.blockers);
    blockers
}

/// Run every planned `main` entry forward, journaling each.
pub(crate) fn forward(
    journal: &mut Journal,
    paths: &Paths,
    scope: &procs::Scope,
) -> Result<(), Stop> {
    for i in 0..journal.main.len() {
        if journal.main[i].status != Status::Planned {
            continue;
        }
        let seq = journal.main[i].seq;
        seams::before_step(seq);
        let op = journal.main[i].op;
        if op.is_carrier_step() {
            let blockers = rescan(paths, scope);
            if !blockers.is_empty() {
                return Err(Stop::Refusal(blockers));
            }
        }
        let disk = probe(&journal.main[i]).map_err(Stop::Failed)?;
        if disk == Disk::Neither {
            return Err(Stop::Disagree(seq));
        }
        if seams::crash(seq, CrashPoint::BeforeOp) {
            return Err(Stop::Crash(seq));
        }
        if disk != Disk::After {
            seams::log(|| format!("op {seq}"));
            apply(&journal.main[i]).map_err(Stop::Failed)?;
        }
        if seams::crash(seq, CrashPoint::AfterOp) {
            return Err(Stop::Crash(seq));
        }
        journal.main[i].status = Status::Done;
        journal.write(paths).map_err(Stop::Failed)?;
    }
    Ok(())
}

/// M8's last checks, then `complete`: the final process rescan and the
/// install-source assert, then the one durable write that ends guest mode.
fn commit(journal: &mut Journal, paths: &Paths, scope: &procs::Scope) -> Result<(), Stop> {
    let blockers = rescan(paths, scope);
    if !blockers.is_empty() {
        return Err(Stop::Refusal(blockers));
    }
    for p in &journal.profiles {
        let Some(want) = &p.install_source else {
            continue;
        };
        let got = crate::claude::install_source_in(&paths.tollgate_profile(&p.dst));
        let got = got.file_name().map(|n| n.to_string_lossy().into_owned());
        if got.as_deref() != Some(want.as_str()) {
            return Err(Stop::Failed(anyhow!(
                "profile '{}''s install source changed from {want} to {}; the import is reversed",
                p.dst,
                got.unwrap_or_default()
            )));
        }
    }
    journal.set_state(ImportState::Complete);
    journal.completed_at = Some(super::now_rfc3339());
    journal.write(paths).map_err(Stop::Failed)
}

/// What a committed import brought over.
#[derive(Debug, Clone)]
pub(crate) struct Committed {
    pub(crate) claude: usize,
    pub(crate) codex: usize,
    pub(crate) warnings: Vec<Finding>,
}

fn committed(journal: &Journal, warnings: Vec<Finding>) -> Committed {
    Committed {
        claude: journal
            .profiles
            .iter()
            .filter(|p| p.harness == "claude")
            .count(),
        codex: journal
            .profiles
            .iter()
            .filter(|p| p.harness == "codex")
            .count(),
        warnings,
    }
}

/// The commit line (spec §2.3).
pub(crate) fn commit_message(c: &Committed) -> String {
    format!(
        "tollgate: imported {} claude and {} codex profiles from ~/.clauth; guest mode is off. Next: tollgate import retire",
        c.claude, c.codex
    )
}

/// `tollgate import clauth` (the engine; the part-1 CLI does not reach it).
/// `confirm` sees the report and answers the prompt.
pub(crate) fn run(
    opts: &Options,
    confirm: &mut dyn FnMut(&Report) -> Result<bool>,
) -> Result<Committed> {
    let paths = Paths::resolve()?;
    let first = survey_at(&paths, opts, Mode::Run, false)?;
    blocked_outcome(&first, false)?;
    let mut warnings = first.warnings.clone();
    if !confirm(&report(&first, "run"))? {
        anyhow::bail!("tollgate import clauth: not confirmed; nothing was changed");
    }
    seams::after_confirm();
    // Post-confirmation recheck (I21): the prompt was unbounded.
    let scope = first.scope();
    let mut recheck = procs::check(&paths, &scope).blockers;
    recheck.extend(procs::markers(&paths).0);
    recheck.extend(super::fence::probe(&paths, &first.upstream_names()).1);
    if !recheck.is_empty() {
        return Err(ImportBlocked {
            blockers: recheck,
            printed: false,
        }
        .into());
    }
    // M3.
    let _fence = Fence::acquire(&paths, &first.upstream_names()).map_err(|f| ImportBlocked {
        blockers: vec![f],
        printed: false,
    })?;
    // M4: the same checks under the fence, the same tree.
    let under = survey_at(&paths, opts, Mode::InFence, false)?;
    blocked_outcome(&under, false)?;
    if under.hash != first.hash {
        return Err(ImportBlocked {
            blockers: vec![Finding::new(
                "inventory_changed",
                "~/.clauth or a live slot changed between the check and the fence; nothing was changed, retry",
            )],
            printed: false,
        }
        .into());
    }
    let planned = match under.plan.clone() {
        Some(p) => p,
        None => plan(&under)?,
    };
    if matches!(
        crate::identity::import_state_at(&paths.journal()),
        ImportState::Aborted | ImportState::RolledBack
    ) {
        journal::Journal::archive(&paths)?;
    }
    for (path, bytes) in &planned.backups {
        journal::write_backup(path, bytes)?;
    }
    let now = super::now_rfc3339();
    let mut j = Journal {
        schema_version: journal::SCHEMA_VERSION,
        state: ImportState::InProgress.as_str().to_string(),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: now.clone(),
        updated_at: now,
        completed_at: None,
        uid: fsops::current_uid(&paths.home),
        source: paths.source.clone(),
        target: paths.target.clone(),
        source_dev: fsops::dev_of(&paths.source).unwrap_or(0),
        options: opts.clone(),
        upstream_bins: under.bins.clone(),
        profiles: planned.profiles.clone(),
        pre: Vec::new(),
        main: planned.entries,
        retire: Vec::new(),
        rollback_from: None,
    };
    j.write(&paths)?;
    warnings.extend(under.warnings.iter().cloned());
    let scope = under.scope();
    let outcome = forward(&mut j, &paths, &scope).and_then(|()| commit(&mut j, &paths, &scope));
    finish(outcome, &mut j, &paths, warnings)
}

/// Settle a forward pass: commit, a simulated crash (left as is), or the
/// automatic reversal of every done step (exit 1, `aborted`).
fn finish(
    outcome: Result<(), Stop>,
    j: &mut Journal,
    paths: &Paths,
    mut warnings: Vec<Finding>,
) -> Result<Committed> {
    let reason = match outcome {
        Ok(()) => return Ok(committed(j, warnings)),
        Err(Stop::Crash(seq)) => return Err(SimulatedCrash(seq).into()),
        Err(Stop::Refusal(b)) => b
            .iter()
            .map(|f| format!("{}: {}", f.code, f.message))
            .collect::<Vec<_>>()
            .join("; "),
        Err(Stop::Failed(e)) => {
            if fsops::is_exdev(&e) {
                format!("cross_device: {e:#}")
            } else {
                format!("{e:#}")
            }
        }
        Err(Stop::Disagree(seq)) => {
            format!("journal_disagrees: disk matches neither side of step {seq}")
        }
    };
    super::rollback::reverse(j, paths, &mut warnings, true)?;
    Err(anyhow!(
        "tollgate import clauth: {reason}; every step was reversed (journal aborted)"
    ))
}

/// `tollgate import clauth --resume`: continue an interrupted journal
/// forward under the fence. A step whose disk matches neither side stops
/// everything with exit 4 and changes nothing more.
pub(crate) fn resume() -> Result<Committed> {
    let paths = Paths::resolve()?;
    let Some(mut j) = Journal::load(&paths)? else {
        return Err(crate::usage_error(
            "tollgate import clauth --resume: there is no interrupted import",
        ));
    };
    if j.state() != ImportState::InProgress {
        return Err(crate::usage_error(format!(
            "tollgate import clauth --resume: the journal is {}, not interrupted",
            j.state
        )));
    }
    let scope = journal_scope(&j);
    let names: Vec<String> = j.profiles.iter().map(|p| p.name.clone()).collect();
    let mut blockers = procs::check(&paths, &scope).blockers;
    blockers.extend(procs::markers(&paths).0);
    blockers.extend(super::fence::probe(&paths, &names).1);
    if !blockers.is_empty() {
        return Err(ImportBlocked {
            blockers,
            printed: false,
        }
        .into());
    }
    let _fence = Fence::acquire(&paths, &names).map_err(|f| ImportBlocked {
        blockers: vec![f],
        printed: false,
    })?;
    let outcome = forward(&mut j, &paths, &scope).and_then(|()| commit(&mut j, &paths, &scope));
    match outcome {
        Err(Stop::Disagree(seq)) => Err(ImportNeedsAttention {
            state: j.state.clone(),
            step: Some(seq),
            reason: "journal_disagrees: disk matches neither side of this step; nothing more was changed".to_string(),
        }
        .into()),
        other => finish(other, &mut j, &paths, Vec::new()),
    }
}

/// The process scope a journal implies (resume, rollback).
pub(crate) fn journal_scope(j: &Journal) -> procs::Scope {
    let mut bins: Vec<PathBuf> = j.upstream_bins.iter().map(|b| b.path.clone()).collect();
    for e in &j.main {
        if e.op == Op::RetireBin
            && let Some(r) = &e.after.retired
        {
            bins.push(r.clone());
        }
    }
    procs::Scope {
        codex_in_scope: j.profiles.iter().any(|p| p.harness == "codex"),
        upstream_bins: bins,
        self_exe: seams::current_exe(),
    }
}
