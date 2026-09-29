//! Private, monitor-only credential overlay. Never mutates the process environment.
use crate::out::{errln, outln};
use crate::usage::monitor::source::Secret;
use anyhow::{Result, bail};
use clap::Subcommand;
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use zeroize::Zeroizing;

static PREFER_STORE: AtomicBool = AtomicBool::new(false);
static LOAD_WARNING: AtomicBool = AtomicBool::new(false);
const MAX_FILE: u64 = 65_536;
#[derive(PartialEq, Eq)]
struct StoreStamp {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    modified: Option<std::time::SystemTime>,
    len: u64,
}
impl StoreStamp {
    fn new(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            #[cfg(unix)]
            dev: meta.dev(),
            #[cfg(unix)]
            ino: meta.ino(),
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}
struct CachedStore {
    path: PathBuf,
    stamp: Option<StoreStamp>,
    values: BTreeMap<String, Secret>,
}
static STORE_CACHE: Mutex<Option<CachedStore>> = Mutex::new(None);
fn invalidate() {
    *STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[derive(Debug, Subcommand)]
pub(crate) enum SecretCommand {
    /// Store a value read with echo off, or from one stdin line.
    Set {
        name: String,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        force: bool,
    },
    /// List names only; values are never printed.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove a stored credential.
    Rm {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

pub(crate) fn configure_prefer_store(prefer: bool) {
    PREFER_STORE.store(prefer, Ordering::Relaxed);
}
fn prefer_store() -> bool {
    PREFER_STORE.load(Ordering::Relaxed)
        || std::env::var("TOLLGATE_PREFER_STORE").as_deref() == Ok("1")
}
fn validate_name(name: &str) -> Result<()> {
    if name.contains('=') {
        return Err(crate::usage_error(
            "tollgate: pass only the NAME; the value is prompted with echo off",
        ));
    }
    crate::usage::monitor::config::validate_env_name(name)
        .map_err(|e| crate::usage_error(e.to_string()))?;
    if !crate::providers::billing_key::valid_env_name(name) {
        return Err(crate::usage_error(
            "that variable belongs to the process or a managed profile",
        ));
    }
    Ok(())
}
fn validate_value(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        bail!(
            "secret values must be nonempty, at most 4096 bytes, and contain no whitespace or control characters"
        );
    }
    Ok(())
}
fn unsafe_store(reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "tollgate: ~/.tollgate/secrets.env is not private ({reason}); refusing to load it. Fix: chmod 600 ~/.tollgate/secrets.env"
    )
}
#[cfg(unix)]
fn check_metadata(meta: &std::fs::Metadata, directory: bool) -> Result<()> {
    // Metadata only: /proc/self belongs to this effective process on Linux;
    // geteuid is read-only and cannot invalidate any Rust reference.
    #[allow(unsafe_code)]
    let uid = unsafe { libc::geteuid() };
    check_metadata_for_uid(meta, directory, uid)
}
#[cfg(unix)]
fn check_metadata_for_uid(meta: &std::fs::Metadata, directory: bool, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != uid {
        return Err(unsafe_store("owner"));
    }
    if meta.mode() & if directory { 0o022 } else { 0o077 } != 0 {
        return Err(unsafe_store("mode"));
    }
    if !directory && (!meta.is_file() || meta.nlink() != 1) {
        return Err(unsafe_store("links"));
    }
    Ok(())
}
fn load() -> Result<BTreeMap<String, Secret>> {
    load_entries(true)
}
fn load_entries(include_values: bool) -> Result<BTreeMap<String, Secret>> {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return Ok(BTreeMap::new());
    }
    let dir = crate::profile::tollgate_dir()?;
    let Some(file) = open_store(&dir)? else {
        return Ok(BTreeMap::new());
    };
    parse_store(file, include_values)
}
fn parse_store(file: std::fs::File, include_values: bool) -> Result<BTreeMap<String, Secret>> {
    let meta = file.metadata()?;
    if meta.len() > MAX_FILE {
        bail!("secret store unreadable: file exceeds 64 KiB");
    }
    let mut raw = Zeroizing::new(String::new());
    file.take(MAX_FILE + 1).read_to_string(&mut raw)?;
    if raw.len() as u64 > MAX_FILE {
        bail!("secret store unreadable: file exceeds 64 KiB");
    }
    let mut values = BTreeMap::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            bail!("secret store unreadable: invalid line");
        };
        if validate_name(name).is_err()
            || validate_value(value).is_err()
            || values.contains_key(name)
        {
            bail!("secret store unreadable: invalid entry");
        }
        values.insert(
            name.to_owned(),
            Secret::new(if include_values { value } else { "" }),
        );
        if values.len() > 64 {
            bail!("secret store unreadable: more than 64 names");
        }
    }
    Ok(values)
}
// Open the directory itself without following symlinks, then resolve the store
// relative to that descriptor. Renaming the directory cannot redirect the read.
fn open_directory(dir: &Path) -> Result<Option<std::fs::File>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC);
    }
    let file = match options.open(dir) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            return Err(unsafe_store("symlink"));
        }
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    check_metadata(&file.metadata()?, true)?;
    Ok(Some(file))
}
#[cfg(unix)]
#[allow(unsafe_code)]
fn open_at(directory: &std::fs::File, name: &std::ffi::CStr, flags: i32) -> Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: the directory fd and NUL-terminated name remain alive; on success
    // openat returns a fresh owned descriptor, transferred exactly once to File.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ELOOP) {
            return Err(unsafe_store("symlink"));
        }
        return Err(error.into());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
fn open_store(dir: &Path) -> Result<Option<std::fs::File>> {
    let Some(directory) = open_directory(dir)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    let opened = open_at(&directory, c"secrets.env", libc::O_RDONLY);
    #[cfg(not(unix))]
    let opened: Result<std::fs::File> =
        std::fs::File::open(dir.join("secrets.env")).map_err(Into::into);
    let file = match opened {
        Ok(f) => f,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    check_metadata(&file.metadata()?, false)?;
    Ok(Some(file))
}
fn refresh_cache(cache: &mut Option<CachedStore>, path: PathBuf) -> Result<()> {
    let file = open_store(&path)?;
    let stamp = file
        .as_ref()
        .map(|f| f.metadata().map(|m| StoreStamp::new(&m)))
        .transpose()?;
    if cache
        .as_ref()
        .is_some_and(|c| c.path == path && c.stamp == stamp)
    {
        return Ok(());
    }
    let values = file
        .map(|f| parse_store(f, true))
        .transpose()?
        .unwrap_or_default();
    *cache = Some(CachedStore {
        path,
        stamp,
        values,
    });
    Ok(())
}
/// The daemon scans metadata every ten seconds; unchanged files are not read.
pub(crate) fn reload_if_changed() {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return;
    }
    let mut cache = STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let result = crate::profile::tollgate_dir().and_then(|path| refresh_cache(&mut cache, path));
    if let Err(e) = result {
        *cache = None;
        warn_load(&e);
    }
}
fn warn_load(e: &anyhow::Error) {
    if !LOAD_WARNING.swap(true, Ordering::Relaxed) {
        crate::logline::logline!("{e}");
    }
}
fn cached_value(name: &str) -> Option<String> {
    #[cfg(test)]
    if !crate::profile::home_override_active() {
        return None;
    }
    let path = crate::profile::tollgate_dir().ok()?;
    let mut cache = STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if cache.as_ref().is_none_or(|c| c.path != path)
        && let Err(e) = refresh_cache(&mut cache, path)
    {
        *cache = None;
        warn_load(&e);
        return None;
    }
    cache
        .as_ref()?
        .values
        .get(name)
        .map(|s| s.expose().to_owned())
}
fn load_for_fetch(include_values: bool) -> BTreeMap<String, Secret> {
    load_entries(include_values).unwrap_or_else(|e| {
        if !LOAD_WARNING.swap(true, Ordering::Relaxed) {
            crate::logline::logline!("{e}");
        }
        BTreeMap::new()
    })
}
pub(crate) fn stored_names() -> Vec<String> {
    load_for_fetch(false).into_keys().collect()
}
pub(crate) fn resolve(name: &str) -> Option<String> {
    let env = std::env::var(name)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned());
    if !prefer_store() && env.is_some() {
        return env;
    }
    cached_value(name).or(env)
}
fn edit<T>(f: impl FnOnce(&mut BTreeMap<String, Secret>) -> Result<T>) -> Result<T> {
    let dir = crate::profile::tollgate_dir()?;
    crate::profile::mkdir_700(&dir)?;
    let directory =
        open_directory(&dir)?.ok_or_else(|| anyhow::anyhow!("secret directory disappeared"))?;
    // Validate before creating the lock, and pin the lock, read and write to
    // this directory so a path swap cannot redirect a credential write.
    let read_current = || -> Result<BTreeMap<String, Secret>> {
        #[cfg(unix)]
        let opened = open_at(&directory, c"secrets.env", libc::O_RDONLY);
        #[cfg(not(unix))]
        let opened: Result<std::fs::File> =
            std::fs::File::open(dir.join("secrets.env")).map_err(Into::into);
        match opened {
            Ok(file) => {
                #[cfg(unix)]
                check_metadata(&file.metadata()?, false)?;
                parse_store(file, true)
            }
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(BTreeMap::new())
            }
            Err(e) => Err(e),
        }
    };
    read_current()?;
    #[cfg(unix)]
    let lock = open_at(&directory, c".secrets.lock", libc::O_RDWR | libc::O_CREAT)?;
    #[cfg(not(unix))]
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".secrets.lock"))?;
    #[cfg(unix)]
    check_metadata(&lock.metadata()?, false)?;
    crate::lock::lock_file_with_timeout(&lock, std::time::Duration::from_secs(5))?;
    let mut values = read_current()?;
    let result = f(&mut values)?;
    if values.len() > 64 {
        bail!("secret store allows at most 64 names");
    }
    let mut body = Zeroizing::new(String::from(
        "# tollgate secrets v1 — managed by `tollgate secret`; values are never printed\n",
    ));
    for (name, value) in values {
        body.push_str(&name);
        body.push('=');
        body.push_str(value.expose());
        body.push('\n');
    }
    if body.len() as u64 > MAX_FILE {
        bail!("secret store exceeds 64 KiB");
    }
    #[cfg(target_os = "linux")]
    let write_path = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join("secrets.env")
    };
    #[cfg(not(target_os = "linux"))]
    let write_path = dir.join("secrets.env");
    crate::profile::atomic_write_600(&write_path, body.as_bytes())?;
    invalidate();
    Ok(result)
}
fn users(name: &str) -> Vec<String> {
    crate::usage::monitor::config::load()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.api_key_env.as_deref() == Some(name) || m.billing_key_env.as_deref() == Some(name)
        })
        .map(|m| m.id)
        .collect()
}
fn confirm(message: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{message}; pass --yes on non-terminal stdin");
    }
    errln!("{message} [y/N]");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}
fn prefix(value: &str) -> &'static str {
    for p in ["sk-or-v1-", "sk-proj-", "sk-admin-", "sk-", "AIza"] {
        if value.starts_with(p) {
            return p;
        }
    }
    ""
}
fn stored_confirmation(name: &str, value: &str) -> String {
    let vendor = prefix(value);
    let suffix = if vendor.is_empty() {
        String::new()
    } else {
        format!(", {vendor}…")
    };
    format!(
        "tollgate: stored {name} ({} chars{suffix}) in ~/.tollgate/secrets.env; the daemon picks it up within 10s",
        value.chars().count()
    )
}
fn list_output(json: bool) -> Result<String> {
    let values = load_entries(false)?;
    if json {
        Ok(serde_json::to_string(&values.keys().collect::<Vec<_>>())?)
    } else {
        let mut lines = Vec::new();
        for name in values.keys() {
            let shadow = std::env::var(name).is_ok_and(|s| !s.trim().is_empty());
            let origin = if shadow {
                if prefer_store() {
                    "store+env (store wins)"
                } else {
                    "store+env (env wins)"
                }
            } else {
                "store"
            };
            let ids = users(name);
            lines.push(format!(
                "{name}  {origin}  used by: {}",
                if ids.is_empty() {
                    "nothing".to_owned()
                } else {
                    ids.join(", ")
                }
            ));
        }
        Ok(lines.join("\n"))
    }
}
pub(crate) fn dispatch(command: SecretCommand) -> Result<()> {
    match command {
        SecretCommand::Set { name, stdin, force } => {
            validate_name(&name)?;
            if !stdin && !std::io::stdin().is_terminal() {
                return Err(crate::usage_error(
                    "tollgate: no terminal to prompt on; pipe the value with --stdin",
                ));
            }
            let mut replace = force;
            if load()?.contains_key(&name) && !force {
                if !std::io::stdin().is_terminal() {
                    bail!("tollgate: {name} is already stored; pass --force to replace");
                }
                if !confirm(&format!("tollgate: {name} is already stored; replace it?"))? {
                    return Ok(());
                }
                replace = true;
            }
            let value = if stdin {
                let mut bytes = Zeroizing::new(Vec::new());
                std::io::stdin()
                    .lock()
                    .take(4099)
                    .read_until(b'\n', &mut bytes)?;
                if bytes.last() == Some(&b'\n') {
                    bytes.pop();
                    if bytes.last() == Some(&b'\r') {
                        bytes.pop();
                    }
                }
                Zeroizing::new(
                    std::str::from_utf8(&bytes)
                        .map_err(|_| anyhow::anyhow!("secret must be UTF-8"))?
                        .to_owned(),
                )
            } else {
                Zeroizing::new(rpassword::prompt_password(format!(
                    "Value for {name} (input hidden): "
                ))?)
            };
            validate_value(&value)?;
            edit(|values| {
                if values.contains_key(&name) && !replace {
                    bail!("tollgate: {name} is already stored; pass --force to replace");
                }
                values.insert(name.clone(), Secret::new(value.as_str()));
                Ok(())
            })?;
            outln!("{}", stored_confirmation(&name, &value));
            if std::env::var(&name).is_ok_and(|v| !v.trim().is_empty()) {
                errln!(
                    "note: ${name} is also set in this environment; tollgate processes started from it use that value (--prefer-store uses the stored one)"
                );
            }
        }
        SecretCommand::List { json } => {
            let text = list_output(json)?;
            if !text.is_empty() {
                outln!("{text}");
            }
        }
        SecretCommand::Rm { name, yes } => {
            validate_name(&name)?;
            load()?;
            if !yes && !confirm(&format!("tollgate: remove {name}?"))? {
                return Ok(());
            }
            edit(|values| {
                if values.remove(&name).is_none() {
                    bail!("tollgate: {name} is not stored");
                }
                Ok(())
            })?;
            let ids = users(&name);
            let suffix = if ids.is_empty() {
                String::new()
            } else {
                format!("; monitor(s) {} will report it missing", ids.join(", "))
            };
            outln!("tollgate: removed {name}{suffix}");
        }
    }
    Ok(())
}
#[cfg(test)]
#[path = "../tests/inline/secrets.rs"]
mod tests;
