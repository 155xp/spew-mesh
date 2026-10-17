//! Session operations of the control socket (6.2, 6.4) and getting messages in front of agents (7.3, 8.1):
//! `session.*`, `hook.event`, `messages.ack`, `messages.fetch` and `responder.take`.

use std::time::{Duration, Instant};

use super::send::MAX_WAIT_SECS;
use super::{Core, Courier, render, store_err, warn_store};
use crate::address::is_card;
use crate::agents::cards::{self, ActivityUpdate};
use crate::agents::hook::{HookAnswer, HookCall, MeshEvent};
use crate::control::protocol::{
    Ack, AskerView, FetchParams, Fetched, QuestionView, RpcError, SessionOpen, SessionOpened,
    SessionUpdate, TakeParams, Taken,
};
use crate::control::server::Conn;
use crate::link::Links;
use crate::sessions::{EndReason, OpenRequest, apply_cwd};
use crate::store::{MessageState, SessionRecord, StoredMessage};
use crate::text::{INJECTION_BUDGET, render_batch, sanitize_line, start_line, unanswered_line};
use crate::wire::{ActivityState, MAX_ACTIVITY, MessageKind, Reach, SessionKind};

/// The most cards one `session.update` may name.
const MAX_CARD_CHANGES: usize = 50;
/// The most text one `messages.fetch` returns; the rest stays unread and is counted in "N more".
pub const FETCH_BUDGET: usize = 64 * 1024;

fn check_cards(list: &[String]) -> Result<Vec<String>, RpcError> {
    if list.len() > MAX_CARD_CHANGES {
        return Err(RpcError::invalid("too many cards in one update"));
    }
    list.iter()
        .map(|c| {
            let c = c.trim().to_ascii_lowercase();
            if is_card(&c) {
                Ok(c)
            } else {
                Err(RpcError::invalid(format!(
                    "`{c}` is not a card ref: use <project key>#<number>, such as spew#42"
                )))
            }
        })
        .collect()
}

fn card_project(card: &str) -> &str {
    card.split('#').next().unwrap_or_default()
}

// Moves a session to `state`; true when that changed it.
fn set_state(record: &mut SessionRecord, state: ActivityState, now: u64) -> bool {
    if record.state == state {
        return false;
    }
    record.state = state;
    record.state_since = now;
    true
}

impl<L: Links> Courier<L> {
    /// `session.open` (6.2): opens a session, or re-attaches the one the request belongs to (8.2 to 8.4).
    pub fn open_session(&self, conn: &Conn, p: SessionOpen) -> Result<SessionOpened, RpcError> {
        let now = self.now();
        let reach = match p.kind {
            SessionKind::Responder => Reach::Serve,
            _ => Reach::Inbox,
        };
        let req = OpenRequest {
            client: p.client,
            client_session: p.client_session,
            client_pid: p.client_pid,
            cwd: p.cwd,
            kind: p.kind,
            name: p.name,
            spew_project: p.spew_project,
            reach,
        };
        let mut guard = self.core();
        let c = &mut *guard;
        let ctx = self.ctx(now);
        let opened = c.reg.open(&c.store, &req, &ctx)?;
        let token = opened
            .token
            .ok_or_else(|| RpcError::unavailable("no token was issued for the session"))?;
        c.reg
            .attach(&c.store, conn.id(), &opened.session.id)
            .map_err(store_err)?;
        for gone in &opened.merged {
            self.waits.wake(gone);
        }
        if opened.session.kind != SessionKind::Terminal {
            if opened.created {
                tracing::info!(session = %opened.session.name, client = %opened.session.client, "session opened");
            }
            self.presence_changed();
        }
        Ok(SessionOpened {
            session: opened.session.id.clone(),
            name: opened.session.name.clone(),
            address: self.address(c, &opened.session),
            token,
        })
    }

    /// `session.update` (6.2).
    pub fn update_session(&self, p: SessionUpdate) -> Result<(), RpcError> {
        let now = self.now();
        let add = check_cards(&p.cards_add)?;
        let remove = check_cards(&p.cards_remove)?;
        if let Some(cwd) = &p.cwd
            && (cwd.is_empty() || cwd.contains('\0'))
        {
            return Err(RpcError::invalid("cwd must be a folder path"));
        }
        if let Some(cs) = &p.client_session
            && (cs.is_empty() || cs.len() > 200 || cs.chars().any(char::is_control))
        {
            return Err(RpcError::invalid("client_session is not a session id"));
        }
        let mut guard = self.core();
        let c = &mut *guard;
        let mut record = c.reg.session_for_token(&c.store, &p.token)?;
        let ctx = self.ctx(now);
        let mut changed = false;
        if let Some(cs) = &p.client_session
            && record.client_session.as_deref() != Some(cs.as_str())
        {
            let (bound, merged) = c.reg.bind_client_session(&c.store, &record.id, cs, &ctx)?;
            record = bound;
            changed |= !merged.is_empty();
            for gone in &merged {
                self.waits.wake(gone);
            }
        }
        if let Some(cwd) = &p.cwd
            && record.cwd.as_deref() != Some(cwd.as_str())
        {
            // A room from SPEW_PROJECT stays with the session when it moves (11).
            let env_room = if record.room_source.as_deref() == Some("env") {
                record.room.clone()
            } else {
                None
            };
            apply_cwd(
                &mut record,
                cwd,
                env_room.as_deref(),
                self.cfg.default_room.as_deref(),
            );
            changed = true;
        }
        if let Some(state) = p.state {
            changed |= set_state(&mut record, state, now);
        }
        if let Some(activity) = &p.activity {
            let line = sanitize_line(activity, MAX_ACTIVITY);
            let new = Some(line).filter(|l| !l.is_empty());
            if record.activity != new {
                record.activity = new;
                record.activity_project = None;
                changed = true;
            }
        }
        let held: Vec<String> = c
            .store
            .cards(&record.id)
            .map_err(store_err)?
            .into_iter()
            .map(|h| h.card)
            .collect();
        for card in &add {
            changed |= !held.contains(card);
            c.store
                .put_card(&record.id, card, card_project(card), now)
                .map_err(store_err)?;
        }
        for card in &remove {
            changed |= held.contains(card);
            c.store.remove_card(&record.id, card).map_err(store_err)?;
        }
        record.last_seen_at = now;
        c.store.put_session(&record).map_err(store_err)?;
        if changed {
            self.presence_changed();
        }
        Ok(())
    }

    /// `session.close` (6.2, rule 1 of 6.4).
    pub fn close_session(&self, token: &str) -> Result<(), RpcError> {
        let mut guard = self.core();
        let c = &mut *guard;
        let record = c.reg.session_for_token(&c.store, token)?;
        self.end_session(c, &record.id, EndReason::Closed);
        Ok(())
    }

    // Messages to a session (and every id merged into it) in `states`, oldest first.
    fn messages_for(&self, c: &Core, session: &str, states: &[MessageState]) -> Vec<StoredMessage> {
        let mut out: Vec<StoredMessage> = c
            .reg
            .ids_of(session)
            .iter()
            .flat_map(|sid| {
                warn_store(
                    c.store.messages_to(sid, states),
                    "could not read a session's mail",
                )
                .unwrap_or_default()
            })
            .filter(|m| m.message.to.device == self.cfg.device.hex)
            .collect();
        out.sort_by_key(|m| m.seq);
        out
    }

    // Asks a session was shown before and hasn't answered (7.3), while the asker may still take an answer: once
    // an ask's time has run out it stops coming back.
    fn unanswered_asks(&self, c: &Core, session: &str) -> Vec<StoredMessage> {
        let now = self.now();
        self.messages_for(c, session, &[MessageState::Shown])
            .into_iter()
            .filter(|m| {
                m.message.kind == MessageKind::Ask
                    && now
                        < m.created_at
                            .saturating_add(m.message.ttl_secs.saturating_mul(1000))
            })
            .collect()
    }

    // Unread messages rendered within the injection budget (7.8); empty text when there are none.
    fn injection(&self, c: &Core, session: &str) -> crate::text::Rendered {
        let unread = self.messages_for(c, session, &[MessageState::Delivered]);
        let me = c.roster.me().username.clone();
        let blocks: Vec<_> = unread
            .iter()
            .map(|m| render::block(m, &me, false))
            .collect();
        render_batch(&blocks, Some(INJECTION_BUDGET))
    }

    /// `hook.event` (6.2, 8.1): applies one hook event to its session and says what the hook shows.
    pub fn hook_event(&self, call: HookCall) -> Result<HookAnswer, RpcError> {
        // Subagents aren't mesh sessions (8.2, 8.3); hooks skip them, and so does the daemon.
        if call.facts.subagent {
            return Ok(HookAnswer::default());
        }
        let now = self.now();
        let mut guard = self.core();
        let c = &mut *guard;
        let ctx = self.ctx(now);
        let Some(opened) = c.reg.for_hook(&c.store, &call, &ctx)? else {
            return Ok(HookAnswer::default());
        };
        for gone in &opened.merged {
            self.waits.wake(gone);
        }
        let mut record = opened.session;
        let mut answer = HookAnswer {
            session: record.id.clone(),
            address: self.address(c, &record),
            context: None,
            ack_ids: Vec::new(),
        };
        if opened.created {
            tracing::info!(session = %record.name, client = %record.client, "session opened by a hook");
        }
        let mut changed = opened.created || !opened.merged.is_empty();
        match call.event {
            MeshEvent::End => {
                self.end_session(c, &record.id, EndReason::Closed);
                return Ok(answer);
            }
            // A session that just started waits for its first prompt.
            MeshEvent::Start | MeshEvent::Stop => {
                changed |= set_state(&mut record, ActivityState::Idle, now);
            }
            MeshEvent::Prompt => changed |= set_state(&mut record, ActivityState::Busy, now),
            MeshEvent::Tool => {
                // A tool call means the session is working, whatever an earlier event said.
                changed |= set_state(&mut record, ActivityState::Busy, now);
                changed |= self.capture_cards(c, &mut record, &call, now)?;
            }
        }
        c.store.put_session(&record).map_err(store_err)?;
        if changed {
            self.presence_changed();
        }
        // A stop hook never blocks the agent, so it never carries messages (8.1).
        if !call.show || call.event == MeshEvent::Stop {
            return Ok(answer);
        }
        let rendered = self.injection(c, &record.id);
        let mut text = rendered.text;
        match call.event {
            MeshEvent::Start if text.is_empty() => {
                let me = c.roster.me().username.clone();
                text = start_line(
                    &me,
                    &record.name,
                    &self.cfg.device.name,
                    record.room.as_deref(),
                );
            }
            MeshEvent::Prompt => {
                let asks: Vec<String> = self
                    .unanswered_asks(c, &record.id)
                    .into_iter()
                    .map(|m| m.message.id)
                    .collect();
                if !asks.is_empty() && c.reg.mention_unanswered(&record.id, &asks) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&unanswered_line(asks.len()));
                }
            }
            MeshEvent::Tool if text.is_empty() || !c.reg.tool_show_allowed(&record.id, now) => {
                return Ok(answer);
            }
            _ => {}
        }
        if !text.is_empty() {
            answer.context = Some(text);
            answer.ack_ids = rendered.shown;
        }
        Ok(answer)
    }

    // Card capture for a `tool` event (8.5); true when the cards or the activity changed.
    fn capture_cards(
        &self,
        c: &mut Core,
        record: &mut SessionRecord,
        call: &HookCall,
        now: u64,
    ) -> Result<bool, RpcError> {
        let stored = c.store.cards(&record.id).map_err(store_err)?;
        let held: Vec<cards::HeldCard> = stored
            .iter()
            .map(|h| cards::HeldCard {
                card: h.card.clone(),
                project: h.project.clone(),
                refreshed_at: h.refreshed_at,
            })
            .collect();
        let capture = cards::capture(&call.facts, &held, now);
        let mut changed = false;
        for old in &stored {
            if !capture.cards.iter().any(|n| n.card == old.card) {
                c.store
                    .remove_card(&record.id, &old.card)
                    .map_err(store_err)?;
                changed = true;
            }
        }
        for new in &capture.cards {
            let before = stored.iter().find(|o| o.card == new.card);
            if before.is_none_or(|o| o.refreshed_at != new.refreshed_at) {
                c.store
                    .put_card(&record.id, &new.card, &new.project, new.refreshed_at)
                    .map_err(store_err)?;
            }
            changed |= before.is_none();
        }
        match capture.activity {
            ActivityUpdate::Keep => {}
            ActivityUpdate::Set(a) => {
                let text = sanitize_line(&a.text, MAX_ACTIVITY);
                if record.activity.as_deref() != Some(text.as_str())
                    || record.activity_project != a.project
                {
                    record.activity = Some(text).filter(|t| !t.is_empty());
                    record.activity_project = a.project;
                    changed = true;
                }
            }
            ActivityUpdate::Clear => {
                if record.activity.is_some() {
                    record.activity = None;
                    record.activity_project = None;
                    changed = true;
                }
            }
        }
        Ok(changed)
    }

    /// `messages.ack` (6.2, 7.3): marks messages shown once a hook's output is written.
    pub fn ack(&self, p: Ack) -> Result<(), RpcError> {
        let mut guard = self.core();
        let c = &mut *guard;
        let id = match (&p.token, &p.client, &p.client_session) {
            (Some(token), _, _) => c.reg.session_for_token(&c.store, token)?.id,
            (None, Some(client), Some(cs)) => {
                c.store
                    .session_by_client(client, cs)
                    .map_err(store_err)?
                    .ok_or_else(|| {
                        RpcError::not_found("no open session has that client session id")
                    })?
                    .id
            }
            _ => {
                return Err(RpcError::invalid(
                    "messages.ack needs a token, or a client and its client_session",
                ));
            }
        };
        let id = c.reg.resolve_id(&id).to_string();
        for sid in c.reg.ids_of(&id) {
            for mid in &p.ids {
                for m in c.store.received_by_id(&sid, mid).map_err(store_err)? {
                    if m.message.to.device == self.cfg.device.hex {
                        self.mark_shown(c, &m);
                    }
                }
            }
        }
        Ok(())
    }

    /// `messages.fetch` (6.2, 7.3): unread messages first, then asks shown before and not answered, marked
    /// `again`. Waits up to `wait_seconds` when nothing is unread.
    pub async fn fetch(&self, conn: &Conn, p: FetchParams) -> Result<Fetched, RpcError> {
        let session = {
            let c = self.core();
            c.reg.session_for_token(&c.store, &p.token)?.id
        };
        if let Some(wait) = p.wait_seconds.filter(|w| *w > 0) {
            let deadline = Instant::now() + Duration::from_secs(wait.min(MAX_WAIT_SECS));
            self.wait_for(&session, deadline, conn, |me| {
                let c = me.core();
                let id = c.reg.resolve_id(&session);
                Ok((me.unread_count(&c, id) > 0).then_some(()))
            })
            .await?;
        }
        let mut guard = self.core();
        let c = &mut *guard;
        let id = c.reg.resolve_id(&session).to_string();
        match c.store.session(&id).map_err(store_err)? {
            Some(s) if s.ended_at.is_none() => {}
            _ => return Err(RpcError::not_found("this session ended while it waited")),
        }
        let unread = self.messages_for(c, &id, &[MessageState::Delivered]);
        let again = self.unanswered_asks(c, &id);
        let me = c.roster.me().username.clone();
        let listed: Vec<(&StoredMessage, bool)> = unread
            .iter()
            .map(|m| (m, false))
            .chain(again.iter().map(|m| (m, true)))
            .collect();
        let blocks: Vec<_> = listed
            .iter()
            .map(|(m, is_again)| render::block(m, &me, *is_again))
            .collect();
        let rendered = render_batch(&blocks, Some(FETCH_BUDGET));
        let in_text = |m: &StoredMessage| {
            rendered.shown.contains(&m.message.id) || rendered.cut.as_ref() == Some(&m.message.id)
        };
        let views = listed
            .iter()
            .filter(|(m, _)| in_text(m))
            .map(|(m, is_again)| render::view(m, *is_again))
            .collect();
        if !p.keep_unread {
            for m in unread
                .iter()
                .filter(|m| rendered.shown.contains(&m.message.id))
            {
                self.mark_shown(c, m);
            }
        }
        Ok(Fetched {
            messages: views,
            text: rendered.text,
            unread: self.unread_count(c, &id),
        })
    }

    /// `responder.take` (6.2, 9): the oldest waiting question, taken (and so acknowledged) at once.
    pub async fn take(&self, conn: &Conn, p: TakeParams) -> Result<Taken, RpcError> {
        let session = {
            let c = self.core();
            let s = c.reg.session_for_token(&c.store, &p.token)?;
            if s.kind != SessionKind::Responder {
                return Err(RpcError::forbidden(
                    "only a serve responder takes questions",
                ));
            }
            s.id
        };
        let deadline = Instant::now() + Duration::from_secs(p.wait_seconds.min(MAX_WAIT_SECS));
        let question = self
            .wait_for(&session, deadline, conn, |me| {
                let mut c = me.core();
                Ok(me.take_next(&mut c, &session))
            })
            .await?;
        Ok(Taken { question })
    }

    fn take_next(&self, c: &mut Core, session: &str) -> Option<QuestionView> {
        let id = c.reg.resolve_id(session).to_string();
        let m = self
            .messages_for(c, &id, &[MessageState::Delivered])
            .into_iter()
            .find(|m| matches!(m.message.kind, MessageKind::Ask | MessageKind::Tell))?;
        self.mark_shown(c, &m);
        let now = self.now();
        if let Ok(Some(mut s)) = c.store.session(&id)
            && set_state(&mut s, ActivityState::Busy, now)
        {
            warn_store(c.store.put_session(&s), "could not mark a responder busy");
        }
        self.presence_changed();
        let me = c.roster.me().username.clone();
        let msg = m.message;
        Some(QuestionView {
            id: msg.id,
            conversation: msg.conversation,
            kind: msg.kind,
            from: AskerView {
                own: msg.from.person == me,
                person: msg.from.person,
                device: msg.from.device_name,
                device_id: msg.from.device,
                session: msg.from.session,
                name: msg.from.name,
                client: msg.from_client,
                room: msg.room,
            },
            repo: msg.repo,
            card: msg.card,
            body: msg.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_lists_are_checked() {
        assert_eq!(check_cards(&["SPEW#4".into()]).unwrap(), ["spew#4"]);
        assert!(check_cards(&["spew/t7".into()]).is_err());
        assert_eq!(card_project("web#12"), "web");
    }
}
