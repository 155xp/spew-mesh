//! Delivery (5.5, 5.6, 7): storing messages, the outbox and its backoff, receipts, waits and notices, presence
//! in both directions, and the session operations of the control socket.
//!
//! One [`Courier`] holds the daemon's state behind one lock: the store, the session registry, the roster in
//! force and peers' presence. Nothing awaits while holding it; network calls go through [`Links`] outside it.

mod guard;
mod inbox;
pub mod outbox;
mod receive;
pub mod render;
mod send;
mod waits;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use tokio::sync::Notify;

use crate::control::protocol::{
    Counts, DeviceRow, Directory, DirectoryQuery, NetworkStatus, RosterStatus, RpcError,
};
use crate::control::server::Conn;
use crate::identity::parse_endpoint_id;
use crate::link::{LinkEvent, Links};
use crate::presence::{RemotePresence, Seq};
use crate::roster::{ActiveRoster, Me, PRIVATE_ROOM_ID, Roster};
use crate::sessions::directory::{Entry, ListingInput, listing, local_presence};
use crate::sessions::{Context, EndReason, Registry};
use crate::spew_api::Clock;
use crate::store::{
    Message, MessageState, OutboxKind, Party, SessionRecord, Store, StoreError, StoredMessage,
};
use crate::text::{NoticeEvent, notice_body};
use crate::wire::{
    DEFAULT_TTL_SECS, MessageKind, PresenceDevice, PresencePeer, PresenceSnapshot, ReceiptState,
    ReceiptUpdate, RefusalCode, SessionKind, filter_for_peer,
};

use guard::Guard;
use waits::Waits;

pub use guard::{LIMIT as CONVERSATION_LIMIT, WINDOW_MS as CONVERSATION_WINDOW_MS};

/// The sender name and session of the daemon's own notices (7.6).
pub const MESH: &str = "mesh";
/// The refusal code kept on mail that failed because its device left every shared room (4.3). It never goes
/// on the wire.
pub const NOT_SHARED: &str = "not-shared";
/// How long a sent message waits for the target device's receipt during the send call (7.1).
pub const RECEIPT_WAIT: Duration = Duration::from_secs(8);
/// How often delivered messages are checked for expiry.
const EXPIRY_CHECK_MS: u64 = 60_000;
/// How often the store is swept (11.1).
const SWEEP_MS: u64 = 24 * 60 * 60 * 1000;

/// This device as the courier sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDevice {
    /// Endpoint id.
    pub id: EndpointId,
    /// Endpoint id as 64 hex characters.
    pub hex: String,
    /// Device name.
    pub name: String,
    /// `macos`, `linux`.
    pub platform: String,
}

impl LocalDevice {
    /// This device from its endpoint id and name.
    pub fn new(id: EndpointId, name: &str, platform: &str) -> LocalDevice {
        LocalDevice {
            id,
            hex: id.to_string(),
            name: name.to_string(),
            platform: platform.to_string(),
        }
    }
}

/// What the courier needs from settings and the environment.
pub struct CourierConfig {
    /// This device.
    pub device: LocalDevice,
    /// `[roster] default_room`.
    pub default_room: Option<String>,
    /// `[presence] branch`.
    pub send_branch: bool,
    /// `[presence] place`.
    pub send_place: bool,
    /// `[serve]`, for responder gates (9).
    pub serve: crate::responder::ServeSettings,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// A process's start time, for session rule 2.
    pub start_time: fn(u32) -> Option<u64>,
}

/// The roster provider's state, for `daemon.status` and `doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RosterInfo {
    /// The provider in force.
    pub provider: String,
    /// The provider settings ask for.
    pub configured: String,
    /// One line on its state.
    pub state: String,
    /// What keeps the configured provider from working.
    pub problem: Option<String>,
}

pub(crate) struct Core {
    store: Store,
    reg: Registry,
    roster: ActiveRoster,
    roster_info: RosterInfo,
    remote: RemotePresence,
    guard: Guard,
    gates: HashMap<String, crate::responder::limits::Gate>,
    waiting_asks: HashMap<String, usize>,
    in_flight: HashSet<i64>,
    pumping: HashSet<String>,
    seq: Seq,
    presence_sent: HashMap<EndpointId, u64>,
    last_pid_check: u64,
    last_expiry_check: u64,
    last_sweep: u64,
}

/// The daemon's delivery engine, generic over the link layer.
pub struct Courier<L: Links> {
    links: L,
    core: Mutex<Core>,
    waits: Waits,
    cfg: CourierConfig,
    presence_kick: Notify,
    outbox_kick: Notify,
}

pub(crate) fn store_err(e: StoreError) -> RpcError {
    RpcError::unavailable(format!("the store failed: {e}"))
}

/// What a refusal code means for the sender, in words.
pub fn refusal_reason(code: RefusalCode) -> &'static str {
    match code {
        RefusalCode::RateLimited => {
            "that device is getting too many messages; it stays queued and is sent again"
        }
        RefusalCode::Invalid => "the receiving device found it malformed",
        RefusalCode::NoSuchSession => {
            "no such session takes messages from you there; the who tool lists sessions"
        }
        RefusalCode::SessionEnded => {
            "that session ended; send it again to whatever session is there now (the who tool lists them)"
        }
        RefusalCode::Busy => "the responder is busy; try again later",
        RefusalCode::ConversationLimit => crate::text::CONVERSATION_LIMIT_NOTE,
    }
}

fn hex_of(id: &EndpointId) -> String {
    id.to_string()
}

impl<L: Links> Courier<L> {
    /// A courier over `store`, starting from the roster in force. The links admit nobody until
    /// [`Courier::start_links`], so the inbound handler can be bound first.
    pub fn new(
        links: L,
        store: Store,
        roster: ActiveRoster,
        roster_info: RosterInfo,
        cfg: CourierConfig,
    ) -> Result<Courier<L>, StoreError> {
        let now = cfg.clock.now_ms();
        let reg = Registry::restore(&store, now)?;
        let remote = RemotePresence::restore(&store.presences()?);
        Ok(Courier {
            links,
            core: Mutex::new(Core {
                store,
                reg,
                roster,
                roster_info,
                remote,
                guard: Guard::default(),
                gates: HashMap::new(),
                waiting_asks: HashMap::new(),
                in_flight: HashSet::new(),
                pumping: HashSet::new(),
                seq: Seq::default(),
                presence_sent: HashMap::new(),
                last_pid_check: 0,
                last_expiry_check: 0,
                last_sweep: 0,
            }),
            waits: Waits::default(),
            cfg,
            presence_kick: Notify::new(),
            outbox_kick: Notify::new(),
        })
    }

    /// Puts the roster's admission into force on the links (5.2).
    pub fn start_links(&self) {
        let admission = self.core().roster.admission().clone();
        self.links.set_admission(admission);
    }

    /// Stores a provider's roster cache (4.2).
    pub fn save_roster_cache(&self, cached: &crate::store::CachedRoster) {
        if let Err(e) = self.core().store.put_roster(cached) {
            tracing::warn!(error = %e, "could not store the roster cache");
        }
    }

    pub(crate) fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The link layer.
    pub fn links(&self) -> &L {
        &self.links
    }

    /// This device.
    pub fn device(&self) -> &LocalDevice {
        &self.cfg.device
    }

    /// Now, ms.
    pub fn now(&self) -> u64 {
        self.cfg.clock.now_ms()
    }

    /// Resolves after something local changed that peers should hear about (5.6).
    pub async fn presence_changed_signal(&self) {
        self.presence_kick.notified().await;
    }

    /// Resolves when new mail is queued.
    pub async fn outbox_signal(&self) {
        self.outbox_kick.notified().await;
    }

    pub(crate) fn presence_changed(&self) {
        self.presence_kick.notify_one();
    }

    fn ctx(&self, now: u64) -> Context<'_> {
        Context {
            now,
            default_room: self.cfg.default_room.as_deref(),
            start_time: &self.cfg.start_time,
        }
    }

    /// Who this device's person is.
    pub fn me(&self) -> Me {
        self.core().roster.me().clone()
    }

    fn address(&self, c: &Core, record: &SessionRecord) -> String {
        format!(
            "{}@{}/{}",
            c.roster.me().username,
            self.cfg.device.name,
            record.name
        )
    }

    fn local_party(&self, c: &Core, record: &SessionRecord) -> Party {
        Party {
            device: self.cfg.device.hex.clone(),
            device_name: self.cfg.device.name.clone(),
            person: c.roster.me().username.clone(),
            session: record.id.clone(),
            name: record.name.clone(),
        }
    }

    /// Wakes the waiters of a session and of every id merged into it.
    pub(crate) fn wake(&self, c: &Core, session: &str) {
        let id = c.reg.resolve_id(session);
        for sid in c.reg.ids_of(id) {
            self.waits.wake(&sid);
        }
    }

    // A wait that ends when `check` gives something, at `deadline`, or when the caller's connection closes
    // (7.5). It subscribes before each check.
    async fn wait_for<T>(
        &self,
        session: &str,
        deadline: Instant,
        conn: &Conn,
        mut check: impl FnMut(&Self) -> Result<Option<T>, RpcError>,
    ) -> Result<Option<T>, RpcError> {
        loop {
            let id = self.core().reg.resolve_id(session).to_string();
            let mut rx = self.waits.subscribe(&id);
            if let Some(found) = check(self)? {
                return Ok(Some(found));
            }
            if Instant::now() >= deadline || conn.is_closed() {
                return Ok(None);
            }
            tokio::select! {
                _ = rx.changed() => {}
                _ = tokio::time::sleep_until(deadline.into()) => return Ok(None),
                _ = conn.closed() => return Ok(None),
            }
        }
    }

    /// Stores a notice for a live session (7.6).
    pub(crate) fn notice(
        &self,
        c: &mut Core,
        to_session: &str,
        event: &NoticeEvent,
        related: Option<&str>,
    ) {
        let to_id = c.reg.resolve_id(to_session).to_string();
        let Ok(Some(target)) = c.store.session(&to_id) else {
            return;
        };
        if target.ended_at.is_some() {
            return;
        }
        let now = self.now();
        let id = ulid::Ulid::new().to_string();
        let me = c.roster.me().username.clone();
        let msg = Message {
            id: id.clone(),
            conversation: id,
            kind: MessageKind::Notice,
            in_reply_to: related.map(str::to_string),
            from: Party {
                device: self.cfg.device.hex.clone(),
                device_name: self.cfg.device.name.clone(),
                person: me,
                session: MESH.to_string(),
                name: MESH.to_string(),
            },
            from_client: MESH.to_string(),
            to: self.local_party(c, &target),
            body: notice_body(event),
            card: None,
            repo: None,
            room: None,
            sent_at: now,
            ttl_secs: DEFAULT_TTL_SECS,
            state: MessageState::Delivered,
            code: None,
        };
        match c.store.insert_message(&msg, now) {
            Ok(_) => self.wake(c, &to_id),
            Err(e) => tracing::warn!(error = %e, "could not store a notice"),
        }
    }

    /// Queues a receipt for the device that sent a message (7.3, 5.4).
    pub(crate) fn queue_receipt(
        &self,
        c: &mut Core,
        m: &StoredMessage,
        state: ReceiptState,
        code: Option<RefusalCode>,
    ) {
        if m.message.from.device == self.cfg.device.hex || m.message.kind == MessageKind::Notice {
            return;
        }
        let now = self.now();
        let update = ReceiptUpdate {
            id: m.message.id.clone(),
            state,
            code,
            at: now,
        };
        let Ok(payload) = serde_json::to_string(&update) else {
            return;
        };
        let entry = crate::store::NewOutbox {
            device: &m.message.from.device,
            kind: OutboxKind::Receipt,
            message_seq: Some(m.seq),
            payload: &payload,
            next_attempt_at: now,
            expires_at: now + DEFAULT_TTL_SECS * 1000,
        };
        match c.store.outbox_push(&entry, now) {
            Ok(_) => self.outbox_kick.notify_one(),
            Err(e) => tracing::warn!(error = %e, "could not queue a receipt"),
        }
    }

    /// Marks a received message shown and tells its sender (7.3).
    pub(crate) fn mark_shown(&self, c: &mut Core, m: &StoredMessage) {
        if m.message.state != MessageState::Delivered {
            return;
        }
        if let Ok(true) = c
            .store
            .set_state(m.seq, MessageState::Shown, None, self.now())
        {
            self.queue_receipt(c, m, ReceiptState::Shown, None);
            if m.message.from.device == self.cfg.device.hex {
                self.wake(c, &m.message.from.session);
            }
        }
    }

    /// Ends a session (6.4): its name and token go, its unread messages are refused with `session-ended`.
    pub(crate) fn end_session(&self, c: &mut Core, id: &str, reason: EndReason) {
        let now = self.now();
        let ids = c.reg.ids_of(id);
        match c.store.end_session(id, now) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::warn!(error = %e, "could not end a session");
                return;
            }
        }
        let name = c
            .store
            .session(id)
            .ok()
            .flatten()
            .map(|s| s.name)
            .unwrap_or_default();
        tracing::info!(session = %name, reason = reason.as_str(), "session ended");
        let _ = c.store.clear_cards(id);
        for sid in &ids {
            let unread = c
                .store
                .messages_to(sid, &[MessageState::Delivered])
                .unwrap_or_default();
            for m in unread {
                self.refuse_received(c, &m, RefusalCode::SessionEnded);
            }
        }
        c.reg.forget(id);
        c.gates.remove(id);
        for sid in &ids {
            self.waits.wake(sid);
            self.waits.forget(sid);
        }
        self.presence_changed();
    }

    // Refuses a stored message after it was accepted: the sender hears through a notice or a receipt.
    fn refuse_received(&self, c: &mut Core, m: &StoredMessage, code: RefusalCode) {
        let now = self.now();
        if !matches!(
            c.store
                .set_state(m.seq, MessageState::Refused, Some(code.as_str()), now),
            Ok(true)
        ) {
            return;
        }
        if m.message.kind == MessageKind::Notice {
            return;
        }
        if m.message.from.device == self.cfg.device.hex {
            let event = NoticeEvent::Refused {
                id: m.message.id.clone(),
                to: render::party_address(&m.message.to),
                code,
            };
            self.notice(c, &m.message.from.session, &event, Some(&m.message.id));
        } else {
            self.queue_receipt(c, m, ReceiptState::Refused, Some(code));
        }
    }

    /// Every session this device can see as directory entries: its own live sessions and, when
    /// `include_offline` is set, cached presence of offline devices too.
    pub(crate) fn entries(&self, c: &Core, include_offline: bool) -> Vec<Entry> {
        let me = c.roster.me().username.clone();
        let mut out = Vec::new();
        for s in c.store.live_sessions().unwrap_or_default() {
            if s.kind == SessionKind::Terminal {
                continue;
            }
            let cards = c.store.cards(&s.id).unwrap_or_default();
            let queue = self.queue_len(c, &s);
            let card_since = cards
                .iter()
                .map(|h| (h.card.clone(), h.refreshed_at))
                .collect();
            out.push(Entry {
                person: me.clone(),
                device: self.cfg.device.name.clone(),
                device_id: self.cfg.device.hex.clone(),
                this_device: true,
                own_device: true,
                online: true,
                session: local_presence(&s, &cards, queue).session,
                card_since,
            });
        }
        for (id, dev) in c.remote.devices() {
            let Some(admitted) = c.roster.admission().get(id) else {
                continue;
            };
            if !dev.online && !include_offline {
                continue;
            }
            let Some(snapshot) = &dev.snapshot else {
                continue;
            };
            for s in &snapshot.sessions {
                let card_since = s
                    .cards
                    .iter()
                    .map(|card| (card.clone(), dev.card_since(&s.session, card).unwrap_or(0)))
                    .collect();
                out.push(Entry {
                    person: admitted.person.clone(),
                    device: admitted.name.clone(),
                    device_id: hex_of(id),
                    this_device: false,
                    own_device: admitted.own,
                    online: dev.online,
                    session: s.clone(),
                    card_since,
                });
            }
        }
        out
    }

    /// A responder's waiting questions: asks and tells stored and not yet taken.
    pub(crate) fn queue_len(&self, c: &Core, s: &SessionRecord) -> u32 {
        if s.kind != SessionKind::Responder {
            return 0;
        }
        let waiting = c
            .reg
            .ids_of(&s.id)
            .iter()
            .flat_map(|sid| {
                c.store
                    .messages_to(sid, &[MessageState::Delivered])
                    .unwrap_or_default()
            })
            .filter(|m| matches!(m.message.kind, MessageKind::Ask | MessageKind::Tell))
            .count();
        u32::try_from(waiting).unwrap_or(u32::MAX)
    }

    /// `directory.list` (6.3).
    pub fn directory(&self, q: &DirectoryQuery) -> Result<Directory, RpcError> {
        let c = self.core();
        let caller = match &q.token {
            Some(token) => Some(c.reg.session_for_token(&c.store, token)?),
            None => None,
        };
        let entries = self.entries(&c, true);
        let links = self.links.status();
        let mut devices = vec![DeviceRow {
            endpoint_id: self.cfg.device.hex.clone(),
            name: self.cfg.device.name.clone(),
            person: c.roster.me().username.clone(),
            own: true,
            this: true,
            online: true,
            last_seen: None,
            path: None,
            rtt_ms: None,
        }];
        for admitted in c.roster.admission().devices() {
            let hex = hex_of(&admitted.endpoint_id);
            let remote = c.remote.get(&admitted.endpoint_id);
            let online = remote.is_some_and(|d| d.online);
            if !online && !q.include_offline {
                continue;
            }
            let link = links.links.iter().find(|l| l.device == hex);
            devices.push(DeviceRow {
                endpoint_id: hex,
                name: admitted.name.clone(),
                person: admitted.person.clone(),
                own: admitted.own,
                this: false,
                online,
                last_seen: if online {
                    None
                } else {
                    remote.and_then(|d| d.last_seen)
                },
                path: link.map(|l| l.path.clone()),
                rtt_ms: link.and_then(|l| l.rtt_ms),
            });
        }
        let room_names: HashMap<String, String> = c
            .roster
            .roster()
            .rooms
            .iter()
            .filter(|r| r.id != PRIVATE_ROOM_ID)
            .map(|r| (r.id.clone(), r.name.clone()))
            .collect();
        let unread = match &caller {
            Some(s) => self.unread_count(&c, &s.id),
            None => 0,
        };
        let me = c.roster.me().username.clone();
        Ok(listing(&ListingInput {
            me: &me,
            device: &self.cfg.device.name,
            entries: &entries,
            devices,
            this_session: caller.as_ref().map(|s| s.id.as_str()),
            filter: q.filter.as_deref(),
            include_offline: q.include_offline,
            room_names: &room_names,
            unread,
        }))
    }

    /// A session's unread messages.
    pub(crate) fn unread_count(&self, c: &Core, session: &str) -> usize {
        c.reg
            .ids_of(session)
            .iter()
            .map(|sid| {
                c.store
                    .messages_to(sid, &[MessageState::Delivered])
                    .map(|v| v.len())
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Takes a new roster (4.3): admission is recomputed, links to removed devices close, their presence goes,
    /// and mail queued for them fails.
    pub fn apply_roster(&self, me: Me, roster: Roster, info: RosterInfo) {
        let mut c = self.core();
        let me_changed = *c.roster.me() != me;
        let change = c.roster.replace(me, roster);
        c.roster_info = info;
        if change.is_empty() && !me_changed {
            return;
        }
        self.links.set_admission(c.roster.admission().clone());
        for device in &change.removed {
            let hex = hex_of(device);
            c.remote.remove(device);
            let _ = c.store.remove_presence(&hex);
            for entry in c.store.outbox_for(&hex).unwrap_or_default() {
                if entry.kind == OutboxKind::Deliver
                    && let Some(seq) = entry.message_seq
                    && let Ok(Some(m)) = c.store.message_by_seq(seq)
                    && matches!(
                        c.store
                            .set_state(seq, MessageState::Refused, Some(NOT_SHARED), self.now()),
                        Ok(true)
                    )
                {
                    let event = NoticeEvent::LeftRooms {
                        id: m.message.id.clone(),
                        to: render::party_address(&m.message.to),
                    };
                    self.notice(&mut c, &m.message.from.session, &event, Some(&m.message.id));
                }
                let _ = c.store.outbox_remove(entry.seq);
            }
            tracing::info!(device = %device.fmt_short(), "a device left every shared room");
        }
        self.presence_changed();
    }

    /// Takes a link event; returns the device whose outbox and presence should go out now.
    pub fn on_link_event(&self, event: &LinkEvent) -> Option<EndpointId> {
        let mut c = self.core();
        match event {
            LinkEvent::Up(device) => {
                c.remote.link_up(*device);
                Some(*device)
            }
            LinkEvent::Down {
                device,
                last_seen_ms,
            } => {
                c.remote.link_down(*device, *last_seen_ms);
                None
            }
        }
    }

    fn snapshot_for(&self, c: &Core, device: &EndpointId, seq: u64) -> Option<PresenceSnapshot> {
        let admitted = c.roster.admission().get(device)?;
        let locals: Vec<_> = c
            .store
            .live_sessions()
            .unwrap_or_default()
            .iter()
            .filter(|s| s.kind != SessionKind::Terminal)
            .map(|s| {
                let cards = c.store.cards(&s.id).unwrap_or_default();
                local_presence(s, &cards, self.queue_len(c, s))
            })
            .collect();
        let shared: Vec<String> = admitted
            .rooms
            .iter()
            .filter(|r| r.as_str() != PRIVATE_ROOM_ID)
            .cloned()
            .collect();
        let peer = if admitted.own {
            PresencePeer::Own
        } else {
            PresencePeer::Teammate {
                shared_rooms: &shared,
            }
        };
        Some(PresenceSnapshot {
            seq,
            device: PresenceDevice {
                name: self.cfg.device.name.clone(),
                platform: self.cfg.device.platform.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            sessions: filter_for_peer(&locals, peer, self.cfg.send_branch, self.cfg.send_place),
        })
    }

    /// Snapshots for `devices` (every online admitted device when `None`), each filtered for its peer (5.6),
    /// with how long to hold each so a peer never gets two within a second.
    pub fn presence_round(
        &self,
        devices: Option<&[EndpointId]>,
    ) -> Vec<(EndpointId, PresenceSnapshot, Duration)> {
        let mut c = self.core();
        let now = self.now();
        let seq = c.seq.next(now);
        let targets: Vec<EndpointId> = match devices {
            Some(list) => list.to_vec(),
            None => c.remote.online(),
        };
        let mut out = Vec::new();
        for device in targets {
            if !c.roster.admission().is_admitted(&device) {
                continue;
            }
            let Some(snapshot) = self.snapshot_for(&c, &device, seq) else {
                continue;
            };
            let last = c.presence_sent.get(&device).copied().unwrap_or(0);
            let hold = (last + crate::presence::MIN_ACCEPT_GAP_MS + 50).saturating_sub(now);
            c.presence_sent.insert(device, now + hold);
            out.push((device, snapshot, Duration::from_millis(hold)));
        }
        out
    }

    /// The periodic work (6.4, 7.4, 11.1): ends sessions by rules 2 to 4, expires mail, sweeps the store.
    /// Returns the devices whose outbox has entries due now.
    pub fn tick(&self) -> Vec<EndpointId> {
        let now = self.now();
        let mut c = self.core();
        let check_pids = now.saturating_sub(c.last_pid_check) >= crate::sessions::PID_CHECK_MS;
        if check_pids {
            c.last_pid_check = now;
        }
        let live = c.store.live_sessions().unwrap_or_default();
        let start_time = self.cfg.start_time;
        for (id, reason) in c.reg.due(&live, now, check_pids, &start_time) {
            self.end_session(&mut c, &id, reason);
        }
        self.expire_outbox(&mut c, now);
        if now.saturating_sub(c.last_expiry_check) >= EXPIRY_CHECK_MS {
            c.last_expiry_check = now;
            self.expire_delivered(&mut c, now);
            c.guard.sweep(now);
        }
        if now.saturating_sub(c.last_sweep) >= SWEEP_MS {
            c.last_sweep = now;
            match c.store.sweep(now) {
                Ok(swept) => tracing::debug!(?swept, "swept the store"),
                Err(e) => tracing::warn!(error = %e, "store sweep failed"),
            }
        }
        let mut due = Vec::new();
        for hex in c.store.outbox_devices().unwrap_or_default() {
            if c.pumping.contains(&hex) {
                continue;
            }
            let has_due = c
                .store
                .outbox_due(&hex, now)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            if has_due && let Some(id) = parse_endpoint_id(&hex) {
                due.push(id);
            }
        }
        due
    }

    // Mail whose time ran out before the target device accepted it (7.4).
    fn expire_outbox(&self, c: &mut Core, now: u64) {
        for entry in c.store.outbox_expired(now).unwrap_or_default() {
            if c.in_flight.contains(&entry.seq) {
                continue;
            }
            if entry.kind == OutboxKind::Deliver
                && let Some(seq) = entry.message_seq
                && let Ok(Some(m)) = c.store.message_by_seq(seq)
                && matches!(
                    c.store.set_state(seq, MessageState::Expired, None, now),
                    Ok(true)
                )
            {
                let event = NoticeEvent::Expired {
                    id: m.message.id.clone(),
                    to: render::party_address(&m.message.to),
                    delivered: false,
                };
                self.notice(c, &m.message.from.session, &event, Some(&m.message.id));
            }
            let _ = c.store.outbox_remove(entry.seq);
        }
    }

    // Stored messages nobody was shown before their time ran out (7.1).
    fn expire_delivered(&self, c: &mut Core, now: u64) {
        for s in c.store.live_sessions().unwrap_or_default() {
            for m in c
                .store
                .messages_to(&s.id, &[MessageState::Delivered])
                .unwrap_or_default()
            {
                let ends = m
                    .created_at
                    .saturating_add(m.message.ttl_secs.saturating_mul(1000));
                if now < ends || m.message.kind == MessageKind::Notice {
                    continue;
                }
                if !matches!(
                    c.store.set_state(m.seq, MessageState::Expired, None, now),
                    Ok(true)
                ) {
                    continue;
                }
                if m.message.from.device == self.cfg.device.hex {
                    let event = NoticeEvent::Expired {
                        id: m.message.id.clone(),
                        to: render::party_address(&m.message.to),
                        delivered: true,
                    };
                    self.notice(c, &m.message.from.session, &event, Some(&m.message.id));
                } else {
                    self.queue_receipt(c, &m, ReceiptState::Expired, None);
                }
            }
        }
    }

    /// A control connection closed: waits end on their own; sessions it held start their rule 3 grace.
    pub fn connection_closed(&self, conn: u64) {
        let now = self.now();
        self.core().reg.connection_closed(conn, now);
    }

    /// The roster part of `daemon.status`.
    pub fn roster_status(&self) -> RosterStatus {
        let c = self.core();
        let roster = c.roster.roster();
        RosterStatus {
            provider: c.roster_info.provider.clone(),
            configured: c.roster_info.configured.clone(),
            person: c.roster.me().username.clone(),
            version: roster.version.clone(),
            state: c.roster_info.state.clone(),
            problem: c.roster_info.problem.clone(),
            rooms: roster
                .rooms
                .iter()
                .filter(|r| r.id != PRIVATE_ROOM_ID)
                .map(|r| r.id.clone())
                .collect(),
            admitted: c.roster.admission().len(),
        }
    }

    /// The counts and network parts of `daemon.status`.
    pub fn counts(&self) -> (Counts, NetworkStatus) {
        let c = self.core();
        let live = c.store.live_sessions().unwrap_or_default();
        let unread = live.iter().map(|s| self.unread_count(&c, &s.id)).sum();
        let outbox = c
            .store
            .outbox_devices()
            .unwrap_or_default()
            .iter()
            .map(|d| c.store.outbox_for(d).map(|v| v.len()).unwrap_or(0))
            .sum();
        let counts = Counts {
            sessions: live
                .iter()
                .filter(|s| s.kind != SessionKind::Terminal)
                .count(),
            unread,
            outbox,
            online: c.remote.online().len(),
        };
        (counts, NetworkStatus::from(self.links.status()))
    }

    /// The live sessions, for tests and `doctor`.
    pub fn live_sessions(&self) -> Vec<SessionRecord> {
        self.core().store.live_sessions().unwrap_or_default()
    }

    /// The state of a message this device sent, by id.
    pub fn sent_state(&self, id: &str) -> Option<MessageState> {
        let c = self.core();
        c.store
            .message(&self.cfg.device.hex, id)
            .ok()
            .flatten()
            .map(|m| m.message.state)
    }
}

#[cfg(test)]
mod tests;
