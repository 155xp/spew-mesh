//! The session registry (2.1, 6.2, 6.4): opening and re-attaching sessions, names, tokens, the identity rules per
//! client (8.2 to 8.4), merging, and the four rules that end a session.
//!
//! Records live in the store so sessions survive a daemon restart; what only matters while the daemon runs
//! (issued tokens, attached connections, merged ids) lives in [`Registry`].

pub mod directory;
pub mod git;
pub mod process;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::address::{RESERVED_NAME, allocate_name, is_client, is_session_name, session_base_name};
use crate::agents::Client;
use crate::agents::hook::{HookCall, MeshEvent};
use crate::control::protocol::RpcError;
use crate::settings::{RoomSource, resolve_room};
use crate::store::{SessionRecord, Store, StoreError, hash_token};
use crate::text::sanitize_line;
use crate::wire::{ActivityState, MAX_BRANCH, Reach, SessionKind, place_from_cwd};

/// Rule 3: how long a session with no process waits for a new attach after its last connection closed (6.4).
pub const DETACH_GRACE_MS: u64 = 10_000;
/// After a daemon start, how long rule 3 waits for clients to re-attach (6.4).
pub const RESTART_GRACE_MS: u64 = 60_000;
/// Rule 4: a session with neither a process nor a connection ends after this long with no hook event (6.4).
pub const SILENT_LIMIT_MS: u64 = 60 * 60 * 1000;
/// Rule 2: how often client processes are checked (6.4).
pub const PID_CHECK_MS: u64 = 30_000;
/// The terminal session's name (2.1).
pub const TERMINAL_NAME: &str = "terminal";
/// The terminal session's client.
pub const TERMINAL_CLIENT: &str = "terminal";

/// What opens or re-attaches a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// Client slug.
    pub client: String,
    /// The client's own session or thread id.
    pub client_session: Option<String>,
    /// The client's process id.
    pub client_pid: Option<u32>,
    /// Working directory.
    pub cwd: String,
    /// live, responder or terminal.
    pub kind: SessionKind,
    /// A responder's `--name`.
    pub name: Option<String>,
    /// `SPEW_PROJECT` from the client's environment.
    pub spew_project: Option<String>,
    /// How messages reach it: `inbox` from an MCP server, `serve` for a responder.
    pub reach: Reach,
}

/// What the registry needs from outside: the time, settings and the process table.
pub struct Context<'a> {
    /// Now, ms.
    pub now: u64,
    /// `[roster] default_room`.
    pub default_room: Option<&'a str>,
    /// A process's start time ([`process::start_time`] outside tests).
    pub start_time: &'a dyn Fn(u32) -> Option<u64>,
}

/// A session that was opened or re-attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The session.
    pub session: SessionRecord,
    /// A fresh token for it, when one was issued.
    pub token: Option<String>,
    /// Whether the session is new.
    pub created: bool,
    /// Sessions merged into it on the way (6.4).
    pub merged: Vec<String>,
}

/// Why a session ended (6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// Rule 1: `session.close` or an `end` hook event.
    Closed,
    /// Rule 2: its client process exited.
    ProcessExited,
    /// Rule 3: its last connection closed and nothing re-attached.
    Detached,
    /// Rule 4: no hook event for 60 minutes.
    Silent,
}

impl EndReason {
    /// The reason in words, for the log.
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Closed => "closed",
            EndReason::ProcessExited => "client process exited",
            EndReason::Detached => "last connection closed",
            EndReason::Silent => "no hook event for 60 minutes",
        }
    }
}

fn store_err(e: StoreError) -> RpcError {
    RpcError::unavailable(format!("the store failed: {e}"))
}

/// A random 128-bit session token as hex (6.2).
pub fn new_token() -> Result<String, RpcError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| RpcError::unavailable(format!("no random bytes for a token: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// How a room source is kept in the store.
pub fn room_source_text(source: &RoomSource) -> &'static str {
    match source {
        RoomSource::Env => "env",
        RoomSource::SpewJson(_) => "spew-json",
        RoomSource::DefaultRoom => "default",
        RoomSource::None => "none",
        RoomSource::Invalid(_) => "invalid",
    }
}

/// Whether a stored room source says the room came from `.spew.json` (5.6).
pub fn room_from_spew_json(record: &SessionRecord) -> bool {
    record.room_source.as_deref() == Some("spew-json")
}

/// Fills in what the working directory says: repo, branch, place and room (2.1, 5.6, 11).
pub fn apply_cwd(
    record: &mut SessionRecord,
    cwd: &str,
    spew_project: Option<&str>,
    default_room: Option<&str>,
) {
    let path = Path::new(cwd);
    let info = git::git_info(path);
    record.cwd = Some(cwd.to_string());
    record.repo = info.as_ref().and_then(|i| i.repo.clone());
    // A branch name is whatever .git/HEAD says; it goes out in presence, so it gets the peer-text rules (5.6).
    record.branch = info
        .as_ref()
        .and_then(|i| i.branch.as_deref())
        .map(|b| sanitize_line(b, MAX_BRANCH))
        .filter(|b| !b.is_empty());
    record.place = place_from_cwd(path);
    let choice = resolve_room(spew_project, path, default_room);
    if let RoomSource::Invalid(why) = &choice.source {
        tracing::info!(session = %record.name, "session stays private: {why}");
    }
    record.room = choice.room;
    record.room_source = Some(room_source_text(&choice.source).to_string());
}

/// The name a new session asks for before clashes (2.1).
pub fn base_name(req: &OpenRequest) -> String {
    match req.kind {
        SessionKind::Terminal => TERMINAL_NAME.to_string(),
        SessionKind::Responder => req.name.clone().unwrap_or_else(|| req.client.clone()),
        SessionKind::Live => {
            let path = Path::new(&req.cwd);
            let place = git::git_info(path)
                .and_then(|g| g.place())
                .or_else(|| place_from_cwd(path));
            session_base_name(&req.client, place.as_deref())
        }
    }
}

/// Checks the parts of a request the caller controls.
pub fn check_request(req: &OpenRequest) -> Result<(), RpcError> {
    if !is_client(&req.client) || req.client == RESERVED_NAME {
        return Err(RpcError::invalid(format!(
            "`{}` is not a client name: use a slug of at most 16 characters",
            req.client
        )));
    }
    if (req.kind == SessionKind::Terminal) != (req.client == TERMINAL_CLIENT) {
        return Err(RpcError::invalid(
            "only the terminal client opens a terminal session",
        ));
    }
    if req.cwd.is_empty() || req.cwd.contains('\0') {
        return Err(RpcError::invalid("cwd must be a folder path"));
    }
    if let Some(name) = &req.name {
        if req.kind != SessionKind::Responder {
            return Err(RpcError::invalid("only a responder picks its own name"));
        }
        if !is_session_name(name) || name == RESERVED_NAME || name == TERMINAL_NAME {
            return Err(RpcError::invalid(format!(
                "`{name}` is not a session name: lowercase letters, digits and single hyphens, at most 48"
            )));
        }
    }
    if let Some(cs) = &req.client_session
        && (cs.is_empty() || cs.len() > 200 || cs.chars().any(char::is_control))
    {
        return Err(RpcError::invalid("client_session is not a session id"));
    }
    Ok(())
}

/// What the registry keeps in memory alongside the store.
#[derive(Debug, Default)]
pub struct Registry {
    tokens: HashMap<String, String>,
    attached: HashMap<u64, HashSet<String>>,
    connected: HashSet<String>,
    detached_at: HashMap<String, u64>,
    aliases: HashMap<String, String>,
    last_tool_show: HashMap<String, u64>,
    mentioned: HashMap<String, HashSet<String>>,
}

impl Registry {
    /// Loads the live sessions after a daemon start. Sessions that were reached through a connection get
    /// [`RESTART_GRACE_MS`] to re-attach (6.4); a hooked session counts when it also had one.
    pub fn restore(store: &Store, now: u64) -> Result<Registry, StoreError> {
        let mut reg = Registry::default();
        let connected = store.had_connection()?;
        for s in store.live_sessions()? {
            reg.tokens.insert(s.token_hash.clone(), s.id.clone());
            let had_connection = connected.contains(&s.id)
                || (s.client_pid.is_none() && matches!(s.reach, Reach::Inbox | Reach::Serve));
            if had_connection && s.kind != SessionKind::Terminal {
                reg.connected.insert(s.id.clone());
                reg.detached_at
                    .insert(s.id.clone(), now + RESTART_GRACE_MS - DETACH_GRACE_MS);
            }
        }
        Ok(reg)
    }

    /// The live session that `id` names now: itself, or the session it was merged into.
    pub fn resolve_id<'a>(&'a self, id: &'a str) -> &'a str {
        let mut current = id;
        // Merges only ever point at an older live session, so this ends; the bound is a guard.
        for _ in 0..16 {
            match self.aliases.get(current) {
                Some(next) => current = next,
                None => break,
            }
        }
        current
    }

    /// A session's id and every id merged into it, for reading its messages.
    pub fn ids_of(&self, id: &str) -> Vec<String> {
        let mut ids = vec![id.to_string()];
        ids.extend(
            self.aliases
                .keys()
                .filter(|old| self.resolve_id(old) == id)
                .cloned(),
        );
        ids
    }

    /// The live session a token belongs to (6.2).
    pub fn session_for_token(&self, store: &Store, token: &str) -> Result<SessionRecord, RpcError> {
        let hash = hash_token(token);
        let gone = || {
            RpcError::not_found(
                "this session is not open any more (it ended, or the daemon restarted); open it again",
            )
        };
        if let Some(id) = self.tokens.get(&hash) {
            let id = self.resolve_id(id);
            return match store.session(id).map_err(store_err)? {
                Some(s) if s.ended_at.is_none() => Ok(s),
                _ => Err(gone()),
            };
        }
        store
            .session_by_token(&hash)
            .map_err(store_err)?
            .ok_or_else(gone)
    }

    fn issue_token(&mut self, record: &mut SessionRecord) -> Result<String, RpcError> {
        let token = new_token()?;
        record.token_hash = hash_token(&token);
        self.tokens
            .insert(record.token_hash.clone(), record.id.clone());
        Ok(token)
    }

    /// Records that connection `conn` uses session `id` (rule 3), and in the store that it had a connection,
    /// so rule 3 still holds after a restart.
    pub fn attach(&mut self, store: &Store, conn: u64, id: &str) -> Result<(), StoreError> {
        if !self.connected.contains(id) {
            store.mark_had_connection(id)?;
        }
        self.attached
            .entry(conn)
            .or_default()
            .insert(id.to_string());
        self.connected.insert(id.to_string());
        self.detached_at.remove(id);
        Ok(())
    }

    fn is_attached(&self, id: &str) -> bool {
        self.attached.values().any(|ids| ids.contains(id))
    }

    /// A connection closed: sessions it was the last attachment of start their rule 3 grace.
    pub fn connection_closed(&mut self, conn: u64, now: u64) {
        let Some(ids) = self.attached.remove(&conn) else {
            return;
        };
        for id in ids {
            if !self.is_attached(&id) {
                self.detached_at.insert(id, now);
            }
        }
    }

    /// Forgets everything kept in memory for an ended session.
    pub fn forget(&mut self, id: &str) {
        self.tokens.retain(|_, sid| sid != id);
        for ids in self.attached.values_mut() {
            ids.remove(id);
        }
        self.connected.remove(id);
        self.detached_at.remove(id);
        self.last_tool_show.remove(id);
        self.mentioned.remove(id);
        self.aliases.retain(|old, to| old != id && to != id);
    }

    /// Whether a `tool` hook may show messages now, at most once every 30 s per session (8.1); records it when
    /// it may.
    pub fn tool_show_allowed(&mut self, id: &str, now: u64) -> bool {
        let ok = self
            .last_tool_show
            .get(id)
            .is_none_or(|t| now.saturating_sub(*t) >= 30_000);
        if ok {
            self.last_tool_show.insert(id.to_string(), now);
        }
        ok
    }

    /// Of `asks` (ids shown before and unanswered), whether any wasn't mentioned by the prompt line yet; marks
    /// them all mentioned (8.1, "once per ask").
    pub fn mention_unanswered(&mut self, id: &str, asks: &[String]) -> bool {
        let seen = self.mentioned.entry(id.to_string()).or_default();
        let mut fresh = false;
        for ask in asks {
            fresh |= seen.insert(ask.clone());
        }
        fresh
    }

    /// Opens a session or re-attaches the one the request belongs to (6.2 `session.open`, 8.2 to 8.4).
    pub fn open(
        &mut self,
        store: &Store,
        req: &OpenRequest,
        ctx: &Context<'_>,
    ) -> Result<Opened, RpcError> {
        check_request(req)?;
        let live = store.live_sessions().map_err(store_err)?;
        let found = find_for_open(&live, req, ctx).cloned();
        let mut merged = Vec::new();
        let (mut record, created) = match found {
            Some(mut s) => {
                if s.cwd.as_deref() != Some(req.cwd.as_str()) || req.spew_project.is_some() {
                    apply_cwd(
                        &mut s,
                        &req.cwd,
                        req.spew_project.as_deref(),
                        ctx.default_room,
                    );
                }
                if s.client_pid.is_none()
                    && let Some(pid) = req.client_pid
                {
                    s.client_pid = Some(pid);
                    s.client_pid_start = (ctx.start_time)(pid);
                }
                if req.reach == Reach::Hooked {
                    s.reach = Reach::Hooked;
                }
                (s, false)
            }
            None => (self.new_record(&live, req, ctx)?, true),
        };
        // The terminal is re-opened by every CLI command and never ends; only the newest token stays good, so
        // the old hashes don't pile up for the daemon's life.
        if record.kind == SessionKind::Terminal {
            self.tokens.retain(|_, sid| sid != &record.id);
        }
        let token = self.issue_token(&mut record)?;
        record.last_seen_at = ctx.now;
        store.put_session(&record).map_err(store_err)?;
        if let Some(cs) = &req.client_session
            && record.client_session.as_deref() != Some(cs.as_str())
        {
            let (bound, gone) = self.bind_client_session(store, &record.id, cs, ctx)?;
            record = bound;
            merged.extend(gone);
        }
        Ok(Opened {
            session: record,
            token: Some(token),
            created,
            merged,
        })
    }

    fn new_record(
        &self,
        live: &[SessionRecord],
        req: &OpenRequest,
        ctx: &Context<'_>,
    ) -> Result<SessionRecord, RpcError> {
        let taken: HashSet<&str> = live.iter().map(|s| s.name.as_str()).collect();
        let name = allocate_name(&base_name(req), |n| taken.contains(n))
            .ok_or_else(|| RpcError::conflict("every name for this session is taken"))?;
        let state = match req.kind {
            SessionKind::Responder => ActivityState::Idle,
            _ => ActivityState::Unknown,
        };
        let mut record = SessionRecord {
            id: ulid::Ulid::new().to_string(),
            name,
            client: req.client.clone(),
            kind: req.kind,
            client_session: None,
            client_pid: req.client_pid,
            client_pid_start: req.client_pid.and_then(|pid| (ctx.start_time)(pid)),
            token_hash: String::new(),
            cwd: None,
            repo: None,
            branch: None,
            place: None,
            room: None,
            room_source: None,
            reach: req.reach,
            state,
            state_since: ctx.now,
            activity: None,
            activity_project: None,
            opened_at: ctx.now,
            last_seen_at: ctx.now,
            ended_at: None,
        };
        apply_cwd(
            &mut record,
            &req.cwd,
            req.spew_project.as_deref(),
            ctx.default_room,
        );
        // A responder re-opens with its client_session after a lost connection; bind it right away.
        if req.kind == SessionKind::Responder {
            record.client_session = req.client_session.clone();
        }
        Ok(record)
    }

    /// Gives session `id` the client session id `cs`, merging it with the session that already has it (6.4).
    /// Returns the session that holds it now and the ids that were merged away.
    pub fn bind_client_session(
        &mut self,
        store: &Store,
        id: &str,
        cs: &str,
        ctx: &Context<'_>,
    ) -> Result<(SessionRecord, Vec<String>), RpcError> {
        let mut record = store
            .session(id)
            .map_err(store_err)?
            .filter(|s| s.ended_at.is_none())
            .ok_or_else(|| RpcError::not_found("that session is not open"))?;
        if record.client_session.as_deref() == Some(cs) {
            return Ok((record, Vec::new()));
        }
        let other = store
            .session_by_client(&record.client, cs)
            .map_err(store_err)?
            .filter(|o| o.id != record.id);
        let Some(other) = other else {
            record.client_session = Some(cs.to_string());
            store.put_session(&record).map_err(store_err)?;
            return Ok((record, Vec::new()));
        };
        let (mut survivor, loser) = self.merge(store, record, other, ctx)?;
        survivor.client_session = Some(cs.to_string());
        store.put_session(&survivor).map_err(store_err)?;
        Ok((survivor, vec![loser]))
    }

    // The older session keeps its name; the newer one ends and every id, token and connection that named it
    // names the older one from now on (6.4).
    fn merge(
        &mut self,
        store: &Store,
        a: SessionRecord,
        b: SessionRecord,
        ctx: &Context<'_>,
    ) -> Result<(SessionRecord, String), RpcError> {
        let (mut survivor, loser) = if (a.opened_at, &a.id) <= (b.opened_at, &b.id) {
            (a, b)
        } else {
            (b, a)
        };
        if survivor.client_pid.is_none() {
            survivor.client_pid = loser.client_pid;
            survivor.client_pid_start = loser.client_pid_start;
        }
        if loser.reach == Reach::Hooked {
            survivor.reach = Reach::Hooked;
        }
        if survivor.activity.is_none() {
            survivor.activity = loser.activity.clone();
            survivor.activity_project = loser.activity_project.clone();
        }
        survivor.last_seen_at = survivor.last_seen_at.max(loser.last_seen_at);
        for card in store.cards(&loser.id).map_err(store_err)? {
            store
                .put_card(&survivor.id, &card.card, &card.project, card.refreshed_at)
                .map_err(store_err)?;
        }
        store.clear_cards(&loser.id).map_err(store_err)?;
        store.end_session(&loser.id, ctx.now).map_err(store_err)?;
        // The caller binds the client session id that caused the merge right after this.
        survivor.client_session = None;
        store.put_session(&survivor).map_err(store_err)?;
        tracing::info!(survivor = %survivor.name, merged = %loser.name, "merged two sessions that are one");

        for to in self.aliases.values_mut() {
            if *to == loser.id {
                to.clone_from(&survivor.id);
            }
        }
        self.aliases.insert(loser.id.clone(), survivor.id.clone());
        for sid in self.tokens.values_mut() {
            if *sid == loser.id {
                sid.clone_from(&survivor.id);
            }
        }
        for ids in self.attached.values_mut() {
            if ids.remove(&loser.id) {
                ids.insert(survivor.id.clone());
            }
        }
        if self.connected.remove(&loser.id) {
            store.mark_had_connection(&survivor.id).map_err(store_err)?;
            self.connected.insert(survivor.id.clone());
        }
        self.detached_at.remove(&loser.id);
        if self.is_attached(&survivor.id) {
            self.detached_at.remove(&survivor.id);
        }
        if let Some(seen) = self.mentioned.remove(&loser.id) {
            self.mentioned
                .entry(survivor.id.clone())
                .or_default()
                .extend(seen);
        }
        self.last_tool_show.remove(&loser.id);
        Ok((survivor, loser.id))
    }

    /// The session a hook event names, opened when the daemon doesn't know it yet (6.4, 8.2 to 8.4). `None` for
    /// an `end` event naming no known session.
    pub fn for_hook(
        &mut self,
        store: &Store,
        call: &HookCall,
        ctx: &Context<'_>,
    ) -> Result<Option<Opened>, RpcError> {
        let client = call.client.slug();
        if call.client_session.is_empty() {
            return Err(RpcError::invalid("the hook event names no session"));
        }
        let mut merged = Vec::new();
        let mut found = store
            .session_by_client(client, &call.client_session)
            .map_err(store_err)?;
        if found.is_none() {
            let live = store.live_sessions().map_err(store_err)?;
            let joined = match call.client {
                // Hooks and the MCP server share the Claude Code process id (8.2); a clear or resume keeps it.
                Client::Claude => call.client_pid.and_then(|pid| {
                    live.iter()
                        .find(|s| s.client == client && same_process(s, pid, ctx))
                        .cloned()
                }),
                // An unknown thread joins the only Codex session in its folder with no thread id yet (8.3).
                Client::Codex => {
                    let mut open = live.iter().filter(|s| {
                        s.client == client
                            && s.kind == SessionKind::Live
                            && s.client_session.is_none()
                            && s.cwd.as_deref() == Some(call.cwd.as_str())
                    });
                    match (open.next(), open.next()) {
                        (Some(only), None) => Some(only.clone()),
                        _ => None,
                    }
                }
                // One OpenCode process runs many sessions; its MCP server binds its session at a tool call (8.4).
                Client::Opencode => None,
            };
            if let Some(s) = joined {
                let (bound, gone) =
                    self.bind_client_session(store, &s.id, &call.client_session, ctx)?;
                merged.extend(gone);
                found = Some(bound);
            }
        }
        let (mut record, created) = match found {
            Some(s) => (s, false),
            None if call.event == MeshEvent::End => return Ok(None),
            None => {
                let req = OpenRequest {
                    client: client.to_string(),
                    client_session: Some(call.client_session.clone()),
                    client_pid: call.client_pid,
                    cwd: call.cwd.clone(),
                    kind: SessionKind::Live,
                    name: None,
                    spew_project: call.facts.spew_project.clone(),
                    reach: Reach::Hooked,
                };
                check_request(&req)?;
                let live = store.live_sessions().map_err(store_err)?;
                let mut record = self.new_record(&live, &req, ctx)?;
                record.client_session = Some(call.client_session.clone());
                // A hooked session has no token holder; a fresh token's hash keeps the column filled.
                let _ = self.issue_token(&mut record)?;
                (record, true)
            }
        };
        if !created && call.event != MeshEvent::Tool {
            apply_cwd(
                &mut record,
                &call.cwd,
                call.facts.spew_project.as_deref(),
                ctx.default_room,
            );
        }
        if record.client_pid.is_none()
            && let Some(pid) = call.client_pid
        {
            record.client_pid = Some(pid);
            record.client_pid_start = (ctx.start_time)(pid);
        }
        record.reach = Reach::Hooked;
        record.last_seen_at = ctx.now;
        store.put_session(&record).map_err(store_err)?;
        Ok(Some(Opened {
            session: record,
            token: None,
            created,
            merged,
        }))
    }

    /// The sessions that rules 2 to 4 end now (6.4). Rule 2 runs only when `check_pids` is set (every 30 s).
    pub fn due(
        &self,
        live: &[SessionRecord],
        now: u64,
        check_pids: bool,
        start_time: &dyn Fn(u32) -> Option<u64>,
    ) -> Vec<(String, EndReason)> {
        let mut out = Vec::new();
        for s in live {
            if s.kind == SessionKind::Terminal {
                continue;
            }
            if let Some(pid) = s.client_pid {
                if check_pids && !process::is_alive(pid, s.client_pid_start, start_time) {
                    out.push((s.id.clone(), EndReason::ProcessExited));
                }
                continue;
            }
            if self.connected.contains(&s.id) {
                let gone_since = self.detached_at.get(&s.id);
                if !self.is_attached(&s.id)
                    && gone_since.is_some_and(|t| now >= t.saturating_add(DETACH_GRACE_MS))
                {
                    out.push((s.id.clone(), EndReason::Detached));
                }
                continue;
            }
            if s.kind == SessionKind::Responder {
                continue;
            }
            if now >= s.last_seen_at.saturating_add(SILENT_LIMIT_MS) {
                out.push((s.id.clone(), EndReason::Silent));
            }
        }
        out
    }
}

fn same_process(s: &SessionRecord, pid: u32, ctx: &Context<'_>) -> bool {
    s.client_pid == Some(pid)
        && match s.client_pid_start {
            Some(start) => (ctx.start_time)(pid) == Some(start),
            None => true,
        }
}

// The live session a `session.open` re-attaches, if any (8.2 to 8.4).
fn find_for_open<'a>(
    live: &'a [SessionRecord],
    req: &OpenRequest,
    ctx: &Context<'_>,
) -> Option<&'a SessionRecord> {
    let same_client = |s: &&SessionRecord| s.client == req.client && s.kind == req.kind;
    match req.kind {
        SessionKind::Terminal => live.iter().find(|s| s.kind == SessionKind::Terminal),
        SessionKind::Responder => {
            let cs = req.client_session.as_deref()?;
            live.iter()
                .filter(same_client)
                .find(|s| s.client_session.as_deref() == Some(cs))
        }
        SessionKind::Live => {
            // Claude Code: the process id is the key, the session id the second key (8.2).
            if req.client == Client::Claude.slug()
                && let Some(pid) = req.client_pid
                && let Some(s) = live
                    .iter()
                    .filter(same_client)
                    .find(|s| same_process(s, pid, ctx))
            {
                return Some(s);
            }
            if let Some(cs) = req.client_session.as_deref() {
                return live
                    .iter()
                    .filter(same_client)
                    .find(|s| s.client_session.as_deref() == Some(cs));
            }
            // Codex MCP servers share one parent, so a pid says nothing there (8.3). Elsewhere a server with no
            // session id is one inbox session per client process (OpenCode without its plugin, 8.4).
            if req.client == Client::Codex.slug() {
                return None;
            }
            let pid = req.client_pid?;
            live.iter()
                .filter(same_client)
                .find(|s| s.client_session.is_none() && same_process(s, pid, ctx))
        }
    }
}

#[cfg(test)]
mod tests;
