//! The spew roster (4.2): the last good peers response with its ETag, and the staleness rules that decide how
//! much of it still admits devices.

use serde::{Deserialize, Serialize};

use super::{
    DeviceEntry, EndpointId, MAX_VERSION, Me, Room, RoomKind, Roster, RosterError, is_display_text,
    private_room, this_device_entry,
};
use crate::spew_api::{ApiError, SpewApi};

/// The largest `ttl_secs` accepted from spew. Teammates stay admitted for 15 × `ttl_secs` while spew can't be
/// reached, so an absurd value would keep a removed member in for days.
pub const MAX_TTL_SECS: u64 = 86_400;
/// The floor of the teammate window, in seconds.
pub const MIN_TEAMMATE_WINDOW_SECS: u64 = 900;
/// Refreshes caused by an unknown endpoint id trying to connect happen at most this often (4.2).
pub const UNKNOWN_REFRESH_SECS: u64 = 10;

fn default_ttl() -> u64 {
    60
}

/// The body of `GET /api/mesh/v1/peers` (14.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peers {
    /// Opaque version; changes when membership or devices change.
    pub version: String,
    /// How long, in seconds, the response may be used before asking again.
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
    /// One room per project chosen at consent.
    pub rooms: Vec<Room>,
}

/// The outcome of one peers request that spew answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeersFetch {
    /// A new roster (200), with its `ETag` when spew sent one.
    Modified {
        /// The response's `ETag` header.
        etag: Option<String>,
        /// The roster.
        peers: Peers,
    },
    /// The roster sent with `If-None-Match` is still current (304).
    NotModified,
}

/// How much of the cached roster is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpewRosterState {
    /// No roster has been fetched yet: only this device.
    NoRoster,
    /// Teammates and own devices are admitted.
    Fresh,
    /// spew couldn't be reached for longer than the teammate window: only own devices.
    Stale,
    /// spew couldn't be reached for longer than `max_stale_seconds`: only this device.
    Expired,
    /// A 401 that a refresh couldn't fix: teammates dropped; own devices kept while the roster is younger than
    /// `max_stale_seconds`.
    SignedOut,
}

/// The roster in force at one moment, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpewView {
    /// Which staleness rule applies.
    pub state: SpewRosterState,
    /// Whether teammates' devices are admitted.
    pub teammates: bool,
    /// Whether my other devices from the roster are admitted.
    pub own_devices: bool,
    /// Seconds since the roster was last confirmed, when there is one.
    pub age_secs: Option<u64>,
    /// The roster, already reduced to what the state allows.
    pub roster: Roster,
}

impl SpewView {
    /// One line for `doctor` and `daemon.status`.
    pub fn describe(&self) -> String {
        let age = self.age_secs.unwrap_or(0);
        match self.state {
            SpewRosterState::NoRoster => {
                "no roster from spew yet: only this device is admitted".to_string()
            }
            SpewRosterState::Fresh => format!("roster from spew, confirmed {age} s ago"),
            SpewRosterState::Stale => format!(
                "spew not reached for {age} s: teammates dropped, your own devices kept"
            ),
            SpewRosterState::Expired => {
                format!("spew not reached for {age} s: only this device is admitted")
            }
            SpewRosterState::SignedOut if self.own_devices => {
                "signed out of spew: teammates dropped, your own devices kept for now; run `spew mesh login`"
                    .to_string()
            }
            SpewRosterState::SignedOut => {
                "signed out of spew: only this device is admitted; run `spew mesh login`".to_string()
            }
        }
    }
}

/// The spew roster cache: who I am, the last good peers response, its `ETag`, when it was last confirmed and
/// whether spew signed us out. The daemon keeps it in the store so staleness survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpewRosterCache {
    me: Me,
    etag: Option<String>,
    peers: Option<Peers>,
    fetched_at: Option<u64>,
    signed_out: bool,
}

impl SpewRosterCache {
    /// An empty cache for the person signed in as `me`.
    pub fn new(me: Me) -> Self {
        Self {
            me,
            etag: None,
            peers: None,
            fetched_at: None,
            signed_out: false,
        }
    }

    /// Restores a cache saved with [`SpewRosterCache::to_json`]. A cache that doesn't parse or belongs to
    /// another person gives an empty one, which admits only this device.
    pub fn restore(json: &str, me: Me) -> Self {
        match serde_json::from_str::<Self>(json) {
            Ok(cache) if cache.me.username == me.username => Self { me, ..cache },
            _ => Self::new(me),
        }
    }

    /// The cache as JSON, for the store.
    pub fn to_json(&self) -> String {
        // A struct of strings, numbers and bools always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Who I am.
    pub fn me(&self) -> &Me {
        &self.me
    }

    /// The `If-None-Match` value for the next request: the last `ETag`, while there is a roster to keep.
    pub fn if_none_match(&self) -> Option<&str> {
        self.peers.as_ref().and(self.etag.as_deref())
    }

    /// When the roster was last confirmed, in ms.
    pub fn fetched_at(&self) -> Option<u64> {
        self.fetched_at
    }

    /// Whether spew signed us out.
    pub fn is_signed_out(&self) -> bool {
        self.signed_out
    }

    /// The last good peers response.
    pub fn peers(&self) -> Option<&Peers> {
        self.peers.as_ref()
    }

    /// Takes in an answer from spew. A roster that breaks the rules is refused and the last good one stays.
    pub fn apply(
        &mut self,
        fetch: PeersFetch,
        this_device: &EndpointId,
        device_name: &str,
        now_ms: u64,
    ) -> Result<(), RosterError> {
        match fetch {
            PeersFetch::Modified { etag, peers } => {
                if let Some(tag) = &etag
                    && !is_display_text(tag, MAX_VERSION)
                {
                    return Err(RosterError("invalid ETag from spew".into()));
                }
                build(&self.me, &peers, this_device, device_name, true, true)?;
                self.peers = Some(peers);
                self.etag = etag;
            }
            PeersFetch::NotModified => {
                if self.peers.is_none() {
                    return Err(RosterError(
                        "spew answered 304 but there is no roster to keep".into(),
                    ));
                }
            }
        }
        self.fetched_at = Some(now_ms);
        self.signed_out = false;
        Ok(())
    }

    /// Records a 401 that a token refresh couldn't fix: teammates drop at once (4.2).
    pub fn mark_signed_out(&mut self) {
        self.signed_out = true;
    }

    /// The roster in force at `now_ms` under the staleness rules.
    ///
    /// Teammates stay while the roster is younger than max(15 × `ttl_secs`, 900) seconds, own devices while
    /// it is younger than `max_stale_seconds`, then only this device remains. A signed-out cache drops
    /// teammates at once and keeps own devices under the same age limit.
    pub fn view(
        &self,
        this_device: &EndpointId,
        device_name: &str,
        now_ms: u64,
        max_stale_seconds: u64,
    ) -> SpewView {
        let (Some(peers), Some(fetched_at)) = (&self.peers, self.fetched_at) else {
            return self.only_this_device(
                SpewRosterState::NoRoster,
                None,
                this_device,
                device_name,
            );
        };
        let age_ms = now_ms.saturating_sub(fetched_at);
        let age_secs = Some(age_ms / 1000);
        let own_window_ms = max_stale_seconds.saturating_mul(1000);
        // The teammate window never outlasts the own-device window: teammates must not outstay my devices.
        let teammate_window_ms = peers
            .ttl_secs
            .saturating_mul(15)
            .max(MIN_TEAMMATE_WINDOW_SECS)
            .saturating_mul(1000)
            .min(own_window_ms);
        let own_ok = age_ms < own_window_ms;
        let (state, teammates, own_devices) = if self.signed_out {
            (SpewRosterState::SignedOut, false, own_ok)
        } else if age_ms < teammate_window_ms {
            (SpewRosterState::Fresh, true, true)
        } else if own_ok {
            (SpewRosterState::Stale, false, true)
        } else {
            (SpewRosterState::Expired, false, false)
        };
        if !own_devices {
            return self.only_this_device(state, age_secs, this_device, device_name);
        }
        match build(
            &self.me,
            peers,
            this_device,
            device_name,
            teammates,
            own_devices,
        ) {
            Ok(mut roster) => {
                roster.fetched_at = fetched_at;
                SpewView {
                    state,
                    teammates,
                    own_devices,
                    age_secs,
                    roster,
                }
            }
            // Only reachable when this device's key changed since the roster was fetched.
            Err(_) => self.only_this_device(state, age_secs, this_device, device_name),
        }
    }

    fn only_this_device(
        &self,
        state: SpewRosterState,
        age_secs: Option<u64>,
        this_device: &EndpointId,
        device_name: &str,
    ) -> SpewView {
        SpewView {
            state,
            teammates: false,
            own_devices: false,
            age_secs,
            roster: Roster {
                version: self
                    .peers
                    .as_ref()
                    .map_or_else(|| "none".to_string(), |p| p.version.clone()),
                fetched_at: self.fetched_at.unwrap_or(0),
                rooms: vec![private_room(
                    &self.me,
                    vec![this_device_entry(this_device, device_name)],
                )],
            },
        }
    }
}

/// Builds a validated roster from a peers response: my private room (my devices from every project room, plus
/// this device), then the project rooms when teammates are admitted.
fn build(
    me: &Me,
    peers: &Peers,
    this_device: &EndpointId,
    device_name: &str,
    teammates: bool,
    own_devices: bool,
) -> Result<Roster, RosterError> {
    if peers.ttl_secs > MAX_TTL_SECS {
        return Err(RosterError(format!(
            "ttl_secs {} from spew is over {MAX_TTL_SECS}",
            peers.ttl_secs
        )));
    }
    for room in &peers.rooms {
        if room.kind != RoomKind::Project {
            return Err(RosterError(format!(
                "room {:?} from spew isn't a project room",
                room.id
            )));
        }
    }
    // When spew lists this device, use the name it was registered with so the key has one name throughout.
    let listed_name = peers
        .rooms
        .iter()
        .flat_map(|r| r.members.iter())
        .flat_map(|m| m.devices.iter())
        .find(|d| d.endpoint_id == *this_device)
        .map_or(device_name, |d| d.name.as_str());
    let mut mine: Vec<DeviceEntry> = vec![this_device_entry(this_device, listed_name)];
    if own_devices {
        for room in &peers.rooms {
            for member in room.members.iter().filter(|m| m.username == me.username) {
                for device in &member.devices {
                    if !mine.iter().any(|d| d.endpoint_id == device.endpoint_id) {
                        mine.push(device.clone());
                    }
                }
            }
        }
    }
    let mut rooms = vec![private_room(me, mine)];
    if teammates {
        rooms.extend(peers.rooms.iter().cloned());
    }
    let mut roster = Roster {
        version: peers.version.clone(),
        fetched_at: 0,
        rooms,
    };
    // Also refuses a roster that lists this device under another person.
    roster.validate(me)?;
    Ok(roster)
}

/// What one roster refresh did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// spew sent a new roster and it was taken.
    Updated,
    /// spew confirmed the cached roster (304).
    Unchanged,
    /// spew refused our token and a refresh didn't help: signed out.
    SignedOut,
    /// spew couldn't be reached or sent something unusable; the last good roster stays.
    Failed(String),
}

/// Asks spew for the roster with `If-None-Match` and applies the answer to the cache (4.2, 14.4).
pub async fn refresh(
    cache: &mut SpewRosterCache,
    api: &SpewApi,
    this_device: &EndpointId,
    device_name: &str,
) -> RefreshOutcome {
    match api.fetch_peers(cache.if_none_match()).await {
        Ok(fetch) => {
            let unchanged = fetch == PeersFetch::NotModified;
            match cache.apply(fetch, this_device, device_name, api.now_ms()) {
                Ok(()) if unchanged => RefreshOutcome::Unchanged,
                Ok(()) => RefreshOutcome::Updated,
                Err(e) => RefreshOutcome::Failed(format!("unusable roster from spew: {e}")),
            }
        }
        Err(ApiError::SignedOut(_) | ApiError::NotSignedIn) => {
            cache.mark_signed_out();
            RefreshOutcome::SignedOut
        }
        Err(e) => RefreshOutcome::Failed(e.to_string()),
    }
}

/// Limits refreshes caused by unknown endpoint ids to one every [`UNKNOWN_REFRESH_SECS`] (4.2).
#[derive(Debug, Clone, Default)]
pub struct UnknownPeerRefresh {
    last: Option<u64>,
}

impl UnknownPeerRefresh {
    /// Whether an unknown endpoint id at `now_ms` may trigger a refresh; records it when it may.
    pub fn allow(&mut self, now_ms: u64) -> bool {
        let ok = self
            .last
            .is_none_or(|t| now_ms.saturating_sub(t) >= UNKNOWN_REFRESH_SECS * 1000);
        if ok {
            self.last = Some(now_ms);
        }
        ok
    }
}
