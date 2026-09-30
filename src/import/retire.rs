//! `tollgate import retire` (spec §4.11): the checklist that runs once an
//! import is `complete`, each step journaled in the journal's `retire`
//! section and reversed first by a rollback.
//!
//! - **R1** upstream's Claude Code wiring out of the shared files:
//!   `enabledPlugins["clauth@clauth"]` and `extraKnownMarketplaces.clauth` in
//!   `settings.json`, `clauth` in `plugins/known_marketplaces.json`,
//!   `plugins["clauth@clauth"]` in `installed_plugins.json`, and
//!   `mcpServers.clauth` in `~/.claude.json`. Each removal records the value
//!   it took (JSON, never a secret: a value that looks like one is left in
//!   place and named).
//! - **R2** tollgate's plugin installed (`tollgate@tollgate`); undo removes
//!   only the plugin.
//! - **R3** tollgate's herdr plugin installed; `--yes` means `--no-config`
//!   (no key to bind). Undo uninstalls it.
//! - **R4** the `.bashrc` completion line: upstream's `source` line replaced
//!   by tollgate's (the one line, and its `# clauth completions` comment).
//!
//! Never: delete `clauth-0.16.0.retired`, run `cargo uninstall`, or remove
//! `~/.clauth`. Those stay the owner's last steps, printed at the end.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::edits::{self, JsonDoc};
use super::journal::{Entry, Facts, Journal, Op, Status};
use super::{Finding, ImportBlocked, Paths, fsops, procs, seams, txn};
use crate::identity::ImportState;

/// One retire step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    R1,
    R2,
    R3,
    R4,
}

impl Step {
    pub(crate) const ALL: [Step; 4] = [Step::R1, Step::R2, Step::R3, Step::R4];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Step::R1 => "r1",
            Step::R2 => "r2",
            Step::R3 => "r3",
            Step::R4 => "r4",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Step::ALL
            .into_iter()
            .find(|st| st.as_str().eq_ignore_ascii_case(s))
    }

    fn describe(self) -> &'static str {
        match self {
            Step::R1 => "remove upstream's clauth@clauth plugin, marketplace and mcpServers.clauth",
            Step::R2 => "install tollgate@tollgate into ~/.claude",
            Step::R3 => "install tollgate's herdr plugin",
            Step::R4 => "replace upstream's completion line in ~/.bashrc with tollgate's",
        }
    }
}

/// What `import retire` did.
#[derive(Debug, Clone, Default)]
pub(crate) struct Retired {
    /// One line per step: done, skipped (and why) or already done.
    pub(crate) lines: Vec<String>,
    pub(crate) warnings: Vec<Finding>,
}

/// The owner's last steps, never taken by tollgate (spec §4.11).
pub(crate) fn last_steps(paths: &Paths, j: &Journal) -> Vec<String> {
    let mut out = Vec::new();
    for e in j.main.iter().filter(|e| e.op == Op::RetireBin) {
        if let Some(retired) = &e.after.retired {
            out.push(format!(
                "when you are sure: delete {} (the retired upstream binary)",
                paths.tilde(retired)
            ));
        }
    }
    out.push(format!(
        "when you are sure: run 'cargo uninstall {}' and remove ~/.clauth yourself; tollgate never does",
        crate::identity::UPSTREAM_NAME
    ));
    out
}

fn step_of(e: &Entry) -> Option<Step> {
    e.after.step.as_deref().and_then(Step::parse)
}

/// Whether `step` already ran (done, or settled as nothing to do).
fn settled(j: &Journal, step: Step) -> bool {
    j.retire
        .iter()
        .any(|e| step_of(e) == Some(step) && matches!(e.status, Status::Done | Status::Skipped))
}

fn next_seq(j: &Journal) -> u64 {
    j.pre
        .iter()
        .chain(&j.main)
        .chain(&j.retire)
        .map(|e| e.seq)
        .max()
        .unwrap_or(0)
        + 1
}

fn entry(
    j: &Journal,
    op: Op,
    dst: Option<PathBuf>,
    prior: Facts,
    mut after: Facts,
    step: Step,
) -> Entry {
    after.step = Some(step.as_str().to_string());
    Entry {
        seq: next_seq(j),
        op,
        src: None,
        dst,
        secret: false,
        prior,
        after,
        status: Status::Planned,
    }
}

/// A settled step with nothing to do: journaled `skipped`, so a later
/// `retire` does not ask again.
fn skip(j: &mut Journal, paths: &Paths, op: Op, step: Step, why: &str) -> Result<String> {
    let mut e = entry(j, op, None, Facts::default(), Facts::default(), step);
    e.status = Status::Skipped;
    e.after.key = Some(why.to_string());
    j.retire.push(e);
    j.write(paths)?;
    Ok(format!("{}: nothing to do ({why})", step.as_str()))
}

/// Append `e` planned, run `op`, and settle it `done` (write-ahead, like
/// `main`). A failed op takes its planned entry back out: nothing landed
/// that an undo would have to reverse.
fn journaled(
    j: &mut Journal,
    paths: &Paths,
    e: Entry,
    op: impl FnOnce(&mut Entry) -> Result<()>,
) -> Result<()> {
    j.retire.push(e);
    j.write(paths)?;
    let i = j.retire.len() - 1;
    let mut e = j.retire[i].clone();
    match op(&mut e) {
        Ok(()) => {
            e.status = Status::Done;
            j.retire[i] = e;
            j.write(paths)
        }
        Err(err) => {
            j.retire.pop();
            j.write(paths)?;
            Err(err)
        }
    }
}

/// R1's removals: `(file, pointer)`.
fn r1_targets(paths: &Paths) -> Vec<(PathBuf, String)> {
    let plugin = crate::identity::UPSTREAM_CC_PLUGIN;
    let market = crate::identity::UPSTREAM_NAME;
    let settings = paths.claude.join("settings.json");
    let plugins = paths.claude.join("plugins");
    vec![
        (
            settings.clone(),
            edits::pointer(&["enabledPlugins", plugin]),
        ),
        (
            settings,
            edits::pointer(&["extraKnownMarketplaces", market]),
        ),
        (
            plugins.join("known_marketplaces.json"),
            edits::pointer(&[market]),
        ),
        (
            plugins.join("installed_plugins.json"),
            edits::pointer(&["plugins", plugin]),
        ),
        (
            paths.home.join(".claude.json"),
            edits::pointer(&["mcpServers", market]),
        ),
    ]
}

fn r1(j: &mut Journal, paths: &Paths, out: &mut Retired) -> Result<()> {
    let mut removed = 0;
    for (file, ptr) in r1_targets(paths) {
        if fsops::lmeta(&file).is_none() {
            continue;
        }
        let doc: JsonDoc = edits::read_doc(&file)?;
        let Some(value) = edits::lookup(&doc.map, &ptr) else {
            continue;
        };
        if edits::looks_secret(value) {
            out.warnings.push(
                Finding::new(
                    "retire_secret_bearing",
                    format!(
                        "{} {ptr} is left in place: it carries an env, header or key block tollgate does not journal; remove it by hand",
                        paths.tilde(&file)
                    ),
                )
                .with_path(paths.tilde(&file)),
            );
            continue;
        }
        let (prior, after) = edits::rewrite_facts(&doc, &ptr, None, Step::R1.as_str())?;
        let e = entry(j, Op::RewriteJson, Some(file), prior, after, Step::R1);
        journaled(j, paths, e, |e| edits::apply_json(e))?;
        removed += 1;
    }
    if removed == 0 {
        out.lines.push(skip(
            j,
            paths,
            Op::RewriteJson,
            Step::R1,
            "no clauth wiring left",
        )?);
    } else {
        out.lines.push(format!(
            "r1: removed {removed} of upstream's clauth entries"
        ));
    }
    Ok(())
}

fn r2(j: &mut Journal, paths: &Paths, out: &mut Retired) -> Result<()> {
    let e = entry(
        j,
        Op::PluginInstall,
        None,
        Facts::default(),
        Facts::default(),
        Step::R2,
    );
    let mut outcome = String::new();
    journaled(j, paths, e, |e| {
        let o = crate::plugin_host::install()?;
        outcome = o.to_string();
        e.after.key = Some(outcome.clone());
        if o == agentgear::Outcome::NoOp {
            // Already installed before the retire: not ours to undo.
            e.after.exists = Some(true);
        }
        Ok(())
    })?;
    if let Some(last) = j.retire.last_mut()
        && last.after.exists == Some(true)
    {
        last.status = Status::Skipped;
        j.write(paths)?;
    }
    out.lines.push(format!("r2: tollgate@tollgate {outcome}"));
    Ok(())
}

fn r3(j: &mut Journal, paths: &Paths, yes: bool, out: &mut Retired) -> Result<()> {
    if seams::herdr_bin().is_none() {
        out.lines.push(skip(
            j,
            paths,
            Op::HerdrInstall,
            Step::R3,
            "herdr is not installed",
        )?);
        return Ok(());
    }
    // `--yes` names no key to bind, so the config is left alone.
    let no_config = yes;
    let e = entry(
        j,
        Op::HerdrInstall,
        None,
        Facts::default(),
        Facts {
            exists: Some(no_config),
            ..Facts::default()
        },
        Step::R3,
    );
    journaled(j, paths, e, |_| seams::herdr_plugin(true, no_config, yes))?;
    out.lines.push(format!(
        "r3: installed tollgate's herdr plugin{}",
        if no_config {
            " (config left alone: --yes binds no key)"
        } else {
            ""
        }
    ));
    Ok(())
}

/// `text` split into lines that keep their endings.
fn lines_of(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_string).collect()
}

fn body(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn write_lines(path: &Path, lines: &[String]) -> Result<()> {
    crate::guest_write::atomic_replace(path, lines.concat().as_bytes())
}

fn r4(j: &mut Journal, paths: &Paths, out: &mut Retired) -> Result<()> {
    let rc = paths.home.join(".bashrc");
    let upstream = format!(
        "source \"{}\"",
        paths
            .source
            .join("completions")
            .join(format!("{}.bash", crate::identity::UPSTREAM_NAME))
            .display()
    );
    let text = match std::fs::read_to_string(&rc) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            out.lines
                .push(skip(j, paths, Op::RewriteLine, Step::R4, "no ~/.bashrc")?);
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", rc.display())),
    };
    let lines = lines_of(&text);
    let Some(at) = lines.iter().position(|l| body(l).trim() == upstream) else {
        out.lines.push(skip(
            j,
            paths,
            Op::RewriteLine,
            Step::R4,
            "no clauth completion line in ~/.bashrc",
        )?);
        return Ok(());
    };
    let ours = crate::completions::write_bash_script()?;
    let mut edits = vec![(at, body(&lines[at]).to_string(), ours)];
    let comment = format!("# {} completions", crate::identity::UPSTREAM_NAME);
    if at > 0 && body(&lines[at - 1]).trim() == comment {
        edits.push((
            at - 1,
            body(&lines[at - 1]).to_string(),
            format!("# {} completions", crate::identity::NAME),
        ));
    }
    for (index, prior, new) in edits {
        let e = entry(
            j,
            Op::RewriteLine,
            Some(rc.clone()),
            Facts {
                line: Some(prior),
                index: Some(index),
                ..Facts::default()
            },
            Facts {
                line: Some(new),
                ..Facts::default()
            },
            Step::R4,
        );
        journaled(j, paths, e, |e| swap_line(e, false).map(|_| ()))?;
    }
    out.lines
        .push("r4: ~/.bashrc now sources tollgate's completions".to_string());
    Ok(())
}

/// Replace one rc line: `prior.line` → `after.line` (or back when `undo`).
/// The line is found at its journaled index, else by its text. Returns
/// whether it changed anything.
fn swap_line(e: &Entry, undo: bool) -> Result<bool> {
    let rc = e
        .dst
        .as_deref()
        .ok_or_else(|| anyhow!("entry has no file"))?;
    let (from, to) = match (&e.prior.line, &e.after.line) {
        (Some(p), Some(a)) if undo => (a, p),
        (Some(p), Some(a)) => (p, a),
        _ => return Err(anyhow!("entry has no line")),
    };
    let text =
        std::fs::read_to_string(rc).with_context(|| format!("failed to read {}", rc.display()))?;
    let mut lines = lines_of(&text);
    let hit = e
        .prior
        .index
        .filter(|i| lines.get(*i).is_some_and(|l| body(l) == from))
        .or_else(|| lines.iter().position(|l| body(l) == from));
    let Some(i) = hit else {
        return Ok(false);
    };
    let ending = lines[i][body(&lines[i]).len()..].to_string();
    lines[i] = format!("{to}{ending}");
    write_lines(rc, &lines)?;
    Ok(true)
}

/// `tollgate import retire`: each pending step of `steps` (all four when
/// empty), in order. Requires a `complete` journal. Refuses while Claude
/// Code, upstream clauth or another tollgate runs: R1 rewrites files a live
/// session rewrites too.
pub(crate) fn retire(steps: &[Step], yes: bool) -> Result<Retired> {
    let paths = Paths::resolve()?;
    let Some(mut j) = Journal::load(&paths)? else {
        return Err(crate::usage_error(
            "tollgate import retire: there is no import; run 'tollgate import clauth --dry-run' first",
        ));
    };
    if j.state() != ImportState::Complete {
        return Err(crate::usage_error(format!(
            "tollgate import retire: the import is {}, not complete",
            j.state
        )));
    }
    let scope = txn::journal_scope(&j);
    let blockers = procs::check(&paths, &scope).blockers;
    if !blockers.is_empty() {
        return Err(ImportBlocked {
            blockers,
            printed: false,
        }
        .into());
    }
    let wanted: Vec<Step> = if steps.is_empty() {
        Step::ALL.to_vec()
    } else {
        Step::ALL
            .into_iter()
            .filter(|s| steps.contains(s))
            .collect()
    };
    let mut out = Retired::default();
    for step in wanted {
        if settled(&j, step) {
            out.lines.push(format!("{}: already done", step.as_str()));
            continue;
        }
        seams::log(|| format!("retire {}", step.as_str()));
        match step {
            Step::R1 => r1(&mut j, &paths, &mut out)?,
            Step::R2 => r2(&mut j, &paths, &mut out)?,
            Step::R3 => r3(&mut j, &paths, yes, &mut out)?,
            Step::R4 => r4(&mut j, &paths, &mut out)?,
        }
    }
    Ok(out)
}

/// The steps `retire` would run, for the confirmation.
pub(crate) fn pending(steps: &[Step]) -> Result<Vec<(Step, &'static str)>> {
    let paths = Paths::resolve()?;
    let j = Journal::load(&paths)?;
    Ok(Step::ALL
        .into_iter()
        .filter(|s| steps.is_empty() || steps.contains(s))
        .filter(|s| j.as_ref().is_none_or(|j| !settled(j, *s)))
        .map(|s| (s, s.describe()))
        .collect())
}

/// Reverse one done retire entry.
pub(crate) fn revert(e: &Entry, paths: &Paths, warnings: &mut Vec<Finding>) -> Result<()> {
    match e.op {
        Op::RewriteJson => edits::revert_json(e, paths, warnings),
        Op::PluginInstall => crate::plugin_host::uninstall_plugin().map(|_| ()),
        Op::HerdrInstall => {
            if seams::herdr_bin().is_none() {
                warnings.push(Finding::new(
                    "herdr_absent",
                    "herdr is gone, so tollgate's herdr plugin is not uninstalled",
                ));
                return Ok(());
            }
            seams::herdr_plugin(false, e.after.exists.unwrap_or(false), true)
        }
        Op::RewriteLine => {
            if !swap_line(e, true)? {
                warnings.push(Finding::new(
                    "line_kept",
                    format!(
                        "{} no longer carries the line retire wrote; it is left as is",
                        e.dst
                            .as_deref()
                            .map_or_else(String::new, |p| paths.tilde(p))
                    ),
                ));
            }
            Ok(())
        }
        other => Err(anyhow!("step {} ({other:?}) is not a retire op", e.seq)),
    }
}

/// Undo every done retire entry, newest first (spec §4.10 step 1). Runs
/// before the rollback takes its fence: R2's and R3's undo spawn `claude`
/// and herdr, and nothing inside the hold spawns a process (I15). The
/// journal stays `complete` until they are all undone.
pub(crate) fn undo_all(j: &mut Journal, paths: &Paths, warnings: &mut Vec<Finding>) -> Result<()> {
    for i in (0..j.retire.len()).rev() {
        if j.retire[i].status != Status::Done {
            continue;
        }
        let seq = j.retire[i].seq;
        seams::log(|| format!("undo retire {seq}"));
        if let Err(e) = revert(&j.retire[i], paths, warnings) {
            return Err(super::ImportNeedsAttention {
                state: j.state.clone(),
                step: Some(seq),
                reason: format!("undoing retire step {seq} stopped: {e:#}"),
            }
            .into());
        }
        j.retire[i].status = Status::Undone;
        j.write(paths)?;
    }
    Ok(())
}
