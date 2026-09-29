//! The move engine's primitives (spec §4.4): a rename that refuses to fall
//! back to a copy, device and owner checks, the durable 0600 copy, and the
//! tree walk a `copy_tree` plans from. Linux-only in practice (the import
//! refuses every other platform at M-1); the unix bits are gated so the crate
//! still builds elsewhere.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::seams;

/// `EXDEV` (cross-device link), what `rename(2)` answers across filesystems.
const EXDEV: i32 = 18;

/// Metadata the journal records for a moved inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) ino: u64,
    pub(crate) dev: u64,
    pub(crate) mode: u32,
    pub(crate) nlink: u64,
    pub(crate) uid: u32,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
    pub(crate) is_dir: bool,
    pub(crate) is_file: bool,
    pub(crate) is_symlink: bool,
}

/// `lstat(path)`, or `None` when it does not exist (or cannot be read).
pub(crate) fn lmeta(path: &Path) -> Option<Meta> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(to_meta(&meta))
}

#[cfg(unix)]
fn to_meta(meta: &std::fs::Metadata) -> Meta {
    use std::os::unix::fs::MetadataExt as _;
    Meta {
        ino: meta.ino(),
        dev: meta.dev(),
        mode: meta.mode() & 0o7777,
        nlink: meta.nlink(),
        uid: meta.uid(),
        size: meta.size(),
        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        is_dir: meta.is_dir(),
        is_file: meta.is_file(),
        is_symlink: meta.file_type().is_symlink(),
    }
}

#[cfg(not(unix))]
fn to_meta(meta: &std::fs::Metadata) -> Meta {
    Meta {
        ino: 0,
        dev: 0,
        mode: 0,
        nlink: 1,
        uid: 0,
        size: meta.len(),
        mtime_ns: 0,
        is_dir: meta.is_dir(),
        is_file: meta.is_file(),
        is_symlink: meta.file_type().is_symlink(),
    }
}

/// The uid this process runs as: the owner of `/proc/<self>` (no `unsafe`
/// `getuid` call needed), else the home's owner.
pub(crate) fn current_uid(home: &Path) -> u32 {
    // `/proc/self` is a root-owned symlink; its TARGET (`/proc/<pid>`) is
    // owned by the process's uid, so this follows it.
    std::fs::metadata("/proc/self")
        .ok()
        .map(|m| to_meta(&m))
        .or_else(|| lmeta(home))
        .map_or(u32::MAX, |m| m.uid)
}

/// The `st_dev` of `path`, or of its nearest existing ancestor when it does
/// not exist yet (a destination about to be created lands on its parent's
/// filesystem). The [`seams::foreign_dev`] seam poses another filesystem.
pub(crate) fn dev_of(path: &Path) -> Option<u64> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if let Some(m) = lmeta(p) {
            if seams::foreign_dev(p) {
                return Some(m.dev.wrapping_add(1));
            }
            return Some(m.dev);
        }
        cur = p.parent();
    }
    None
}

/// Whether `a` and `b` (or their nearest existing ancestors) share a
/// filesystem.
pub(crate) fn same_device(a: &Path, b: &Path) -> bool {
    match (dev_of(a), dev_of(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// `rename(2)`, never a copy: an `EXDEV` (or any error) fails the step.
pub(crate) fn rename(src: &Path, dst: &Path) -> Result<()> {
    if seams::exdev(src) {
        return Err(std::io::Error::from_raw_os_error(EXDEV)).with_context(|| {
            format!(
                "{} and {} are on different filesystems; a credential is never copied",
                src.display(),
                dst.display()
            )
        });
    }
    std::fs::rename(src, dst).map_err(|e| {
        let cross = e.raw_os_error() == Some(EXDEV);
        let err = anyhow::Error::new(e);
        if cross {
            err.context(format!(
                "{} and {} are on different filesystems; a credential is never copied",
                src.display(),
                dst.display()
            ))
        } else {
            err.context(format!(
                "failed to rename {} to {}",
                src.display(),
                dst.display()
            ))
        }
    })
}

/// Whether an error chain carries `EXDEV`.
pub(crate) fn is_exdev(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.raw_os_error() == Some(EXDEV))
    })
}

/// `chmod`, unix only.
pub(crate) fn chmod(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to chmod {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// `fsync` the directories an op touched; a missing one is skipped.
pub(crate) fn sync_dirs(dirs: &[&Path]) -> Result<()> {
    for dir in dirs {
        if dir.is_dir() {
            crate::profile::sync_dir(dir)
                .with_context(|| format!("failed to fsync {}", dir.display()))?;
        }
    }
    Ok(())
}

/// The parent of `path`, or `path` itself when it has none.
pub(crate) fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// SHA-256 of the file at `path`, `None` when it cannot be read.
pub(crate) fn sha256_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| sha256_hex(&b))
}

/// Copy one regular file through the durable 0600 write. Returns its size.
pub(crate) fn copy_file_600(src: &Path, dst: &Path) -> Result<u64> {
    let bytes = std::fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    crate::profile::write_durable_600(dst, &bytes)
        .with_context(|| format!("failed to write {}", dst.display()))?;
    Ok(bytes.len() as u64)
}

/// Create one directory (its parent must exist) at 0700.
pub(crate) fn mkdir_one_700(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(e) => return Err(e).with_context(|| format!("failed to create {}", path.display())),
    }
    chmod(path, 0o700)
}

/// A planned tree copy: the relative paths to create at the destination,
/// parents before children (`.` when the destination root itself is new),
/// plus the file and byte counts and the symlinks the copy skips.
#[derive(Debug, Clone, Default)]
pub(crate) struct TreePlan {
    pub(crate) created: Vec<String>,
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) skipped_links: Vec<PathBuf>,
}

/// Plan copying the tree at `src` onto `dst`, the destination winning on an
/// existing name. Never follows a symlink.
pub(crate) fn plan_tree(src: &Path, dst: &Path) -> Result<TreePlan> {
    let mut plan = TreePlan::default();
    if !dst.exists() {
        plan.created.push(".".to_string());
    }
    walk_plan(src, dst, Path::new(""), &mut plan)?;
    Ok(plan)
}

fn walk_plan(src_root: &Path, dst_root: &Path, rel: &Path, plan: &mut TreePlan) -> Result<()> {
    let dir = src_root.join(rel);
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .flatten()
        .map(|e| e.file_name())
        .collect();
    names.sort();
    for name in names {
        let rel_child = rel.join(&name);
        let src = src_root.join(&rel_child);
        let Some(meta) = lmeta(&src) else { continue };
        if meta.is_symlink {
            plan.skipped_links.push(src);
            continue;
        }
        let dst = dst_root.join(&rel_child);
        let exists = std::fs::symlink_metadata(&dst).is_ok();
        if meta.is_dir {
            if !exists {
                plan.created.push(rel_child.display().to_string());
            }
            if !exists || dst.is_dir() {
                walk_plan(src_root, dst_root, &rel_child, plan)?;
            }
        } else if meta.is_file && !exists {
            plan.created.push(rel_child.display().to_string());
            plan.files += 1;
            plan.bytes += meta.size;
        }
    }
    Ok(())
}

/// Perform (or finish) a planned tree copy: every `created` path that does
/// not exist yet is made, directories at 0700 and files through the durable
/// 0600 write. Idempotent, so a replay after a crash completes it.
pub(crate) fn copy_planned(src: &Path, dst: &Path, created: &[String]) -> Result<()> {
    for rel in created {
        let (s, d) = if rel == "." {
            (src.to_path_buf(), dst.to_path_buf())
        } else {
            (src.join(rel), dst.join(rel))
        };
        if std::fs::symlink_metadata(&d).is_ok() {
            continue;
        }
        let Some(meta) = lmeta(&s) else {
            bail!("{} vanished during the import", s.display());
        };
        if meta.is_dir {
            mkdir_one_700(&d)?;
        } else if meta.is_file {
            copy_file_600(&s, &d)?;
        }
    }
    Ok(())
}

/// How many `created` paths exist at `dst`.
pub(crate) fn count_present(dst: &Path, created: &[String]) -> usize {
    created
        .iter()
        .filter(|rel| {
            let p = if rel.as_str() == "." {
                dst.to_path_buf()
            } else {
                dst.join(rel.as_str())
            };
            std::fs::symlink_metadata(p).is_ok()
        })
        .count()
}

/// Remove what a tree copy created, children before parents. A created
/// directory that now holds something else (written after the import) is
/// kept and reported.
pub(crate) fn remove_created(dst: &Path, created: &[String]) -> Vec<PathBuf> {
    let mut kept = Vec::new();
    for rel in created.iter().rev() {
        let p = if rel == "." {
            dst.to_path_buf()
        } else {
            dst.join(rel)
        };
        let Some(meta) = lmeta(&p) else { continue };
        let removed = if meta.is_dir {
            std::fs::remove_dir(&p)
        } else {
            std::fs::remove_file(&p)
        };
        if removed.is_err() {
            kept.push(p);
        }
    }
    kept
}

/// Whether `name` is shaped like a chain carrier or a staged chain: the
/// files a rollback must never delete with a profile dir.
pub(crate) fn is_carrier_shaped(name: &str) -> bool {
    matches!(
        name,
        "credentials.json"
            | "session-token.json"
            | "session-token.static.json"
            | "mcp-logins.json"
            | "quarantine"
            | "auth.json"
            | "auth.lkg.json"
            | "auth.quarantine.json"
            | ".credentials.json"
    ) || name.ends_with(".pending")
        || is_stray_temp(name)
}

/// A crashed atomic write's staging file (`.<name>.tmp.<…>`), which may hold
/// the newest chain.
pub(crate) fn is_stray_temp(name: &str) -> bool {
    name.starts_with('.') && name.contains(".tmp.")
}

/// Whether the tree at `dir` holds a regular file named one of `names` at any
/// depth, symlinks never followed. Returns the first such path.
pub(crate) fn find_regular_named(dir: &Path, names: &[&str], depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut children: Vec<_> = entries.flatten().collect();
    children.sort_by_key(|e| e.file_name());
    for entry in children {
        let path = entry.path();
        let Some(meta) = lmeta(&path) else { continue };
        if meta.is_symlink {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if meta.is_file && names.contains(&name.as_str()) {
            return Some(path);
        }
        if meta.is_dir
            && let Some(found) = find_regular_named(&path, names, depth - 1)
        {
            return Some(found);
        }
    }
    None
}

/// Point `link` at `target` atomically: a temp symlink at `temp`, renamed
/// over `link`. A leftover `temp` from a crash is replaced.
#[cfg(unix)]
pub(crate) fn repoint_link(link: &Path, target: &Path, temp: &Path) -> Result<()> {
    if std::fs::symlink_metadata(temp).is_ok() {
        std::fs::remove_file(temp)
            .with_context(|| format!("failed to remove stale {}", temp.display()))?;
    }
    std::os::unix::fs::symlink(target, temp)
        .with_context(|| format!("failed to create {}", temp.display()))?;
    if let Err(e) = std::fs::rename(temp, link) {
        let _ = std::fs::remove_file(temp);
        return Err(e).with_context(|| format!("failed to repoint {}", link.display()));
    }
    sync_dirs(&[parent(link)])
}

#[cfg(not(unix))]
pub(crate) fn repoint_link(link: &Path, _target: &Path, _temp: &Path) -> Result<()> {
    bail!("{}: symlinks need unix", link.display())
}

/// Where `link` points (a relative target resolved against its dir), `None`
/// when it is not a symlink.
pub(crate) fn link_target(link: &Path) -> Option<PathBuf> {
    crate::profile::symlink_target(link)
}

/// `path` with `.` and `..` folded away lexically (no filesystem access).
pub(crate) fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether the symlink at `link` names `target`: a relative target (either
/// side) is taken against the link's own directory, then compared lexically
/// and, when both exist, canonically.
pub(crate) fn resolves_to(link: &Path, target: &Path) -> bool {
    let Some(have) = link_target(link) else {
        return false;
    };
    let want = if target.is_relative() {
        parent(link).join(target)
    } else {
        target.to_path_buf()
    };
    if normalize(&have) == normalize(&want) {
        return true;
    }
    matches!((have.canonicalize(), want.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Remove `path` when it exists; a missing file is fine.
pub(crate) fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

/// Whether a directory is writable by its owner and owned by `uid`: where F1
/// may put a shim.
pub(crate) fn owned_writable_dir(dir: &Path, uid: u32) -> bool {
    lmeta(dir).is_some_and(|m| m.is_dir && m.uid == uid && m.mode & 0o200 != 0)
}
