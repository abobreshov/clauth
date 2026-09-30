//! The reverse replay (spec §4.10): `retire` entries, then `main` in
//! reverse with the upstream binary last (stores before the binary), under
//! the same fence. Also the automatic reversal a refusal after M5 started
//! runs (journal `aborted`).
//!
//! Carriers go back as whatever inode tollgate holds now (a rotation since
//! the import is kept, never discarded); copies are deleted only when the
//! journal says the import made them; the rosters come back byte for byte
//! when untouched since, else only the import's own names leave; profiles
//! created after the import stay; nothing of tollgate's roster is ever
//! written into `~/.clauth`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::fence::Fence;
use super::journal::{Entry, Journal, Op, Status};
use super::txn::{self, Disk};
use super::{Finding, ImportBlocked, ImportNeedsAttention, Options, Paths, fsops, procs, slots};
use crate::identity::ImportState;
use crate::profile::ClaudeCredentials;

/// What a finished rollback reports.
#[derive(Debug, Clone)]
pub(crate) struct RolledBack {
    pub(crate) warnings: Vec<Finding>,
}

fn attention(j: &Journal, seq: u64, reason: String) -> anyhow::Error {
    ImportNeedsAttention {
        state: j.state.clone(),
        step: Some(seq),
        reason,
    }
    .into()
}

/// A journal section the reverse replay walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Pre,
    Main,
}

fn section(j: &mut Journal, s: Section) -> &mut Vec<Entry> {
    match s {
        Section::Pre => &mut j.pre,
        Section::Main => &mut j.main,
    }
}

/// Revert entry `i` of section `s` and journal the result.
fn revert_entry(
    j: &mut Journal,
    s: Section,
    i: usize,
    paths: &Paths,
    warnings: &mut Vec<Finding>,
) -> Result<()> {
    let seq = section(j, s)[i].seq;
    let status = section(j, s)[i].status;
    let act = match status {
        Status::Done => true,
        Status::Planned => match txn::probe(&section(j, s)[i])? {
            Disk::Prior => {
                section(j, s)[i].status = Status::Skipped;
                j.write(paths)?;
                return Ok(());
            }
            Disk::After | Disk::Partial => true,
            Disk::Neither => {
                return Err(attention(
                    j,
                    seq,
                    "journal_disagrees: disk matches neither side of this step; the reversal stopped here".to_string(),
                ));
            }
        },
        Status::Undone | Status::Skipped => false,
    };
    if act {
        let entry = section(j, s)[i].clone();
        let op = entry.op;
        super::seams::log(|| format!("undo {seq} {op:?}"));
        if let Err(e) = txn::revert(&entry, paths, warnings) {
            return Err(attention(
                j,
                seq,
                format!("the reversal stopped part-way: {e:#}"),
            ));
        }
        section(j, s)[i].status = Status::Undone;
        j.write(paths)?;
    }
    Ok(())
}

/// Reverse every step of `j` (the fence must be held): `main` in reverse
/// except `retire_bin`, then `retire_bin` (stores before the binary), then
/// `pre` in reverse (G2's config bytes; nothing is spawned). The `retire`
/// section is undone before the fence by [`super::retire::undo_all`].
/// `automatic` ends in `aborted`, a rollback in `rolled_back`; either way
/// the caller runs [`txn::after_fence`] once the fence is released.
pub(crate) fn reverse(
    j: &mut Journal,
    paths: &Paths,
    warnings: &mut Vec<Finding>,
    automatic: bool,
) -> Result<()> {
    if automatic {
        j.rollback_from = Some(j.state.clone());
    }
    j.set_state(ImportState::RollingBack);
    j.write(paths)?;
    let order: Vec<usize> = (0..j.main.len())
        .rev()
        .filter(|&i| j.main[i].op != Op::RetireBin)
        .chain(
            (0..j.main.len())
                .rev()
                .filter(|&i| j.main[i].op == Op::RetireBin),
        )
        .collect();
    for i in order {
        revert_entry(j, Section::Main, i, paths, warnings)?;
    }
    for i in (0..j.pre.len()).rev() {
        revert_entry(j, Section::Pre, i, paths, warnings)?;
    }
    fix_slots(j, paths)?;
    j.set_state(if automatic {
        ImportState::Aborted
    } else {
        ImportState::RolledBack
    });
    j.write(paths)
}

/// The imported `(upstream name, tollgate name)` pairs.
fn imported(j: &Journal) -> Vec<(String, String)> {
    j.profiles
        .iter()
        .map(|p| (p.name.clone(), p.dst.clone()))
        .collect()
}

/// `(profile dir name, file)` when `target` lies under `root/<p>/<f>`.
fn under_profiles(root: &Path, target: &Path) -> Option<(String, String)> {
    let rel = target.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match parts.as_slice() {
        [p, f] => Some((p.clone(), f.clone())),
        _ => None,
    }
}

/// After the replay: a slot still linked into an imported tollgate profile
/// (a `tollgate switch` after the import) is repointed at the restored
/// upstream store, so no slot dangles. Always a link, never a copy.
fn fix_slots(j: &Journal, paths: &Paths) -> Result<()> {
    let pairs = imported(j);
    let root = paths.target.join("profiles");
    for slot in [paths.claude_slot(), paths.codex_slot()] {
        let Some(target) = fsops::link_target(&slot) else {
            continue;
        };
        let Some((dst, file)) = under_profiles(&root, &fsops::normalize(&target)) else {
            continue;
        };
        let Some((src, _)) = pairs.iter().find(|(_, d)| *d == dst) else {
            continue;
        };
        let upstream = paths.upstream_profile(src).join(&file);
        fsops::repoint_link(&slot, &upstream, &slots::temp_for(&slot))?;
    }
    Ok(())
}

fn access_token(path: &Path) -> Option<String> {
    crate::profile::read_json_file::<ClaudeCredentials>(path)
        .ok()?
        .access_token()
        .map(str::to_string)
}

/// The tollgate profile a regular claude slot belongs to at rollback: the
/// one its import relinked or captured, else tollgate's active profile when
/// imported.
fn slot_profile(j: &Journal, paths: &Paths) -> Option<String> {
    let slot = paths.claude_slot();
    let dsts: Vec<String> = j.profiles.iter().map(|p| p.dst.clone()).collect();
    for e in &j.main {
        let linked = match e.op {
            Op::MoveRelink => e.prior.link.as_deref() == Some(slot.as_path()),
            Op::Capture => e.src.as_deref() == Some(slot.as_path()),
            _ => false,
        };
        if linked
            && let Some(dst) = &e.dst
            && let Some(name) = dst.parent().and_then(|p| p.file_name())
        {
            return Some(name.to_string_lossy().into_owned());
        }
    }
    let active = std::fs::read_to_string(paths.target.join("profiles.toml"))
        .ok()
        .and_then(|t| super::roster::active_of(&t))?;
    dsts.contains(&active).then_some(active)
}

/// Whether the regular slot is still the very inode a `capture` or a
/// `move_relink` recorded as the live file (the import stopped before it
/// touched the slot): the replay itself leaves it where it is.
fn slot_is_journaled_live(j: &Journal, slot: &Path) -> bool {
    let Some(ino) = fsops::lmeta(slot).filter(|m| m.is_file).map(|m| m.ino) else {
        return false;
    };
    j.main.iter().any(|e| match e.op {
        Op::MoveRelink => {
            e.prior.link.as_deref() == Some(slot)
                && e.prior.live_regular.as_ref().is_some_and(|l| l.ino == ino)
        }
        Op::Capture => e.src.as_deref() == Some(slot) && e.prior.live_ino == Some(ino),
        _ => false,
    })
}

/// A regular claude slot's fate at rollback.
enum SlotFix {
    /// Nothing to do before the replay.
    None,
    /// Diverged under `--adopt-live`: renamed onto this tollgate store first.
    Adopt(PathBuf),
    /// Same as this tollgate profile's install source: once the stores are
    /// home the copy is replaced by a link to the upstream store. `park`: its
    /// bytes differ from the store's (an `mcpOAuth` only it holds), so it is
    /// kept in the profile's quarantine first, never unlinked; `ino` is the
    /// slot inode seen at the check.
    Relink {
        profile: String,
        park: bool,
        ino: u64,
    },
}

/// Where a rollback keeps a regular slot it relinks (`Relink { park }`).
const ROLLBACK_LIVE: &str = "credentials.json.rollback-live";
/// Where a rollback `--adopt-live` keeps the tollgate store it supersedes.
const ROLLBACK_SUPERSEDED: &str = "credentials.json.rollback-superseded";

fn creds(path: &Path) -> Option<ClaudeCredentials> {
    crate::profile::read_json_file::<ClaudeCredentials>(path).ok()
}

/// The imported tollgate profile other than `p` whose store holds the slot's
/// chain (its refresh or access token).
fn other_owner(
    paths: &Paths,
    pairs: &[(String, String)],
    p: &str,
    live: &ClaudeCredentials,
) -> Option<String> {
    let rt = live.refresh_token().filter(|t| !t.is_empty());
    let at = live.access_token().filter(|t| !t.is_empty());
    pairs
        .iter()
        .map(|(_, d)| d)
        .filter(|d| d.as_str() != p)
        .find(|d| {
            slots::claude_stores(&paths.tollgate_profile(d))
                .iter()
                .filter_map(|f| creds(f))
                .any(|c| {
                    (rt.is_some() && c.refresh_token() == rt)
                        || (at.is_some() && c.access_token() == at)
                })
        })
        .cloned()
}

/// `tollgate import rollback` (the engine; the part-1 CLI does not reach
/// it). Refuses while tollgate, Claude Code, codex (when codex was imported)
/// or the clauth shim runs, while an imported profile holds a staged chain,
/// while the claude slot is on a profile created after the import, and while
/// an upstream store path is occupied.
#[cfg(test)]
pub(crate) fn rollback(opts: &Options) -> Result<RolledBack> {
    rollback_with(opts, &mut |_| Ok(true))
}

/// What a rollback is about to undo, for its confirmation.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub(crate) state: &'static str,
    pub(crate) profiles: usize,
    pub(crate) steps: usize,
}

/// [`rollback`] with a confirmation between its checks and its first write.
pub(crate) fn rollback_with(
    opts: &Options,
    confirm: &mut dyn FnMut(&Pending) -> Result<bool>,
) -> Result<RolledBack> {
    let paths = Paths::resolve()?;
    let Some(mut j) = Journal::load(&paths)? else {
        return Err(crate::usage_error(
            "tollgate import rollback: there is no import to roll back",
        ));
    };
    match j.state() {
        ImportState::Complete
        | ImportState::InProgress
        | ImportState::RollingBack
        | ImportState::Pre => {}
        other => {
            return Err(crate::usage_error(format!(
                "tollgate import rollback: the journal is {}, so there is nothing to roll back",
                other.as_str()
            )));
        }
    }
    let scope = txn::journal_scope(&j);
    let mut blockers = procs::check(&paths, &scope).blockers;
    blockers.extend(procs::markers(&paths).0);
    let pairs = imported(&j);
    for (_, dst) in &pairs {
        let dir = paths.tollgate_profile(dst);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".pending") || fsops::is_carrier_temp(&name) {
                blockers.push(Finding::new(
                    "pending_rotation",
                    format!(
                        "~/.tollgate/profiles/{dst}/{name} is a staged chain; run 'tollgate list' once so tollgate adopts it, then retry"
                    ),
                ));
            }
        }
    }
    let slot = paths.claude_slot();
    let mut fix = SlotFix::None;
    let root = paths.target.join("profiles");
    if let Some(target) = fsops::link_target(&slot) {
        if let Some((p, _)) = under_profiles(&root, &fsops::normalize(&target))
            && !pairs.iter().any(|(_, d)| *d == p)
        {
            blockers.push(Finding::new(
                "live_slot_on_new_profile",
                format!(
                    "~/.claude/.credentials.json links to '{p}', a profile created after the import; run 'tollgate switch <imported profile>' first"
                ),
            ));
        }
    } else if fsops::lmeta(&slot).is_some_and(|m| m.is_file)
        && !slot_is_journaled_live(&j, &slot)
        && let Some(p) = slot_profile(&j, &paths)
    {
        let store = crate::claude::install_source_in(&paths.tollgate_profile(&p));
        let live = access_token(&slot);
        let same = live.is_some() && live == access_token(&store);
        let live_creds = creds(&slot);
        let owner = live_creds
            .as_ref()
            .and_then(|l| other_owner(&paths, &pairs, &p, l));
        let stored_exp = creds(&store).and_then(|c| c.access_token_expires_at());
        let older = live_creds
            .as_ref()
            .and_then(ClaudeCredentials::access_token_expires_at)
            .zip(stored_exp)
            .is_some_and(|(l, s)| s > l);
        if same {
            fix = SlotFix::Relink {
                profile: p.clone(),
                park: std::fs::read(&slot).ok() != std::fs::read(&store).ok(),
                ino: fsops::lmeta(&slot).map_or(0, |m| m.ino),
            };
        } else if let Some(owner) = owner {
            blockers.push(Finding::new(
                "live_is_other_profile",
                format!(
                    "~/.claude/.credentials.json holds profile '{owner}''s login, not '{p}''s; run 'tollgate switch {owner}' first (--adopt-live never moves one profile's login onto another)"
                ),
            ));
        } else if opts.adopt_live && older {
            blockers.push(Finding::new(
                "live_older_than_store",
                format!(
                    "~/.claude/.credentials.json holds an older login than profile '{p}''s store; --adopt-live would replace the newer chain with it"
                ),
            ));
        } else {
            if opts.adopt_live {
                fix = SlotFix::Adopt(paths.tollgate_profile(&p).join("credentials.json"));
            } else {
                blockers.push(Finding::new(
                    "live_slot_diverged",
                    format!(
                        "~/.claude/.credentials.json holds a login that differs from profile '{p}'; pass --adopt-live to roll it back as '{p}'"
                    ),
                ));
            }
        }
    }
    for e in &j.main {
        if e.status == Status::Done
            && matches!(e.op, Op::Move | Op::MoveRelink)
            && let Some(src) = &e.src
            && fsops::lmeta(src).is_some()
        {
            blockers.push(Finding::new(
                "destination_exists",
                format!(
                    "{} exists again; move it aside, then retry",
                    paths.tilde(src)
                ),
            ));
        }
    }
    let names: Vec<String> = j.profiles.iter().map(|p| p.name.clone()).collect();
    blockers.extend(super::fence::probe(&paths, &names).1);
    if !blockers.is_empty() {
        return Err(ImportBlocked {
            blockers,
            printed: false,
        }
        .into());
    }
    let steps = j
        .pre
        .iter()
        .chain(&j.main)
        .chain(&j.retire)
        .filter(|e| matches!(e.status, Status::Done | Status::Planned))
        .count();
    let pending = Pending {
        state: j.state().as_str(),
        profiles: j.profiles.len(),
        steps,
    };
    if !confirm(&pending)? {
        anyhow::bail!("tollgate import rollback: not confirmed; nothing was changed");
    }
    super::seams::after_confirm();
    // The prompt was unbounded: a session or lock holder that started while
    // it was open refuses here, before the first write (as the import's own
    // post-confirmation recheck does, I21).
    let mut recheck = procs::check(&paths, &scope).blockers;
    recheck.extend(procs::markers(&paths).0);
    recheck.extend(super::fence::probe(&paths, &names).1);
    if !recheck.is_empty() {
        return Err(ImportBlocked {
            blockers: recheck,
            printed: false,
        }
        .into());
    }
    let mut warnings = Vec::new();
    // Step 1, before the fence: the retire section. Its undo spawns
    // `claude` and herdr, which never run inside the hold (I15), and it
    // writes only files the fence does not guard.
    super::retire::undo_all(&mut j, &paths, &mut warnings)?;
    let result = (|| -> Result<()> {
        let fence = Fence::acquire(&paths, &names).map_err(|f| ImportBlocked {
            blockers: vec![f],
            printed: false,
        })?;
        if let SlotFix::Adopt(store) = &fix {
            // The store the adopted login supersedes is kept, never unlinked.
            if fsops::lmeta(store).is_some() {
                let q = txn::quarantine_path(store, ROLLBACK_SUPERSEDED);
                txn::park(store, &q)?;
                warnings.push(
                    Finding::new(
                        "quarantine_kept",
                        format!(
                            "the login --adopt-live superseded is kept in {}",
                            paths.tilde(&q)
                        ),
                    )
                    .with_path(paths.tilde(&q)),
                );
            }
            fsops::rename(&slot, store)?;
            fsops::chmod(store, 0o600)?;
        }
        if let SlotFix::Relink {
            profile: p,
            park: true,
            ..
        } = &fix
        {
            // What only the slot holds survives the relink.
            let q = txn::quarantine_path(
                &paths.tollgate_profile(p).join("credentials.json"),
                ROLLBACK_LIVE,
            );
            txn::park(&slot, &q)?;
            warnings.push(
                Finding::new(
                    "quarantine_kept",
                    format!(
                        "~/.claude/.credentials.json differed from its store; it is kept in {}",
                        paths.tilde(&q)
                    ),
                )
                .with_path(paths.tilde(&q)),
            );
        }
        reverse(&mut j, &paths, &mut warnings, false)?;
        // The slot's own fix, still inside the hold: a copy becomes a link to
        // the restored store, an adopted login links where it landed. A slot
        // the replay itself put back (a quarantined live copy) is left alone.
        if let SlotFix::Relink {
            profile: p, ino, ..
        } = &fix
            && fsops::lmeta(&slot).is_none_or(|m| m.is_file && m.ino == *ino)
            && let Some((src, _)) = pairs.iter().find(|(_, d)| d == p)
        {
            let file = crate::claude::install_source_in(&paths.upstream_profile(src));
            fsops::repoint_link(&slot, &file, &slots::temp_for(&slot))?;
        }
        if let SlotFix::Adopt(store) = &fix
            && fsops::lmeta(&slot).is_none()
            && let Some((dst, file)) = under_profiles(&root, store)
            && let Some((src, _)) = pairs.iter().find(|(_, d)| *d == dst)
        {
            let upstream = paths.upstream_profile(src).join(file);
            fsops::repoint_link(&slot, &upstream, &slots::temp_for(&slot))?;
        }
        drop(fence);
        Ok(())
    })();
    txn::after_fence(&j);
    result?;
    Ok(RolledBack { warnings })
}
