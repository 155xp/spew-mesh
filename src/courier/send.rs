//! Sending (7.1, 7.2): `ask`, `tell` and `reply`, local delivery, the first delivery attempt to another device,
//! and waiting for answers.

use std::time::{Duration, Instant};

use super::outbox::{Attempt, read_deliver_response};
use super::{Core, Courier, RECEIPT_WAIT, refusal_reason, render, store_err};
use crate::address::{Address, is_card};
use crate::control::protocol::{
    AnswerView, AskParams, ReplyParams, RpcError, SendResult, SendStatus, TellParams,
};
use crate::control::server::Conn;
use crate::identity::parse_endpoint_id;
use crate::link::Links;
use crate::resolve::{Asker, resolve};
use crate::responder::limits::{Gate, MAX_WAITING, Refusal};
use crate::sessions::directory::Entry;
use crate::store::{
    Message, MessageState, NewOutbox, OutboxKind, Party, SessionRecord, StoredMessage,
};
use crate::text::{Block, NoticeEvent, render_batch, sanitize_text};
use crate::wire::{
    ActivityState, DEFAULT_TTL_SECS, Envelope, MAX_BODY, MessageKind, ReceiptState, Recipient,
    RefusalCode, Request, Sender, SessionKind,
};

/// Default `ask` wait (7.2).
pub const DEFAULT_WAIT_SECS: u64 = 100;
/// Longest `ask` wait (7.2).
pub const MAX_WAIT_SECS: u64 = 900;
/// How long an ask to an idle live session waits (7.2).
pub const IDLE_WAIT: Duration = Duration::from_secs(10);

/// Where a new message goes.
#[derive(Debug, Clone)]
struct Target {
    party: Party,
    /// None for a session on this device.
    device: Option<iroh::EndpointId>,
    kind: SessionKind,
    state: ActivityState,
    room: Option<String>,
    address: String,
}

impl Target {
    fn from_entry(e: &Entry) -> Target {
        Target {
            party: Party {
                device: e.device_id.clone(),
                device_name: e.device.clone(),
                person: e.person.clone(),
                session: e.session.session.clone(),
                name: e.session.name.clone(),
            },
            device: if e.this_device {
                None
            } else {
                parse_endpoint_id(&e.device_id)
            },
            kind: e.session.kind,
            state: e.session.state,
            room: e.session.room.clone(),
            address: e.address(),
        }
    }
}

// An outbox entry to try at once: entry row, message row and the envelope.
type FirstTry = (i64, i64, Envelope);

/// How a send went, before any wait.
#[derive(Debug, Clone)]
enum Sent {
    Delivered,
    Queued(String),
    Refused(RefusalCode),
}

struct Outgoing {
    kind: MessageKind,
    conversation: Option<String>,
    in_reply_to: Option<String>,
    body: String,
    card: Option<String>,
}

/// Checks and sanitizes a message text (7.1).
pub fn check_text(text: &str) -> Result<String, RpcError> {
    let body = sanitize_text(text);
    if body.trim().is_empty() {
        return Err(RpcError::invalid("the text is empty"));
    }
    if body.len() > MAX_BODY {
        return Err(RpcError::invalid(format!(
            "the text is {} bytes; a message holds at most 32 KiB",
            body.len()
        )));
    }
    Ok(body)
}

fn check_card(card: Option<&str>) -> Result<Option<String>, RpcError> {
    match card.map(|c| c.trim().to_ascii_lowercase()) {
        None => Ok(None),
        Some(c) if c.is_empty() => Ok(None),
        Some(c) if is_card(&c) => Ok(Some(c)),
        Some(c) => Err(RpcError::invalid(format!(
            "`{c}` is not a card ref: use <project key>#<number>, such as spew#42"
        ))),
    }
}

fn wait_secs(given: Option<u64>) -> Duration {
    Duration::from_secs(given.unwrap_or(DEFAULT_WAIT_SECS).min(MAX_WAIT_SECS))
}

impl<L: Links> Courier<L> {
    // The target an address names, first among online sessions, then among offline devices' last presence.
    fn resolve_target(
        &self,
        c: &Core,
        sender: &SessionRecord,
        to: &str,
    ) -> Result<Target, RpcError> {
        let address = Address::parse(to).map_err(|e| RpcError::invalid(e.to_string()))?;
        let me = c.roster.me().username.clone();
        let asker = Asker {
            person: &me,
            session: Some(&sender.id),
            repo: sender.repo.as_deref(),
        };
        let online = self.entries(c, false);
        match resolve(&address, &asker, &online) {
            Ok(e) => Ok(Target::from_entry(e)),
            Err(err) if err.is_not_found() => {
                let all = self.entries(c, true);
                resolve(&address, &asker, &all)
                    .map(Target::from_entry)
                    .map_err(|_| err.rpc())
            }
            Err(err) => Err(err.rpc()),
        }
    }

    pub(super) fn responder_gate<'c>(
        &self,
        c: &'c mut Core,
        target: &SessionRecord,
    ) -> &'c mut Gate {
        c.gates
            .entry(target.id.clone())
            .or_insert_with(|| Gate::new(&self.cfg.serve))
    }

    // The checks a local delivery shares with 5.5 steps 6 and 7.
    fn accept_local(
        &self,
        c: &mut Core,
        msg: &Message,
        target: &SessionRecord,
    ) -> Result<(), RefusalCode> {
        let now = self.now();
        if target.kind == SessionKind::Responder
            && matches!(msg.kind, MessageKind::Ask | MessageKind::Tell)
        {
            let room = target.room.clone();
            let has_room = (self.queue_len(c, target) as usize) < MAX_WAITING;
            let asker = crate::responder::Asker {
                person: msg.from.person.clone(),
                device: msg.from.device_name.clone(),
                device_id: msg.from.device.clone(),
                session: msg.from.session.clone(),
                name: msg.from.name.clone(),
                client: msg.from_client.clone(),
                own: true,
                room,
            };
            self.responder_gate(c, target)
                .admit(now, &asker, true, has_room)
                .map_err(|r| match r {
                    Refusal::Busy => RefusalCode::Busy,
                    Refusal::NoSuchSession => RefusalCode::NoSuchSession,
                })?;
        }
        if !c.guard.allows(&msg.conversation, now) {
            return Err(RefusalCode::ConversationLimit);
        }
        Ok(())
    }

    // Stores and routes one message. Returns the stored row (when stored), how it went, and for another device
    // the outbox entry and envelope to try right away.
    fn route(
        &self,
        c: &mut Core,
        sender: &SessionRecord,
        target: &Target,
        out: Outgoing,
    ) -> Result<(Message, Sent, Option<FirstTry>), RpcError> {
        let now = self.now();
        let id = ulid::Ulid::new().to_string();
        let msg = Message {
            conversation: out.conversation.unwrap_or_else(|| id.clone()),
            id,
            kind: out.kind,
            in_reply_to: out.in_reply_to,
            from: self.local_party(c, sender),
            from_client: sender.client.clone(),
            to: target.party.clone(),
            body: out.body,
            card: out.card,
            repo: sender.repo.clone(),
            room: target.room.clone(),
            sent_at: now,
            ttl_secs: DEFAULT_TTL_SECS,
            state: MessageState::Delivered,
            code: None,
        };
        let Some(device) = target.device else {
            let target_id = c.reg.resolve_id(&target.party.session).to_string();
            let record = c
                .store
                .session(&target_id)
                .map_err(store_err)?
                .filter(|s| s.ended_at.is_none());
            let Some(record) = record else {
                return Ok((msg, Sent::Refused(RefusalCode::SessionEnded), None));
            };
            if let Err(code) = self.accept_local(c, &msg, &record) {
                return Ok((msg, Sent::Refused(code), None));
            }
            let mut msg = msg;
            msg.to.session.clone_from(&record.id);
            c.store.insert_message(&msg, now).map_err(store_err)?;
            c.guard.record(&msg.conversation, now);
            self.wake(c, &record.id);
            if record.kind == SessionKind::Responder {
                self.presence_changed();
            }
            if msg.kind == MessageKind::Answer
                && let Some(ask_id) = msg.in_reply_to.clone()
                && !c.waiting_asks.contains_key(&ask_id)
            {
                let event = NoticeEvent::AnsweredLate {
                    ask_id: ask_id.clone(),
                    to: render::party_address(&msg.from),
                    answer_id: msg.id.clone(),
                };
                self.notice(c, &record.id, &event, Some(&ask_id));
            }
            return Ok((msg, Sent::Delivered, None));
        };
        if !c.guard.allows(&msg.conversation, now) {
            return Ok((msg, Sent::Refused(RefusalCode::ConversationLimit), None));
        }
        let mut msg = msg;
        msg.state = MessageState::Queued;
        let envelope = Envelope {
            id: msg.id.clone(),
            conversation: msg.conversation.clone(),
            kind: msg.kind,
            in_reply_to: msg.in_reply_to.clone(),
            from: Sender {
                session: sender.id.clone(),
                name: sender.name.clone(),
                client: sender.client.clone(),
            },
            to: Recipient {
                session: target.party.session.clone(),
                name: target.party.name.clone(),
            },
            body: msg.body.clone(),
            card: msg.card.clone(),
            repo: msg.repo.clone(),
            sent_at: msg.sent_at,
            ttl_secs: msg.ttl_secs,
        };
        let payload = serde_json::to_string(&envelope)
            .map_err(|e| RpcError::unavailable(format!("could not encode the message: {e}")))?;
        let message_seq = c.store.insert_message(&msg, now).map_err(store_err)?;
        c.guard.record(&msg.conversation, now);
        let entry_seq = c
            .store
            .outbox_push(
                &NewOutbox {
                    device: &target.party.device,
                    kind: OutboxKind::Deliver,
                    message_seq: Some(message_seq),
                    payload: &payload,
                    next_attempt_at: now,
                    expires_at: now + msg.ttl_secs * 1000,
                },
                now,
            )
            .map_err(store_err)?;
        // The first retry comes 10 s from now whether or not the attempt below happens (7.4).
        c.store
            .outbox_retry(entry_seq, now + super::outbox::retry_delay_ms(1))
            .map_err(store_err)?;
        if !c.remote.is_online(&device) {
            return Ok((
                msg,
                Sent::Queued(format!("{} is offline", target.party.device_name)),
                None,
            ));
        }
        c.in_flight.insert(entry_seq);
        Ok((
            msg,
            Sent::Queued(String::new()),
            Some((entry_seq, message_seq, envelope)),
        ))
    }

    // The first delivery attempt while the send call waits, up to 8 s (7.1).
    async fn first_attempt(
        &self,
        device: iroh::EndpointId,
        target: &Target,
        entry_seq: i64,
        message_seq: i64,
        envelope: Envelope,
    ) -> Sent {
        let id = envelope.id.clone();
        let response = self
            .call(device, Request::Deliver(envelope), RECEIPT_WAIT)
            .await;
        let attempt = read_deliver_response(&id, response);
        let mut c = self.core();
        c.in_flight.remove(&entry_seq);
        self.settle_deliver(&mut c, entry_seq, message_seq, 1, &attempt, true);
        match attempt {
            Attempt::Delivered => Sent::Delivered,
            Attempt::Refused(code) => Sent::Refused(code),
            Attempt::Retry { reason, .. } => {
                let _ = target;
                Sent::Queued(format!("no receipt yet ({reason})"))
            }
        }
    }

    // Sends one message from `sender` to `target` and reports how it went.
    async fn send_one(
        &self,
        sender: SessionRecord,
        target: Target,
        out: Outgoing,
        waiting_ask: bool,
    ) -> Result<(Message, Sent), RpcError> {
        let (msg, sent, attempt) = {
            let mut c = self.core();
            let routed = self.route(&mut c, &sender, &target, out)?;
            if waiting_ask && !matches!(routed.1, Sent::Refused(_)) {
                *c.waiting_asks.entry(routed.0.id.clone()).or_default() += 1;
            }
            routed
        };
        let sent = match (attempt, target.device) {
            (Some((entry_seq, message_seq, envelope)), Some(device)) => {
                self.first_attempt(device, &target, entry_seq, message_seq, envelope)
                    .await
            }
            _ => sent,
        };
        if let Sent::Queued(_) = sent {
            self.outbox_kick.notify_one();
        }
        Ok((msg, sent))
    }

    fn result(
        &self,
        msg: &Message,
        target: &Target,
        sent: &Sent,
        note: String,
        unread: usize,
    ) -> SendResult {
        let (status, code, reason) = match sent {
            Sent::Delivered => (SendStatus::Delivered, None, None),
            Sent::Queued(why) => (
                SendStatus::Queued,
                None,
                Some(why.clone()).filter(|w| !w.is_empty()),
            ),
            Sent::Refused(code) => (
                SendStatus::Refused,
                Some(code.as_str().to_string()),
                Some(refusal_reason(*code).to_string()),
            ),
        };
        SendResult {
            status,
            id: msg.id.clone(),
            conversation: msg.conversation.clone(),
            to: target.address.clone(),
            code,
            reason,
            note,
            answer: None,
            unread,
        }
    }

    fn sent_note(target: &Target, sent: &Sent) -> String {
        match sent {
            Sent::Delivered => {
                let mut note = format!("Delivered to {}.", target.address);
                if target.kind == SessionKind::Live && target.state == ActivityState::Idle {
                    note.push_str(" It is idle and sees this at its next prompt.");
                }
                note
            }
            Sent::Queued(why) if why.is_empty() => format!(
                "Queued for {}; it is sent when that device is reachable.",
                target.address
            ),
            Sent::Queued(why) => format!(
                "Queued for {} ({why}); it is sent when that device is reachable.",
                target.address
            ),
            Sent::Refused(code) => {
                format!("{} refused it: {}.", target.address, refusal_reason(*code))
            }
        }
    }

    /// `message.tell` (7.2).
    pub async fn tell(&self, p: TellParams) -> Result<SendResult, RpcError> {
        let body = check_text(&p.text)?;
        let card = check_card(p.card.as_deref())?;
        let (sender, target) = {
            let c = self.core();
            let sender = c.reg.session_for_token(&c.store, &p.token)?;
            let target = self.resolve_target(&c, &sender, &p.to)?;
            (sender, target)
        };
        let out = Outgoing {
            kind: MessageKind::Tell,
            conversation: None,
            in_reply_to: None,
            body,
            card,
        };
        let sender_id = sender.id.clone();
        let (msg, sent) = self.send_one(sender, target.clone(), out, false).await?;
        let unread = self.unread_count(&self.core(), &sender_id);
        Ok(self.result(
            &msg,
            &target,
            &sent,
            Self::sent_note(&target, &sent),
            unread,
        ))
    }

    /// `message.ask` (7.2): sends and waits for the answer.
    pub async fn ask(&self, conn: &Conn, p: AskParams) -> Result<SendResult, RpcError> {
        let body = check_text(&p.text)?;
        let card = check_card(p.card.as_deref())?;
        let wait = wait_secs(p.wait_seconds);
        let (sender, target) = {
            let c = self.core();
            let sender = c.reg.session_for_token(&c.store, &p.token)?;
            let target = self.resolve_target(&c, &sender, &p.to)?;
            (sender, target)
        };
        let out = Outgoing {
            kind: MessageKind::Ask,
            conversation: None,
            in_reply_to: None,
            body,
            card,
        };
        self.ask_and_wait(conn, sender, target, out, wait).await
    }

    async fn ask_and_wait(
        &self,
        conn: &Conn,
        sender: SessionRecord,
        target: Target,
        out: Outgoing,
        wait: Duration,
    ) -> Result<SendResult, RpcError> {
        let started = Instant::now();
        let sender_id = sender.id.clone();
        let (msg, sent) = self.send_one(sender, target.clone(), out, true).await?;
        if let Sent::Refused(_) = sent {
            let unread = self.unread_count(&self.core(), &sender_id);
            return Ok(self.result(
                &msg,
                &target,
                &sent,
                Self::sent_note(&target, &sent),
                unread,
            ));
        }
        let idle = target.kind == SessionKind::Live && target.state == ActivityState::Idle;
        let limit = if idle { wait.min(IDLE_WAIT) } else { wait };
        let deadline = started + limit;
        let ask_id = msg.id.clone();
        let found = self
            .wait_for(&sender_id, deadline, conn, |me| {
                me.find_answer(&sender_id, &msg)
            })
            .await;
        {
            let mut c = self.core();
            if let Some(n) = c.waiting_asks.get_mut(&ask_id) {
                *n -= 1;
                if *n == 0 {
                    c.waiting_asks.remove(&ask_id);
                }
            }
        }
        let found = found?;
        let mut c = self.core();
        let unread_after = |c: &Core| self.unread_count(c, &sender_id);
        if let Some(answer) = found {
            self.mark_shown(&mut c, &answer);
            let from = render::sender_address(&answer);
            let block: Block = render::block(&answer, &c.roster.me().username, false);
            let view = AnswerView {
                id: answer.message.id.clone(),
                from: from.clone(),
                body: answer.message.body.clone(),
                text: render_batch(&[block], None).text,
            };
            let mut result = self.result(
                &msg,
                &target,
                &Sent::Delivered,
                format!("{from} answered."),
                unread_after(&c),
            );
            result.status = SendStatus::Answered;
            result.answer = Some(view);
            return Ok(result);
        }
        let state = c
            .store
            .message(&self.cfg.device.hex, &ask_id)
            .ok()
            .flatten()
            .map(|m| (m.message.state, m.message.code.clone()));
        let later = "the answer will arrive with your other messages.";
        let (status, note) = match state {
            Some((MessageState::Queued, _)) => (
                SendStatus::Queued,
                format!("The question to {} is queued; {later}", target.address),
            ),
            Some((MessageState::Delivered, _)) if idle => (
                SendStatus::Delivered,
                format!(
                    "{} is idle and sees this at its next prompt; {later}",
                    target.address
                ),
            ),
            Some((MessageState::Delivered, _)) => (
                SendStatus::Delivered,
                format!(
                    "The question to {} is delivered, not seen yet; {later}",
                    target.address
                ),
            ),
            Some((MessageState::Shown, _)) => (
                SendStatus::Shown,
                format!(
                    "{} has seen the question, no answer yet; {later}",
                    target.address
                ),
            ),
            Some((MessageState::Answered, _)) => (
                SendStatus::Answered,
                format!(
                    "{} answered; the answer is with your other messages.",
                    target.address
                ),
            ),
            Some((MessageState::Refused, code)) => {
                let code = code.and_then(|c| RefusalCode::from_name(&c));
                let mut r = self.result(
                    &msg,
                    &target,
                    &Sent::Refused(code.unwrap_or(RefusalCode::NoSuchSession)),
                    String::new(),
                    unread_after(&c),
                );
                r.note = match code {
                    Some(code) => {
                        format!("{} refused it: {}.", target.address, refusal_reason(code))
                    }
                    None => format!("{} is no longer in a room you share.", target.address),
                };
                return Ok(r);
            }
            Some((MessageState::Expired, _)) => (
                SendStatus::Expired,
                format!(
                    "The question to {} expired before it was answered.",
                    target.address
                ),
            ),
            None => (
                SendStatus::Queued,
                format!("The question to {} is queued; {later}", target.address),
            ),
        };
        let mut result = self.result(&msg, &target, &sent, note, unread_after(&c));
        result.status = status;
        if status != SendStatus::Queued {
            result.reason = None;
        }
        Ok(result)
    }

    // The answer to `ask` among the messages to `asker`, if it came.
    fn find_answer(&self, asker: &str, ask: &Message) -> Result<Option<StoredMessage>, RpcError> {
        let c = self.core();
        let id = c.reg.resolve_id(asker).to_string();
        match c.store.session(&id).map_err(store_err)? {
            Some(s) if s.ended_at.is_none() => {}
            _ => return Err(RpcError::not_found("the asking session ended")),
        }
        let ids = c.reg.ids_of(&id);
        let found = c
            .store
            .conversation(&ask.conversation)
            .map_err(store_err)?
            .into_iter()
            .find(|m| {
                m.message.kind == MessageKind::Answer
                    && m.message.in_reply_to.as_deref() == Some(ask.id.as_str())
                    && ids.contains(&m.message.to.session)
                    && m.message.to.device == self.cfg.device.hex
            });
        Ok(found)
    }

    /// `message.reply` (7.2): answers a message the calling session received.
    pub async fn reply(&self, conn: &Conn, p: ReplyParams) -> Result<SendResult, RpcError> {
        let body = check_text(&p.text)?;
        let (sender, target, out, follow_up) = {
            let mut c = self.core();
            let sender = c.reg.session_for_token(&c.store, &p.token)?;
            let mut received = Vec::new();
            for sid in c.reg.ids_of(&sender.id) {
                received.extend(c.store.received_by_id(&sid, &p.id).map_err(store_err)?);
            }
            let original = received
                .into_iter()
                .filter(|m| m.message.kind != MessageKind::Notice && m.message.to.device == self.cfg.device.hex)
                .max_by_key(|m| m.seq)
                .ok_or_else(|| {
                    RpcError::not_found(format!(
                        "no message {} came to this session; reply answers a message it received (the messages tool lists them)",
                        p.id
                    ))
                })?;
            let o = &original.message;
            let answers_ask = o.kind == MessageKind::Ask && o.state != MessageState::Answered;
            let follow_up = p.wait_seconds.is_some() && o.kind != MessageKind::Ask;
            let kind = if follow_up {
                MessageKind::Ask
            } else if answers_ask {
                MessageKind::Answer
            } else {
                MessageKind::Tell
            };
            let local = o.from.device == self.cfg.device.hex;
            let target = Target {
                party: o.from.clone(),
                device: if local {
                    None
                } else {
                    parse_endpoint_id(&o.from.device)
                },
                kind: SessionKind::Live,
                state: ActivityState::Unknown,
                room: o.room.clone(),
                address: render::party_address(&o.from),
            };
            if !local && target.device.is_none() {
                return Err(RpcError::invalid(
                    "that message came from a device with a bad id",
                ));
            }
            let now = self.now();
            if kind == MessageKind::Answer {
                if let Ok(true) = c
                    .store
                    .set_state(original.seq, MessageState::Answered, None, now)
                {
                    self.queue_receipt(&mut c, &original, ReceiptState::Answered, None);
                }
            } else {
                self.mark_shown(&mut c, &original);
            }
            if local {
                self.wake(&c, &o.from.session);
            }
            if sender.kind == SessionKind::Responder {
                let mut s = sender.clone();
                if self.queue_len(&c, &s) == 0 && s.state != ActivityState::Idle {
                    s.state = ActivityState::Idle;
                    s.state_since = now;
                    let _ = c.store.put_session(&s);
                    self.presence_changed();
                }
            }
            let out = Outgoing {
                kind,
                conversation: Some(o.conversation.clone()),
                in_reply_to: Some(o.id.clone()),
                body,
                card: o.card.clone(),
            };
            (sender, target, out, follow_up)
        };
        if follow_up {
            return self
                .ask_and_wait(conn, sender, target, out, wait_secs(p.wait_seconds))
                .await;
        }
        let sender_id = sender.id.clone();
        let (msg, sent) = self.send_one(sender, target.clone(), out, false).await?;
        let unread = self.unread_count(&self.core(), &sender_id);
        Ok(self.result(
            &msg,
            &target,
            &sent,
            Self::sent_note(&target, &sent),
            unread,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_sanitized_and_bounded() {
        assert_eq!(check_text("hi\u{202e}there").unwrap(), "hithere");
        assert!(check_text("  \n ").is_err());
        assert!(check_text(&"x".repeat(MAX_BODY + 1)).is_err());
        assert!(check_text(&"x".repeat(MAX_BODY)).is_ok());
        assert_eq!(
            check_card(Some("SPEW#42")).unwrap().as_deref(),
            Some("spew#42")
        );
        assert!(check_card(Some("nope")).is_err());
        assert_eq!(wait_secs(None), Duration::from_secs(100));
        assert_eq!(wait_secs(Some(5000)), Duration::from_secs(900));
    }
}
