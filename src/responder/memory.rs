//! Conversation memory (9): which client session or thread continues an asker's conversation.
//!
//! The map lives in the store's serve conversation table (11.1) through [`StoreConversations`];
//! [`ConversationStore`] is the seam, and [`MemoryConversations`] keeps the map in memory for tests.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context as _;

use super::{Question, ServeClient};
use crate::store::Store;

/// One conversation, as seen from a responder: (client, asker device, asker session, conversation), the
/// store table's key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConversationKey {
    /// The responder's client; a responder never picks up another client's ids.
    pub client: ServeClient,
    /// The asking device's stable id.
    pub device: String,
    /// The asking session's id.
    pub session: String,
    /// The conversation id.
    pub conversation: String,
}

impl ConversationKey {
    /// The key for a question to a responder of `client`.
    pub fn of(client: ServeClient, q: &Question) -> Self {
        ConversationKey {
            client,
            device: q.from.device_id.clone(),
            session: q.from.session.clone(),
            conversation: q.conversation.clone(),
        }
    }
}

/// Where the conversation map lives.
pub trait ConversationStore: Send {
    /// The client session or thread id that continues a conversation.
    fn get(&self, key: &ConversationKey) -> anyhow::Result<Option<String>>;
    /// Records or replaces the session for a conversation.
    fn put(&mut self, key: &ConversationKey, session_id: &str, now_ms: u64) -> anyhow::Result<()>;
    /// Forgets a conversation, so its next question starts a new session.
    fn forget(&mut self, key: &ConversationKey, now_ms: u64) -> anyhow::Result<()>;
}

/// The conversation map in the store (11.1). The serve process opens the daemon's store file for it; SQLite
/// in WAL mode lets both write.
#[derive(Debug)]
pub struct StoreConversations {
    path: Option<PathBuf>,
    store: OnceCell<Store>,
}

impl StoreConversations {
    /// The map in an open store.
    pub fn new(store: Store) -> Self {
        StoreConversations {
            path: None,
            store: OnceCell::from(store),
        }
    }

    /// The map in the store file at `path`, opened at first use. serve checks its client before it reaches
    /// the daemon, and the daemon is the one that creates and migrates the store.
    pub fn open_later(path: impl Into<PathBuf>) -> Self {
        StoreConversations {
            path: Some(path.into()),
            store: OnceCell::new(),
        }
    }

    fn store(&self) -> anyhow::Result<&Store> {
        if let Some(store) = self.store.get() {
            return Ok(store);
        }
        let path = self
            .path
            .as_deref()
            .context("the serve conversation map has no store")?;
        let store = Store::open(path).with_context(|| format!("can't open {}", path.display()))?;
        Ok(self.store.get_or_init(|| store))
    }
}

impl ConversationStore for StoreConversations {
    fn get(&self, key: &ConversationKey) -> anyhow::Result<Option<String>> {
        let thread = self.store()?.serve_thread(
            key.client.as_str(),
            &key.device,
            &key.session,
            &key.conversation,
        )?;
        Ok(thread.filter(|t| !t.is_empty()))
    }

    fn put(&mut self, key: &ConversationKey, session_id: &str, now_ms: u64) -> anyhow::Result<()> {
        if session_id.is_empty() {
            anyhow::bail!("a serve conversation needs a session id");
        }
        self.store()?.set_serve_thread(
            key.client.as_str(),
            &key.device,
            &key.session,
            &key.conversation,
            session_id,
            now_ms,
        )?;
        Ok(())
    }

    fn forget(&mut self, key: &ConversationKey, now_ms: u64) -> anyhow::Result<()> {
        // The store has no delete for this table; an empty thread reads as "start a new session".
        self.store()?.set_serve_thread(
            key.client.as_str(),
            &key.device,
            &key.session,
            &key.conversation,
            "",
            now_ms,
        )?;
        Ok(())
    }
}

/// The conversation map in memory.
#[derive(Clone, Debug, Default)]
pub struct MemoryConversations {
    map: HashMap<ConversationKey, (String, u64)>,
}

impl MemoryConversations {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops conversations not used since `before_ms`; returns how many.
    pub fn sweep(&mut self, before_ms: u64) -> usize {
        let len = self.map.len();
        self.map.retain(|_, (_, at)| *at >= before_ms);
        len - self.map.len()
    }

    /// When a conversation was last used, ms.
    pub fn used_at(&self, key: &ConversationKey) -> Option<u64> {
        self.map.get(key).map(|(_, at)| *at)
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
    fn get(&self, key: &ConversationKey) -> anyhow::Result<Option<String>> {
        Ok(self.map.get(key).map(|(id, _)| id.clone()))
    }

    fn put(&mut self, key: &ConversationKey, session_id: &str, now_ms: u64) -> anyhow::Result<()> {
        self.map
            .insert(key.clone(), (session_id.to_string(), now_ms));
        Ok(())
    }

    fn forget(&mut self, key: &ConversationKey, _now_ms: u64) -> anyhow::Result<()> {
        self.map.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(conversation: &str) -> ConversationKey {
        ConversationKey {
            client: ServeClient::Claude,
            device: "maya-id".into(),
            session: "01J9ZK0000000000000000000A".into(),
            conversation: conversation.into(),
        }
    }

    fn maps() -> Vec<Box<dyn ConversationStore>> {
        vec![
            Box::new(MemoryConversations::new()),
            Box::new(StoreConversations::new(Store::open_in_memory().unwrap())),
        ]
    }

    #[test]
    fn put_get_replace_forget() {
        for mut m in maps() {
            assert_eq!(m.get(&key("c1")).unwrap(), None);
            m.put(&key("c1"), "s1", 1).unwrap();
            m.put(&key("c2"), "s2", 2).unwrap();
            assert_eq!(m.get(&key("c1")).unwrap().as_deref(), Some("s1"));
            m.put(&key("c1"), "s3", 3).unwrap();
            assert_eq!(m.get(&key("c1")).unwrap().as_deref(), Some("s3"));
            m.forget(&key("c1"), 4).unwrap();
            assert_eq!(m.get(&key("c1")).unwrap(), None);
            assert_eq!(m.get(&key("c2")).unwrap().as_deref(), Some("s2"));
            // A forgotten conversation can start again.
            m.put(&key("c1"), "s4", 5).unwrap();
            assert_eq!(m.get(&key("c1")).unwrap().as_deref(), Some("s4"));
        }
    }

    #[test]
    fn keys_differ_by_client_device_session_and_conversation() {
        for mut m in maps() {
            m.put(&key("c1"), "s1", 1).unwrap();
            let other_device = ConversationKey {
                device: "lee-id".into(),
                ..key("c1")
            };
            let other_session = ConversationKey {
                session: "01J9ZK0000000000000000000B".into(),
                ..key("c1")
            };
            let other_client = ConversationKey {
                client: ServeClient::Codex,
                ..key("c1")
            };
            assert_eq!(m.get(&other_device).unwrap(), None);
            assert_eq!(m.get(&other_session).unwrap(), None);
            assert_eq!(m.get(&other_client).unwrap(), None);
        }
    }

    #[test]
    fn the_store_map_survives_reopening_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.sqlite3");
        {
            let mut m = StoreConversations::new(Store::open(&path).unwrap());
            m.put(&key("c1"), "s1", 1).unwrap();
            assert!(m.put(&key("c2"), "", 1).is_err());
        }
        let m = StoreConversations::open_later(&path);
        assert_eq!(m.get(&key("c1")).unwrap().as_deref(), Some("s1"));
        assert_eq!(m.get(&key("c2")).unwrap(), None);
    }

    #[test]
    fn a_store_opened_later_is_opened_at_first_use() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.sqlite3");
        let mut m = StoreConversations::open_later(&path);
        assert!(!path.exists());
        m.put(&key("c1"), "s1", 1).unwrap();
        assert!(path.exists());
        assert_eq!(m.get(&key("c1")).unwrap().as_deref(), Some("s1"));
        let missing = StoreConversations::open_later(tmp.path().join("no/such/dir/store.sqlite3"));
        assert!(missing.get(&key("c1")).is_err());
    }

    #[test]
    fn sweep_drops_old_conversations() {
        let mut m = MemoryConversations::new();
        m.put(&key("old"), "s1", 10).unwrap();
        m.put(&key("new"), "s2", 100).unwrap();
        assert_eq!(m.sweep(50), 1);
        assert!(m.get(&key("old")).unwrap().is_none());
        assert!(m.get(&key("new")).unwrap().is_some());
        assert_eq!(m.len(), 1);
    }
}
