//! Calls from admitted peers (5.4 to 5.6): `deliver` with checks 3 to 8 of 5.5, `presence`, `receipt` and
//! `ping`. Admission (step 1) and the per-peer rate (step 2) were applied by the link layer.

use serde_json::Value;

use super::{Core, Courier, render, warn_store};
use crate::address::is_ulid;
use crate::link::{Links, Peer};
use crate::responder::limits::{MAX_WAITING, Refusal};
use crate::roster::PRIVATE_ROOM_ID;
use crate::store::{
    Message, MessageState, OutboxKind, Party, Received, SessionRecord, StoredMessage,
};
use crate::text::NoticeEvent;
use crate::wire::{
    DeliverReceipt, DeliverState, Envelope, ErrorCode, Incoming, MessageKind, PresenceSnapshot,
    ReceiptState, ReceiptUpdate, RefusalCode, Response, SessionKind,
};

fn receipt(id: String, state: DeliverState, code: Option<RefusalCode>) -> Response {
    Response::Receipt(DeliverReceipt { id, state, code })
}

// A local store failure is passing trouble, not a verdict on the message: `rate-limited` is the one refusal a
// sender keeps queued and retries (5.5).
const STORE_TROUBLE: RefusalCode = RefusalCode::RateLimited;

impl<L: Links> Courier<L> {
    /// Answers one call from an admitted peer.
    pub async fn handle_inbound(&self, peer: Peer, call: Incoming) -> Response {
        match call {
            Incoming::Ping => Response::Pong,
            Incoming::Deliver(body) => self.deliver(&peer, body),
            Incoming::Presence(body) => self.take_presence(&peer, body),
            Incoming::Receipt(update) => {
                self.take_receipt(&peer, &update);
                Response::Ok
            }
            // `hello` belongs to the link layer and never reaches the daemon.
            Incoming::Hello(_) => Response::Error {
                code: ErrorCode::Malformed,
            },
        }
    }

    fn deliver(&self, peer: &Peer, body: Value) -> Response {
        let id_hint = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| is_ulid(s))
            .map(str::to_string);
        let envelope = match Envelope::parse(body) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(peer = %peer.device.fmt_short(), error = %e, "refused a malformed message");
                return match id_hint {
                    Some(id) => receipt(id, DeliverState::Refused, Some(RefusalCode::Invalid)),
                    None => Response::Error {
                        code: ErrorCode::Malformed,
                    },
                };
            }
        };
        let id = envelope.id.clone();
        let mut guard = self.core();
        match self.check_and_store(&mut guard, peer, envelope) {
            Ok(state) => receipt(id, state, None),
            Err(code) => {
                tracing::debug!(peer = %peer.device.fmt_short(), code = code.as_str(), "refused a message");
                receipt(id, DeliverState::Refused, Some(code))
            }
        }
    }

    // 5.5 steps 4 to 8.
    fn check_and_store(
        &self,
        c: &mut Core,
        peer: &Peer,
        env: Envelope,
    ) -> Result<DeliverState, RefusalCode> {
        let peer_hex = peer.device.to_string();
        let now = self.now();
        // 4. Dedupe.
        if c.store
            .is_duplicate(&peer_hex, &env.id)
            .map_err(|_| STORE_TROUBLE)?
        {
            return Ok(DeliverState::Duplicate);
        }
        // 5. Threading.
        env.check_thread_root().map_err(|_| RefusalCode::Invalid)?;
        let parent = match &env.in_reply_to {
            None => None,
            Some(parent_id) => Some(self.check_parent(c, &peer_hex, &env, parent_id)?),
        };
        // 6. Target and visibility.
        let target_id = c.reg.resolve_id(&env.to.session).to_string();
        let target = c
            .store
            .session(&target_id)
            .map_err(|_| STORE_TROUBLE)?
            .ok_or(RefusalCode::NoSuchSession)?;
        let shares_target_room = target.room.as_deref().is_some_and(|r| {
            r != PRIVATE_ROOM_ID && c.roster.admission().shares_room(&peer.device, r)
        });
        let replying = matches!(env.kind, MessageKind::Answer | MessageKind::Tell)
            && parent.as_ref().is_some_and(|p| {
                now < p
                    .created_at
                    .saturating_add(p.message.ttl_secs.saturating_mul(1000))
            });
        // Visibility comes before `session-ended`, so whether a session exists doesn't leak.
        if !(peer.own || shares_target_room || replying) {
            return Err(RefusalCode::NoSuchSession);
        }
        if target.ended_at.is_some() {
            return Err(RefusalCode::SessionEnded);
        }
        let room = self.message_room(c, peer, &target);
        // mesh#10: the gate is checked here and the run counted only once the message is stored, so a question
        // a later step refuses uses up nothing.
        let mut counted = None;
        if target.kind == SessionKind::Responder
            && matches!(env.kind, MessageKind::Ask | MessageKind::Tell)
        {
            let asker = crate::responder::Asker {
                person: peer.person.clone(),
                device: peer.device_name.clone(),
                device_id: peer_hex.clone(),
                session: env.from.session.clone(),
                name: env.from.name.clone(),
                client: env.from.client.clone(),
                own: peer.own,
                room: room.clone(),
            };
            let has_room = (self.queue_len(c, &target) as usize) < MAX_WAITING;
            self.responder_gate(c, &target)
                .check(now, &asker, shares_target_room, has_room)
                .map_err(|r| match r {
                    Refusal::Busy => RefusalCode::Busy,
                    Refusal::NoSuchSession => RefusalCode::NoSuchSession,
                })?;
            counted = Some(asker);
        }
        // 7. Conversation guard.
        if !c.guard.allows(&env.conversation, now) {
            return Err(RefusalCode::ConversationLimit);
        }
        // 8. Store.
        let msg = Message {
            id: env.id,
            conversation: env.conversation,
            kind: env.kind,
            in_reply_to: env.in_reply_to,
            from: Party {
                device: peer_hex,
                device_name: peer.device_name.clone(),
                person: peer.person.clone(),
                session: env.from.session,
                name: env.from.name,
            },
            from_client: env.from.client,
            to: self.local_party(c, &target),
            body: env.body,
            card: env.card,
            repo: env.repo,
            room,
            sent_at: env.sent_at,
            ttl_secs: env.ttl_secs,
            state: MessageState::Delivered,
            code: None,
        };
        match c.store.insert_received(&msg, now) {
            Ok(Received::Duplicate) => return Ok(DeliverState::Duplicate),
            Ok(Received::Stored(_)) => {}
            Err(e) => {
                tracing::warn!(error = %e, "could not store a received message");
                return Err(STORE_TROUBLE);
            }
        }
        c.guard.record(&msg.conversation, now);
        if let Some(asker) = counted {
            self.responder_gate(c, &target).record(now, &asker);
        }
        self.wake(c, &target.id);
        if target.kind == SessionKind::Responder {
            self.presence_changed();
        }
        if msg.kind == MessageKind::Answer
            && let Some(ask) = parent
        {
            self.answer_arrived(c, &ask, &msg);
        }
        Ok(DeliverState::Accepted)
    }

    // The message `in_reply_to` names (5.5 step 5): sent by this device from `to.session` to the peer, in the
    // same conversation; an answer only to an ask, and only one answer per ask.
    fn check_parent(
        &self,
        c: &Core,
        peer_hex: &str,
        env: &Envelope,
        parent_id: &str,
    ) -> Result<StoredMessage, RefusalCode> {
        let parent = c
            .store
            .message(&self.cfg.device.hex, parent_id)
            .map_err(|_| STORE_TROUBLE)?
            .ok_or(RefusalCode::Invalid)?;
        let p = &parent.message;
        if p.kind == MessageKind::Notice
            || p.from.session != env.to.session
            || p.to.device != peer_hex
            || p.conversation != env.conversation
        {
            return Err(RefusalCode::Invalid);
        }
        if env.kind == MessageKind::Answer {
            if p.kind != MessageKind::Ask {
                return Err(RefusalCode::Invalid);
            }
            let answered = c
                .store
                .conversation(&env.conversation)
                .map_err(|_| STORE_TROUBLE)?
                .iter()
                .any(|m| {
                    m.message.kind == MessageKind::Answer
                        && m.message.from.device == peer_hex
                        && m.message.in_reply_to.as_deref() == Some(parent_id)
                });
            if answered {
                return Err(RefusalCode::Invalid);
            }
        }
        Ok(parent)
    }

    // The room a received message came through: the target session's room when the sender shares it, else none
    // (5.5). A reply reaching a private session names no room rather than an unrelated shared one.
    fn message_room(&self, c: &Core, peer: &Peer, target: &SessionRecord) -> Option<String> {
        let admitted = c.roster.admission().get(&peer.device)?;
        target
            .room
            .as_deref()
            .filter(|r| *r != PRIVATE_ROOM_ID && admitted.rooms.contains(*r))
            .map(str::to_string)
    }

    // An answer to one of this device's asks was stored: the ask is answered, and when no `ask` call waits for
    // it any more the asking session gets a notice (7.6).
    pub(crate) fn answer_arrived(&self, c: &mut Core, ask: &StoredMessage, answer: &Message) {
        let now = self.now();
        warn_store(
            c.store
                .set_state(ask.seq, MessageState::Answered, None, now),
            "could not mark an ask answered",
        );
        if c.waiting_asks.contains_key(&ask.message.id) {
            return;
        }
        let event = NoticeEvent::AnsweredLate {
            ask_id: ask.message.id.clone(),
            to: render::party_address(&ask.message.to),
            answer_id: answer.id.clone(),
        };
        self.notice(c, &ask.message.from.session, &event, Some(&ask.message.id));
    }

    fn take_presence(&self, peer: &Peer, body: Value) -> Response {
        let snapshot = match PresenceSnapshot::parse(body) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(peer = %peer.device.fmt_short(), error = %e.0, "dropped a malformed presence");
                return Response::Error {
                    code: ErrorCode::Malformed,
                };
            }
        };
        let now = self.now();
        let mut c = self.core();
        let seq = snapshot.seq;
        let text = serde_json::to_string(&snapshot).ok();
        if c.remote.accept(peer.device, snapshot, now).is_ok()
            && let Some(text) = text
            && let Err(e) = c
                .store
                .put_presence(&peer.device.to_string(), seq, &text, now)
        {
            tracing::warn!(error = %e, "could not cache presence");
        }
        Response::Ok
    }

    // A later state of a message this device sent, from the device it went to (5.4).
    fn take_receipt(&self, peer: &Peer, u: &ReceiptUpdate) {
        let mut guard = self.core();
        let c = &mut *guard;
        let peer_hex = peer.device.to_string();
        let Ok(Some(m)) = c.store.message(&self.cfg.device.hex, &u.id) else {
            return;
        };
        if m.message.to.device != peer_hex || m.message.kind == MessageKind::Notice {
            return;
        }
        // Only an ask can be answered (7.1), and a refusal always says why; anything else is ignored.
        let (state, code) = match (u.state, u.code) {
            (ReceiptState::Shown, _) => (MessageState::Shown, None),
            (ReceiptState::Answered, _) if m.message.kind == MessageKind::Ask => {
                (MessageState::Answered, None)
            }
            (ReceiptState::Answered, _) => return,
            (ReceiptState::Expired, _) => (MessageState::Expired, None),
            (ReceiptState::Refused, Some(code)) => (MessageState::Refused, Some(code)),
            (ReceiptState::Refused, None) => return,
        };
        let now = self.now();
        if !matches!(
            c.store
                .set_state(m.seq, state, code.map(RefusalCode::as_str), now),
            Ok(true)
        ) {
            return;
        }
        // A receipt can overtake the deliver's own answer; the device has the message either way.
        if m.message.state == MessageState::Queued {
            let queued = warn_store(c.store.outbox_for(&peer_hex), "could not read the outbox");
            for entry in queued.unwrap_or_default() {
                if entry.kind == OutboxKind::Deliver
                    && entry.message_seq == Some(m.seq)
                    && !c.in_flight.contains(&entry.seq)
                {
                    warn_store(
                        c.store.outbox_remove(entry.seq),
                        "could not drop a delivered outbox entry",
                    );
                }
            }
        }
        let to = render::party_address(&m.message.to);
        let event = match (u.state, code) {
            (ReceiptState::Refused, Some(code)) => Some(NoticeEvent::Refused {
                id: m.message.id.clone(),
                to,
                code,
            }),
            (ReceiptState::Expired, _) => Some(NoticeEvent::Expired {
                id: m.message.id.clone(),
                to,
                delivered: true,
            }),
            _ => None,
        };
        if let Some(event) = event {
            self.notice(c, &m.message.from.session, &event, Some(&m.message.id));
        }
        self.wake(c, &m.message.from.session);
    }
}
