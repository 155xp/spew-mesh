//! Stored messages as agents and people see them (7.8): message blocks, message views and addresses.

use crate::address::RESERVED_NAME;
use crate::control::protocol::MessageView;
use crate::store::{Party, StoredMessage};
use crate::text::{Block, Origin};
use crate::wire::MessageKind;

/// `person@device/name` for one end of a message.
pub fn party_address(p: &Party) -> String {
    format!("{}@{}/{}", p.person, p.device_name, p.name)
}

/// The sender as shown: the full address, or `mesh` for a notice.
pub fn sender_address(m: &StoredMessage) -> String {
    if m.message.kind == MessageKind::Notice {
        RESERVED_NAME.to_string()
    } else {
        party_address(&m.message.from)
    }
}

/// A stored message as a block (7.8). `me` is this device's person, for the trailer's origin.
pub fn block(m: &StoredMessage, me: &str, again: bool) -> Block {
    let msg = &m.message;
    let origin = if msg.kind == MessageKind::Notice {
        Origin::Mesh
    } else if msg.from.person == me {
        Origin::Own
    } else {
        Origin::Teammate {
            person: msg.from.person.clone(),
            room: msg.room.clone(),
        }
    };
    Block {
        id: msg.id.clone(),
        kind: msg.kind,
        again,
        from_person: msg.from.person.clone(),
        from_name: msg.from.name.clone(),
        device: msg.from.device_name.clone(),
        client: msg.from_client.clone(),
        repo: msg.repo.clone(),
        card: msg.card.clone(),
        conversation: msg.conversation.clone(),
        body: msg.body.clone(),
        origin,
    }
}

/// A stored message as `messages.fetch` returns it.
pub fn view(m: &StoredMessage, again: bool) -> MessageView {
    let msg = &m.message;
    MessageView {
        id: msg.id.clone(),
        kind: msg.kind,
        conversation: msg.conversation.clone(),
        in_reply_to: msg.in_reply_to.clone(),
        from: sender_address(m),
        person: msg.from.person.clone(),
        body: msg.body.clone(),
        card: msg.card.clone(),
        repo: msg.repo.clone(),
        room: msg.room.clone(),
        sent_at: msg.sent_at,
        again,
    }
}
