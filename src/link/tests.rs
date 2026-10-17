//! In-process tests of the link layer: endpoints on loopback with relays and mDNS off, found through the roster's
//! address hints. Nothing here waits a fixed time; every wait is for an event or a condition, with a deadline.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{
    Connection, ConnectionError, PortmapperConfig, RecvStream, SendStream, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use tokio::sync::{broadcast, mpsc};

use super::*;
use crate::roster::{DeviceEntry, Me, Member, PRIVATE_ROOM_ID, Room, RoomKind, Roster};
use crate::settings::{NetworkMode, NetworkSettings};
use crate::wire::{
    ALPN, CallKind, DeliverReceipt, DeliverState, Envelope, ErrorCode, Hello, Incoming, MAX_FRAME,
    MessageKind, PresenceDevice, PresenceSnapshot, Recipient, RefusalCode, Request, Response,
    Sender, encode_frame, read_frame,
};

/// Deadline for anything a test waits on.
const WAIT: Duration = Duration::from_secs(10);

fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes(&[seed; 32])
}

/// Two keys, the first being the designated dialer of the pair (the greater endpoint id).
fn ordered(a: u8, b: u8) -> (SecretKey, SecretKey) {
    let (ka, kb) = (key(a), key(b));
    if ka.public().as_bytes() > kb.public().as_bytes() {
        (ka, kb)
    } else {
        (kb, ka)
    }
}

fn unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn fast() -> Timing {
    Timing {
        first_frame: Duration::from_secs(2),
        dial_timeout: Duration::from_secs(3),
        backoff_base: Duration::from_millis(50),
        backoff_max: Duration::from_millis(400),
        lazy_dial_after: Duration::from_secs(30),
        call_timeout: Duration::from_secs(5),
        keep_alive: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(30),
    }
}

fn loopback() -> NetworkSettings {
    NetworkSettings {
        mode: NetworkMode::Local,
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: 0,
        mdns: false,
        relays: Vec::new(),
        relay_token: None,
    }
}

/// Another device as this device's roster lists it.
#[derive(Clone)]
struct Other {
    id: EndpointId,
    person: &'static str,
    name: &'static str,
    addrs: Vec<SocketAddr>,
}

fn other(id: EndpointId, person: &'static str, name: &'static str) -> Other {
    Other {
        id,
        person,
        name,
        addrs: Vec::new(),
    }
}

impl Other {
    fn at(mut self, addrs: Vec<SocketAddr>) -> Self {
        self.addrs = addrs;
        self
    }
}

/// The admission of device `me`, of person `me_person`, whose roster has one team room with everyone in
/// `others`; other devices of `me_person` are in the private room too.
fn admission_as(me_person: &str, me: EndpointId, others: &[Other]) -> Admission {
    let entry = |o: &Other| DeviceEntry {
        endpoint_id: o.id,
        name: o.name.into(),
        relay: None,
        addrs: o.addrs.clone(),
    };
    let mut own = vec![DeviceEntry {
        endpoint_id: me,
        name: "this-device".into(),
        relay: None,
        addrs: Vec::new(),
    }];
    own.extend(others.iter().filter(|o| o.person == me_person).map(entry));
    let mut team: Vec<Member> = vec![Member {
        username: me_person.into(),
        display: String::new(),
        devices: own.clone(),
    }];
    for o in others.iter().filter(|o| o.person != me_person) {
        match team.iter_mut().find(|m| m.username == o.person) {
            Some(m) => m.devices.push(entry(o)),
            None => team.push(Member {
                username: o.person.into(),
                display: String::new(),
                devices: vec![entry(o)],
            }),
        }
    }
    let who = Me {
        username: me_person.into(),
        display: me_person.into(),
    };
    let mut roster = Roster {
        version: "v1".into(),
        fetched_at: 0,
        rooms: vec![
            Room {
                id: PRIVATE_ROOM_ID.into(),
                name: "private".into(),
                kind: RoomKind::Private,
                members: vec![Member {
                    username: me_person.into(),
                    display: String::new(),
                    devices: own,
                }],
            },
            Room {
                id: "team".into(),
                name: "team".into(),
                kind: RoomKind::Team,
                members: team,
            },
        ],
    };
    roster.validate(&who).expect("a valid test roster");
    Admission::compute(&who, &me, &roster)
}

/// The admission of one of sam's devices.
fn admission(me: EndpointId, others: &[Other]) -> Admission {
    admission_as("sam", me, others)
}

/// An [`Inbound`] that answers every call and reports it.
struct Recorder {
    tx: mpsc::UnboundedSender<(Peer, Incoming)>,
}

impl Inbound for Recorder {
    async fn handle(&self, peer: Peer, call: Incoming) -> Response {
        let response = match &call {
            Incoming::Deliver(body) => Response::Receipt(DeliverReceipt {
                id: body["id"].as_str().unwrap_or_default().to_string(),
                state: DeliverState::Accepted,
                code: None,
            }),
            Incoming::Presence(_) | Incoming::Receipt(_) => Response::Ok,
            Incoming::Ping => Response::Pong,
            Incoming::Hello(_) => Response::Error {
                code: ErrorCode::Malformed,
            },
        };
        let _ = self.tx.send((peer, call));
        response
    }
}

struct Node {
    id: EndpointId,
    links: IrohLinks,
    events: broadcast::Receiver<LinkEvent>,
    calls: mpsc::UnboundedReceiver<(Peer, Incoming)>,
}

async fn node_with(
    key: SecretKey,
    admission: Admission,
    network: NetworkSettings,
    timing: Timing,
) -> Node {
    let (tx, calls) = mpsc::unbounded_channel();
    let config = LinkConfig {
        secret_key: key.clone(),
        device_name: "this-device".into(),
        network,
        timing,
    };
    let links = IrohLinks::start(config, admission, Arc::new(Recorder { tx }))
        .await
        .expect("links start");
    let events = links.events();
    Node {
        id: key.public(),
        links,
        events,
        calls,
    }
}

async fn node(key: SecretKey, admission: Admission, timing: Timing) -> Node {
    node_with(key, admission, loopback(), timing).await
}

impl Node {
    fn addrs(&self) -> Vec<SocketAddr> {
        let addrs = self.links.bound_sockets();
        assert!(!addrs.is_empty(), "the endpoint is bound");
        addrs
    }

    async fn next_event(&mut self) -> LinkEvent {
        match tokio::time::timeout(WAIT, self.events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => panic!("event channel: {error}"),
            Err(_) => panic!("no link event within {WAIT:?}"),
        }
    }

    async fn expect_up(&mut self, device: EndpointId) {
        assert_eq!(self.next_event().await, LinkEvent::Up(device));
    }

    async fn expect_down(&mut self, device: EndpointId) -> u64 {
        match self.next_event().await {
            LinkEvent::Down {
                device: d,
                last_seen_ms,
            } if d == device => last_seen_ms,
            other => panic!("expected Down for {device}, got {other:?}"),
        }
    }

    fn assert_no_event(&mut self) {
        match self.events.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            other => panic!("unexpected link event: {other:?}"),
        }
    }

    async fn next_call(&mut self) -> (Peer, Incoming) {
        tokio::time::timeout(WAIT, self.calls.recv())
            .await
            .expect("a call within the deadline")
            .expect("calls channel open")
    }

    /// The kinds of the calls handed to the daemon so far, without waiting.
    fn drain_calls(&mut self) -> Vec<CallKind> {
        let mut kinds = Vec::new();
        while let Ok((_, call)) = self.calls.try_recv() {
            kinds.push(call.kind());
        }
        kinds
    }

    fn has_problem(&self, device: EndpointId, problem: &str) -> bool {
        self.links
            .status()
            .problems
            .iter()
            .any(|p| p.device == device.to_string() && p.problem == problem)
    }
}

/// Two linked devices: `d`, sam's, is the designated dialer; `o` is maya's.
async fn pair(timing: Timing) -> (Node, Node) {
    let (kd, ko) = ordered(1, 2);
    let mut o = node(
        ko.clone(),
        admission_as("maya", ko.public(), &[other(kd.public(), "sam", "sam-mbp")]),
        timing,
    )
    .await;
    let o_addrs = o.addrs();
    let mut d = node(
        kd.clone(),
        admission(
            kd.public(),
            &[other(ko.public(), "maya", "maya-mbp").at(o_addrs)],
        ),
        timing,
    )
    .await;
    d.expect_up(o.id).await;
    o.expect_up(d.id).await;
    (d, o)
}

fn ulid() -> String {
    ulid::Ulid::new().to_string()
}

fn envelope() -> Envelope {
    let id = ulid();
    Envelope {
        id: id.clone(),
        conversation: id,
        kind: MessageKind::Tell,
        in_reply_to: None,
        from: Sender {
            session: ulid(),
            name: "claude-api".into(),
            client: "claude".into(),
        },
        to: Recipient {
            session: ulid(),
            name: "codex-api".into(),
        },
        body: "hello there".into(),
        card: None,
        repo: None,
        sent_at: 1,
        ttl_secs: 60,
    }
}

fn snapshot(seq: u64) -> PresenceSnapshot {
    PresenceSnapshot {
        seq,
        device: PresenceDevice {
            name: "sam-mbp".into(),
            platform: "linux".into(),
            version: "0.1.0".into(),
        },
        sessions: Vec::new(),
    }
}

/// Waits for a condition that has no event of its own, checking every 10 ms up to the deadline.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---- raw peers: plain iroh endpoints that speak frames by hand ----

async fn raw_endpoint(key: SecretKey) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .portmapper_config(PortmapperConfig::Disabled)
        .address_lookup(MemoryLookup::new())
        .clear_ip_transports()
        .bind_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .expect("bind address")
        .bind()
        .await
        .expect("raw endpoint")
}

fn addr_of(node: &Node) -> EndpointAddr {
    node.addrs()
        .into_iter()
        .fold(EndpointAddr::new(node.id), EndpointAddr::with_ip_addr)
}

async fn raw_connect(ep: &Endpoint, to: &Node) -> Connection {
    tokio::time::timeout(WAIT, ep.connect(addr_of(to), ALPN))
        .await
        .expect("connect in time")
        .expect("connect")
}

fn raw_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// Sends raw bytes as one call and reads the response frame.
async fn raw_call(conn: &Connection, bytes: &[u8]) -> Response {
    let exchange = async {
        let (mut send, mut recv) = conn.open_bi().await.expect("open stream");
        send.write_all(bytes).await.expect("write");
        send.finish().expect("finish");
        let payload = read_frame(&mut recv).await.expect("a response frame");
        Response::parse(&payload).expect("a valid response")
    };
    tokio::time::timeout(WAIT, exchange)
        .await
        .expect("response in time")
}

async fn raw_request(conn: &Connection, request: &Request) -> Response {
    raw_call(conn, &encode_frame(request).unwrap()).await
}

async fn raw_hello(conn: &Connection) -> Response {
    raw_request(conn, &Request::Hello(Hello::ours("raw-peer"))).await
}

fn hello_with(min: u32, max: u32) -> Hello {
    Hello {
        proto_min: min,
        proto_max: max,
        ..Hello::ours("raw-peer")
    }
}

async fn closed_code(conn: &Connection) -> u64 {
    let reason = tokio::time::timeout(WAIT, conn.closed())
        .await
        .expect("connection closed in time");
    match reason {
        ConnectionError::ApplicationClosed(close) => close.error_code.into_inner(),
        other => panic!("expected an application close, got {other}"),
    }
}

/// Answers calls on a raw connection: `hello` with `hello`, `ping` with `pong`, anything else with `ok`.
fn raw_serve(conn: Connection, hello: Hello) {
    tokio::spawn(async move {
        while let Ok((send, recv)) = conn.accept_bi().await {
            tokio::spawn(raw_answer(send, recv, hello.clone()));
        }
    });
}

async fn raw_answer(mut send: SendStream, mut recv: RecvStream, hello: Hello) {
    let Ok(payload) = read_frame(&mut recv).await else {
        return;
    };
    let response = match crate::wire::parse_incoming(&payload) {
        Ok(Incoming::Hello(_)) => Response::Hello(hello),
        Ok(Incoming::Ping) => Response::Pong,
        _ => Response::Ok,
    };
    let _ = crate::wire::write_frame(&mut send, &response).await;
    let _ = send.finish();
}

fn poll_once<F: Future>(fut: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    fut.poll(&mut Context::from_waker(Waker::noop()))
}

// ---- tests ----

#[tokio::test]
async fn call_and_response() {
    let (mut d, mut o) = pair(fast()).await;

    assert_eq!(
        d.links.call(o.id, Request::Ping).await.unwrap(),
        Response::Pong
    );
    let (peer, call) = o.next_call().await;
    assert_eq!(call, Incoming::Ping);
    assert_eq!(
        peer,
        Peer {
            device: d.id,
            person: "sam".into(),
            device_name: "sam-mbp".into(),
            own: false,
        },
        "person and device come from the roster"
    );

    // The other way over the same link.
    let env = envelope();
    let response = o
        .links
        .call(d.id, Request::Deliver(env.clone()))
        .await
        .unwrap();
    assert_eq!(
        response,
        Response::Receipt(DeliverReceipt {
            id: env.id.clone(),
            state: DeliverState::Accepted,
            code: None,
        })
    );
    let (peer, call) = d.next_call().await;
    assert_eq!(peer.person, "maya");
    assert_eq!(peer.device_name, "maya-mbp");
    match call {
        Incoming::Deliver(body) => assert_eq!(body["id"], env.id.as_str()),
        other => panic!("expected deliver, got {other:?}"),
    }

    assert!(matches!(
        d.links
            .call(o.id, Request::Hello(Hello::ours("sam-mbp")))
            .await,
        Err(CallError::Protocol(_))
    ));
    let stranger = key(9).public();
    assert!(!d.links.is_admitted(&stranger));
    assert!(matches!(
        d.links.call(stranger, Request::Ping).await,
        Err(CallError::NotAdmitted)
    ));

    let status = d.links.status();
    assert_eq!(status.network, "local");
    assert_eq!(status.endpoint_id, Some(d.id.to_string()));
    assert_eq!(status.relay, None);
    assert!(status.relays.is_empty());
    assert_eq!(status.error, None);
    assert!(!status.bound_sockets.is_empty());
    assert_eq!(status.links.len(), 1);
    let link = &status.links[0];
    assert_eq!(link.device, o.id.to_string());
    assert_eq!(link.path, "direct");
    assert!(link.rtt_ms.is_some());
    assert_eq!(link.name, "maya-mbp");
    assert_eq!(link.person, "maya");
    assert_eq!(link.version, 1);
    assert!(link.dialed, "the designated dialer dialed");
    assert!(!o.links.status().links[0].dialed);
    assert_eq!(
        *d.links.home_relay().borrow(),
        None,
        "no relays in local mode"
    );
    d.assert_no_event();
    o.assert_no_event();
}

#[tokio::test]
async fn own_devices_are_marked_own() {
    let (kd, ko) = ordered(34, 35);
    let mut o = node(
        ko.clone(),
        admission(ko.public(), &[other(kd.public(), "sam", "sam-mbp")]),
        fast(),
    )
    .await;
    let o_addrs = o.addrs();
    let mut d = node(
        kd.clone(),
        admission(
            kd.public(),
            &[other(ko.public(), "sam", "sam-linux").at(o_addrs)],
        ),
        fast(),
    )
    .await;
    d.expect_up(o.id).await;
    o.expect_up(d.id).await;
    assert_eq!(
        d.links.call(o.id, Request::Ping).await.unwrap(),
        Response::Pong
    );
    let (peer, _) = o.next_call().await;
    assert!(peer.own);
    assert_eq!(peer.person, "sam");
    assert_eq!(peer.device_name, "sam-mbp");
}

#[tokio::test]
async fn network_is_off_without_anyone_to_admit() {
    let k = key(3);
    let mut n = node(k.clone(), Admission::default(), fast()).await;
    let status = n.links.status();
    assert_eq!(status.network, "off");
    assert_eq!(status.endpoint_id, None);
    assert!(status.bound_sockets.is_empty());
    assert!(n.links.bound_sockets().is_empty());

    // The endpoint comes up when another device is admitted, and closes when none is.
    let peer = key(4).public();
    n.links
        .set_admission(admission(k.public(), &[other(peer, "maya", "maya-mbp")]));
    assert!(n.links.is_admitted(&peer));
    eventually("the endpoint to come up", || {
        n.links.status().network == "local"
    })
    .await;
    assert!(!n.links.bound_sockets().is_empty());
    n.links.set_admission(Admission::default());
    assert!(!n.links.is_admitted(&peer));
    eventually("the endpoint to close", || {
        n.links.status().network == "off"
    })
    .await;
    n.assert_no_event();
}

#[tokio::test]
async fn a_bind_failure_shows_in_status() {
    // Another socket holds the port, so the endpoint can't bind it.
    let taken = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut network = loopback();
    network.port = taken.local_addr().unwrap().port();
    let k = key(36);
    let n = node_with(
        k.clone(),
        admission(k.public(), &[other(key(37).public(), "maya", "maya-mbp")]),
        network,
        fast(),
    )
    .await;
    let status = n.links.status();
    assert_eq!(status.network, "off");
    assert!(status.error.is_some(), "{status:?}");
    match n.links.call(key(37).public(), Request::Ping).await {
        Err(CallError::Unreachable(_)) => {}
        other => panic!("expected unreachable, got {other:?}"),
    }
}

#[tokio::test]
async fn unadmitted_device_is_refused_at_the_handshake() {
    let kb = key(5);
    let mut b = node(
        kb.clone(),
        admission(kb.public(), &[other(key(6).public(), "maya", "maya-mbp")]),
        fast(),
    )
    .await;
    let mut refused = b.links.refused();

    // A stranger's connection is closed with code 1 right after the handshake.
    let stranger = raw_endpoint(key(7)).await;
    let conn = raw_connect(&stranger, &b).await;
    assert_eq!(closed_code(&conn).await, 1);
    let reported = tokio::time::timeout(WAIT, refused.recv())
        .await
        .expect("refusal reported in time")
        .unwrap();
    assert_eq!(reported, key(7).public());

    // A device whose roster lists b, but which b's roster doesn't list, gets no link either.
    let kc = key(8);
    let mut timing = fast();
    timing.lazy_dial_after = Duration::ZERO;
    let mut c = node(
        kc.clone(),
        admission(
            kc.public(),
            &[other(b.id, "sam", "sam-linux").at(b.addrs())],
        ),
        timing,
    )
    .await;
    let error = c.links.call(b.id, Request::Ping).await.unwrap_err();
    assert!(matches!(error, CallError::Unreachable(_)), "{error}");
    eventually("c to record the refusal", || c.has_problem(b.id, "refused")).await;
    b.assert_no_event();
    c.assert_no_event();
    assert!(b.drain_calls().is_empty());
}

#[tokio::test]
async fn removal_closes_the_link_with_code_1() {
    let (kr, kb) = ordered(10, 11);
    let x = other(key(12).public(), "lee", "lee-mac");
    let r_entry = other(kr.public(), "maya", "maya-mbp");
    let mut b = node(
        kb.clone(),
        admission(kb.public(), &[r_entry.clone(), x.clone()]),
        fast(),
    )
    .await;
    let raw = raw_endpoint(kr.clone()).await;
    let conn = raw_connect(&raw, &b).await;
    assert!(matches!(raw_hello(&conn).await, Response::Hello(_)));
    b.expect_up(kr.public()).await;
    assert_eq!(raw_request(&conn, &Request::Ping).await, Response::Pong);

    b.links
        .set_admission(admission(kb.public(), std::slice::from_ref(&x)));
    assert_eq!(closed_code(&conn).await, 1, "closed with not-admitted");
    b.expect_down(kr.public()).await;
    assert!(!b.links.is_admitted(&kr.public()));
    assert!(b.links.status().links.is_empty());
    assert_eq!(
        b.links.status().network,
        "local",
        "still up for the other device"
    );

    // A new connection from the removed device is refused at the handshake.
    let again = raw_connect(&raw, &b).await;
    assert_eq!(closed_code(&again).await, 1);
    b.assert_no_event();
}

#[tokio::test]
async fn readmitted_peer_reconnects() {
    let (kd, ko) = ordered(13, 14);
    let x = other(key(15).public(), "lee", "lee-mac");
    let with_d = admission_as(
        "maya",
        ko.public(),
        &[other(kd.public(), "sam", "sam-mbp"), x.clone()],
    );
    let mut o = node(ko.clone(), with_d.clone(), fast()).await;
    let o_addrs = o.addrs();
    let mut d = node(
        kd.clone(),
        admission(
            kd.public(),
            &[other(ko.public(), "maya", "maya-mbp").at(o_addrs)],
        ),
        fast(),
    )
    .await;
    d.expect_up(o.id).await;
    o.expect_up(d.id).await;

    // o drops d from its roster: the link closes with code 1 and d's redials are refused.
    o.links
        .set_admission(admission_as("maya", ko.public(), &[x]));
    o.expect_down(d.id).await;
    d.expect_down(o.id).await;
    eventually("d to see the refusal", || d.has_problem(o.id, "refused")).await;

    // Admitted again: the designated dialer's next attempt brings the link back.
    o.links.set_admission(with_d);
    o.expect_up(d.id).await;
    d.expect_up(o.id).await;
    assert!(d.links.status().problems.is_empty(), "cleared once linked");
    assert_eq!(
        o.links.call(d.id, Request::Ping).await.unwrap(),
        Response::Pong
    );
    d.assert_no_event();
    o.assert_no_event();
}

#[tokio::test]
async fn hello_with_no_common_version_closes_with_code_3() {
    // A dialer that speaks only versions 2 and 3 gets our hello back, then the link closes with code 3.
    let (kr, kb) = ordered(16, 17);
    // The dialer gets one first-frame deadline to close the link itself.
    let mut timing = fast();
    timing.first_frame = Duration::from_millis(300);
    let mut b = node(
        kb.clone(),
        admission(kb.public(), &[other(kr.public(), "maya", "maya-mbp")]),
        timing,
    )
    .await;
    let raw = raw_endpoint(kr.clone()).await;
    let conn = raw_connect(&raw, &b).await;
    match raw_request(&conn, &Request::Hello(hello_with(2, 3))).await {
        Response::Hello(theirs) => assert_eq!((theirs.proto_min, theirs.proto_max), (1, 1)),
        other => panic!("expected hello, got {other:?}"),
    }
    assert_eq!(closed_code(&conn).await, 3);
    let problems = b.links.status().problems;
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].problem, "version");
    assert!(
        problems[0].detail.contains("2 to 3"),
        "{}",
        problems[0].detail
    );
    b.assert_no_event();

    // As the dialer, we close with code 3 when the other side's hello shares no version.
    let (ka, kq) = ordered(18, 19);
    let quiet = raw_endpoint(kq.clone()).await;
    let quiet_addrs = quiet.bound_sockets();
    let accepted = tokio::spawn({
        let quiet = quiet.clone();
        async move {
            let incoming = quiet.accept().await.expect("an incoming connection");
            let conn = incoming.await.expect("accepted");
            raw_serve(conn.clone(), hello_with(2, 2));
            closed_code(&conn).await
        }
    });
    let mut a = node(
        ka.clone(),
        admission(
            ka.public(),
            &[other(kq.public(), "maya", "maya-mbp").at(quiet_addrs)],
        ),
        fast(),
    )
    .await;
    assert_eq!(
        tokio::time::timeout(WAIT, accepted).await.unwrap().unwrap(),
        3
    );
    eventually("a to record the version problem", || {
        a.has_problem(kq.public(), "version")
    })
    .await;
    a.assert_no_event();
}

#[tokio::test]
async fn frame_limits_and_errors() {
    let (kr, kb) = ordered(20, 21);
    let mut b = node(
        kb.clone(),
        admission(kb.public(), &[other(kr.public(), "maya", "maya-mbp")]),
        fast(),
    )
    .await;
    let raw = raw_endpoint(kr.clone()).await;
    let conn = raw_connect(&raw, &b).await;
    let error = |code| Response::Error { code };

    // Before hello, nothing else is taken.
    assert_eq!(
        raw_request(&conn, &Request::Ping).await,
        error(ErrorCode::HelloRequired)
    );
    assert!(matches!(raw_hello(&conn).await, Response::Hello(_)));
    b.expect_up(kr.public()).await;

    // Over 128 KiB: refused on the length prefix alone.
    let mut big = (u32::try_from(MAX_FRAME).unwrap() + 1)
        .to_be_bytes()
        .to_vec();
    big.extend_from_slice(b"{\"t\":\"ping\"}");
    assert_eq!(raw_call(&conn, &big).await, error(ErrorCode::TooLarge));
    assert_eq!(
        raw_call(&conn, &raw_frame(b"")).await,
        error(ErrorCode::Malformed)
    );
    assert_eq!(
        raw_call(&conn, &raw_frame(b"{")).await,
        error(ErrorCode::Malformed)
    );
    assert_eq!(
        raw_call(&conn, &raw_frame(b"[1]")).await,
        error(ErrorCode::Malformed)
    );
    assert_eq!(
        raw_call(&conn, &raw_frame(br#"{"t":"ping","extra":1}"#)).await,
        error(ErrorCode::Malformed)
    );
    assert_eq!(
        raw_call(&conn, &raw_frame(br#"{"t":"teleport"}"#)).await,
        error(ErrorCode::UnknownCall),
        "newer peers can probe"
    );
    // A frame cut short by the end of the stream.
    let mut short = 100u32.to_be_bytes().to_vec();
    short.extend_from_slice(b"{\"t\":");
    assert_eq!(raw_call(&conn, &short).await, error(ErrorCode::Malformed));
    assert!(
        b.drain_calls().is_empty(),
        "none of those reached the daemon"
    );

    // Exactly 128 KiB is allowed.
    let id = ulid();
    let mut payload = format!(r#"{{"t":"deliver","id":"{id}","pad":""#).into_bytes();
    payload.resize(MAX_FRAME - 2, b'x');
    payload.extend_from_slice(b"\"}");
    assert_eq!(payload.len(), MAX_FRAME);
    match raw_call(&conn, &raw_frame(&payload)).await {
        Response::Receipt(receipt) => assert_eq!(receipt.id, id),
        other => panic!("expected a receipt, got {other:?}"),
    }
    let (_, call) = b.next_call().await;
    assert_eq!(call.kind(), CallKind::Deliver);
    // The link survives all of it.
    assert_eq!(raw_request(&conn, &Request::Ping).await, Response::Pong);
    b.assert_no_event();
}

#[tokio::test]
async fn request_must_arrive_within_the_first_frame_deadline() {
    let (kr, kb) = ordered(22, 23);
    let mut timing = fast();
    timing.first_frame = Duration::from_millis(300);
    let mut b = node(
        kb.clone(),
        admission(kb.public(), &[other(kr.public(), "maya", "maya-mbp")]),
        timing,
    )
    .await;
    let raw = raw_endpoint(kr.clone()).await;
    let conn = raw_connect(&raw, &b).await;
    assert!(matches!(raw_hello(&conn).await, Response::Hello(_)));
    b.expect_up(kr.public()).await;

    let started = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    // Half a length prefix, then nothing.
    send.write_all(&[0, 0]).await.unwrap();
    let result = tokio::time::timeout(WAIT, read_frame(&mut recv))
        .await
        .expect("the stream is reset before the test deadline");
    assert!(
        result.is_err(),
        "no response, the stream was reset: {result:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(250));
    // Only that stream: the link still works.
    assert_eq!(raw_request(&conn, &Request::Ping).await, Response::Pong);
    b.assert_no_event();
}

#[tokio::test]
async fn at_most_16_inbound_streams_per_link() {
    let (kr, kb) = ordered(24, 25);
    let mut timing = fast();
    timing.first_frame = Duration::from_secs(30);
    timing.dial_timeout = Duration::from_secs(30);
    let b = node(
        kb.clone(),
        admission(kb.public(), &[other(kr.public(), "maya", "maya-mbp")]),
        timing,
    )
    .await;
    let raw = raw_endpoint(kr.clone()).await;
    // A fresh connection, so no stream has been used yet; hello doesn't matter for the stream count.
    let conn = raw_connect(&raw, &b).await;

    // Sixteen streams, each holding half a length prefix, open at once.
    let mut open = Vec::new();
    for i in 0..MAX_INBOUND_STREAMS {
        let mut opening = pin!(conn.open_bi());
        let Poll::Ready(stream) = poll_once(opening.as_mut()) else {
            panic!("stream {i} should open at once");
        };
        let (mut send, recv) = stream.unwrap();
        send.write_all(&[0, 0]).await.unwrap();
        open.push((send, recv));
    }
    // The 17th has to wait: the peer grants no more until some close.
    let mut seventeenth = pin!(conn.open_bi());
    assert!(poll_once(seventeenth.as_mut()).is_pending());

    // Finishing the requests frees their streams.
    let ping = br#"{"t":"ping"}"#;
    let len = u32::try_from(ping.len()).unwrap().to_be_bytes();
    for (mut send, mut recv) in open {
        send.write_all(&len[2..]).await.unwrap();
        send.write_all(ping).await.unwrap();
        send.finish().unwrap();
        let payload = tokio::time::timeout(WAIT, read_frame(&mut recv))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            Response::parse(&payload).unwrap(),
            Response::Error {
                code: ErrorCode::HelloRequired
            }
        );
    }
    let (mut send, mut recv) = tokio::time::timeout(WAIT, seventeenth)
        .await
        .expect("a stream once others closed")
        .unwrap();
    send.write_all(&raw_frame(ping)).await.unwrap();
    send.finish().unwrap();
    let payload = tokio::time::timeout(WAIT, read_frame(&mut recv))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        Response::parse(&payload).unwrap(),
        Response::Error { .. }
    ));
}

#[tokio::test]
async fn deliver_and_presence_rate_limits() {
    let (d, mut o) = pair(fast()).await;
    for i in 0..DELIVER_PER_MINUTE {
        match d
            .links
            .call(o.id, Request::Deliver(envelope()))
            .await
            .unwrap()
        {
            Response::Receipt(r) => assert_eq!(r.state, DeliverState::Accepted, "call {i}"),
            other => panic!("expected a receipt, got {other:?}"),
        }
    }
    let env = envelope();
    assert_eq!(
        d.links
            .call(o.id, Request::Deliver(env.clone()))
            .await
            .unwrap(),
        Response::Receipt(DeliverReceipt {
            id: env.id,
            state: DeliverState::Refused,
            code: Some(RefusalCode::RateLimited),
        }),
        "the 121st in a minute"
    );
    let delivered = o.drain_calls();
    assert_eq!(delivered.len(), DELIVER_PER_MINUTE);
    assert!(delivered.iter().all(|k| *k == CallKind::Deliver));

    // Presence: at most one a second reaches the daemon; the rest are answered and dropped.
    for seq in 0..3 {
        assert_eq!(
            d.links
                .call(o.id, Request::Presence(snapshot(seq)))
                .await
                .unwrap(),
            Response::Ok
        );
    }
    let (_, first) = o.next_call().await;
    assert_eq!(first.kind(), CallKind::Presence);
    // Other calls still pass.
    assert_eq!(
        d.links.call(o.id, Request::Ping).await.unwrap(),
        Response::Pong
    );
    assert_eq!(o.next_call().await.1, Incoming::Ping);
    assert!(o.drain_calls().is_empty());
}

#[tokio::test]
async fn link_events_come_in_order() {
    // o is the designated dialer here, so after a restart it dials d at d's unchanged address.
    let (ko, kd) = ordered(26, 27);
    let mut d = node(
        kd.clone(),
        admission(kd.public(), &[other(ko.public(), "maya", "maya-mbp")]),
        fast(),
    )
    .await;
    let d_addrs = d.addrs();
    let o_admission = admission_as(
        "maya",
        ko.public(),
        &[other(kd.public(), "sam", "sam-mbp").at(d_addrs)],
    );
    let mut o = node(ko.clone(), o_admission.clone(), fast()).await;
    d.expect_up(o.id).await;
    o.expect_up(d.id).await;

    let before = unix_ms();
    o.links.shutdown().await;
    let last_seen = d.expect_down(o.id).await;
    assert!(
        (before..=unix_ms()).contains(&last_seen),
        "a peer that closed was seen at the close"
    );
    o.expect_down(d.id).await;
    o.assert_no_event();
    assert_eq!(o.links.status().network, "off");
    assert!(d.links.status().links.is_empty());

    // d isn't the designated dialer and a link was up moments ago, so it waits for o rather than dialing.
    match d.links.call(o.id, Request::Ping).await {
        Err(CallError::Unreachable(why)) => assert!(why.contains("waiting"), "{why}"),
        other => panic!("expected to wait for the peer, got {other:?}"),
    }

    // o comes back with the same key: d sees it up again.
    drop(o);
    let mut o2 = node(ko, o_admission, fast()).await;
    d.expect_up(o2.id).await;
    o2.expect_up(d.id).await;
    assert_eq!(
        d.links.call(o2.id, Request::Ping).await.unwrap(),
        Response::Pong
    );
    d.assert_no_event();
    o2.assert_no_event();
}

#[tokio::test]
async fn both_sides_keep_the_designated_dialers_link() {
    // r (raw) is the designated dialer; a dials first because it has mail and no wait.
    let (kr, ka) = ordered(28, 29);
    let raw = raw_endpoint(kr.clone()).await;
    let raw_addrs = raw.bound_sockets();
    let hello = Hello::ours("raw-peer");
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    tokio::spawn({
        let server = raw.clone();
        let hello = hello.clone();
        async move {
            let incoming = server.accept().await.expect("a's dial");
            let conn = incoming.await.expect("accepted");
            raw_serve(conn.clone(), hello);
            let _ = first_tx.send(conn);
        }
    });
    let mut timing = fast();
    timing.lazy_dial_after = Duration::ZERO;
    let mut a = node(
        ka.clone(),
        admission(
            ka.public(),
            &[other(kr.public(), "maya", "maya-mbp").at(raw_addrs)],
        ),
        timing,
    )
    .await;
    assert_eq!(
        a.links.call(kr.public(), Request::Ping).await.unwrap(),
        Response::Pong
    );
    a.expect_up(kr.public()).await;
    assert!(a.links.status().links[0].dialed);
    let first = tokio::time::timeout(WAIT, first_rx).await.unwrap().unwrap();

    // r dials too: a keeps r's link and closes its own with code 2.
    let second = raw_connect(&raw, &a).await;
    raw_serve(second.clone(), hello.clone());
    assert!(matches!(raw_hello(&second).await, Response::Hello(_)));
    assert_eq!(closed_code(&first).await, 2);
    a.expect_down(kr.public()).await;
    a.expect_up(kr.public()).await;
    assert_eq!(
        a.links.call(kr.public(), Request::Ping).await.unwrap(),
        Response::Pong
    );
    assert!(!a.links.status().links[0].dialed);

    // Between two links from the same dialer the newer wins: the older is likely left from a restart.
    let third = raw_connect(&raw, &a).await;
    raw_serve(third.clone(), hello);
    assert!(matches!(raw_hello(&third).await, Response::Hello(_)));
    assert_eq!(closed_code(&second).await, 2);
    a.expect_down(kr.public()).await;
    a.expect_up(kr.public()).await;
    assert_eq!(raw_request(&third, &Request::Ping).await, Response::Pong);
    a.assert_no_event();
}

#[tokio::test]
async fn a_second_link_from_the_other_device_is_closed() {
    // a is the designated dialer; r (raw) dials anyway and loses.
    let (ka, kr) = ordered(30, 31);
    let raw = raw_endpoint(kr.clone()).await;
    let raw_addrs = raw.bound_sockets();
    tokio::spawn({
        let server = raw.clone();
        async move {
            while let Some(incoming) = server.accept().await {
                if let Ok(conn) = incoming.await {
                    raw_serve(conn, Hello::ours("raw-peer"));
                }
            }
        }
    });
    let mut a = node(
        ka.clone(),
        admission(
            ka.public(),
            &[other(kr.public(), "maya", "maya-mbp").at(raw_addrs)],
        ),
        fast(),
    )
    .await;
    a.expect_up(kr.public()).await;

    // r's own dial: its hello is answered by closing the link with code 2.
    let extra = raw_connect(&raw, &a).await;
    let (mut send, _recv) = extra.open_bi().await.unwrap();
    send.write_all(&encode_frame(&Request::Hello(Hello::ours("raw-peer"))).unwrap())
        .await
        .unwrap();
    send.finish().unwrap();
    assert_eq!(closed_code(&extra).await, 2);
    assert_eq!(
        a.links.call(kr.public(), Request::Ping).await.unwrap(),
        Response::Pong
    );
    assert!(a.links.status().links[0].dialed);
    a.assert_no_event();
}

#[tokio::test]
async fn a_dial_that_loses_to_the_peers_link_is_no_failure() {
    // r (raw) is the designated dialer. a dials first; r holds that dial without answering hello, links the other
    // way, and then closes a's dial with code 2.
    let (kr, ka) = ordered(40, 41);
    let raw = raw_endpoint(kr.clone()).await;
    let raw_addrs = raw.bound_sockets();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    tokio::spawn({
        let server = raw.clone();
        async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let _ = held_tx.send(conn);
        }
    });
    let mut timing = fast();
    timing.lazy_dial_after = Duration::ZERO;
    let mut a = node(
        ka.clone(),
        admission(
            ka.public(),
            &[other(kr.public(), "maya", "maya-mbp").at(raw_addrs)],
        ),
        timing,
    )
    .await;
    let a_addr = addr_of(&a);
    let peer = async {
        let held = tokio::time::timeout(WAIT, held_rx).await.unwrap().unwrap();
        let link = tokio::time::timeout(WAIT, raw.connect(a_addr, ALPN))
            .await
            .unwrap()
            .unwrap();
        raw_serve(link.clone(), Hello::ours("raw-peer"));
        // a registers r's link before it answers this hello.
        assert!(matches!(raw_hello(&link).await, Response::Hello(_)));
        held.close(VarInt::from_u32(2), b"duplicate");
        link
    };
    let (answer, _link) = tokio::join!(a.links.call(kr.public(), Request::Ping), peer);
    assert_eq!(answer.unwrap(), Response::Pong, "the call took r's link");
    a.expect_up(kr.public()).await;
    let status = a.links.status();
    assert!(status.problems.is_empty(), "{:?}", status.problems);
    assert_eq!(status.links.len(), 1);
    assert!(!status.links[0].dialed);
    a.assert_no_event();
}

#[tokio::test]
async fn a_call_without_an_answer_times_out() {
    // r (raw) answers hello and then never answers anything else.
    let (ka, kr) = ordered(38, 39);
    let raw = raw_endpoint(kr.clone()).await;
    let raw_addrs = raw.bound_sockets();
    tokio::spawn({
        let server = raw.clone();
        async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let payload = read_frame(&mut recv).await.unwrap();
                if let Ok(Incoming::Hello(_)) = crate::wire::parse_incoming(&payload) {
                    let hello = Response::Hello(Hello::ours("raw-peer"));
                    crate::wire::write_frame(&mut send, &hello).await.unwrap();
                    send.finish().unwrap();
                } else {
                    // Keep the stream open, unanswered.
                    tokio::spawn(async move {
                        let _held = (send, recv);
                        std::future::pending::<()>().await;
                    });
                }
            }
        }
    });
    let mut timing = fast();
    timing.call_timeout = Duration::from_millis(300);
    let mut a = node(
        ka.clone(),
        admission(
            ka.public(),
            &[other(kr.public(), "maya", "maya-mbp").at(raw_addrs)],
        ),
        timing,
    )
    .await;
    a.expect_up(kr.public()).await;
    assert!(matches!(
        a.links.call(kr.public(), Request::Ping).await,
        Err(CallError::Timeout)
    ));
    // The link stays up.
    assert_eq!(a.links.status().links.len(), 1);
    a.assert_no_event();
}

#[tokio::test]
#[ignore = "uses LAN multicast; run with --ignored where mDNS is allowed"]
async fn mdns_finds_peers_without_hints() {
    let (kd, ko) = ordered(32, 33);
    let mut network = loopback();
    network.bind = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    network.mdns = true;
    let mut o = node_with(
        ko.clone(),
        admission_as("maya", ko.public(), &[other(kd.public(), "sam", "sam-mbp")]),
        network.clone(),
        fast(),
    )
    .await;
    let mut d = node_with(
        kd.clone(),
        admission(kd.public(), &[other(ko.public(), "maya", "maya-mbp")]),
        network,
        fast(),
    )
    .await;
    d.expect_up(o.id).await;
    o.expect_up(d.id).await;
}
