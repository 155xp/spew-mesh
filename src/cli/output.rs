//! What the CLI prints (10): the `who` listing, daemon state and send results. Peer text is sanitized again
//! before it reaches the terminal (7.7).

use std::fmt::Write as _;

use crate::control::protocol::{Directory, Fetched, SendResult, SendStatus, SessionRow, Status};
use crate::daemon::status::NetworkDetails;
use crate::spew_api::DeviceInfo;
use crate::text::{sanitize_line, sanitize_text};
use crate::wire::{ActivityState, Reach, SessionKind};

/// How long ago `since` was, in one short unit: `40s`, `5m`, `3h`, `2d`.
pub fn age(now: u64, since: u64) -> String {
    let secs = now.saturating_sub(since) / 1000;
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn line(s: &str) -> String {
    sanitize_line(s, 200)
}

fn state_name(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Busy => "busy",
        ActivityState::Idle => "idle",
        ActivityState::Unknown => "unknown",
    }
}

fn reach_name(reach: Reach) -> &'static str {
    match reach {
        Reach::Hooked => "hooked",
        Reach::Inbox => "inbox",
        Reach::Serve => "serve",
    }
}

fn session_row(row: &SessionRow, now: u64) -> String {
    let mut parts = vec![line(&row.address), line(&row.client)];
    if row.kind == SessionKind::Responder {
        let mut kind = "responder".to_string();
        if row.queue > 0 {
            let _ = write!(kind, " ({} waiting)", row.queue);
        }
        parts.push(kind);
    }
    parts.push(format!("{} {}", state_name(row.state), age(now, row.since)));
    match (&row.repo, &row.branch, &row.place) {
        (Some(repo), Some(branch), _) => parts.push(format!("{}@{}", line(repo), line(branch))),
        (Some(repo), None, _) => parts.push(line(repo)),
        (None, _, Some(place)) => parts.push(line(place)),
        (None, _, None) => {}
    }
    if !row.cards.is_empty() {
        parts.push(
            row.cards
                .iter()
                .map(|c| line(c))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    if let Some(activity) = &row.activity {
        parts.push(format!("\"{}\"", line(activity)));
    }
    parts.push(reach_name(row.reach).to_string());
    if !row.online {
        parts.push("(offline)".into());
    }
    if row.this {
        parts.push("(this session)".into());
    }
    parts.join("  ")
}

/// The directory listing (6.3), grouped by room then person; `links` adds each device's path and round trip.
pub fn who(d: &Directory, now: u64, links: bool) -> String {
    let mut out = String::new();
    let sessions: usize = d
        .rooms
        .iter()
        .flat_map(|r| &r.people)
        .map(|p| p.sessions.len())
        .sum();
    if sessions == 0 {
        out.push_str("No agent sessions are online.\n");
    }
    for room in &d.rooms {
        let _ = writeln!(out, "{}", line(&room.name));
        for person in &room.people {
            let you = if person.own { " (you)" } else { "" };
            let _ = writeln!(out, "  {}{you}", line(&person.person));
            for row in &person.sessions {
                let _ = writeln!(out, "    {}", session_row(row, now));
            }
        }
    }
    if !d.devices.is_empty() {
        out.push_str("devices\n");
        for dev in &d.devices {
            let mut parts = vec![format!("{}@{}", line(&dev.person), line(&dev.name))];
            if dev.this {
                parts.push("this device".into());
            } else if dev.online {
                parts.push("online".into());
            } else {
                match dev.last_seen {
                    Some(t) => parts.push(format!("offline, last seen {} ago", age(now, t))),
                    None => parts.push("offline".into()),
                }
            }
            if links && !dev.this {
                if let Some(path) = &dev.path {
                    parts.push(line(path));
                }
                if let Some(rtt) = dev.rtt_ms {
                    parts.push(format!("{rtt} ms"));
                }
            }
            let _ = writeln!(out, "  {}", parts.join("  "));
        }
    }
    if d.unread > 0 {
        let _ = writeln!(out, "{}", waiting(d.unread));
    }
    out
}

/// The unread line for the terminal session.
pub fn waiting(unread: usize) -> String {
    match unread {
        1 => "1 message waiting; `spew mesh messages` shows it.".to_string(),
        n => format!("{n} messages waiting; `spew mesh messages` shows them."),
    }
}

/// Daemon state in one line.
pub fn status_line(s: &Status) -> String {
    let mut text = format!(
        "spew mesh {} on {} ({}): roster {}",
        s.version,
        line(&s.device.name),
        s.device.short_id,
        line(&s.roster.provider)
    );
    if !s.roster.person.is_empty() {
        let _ = write!(text, " as {}", line(&s.roster.person));
    }
    let _ = write!(
        text,
        ", network {}, {} session{}, {} device{} online",
        line(&s.network.network),
        s.counts.sessions,
        plural(s.counts.sessions),
        s.counts.online,
        plural(s.counts.online),
    );
    if s.counts.outbox > 0 {
        let _ = write!(text, ", {} queued", s.counts.outbox);
    }
    text
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Daemon state in full, for `daemon status` and `doctor`.
pub fn status_details(s: &Status, net: &NetworkDetails, now: u64) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "daemon: {} (pid {}, up {})",
        s.version,
        s.pid,
        age(now, s.started_at)
    );
    let _ = writeln!(out, "home: {}", s.home);
    let _ = writeln!(out, "socket: {}", s.socket);
    let _ = writeln!(
        out,
        "device: {} ({}) on {}",
        line(&s.device.name),
        s.device.short_id,
        line(&s.device.platform)
    );
    let _ = writeln!(out, "endpoint id: {}", s.device.endpoint_id);
    let r = &s.roster;
    let mut roster = format!("roster: {}", line(&r.provider));
    if r.configured != r.provider {
        let _ = write!(roster, " (settings say {})", line(&r.configured));
    }
    let _ = write!(roster, ", {}", line(&r.state));
    let _ = writeln!(out, "{roster}");
    if !r.rooms.is_empty() {
        let rooms: Vec<String> = r.rooms.iter().map(|x| line(x)).collect();
        let _ = writeln!(out, "rooms: {}", rooms.join(", "));
    }
    let _ = writeln!(out, "admitted devices: {}", r.admitted);
    if let Some(problem) = &r.problem {
        let _ = writeln!(out, "fix: {}", line(problem));
    }
    out.push_str(&network(net, now));
    let _ = writeln!(
        out,
        "sessions: {}, unread: {}, queued: {}, online devices: {}",
        s.counts.sessions, s.counts.unread, s.counts.outbox, s.counts.online
    );
    out
}

/// The network part of `daemon status` and `doctor` (5.1, 5.3): mode, endpoint, home relay and its status,
/// bound sockets, each admitted device's link, and problems as `fix:` lines when a person can act on them.
pub fn network(net: &NetworkDetails, now: u64) -> String {
    let mut out = String::new();
    let mut head = format!("network: {}", line(&net.network));
    if net.network == "off" {
        head.push_str(" (no other device is admitted)");
    }
    if let Some(id) = &net.endpoint_id {
        let _ = write!(head, ", endpoint {}", id.get(..10).unwrap_or(id));
    }
    let _ = writeln!(out, "{head}");
    for relay in &net.relays {
        let mut text = format!("  home relay {}: ", line(&relay.url));
        if relay.connected {
            text.push_str("connected");
        } else {
            text.push_str("not connected");
            if let Some(e) = &relay.error {
                let _ = write!(text, " ({})", line(e));
            }
        }
        let _ = writeln!(out, "{text}");
        if let Some(why) = &relay.auth_denied {
            let _ = writeln!(
                out,
                "fix: relay {} refused relay_token: {}",
                line(&relay.url),
                line(why)
            );
        }
    }
    if !net.bound_sockets.is_empty() {
        let _ = writeln!(out, "bound: {}", net.bound_sockets.join(", "));
    }
    for d in &net.devices {
        let mut parts = vec![format!("{}@{}", line(&d.person), line(&d.name))];
        if d.online {
            parts.push("online".into());
            if let Some(path) = &d.path {
                parts.push(line(path));
            }
            if let Some(rtt) = d.rtt_ms {
                parts.push(format!("{rtt} ms"));
            }
        } else {
            match d.last_seen {
                Some(t) => parts.push(format!("offline, last seen {} ago", age(now, t))),
                None => parts.push("offline".into()),
            }
        }
        let _ = writeln!(out, "  link {}", parts.join(", "));
    }
    if let Some(e) = &net.error {
        let _ = writeln!(out, "fix: the network endpoint didn't start: {}", line(e));
    }
    for p in &net.problems {
        let mark = if p.needs_fixing() { "fix" } else { "  note" };
        let _ = writeln!(
            out,
            "{mark}: {} {} ago: {}",
            line(&net.device_label(&p.device)),
            age(now, p.at_ms),
            line(&p.detail)
        );
    }
    out
}

/// How many `fix:` lines [`network`] prints.
pub fn network_fixes(net: &NetworkDetails) -> usize {
    usize::from(net.error.is_some())
        + net.problems.iter().filter(|p| p.needs_fixing()).count()
        + net
            .relays
            .iter()
            .filter(|r| r.auth_denied.is_some())
            .count()
}

/// A send result for the terminal: what happened, the answer when one came, and the message id.
pub fn send_result(r: &SendResult) -> String {
    let mut out = String::new();
    match (&r.answer, r.status) {
        (Some(answer), SendStatus::Answered) => {
            let _ = writeln!(out, "{} answered:", line(&answer.from));
            let body = sanitize_text(&answer.body);
            let _ = writeln!(out, "{}", body.trim_end());
            let _ = writeln!(
                out,
                "(answer id {}; `spew mesh reply {}` continues)",
                answer.id, answer.id
            );
        }
        _ => {
            let _ = writeln!(out, "{}", sanitize_text(&r.note).trim_end());
            if r.status == SendStatus::Refused
                && let Some(reason) = &r.reason
                && !r.note.contains(reason.as_str())
            {
                let _ = writeln!(out, "{}", line(reason));
            }
            let _ = writeln!(out, "(message id {})", r.id);
        }
    }
    if r.unread > 0 {
        let _ = writeln!(out, "{}", waiting(r.unread));
    }
    out
}

/// The terminal session's messages.
pub fn messages(f: &Fetched) -> String {
    if f.messages.is_empty() {
        return "No messages.\n".into();
    }
    let mut text = sanitize_text(&f.text);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Your devices from spew.
pub fn devices(list: &[DeviceInfo], now: u64) -> String {
    if list.is_empty() {
        return "No devices are registered with spew.\n".into();
    }
    let mut out = String::new();
    for d in list {
        let mut parts = vec![line(&d.name), d.public_key.fmt_short().to_string()];
        if let Some(p) = &d.platform {
            parts.push(line(p));
        }
        if let Some(t) = d.last_seen_at {
            parts.push(format!("seen {} ago", age(now, t)));
        }
        if d.is_self {
            parts.push("(this device)".into());
        }
        let _ = writeln!(out, "{}", parts.join("  "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_state_says_so() {
        assert_eq!(state_name(ActivityState::Unknown), "unknown");
        assert_eq!(state_name(ActivityState::Busy), "busy");
    }

    #[test]
    fn ages_are_short() {
        assert_eq!(age(61_000, 1_000), "1m");
        assert_eq!(age(5_000, 1_000), "4s");
        assert_eq!(age(3 * 86_400_000, 0), "3d");
        assert_eq!(age(0, 9_000), "0s");
    }

    #[test]
    fn the_network_shows_links_relays_and_what_to_fix() {
        use crate::daemon::status::{DeviceLink, ProblemRow, RelayRow};
        let net = NetworkDetails {
            network: "n0".into(),
            endpoint_id: Some("ab".repeat(32)),
            relay: None,
            relays: vec![RelayRow {
                url: "https://relay.example.com/".into(),
                connected: false,
                error: Some("timed out".into()),
                auth_denied: None,
            }],
            bound_sockets: vec!["0.0.0.0:7461".into()],
            devices: vec![
                DeviceLink {
                    endpoint_id: "cd".repeat(32),
                    name: "maya-mbp".into(),
                    person: "maya".into(),
                    online: true,
                    last_seen: None,
                    path: Some("direct".into()),
                    rtt_ms: Some(4),
                },
                DeviceLink {
                    endpoint_id: "ef".repeat(32),
                    name: "lee-linux".into(),
                    person: "lee".into(),
                    online: false,
                    last_seen: Some(1_000),
                    path: None,
                    rtt_ms: None,
                },
            ],
            problems: vec![
                ProblemRow {
                    device: "ef".repeat(32),
                    problem: "refused".into(),
                    detail: "it refused the link: its roster doesn't admit this device".into(),
                    at_ms: 1_000,
                },
                ProblemRow {
                    device: "12".repeat(32),
                    problem: "unreachable".into(),
                    detail: "no link within 15 s".into(),
                    at_ms: 1_000,
                },
            ],
            ..NetworkDetails::default()
        };
        let text = network(&net, 61_000);
        assert!(text.contains("network: n0, endpoint ababababab"), "{text}");
        assert!(text.contains("home relay https://relay.example.com/: not connected (timed out)"));
        assert!(text.contains("bound: 0.0.0.0:7461"));
        assert!(
            text.contains("link maya@maya-mbp, online, direct, 4 ms"),
            "{text}"
        );
        assert!(text.contains("link lee@lee-linux, offline, last seen 1m ago"));
        assert!(
            text.contains("fix: lee@lee-linux 1m ago: it refused the link"),
            "{text}"
        );
        assert!(
            text.contains("note: 1212121212 1m ago: no link within 15 s"),
            "{text}"
        );
        assert_eq!(network_fixes(&net), 1);
        let off = network(
            &NetworkDetails {
                network: "off".into(),
                ..NetworkDetails::default()
            },
            0,
        );
        assert_eq!(off, "network: off (no other device is admitted)\n");
    }

    #[test]
    fn peer_text_is_cleaned_for_the_terminal() {
        let r = SendResult {
            status: SendStatus::Delivered,
            id: "01J9ZK0000000000000000000A".into(),
            conversation: "01J9ZK0000000000000000000A".into(),
            to: "maya@maya-mbp/claude-web".into(),
            code: None,
            reason: None,
            note: "Delivered\u{1b}[31m to maya@maya-mbp/claude-web.".into(),
            answer: None,
            unread: 2,
        };
        let text = send_result(&r);
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("2 messages waiting"));
    }
}
