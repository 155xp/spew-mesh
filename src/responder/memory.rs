//! Conversation memory (9): which client session or thread continues an asker's conversation.
//!
//! The spec keeps the map in the store (11.1); [`ConversationStore`] is the seam, and [`MemoryConversations`]
//! is the in-process version used until the store table exists and in tests.

use std::collections::HashMap;

use super::{Question, ServeClient};

/// One conversation, as seen from the responder: (asker device, asker session, conversation).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConversationKey {
    /// The asking device's stable id.
    pub device: String,
    /// The asking session's id.
    pub session: String,
    /// The conversation id.
    pub conversation: String,
}

impl ConversationKey {
    /// The key for a question.
    pub fn of(q: &Question) -> Self {
        ConversationKey {
            device: q.from.device_id.clone(),
            session: q.from.session.clone(),
            conversation: q.conversation.clone(),
        }
    }
}

/// The client session a conversation continues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSession {
    /// Which client the id belongs to; a responder ignores another client's ids.
    pub client: ServeClient,
    /// Claude Code session UUID, Codex thread id or OpenCode session id.
    pub session_id: String,
    /// When the conversation last ran, in ms since the epoch.
    pub last_used_ms: u64,
}

/// Where the conversation map lives.
pub trait ConversationStore: Send {
    /// The stored session for a conversation.
    fn get(&self, key: &ConversationKey) -> anyhow::Result<Option<StoredSession>>;
    /// Records or replaces the session for a conversation.
    fn put(&mut self, key: ConversationKey, value: StoredSession) -> anyhow::Result<()>;
    /// Forgets a conversation.
    fn remove(&mut self, key: &ConversationKey) -> anyhow::Result<()>;
}

/// The conversation map in memory.
#[derive(Clone, Debug, Default)]
pub struct MemoryConversations {
    map: HashMap<ConversationKey, StoredSession>,
}

impl MemoryConversations {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops conversations not used since `before_ms`; returns how many.
    pub fn sweep(&mut self, before_ms: u64) -> usize {
        let len = self.map.len();
        self.map.retain(|_, s| s.last_used_ms >= before_ms);
        len - self.map.len()
    }

    /// Number of conversations held.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl ConversationStore for MemoryConversations {
    fn get(&self, key: &ConversationKey) -> anyhow::Result<Option<StoredSession>> {
        Ok(self.map.get(key).cloned())
    }

    fn put(&mut self, key: ConversationKey, value: StoredSession) -> anyhow::Result<()> {
        self.map.insert(key, value);
        Ok(())
    }

    fn remove(&mut self, key: &ConversationKey) -> anyhow::Result<()> {
        self.map.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(conversation: &str) -> ConversationKey {
        ConversationKey {
            device: "maya-id".into(),
            session: "01J9ZK0000000000000000000A".into(),
            conversation: conversation.into(),
        }
    }

    fn stored(id: &str, at: u64) -> StoredSession {
        StoredSession {
            client: ServeClient::Claude,
            session_id: id.into(),
            last_used_ms: at,
        }
    }

    #[test]
    fn put_get_replace_remove() {
        let mut m = MemoryConversations::new();
        assert_eq!(m.get(&key("c1")).unwrap(), None);
        m.put(key("c1"), stored("s1", 1)).unwrap();
        m.put(key("c2"), stored("s2", 2)).unwrap();
        assert_eq!(m.get(&key("c1")).unwrap(), Some(stored("s1", 1)));
        m.put(key("c1"), stored("s3", 3)).unwrap();
        assert_eq!(m.get(&key("c1")).unwrap(), Some(stored("s3", 3)));
        m.remove(&key("c1")).unwrap();
        assert_eq!(m.get(&key("c1")).unwrap(), None);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn keys_differ_by_device_session_and_conversation() {
        let mut m = MemoryConversations::new();
        m.put(key("c1"), stored("s1", 1)).unwrap();
        let other_device = ConversationKey {
            device: "lee-id".into(),
            ..key("c1")
        };
        let other_session = ConversationKey {
            session: "01J9ZK0000000000000000000B".into(),
            ..key("c1")
        };
        assert_eq!(m.get(&other_device).unwrap(), None);
        assert_eq!(m.get(&other_session).unwrap(), None);
    }

    #[test]
    fn sweep_drops_old_conversations() {
        let mut m = MemoryConversations::new();
        m.put(key("old"), stored("s1", 10)).unwrap();
        m.put(key("new"), stored("s2", 100)).unwrap();
        assert_eq!(m.sweep(50), 1);
        assert!(m.get(&key("old")).unwrap().is_none());
        assert!(m.get(&key("new")).unwrap().is_some());
    }
}
