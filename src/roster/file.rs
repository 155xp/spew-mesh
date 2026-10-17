//! The roster file (4.2): people, their devices and rooms, for self-hosting without spew, LAN-only teams and
//! tests.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{
    DEVICE_NAME_RULE, DeviceEntry, EndpointId, MAX_DISPLAY, Me, Member, NAME_RULE, RelayUrl, Room,
    RoomKind, Roster, RosterError, is_display_text, is_valid_device_name, is_valid_person_name,
    is_valid_room_id, parse_endpoint_id, private_room,
};

/// How often the daemon checks the file's modification time (4.2).
pub const POLL_SECONDS: u64 = 5;

// A roster file this big is a mistake, not a team.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    person: Vec<RawPerson>,
    #[serde(default)]
    room: Vec<RawRoom>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPerson {
    name: String,
    display: Option<String>,
    #[serde(default)]
    devices: Vec<RawDevice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDevice {
    id: String,
    name: String,
    relay: Option<String>,
    #[serde(default)]
    addrs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoom {
    name: String,
    people: Vec<String>,
}

/// A parsed and validated roster file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterFile {
    version: String,
    people: BTreeMap<String, Member>,
    rooms: Vec<(String, Vec<String>)>,
}

/// Why the file roster can't be used. With any of these the daemon runs with the local provider and `doctor`
/// shows the message.
#[derive(Debug, thiserror::Error)]
pub enum FileRosterError {
    /// The file can't be read.
    #[error("can't read the roster file {path}: {source}")]
    Read {
        /// The roster file.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
    /// The file doesn't parse or breaks a rule.
    #[error("roster file {path}: {message}")]
    Invalid {
        /// The roster file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
    /// The file is fine but doesn't list this device under the settings username.
    #[error("{0}")]
    NotListed(String),
}

/// Reads and validates the roster file.
pub fn load(path: &Path) -> Result<RosterFile, FileRosterError> {
    let read_err = |source| FileRosterError::Read {
        path: path.to_path_buf(),
        source,
    };
    let file = std::fs::File::open(path).map_err(read_err)?;
    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(read_err)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(FileRosterError::Invalid {
            path: path.to_path_buf(),
            message: format!("larger than {MAX_FILE_BYTES} bytes"),
        });
    }
    parse(&text).map_err(|e| FileRosterError::Invalid {
        path: path.to_path_buf(),
        message: e.0,
    })
}

/// Parses and validates roster file text.
pub fn parse(text: &str) -> Result<RosterFile, RosterError> {
    let raw: RawFile = toml::from_str(text).map_err(|e| RosterError(e.to_string()))?;
    let mut people = BTreeMap::new();
    let mut seen_ids: BTreeMap<EndpointId, String> = BTreeMap::new();
    for person in raw.person {
        if !is_valid_person_name(&person.name) {
            return Err(RosterError(format!(
                "person name {:?} must be {NAME_RULE}",
                person.name
            )));
        }
        if people.contains_key(&person.name) {
            return Err(RosterError(format!(
                "person {:?} listed twice",
                person.name
            )));
        }
        let display = match person.display {
            Some(d) if !is_display_text(&d, MAX_DISPLAY) => {
                return Err(RosterError(format!(
                    "display name of {:?} must be one printable line of at most {MAX_DISPLAY} characters",
                    person.name
                )));
            }
            Some(d) => d,
            None => person.name.clone(),
        };
        let mut devices = Vec::new();
        let mut names = BTreeSet::new();
        for device in person.devices {
            let entry = parse_device(&person.name, device)?;
            if let Some(owner) = seen_ids.insert(entry.endpoint_id, person.name.clone()) {
                return Err(RosterError(format!(
                    "device {} is listed for both {owner:?} and {:?}",
                    entry.endpoint_id.fmt_short(),
                    person.name
                )));
            }
            if !names.insert(entry.name.clone()) {
                return Err(RosterError(format!(
                    "{:?} has two devices named {:?}",
                    person.name, entry.name
                )));
            }
            devices.push(entry);
        }
        people.insert(
            person.name.clone(),
            Member {
                username: person.name,
                display,
                devices,
            },
        );
    }
    let mut rooms: Vec<(String, Vec<String>)> = Vec::new();
    for room in raw.room {
        if !is_valid_room_id(&room.name) {
            return Err(RosterError(format!(
                "room name {:?} must be {NAME_RULE}",
                room.name
            )));
        }
        if rooms.iter().any(|(n, _)| *n == room.name) {
            return Err(RosterError(format!("room {:?} listed twice", room.name)));
        }
        if room.people.is_empty() {
            return Err(RosterError(format!("room {:?} has no people", room.name)));
        }
        let mut in_room = BTreeSet::new();
        for p in &room.people {
            if !people.contains_key(p) {
                return Err(RosterError(format!(
                    "room {:?} names {p:?}, who isn't a [[person]]",
                    room.name
                )));
            }
            if !in_room.insert(p.as_str()) {
                return Err(RosterError(format!(
                    "room {:?} lists {p:?} twice",
                    room.name
                )));
            }
        }
        rooms.push((room.name, room.people));
    }
    let digest = Sha256::digest(text.as_bytes());
    let version = digest[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(RosterFile {
        version: format!("file-{version}"),
        people,
        rooms,
    })
}

fn parse_device(person: &str, device: RawDevice) -> Result<DeviceEntry, RosterError> {
    let endpoint_id = parse_endpoint_id(&device.id)
        .map_err(|e| RosterError(format!("a device of {person:?}: {e}")))?;
    if !is_valid_device_name(&device.name) {
        return Err(RosterError(format!(
            "device name {:?} of {person:?} must be {DEVICE_NAME_RULE}",
            device.name
        )));
    }
    let relay = match device.relay {
        None => None,
        Some(r) => {
            let url: RelayUrl = r
                .parse()
                .map_err(|e| RosterError(format!("relay {r:?} of {:?}: {e}", device.name)))?;
            if !matches!(url.scheme(), "https" | "http") {
                return Err(RosterError(format!(
                    "relay {r:?} of {:?} isn't an http(s) URL",
                    device.name
                )));
            }
            Some(url)
        }
    };
    let addrs = device
        .addrs
        .iter()
        .map(|a| {
            a.parse::<SocketAddr>().map_err(|_| {
                RosterError(format!(
                    "address {a:?} of {:?} isn't an IP address with a port",
                    device.name
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DeviceEntry {
        endpoint_id,
        name: device.name,
        relay,
        addrs,
    })
}

impl RosterFile {
    /// The file's version: a hash of its text.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The person with this name.
    pub fn person(&self, name: &str) -> Option<&Member> {
        self.people.get(name)
    }

    /// Builds the roster for this device. `username` (settings `username`) must name a person in the file whose
    /// devices include `this_device`; the private room holds that person's devices and every `[[room]]` that
    /// lists them becomes a `team` room.
    pub fn roster_for(
        &self,
        username: Option<&str>,
        this_device: &EndpointId,
        now_ms: u64,
    ) -> Result<(Me, Roster), FileRosterError> {
        let username = username.filter(|u| !u.is_empty()).ok_or_else(|| {
            FileRosterError::NotListed(
                "settings has no username; the roster file needs one that names a [[person]]"
                    .into(),
            )
        })?;
        let person = self.people.get(username).ok_or_else(|| {
            FileRosterError::NotListed(format!(
                "settings username {username:?} isn't a [[person]] in the roster file"
            ))
        })?;
        if !person.devices.iter().any(|d| d.endpoint_id == *this_device) {
            return Err(FileRosterError::NotListed(format!(
                "this device ({}) isn't among {username:?}'s devices in the roster file",
                this_device.fmt_short()
            )));
        }
        let me = Me {
            username: person.username.clone(),
            display: person.display.clone(),
        };
        let mut rooms = vec![private_room(&me, person.devices.clone())];
        for (name, people) in &self.rooms {
            if !people.iter().any(|p| p == username) {
                continue;
            }
            rooms.push(Room {
                id: name.clone(),
                name: name.clone(),
                kind: RoomKind::Team,
                members: people
                    .iter()
                    .filter_map(|p| self.people.get(p).cloned())
                    .collect(),
            });
        }
        let mut roster = Roster {
            version: self.version.clone(),
            fetched_at: now_ms,
            rooms,
        };
        roster
            .validate(&me)
            .map_err(|e| FileRosterError::NotListed(format!("roster file: {e}")))?;
        Ok((me, roster))
    }
}

/// Notices when the roster file's modification time or size changes, so the daemon re-reads it (4.2).
#[derive(Debug, Clone)]
pub struct FileWatch {
    path: PathBuf,
    stamp: Option<Option<(Option<SystemTime>, u64)>>,
}

impl FileWatch {
    /// Watches `path`. The first [`FileWatch::changed`] call reports a change.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            stamp: None,
        }
    }

    /// The watched file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file changed (appeared, vanished, or got a new modification time or size) since the last
    /// call.
    pub fn changed(&mut self) -> bool {
        let now = std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.modified().ok(), m.len()));
        let changed = self.stamp != Some(now);
        self.stamp = Some(now);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    fn sample() -> String {
        format!(
            r#"
[[person]]
name = "maya"
display = "Maya"
devices = [ {{ id = "{maya}", name = "maya-mbp" }} ]

[[person]]
name = "sam"
devices = [
  {{ id = "{sam1}", name = "sam-mbp" }},
  {{ id = "{sam2}", name = "sam-linux", relay = "https://relay.example.com", addrs = ["192.168.1.20:7461"] }},
]

[[person]]
name = "lee"
devices = [ {{ id = "{lee}", name = "lee-mac" }} ]

[[room]]
name = "team"
people = ["sam", "maya"]

[[room]]
name = "other"
people = ["maya", "lee"]
"#,
            maya = key(2),
            sam1 = key(1),
            sam2 = key(3),
            lee = key(4),
        )
    }

    #[test]
    fn parses_and_builds_my_roster() {
        let file = parse(&sample()).unwrap();
        let (me, roster) = file.roster_for(Some("sam"), &key(1), 9).unwrap();
        assert_eq!(me.username, "sam");
        assert_eq!(me.display, "sam");
        assert_eq!(roster.fetched_at, 9);
        assert!(roster.version.starts_with("file-"));
        let ids: Vec<_> = roster.rooms.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec![super::super::PRIVATE_ROOM_ID, "team"]);
        let private = &roster.rooms[0].members[0];
        assert_eq!(private.devices.len(), 2);
        assert_eq!(
            private.devices[1].addrs,
            vec!["192.168.1.20:7461".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(roster.rooms[1].members[1].display, "Maya");
    }

    #[test]
    fn not_listed_reasons() {
        let file = parse(&sample()).unwrap();
        let reason = |r: Result<(Me, Roster), FileRosterError>| match r {
            Err(FileRosterError::NotListed(m)) => m,
            other => panic!("expected NotListed, got {other:?}"),
        };
        assert!(reason(file.roster_for(None, &key(1), 0)).contains("no username"));
        assert!(reason(file.roster_for(Some("kim"), &key(1), 0)).contains("isn't a [[person]]"));
        assert!(reason(file.roster_for(Some("sam"), &key(2), 0)).contains("isn't among"));
    }

    #[test]
    fn rejects_bad_files() {
        let k1 = key(1);
        let k2 = key(2);
        let bad = [
            (
                "unknown key",
                "[[person]]\nname = \"sam\"\nemail = \"x\"\n".to_string(),
            ),
            ("bad toml", "[[person]\n".to_string()),
            (
                "duplicate person",
                "[[person]]\nname = \"sam\"\n[[person]]\nname = \"sam\"\n".to_string(),
            ),
            (
                "same key twice",
                format!(
                    "[[person]]\nname = \"sam\"\ndevices = [{{ id = \"{k1}\", name = \"a\" }}]\n\
                     [[person]]\nname = \"maya\"\ndevices = [{{ id = \"{k1}\", name = \"b\" }}]\n"
                ),
            ),
            (
                "same device name twice",
                format!(
                    "[[person]]\nname = \"sam\"\ndevices = [{{ id = \"{k1}\", name = \"a\" }}, {{ id = \"{k2}\", name = \"a\" }}]\n"
                ),
            ),
            (
                "bad key",
                "[[person]]\nname = \"sam\"\ndevices = [{ id = \"abc\", name = \"a\" }]\n"
                    .to_string(),
            ),
            (
                "bad address",
                format!(
                    "[[person]]\nname = \"sam\"\ndevices = [{{ id = \"{k1}\", name = \"a\", addrs = [\"host:1\"] }}]\n"
                ),
            ),
            (
                "bad relay",
                format!(
                    "[[person]]\nname = \"sam\"\ndevices = [{{ id = \"{k1}\", name = \"a\", relay = \"ftp://x.example.com\" }}]\n"
                ),
            ),
            (
                "unknown person in room",
                "[[person]]\nname = \"sam\"\n[[room]]\nname = \"t\"\npeople = [\"sam\", \"kim\"]\n"
                    .to_string(),
            ),
            (
                "person twice in room",
                "[[person]]\nname = \"sam\"\n[[room]]\nname = \"t\"\npeople = [\"sam\", \"sam\"]\n"
                    .to_string(),
            ),
            (
                "empty room",
                "[[person]]\nname = \"sam\"\n[[room]]\nname = \"t\"\npeople = []\n".to_string(),
            ),
            (
                "room name",
                "[[person]]\nname = \"sam\"\n[[room]]\nname = \"a/b\"\npeople = [\"sam\"]\n"
                    .to_string(),
            ),
            ("person named me", "[[person]]\nname = \"me\"\n".to_string()),
            (
                "uppercase person",
                "[[person]]\nname = \"Sam\"\n".to_string(),
            ),
            (
                "room named me",
                "[[person]]\nname = \"sam\"\n[[room]]\nname = \"me\"\npeople = [\"sam\"]\n"
                    .to_string(),
            ),
            (
                "uppercase device",
                format!(
                    "[[person]]\nname = \"sam\"\ndevices = [{{ id = \"{k1}\", name = \"Sam-MBP\" }}]\n"
                ),
            ),
            (
                "person name",
                "[[person]]\nname = \"sam@mbp\"\n".to_string(),
            ),
            (
                "display with newline",
                "[[person]]\nname = \"sam\"\ndisplay = \"a\\nb\"\n".to_string(),
            ),
        ];
        for (what, text) in bad {
            assert!(parse(&text).is_err(), "{what} should be refused");
        }
    }

    #[test]
    fn load_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roster.toml");
        assert!(matches!(load(&path), Err(FileRosterError::Read { .. })));
        std::fs::write(&path, "nonsense = [").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("roster.toml"), "{err}");
        std::fs::write(&path, sample()).unwrap();
        assert!(load(&path).is_ok());
    }

    #[test]
    fn watch_sees_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roster.toml");
        let mut watch = FileWatch::new(&path);
        assert!(watch.changed(), "first call");
        assert!(!watch.changed());
        std::fs::write(&path, "a").unwrap();
        assert!(watch.changed(), "appeared");
        assert!(!watch.changed());
        std::fs::write(&path, "abc").unwrap();
        assert!(watch.changed(), "size changed");
        std::fs::remove_file(&path).unwrap();
        assert!(watch.changed(), "vanished");
    }
}
