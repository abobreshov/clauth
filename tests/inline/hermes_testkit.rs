#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Hermetic Hermes fixtures (spec §7): a fake venv whose `bin/python` is a
//! shell stub, so no test ever runs a real `hermes`, `mise` or `python`.
//!
//! The stub is both halves of what tollgate spawns:
//! - run as `python -I -B -c <PROJECTOR> <home> <managed>`, it prints
//!   `ctl/projection.json`, runs `ctl/projector.after` if present, and exits
//!   `ctl/projector.exit` (default 0);
//! - run as the entrypoint's interpreter (`bin/hermes` has the shebang
//!   `#!<venv>/bin/python`), it records the call and exits `ctl/hermes.exit`.
//!
//! Every call is appended to `rec` as one block: `CALL` with the argv
//! (unit-separated), `HOME`, `CWD`, every env var as `ENV K=V`, the profile
//! dir listing and the live-session registry listing, then `END`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) struct Fixture {
    pub(crate) entry: PathBuf,
    pub(crate) python: PathBuf,
    pub(crate) hsp: PathBuf,
    pub(crate) rec: PathBuf,
    pub(crate) ctl: PathBuf,
}

/// One recorded stub call.
#[derive(Debug, Clone, Default)]
pub(crate) struct Call {
    /// `["projector", <home>, <managed>]` for a projector run, else the
    /// entrypoint's args (after the script path).
    pub(crate) argv: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: String,
    /// `ls -a` of the profile dir, when HERMES_HOME was set.
    pub(crate) profile_dir: Vec<String>,
    /// `ls` of `~/.tollgate/live_sessions`.
    pub(crate) live_rows: Vec<String>,
    /// The live-session rows' JSON, one per file.
    pub(crate) rows: Vec<String>,
}

impl Call {
    pub(crate) fn is_projector(&self) -> bool {
        self.argv.first().is_some_and(|a| a == "projector")
    }
}

fn write_exec(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Fixture {
    /// A 0.19.0 venv under `root`.
    pub(crate) fn new(root: &Path) -> Self {
        Self::with_version(root, "0.19.0")
    }

    pub(crate) fn with_version(root: &Path, version: &str) -> Self {
        Self::in_venv(root, &root.join("venv"), version)
    }

    /// A fixture whose venv is exactly `venv` (a mise or pipx layout); its
    /// control files and record live under `root`.
    pub(crate) fn in_venv(root: &Path, venv: &Path, version: &str) -> Self {
        let venv = venv.to_path_buf();
        let ctl = root.join("ctl");
        let rec = root.join("rec.log");
        std::fs::create_dir_all(&ctl).unwrap();
        let python = venv.join("bin").join("python");
        let entry = venv.join("bin").join("hermes");
        let hsp = venv.join("lib").join("python3.13").join("site-packages");
        let stub = format!(
            r#"#!/bin/sh
CTL='{ctl}'
REC='{rec}'
{{
  printf 'CALL'
  if [ "$1" = "-I" ]; then
    printf '\037projector\037%s\037%s' "$5" "$6"
  else
    shift
    for a in "$@"; do printf '\037%s' "$a"; done
  fi
  printf '\n'
  printf 'CWD=%s\n' "$(pwd)"
  if [ -n "$HERMES_HOME" ]; then
    printf 'PROFILE_DIR'; for f in $(ls -a "$(dirname "$HERMES_HOME")"); do printf '\037%s' "$f"; done; printf '\n'
    TG="$(dirname "$(dirname "$(dirname "$HERMES_HOME")")")"
    printf 'LIVE'; for f in $(ls "$TG/live_sessions" 2>/dev/null); do printf '\037%s' "$f"; done; printf '\n'
    for f in "$TG"/live_sessions/*.json; do [ -f "$f" ] && printf 'ROW %s\n' "$(tr -d '\n' < "$f")"; done
  fi
  env | LC_ALL=C sort | sed 's/^/ENV /'
  printf 'END\n'
}} >> "$REC"
if [ "$1" = "-I" ]; then
  if [ -f "$CTL/projector.sleep" ]; then sleep "$(cat "$CTL/projector.sleep")"; fi
  cat "$CTL/projection.json"
  if [ -f "$CTL/projector.after" ]; then sh "$CTL/projector.after"; fi
  exit "$(cat "$CTL/projector.exit" 2>/dev/null || echo 0)"
fi
exit "$(cat "$CTL/hermes.exit" 2>/dev/null || echo 0)"
"#,
            ctl = ctl.display(),
            rec = rec.display()
        );
        write_exec(&python, &stub);
        write_exec(
            &entry,
            &format!(
                "#!{}\n# -*- coding: utf-8 -*-\nfrom hermes_cli.main import main\n",
                python.display()
            ),
        );
        std::fs::create_dir_all(hsp.join("hermes_cli")).unwrap();
        std::fs::write(hsp.join("hermes_cli").join("main.py"), "").unwrap();
        let dist = hsp.join(format!("hermes_agent-{version}.dist-info"));
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(
            dist.join("METADATA"),
            format!("Metadata-Version: 2.4\nName: hermes-agent\nVersion: {version}\n\nbody\n"),
        )
        .unwrap();
        let plugin = hsp.join("plugins/model-providers/openrouter");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("__init__.py"),
            "profile = ProviderProfile(\n    name=\"openrouter\",\n    env_vars=(\"OPENROUTER_API_KEY\", \"TG_PLUGIN_ONLY_KEY\"),\n)\n",
        )
        .unwrap();
        let fx = Self {
            entry,
            python,
            hsp,
            rec,
            ctl,
        };
        fx.set_projection(&passing_projection("openrouter"));
        fx
    }

    pub(crate) fn set_projection(&self, value: &serde_json::Value) {
        std::fs::write(
            self.ctl.join("projection.json"),
            serde_json::to_string(value).unwrap(),
        )
        .unwrap();
    }

    pub(crate) fn set_ctl(&self, name: &str, body: &str) {
        std::fs::write(self.ctl.join(name), body).unwrap();
    }

    /// Point the roster's `[settings] bin` at this fixture's entrypoint
    /// (writes a fresh roster, so call it before any profile exists).
    pub(crate) fn install_as_settings_bin(&self) {
        let dir = crate::profile::tollgate_dir().unwrap();
        crate::profile::mkdir_700(&dir).unwrap();
        std::fs::write(
            dir.join("hermes-profiles.toml"),
            format!(
                "schema_version = 1\n[settings]\nbin = \"{}\"\n",
                self.entry.display()
            ),
        )
        .unwrap();
    }

    pub(crate) fn clear_rec(&self) {
        let _ = std::fs::remove_file(&self.rec);
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        let Ok(text) = std::fs::read_to_string(&self.rec) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut cur: Option<Call> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("CALL") {
                cur = Some(Call {
                    argv: rest.split('\u{1f}').skip(1).map(str::to_string).collect(),
                    ..Call::default()
                });
            } else if let Some(c) = cur.as_mut() {
                if line == "END" {
                    out.push(cur.take().unwrap());
                } else if let Some(v) = line.strip_prefix("CWD=") {
                    c.cwd = v.to_string();
                } else if let Some(rest) = line.strip_prefix("PROFILE_DIR") {
                    c.profile_dir = rest.split('\u{1f}').skip(1).map(str::to_string).collect();
                } else if let Some(rest) = line.strip_prefix("LIVE") {
                    c.live_rows = rest.split('\u{1f}').skip(1).map(str::to_string).collect();
                } else if let Some(row) = line.strip_prefix("ROW ") {
                    c.rows.push(row.to_string());
                } else if let Some(kv) = line.strip_prefix("ENV ")
                    && let Some((k, v)) = kv.split_once('=')
                {
                    c.env.insert(k.to_string(), v.to_string());
                }
            }
        }
        out
    }

    /// The entrypoint calls only (not the projector).
    pub(crate) fn hermes_calls(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| !c.is_projector())
            .collect()
    }
}

/// A projection every guard passes: nothing managed, no secrets, every
/// pinned auxiliary task on `provider`.
pub(crate) fn passing_projection(provider: &str) -> serde_json::Value {
    let aux: serde_json::Map<String, serde_json::Value> = super::guards::HERMES_AUX_TASKS
        .iter()
        .map(|t| {
            (
                (*t).to_string(),
                serde_json::json!({"provider": provider, "base_url_host": null}),
            )
        })
        .collect();
    serde_json::json!({
        "config": {
            "model_provider": provider,
            "model": null,
            "model_base_url_host": null,
            "providers": [],
            "custom_providers": [],
            "fallback_providers": [],
            "fallback_model": [],
            "auxiliary": aux,
            "delegation": {"provider": null, "base_url_host": null},
            "credential_pool_strategies": {},
            "secrets": {},
            "plugins_enabled": [],
        },
        "managed_config_top_keys": [],
        // What the projector parses out of a home `.env` that `new` wrote for
        // an env-mode openrouter profile (the writer cross-checks against it).
        "env_keys": {
            "home": [{"key": "OPENROUTER_API_KEY", "nonblank": true}],
            "op_env": [],
            "managed": [],
        },
    })
}

/// RAII: point the managed-scope default at a path that does not exist, so
/// no test ever consults the real `/etc/hermes`. Borrows the sandbox so it
/// cannot outlive `HOME_TEST_LOCK`.
pub(crate) struct NoManagedScope<'a>(std::marker::PhantomData<&'a crate::testutil::HomeSandbox>);

impl<'a> NoManagedScope<'a> {
    pub(crate) fn new(home: &'a crate::testutil::HomeSandbox) -> Self {
        Self::at(home, &home.home().join("no-such-managed-dir"))
    }

    pub(crate) fn at(_home: &'a crate::testutil::HomeSandbox, dir: &Path) -> Self {
        *super::guards::MANAGED_DIR_OVERRIDE.lock().unwrap() = Some(dir.to_path_buf());
        Self(std::marker::PhantomData)
    }
}

impl Drop for NoManagedScope<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = super::guards::MANAGED_DIR_OVERRIDE.lock() {
            *g = None;
        }
    }
}

/// The test key and its Hermes fingerprint, computed in Python:
/// `"sha256:" + hashlib.sha256(b"sk-or-v1-tollgate-test-key").hexdigest()[:16]`.
pub(crate) const TEST_KEY: &str = "sk-or-v1-tollgate-test-key";
pub(crate) const TEST_KEY_FINGERPRINT: &str = "sha256:1b43f854967b5ae2";

/// Create an env-mode openrouter profile through `new`, with the fixture as
/// the entrypoint.
pub(crate) fn new_openrouter(name: &str) {
    let opts = super::NewOpts {
        name: name.to_string(),
        provider: super::profiles::Provider::Openrouter,
        model: None,
        pool: false,
        env_key: false,
        no_key: false,
    };
    super::new_profile(&opts, &mut |_| Ok(TEST_KEY.to_string())).expect("hermes new");
}

/// Everything under `root` as `(relative path, kind, content hash)`, links by
/// target: a byte-for-byte fingerprint of a tree.
pub(crate) fn tree_digest(root: &Path) -> Vec<String> {
    use sha2::Digest as _;
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let meta = path.symlink_metadata().unwrap();
            if meta.file_type().is_symlink() {
                out.push(format!(
                    "l {rel} -> {}",
                    std::fs::read_link(&path).unwrap().display()
                ));
            } else if meta.is_dir() {
                out.push(format!("d {rel}"));
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.push(format!(
                    "f {rel} {}",
                    hex::encode(sha2::Sha256::digest(&bytes))
                ));
            }
        }
    }
    out.sort();
    out
}
