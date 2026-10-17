//! The spew roster (4.2): the last good peers response with its ETag, and the staleness rules that decide how
//! much of it still admits devices.

use serde::{Deserialize, Serialize};

use super::{
    DeviceEntry, EndpointId, MAX_DISPLAY, MAX_ROOM_NAME, MAX_VERSION, Me, Member, RelayUrl, Room,
    RoomKind, Roster, RosterError, is_display_text, parse_endpoint_id, private_room,
    this_device_entry,
};
use crate::address::{is_device_name, is_person, is_room};
use crate::spew_api::{ApiError, SpewApi};

/// The largest `ttl_secs` accepted from spew (4.2). Teammates stay admitted for 15 × `ttl_secs` while spew
/// can't be reached, so an absurd value would keep a removed member in for days.
pub const MAX_TTL_SECS: u64 = 86_400;
// Enough dropped entries to diagnose a broken roster without growing without bound.
const MAX_DROPPED_REPORTS: usize = 20;
/// The floor of the teammate window, in seconds.
pub const MIN_TEAMMATE_WINDOW_SECS: u64 = 900;
/// Refreshes caused by an unknown endpoint id trying to connect happen at most this often (4.2).
pub const UNKNOWN_REFRESH_SECS: u64 = 10;

fn default_ttl() -> u64 {
    60
}

/// The body of `GET /api/mesh/v1/peers` (14.4) as spew sent it. Names and keys are checked when the roster is
/// built, so an entry outside the rules (2) is dropped and reported instead of refusing the whole roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peers {
    /// Opaque version; changes when membership or devices change.
    pub version: String,
    /// How long, in seconds, the response may be used before asking again.
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
    /// The caller and all of their registered devices, whatever projects were chosen; the private room.
    #[serde(default)]
    pub me: Option<PeerMember>,
    /// One room per project chosen at consent.
    #[serde(default)]
    pub rooms: Vec<PeerRoom>,
}

/// A room in the peers response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRoom {
    /// Project key.
    pub id: String,
    /// Project name.
    #[serde(default)]
    pub name: Option<String>,
    /// Always `project` from spew; anything else is dropped.
    pub kind: String,
    /// Members of the project.
    #[serde(default)]
    pub members: Vec<PeerMember>,
}

/// A person in the peers response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMember {
    /// spew username.
    pub username: String,
    /// Display name.
    #[serde(default)]
    pub display: Option<String>,
    /// Registered, unrevoked devices.
    #[serde(default)]
    pub devices: Vec<PeerDevice>,
}

/// A device in the peers response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDevice {
    /// Endpoint id, 64 hex characters.
    pub endpoint_id: String,
    /// Device name.
    pub name: String,
    /// Home relay.
    #[serde(default)]
    pub relay: Option<String>,
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
    /// Entries of the cached roster that were dropped for breaking the name rules, for `doctor`.
    pub dropped: Vec<String>,
}

impl SpewView {
    /// One line for `doctor` and `daemon.status`.
    pub fn describe(&self) -> String {
        let line = self.describe_state();
        match self.dropped.len() {
            0 => line,
            n => format!("{line}; {n} roster entries from spew dropped"),
        }
    }

    fn describe_state(&self) -> String {
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
                let cleaned = clean(&self.me, &peers)?;
                build(
                    &self.me,
                    &peers,
                    &cleaned,
                    this_device,
                    device_name,
                    true,
                    true,
                )?;
                self.peers = Some(peers);
                // An odd ETag only costs a full fetch next time; it says nothing about who is admitted.
                self.etag = etag.filter(|t| is_display_text(t, MAX_VERSION));
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
        let Ok(cleaned) = clean(&self.me, peers) else {
            // apply() checked it; only a cache from an older build can get here.
            return self.only_this_device(state, age_secs, this_device, device_name);
        };
        if !own_devices {
            let mut view = self.only_this_device(state, age_secs, this_device, device_name);
            view.dropped = cleaned.dropped;
            return view;
        }
        match build(
            &self.me,
            peers,
            &cleaned,
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
                    dropped: cleaned.dropped,
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
            dropped: Vec::new(),
        }
    }
}

/// A peers response with every entry checked: what's left, and what was dropped.
struct Cleaned {
    /// My devices from `me`, or `None` when spew sent no `me`.
    me_devices: Option<Vec<DeviceEntry>>,
    rooms: Vec<Room>,
    dropped: Vec<String>,
}

/// A name from spew as it may appear in a report: quoted, escaped, cut short.
fn shown(s: &str) -> String {
    let cut: String = s.chars().take(40).collect();
    format!("{cut:?}")
}

fn report(dropped: &mut Vec<String>, line: String) {
    if dropped.len() < MAX_DROPPED_REPORTS {
        dropped.push(line);
    } else if dropped.len() == MAX_DROPPED_REPORTS {
        dropped.push("more entries dropped".into());
    }
}

/// Checks a peers response (2, 4.2, 14.4). A `ttl_secs` outside 1 to 86400, or a `me` for another person,
/// refuses the whole roster; a room, member or device outside the name rules is dropped and reported, and the
/// rest still applies.
fn clean(me: &Me, peers: &Peers) -> Result<Cleaned, RosterError> {
    if !(1..=MAX_TTL_SECS).contains(&peers.ttl_secs) {
        return Err(RosterError(format!(
            "ttl_secs {} from spew is outside 1 to {MAX_TTL_SECS}",
            peers.ttl_secs
        )));
    }
    let mut dropped = Vec::new();
    let me_devices = match &peers.me {
        Some(m) if m.username != me.username => {
            return Err(RosterError(format!(
                "spew's roster is for {}, not {:?}",
                shown(&m.username),
                me.username
            )));
        }
        Some(m) => Some(clean_devices(&m.devices, &me.username, &mut dropped)),
        None => None,
    };
    let mut rooms = Vec::new();
    for room in &peers.rooms {
        if room.kind != "project" {
            report(
                &mut dropped,
                format!(
                    "room {}: kind {} isn't \"project\"",
                    shown(&room.id),
                    shown(&room.kind)
                ),
            );
            continue;
        }
        if !is_room(&room.id) {
            report(
                &mut dropped,
                format!("room {}: id breaks the name rules", shown(&room.id)),
            );
            continue;
        }
        let mut members = Vec::new();
        for member in &room.members {
            if !is_person(&member.username) {
                report(
                    &mut dropped,
                    format!(
                        "person {} in room {}: name breaks the name rules",
                        shown(&member.username),
                        room.id
                    ),
                );
                continue;
            }
            members.push(Member {
                username: member.username.clone(),
                display: member
                    .display
                    .clone()
                    .filter(|d| is_display_text(d, MAX_DISPLAY))
                    .unwrap_or_else(|| member.username.clone()),
                devices: clean_devices(&member.devices, &member.username, &mut dropped),
            });
        }
        rooms.push(Room {
            id: room.id.clone(),
            name: room
                .name
                .clone()
                .filter(|n| is_display_text(n, MAX_ROOM_NAME))
                .unwrap_or_else(|| room.id.clone()),
            kind: RoomKind::Project,
            members,
        });
    }
    Ok(Cleaned {
        me_devices,
        rooms,
        dropped,
    })
}

fn clean_devices(
    devices: &[PeerDevice],
    owner: &str,
    dropped: &mut Vec<String>,
) -> Vec<DeviceEntry> {
    let mut out = Vec::new();
    for device in devices {
        let Ok(endpoint_id) = parse_endpoint_id(&device.endpoint_id) else {
            report(
                dropped,
                format!(
                    "device {} of {owner}: endpoint id isn't 64 hex characters",
                    shown(&device.name)
                ),
            );
            continue;
        };
        if !is_device_name(&device.name) {
            report(
                dropped,
                format!(
                    "device {} of {owner}: name breaks the name rules",
                    shown(&device.name)
                ),
            );
            continue;
        }
        // The relay is only a dialing hint: a bad one is ignored, the device stays.
        let relay = device.relay.as_deref().and_then(|r| {
            let url = r
                .parse::<RelayUrl>()
                .ok()
                .filter(|u| matches!(u.scheme(), "https" | "http"));
            if url.is_none() {
                report(
                    dropped,
                    format!(
                        "relay of device {} of {owner}: not an http(s) URL, ignored",
                        device.name
                    ),
                );
            }
            url
        });
        out.push(DeviceEntry {
            endpoint_id,
            name: device.name.clone(),
            relay,
            addrs: Vec::new(),
        });
    }
    out
}

/// Builds a validated roster from a checked peers response: my private room (this device and my devices from
/// `me`), then the project rooms when teammates are admitted.
fn build(
    me: &Me,
    peers: &Peers,
    cleaned: &Cleaned,
    this_device: &EndpointId,
    device_name: &str,
    teammates: bool,
    own_devices: bool,
) -> Result<Roster, RosterError> {
    let room_devices = || {
        cleaned
            .rooms
            .iter()
            .flat_map(|r| r.members.iter())
            .flat_map(|m| m.devices.iter())
    };
    // When spew lists this device, use the name it was registered with so the key has one name throughout.
    let listed_name = cleaned
        .me_devices
        .iter()
        .flatten()
        .chain(room_devices())
        .find(|d| d.endpoint_id == *this_device)
        .map_or(device_name, |d| d.name.as_str());
    let mut mine: Vec<DeviceEntry> = vec![this_device_entry(this_device, listed_name)];
    if own_devices {
        let own: Vec<&DeviceEntry> = match &cleaned.me_devices {
            Some(devices) => devices.iter().collect(),
            // A spew that sends no `me` (it came later in 14.4): gather my devices from the project rooms.
            None => cleaned
                .rooms
                .iter()
                .flat_map(|r| r.members.iter())
                .filter(|m| m.username == me.username)
                .flat_map(|m| m.devices.iter())
                .collect(),
        };
        for device in own {
            if !mine.iter().any(|d| d.endpoint_id == device.endpoint_id) {
                mine.push(device.clone());
            }
        }
    }
    let mut rooms = vec![private_room(me, mine)];
    if teammates {
        rooms.extend(cleaned.rooms.iter().cloned());
    }
    let mut roster = Roster {
        version: peers.version.clone(),
        fetched_at: 0,
        rooms,
    };
    // Also refuses a roster that lists this device under another person, or one key under two people.
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
