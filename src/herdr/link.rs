//! `tollgate herdr link` / `tollgate herdr unlink`: the dev install path.
//!
//! The fork has no `tollgate-v*` release tag yet, so `tollgate herdr install`
//! (which fetches the published plugin) has nothing of the fork's to fetch.
//! Linking registers a local checkout's `herdr-plugin/` directory instead
//! (`herdr plugin link <dir>`); herdr runs it in place, so a script edit is
//! live on the next hook.
//!
//! Both halves refuse unless the manifest they act on carries this tool's
//! plugin id: the fork's history carries upstream's tree (id `clauth`), and
//! linking that would register over upstream's live `clauth` plugin. Neither
//! writes herdr's `config.toml` — only herdr's own registry entry for the
//! `tollgate` id — so guest mode (plan §4.0, which guards the herdr config and
//! upstream's blocks in it) does not gate them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{PLUGIN_ID, RegistryEntry, herdr_bin, manifest_id, registry_probe, run};
use crate::out::outln;

/// The manifest's file name inside a plugin directory.
const MANIFEST_FILE: &str = "herdr-plugin.toml";
/// The plugin directory's name inside the repo.
const PLUGIN_DIR: &str = "herdr-plugin";

/// The plugin directory `path` names: the directory holding the manifest,
/// given the manifest itself, the plugin directory, or a repo root with
/// `herdr-plugin/` inside. `None` when none of those shapes fits.
pub(crate) fn plugin_dir_at(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        if path.file_name()? != MANIFEST_FILE {
            return None;
        }
        return path.parent().map(Path::to_path_buf);
    }
    if path.join(MANIFEST_FILE).is_file() {
        return Some(path.to_path_buf());
    }
    let nested = path.join(PLUGIN_DIR);
    nested.join(MANIFEST_FILE).is_file().then_some(nested)
}

/// The directory `link` registers: `--path` when given (and it must fit),
/// else the working directory's checkout, else the checkout this binary was
/// built from — a `cargo build` in the repo links that repo's plugin.
pub(crate) fn resolve_plugin_dir(
    explicit: Option<&Path>,
    cwd: Option<&Path>,
    built_from: &Path,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let dir = plugin_dir_at(path).with_context(|| {
            format!(
                "{} holds no {MANIFEST_FILE}, directly or under {PLUGIN_DIR}/",
                path.display()
            )
        })?;
        return canonical(&dir);
    }
    if let Some(dir) = cwd.and_then(plugin_dir_at) {
        return canonical(&dir);
    }
    if let Some(dir) = plugin_dir_at(built_from) {
        return canonical(&dir);
    }
    bail!(
        "found no {PLUGIN_DIR}/{MANIFEST_FILE} here or in the checkout this binary was built from; \
         pass `--path <repo or plugin dir>`"
    )
}

fn canonical(dir: &Path) -> Result<PathBuf> {
    dir.canonicalize()
        .with_context(|| format!("could not resolve {}", dir.display()))
}

/// Refuse unless the manifest in `dir` names this tool's plugin id.
pub(crate) fn check_local_manifest(dir: &Path) -> Result<()> {
    let path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let id =
        manifest_id(&text).with_context(|| format!("{} names no plugin id", path.display()))?;
    if id == PLUGIN_ID {
        return Ok(());
    }
    bail!(
        "{} has id `{id}`, not `{PLUGIN_ID}`: linking it would register over the `{id}` plugin; \
         nothing was linked",
        path.display()
    )
}

/// What `link` does given the registry's current `tollgate` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkPlan {
    /// Nothing registered under the id: link.
    Link,
    /// Already linked from this very directory: nothing to do.
    AlreadyLinked,
}

/// The pure verdict over the registry entry. A GitHub install or a link from
/// another tree is the user's own, never replaced behind them.
pub(crate) fn link_plan(entry: Option<&RegistryEntry>, dir: &Path) -> Result<LinkPlan> {
    let Some(entry) = entry else {
        return Ok(LinkPlan::Link);
    };
    let root = entry.plugin_root.clone().unwrap_or_default();
    match entry.source_kind.as_deref() {
        Some("local") => {
            let same = Path::new(&root)
                .canonicalize()
                .is_ok_and(|linked| linked == dir);
            if same {
                return Ok(LinkPlan::AlreadyLinked);
            }
            bail!(
                "herdr already has the {PLUGIN_ID} plugin linked from {root}; \
                 run `tollgate herdr unlink` first to link {}",
                dir.display()
            )
        }
        _ => bail!(
            "herdr has the {PLUGIN_ID} plugin installed from GitHub; \
             run `tollgate herdr uninstall --no-config` first to link {} in its place",
            dir.display()
        ),
    }
}

/// `herdr plugin link <dir>`'s argv.
pub(crate) fn link_args(dir: &Path) -> Result<Vec<String>> {
    let dir = dir
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", dir.display()))?;
    Ok(vec!["plugin".into(), "link".into(), dir.to_string()])
}

/// `herdr plugin unlink tollgate`'s argv: the id is this tool's constant, so
/// the call can never name upstream's plugin.
pub(crate) fn unlink_args() -> [&'static str; 3] {
    ["plugin", "unlink", PLUGIN_ID]
}

/// `tollgate herdr link [--path <dir>]`.
pub(crate) fn link(path: Option<&Path>) -> Result<()> {
    if cfg!(windows) {
        bail!("the herdr plugin is linux and macos only: its entrypoints are POSIX shell scripts");
    }
    let cwd = std::env::current_dir().ok();
    let dir = resolve_plugin_dir(path, cwd.as_deref(), Path::new(env!("CARGO_MANIFEST_DIR")))?;
    check_local_manifest(&dir)?;

    let bin = herdr_bin();
    // A probe that cannot run proceeds: herdr's own link answers loudly.
    let (entry, error) = registry_probe(&bin);
    let entry = if error.is_none() { entry } else { None };
    match link_plan(entry.as_ref(), &dir)? {
        LinkPlan::AlreadyLinked => {
            outln!("tollgate: herdr already links {}", dir.display());
            return Ok(());
        }
        LinkPlan::Link => {}
    }

    outln!("tollgate: linking {} into herdr", dir.display());
    let args = link_args(&dir)?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&bin, &args)?;
    outln!(
        "tollgate: linked; paste the key and sidebar rows from {}/README.md, \
         and `tollgate herdr unlink` takes the link back out",
        dir.display()
    );
    Ok(())
}

/// Refuse to unlink anything but a local link of this tool's plugin.
pub(crate) fn unlink_plan(entry: Option<&RegistryEntry>) -> Result<bool> {
    let Some(entry) = entry else {
        return Ok(false);
    };
    if entry.source_kind.as_deref() != Some("local") {
        bail!(
            "herdr has the {PLUGIN_ID} plugin installed from GitHub, not linked; \
             `tollgate herdr uninstall` removes that one"
        );
    }
    // The registry keys on the id already; the manifest on disk is read too,
    // so a tree whose manifest now names another plugin is left for herdr's
    // own `plugin unlink` by hand. A manifest that is gone (a deleted
    // checkout) is exactly what unlink is for, so it proceeds.
    if let Some(root) = &entry.plugin_root
        && let Ok(text) = std::fs::read_to_string(Path::new(root).join(MANIFEST_FILE))
        && let Some(id) = manifest_id(&text)
        && id != PLUGIN_ID
    {
        bail!(
            "{root}/{MANIFEST_FILE} now has id `{id}`, not `{PLUGIN_ID}`; \
             nothing was unlinked"
        );
    }
    Ok(true)
}

/// `tollgate herdr unlink`.
pub(crate) fn unlink() -> Result<()> {
    let bin = herdr_bin();
    let (entry, error) = registry_probe(&bin);
    if let Some(error) = error {
        bail!("{error}; nothing was unlinked");
    }
    if !unlink_plan(entry.as_ref())? {
        outln!("tollgate: herdr has no {PLUGIN_ID} plugin linked");
        return Ok(());
    }
    run(&bin, &unlink_args())?;
    outln!("tollgate: unlinked the herdr plugin (its files were left alone)");
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/inline/herdr_link.rs"]
mod tests;
