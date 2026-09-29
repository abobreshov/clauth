//! Local-read probes backing the Plugin tab: binary-on-`PATH` resolution, Claude
//! Code's plugin registry (`installed_plugins.json` / `known_marketplaces.json`),
//! the manual `mcpServers` wiring, the `claude --version` string, and the one
//! safe write the tab performs (wire `mcpServers.tollgate`).
//!
//! Everything here is a cheap filesystem/`PATH` read except [`cc_version`], which
//! runs one short subprocess; the Plugin tab caches that result. Nothing spawns a
//! background thread. All path reads route through the test-overridable
//! `home_dir()` / `claude_dir()`, so the inline tests can sandbox `$HOME`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Map, Value};

use crate::profile::{atomic_write, claude_dir, home_dir};

/// Plugin id in the registry (`<plugin>@<marketplace>`).
pub(crate) const PLUGIN_ID: &str = crate::identity::CC_PLUGIN;
/// Marketplace key in `known_marketplaces.json`.
pub(crate) const MARKETPLACE_KEY: &str = crate::identity::NAME;

/// Resolve `binary` against `PATH`, returning the first hit. The OS does this
/// implicitly when spawning, but a presence *check* needs it spelled out. On
/// Windows the usual executable extensions are tried too; on Unix the exec bit is
/// required so a non-executable file named `tollgate` doesn't read as "resolved".
pub(crate) fn on_path(binary: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{binary}{ext}")))
            .find(|candidate| is_executable(candidate))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// One install record from `plugins["tollgate@tollgate"]`. Every field is optional —
/// CC's schema is treated leniently so a shape change degrades to "unknown"
/// rather than a parse error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InstallRecord {
    pub(crate) scope: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) git_commit_sha: Option<String>,
    pub(crate) installed_at: Option<String>,
    pub(crate) install_path: Option<String>,
    pub(crate) project_path: Option<String>,
}

/// Marketplace source for `tollgate`, from `known_marketplaces.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MarketplaceInfo {
    pub(crate) repo: Option<String>,
    pub(crate) install_location: Option<String>,
}

/// Where a manual `mcpServers.tollgate` entry lives, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpWiring {
    /// `~/.claude.json` (user-global) carries it.
    GlobalConfig,
    /// A project `./.mcp.json` carries it.
    ProjectFile,
    /// No manual wiring found.
    None,
}

/// Install records for the tollgate plugin; empty when the registry is absent,
/// unreadable, or carries no entry (all read as "not installed").
pub(crate) fn installed_records() -> Vec<InstallRecord> {
    let Some(root) = read_json(plugins_dir().map(|dir| dir.join("installed_plugins.json"))) else {
        return Vec::new();
    };
    root.get("plugins")
        .and_then(|plugins| plugins.get(PLUGIN_ID))
        .and_then(Value::as_array)
        .map(|records| records.iter().map(install_record_from).collect())
        .unwrap_or_default()
}

/// Marketplace record for tollgate, when the marketplace is known to CC.
pub(crate) fn marketplace_known() -> Option<MarketplaceInfo> {
    let root = read_json(plugins_dir().map(|d| d.join("known_marketplaces.json")))?;
    let entry = root.get(MARKETPLACE_KEY)?;
    Some(MarketplaceInfo {
        repo: entry
            .get("source")
            .and_then(|source| source.get("repo"))
            .and_then(Value::as_str)
            .map(str::to_string),
        install_location: entry
            .get("installLocation")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Manual `mcpServers.tollgate` wiring, preferring the user-global config over a
/// project file (the global is the one the fix writes).
pub(crate) fn manual_mcp_wiring() -> McpWiring {
    if global_claude_json_path().is_some_and(|path| json_has_tollgate_mcp(&path)) {
        McpWiring::GlobalConfig
    } else if json_has_tollgate_mcp(Path::new(".mcp.json")) {
        McpWiring::ProjectFile
    } else {
        McpWiring::None
    }
}

/// Whether the user-global `mcpServers.tollgate` entry matches the canonical stdio
/// entry tollgate writes. `None` when no global manual entry exists (nothing to
/// validate — a plugin install or a project file is judged elsewhere). `Some(false)`
/// flags drift: a stale absolute `command` or `args` missing `mcp` reads as "wired"
/// but won't launch the current server, so the tab re-offers the canonical write.
pub(crate) fn global_entry_drifted() -> Option<bool> {
    let entry = read_json(global_claude_json_path()).and_then(|root| {
        root.get("mcpServers")
            .and_then(|servers| servers.get(crate::identity::NAME))
            .cloned()
    })?;
    let canon = tollgate_mcp_entry();
    // `type` is allowed to be absent (CC defaults stdio); only command + args are
    // load-bearing for the launch.
    let same =
        entry.get("command") == canon.get("command") && entry.get("args") == canon.get("args");
    Some(!same)
}

/// Verdict of a live `tollgate mcp` discovery handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpProbe {
    /// Server answered `server/discover` with a result advertising its tools.
    Ok,
    /// Server couldn't be spawned or didn't answer a valid result (reason).
    Failed(String),
}

/// Spawn `tollgate mcp`, send one JSON-RPC `server/discover`, and confirm the reply
/// advertises tools. Client-faithful: catches a `tollgate` that resolves on PATH but is
/// too old to serve (no `mcp` subcommand, or no stateless protocol) or boots then dies. Heavier than the
/// other probes — the server runs `gc_stale_runtimes()` at startup — so the tab
/// gates it behind `r` only. Drains stdout on a thread so a chatty server can't
/// deadlock the pipe; 3s budget, then kill.
fn probe_command() -> Command {
    let mut cmd = Command::new(probe_exe());
    cmd.arg("mcp")
        .env(crate::mcp::MCP_PROBE_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::providers::billing_key::scrub_helper_env(&mut cmd);
    cmd
}

/// The binary the probe spawns: the running executable (a replaced binary's
/// `(deleted)` suffix stripped), never a `PATH` lookup, so the probe can only
/// ever start this tool's own server. Test builds keep the bare name: the
/// running executable there is the test harness, which must not be re-run as
/// a probe child.
fn probe_exe() -> std::path::PathBuf {
    if cfg!(test) {
        return std::path::PathBuf::from(crate::identity::NAME);
    }
    std::env::current_exe().map_or_else(
        |_| std::path::PathBuf::from(crate::identity::NAME),
        |exe| crate::platform::installed_exe_path(&exe),
    )
}

pub(crate) fn mcp_boots() -> McpProbe {
    let mut child = match probe_command().spawn() {
        Ok(child) => child,
        Err(e) => return McpProbe::Failed(format!("spawn failed: {e}")),
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return McpProbe::Failed("no stdio pipes".to_string());
    };

    let req = discover_frame();
    if writeln!(stdin, "{req}")
        .and_then(|()| stdin.flush())
        .is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return McpProbe::Failed("write failed".to_string());
    }

    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(result);
    });

    let verdict = match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(line)) => parse_discover_reply(&line),
        Ok(Err(e)) => McpProbe::Failed(format!("read failed: {e}")),
        Err(_) => McpProbe::Failed("no reply within 3s".to_string()),
    };
    // EOF on stdin + kill ends the server; the reader unblocks once stdout closes.
    let _ = child.kill();
    let _ = child.wait();
    drop(stdin);
    let _ = reader.join();
    verdict
}

/// The stateless opener the probe sends. An `initialize` frame would keep
/// passing against a server no modern client can talk to, since rmcp answers
/// both eras; there is no handshake left, so the version and the client's
/// capabilities ride in `_meta` on the request itself.
fn discover_frame() -> Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "tollgate-probe", "version": env!("CARGO_PKG_VERSION")
                },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
}

/// JSON-RPC "method not found", which is what a `tollgate` predating the stateless
/// protocol answers `server/discover` with.
const METHOD_NOT_FOUND: i64 = -32601;

/// Classify the first stdout line of a `server/discover` handshake. A result
/// proves the server booted, and the `tools` capability inside it is what
/// decides whether a real client exposes any tools at all — a forced
/// `tools/list` answers either way, so it cannot stand in for this.
fn parse_discover_reply(line: &str) -> McpProbe {
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return McpProbe::Failed("unparseable reply".to_string());
    };
    if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64);
        return McpProbe::Failed(if code == Some(METHOD_NOT_FOUND) {
            "no `server/discover`: the tollgate on PATH predates the stateless protocol".to_string()
        } else {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            format!("server rejected `server/discover`: {message}")
        });
    }
    let Some(result) = value.get("result") else {
        return McpProbe::Failed("no result in reply".to_string());
    };
    if result.pointer("/capabilities/tools").is_none() {
        return McpProbe::Failed("server advertises no tools capability".to_string());
    }
    McpProbe::Ok
}

/// `claude --version`, trimmed to its first line. `None` when the binary is
/// missing or the call fails — the row then reports "unknown".
pub(crate) fn cc_version() -> Option<String> {
    let output = crate::runtime::claude_command()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// `~/.claude.json` (user-global), the file the wire fix edits.
pub(crate) fn global_claude_json_path() -> Option<PathBuf> {
    Some(home_dir().ok()?.join(".claude.json"))
}

/// Write `mcpServers.tollgate` into `~/.claude.json`, preserving every other field
/// (key order is kept via serde_json's `preserve_order`). The entry mirrors the
/// plugin manifest so a manual wire matches a plugin install. Creates the file
/// when absent. Any other read failure, and a file that does not parse as a
/// JSON object (Claude Code caught mid-write, a hand edit), is an error: a
/// fresh map there would replace the whole file, every other server and the
/// account identity included.
///
/// Guest mode: `~/.claude.json` is upstream clauth's, but `mcpServers.tollgate`
/// is tollgate's own key, so the write goes through
/// `guest_write::guest_additive_write` — under upstream's lock, refused when
/// anything but that entry would change (a non-object `mcpServers` included),
/// atomic and mode-preserving.
pub(crate) fn wire_mcp_server() -> Result<()> {
    let path = home_dir()?.join(".claude.json");
    if crate::identity::upstream_active() {
        crate::guest_write::guest_additive_write(
            &path,
            crate::guest_write::CLAUDE_JSON_KEYS,
            |root| {
                insert_tollgate_mcp(root);
                Ok(())
            },
        )?;
        return Ok(());
    }
    let mut root: Map<String, Value> = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => map,
            Ok(_) => anyhow::bail!("{} is not a JSON object; left it alone", path.display()),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "{} does not parse (Claude Code may be writing it); left it alone, retry",
                        path.display()
                    )
                });
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    insert_tollgate_mcp(&mut root);
    atomic_write(&path, serde_json::to_vec_pretty(&Value::Object(root))?)?;
    Ok(())
}

/// Remove `mcpServers.tollgate` from `~/.claude.json`, and `mcpServers`
/// itself when that leaves it empty. Every other key stays byte-for-byte
/// what it was in value: the write goes through
/// `guest_write::guest_additive_write` in every mode, which refuses when
/// anything but tollgate's own entry would change. Returns whether the file
/// changed; an absent file or entry is left alone (never created).
pub(crate) fn unwire_mcp_server() -> Result<bool> {
    let path = home_dir()?.join(".claude.json");
    crate::guest_write::guest_additive_write(&path, crate::guest_write::CLAUDE_JSON_KEYS, |root| {
        let emptied = match root.get_mut("mcpServers") {
            Some(Value::Object(servers)) => {
                servers.shift_remove(crate::identity::NAME);
                servers.is_empty()
            }
            _ => false,
        };
        if emptied {
            root.shift_remove("mcpServers");
        }
        Ok(())
    })
}

/// Set `mcpServers.tollgate` to the canonical entry, creating `mcpServers`
/// when absent. A non-object `mcpServers` is replaced by a fresh map — which
/// guest mode's owned-keys check then refuses, since that value is not
/// tollgate's.
fn insert_tollgate_mcp(root: &mut Map<String, Value>) {
    let entry = tollgate_mcp_entry();
    match root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(servers) => {
            servers.insert(crate::identity::NAME.to_string(), entry);
        }
        // `mcpServers` existed but wasn't an object — replace it with a fresh map.
        other => {
            *other = Value::Object(Map::from_iter([(crate::identity::NAME.to_string(), entry)]))
        }
    }
}

/// The canonical stdio entry tollgate registers (matches `plugin.json`).
fn tollgate_mcp_entry() -> Value {
    serde_json::json!({ "type": "stdio", "command": crate::identity::NAME, "args": ["mcp"] })
}

/// CC's user-scope plugin registry. `claude` resolves its config dir as
/// `$CLAUDE_CONFIG_DIR` when set and non-empty, else `~/.claude`, and the
/// registry lives under it — so the probe reads the same resolution the CLI
/// writes. That is what makes the install fix converge: agentgear drives the
/// real CLI, which honors the override, and the recomputed row must read the
/// registry the CLI just wrote. `profile::claude_dir()` stays the `~/.claude`
/// view the credentials link uses; the registry is not the link.
fn plugins_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => claude_dir().ok()?,
    };
    Some(base.join("plugins"))
}

/// Parse a JSON file into a `Value`, returning `None` on any missing/unreadable/
/// unparseable input (graceful — the registry files often don't exist).
fn read_json(path: Option<PathBuf>) -> Option<Value> {
    let bytes = std::fs::read(path?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn json_has_tollgate_mcp(path: &Path) -> bool {
    read_json(Some(path.to_path_buf()))
        .and_then(|root| {
            root.get("mcpServers")
                .and_then(|servers| servers.get(crate::identity::NAME))
                .cloned()
        })
        .is_some()
}

fn install_record_from(value: &Value) -> InstallRecord {
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    InstallRecord {
        scope: field("scope"),
        version: field("version"),
        git_commit_sha: field("gitCommitSha"),
        installed_at: field("installedAt"),
        install_path: field("installPath"),
        project_path: field("projectPath"),
    }
}

#[cfg(test)]
#[path = "../tests/inline/plugin_probe.rs"]
mod tests;
