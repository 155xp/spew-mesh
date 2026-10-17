//! spew card capture (8.5): which cards a session holds and what it is doing, from its spew tool calls.
//!
//! Hooks only pass facts; the daemon calls [`capture`] with them, so the rules live here, in one place.

use serde::{Deserialize, Serialize};

use super::hook::Facts;

/// A card not refreshed for this long is dropped, since spew lets a silent claim lapse (8.5).
pub const CARD_TTL_MS: u64 = 60 * 60 * 1000;

/// Activity text is cut to this many characters (8.5).
pub const ACTIVITY_MAX_CHARS: usize = 140;

/// Longest card ref accepted (5.5).
pub const CARD_MAX_LEN: usize = 40;

/// A card a session holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldCard {
    /// `<key>#<n>`, such as `spew#42`.
    pub card: String,
    /// The card's project key, used to filter presence per room (5.6).
    pub project: String,
    /// When a spew call last named it, in ms since the epoch.
    pub refreshed_at: u64,
}

impl HeldCard {
    fn new(card: String, now_ms: u64) -> HeldCard {
        let project = card.split('#').next().unwrap_or_default().to_string();
        HeldCard {
            card,
            project,
            refreshed_at: now_ms,
        }
    }
}

/// What a session says it is doing, from spew's `status` call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// One line, at most [`ACTIVITY_MAX_CHARS`] characters.
    pub text: String,
    /// The `project` the status call named, if any (5.6 filters on it).
    pub project: Option<String>,
}

/// How the session's activity changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityUpdate {
    Keep,
    Set(Activity),
    Clear,
}

/// The session's cards and activity after a tool event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub cards: Vec<HeldCard>,
    pub activity: ActivityUpdate,
}

/// Applies one tool event's facts to the cards a session holds, at time `now_ms` (8.5).
pub fn capture(facts: &Facts, held: &[HeldCard], now_ms: u64) -> Capture {
    let mut cards = drop_stale(held, now_ms);
    let mut activity = ActivityUpdate::Keep;
    let Some(tool) = facts.spew_tool.as_deref() else {
        return Capture { cards, activity };
    };
    let args = facts.spew_args.clone().unwrap_or_default();
    let project = args
        .project
        .as_deref()
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty());
    let result_card = facts
        .spew_result
        .as_deref()
        .and_then(|r| resolve_ref(r, None));
    let target = args
        .target
        .as_deref()
        .and_then(|t| resolve_ref(t, project.as_deref()));
    let issue = args
        .issue
        .as_deref()
        .and_then(|t| resolve_ref(t, project.as_deref()));

    // Any call naming a held card refreshes it.
    let mut named: Vec<String> = [target.clone(), issue.clone(), result_card.clone()]
        .into_iter()
        .flatten()
        .collect();
    if let Some(text) = args.text.as_deref() {
        named.extend(find_cards(text));
    }
    for c in cards.iter_mut() {
        if named.contains(&c.card) {
            c.refreshed_at = now_ms;
        }
    }

    match tool {
        "claim" => {
            let releasing = args.action.as_deref() == Some("release");
            let is_next = args.target.as_deref().map(str::trim) == Some("next");
            let card = if is_next { result_card } else { target };
            if let Some(card) = card {
                if releasing {
                    remove(&mut cards, &card);
                } else {
                    add(&mut cards, card, now_ms);
                }
            }
        }
        "issue_update" => {
            if matches!(args.status.as_deref(), Some("review") | Some("done"))
                && let Some(card) = issue
            {
                remove(&mut cards, &card);
            }
        }
        "handoff" => {
            // `issue` defaults to the issue the agent holds (spew's handoff tool); with no argument and one card
            // held, that is the one, else the card spew's answer names when the session holds it.
            let card = issue.or_else(|| match cards.as_slice() {
                [only] => Some(only.card.clone()),
                _ => result_card.filter(|c| cards.iter().any(|h| &h.card == c)),
            });
            if let Some(card) = card {
                remove(&mut cards, &card);
            }
        }
        "status" => {
            if args.done == Some(true) {
                cards.clear();
                activity = ActivityUpdate::Clear;
            } else if let Some(text) = args.text.as_deref() {
                let text = one_line(text, ACTIVITY_MAX_CHARS);
                activity = if text.is_empty() {
                    ActivityUpdate::Clear
                } else {
                    ActivityUpdate::Set(Activity { text, project })
                };
            }
        }
        _ => {}
    }
    Capture { cards, activity }
}

/// Drops cards not refreshed within [`CARD_TTL_MS`] of `now_ms`.
pub fn drop_stale(held: &[HeldCard], now_ms: u64) -> Vec<HeldCard> {
    held.iter()
        .filter(|c| now_ms.saturating_sub(c.refreshed_at) <= CARD_TTL_MS)
        .cloned()
        .collect()
}

fn add(cards: &mut Vec<HeldCard>, card: String, now_ms: u64) {
    match cards.iter_mut().find(|c| c.card == card) {
        Some(c) => c.refreshed_at = now_ms,
        None => cards.push(HeldCard::new(card, now_ms)),
    }
}

fn remove(cards: &mut Vec<HeldCard>, card: &str) {
    cards.retain(|c| c.card != card);
}

/// Normalizes an issue ref from a spew call: `spew#42`, or `#42` with the call's `project`. `None` for anything
/// that isn't a card (a thread ref, `next`, junk).
pub fn resolve_ref(r: &str, project: Option<&str>) -> Option<String> {
    let r = r.trim().to_ascii_lowercase();
    let full = match (r.strip_prefix('#'), project) {
        (Some(n), Some(p)) => format!("{p}#{n}"),
        (Some(_), None) => return None,
        (None, _) => r,
    };
    is_card(&full).then_some(full)
}

/// Whether `s` is a card ref: `[a-z0-9-]+#[0-9]+`, at most [`CARD_MAX_LEN`] characters (7.7).
pub fn is_card(s: &str) -> bool {
    let Some((key, num)) = s.split_once('#') else {
        return false;
    };
    s.len() <= CARD_MAX_LEN
        && !key.is_empty()
        && !num.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && num.bytes().all(|b| b.is_ascii_digit())
}

/// Every card ref in `text`, in order, without repeats.
pub fn find_cards(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let key_byte = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-';
    let mut out: Vec<String> = Vec::new();
    for (i, _) in text.match_indices('#') {
        let mut start = i;
        while start > 0 && key_byte(b[start - 1]) {
            start -= 1;
        }
        while start < i && b[start] == b'-' {
            start += 1;
        }
        let mut end = i + 1;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        if start == i || end == i + 1 {
            continue;
        }
        let card = &text[start..end];
        if is_card(card) && !out.iter().any(|c| c == card) {
            out.push(card.to_string());
        }
    }
    out
}

/// The first card ref in `text`.
pub fn first_card(text: &str) -> Option<String> {
    find_cards(text).into_iter().next()
}

/// One printable line of at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    words.join(" ").chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::hook::SpewArgs;

    const T0: u64 = 1_790_000_000_000;

    fn facts(tool: &str, args: SpewArgs, result: Option<&str>) -> Facts {
        Facts {
            spew_tool: Some(tool.to_string()),
            spew_args: Some(args),
            spew_result: result.map(str::to_string),
            ..Facts::default()
        }
    }

    fn held(cards: &[&str], at: u64) -> Vec<HeldCard> {
        cards
            .iter()
            .map(|c| HeldCard::new(c.to_string(), at))
            .collect()
    }

    fn names(c: &Capture) -> Vec<&str> {
        c.cards.iter().map(|c| c.card.as_str()).collect()
    }

    #[test]
    fn finds_card_refs() {
        assert_eq!(
            find_cards("claimed spew#42, see mesh-kit#7 and spew#42"),
            vec!["spew#42", "mesh-kit#7"]
        );
        assert_eq!(
            first_card("{\"ref\":\"mesh#8\",\"x\":\"#9\"}"),
            Some("mesh#8".to_string())
        );
        assert_eq!(find_cards("ABC#1 -x#2 a#b #3"), vec!["x#2"]);
        assert!(
            !is_card("Spew#1") && !is_card("spew#") && !is_card(&format!("{}#1", "a".repeat(40)))
        );
        assert_eq!(resolve_ref("#5", Some("mesh")), Some("mesh#5".into()));
        assert_eq!(resolve_ref(" MESH#5 ", None), Some("mesh#5".into()));
        assert_eq!(resolve_ref("spew/t7", None), None);
        assert_eq!(resolve_ref("next", Some("spew")), None);
    }

    #[test]
    fn claim_adds_and_release_removes() {
        let c = capture(
            &facts(
                "claim",
                SpewArgs {
                    target: Some("spew#42".into()),
                    ..Default::default()
                },
                None,
            ),
            &[],
            T0,
        );
        assert_eq!(names(&c), vec!["spew#42"]);
        assert_eq!(c.cards[0].project, "spew");
        assert_eq!(c.activity, ActivityUpdate::Keep);

        let next = SpewArgs {
            target: Some("next".into()),
            ..Default::default()
        };
        let c = capture(&facts("claim", next, Some("mesh#3")), &c.cards, T0 + 1);
        assert_eq!(names(&c), vec!["spew#42", "mesh#3"]);

        let rel = SpewArgs {
            target: Some("spew#42".into()),
            action: Some("release".into()),
            ..Default::default()
        };
        let c = capture(&facts("claim", rel, Some("spew#42")), &c.cards, T0 + 2);
        assert_eq!(names(&c), vec!["mesh#3"]);

        let short = SpewArgs {
            target: Some("#9".into()),
            project: Some("mesh".into()),
            ..Default::default()
        };
        let c = capture(&facts("claim", short, None), &c.cards, T0 + 3);
        assert_eq!(names(&c), vec!["mesh#3", "mesh#9"]);
    }

    #[test]
    fn review_done_and_handoff_remove_the_card() {
        let start = held(&["spew#1", "spew#2", "mesh#3"], T0);
        let review = SpewArgs {
            issue: Some("spew#1".into()),
            status: Some("review".into()),
            ..Default::default()
        };
        let c = capture(&facts("issue_update", review, None), &start, T0);
        assert_eq!(names(&c), vec!["spew#2", "mesh#3"]);
        let doing = SpewArgs {
            issue: Some("spew#2".into()),
            status: Some("doing".into()),
            ..Default::default()
        };
        let c = capture(&facts("issue_update", doing, None), &c.cards, T0);
        assert_eq!(names(&c), vec!["spew#2", "mesh#3"]);
        let hand = SpewArgs {
            issue: Some("mesh#3".into()),
            ..Default::default()
        };
        let c = capture(&facts("handoff", hand, None), &c.cards, T0);
        assert_eq!(names(&c), vec!["spew#2"]);
        let c = capture(&facts("handoff", SpewArgs::default(), None), &c.cards, T0);
        assert!(
            c.cards.is_empty(),
            "with no issue, the one held card is handed off"
        );
    }

    #[test]
    fn status_sets_and_clears_activity() {
        let text = format!(
            "Writing\nthe settings loader for spew#1 {}",
            "x".repeat(200)
        );
        let args = SpewArgs {
            text: Some(text),
            project: Some("spew".into()),
            ..Default::default()
        };
        let c = capture(&facts("status", args, None), &held(&["spew#1"], T0), T0 + 5);
        let ActivityUpdate::Set(a) = &c.activity else {
            panic!("{:?}", c.activity)
        };
        assert!(
            a.text
                .starts_with("Writing the settings loader for spew#1 xxx")
        );
        assert_eq!(a.text.chars().count(), ACTIVITY_MAX_CHARS);
        assert_eq!(a.project.as_deref(), Some("spew"));
        assert_eq!(
            c.cards[0].refreshed_at,
            T0 + 5,
            "naming a held card in the text refreshes it"
        );

        let none = SpewArgs {
            text: Some("Reading".into()),
            ..Default::default()
        };
        let c = capture(&facts("status", none, None), &[], T0);
        assert_eq!(
            c.activity,
            ActivityUpdate::Set(Activity {
                text: "Reading".into(),
                project: None
            })
        );

        let done = SpewArgs {
            done: Some(true),
            ..Default::default()
        };
        let c = capture(
            &facts("status", done, None),
            &held(&["spew#1", "mesh#2"], T0),
            T0,
        );
        assert!(c.cards.is_empty());
        assert_eq!(c.activity, ActivityUpdate::Clear);
    }

    #[test]
    fn cards_refresh_when_named_and_lapse_after_an_hour() {
        let start = held(&["spew#1", "spew#2"], T0);
        let read = SpewArgs {
            issue: Some("spew#1".into()),
            ..Default::default()
        };
        let later = T0 + CARD_TTL_MS - 1000;
        let c = capture(&facts("issue_read", read, None), &start, later);
        assert_eq!(c.cards[0].refreshed_at, later);
        assert_eq!(c.cards[1].refreshed_at, T0);
        let c2 = capture(&Facts::default(), &c.cards, T0 + CARD_TTL_MS + 1);
        assert_eq!(names(&c2), vec!["spew#1"]);
        let c3 = capture(
            &facts("search", SpewArgs::default(), Some("spew#1")),
            &c2.cards,
            later + 10,
        );
        assert_eq!(
            c3.cards[0].refreshed_at,
            later + 10,
            "a card in the result refreshes too"
        );
    }
}
