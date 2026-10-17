//! Local presence (5.6) and the directory listing (6.3): every session this device can see, local or from a
//! peer's presence, in one shape.

use std::collections::{BTreeMap, HashMap};

use crate::control::protocol::{DeviceRow, Directory, PersonGroup, RoomGroup, SessionRow};
use crate::store::{HeldCard, SessionRecord};
use crate::wire::{LocalPresence, PresenceSession, SessionKind};

use super::room_from_spew_json;

/// The name shown for sessions with no room (11).
pub const PRIVATE_LABEL: &str = "private: no .spew.json";

/// One session as the listing and address resolution see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its person.
    pub person: String,
    /// Its device's name.
    pub device: String,
    /// Its device's endpoint id.
    pub device_id: String,
    /// The device is this one.
    pub this_device: bool,
    /// The device is one of this person's own.
    pub own_device: bool,
    /// The device is online (always for this device).
    pub online: bool,
    /// The session, with every field the device let us see.
    pub session: PresenceSession,
    /// When each card was first seen on the session, ms, for "most recently advertised" (2.2).
    pub card_since: HashMap<String, u64>,
}

impl Entry {
    /// `person@device/name`.
    pub fn address(&self) -> String {
        format!("{}@{}/{}", self.person, self.device, self.session.name)
    }
}

/// A local session as presence filtering needs it (5.6).
pub fn local_presence(record: &SessionRecord, cards: &[HeldCard], queue: u32) -> LocalPresence {
    LocalPresence {
        session: PresenceSession {
            session: record.id.clone(),
            name: record.name.clone(),
            client: record.client.clone(),
            kind: record.kind,
            repo: record.repo.clone(),
            branch: record.branch.clone(),
            place: record.place.clone(),
            room: record.room.clone(),
            cards: cards.iter().map(|c| c.card.clone()).collect(),
            activity: record.activity.clone(),
            state: record.state,
            since: record.state_since,
            reach: record.reach,
            queue,
        },
        room_from_spew_json: room_from_spew_json(record),
        activity_project: record.activity_project.clone(),
    }
}

/// What the listing is built from.
#[derive(Debug, Clone)]
pub struct ListingInput<'a> {
    /// You.
    pub me: &'a str,
    /// This device's name.
    pub device: &'a str,
    /// Every session known, online or not.
    pub entries: &'a [Entry],
    /// The device rows, already limited to what should show.
    pub devices: Vec<DeviceRow>,
    /// The caller's session on this device.
    pub this_session: Option<&'a str>,
    /// `match`.
    pub filter: Option<&'a str>,
    /// `include_offline`.
    pub include_offline: bool,
    /// Room names from the roster, by id.
    pub room_names: &'a HashMap<String, String>,
    /// The caller's unread messages.
    pub unread: usize,
}

fn matches(entry: &Entry, needle: &str) -> bool {
    let s = &entry.session;
    let mut fields = vec![entry.address(), s.client.clone()];
    fields.extend(s.repo.clone());
    fields.extend(s.branch.clone());
    fields.extend(s.cards.iter().cloned());
    fields.extend(s.activity.clone());
    fields.iter().any(|f| f.to_lowercase().contains(needle))
}

/// The directory listing (6.3): grouped by room, then by person, your own first.
pub fn listing(input: &ListingInput<'_>) -> Directory {
    let needle = input
        .filter
        .map(|f| f.trim().to_lowercase())
        .filter(|f| !f.is_empty());
    let mut rooms: BTreeMap<Option<String>, BTreeMap<String, Vec<&Entry>>> = BTreeMap::new();
    for entry in input.entries {
        if entry.session.kind == SessionKind::Terminal
            || !(entry.online || input.include_offline)
            || needle.as_deref().is_some_and(|n| !matches(entry, n))
        {
            continue;
        }
        rooms
            .entry(entry.session.room.clone())
            .or_default()
            .entry(entry.person.clone())
            .or_default()
            .push(entry);
    }
    let mut groups: Vec<RoomGroup> = rooms
        .into_iter()
        .map(|(room, people)| {
            let mut people: Vec<PersonGroup> = people
                .into_iter()
                .map(|(person, mut entries)| {
                    entries.sort_by(|a, b| {
                        (!a.this_device, &a.device, &a.session.name).cmp(&(
                            !b.this_device,
                            &b.device,
                            &b.session.name,
                        ))
                    });
                    PersonGroup {
                        own: person == input.me,
                        person,
                        sessions: entries
                            .into_iter()
                            .map(|e| row(e, input.this_session))
                            .collect(),
                    }
                })
                .collect();
            people.sort_by(|a, b| (!a.own, &a.person).cmp(&(!b.own, &b.person)));
            let name = match &room {
                Some(id) => input
                    .room_names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.clone()),
                None => PRIVATE_LABEL.to_string(),
            };
            RoomGroup { room, name, people }
        })
        .collect();
    groups.sort_by(|a, b| {
        let own = |g: &RoomGroup| g.people.iter().any(|p| p.own);
        (!own(a), a.room.is_none(), &a.name).cmp(&(!own(b), b.room.is_none(), &b.name))
    });
    Directory {
        me: input.me.to_string(),
        device: input.device.to_string(),
        rooms: groups,
        devices: input.devices.clone(),
        unread: input.unread,
    }
}

fn row(entry: &Entry, this_session: Option<&str>) -> SessionRow {
    let s = &entry.session;
    SessionRow {
        address: entry.address(),
        person: entry.person.clone(),
        device: entry.device.clone(),
        name: s.name.clone(),
        client: s.client.clone(),
        kind: s.kind,
        state: s.state,
        since: s.since,
        repo: s.repo.clone(),
        branch: s.branch.clone(),
        place: s.place.clone(),
        room: s.room.clone(),
        cards: s.cards.clone(),
        activity: s.activity.clone(),
        reach: s.reach,
        queue: s.queue,
        this: entry.this_device && this_session == Some(s.session.as_str()),
        online: entry.online,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ActivityState, Reach};

    fn entry(person: &str, device: &str, name: &str, room: Option<&str>, online: bool) -> Entry {
        Entry {
            person: person.into(),
            device: device.into(),
            device_id: format!("{device}-id"),
            this_device: device == "sam-mac",
            own_device: person == "sam",
            online,
            session: PresenceSession {
                session: format!("{name}-id"),
                name: name.into(),
                client: "claude".into(),
                kind: SessionKind::Live,
                repo: Some("acme/api".into()),
                branch: Some("main".into()),
                place: Some("api".into()),
                room: room.map(str::to_string),
                cards: if name == "claude-web" {
                    vec!["spew#42".into()]
                } else {
                    vec![]
                },
                activity: None,
                state: ActivityState::Idle,
                since: 1,
                reach: Reach::Hooked,
                queue: 0,
            },
            card_since: HashMap::new(),
        }
    }

    #[test]
    fn groups_by_room_then_person_own_first() {
        let entries = vec![
            entry("maya", "maya-mbp", "claude-api", Some("spew"), true),
            entry("sam", "sam-mac", "claude-api", Some("spew"), true),
            entry("sam", "sam-mac", "codex-notes", None, true),
            entry("maya", "maya-mbp", "claude-web", Some("web"), true),
            entry("maya", "maya-mbp", "claude-old", Some("spew"), false),
        ];
        let names = HashMap::from([("spew".to_string(), "spew app".to_string())]);
        let input = ListingInput {
            me: "sam",
            device: "sam-mac",
            entries: &entries,
            devices: vec![],
            this_session: Some("claude-api-id"),
            filter: None,
            include_offline: false,
            room_names: &names,
            unread: 2,
        };
        let dir = listing(&input);
        let rooms: Vec<&str> = dir.rooms.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(rooms, ["spew app", PRIVATE_LABEL, "web"]);
        let spew = &dir.rooms[0];
        assert_eq!(spew.people[0].person, "sam");
        assert!(spew.people[0].sessions[0].this);
        assert_eq!(spew.people[1].sessions.len(), 1, "offline rows stay out");
        assert_eq!(dir.unread, 2);

        let offline = listing(&ListingInput {
            include_offline: true,
            ..input.clone()
        });
        assert_eq!(offline.rooms[0].people[1].sessions.len(), 2);

        let filtered = listing(&ListingInput {
            filter: Some("SPEW#42"),
            ..input
        });
        assert_eq!(filtered.rooms.len(), 1);
        assert_eq!(filtered.rooms[0].people[0].sessions[0].name, "claude-web");
    }
}
