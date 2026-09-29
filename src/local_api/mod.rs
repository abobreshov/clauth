//! The local agent API: a read-only HTTP/1.1 JSON API on loopback, plus the
//! same routes on a unix socket, for agents on this machine to read what
//! tollgate observes (plan v3.1 §4.1's `accounts[]`, the status feed, the
//! provider catalog).
//!
//! Shape, and why:
//!   * **Loopback only.** [`parse_listen`] refuses any address that is not
//!     127.0.0.0/8 or `::1`. There is no TLS here, so a LAN bind would put the
//!     bearer on the wire; the TLS REST API (`daemon --listen`) is the remote
//!     surface.
//!   * **Two doors.** TCP needs `Authorization: Bearer <token>`, the token
//!     living in `~/.tollgate/api-token` (0600, generated on first use), and a
//!     loopback `Host` (`localhost`, `127.0.0.1`, `[::1]`, any port): a
//!     DNS-rebound browser page carries its own name and is refused 421 before
//!     the token is looked at. The unix socket `~/.tollgate/api.sock` (0600,
//!     inside the data dir, which is verified ours and tightened to 0700
//!     before the bind) needs no token: reaching it already proves the caller
//!     is this user.
//!   * **Read-only.** Every route is a `GET`; anything else answers 405. The
//!     handlers read caches through [`crate::usage::collect::collect`] and never
//!     fetch, so an agent polling this spends no provider quota, and they write
//!     nothing: the roster loads through
//!     [`crate::profile::load_config_read_only`], so a poll leaves the data dir
//!     byte-identical.
//!   * **No framework.** The daemon's own HTTP/1.1 reader and writer
//!     ([`crate::daemon::api::http`]) are generic over the stream, so they run
//!     here over a plain `TcpStream` / `UnixStream` with every size cap and
//!     framing rule they already enforce. No CORS headers are ever sent: this
//!     is not for browsers.
//!
//! Hosted by `tollgate daemon` when `local_api.enabled` (the default), or run
//! alone by `tollgate api serve`.

pub(crate) mod routes;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use subtle::ConstantTimeEq;

use crate::daemon::api::http::{self, Disposition, RequestError, RequestReader};
use crate::logline::logline;
use crate::out::{errln, outln};

/// Where the API listens unless `local_api.listen` says otherwise.
pub(crate) const DEFAULT_LISTEN: &str = "127.0.0.1:8454";
/// `~/.tollgate/api-token`: the TCP bearer.
pub(crate) const TOKEN_FILE: &str = "api-token";
/// `~/.tollgate/api.sock`: the token-less unix door.
pub(crate) const SOCKET_FILE: &str = "api.sock";
/// Opt-out for the daemon-hosted listener without editing `profiles.toml`
/// (only `"1"` opts out), matching `TOLLGATE_NO_API` for the TLS API.
pub(crate) const NO_LOCAL_API_ENV: &str = "TOLLGATE_NO_LOCAL_API";

/// Concurrent connections, both doors together. Agents poll; they do not
/// hold dozens of sockets open.
const MAX_CONNECTIONS: usize = 16;
/// One read or write may block this long.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// A kept-alive connection lives at most this long, idle time included.
const LIFETIME: Duration = Duration::from_secs(60);
/// Requests served on one connection before it is closed.
const MAX_REQUESTS: u32 = 100;
/// Pause after an accept error nothing can be attributed to, so a
/// persistent failure cannot spin a core.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The `[local_api]` table of `profiles.toml`
/// (`local_api = { enabled = true, listen = "127.0.0.1:8454" }`). Absent or
/// partial fills from [`Default`]. `listen` stays a string so a bad value is
/// reported when the listener starts rather than failing every config load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct LocalApiSettings {
    /// Whether `tollgate daemon` hosts the API (default on).
    pub(crate) enabled: bool,
    /// The loopback `ip:port` to bind.
    pub(crate) listen: String,
}

impl Default for LocalApiSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: DEFAULT_LISTEN.to_string(),
        }
    }
}

/// The saved settings, or the defaults when the config does not load.
fn saved_settings() -> LocalApiSettings {
    crate::profile::load_config()
        .map(|c| c.state.local_api)
        .unwrap_or_default()
}

/// Parse a listen address and refuse anything that is not loopback.
/// `localhost:<port>` is accepted as `127.0.0.1:<port>`; no other name is
/// resolved, so what is bound is exactly what was written.
pub(crate) fn parse_listen(raw: &str) -> Result<SocketAddr> {
    let raw = raw.trim();
    let addr: SocketAddr = match raw.strip_prefix("localhost:") {
        Some(port) => SocketAddr::from((
            [127, 0, 0, 1],
            port.parse::<u16>()
                .with_context(|| format!("invalid port in local API address {raw:?}"))?,
        )),
        None => raw.parse().with_context(|| {
            format!("invalid local API address {raw:?}: expected ip:port, e.g. {DEFAULT_LISTEN}")
        })?,
    };
    if !addr.ip().is_loopback() {
        bail!(
            "refusing to serve the local API on {addr}: only a loopback address (127.0.0.1, \
             ::1) is allowed. It has no TLS and is for agents on this machine; use \
             `tollgate daemon --listen` for the remote REST API"
        );
    }
    Ok(addr)
}

// ── The token ─────────────────────────────────────────────────────────────────

/// `~/.tollgate/api-token`.
pub(crate) fn token_path() -> Result<PathBuf> {
    Ok(crate::profile::tollgate_dir()?.join(TOKEN_FILE))
}

/// `~/.tollgate/api.sock`.
pub(crate) fn socket_path() -> Result<PathBuf> {
    Ok(crate::profile::tollgate_dir()?.join(SOCKET_FILE))
}

/// The exact shape [`generate_token`] emits: 64 lowercase hex characters.
fn well_formed(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn generate_token() -> Result<String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("CSPRNG failure: {e}"))?;
    Ok(hex::encode(seed))
}

/// The token on disk, when there is a well-formed one.
fn read_token(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let token = raw.trim();
    well_formed(token).then(|| token.to_string())
}

/// The token, generating `~/.tollgate/api-token` (0600) on first use.
///
/// The publish is no-clobber (a hard link of a fully written 0600 temp), so two
/// processes starting at once agree on one token: the loser's link fails and
/// it reads the winner's. A malformed file is replaced outright. Whatever is
/// on disk afterwards is the answer.
pub(crate) fn ensure_token() -> Result<String> {
    let path = token_path()?;
    if let Some(token) = read_token(&path) {
        tighten_600(&path);
        return Ok(token);
    }
    let dir = path.parent().context("token path has no parent")?;
    crate::profile::mkdir_700(dir).context("failed to create ~/.tollgate")?;
    let tmp = dir.join(format!(".{TOKEN_FILE}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    write_new_600(&tmp, format!("{}\n", generate_token()?).as_bytes())
        .context("failed to write the API token")?;
    let published = if path.exists() {
        // Present but malformed: replace it.
        std::fs::rename(&tmp, &path)
    } else {
        match std::fs::hard_link(&tmp, &path) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
            _ => Ok(()),
        }
    };
    let _ = std::fs::remove_file(&tmp);
    published.with_context(|| format!("failed to publish {}", path.display()))?;
    read_token(&path).with_context(|| format!("{} is not a usable token", path.display()))
}

/// Create `path` (which must not exist) at 0600 and write `bytes`.
fn write_new_600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Owner-only, for a file an older tool or a hand edit left looser.
fn tighten_600(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path)
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Whether `presented` is the token in `path`. Read per request, so deleting
/// the file and running `tollgate api token` rotates it without a restart.
/// Both sides are hashed before the constant-time compare, so neither the
/// token's length nor the first wrong byte shows in the timing.
pub(crate) fn token_matches(path: &Path, presented: Option<&str>) -> bool {
    let (Some(presented), Some(stored)) = (presented, read_token(path)) else {
        return false;
    };
    let a = <[u8; 32]>::from(sha2::Sha256::digest(presented.as_bytes()));
    let b = <[u8; 32]>::from(sha2::Sha256::digest(stored.as_bytes()));
    bool::from(a.ct_eq(&b))
}

// ── The server ────────────────────────────────────────────────────────────────

static LIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Releases a [`LIVE_CONNECTIONS`] slot however the connection thread ends.
struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        LIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn claim_slot() -> Option<Slot> {
    LIVE_CONNECTIONS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
            (live < MAX_CONNECTIONS).then_some(live + 1)
        })
        .ok()
        .map(|_| Slot)
}

/// Which door a connection came through. Only [`Door::Unix`] skips the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Door {
    Tcp,
    #[cfg_attr(not(unix), allow(dead_code))] // no socket door off unix
    Unix,
}

/// What to start.
pub(crate) struct StartOpts {
    /// The loopback address; `None` serves the unix socket only.
    pub(crate) listen: Option<SocketAddr>,
    /// Serve `~/.tollgate/api.sock` too (unix only; ignored elsewhere).
    pub(crate) unix_socket: bool,
    /// The status feed `/v1/status` passes through.
    pub(crate) status_path: PathBuf,
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct SocketIdentity {
    path: (u64, u64),
    listener: (u64, u64),
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn listener_identity(listener: &std::os::unix::net::UnixListener) -> std::io::Result<(u64, u64)> {
    use std::os::fd::AsRawFd;

    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(listener.as_raw_fd(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

#[cfg(unix)]
fn socket_path_matches(path: &Path, identity: (u64, u64)) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    path.symlink_metadata().is_ok_and(|metadata| {
        metadata.file_type().is_socket() && (metadata.dev(), metadata.ino()) == identity
    })
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn stop_unix_accept(listener: &std::os::unix::net::UnixListener) {
    use std::os::fd::AsRawFd;

    let _ = unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RD) };
}

/// A running API. Dropping it stops accepting (connections in flight finish
/// on their own timeouts) and removes the socket file it created.
pub(crate) struct Server {
    tcp: Option<SocketAddr>,
    socket: Option<PathBuf>,
    #[cfg(unix)]
    socket_identity: Option<SocketIdentity>,
    #[cfg(unix)]
    socket_listener: Option<Arc<std::os::unix::net::UnixListener>>,
    socket_error: Option<String>,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// The bound TCP address (the real port when `:0` was asked for).
    pub(crate) fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp
    }

    /// The unix socket path, when one is served.
    pub(crate) fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// Why the unix socket was asked for but is not served (TCP still is).
    pub(crate) fn socket_error(&self) -> Option<&str> {
        self.socket_error.as_deref()
    }

    #[cfg(unix)]
    pub(crate) fn cleanup_socket(&self) {
        self.stop.store(true, Ordering::Release);
        let (Some(path), Some(identity), Some(listener)) =
            (&self.socket, self.socket_identity, &self.socket_listener)
        else {
            return;
        };
        if listener_identity(listener).ok() == Some(identity.listener)
            && socket_path_matches(path, identity.path)
        {
            let _ = std::fs::remove_file(path);
        }
        stop_unix_accept(listener);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake each blocked accept so it sees the flag.
        if let Some(addr) = self.tcp {
            let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
        }
        #[cfg(unix)]
        self.cleanup_socket();
    }
}

/// Bind and serve. The token is ensured first, so a TCP caller always has a
/// file to read it from.
///
/// Everything is bound before anything is spawned, so a failed start leaves no
/// thread serving. A TCP bind failure is an error. The unix socket is the
/// secondary door: when TCP is served, a socket that cannot be bound (a path
/// over `SUN_LEN`, another live server already answering on it, which is never
/// stolen) is reported through [`Server::socket_error`] and the API runs on TCP
/// alone; serving the socket only, it is an error. A stale socket left by a
/// killed process is replaced.
pub(crate) fn start(opts: StartOpts) -> Result<Server> {
    ensure_token()?;
    let ctx = Arc::new(routes::Ctx {
        token_path: token_path()?,
        status_path: opts.status_path,
    });

    let tcp_listener = match opts.listen {
        Some(addr) => {
            // Re-checked here so no caller can bypass the loopback rule.
            let addr = parse_listen(&addr.to_string())?;
            Some(TcpListener::bind(addr).with_context(|| {
                format!(
                    "failed to bind the local API to {addr} (is `tollgate daemon` already \
                     serving it?)"
                )
            })?)
        }
        None => None,
    };
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut socket_error = None;
    #[cfg(unix)]
    let unix_listener = if opts.unix_socket {
        let path = socket_path()?;
        match bind_unix(&path) {
            Ok((listener, identity)) => Some((listener, path, identity)),
            Err(e) if tcp_listener.is_some() => {
                socket_error = Some(format!("{e:#}"));
                None
            }
            Err(e) => return Err(e),
        }
    } else {
        None
    };

    let stop = Arc::new(AtomicBool::new(false));
    let mut server = Server {
        tcp: None,
        socket: None,
        #[cfg(unix)]
        socket_identity: None,
        #[cfg(unix)]
        socket_listener: None,
        socket_error,
        stop: Arc::clone(&stop),
    };
    if let Some(listener) = tcp_listener {
        server.tcp = Some(listener.local_addr()?);
        let (ctx, stop) = (Arc::clone(&ctx), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("tollgate-local-api-tcp".into())
            .spawn(move || accept_tcp(&listener, &ctx, &stop))
            .context("failed to spawn the local API accept thread")?;
    }
    #[cfg(unix)]
    if let Some((listener, path, identity)) = unix_listener {
        // Recorded before the spawn so a failed spawn's drop removes the node.
        server.socket = Some(path);
        server.socket_identity = Some(identity);
        let listener = Arc::new(listener);
        server.socket_listener = Some(Arc::clone(&listener));
        let (ctx, stop) = (Arc::clone(&ctx), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("tollgate-local-api-unix".into())
            .spawn(move || accept_unix(&listener, &ctx, &stop))
            .context("failed to spawn the local API socket thread")?;
    }
    Ok(server)
}

/// This process's effective uid.
#[cfg(unix)]
#[allow(unsafe_code)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// Make `dir` a directory only this uid can enter, or refuse: created 0700
/// when absent; a symlink, a non-directory or another uid's directory is an
/// error (the socket inside would live where someone else decides); an owned
/// directory with group/other bits is tightened to 0700.
///
/// This is what makes the bind below safe without touching the process-wide
/// umask (which another thread could be relying on): between `bind` and the
/// `chmod` the node carries umask bits, and a connect needs search permission
/// on every directory above it, which a 0700 dir owned by this uid gives no one
/// else.
#[cfg(unix)]
pub(crate) fn secure_socket_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    crate::profile::mkdir_700(dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let meta = dir
        .symlink_metadata()
        .with_context(|| format!("failed to inspect {}", dir.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "refusing to serve the local API socket: {} is a symlink",
            dir.display()
        );
    }
    if !meta.is_dir() {
        bail!(
            "refusing to serve the local API socket: {} is not a directory",
            dir.display()
        );
    }
    if meta.uid() != effective_uid() {
        bail!(
            "refusing to serve the local API socket: {} is owned by uid {}, not this user",
            dir.display(),
            meta.uid()
        );
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to restrict {} to 0700", dir.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn bind_unix(path: &Path) -> Result<(std::os::unix::net::UnixListener, SocketIdentity)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = path.parent().context("the socket path has no parent")?;
    secure_socket_dir(dir)?;
    // `symlink_metadata`, not `exists`: a dangling link at the node is still a
    // node `bind` would trip over, so it is replaced like a stale socket.
    if path.symlink_metadata().is_ok() {
        if UnixStream::connect(path).is_ok() {
            bail!(
                "another local API already answers on {} (is `tollgate daemon` running?)",
                path.display()
            );
        }
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove the stale socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("failed to bind the local API socket {}", path.display()))?;
    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("failed to inspect bound socket {}", path.display()))?;
    let path_identity = (metadata.dev(), metadata.ino());
    let prepared = (|| {
        if !metadata.file_type().is_socket() {
            bail!("{} is no longer the bound socket", path.display());
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict {}", path.display()))?;
        let listener_identity = listener_identity(&listener)
            .with_context(|| format!("failed to inspect bound listener for {}", path.display()))?;
        if !socket_path_matches(path, path_identity) {
            bail!("{} changed while binding the socket", path.display());
        }
        Ok(SocketIdentity {
            path: path_identity,
            listener: listener_identity,
        })
    })();
    match prepared {
        Ok(identity) => Ok((listener, identity)),
        Err(error) => {
            if socket_path_matches(path, path_identity) {
                let _ = std::fs::remove_file(path);
            }
            Err(error)
        }
    }
}

fn accept_tcp(listener: &TcpListener, ctx: &Arc<routes::Ctx>, stop: &AtomicBool) {
    loop {
        let accepted = listener.accept();
        if stop.load(Ordering::Acquire) {
            return;
        }
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                logline!("tollgate local api: accept failed: {e}");
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        // Loopback-bound, so this cannot fail today; kept so a future bind
        // change cannot quietly open the door.
        if !peer.ip().is_loopback() {
            continue;
        }
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_nodelay(true);
        spawn_connection(stream, Door::Tcp, ctx);
    }
}

#[cfg(unix)]
fn accept_unix(
    listener: &std::os::unix::net::UnixListener,
    ctx: &Arc<routes::Ctx>,
    stop: &AtomicBool,
) {
    loop {
        let accepted = listener.accept();
        if stop.load(Ordering::Acquire) {
            return;
        }
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(e) => {
                logline!("tollgate local api: socket accept failed: {e}");
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        spawn_connection(stream, Door::Unix, ctx);
    }
}

fn spawn_connection<S: Read + Write + Send + 'static>(
    stream: S,
    door: Door,
    ctx: &Arc<routes::Ctx>,
) {
    // Over the cap: drop the connection unanswered, the cheapest refusal.
    let Some(slot) = claim_slot() else {
        return;
    };
    let ctx = Arc::clone(ctx);
    let spawned = std::thread::Builder::new()
        .name("tollgate-local-api-conn".into())
        .spawn(move || {
            let _slot = slot;
            serve_connection(stream, door, &ctx);
        });
    if let Err(e) = spawned {
        logline!("tollgate local api: failed to spawn a connection thread: {e}");
    }
}

/// One connection: requests in order until the client stops, the budget runs
/// out, or an answer that is not a success closes it.
fn serve_connection<S: Read + Write>(stream: S, door: Door, ctx: &routes::Ctx) {
    let opened = Instant::now();
    let expires = opened + LIFETIME;
    // The first request gets one I/O timeout to arrive, not the whole lifetime.
    let mut reader = RequestReader::new(stream, expires.min(opened + IO_TIMEOUT));
    let mut served: u32 = 0;
    loop {
        let request = match reader.next_request() {
            Ok(None) | Err(RequestError::Io(_)) => break,
            Ok(Some(request)) => request,
            Err(e) => {
                let _ = http::write_response(
                    reader.stream_mut(),
                    e.response(),
                    &Disposition::Close,
                    expires,
                );
                break;
            }
        };
        served = served.saturating_add(1);
        let response = routes::handle(ctx, &request, door);
        // A HEAD answer ends at the blank line whatever it says.
        let response = if request.method == "HEAD" {
            response.into_head()
        } else {
            response
        };
        // Only a success keeps the connection: a caller without the token
        // cannot hold a slot by sending one bad request and going quiet.
        let remaining = expires.saturating_duration_since(Instant::now());
        let disposition = if request.keep_alive
            && (200..300).contains(&response.status)
            && served < MAX_REQUESTS
            && !remaining.is_zero()
        {
            Disposition::KeepAlive {
                timeout_secs: remaining.as_secs(),
                max_requests: MAX_REQUESTS - served,
            }
        } else {
            Disposition::Close
        };
        if http::write_response(reader.stream_mut(), response, &disposition, expires).is_err()
            || matches!(disposition, Disposition::Close)
        {
            break;
        }
        reader.set_deadline(expires);
    }
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// Why the daemon does not host the API, or `None` when it should.
fn daemon_skip_reason(settings: &LocalApiSettings) -> Option<String> {
    if std::env::var(NO_LOCAL_API_ENV).as_deref() == Ok("1") {
        return Some(format!("{NO_LOCAL_API_ENV}=1 is set"));
    }
    (!settings.enabled).then(|| "local_api.enabled is false".to_string())
}

/// `tollgate daemon`'s start of the API. Never fatal: a daemon without its
/// agent API still refreshes and switches, so a taken port or a bad address is
/// logged and the daemon carries on. The returned server must be kept alive.
pub(crate) fn start_in_daemon(settings: &LocalApiSettings, status_path: PathBuf) -> Option<Server> {
    if let Some(reason) = daemon_skip_reason(settings) {
        logline!("tollgate daemon: {reason}; not serving the local agent API");
        return None;
    }
    let started = parse_listen(&settings.listen).and_then(|addr| {
        start(StartOpts {
            listen: Some(addr),
            unix_socket: true,
            status_path,
        })
    });
    match started {
        Ok(server) => {
            logline!("tollgate daemon: {}", listening_line(&server));
            if let Some(e) = server.socket_error() {
                logline!("tollgate daemon: local agent API socket not served: {e}");
            }
            Some(server)
        }
        Err(e) => {
            logline!("tollgate daemon: local agent API not started: {e:#}");
            None
        }
    }
}

fn listening_line(server: &Server) -> String {
    let mut line = String::from("local agent API listening");
    if let Some(addr) = server.tcp_addr() {
        line.push_str(&format!(" on http://{addr}"));
    }
    if let Some(path) = server.socket() {
        line.push_str(&format!(" and {}", path.display()));
    }
    line
}

fn status_path() -> Result<PathBuf> {
    Ok(crate::profile::tollgate_dir()?.join(crate::daemon::STATUS_FILE))
}

/// `tollgate api serve [--listen ADDR]`: serve until a stop signal.
pub(crate) fn cmd_serve(listen: Option<String>) -> Result<()> {
    #[cfg(unix)]
    let watched: Vec<_> = [
        signal_hook::consts::signal::SIGINT,
        signal_hook::consts::signal::SIGTERM,
    ]
    .into_iter()
    .filter(|signal| !crate::daemon::gateway::inherited_ignored(*signal))
    .collect();
    #[cfg(unix)]
    let mut signals = if watched.is_empty() {
        None
    } else {
        Some(
            signal_hook::iterator::Signals::new(watched)
                .context("failed to install local API stop signals")?,
        )
    };
    let raw = listen.unwrap_or_else(|| saved_settings().listen);
    let addr = parse_listen(&raw)?;
    let server = start(StartOpts {
        listen: Some(addr),
        unix_socket: true,
        status_path: status_path()?,
    })?;
    outln!("tollgate: {}", listening_line(&server));
    if let Some(e) = server.socket_error() {
        errln!("tollgate: unix socket not served: {e}");
    }
    outln!("token: {}", token_path()?.display());
    #[cfg(unix)]
    if let Some(signals) = signals.as_mut() {
        let _ = signals.forever().next();
        drop(server);
        return Ok(());
    }
    loop {
        std::thread::park();
    }
}

/// `tollgate api token [--show]`: the token file's path, or with `--show` the
/// token itself alone on one line (for `$(tollgate api token --show)`).
pub(crate) fn cmd_token(show: bool) -> Result<()> {
    let token = ensure_token()?;
    if show {
        outln!("{token}");
    } else {
        outln!("{}", token_path()?.display());
    }
    Ok(())
}

/// `tollgate api url`: the base URL and a working curl line for each door.
pub(crate) fn cmd_url() -> Result<()> {
    let settings = saved_settings();
    let addr = parse_listen(&settings.listen)?;
    outln!(
        "{}",
        url_text(addr, &token_path()?, &socket_path()?, settings.enabled)
    );
    Ok(())
}

/// [`cmd_url`]'s text, pure so it is testable.
pub(crate) fn url_text(addr: SocketAddr, token: &Path, socket: &Path, enabled: bool) -> String {
    let base = format!("http://{addr}");
    let mut out = format!(
        "{base}\n\n\
         curl -s -H \"Authorization: Bearer $(cat {token})\" {base}/v1/usage\n",
        token = token.display(),
    );
    if cfg!(unix) {
        out.push_str(&format!(
            "curl -s --unix-socket {} http://localhost/v1/usage\n",
            socket.display()
        ));
    }
    out.push_str(
        "\nroutes: /v1/health /v1/accounts /v1/accounts/{id} /v1/usage /v1/providers \
         /v1/status /v1/openapi.json",
    );
    if !enabled {
        out.push_str(
            "\n\nnote: local_api.enabled is false, so `tollgate daemon` does not serve this; \
             run `tollgate api serve`",
        );
    }
    out
}

#[cfg(test)]
#[path = "../../tests/inline/local_api.rs"]
mod tests;
