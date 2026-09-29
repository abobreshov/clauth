//! The on-disk layout of a Hermes profile (spec §3) and the child home.
//!
//! ```text
//! ~/.tollgate/profiles/<name>/            0700, tollgate's
//!   hermes-home/                          = HERMES_HOME (contents are Hermes')
//!     shared/                             = HERMES_SHARED_AUTH_DIR
//!     .env                                one line managed by tollgate (§4.2)
//!   child-home/                           = the child's HOME (0700)
//!     .gitconfig  -> ~/.gitconfig         each only when its target exists
//!     .config/git -> ~/.config/git
//!     .ssh        -> ~/.ssh
//!   sessions-<sid>/<sid>                  the liveness marker (flock)
//! ```
//!
//! The child home is what makes the implicit Anthropic route (G10a) harmless:
//! Hermes resolves `Path.home()` from `HOME`, and the child home holds no
//! `.claude`, `.claude.json`, `.codex`, `.qwen`, `.config/gh` or `.hermes`, so
//! Hermes' Claude Code credential reader finds nothing to read or rewrite. It
//! is a sibling of `hermes-home`, never inside it, because `hermes backup` zips
//! the whole Hermes root and would read through the links (D-H18).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::profile::{ProfileName, mkdir_700, profile_dir};

pub(crate) const HERMES_HOME_DIR: &str = "hermes-home";
pub(crate) const CHILD_HOME_DIR: &str = "child-home";
pub(crate) const SHARED_DIR: &str = "shared";

/// The child home's allowlist: `(entry under the child home, target under the
/// operator's home)`. Nothing else may live there (G2a).
pub(crate) const CHILD_HOME_LINKS: [(&str, &str); 3] = [
    (".gitconfig", ".gitconfig"),
    (".config/git", ".config/git"),
    (".ssh", ".ssh"),
];

/// Every path one Hermes profile addresses. `home` is the literal,
/// non-canonical `tollgate_dir()/profiles/<name>/hermes-home`: it is also the
/// exact string `HERMES_HOME` receives, because Hermes tests
/// `Path(HERMES_HOME).parent.name` on the raw value (G1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HermesPaths {
    pub(crate) profile: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) shared: PathBuf,
    pub(crate) child_home: PathBuf,
}

impl HermesPaths {
    pub(crate) fn for_name(name: &str) -> Result<Self> {
        let profile = profile_dir(&ProfileName::from(name))?;
        let home = profile.join(HERMES_HOME_DIR);
        Ok(Self {
            shared: home.join(SHARED_DIR),
            child_home: profile.join(CHILD_HOME_DIR),
            home,
            profile,
        })
    }

    pub(crate) fn env_file(&self) -> PathBuf {
        self.home.join(".env")
    }
}

/// Whether `path` is a Hermes home or a child home by POSITION:
/// `~/.tollgate/profiles/<name>/{hermes-home,child-home}`. The perms sweep
/// stops at these thresholds. A Hermes home's contents are Hermes' own
/// (plugins carry exec bits), and the child home holds links into the
/// operator's home that a sweep must never chmod through.
pub(crate) fn is_perms_threshold(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n == HERMES_HOME_DIR || n == CHILD_HOME_DIR)
        && path
            .parent()
            .is_some_and(crate::profile::is_own_profile_dir)
}

/// Whether `dir` (a profile dir) holds nothing but what a crashed `new` could
/// have left: `hermes-home/`, `child-home/` and `sessions-*` (§4.1 step 5.2).
/// An absent dir counts as adoptable.
pub(crate) fn is_adoptable_leftover(dir: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let known =
            name == HERMES_HOME_DIR || name == CHILD_HOME_DIR || name.starts_with("sessions-");
        if !known {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Create the profile's dirs (all 0700) and the child home's allowlisted
/// links. Idempotent: an existing dir or link is left as it is, so it also
/// adopts a leftover and backfills a missing child home (G2a).
pub(crate) fn build_layout(paths: &HermesPaths) -> Result<()> {
    for dir in [&paths.profile, &paths.home, &paths.shared] {
        mkdir_700(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    build_child_home(&paths.child_home)
}

/// The child home and its links (§3): `.gitconfig`, `.config/git` and
/// `.ssh`, each only when its target exists in the operator's home.
pub(crate) fn build_child_home(child_home: &Path) -> Result<()> {
    mkdir_700(child_home).with_context(|| format!("failed to create {}", child_home.display()))?;
    let operator = crate::profile::home_dir()?;
    for (entry, target) in CHILD_HOME_LINKS {
        let target = operator.join(target);
        if !target.exists() {
            continue;
        }
        let link = child_home.join(entry);
        if link.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = link.parent()
            && parent != child_home
        {
            mkdir_700(parent).with_context(|| format!("failed to create {}", parent.display()))?;
        }
        symlink(&target, &link).with_context(|| {
            format!("failed to link {} -> {}", link.display(), target.display())
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "Hermes profiles are supported on Linux and macOS only",
    ))
}

/// What the child-home audit (G2a) found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChildHomeVerdict {
    Ok,
    /// The child home is absent (a profile made before the rule): the caller
    /// creates it under its RotationGuard.
    Missing,
    /// The first entry that is not on the allowlist, relative to the child
    /// home (`.claude`, `.config/gh`, or `.` for the node itself).
    Foreign(String),
}

/// G2a: `<child-home>` is a dir owned by this uid, not a symlink, and every
/// entry is one of the allowlisted links pointing where §3 says. A looser mode
/// on the node is tightened to 0700.
pub(crate) fn audit_child_home(child_home: &Path) -> Result<ChildHomeVerdict> {
    let meta = match child_home.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ChildHomeVerdict::Missing),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to stat {}", child_home.display()));
        }
    };
    if meta.file_type().is_symlink() || !meta.is_dir() || !owned_by_me(&meta) {
        return Ok(ChildHomeVerdict::Foreign(".".to_string()));
    }
    tighten_700(child_home, &meta);
    let operator = crate::profile::home_dir()?;
    for name in sorted_entries(child_home)? {
        match name.as_str() {
            ".gitconfig" | ".ssh" => {
                if !is_link_to(&child_home.join(&name), &operator.join(&name)) {
                    return Ok(ChildHomeVerdict::Foreign(name));
                }
            }
            ".config" => {
                let dir = child_home.join(".config");
                let Ok(meta) = dir.symlink_metadata() else {
                    return Ok(ChildHomeVerdict::Foreign(name));
                };
                if meta.file_type().is_symlink() || !meta.is_dir() || !owned_by_me(&meta) {
                    return Ok(ChildHomeVerdict::Foreign(name));
                }
                for inner in sorted_entries(&dir)? {
                    let rel = format!(".config/{inner}");
                    if inner != "git"
                        || !is_link_to(&dir.join("git"), &operator.join(".config").join("git"))
                    {
                        return Ok(ChildHomeVerdict::Foreign(rel));
                    }
                }
            }
            _ => return Ok(ChildHomeVerdict::Foreign(name)),
        }
    }
    Ok(ChildHomeVerdict::Ok)
}

fn sorted_entries(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        out.push(entry?.file_name().to_string_lossy().into_owned());
    }
    out.sort();
    Ok(out)
}

fn is_link_to(link: &Path, target: &Path) -> bool {
    link.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        && std::fs::read_link(link).is_ok_and(|t| t == target)
}

#[cfg(unix)]
pub(crate) fn owned_by_me(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and cannot fail.
    #[allow(unsafe_code)]
    let uid = unsafe { libc::getuid() };
    meta.uid() == uid
}

#[cfg(not(unix))]
pub(crate) fn owned_by_me(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Tighten a dir's mode to 0700 when it is looser; best effort.
pub(crate) fn tighten_700(dir: &Path, meta: &std::fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != 0o700 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    let _ = (dir, meta);
}

#[cfg(test)]
#[path = "../../tests/inline/hermes_home.rs"]
mod tests;
