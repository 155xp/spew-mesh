//! The MCP server's link to the daemon (6.1, 6.4, 8.6): it connects in the background once the handshake is
//! answered, opens the server's session with what is known at spawn time, binds identity that only arrives with
//! a tool call (Codex `_meta.sessionId`, OpenCode `_mesh_session`), and reconnects and re-opens its sessions
//! when the daemon restarts. A reconnect starts a missing daemon only while `run/stopped` is absent; a tool call
//! may start one whatever it says (6.1).

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::{Mutex, Notify, watch};

use crate::address::{RESERVED_NAME, is_client};
use crate::control::client::{Client, ClientError, ConnectOptions, reconnect_delay};
use crate::control::protocol::{Reason, SessionOpen, SessionUpdate};
use crate::paths::Home;
use crate::sessions::TERMINAL_CLIENT;
use crate::wire::SessionKind;

/// How long a tool call waits for the daemon connection before it reports the problem: the 8 s a client waits
/// for a daemon to start (6.1), and a little more for the session to open.
pub const CALL_CONNECT_WAIT: Duration = Duration::from_secs(10);

/// How long one session step (open, bind, probe) may take: the daemon answers these at once, so a longer wait
/// means it is stuck.
const STEP_LIMIT: Duration = Duration::from_secs(10);

/// `<home>/run/stopped`, which `daemon stop` and `uninstall` write and a daemon removes once it holds the lock
/// (6.1). The CLI and the daemon own the file; the MCP server only looks for it.
const STOPPED_FILE: &str = "stopped";

/// The longest client session id the daemon takes.
const MAX_CLIENT_SESSION: usize = 200;

/// What the server knows about its client when it starts (8.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnFacts {
    /// `SPEW_MESH_CLIENT`, set in the MCP configs setup writes.
    pub client_env: Option<String>,
    /// The working directory.
    pub cwd: String,
    /// The parent process: Claude Code itself for its MCP servers (8.2).
    pub parent_pid: Option<u32>,
    /// `CLAUDE_CODE_SESSION_ID` as it was at spawn time (8.2).
    pub claude_session: Option<String>,
    /// `OPENCODE_PID`, the OpenCode process (8.4).
    pub opencode_pid: Option<u32>,
    /// `SPEW_PROJECT`, which picks the session's room (11).
    pub spew_project: Option<String>,
}

impl SpawnFacts {
    /// Reads the facts through `env`, with the working directory and parent process given.
    pub fn from_env(
        env: &dyn Fn(&str) -> Option<String>,
        cwd: String,
        parent_pid: Option<u32>,
    ) -> SpawnFacts {
        let text = |k: &str| {
            env(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let pid = |k: &str| {
            text(k)
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|p| *p > 1)
        };
        SpawnFacts {
            client_env: text("SPEW_MESH_CLIENT"),
            cwd,
            // A parent of 1 means the client is already gone and the process was handed to init.
            parent_pid: parent_pid.filter(|p| *p > 1),
            claude_session: text("CLAUDE_CODE_SESSION_ID").filter(|s| valid_client_session(s)),
            opencode_pid: pid("OPENCODE_PID"),
            spew_project: text(crate::settings::PROJECT_ENV),
        }
    }
}

/// Whether `s` can be a client's session or thread id: what the daemon accepts, kept to printable text.
pub fn valid_client_session(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_CLIENT_SESSION && !s.chars().any(char::is_control)
}

/// The client slug for the session (8.6): `SPEW_MESH_CLIENT` when set, else the handshake's `clientInfo.name`,
/// else `other`. A `SPEW_MESH_CLIENT` that isn't a client name is an error rather than a quiet `other`.
pub fn client_slug(env: Option<&str>, client_info: Option<&str>) -> Result<String, String> {
    if let Some(slug) = env {
        if is_client(slug) && slug != RESERVED_NAME && slug != TERMINAL_CLIENT {
            return Ok(slug.to_string());
        }
        return Err(format!(
            "SPEW_MESH_CLIENT is `{}`, which is not a client name: use a slug of lowercase letters, digits and \
             hyphens, at most 16 characters, such as `cursor`",
            crate::text::sanitize_line(slug, 40)
        ));
    }
    Ok(match client_info {
        Some("claude-code") => "claude",
        Some("codex-mcp-client") => "codex",
        Some("opencode") => "opencode",
        _ => "other",
    }
    .to_string())
}

/// Identity a tool call carries (8.3, 8.4), bound to a session at the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// Nothing: the server's own session.
    None,
    /// A Codex root thread id from `_meta.sessionId` or `_meta.threadId`.
    Codex(String),
    /// An OpenCode session id from the plugin's `_mesh_session` argument.
    Opencode(String),
}

/// Which of the server's sessions a token belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SlotKey {
    Main,
    Opencode(String),
}

#[derive(Debug, Clone)]
struct Slot {
    open: SessionOpen,
    token: Option<String>,
}

#[derive(Debug, Default)]
struct Sessions {
    main: Option<Slot>,
    // OpenCode runs several sessions in one process; each one past the first gets a session of its own (8.4).
    extra: HashMap<String, Slot>,
    // Bumped at every connection; tokens belong to the connection they were issued on.
    generation: u64,
    warned_rebind: bool,
}

/// Where the connection stands.
#[derive(Debug, Clone)]
enum State {
    /// Not started: the handshake hasn't been answered.
    Idle,
    /// Connecting, or re-connecting after a lost connection.
    Connecting,
    /// Connected, with the sessions open for this generation.
    Up(Client, u64),
    /// The last attempt failed; the reason is for the agent. `retrying` is false when it never will.
    Failed { reason: String, retrying: bool },
}

/// Why a tool call couldn't reach the daemon or the daemon refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// One sentence for the agent.
    pub message: String,
    /// The daemon's reason (6.2), when it gave one.
    pub reason: Option<&'static str>,
}

impl Failure {
    fn new(message: impl Into<String>) -> Failure {
        Failure {
            message: message.into(),
            reason: None,
        }
    }
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Failure {
        match e {
            ClientError::Rpc(e) => Failure {
                message: e.message,
                reason: Some(e.reason.as_str()),
            },
            ClientError::Disconnected => Failure::new(
                "lost the connection to the spew mesh daemon; it reconnects on its own, so try again in a moment",
            ),
            other => Failure::new(other.to_string()),
        }
    }
}

/// The daemon side of one MCP server.
#[derive(Debug)]
pub struct DaemonLink {
    home: Home,
    facts: SpawnFacts,
    opts: ConnectOptions,
    started: AtomicBool,
    client: OnceLock<String>,
    client_name: OnceLock<Option<String>>,
    state: watch::Sender<State>,
    sessions: Mutex<Sessions>,
    // Set by a tool call that found no daemon: the next attempt may start one even after a stop (6.1).
    start_wanted: AtomicBool,
    // Cuts the backoff short for that attempt.
    wake: Notify,
}

impl DaemonLink {
    /// A link for the daemon of `home`; nothing happens until [`DaemonLink::start`].
    pub fn new(home: Home, facts: SpawnFacts, opts: ConnectOptions) -> Arc<DaemonLink> {
        let (state, _) = watch::channel(State::Idle);
        Arc::new(DaemonLink {
            home,
            facts,
            opts,
            started: AtomicBool::new(false),
            client: OnceLock::new(),
            client_name: OnceLock::new(),
            state,
            sessions: Mutex::new(Sessions::default()),
            start_wanted: AtomicBool::new(false),
            wake: Notify::new(),
        })
    }

    /// The client slug, once started.
    pub fn client(&self) -> Option<&str> {
        self.client.get().map(String::as_str)
    }

    /// Starts connecting in the background, once: called when the handshake is answered (or at the first
    /// request of a client that skips it). `client_info` is the handshake's `clientInfo.name`.
    pub fn start(self: &Arc<Self>, client_info: Option<&str>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let name = client_info.map(|n| crate::text::sanitize_line(n, 64));
        let _ = self.client_name.set(name);
        let slug = match client_slug(self.facts.client_env.as_deref(), client_info) {
            Ok(slug) => slug,
            Err(reason) => {
                tracing::warn!("{reason}");
                self.state.send_replace(State::Failed {
                    reason,
                    retrying: false,
                });
                return;
            }
        };
        let _ = self.client.set(slug.clone());
        let main = Slot {
            open: self.main_open(&slug),
            token: None,
        };
        let link = self.clone();
        tokio::spawn(async move {
            link.sessions.lock().await.main = Some(main);
            link.keep_connected().await;
        });
    }

    // What the server's own session opens with (8.2 to 8.4, 8.6).
    fn main_open(&self, client: &str) -> SessionOpen {
        let f = &self.facts;
        let (client_session, client_pid) = match client {
            // The daemon keys a Claude Code session on its process, our parent; the session id is a second key.
            "claude" => (f.claude_session.clone(), f.parent_pid),
            // Codex MCP servers share one parent, so a pid says nothing; the thread id binds at a tool call.
            "codex" => (None, None),
            // One inbox session per OpenCode process until the plugin names a session.
            "opencode" => (None, f.opencode_pid.or(f.parent_pid)),
            // Any other client: a session id of our own, so a reconnect after a daemon restart finds the same
            // session; with no process to check, the session ends 10 s after this server's connection does.
            _ => (Some(format!("mcp-{}", ulid::Ulid::new())), None),
        };
        SessionOpen {
            client: client.to_string(),
            client_session,
            client_pid,
            cwd: f.cwd.clone(),
            client_name: self.client_name.get().cloned().flatten(),
            kind: SessionKind::Live,
            name: None,
            spew_project: f.spew_project.clone(),
            serve: None,
        }
    }

    // Connects, opens every session, waits for the connection to drop, and starts over with backoff (6.1).
    async fn keep_connected(self: Arc<Self>) {
        let mut attempt = 0u32;
        let mut first = true;
        loop {
            let opts = self.attempt_options(first);
            first = false;
            match Client::connect(&self.home, &opts).await {
                Ok(client) => match self.open_all(&client).await {
                    Ok(generation) => {
                        attempt = 0;
                        // A call that asked for a start while this attempt ran has its connection now.
                        self.start_wanted.store(false, Ordering::Release);
                        self.state
                            .send_replace(State::Up(client.clone(), generation));
                        client.closed().await;
                        tracing::info!("lost the daemon connection; reconnecting");
                        self.state.send_replace(State::Connecting);
                    }
                    Err(f) => {
                        tracing::warn!(error = %f.message, "could not open the mesh session");
                        self.state.send_replace(State::Failed {
                            reason: f.message,
                            retrying: true,
                        });
                    }
                },
                Err(ClientError::NotRunning) => {
                    // Only a reconnect after a stop gets here, so this is expected and logged quietly.
                    tracing::info!("the daemon is stopped; waiting for something to start it");
                    self.state.send_replace(State::Failed {
                        reason: "no daemon is running".into(),
                        retrying: true,
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not reach the spew mesh daemon");
                    self.state.send_replace(State::Failed {
                        reason: e.to_string(),
                        retrying: true,
                    });
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(reconnect_delay(attempt)) => {}
                _ = self.wake.notified() => {}
            }
            attempt = attempt.saturating_add(1);
        }
    }

    // The options for one connection attempt (6.1). The first attempt, and one a tool call asked for, may start
    // a missing daemon; a reconnect starts one only while `run/stopped` is absent, so a stopped daemon stays
    // stopped and a crashed one comes back.
    fn attempt_options(&self, first: bool) -> ConnectOptions {
        let asked = self.start_wanted.swap(false, Ordering::AcqRel);
        if first || asked || !self.stopped() {
            return self.opts.clone();
        }
        // No replacing an older daemon either: shutting it down without starting another would leave none.
        ConnectOptions {
            start: false,
            upgrade: false,
            ..self.opts.clone()
        }
    }

    // Whether `run/stopped` is there. Anything but a clear "not found" counts as there, so a reconnect never
    // undoes a stop it can't see.
    fn stopped(&self) -> bool {
        match std::fs::symlink_metadata(self.home.run_dir().join(STOPPED_FILE)) {
            Ok(_) => true,
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    }

    // Opens (or re-attaches) every session on a new connection and returns its generation.
    async fn open_all(&self, client: &Client) -> Result<u64, Failure> {
        let mut s = self.sessions.lock().await;
        let main = s
            .main
            .as_mut()
            .ok_or_else(|| Failure::new("the session isn't set up yet"))?;
        let opened = step("open the session", client.session_open(&main.open)).await?;
        main.token = Some(opened.token);
        // The hooks keep a Claude Code session's id current through a clear or resume (8.2); re-opening by the
        // process alone keeps a reconnect from binding the id this server saw at spawn time again.
        if main.open.client == "claude" && main.open.client_pid.is_some() {
            main.open.client_session = None;
        }
        let mut gone = Vec::new();
        for (id, slot) in s.extra.iter_mut() {
            match step("open the session", client.session_open(&slot.open)).await {
                Ok(opened) => slot.token = Some(opened.token),
                Err(e) => {
                    tracing::info!(session = %id, error = %e.message, "dropped a session that would not re-open");
                    gone.push(id.clone());
                }
            }
        }
        for id in gone {
            s.extra.remove(&id);
        }
        s.generation += 1;
        Ok(s.generation)
    }

    // The connection, waiting up to `limit` for one. A failed attempt that will be retried doesn't end the wait:
    // the call asks for the next attempt at once, allowed to start a daemon (6.1), and waits for it.
    async fn connection(&self, limit: Duration) -> Result<(Client, u64), Failure> {
        let mut rx = self.state.subscribe();
        let deadline = tokio::time::Instant::now() + limit;
        let mut asked = false;
        let mut last_reason = None;
        loop {
            let state = rx.borrow_and_update().clone();
            match state {
                State::Up(client, generation) if !client.is_closed() => {
                    return Ok((client, generation));
                }
                State::Failed {
                    reason,
                    retrying: false,
                } => {
                    return Err(Failure::new(format!(
                        "spew mesh can't reach its daemon: {reason}"
                    )));
                }
                State::Failed {
                    reason,
                    retrying: true,
                } => {
                    // Once per call, so a daemon that can't start isn't spawned in a tight loop.
                    if !asked {
                        asked = true;
                        self.start_wanted.store(true, Ordering::Release);
                        self.wake.notify_one();
                    }
                    last_reason = Some(reason);
                }
                _ => {}
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => {
                    return Err(Failure::new(match last_reason {
                        Some(reason) => format!(
                            "spew mesh can't reach its daemon: {reason}; `spew mesh doctor` shows what is wrong"
                        ),
                        None => "the spew mesh daemon didn't answer in time; `spew mesh doctor` shows what \
                                 is wrong"
                            .to_string(),
                    }));
                }
            }
        }
    }

    // The connection and the token of the session a call acts as, binding the call's identity first (6.4).
    async fn session(&self, binding: &Binding) -> Result<(Client, String, SlotKey), Failure> {
        for _ in 0..3 {
            let (client, generation) = self.connection(CALL_CONNECT_WAIT).await?;
            let mut s = self.sessions.lock().await;
            if s.generation != generation {
                // A reconnect happened in between; take the new connection.
                continue;
            }
            return self.bind(&client, &mut s, binding).await;
        }
        Err(Failure::new(
            "the connection to the spew mesh daemon keeps dropping; try again in a moment",
        ))
    }

    async fn bind(
        &self,
        client: &Client,
        s: &mut Sessions,
        binding: &Binding,
    ) -> Result<(Client, String, SlotKey), Failure> {
        let main_token = |s: &Sessions| {
            s.main
                .as_ref()
                .and_then(|m| m.token.clone())
                .ok_or_else(|| Failure::new("the mesh session isn't open yet"))
        };
        let id = match binding {
            Binding::None => return Ok((client.clone(), main_token(s)?, SlotKey::Main)),
            Binding::Codex(id) | Binding::Opencode(id) => id.clone(),
        };
        let bound = s.main.as_ref().and_then(|m| m.open.client_session.clone());
        match bound {
            None => {
                // The first call that names the session binds the server's own session to it; the daemon merges
                // it with a session the hooks opened for the same id (6.4).
                let token = main_token(s)?;
                let update = SessionUpdate {
                    token: token.clone(),
                    client_session: Some(id.clone()),
                    ..SessionUpdate::default()
                };
                step("bind the session", client.session_update(&update)).await?;
                if let Some(main) = s.main.as_mut() {
                    main.open.client_session = Some(id);
                }
                Ok((client.clone(), token, SlotKey::Main))
            }
            Some(b) if b == id => Ok((client.clone(), main_token(s)?, SlotKey::Main)),
            Some(_) => match binding {
                Binding::Opencode(_) => self.opencode_slot(client, s, id).await,
                _ => {
                    // One Codex MCP server serves one thread (8.3); a second id is unexpected, and acting as the
                    // first thread's session is what the server can do.
                    if !s.warned_rebind {
                        s.warned_rebind = true;
                        tracing::warn!(
                            "a tool call named a second Codex thread; keeping the first"
                        );
                    }
                    Ok((client.clone(), main_token(s)?, SlotKey::Main))
                }
            },
        }
    }

    // A session for an OpenCode session id other than the one the server's own session is bound to (8.4).
    async fn opencode_slot(
        &self,
        client: &Client,
        s: &mut Sessions,
        id: String,
    ) -> Result<(Client, String, SlotKey), Failure> {
        let key = SlotKey::Opencode(id.clone());
        if let Some(token) = s.extra.get(&id).and_then(|slot| slot.token.clone()) {
            return Ok((client.clone(), token, key));
        }
        let mut open = match s.main.as_ref() {
            Some(main) => main.open.clone(),
            None => return Err(Failure::new("the mesh session isn't open yet")),
        };
        open.client_session = Some(id.clone());
        let opened = step("open the session", client.session_open(&open)).await?;
        s.extra.insert(
            id,
            Slot {
                open,
                token: Some(opened.token.clone()),
            },
        );
        Ok((client.clone(), opened.token, key))
    }

    // Opens a slot's session again after its token stopped working (it ended, 6.4).
    async fn reopen(&self, client: &Client, key: &SlotKey) -> Result<String, Failure> {
        let mut s = self.sessions.lock().await;
        let slot = match key {
            SlotKey::Main => s.main.as_mut(),
            SlotKey::Opencode(id) => s.extra.get_mut(id),
        }
        .ok_or_else(|| Failure::new("the mesh session is gone"))?;
        let opened = step("re-open the session", client.session_open(&slot.open)).await?;
        slot.token = Some(opened.token.clone());
        Ok(opened.token)
    }

    /// Runs `op` as the session `binding` names. When the daemon says the token's session is gone (it ended,
    /// 6.4), the session is opened again and `op` runs once more.
    pub async fn with_session<T, F, Fut>(&self, binding: &Binding, op: F) -> Result<T, Failure>
    where
        F: Fn(Client, String) -> Fut,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        let (client, token, key) = self.session(binding).await?;
        match op(client.clone(), token.clone()).await {
            Err(ClientError::Rpc(e)) if e.reason == Reason::NotFound => {
                if !token_is_dead(&client, &token).await {
                    return Err(ClientError::Rpc(e).into());
                }
                let token = self.reopen(&client, &key).await?;
                Ok(op(client, token).await?)
            }
            other => Ok(other?),
        }
    }

    /// Closes this server's sessions that have no client process for the daemon to watch, each within `limit`,
    /// best effort: called when the client goes away, so they end now instead of 10 s later (rule 3 of 6.4).
    /// A session tied to a process (Claude Code, OpenCode) stays: hooks share it, rule 2 ends it with the
    /// process, and a client that only restarts its MCP server keeps its session and unread messages.
    pub async fn close_all(&self, limit: Duration) {
        let state = self.state.borrow().clone();
        let State::Up(client, _) = state else { return };
        if client.is_closed() {
            return;
        }
        let Ok(s) = tokio::time::timeout(limit, self.sessions.lock()).await else {
            return;
        };
        let tokens: Vec<String> = s
            .main
            .iter()
            .chain(s.extra.values())
            .filter(|slot| slot.open.client_pid.is_none())
            .filter_map(|slot| slot.token.clone())
            .collect();
        drop(s);
        for token in tokens {
            match tokio::time::timeout(limit, client.session_close(&token)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::debug!(error = %e, "could not close a session on exit"),
                Err(_) => tracing::debug!("closing a session on exit took too long"),
            }
        }
    }

    /// Whether the link has given up for good (a bad `SPEW_MESH_CLIENT`), with the reason.
    pub fn fatal(&self) -> Option<String> {
        match &*self.state.borrow() {
            State::Failed {
                reason,
                retrying: false,
            } => Some(reason.clone()),
            _ => None,
        }
    }
}

// Whether the daemon no longer knows `token`: an empty update touches nothing else. No answer in time counts
// as alive, so the call's own error stands.
async fn token_is_dead(client: &Client, token: &str) -> bool {
    let probe = SessionUpdate {
        token: token.to_string(),
        ..SessionUpdate::default()
    };
    matches!(
        tokio::time::timeout(STEP_LIMIT, client.session_update(&probe)).await,
        Ok(Err(ClientError::Rpc(e))) if e.reason == Reason::NotFound
    )
}

// One session step, bounded by STEP_LIMIT; `what` finishes "the daemon didn't ... in time".
async fn step<T>(
    what: &str,
    call: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, Failure> {
    match tokio::time::timeout(STEP_LIMIT, call).await {
        Ok(r) => r.map_err(Failure::from),
        Err(_) => Err(Failure {
            message: format!("the spew mesh daemon didn't {what} in time"),
            reason: Some("timeout"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: Map<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn the_client_comes_from_the_env_then_the_handshake() {
        assert_eq!(
            client_slug(Some("cursor"), Some("opencode")).unwrap(),
            "cursor"
        );
        assert_eq!(client_slug(None, Some("claude-code")).unwrap(), "claude");
        assert_eq!(
            client_slug(None, Some("codex-mcp-client")).unwrap(),
            "codex"
        );
        assert_eq!(client_slug(None, Some("opencode")).unwrap(), "opencode");
        assert_eq!(client_slug(None, Some("some-ide")).unwrap(), "other");
        assert_eq!(client_slug(None, None).unwrap(), "other");
        for bad in [
            "Claude",
            "terminal",
            "mesh",
            "a-very-long-client-name",
            "x y",
        ] {
            assert!(client_slug(Some(bad), None).is_err(), "{bad}");
        }
    }

    #[test]
    fn spawn_facts_are_read_and_checked() {
        let env = env_of(&[
            ("SPEW_MESH_CLIENT", " claude "),
            ("CLAUDE_CODE_SESSION_ID", "5f0c2a9e-0001"),
            ("OPENCODE_PID", "nope"),
            ("SPEW_PROJECT", "spew"),
        ]);
        let f = SpawnFacts::from_env(&env, "/w".into(), Some(4242));
        assert_eq!(f.client_env.as_deref(), Some("claude"));
        assert_eq!(f.claude_session.as_deref(), Some("5f0c2a9e-0001"));
        assert_eq!(f.opencode_pid, None);
        assert_eq!(f.parent_pid, Some(4242));
        assert_eq!(f.spew_project.as_deref(), Some("spew"));
        let f = SpawnFacts::from_env(&env_of(&[("OPENCODE_PID", "77")]), "/w".into(), Some(1));
        assert_eq!((f.opencode_pid, f.parent_pid), (Some(77), None));
        assert!(!valid_client_session(""));
        assert!(!valid_client_session("a\nb"));
        assert!(!valid_client_session(&"x".repeat(201)));
        assert!(valid_client_session("ses_3f9a0000000001"));
    }

    #[test]
    fn each_client_opens_with_its_own_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let facts = SpawnFacts {
            client_env: None,
            cwd: "/w".into(),
            parent_pid: Some(500),
            claude_session: Some("cs-1".into()),
            opencode_pid: Some(600),
            spew_project: None,
        };
        let link = DaemonLink::new(
            Home::at(tmp.path()).unwrap(),
            facts,
            ConnectOptions::existing(),
        );
        let c = link.main_open("claude");
        assert_eq!(
            (c.client_session.as_deref(), c.client_pid),
            (Some("cs-1"), Some(500))
        );
        let x = link.main_open("codex");
        assert_eq!((x.client_session, x.client_pid), (None, None));
        let o = link.main_open("opencode");
        assert_eq!((o.client_session, o.client_pid), (None, Some(600)));
        let g = link.main_open("gemini");
        assert!(g.client_session.unwrap().starts_with("mcp-"));
        assert_eq!(g.client_pid, None);
        assert_eq!(g.kind, SessionKind::Live);
    }

    #[tokio::test]
    async fn a_bad_client_name_fails_every_call_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let facts = SpawnFacts {
            client_env: Some("Bad Name".into()),
            cwd: "/w".into(),
            ..SpawnFacts::default()
        };
        let link = DaemonLink::new(
            Home::at(tmp.path()).unwrap(),
            facts,
            ConnectOptions::existing(),
        );
        link.start(Some("claude-code"));
        assert!(link.fatal().unwrap().contains("SPEW_MESH_CLIENT"));
        let err = link
            .with_session(&Binding::None, |_, _| async { Ok::<_, ClientError>(()) })
            .await
            .unwrap_err();
        assert!(err.message.contains("SPEW_MESH_CLIENT"), "{}", err.message);
    }

    #[tokio::test(start_paused = true)]
    async fn no_daemon_is_reported_when_the_call_wait_runs_out() {
        let tmp = tempfile::tempdir().unwrap();
        let facts = SpawnFacts {
            cwd: "/w".into(),
            ..SpawnFacts::default()
        };
        let link = DaemonLink::new(
            Home::at(tmp.path()).unwrap(),
            facts,
            ConnectOptions::existing(),
        );
        link.start(None);
        assert_eq!(link.client(), Some("other"));
        let begun = tokio::time::Instant::now();
        let err = link
            .with_session(&Binding::None, |_, _| async { Ok::<_, ClientError>(()) })
            .await
            .unwrap_err();
        assert!(err.message.contains("can't reach"), "{}", err.message);
        assert!(
            err.message.contains("no daemon is running"),
            "{}",
            err.message
        );
        // A failed attempt that will be retried doesn't fail the call before its wait is over.
        assert!(begun.elapsed() >= CALL_CONNECT_WAIT);
    }

    #[test]
    fn reconnects_start_a_daemon_only_while_it_was_not_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("mesh")).unwrap();
        std::fs::create_dir_all(home.run_dir()).unwrap();
        let link = DaemonLink::new(home.clone(), SpawnFacts::default(), ConnectOptions::cli());
        let starts = |o: &ConnectOptions| (o.start, o.upgrade);
        assert_eq!(starts(&link.attempt_options(true)), (true, true));
        assert_eq!(starts(&link.attempt_options(false)), (true, true));
        std::fs::write(home.run_dir().join(STOPPED_FILE), "").unwrap();
        // The first connect is the agent starting: it may start the daemon.
        assert_eq!(starts(&link.attempt_options(true)), (true, true));
        assert_eq!(starts(&link.attempt_options(false)), (false, false));
        // A tool call asked: that one attempt may start it, once.
        link.start_wanted.store(true, Ordering::Release);
        assert_eq!(starts(&link.attempt_options(false)), (true, true));
        assert_eq!(starts(&link.attempt_options(false)), (false, false));
        std::fs::remove_file(home.run_dir().join(STOPPED_FILE)).unwrap();
        assert_eq!(starts(&link.attempt_options(false)), (true, true));
    }
}
