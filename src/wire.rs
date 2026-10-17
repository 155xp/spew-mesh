//! Frames and the types sent between devices (5.4 to 5.6).
//!
//! This module holds the codec, the call and response types, and every check that needs nothing but the payload:
//! the envelope shape check (5.5 step 3) and presence sanitizing (5.6). Admission, rate, dedupe, threading
//! against stored messages and target checks need daemon state and live with the daemon.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::address::{
    RESERVED_NAME, is_card, is_client, is_device_name, is_repo, is_room, is_session_name, is_ulid,
};
use crate::text::{sanitize_line, sanitize_text};

/// The ALPN every link uses (5.1).
pub const ALPN: &[u8] = b"spew-mesh/1";
/// Lowest protocol version this build speaks (5.4).
pub const PROTO_MIN: u32 = 1;
/// Highest protocol version this build speaks (5.4).
pub const PROTO_MAX: u32 = 1;
/// Largest frame payload in bytes (5.4).
pub const MAX_FRAME: usize = 128 * 1024;
/// Largest message body in bytes, after sanitizing (5.5, 7.1).
pub const MAX_BODY: usize = 32 * 1024;
/// Default message lifetime (7.4).
pub const DEFAULT_TTL_SECS: u64 = 86_400;
/// Longest message lifetime (7.4).
pub const MAX_TTL_SECS: u64 = 604_800;
/// Most sessions a presence snapshot may carry (5.6).
pub const MAX_PRESENCE_SESSIONS: usize = 100;
/// Longest `place` (5.6).
pub const MAX_PLACE: usize = 64;
/// Longest `branch` (5.6).
pub const MAX_BRANCH: usize = 100;
/// Longest `activity`, in characters (5.6).
pub const MAX_ACTIVITY: usize = 140;
/// Most cards kept per presence session. The spec sets no limit; the frame size bounds it anyway.
pub const MAX_PRESENCE_CARDS: usize = 50;

/// The `app` string this build sends in `hello`.
pub fn app_version() -> String {
    format!("spew-mesh/{}", env!("CARGO_PKG_VERSION"))
}

/// Application close codes for links (5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCode {
    /// The remote endpoint id is not admitted.
    NotAdmitted,
    /// A second link between the same pair of devices.
    Duplicate,
    /// No protocol version in common.
    Version,
    /// The daemon is stopping.
    Shutdown,
}

impl CloseCode {
    /// The numeric code sent in the QUIC close frame.
    pub fn code(self) -> u32 {
        match self {
            CloseCode::NotAdmitted => 1,
            CloseCode::Duplicate => 2,
            CloseCode::Version => 3,
            CloseCode::Shutdown => 4,
        }
    }

    /// The reason text sent with the code.
    pub fn reason(self) -> &'static str {
        match self {
            CloseCode::NotAdmitted => "not-admitted",
            CloseCode::Duplicate => "duplicate",
            CloseCode::Version => "version",
            CloseCode::Shutdown => "shutdown",
        }
    }

    /// The close code for a number, if it is one of ours.
    pub fn from_code(code: u64) -> Option<CloseCode> {
        match code {
            1 => Some(CloseCode::NotAdmitted),
            2 => Some(CloseCode::Duplicate),
            3 => Some(CloseCode::Version),
            4 => Some(CloseCode::Shutdown),
            _ => None,
        }
    }
}

/// A frame that couldn't be read, written or parsed.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The length prefix is over [`MAX_FRAME`].
    #[error("frame of {0} bytes is over the {MAX_FRAME}-byte limit")]
    TooLarge(usize),
    /// The length prefix is zero.
    #[error("empty frame")]
    Empty,
    /// The stream ended before the frame did.
    #[error("the stream ended inside a frame")]
    Truncated,
    /// The payload isn't the JSON expected.
    #[error("frame is not valid: {0}")]
    Json(String),
    /// Reading or writing failed.
    #[error("frame i/o failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Encodes a value as one frame: a 4-byte big-endian length, then that many bytes of JSON (5.4).
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let json = serde_json::to_vec(value).map_err(|e| FrameError::Json(e.to_string()))?;
    if json.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(json.len()));
    }
    let len = u32::try_from(json.len()).map_err(|_| FrameError::TooLarge(json.len()))?;
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

fn frame_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
    let len = usize::try_from(u32::from_be_bytes(prefix))
        .map_err(|_| FrameError::TooLarge(usize::MAX))?;
    if len == 0 {
        Err(FrameError::Empty)
    } else if len > MAX_FRAME {
        Err(FrameError::TooLarge(len))
    } else {
        Ok(len)
    }
}

/// Splits one frame off the front of `buf`: its payload and the bytes it used, or `None` when more bytes are
/// needed. An oversized length fails before any payload is buffered.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(&[u8], usize)>, FrameError> {
    let Some(prefix) = buf.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = frame_len(*prefix)?;
    Ok(buf.get(4..4 + len).map(|payload| (payload, 4 + len)))
}

/// Writes one frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let frame = encode_frame(value)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one frame's payload. The length is checked before the payload is read.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0u8; 4];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(eof_is_truncated)?;
    let len = frame_len(prefix)?;
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(eof_is_truncated)?;
    Ok(payload)
}

fn eof_is_truncated(err: std::io::Error) -> FrameError {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
        FrameError::Truncated
    } else {
        FrameError::Io(err)
    }
}

/// The calls one device makes on another (5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallKind {
    /// Version and features.
    Hello,
    /// A presence snapshot.
    Presence,
    /// One message envelope.
    Deliver,
    /// A later state of a message the peer sent.
    Receipt,
    /// Round-trip check.
    Ping,
}

impl CallKind {
    /// The `"t"` value of the call.
    pub fn as_str(self) -> &'static str {
        match self {
            CallKind::Hello => "hello",
            CallKind::Presence => "presence",
            CallKind::Deliver => "deliver",
            CallKind::Receipt => "receipt",
            CallKind::Ping => "ping",
        }
    }

    /// The call a `"t"` value names, if known.
    pub fn from_name(t: &str) -> Option<CallKind> {
        match t {
            "hello" => Some(CallKind::Hello),
            "presence" => Some(CallKind::Presence),
            "deliver" => Some(CallKind::Deliver),
            "receipt" => Some(CallKind::Receipt),
            "ping" => Some(CallKind::Ping),
            _ => None,
        }
    }
}

/// `hello`: version and features, first on every new link (5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Lowest version the sender speaks.
    pub proto_min: u32,
    /// Highest version the sender speaks.
    pub proto_max: u32,
    /// `spew-mesh/<version>`.
    pub app: String,
    /// The sender's device name as it calls itself; receivers use the roster's name.
    pub device_name: String,
    /// Optional features; none in v0.1.
    #[serde(default)]
    pub features: Vec<String>,
}

const MAX_FEATURES: usize = 32;
const MAX_APP: usize = 64;

impl Hello {
    /// This build's `hello`.
    pub fn ours(device_name: &str) -> Hello {
        Hello {
            proto_min: PROTO_MIN,
            proto_max: PROTO_MAX,
            app: app_version(),
            device_name: device_name.to_string(),
            features: Vec::new(),
        }
    }

    /// Checks the fields; a bad `hello` is a malformed call.
    pub fn validate(&self) -> Result<(), String> {
        if self.proto_min == 0 || self.proto_min > self.proto_max {
            return Err("proto_min and proto_max".into());
        }
        if self.app.is_empty()
            || self.app.len() > MAX_APP
            || !self.app.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err("app".into());
        }
        if !is_device_name(&self.device_name) {
            return Err("device_name".into());
        }
        if self.features.len() > MAX_FEATURES
            || !self.features.iter().all(|f| crate::address::is_slug(f, 32))
        {
            return Err("features".into());
        }
        Ok(())
    }

    /// The highest version both sides speak, if any.
    pub fn negotiate(&self, other: &Hello) -> Option<u32> {
        let low = self.proto_min.max(other.proto_min);
        let high = self.proto_max.min(other.proto_max);
        (low <= high).then_some(high)
    }
}

/// Why a message was refused (5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalCode {
    /// Over 120 `deliver` calls a minute from that device. The only code the sender retries.
    RateLimited,
    /// The envelope failed a shape or threading check.
    Invalid,
    /// No such session, or not visible to the sender.
    NoSuchSession,
    /// The target session ended.
    SessionEnded,
    /// The responder is full or over its run limits.
    Busy,
    /// The conversation guard tripped.
    ConversationLimit,
}

impl RefusalCode {
    /// The code as sent.
    pub fn as_str(self) -> &'static str {
        match self {
            RefusalCode::RateLimited => "rate-limited",
            RefusalCode::Invalid => "invalid",
            RefusalCode::NoSuchSession => "no-such-session",
            RefusalCode::SessionEnded => "session-ended",
            RefusalCode::Busy => "busy",
            RefusalCode::ConversationLimit => "conversation-limit",
        }
    }

    /// The code for its text, if known.
    pub fn from_name(s: &str) -> Option<RefusalCode> {
        [
            RefusalCode::RateLimited,
            RefusalCode::Invalid,
            RefusalCode::NoSuchSession,
            RefusalCode::SessionEnded,
            RefusalCode::Busy,
            RefusalCode::ConversationLimit,
        ]
        .into_iter()
        .find(|code| code.as_str() == s)
    }

    /// Whether the refusal ends the message; only `rate-limited` keeps it queued (5.5).
    pub fn is_final(self) -> bool {
        self != RefusalCode::RateLimited
    }
}

/// The state in a `deliver` call's receipt (5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliverState {
    /// Stored.
    Accepted,
    /// Already stored; the sender treats it as delivered.
    Duplicate,
    /// Not stored; see the code.
    Refused,
}

/// The answer to `deliver` (5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliverReceipt {
    /// The message id.
    pub id: String,
    /// What happened.
    pub state: DeliverState,
    /// Why, when refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<RefusalCode>,
}

impl DeliverReceipt {
    /// Checks that the id is a ULID and that a code comes with `refused` and only then.
    pub fn validate(&self) -> Result<(), String> {
        if !is_ulid(&self.id) {
            return Err("id".into());
        }
        if (self.state == DeliverState::Refused) != self.code.is_some() {
            return Err("code".into());
        }
        Ok(())
    }
}

/// A later state in a `receipt` call (5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptState {
    /// The target session was shown the message.
    Shown,
    /// The target session replied.
    Answered,
    /// It ran out of time on the receiving device.
    Expired,
    /// Refused after it was accepted, such as `session-ended`.
    Refused,
}

/// The `receipt` call: a later state of a message the peer sent (5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptUpdate {
    /// The message id.
    pub id: String,
    /// The new state.
    pub state: ReceiptState,
    /// Why, when refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<RefusalCode>,
    /// When, in the receiving device's milliseconds.
    pub at: u64,
}

impl ReceiptUpdate {
    /// Checks that the id is a ULID and that a code comes with `refused` and only then.
    pub fn validate(&self) -> Result<(), String> {
        if !is_ulid(&self.id) {
            return Err("id".into());
        }
        if (self.state == ReceiptState::Refused) != self.code.is_some() {
            return Err("code".into());
        }
        Ok(())
    }
}

/// A call as the caller sends it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Request {
    /// `hello`.
    Hello(Hello),
    /// `presence`.
    Presence(PresenceSnapshot),
    /// `deliver`.
    Deliver(Envelope),
    /// `receipt`.
    Receipt(ReceiptUpdate),
    /// `ping`.
    Ping,
}

impl Request {
    /// Which call this is.
    pub fn kind(&self) -> CallKind {
        match self {
            Request::Hello(_) => CallKind::Hello,
            Request::Presence(_) => CallKind::Presence,
            Request::Deliver(_) => CallKind::Deliver,
            Request::Receipt(_) => CallKind::Receipt,
            Request::Ping => CallKind::Ping,
        }
    }
}

/// A call as the callee reads it. `presence` and `deliver` bodies stay raw JSON so the rate checks can run before
/// they are parsed (5.5 step 2, 5.6); parse them with [`PresenceSnapshot::parse`] and [`Envelope::parse`].
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// `hello`, validated.
    Hello(Hello),
    /// `presence`, unparsed, without its `"t"`.
    Presence(Value),
    /// `deliver`, unparsed, without its `"t"`.
    Deliver(Value),
    /// `receipt`, validated.
    Receipt(ReceiptUpdate),
    /// `ping`.
    Ping,
}

impl Incoming {
    /// Which call this is.
    pub fn kind(&self) -> CallKind {
        match self {
            Incoming::Hello(_) => CallKind::Hello,
            Incoming::Presence(_) => CallKind::Presence,
            Incoming::Deliver(_) => CallKind::Deliver,
            Incoming::Receipt(_) => CallKind::Receipt,
            Incoming::Ping => CallKind::Ping,
        }
    }
}

/// Error codes in `{"t":"error"}` responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// The `"t"` names no call this device knows (5.4).
    UnknownCall,
    /// A call other than `hello` before the `hello` exchange (5.4).
    HelloRequired,
    /// The frame isn't a valid call. The spec names no code for this; `malformed` is ours.
    Malformed,
    /// The frame is over 128 KiB. The spec names no code for this; `too-large` is ours.
    TooLarge,
}

/// Why an incoming request couldn't be taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IncomingError {
    /// Not a JSON object with a string `"t"`, or a known call with bad fields.
    #[error("malformed call: {0}")]
    Malformed(String),
    /// A `"t"` this build doesn't know.
    #[error("unknown call `{0}`")]
    UnknownCall(String),
}

impl IncomingError {
    /// The error response to send back.
    pub fn response(&self) -> Response {
        match self {
            IncomingError::Malformed(_) => Response::Error {
                code: ErrorCode::Malformed,
            },
            IncomingError::UnknownCall(_) => Response::Error {
                code: ErrorCode::UnknownCall,
            },
        }
    }
}

impl From<&FrameError> for Response {
    fn from(err: &FrameError) -> Response {
        let code = match err {
            FrameError::TooLarge(_) => ErrorCode::TooLarge,
            _ => ErrorCode::Malformed,
        };
        Response::Error { code }
    }
}

/// Parses a request frame's payload (5.4).
pub fn parse_incoming(payload: &[u8]) -> Result<Incoming, IncomingError> {
    let value: Value =
        serde_json::from_slice(payload).map_err(|e| IncomingError::Malformed(e.to_string()))?;
    let Value::Object(mut map) = value else {
        return Err(IncomingError::Malformed("not a JSON object".into()));
    };
    let t = match map.remove("t") {
        Some(Value::String(t)) => t,
        _ => return Err(IncomingError::Malformed("no string \"t\"".into())),
    };
    let kind = CallKind::from_name(&t).ok_or(IncomingError::UnknownCall(t))?;
    let body = Value::Object(map);
    let malformed = |e: String| IncomingError::Malformed(format!("{}: {e}", kind.as_str()));
    match kind {
        CallKind::Hello => {
            let hello: Hello =
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?;
            hello.validate().map_err(malformed)?;
            Ok(Incoming::Hello(hello))
        }
        CallKind::Presence => Ok(Incoming::Presence(body)),
        CallKind::Deliver => Ok(Incoming::Deliver(body)),
        CallKind::Receipt => {
            let receipt: ReceiptUpdate =
                serde_json::from_value(body).map_err(|e| malformed(e.to_string()))?;
            receipt.validate().map_err(malformed)?;
            Ok(Incoming::Receipt(receipt))
        }
        CallKind::Ping => {
            if body.as_object().is_some_and(|m| !m.is_empty()) {
                return Err(malformed("unexpected fields".into()));
            }
            Ok(Incoming::Ping)
        }
    }
}

/// A response frame (5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Response {
    /// The callee's own `hello`.
    Hello(Hello),
    /// `{"t":"ok"}`.
    Ok,
    /// The answer to `deliver`.
    Receipt(DeliverReceipt),
    /// The answer to `ping`.
    Pong,
    /// The call failed.
    Error {
        /// Why.
        code: ErrorCode,
    },
}

impl Response {
    /// Parses and validates a response frame's payload.
    pub fn parse(payload: &[u8]) -> Result<Response, FrameError> {
        let response: Response =
            serde_json::from_slice(payload).map_err(|e| FrameError::Json(e.to_string()))?;
        match &response {
            Response::Hello(hello) => hello.validate().map_err(FrameError::Json)?,
            Response::Receipt(receipt) => receipt.validate().map_err(FrameError::Json)?,
            _ => {}
        }
        Ok(response)
    }

    /// Whether this is a fitting answer to `call`: its own response type, or an error.
    pub fn answers(&self, call: CallKind) -> bool {
        matches!(
            (self, call),
            (Response::Error { .. }, _)
                | (Response::Hello(_), CallKind::Hello)
                | (Response::Ok, CallKind::Presence | CallKind::Receipt)
                | (Response::Receipt(_), CallKind::Deliver)
                | (Response::Pong, CallKind::Ping)
        )
    }
}

/// A message kind. `notice` exists only on the receiving device (7.6); peers can't send it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    /// A question that expects an answer.
    Ask,
    /// A note that expects none.
    Tell,
    /// The answer to an ask.
    Answer,
    /// A local message from the daemon.
    Notice,
}

impl MessageKind {
    /// The kind as written.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Ask => "ask",
            MessageKind::Tell => "tell",
            MessageKind::Answer => "answer",
            MessageKind::Notice => "notice",
        }
    }

    /// The kind for its text, if known.
    pub fn from_name(s: &str) -> Option<MessageKind> {
        match s {
            "ask" => Some(MessageKind::Ask),
            "tell" => Some(MessageKind::Tell),
            "answer" => Some(MessageKind::Answer),
            "notice" => Some(MessageKind::Notice),
            _ => None,
        }
    }
}

/// The sending session, always one on the sending device (5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sender {
    /// Session id on the sending device.
    pub session: String,
    /// Session name.
    pub name: String,
    /// Client slug.
    pub client: String,
}

/// The target session on the receiving device (5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    /// Session id on the receiving device.
    pub session: String,
    /// Session name.
    pub name: String,
}

/// A message envelope (5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// ULID chosen by the sending device.
    pub id: String,
    /// Id of the conversation's first message.
    pub conversation: String,
    /// `ask`, `tell` or `answer`.
    pub kind: MessageKind,
    /// The message this one replies to.
    pub in_reply_to: Option<String>,
    /// The sending session.
    pub from: Sender,
    /// The target session.
    pub to: Recipient,
    /// The text.
    pub body: String,
    /// A card ref the message is about.
    pub card: Option<String>,
    /// The sender's repo.
    pub repo: Option<String>,
    /// Sender clock in milliseconds, for display only.
    pub sent_at: u64,
    /// Lifetime in seconds.
    pub ttl_secs: u64,
}

/// Why an envelope failed the shape check. Every case is refused with code `invalid`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShapeError {
    /// Not an envelope at all: wrong types, missing or unknown fields.
    #[error("not a message envelope: {0}")]
    Json(String),
    /// A field broke its rule.
    #[error("the envelope's {0} is invalid")]
    Field(&'static str),
}

impl ShapeError {
    /// The refusal code for every shape failure (5.5 step 3).
    pub fn code(&self) -> RefusalCode {
        RefusalCode::Invalid
    }
}

impl Envelope {
    /// Parses a `deliver` body and runs the shape check, which also sanitizes the body.
    pub fn parse(body: Value) -> Result<Envelope, ShapeError> {
        let mut envelope: Envelope =
            serde_json::from_value(body).map_err(|e| ShapeError::Json(e.to_string()))?;
        envelope.check_shape()?;
        Ok(envelope)
    }

    /// The shape check (5.5 step 3). Sanitizes the body in place (7.7) and then checks its size.
    pub fn check_shape(&mut self) -> Result<(), ShapeError> {
        let field = |ok: bool, name: &'static str| {
            if ok {
                Ok(())
            } else {
                Err(ShapeError::Field(name))
            }
        };
        field(is_ulid(&self.id), "id")?;
        field(is_ulid(&self.conversation), "conversation")?;
        field(
            self.in_reply_to.as_deref().is_none_or(is_ulid),
            "in_reply_to",
        )?;
        field(is_ulid(&self.from.session), "from.session")?;
        field(is_ulid(&self.to.session), "to.session")?;
        field(self.kind != MessageKind::Notice, "kind")?;
        field(
            is_session_name(&self.from.name) && self.from.name != RESERVED_NAME,
            "from.name",
        )?;
        field(is_client(&self.from.client), "from.client")?;
        field(is_session_name(&self.to.name), "to.name")?;
        field(self.card.as_deref().is_none_or(is_card), "card")?;
        field(self.repo.as_deref().is_none_or(is_repo), "repo")?;
        // The spec bounds ttl_secs for senders (7.4); a receiver holds peers to the same bound.
        field((1..=MAX_TTL_SECS).contains(&self.ttl_secs), "ttl_secs")?;
        self.body = sanitize_text(&self.body);
        field((1..=MAX_BODY).contains(&self.body.len()), "body")?;
        Ok(())
    }

    /// The part of the threading check (5.5 step 5) that needs no stored message: without `in_reply_to` the kind
    /// is `ask` or `tell` and the conversation is the message's own id; with it, it names another message.
    pub fn check_thread_root(&self) -> Result<(), ShapeError> {
        match &self.in_reply_to {
            None if self.kind == MessageKind::Answer => Err(ShapeError::Field("kind")),
            None if self.conversation != self.id => Err(ShapeError::Field("conversation")),
            Some(parent) if *parent == self.id => Err(ShapeError::Field("in_reply_to")),
            _ => Ok(()),
        }
    }
}

/// A session's kind (6.2). Presence carries only `live` and `responder`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    /// An agent session.
    Live,
    /// A serve responder.
    Responder,
    /// The CLI's terminal session; never in presence.
    Terminal,
}

/// Busy, idle or unknown (5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityState {
    /// Working on a turn.
    Busy,
    /// Waiting for its person.
    Idle,
    /// Not known.
    Unknown,
}

/// How a message reaches a session (5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    /// Hooks show it at the next step.
    Hooked,
    /// The agent must call the messages tool.
    Inbox,
    /// A responder answers it.
    Serve,
}

/// The sending device in a presence snapshot (5.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceDevice {
    /// Its name as it calls itself; receivers use the roster's name.
    pub name: String,
    /// `macos`, `linux` and so on.
    pub platform: String,
    /// The spew mesh version.
    pub version: String,
}

/// One session in a presence snapshot (5.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceSession {
    /// Session id.
    pub session: String,
    /// Session name.
    pub name: String,
    /// Client slug.
    pub client: String,
    /// `live` or `responder`.
    pub kind: SessionKind,
    /// `owner/repo` from the origin remote.
    #[serde(default)]
    pub repo: Option<String>,
    /// Git branch.
    #[serde(default)]
    pub branch: Option<String>,
    /// Last component of the working directory.
    #[serde(default)]
    pub place: Option<String>,
    /// The session's room.
    #[serde(default)]
    pub room: Option<String>,
    /// Cards held.
    #[serde(default)]
    pub cards: Vec<String>,
    /// One line of activity.
    #[serde(default)]
    pub activity: Option<String>,
    /// Busy, idle or unknown.
    pub state: ActivityState,
    /// Since when, in the sender's milliseconds.
    pub since: u64,
    /// How messages reach it.
    pub reach: Reach,
    /// A responder's waiting questions.
    #[serde(default)]
    pub queue: u32,
}

impl PresenceSession {
    /// Cleans a session from a peer (5.6): `None` when it can't be shown at all (a bad id, name or client, or a
    /// terminal session); otherwise optional fields that break their rules are dropped and text is cut to one
    /// printable line.
    pub fn sanitized(mut self) -> Option<PresenceSession> {
        if !is_ulid(&self.session)
            || !is_session_name(&self.name)
            || self.name == RESERVED_NAME
            || !is_client(&self.client)
            || self.kind == SessionKind::Terminal
        {
            return None;
        }
        self.repo = self.repo.filter(|r| is_repo(r));
        self.branch = self
            .branch
            .map(|b| sanitize_line(&b, MAX_BRANCH))
            .filter(|b| !b.is_empty());
        self.place = self
            .place
            .map(|p| sanitize_line(&p, MAX_PLACE))
            .filter(|p| !p.is_empty());
        self.room = self.room.filter(|r| is_room(r));
        let mut seen = HashSet::new();
        self.cards.retain(|c| is_card(c) && seen.insert(c.clone()));
        self.cards.truncate(MAX_PRESENCE_CARDS);
        self.activity = self
            .activity
            .map(|a| sanitize_line(&a, MAX_ACTIVITY))
            .filter(|a| !a.is_empty());
        Some(self)
    }
}

/// A device's presence snapshot, without the `"t"` that [`Request::Presence`] adds (5.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceSnapshot {
    /// Goes up on every change on the sending device.
    pub seq: u64,
    /// The sending device.
    pub device: PresenceDevice,
    /// Its live sessions, filtered for the receiving peer.
    pub sessions: Vec<PresenceSession>,
}

/// A presence body that isn't a snapshot at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a presence snapshot: {0}")]
pub struct PresenceError(pub String);

impl PresenceSnapshot {
    /// Parses and sanitizes a `presence` body from a peer (5.6). The snapshot fails only when `seq`, `device` or
    /// the session list is missing or mistyped; a bad session is dropped alone, as are sessions past the first
    /// 100 and repeated session ids or names.
    pub fn parse(mut body: Value) -> Result<PresenceSnapshot, PresenceError> {
        let map = body
            .as_object_mut()
            .ok_or_else(|| PresenceError("not a JSON object".into()))?;
        map.remove("t");
        let seq = map
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| PresenceError("seq".into()))?;
        let device: PresenceDevice = map
            .remove("device")
            .ok_or_else(|| PresenceError("device".into()))
            .and_then(|d| {
                serde_json::from_value(d).map_err(|e| PresenceError(format!("device: {e}")))
            })?;
        let Some(Value::Array(raw)) = map.remove("sessions") else {
            return Err(PresenceError("sessions".into()));
        };
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let sessions = raw
            .into_iter()
            .take(MAX_PRESENCE_SESSIONS)
            .filter_map(|s| serde_json::from_value::<PresenceSession>(s).ok())
            .filter_map(PresenceSession::sanitized)
            .filter(|s| ids.insert(s.session.clone()) && names.insert(s.name.clone()))
            .collect();
        let device = PresenceDevice {
            name: if is_device_name(&device.name) {
                device.name
            } else {
                String::new()
            },
            platform: sanitize_line(&device.platform, 16),
            version: sanitize_line(&device.version, 32),
        };
        Ok(PresenceSnapshot {
            seq,
            device,
            sessions,
        })
    }
}

/// A local session as presence filtering needs it: the snapshot entry with every field, plus where its room and
/// activity came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPresence {
    /// The full entry, as your own devices get it.
    pub session: PresenceSession,
    /// The room came from the repo's `.spew.json` (not `SPEW_PROJECT` or `[roster] default_room`).
    pub room_from_spew_json: bool,
    /// The project the spew `status` call that set the activity named, if any (8.5).
    pub activity_project: Option<String>,
}

/// Who a snapshot is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresencePeer<'a> {
    /// One of your own devices: every session, every field.
    Own,
    /// A teammate's device and the rooms it shares with you.
    Teammate {
        /// Rooms shared with that device's person.
        shared_rooms: &'a [String],
    },
}

/// Filters local sessions for one peer (5.6). Terminal sessions never go out. A teammate gets only sessions in a
/// room it shares, only that room's cards, activity only when it belongs to that room, and branch and place only
/// when `[presence]` allows them and the room came from `.spew.json`.
pub fn filter_for_peer(
    sessions: &[LocalPresence],
    peer: PresencePeer<'_>,
    send_branch: bool,
    send_place: bool,
) -> Vec<PresenceSession> {
    sessions
        .iter()
        .filter(|local| local.session.kind != SessionKind::Terminal)
        .filter_map(|local| match peer {
            PresencePeer::Own => Some(local.session.clone()),
            PresencePeer::Teammate { shared_rooms } => {
                let room = local.session.room.as_deref()?;
                if !shared_rooms.iter().any(|r| r == room) {
                    return None;
                }
                let full = &local.session;
                let card_key = |c: &String| c.split_once('#').map(|(key, _)| key.to_string());
                let cards_here = full
                    .cards
                    .iter()
                    .all(|c| card_key(c).as_deref() == Some(room));
                let activity_here = match local.activity_project.as_deref() {
                    Some(project) => project == room,
                    None => cards_here,
                };
                let mut out = full.clone();
                out.cards.retain(|c| card_key(c).as_deref() == Some(room));
                if !activity_here {
                    out.activity = None;
                }
                if !(send_branch && local.room_from_spew_json) {
                    out.branch = None;
                }
                if !(send_place && local.room_from_spew_json) {
                    out.place = None;
                }
                Some(out)
            }
        })
        .collect()
}

/// The repo slug from an `origin` remote URL (5.6): the last two path segments with `.git` removed, never the
/// host, port or user info. `None` when no valid slug remains.
pub fn repo_from_origin(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let path = if let Some(idx) = s.find("://") {
        let rest = &s[idx + 3..];
        rest.find('/').map_or("", |slash| &rest[slash..])
    } else {
        match s.find(':') {
            // scp-like `user@host:owner/repo`; a colon after a slash belongs to a local path.
            Some(colon) if !s[..colon].contains('/') => &s[colon + 1..],
            _ => s,
        }
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path
        .split('/')
        .filter(|seg| !matches!(*seg, "" | "." | ".."))
        .collect();
    let tail = &segments[segments.len().saturating_sub(2)..];
    let repo = tail.join("/");
    is_repo(&repo).then_some(repo)
}

/// The `place` for a working directory: its last component as one printable line of at most 64 characters, never
/// a whole path (5.6).
pub fn place_from_cwd(cwd: &std::path::Path) -> Option<String> {
    let name = cwd.file_name()?.to_string_lossy();
    let place = sanitize_line(&name, MAX_PLACE);
    (!place.is_empty()).then_some(place)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ulid(n: u128) -> String {
        ulid::Ulid::from_parts(1_730_000_000_000, n).to_string()
    }

    fn envelope() -> Envelope {
        Envelope {
            id: ulid(1),
            conversation: ulid(1),
            kind: MessageKind::Ask,
            in_reply_to: None,
            from: Sender {
                session: ulid(2),
                name: "claude-api".into(),
                client: "claude".into(),
            },
            to: Recipient {
                session: ulid(3),
                name: "codex-api".into(),
            },
            body: "Which table holds sessions?".into(),
            card: Some("spew#42".into()),
            repo: Some("acme/api".into()),
            sent_at: 1_730_000_000_000,
            ttl_secs: DEFAULT_TTL_SECS,
        }
    }

    fn presence_session(n: u128, name: &str) -> PresenceSession {
        PresenceSession {
            session: ulid(n),
            name: name.into(),
            client: "claude".into(),
            kind: SessionKind::Live,
            repo: Some("acme/api".into()),
            branch: Some("main".into()),
            place: Some("api".into()),
            room: Some("spew".into()),
            cards: vec!["spew#42".into()],
            activity: Some("fixing tests".into()),
            state: ActivityState::Busy,
            since: 1_730_000_000_000,
            reach: Reach::Hooked,
            queue: 0,
        }
    }

    #[test]
    fn frame_round_trip() {
        let value = json!({"t": "ping"});
        let frame = encode_frame(&value).unwrap();
        assert_eq!(&frame[..4], &(frame.len() as u32 - 4).to_be_bytes());
        let (payload, used) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(used, frame.len());
        assert_eq!(serde_json::from_slice::<Value>(payload).unwrap(), value);
        let mut two = frame.clone();
        two.extend_from_slice(&frame);
        let (_, used) = decode_frame(&two).unwrap().unwrap();
        assert_eq!(used, frame.len());
    }

    #[test]
    fn frame_truncated_and_oversized() {
        let frame = encode_frame(&json!({"t": "ping"})).unwrap();
        assert!(decode_frame(&frame[..3]).unwrap().is_none());
        assert!(decode_frame(&frame[..frame.len() - 1]).unwrap().is_none());
        let over = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(matches!(decode_frame(&over), Err(FrameError::TooLarge(n)) if n == MAX_FRAME + 1));
        assert!(matches!(
            decode_frame(&[0, 0, 0, 0]),
            Err(FrameError::Empty)
        ));
        let big = "x".repeat(MAX_FRAME);
        assert!(matches!(encode_frame(&big), Err(FrameError::TooLarge(_))));
        // A 32 KiB body fits even when escaping doubles it.
        let mut env = envelope();
        env.body = "\"".repeat(MAX_BODY);
        assert!(encode_frame(&Request::Deliver(env)).is_ok());
    }

    #[tokio::test]
    async fn frame_over_a_stream() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        write_frame(&mut a, &Request::Ping).await.unwrap();
        let payload = read_frame(&mut b).await.unwrap();
        assert_eq!(parse_incoming(&payload).unwrap(), Incoming::Ping);

        let frame = encode_frame(&Request::Ping).unwrap();
        a.write_all(&frame[..frame.len() - 2]).await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame(&mut b).await,
            Err(FrameError::Truncated)
        ));

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(matches!(
            read_frame(&mut b).await,
            Err(FrameError::TooLarge(_))
        ));

        let (a, mut b) = tokio::io::duplex(64);
        drop(a);
        assert!(matches!(
            read_frame(&mut b).await,
            Err(FrameError::Truncated)
        ));
    }

    #[test]
    fn requests_round_trip() {
        let hello = Hello::ours("sam-mbp");
        let snapshot = PresenceSnapshot {
            seq: 4,
            device: PresenceDevice {
                name: "sam-mbp".into(),
                platform: "macos".into(),
                version: "0.1.0".into(),
            },
            sessions: vec![presence_session(1, "claude-api")],
        };
        let receipt = ReceiptUpdate {
            id: ulid(9),
            state: ReceiptState::Refused,
            code: Some(RefusalCode::SessionEnded),
            at: 5,
        };
        let requests = [
            Request::Hello(hello.clone()),
            Request::Presence(snapshot.clone()),
            Request::Deliver(envelope()),
            Request::Receipt(receipt.clone()),
            Request::Ping,
        ];
        for request in requests {
            let bytes = serde_json::to_vec(&request).unwrap();
            let incoming = parse_incoming(&bytes).unwrap();
            assert_eq!(incoming.kind(), request.kind());
            match (request, incoming) {
                (Request::Hello(a), Incoming::Hello(b)) => assert_eq!(a, b),
                (Request::Presence(a), Incoming::Presence(b)) => {
                    assert_eq!(PresenceSnapshot::parse(b).unwrap(), a);
                }
                (Request::Deliver(a), Incoming::Deliver(b)) => {
                    assert_eq!(Envelope::parse(b).unwrap(), a)
                }
                (Request::Receipt(a), Incoming::Receipt(b)) => assert_eq!(a, b),
                (Request::Ping, Incoming::Ping) => {}
                other => panic!("mismatch {other:?}"),
            }
        }
        let wire = serde_json::to_value(Request::Deliver(envelope())).unwrap();
        assert_eq!(wire["t"], "deliver");
        assert_eq!(wire["in_reply_to"], Value::Null);
        assert_eq!(
            serde_json::to_value(Request::Ping).unwrap(),
            json!({"t": "ping"})
        );
    }

    #[test]
    fn unknown_and_malformed_calls() {
        assert_eq!(
            parse_incoming(br#"{"t":"teleport"}"#),
            Err(IncomingError::UnknownCall("teleport".into()))
        );
        assert_eq!(
            serde_json::to_value(IncomingError::UnknownCall("x".into()).response()).unwrap(),
            json!({"t": "error", "code": "unknown-call"})
        );
        for bad in [
            &b"not json"[..],
            br#"[1,2]"#,
            br#"{"x":1}"#,
            br#"{"t":5}"#,
            br#"{"t":"hello","proto_min":1}"#,
            br#"{"t":"hello","proto_min":2,"proto_max":1,"app":"spew-mesh/0.1.0","device_name":"sam-mbp"}"#,
            br#"{"t":"hello","proto_min":1,"proto_max":1,"app":"spew mesh","device_name":"sam-mbp"}"#,
            br#"{"t":"hello","proto_min":1,"proto_max":1,"app":"spew-mesh/0.1.0","device_name":"Sam Mbp"}"#,
            br#"{"t":"receipt","id":"nope","state":"shown","at":1}"#,
            br#"{"t":"receipt","id":"01J9ZKQ4V4M0Q2W4V4M0Q2W4V4","state":"refused","at":1}"#,
            br#"{"t":"receipt","id":"01J9ZKQ4V4M0Q2W4V4M0Q2W4V4","state":"shown","code":"busy","at":1}"#,
            br#"{"t":"receipt","id":"01J9ZKQ4V4M0Q2W4V4M0Q2W4V4","state":"accepted","at":1}"#,
            br#"{"t":"ping","extra":1}"#,
        ] {
            let result = parse_incoming(bad);
            assert!(matches!(result, Err(IncomingError::Malformed(_))), "{}", String::from_utf8_lossy(bad));
            assert_eq!(
                result.unwrap_err().response(),
                Response::Error {
                    code: ErrorCode::Malformed
                }
            );
        }
        assert_eq!(
            Response::from(&FrameError::TooLarge(1 << 20)),
            Response::Error {
                code: ErrorCode::TooLarge
            }
        );
    }

    #[test]
    fn responses() {
        let cases = [
            (json!({"t": "ok"}), Response::Ok),
            (json!({"t": "pong"}), Response::Pong),
            (
                json!({"t": "error", "code": "hello-required"}),
                Response::Error {
                    code: ErrorCode::HelloRequired,
                },
            ),
            (
                json!({"t": "receipt", "id": ulid(1), "state": "refused", "code": "rate-limited"}),
                Response::Receipt(DeliverReceipt {
                    id: ulid(1),
                    state: DeliverState::Refused,
                    code: Some(RefusalCode::RateLimited),
                }),
            ),
            (
                json!({"t": "receipt", "id": ulid(1), "state": "accepted"}),
                Response::Receipt(DeliverReceipt {
                    id: ulid(1),
                    state: DeliverState::Accepted,
                    code: None,
                }),
            ),
        ];
        for (value, expected) in cases {
            let bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(Response::parse(&bytes).unwrap(), expected);
            assert_eq!(serde_json::to_value(&expected).unwrap(), value);
        }
        for bad in [
            json!({"t": "receipt", "id": ulid(1), "state": "refused"}),
            json!({"t": "receipt", "id": ulid(1), "state": "shown"}),
            json!({"t": "receipt", "id": "x", "state": "accepted"}),
            json!({"t": "error", "code": "bogus"}),
            json!({"t": "what"}),
        ] {
            assert!(
                Response::parse(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "{bad}"
            );
        }
        assert!(Response::Ok.answers(CallKind::Presence));
        assert!(Response::Ok.answers(CallKind::Receipt));
        assert!(!Response::Ok.answers(CallKind::Deliver));
        assert!(Response::Pong.answers(CallKind::Ping));
        assert!(
            Response::Error {
                code: ErrorCode::Malformed
            }
            .answers(CallKind::Hello)
        );
    }

    #[test]
    fn hello_negotiation() {
        let ours = Hello::ours("sam-mbp");
        assert!(ours.validate().is_ok());
        assert!(ours.app.starts_with("spew-mesh/"));
        let newer = Hello {
            proto_min: 1,
            proto_max: 3,
            ..ours.clone()
        };
        assert_eq!(ours.negotiate(&newer), Some(1));
        let future = Hello {
            proto_min: 2,
            proto_max: 3,
            ..ours.clone()
        };
        assert_eq!(ours.negotiate(&future), None);
        assert_eq!(future.negotiate(&newer), Some(3));
    }

    #[test]
    fn close_codes() {
        for code in [
            CloseCode::NotAdmitted,
            CloseCode::Duplicate,
            CloseCode::Version,
            CloseCode::Shutdown,
        ] {
            assert_eq!(CloseCode::from_code(u64::from(code.code())), Some(code));
        }
        assert_eq!(CloseCode::NotAdmitted.code(), 1);
        assert_eq!(CloseCode::Shutdown.reason(), "shutdown");
        assert_eq!(CloseCode::from_code(9), None);
    }

    #[test]
    fn refusal_codes() {
        for code in [
            RefusalCode::RateLimited,
            RefusalCode::Invalid,
            RefusalCode::NoSuchSession,
            RefusalCode::SessionEnded,
            RefusalCode::Busy,
            RefusalCode::ConversationLimit,
        ] {
            assert_eq!(RefusalCode::from_name(code.as_str()), Some(code));
            assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
            assert_eq!(code.is_final(), code != RefusalCode::RateLimited);
        }
        assert_eq!(RefusalCode::from_name("nope"), None);
    }

    type Mutation = Box<dyn FnOnce(&mut Value)>;

    fn shape(mutate: impl FnOnce(&mut Value)) -> Result<Envelope, ShapeError> {
        let mut value = serde_json::to_value(envelope()).unwrap();
        mutate(&mut value);
        Envelope::parse(value)
    }

    #[test]
    fn envelope_shape_accepts_valid() {
        let env = shape(|_| {}).unwrap();
        assert_eq!(env, envelope());
        let env = shape(|v| {
            v["card"] = Value::Null;
            v["repo"] = Value::Null;
            v.as_object_mut().unwrap().remove("in_reply_to");
        })
        .unwrap();
        assert_eq!(env.card, None);
        assert_eq!(env.in_reply_to, None);
    }

    #[test]
    fn envelope_shape_rejects_each_rule() {
        let long = "a".repeat(49);
        let cases: Vec<(&str, Mutation)> = vec![
            ("id", Box::new(|v| v["id"] = json!("not-a-ulid"))),
            (
                "id lowercase",
                Box::new(|v| v["id"] = json!(ulid(1).to_lowercase())),
            ),
            ("conversation", Box::new(|v| v["conversation"] = json!("x"))),
            ("in_reply_to", Box::new(|v| v["in_reply_to"] = json!("x"))),
            (
                "from.session",
                Box::new(|v| v["from"]["session"] = json!("x")),
            ),
            ("to.session", Box::new(|v| v["to"]["session"] = json!(""))),
            ("kind notice", Box::new(|v| v["kind"] = json!("notice"))),
            ("kind other", Box::new(|v| v["kind"] = json!("shout"))),
            (
                "from.name mesh",
                Box::new(|v| v["from"]["name"] = json!("mesh")),
            ),
            (
                "from.name long",
                Box::new(move |v| v["from"]["name"] = json!(long)),
            ),
            (
                "from.name chars",
                Box::new(|v| v["from"]["name"] = json!("claude api")),
            ),
            (
                "from.client",
                Box::new(|v| v["from"]["client"] = json!("a-very-long-client-x")),
            ),
            ("to.name", Box::new(|v| v["to"]["name"] = json!("Codex"))),
            ("card", Box::new(|v| v["card"] = json!("spew-42"))),
            (
                "card long",
                Box::new(|v| v["card"] = json!(format!("{}#1", "a".repeat(39)))),
            ),
            ("repo chars", Box::new(|v| v["repo"] = json!("acme api"))),
            (
                "repo long",
                Box::new(|v| v["repo"] = json!("a".repeat(101))),
            ),
            ("ttl zero", Box::new(|v| v["ttl_secs"] = json!(0))),
            (
                "ttl long",
                Box::new(|v| v["ttl_secs"] = json!(MAX_TTL_SECS + 1)),
            ),
            ("body empty", Box::new(|v| v["body"] = json!(""))),
            (
                "body only controls",
                Box::new(|v| v["body"] = json!("\u{1b}\u{200B}\u{7}")),
            ),
            (
                "body long",
                Box::new(|v| v["body"] = json!("x".repeat(MAX_BODY + 1))),
            ),
            ("unknown field", Box::new(|v| v["room"] = json!("spew"))),
            (
                "unknown nested",
                Box::new(|v| v["from"]["person"] = json!("maya")),
            ),
            (
                "missing field",
                Box::new(|v| drop(v.as_object_mut().unwrap().remove("body"))),
            ),
            ("wrong type", Box::new(|v| v["sent_at"] = json!("now"))),
        ];
        for (label, mutate) in cases {
            let err = shape(mutate).unwrap_err();
            assert_eq!(err.code(), RefusalCode::Invalid, "{label}");
        }
    }

    #[test]
    fn envelope_body_is_sanitized_before_measuring() {
        let env = shape(|v| v["body"] = json!("hi\r\nthere\u{202E}\u{1b}[2J")).unwrap();
        assert_eq!(env.body, "hi\nthere[2J");
        let padded = format!("{}{}", "x".repeat(MAX_BODY), "\u{200B}".repeat(100));
        assert_eq!(
            shape(|v| v["body"] = json!(padded)).unwrap().body.len(),
            MAX_BODY
        );
    }

    #[test]
    fn thread_root_rules() {
        let root = envelope();
        assert!(root.check_thread_root().is_ok());
        let mut answer_root = envelope();
        answer_root.kind = MessageKind::Answer;
        assert!(answer_root.check_thread_root().is_err());
        let mut wrong_conv = envelope();
        wrong_conv.conversation = ulid(7);
        assert!(wrong_conv.check_thread_root().is_err());
        let mut reply = envelope();
        reply.id = ulid(5);
        reply.kind = MessageKind::Answer;
        reply.in_reply_to = Some(ulid(1));
        assert!(reply.check_thread_root().is_ok());
        reply.in_reply_to = Some(ulid(5));
        assert!(reply.check_thread_root().is_err());
    }

    #[test]
    fn presence_parse_sanitizes() {
        let good = serde_json::to_value(presence_session(1, "claude-api")).unwrap();
        let mut messy = serde_json::to_value(presence_session(2, "codex-api")).unwrap();
        messy["activity"] = json!(format!("line one\nline two\u{1b}[31m {}", "x".repeat(300)));
        messy["branch"] = json!(format!("feat/\u{202E}x{}", "b".repeat(200)));
        messy["place"] = json!("p".repeat(100));
        messy["repo"] = json!("https://host/acme/api");
        messy["room"] = json!("bad room");
        messy["cards"] = json!(["spew#42", "spew#42", "bad", "other#1"]);
        messy["extra"] = json!("ignored");
        let terminal = {
            let mut t = serde_json::to_value(presence_session(3, "terminal")).unwrap();
            t["kind"] = json!("terminal");
            t
        };
        let mesh_name = serde_json::to_value(presence_session(4, "mesh")).unwrap();
        let bad_id = {
            let mut t = serde_json::to_value(presence_session(5, "x")).unwrap();
            t["session"] = json!("nope");
            t
        };
        let wrong_type = json!({"session": ulid(6), "name": 5});
        let dup_id = serde_json::to_value(presence_session(1, "other-name")).unwrap();
        let dup_name = serde_json::to_value(presence_session(8, "claude-api")).unwrap();
        let body = json!({
            "t": "presence",
            "seq": 42,
            "device": {"name": "maya-mbp", "platform": "macos\n", "version": "0.1.0"},
            "sessions": [good, messy, terminal, mesh_name, bad_id, wrong_type, dup_id, dup_name],
        });
        let snap = PresenceSnapshot::parse(body).unwrap();
        assert_eq!(snap.seq, 42);
        assert_eq!(snap.device.platform, "macos");
        let names: Vec<&str> = snap.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["claude-api", "codex-api"]);
        let messy = &snap.sessions[1];
        let activity = messy.activity.as_deref().unwrap();
        assert_eq!(activity.chars().count(), MAX_ACTIVITY);
        assert!(activity.starts_with("line one line two[31m x"));
        assert_eq!(messy.branch.as_deref().unwrap().chars().count(), MAX_BRANCH);
        assert!(!messy.branch.as_deref().unwrap().contains('\u{202E}'));
        assert_eq!(messy.place.as_deref().unwrap().len(), MAX_PLACE);
        assert_eq!(messy.repo, None);
        assert_eq!(messy.room, None);
        assert_eq!(messy.cards, ["spew#42", "other#1"]);
    }

    #[test]
    fn presence_limits_and_failures() {
        let many: Vec<Value> = (1..=150)
            .map(|n| serde_json::to_value(presence_session(n, &format!("s-{n}"))).unwrap())
            .collect();
        let body = json!({"seq": 1, "device": {"name": "x", "platform": "linux", "version": "0.1.0"}, "sessions": many});
        assert_eq!(
            PresenceSnapshot::parse(body).unwrap().sessions.len(),
            MAX_PRESENCE_SESSIONS
        );
        for bad in [
            json!([]),
            json!({"device": {"name": "x", "platform": "linux", "version": "1"}, "sessions": []}),
            json!({"seq": -1, "device": {"name": "x", "platform": "linux", "version": "1"}, "sessions": []}),
            json!({"seq": 1, "sessions": []}),
            json!({"seq": 1, "device": {"name": "x", "platform": "linux", "version": "1"}, "sessions": {}}),
        ] {
            assert!(PresenceSnapshot::parse(bad.clone()).is_err(), "{bad}");
        }
        let body = json!({"seq": 1, "device": {"name": "Bad Name", "platform": "linux", "version": "1"}, "sessions": []});
        assert_eq!(PresenceSnapshot::parse(body).unwrap().device.name, "");
    }

    fn local(
        n: u128,
        room: Option<&str>,
        cards: &[&str],
        from_json: bool,
        project: Option<&str>,
    ) -> LocalPresence {
        let mut session = presence_session(n, &format!("s-{n}"));
        session.room = room.map(String::from);
        session.cards = cards.iter().map(|c| c.to_string()).collect();
        LocalPresence {
            session,
            room_from_spew_json: from_json,
            activity_project: project.map(String::from),
        }
    }

    #[test]
    fn presence_filter_for_own_devices() {
        let mut terminal = local(9, None, &[], false, None);
        terminal.session.kind = SessionKind::Terminal;
        let sessions = [local(1, None, &["web#1"], false, Some("web")), terminal];
        let out = filter_for_peer(&sessions, PresencePeer::Own, false, false);
        assert_eq!(out, vec![sessions[0].session.clone()]);
    }

    #[test]
    fn presence_filter_for_teammates() {
        let rooms = vec!["spew".to_string()];
        let peer = PresencePeer::Teammate {
            shared_rooms: &rooms,
        };
        let sessions = [
            // In the shared room, room from .spew.json, activity named the room.
            local(1, Some("spew"), &["spew#42", "web#7"], true, Some("spew")),
            // Activity named another project.
            local(2, Some("spew"), &["spew#1"], true, Some("web")),
            // Activity named none while holding another project's card.
            local(3, Some("spew"), &["web#2"], false, None),
            // Activity named none, only this room's cards.
            local(4, Some("spew"), &["spew#3"], false, None),
            // Another room, and no room.
            local(5, Some("web"), &[], true, None),
            local(6, None, &[], true, None),
        ];
        let out = filter_for_peer(&sessions, peer, true, true);
        let names: Vec<&str> = out.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["s-1", "s-2", "s-3", "s-4"]);
        assert_eq!(out[0].cards, ["spew#42"]);
        assert!(out[0].activity.is_some());
        assert_eq!(out[0].branch.as_deref(), Some("main"));
        assert_eq!(out[0].place.as_deref(), Some("api"));
        assert_eq!(out[1].activity, None);
        assert_eq!(out[2].activity, None);
        assert!(out[2].cards.is_empty());
        assert!(out[3].activity.is_some());
        // A room from default_room or SPEW_PROJECT never sends branch or place.
        assert_eq!(out[3].branch, None);
        assert_eq!(out[3].place, None);
        // [presence] branch and place off.
        let out = filter_for_peer(&sessions[..1], peer, false, true);
        assert_eq!(out[0].branch, None);
        assert_eq!(out[0].place.as_deref(), Some("api"));
        let out = filter_for_peer(&sessions[..1], peer, true, false);
        assert_eq!(out[0].place, None);
        let none = PresencePeer::Teammate { shared_rooms: &[] };
        assert!(filter_for_peer(&sessions, none, true, true).is_empty());
    }

    #[test]
    fn repo_slugs() {
        let cases = [
            ("https://github.com/acme/api.git", Some("acme/api")),
            ("https://github.com/acme/api", Some("acme/api")),
            ("https://github.com/acme/api/", Some("acme/api")),
            (
                "https://sam:secret-token@example.com:8443/acme/api.git",
                Some("acme/api"),
            ),
            (
                "ssh://git@example.com:2222/group/sub/api.git",
                Some("sub/api"),
            ),
            ("git@example.com:acme/api.git", Some("acme/api")),
            ("example.com:acme/api", Some("acme/api")),
            ("/srv/git/acme/api.git", Some("acme/api")),
            ("file:///srv/git/acme/api.git", Some("acme/api")),
            ("../api", Some("api")),
            (
                "https://example.com/acme/api.git?ref=x#frag",
                Some("acme/api"),
            ),
            ("https://example.com/single.git", Some("single")),
            ("https://example.com/", None),
            ("https://example.com/acme/a%20b", None),
            ("", None),
        ];
        for (url, want) in cases {
            assert_eq!(repo_from_origin(url).as_deref(), want, "{url}");
        }
    }

    #[test]
    fn place_is_the_last_component() {
        use std::path::Path;
        assert_eq!(
            place_from_cwd(Path::new("/home/sam/code/api")).as_deref(),
            Some("api")
        );
        assert_eq!(place_from_cwd(Path::new("/")), None);
        assert_eq!(
            place_from_cwd(Path::new("/x/a\u{1b}b")).as_deref(),
            Some("ab")
        );
        assert_eq!(
            place_from_cwd(Path::new(&format!("/x/{}", "p".repeat(90))))
                .unwrap()
                .len(),
            MAX_PLACE
        );
    }
}
