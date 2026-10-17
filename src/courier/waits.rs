//! Waiters per session (7.5): a wait subscribes before it looks, so nothing slips between the check and the
//! wait, and every change for a session wakes that session's waiters.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::watch;

/// One wake-up channel per session.
#[derive(Debug, Default)]
pub struct Waits {
    map: Mutex<HashMap<String, watch::Sender<u64>>>,
}

impl Waits {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, watch::Sender<u64>>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers interest in `session`; the receiver sees every wake after this call.
    pub fn subscribe(&self, session: &str) -> watch::Receiver<u64> {
        self.lock()
            .entry(session.to_string())
            .or_insert_with(|| watch::channel(0).0)
            .subscribe()
    }

    /// Wakes everyone waiting on `session`.
    pub fn wake(&self, session: &str) {
        if let Some(tx) = self.lock().get(session) {
            tx.send_modify(|n| *n = n.wrapping_add(1));
        }
    }

    /// Drops a session's channel; its waiters wake and find it gone.
    pub fn forget(&self, session: &str) {
        self.lock().remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_wake_after_subscribing_is_never_missed() {
        let waits = Waits::default();
        let mut rx = waits.subscribe("s");
        waits.wake("s");
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.changed())
            .await
            .unwrap()
            .unwrap();
        waits.wake("other");
        let mut rx2 = waits.subscribe("s");
        waits.forget("s");
        assert!(rx2.changed().await.is_err());
    }
}
