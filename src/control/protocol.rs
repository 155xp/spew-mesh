//! The control protocol (6.2): JSON-RPC 2.0, one JSON object per line, and the params and results of every
//! method. The daemon and every client share these types, so a field is spelled once.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents::hook::{HookAnswer, HookCall};
use crate::link::{LinkInfo, LinkStatus};
use crate::wire::{ActivityState, MessageKind, Reach, SessionKind};

/// The largest line either side accepts. A 32 KiB body fits many times over even when escaped.
pub const MAX_LINE: usize = 1024 * 1024;

/// Method names (6.2).
pub mod method {
    /// Opens or re-attaches a session.
    pub const SESSION_OPEN: &str = "session.open";
    /// Updates a session.
    pub const SESSION_UPDATE: &str = "session.update";
    /// Ends a session.
    pub const SESSION_CLOSE: &str = "session.close";
    /// One hook event (8.1).
    pub const HOOK_EVENT: &str = "hook.event";
    /// Marks messages shown.
    pub const MESSAGES_ACK: &str = "messages.ack";
    /// The directory listing (6.3).
    pub const DIRECTORY_LIST: &str = "directory.list";
    /// Asks and waits for the answer (7.2).
    pub const MESSAGE_ASK: &str = "message.ask";
    /// Sends without waiting (7.2).
    pub const MESSAGE_TELL: &str = "message.tell";
    /// Answers a received message (7.2).
    pub const MESSAGE_REPLY: &str = "message.reply";
    /// Returns a session's messages (7.3).
    pub const MESSAGES_FETCH: &str = "messages.fetch";
    /// Hands a responder its next question (9).
    pub const RESPONDER_TAKE: &str = "responder.take";
    /// The daemon's state.
    pub const DAEMON_STATUS: &str = "daemon.status";
    /// Re-reads the roster now.
    pub const DIRECTORY_REFRESH: &str = "directory.refresh";
    /// Your devices from spew (14.3).
    pub const DEVICES_LIST: &str = "devices.list";
    /// Revokes one of your devices (14.3).
    pub const DEVICES_REVOKE: &str = "devices.revoke";
    /// spew sign-in (14.1).
    pub const AUTH_LOGIN: &str = "auth.login";
    /// spew sign-out (14.3).
    pub const AUTH_LOGOUT: &str = "auth.logout";
    /// Asks the daemon to exit.
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    /// Notification the daemon sends during `auth.login` with the page to open.
    pub const AUTH_LOGIN_URL: &str = "auth.login.url";
}

/// The `data.reason` of an error (6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    /// No such session, message or device.
    NotFound,
    /// The message or request was refused.
    Refused,
    /// The caller may not do this.
    Forbidden,
    /// It clashes with something: an ambiguous address, a name in use.
    Conflict,
    /// A wait ran out.
    Timeout,
    /// The request is malformed.
    Invalid,
    /// Something the daemon needs isn't there: spew, the network.
    Unavailable,
}

impl Reason {
    /// The JSON-RPC error code for this reason, in the -32000 range.
    pub fn code(self) -> i64 {
        match self {
            Reason::NotFound => -32001,
            Reason::Refused => -32002,
            Reason::Forbidden => -32003,
            Reason::Conflict => -32004,
            Reason::Timeout => -32005,
            Reason::Invalid => -32006,
            Reason::Unavailable => -32007,
        }
    }

    /// The reason as sent.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::NotFound => "not-found",
            Reason::Refused => "refused",
            Reason::Forbidden => "forbidden",
            Reason::Conflict => "conflict",
            Reason::Timeout => "timeout",
            Reason::Invalid => "invalid",
            Reason::Unavailable => "unavailable",
        }
    }
}

/// An error a method returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    /// Why, as `data.reason`.
    pub reason: Reason,
    /// One sentence for the person or agent.
    pub message: String,
    /// The JSON-RPC code; [`Reason::code`] unless a protocol error.
    pub code: i64,
}

impl RpcError {
    /// An error with the code that goes with `reason`.
    pub fn new(reason: Reason, message: impl Into<String>) -> RpcError {
        RpcError {
            reason,
            message: message.into(),
            code: reason.code(),
        }
    }

    /// `not-found`.
    pub fn not_found(message: impl Into<String>) -> RpcError {
        RpcError::new(Reason::NotFound, message)
    }

    /// `invalid`.
    pub fn invalid(message: impl Into<String>) -> RpcError {
        RpcError::new(Reason::Invalid, message)
    }

    /// `refused`.
    pub fn refused(message: impl Into<String>) -> RpcError {
        RpcError::new(Reason::Refused, message)
    }

    /// `forbidden`.
    pub fn forbidden(message: impl Into<String>) -> RpcError {
        RpcError::new(Reason::Forbidden, message)
    }

    /// `conflict`.
    pub fn conflict(message: impl Into<String>) -> RpcError {
        RpcError::new(Reason::Conflict, message)
    }

    /// `unavailable`.
    pub fn unavailable(message: impl Into<String>) -> RpcError {
        RpcError::new(Reason::Unavailable, message)
    }

    /// A request line that isn't JSON (JSON-RPC's parse error, with reason `invalid`).
    pub fn parse_error(message: impl Into<String>) -> RpcError {
        RpcError {
            reason: Reason::Invalid,
            message: message.into(),
            code: -32700,
        }
    }

    /// A method this daemon doesn't have (JSON-RPC's method-not-found, with reason `not-found`).
    pub fn unknown_method(method: &str) -> RpcError {
        RpcError {
            reason: Reason::NotFound,
            message: format!("no method {method:?}"),
            code: -32601,
        }
    }

    /// Params that don't fit the method (JSON-RPC's invalid-params, with reason `invalid`).
    pub fn bad_params(message: impl Into<String>) -> RpcError {
        RpcError {
            reason: Reason::Invalid,
            message: message.into(),
            code: -32602,
        }
    }

    /// The JSON-RPC error object.
    pub fn to_object(&self) -> Value {
        serde_json::json!({
            "code": self.code,
            "message": self.message,
            "data": {"reason": self.reason.as_str()},
        })
    }

    /// Reads a JSON-RPC error object. One without a known `data.reason` is `unavailable`, never something wider.
    pub fn from_object(value: &Value) -> RpcError {
        let code = value.get("code").and_then(Value::as_i64).unwrap_or(-32000);
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the daemon returned an error")
            .to_string();
        let reason = value
            .get("data")
            .and_then(|d| d.get("reason"))
            .cloned()
            .and_then(|r| serde_json::from_value(r).ok())
            .unwrap_or(Reason::Unavailable);
        RpcError {
            reason,
            message,
            code,
        }
    }
}

/// A request id: a number or a string (JSON-RPC 2.0).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A number.
    Number(i64),
    /// A string.
    Text(String),
}

/// One incoming line, parsed far enough to dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A request with an id, to be answered.
    Request {
        /// Its id.
        id: RequestId,
        /// The method.
        method: String,
        /// The params; `null` when absent.
        params: Value,
    },
    /// A notification: no id, no answer.
    Notification {
        /// The method.
        method: String,
        /// The params.
        params: Value,
    },
    /// A response to a request this side sent.
    Response {
        /// The request's id.
        id: RequestId,
        /// The result or the error.
        outcome: Result<Value, RpcError>,
    },
}

/// Parses one line. `Err` carries the id when one could be read, so the error can still be answered.
pub fn parse_line(line: &str) -> Result<Inbound, (Option<RequestId>, RpcError)> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| (None, RpcError::parse_error(format!("not JSON: {e}"))))?;
    let Value::Object(mut map) = value else {
        return Err((
            None,
            RpcError::parse_error("a JSON-RPC message is an object"),
        ));
    };
    let id = match map.remove("id") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(serde_json::from_value::<RequestId>(raw).map_err(|_| {
            (
                None,
                RpcError::parse_error("the id must be a number or a string"),
            )
        })?),
    };
    if map.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((id, RpcError::parse_error("jsonrpc must be \"2.0\"")));
    }
    if let Some(method) = map.remove("method") {
        let Value::String(method) = method else {
            return Err((id, RpcError::parse_error("the method must be a string")));
        };
        let params = map.remove("params").unwrap_or(Value::Null);
        return Ok(match id {
            Some(id) => Inbound::Request { id, method, params },
            None => Inbound::Notification { method, params },
        });
    }
    let Some(id) = id else {
        return Err((None, RpcError::parse_error("a response needs an id")));
    };
    if let Some(error) = map.remove("error") {
        return Ok(Inbound::Response {
            id,
            outcome: Err(RpcError::from_object(&error)),
        });
    }
    match map.remove("result") {
        Some(result) => Ok(Inbound::Response {
            id,
            outcome: Ok(result),
        }),
        None => Err((
            Some(id),
            RpcError::parse_error("neither a method, a result nor an error"),
        )),
    }
}

/// A request line.
pub fn request_line(id: &RequestId, method: &str, params: &Value) -> String {
    let mut line =
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            .to_string();
    line.push('\n');
    line
}

/// A notification line.
pub fn notification_line(method: &str, params: &Value) -> String {
    let mut line =
        serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
    line.push('\n');
    line
}

/// A response line.
pub fn response_line(id: Option<&RequestId>, outcome: &Result<Value, RpcError>) -> String {
    let mut line = match outcome {
        Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => serde_json::json!({"jsonrpc": "2.0", "id": id, "error": error.to_object()}),
    }
    .to_string();
    line.push('\n');
    line
}

/// Reads typed params; a mismatch is `invalid params`.
pub fn params<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    let value = if value.is_null() {
        Value::Object(Default::default())
    } else {
        value
    };
    serde_json::from_value(value).map_err(|e| RpcError::bad_params(format!("bad params: {e}")))
}

/// Encodes a result.
pub fn result<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value)
        .map_err(|e| RpcError::unavailable(format!("could not encode the result: {e}")))
}

/// `{}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// `session.open` params (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpen {
    /// The client slug: `claude`, `codex`, `opencode`, `terminal`, a `SPEW_MESH_CLIENT` slug or `other`.
    pub client: String,
    /// The client's own session or thread id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_session: Option<String>,
    /// The client's process id, for clients that give one (8.2 to 8.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_pid: Option<u32>,
    /// Working directory.
    pub cwd: String,
    /// The name the client gave itself in the MCP handshake, for logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// live, responder or terminal.
    pub kind: SessionKind,
    /// A responder's `--name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `SPEW_PROJECT` from the client's environment (11). Not in 6.2's table; the room needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spew_project: Option<String>,
    /// A responder's `[serve]` limits as serve read them (9). Not in 6.2's table: the daemon applies them when
    /// a question arrives (5.5 step 6), so a settings change that serve picked up holds even when the daemon
    /// started before it. Only responders send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serve: Option<crate::responder::limits::GateSettings>,
}

/// `session.open` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOpened {
    /// Session id.
    pub session: String,
    /// Session name.
    pub name: String,
    /// Full address, `person@device/name`.
    pub address: String,
    /// The token every session method needs.
    pub token: String,
}

/// `session.update` params (6.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdate {
    /// The session's token.
    pub token: String,
    /// A new working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Busy, idle or unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ActivityState>,
    /// A new activity line; empty clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// Cards to add.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards_add: Vec<String>,
    /// Cards to drop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cards_remove: Vec<String>,
    /// The client session id learned at a tool call (Codex `_meta.sessionId`, 8.3). Not in 6.2's table; binding
    /// it may merge two sessions (6.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_session: Option<String>,
}

/// Params that are only a token (`session.close`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenOnly {
    /// The session's token.
    pub token: String,
}

/// `hook.event` params: the hook's facts (6.2, 8.1).
pub type HookEvent = HookCall;
/// `hook.event` result.
pub type HookResult = HookAnswer;

/// `messages.ack` params: a token, or the (client, client session) a hook names (6.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    /// The session's token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The hook's client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// The hook's client session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_session: Option<String>,
    /// Message ids to mark shown.
    pub ids: Vec<String>,
}

/// `directory.list` params (6.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryQuery {
    /// The caller's token, for the "this session" marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// A substring over address, client, repo, branch, card and activity.
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Also list offline devices with their last-seen time.
    #[serde(default)]
    pub include_offline: bool,
}

/// One session row of the listing (6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    /// `person@device/name`.
    pub address: String,
    /// The person.
    pub person: String,
    /// The device name.
    pub device: String,
    /// The session name.
    pub name: String,
    /// Client slug.
    pub client: String,
    /// live or responder.
    pub kind: SessionKind,
    /// busy, idle or unknown.
    pub state: ActivityState,
    /// Since when, ms.
    pub since: u64,
    /// Repo slug.
    #[serde(default)]
    pub repo: Option<String>,
    /// Branch.
    #[serde(default)]
    pub branch: Option<String>,
    /// Place.
    #[serde(default)]
    pub place: Option<String>,
    /// The session's room; none means private.
    #[serde(default)]
    pub room: Option<String>,
    /// Cards held.
    #[serde(default)]
    pub cards: Vec<String>,
    /// Activity line.
    #[serde(default)]
    pub activity: Option<String>,
    /// How messages reach it.
    pub reach: Reach,
    /// A responder's waiting questions.
    #[serde(default)]
    pub queue: u32,
    /// The caller's own session.
    #[serde(default)]
    pub this: bool,
    /// Whether the device is online.
    pub online: bool,
}

/// The sessions of one person in one room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonGroup {
    /// The person.
    pub person: String,
    /// Whether it is you.
    pub own: bool,
    /// Their sessions in this room.
    pub sessions: Vec<SessionRow>,
}

/// One room of the listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomGroup {
    /// Room id; none for your sessions with no room ("private: no .spew.json").
    pub room: Option<String>,
    /// Name to show.
    pub name: String,
    /// People, you first.
    pub people: Vec<PersonGroup>,
}

/// One device in the listing, for `--offline` and `--links`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRow {
    /// Endpoint id.
    pub endpoint_id: String,
    /// The device name.
    pub name: String,
    /// Its person.
    pub person: String,
    /// One of your own devices.
    pub own: bool,
    /// This device.
    pub this: bool,
    /// Whether a link is up (always true for this device).
    pub online: bool,
    /// When it was last seen, ms, for offline devices.
    #[serde(default)]
    pub last_seen: Option<u64>,
    /// `direct` or `relay`.
    #[serde(default)]
    pub path: Option<String>,
    /// Round-trip time in ms.
    #[serde(default)]
    pub rtt_ms: Option<u64>,
}

/// `directory.list` result (6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directory {
    /// You.
    pub me: String,
    /// This device's name.
    pub device: String,
    /// Grouped by room, then by person; yours first.
    pub rooms: Vec<RoomGroup>,
    /// Devices: online ones, and offline ones when asked for.
    pub devices: Vec<DeviceRow>,
    /// The caller's unread messages, when a token was given.
    #[serde(default)]
    pub unread: usize,
}

/// `message.ask` params (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskParams {
    /// The asking session's token.
    pub token: String,
    /// The address (2.2).
    pub to: String,
    /// The question.
    pub text: String,
    /// A card ref it is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// How long to wait for the answer; default 100, at most 900 (7.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<u64>,
}

/// `message.tell` params (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TellParams {
    /// The sending session's token.
    pub token: String,
    /// The address (2.2).
    pub to: String,
    /// The text.
    pub text: String,
    /// A card ref it is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
}

/// `message.reply` params (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyParams {
    /// The replying session's token.
    pub token: String,
    /// The id of a message the session received.
    pub id: String,
    /// The text.
    pub text: String,
    /// With a message that isn't an ask: send a follow-up ask and wait this long (7.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<u64>,
}

/// Where a sent message is (7.1, 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendStatus {
    /// In the outbox: the device is offline or no receipt came yet.
    Queued,
    /// The target device stored it.
    Delivered,
    /// The target session was shown it, with no answer yet.
    Shown,
    /// The answer is in `answer`.
    Answered,
    /// Refused; see `code` and `reason`.
    Refused,
    /// It ran out of time.
    Expired,
}

/// An answer as `ask` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerView {
    /// The answer's message id.
    pub id: String,
    /// Who gave it, `person@device/name`.
    pub from: String,
    /// The sanitized body.
    pub body: String,
    /// The answer as a message block (7.8).
    pub text: String,
}

/// The result of `ask`, `tell` and `reply` (6.2, 7.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendResult {
    /// Where the message is.
    pub status: SendStatus,
    /// The message id.
    pub id: String,
    /// Its conversation.
    pub conversation: String,
    /// The full address it went to.
    pub to: String,
    /// The refusal code, when refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Why it is queued or refused, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// One sentence on where things stand, for the agent.
    pub note: String,
    /// The answer, for an ask that got one in time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<AnswerView>,
    /// The caller's unread messages, for the "N messages waiting" line (7.3).
    #[serde(default)]
    pub unread: usize,
}

/// `messages.fetch` params (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchParams {
    /// The session's token.
    pub token: String,
    /// Wait this long for a message when none is unread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_seconds: Option<u64>,
    /// Return messages without marking them shown.
    #[serde(default)]
    pub keep_unread: bool,
}

/// One message as `messages.fetch` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageView {
    /// Message id.
    pub id: String,
    /// ask, tell, answer or notice.
    pub kind: MessageKind,
    /// Conversation id.
    pub conversation: String,
    /// The message it replies to.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// Sender address, `person@device/name` (`mesh` for notices).
    pub from: String,
    /// Sender's person.
    pub person: String,
    /// Sanitized body.
    pub body: String,
    /// Card ref.
    #[serde(default)]
    pub card: Option<String>,
    /// Sender's repo.
    #[serde(default)]
    pub repo: Option<String>,
    /// The room it came through.
    #[serde(default)]
    pub room: Option<String>,
    /// Sender clock, ms.
    pub sent_at: u64,
    /// An ask shown before and still unanswered (7.3).
    pub again: bool,
}

/// `messages.fetch` result (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fetched {
    /// Unread messages first, then asks shown before and not answered.
    pub messages: Vec<MessageView>,
    /// The same as message blocks with the trailer (7.8); empty when there are none.
    pub text: String,
    /// Unread messages left (with `keep_unread`, all of them).
    pub unread: usize,
}

/// `responder.take` params (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TakeParams {
    /// The responder's token.
    pub token: String,
    /// How long to wait for a question.
    pub wait_seconds: u64,
}

/// Who asked a responder (9), as the daemon knows it from the link and the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskerView {
    /// The asking person.
    pub person: String,
    /// The asking device's name.
    pub device: String,
    /// The asking device's endpoint id.
    pub device_id: String,
    /// The asking session's id on its device.
    pub session: String,
    /// The asking session's name.
    pub name: String,
    /// Its client.
    pub client: String,
    /// One of the responder's person's own devices.
    pub own: bool,
    /// The room the message came through.
    #[serde(default)]
    pub room: Option<String>,
}

/// A question handed to a responder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionView {
    /// Message id; `message.reply` answers it.
    pub id: String,
    /// Conversation id.
    pub conversation: String,
    /// ask or tell.
    pub kind: MessageKind,
    /// Who sent it.
    pub from: AskerView,
    /// The asker's repo.
    #[serde(default)]
    pub repo: Option<String>,
    /// The card it carries.
    #[serde(default)]
    pub card: Option<String>,
    /// The sanitized body.
    pub body: String,
}

impl QuestionView {
    /// The responder's own type, for [`crate::responder::Responder::answer`]. `None` for a kind a responder
    /// doesn't take.
    pub fn into_question(self) -> Option<crate::responder::Question> {
        use crate::responder::{Asker, MessageKind as Kind, Question};
        let kind = match self.kind {
            MessageKind::Ask => Kind::Ask,
            MessageKind::Tell => Kind::Tell,
            MessageKind::Answer | MessageKind::Notice => return None,
        };
        Some(Question {
            id: self.id,
            conversation: self.conversation,
            kind,
            from: Asker {
                person: self.from.person,
                device: self.from.device,
                device_id: self.from.device_id,
                session: self.from.session,
                name: self.from.name,
                client: self.from.client,
                own: self.from.own,
                room: self.from.room,
            },
            repo: self.repo,
            card: self.card,
            body: self.body,
        })
    }
}

/// `responder.take` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Taken {
    /// The next question, or none when the wait ran out.
    #[serde(default)]
    pub question: Option<QuestionView>,
}

/// This device in `daemon.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    /// Device name.
    pub name: String,
    /// Endpoint id, 64 hex characters.
    pub endpoint_id: String,
    /// Its first 10 characters.
    pub short_id: String,
    /// `macos`, `linux`.
    pub platform: String,
}

/// The roster in `daemon.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterStatus {
    /// The provider in force: local, file or spew.
    pub provider: String,
    /// The provider settings ask for.
    pub configured: String,
    /// You.
    pub person: String,
    /// The roster's version.
    pub version: String,
    /// One line on its state.
    pub state: String,
    /// What keeps the configured provider from working, for `doctor`.
    #[serde(default)]
    pub problem: Option<String>,
    /// Rooms you are in, other than your private room.
    pub rooms: Vec<String>,
    /// Admitted devices.
    pub admitted: usize,
}

/// Counts in `daemon.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Live sessions on this device.
    pub sessions: usize,
    /// Unread messages for them.
    pub unread: usize,
    /// Outbox entries.
    pub outbox: usize,
    /// Devices with a link up.
    pub online: usize,
}

/// `daemon.status` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The daemon's version.
    pub version: String,
    /// Its process id.
    pub pid: u32,
    /// When it started, ms.
    pub started_at: u64,
    /// Its home folder.
    pub home: String,
    /// The control socket.
    pub socket: String,
    /// This device.
    pub device: DeviceStatus,
    /// The roster.
    pub roster: RosterStatus,
    /// Network state from the link layer (5.1).
    pub network: NetworkStatus,
    /// Sockets the endpoint is bound to; empty while the network is off.
    pub bound_sockets: Vec<String>,
    /// Counts.
    pub counts: Counts,
}

/// The link layer's state as `daemon.status` reports it (5.1): a copy of [`LinkStatus`] that clients can read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStatus {
    /// `off`, `local`, `n0` or `own`.
    pub network: String,
    /// This device's endpoint id, when an endpoint is up.
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// The home relay, when connected to one.
    #[serde(default)]
    pub relay: Option<String>,
    /// One entry per open link.
    #[serde(default)]
    pub links: Vec<LinkRow>,
}

/// One open link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRow {
    /// The peer's endpoint id.
    pub device: String,
    /// `direct` or `relay`.
    pub path: String,
    /// Round-trip time in ms.
    #[serde(default)]
    pub rtt_ms: Option<u64>,
}

impl From<LinkStatus> for NetworkStatus {
    fn from(status: LinkStatus) -> NetworkStatus {
        NetworkStatus {
            network: status.network,
            endpoint_id: status.endpoint_id,
            relay: status.relay,
            links: status.links.into_iter().map(LinkRow::from).collect(),
        }
    }
}

impl From<LinkInfo> for LinkRow {
    fn from(info: LinkInfo) -> LinkRow {
        LinkRow {
            device: info.device,
            path: info.path,
            rtt_ms: info.rtt_ms,
        }
    }
}

/// `devices.revoke` params.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeParams {
    /// A device name, an endpoint id or its short form.
    pub device: String,
}

/// `devices.list` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    /// Your devices.
    pub devices: Vec<crate::spew_api::DeviceInfo>,
}

/// `devices.revoke` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoked {
    /// The device's name.
    pub name: String,
    /// Its endpoint id.
    pub endpoint_id: String,
}

/// The `auth.login.url` notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginUrl {
    /// The page to open.
    pub url: String,
}

/// `auth.login` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginResult {
    /// Your spew username.
    pub username: String,
    /// Your display name.
    pub display: String,
    /// Project keys chosen at consent.
    pub projects: Vec<String>,
    /// This device's name as registered.
    pub device: String,
    /// Whether the device registration went through.
    pub registered: bool,
    /// A problem worth showing, such as a failed registration.
    #[serde(default)]
    pub note: Option<String>,
}

/// `auth.logout` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutResult {
    /// There was a sign-in to undo.
    pub was_signed_in: bool,
    /// spew unregistered this device.
    pub device_unregistered: bool,
    /// spew revoked the refresh token.
    pub token_revoked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_round_trip_and_codes() {
        for reason in [
            Reason::NotFound,
            Reason::Refused,
            Reason::Forbidden,
            Reason::Conflict,
            Reason::Timeout,
            Reason::Invalid,
            Reason::Unavailable,
        ] {
            assert!((-32099..=-32000).contains(&reason.code()));
            let err = RpcError::new(reason, "x");
            let back = RpcError::from_object(&err.to_object());
            assert_eq!(back, err);
            assert_eq!(serde_json::to_value(reason).unwrap(), reason.as_str());
        }
        let odd = RpcError::from_object(&serde_json::json!({"code": -1, "message": "m"}));
        assert_eq!(odd.reason, Reason::Unavailable);
    }

    #[test]
    fn lines_parse() {
        let id = RequestId::Number(7);
        let line = request_line(&id, "daemon.status", &Value::Null);
        assert!(line.ends_with('\n'));
        assert_eq!(
            parse_line(line.trim()).unwrap(),
            Inbound::Request {
                id: id.clone(),
                method: "daemon.status".into(),
                params: Value::Null
            }
        );
        let ok = response_line(Some(&id), &Ok(serde_json::json!({"a": 1})));
        assert_eq!(
            parse_line(ok.trim()).unwrap(),
            Inbound::Response {
                id: id.clone(),
                outcome: Ok(serde_json::json!({"a": 1}))
            }
        );
        let err = response_line(Some(&id), &Err(RpcError::not_found("gone")));
        match parse_line(err.trim()).unwrap() {
            Inbound::Response {
                outcome: Err(e), ..
            } => {
                assert_eq!(e.reason, Reason::NotFound);
                assert_eq!(e.message, "gone");
            }
            other => panic!("{other:?}"),
        }
        let note = notification_line("auth.login.url", &serde_json::json!({"url": "u"}));
        assert!(matches!(
            parse_line(note.trim()).unwrap(),
            Inbound::Notification { .. }
        ));
        let text_id = r#"{"jsonrpc":"2.0","id":"a","method":"m"}"#;
        assert!(matches!(
            parse_line(text_id).unwrap(),
            Inbound::Request {
                id: RequestId::Text(_),
                ..
            }
        ));
    }

    #[test]
    fn bad_lines() {
        for (line, has_id) in [
            ("not json", false),
            ("[1]", false),
            (r#"{"jsonrpc":"1.0","id":1,"method":"m"}"#, true),
            (r#"{"jsonrpc":"2.0","id":1,"method":5}"#, true),
            (r#"{"jsonrpc":"2.0","id":{},"method":"m"}"#, false),
            (r#"{"jsonrpc":"2.0","id":1}"#, true),
            (r#"{"jsonrpc":"2.0"}"#, false),
        ] {
            let (id, err) = parse_line(line).unwrap_err();
            assert_eq!(id.is_some(), has_id, "{line}");
            assert_eq!(err.code, -32700, "{line}");
        }
    }

    #[test]
    fn params_are_strict() {
        assert!(params::<TokenOnly>(serde_json::json!({"token": "t"})).is_ok());
        let err = params::<TokenOnly>(serde_json::json!({"token": "t", "x": 1})).unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(params::<Empty>(Value::Null).is_ok());
        let query: DirectoryQuery =
            params(serde_json::json!({"match": "api", "include_offline": true})).unwrap();
        assert_eq!(query.filter.as_deref(), Some("api"));
    }
}
