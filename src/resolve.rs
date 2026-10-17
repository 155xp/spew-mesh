//! Address resolution (2.2): which session an address means, from the asker's point of view.
//!
//! Parsing lives in [`crate::address`]; this module picks among the sessions the asker can see, which the daemon
//! hands over as directory [`Entry`] rows (local sessions and peers' presence alike).

use crate::address::{Address, PersonRef, is_client, is_person};
use crate::control::protocol::RpcError;
use crate::sessions::directory::Entry;
use crate::wire::{ActivityState, SessionKind};

/// Who is asking.
#[derive(Debug, Clone, Copy)]
pub struct Asker<'a> {
    /// The asking person.
    pub person: &'a str,
    /// The asking session on this device; never a candidate.
    pub session: Option<&'a str>,
    /// The asking session's repo, for the "busy in your repo" rule.
    pub repo: Option<&'a str>,
}

/// Why an address names no single session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// Nothing matches.
    #[error("{0}")]
    NotFound(String),
    /// Several sessions match and the rules don't pick one.
    #[error("{0}")]
    Ambiguous(String),
    /// A card nobody advertises (2.2).
    #[error("nobody holds {card} right now{}", list_suffix(.sessions, &format!("sessions in project {}", .project)))]
    NobodyHolds {
        /// The card.
        card: String,
        /// Its project key.
        project: String,
        /// Addresses of the sessions in that project.
        sessions: Vec<String>,
    },
}

fn list_suffix(items: &[String], label: &str) -> String {
    if items.is_empty() {
        String::new()
    } else {
        format!("; {label}: {}", items.join(", "))
    }
}

impl ResolveError {
    /// The control error for it.
    pub fn rpc(&self) -> RpcError {
        match self {
            ResolveError::Ambiguous(_) => RpcError::conflict(self.to_string()),
            _ => RpcError::not_found(self.to_string()),
        }
    }

    /// Whether looking again among offline devices could help.
    pub fn is_not_found(&self) -> bool {
        !matches!(self, ResolveError::Ambiguous(_))
    }
}

fn addresses(entries: &[&Entry]) -> String {
    entries
        .iter()
        .map(|e| e.address())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Among several matches: the only busy live session in the asker's repo, else the only responder, else an error
/// listing them all (2.2).
fn pick<'e>(
    what: &str,
    matches: Vec<&'e Entry>,
    asker: &Asker<'_>,
) -> Result<&'e Entry, ResolveError> {
    match matches.as_slice() {
        [] => {
            return Err(ResolveError::NotFound(format!(
                "no session you can see matches `{what}`; the who tool lists sessions"
            )));
        }
        [only] => return Ok(only),
        _ => {}
    }
    let busy_here: Vec<&Entry> = matches
        .iter()
        .copied()
        .filter(|e| {
            e.session.kind == SessionKind::Live
                && e.session.state == ActivityState::Busy
                && asker.repo.is_some()
                && e.session.repo.as_deref() == asker.repo
        })
        .collect();
    if let [only] = busy_here.as_slice() {
        return Ok(only);
    }
    let responders: Vec<&Entry> = matches
        .iter()
        .copied()
        .filter(|e| e.session.kind == SessionKind::Responder)
        .collect();
    if let [only] = responders.as_slice() {
        return Ok(only);
    }
    Err(ResolveError::Ambiguous(format!(
        "`{what}` matches several sessions: {}. Use one of these full addresses",
        addresses(&matches)
    )))
}

fn same_set(a: &[&Entry], b: &[&Entry]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.iter().any(|y| std::ptr::eq(*x, *y)))
}

/// Resolves `address` among `entries` (2.2). Terminal sessions and the asking session are never candidates.
pub fn resolve<'e>(
    address: &Address,
    asker: &Asker<'_>,
    entries: &'e [Entry],
) -> Result<&'e Entry, ResolveError> {
    let cands: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.session.kind != SessionKind::Terminal)
        .filter(|e| !(e.this_device && Some(e.session.session.as_str()) == asker.session))
        .collect();
    let select = |f: &dyn Fn(&Entry) -> bool| -> Vec<&'e Entry> {
        cands.iter().copied().filter(|e| f(e)).collect()
    };
    let person_of = |p: &PersonRef| match p {
        PersonRef::Me => asker.person.to_string(),
        PersonRef::Named(name) => name.clone(),
    };
    let what = address.to_string();
    match address {
        Address::Bare(token) => {
            let mut name_reading =
                select(&|e| e.person == asker.person && e.session.name == *token);
            if name_reading.is_empty() && is_client(token) {
                name_reading = select(&|e| e.person == asker.person && e.session.client == *token);
            }
            let person_reading = if is_person(token) {
                select(&|e| e.person == *token)
            } else {
                Vec::new()
            };
            match (name_reading.is_empty(), person_reading.is_empty()) {
                (false, true) => pick(&what, name_reading, asker),
                (true, false) => pick(&what, person_reading, asker),
                (false, false) if same_set(&name_reading, &person_reading) => {
                    pick(&what, name_reading, asker)
                }
                (false, false) => Err(ResolveError::Ambiguous(format!(
                    "`{token}` could be your session ({}) or {token}'s ({}); use me/{token} or {token}/<name>",
                    addresses(&name_reading),
                    addresses(&person_reading)
                ))),
                (true, true) => Err(ResolveError::NotFound(format!(
                    "no session of yours is called `{token}` and nobody called {token} has a session you can see; \
                     the who tool lists sessions"
                ))),
            }
        }
        Address::Me => pick(&what, select(&|e| e.person == asker.person), asker),
        Address::Named { person, name } => {
            let person = person_of(person);
            let mut found = select(&|e| e.person == person && e.session.name == *name);
            if found.is_empty() {
                found = select(&|e| e.person == person && e.session.client == *name);
            }
            pick(&what, found, asker)
        }
        Address::Exact {
            person,
            device,
            name,
        } => {
            let person = person_of(person);
            select(&|e| e.person == person && e.device == *device && e.session.name == *name)
                .first()
                .copied()
                .ok_or_else(|| {
                    ResolveError::NotFound(format!(
                        "no session you can see is `{what}`; the who tool lists sessions"
                    ))
                })
        }
        Address::Card(card) => {
            let holders = select(&|e| e.session.cards.iter().any(|c| c == card.as_str()));
            if holders.is_empty() {
                let mut sessions: Vec<String> =
                    select(&|e| e.session.room.as_deref() == Some(card.key()))
                        .iter()
                        .map(|e| e.address())
                        .collect();
                sessions.sort();
                return Err(ResolveError::NobodyHolds {
                    card: card.to_string(),
                    project: card.key().to_string(),
                    sessions,
                });
            }
            let since = |e: &Entry| e.card_since.get(card.as_str()).copied().unwrap_or(0);
            let latest = holders.iter().map(|e| since(e)).max().unwrap_or(0);
            let newest: Vec<&Entry> = holders.into_iter().filter(|e| since(e) == latest).collect();
            pick(&what, newest, asker)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::wire::{PresenceSession, Reach};

    fn entry(person: &str, device: &str, name: &str, client: &str) -> Entry {
        Entry {
            person: person.into(),
            device: device.into(),
            device_id: format!("{device}-id"),
            this_device: device == "sam-mac",
            own_device: person == "sam",
            online: true,
            session: PresenceSession {
                session: format!("{device}-{name}"),
                name: name.into(),
                client: client.into(),
                kind: SessionKind::Live,
                repo: Some("acme/api".into()),
                branch: None,
                place: None,
                room: Some("spew".into()),
                cards: vec![],
                activity: None,
                state: ActivityState::Idle,
                since: 0,
                reach: Reach::Hooked,
                queue: 0,
            },
            card_since: HashMap::new(),
        }
    }

    fn sam(session: Option<&str>) -> Asker<'_> {
        Asker {
            person: "sam",
            session,
            repo: Some("acme/api"),
        }
    }

    fn run<'e>(
        addr: &str,
        asker: &Asker<'_>,
        entries: &'e [Entry],
    ) -> Result<&'e Entry, ResolveError> {
        resolve(&Address::parse(addr).unwrap(), asker, entries)
    }

    #[test]
    fn bare_names_clients_and_people() {
        let entries = vec![
            entry("sam", "sam-mac", "claude-api", "claude"),
            entry("sam", "sam-linux", "codex-web", "codex"),
            entry("maya", "maya-mbp", "claude-api", "claude"),
        ];
        let asker = sam(None);
        assert_eq!(
            run("claude-api", &asker, &entries).unwrap().device,
            "sam-mac"
        );
        assert_eq!(
            run("codex", &asker, &entries).unwrap().session.name,
            "codex-web"
        );
        assert_eq!(run("maya", &asker, &entries).unwrap().device, "maya-mbp");
        assert_eq!(
            run("maya/claude", &asker, &entries).unwrap().device,
            "maya-mbp"
        );
        assert_eq!(
            run("me/codex-web", &asker, &entries).unwrap().device,
            "sam-linux"
        );
        assert_eq!(
            run("sam@sam-linux/codex-web", &asker, &entries)
                .unwrap()
                .session
                .name,
            "codex-web"
        );
        assert!(matches!(
            run("nobody", &asker, &entries),
            Err(ResolveError::NotFound(_))
        ));
        assert!(matches!(
            run("maya@maya-mbp/other", &asker, &entries),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    fn both_readings_are_an_error() {
        let entries = vec![
            entry("sam", "sam-mac", "claude-api", "claude"),
            entry("claude", "claude-mbp", "codex-x", "codex"),
        ];
        let err = run("claude", &sam(None), &entries).unwrap_err();
        assert!(matches!(err, ResolveError::Ambiguous(_)));
        assert!(err.to_string().contains("me/claude"), "{err}");
        assert_eq!(
            run("me/claude", &sam(None), &entries).unwrap().device,
            "sam-mac"
        );
        assert_eq!(
            run("claude/codex-x", &sam(None), &entries).unwrap().device,
            "claude-mbp"
        );
    }

    #[test]
    fn several_matches_follow_the_ladder() {
        let mut busy = entry("maya", "maya-mbp", "claude-api", "claude");
        busy.session.state = ActivityState::Busy;
        let idle = entry("maya", "maya-mbp", "codex-api", "codex");
        let entries = vec![busy, idle];
        assert_eq!(
            run("maya", &sam(None), &entries).unwrap().session.name,
            "claude-api"
        );

        let mut other_repo = entries.clone();
        other_repo[0].session.repo = Some("acme/web".into());
        let err = run("maya", &sam(None), &other_repo).unwrap_err();
        assert!(
            err.to_string().contains("maya@maya-mbp/claude-api"),
            "{err}"
        );

        let mut with_responder = other_repo.clone();
        with_responder[1].session.kind = SessionKind::Responder;
        assert_eq!(
            run("maya", &sam(None), &with_responder)
                .unwrap()
                .session
                .name,
            "codex-api"
        );
    }

    #[test]
    fn the_asker_and_terminals_are_never_candidates() {
        let mut term = entry("sam", "sam-mac", "terminal", "terminal");
        term.session.kind = SessionKind::Terminal;
        let entries = vec![entry("sam", "sam-mac", "claude-api", "claude"), term];
        assert!(run("claude-api", &sam(Some("sam-mac-claude-api")), &entries).is_err());
        assert!(run("terminal", &sam(None), &entries).is_err());
        assert_eq!(
            run("me", &sam(None), &entries).unwrap().session.name,
            "claude-api"
        );
    }

    #[test]
    fn cards_go_to_the_latest_holder() {
        let mut a = entry("maya", "maya-mbp", "claude-api", "claude");
        a.session.cards = vec!["spew#42".into()];
        a.card_since.insert("spew#42".into(), 10);
        let mut b = entry("sam", "sam-linux", "codex-web", "codex");
        b.session.cards = vec!["spew#42".into()];
        b.card_since.insert("spew#42".into(), 20);
        let entries = vec![a, b, entry("maya", "maya-mbp", "codex-x", "codex")];
        assert_eq!(
            run("spew#42", &sam(None), &entries).unwrap().session.name,
            "codex-web"
        );
        let err = run("spew#7", &sam(None), &entries).unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("nobody holds spew#7 right now"), "{text}");
        assert!(text.contains("maya@maya-mbp/codex-x"), "{text}");
        assert_eq!(err.rpc().reason, crate::control::protocol::Reason::NotFound);
    }
}
