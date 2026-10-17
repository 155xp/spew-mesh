//! The SQLite store and its migrations (11.1).
//!
//! One file, `store.sqlite3`, in WAL mode, migrated with `PRAGMA user_version` steps at open. It holds messages in
//! both directions with their state history, outbox entries, the session registry and its cards, the serve
//! conversation map, the remote presence cache, the roster cache and dedupe keys.
//!
//! A `Store` wraps one connection and is not `Sync`; the daemon owns it behind a lock or on one task.

use std::path::Path;
use std::time::Duration;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths::{FILE_MODE, PathsError, mode_of, private_open_options, set_mode};
use crate::wire::{ActivityState, MessageKind, Reach, SessionKind};

/// How long messages and receipts are kept, ended sessions after they end, and serve conversations after their
/// last use (11.1).
pub const MESSAGE_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// How long dedupe keys are kept (11.1).
pub const DEDUPE_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

const V1: &str = r#"
CREATE TABLE messages (
    seq              INTEGER PRIMARY KEY AUTOINCREMENT,
    id               TEXT NOT NULL,
    from_device      TEXT NOT NULL,
    from_device_name TEXT NOT NULL,
    from_person      TEXT NOT NULL,
    from_session     TEXT NOT NULL,
    from_name        TEXT NOT NULL,
    from_client      TEXT NOT NULL,
    to_device        TEXT NOT NULL,
    to_device_name   TEXT NOT NULL,
    to_person        TEXT NOT NULL,
    to_session       TEXT NOT NULL,
    to_name          TEXT NOT NULL,
    conversation     TEXT NOT NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('ask', 'tell', 'answer', 'notice')),
    in_reply_to      TEXT,
    body             TEXT NOT NULL,
    card             TEXT,
    repo             TEXT,
    room             TEXT,
    sent_at          INTEGER NOT NULL,
    ttl_secs         INTEGER NOT NULL,
    state            TEXT NOT NULL
                     CHECK (state IN ('queued', 'delivered', 'shown', 'answered', 'refused', 'expired')),
    code             TEXT,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (from_device, id)
);
CREATE INDEX messages_to_session ON messages (to_session, state);
CREATE INDEX messages_from_session ON messages (from_session, state);
CREATE INDEX messages_conversation ON messages (conversation);
CREATE INDEX messages_created ON messages (created_at);

CREATE TABLE message_states (
    message_seq INTEGER NOT NULL REFERENCES messages (seq) ON DELETE CASCADE,
    state       TEXT NOT NULL,
    code        TEXT,
    at          INTEGER NOT NULL
);
CREATE INDEX message_states_seq ON message_states (message_seq);

CREATE TABLE dedupe (
    device TEXT NOT NULL,
    id     TEXT NOT NULL,
    at     INTEGER NOT NULL,
    PRIMARY KEY (device, id)
) WITHOUT ROWID;

CREATE TABLE outbox (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    device          TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('deliver', 'receipt')),
    message_seq     INTEGER REFERENCES messages (seq) ON DELETE CASCADE,
    payload         TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    created_at      INTEGER NOT NULL
);
CREATE INDEX outbox_device ON outbox (device, next_attempt_at);

CREATE TABLE sessions (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    client           TEXT NOT NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('live', 'responder', 'terminal')),
    client_session   TEXT,
    client_pid       INTEGER,
    client_pid_start INTEGER,
    token_hash       TEXT NOT NULL,
    cwd              TEXT,
    repo             TEXT,
    branch           TEXT,
    place            TEXT,
    room             TEXT,
    room_source      TEXT,
    reach            TEXT NOT NULL CHECK (reach IN ('hooked', 'inbox', 'serve')),
    state            TEXT NOT NULL CHECK (state IN ('busy', 'idle', 'unknown')),
    state_since      INTEGER NOT NULL,
    activity         TEXT,
    activity_project TEXT,
    opened_at        INTEGER NOT NULL,
    last_seen_at     INTEGER NOT NULL,
    ended_at         INTEGER
);
CREATE UNIQUE INDEX sessions_live_name ON sessions (name) WHERE ended_at IS NULL;
CREATE INDEX sessions_client ON sessions (client, client_session);
CREATE INDEX sessions_token ON sessions (token_hash);

CREATE TABLE session_cards (
    session_id   TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    card         TEXT NOT NULL,
    project      TEXT NOT NULL,
    refreshed_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, card)
) WITHOUT ROWID;

CREATE TABLE serve_conversations (
    client        TEXT NOT NULL,
    asker_device  TEXT NOT NULL,
    asker_session TEXT NOT NULL,
    conversation  TEXT NOT NULL,
    thread        TEXT NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (client, asker_device, asker_session, conversation)
) WITHOUT ROWID;

CREATE TABLE presence_cache (
    device      TEXT PRIMARY KEY,
    seq         INTEGER NOT NULL,
    snapshot    TEXT NOT NULL,
    received_at INTEGER NOT NULL
);

CREATE TABLE roster_cache (
    provider   TEXT PRIMARY KEY CHECK (provider IN ('local', 'file', 'spew')),
    version    TEXT,
    etag       TEXT,
    roster     TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);
"#;

// Whether a session was ever reached through a control connection, so rule 3 still applies to it after a
// daemon restart (6.4).
const V2: &str = r#"
ALTER TABLE sessions ADD COLUMN had_connection INTEGER NOT NULL DEFAULT 0;
"#;

/// The migration steps; step `n` takes `user_version` from `n` to `n + 1`. Steps are only ever appended.
const MIGRATIONS: &[&str] = &[V1, V2];

/// The schema version this build writes.
pub fn schema_version() -> u32 {
    u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX)
}

/// A store operation that failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Creating or securing the file failed.
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// SQLite failed.
    #[error("store: could not {what}: {error}")]
    Sql {
        /// What was being done.
        what: &'static str,
        /// The SQLite error.
        error: rusqlite::Error,
    },
    /// The file was written by a newer spew mesh.
    #[error(
        "the store is at schema version {found}, newer than this spew mesh knows ({known}); update spew mesh"
    )]
    TooNew {
        /// The file's version.
        found: i64,
        /// This build's version.
        known: u32,
    },
    /// WAL mode couldn't be turned on.
    #[error("the store could not use WAL mode (journal mode is {0})")]
    NotWal(String),
}

impl StoreError {
    /// Whether a constraint failed, such as a second live session with the same name.
    pub fn is_constraint(&self) -> bool {
        matches!(self, StoreError::Sql { error, .. } if is_constraint(error))
    }
}

trait Context<T> {
    fn ctx(self, what: &'static str) -> Result<T, StoreError>;
}

impl<T> Context<T> for rusqlite::Result<T> {
    fn ctx(self, what: &'static str) -> Result<T, StoreError> {
        self.map_err(|error| StoreError::Sql { what, error })
    }
}

/// The state of a message (7.1). On the receiving device a stored message is `delivered` until shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageState {
    /// In the outbox.
    Queued,
    /// The target device stored it.
    Delivered,
    /// The target session was shown it.
    Shown,
    /// The target session replied.
    Answered,
    /// Refused, with a code.
    Refused,
    /// Ran out of time.
    Expired,
}

impl MessageState {
    /// Whether nothing follows this state.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            MessageState::Answered | MessageState::Refused | MessageState::Expired
        )
    }

    /// Whether a message may move from this state to `next` (7.1). Receipts can arrive out of order, so a message
    /// may skip ahead (`queued` to `shown`), but it never moves back or out of a final state.
    pub fn can_become(self, next: MessageState) -> bool {
        use MessageState::*;
        match self {
            Queued => matches!(next, Delivered | Shown | Answered | Refused | Expired),
            Delivered => matches!(next, Shown | Answered | Refused | Expired),
            Shown => next == Answered,
            Answered | Refused | Expired => false,
        }
    }
}

/// What an outbox entry sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxKind {
    /// A `deliver` call.
    Deliver,
    /// A `receipt` call.
    Receipt,
}

/// The roster provider a cached roster came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RosterProvider {
    /// The local roster.
    Local,
    /// A roster file.
    File,
    /// spew.
    Spew,
}

// Every enum stored as text uses its serde name, so the store and the wire spell values the same way.
macro_rules! sql_text_enum {
    ($($ty:ty),* $(,)?) => {$(
        impl ToSql for $ty {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                match serde_json::to_value(self) {
                    Ok(serde_json::Value::String(s)) => Ok(ToSqlOutput::from(s)),
                    _ => Err(rusqlite::Error::ToSqlConversionFailure(
                        format!("{} is not stored as text", stringify!($ty)).into(),
                    )),
                }
            }
        }

        impl FromSql for $ty {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                serde_json::from_value(serde_json::Value::String(text.to_string()))
                    .map_err(|e| FromSqlError::Other(Box::new(e)))
            }
        }
    )*};
}

sql_text_enum!(
    MessageState,
    OutboxKind,
    RosterProvider,
    MessageKind,
    SessionKind,
    ActivityState,
    Reach
);

// Times are milliseconds and lifetimes seconds; both fit i64 by far. A larger value from a peer clock is clamped.
fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn uint(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// One end of a message: device, person and session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Party {
    /// Endpoint id of the device, 64 hex characters.
    pub device: String,
    /// The device's name when the message was stored.
    pub device_name: String,
    /// The device's person.
    pub person: String,
    /// Session id on that device.
    pub session: String,
    /// Session name.
    pub name: String,
}

/// A message in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Id chosen by the sending device.
    pub id: String,
    /// Conversation id.
    pub conversation: String,
    /// Kind, including `notice` for local notices.
    pub kind: MessageKind,
    /// The message this one replies to.
    pub in_reply_to: Option<String>,
    /// The sender.
    pub from: Party,
    /// The sender's client.
    pub from_client: String,
    /// The target.
    pub to: Party,
    /// The sanitized body.
    pub body: String,
    /// Card ref.
    pub card: Option<String>,
    /// Sender's repo.
    pub repo: Option<String>,
    /// The room the receiver assigned (5.5).
    pub room: Option<String>,
    /// Sender clock, milliseconds.
    pub sent_at: u64,
    /// Lifetime, seconds.
    pub ttl_secs: u64,
    /// Current state.
    pub state: MessageState,
    /// Refusal or failure code.
    pub code: Option<String>,
}

/// A stored message with its row number and times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// Local row number, in arrival order.
    pub seq: i64,
    /// The message.
    pub message: Message,
    /// When it was stored here.
    pub created_at: u64,
    /// When its state last changed.
    pub updated_at: u64,
}

/// One recorded state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateChange {
    /// The new state.
    pub state: MessageState,
    /// Its code.
    pub code: Option<String>,
    /// When, milliseconds.
    pub at: u64,
}

/// What storing a peer's message did (5.5 steps 4 and 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// Stored, with its row number.
    Stored(i64),
    /// (peer device, id) was already stored.
    Duplicate,
}

/// An outbox entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// Row number, in queue order.
    pub seq: i64,
    /// Target endpoint id.
    pub device: String,
    /// What it sends.
    pub kind: OutboxKind,
    /// The message it is about, for `deliver`.
    pub message_seq: Option<i64>,
    /// The request JSON to send.
    pub payload: String,
    /// Attempts so far.
    pub attempts: u32,
    /// When to try next, milliseconds.
    pub next_attempt_at: u64,
    /// When it gives up, milliseconds.
    pub expires_at: u64,
    /// When it was queued, milliseconds.
    pub created_at: u64,
}

/// An outbox entry to queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewOutbox<'a> {
    /// Target endpoint id.
    pub device: &'a str,
    /// What it sends.
    pub kind: OutboxKind,
    /// The message it is about, for `deliver`; the entry goes when the message does.
    pub message_seq: Option<i64>,
    /// The request JSON to send.
    pub payload: &'a str,
    /// When to try first, milliseconds.
    pub next_attempt_at: u64,
    /// When to give up, milliseconds.
    pub expires_at: u64,
}

/// A row of the session registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Session id (ULID).
    pub id: String,
    /// Name, unique among live sessions.
    pub name: String,
    /// Client slug.
    pub client: String,
    /// Live, responder or terminal.
    pub kind: SessionKind,
    /// The client's own session or thread id.
    pub client_session: Option<String>,
    /// The client's process id.
    pub client_pid: Option<u32>,
    /// That process's start time, to tell a reused pid apart (6.4).
    pub client_pid_start: Option<u64>,
    /// SHA-256 of the session token ([`hash_token`]); never the token itself.
    pub token_hash: String,
    /// Working directory; local only, never sent.
    pub cwd: Option<String>,
    /// Repo slug.
    pub repo: Option<String>,
    /// Branch.
    pub branch: Option<String>,
    /// Place.
    pub place: Option<String>,
    /// Room.
    pub room: Option<String>,
    /// Where the room came from: `env`, `spew-json` or `default`.
    pub room_source: Option<String>,
    /// How messages reach it.
    pub reach: Reach,
    /// Busy, idle or unknown.
    pub state: ActivityState,
    /// Since when, milliseconds.
    pub state_since: u64,
    /// Activity line.
    pub activity: Option<String>,
    /// Project the activity's status call named.
    pub activity_project: Option<String>,
    /// When it opened, milliseconds.
    pub opened_at: u64,
    /// Last hook event or attach, milliseconds.
    pub last_seen_at: u64,
    /// When it ended, milliseconds.
    pub ended_at: Option<u64>,
}

/// A card a session holds (8.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldCard {
    /// The card ref.
    pub card: String,
    /// Its project key.
    pub project: String,
    /// Last refresh, milliseconds.
    pub refreshed_at: u64,
}

/// A cached presence snapshot from a peer device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPresence {
    /// The peer's endpoint id.
    pub device: String,
    /// The snapshot's `seq`.
    pub seq: u64,
    /// The sanitized snapshot as JSON.
    pub snapshot: String,
    /// When it arrived, milliseconds.
    pub received_at: u64,
}

/// A cached roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedRoster {
    /// Where it came from.
    pub provider: RosterProvider,
    /// The roster's version.
    pub version: Option<String>,
    /// The HTTP ETag, for spew.
    pub etag: Option<String>,
    /// The roster as JSON.
    pub roster: String,
    /// When it was fetched, milliseconds.
    pub fetched_at: u64,
}

/// What a sweep deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Swept {
    /// Messages, with their history and outbox entries.
    pub messages: usize,
    /// Receipt outbox entries.
    pub receipts: usize,
    /// Dedupe keys.
    pub dedupe: usize,
    /// Sessions that ended, with the cards they held.
    pub sessions: usize,
    /// Serve conversations.
    pub serve_conversations: usize,
}

/// SHA-256 of a session token as lowercase hex, the only form of a token the store keeps.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The SQLite store.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

const MESSAGE_COLUMNS: &str = "seq, id, conversation, kind, in_reply_to, \
     from_device, from_device_name, from_person, from_session, from_name, from_client, \
     to_device, to_device_name, to_person, to_session, to_name, \
     body, card, repo, room, sent_at, ttl_secs, state, code, created_at, updated_at";

fn message_row(row: &Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        seq: row.get(0)?,
        message: Message {
            id: row.get(1)?,
            conversation: row.get(2)?,
            kind: row.get(3)?,
            in_reply_to: row.get(4)?,
            from: Party {
                device: row.get(5)?,
                device_name: row.get(6)?,
                person: row.get(7)?,
                session: row.get(8)?,
                name: row.get(9)?,
            },
            from_client: row.get(10)?,
            to: Party {
                device: row.get(11)?,
                device_name: row.get(12)?,
                person: row.get(13)?,
                session: row.get(14)?,
                name: row.get(15)?,
            },
            body: row.get(16)?,
            card: row.get(17)?,
            repo: row.get(18)?,
            room: row.get(19)?,
            sent_at: uint(row.get(20)?),
            ttl_secs: uint(row.get(21)?),
            state: row.get(22)?,
            code: row.get(23)?,
        },
        created_at: uint(row.get(24)?),
        updated_at: uint(row.get(25)?),
    })
}

const OUTBOX_COLUMNS: &str =
    "seq, device, kind, message_seq, payload, attempts, next_attempt_at, expires_at, created_at";

fn outbox_row(row: &Row<'_>) -> rusqlite::Result<OutboxEntry> {
    Ok(OutboxEntry {
        seq: row.get(0)?,
        device: row.get(1)?,
        kind: row.get(2)?,
        message_seq: row.get(3)?,
        payload: row.get(4)?,
        attempts: row.get(5)?,
        next_attempt_at: uint(row.get(6)?),
        expires_at: uint(row.get(7)?),
        created_at: uint(row.get(8)?),
    })
}

const SESSION_COLUMNS: &str = "id, name, client, kind, client_session, client_pid, client_pid_start, token_hash, \
     cwd, repo, branch, place, room, room_source, reach, state, state_since, activity, activity_project, \
     opened_at, last_seen_at, ended_at";

fn session_row(row: &Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        client: row.get(2)?,
        kind: row.get(3)?,
        client_session: row.get(4)?,
        client_pid: row.get(5)?,
        client_pid_start: row.get::<_, Option<i64>>(6)?.map(uint),
        token_hash: row.get(7)?,
        cwd: row.get(8)?,
        repo: row.get(9)?,
        branch: row.get(10)?,
        place: row.get(11)?,
        room: row.get(12)?,
        room_source: row.get(13)?,
        reach: row.get(14)?,
        state: row.get(15)?,
        state_since: uint(row.get(16)?),
        activity: row.get(17)?,
        activity_project: row.get(18)?,
        opened_at: uint(row.get(19)?),
        last_seen_at: uint(row.get(20)?),
        ended_at: row.get::<_, Option<i64>>(21)?.map(uint),
    })
}

fn is_constraint(err: &rusqlite::Error) -> bool {
    err.sqlite_error_code() == Some(ErrorCode::ConstraintViolation)
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let found: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .ctx("read the schema version")?;
    let start = usize::try_from(found)
        .ok()
        .filter(|&v| v <= MIGRATIONS.len())
        .ok_or(StoreError::TooNew {
            found,
            known: schema_version(),
        })?;
    for (index, step) in MIGRATIONS.iter().enumerate().skip(start) {
        let tx = conn.transaction().ctx("start a migration")?;
        tx.execute_batch(step).ctx("apply a migration")?;
        let version = u32::try_from(index + 1).unwrap_or(u32::MAX);
        tx.pragma_update(None, "user_version", version)
            .ctx("record the schema version")?;
        tx.commit().ctx("commit a migration")?;
    }
    Ok(())
}

impl Store {
    /// Opens or creates the store at `path`: the file is created 0600 before SQLite opens it, so its `-wal` and
    /// `-shm` files get the same mode, then WAL mode is turned on and migrations run.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        private_open_options()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| PathsError::Io {
                action: "create",
                path: path.to_path_buf(),
                error,
            })?;
        if let Ok(Some(mode)) = mode_of(path)
            && mode != FILE_MODE
        {
            set_mode(path, FILE_MODE).map_err(|error| PathsError::Io {
                action: "set the mode of",
                path: path.to_path_buf(),
                error,
            })?;
        }
        let conn = Connection::open(path).ctx("open the database")?;
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .ctx("turn on WAL mode")?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::NotWal(mode));
        }
        Store::setup(conn)
    }

    /// An in-memory store, for tests.
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Store::setup(Connection::open_in_memory().ctx("open the database")?)
    }

    fn setup(mut conn: Connection) -> Result<Store, StoreError> {
        conn.busy_timeout(Duration::from_secs(5))
            .ctx("set the busy timeout")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .ctx("set synchronous")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .ctx("turn on foreign keys")?;
        migrate(&mut conn)?;
        Ok(Store { conn })
    }

    /// The schema version of the open file.
    pub fn version(&self) -> Result<u32, StoreError> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .ctx("read the schema version")
    }

    fn insert_message_row(conn: &Connection, message: &Message, now: u64) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO messages (id, conversation, kind, in_reply_to, \
             from_device, from_device_name, from_person, from_session, from_name, from_client, \
             to_device, to_device_name, to_person, to_session, to_name, \
             body, card, repo, room, sent_at, ttl_secs, state, code, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, \
             ?21, ?22, ?23, ?24, ?24)",
            params![
                message.id,
                message.conversation,
                message.kind,
                message.in_reply_to,
                message.from.device,
                message.from.device_name,
                message.from.person,
                message.from.session,
                message.from.name,
                message.from_client,
                message.to.device,
                message.to.device_name,
                message.to.person,
                message.to.session,
                message.to.name,
                message.body,
                message.card,
                message.repo,
                message.room,
                int(message.sent_at),
                int(message.ttl_secs),
                message.state,
                message.code,
                int(now),
            ],
        )?;
        let seq = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO message_states (message_seq, state, code, at) VALUES (?1, ?2, ?3, ?4)",
            params![seq, message.state, message.code, int(now)],
        )?;
        Ok(seq)
    }

    /// Stores a message sent from or delivered between local sessions, with its first state. Fails when (sending
    /// device, id) is already stored.
    pub fn insert_message(&self, message: &Message, now: u64) -> Result<i64, StoreError> {
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("start a transaction")?;
        let seq = Store::insert_message_row(&tx, message, now).ctx("store a message")?;
        tx.commit().ctx("commit a message")?;
        Ok(seq)
    }

    /// Stores a message accepted from a peer together with its dedupe key, in one transaction (5.5 step 8). A key
    /// already present means `Duplicate` and nothing is written. Never call it for a refusal, so a refused message
    /// can be sent again.
    pub fn insert_received(&self, message: &Message, now: u64) -> Result<Received, StoreError> {
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("start a transaction")?;
        let fresh = tx
            .execute(
                "INSERT INTO dedupe (device, id, at) VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
                params![message.from.device, message.id, int(now)],
            )
            .ctx("write a dedupe key")?;
        if fresh == 0 {
            return Ok(Received::Duplicate);
        }
        let seq = match Store::insert_message_row(&tx, message, now) {
            Ok(seq) => seq,
            // The message outlived its dedupe key; it is still the same message.
            Err(err) if is_constraint(&err) => return Ok(Received::Duplicate),
            Err(error) => {
                return Err(StoreError::Sql {
                    what: "store a received message",
                    error,
                });
            }
        };
        tx.commit().ctx("commit a received message")?;
        Ok(Received::Stored(seq))
    }

    /// Whether (peer device, id) has a dedupe key (5.5 step 4).
    pub fn is_duplicate(&self, device: &str, id: &str) -> Result<bool, StoreError> {
        self.conn
            .query_row(
                "SELECT 1 FROM dedupe WHERE device = ?1 AND id = ?2",
                params![device, id],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .ctx("check a dedupe key")
    }

    /// A message by sending device and id.
    pub fn message(
        &self,
        from_device: &str,
        id: &str,
    ) -> Result<Option<StoredMessage>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE from_device = ?1 AND id = ?2"
                ),
                params![from_device, id],
                message_row,
            )
            .optional()
            .ctx("read a message")
    }

    /// A message by row number.
    pub fn message_by_seq(&self, seq: i64) -> Result<Option<StoredMessage>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE seq = ?1"),
                params![seq],
                message_row,
            )
            .optional()
            .ctx("read a message")
    }

    /// Messages a session received with the given id. Ids are unique only per sending device, so this can return
    /// more than one.
    pub fn received_by_id(
        &self,
        to_session: &str,
        id: &str,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        self.query_messages(
            &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE to_session = ?1 AND id = ?2 ORDER BY seq"),
            params![to_session, id],
        )
    }

    /// Messages to a session in any of `states`, oldest first.
    pub fn messages_to(
        &self,
        to_session: &str,
        states: &[MessageState],
    ) -> Result<Vec<StoredMessage>, StoreError> {
        self.messages_in_states("to_session", to_session, states)
    }

    /// Messages from a session in any of `states`, oldest first.
    pub fn messages_from(
        &self,
        from_session: &str,
        states: &[MessageState],
    ) -> Result<Vec<StoredMessage>, StoreError> {
        self.messages_in_states("from_session", from_session, states)
    }

    // `column` is always one of the two literals above, never caller text.
    fn messages_in_states(
        &self,
        column: &'static str,
        session: &str,
        states: &[MessageState],
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let placeholders: Vec<String> = (0..states.len()).map(|i| format!("?{}", i + 2)).collect();
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages WHERE {column} = ?1 AND state IN ({}) ORDER BY seq",
            placeholders.join(", ")
        );
        let mut args: Vec<&dyn ToSql> = vec![&session];
        args.extend(states.iter().map(|state| state as &dyn ToSql));
        self.query_messages(&sql, args.as_slice())
    }

    /// Every message in a conversation, oldest first.
    pub fn conversation(&self, conversation: &str) -> Result<Vec<StoredMessage>, StoreError> {
        self.query_messages(
            &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation = ?1 ORDER BY seq"),
            params![conversation],
        )
    }

    fn query_messages(
        &self,
        sql: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        let mut stmt = self.conn.prepare(sql).ctx("read messages")?;
        let rows = stmt.query_map(args, message_row).ctx("read messages")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .ctx("read messages")
    }

    /// Moves a message to `state` and records it in its history, if 7.1 allows the move. Returns whether it moved.
    pub fn set_state(
        &self,
        seq: i64,
        state: MessageState,
        code: Option<&str>,
        at: u64,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("start a transaction")?;
        let current: Option<MessageState> = tx
            .query_row(
                "SELECT state FROM messages WHERE seq = ?1",
                params![seq],
                |row| row.get(0),
            )
            .optional()
            .ctx("read a message state")?;
        match current {
            Some(current) if current.can_become(state) => {}
            _ => return Ok(false),
        }
        tx.execute(
            "UPDATE messages SET state = ?2, code = ?3, updated_at = ?4 WHERE seq = ?1",
            params![seq, state, code, int(at)],
        )
        .ctx("change a message state")?;
        tx.execute(
            "INSERT INTO message_states (message_seq, state, code, at) VALUES (?1, ?2, ?3, ?4)",
            params![seq, state, code, int(at)],
        )
        .ctx("record a message state")?;
        tx.commit().ctx("commit a message state")?;
        Ok(true)
    }

    /// A message's state history, oldest first.
    pub fn state_history(&self, seq: i64) -> Result<Vec<StateChange>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT state, code, at FROM message_states WHERE message_seq = ?1 ORDER BY rowid",
            )
            .ctx("read a message history")?;
        let rows = stmt
            .query_map(params![seq], |row| {
                Ok(StateChange {
                    state: row.get(0)?,
                    code: row.get(1)?,
                    at: uint(row.get(2)?),
                })
            })
            .ctx("read a message history")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .ctx("read a message history")
    }

    fn insert_outbox_row(
        conn: &Connection,
        entry: &NewOutbox<'_>,
        message_seq: Option<i64>,
        now: u64,
    ) -> rusqlite::Result<i64> {
        conn.execute(
            "INSERT INTO outbox (device, kind, message_seq, payload, next_attempt_at, expires_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.device,
                entry.kind,
                message_seq,
                entry.payload,
                int(entry.next_attempt_at),
                int(entry.expires_at),
                int(now)
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Queues a call for a device; returns the entry's row number.
    pub fn outbox_push(&self, entry: &NewOutbox<'_>, now: u64) -> Result<i64, StoreError> {
        Store::insert_outbox_row(&self.conn, entry, entry.message_seq, now)
            .ctx("queue an outbox entry")
    }

    /// Stores a message for another device and its outbox entry in one transaction, so a queued message always
    /// has an entry that sends or expires it (7.4). The entry's `message_seq` is the new message's. Returns the
    /// message's and the entry's row numbers.
    pub fn insert_queued(
        &self,
        message: &Message,
        entry: &NewOutbox<'_>,
        now: u64,
    ) -> Result<(i64, i64), StoreError> {
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("start a transaction")?;
        let seq = Store::insert_message_row(&tx, message, now).ctx("store a message")?;
        let entry_seq =
            Store::insert_outbox_row(&tx, entry, Some(seq), now).ctx("queue an outbox entry")?;
        tx.commit().ctx("commit a queued message")?;
        Ok((seq, entry_seq))
    }

    /// Every entry for a device in queue order, for when its link comes up.
    pub fn outbox_for(&self, device: &str) -> Result<Vec<OutboxEntry>, StoreError> {
        self.query_outbox(
            &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE device = ?1 ORDER BY seq"),
            params![device],
        )
    }

    /// Entries for a device whose next attempt is due, in queue order.
    pub fn outbox_due(&self, device: &str, now: u64) -> Result<Vec<OutboxEntry>, StoreError> {
        self.query_outbox(
            &format!(
                "SELECT {OUTBOX_COLUMNS} FROM outbox WHERE device = ?1 AND next_attempt_at <= ?2 ORDER BY seq"
            ),
            params![device, int(now)],
        )
    }

    /// Entries whose time ran out, oldest first.
    pub fn outbox_expired(&self, now: u64) -> Result<Vec<OutboxEntry>, StoreError> {
        self.query_outbox(
            &format!("SELECT {OUTBOX_COLUMNS} FROM outbox WHERE expires_at <= ?1 ORDER BY seq"),
            params![int(now)],
        )
    }

    /// The devices with queued entries.
    pub fn outbox_devices(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT device FROM outbox ORDER BY device")
            .ctx("read outbox devices")?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .ctx("read outbox devices")?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
            .ctx("read outbox devices")
    }

    fn query_outbox(
        &self,
        sql: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<OutboxEntry>, StoreError> {
        let mut stmt = self.conn.prepare(sql).ctx("read the outbox")?;
        let rows = stmt.query_map(args, outbox_row).ctx("read the outbox")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .ctx("read the outbox")
    }

    /// Records a failed attempt and when to try next.
    pub fn outbox_retry(&self, seq: i64, next_attempt_at: u64) -> Result<(), StoreError> {
        self.conn
            .execute(
                "UPDATE outbox SET attempts = attempts + 1, next_attempt_at = ?2 WHERE seq = ?1",
                params![seq, int(next_attempt_at)],
            )
            .ctx("reschedule an outbox entry")?;
        Ok(())
    }

    /// Removes an entry once sent or given up.
    pub fn outbox_remove(&self, seq: i64) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM outbox WHERE seq = ?1", params![seq])
            .ctx("remove an outbox entry")?;
        Ok(())
    }

    /// Inserts a session or replaces every field of an existing one, keeping its cards.
    pub fn put_session(&self, s: &SessionRecord) -> Result<(), StoreError> {
        self.conn
            .execute(
                &format!(
                    "INSERT INTO sessions ({SESSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
                     ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22) \
                     ON CONFLICT (id) DO UPDATE SET name = excluded.name, client = excluded.client, \
                     kind = excluded.kind, client_session = excluded.client_session, \
                     client_pid = excluded.client_pid, client_pid_start = excluded.client_pid_start, \
                     token_hash = excluded.token_hash, cwd = excluded.cwd, repo = excluded.repo, \
                     branch = excluded.branch, place = excluded.place, room = excluded.room, \
                     room_source = excluded.room_source, reach = excluded.reach, state = excluded.state, \
                     state_since = excluded.state_since, activity = excluded.activity, \
                     activity_project = excluded.activity_project, opened_at = excluded.opened_at, \
                     last_seen_at = excluded.last_seen_at, ended_at = excluded.ended_at"
                ),
                params![
                    s.id,
                    s.name,
                    s.client,
                    s.kind,
                    s.client_session,
                    s.client_pid,
                    s.client_pid_start.map(int),
                    s.token_hash,
                    s.cwd,
                    s.repo,
                    s.branch,
                    s.place,
                    s.room,
                    s.room_source,
                    s.reach,
                    s.state,
                    int(s.state_since),
                    s.activity,
                    s.activity_project,
                    int(s.opened_at),
                    int(s.last_seen_at),
                    s.ended_at.map(int),
                ],
            )
            .ctx("store a session")?;
        Ok(())
    }

    /// A session by id, live or ended.
    pub fn session(&self, id: &str) -> Result<Option<SessionRecord>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                params![id],
                session_row,
            )
            .optional()
            .ctx("read a session")
    }

    /// The live session holding a token hash.
    pub fn session_by_token(&self, token_hash: &str) -> Result<Option<SessionRecord>, StoreError> {
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE token_hash = ?1 AND ended_at IS NULL"),
                params![token_hash],
                session_row,
            )
            .optional()
            .ctx("read a session")
    }

    /// The live session a client knows by its own session id.
    pub fn session_by_client(
        &self,
        client: &str,
        client_session: &str,
    ) -> Result<Option<SessionRecord>, StoreError> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions \
                     WHERE client = ?1 AND client_session = ?2 AND ended_at IS NULL ORDER BY opened_at LIMIT 1"
                ),
                params![client, client_session],
                session_row,
            )
            .optional()
            .ctx("read a session")
    }

    /// Every live session, oldest first.
    pub fn live_sessions(&self) -> Result<Vec<SessionRecord>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE ended_at IS NULL ORDER BY opened_at, id"
            ))
            .ctx("read sessions")?;
        let rows = stmt.query_map([], session_row).ctx("read sessions")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .ctx("read sessions")
    }

    /// Records that a session was reached through a control connection (6.4 rule 3).
    pub fn mark_had_connection(&self, id: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "UPDATE sessions SET had_connection = 1 WHERE id = ?1 AND had_connection = 0",
                params![id],
            )
            .ctx("mark a session connected")?;
        Ok(())
    }

    /// The live sessions that were ever reached through a control connection.
    pub fn had_connection(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE ended_at IS NULL AND had_connection = 1")
            .ctx("read connected sessions")?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .ctx("read connected sessions")?;
        rows.collect::<rusqlite::Result<_>>()
            .ctx("read connected sessions")
    }

    /// Ends a session, which frees its name (6.4). Returns whether a live session ended.
    pub fn end_session(&self, id: &str, at: u64) -> Result<bool, StoreError> {
        let changed = self
            .conn
            .execute(
                "UPDATE sessions SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
                params![id, int(at)],
            )
            .ctx("end a session")?;
        Ok(changed > 0)
    }

    /// Adds or refreshes a card a session holds.
    pub fn put_card(
        &self,
        session_id: &str,
        card: &str,
        project: &str,
        at: u64,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO session_cards (session_id, card, project, refreshed_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (session_id, card) DO UPDATE SET project = excluded.project, \
                 refreshed_at = excluded.refreshed_at",
                params![session_id, card, project, int(at)],
            )
            .ctx("store a card")?;
        Ok(())
    }

    /// Removes a card from a session.
    pub fn remove_card(&self, session_id: &str, card: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "DELETE FROM session_cards WHERE session_id = ?1 AND card = ?2",
                params![session_id, card],
            )
            .ctx("remove a card")?;
        Ok(())
    }

    /// Removes every card from a session.
    pub fn clear_cards(&self, session_id: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "DELETE FROM session_cards WHERE session_id = ?1",
                params![session_id],
            )
            .ctx("clear cards")?;
        Ok(())
    }

    /// The cards a session holds, by card ref.
    pub fn cards(&self, session_id: &str) -> Result<Vec<HeldCard>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT card, project, refreshed_at FROM session_cards WHERE session_id = ?1 ORDER BY card")
            .ctx("read cards")?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(HeldCard {
                    card: row.get(0)?,
                    project: row.get(1)?,
                    refreshed_at: uint(row.get(2)?),
                })
            })
            .ctx("read cards")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().ctx("read cards")
    }

    /// The client thread that continues a serve conversation (9).
    pub fn serve_thread(
        &self,
        client: &str,
        asker_device: &str,
        asker_session: &str,
        conversation: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT thread FROM serve_conversations \
                 WHERE client = ?1 AND asker_device = ?2 AND asker_session = ?3 AND conversation = ?4",
                params![client, asker_device, asker_session, conversation],
                |row| row.get(0),
            )
            .optional()
            .ctx("read a serve conversation")
    }

    /// Records or replaces the client thread for a serve conversation (9).
    pub fn set_serve_thread(
        &self,
        client: &str,
        asker_device: &str,
        asker_session: &str,
        conversation: &str,
        thread: &str,
        now: u64,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO serve_conversations (client, asker_device, asker_session, conversation, thread, \
                 updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (client, asker_device, asker_session, conversation) DO UPDATE SET \
                 thread = excluded.thread, updated_at = excluded.updated_at",
                params![client, asker_device, asker_session, conversation, thread, int(now)],
            )
            .ctx("store a serve conversation")?;
        Ok(())
    }

    /// Stores a peer's latest presence snapshot.
    pub fn put_presence(
        &self,
        device: &str,
        seq: u64,
        snapshot: &str,
        now: u64,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO presence_cache (device, seq, snapshot, received_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (device) DO UPDATE SET seq = excluded.seq, snapshot = excluded.snapshot, \
                 received_at = excluded.received_at",
                params![device, int(seq), snapshot, int(now)],
            )
            .ctx("store presence")?;
        Ok(())
    }

    /// A peer's cached presence.
    pub fn presence(&self, device: &str) -> Result<Option<CachedPresence>, StoreError> {
        self.conn
            .query_row(
                "SELECT device, seq, snapshot, received_at FROM presence_cache WHERE device = ?1",
                params![device],
                |row| {
                    Ok(CachedPresence {
                        device: row.get(0)?,
                        seq: uint(row.get(1)?),
                        snapshot: row.get(2)?,
                        received_at: uint(row.get(3)?),
                    })
                },
            )
            .optional()
            .ctx("read presence")
    }

    /// Every cached presence snapshot.
    pub fn presences(&self) -> Result<Vec<CachedPresence>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT device, seq, snapshot, received_at FROM presence_cache ORDER BY device",
            )
            .ctx("read presence")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CachedPresence {
                    device: row.get(0)?,
                    seq: uint(row.get(1)?),
                    snapshot: row.get(2)?,
                    received_at: uint(row.get(3)?),
                })
            })
            .ctx("read presence")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .ctx("read presence")
    }

    /// Drops a peer's presence, such as when it is no longer admitted (4.3).
    pub fn remove_presence(&self, device: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "DELETE FROM presence_cache WHERE device = ?1",
                params![device],
            )
            .ctx("remove presence")?;
        Ok(())
    }

    /// Stores the last good roster from a provider.
    pub fn put_roster(&self, roster: &CachedRoster) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO roster_cache (provider, version, etag, roster, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (provider) DO UPDATE SET version = excluded.version, etag = excluded.etag, \
                 roster = excluded.roster, fetched_at = excluded.fetched_at",
                params![
                    roster.provider,
                    roster.version,
                    roster.etag,
                    roster.roster,
                    int(roster.fetched_at)
                ],
            )
            .ctx("store a roster")?;
        Ok(())
    }

    /// The cached roster from a provider.
    pub fn roster(&self, provider: RosterProvider) -> Result<Option<CachedRoster>, StoreError> {
        self.conn
            .query_row(
                "SELECT provider, version, etag, roster, fetched_at FROM roster_cache WHERE provider = ?1",
                params![provider],
                |row| {
                    Ok(CachedRoster {
                        provider: row.get(0)?,
                        version: row.get(1)?,
                        etag: row.get(2)?,
                        roster: row.get(3)?,
                        fetched_at: uint(row.get(4)?),
                    })
                },
            )
            .optional()
            .ctx("read a roster")
    }

    /// The daily sweep (11.1): messages and receipts older than 30 days, sessions that ended more than 30 days ago,
    /// serve conversations unused for 30 days, dedupe keys older than 7 days. A serve conversation's `updated_at`
    /// is its last use: serve writes it after every run that continues or starts it.
    pub fn sweep(&self, now: u64) -> Result<Swept, StoreError> {
        let tx = self
            .conn
            .unchecked_transaction()
            .ctx("start a transaction")?;
        let message_cutoff = int(now.saturating_sub(MESSAGE_RETENTION_MS));
        let dedupe_cutoff = int(now.saturating_sub(DEDUPE_RETENTION_MS));
        let messages = tx
            .execute(
                "DELETE FROM messages WHERE created_at < ?1",
                params![message_cutoff],
            )
            .ctx("sweep messages")?;
        let receipts = tx
            .execute(
                "DELETE FROM outbox WHERE kind = 'receipt' AND created_at < ?1",
                params![message_cutoff],
            )
            .ctx("sweep receipts")?;
        let dedupe = tx
            .execute("DELETE FROM dedupe WHERE at < ?1", params![dedupe_cutoff])
            .ctx("sweep dedupe keys")?;
        let sessions = tx
            .execute(
                "DELETE FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?1",
                params![message_cutoff],
            )
            .ctx("sweep ended sessions")?;
        let serve_conversations = tx
            .execute(
                "DELETE FROM serve_conversations WHERE updated_at < ?1",
                params![message_cutoff],
            )
            .ctx("sweep serve conversations")?;
        tx.commit().ctx("commit the sweep")?;
        Ok(Swept {
            messages,
            receipts,
            dedupe,
            sessions,
            serve_conversations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60 * 1000;
    const MAYA_DEVICE: &str = "aa00000000000000000000000000000000000000000000000000000000000000";
    const SAM_DEVICE: &str = "bb00000000000000000000000000000000000000000000000000000000000000";

    fn ulid(n: u128) -> String {
        ulid::Ulid::from_parts(1_730_000_000_000, n).to_string()
    }

    fn message(n: u128, state: MessageState) -> Message {
        Message {
            id: ulid(n),
            conversation: ulid(n),
            kind: MessageKind::Ask,
            in_reply_to: None,
            from: Party {
                device: MAYA_DEVICE.into(),
                device_name: "maya-mbp".into(),
                person: "maya".into(),
                session: ulid(100),
                name: "claude-api".into(),
            },
            from_client: "claude".into(),
            to: Party {
                device: SAM_DEVICE.into(),
                device_name: "sam-mbp".into(),
                person: "sam".into(),
                session: ulid(200),
                name: "codex-api".into(),
            },
            body: "Which table holds sessions?".into(),
            card: Some("spew#42".into()),
            repo: Some("acme/api".into()),
            room: Some("spew".into()),
            sent_at: 1_730_000_000_000,
            ttl_secs: 86_400,
            state,
            code: None,
        }
    }

    fn session(n: u128, name: &str) -> SessionRecord {
        SessionRecord {
            id: ulid(n),
            name: name.into(),
            client: "claude".into(),
            kind: SessionKind::Live,
            client_session: Some(format!("client-{n}")),
            client_pid: Some(4242),
            client_pid_start: Some(1_000),
            token_hash: hash_token(&format!("token-{n}")),
            cwd: Some("/work/api".into()),
            repo: Some("acme/api".into()),
            branch: Some("main".into()),
            place: Some("api".into()),
            room: Some("spew".into()),
            room_source: Some("spew-json".into()),
            reach: Reach::Hooked,
            state: ActivityState::Idle,
            state_since: 10,
            activity: None,
            activity_project: None,
            opened_at: 10,
            last_seen_at: 10,
            ended_at: None,
        }
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn migrations_reach_the_latest_version() {
        let s = store();
        assert_eq!(s.version().unwrap(), schema_version());
        assert!(MIGRATIONS.iter().all(|m| !m.trim().is_empty()));
    }

    #[test]
    fn upgrades_from_every_earlier_version() {
        for from in 0..MIGRATIONS.len() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("store.sqlite3");
            {
                let mut conn = Connection::open(&path).unwrap();
                for (index, step) in MIGRATIONS.iter().enumerate().take(from) {
                    let tx = conn.transaction().unwrap();
                    tx.execute_batch(step).unwrap();
                    tx.pragma_update(None, "user_version", index as u32 + 1)
                        .unwrap();
                    tx.commit().unwrap();
                }
            }
            let s = Store::open(&path).unwrap();
            assert_eq!(s.version().unwrap(), schema_version(), "from {from}");
            s.insert_message(&message(1, MessageState::Delivered), 5)
                .unwrap();
            drop(s);
            // Opening again is a no-op that keeps the data.
            let s = Store::open(&path).unwrap();
            assert!(s.message(MAYA_DEVICE, &ulid(1)).unwrap().is_some());
        }
    }

    #[test]
    fn a_v1_store_with_a_session_upgrades_and_reads_had_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.sqlite3");
        let id = ulid(1);
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(V1).unwrap();
            tx.pragma_update(None, "user_version", 1).unwrap();
            tx.commit().unwrap();
            conn.execute(
                "INSERT INTO sessions (id, name, client, kind, client_session, client_pid, client_pid_start, \
                 token_hash, cwd, repo, branch, place, room, room_source, reach, state, state_since, activity, \
                 activity_project, opened_at, last_seen_at, ended_at) VALUES (?1, 'codex-api', 'codex', 'live', \
                 'thread-1', NULL, NULL, 'hash', '/work/api', 'acme/api', 'main', 'api', 'spew', 'spew-json', \
                 'hooked', 'idle', 10, NULL, NULL, 10, 20, NULL)",
                params![id],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.version().unwrap(), schema_version());
        let kept = s
            .session(&id)
            .unwrap()
            .expect("the session survives the upgrade");
        assert_eq!(kept.name, "codex-api");
        assert_eq!(kept.reach, Reach::Hooked);
        // A session from before the column existed counts as never connected, so rule 3 doesn't end it early.
        assert!(s.had_connection().unwrap().is_empty());
        s.mark_had_connection(&id).unwrap();
        let connected = s.had_connection().unwrap();
        assert!(connected.len() == 1 && connected.contains(&id));
        drop(s);
        let s = Store::open(&path).unwrap();
        assert!(s.had_connection().unwrap().contains(&id));
    }

    #[test]
    fn refuses_a_newer_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.sqlite3");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        let err = Store::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::TooNew { found: 99, .. }), "{err}");
        assert!(err.to_string().contains("update spew mesh"));
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private_and_in_wal_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("store.sqlite3");
        let s = Store::open(&path).unwrap();
        s.insert_message(&message(1, MessageState::Delivered), 5)
            .unwrap();
        let journal: String = s
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        for file in ["store.sqlite3", "store.sqlite3-wal", "store.sqlite3-shm"] {
            let p = tmp.path().join(file);
            assert_eq!(mode_of(&p).unwrap(), Some(0o600), "{file}");
        }
        let foreign: i64 = s
            .conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign, 1);
    }

    #[test]
    fn message_round_trip_and_history() {
        let s = store();
        let m = message(1, MessageState::Queued);
        let seq = s.insert_message(&m, 1_000).unwrap();
        let stored = s.message(MAYA_DEVICE, &m.id).unwrap().unwrap();
        assert_eq!(stored.seq, seq);
        assert_eq!(stored.message, m);
        assert_eq!(stored.created_at, 1_000);
        assert_eq!(s.message_by_seq(seq).unwrap().unwrap().message, m);
        assert!(s.insert_message(&m, 1_001).is_err());

        assert!(
            s.set_state(seq, MessageState::Delivered, None, 2_000)
                .unwrap()
        );
        assert!(s.set_state(seq, MessageState::Shown, None, 3_000).unwrap());
        assert!(
            !s.set_state(seq, MessageState::Delivered, None, 4_000)
                .unwrap()
        );
        assert!(
            !s.set_state(seq, MessageState::Expired, None, 4_000)
                .unwrap()
        );
        assert!(
            s.set_state(seq, MessageState::Answered, None, 5_000)
                .unwrap()
        );
        assert!(
            !s.set_state(seq, MessageState::Refused, Some("busy"), 6_000)
                .unwrap()
        );
        assert!(
            !s.set_state(9_999, MessageState::Delivered, None, 1)
                .unwrap()
        );
        let history: Vec<(MessageState, u64)> = s
            .state_history(seq)
            .unwrap()
            .into_iter()
            .map(|c| (c.state, c.at))
            .collect();
        assert_eq!(
            history,
            [
                (MessageState::Queued, 1_000),
                (MessageState::Delivered, 2_000),
                (MessageState::Shown, 3_000),
                (MessageState::Answered, 5_000)
            ]
        );
        let stored = s.message_by_seq(seq).unwrap().unwrap();
        assert_eq!(stored.message.state, MessageState::Answered);
        assert_eq!(stored.updated_at, 5_000);
    }

    #[test]
    fn refusal_codes_are_recorded() {
        let s = store();
        let seq = s
            .insert_message(&message(1, MessageState::Delivered), 1)
            .unwrap();
        assert!(
            s.set_state(seq, MessageState::Refused, Some("session-ended"), 2)
                .unwrap()
        );
        let stored = s.message_by_seq(seq).unwrap().unwrap();
        assert_eq!(stored.message.code.as_deref(), Some("session-ended"));
        assert_eq!(
            s.state_history(seq).unwrap()[1].code.as_deref(),
            Some("session-ended")
        );
    }

    #[test]
    fn state_moves() {
        use MessageState::*;
        assert!(Queued.can_become(Delivered));
        assert!(Queued.can_become(Expired));
        assert!(Delivered.can_become(Shown));
        assert!(Delivered.can_become(Refused));
        assert!(Shown.can_become(Answered));
        assert!(!Shown.can_become(Refused));
        assert!(!Delivered.can_become(Queued));
        for end in [Answered, Refused, Expired] {
            assert!(end.is_final());
            for next in [Queued, Delivered, Shown, Answered, Refused, Expired] {
                assert!(!end.can_become(next));
            }
        }
    }

    #[test]
    fn received_message_and_dedupe_key_in_one_transaction() {
        let s = store();
        let m = message(1, MessageState::Delivered);
        let Received::Stored(seq) = s.insert_received(&m, 10).unwrap() else {
            panic!("not stored");
        };
        assert!(s.is_duplicate(MAYA_DEVICE, &m.id).unwrap());
        assert_eq!(s.insert_received(&m, 11).unwrap(), Received::Duplicate);
        // The same id from another device is a different message.
        let mut other = m.clone();
        other.from.device = SAM_DEVICE.into();
        assert!(matches!(
            s.insert_received(&other, 12).unwrap(),
            Received::Stored(_)
        ));
        assert_eq!(s.received_by_id(&m.to.session, &m.id).unwrap().len(), 2);
        assert_eq!(s.message_by_seq(seq).unwrap().unwrap().message, m);
    }

    #[test]
    fn a_failed_insert_writes_no_dedupe_key() {
        let s = store();
        let broken = message(1, MessageState::Delivered);
        // Insert a row that collides on (from_device, id) without a dedupe key, as after a dedupe sweep.
        s.insert_message(&broken, 1).unwrap();
        assert!(!s.is_duplicate(MAYA_DEVICE, &broken.id).unwrap());
        assert_eq!(s.insert_received(&broken, 2).unwrap(), Received::Duplicate);
        // The dedupe insert was rolled back with the failed message insert.
        assert!(!s.is_duplicate(MAYA_DEVICE, &broken.id).unwrap());
    }

    #[test]
    fn a_refused_message_can_be_sent_again() {
        let s = store();
        let m = message(1, MessageState::Delivered);
        // A refusal stores nothing, so the same id is accepted later.
        assert!(!s.is_duplicate(MAYA_DEVICE, &m.id).unwrap());
        assert!(matches!(
            s.insert_received(&m, 5).unwrap(),
            Received::Stored(_)
        ));
    }

    #[test]
    fn messages_by_session_and_conversation() {
        let s = store();
        for n in 1..=3 {
            s.insert_message(&message(n, MessageState::Delivered), n as u64)
                .unwrap();
        }
        let shown = s
            .insert_message(&message(4, MessageState::Shown), 4)
            .unwrap();
        let to = ulid(200);
        let unread = s.messages_to(&to, &[MessageState::Delivered]).unwrap();
        assert_eq!(unread.len(), 3);
        assert!(unread.windows(2).all(|w| w[0].seq < w[1].seq));
        let seen = s.messages_to(&to, &[MessageState::Shown]).unwrap();
        assert_eq!(seen[0].seq, shown);
        assert_eq!(
            s.messages_from(&ulid(100), &[MessageState::Delivered])
                .unwrap()
                .len(),
            3
        );
        assert_eq!(s.conversation(&ulid(2)).unwrap().len(), 1);
        assert!(
            s.messages_to("nobody", &[MessageState::Delivered])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn notices_are_stored_like_messages() {
        let s = store();
        let mut notice = message(1, MessageState::Delivered);
        notice.kind = MessageKind::Notice;
        notice.from.name = "mesh".into();
        let seq = s.insert_message(&notice, 1).unwrap();
        assert_eq!(
            s.message_by_seq(seq).unwrap().unwrap().message.kind,
            MessageKind::Notice
        );
    }

    #[test]
    fn a_queued_message_and_its_outbox_entry_are_written_together() {
        let s = store();
        let m = message(1, MessageState::Queued);
        let entry = NewOutbox {
            device: SAM_DEVICE,
            kind: OutboxKind::Deliver,
            message_seq: None,
            payload: "{}",
            next_attempt_at: 10,
            expires_at: 1_000,
        };
        let (seq, entry_seq) = s.insert_queued(&m, &entry, 1).unwrap();
        let queued = s.outbox_for(SAM_DEVICE).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].seq, entry_seq);
        assert_eq!(queued[0].message_seq, Some(seq));
        // A message that can't be stored leaves no outbox entry behind.
        assert!(s.insert_queued(&m, &entry, 2).is_err());
        assert_eq!(s.outbox_for(SAM_DEVICE).unwrap().len(), 1);
    }

    #[test]
    fn sessions_remember_having_had_a_connection() {
        let s = store();
        let a = session(1, "claude-api");
        let b = session(2, "codex-web");
        s.put_session(&a).unwrap();
        s.put_session(&b).unwrap();
        s.mark_had_connection(&a.id).unwrap();
        // Rewriting the record keeps the flag.
        s.put_session(&a).unwrap();
        let connected = s.had_connection().unwrap();
        assert!(connected.contains(&a.id) && !connected.contains(&b.id));
        s.end_session(&a.id, 20).unwrap();
        assert!(s.had_connection().unwrap().is_empty());
    }

    #[test]
    fn outbox_queue() {
        let s = store();
        let seq = s
            .insert_message(&message(1, MessageState::Queued), 1)
            .unwrap();
        let a = s
            .outbox_push(
                &NewOutbox {
                    device: MAYA_DEVICE,
                    kind: OutboxKind::Deliver,
                    message_seq: Some(seq),
                    payload: "{\"t\":\"deliver\"}",
                    next_attempt_at: 100,
                    expires_at: 1_000,
                },
                1,
            )
            .unwrap();
        let b = s
            .outbox_push(
                &NewOutbox {
                    device: MAYA_DEVICE,
                    kind: OutboxKind::Receipt,
                    message_seq: None,
                    payload: "{\"t\":\"receipt\"}",
                    next_attempt_at: 500,
                    expires_at: 2_000,
                },
                2,
            )
            .unwrap();
        s.outbox_push(
            &NewOutbox {
                device: SAM_DEVICE,
                kind: OutboxKind::Receipt,
                message_seq: None,
                payload: "{}",
                next_attempt_at: 50,
                expires_at: 3_000,
            },
            3,
        )
        .unwrap();
        assert_eq!(s.outbox_devices().unwrap(), [MAYA_DEVICE, SAM_DEVICE]);
        let all: Vec<i64> = s
            .outbox_for(MAYA_DEVICE)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(all, [a, b]);
        let due = s.outbox_due(MAYA_DEVICE, 200).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].kind, OutboxKind::Deliver);
        assert_eq!(due[0].message_seq, Some(seq));
        assert_eq!(due[0].attempts, 0);
        s.outbox_retry(a, 600).unwrap();
        assert!(s.outbox_due(MAYA_DEVICE, 200).unwrap().is_empty());
        let retried = s.outbox_due(MAYA_DEVICE, 600).unwrap();
        assert_eq!(retried[0].attempts, 1);
        assert_eq!(retried.len(), 2);
        let expired: Vec<i64> = s
            .outbox_expired(1_500)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(expired, [a]);
        s.outbox_remove(a).unwrap();
        assert_eq!(s.outbox_for(MAYA_DEVICE).unwrap().len(), 1);
    }

    #[test]
    fn sessions_and_names() {
        let s = store();
        let mut first = session(1, "claude-api");
        s.put_session(&first).unwrap();
        assert_eq!(s.session(&first.id).unwrap().unwrap(), first);
        assert_eq!(
            s.session_by_token(&hash_token("token-1"))
                .unwrap()
                .unwrap()
                .id,
            first.id
        );
        assert_eq!(
            s.session_by_client("claude", "client-1")
                .unwrap()
                .unwrap()
                .id,
            first.id
        );
        // A live name is unique.
        assert!(
            s.put_session(&session(2, "claude-api"))
                .unwrap_err()
                .is_constraint()
        );
        s.put_card(&first.id, "spew#42", "spew", 5).unwrap();
        first.activity = Some("fixing tests".into());
        first.state = ActivityState::Busy;
        s.put_session(&first).unwrap();
        assert_eq!(
            s.session(&first.id).unwrap().unwrap().activity.as_deref(),
            Some("fixing tests")
        );
        // Updating a session keeps its cards.
        assert_eq!(s.cards(&first.id).unwrap().len(), 1);
        assert!(s.end_session(&first.id, 20).unwrap());
        assert!(!s.end_session(&first.id, 21).unwrap());
        assert!(
            s.session_by_token(&hash_token("token-1"))
                .unwrap()
                .is_none()
        );
        assert!(s.live_sessions().unwrap().is_empty());
        // The name is free again once the session ended.
        s.put_session(&session(2, "claude-api")).unwrap();
        assert_eq!(s.live_sessions().unwrap().len(), 1);
        assert_eq!(s.session(&first.id).unwrap().unwrap().ended_at, Some(20));
    }

    #[test]
    fn cards() {
        let s = store();
        let rec = session(1, "claude-api");
        s.put_session(&rec).unwrap();
        s.put_card(&rec.id, "spew#42", "spew", 5).unwrap();
        s.put_card(&rec.id, "web#7", "web", 6).unwrap();
        s.put_card(&rec.id, "spew#42", "spew", 9).unwrap();
        let cards = s.cards(&rec.id).unwrap();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].refreshed_at, 9);
        s.remove_card(&rec.id, "web#7").unwrap();
        assert_eq!(s.cards(&rec.id).unwrap().len(), 1);
        s.clear_cards(&rec.id).unwrap();
        assert!(s.cards(&rec.id).unwrap().is_empty());
        assert!(s.put_card(&ulid(999), "spew#1", "spew", 1).is_err());
    }

    #[test]
    fn serve_conversation_map() {
        let s = store();
        assert_eq!(
            s.serve_thread("claude", MAYA_DEVICE, &ulid(1), &ulid(2))
                .unwrap(),
            None
        );
        s.set_serve_thread("claude", MAYA_DEVICE, &ulid(1), &ulid(2), "thread-a", 1)
            .unwrap();
        s.set_serve_thread("codex", MAYA_DEVICE, &ulid(1), &ulid(2), "thread-b", 1)
            .unwrap();
        assert_eq!(
            s.serve_thread("claude", MAYA_DEVICE, &ulid(1), &ulid(2))
                .unwrap()
                .as_deref(),
            Some("thread-a")
        );
        s.set_serve_thread("claude", MAYA_DEVICE, &ulid(1), &ulid(2), "thread-c", 2)
            .unwrap();
        assert_eq!(
            s.serve_thread("claude", MAYA_DEVICE, &ulid(1), &ulid(2))
                .unwrap()
                .as_deref(),
            Some("thread-c")
        );
        assert_eq!(
            s.serve_thread("codex", MAYA_DEVICE, &ulid(1), &ulid(2))
                .unwrap()
                .as_deref(),
            Some("thread-b")
        );
    }

    #[test]
    fn presence_and_roster_caches() {
        let s = store();
        s.put_presence(MAYA_DEVICE, 1, "{\"seq\":1}", 10).unwrap();
        s.put_presence(MAYA_DEVICE, 2, "{\"seq\":2}", 20).unwrap();
        let cached = s.presence(MAYA_DEVICE).unwrap().unwrap();
        assert_eq!((cached.seq, cached.received_at), (2, 20));
        assert_eq!(s.presences().unwrap().len(), 1);
        s.remove_presence(MAYA_DEVICE).unwrap();
        assert!(s.presence(MAYA_DEVICE).unwrap().is_none());

        let roster = CachedRoster {
            provider: RosterProvider::Spew,
            version: Some("v1".into()),
            etag: Some("\"abc\"".into()),
            roster: "{\"rooms\":[]}".into(),
            fetched_at: 30,
        };
        s.put_roster(&roster).unwrap();
        assert_eq!(s.roster(RosterProvider::Spew).unwrap().unwrap(), roster);
        assert!(s.roster(RosterProvider::File).unwrap().is_none());
        let newer = CachedRoster {
            version: Some("v2".into()),
            ..roster
        };
        s.put_roster(&newer).unwrap();
        assert_eq!(
            s.roster(RosterProvider::Spew)
                .unwrap()
                .unwrap()
                .version
                .as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn sweep_drops_old_rows() {
        let s = store();
        let now = 100 * DAY;
        let old = s
            .insert_message(&message(1, MessageState::Delivered), now - 31 * DAY)
            .unwrap();
        let recent = s
            .insert_message(&message(2, MessageState::Delivered), now - 29 * DAY)
            .unwrap();
        s.outbox_push(
            &NewOutbox {
                device: MAYA_DEVICE,
                kind: OutboxKind::Deliver,
                message_seq: Some(old),
                payload: "{}",
                next_attempt_at: 0,
                expires_at: now,
            },
            now - 31 * DAY,
        )
        .unwrap();
        s.outbox_push(
            &NewOutbox {
                device: MAYA_DEVICE,
                kind: OutboxKind::Receipt,
                message_seq: None,
                payload: "{}",
                next_attempt_at: 0,
                expires_at: now,
            },
            now - 31 * DAY,
        )
        .unwrap();
        s.outbox_push(
            &NewOutbox {
                device: MAYA_DEVICE,
                kind: OutboxKind::Receipt,
                message_seq: None,
                payload: "{}",
                next_attempt_at: 0,
                expires_at: now,
            },
            now - DAY,
        )
        .unwrap();
        let mut old_key = message(3, MessageState::Delivered);
        old_key.from.device = SAM_DEVICE.into();
        s.insert_received(&old_key, now - 8 * DAY).unwrap();
        let mut new_key = message(4, MessageState::Delivered);
        new_key.from.device = SAM_DEVICE.into();
        s.insert_received(&new_key, now - 6 * DAY).unwrap();
        // Ended 31 days ago, ended 29 days ago, and live since long ago.
        s.put_session(&session(10, "claude-old")).unwrap();
        s.put_card(&ulid(10), "spew#1", "spew", now - 40 * DAY)
            .unwrap();
        s.end_session(&ulid(10), now - 31 * DAY).unwrap();
        s.put_session(&session(11, "claude-recent")).unwrap();
        s.end_session(&ulid(11), now - 29 * DAY).unwrap();
        s.put_session(&session(12, "claude-live")).unwrap();
        // Last used 31 and 29 days ago.
        s.set_serve_thread(
            "claude",
            MAYA_DEVICE,
            &ulid(20),
            &ulid(21),
            "old",
            now - 31 * DAY,
        )
        .unwrap();
        s.set_serve_thread(
            "claude",
            MAYA_DEVICE,
            &ulid(20),
            &ulid(22),
            "new",
            now - 29 * DAY,
        )
        .unwrap();

        let swept = s.sweep(now).unwrap();
        assert_eq!(
            swept,
            Swept {
                messages: 1,
                receipts: 1,
                dedupe: 1,
                sessions: 1,
                serve_conversations: 1,
            }
        );
        assert_eq!(
            s.serve_thread("claude", MAYA_DEVICE, &ulid(20), &ulid(21))
                .unwrap(),
            None
        );
        assert_eq!(
            s.serve_thread("claude", MAYA_DEVICE, &ulid(20), &ulid(22))
                .unwrap()
                .as_deref(),
            Some("new")
        );
        assert!(s.session(&ulid(10)).unwrap().is_none());
        assert!(s.cards(&ulid(10)).unwrap().is_empty());
        assert!(s.session(&ulid(11)).unwrap().is_some());
        assert!(s.session(&ulid(12)).unwrap().is_some());
        assert!(s.message_by_seq(old).unwrap().is_none());
        assert!(s.state_history(old).unwrap().is_empty());
        assert!(s.message_by_seq(recent).unwrap().is_some());
        // The old deliver entry went with its message; one recent receipt stays.
        assert_eq!(s.outbox_for(MAYA_DEVICE).unwrap().len(), 1);
        assert!(!s.is_duplicate(SAM_DEVICE, &old_key.id).unwrap());
        assert!(s.is_duplicate(SAM_DEVICE, &new_key.id).unwrap());
    }

    #[test]
    fn token_hash() {
        let h = hash_token("abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(hash_token("abd"), h);
    }

    #[test]
    fn bad_enum_text_is_an_error() {
        let s = store();
        let seq = s
            .insert_message(&message(1, MessageState::Delivered), 1)
            .unwrap();
        s.conn
            .execute(
                "UPDATE message_states SET state = 'weird' WHERE message_seq = ?1",
                params![seq],
            )
            .unwrap();
        assert!(s.state_history(seq).is_err());
    }
}
