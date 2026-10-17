//! Courier tests: the `deliver` checks of 5.5, receipts and notices, against couriers on an in-memory network.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use serde_json::{Value, json};
use tokio::sync::watch;

use super::*;
use crate::control::protocol::{
    FetchParams, Reason, ReplyParams, SendResult, SendStatus, SessionOpen, TakeParams, TellParams,
};
use crate::daemon::links::InboundDyn;
use crate::daemon::testnet::{MemLinks, MemNet};
use crate::link::Peer;
use crate::responder::limits::{GateSettings, MAX_RUN_LIMIT};
use crate::roster::file;
use crate::settings::ServeFrom;
use crate::wire::{
    ActivityState, DeliverState, ErrorCode, Incoming, PresenceDevice, Reach, Response,
};

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn id(n: u8) -> EndpointId {
    iroh::SecretKey::from_bytes(&[n; 32]).public()
}

const SAM_MAC: u8 = 1;
const MAYA_MBP: u8 = 2;
const SAM_LINUX: u8 = 3;

fn roster_text() -> String {
    format!(
        "[[person]]\nname = \"sam\"\ndevices = [ {{ id = \"{}\", name = \"sam-mac\" }}, {{ id = \"{}\", name = \"sam-linux\" }} ]\n\n\
         [[person]]\nname = \"maya\"\ndevices = [ {{ id = \"{}\", name = \"maya-mbp\" }} ]\n\n\
         [[room]]\nname = \"team\"\npeople = [\"sam\", \"maya\"]\n",
        id(SAM_MAC),
        id(SAM_LINUX),
        id(MAYA_MBP)
    )
}

struct Node {
    courier: Arc<Courier<MemLinks>>,
    clock: Arc<TestClock>,
    dir: tempfile::TempDir,
    closers: StdMutex<Vec<watch::Sender<bool>>>,
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1_000);

impl Node {
    fn new(net: &MemNet, n: u8, person: &str, name: &str) -> Node {
        Node::with_serve(net, n, person, name, Default::default())
    }

    fn with_serve(
        net: &MemNet,
        n: u8,
        person: &str,
        name: &str,
        serve: crate::responder::ServeSettings,
    ) -> Node {
        let device = id(n);
        let (me, roster) = file::parse(&roster_text())
            .unwrap()
            .roster_for(Some(person), &device, 1_000)
            .unwrap();
        let clock = Arc::new(TestClock(AtomicU64::new(1_000_000)));
        let cfg = CourierConfig {
            device: LocalDevice::new(device, name, "linux"),
            default_room: None,
            send_branch: true,
            send_place: true,
            serve,
            clock: clock.clone(),
            start_time: |_| Some(1),
        };
        let (links, inbound) = net.join(device);
        let active = ActiveRoster::new(me, device, roster);
        let store = Store::open_in_memory().unwrap();
        let courier =
            Arc::new(Courier::new(links, store, active, RosterInfo::default(), cfg).unwrap());
        inbound.bind(Arc::downgrade(&courier) as Weak<dyn InboundDyn>);
        courier.start_links();
        Node {
            courier,
            clock,
            dir: tempfile::tempdir().unwrap(),
            closers: StdMutex::new(Vec::new()),
        }
    }

    fn advance(&self, ms: u64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }

    fn conn(&self) -> Conn {
        let (conn, closer) = Conn::detached(NEXT_CONN.fetch_add(1, Ordering::Relaxed));
        self.closers.lock().unwrap().push(closer);
        conn
    }

    // Opens a live session in folder `place`, in `room` when given; returns (token, session id, name).
    fn open(&self, place: &str, room: Option<&str>) -> (String, String, String) {
        let cwd = self.dir.path().join(place);
        std::fs::create_dir_all(&cwd).unwrap();
        let opened = self
            .courier
            .open_session(
                &self.conn(),
                SessionOpen {
                    client: "claude".into(),
                    client_session: None,
                    client_pid: None,
                    cwd: cwd.to_string_lossy().into_owned(),
                    client_name: None,
                    kind: SessionKind::Live,
                    name: None,
                    spew_project: room.map(str::to_string),
                    serve: None,
                },
            )
            .unwrap();
        (opened.token, opened.session, opened.name)
    }

    // Opens a serve responder in folder `place`, in `room`; returns (token, session id, name).
    fn open_responder(&self, place: &str, room: &str) -> (String, String, String) {
        let cwd = self.dir.path().join(place);
        std::fs::create_dir_all(&cwd).unwrap();
        let opened = self
            .courier
            .open_session(
                &self.conn(),
                SessionOpen {
                    client: "claude".into(),
                    client_session: Some(format!("serve-{place}")),
                    client_pid: None,
                    cwd: cwd.to_string_lossy().into_owned(),
                    client_name: None,
                    kind: SessionKind::Responder,
                    name: None,
                    spew_project: Some(room.into()),
                    serve: None,
                },
            )
            .unwrap();
        (opened.token, opened.session, opened.name)
    }

    async fn reply(&self, token: &str, id: &str, text: &str, wait: Option<u64>) -> SendResult {
        self.courier
            .reply(
                &self.conn(),
                ReplyParams {
                    token: token.into(),
                    id: id.into(),
                    text: text.into(),
                    wait_seconds: wait,
                },
            )
            .await
            .unwrap()
    }

    fn outbox_kinds(&self, device: u8) -> Vec<OutboxKind> {
        self.courier
            .core()
            .store
            .outbox_for(&id(device).to_string())
            .unwrap()
            .iter()
            .map(|e| e.kind)
            .collect()
    }

    // Every address of another device the directory lists, offline devices included.
    fn listed(&self) -> Vec<String> {
        let me = format!("@{}/", self.courier.device().name);
        let dir = self
            .courier
            .directory(&DirectoryQuery {
                include_offline: true,
                ..DirectoryQuery::default()
            })
            .unwrap();
        dir.rooms
            .iter()
            .flat_map(|r| &r.people)
            .flat_map(|p| &p.sessions)
            .map(|s| s.address.clone())
            .filter(|a| !a.contains(&me))
            .collect()
    }

    async fn fetch(&self, token: &str) -> crate::control::protocol::Fetched {
        self.courier
            .fetch(
                &self.conn(),
                FetchParams {
                    token: token.into(),
                    wait_seconds: None,
                    keep_unread: false,
                },
            )
            .await
            .unwrap()
    }
}

fn ulid() -> String {
    ulid::Ulid::new().to_string()
}

fn sam_peer() -> Peer {
    Peer {
        device: id(SAM_MAC),
        person: "sam".into(),
        device_name: "sam-mac".into(),
        own: false,
    }
}

const SAM_SESSION: &str = "01J9ZK00000000000000000SAM";

fn envelope(
    kind: &str,
    reply_to: Option<&str>,
    conversation: Option<&str>,
    to: (&str, &str),
) -> Value {
    let id = ulid();
    json!({
        "id": id,
        "conversation": conversation.unwrap_or(&id),
        "kind": kind,
        "in_reply_to": reply_to,
        "from": {"session": SAM_SESSION, "name": "claude-api", "client": "claude"},
        "to": {"session": to.0, "name": to.1},
        "body": "is the login fix merged?",
        "card": null,
        "repo": null,
        "sent_at": 1,
        "ttl_secs": 86400,
    })
}

async fn deliver(node: &Node, body: Value) -> Response {
    node.courier
        .handle_inbound(sam_peer(), Incoming::Deliver(body))
        .await
}

fn accepted(r: &Response) -> bool {
    matches!(r, Response::Receipt(d) if d.state == DeliverState::Accepted)
}

fn refused(r: &Response, code: RefusalCode) -> bool {
    matches!(r, Response::Receipt(d) if d.state == DeliverState::Refused && d.code == Some(code))
}

// A message maya's session sent to sam's session on sam-mac, stored as if it had gone out.
fn sent_to_sam(node: &Node, from: (&str, &str), kind: MessageKind) -> String {
    let id = ulid();
    let c = node.courier.core();
    let msg = Message {
        id: id.clone(),
        conversation: id.clone(),
        kind,
        in_reply_to: None,
        from: Party {
            device: node.courier.device().hex.clone(),
            device_name: "maya-mbp".into(),
            person: "maya".into(),
            session: from.0.into(),
            name: from.1.into(),
        },
        from_client: "claude".into(),
        to: Party {
            device: self::id(SAM_MAC).to_string(),
            device_name: "sam-mac".into(),
            person: "sam".into(),
            session: SAM_SESSION.into(),
            name: "claude-api".into(),
        },
        body: "which branch has it?".into(),
        card: None,
        repo: None,
        room: None,
        sent_at: 1,
        ttl_secs: DEFAULT_TTL_SECS,
        state: MessageState::Delivered,
        code: None,
    };
    c.store.insert_message(&msg, node.courier.now()).unwrap();
    id
}

#[tokio::test]
async fn deliver_runs_the_checks_in_order() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (_t, web, web_name) = maya.open("web", Some("team"));
    let (_p, private, private_name) = maya.open("notes", None);

    // Shape: malformed with an id is refused `invalid`; without one the call itself is malformed.
    let mut bad = envelope("ask", None, None, (&web, &web_name));
    bad["body"] = json!("");
    assert!(refused(&deliver(&maya, bad).await, RefusalCode::Invalid));
    assert_eq!(
        deliver(&maya, json!({"body": "x"})).await,
        Response::Error {
            code: ErrorCode::Malformed
        }
    );

    // Accepted once, then a duplicate.
    let ask = envelope("ask", None, None, (&web, &web_name));
    assert!(accepted(&deliver(&maya, ask.clone()).await));
    let again = deliver(&maya, ask).await;
    assert!(matches!(again, Response::Receipt(ref d) if d.state == DeliverState::Duplicate));

    // Threading: an answer needs a parent; a parent must be a message this device sent to the peer.
    let orphan = envelope("answer", None, None, (&web, &web_name));
    assert!(refused(&deliver(&maya, orphan).await, RefusalCode::Invalid));
    let unknown = ulid();
    let stray = envelope("tell", Some(&unknown), Some(&unknown), (&web, &web_name));
    assert!(refused(&deliver(&maya, stray).await, RefusalCode::Invalid));

    // Target: a private session is invisible to a teammate, and so is a session that doesn't exist.
    let hidden = envelope("ask", None, None, (&private, &private_name));
    assert!(refused(
        &deliver(&maya, hidden).await,
        RefusalCode::NoSuchSession
    ));
    let nobody = envelope(
        "ask",
        None,
        None,
        ("01J9ZK00000000000000000XYZ", "claude-x"),
    );
    assert!(refused(
        &deliver(&maya, nobody).await,
        RefusalCode::NoSuchSession
    ));

    // A session that ended says so, once it is visible.
    let (gone_token, gone, gone_name) = maya.open("api", Some("team"));
    maya.courier.close_session(&gone_token).unwrap();
    let late = envelope("ask", None, None, (&gone, &gone_name));
    assert!(refused(
        &deliver(&maya, late).await,
        RefusalCode::SessionEnded
    ));
}

#[tokio::test]
async fn replies_reach_a_private_session_that_wrote_first() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, private, name) = maya.open("notes", None);
    let tell_id = sent_to_sam(&maya, (&private, &name), MessageKind::Tell);
    let ask_id = sent_to_sam(&maya, (&private, &name), MessageKind::Ask);

    // A tell back is fine; an answer to a tell isn't.
    let back = envelope("tell", Some(&tell_id), Some(&tell_id), (&private, &name));
    assert!(accepted(&deliver(&maya, back).await));
    let wrong = envelope("answer", Some(&tell_id), Some(&tell_id), (&private, &name));
    assert!(refused(&deliver(&maya, wrong).await, RefusalCode::Invalid));

    // One answer per ask; the ask becomes answered and, with nobody waiting, a notice says so.
    let answer = envelope("answer", Some(&ask_id), Some(&ask_id), (&private, &name));
    assert!(accepted(&deliver(&maya, answer).await));
    let second = envelope("answer", Some(&ask_id), Some(&ask_id), (&private, &name));
    assert!(refused(&deliver(&maya, second).await, RefusalCode::Invalid));
    assert_eq!(
        maya.courier.sent_state(&ask_id),
        Some(MessageState::Answered)
    );

    let fetched = maya.fetch(&token).await;
    // A private session's room is shared with nobody, so the reply names no room.
    assert_eq!(fetched.messages[0].room, None);
    let kinds: Vec<MessageKind> = fetched.messages.iter().map(|m| m.kind).collect();
    assert_eq!(
        kinds,
        [MessageKind::Tell, MessageKind::Answer, MessageKind::Notice]
    );
    assert!(fetched.text.contains("[mesh-notice"));

    // Once the message it answers has run out of time, a reply to a private session isn't visible.
    let old = sent_to_sam(&maya, (&private, &name), MessageKind::Tell);
    maya.advance(DEFAULT_TTL_SECS * 1000 + 1);
    let stale = envelope("tell", Some(&old), Some(&old), (&private, &name));
    assert!(refused(
        &deliver(&maya, stale).await,
        RefusalCode::NoSuchSession
    ));
}

#[tokio::test]
async fn receipts_move_sent_messages_and_raise_notices() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, web, name) = maya.open("web", Some("team"));
    let shown = sent_to_sam(&maya, (&web, &name), MessageKind::Ask);
    let gone = sent_to_sam(&maya, (&web, &name), MessageKind::Tell);
    let receipt = |id: &str, state: ReceiptState, code: Option<RefusalCode>| {
        Incoming::Receipt(ReceiptUpdate {
            id: id.into(),
            state,
            code,
            at: 5,
        })
    };
    // From a device the message didn't go to: ignored.
    let stranger = Peer {
        device: id(SAM_LINUX),
        person: "sam".into(),
        device_name: "sam-linux".into(),
        own: false,
    };
    maya.courier
        .handle_inbound(stranger, receipt(&shown, ReceiptState::Shown, None))
        .await;
    assert_eq!(
        maya.courier.sent_state(&shown),
        Some(MessageState::Delivered)
    );

    maya.courier
        .handle_inbound(sam_peer(), receipt(&shown, ReceiptState::Shown, None))
        .await;
    assert_eq!(maya.courier.sent_state(&shown), Some(MessageState::Shown));
    maya.courier
        .handle_inbound(sam_peer(), receipt(&shown, ReceiptState::Answered, None))
        .await;
    assert_eq!(
        maya.courier.sent_state(&shown),
        Some(MessageState::Answered)
    );
    // Never backwards.
    maya.courier
        .handle_inbound(sam_peer(), receipt(&shown, ReceiptState::Shown, None))
        .await;
    assert_eq!(
        maya.courier.sent_state(&shown),
        Some(MessageState::Answered)
    );

    // A tell can't be answered, and a refusal always says why: both are ignored.
    maya.courier
        .handle_inbound(sam_peer(), receipt(&gone, ReceiptState::Answered, None))
        .await;
    maya.courier
        .handle_inbound(sam_peer(), receipt(&gone, ReceiptState::Refused, None))
        .await;
    assert_eq!(
        maya.courier.sent_state(&gone),
        Some(MessageState::Delivered)
    );
    maya.courier
        .handle_inbound(
            sam_peer(),
            receipt(
                &gone,
                ReceiptState::Refused,
                Some(RefusalCode::SessionEnded),
            ),
        )
        .await;
    assert_eq!(maya.courier.sent_state(&gone), Some(MessageState::Refused));
    let fetched = maya.fetch(&token).await;
    assert_eq!(fetched.messages.len(), 1);
    assert_eq!(fetched.messages[0].kind, MessageKind::Notice);
    assert!(fetched.messages[0].body.contains(&gone));
}

#[tokio::test]
async fn a_session_that_ends_refuses_its_unread_mail() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (a_token, _a, _) = maya.open("web", Some("team"));
    let (b_token, b, b_name) = maya.open("api", Some("team"));
    let sent = maya
        .courier
        .tell(TellParams {
            token: a_token.clone(),
            to: b_name.clone(),
            text: "the deploy is paused".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(sent.status, SendStatus::Delivered);
    // A peer's message to the same session.
    let from_sam = envelope("tell", None, None, (&b, &b_name));
    assert!(accepted(&deliver(&maya, from_sam).await));

    maya.courier.close_session(&b_token).unwrap();
    assert_eq!(
        maya.courier.sent_state(&sent.id),
        Some(MessageState::Refused)
    );
    // The local sender gets a notice; the peer gets a receipt through the outbox.
    let fetched = maya.fetch(&a_token).await;
    assert_eq!(fetched.messages.len(), 1);
    assert!(fetched.messages[0].body.contains("ended"));
    let queued = maya
        .courier
        .core()
        .store
        .outbox_for(&id(SAM_MAC).to_string())
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].kind, OutboxKind::Receipt);
    assert!(queued[0].payload.contains("session-ended"));
    // Its token is gone.
    assert!(maya.courier.close_session(&b_token).is_err());
}

#[tokio::test]
async fn presence_is_filtered_and_held_to_once_a_second() {
    let net = MemNet::new();
    let sam = Node::new(&net, SAM_MAC, "sam", "sam-mac");
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    sam.open("api", Some("team"));
    sam.open("notes", None);
    assert!(net.is_up(&id(SAM_MAC), &id(MAYA_MBP)));
    sam.courier.on_link_event(&LinkEvent::Up(id(MAYA_MBP)));
    maya.courier.on_link_event(&LinkEvent::Up(id(SAM_MAC)));
    let round = sam.courier.presence_round(None);
    assert_eq!(round.len(), 1);
    let (device, snapshot, hold) = round.into_iter().next().unwrap();
    assert_eq!(device, id(MAYA_MBP));
    assert!(hold.is_zero());
    // Maya shares only `team`: the private session stays home.
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].room.as_deref(), Some("team"));
    let resp = sam
        .courier
        .call(
            device,
            crate::wire::Request::Presence(snapshot),
            RECEIPT_WAIT,
        )
        .await
        .unwrap();
    assert_eq!(resp, Response::Ok);
    let dir = maya.courier.directory(&DirectoryQuery::default()).unwrap();
    let addresses: Vec<String> = dir
        .rooms
        .iter()
        .flat_map(|r| &r.people)
        .flat_map(|p| &p.sessions)
        .map(|s| s.address.clone())
        .collect();
    assert_eq!(addresses, ["sam@sam-mac/claude-api"]);
    // A second round right away is held back about a second.
    let again = sam.courier.presence_round(None);
    assert!(again[0].2 >= Duration::from_millis(1_200));
}

#[tokio::test]
async fn presence_hides_an_own_only_responder_from_teammates() {
    let net = MemNet::new();
    let mac = Node::new(&net, SAM_MAC, "sam", "sam-mac");
    let linux = Node::new(&net, SAM_LINUX, "sam", "sam-linux");
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    mac.open("web", Some("team"));
    let own = GateSettings {
        from: ServeFrom::Own,
        max_runs_per_person_per_hour: 10,
        max_runs_per_hour: 30,
    };
    let opened = mac
        .courier
        .open_session(
            &mac.conn(),
            responder_open(&mac, SessionKind::Responder, Some(own)),
        )
        .unwrap();
    for (node, peer) in [(&linux, SAM_LINUX), (&maya, MAYA_MBP)] {
        assert!(net.is_up(&id(SAM_MAC), &id(peer)));
        mac.courier.on_link_event(&LinkEvent::Up(id(peer)));
        node.courier.on_link_event(&LinkEvent::Up(id(SAM_MAC)));
    }
    let kinds = |device: u8| -> Vec<(SessionKind, String)> {
        let round = mac.courier.presence_round(None);
        let (_, snapshot, _) = round
            .into_iter()
            .find(|(d, _, _)| *d == id(device))
            .unwrap();
        snapshot
            .sessions
            .into_iter()
            .map(|s| (s.kind, s.session))
            .collect()
    };
    let to_maya = kinds(MAYA_MBP);
    assert_eq!(to_maya.len(), 1, "{to_maya:?}");
    assert_eq!(to_maya[0].0, SessionKind::Live);
    let to_linux = kinds(SAM_LINUX);
    assert!(
        to_linux.contains(&(SessionKind::Responder, opened.session.clone())),
        "{to_linux:?}"
    );

    // Serving the room again shows it to maya.
    let room = GateSettings {
        from: ServeFrom::Room,
        ..own
    };
    mac.courier
        .open_session(
            &mac.conn(),
            responder_open(&mac, SessionKind::Responder, Some(room)),
        )
        .unwrap();
    assert!(kinds(MAYA_MBP).contains(&(SessionKind::Responder, opened.session)));
}

fn presence_session(
    session: &str,
    name: &str,
    room: Option<&str>,
    cards: &[&str],
) -> PresenceSession {
    PresenceSession {
        session: session.into(),
        name: name.into(),
        client: "claude".into(),
        kind: SessionKind::Live,
        repo: None,
        branch: None,
        place: None,
        room: room.map(str::to_string),
        cards: cards.iter().map(|c| c.to_string()).collect(),
        activity: None,
        state: ActivityState::Idle,
        since: 1,
        reach: Reach::Hooked,
        queue: 0,
    }
}

fn presence(seq: u64, sessions: Vec<PresenceSession>) -> Incoming {
    let snapshot = PresenceSnapshot {
        seq,
        device: PresenceDevice {
            name: "sam-mac".into(),
            platform: "macos".into(),
            version: "0.1.0".into(),
        },
        sessions,
    };
    Incoming::Presence(serde_json::to_value(snapshot).unwrap())
}

fn roster_without_sam() -> (Me, Roster) {
    let text = format!(
        "[[person]]\nname = \"maya\"\ndevices = [ {{ id = \"{}\", name = \"maya-mbp\" }} ]\n",
        id(MAYA_MBP)
    );
    file::parse(&text)
        .unwrap()
        .roster_for(Some("maya"), &id(MAYA_MBP), 2_000)
        .unwrap()
}

#[tokio::test]
async fn the_guard_refuses_a_busy_conversation_and_a_refused_message_comes_back_later() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (_t, web, web_name) = maya.open("web", Some("team"));
    // Maya's session wrote first; sam's session keeps replying in that conversation.
    let root = sent_to_sam(&maya, (&web, &web_name), MessageKind::Tell);
    let reply = || envelope("tell", Some(&root), Some(&root), (&web, &web_name));
    for n in 0..CONVERSATION_LIMIT {
        assert!(accepted(&deliver(&maya, reply()).await), "message {n}");
    }
    let one_too_many = reply();
    assert!(refused(
        &deliver(&maya, one_too_many.clone()).await,
        RefusalCode::ConversationLimit
    ));
    // A refusal writes no dedupe key (5.5 step 8): the same message is accepted once the window has passed.
    maya.advance(CONVERSATION_WINDOW_MS);
    assert!(accepted(&deliver(&maya, one_too_many.clone()).await));
    let again = deliver(&maya, one_too_many).await;
    assert!(matches!(again, Response::Receipt(ref d) if d.state == DeliverState::Duplicate));
}

#[tokio::test]
async fn the_guard_holds_between_sessions_on_one_device() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (a, _, _) = maya.open("web", Some("team"));
    let (b, _, b_name) = maya.open("api", Some("team"));
    let first = maya
        .courier
        .tell(TellParams {
            token: a.clone(),
            to: b_name,
            text: "can you look at the login bug?".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(first.status, SendStatus::Delivered);
    // Two agents replying to each other in one conversation.
    let mut last = first.id;
    for n in 1..CONVERSATION_LIMIT {
        let from = if n % 2 == 1 { &b } else { &a };
        let r = maya.reply(from, &last, "more on that", None).await;
        assert_eq!(r.status, SendStatus::Delivered, "message {n}");
        last = r.id;
    }
    let from = if CONVERSATION_LIMIT % 2 == 1 { &b } else { &a };
    let stopped = maya.reply(from, &last, "and another thing", None).await;
    assert_eq!(stopped.status, SendStatus::Refused);
    assert_eq!(stopped.code.as_deref(), Some("conversation-limit"));
    assert!(
        stopped
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("check with your person"))
    );
    // A follow-up question is held the same way.
    let asked = maya.reply(from, &last, "should I go on?", Some(0)).await;
    assert_eq!(asked.status, SendStatus::Refused);
    assert_eq!(asked.code.as_deref(), Some("conversation-limit"));
}

#[tokio::test]
async fn a_device_that_leaves_every_shared_room_loses_its_mail_and_presence() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, web, web_name) = maya.open("web", Some("team"));
    let shown = maya
        .courier
        .handle_inbound(
            sam_peer(),
            presence(
                2_000_000,
                vec![presence_session(
                    SAM_SESSION,
                    "claude-api",
                    Some("team"),
                    &[],
                )],
            ),
        )
        .await;
    assert_eq!(shown, Response::Ok);
    assert_eq!(maya.listed(), ["sam@sam-mac/claude-api"]);
    // Sam asks; maya answers while sam-mac is offline, so the answer waits beside the answered receipt.
    let ask = envelope("ask", None, None, (&web, &web_name));
    let ask_id = ask["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, ask).await));
    let answer = maya.reply(&token, &ask_id, "yes, it's on main", None).await;
    assert_eq!(answer.status, SendStatus::Queued);
    let kinds = maya.outbox_kinds(SAM_MAC);
    assert!(kinds.contains(&OutboxKind::Deliver) && kinds.contains(&OutboxKind::Receipt));
    let later = envelope("tell", None, None, (&web, &web_name));
    let later_id = later["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, later).await));

    let (me, roster) = roster_without_sam();
    maya.courier.apply_roster(me, roster, RosterInfo::default());

    // A reply to a device that left is refused at once instead of waiting a day in the outbox.
    let too_late = maya.reply(&token, &later_id, "thanks", None).await;
    assert_eq!(too_late.status, SendStatus::Refused);
    assert_eq!(too_late.code.as_deref(), Some(NOT_SHARED));
    assert!(
        too_late.note.contains("no longer in a shared room"),
        "{}",
        too_late.note
    );

    let stored = maya
        .courier
        .core()
        .store
        .message(&maya.courier.device().hex, &answer.id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.message.state, MessageState::Refused);
    assert_eq!(stored.message.code.as_deref(), Some(NOT_SHARED));
    assert!(maya.outbox_kinds(SAM_MAC).is_empty());
    assert!(
        maya.courier
            .core()
            .store
            .presence(&id(SAM_MAC).to_string())
            .unwrap()
            .is_none()
    );
    assert!(maya.listed().is_empty());
    let f = maya.fetch(&token).await;
    let notice = f
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::Notice)
        .unwrap();
    assert!(notice.body.contains(&answer.id));
    assert!(notice.body.contains("no longer in a shared room"));
}

#[tokio::test]
async fn mail_that_runs_out_of_time_expires_with_notices_and_receipts() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (a, _, _) = maya.open("web", Some("team"));
    let (b_token, b_id, b_name) = maya.open("api", Some("team"));
    // Queued for offline sam-mac: a reply to something sam sent.
    let ask = envelope("ask", None, None, (&b_id, &b_name));
    let ask_id = ask["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, ask).await));
    let tell = envelope("tell", None, None, (&b_id, &b_name));
    assert!(accepted(&deliver(&maya, tell).await));
    let queued = maya
        .reply(&b_token, &ask_id, "on main since monday", None)
        .await;
    assert_eq!(queued.status, SendStatus::Queued);
    // Delivered to a local session that never looks.
    let local = maya
        .courier
        .tell(TellParams {
            token: a.clone(),
            to: b_name.clone(),
            text: "the staging deploy is paused".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(local.status, SendStatus::Delivered);

    maya.advance(DEFAULT_TTL_SECS * 1000);
    maya.courier.tick();

    // Never accepted by sam-mac: expired, and the sender is told it wasn't delivered.
    assert_eq!(
        maya.courier.sent_state(&queued.id),
        Some(MessageState::Expired)
    );
    // Delivered but never seen: expired; the local sender gets a notice, sam-mac an expired receipt.
    assert_eq!(
        maya.courier.sent_state(&local.id),
        Some(MessageState::Expired)
    );
    let receipts = maya
        .courier
        .core()
        .store
        .outbox_for(&id(SAM_MAC).to_string())
        .unwrap();
    assert!(
        receipts
            .iter()
            .any(|e| e.kind == OutboxKind::Receipt && e.payload.contains("\"expired\""))
    );
    assert!(!receipts.iter().any(|e| e.kind == OutboxKind::Deliver));
    let to_a = maya.fetch(&a).await;
    assert_eq!(to_a.messages.len(), 1);
    assert!(to_a.messages[0].body.contains(&local.id));
    assert!(to_a.messages[0].body.contains("before that session saw it"));
    let to_b = maya.fetch(&b_token).await;
    assert!(to_b.messages.iter().any(|m| {
        m.kind == MessageKind::Notice
            && m.body.contains(&queued.id)
            && m.body.contains("before it reached that device")
    }));
}

#[tokio::test]
async fn a_responder_with_a_full_queue_is_busy() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, helper, helper_name) = maya.open_responder("web", "team");
    for n in 0..crate::responder::limits::MAX_WAITING {
        let ask = envelope("ask", None, None, (&helper, &helper_name));
        assert!(accepted(&deliver(&maya, ask).await), "question {n}");
    }
    let ask = envelope("ask", None, None, (&helper, &helper_name));
    assert!(refused(
        &deliver(&maya, ask.clone()).await,
        RefusalCode::Busy
    ));
    // Taking a question makes room; the refused one is accepted when sent again.
    let taken = maya
        .courier
        .take(
            &maya.conn(),
            TakeParams {
                token,
                wait_seconds: 0,
            },
        )
        .await
        .unwrap();
    assert!(taken.question.is_some());
    assert!(accepted(&deliver(&maya, ask).await));
}

#[tokio::test]
async fn a_responder_answers_an_ask_only_once() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, helper, helper_name) = maya.open_responder("web", "team");
    let ask = envelope("ask", None, None, (&helper, &helper_name));
    let ask_id = ask["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, ask).await));
    let first = maya.reply(&token, &ask_id, "on main", None).await;
    assert_eq!(first.status, SendStatus::Queued);
    // The same reply sent again after a dropped connection is refused instead of going out as a tell.
    let again = maya
        .courier
        .reply(
            &maya.conn(),
            ReplyParams {
                token: token.clone(),
                id: ask_id.clone(),
                text: "on main".into(),
                wait_seconds: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(again.reason, Reason::Conflict);
    assert!(
        again.message.contains("already answered"),
        "{}",
        again.message
    );
    let delivers = |node: &Node| {
        node.outbox_kinds(SAM_MAC)
            .into_iter()
            .filter(|k| *k == OutboxKind::Deliver)
            .count()
    };
    assert_eq!(delivers(&maya), 1);

    // The same goes for a reply to a tell.
    let tell = envelope("tell", None, None, (&helper, &helper_name));
    let tell_id = tell["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, tell).await));
    let first = maya.reply(&token, &tell_id, "noted", None).await;
    assert_eq!(first.status, SendStatus::Queued);
    let again = maya
        .courier
        .reply(
            &maya.conn(),
            ReplyParams {
                token: token.clone(),
                id: tell_id.clone(),
                text: "noted".into(),
                wait_seconds: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(again.reason, Reason::Conflict);
    assert_eq!(delivers(&maya), 2);

    // A live session's second reply still goes, as a tell.
    let (live_token, live, live_name) = maya.open("api", Some("team"));
    let ask = envelope("ask", None, None, (&live, &live_name));
    let ask_id = ask["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, ask).await));
    maya.reply(&live_token, &ask_id, "yes", None).await;
    let tell = maya
        .reply(&live_token, &ask_id, "and one more thing", None)
        .await;
    assert_eq!(tell.status, SendStatus::Queued);
    assert_eq!(delivers(&maya), 4);
}

#[tokio::test]
async fn a_responder_serving_only_its_owner_hides_from_teammates() {
    let net = MemNet::new();
    let serve = crate::responder::ServeSettings {
        from: crate::responder::ServeFrom::Own,
        ..Default::default()
    };
    let maya = Node::with_serve(&net, MAYA_MBP, "maya", "maya-mbp", serve);
    let (_t, helper, helper_name) = maya.open_responder("web", "team");
    let ask = envelope("ask", None, None, (&helper, &helper_name));
    assert!(refused(
        &deliver(&maya, ask).await,
        RefusalCode::NoSuchSession
    ));
}

#[tokio::test]
async fn the_first_retry_comes_ten_seconds_after_the_first_attempt() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, web, web_name) = maya.open("web", Some("team"));
    let first = envelope("tell", None, None, (&web, &web_name));
    let first_id = first["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, first).await));
    let second = envelope("tell", None, None, (&web, &web_name));
    let second_id = second["id"].as_str().unwrap().to_string();
    assert!(accepted(&deliver(&maya, second).await));
    let deliver_entry = |maya: &Node, message: &str| {
        let c = maya.courier.core();
        let seq = c
            .store
            .message(&maya.courier.device().hex, message)
            .unwrap()
            .unwrap()
            .seq;
        c.store
            .outbox_for(&id(SAM_MAC).to_string())
            .unwrap()
            .into_iter()
            .find(|e| e.message_seq == Some(seq))
            .unwrap()
    };

    // The courier thinks sam-mac is online, but the attempt inside the send fails.
    maya.courier.on_link_event(&LinkEvent::Up(id(SAM_MAC)));
    let tried = maya.reply(&token, &first_id, "thanks", None).await;
    assert_eq!(tried.status, SendStatus::Queued);
    let entry = deliver_entry(&maya, &tried.id);
    assert_eq!(entry.attempts, 1);
    assert_eq!(entry.next_attempt_at, maya.courier.now() + 10_000);

    // Offline from the start counts the same: one failed attempt, the next in 10 s, then 30 s.
    maya.courier.on_link_event(&LinkEvent::Down {
        device: id(SAM_MAC),
        last_seen_ms: 1,
    });
    let offline = maya.reply(&token, &second_id, "noted", None).await;
    assert_eq!(offline.status, SendStatus::Queued);
    let entry = deliver_entry(&maya, &offline.id);
    assert_eq!(entry.attempts, 1);
    assert_eq!(entry.next_attempt_at, maya.courier.now() + 10_000);
    maya.advance(10_000);
    maya.courier.pump(id(SAM_MAC), false).await;
    let entry = deliver_entry(&maya, &offline.id);
    assert_eq!(entry.attempts, 2);
    assert_eq!(entry.next_attempt_at, maya.courier.now() + 30_000);
}

#[tokio::test]
async fn a_stop_hook_never_carries_messages() {
    use crate::agents::Client as Agent;
    use crate::agents::hook::{Facts, HookCall, MeshEvent};
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let cwd = maya.dir.path().join("api");
    std::fs::create_dir_all(&cwd).unwrap();
    let call = |event| HookCall {
        client: Agent::Claude,
        event,
        client_session: "cc-1".into(),
        client_pid: None,
        cwd: cwd.to_string_lossy().into_owned(),
        show: true,
        facts: Facts::default(),
    };
    let started = maya.courier.hook_event(call(MeshEvent::Start)).unwrap();
    let hooked = started.address.rsplit('/').next().unwrap().to_string();
    let (a, _, _) = maya.open("web", Some("team"));
    let sent = maya
        .courier
        .tell(TellParams {
            token: a,
            to: hooked,
            text: "the staging deploy is paused".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(sent.status, SendStatus::Delivered);
    let stop = maya.courier.hook_event(call(MeshEvent::Stop)).unwrap();
    assert!(stop.context.is_none());
    assert!(stop.ack_ids.is_empty());
    let prompt = maya.courier.hook_event(call(MeshEvent::Prompt)).unwrap();
    assert!(
        prompt
            .context
            .is_some_and(|t| t.contains("the staging deploy is paused"))
    );
    assert_eq!(prompt.ack_ids, [sent.id]);
}

#[tokio::test]
async fn an_unanswered_ask_stops_coming_back_once_its_time_is_up() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, web, web_name) = maya.open("web", Some("team"));
    let ask = envelope("ask", None, None, (&web, &web_name));
    assert!(accepted(&deliver(&maya, ask).await));
    let first = maya.fetch(&token).await;
    assert_eq!(first.messages.len(), 1);
    assert!(!first.messages[0].again);
    let second = maya.fetch(&token).await;
    assert_eq!(second.messages.len(), 1);
    assert!(second.messages[0].again);
    maya.advance(DEFAULT_TTL_SECS * 1000);
    assert!(maya.fetch(&token).await.messages.is_empty());
}

#[tokio::test]
async fn one_fetch_returns_at_most_64_kib() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let (token, web, web_name) = maya.open("web", Some("team"));
    for n in 0..3 {
        let mut tell = envelope("tell", None, None, (&web, &web_name));
        tell["body"] = json!(format!("{n}{}", "x".repeat(30_000)));
        assert!(accepted(&deliver(&maya, tell).await));
    }
    let first = maya.fetch(&token).await;
    assert!(first.text.len() <= super::inbox::FETCH_BUDGET);
    assert_eq!(first.unread, 1);
    let rest = maya.fetch(&token).await;
    assert_eq!(rest.messages.len(), 1);
    assert!(rest.messages[0].body.starts_with('2'));
    assert_eq!(rest.unread, 0);
}

// mesh#10: the `[serve]` limits a responder sends in `session.open` (9).
fn responder_open(node: &Node, kind: SessionKind, serve: Option<GateSettings>) -> SessionOpen {
    let cwd = node.dir.path().join("api");
    std::fs::create_dir_all(&cwd).unwrap();
    SessionOpen {
        client: "claude".into(),
        client_session: Some("serve-test".into()),
        client_pid: None,
        cwd: cwd.to_string_lossy().into_owned(),
        client_name: None,
        kind,
        name: None,
        spew_project: Some("team".into()),
        serve,
    }
}

#[tokio::test]
async fn a_responders_own_limits_apply_when_questions_arrive() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let one = GateSettings {
        from: ServeFrom::Room,
        max_runs_per_person_per_hour: 1,
        max_runs_per_hour: 30,
    };

    // Only a responder sends limits, and only in range.
    let live = maya.courier.open_session(
        &maya.conn(),
        responder_open(&maya, SessionKind::Live, Some(one)),
    );
    assert_eq!(live.unwrap_err().reason, Reason::Invalid);
    let huge = GateSettings {
        max_runs_per_hour: MAX_RUN_LIMIT + 1,
        ..one
    };
    let refused_open = maya.courier.open_session(
        &maya.conn(),
        responder_open(&maya, SessionKind::Responder, Some(huge)),
    );
    assert_eq!(refused_open.unwrap_err().reason, Reason::Invalid);

    let conn = maya.conn();
    let opened = maya
        .courier
        .open_session(
            &conn,
            responder_open(&maya, SessionKind::Responder, Some(one)),
        )
        .unwrap();
    let to = (opened.session.as_str(), opened.name.as_str());
    assert!(accepted(
        &deliver(&maya, envelope("ask", None, None, to)).await
    ));
    // Over sam's limit: busy, and the refused ask isn't counted.
    assert!(refused(
        &deliver(&maya, envelope("ask", None, None, to)).await,
        RefusalCode::Busy
    ));

    // A re-open keeps the count; `from = own` turns a teammate away as if the responder weren't there.
    let own = GateSettings {
        from: ServeFrom::Own,
        max_runs_per_person_per_hour: 5,
        ..one
    };
    maya.courier
        .open_session(
            &conn,
            responder_open(&maya, SessionKind::Responder, Some(own)),
        )
        .unwrap();
    assert!(refused(
        &deliver(&maya, envelope("ask", None, None, to)).await,
        RefusalCode::NoSuchSession
    ));
    let two = GateSettings {
        max_runs_per_person_per_hour: 2,
        ..one
    };
    maya.courier
        .open_session(
            &conn,
            responder_open(&maya, SessionKind::Responder, Some(two)),
        )
        .unwrap();
    assert!(accepted(
        &deliver(&maya, envelope("ask", None, None, to)).await
    ));
    assert!(refused(
        &deliver(&maya, envelope("ask", None, None, to)).await,
        RefusalCode::Busy
    ));
}

#[tokio::test]
async fn a_responder_that_lapsed_and_re_opened_keeps_its_counts() {
    let net = MemNet::new();
    let maya = Node::new(&net, MAYA_MBP, "maya", "maya-mbp");
    let one = GateSettings {
        from: ServeFrom::Room,
        max_runs_per_person_per_hour: 1,
        max_runs_per_hour: 30,
    };
    let open = |client_session: &str| {
        let mut p = responder_open(&maya, SessionKind::Responder, Some(one));
        p.client_session = Some(client_session.into());
        maya.courier.open_session(&maya.conn(), p).unwrap()
    };
    let first = open("serve-a");
    let to = (first.session.as_str(), first.name.as_str());
    assert!(accepted(
        &deliver(&maya, envelope("ask", None, None, to)).await
    ));
    maya.courier.close_session(&first.token).unwrap();

    // The same serve process opens again and gets a new session: sam's run this hour still counts.
    let again = open("serve-a");
    assert_ne!(again.session, first.session);
    let to = (again.session.as_str(), again.name.as_str());
    assert!(refused(
        &deliver(&maya, envelope("ask", None, None, to)).await,
        RefusalCode::Busy
    ));
    maya.courier.close_session(&again.token).unwrap();

    // Another serve process starts with its own counts.
    let other = open("serve-b");
    let to = (other.session.as_str(), other.name.as_str());
    assert!(accepted(
        &deliver(&maya, envelope("ask", None, None, to)).await
    ));
}

#[tokio::test]
async fn asks_from_own_devices_count_only_toward_the_total() {
    let net = MemNet::new();
    let sam = Node::new(&net, SAM_MAC, "sam", "sam-mac");
    let limits = GateSettings {
        from: ServeFrom::Own,
        max_runs_per_person_per_hour: 1,
        max_runs_per_hour: 3,
    };
    let opened = sam
        .courier
        .open_session(
            &sam.conn(),
            responder_open(&sam, SessionKind::Responder, Some(limits)),
        )
        .unwrap();
    let to = (opened.session.as_str(), opened.name.as_str());
    let linux = Peer {
        device: id(SAM_LINUX),
        person: "sam".into(),
        device_name: "sam-linux".into(),
        own: true,
    };
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(
            sam.courier
                .handle_inbound(
                    linux.clone(),
                    Incoming::Deliver(envelope("ask", None, None, to)),
                )
                .await,
        );
    }
    assert!(answers[..3].iter().all(accepted), "{answers:?}");
    assert!(refused(&answers[3], RefusalCode::Busy), "{answers:?}");
}
