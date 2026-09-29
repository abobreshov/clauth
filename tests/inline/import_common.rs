//! Shared scaffolding for the `import clauth` suites (spec §7): a sandbox
//! home with an upstream tree, the engine seams at their safe defaults, and
//! the helpers every suite reads results through.

#![allow(dead_code, reason = "each suite uses its own subset")]

use std::path::{Path, PathBuf};

pub(crate) use crate::testutil::{
    HomeSandbox, LockHolder, TreeSnapshot, UpstreamTree, fixture_access, fixture_oauth_body,
    fixture_refresh,
};

pub(crate) use super::journal::{Journal, Op, Status};
pub(crate) use super::procs::FakeProc;
pub(crate) use super::seams::{self, CrashPoint};
pub(crate) use super::{ImportBlocked, ImportNeedsAttention, Options, Paths, txn};

/// A sandbox home with an empty upstream tree and fresh seams. Field order
/// is drop order: the seams reset before the home goes.
pub(crate) struct Env {
    pub(crate) seams: seams::Guard,
    pub(crate) tree: UpstreamTree,
    pub(crate) home: HomeSandbox,
}

impl Env {
    pub(crate) fn new() -> Self {
        let home = HomeSandbox::new();
        let tree = UpstreamTree::new(&home);
        let seams = seams::install();
        Self { seams, tree, home }
    }

    pub(crate) fn h(&self) -> &Path {
        self.home.home()
    }

    pub(crate) fn paths(&self) -> Paths {
        Paths::at(self.h())
    }

    pub(crate) fn p(&self, rel: &str) -> PathBuf {
        self.h().join(rel)
    }

    /// The upstream `clauth` fixture, pinned as the only `PATH` dir.
    pub(crate) fn with_upstream_bin(&self) -> PathBuf {
        let bin = self.tree.upstream_bin();
        let dirs = vec![bin.clone()];
        self.seams.set(|s| s.path_dirs = dirs);
        bin.join("clauth")
    }

    /// Pose a process table.
    pub(crate) fn procs(&self, table: Vec<FakeProc>) {
        self.seams.set(|s| s.procs = table);
    }

    pub(crate) fn journal(&self) -> Journal {
        Journal::load(&self.paths())
            .expect("journal loads")
            .expect("journal exists")
    }

    pub(crate) fn snapshot(&self) -> TreeSnapshot {
        TreeSnapshot::of(self.h())
    }
}

/// The dry-run survey under `opts`.
pub(crate) fn survey(opts: &Options) -> txn::Survey {
    txn::survey(opts, txn::Mode::DryRun).expect("survey")
}

/// The blocker codes of a survey.
pub(crate) fn codes(s: &txn::Survey) -> Vec<String> {
    s.blockers.iter().map(|b| b.code.clone()).collect()
}

pub(crate) fn warning_codes(s: &txn::Survey) -> Vec<String> {
    s.warnings.iter().map(|b| b.code.clone()).collect()
}

/// Run the import, auto-confirmed.
pub(crate) fn run(opts: &Options) -> anyhow::Result<txn::Committed> {
    txn::run(opts, &mut |_| Ok(true))
}

pub(crate) fn run_ok() -> txn::Committed {
    run(&Options::default()).expect("import commits")
}

pub(crate) fn rollback(opts: &Options) -> anyhow::Result<super::rollback::RolledBack> {
    super::rollback::rollback(opts)
}

/// The blocker codes an `ImportBlocked` error carries.
pub(crate) fn blocked_codes(e: &anyhow::Error) -> Vec<String> {
    e.downcast_ref::<ImportBlocked>()
        .unwrap_or_else(|| panic!("expected ImportBlocked, got {e:#}"))
        .blockers
        .iter()
        .map(|b| b.code.clone())
        .collect()
}

/// `main::exit_code` for an import outcome.
pub(crate) fn exit_of(r: anyhow::Result<()>) -> i32 {
    crate::exit_code(r)
}

pub(crate) fn ino(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .ino()
}

pub(crate) fn nlink(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).expect("stat").nlink()
}

pub(crate) fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).expect("stat").mode() & 0o7777
}

/// Paths the fence and the journal create and leave behind by design
/// (spec I17: absent lock files are created and left; the journal, its
/// archives and the byte backups are the import's own record).
pub(crate) fn is_bookkeeping(rel: &str) -> bool {
    let lock_file = |dir: &str| {
        rel.strip_prefix(dir)
            .is_some_and(|f| !f.contains('/') && f.ends_with(".lock"))
    };
    lock_file(".clauth/")
        || lock_file(".tollgate/")
        || rel == ".clauth/rotation-locks"
        || rel.starts_with(".clauth/rotation-locks/")
        || rel.starts_with(".tollgate/import-journal")
        || rel == ".tollgate/import-backup"
        || rel.starts_with(".tollgate/import-backup/")
}

/// Every output of the leak test: no fixture token may appear.
pub(crate) fn assert_no_fixture_secret(label: &str, text: &str) {
    for needle in [
        "FIXTURE-RT",
        "sk-ant-oat01-FIXTURE",
        "FIXTURE-KEY",
        "FIXTURE-CODEX",
        "FIXTURE-MCP",
    ] {
        assert!(
            !text.contains(needle),
            "{label} echoes a fixture secret ({needle}):\n{text}"
        );
    }
}

/// A process table row running `argv`.
pub(crate) fn proc_row(pid: u32, argv: &[&str]) -> FakeProc {
    FakeProc::new(pid, argv)
}

/// Every file under `dir` (recursively) as text, for a leak scan.
pub(crate) fn read_tree_text(dir: &Path) -> String {
    let mut out = String::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.push_str(&read_tree_text(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    out
}

/// Assert two snapshots are equal, naming only the paths that differ.
pub(crate) fn assert_same_tree(have: &TreeSnapshot, want: &TreeSnapshot, label: &str) {
    let mut diffs = Vec::new();
    for (k, v) in &want.0 {
        match have.0.get(k) {
            None => diffs.push(format!("missing {k}: {v:?}")),
            Some(h) if h != v => diffs.push(format!("changed {k}: want {v:?}, have {h:?}")),
            Some(_) => {}
        }
    }
    for (k, v) in &have.0 {
        if !want.0.contains_key(k) {
            diffs.push(format!("extra {k}: {v:?}"));
        }
    }
    assert!(
        diffs.is_empty(),
        "{label}: trees differ:\n{}",
        diffs.join("\n")
    );
}

/// The view a rollback must restore exactly: everything but the import's
/// bookkeeping, inodes included — except the two upstream rosters F2
/// rewrites, which come back byte- and mode-identical on a new inode (a
/// rewrite is a rename of a fresh file; neither holds a chain).
pub(crate) fn rollback_view(snap: &TreeSnapshot) -> TreeSnapshot {
    let mut v = snap.without(is_bookkeeping);
    for rel in [".clauth/profiles.toml", ".clauth/codex-profiles.toml"] {
        if let Some(node) = v.0.get_mut(rel) {
            node.ino = 0;
        }
    }
    v
}
