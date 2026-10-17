//! serve: answering questions with headless agent runs (9).
//!
//! [`Responder`] takes one question and returns the answer text, or a failure whose reply line ends the
//! asker's wait. Underneath: the command line, environment and permission set for each client (8.2 to 8.4),
//! a runner that spawns each run in its own process group with a timeout and an output cap, the run limits
//! and queue, the conversation map, and the prompt. Taking questions from the daemon (`responder.take`) and
//! sending answers back (`message.reply`) are wired on top of [`Responder::answer`].

pub mod claude;
pub mod codex;
mod config;
pub mod env;
pub mod limits;
pub mod memory;
pub mod opencode;
pub mod process;
pub mod prompt;
mod serve;

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

pub use config::{ServeConfig, ServeFrom, ServeSettings};
pub use serve::{Answer, MAX_ANSWER_BYTES, Responder};

/// The agent CLI a responder drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServeClient {
    /// Claude Code (`claude -p`).
    Claude,
    /// Codex (`codex exec`).
    Codex,
    /// OpenCode (`opencode run`).
    Opencode,
}

impl ServeClient {
    /// The client's name as used in session names, presence and the conversation map.
    pub fn as_str(self) -> &'static str {
        match self {
            ServeClient::Claude => "claude",
            ServeClient::Codex => "codex",
            ServeClient::Opencode => "opencode",
        }
    }

    /// The executable looked up on PATH.
    pub fn program(self) -> &'static str {
        self.as_str()
    }
}

impl fmt::Display for ServeClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ServeClient {
    type Err = ServeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(ServeClient::Claude),
            "codex" => Ok(ServeClient::Codex),
            "opencode" => Ok(ServeClient::Opencode),
            other => Err(ServeError::Invalid(format!(
                "serve takes claude, codex or opencode, not {other:?}"
            ))),
        }
    }
}

/// Why a responder refuses to start. Each one is shown to the person (and by `doctor`).
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// A setting or flag that is out of range or malformed.
    #[error("invalid serve setting: {0}")]
    Invalid(String),
    /// The client's command is not on PATH.
    #[error("{0} is not on PATH")]
    ProgramNotFound(&'static str),
    /// A config file that can't be read or parsed.
    #[error("can't use {path}: {reason}")]
    Config { path: PathBuf, reason: String },
    /// The Codex provider table holds a header or token key (8.3).
    #[error(
        "the Codex provider {provider:?} sets {key:?}; serve passes the provider on the command line, \
         where other local users can see it, so use a provider without inline headers or tokens"
    )]
    ProviderSecret { provider: String, key: String },
    /// A repo's Codex project config sets a key that would weaken the serve profile (8.3).
    #[error(
        "{path} sets {key:?}, which would change what a serve run may do; serve won't run in this repo"
    )]
    ProjectConfig { path: PathBuf, key: String },
    /// The Codex permission profile self-check failed (8.3).
    #[error("the Codex sandbox check failed: {0}")]
    Canary(String),
    /// A file system or process error while preparing.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl ServeError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        ServeError::Io {
            context: context.into(),
            source,
        }
    }
}

/// What a finished run produced.
#[derive(Clone, Debug, PartialEq)]
pub struct RunResult {
    /// The agent's final text.
    pub answer: String,
    /// The client's session or thread id, for follow-ups.
    pub session_id: Option<String>,
    /// The cost the client reported (Claude Code's `total_cost_usd`).
    pub cost_usd: Option<f64>,
}

/// A run that produced no usable answer (9).
#[derive(Clone, Debug, PartialEq)]
pub struct RunFailure {
    /// One line naming the failure, without a trailing period.
    pub reason: String,
    /// The session id, when the client reported one before failing.
    pub session_id: Option<String>,
    /// The cost the client reported, if any.
    pub cost_usd: Option<f64>,
}

impl RunFailure {
    /// A failure with only a reason.
    pub fn new(reason: impl Into<String>) -> Self {
        RunFailure {
            reason: one_line(&reason.into(), 300),
            session_id: None,
            cost_usd: None,
        }
    }

    /// The reply sent to the asker: "The serve run failed: <reason>."
    pub fn reply_text(&self) -> String {
        let reason = self.reason.trim().trim_end_matches('.');
        format!("The serve run failed: {reason}.")
    }
}

impl fmt::Display for RunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

/// The kind of message a responder answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    /// A question that waits for an answer.
    Ask,
    /// A note; the responder's reply goes back as a message of its own.
    Tell,
}

impl MessageKind {
    /// The kind as written in the message block header.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Ask => "ask",
            MessageKind::Tell => "tell",
        }
    }
}

/// Who sent a question, as the daemon knows it from the link and the roster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asker {
    /// The asking person (`maya`).
    pub person: String,
    /// The asking device's name (`maya-mbp`).
    pub device: String,
    /// A stable id for the asking device (its endpoint id), used in the conversation map.
    pub device_id: String,
    /// The asking session's id on its device.
    pub session: String,
    /// The asking session's name (`claude-api`).
    pub name: String,
    /// The asking session's client (`claude`).
    pub client: String,
    /// True when the asker is one of the responder's person's own devices.
    pub own: bool,
    /// The room the message came through, if any (`spew`).
    pub room: Option<String>,
}

impl Asker {
    /// The asker's address as shown to people and agents: `person/name`.
    pub fn address(&self) -> String {
        format!("{}/{}", self.person, self.name)
    }
}

/// One question for a responder, as `responder.take` hands it over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The message id.
    pub id: String,
    /// The conversation id (the id of the conversation's first message).
    pub conversation: String,
    /// ask or tell.
    pub kind: MessageKind,
    /// Who sent it.
    pub from: Asker,
    /// The asker's repo slug, if it sent one.
    pub repo: Option<String>,
    /// The card the message carries, if any.
    pub card: Option<String>,
    /// The message text, as stored by the daemon.
    pub body: String,
}

/// Cuts text to its first non-empty line, at most `max` bytes (on a char boundary), for reasons.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let clean: String = line.chars().filter(|c| !c.is_control()).collect();
    if clean.len() <= max {
        return clean;
    }
    let mut end = max;
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &clean[..end])
}

/// Random lowercase hex, for canary tokens.
pub(crate) fn random_hex(bytes: usize) -> Result<String, ServeError> {
    random_bytes(bytes).map(|buf| buf.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn random_bytes(len: usize) -> Result<Vec<u8>, ServeError> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).map_err(|e| {
        ServeError::io(
            "can't read random bytes",
            std::io::Error::other(e.to_string()),
        )
    })?;
    Ok(buf)
}

/// An id a client handed back that is safe to pass as one argv element: no leading `-`, a small
/// character set, bounded length.
pub(crate) fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_text_has_one_period() {
        assert_eq!(
            RunFailure::new("codex exited with status 1.").reply_text(),
            "The serve run failed: codex exited with status 1."
        );
        assert_eq!(
            RunFailure::new("timeout").reply_text(),
            "The serve run failed: timeout."
        );
    }

    #[test]
    fn one_line_takes_first_line_and_cuts() {
        assert_eq!(one_line("\n  first \nsecond", 100), "first");
        assert_eq!(one_line("abcdef", 3), "abc...");
        assert_eq!(one_line("a\u{7}b", 10), "ab");
        assert_eq!(one_line("ééé", 3), "é...");
    }

    #[test]
    fn safe_ids() {
        assert!(is_safe_id("ses_01ABCdef"));
        assert!(is_safe_id("0199a3e1-0000-7000-8000-000000000001"));
        assert!(!is_safe_id("--model"));
        assert!(!is_safe_id("a b"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("a/b"));
    }

    #[test]
    fn client_names_parse() {
        assert_eq!(
            "claude".parse::<ServeClient>().ok(),
            Some(ServeClient::Claude)
        );
        assert_eq!(
            "codex".parse::<ServeClient>().ok(),
            Some(ServeClient::Codex)
        );
        assert_eq!(
            "opencode".parse::<ServeClient>().ok(),
            Some(ServeClient::Opencode)
        );
        assert!("cursor".parse::<ServeClient>().is_err());
    }
}
