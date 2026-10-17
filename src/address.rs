//! Names, cards and addresses: validation and parsing (2, 2.1, 2.2, 5.5, 7.7).
//!
//! Resolving an address to sessions needs the session registry and the directory, so it lives with the daemon.

use std::fmt;

/// Longest session name (2.1, 5.5).
pub const MAX_NAME: usize = 48;
/// Longest client slug (2, 5.5).
pub const MAX_CLIENT: usize = 16;
/// Longest card ref (5.5).
pub const MAX_CARD: usize = 40;
/// Longest repo slug (5.5).
pub const MAX_REPO: usize = 100;
/// Longest person name. The spec gives no limit; names share the session-name limit.
pub const MAX_PERSON: usize = 48;
/// Longest device name. The spec gives no limit; names share the session-name limit.
pub const MAX_DEVICE: usize = 48;
/// Longest room id. The spec gives no limit; rooms share the session-name limit.
pub const MAX_ROOM: usize = 48;
/// The sender name reserved for the daemon's own notices (7.6).
pub const RESERVED_NAME: &str = "mesh";
/// The client kinds setup wires and serve runs (2, 9).
pub const CLIENT_KINDS: [&str; 3] = ["claude", "codex", "opencode"];

/// Lowercase ASCII letters, digits and single inner hyphens, 1 to `max` bytes.
pub fn is_slug(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

/// A session name (2.1).
pub fn is_session_name(s: &str) -> bool {
    is_slug(s, MAX_NAME)
}

/// A client slug (2).
pub fn is_client(s: &str) -> bool {
    is_slug(s, MAX_CLIENT)
}

/// A device name such as `maya-mbp` (2).
pub fn is_device_name(s: &str) -> bool {
    is_slug(s, MAX_DEVICE)
}

// Person names and rooms come from spew usernames, project keys and roster files. The spec names no character
// set, so they allow `.` and `_` besides the slug characters, and never `/`, `@`, `#`, `|` or `]`, which the
// address syntax and the message header need.
fn is_token(s: &str, max: usize) -> bool {
    let edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let bytes = s.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(&first), Some(&last)) => {
            s.len() <= max
                && edge_ok(first)
                && edge_ok(last)
                && bytes
                    .iter()
                    .all(|&b| edge_ok(b) || b == b'-' || b == b'_' || b == b'.')
        }
        _ => false,
    }
}

/// A person name such as `maya` (2).
pub fn is_person(s: &str) -> bool {
    is_token(s, MAX_PERSON)
}

/// A room id: a spew project key or a roster room name (2).
pub fn is_room(s: &str) -> bool {
    is_token(s, MAX_ROOM)
}

/// A card ref `<key>#<number>` with key `[a-z0-9-]+`, at most 40 bytes (7.7).
pub fn is_card(s: &str) -> bool {
    if s.len() > MAX_CARD {
        return false;
    }
    match s.split_once('#') {
        Some((key, number)) => {
            !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// A repo slug of `[A-Za-z0-9._/-]`, 1 to 100 bytes (7.7).
pub fn is_repo(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REPO
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

/// A ULID in its canonical 26-character uppercase form.
pub fn is_ulid(s: &str) -> bool {
    // Decoding alone accepts lowercase and overflowing first characters; the round trip rejects both.
    s.len() == ulid::ULID_LEN && ulid::Ulid::from_string(s).is_ok_and(|id| id.to_string() == s)
}

/// Slugs any text: lowercase ASCII letters and digits, runs of anything else become one hyphen, at most `max`
/// bytes, no hyphen at either end. May return an empty string.
pub fn slugify(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out.truncate(max);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The name a live session asks for before clashes: `<client>-<place>`, slugged, at most 48 bytes (2.1).
pub fn session_base_name(client: &str, place: Option<&str>) -> String {
    let joined = match place {
        Some(place) if !slugify(place, MAX_NAME).is_empty() => format!("{client}-{place}"),
        _ => client.to_string(),
    };
    let name = slugify(&joined, MAX_NAME);
    if name.is_empty() {
        "session".to_string()
    } else {
        name
    }
}

/// The first free name from `base`, then `base-b`, `base-c` and so on, keeping 48 bytes (2.1). The reserved name
/// `mesh` counts as taken. `None` only when a thousand names are taken.
pub fn allocate_name(base: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let taken = |name: &str| name == RESERVED_NAME || taken(name);
    if !taken(base) {
        return Some(base.to_string());
    }
    (1..1000u32).find_map(|n| {
        let suffix = clash_suffix(n);
        let room = MAX_NAME.saturating_sub(suffix.len() + 1);
        let stem = base
            .get(..base.len().min(room))
            .unwrap_or(base)
            .trim_end_matches('-');
        let candidate = format!("{stem}-{suffix}");
        (!taken(&candidate)).then_some(candidate)
    })
}

fn clash_suffix(n: u32) -> String {
    match u8::try_from(n) {
        Ok(n @ 1..=25) => char::from(b'a' + n).to_string(),
        // Past `-z` a number keeps names unique; 26 clashes on one device is not a real case.
        _ => (n + 1).to_string(),
    }
}

/// A card ref such as `spew#42`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Card(String);

impl Card {
    /// Parses a card ref, failing unless it fits 7.7.
    pub fn parse(s: &str) -> Result<Card, AddressError> {
        if is_card(s) {
            Ok(Card(s.to_string()))
        } else {
            Err(AddressError::Card(s.to_string()))
        }
    }

    /// The project key, `spew` in `spew#42`.
    pub fn key(&self) -> &str {
        self.0.split_once('#').map_or("", |(key, _)| key)
    }

    /// The issue number as written, `42` in `spew#42`.
    pub fn number(&self) -> &str {
        self.0.split_once('#').map_or("", |(_, number)| number)
    }

    /// The whole ref.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The person part of an address.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PersonRef {
    /// `me`: the asking person.
    Me,
    /// A person by name.
    Named(String),
}

impl fmt::Display for PersonRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PersonRef::Me => f.write_str("me"),
            PersonRef::Named(name) => f.write_str(name),
        }
    }
}

/// A parsed address (2.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Address {
    /// A bare token: one of your own session names, else your sessions of that client, else a person.
    Bare(String),
    /// `me` alone.
    Me,
    /// `person/name` or `me/name`, where name may also be a client kind.
    Named {
        /// Whose session.
        person: PersonRef,
        /// The session name or client kind.
        name: String,
    },
    /// `person@device/name`: exactly one session.
    Exact {
        /// Whose session.
        person: PersonRef,
        /// The device name.
        device: String,
        /// The session name.
        name: String,
    },
    /// `<key>#<n>`: whoever advertises that card.
    Card(Card),
}

impl Address {
    /// Parses an address. Surrounding space is ignored and ASCII letters are lowercased.
    pub fn parse(input: &str) -> Result<Address, AddressError> {
        let s = input.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Err(AddressError::Empty);
        }
        let bad = || AddressError::Syntax(s.clone());
        if s.contains('#') {
            return Card::parse(&s).map(Address::Card);
        }
        if let Some((person, rest)) = s.split_once('@') {
            let (device, name) = rest.split_once('/').ok_or_else(bad)?;
            if !is_device_name(device) || !is_session_name(name) {
                return Err(bad());
            }
            return Ok(Address::Exact {
                person: person_ref(person).ok_or_else(bad)?,
                device: device.to_string(),
                name: name.to_string(),
            });
        }
        if let Some((person, name)) = s.split_once('/') {
            if !is_session_name(name) {
                return Err(bad());
            }
            return Ok(Address::Named {
                person: person_ref(person).ok_or_else(bad)?,
                name: name.to_string(),
            });
        }
        if s == "me" {
            return Ok(Address::Me);
        }
        if is_session_name(&s) || is_person(&s) {
            Ok(Address::Bare(s))
        } else {
            Err(bad())
        }
    }
}

fn person_ref(s: &str) -> Option<PersonRef> {
    if s == "me" {
        Some(PersonRef::Me)
    } else if is_person(s) {
        Some(PersonRef::Named(s.to_string()))
    } else {
        None
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::Bare(token) => f.write_str(token),
            Address::Me => f.write_str("me"),
            Address::Named { person, name } => write!(f, "{person}/{name}"),
            Address::Exact {
                person,
                device,
                name,
            } => write!(f, "{person}@{device}/{name}"),
            Address::Card(card) => card.fmt(f),
        }
    }
}

impl std::str::FromStr for Address {
    type Err = AddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Address::parse(s)
    }
}

/// The full address of one session, `person@device/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionAddress {
    /// The session's person.
    pub person: String,
    /// The session's device name.
    pub device: String,
    /// The session name.
    pub name: String,
}

impl SessionAddress {
    /// The short form used in message headers, `person/name` (7.8).
    pub fn short(&self) -> String {
        format!("{}/{}", self.person, self.name)
    }
}

impl fmt::Display for SessionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}/{}", self.person, self.device, self.name)
    }
}

/// Why an address or card didn't parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    /// Nothing was given.
    #[error(
        "the address is empty: use a session name, a person, person/name, person@device/name or a card like spew#42"
    )]
    Empty,
    /// The text fits no address form.
    #[error(
        "`{0}` is not an address: use a session name, a person, person/name, person@device/name or a card like spew#42"
    )]
    Syntax(String),
    /// A `#` was present but the card ref is malformed.
    #[error("`{0}` is not a card ref: use <project key>#<number>, such as spew#42")]
    Card(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert!(is_slug("claude-spew-app", 48));
        assert!(is_slug("a", 1));
        assert!(!is_slug("", 48));
        assert!(!is_slug("-a", 48));
        assert!(!is_slug("a-", 48));
        assert!(!is_slug("a--b", 48));
        assert!(!is_slug("A", 48));
        assert!(!is_slug("a_b", 48));
        assert!(!is_slug("ab", 1));
        assert!(is_session_name(&"a".repeat(48)));
        assert!(!is_session_name(&"a".repeat(49)));
        assert!(is_client("gemini"));
        assert!(!is_client(&"c".repeat(17)));
    }

    #[test]
    fn people_and_rooms() {
        assert!(is_person("maya"));
        assert!(is_person("maya.k"));
        assert!(is_person("sam_2"));
        assert!(!is_person("maya/claude"));
        assert!(!is_person("maya@x"));
        assert!(!is_person(".maya"));
        assert!(!is_person("maya-"));
        assert!(!is_person("Maya"));
        assert!(!is_person(""));
        assert!(is_room("spew"));
        assert!(!is_room("a|b"));
        assert!(!is_room("a]b"));
    }

    #[test]
    fn cards() {
        assert!(is_card("spew#42"));
        assert!(is_card("my-app#1"));
        assert!(!is_card("spew#"));
        assert!(!is_card("#42"));
        assert!(!is_card("Spew#42"));
        assert!(!is_card("spew#4a"));
        assert!(!is_card("spew#42#1"));
        assert!(!is_card(&format!("{}#1", "a".repeat(39))));
        assert!(is_card(&format!("{}#1", "a".repeat(38))));
        let card = Card::parse("spew#42").unwrap();
        assert_eq!(card.key(), "spew");
        assert_eq!(card.number(), "42");
        assert_eq!(card.to_string(), "spew#42");
        assert!(Card::parse("nope").is_err());
    }

    #[test]
    fn repos() {
        assert!(is_repo("acme/api"));
        assert!(is_repo("Acme/api.rs_2"));
        assert!(!is_repo(""));
        assert!(!is_repo("acme api"));
        assert!(!is_repo("acme|api"));
        assert!(!is_repo(&"a".repeat(101)));
    }

    #[test]
    fn ulids() {
        let id = ulid::Ulid::new().to_string();
        assert!(is_ulid(&id));
        assert!(!is_ulid(&id.to_ascii_lowercase()));
        assert!(!is_ulid("01J9ZK"));
        // A first character above 7 overflows 128 bits.
        assert!(!is_ulid("81J9ZKQ4V4M0Q2W4V4M0Q2W4V4"));
        // Crockford aliases decode but aren't canonical.
        assert!(!is_ulid("0IJ9ZKQ4V4M0Q2W4V4M0Q2W4V4"));
    }

    #[test]
    fn slugify_text() {
        assert_eq!(slugify("Spew App", 48), "spew-app");
        assert_eq!(slugify("--a__b--", 48), "a-b");
        assert_eq!(slugify("日本語", 48), "");
        assert_eq!(slugify("abc-def", 4), "abc");
        assert_eq!(slugify("Sam O'Neil", 48), "sam-o-neil");
    }

    #[test]
    fn base_names() {
        assert_eq!(
            session_base_name("claude", Some("spew-app")),
            "claude-spew-app"
        );
        assert_eq!(session_base_name("codex", Some("API")), "codex-api");
        assert_eq!(session_base_name("claude", Some("日本")), "claude");
        assert_eq!(session_base_name("claude", None), "claude");
        let long = session_base_name("opencode", Some(&"x".repeat(80)));
        assert_eq!(long.len(), 48);
        assert!(is_session_name(&long));
    }

    #[test]
    fn name_clashes() {
        let taken = ["codex-api", "codex-api-b"];
        let name = allocate_name("codex-api", |n| taken.contains(&n)).unwrap();
        assert_eq!(name, "codex-api-c");
        assert_eq!(allocate_name("free", |_| false).unwrap(), "free");
        let base = "a".repeat(48);
        let name = allocate_name(&base, |n| n == base).unwrap();
        assert_eq!(name.len(), 48);
        assert!(name.ends_with("-b"));
        assert!(is_session_name(&name));
        let hyphen_edge = format!("{}-x", "a".repeat(45));
        let name = allocate_name(&hyphen_edge, |n| n == hyphen_edge).unwrap();
        assert!(is_session_name(&name), "{name}");
        let after_z =
            allocate_name("n", |n| n == "n" || (n.len() == 3 && n.starts_with("n-"))).unwrap();
        assert_eq!(after_z, "n-27");
        assert!(allocate_name("n", |_| true).is_none());
        assert_eq!(allocate_name("mesh", |_| false).unwrap(), "mesh-b");
    }

    #[test]
    fn parse_forms() {
        assert_eq!(
            Address::parse("claude-api").unwrap(),
            Address::Bare("claude-api".into())
        );
        assert_eq!(
            Address::parse("maya").unwrap(),
            Address::Bare("maya".into())
        );
        assert_eq!(
            Address::parse("maya.k").unwrap(),
            Address::Bare("maya.k".into())
        );
        assert_eq!(Address::parse("me").unwrap(), Address::Me);
        assert_eq!(
            Address::parse("me/codex").unwrap(),
            Address::Named {
                person: PersonRef::Me,
                name: "codex".into()
            }
        );
        assert_eq!(
            Address::parse("maya/claude-api").unwrap(),
            Address::Named {
                person: PersonRef::Named("maya".into()),
                name: "claude-api".into()
            }
        );
        assert_eq!(
            Address::parse("maya@maya-mbp/claude-api").unwrap(),
            Address::Exact {
                person: PersonRef::Named("maya".into()),
                device: "maya-mbp".into(),
                name: "claude-api".into()
            }
        );
        assert_eq!(
            Address::parse("me@sam-mac/terminal").unwrap(),
            Address::Exact {
                person: PersonRef::Me,
                device: "sam-mac".into(),
                name: "terminal".into()
            }
        );
        assert_eq!(
            Address::parse("spew#42").unwrap(),
            Address::Card(Card::parse("spew#42").unwrap())
        );
        assert_eq!(
            Address::parse("  Maya/Claude-API ").unwrap().to_string(),
            "maya/claude-api"
        );
    }

    #[test]
    fn parse_rejects() {
        assert_eq!(Address::parse("  "), Err(AddressError::Empty));
        for bad in [
            "maya/",
            "/claude",
            "maya/claude/x",
            "maya@/claude",
            "maya@mbp",
            "maya@mbp/",
            "@mbp/claude",
            "maya@mbp/claude/x",
            "maya@m_bp/claude",
            "maya/claude_api",
            "a b",
            "maya|x",
            "x]",
            "-x",
        ] {
            assert!(Address::parse(bad).is_err(), "{bad}");
        }
        assert!(matches!(
            Address::parse("spew#x"),
            Err(AddressError::Card(_))
        ));
        assert!(matches!(Address::parse("maya#4"), Ok(Address::Card(_))));
    }

    #[test]
    fn display_round_trips() {
        for text in [
            "claude-api",
            "me",
            "me/codex",
            "maya/claude-api",
            "maya@maya-mbp/claude-api",
            "spew#42",
        ] {
            let parsed: Address = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
        }
        let full = SessionAddress {
            person: "maya".into(),
            device: "maya-mbp".into(),
            name: "claude-api".into(),
        };
        assert_eq!(full.to_string(), "maya@maya-mbp/claude-api");
        assert_eq!(full.short(), "maya/claude-api");
        assert_eq!(
            Address::parse(&full.to_string()).unwrap(),
            Address::Exact {
                person: PersonRef::Named("maya".into()),
                device: "maya-mbp".into(),
                name: "claude-api".into()
            }
        );
    }
}
