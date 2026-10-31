//! Shared engine for tollgate's carried-rank JSON reconcilers.
//!
//! Two Claude Code files need the same treatment: `.claude.json`
//! ([`crate::claude_json`]) and `settings.json` ([`crate::settings_sync`]). Both
//! exist as one logical document spread across the operator's own copy plus one
//! copy per profile runtime; both must converge on the fields that are user
//! preference while every copy keeps the fields that name its own account; and
//! both are rewritten in place by a Claude Code that can be caught mid-write.
//!
//! Only two things differ between them: the file name, and which keys are
//! per-profile. So the read/parse/skip, carried-rank winner, merge, member
//! enumeration, mtime-cache fast path, and atomic-write-only-on-change
//! machinery lives here once, and each caller supplies its base file plus one
//! [`KeyRule`] function.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::logline::logline;
use crate::profile::{atomic_write_600, atomic_write_600_with, atomic_write_with};

/// The one mtime every back-dated tollgate copy is stamped at, in seconds since
/// `SystemTime::UNIX_EPOCH`: 1980-01-02T00:00:00Z. That is below every Claude
/// Code write on a clock past it, and inside the date range FAT and exFAT
/// store — their date field starts at 1980-01-01, local time — with one day of
/// margin, so in any UTC offset the stamp converts to a local time no earlier
/// than 1980-01-01.
pub(crate) const FLOOR_STAMP_SECS: u64 = 315_619_200;

/// [`FLOOR_STAMP_SECS`] as a `SystemTime`: the back-date stamp the wire's
/// created home and every seed carry.
pub(crate) fn floor_stamp() -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(FLOOR_STAMP_SECS)
}

/// Test-only per-path write counter: incremented once in [`write_member`] for
/// every member the syncer actually rewrites, so a test can pin how many times
/// each copy was written when N wrapper processes race. Process-global, so a
/// test driving several writer threads sums them; every reader resets it while
/// holding a `HomeSandbox`, whose `HOME_TEST_LOCK` serializes the whole sandbox
/// lifetime, and never reads it unsandboxed.
#[cfg(test)]
static WRITE_COUNTS: std::sync::LazyLock<Mutex<std::collections::HashMap<PathBuf, u64>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Clear [`WRITE_COUNTS`]. Test-only.
#[cfg(test)]
pub(crate) fn reset_write_counts() {
    WRITE_COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// How many times `path` has been rewritten since the last
/// [`reset_write_counts`]. Test-only.
#[cfg(test)]
pub(crate) fn write_count(path: &Path) -> u64 {
    WRITE_COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .copied()
        .unwrap_or(0)
}

/// Test-only interleave point inside [`sync_paths`], after every member is read
/// and before the first write. A test arms it to hold every reader at that
/// instant, making the read-then-write race the N-wrapper defect is built on
/// deterministic. `None` is inert. Process-global; armed and disarmed only while
/// the arming test holds a `HomeSandbox`, so `HOME_TEST_LOCK` already serializes
/// it (the hook is taken UNDER the sandbox, never beside it).
#[cfg(test)]
static INTERLEAVE_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`INTERLEAVE_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_interleave_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *INTERLEAVE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed hook, if any. Clones the `Arc` so the hook runs OUTSIDE the
/// slot lock: the hook blocks readers, and holding the slot lock across it would
/// keep every other reader from even reaching the hook.
#[cfg(test)]
fn run_interleave_hook() {
    let hook = INTERLEAVE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Test-only interleave point inside [`sync_paths`]'s write loop, at the top of
/// each non-winner target's iteration — after the winner skip, before the
/// winner/target re-stat checks. A test arms it to park the loop between two
/// target writes, making the "Claude Code write lands mid-loop" race
/// deterministic. `None` is inert. Process-global; armed and disarmed only while
/// the arming test holds a `HomeSandbox`, so `HOME_TEST_LOCK` already serializes
/// it (the hook is taken UNDER the sandbox, never beside it).
#[cfg(test)]
static WRITE_LOOP_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`WRITE_LOOP_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_write_loop_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *WRITE_LOOP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`WRITE_LOOP_HOOK`], if any. Same clone-outside-the-slot
/// discipline as [`run_interleave_hook`]: the hook parks the writer, and the slot
/// lock must not be held across that park.
#[cfg(test)]
fn run_write_loop_hook() {
    let hook = WRITE_LOOP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Test-only interleave point inside [`sync_paths`]'s write loop, after the
/// winner/target re-stat checks and the merge+serialize, immediately before the
/// [`write_member`] rename. A test arms it to park a Claude Code write on the
/// winner inside the span the per-iteration re-stat cannot see — making the
/// "write lands between the re-stat and the rename" race deterministic. `None`
/// is inert; armed and disarmed only while the arming test holds a
/// `HomeSandbox`, so `HOME_TEST_LOCK` already serializes it.
#[cfg(test)]
static PRE_RENAME_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`PRE_RENAME_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_pre_rename_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *PRE_RENAME_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`PRE_RENAME_HOOK`], if any. Same clone-outside-the-slot
/// discipline as [`run_interleave_hook`]: the hook parks the writer, and the
/// slot lock must not be held across that park.
#[cfg(test)]
fn run_pre_rename_hook() {
    let hook = PRE_RENAME_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Test-only interleave point at the end of [`write_member`], after the rename
/// swapped the inode and before the call returns. A test arms it to land a
/// Claude Code write on the just-renamed destination, making the "a write
/// lands right after tollgate's rename" race deterministic. `None` is inert;
/// armed and disarmed only while the arming test holds a `HomeSandbox`, so
/// `HOME_TEST_LOCK` already serializes it.
#[cfg(test)]
static POST_WRITE_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`POST_WRITE_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_post_write_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *POST_WRITE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`POST_WRITE_HOOK`], if any. Same clone-outside-the-slot
/// discipline as [`run_interleave_hook`]: the hook runs OUTSIDE the slot lock.
#[cfg(test)]
fn run_post_write_hook() {
    let hook = POST_WRITE_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Test-only interleave point inside [`write_member`]'s pre-rename callback,
/// after the temp file is written and before the winner re-stat that decides
/// the back-date. A test arms it to park a Claude Code write on the winner in
/// the span the back-date decision must still see.
/// `None` is inert; armed and disarmed only while the arming test holds a
/// `HomeSandbox`, so `HOME_TEST_LOCK` already serializes it.
#[cfg(test)]
static POST_TEMP_HOOK: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>> = Mutex::new(None);

/// Arm or clear the [`POST_TEMP_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_post_temp_hook(hook: Option<Box<dyn Fn() + Send + Sync>>) {
    *POST_TEMP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook.map(std::sync::Arc::from);
}

/// Invoke the armed [`POST_TEMP_HOOK`], if any. Same clone-outside-the-slot
/// discipline as [`run_interleave_hook`]: the hook parks the writer, and the
/// slot lock must not be held across that park.
#[cfg(test)]
fn run_post_temp_hook() {
    let hook = POST_TEMP_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// Test-only seam folded into [`write_copy_and_record`]'s back-date stamp
/// result, forcing the stamped write to fail the way a filesystem this stamp
/// cannot land on would. `false` is inert; armed only while the arming test
/// holds the home test lock (`HomeSandbox` or `with_fake_home`), so
/// `HOME_TEST_LOCK` already serializes it. A mutant that drops `set_modified`'s
/// own result before the fold survives: no portable fault makes that syscall
/// itself fail here.
#[cfg(test)]
static STAMP_ERROR_HOOK: Mutex<bool> = Mutex::new(false);

/// Arm or clear the [`STAMP_ERROR_HOOK`]. Test-only.
#[cfg(test)]
pub(crate) fn set_stamp_error_hook(armed: bool) {
    *STAMP_ERROR_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = armed;
}

/// Fail the stamp when the [`STAMP_ERROR_HOOK`] is armed, else `Ok(())`.
/// Test-only.
#[cfg(test)]
fn run_stamp_error_hook() -> std::io::Result<()> {
    if *STAMP_ERROR_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        Err(std::io::Error::other("test-forced stamp failure"))
    } else {
        Ok(())
    }
}

/// Where a key sits in a synced document. The walk is one level deep, so
/// [`KeyPath::Nested`] is only ever asked about keys of a top-level object whose
/// own rule was [`KeyRule::Nested`]. It names that parent first, so two nested
/// objects (`env`, `enabledPlugins`) never share one rule by accident.
#[derive(Debug, Clone, Copy)]
pub(crate) enum KeyPath<'a> {
    Top(&'a str),
    /// `(parent, key)`: `key` inside the top-level object `parent`.
    Nested(&'a str, &'a str),
}

/// What the syncer does with one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyRule {
    /// The winner's value replaces every other member's, and a key the winner
    /// dropped is removed everywhere.
    Shared,
    /// Every member keeps its own value; the winner's never propagates.
    PerProfile,
    /// Object-valued: descend and rule its keys one by one. Meaningless on a
    /// [`KeyPath::Nested`] key and treated as [`KeyRule::Shared`] there.
    Nested,
}

/// One member's stat for [`run_with_cache`]'s fast path: its mtime and size.
/// A changed member SET, or any member's size or mtime change in either
/// direction, reads as a change — so a member appearing at the floor stamp (the
/// wire's created home) wakes the pass a newest-mtime-only key would skip
/// forever.
#[derive(PartialEq)]
pub(crate) struct MemberSnapshot {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
}

/// One [`MemberSnapshot`] per stattable member, sorted by path so a listing
/// whose order changed with the same members and stats reads as the same
/// snapshot. Stat-only — the fast path callers use to skip a tick with no
/// reads, parses, or writes. A member that cannot be statted (missing,
/// unreadable) is absent, so it reappearing reads as a set change.
fn stat_snapshot(paths: &[PathBuf]) -> Vec<MemberSnapshot> {
    let mut snapshot: Vec<MemberSnapshot> = paths
        .iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            Some(MemberSnapshot {
                path: p.clone(),
                mtime: meta.modified().ok()?,
                size: meta.len(),
            })
        })
        .collect();
    snapshot.sort_by(|a, b| a.path.cmp(&b.path));
    snapshot
}

/// Every file tollgate reconciles for one synced document: the operator's own
/// copy under `base` plus each SHARED per-session runtime copy of `file`.
/// Enumerated through [`crate::runtime::shared_runtime_dirs`] rather than a
/// fixed `<profile>/runtime` path — every session owns its own `runtime-<sid>`,
/// so a fixed path would reconcile nothing while every session ran. Isolated
/// copies stay out: an isolated runtime is built from an empty base, so it must
/// neither win (its empty copy would delete the operator's own fields) nor
/// receive (that would defeat the isolation it exists for).
pub(crate) fn runtime_files_under(base: &Path, file: &str) -> Vec<PathBuf> {
    let mut paths = vec![base.join(file)];
    paths.extend(
        crate::runtime::shared_runtime_dirs()
            .into_iter()
            .map(|dir| dir.join(file)),
    );
    paths
}

/// Stat-only fast path around `body`: skip the tick when the stat snapshot of
/// `paths` — one `(mtime, size)` per stattable member — is unchanged from what
/// `cache` records from the last sync that did work (no reads, parses, or
/// writes), and advance `cache` only when `body` reports it ran — so a paused
/// (`Ok(false)`) or failed tick retries next time. A changed member set or any
/// member's size or mtime change is a change, so a member appearing at the
/// floor stamp still wakes the pass.
pub(crate) fn run_with_cache(
    cache: &Mutex<Option<Vec<MemberSnapshot>>>,
    paths: &[PathBuf],
    body: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    let snapshot = stat_snapshot(paths);
    if snapshot.is_empty() {
        return Ok(());
    }
    {
        let last = cache.lock().unwrap_or_else(|p| p.into_inner());
        if last.as_ref() == Some(&snapshot) {
            return Ok(());
        }
    }
    if !body()? {
        return Ok(());
    }
    // The snapshot was stated before the body, so it can only lag what the body
    // actually saw — costing at most one redundant tick, never a skipped one.
    *cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(snapshot);
    Ok(())
}

struct Member {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
    obj: Map<String, Value>,
    /// Hex SHA-256 of the bytes this pass read: the echo check's digest leg. A
    /// Claude Code write that keeps the size (one digit changed) and lands on
    /// tollgate's own mtime still reads as a different digest, so it is a real
    /// write, not tollgate's echo.
    digest: String,
}

/// Whether a file's on-disk mtime or size moved since its snapshot was read.
/// A changed file means a concurrent in-place write (Claude Code's own) landed
/// inside the read→write window: propagating a stale merge over it would revert
/// that write on the next pass. A vanished or unstatable file counts as changed
/// — never clobber what cannot be re-read. Shared with [`crate::claude_json`]'s
/// strip, which re-stats home before its own write under the same discipline.
pub(crate) fn member_changed_since_read(path: &Path, mtime: SystemTime, size: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    if meta.len() != size {
        return true;
    }
    let Ok(now_mtime) = meta.modified() else {
        return true;
    };
    now_mtime != mtime
}

/// Read and parse each file (skipping missing ones and partial writes), pick as
/// winner the member whose bytes carry the newest Claude Code write (a non-echo
/// ranks by its own mtime, an echo by the carried rank its record entry stores),
/// and rewrite every other as `winner.shared ∪ target.per_profile` — atomically,
/// only on change. Idempotent after convergence. A concurrent Claude Code write
/// that moves the winner aborts the pass and one that moves a target skips it;
/// the next event or tick re-reads and converges.
///
/// `operator_file` is the one member Claude Code owns (the copy under the
/// operator's home rather than under `~/.tollgate`); see [`write_member`] for why
/// it is written differently.
///
/// In guest mode ([`crate::identity::upstream_active`]) a reconciler that names
/// an `operator_file` does nothing at all (plan §4.0: "guest runtime copies are
/// never sync members of the operator base"). Upstream clauth owns that file
/// until an import, so it is not written; and it is not READ as a winner
/// either, because upstream's active account puts its own `[env]` keys there,
/// which tollgate's per-profile key set cannot know and would push into every
/// runtime copy as shared. The runtime copies do not reconcile among
/// themselves instead: with no base to write back into, the next start seeds a
/// fresh copy from the operator's file, and as the newest member it would
/// revert every live sibling's own edits. So each guest runtime copy is seeded
/// once, one-way, at start (`runtime::write_merged_settings`,
/// `runtime::seed_claude_json`) and is the session's own from then on.
pub(crate) fn sync_paths(
    paths: &[PathBuf],
    operator_file: Option<&Path>,
    record: Option<&Path>,
    rule: impl Fn(KeyPath<'_>) -> KeyRule,
) -> Result<()> {
    if operator_file.is_some() && crate::identity::upstream_active() {
        return Ok(());
    }
    let mut members: Vec<Member> = Vec::new();
    for path in paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // Partial CC in-place write fails to parse — skip until next tick.
        let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        members.push(Member {
            path: path.clone(),
            mtime,
            size: meta.len(),
            obj,
            digest: hex::encode(Sha256::digest(&bytes)),
        });
    }
    if members.len() < 2 {
        return Ok(());
    }

    // Test-only interleave: after every member is read, before the first write.
    #[cfg(test)]
    run_interleave_hook();

    // The written-copy record, read once before the winner selection (fail-open
    // to empty): a member matching tollgate's own unchanged echo ranks by the
    // carried rank its entry stores, not by its own mtime.
    let written = record.map(read_record).unwrap_or_default();

    // Every member ranks by the mtime of the Claude Code write its bytes carry:
    // a non-echo (path + mtime + size + digest does not match the record) ranks
    // by its own mtime; an echo ranks by the carried rank its entry stores. The
    // winner is the highest rank; a rank tie goes to the non-echo, then path. An
    // empty record (missing, unreadable or corrupt, an old-format file included)
    // marks nothing an echo, so every member ranks by its own mtime and the
    // plain newest-mtime order decides (fail-open). `is_echo` reversed so
    // `false` (not an echo) sorts greater than `true`. Non-empty invariant: the
    // `members.len() < 2` guard above.
    let is_echo = |m: &Member| written.iter().any(|e| e.matches(m));
    let rank = |m: &Member| -> (i64, u32) {
        written
            .iter()
            .find(|e| e.matches(m))
            .map(|e| (e.rank_secs, e.rank_nanos))
            .or_else(|| mtime_parts(m.mtime))
            .unwrap_or((i64::MIN, 0))
    };
    #[allow(clippy::expect_used, reason = "non-empty invariant guaranteed above")]
    let winner = members
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| is_echo(b).cmp(&is_echo(a)))
                .then_with(|| a.path.cmp(&b.path))
        })
        .map(|(i, _)| i)
        .expect("members is non-empty");
    let winner_rank = rank(&members[winner]);

    // Copies written this pass, recorded as echoes. Persisted once
    // after the loop — on every exit path that wrote something — not per copy.
    let mut recorded: Vec<WrittenEntry> = Vec::new();
    let write_result: Result<()> = (|| {
        for i in 0..members.len() {
            if i == winner {
                continue;
            }
            // Test-only interleave: between target writes, so a test can land a
            // write on the winner mid-loop deterministically.
            #[cfg(test)]
            run_write_loop_hook();
            // The winner is the merge source for every write below. Re-stat it
            // before EACH target write, not just once before the loop: a Claude
            // Code write to the winner landing mid-loop (after an earlier target's
            // rename, before this one's) would otherwise leave this target holding
            // stale bytes with a newer mtime, and the next pass would pick it as
            // winner and revert the winner's write. A moved winner aborts the rest
            // of this pass; the next event/tick re-reads and converges.
            if member_changed_since_read(
                &members[winner].path,
                members[winner].mtime,
                members[winner].size,
            ) {
                return Ok(());
            }
            // A member CC rewrote between our read and now keeps its newer bytes:
            // skip it this pass; the next pass re-reads and re-merges it.
            if member_changed_since_read(&members[i].path, members[i].mtime, members[i].size) {
                continue;
            }
            let merged = merge_member(&members[winner].obj, &members[i].obj, &rule);
            if merged == members[i].obj {
                continue;
            }
            let path = &members[i].path;
            let bytes = serde_json::to_vec_pretty(&Value::Object(merged))
                .with_context(|| format!("failed to serialize merged {}", path.display()))?;
            // Test-only interleave: after the re-stat and serialize, before the
            // temp write — a write on the winner landing here is what the winner
            // re-stat inside `write_member` exists to catch.
            #[cfg(test)]
            run_pre_rename_hook();
            // Write the target. The temp is written first with a fresh mtime, then
            // the winner is re-statted on the still-open temp handle (inside
            // `write_member`) and the temp is back-dated to the winner's read mtime
            // only when the winner moved since this pass read it. The early re-stat
            // above aborted a move that landed between TARGETS; the in-writer
            // re-stat sees a move that landed between the early re-stat and the
            // rename (the hook window) and — running after the temp write — also a
            // move that landed between the decision and the temp write. In the
            // common case the winner held still, so the copy keeps the rename's
            // fresh mtime and a running Claude Code session whose cache cursor sits
            // after the winner's write still reloads it; the back-date stamps
            // tollgate's own temp before the rename, so a Claude Code write landing on
            // this path right after the rename keeps its own mtime.
            if let Some(entry) =
                write_member(path, &bytes, operator_file, &members[winner], winner_rank)
                    .with_context(|| format!("failed to write {}", path.display()))?
            {
                recorded.push(entry);
            }
        }
        Ok(())
    })();

    if let Some(record) = record
        && !recorded.is_empty()
    {
        persist_record(record, paths, &recorded);
    }
    write_result
}

/// Overlay the winner's shared state onto one target: start from the target so
/// its key order and per-profile fields survive, drop the shared keys the winner
/// no longer carries, then upsert the winner's.
fn merge_member(
    winner: &Map<String, Value>,
    target: &Map<String, Value>,
    rule: &impl Fn(KeyPath<'_>) -> KeyRule,
) -> Map<String, Value> {
    let mut merged = target.clone();
    // Per-profile keys are the target's own; nested ones survive the top-level
    // pass and are filtered key-by-key below.
    merged.retain(|k, _| rule(KeyPath::Top(k)) != KeyRule::Shared || winner.contains_key(k));
    for (k, v) in winner {
        if rule(KeyPath::Top(k)) == KeyRule::Shared {
            merged.insert(k.clone(), v.clone());
        }
    }

    // Nested objects, over the union of both sides' keys so the winner can add
    // and drop shared entries without disturbing the target's own.
    let mut nested_keys: Vec<String> = Vec::new();
    for k in merged.keys().chain(winner.keys()) {
        if rule(KeyPath::Top(k)) == KeyRule::Nested && !nested_keys.iter().any(|n| n == k) {
            nested_keys.push(k.clone());
        }
    }
    for key in nested_keys {
        let winner_value = winner.get(&key);
        // A winner whose value is not an object cannot be merged key-by-key, and
        // copying it wholesale would erase every per-profile key the target
        // holds under it — the exact thing this rule exists to protect. Leave
        // the target alone; the writers that produce these files reject the
        // shape outright (`build_claude_settings_json`), so it is a hand-edit or
        // a truncation that still parsed, and the next tick self-heals.
        if winner_value.is_some_and(|v| !v.is_object()) {
            continue;
        }
        let winner_obj = winner_value.and_then(Value::as_object);
        let target_obj = merged.get(&key).and_then(Value::as_object).cloned();
        if winner_obj.is_none() && target_obj.is_none() {
            continue; // neither side holds an object here — leave the target as is
        }
        let mut nested = target_obj.unwrap_or_default();
        nested.retain(|nk, _| {
            rule(KeyPath::Nested(&key, nk)) == KeyRule::PerProfile
                || winner_obj.is_some_and(|w| w.contains_key(nk))
        });
        if let Some(winner_obj) = winner_obj {
            for (nk, nv) in winner_obj {
                if rule(KeyPath::Nested(&key, nk)) != KeyRule::PerProfile {
                    nested.insert(nk.clone(), nv.clone());
                }
            }
        }
        merged.insert(key, Value::Object(nested));
    }
    merged
}

/// Write one synced member. The rename swaps the inode, so the mode is the
/// writer's, not the file's: a tollgate-owned copy (under `~/.tollgate`) gets 0o600
/// so the syncer can't silently revert the seed's owner-only mode, while
/// `operator_file` — Claude Code's own copy under the operator's home — lands at
/// CC's own 0o644 posture, matching what `claude::apply_profile_to_claude_settings`
/// already does to that file. Note this is not preservation: the writer renames
/// a fresh umask-moded temp over the path, so a hand-tightened operator file is
/// widened. Deliberate — tollgate does not own that file and does not restyle it
/// either way. Any path that is not `operator_file` is treated as tollgate-owned,
/// the stricter default.
///
/// `winner` is the merge source. The temp is written first with a fresh mtime,
/// then the winner is re-statted on the still-open temp handle and the temp is
/// back-dated to the winner's read mtime only when the winner moved since this
/// pass read it. The stamp lands on tollgate's own temp before the
/// rename, so a Claude Code write landing on the destination right after the
/// rename keeps its own mtime. `carried_rank` is the rank the copy records — the
/// mtime of the Claude Code write its bytes carry (the winner's rank). Returns
/// the written-copy record entry, `None` when the copy's mtime is unrecordable
/// (fail-open: the copy then never reads as an echo).
fn write_member(
    path: &Path,
    bytes: &[u8],
    operator_file: Option<&Path>,
    winner: &Member,
    carried_rank: (i64, u32),
) -> std::io::Result<Option<WrittenEntry>> {
    #[cfg(test)]
    {
        *WRITE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(path.to_path_buf())
            .or_insert(0) += 1;
    }
    let owner_only = operator_file != Some(path);
    let mut recorded: Option<WrittenEntry> = None;
    let result = write_staged(path, bytes, owner_only, |f| {
        // Test-only interleave: after the temp write, before the winner re-stat.
        #[cfg(test)]
        run_post_temp_hook();
        if member_changed_since_read(&winner.path, winner.mtime, winner.size) {
            f.set_modified(winner.mtime)?;
        }
        recorded = record_written(path, f, bytes, carried_rank);
        Ok(())
    });
    // Test-only interleave: after the rename, before returning — the span a
    // Claude Code write landing on the fresh inode occupies.
    #[cfg(test)]
    run_post_write_hook();
    result.map(|_| recorded)
}

/// Stage `bytes` in `path`'s temp, run `before_rename` on the still-open temp
/// handle, then rename: through the 0o600 writer when `owner_only`, else the
/// plain one. Either writer removes the temp when any step fails.
fn write_staged(
    path: &Path,
    bytes: &[u8],
    owner_only: bool,
    before_rename: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let write = |f: &mut std::fs::File| {
        std::io::Write::write_all(f, bytes)?;
        before_rename(f)
    };
    if owner_only {
        atomic_write_600_with(path, write)
    } else {
        atomic_write_with(path, write)
    }
}

// ── written-copy record (the echo) ───────────────────────────────────────────
//
// Under the sync lock tollgate records each copy it wrote — path, the TEMP
// handle's post-write mtime and size, the SHA-256 of the bytes it wrote, and
// the carried rank — in a small owner-only `~/.tollgate` file. A member matching
// its own record is tollgate's unchanged echo and ranks by the carried rank its
// entry stores (the rank of the Claude Code write its bytes were built from); a
// member no entry matches ranks by its own mtime. The record fails open: a
// missing, unreadable or unparseable record reads as empty (then nothing is an
// echo and every member ranks by its own mtime), and a record write failure is
// logged once and never fails the merge.

/// Latches for [`record_failure`]: one per record path, so the line fires once
/// per process for each record that keeps failing. A record's latch clears only
/// when its own write succeeds again — a clean write of one record never
/// re-arms another's.
static RECORD_WRITE_FAILURES: std::sync::LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// One copy tollgate wrote, as recorded in the written-copy record. Holds the
/// copy's path, its mtime (secs + nanos since the epoch), size, the SHA-256
/// digest of the bytes tollgate wrote, and the carried rank (secs + nanos) — the
/// mtime of the Claude Code write those bytes carry — never the member bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WrittenEntry {
    path: String,
    mtime_secs: i64,
    mtime_nanos: u32,
    size: u64,
    digest: String,
    rank_secs: i64,
    rank_nanos: u32,
}

impl WrittenEntry {
    /// Whether `member` is this entry's echo: the copy tollgate wrote still sits
    /// byte-for-byte on disk. Every leg must match — path, mtime, size AND the
    /// digest of the bytes this pass read — so a Claude Code write that keeps a
    /// copy's size (one digit changed) and lands on tollgate's own mtime is a
    /// real write, not an echo.
    fn matches(&self, member: &Member) -> bool {
        self.matches_parts(&member.path, member.mtime, member.size, &member.digest)
    }

    /// Whether a member read as `(path, mtime, size, digest)` is this entry's
    /// echo — the same legs as [`WrittenEntry::matches`], on the strip's raw
    /// read of home rather than a [`Member`].
    fn matches_parts(&self, path: &Path, mtime: SystemTime, size: u64, digest: &str) -> bool {
        self.size == size
            && self.digest.as_str() == digest
            && Path::new(&self.path) == path
            && mtime_parts(mtime) == Some((self.mtime_secs, self.mtime_nanos))
    }
}

/// `SystemTime` as `(secs, nanos)` since the epoch, or `None` for a pre-epoch
/// time (unrecordable — such a copy then reads as a Claude Code write and ranks
/// by its own mtime).
fn mtime_parts(t: SystemTime) -> Option<(i64, u32)> {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some((d.as_secs() as i64, d.subsec_nanos()))
}

/// The temp handle's post-write mtime and size plus the SHA-256 of `bytes` and
/// the carried rank, for the record. Pins the temp's final mtime first so the
/// recorded one cannot drift when the handle closes; that pin is best-effort and
/// a failed pin may leave the recorded mtime unmatched after a restamp at close
/// (the copy then reads as a Claude Code write and ranks by its own mtime),
/// never fails the write. Stats the still-open TEMP handle — never the
/// destination after the rename — so a Claude Code write landing right after the
/// rename is not recorded as tollgate's own. `None` when the temp's metadata
/// cannot be read or its mtime predates the epoch. Never an error: recording
/// must not fail the copy write.
fn record_written(
    path: &Path,
    f: &std::fs::File,
    bytes: &[u8],
    carried_rank: (i64, u32),
) -> Option<WrittenEntry> {
    if let Ok(final_mtime) = f.metadata().and_then(|m| m.modified()) {
        let _ = f.set_modified(final_mtime);
    }
    let meta = f.metadata().ok()?;
    let (mtime_secs, mtime_nanos) = mtime_parts(meta.modified().ok()?)?;
    Some(WrittenEntry {
        path: path.to_string_lossy().into_owned(),
        mtime_secs,
        mtime_nanos,
        size: meta.len(),
        digest: hex::encode(Sha256::digest(bytes)),
        rank_secs: carried_rank.0,
        rank_nanos: carried_rank.1,
    })
}

/// Read the record at `path` as a list of entries; a missing, unreadable or
/// unparseable record reads as empty (fail-open).
fn read_record(path: &Path) -> Vec<WrittenEntry> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<WrittenEntry>>(&bytes).unwrap_or_default()
}

/// Persist the record for one sync pass: `written` (copies rewritten this pass)
/// replace their old entries, entries for other members of `paths` are kept, and
/// entries for paths no longer in `paths` are dropped (bounds the file).
fn persist_record(record_path: &Path, paths: &[PathBuf], written: &[WrittenEntry]) {
    let mut entries: Vec<WrittenEntry> = read_record(record_path)
        .into_iter()
        .filter(|e| {
            !written
                .iter()
                .any(|w| Path::new(&w.path) == Path::new(&e.path))
                && paths.iter().any(|p| p.as_path() == Path::new(&e.path))
        })
        .collect();
    entries.extend(written.iter().cloned());
    write_record_file(record_path, &entries);
}

/// Merge one written entry into `record_path` (replace its path's old entry,
/// keep every other), then persist best-effort. The strip's single write has no
/// member list to bound the record, so nothing else is dropped.
fn merge_record(record_path: &Path, entry: &WrittenEntry) {
    let mut entries: Vec<WrittenEntry> = read_record(record_path)
        .into_iter()
        .filter(|e| Path::new(&e.path) != Path::new(&entry.path))
        .collect();
    entries.push(entry.clone());
    write_record_file(record_path, &entries);
}

/// The carried rank for a write of `path` whose bytes this pass read as
/// `(mtime, size, bytes)`: the record entry's carried rank when those bytes are
/// tollgate's own unchanged echo, else the read mtime — a real Claude Code write
/// carries its own mtime. The strip's home rewrite records this, so an older
/// runtime copy never outranks home's own newer Claude Code write the strip
/// copied forward. A pre-epoch read mtime cannot be represented and ranks below
/// everything.
pub(crate) fn carried_rank_of(
    record_path: &Path,
    path: &Path,
    mtime: SystemTime,
    size: u64,
    bytes: &[u8],
) -> (i64, u32) {
    let digest = hex::encode(Sha256::digest(bytes));
    read_record(record_path)
        .iter()
        .find(|e| e.matches_parts(path, mtime, size, &digest))
        .map(|e| (e.rank_secs, e.rank_nanos))
        .or_else(|| mtime_parts(mtime))
        .unwrap_or((i64::MIN, 0))
}

/// Write `path` and record the write into `record_path` when it is `Some`,
/// carrying `carried_rank` as the rank of the Claude Code write the rewritten
/// bytes carry. `owner_only` selects the write mode: the strip's home rewrite
/// and the wire's home edit land at the operator's 0o644 posture (`false`), the
/// runtime seed's copy and a wire-edited runtime copy at 0o600 (`true`).
/// `backdate` stamps the temp handle's mtime before the rename — the wire's
/// created home and a seed, recorded or unrecorded, to the floor stamp
/// ([`FLOOR_STAMP_SECS`], below every Claude Code write) — so a write whose
/// record cannot hold its rank ranks below every Claude Code write. A stamp the
/// filesystem refuses fails the write: the caller's error path runs, never a
/// clamp. The caller re-stats the member immediately before this call (the
/// re-stat-to-rename gap). The entry comes from the temp handle before the rename, like
/// [`write_member`]; the record persist never fails the write. Fires the
/// test-only [`run_post_write_hook`] after the rename — the same seam
/// [`write_member`] fires — so a Claude Code write landing right after the
/// rename is observable, and the seam tests land a concurrent write through.
pub(crate) fn write_copy_and_record(
    record_path: Option<&Path>,
    path: &Path,
    bytes: &[u8],
    carried_rank: (i64, u32),
    owner_only: bool,
    backdate: Option<SystemTime>,
) -> std::io::Result<()> {
    let mut recorded: Option<WrittenEntry> = None;
    write_staged(path, bytes, owner_only, |f| {
        if let Some(at) = backdate {
            // The stamp and the test seam share one `Result`, so an edit that
            // drops it drops both.
            let stamped = f.set_modified(at);
            #[cfg(test)]
            let stamped = stamped.and_then(|()| run_stamp_error_hook());
            stamped?;
        }
        if record_path.is_some() {
            recorded = record_written(path, f, bytes, carried_rank);
        }
        Ok(())
    })?;
    // Test-only interleave: after the rename, before returning — the span a
    // Claude Code write landing on the fresh inode occupies.
    #[cfg(test)]
    run_post_write_hook();
    if let (Some(record_path), Some(entry)) = (record_path, recorded) {
        merge_record(record_path, &entry);
    }
    Ok(())
}

/// Serialize and write the record at `record_path` owner-only (0o600 under a
/// 0o700 dir via [`atomic_write_600`]). Best-effort: a serialize or write
/// failure is logged once and never fails the merge.
fn write_record_file(record_path: &Path, entries: &[WrittenEntry]) {
    let Ok(json) = serde_json::to_vec_pretty(entries) else {
        return record_failure(record_path);
    };
    if atomic_write_600(record_path, &json).is_err() {
        record_failure(record_path);
    } else {
        RECORD_WRITE_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(record_path);
    }
}

/// Log the record persist failure once per process per record path. A record
/// keeps its own latch, so one reconciler's clean write cannot re-arm the
/// other's failing one.
fn record_failure(record_path: &Path) {
    let first = RECORD_WRITE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(record_path.to_path_buf());
    if first {
        logline!(
            "tollgate: failed to write the sync written-copy record {}; the copies \
             it wrote read as non-echoes until it writes cleanly",
            record_path.display()
        );
    }
}
