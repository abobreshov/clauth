// Unix-only: the fixtures build POSIX paths into registry JSON and drive a
// `#!/bin/sh` herdr shim; the plugin itself is linux and macos only.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `herdr::link`: `tollgate herdr link` / `unlink`, the dev install path —
//! plugin-dir resolution, the manifest-id refusal, the registry verdicts
//! against the recorded `plugin list` entries, and the argv each command hands
//! herdr, recorded through a shim (never the real herdr, never the real HOME).

use super::*;
use crate::herdr::{GITHUB, LINKED, plugin_list_json, registry_entry_from};
use crate::testutil::{EnvPin, HomeSandbox};

/// A plugin dir under `root` whose manifest carries `id`.
fn plugin_tree(root: &Path, id: &str) -> PathBuf {
    let dir = root.join("herdr-plugin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("herdr-plugin.toml"),
        format!("id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\n"),
    )
    .unwrap();
    dir
}

/// A local-link registry entry rooted at `root`, in the recorded shape.
fn local_entry(root: &Path) -> RegistryEntry {
    let json = LINKED.replace(
        "/home/uwuclxdy/repos/rs/tollgate/herdr-plugin",
        root.to_str().unwrap(),
    );
    registry_entry_from(&plugin_list_json(&json)).expect("entry")
}

#[test]
fn a_repo_root_the_plugin_dir_and_the_manifest_all_name_the_plugin_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = plugin_tree(tmp.path(), "tollgate");
    assert_eq!(plugin_dir_at(tmp.path()), Some(dir.clone()), "repo root");
    assert_eq!(plugin_dir_at(&dir), Some(dir.clone()), "plugin dir");
    assert_eq!(
        plugin_dir_at(&dir.join("herdr-plugin.toml")),
        Some(dir.clone()),
        "manifest path"
    );
    std::fs::write(tmp.path().join("README.md"), "x").unwrap();
    assert_eq!(plugin_dir_at(&tmp.path().join("README.md")), None);
    assert_eq!(plugin_dir_at(&tmp.path().join("missing")), None);
}

#[test]
fn resolution_prefers_the_path_then_the_cwd_then_the_build_checkout() {
    let explicit = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let built = tempfile::tempdir().unwrap();
    let e = plugin_tree(explicit.path(), "tollgate")
        .canonicalize()
        .unwrap();
    let c = plugin_tree(cwd.path(), "tollgate").canonicalize().unwrap();
    let b = plugin_tree(built.path(), "tollgate")
        .canonicalize()
        .unwrap();

    assert_eq!(
        resolve_plugin_dir(Some(explicit.path()), Some(cwd.path()), built.path()).unwrap(),
        e
    );
    assert_eq!(
        resolve_plugin_dir(None, Some(cwd.path()), built.path()).unwrap(),
        c
    );
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_plugin_dir(None, Some(empty.path()), built.path()).unwrap(),
        b
    );
    let err = resolve_plugin_dir(Some(empty.path()), Some(cwd.path()), built.path())
        .expect_err("an explicit path that holds no plugin never falls through");
    assert!(
        err.to_string().contains("holds no herdr-plugin.toml"),
        "{err}"
    );
    assert!(resolve_plugin_dir(None, Some(empty.path()), empty.path()).is_err());
}

/// The checkout this binary builds from resolves, and its shipped manifest is
/// this tool's: a `cargo build` + `tollgate herdr link` links the fork's plugin.
#[test]
fn the_build_checkout_resolves_to_a_tollgate_manifest() {
    let dir = resolve_plugin_dir(None, None, Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    assert!(dir.ends_with("herdr-plugin"));
    check_local_manifest(&dir).expect("the shipped manifest's id is tollgate");
}

#[test]
fn a_manifest_with_another_id_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = plugin_tree(tmp.path(), "clauth");
    let err = check_local_manifest(&dir).expect_err("upstream's id is refused");
    let msg = err.to_string();
    assert!(msg.contains("has id `clauth`, not `tollgate`"), "{msg}");
    assert!(msg.contains("nothing was linked"), "{msg}");
}

#[test]
fn link_verdicts_over_the_recorded_registry_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = plugin_tree(tmp.path(), "tollgate").canonicalize().unwrap();

    assert_eq!(link_plan(None, &dir).unwrap(), LinkPlan::Link);
    assert_eq!(
        link_plan(Some(&local_entry(&dir)), &dir).unwrap(),
        LinkPlan::AlreadyLinked,
        "linked from this very tree: a no-op"
    );

    let other = tempfile::tempdir().unwrap();
    let other_dir = plugin_tree(other.path(), "tollgate");
    let err = link_plan(Some(&local_entry(&other_dir)), &dir).expect_err("another tree");
    assert!(err.to_string().contains("tollgate herdr unlink"), "{err}");

    let github = registry_entry_from(&plugin_list_json(GITHUB)).unwrap();
    let err = link_plan(Some(&github), &dir).expect_err("a GitHub install");
    assert!(
        err.to_string()
            .contains("tollgate herdr uninstall --no-config"),
        "{err}"
    );
}

#[test]
fn unlink_verdicts_over_the_recorded_registry_entries() {
    assert!(!unlink_plan(None).unwrap(), "nothing linked: a no-op");
    let github = registry_entry_from(&plugin_list_json(GITHUB)).unwrap();
    let err = unlink_plan(Some(&github)).expect_err("a GitHub install is not a link");
    assert!(
        err.to_string().contains("tollgate herdr uninstall"),
        "{err}"
    );

    // The recorded entry's tree is gone: unlinking a deleted checkout is
    // exactly what unlink is for.
    let linked = registry_entry_from(&plugin_list_json(LINKED)).unwrap();
    assert!(unlink_plan(Some(&linked)).unwrap());

    let tmp = tempfile::tempdir().unwrap();
    let dir = plugin_tree(tmp.path(), "clauth");
    let err = unlink_plan(Some(&local_entry(&dir))).expect_err("the tree now names another id");
    assert!(err.to_string().contains("nothing was unlinked"), "{err}");
}

#[test]
fn the_argv_names_the_dir_and_this_tools_id() {
    assert_eq!(
        link_args(Path::new("/src/tollgate/herdr-plugin")).unwrap(),
        ["plugin", "link", "/src/tollgate/herdr-plugin"]
    );
    assert_eq!(unlink_args(), ["plugin", "unlink", "tollgate"]);
}

// ── end to end, through a herdr shim ─────────────────────────────────────────

/// A herdr shim answering `plugin list --json` with `$ANSWER` and recording
/// every other argv into `herdr.log`.
#[cfg(unix)]
fn shim_env<'a>(home: &'a HomeSandbox, answer: &str) -> EnvPin<'a> {
    let shim = crate::testutil::write_shim(
        home.home(),
        "herdr",
        "if [ \"$1\" = plugin ] && [ \"$2\" = list ]; then echo \"$ANSWER\"; exit 0; fi; echo \"$@\" >> \"$(dirname \"$0\")/herdr.log\"; exit 0",
    );
    EnvPin::new(
        home,
        &[
            ("HERDR_BIN_PATH", Some(shim.as_os_str())),
            ("ANSWER", Some(std::ffi::OsStr::new(answer))),
        ],
    )
}

#[cfg(unix)]
fn herdr_log(home: &HomeSandbox) -> String {
    std::fs::read_to_string(home.home().join("herdr.log")).unwrap_or_default()
}

/// `link` hands herdr the canonical plugin dir — in guest mode too: it writes
/// only the registry entry for tollgate's own id, never herdr's config.
#[cfg(unix)]
#[test]
fn link_runs_herdr_plugin_link_on_the_plugin_dir_even_in_guest_mode() {
    let home = HomeSandbox::new();
    std::fs::create_dir_all(home.home().join(".clauth")).unwrap();
    assert!(crate::identity::upstream_active(), "guest mode is on");
    let repo = home.home().join("repo");
    let dir = plugin_tree(&repo, "tollgate").canonicalize().unwrap();
    let _env = shim_env(&home, &plugin_list_json(r#"{"plugin_id":"other"}"#));

    link(Some(&repo)).expect("links");
    assert_eq!(
        herdr_log(&home).trim(),
        format!("plugin link {}", dir.display())
    );
    assert!(
        !home.home().join(".config/herdr/config.toml").exists(),
        "no herdr config was written"
    );
}

#[cfg(unix)]
#[test]
fn link_refuses_upstreams_manifest_before_herdr_runs() {
    let home = HomeSandbox::new();
    let repo = home.home().join("upstream");
    plugin_tree(&repo, "clauth");
    let _env = shim_env(&home, &plugin_list_json(r#"{"plugin_id":"other"}"#));

    link(Some(&repo)).expect_err("id clauth is refused");
    assert_eq!(herdr_log(&home), "", "herdr was never asked to link");
}

#[cfg(unix)]
#[test]
fn link_over_the_same_tree_is_a_no_op() {
    let home = HomeSandbox::new();
    let repo = home.home().join("repo");
    let dir = plugin_tree(&repo, "tollgate").canonicalize().unwrap();
    let json = LINKED.replace(
        "/home/uwuclxdy/repos/rs/tollgate/herdr-plugin",
        dir.to_str().unwrap(),
    );
    let _env = shim_env(&home, &plugin_list_json(&json));

    link(Some(&dir)).expect("already linked");
    assert_eq!(herdr_log(&home), "", "nothing re-linked");
}

#[cfg(unix)]
#[test]
fn unlink_runs_herdr_plugin_unlink_tollgate_for_a_local_link_only() {
    let home = HomeSandbox::new();
    {
        let _env = shim_env(&home, &plugin_list_json(LINKED));
        unlink().expect("unlinks");
        assert_eq!(herdr_log(&home).trim(), "plugin unlink tollgate");
    }
    std::fs::remove_file(home.home().join("herdr.log")).unwrap();
    {
        let _env = shim_env(&home, &plugin_list_json(GITHUB));
        unlink().expect_err("a GitHub install is refused");
        assert_eq!(herdr_log(&home), "", "herdr was never asked");
    }
    {
        let _env = shim_env(&home, &plugin_list_json(r#"{"plugin_id":"clauth"}"#));
        unlink().expect("nothing of ours linked: a no-op");
        assert_eq!(herdr_log(&home), "", "upstream's plugin is never named");
    }
}

#[test]
fn link_and_unlink_parse() {
    use crate::cli::{Cli, Command, HerdrCommand};
    use clap::Parser as _;
    let Some(Command::Herdr {
        cmd: HerdrCommand::Link { path },
    }) = Cli::try_parse_from(["tollgate", "herdr", "link", "--path", "/src/t"])
        .unwrap()
        .command
    else {
        panic!("link arm");
    };
    assert_eq!(path.as_deref(), Some(Path::new("/src/t")));
    assert!(matches!(
        Cli::try_parse_from(["tollgate", "herdr", "link"])
            .unwrap()
            .command,
        Some(Command::Herdr {
            cmd: HerdrCommand::Link { path: None }
        })
    ));
    assert!(matches!(
        Cli::try_parse_from(["tollgate", "herdr", "unlink"])
            .unwrap()
            .command,
        Some(Command::Herdr {
            cmd: HerdrCommand::Unlink
        })
    ));
}
