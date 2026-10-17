//! Rosters: who I am, which rooms I share with whom, and who may connect (4.2).
//!
//! Three providers build a [`Roster`]: [`local`] (one private room holding only this device), [`file`] (the
//! roster file) and [`spew`] (the spew directory, with its cache and staleness rules). [`Admission`] turns a
//! roster into the set of endpoint ids that may connect, and [`diff`] compares two of them (4.3).

pub mod diff;
pub mod file;
pub mod local;
pub mod spew;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

pub use diff::{MembershipChange, RoomsChanged};
pub use iroh::{EndpointId, RelayUrl};

use crate::address::{is_device_name, is_person, is_room};

/// Id of every person's private room, which holds only their own devices. It can't collide with a project key
/// or a file room name, because `@` is outside the name rules (2).
pub const PRIVATE_ROOM_ID: &str = "@private";
/// Display name of the private room.
pub const PRIVATE_ROOM_NAME: &str = "private";

/// Longest room display name.
pub const MAX_ROOM_NAME: usize = 100;
/// Longest display name of a person.
pub const MAX_DISPLAY: usize = 64;
/// Longest roster version string.
pub const MAX_VERSION: usize = 200;

// Caps on roster size so a broken or hostile directory can't make the daemon hold unbounded state.
const MAX_ROOMS: usize = 500;
const MAX_MEMBERS_PER_ROOM: usize = 5000;
const MAX_DEVICES_PER_MEMBER: usize = 64;
const MAX_ADDRS_PER_DEVICE: usize = 16;

/// Who this device's person is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// Username (spew username, roster file person name, or the local user name).
    pub username: String,
    /// Display name; the username when none is known.
    pub display: String,
}

/// The rooms this device's person is in, with their people and devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    /// Opaque version: spew's `version`, a hash of the roster file, or `local`.
    pub version: String,
    /// When this roster was last confirmed current, in ms since the Unix epoch.
    pub fetched_at: u64,
    /// The rooms, the private room first.
    pub rooms: Vec<Room>,
}

/// What kind of group a room is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomKind {
    /// A person's own devices.
    Private,
    /// A `[[room]]` from the roster file.
    Team,
    /// A spew project.
    Project,
}

/// A group of people whose devices may connect and see each other's sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    /// Project key, file room name, or [`PRIVATE_ROOM_ID`].
    pub id: String,
    /// Name to show.
    pub name: String,
    /// Kind of room.
    pub kind: RoomKind,
    /// People in the room.
    pub members: Vec<Member>,
}

/// A person in a room and the devices they have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Username.
    pub username: String,
    /// Display name; filled with the username when missing.
    #[serde(default)]
    pub display: String,
    /// Registered devices.
    #[serde(default)]
    pub devices: Vec<DeviceEntry>,
}

/// One device of a member, with optional dialing hints (5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// The device's iroh endpoint id, as 64 lowercase hex characters.
    #[serde(with = "endpoint_hex")]
    pub endpoint_id: EndpointId,
    /// Short device name.
    pub name: String,
    /// Home relay hint.
    #[serde(default)]
    pub relay: Option<RelayUrl>,
    /// Direct address hints.
    #[serde(default)]
    pub addrs: Vec<SocketAddr>,
}

/// A roster that breaks the rules in [`Roster::validate`], or a roster file that can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RosterError(pub String);

impl RosterError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Roster {
    /// The room with this id.
    pub fn room(&self, id: &str) -> Option<&Room> {
        self.rooms.iter().find(|r| r.id == id)
    }

    /// Ids of the rooms that have `username` as a member.
    pub fn rooms_of(&self, username: &str) -> Vec<&str> {
        self.rooms
            .iter()
            .filter(|r| r.has_member(username))
            .map(|r| r.id.as_str())
            .collect()
    }

    /// Checks the rules every roster must meet, whichever provider built it, and fills empty display names.
    ///
    /// Room ids are unique; the private room is [`PRIVATE_ROOM_ID`] and holds only `me`; names fit their
    /// character sets; a person appears once per room; an endpoint id belongs to one person and one device
    /// name across all rooms; relay hints are http(s) URLs; sizes stay under fixed caps.
    pub fn validate(&mut self, me: &Me) -> Result<(), RosterError> {
        if !is_person(&me.username) {
            return Err(RosterError::new(format!(
                "invalid username {:?}",
                me.username
            )));
        }
        if !is_display_text(&self.version, MAX_VERSION) {
            return Err(RosterError::new("invalid roster version"));
        }
        if self.rooms.len() > MAX_ROOMS {
            return Err(RosterError::new(format!(
                "too many rooms ({}, at most {MAX_ROOMS})",
                self.rooms.len()
            )));
        }
        let mut room_ids = BTreeSet::new();
        // endpoint id -> (username, device name), to catch one key claimed twice.
        let mut owners: BTreeMap<EndpointId, (String, String)> = BTreeMap::new();
        for room in &mut self.rooms {
            match room.kind {
                RoomKind::Private => {
                    if room.id != PRIVATE_ROOM_ID {
                        return Err(RosterError::new(format!(
                            "private room has id {:?}",
                            room.id
                        )));
                    }
                    if room.members.len() != 1 || room.members[0].username != me.username {
                        return Err(RosterError::new(
                            "the private room must hold only this device's person",
                        ));
                    }
                }
                RoomKind::Team | RoomKind::Project => {
                    if !is_room(&room.id) {
                        return Err(RosterError::new(format!("invalid room id {:?}", room.id)));
                    }
                }
            }
            if !room_ids.insert(room.id.clone()) {
                return Err(RosterError::new(format!("room {:?} listed twice", room.id)));
            }
            if room.name.is_empty() {
                room.name = room.id.clone();
            }
            if !is_display_text(&room.name, MAX_ROOM_NAME) {
                return Err(RosterError::new(format!(
                    "invalid name for room {:?}",
                    room.id
                )));
            }
            if room.members.len() > MAX_MEMBERS_PER_ROOM {
                return Err(RosterError::new(format!(
                    "room {:?} has too many members",
                    room.id
                )));
            }
            let mut usernames = BTreeSet::new();
            for member in &mut room.members {
                if !is_person(&member.username) {
                    return Err(RosterError::new(format!(
                        "invalid username {:?} in room {:?}",
                        member.username, room.id
                    )));
                }
                if !usernames.insert(member.username.clone()) {
                    return Err(RosterError::new(format!(
                        "{:?} listed twice in room {:?}",
                        member.username, room.id
                    )));
                }
                if member.display.is_empty() {
                    member.display = member.username.clone();
                }
                if !is_display_text(&member.display, MAX_DISPLAY) {
                    return Err(RosterError::new(format!(
                        "invalid display name for {:?}",
                        member.username
                    )));
                }
                if member.devices.len() > MAX_DEVICES_PER_MEMBER {
                    return Err(RosterError::new(format!(
                        "{:?} has too many devices",
                        member.username
                    )));
                }
                let mut in_member = BTreeSet::new();
                for device in &member.devices {
                    validate_device(device)?;
                    if !in_member.insert(device.endpoint_id) {
                        return Err(RosterError::new(format!(
                            "device {} listed twice for {:?}",
                            device.endpoint_id.fmt_short(),
                            member.username
                        )));
                    }
                    match owners.get(&device.endpoint_id) {
                        None => {
                            owners.insert(
                                device.endpoint_id,
                                (member.username.clone(), device.name.clone()),
                            );
                        }
                        Some((owner, name)) => {
                            if *owner != member.username {
                                return Err(RosterError::new(format!(
                                    "device {} is listed for both {:?} and {:?}",
                                    device.endpoint_id.fmt_short(),
                                    owner,
                                    member.username
                                )));
                            }
                            if *name != device.name {
                                return Err(RosterError::new(format!(
                                    "device {} has two names, {:?} and {:?}",
                                    device.endpoint_id.fmt_short(),
                                    name,
                                    device.name
                                )));
                            }
                        }
                    }
                }
            }
        }
        if !room_ids.contains(PRIVATE_ROOM_ID) {
            return Err(RosterError::new("the roster has no private room"));
        }
        Ok(())
    }
}

impl Room {
    /// Whether `username` is a member of this room.
    pub fn has_member(&self, username: &str) -> bool {
        self.members.iter().any(|m| m.username == username)
    }
}

fn validate_device(device: &DeviceEntry) -> Result<(), RosterError> {
    if !is_device_name(&device.name) {
        return Err(RosterError::new(format!(
            "invalid device name {:?}",
            device.name
        )));
    }
    if let Some(relay) = &device.relay
        && !matches!(relay.scheme(), "https" | "http")
    {
        return Err(RosterError::new(format!(
            "relay for {:?} isn't an http(s) URL",
            device.name
        )));
    }
    if device.addrs.len() > MAX_ADDRS_PER_DEVICE {
        return Err(RosterError::new(format!(
            "too many addresses for {:?}",
            device.name
        )));
    }
    Ok(())
}

/// Whether `s` is one printable line of at most `max` characters.
pub fn is_display_text(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
}

/// Parses an endpoint id written as 64 hex characters (the form iroh prints, 4.1). Other encodings iroh accepts
/// are refused so a roster has one spelling per device.
pub fn parse_endpoint_id(s: &str) -> Result<EndpointId, RosterError> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(RosterError::new(format!(
            "endpoint id {s:?} isn't 64 hex characters"
        )));
    }
    s.to_ascii_lowercase()
        .parse::<EndpointId>()
        .map_err(|e| RosterError::new(format!("endpoint id {s:?} isn't a valid key: {e}")))
}

pub(crate) mod endpoint_hex {
    use super::{EndpointId, parse_endpoint_id};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(id: &EndpointId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<EndpointId, D::Error> {
        let s = String::deserialize(d)?;
        parse_endpoint_id(&s).map_err(serde::de::Error::custom)
    }
}

/// What this device knows about a device that may connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedDevice {
    /// The device's endpoint id.
    pub endpoint_id: EndpointId,
    /// Its person's username.
    pub person: String,
    /// Its person's display name.
    pub display: String,
    /// The device name.
    pub name: String,
    /// Whether it is one of my own devices.
    pub own: bool,
    /// Ids of the rooms we share (includes [`PRIVATE_ROOM_ID`] for my own devices).
    pub rooms: BTreeSet<String>,
    /// Home relay hint.
    pub relay: Option<RelayUrl>,
    /// Direct address hints.
    pub addrs: Vec<SocketAddr>,
}

/// The devices that may connect to this one: my own devices, and devices listed in a room that also contains
/// me (4.2). This device itself is never in the set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admission {
    devices: BTreeMap<EndpointId, AdmittedDevice>,
}

impl Admission {
    /// Computes admission from a validated roster.
    pub fn compute(me: &Me, this_device: &EndpointId, roster: &Roster) -> Self {
        let mut devices: BTreeMap<EndpointId, AdmittedDevice> = BTreeMap::new();
        let mut conflicted = BTreeSet::new();
        for room in roster.rooms.iter().filter(|r| r.has_member(&me.username)) {
            for member in &room.members {
                let own = member.username == me.username;
                for device in &member.devices {
                    if device.endpoint_id == *this_device {
                        continue;
                    }
                    match devices.get_mut(&device.endpoint_id) {
                        Some(known) => {
                            // validate() rules this out; stay closed if an unvalidated roster gets here.
                            if known.person != member.username || known.name != device.name {
                                conflicted.insert(device.endpoint_id);
                            }
                            known.rooms.insert(room.id.clone());
                            if known.relay.is_none() {
                                known.relay.clone_from(&device.relay);
                            }
                            for addr in &device.addrs {
                                if !known.addrs.contains(addr) {
                                    known.addrs.push(*addr);
                                }
                            }
                        }
                        None => {
                            let mut rooms = BTreeSet::new();
                            rooms.insert(room.id.clone());
                            if own {
                                rooms.insert(PRIVATE_ROOM_ID.to_string());
                            }
                            devices.insert(
                                device.endpoint_id,
                                AdmittedDevice {
                                    endpoint_id: device.endpoint_id,
                                    person: member.username.clone(),
                                    display: member.display.clone(),
                                    name: device.name.clone(),
                                    own,
                                    rooms,
                                    relay: device.relay.clone(),
                                    addrs: device.addrs.clone(),
                                },
                            );
                        }
                    }
                }
            }
        }
        for id in conflicted {
            devices.remove(&id);
        }
        Self { devices }
    }

    /// The admitted device with this endpoint id.
    pub fn get(&self, id: &EndpointId) -> Option<&AdmittedDevice> {
        self.devices.get(id)
    }

    /// Whether this endpoint id may connect.
    pub fn is_admitted(&self, id: &EndpointId) -> bool {
        self.devices.contains_key(id)
    }

    /// Whether any device other than this one is admitted; the endpoint runs only then (5.1).
    pub fn admits_others(&self) -> bool {
        !self.devices.is_empty()
    }

    /// Whether the device is admitted and shares the room `room_id` with me.
    pub fn shares_room(&self, id: &EndpointId, room_id: &str) -> bool {
        self.devices
            .get(id)
            .is_some_and(|d| d.rooms.contains(room_id))
    }

    /// All admitted devices, ordered by endpoint id.
    pub fn devices(&self) -> impl Iterator<Item = &AdmittedDevice> {
        self.devices.values()
    }

    /// Number of admitted devices.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether no device is admitted.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// The roster in force with its admission, replaced as a whole so every change yields a [`MembershipChange`].
#[derive(Debug, Clone)]
pub struct ActiveRoster {
    me: Me,
    this_device: EndpointId,
    roster: Roster,
    admission: Admission,
}

impl ActiveRoster {
    /// Starts from a validated roster.
    pub fn new(me: Me, this_device: EndpointId, roster: Roster) -> Self {
        let admission = Admission::compute(&me, &this_device, &roster);
        Self {
            me,
            this_device,
            roster,
            admission,
        }
    }

    /// Who I am.
    pub fn me(&self) -> &Me {
        &self.me
    }

    /// The roster in force.
    pub fn roster(&self) -> &Roster {
        &self.roster
    }

    /// Who may connect.
    pub fn admission(&self) -> &Admission {
        &self.admission
    }

    /// Replaces the roster (and possibly who I am) and returns what changed for admission (4.3).
    pub fn replace(&mut self, me: Me, roster: Roster) -> MembershipChange {
        let admission = Admission::compute(&me, &self.this_device, &roster);
        let change = diff::diff(&self.admission, &admission);
        self.me = me;
        self.roster = roster;
        self.admission = admission;
        change
    }
}

/// A private room holding `me` with the given devices.
fn private_room(me: &Me, devices: Vec<DeviceEntry>) -> Room {
    Room {
        id: PRIVATE_ROOM_ID.to_string(),
        name: PRIVATE_ROOM_NAME.to_string(),
        kind: RoomKind::Private,
        members: vec![Member {
            username: me.username.clone(),
            display: me.display.clone(),
            devices,
        }],
    }
}

/// The entry for this device in its own private room, with no dialing hints.
fn this_device_entry(this_device: &EndpointId, device_name: &str) -> DeviceEntry {
    DeviceEntry {
        endpoint_id: *this_device,
        name: device_name.to_string(),
        relay: None,
        addrs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn dev(seed: u8, name: &str) -> DeviceEntry {
        this_device_entry(&key(seed), name)
    }

    fn member(name: &str, devices: Vec<DeviceEntry>) -> Member {
        Member {
            username: name.into(),
            display: String::new(),
            devices,
        }
    }

    fn sam() -> Me {
        Me {
            username: "sam".into(),
            display: "Sam".into(),
        }
    }

    fn roster(rooms: Vec<Room>) -> Roster {
        Roster {
            version: "v1".into(),
            fetched_at: 0,
            rooms,
        }
    }

    fn team(id: &str, members: Vec<Member>) -> Room {
        Room {
            id: id.into(),
            name: String::new(),
            kind: RoomKind::Team,
            members,
        }
    }

    #[test]
    fn names() {
        assert!(is_person("sam"));
        assert!(is_person("a.b_c-1"));
        assert!(is_person("-sam"));
        assert!(is_person(&"a".repeat(32)));
        assert!(!is_person(&"a".repeat(33)));
        assert!(!is_person(""));
        assert!(!is_person("me"));
        assert!(!is_person("Sam"));
        assert!(!is_person("sam/x"));
        assert!(!is_person("sam@x"));
        assert!(!is_person("sám"));
        assert!(is_room("spew"));
        assert!(!is_room("me"));
        assert!(!is_room("spew#1"));
        assert!(!is_room(PRIVATE_ROOM_ID));
        assert!(is_device_name("maya-mbp"));
        assert!(is_device_name("me"));
        assert!(is_device_name(&"d".repeat(48)));
        assert!(!is_device_name(&"d".repeat(49)));
        assert!(!is_device_name("Maya-MBP"));
        assert!(is_display_text("Maya K", 64));
        assert!(!is_display_text("two\nlines", 64));
        assert!(!is_display_text("  ", 64));
    }

    #[test]
    fn endpoint_ids_must_be_hex() {
        let id = key(1);
        let hex = id.to_string();
        assert_eq!(parse_endpoint_id(&hex).unwrap(), id);
        assert_eq!(parse_endpoint_id(&hex.to_ascii_uppercase()).unwrap(), id);
        assert!(parse_endpoint_id(&id.to_z32()).is_err());
        assert!(parse_endpoint_id(&hex[..63]).is_err());
        assert!(parse_endpoint_id(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn validate_rejects_bad_rosters() {
        let me = sam();
        let ok = || {
            roster(vec![
                private_room(&me, vec![dev(1, "sam-mbp")]),
                team(
                    "team",
                    vec![
                        member("sam", vec![]),
                        member("maya", vec![dev(2, "maya-mbp")]),
                    ],
                ),
            ])
        };
        let mut r = ok();
        r.validate(&me).unwrap();
        assert_eq!(r.rooms[1].name, "team");
        assert_eq!(r.rooms[1].members[1].display, "maya");

        let mut r = ok();
        r.rooms.remove(0);
        assert!(r.validate(&me).is_err(), "no private room");

        let mut r = ok();
        r.rooms[0].members.push(member("maya", vec![]));
        assert!(r.validate(&me).is_err(), "someone else in the private room");

        let mut r = ok();
        r.rooms[1].members[1].devices.push(dev(1, "sam-mbp"));
        assert!(r.validate(&me).is_err(), "one key for two people");

        let mut r = ok();
        r.rooms[1].members[0].devices.push(dev(1, "sam-other"));
        assert!(r.validate(&me).is_err(), "one key with two names");

        let mut r = ok();
        r.rooms.push(team("team", vec![]));
        assert!(r.validate(&me).is_err(), "duplicate room");

        let mut r = ok();
        r.rooms[1].id = "@private".into();
        assert!(r.validate(&me).is_err(), "reserved id");

        let mut r = ok();
        r.rooms[1].members.push(member("maya", vec![]));
        assert!(r.validate(&me).is_err(), "person twice in a room");

        let mut r = ok();
        r.rooms[1].members[1].devices[0].relay = Some("ftp://relay.example.com".parse().unwrap());
        assert!(r.validate(&me).is_err(), "relay scheme");
    }

    #[test]
    fn admission_follows_shared_rooms() {
        let me = sam();
        let this = key(1);
        let mut r = roster(vec![
            private_room(&me, vec![dev(1, "sam-mbp"), dev(2, "sam-linux")]),
            team(
                "team",
                vec![
                    member("sam", vec![dev(1, "sam-mbp")]),
                    member("maya", vec![dev(3, "maya-mbp")]),
                ],
            ),
            // A room without me: its devices are not admitted.
            team("other", vec![member("lee", vec![dev(4, "lee-mac")])]),
        ]);
        r.validate(&me).unwrap();
        let a = Admission::compute(&me, &this, &r);
        assert!(!a.is_admitted(&this), "never this device");
        assert!(a.get(&key(2)).unwrap().own);
        assert!(a.shares_room(&key(2), PRIVATE_ROOM_ID));
        let maya = a.get(&key(3)).unwrap();
        assert!(!maya.own);
        assert_eq!(maya.person, "maya");
        assert_eq!(maya.rooms.iter().collect::<Vec<_>>(), vec!["team"]);
        assert!(!a.is_admitted(&key(4)));
        assert_eq!(a.len(), 2);
        assert!(a.admits_others());
    }

    #[test]
    fn admission_drops_conflicting_keys() {
        let me = sam();
        // Not validated on purpose: compute must still stay closed.
        let r = roster(vec![
            private_room(&me, vec![dev(1, "sam-mbp")]),
            team(
                "a",
                vec![
                    member("sam", vec![]),
                    member("maya", vec![dev(3, "maya-mbp")]),
                ],
            ),
            team(
                "b",
                vec![
                    member("sam", vec![]),
                    member("lee", vec![dev(3, "lee-mac")]),
                ],
            ),
        ]);
        let a = Admission::compute(&me, &key(1), &r);
        assert!(!a.is_admitted(&key(3)));
    }
}
