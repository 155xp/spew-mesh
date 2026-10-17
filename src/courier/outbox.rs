//! The outbox (7.4): one persistent queue per target device, re-sent when a link comes up and on a per-message
//! backoff of 10 s, 30 s, 2 min, 10 min, then every 10 min. Receipts back to senders use the same queue.

use std::time::Duration;

use iroh::EndpointId;

use super::{Core, Courier, render, warn_store};
use crate::link::{CallError, Links};
use crate::store::{MessageState, OutboxEntry, OutboxKind};
use crate::text::NoticeEvent;
use crate::wire::{DeliverState, Envelope, ReceiptUpdate, RefusalCode, Request, Response};

/// How long one outbox call may take before it counts as failed.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// The wait before the next attempt, after the failure that made `attempts` failed attempts (7.4).
pub fn retry_delay_ms(attempts: u32) -> u64 {
    match attempts {
        0 | 1 => 10_000,
        2 => 30_000,
        3 => 120_000,
        _ => 600_000,
    }
}

/// What came of one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    /// The target device stored it (or already had it).
    Delivered,
    /// The target device refused it for good.
    Refused(RefusalCode),
    /// Try again later, for this reason; `unreachable` when the device couldn't be reached at all.
    Retry {
        /// Why, in words.
        reason: String,
        /// No link could be made.
        unreachable: bool,
    },
}

/// Reads a `deliver` answer (5.4).
pub fn read_deliver_response(id: &str, response: Result<Response, CallError>) -> Attempt {
    match response {
        Ok(Response::Receipt(r)) if r.id == id => match (r.state, r.code) {
            (DeliverState::Accepted | DeliverState::Duplicate, _) => Attempt::Delivered,
            (DeliverState::Refused, Some(RefusalCode::RateLimited)) => Attempt::Retry {
                reason: "that device is getting too many messages".into(),
                unreachable: false,
            },
            (DeliverState::Refused, Some(code)) => Attempt::Refused(code),
            (DeliverState::Refused, None) => Attempt::Refused(RefusalCode::Invalid),
        },
        Ok(other) => Attempt::Retry {
            reason: format!("unexpected answer from that device: {other:?}"),
            unreachable: false,
        },
        Err(e) => Attempt::Retry {
            unreachable: matches!(e, CallError::Unreachable(_) | CallError::NotAdmitted),
            reason: e.to_string(),
        },
    }
}

impl<L: Links> Courier<L> {
    /// Makes one call on a device with the outbox time limit.
    pub(crate) async fn call(
        &self,
        device: EndpointId,
        request: Request,
        limit: Duration,
    ) -> Result<Response, CallError> {
        match tokio::time::timeout(limit, self.links.call(device, request)).await {
            Ok(result) => result,
            Err(_) => Err(CallError::Timeout),
        }
    }

    /// Records the outcome of a `deliver` attempt for an outbox entry that had failed `attempts` times before.
    /// `in_send` is true while the send call that queued it is still running, so a refusal needs no notice.
    pub(crate) fn settle_deliver(
        &self,
        c: &mut Core,
        entry_seq: i64,
        message_seq: i64,
        attempts: u32,
        attempt: &Attempt,
        in_send: bool,
    ) {
        let now = self.now();
        match attempt {
            Attempt::Delivered => {
                warn_store(
                    c.store
                        .set_state(message_seq, MessageState::Delivered, None, now),
                    "could not mark a message delivered",
                );
                warn_store(
                    c.store.outbox_remove(entry_seq),
                    "could not drop a delivered outbox entry",
                );
            }
            Attempt::Refused(code) => {
                let moved = matches!(
                    c.store
                        .set_state(message_seq, MessageState::Refused, Some(code.as_str()), now),
                    Ok(true)
                );
                warn_store(
                    c.store.outbox_remove(entry_seq),
                    "could not drop a refused outbox entry",
                );
                if moved
                    && !in_send
                    && let Ok(Some(m)) = c.store.message_by_seq(message_seq)
                {
                    let event = NoticeEvent::Refused {
                        id: m.message.id.clone(),
                        to: render::party_address(&m.message.to),
                        code: *code,
                    };
                    self.notice(c, &m.message.from.session, &event, Some(&m.message.id));
                }
            }
            Attempt::Retry { .. } => {
                warn_store(
                    c.store
                        .outbox_retry(entry_seq, now + retry_delay_ms(attempts + 1)),
                    "could not reschedule an outbox entry",
                );
            }
        }
        if let Ok(Some(m)) = c.store.message_by_seq(message_seq) {
            self.wake(c, &m.message.from.session);
        }
    }

    async fn send_entry(&self, device: EndpointId, entry: &OutboxEntry) -> Attempt {
        match entry.kind {
            OutboxKind::Deliver => {
                let Ok(envelope) = serde_json::from_str::<Envelope>(&entry.payload) else {
                    return Attempt::Refused(RefusalCode::Invalid);
                };
                let id = envelope.id.clone();
                let response = self
                    .call(device, Request::Deliver(envelope), CALL_TIMEOUT)
                    .await;
                read_deliver_response(&id, response)
            }
            OutboxKind::Receipt => {
                let Ok(update) = serde_json::from_str::<ReceiptUpdate>(&entry.payload) else {
                    return Attempt::Delivered;
                };
                match self
                    .call(device, Request::Receipt(update), CALL_TIMEOUT)
                    .await
                {
                    Ok(Response::Ok) => Attempt::Delivered,
                    Ok(other) => Attempt::Retry {
                        reason: format!("unexpected answer to a receipt: {other:?}"),
                        unreachable: false,
                    },
                    Err(e) => Attempt::Retry {
                        unreachable: matches!(
                            e,
                            CallError::Unreachable(_) | CallError::NotAdmitted
                        ),
                        reason: e.to_string(),
                    },
                }
            }
        }
    }

    /// Sends a device's outbox in queue order: every entry when `all` (its link just came up), else the entries
    /// that are due. One pump per device runs at a time.
    pub async fn pump(&self, device: EndpointId, all: bool) {
        let hex = device.to_string();
        let entries = {
            let mut c = self.core();
            if !c.pumping.insert(hex.clone()) {
                return;
            }
            let now = self.now();
            let list = if all {
                c.store.outbox_for(&hex)
            } else {
                c.store.outbox_due(&hex, now)
            };
            let list: Vec<OutboxEntry> = warn_store(list, "could not read the outbox")
                .unwrap_or_default()
                .into_iter()
                .filter(|e| !c.in_flight.contains(&e.seq))
                .collect();
            for e in &list {
                c.in_flight.insert(e.seq);
            }
            list
        };
        let mut unreachable = false;
        for entry in &entries {
            let attempt = if unreachable {
                Attempt::Retry {
                    reason: "that device can't be reached".into(),
                    unreachable: true,
                }
            } else {
                self.send_entry(device, entry).await
            };
            if let Attempt::Retry {
                unreachable: true, ..
            } = attempt
            {
                unreachable = true;
            }
            let mut c = self.core();
            c.in_flight.remove(&entry.seq);
            match (entry.kind, entry.message_seq) {
                (OutboxKind::Deliver, Some(message_seq)) => self.settle_deliver(
                    &mut c,
                    entry.seq,
                    message_seq,
                    entry.attempts,
                    &attempt,
                    false,
                ),
                _ => match attempt {
                    Attempt::Retry { .. } => {
                        let now = self.now();
                        warn_store(
                            c.store
                                .outbox_retry(entry.seq, now + retry_delay_ms(entry.attempts + 1)),
                            "could not reschedule a receipt",
                        );
                    }
                    _ => {
                        warn_store(
                            c.store.outbox_remove(entry.seq),
                            "could not drop a sent receipt",
                        );
                    }
                },
            }
        }
        let mut c = self.core();
        for entry in &entries {
            c.in_flight.remove(&entry.seq);
        }
        c.pumping.remove(&hex);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::DeliverReceipt;

    #[test]
    fn backoff_follows_the_schedule() {
        let delays: Vec<u64> = (1..=6).map(retry_delay_ms).collect();
        assert_eq!(delays, [10_000, 30_000, 120_000, 600_000, 600_000, 600_000]);
    }

    #[test]
    fn deliver_answers() {
        let receipt = |state, code| {
            Ok(Response::Receipt(DeliverReceipt {
                id: "x".into(),
                state,
                code,
            }))
        };
        assert_eq!(
            read_deliver_response("x", receipt(DeliverState::Accepted, None)),
            Attempt::Delivered
        );
        assert_eq!(
            read_deliver_response("x", receipt(DeliverState::Duplicate, None)),
            Attempt::Delivered
        );
        assert_eq!(
            read_deliver_response("x", receipt(DeliverState::Refused, Some(RefusalCode::Busy))),
            Attempt::Refused(RefusalCode::Busy)
        );
        assert!(matches!(
            read_deliver_response(
                "x",
                receipt(DeliverState::Refused, Some(RefusalCode::RateLimited))
            ),
            Attempt::Retry {
                unreachable: false,
                ..
            }
        ));
        assert!(matches!(
            read_deliver_response("y", receipt(DeliverState::Accepted, None)),
            Attempt::Retry { .. }
        ));
        assert!(matches!(
            read_deliver_response("x", Err(CallError::Unreachable("down".into()))),
            Attempt::Retry {
                unreachable: true,
                ..
            }
        ));
    }
}
