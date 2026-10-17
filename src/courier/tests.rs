//! Courier tests: the `deliver` checks of 5.5, receipts and notices, against couriers on an in-memory network.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use serde_json::{Value, json};
use tokio::sync::watch;

use super::*;
use crate::control::protocol::{FetchParams, Reason, SendStatus, SessionOpen, TellParams};
use crate::daemon::links::InboundDyn;
use crate::daemon::testnet::{MemLinks, MemNet};
use crate::link::Peer;
use crate::responder::limits::{GateSettings, MAX_RUN_LIMIT};
use crate::roster::file;
use crate::settings::ServeFrom;
use crate::wire::{DeliverState, ErrorCode, Incoming, Response};

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
            serve: Default::default(),
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
    assert!(again[0].2 >= Duration::from_millis(900));
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
