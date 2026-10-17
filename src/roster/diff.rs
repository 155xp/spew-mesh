//! Membership changes between two admission sets (4.3).

use super::{Admission, EndpointId};

/// What changed for admission when the roster changed.
///
/// The daemon closes links to `removed` devices at once, drops their presence and fails mail queued for them
/// with "no longer in a shared room". A device whose person changed counts as removed and added, because the
/// person behind a link must never change while it is up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MembershipChange {
    /// Devices admitted now that weren't before.
    pub added: Vec<EndpointId>,
    /// Devices no longer admitted.
    pub removed: Vec<EndpointId>,
    /// Devices still admitted whose shared rooms changed; presence sent to them must be filtered again.
    pub rooms_changed: Vec<RoomsChanged>,
    /// Devices still admitted whose name, display name or dialing hints changed.
    pub updated: Vec<EndpointId>,
    /// Whether any device other than this one was admitted before.
    pub others_before: bool,
    /// Whether any device other than this one is admitted now.
    pub others_after: bool,
}

/// Shared rooms gained and lost by one device that stays admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomsChanged {
    /// The device.
    pub endpoint_id: EndpointId,
    /// Rooms it now shares with me.
    pub joined: Vec<String>,
    /// Rooms it no longer shares with me.
    pub left: Vec<String>,
}

impl MembershipChange {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.rooms_changed.is_empty()
            && self.updated.is_empty()
    }

    /// `Some(true)` when the endpoint must come up, `Some(false)` when it must close, `None` when it stays as
    /// it is (5.1).
    pub fn network_change(&self) -> Option<bool> {
        (self.others_before != self.others_after).then_some(self.others_after)
    }
}

/// Compares two admission sets.
pub fn diff(old: &Admission, new: &Admission) -> MembershipChange {
    let mut change = MembershipChange {
        others_before: old.admits_others(),
        others_after: new.admits_others(),
        ..MembershipChange::default()
    };
    for before in old.devices() {
        match new.get(&before.endpoint_id) {
            None => change.removed.push(before.endpoint_id),
            Some(after) if after.person != before.person || after.own != before.own => {
                change.removed.push(before.endpoint_id);
                change.added.push(before.endpoint_id);
            }
            Some(after) => {
                if after.rooms != before.rooms {
                    change.rooms_changed.push(RoomsChanged {
                        endpoint_id: before.endpoint_id,
                        joined: after.rooms.difference(&before.rooms).cloned().collect(),
                        left: before.rooms.difference(&after.rooms).cloned().collect(),
                    });
                }
                if after.name != before.name
                    || after.display != before.display
                    || after.relay != before.relay
                    || after.addrs != before.addrs
                {
                    change.updated.push(before.endpoint_id);
                }
            }
        }
    }
    for after in new.devices() {
        if !old.is_admitted(&after.endpoint_id) {
            change.added.push(after.endpoint_id);
        }
    }
    change.added.sort();
    change
}

#[cfg(test)]
mod tests {
    use super::super::{DeviceEntry, Me, Member, PRIVATE_ROOM_ID, RoomKind, Roster, private_room};
    use super::*;
    use crate::roster::Room;

    fn key(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn dev(seed: u8, name: &str) -> DeviceEntry {
        DeviceEntry {
            endpoint_id: key(seed),
            name: name.into(),
            relay: None,
            addrs: vec![],
        }
    }

    fn me() -> Me {
        Me {
            username: "sam".into(),
            display: "sam".into(),
        }
    }

    fn team(id: &str, members: Vec<(&str, Vec<DeviceEntry>)>) -> Room {
        Room {
            id: id.into(),
            name: id.into(),
            kind: RoomKind::Team,
            members: members
                .into_iter()
                .map(|(n, devices)| Member {
                    username: n.into(),
                    display: n.into(),
                    devices,
                })
                .collect(),
        }
    }

    fn admission(rooms: Vec<Room>) -> Admission {
        let me = me();
        let mut all = vec![private_room(&me, vec![dev(1, "sam-mbp")])];
        all.extend(rooms);
        let mut roster = Roster {
            version: "v".into(),
            fetched_at: 0,
            rooms: all,
        };
        roster.validate(&me).unwrap();
        Admission::compute(&me, &key(1), &roster)
    }

    #[test]
    fn removal_addition_and_room_moves() {
        let old = admission(vec![
            team(
                "a",
                vec![
                    ("sam", vec![]),
                    ("maya", vec![dev(2, "maya-mbp")]),
                    ("lee", vec![dev(3, "lee-mac")]),
                ],
            ),
            team(
                "b",
                vec![("sam", vec![]), ("maya", vec![dev(2, "maya-mbp")])],
            ),
        ]);
        let new = admission(vec![team(
            "a",
            vec![
                ("sam", vec![]),
                ("maya", vec![dev(2, "maya-mbp")]),
                ("kim", vec![dev(4, "kim-linux")]),
            ],
        )]);
        let change = diff(&old, &new);
        assert_eq!(change.removed, vec![key(3)]);
        assert_eq!(change.added, vec![key(4)]);
        assert_eq!(
            change.rooms_changed,
            vec![RoomsChanged {
                endpoint_id: key(2),
                joined: vec![],
                left: vec!["b".into()],
            }]
        );
        assert!(change.updated.is_empty());
        assert_eq!(change.network_change(), None);
        assert!(!change.is_empty());
        assert!(diff(&new, &new).is_empty());
    }

    #[test]
    fn person_change_is_remove_and_add() {
        let old = admission(vec![team(
            "a",
            vec![("sam", vec![]), ("maya", vec![dev(2, "maya-mbp")])],
        )]);
        let new = admission(vec![team(
            "a",
            vec![("sam", vec![]), ("lee", vec![dev(2, "maya-mbp")])],
        )]);
        let change = diff(&old, &new);
        assert_eq!(change.removed, vec![key(2)]);
        assert_eq!(change.added, vec![key(2)]);
    }

    #[test]
    fn network_turns_on_and_off() {
        let alone = admission(vec![]);
        let with_maya = admission(vec![team(
            "a",
            vec![("sam", vec![]), ("maya", vec![dev(2, "maya-mbp")])],
        )]);
        assert_eq!(diff(&alone, &with_maya).network_change(), Some(true));
        assert_eq!(diff(&with_maya, &alone).network_change(), Some(false));
        let renamed = admission(vec![team(
            "a",
            vec![("sam", vec![]), ("maya", vec![dev(2, "maya-air")])],
        )]);
        let change = diff(&with_maya, &renamed);
        assert_eq!(change.updated, vec![key(2)]);
        assert!(change.added.is_empty() && change.removed.is_empty());
        assert!(
            with_maya
                .get(&key(2))
                .is_some_and(|d| !d.rooms.contains(PRIVATE_ROOM_ID))
        );
    }
}
