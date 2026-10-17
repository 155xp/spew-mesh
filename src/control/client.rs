//! The typed client of the control socket (6.1, 6.2), for the CLI, MCP servers, responders and tests.
//!
//! [`Client::connect`] finds the daemon through `<home>/run/socket`, starts one when asked to, waits for it, and
//! replaces an older daemon when the caller is newer (CLI and MCP servers only). One connection carries
//! overlapping requests. Long-lived clients watch [`Client::closed`] and reconnect with [`reconnect_delay`].

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, broadcast, oneshot, watch};

use super::place::{current_uid, lock_is_free, read_socket_path, same_user, spawn_daemon};
use super::protocol::{
    self, Ack, AskParams, DeviceList, Directory, DirectoryQuery, Empty, FetchParams, Fetched,
    HookEvent, HookResult, Inbound, LoginResult, LoginUrl, LogoutResult, ReplyParams, RequestId,
    RevokeParams, Revoked, RpcError, SendResult, SessionOpen, SessionOpened, SessionUpdate, Status,
    TakeParams, Taken, TellParams, TokenOnly, method, request_line,
};
use super::server::read_line;
use crate::paths::Home;

/// How long a CLI, MCP server or responder waits for the daemon (6.1).
pub const CONNECT_WAIT: Duration = Duration::from_secs(8);
/// How often it tries (6.1).
pub const CONNECT_EVERY: Duration = Duration::from_millis(100);
/// How long a hook waits (6.1).
pub const HOOK_DEADLINE: Duration = Duration::from_secs(1);
/// How long `daemon.status` may take when checking that a daemon answers.
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
/// A spawned daemon that hasn't taken its lock by now has given up.
const START_GRACE: Duration = Duration::from_secs(3);

/// Why a client call failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No daemon answers and the caller didn't ask to start one.
    #[error("the spew mesh daemon is not running; `spew mesh daemon start` starts it")]
    NotRunning,
    /// A daemon was started but didn't answer in time.
    #[error("the spew mesh daemon did not start{}; see {}", .detail, .log.display())]
    StartFailed {
        /// What the log said, when anything.
        detail: String,
        /// The daemon log.
        log: PathBuf,
    },
    /// The socket belongs to another user (6.1).
    #[error("the control socket {} belongs to another user; refusing to use it", .0.display())]
    OtherUser(PathBuf),
    /// The connection closed before the answer came.
    #[error("lost the connection to the spew mesh daemon")]
    Disconnected,
    /// The daemon answered with an error.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Starting or reaching the daemon failed.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The answer didn't have the expected shape.
    #[error("unexpected answer from the daemon: {0}")]
    Protocol(String),
}

impl ClientError {
    /// The daemon's error, when it sent one.
    pub fn rpc(&self) -> Option<&RpcError> {
        match self {
            ClientError::Rpc(e) => Some(e),
            _ => None,
        }
    }
}

/// How [`Client::connect`] finds the daemon.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Start a daemon when none answers.
    pub start: bool,
    /// How long to wait for one to answer.
    pub wait: Duration,
    /// Replace an older daemon with this build's (CLI and MCP servers; never hooks or responders, 6.1).
    pub upgrade: bool,
    /// The binary to start; `None` is this process's own executable (never a PATH lookup).
    pub exe: Option<PathBuf>,
}

impl ConnectOptions {
    /// What the CLI and MCP servers use: start, wait 8 s, replace an older daemon.
    pub fn cli() -> ConnectOptions {
        ConnectOptions {
            start: true,
            wait: CONNECT_WAIT,
            upgrade: true,
            exe: None,
        }
    }

    /// What responders use: start and wait, but never replace a daemon.
    pub fn responder() -> ConnectOptions {
        ConnectOptions {
            upgrade: false,
            ..ConnectOptions::cli()
        }
    }

    /// Only connect to a running daemon.
    pub fn existing() -> ConnectOptions {
        ConnectOptions {
            start: false,
            wait: Duration::ZERO,
            upgrade: false,
            exe: None,
        }
    }
}

type Pending = std::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>;

struct Inner {
    writer: Mutex<OwnedWriteHalf>,
    pending: Arc<Pending>,
    next_id: AtomicI64,
    notes: broadcast::Sender<(String, Value)>,
    closed: watch::Receiver<bool>,
    reader: tokio::task::JoinHandle<()>,
    socket: PathBuf,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// A connection to the daemon. Cheap to clone; every clone shares the connection.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("socket", &self.inner.socket)
            .finish_non_exhaustive()
    }
}

/// This build's version.
pub fn own_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// Whether version `a` is newer than `b`. Unparseable versions are never newer, so nothing restarts on a guess.
pub fn is_newer(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The wait before reconnect attempt `attempt` (0-based): 500 ms doubling to 20 s, with ±20% jitter (6.1).
pub fn reconnect_delay(attempt: u32) -> Duration {
    let base = 500u64.saturating_mul(1u64 << attempt.min(6)).min(20_000);
    let mut b = [0u8; 2];
    // A jitter of 0..=400 per mille of the base, centred: -20% to +20%.
    let per_mille = if getrandom::fill(&mut b).is_ok() {
        u64::from(u16::from_le_bytes(b)) % 401
    } else {
        200
    };
    Duration::from_millis(base * (800 + per_mille) / 1000)
}

fn log_len(log: &Path) -> u64 {
    std::fs::metadata(log).map(|m| m.len()).unwrap_or(0)
}

fn log_tail(log: &Path, from: u64) -> String {
    let Ok(bytes) = std::fs::read(log) else {
        return String::new();
    };
    let start = usize::try_from(from).unwrap_or(0).min(bytes.len());
    let text = String::from_utf8_lossy(&bytes[start..]);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(3)..];
    if tail.is_empty() {
        String::new()
    } else {
        format!(": {}", crate::text::sanitize_line(&tail.join(" / "), 600))
    }
}

impl Client {
    /// Connects to the socket at `path`, refusing one owned by another user or served by one (6.1).
    pub async fn connect_path(path: &Path) -> Result<Client, ClientError> {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.uid() != current_uid() {
            return Err(ClientError::OtherUser(path.to_path_buf()));
        }
        let stream = UnixStream::connect(path).await?;
        if !same_user(&stream)? {
            return Err(ClientError::OtherUser(path.to_path_buf()));
        }
        let (read, write) = stream.into_split();
        let pending: Arc<Pending> = Arc::new(Default::default());
        let (notes, _) = broadcast::channel(16);
        let (closed_tx, closed) = watch::channel(false);
        let reader = {
            let pending = pending.clone();
            let notes = notes.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(read);
                while let Ok(Some(line)) = read_line(&mut reader).await {
                    match protocol::parse_line(&line) {
                        Ok(Inbound::Response {
                            id: RequestId::Number(id),
                            outcome,
                        }) => {
                            let waiter = lock(&pending).remove(&id);
                            if let Some(waiter) = waiter {
                                let _ = waiter.send(outcome);
                            }
                        }
                        Ok(Inbound::Notification { method, params }) => {
                            let _ = notes.send((method, params));
                        }
                        _ => {}
                    }
                }
                let _ = closed_tx.send(true);
                // Dropping the senders tells every waiter the connection is gone.
                lock(&pending).clear();
            })
        };
        Ok(Client {
            inner: Arc::new(Inner {
                writer: Mutex::new(write),
                pending,
                next_id: AtomicI64::new(1),
                notes,
                closed,
                reader,
                socket: path.to_path_buf(),
            }),
        })
    }

    /// Finds the daemon for `home`, starting one when `opts.start` is set, and checks it answers (6.1).
    pub async fn connect(home: &Home, opts: &ConnectOptions) -> Result<Client, ClientError> {
        let deadline = Instant::now() + opts.wait;
        let mut spawned: Option<(Instant, u64)> = None;
        let mut upgraded = false;
        loop {
            if let Some(client) = Client::try_existing(home).await? {
                match client.status_within(STATUS_TIMEOUT).await {
                    Ok(status) => {
                        if opts.upgrade && !upgraded && is_newer(own_version(), &status.version) {
                            tracing::info!(daemon = %status.version, own = own_version(), "replacing an older daemon");
                            let _ = client.shutdown().await;
                            upgraded = true;
                            spawned = None;
                            wait_for_exit(home, deadline.max(Instant::now() + CONNECT_WAIT)).await;
                            continue;
                        }
                        return Ok(client);
                    }
                    Err(ClientError::Rpc(e)) => return Err(ClientError::Rpc(e)),
                    Err(_) => {}
                }
            }
            if !opts.start {
                return Err(ClientError::NotRunning);
            }
            let log = home.log_file();
            match spawned {
                None if lock_is_free(home) => {
                    let exe = match &opts.exe {
                        Some(exe) => exe.clone(),
                        None => std::env::current_exe()?,
                    };
                    let from = log_len(&log);
                    spawn_daemon(home, &exe)?;
                    spawned = Some((Instant::now(), from));
                }
                Some((at, from)) if at.elapsed() > START_GRACE && lock_is_free(home) => {
                    return Err(ClientError::StartFailed {
                        detail: log_tail(&log, from),
                        log,
                    });
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                let detail = spawned
                    .map(|(_, from)| log_tail(&log, from))
                    .unwrap_or_default();
                return Err(ClientError::StartFailed { detail, log });
            }
            tokio::time::sleep(CONNECT_EVERY).await;
        }
    }

    // A connection to the socket `run/socket` names, if one is listening there.
    async fn try_existing(home: &Home) -> Result<Option<Client>, ClientError> {
        let Some(path) = read_socket_path(home) else {
            return Ok(None);
        };
        match Client::connect_path(&path).await {
            Ok(client) => Ok(Some(client)),
            Err(ClientError::OtherUser(p)) => Err(ClientError::OtherUser(p)),
            Err(_) => Ok(None),
        }
    }

    /// Whether the connection has closed.
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Resolves when the connection closes.
    pub async fn closed(&self) {
        let mut rx = self.inner.closed.clone();
        let _ = rx.wait_for(|closed| *closed).await;
    }

    /// Notifications the daemon sends on this connection.
    pub fn notifications(&self) -> broadcast::Receiver<(String, Value)> {
        self.inner.notes.subscribe()
    }

    /// Sends one request and waits for its answer.
    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.inner.pending).insert(id, tx);
        if self.is_closed() {
            lock(&self.inner.pending).remove(&id);
            return Err(ClientError::Disconnected);
        }
        let line = request_line(&RequestId::Number(id), method, &params);
        {
            let mut writer = self.inner.writer.lock().await;
            if let Err(e) = writer.write_all(line.as_bytes()).await {
                lock(&self.inner.pending).remove(&id);
                return Err(ClientError::Io(e));
            }
        }
        match rx.await {
            Ok(outcome) => outcome.map_err(ClientError::Rpc),
            Err(_) => Err(ClientError::Disconnected),
        }
    }

    /// Writes one request and returns once it is written, without waiting for the answer (a hook's `end`).
    pub async fn send_value(&self, method: &str, params: Value) -> Result<(), ClientError> {
        if self.is_closed() {
            return Err(ClientError::Disconnected);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let line = request_line(&RequestId::Number(id), method, &params);
        let mut writer = self.inner.writer.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Sends one request with typed params and result.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        let params = serde_json::to_value(params)
            .map_err(|e| ClientError::Protocol(format!("could not encode params: {e}")))?;
        let value = self.call_value(method, params).await?;
        serde_json::from_value(value).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    /// `daemon.status`.
    pub async fn status(&self) -> Result<Status, ClientError> {
        self.call(method::DAEMON_STATUS, &Empty {}).await
    }

    async fn status_within(&self, limit: Duration) -> Result<Status, ClientError> {
        tokio::time::timeout(limit, self.status())
            .await
            .map_err(|_| ClientError::Protocol("daemon.status timed out".into()))?
    }

    /// `daemon.shutdown`.
    pub async fn shutdown(&self) -> Result<(), ClientError> {
        let _: Empty = self.call(method::DAEMON_SHUTDOWN, &Empty {}).await?;
        Ok(())
    }

    /// `session.open`.
    pub async fn session_open(&self, params: &SessionOpen) -> Result<SessionOpened, ClientError> {
        self.call(method::SESSION_OPEN, params).await
    }

    /// `session.update`.
    pub async fn session_update(&self, params: &SessionUpdate) -> Result<(), ClientError> {
        let _: Empty = self.call(method::SESSION_UPDATE, params).await?;
        Ok(())
    }

    /// `session.close`.
    pub async fn session_close(&self, token: &str) -> Result<(), ClientError> {
        let params = TokenOnly {
            token: token.to_string(),
        };
        let _: Empty = self.call(method::SESSION_CLOSE, &params).await?;
        Ok(())
    }

    /// `hook.event`.
    pub async fn hook_event(&self, params: &HookEvent) -> Result<HookResult, ClientError> {
        self.call(method::HOOK_EVENT, params).await
    }

    /// `messages.ack`.
    pub async fn ack(&self, params: &Ack) -> Result<(), ClientError> {
        let _: Empty = self.call(method::MESSAGES_ACK, params).await?;
        Ok(())
    }

    /// `directory.list`.
    pub async fn directory(&self, params: &DirectoryQuery) -> Result<Directory, ClientError> {
        self.call(method::DIRECTORY_LIST, params).await
    }

    /// `message.ask`.
    pub async fn ask(&self, params: &AskParams) -> Result<SendResult, ClientError> {
        self.call(method::MESSAGE_ASK, params).await
    }

    /// `message.tell`.
    pub async fn tell(&self, params: &TellParams) -> Result<SendResult, ClientError> {
        self.call(method::MESSAGE_TELL, params).await
    }

    /// `message.reply`.
    pub async fn reply(&self, params: &ReplyParams) -> Result<SendResult, ClientError> {
        self.call(method::MESSAGE_REPLY, params).await
    }

    /// `messages.fetch`.
    pub async fn fetch(&self, params: &FetchParams) -> Result<Fetched, ClientError> {
        self.call(method::MESSAGES_FETCH, params).await
    }

    /// `responder.take`.
    pub async fn take(&self, params: &TakeParams) -> Result<Taken, ClientError> {
        self.call(method::RESPONDER_TAKE, params).await
    }

    /// `directory.refresh`.
    pub async fn refresh(&self) -> Result<(), ClientError> {
        let _: Empty = self.call(method::DIRECTORY_REFRESH, &Empty {}).await?;
        Ok(())
    }

    /// `devices.list`.
    pub async fn devices(&self) -> Result<DeviceList, ClientError> {
        self.call(method::DEVICES_LIST, &Empty {}).await
    }

    /// `devices.revoke`.
    pub async fn revoke(&self, device: &str) -> Result<Revoked, ClientError> {
        let params = RevokeParams {
            device: device.to_string(),
        };
        self.call(method::DEVICES_REVOKE, &params).await
    }

    /// `auth.login`: `on_url` gets the sign-in page as soon as the daemon has it; the result comes when the
    /// person finishes in the browser.
    pub async fn login(&self, on_url: impl Fn(&str)) -> Result<LoginResult, ClientError> {
        let mut notes = self.notifications();
        let call = self.call::<_, LoginResult>(method::AUTH_LOGIN, &Empty {});
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => return result,
                note = notes.recv() => {
                    if let Ok((name, params)) = note
                        && name == method::AUTH_LOGIN_URL
                        && let Ok(url) = serde_json::from_value::<LoginUrl>(params)
                    {
                        on_url(&url.url);
                    }
                }
            }
        }
    }

    /// `auth.logout`.
    pub async fn logout(&self) -> Result<LogoutResult, ClientError> {
        self.call(method::AUTH_LOGOUT, &Empty {}).await
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits until the daemon for `home` has released its lock, or `deadline` passes.
pub async fn wait_for_exit(home: &Home, deadline: Instant) -> bool {
    loop {
        if lock_is_free(home) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(CONNECT_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.1-beta", "0.1.0"));
        assert!(!is_newer("x", "0.1.0"));
        assert!(!is_newer("0.2.0", "garbage"));
        assert!(!is_newer("0.2", "0.1.0"));
    }

    #[test]
    fn reconnect_backs_off_with_jitter() {
        for attempt in 0..12 {
            let base = (500u64 << attempt.min(6)).min(20_000);
            let d = reconnect_delay(attempt).as_millis() as u64;
            assert!(d >= base * 8 / 10 && d <= base * 12 / 10, "{attempt}: {d}");
        }
        assert!(reconnect_delay(30) <= Duration::from_secs(24));
    }

    #[tokio::test]
    async fn no_daemon_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path()).unwrap();
        assert!(matches!(
            Client::connect(&home, &ConnectOptions::existing()).await,
            Err(ClientError::NotRunning)
        ));
    }

    #[tokio::test]
    async fn a_daemon_that_dies_is_reported_with_its_log() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        let fake = tmp.path().join("fake-spew");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'settings.toml is not valid: boom' >&2\nexit 1\n",
        )
        .unwrap();
        crate::paths::set_mode(&fake, 0o700).unwrap();
        let opts = ConnectOptions {
            exe: Some(fake),
            ..ConnectOptions::cli()
        };
        let err = Client::connect(&home, &opts).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("did not start"), "{text}");
        assert!(text.contains("boom"), "{text}");
    }
}
