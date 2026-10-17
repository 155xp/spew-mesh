//! Presence from peers (5.6): what each admitted device last said about its sessions, whether its link is up,
//! and when it was last seen. Also the sequence numbers this device puts on its own snapshots.

use std::collections::HashMap;

use iroh::EndpointId;

use crate::identity::parse_endpoint_id;
use crate::store::CachedPresence;
use crate::wire::PresenceSnapshot;

/// A receiver takes at most one presence call per second per peer (5.6).
pub const MIN_ACCEPT_GAP_MS: u64 = 1_000;
/// Snapshots go out at least this often (5.6).
pub const REFRESH_MS: u64 = 45_000;
/// Changes are sent after this quiet time (5.6).
pub const DEBOUNCE_MS: u64 = 300;

/// What this device knows about one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDevice {
    /// The last snapshot taken from it.
    pub snapshot: Option<PresenceSnapshot>,
    /// Its `seq`.
    pub seq: u64,
    /// When it arrived, ms.
    pub received_at: u64,
    /// Whether a link to it is up.
    pub online: bool,
    /// When it was last seen, for offline devices.
    pub last_seen: Option<u64>,
    fresh: bool,
    last_accept: Option<u64>,
    card_since: HashMap<(String, String), u64>,
}

impl RemoteDevice {
    fn empty() -> RemoteDevice {
        RemoteDevice {
            snapshot: None,
            seq: 0,
            received_at: 0,
            online: false,
            last_seen: None,
            fresh: true,
            last_accept: None,
            card_since: HashMap::new(),
        }
    }

    /// When the card was first seen on that session, ms.
    pub fn card_since(&self, session: &str, card: &str) -> Option<u64> {
        self.card_since
            .get(&(session.to_string(), card.to_string()))
            .copied()
    }
}

/// Why a snapshot was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Less than a second after the last one taken.
    TooSoon,
    /// Not newer than the one already held.
    Old,
}

/// Presence from every peer.
#[derive(Debug, Clone, Default)]
pub struct RemotePresence {
    devices: HashMap<EndpointId, RemoteDevice>,
}

impl RemotePresence {
    /// Starts from the cached snapshots, every device offline until its link comes up.
    pub fn restore(cached: &[CachedPresence]) -> RemotePresence {
        let mut out = RemotePresence::default();
        for c in cached {
            let (Some(id), Ok(value)) = (
                parse_endpoint_id(&c.device),
                serde_json::from_str::<serde_json::Value>(&c.snapshot),
            ) else {
                continue;
            };
            let Ok(snapshot) = PresenceSnapshot::parse(value) else {
                continue;
            };
            let mut device = RemoteDevice::empty();
            for s in &snapshot.sessions {
                for card in &s.cards {
                    device
                        .card_since
                        .insert((s.session.clone(), card.clone()), c.received_at);
                }
            }
            device.snapshot = Some(snapshot);
            device.seq = c.seq;
            device.received_at = c.received_at;
            device.last_seen = Some(c.received_at);
            out.devices.insert(id, device);
        }
        out
    }

    /// A link came up: the next snapshot is taken whatever its `seq` (the peer may have restarted).
    pub fn link_up(&mut self, device: EndpointId) {
        let d = self
            .devices
            .entry(device)
            .or_insert_with(RemoteDevice::empty);
        d.online = true;
        d.fresh = true;
    }

    /// A link went down.
    pub fn link_down(&mut self, device: EndpointId, last_seen: u64) {
        let d = self
            .devices
            .entry(device)
            .or_insert_with(RemoteDevice::empty);
        d.online = false;
        d.last_seen = Some(last_seen);
    }

    /// Takes a sanitized snapshot from a peer, at most one per second and only when newer (5.6).
    pub fn accept(
        &mut self,
        device: EndpointId,
        snapshot: PresenceSnapshot,
        now: u64,
    ) -> Result<(), Dropped> {
        let d = self
            .devices
            .entry(device)
            .or_insert_with(RemoteDevice::empty);
        if d.last_accept
            .is_some_and(|t| now.saturating_sub(t) < MIN_ACCEPT_GAP_MS)
        {
            return Err(Dropped::TooSoon);
        }
        if !d.fresh && d.snapshot.is_some() && snapshot.seq <= d.seq {
            return Err(Dropped::Old);
        }
        let mut since = HashMap::new();
        for s in &snapshot.sessions {
            for card in &s.cards {
                let key = (s.session.clone(), card.clone());
                let first = d.card_since.get(&key).copied().unwrap_or(now);
                since.insert(key, first);
            }
        }
        d.card_since = since;
        d.seq = snapshot.seq;
        d.snapshot = Some(snapshot);
        d.received_at = now;
        d.last_accept = Some(now);
        d.fresh = false;
        d.online = true;
        Ok(())
    }

    /// Drops a device that is no longer admitted (4.3).
    pub fn remove(&mut self, device: &EndpointId) {
        self.devices.remove(device);
    }

    /// One device.
    pub fn get(&self, device: &EndpointId) -> Option<&RemoteDevice> {
        self.devices.get(device)
    }

    /// Every device known.
    pub fn devices(&self) -> impl Iterator<Item = (&EndpointId, &RemoteDevice)> {
        self.devices.iter()
    }

    /// Devices whose link is up.
    pub fn online(&self) -> Vec<EndpointId> {
        let mut ids: Vec<EndpointId> = self
            .devices
            .iter()
            .filter(|(_, d)| d.online)
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        ids
    }

    /// Whether the link to a device is up.
    pub fn is_online(&self, device: &EndpointId) -> bool {
        self.devices.get(device).is_some_and(|d| d.online)
    }
}

/// The `seq` of this device's snapshots: up on every change, and never below the clock so it keeps going up
/// across restarts (5.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seq(u64);

impl Seq {
    /// The next number at `now_ms`.
    pub fn next(&mut self, now_ms: u64) -> u64 {
        self.0 = (self.0 + 1).max(now_ms);
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ActivityState, PresenceDevice, PresenceSession, Reach, SessionKind};

    fn key(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn snap(seq: u64, cards: &[&str]) -> PresenceSnapshot {
        PresenceSnapshot {
            seq,
            device: PresenceDevice {
                name: "maya-mbp".into(),
                platform: "macos".into(),
                version: "0.1.0".into(),
            },
            sessions: vec![PresenceSession {
                session: "01J9ZK0000000000000000000A".into(),
                name: "claude-api".into(),
                client: "claude".into(),
                kind: SessionKind::Live,
                repo: None,
                branch: None,
                place: None,
                room: Some("spew".into()),
                cards: cards.iter().map(|c| c.to_string()).collect(),
                activity: None,
                state: ActivityState::Idle,
                since: 0,
                reach: Reach::Hooked,
                queue: 0,
            }],
        }
    }

    #[test]
    fn takes_one_per_second_and_only_newer() {
        let mut p = RemotePresence::default();
        let d = key(1);
        p.link_up(d);
        assert!(p.accept(d, snap(5, &[]), 1_000).is_ok());
        assert_eq!(p.accept(d, snap(6, &[]), 1_500), Err(Dropped::TooSoon));
        assert_eq!(p.accept(d, snap(4, &[]), 3_000), Err(Dropped::Old));
        assert!(p.accept(d, snap(7, &[]), 3_000).is_ok());
        // A peer that restarted may start lower; the first snapshot after a link comes up is taken.
        p.link_down(d, 4_000);
        assert!(!p.is_online(&d));
        p.link_up(d);
        assert!(p.accept(d, snap(1, &[]), 5_000).is_ok());
        assert_eq!(p.get(&d).unwrap().seq, 1);
    }

    #[test]
    fn remembers_when_each_card_appeared() {
        let mut p = RemotePresence::default();
        let d = key(2);
        p.link_up(d);
        p.accept(d, snap(1, &["spew#1"]), 10_000).unwrap();
        p.accept(d, snap(2, &["spew#1", "spew#2"]), 20_000).unwrap();
        let dev = p.get(&d).unwrap();
        let s = "01J9ZK0000000000000000000A";
        assert_eq!(dev.card_since(s, "spew#1"), Some(10_000));
        assert_eq!(dev.card_since(s, "spew#2"), Some(20_000));
    }

    #[test]
    fn restores_offline_from_the_cache() {
        let d = key(3);
        let cached = CachedPresence {
            device: d.to_string(),
            seq: 9,
            snapshot: serde_json::to_string(&snap(9, &["spew#3"])).unwrap(),
            received_at: 77,
        };
        let p = RemotePresence::restore(&[cached]);
        let dev = p.get(&d).unwrap();
        assert!(!dev.online);
        assert_eq!(dev.last_seen, Some(77));
        assert_eq!(dev.snapshot.as_ref().unwrap().sessions.len(), 1);
    }

    #[test]
    fn seq_goes_up_and_follows_the_clock() {
        let mut seq = Seq::default();
        assert_eq!(seq.next(100), 100);
        assert_eq!(seq.next(100), 101);
        assert_eq!(seq.next(50), 102);
        assert_eq!(seq.next(500), 500);
    }
}
