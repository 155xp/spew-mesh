//! The daemon's background work against a small hand-written spew server: device registration retried after
//! a failure, an early roster refresh when an unknown device knocks (at most once every 10 s), home relay
//! reports, and `run/stopped` cleared once a daemon holds the lock.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use iroh::RelayUrl;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::roster::{Admission, file as roster_file};
use crate::settings::RosterKind;
use crate::spew_api::BaseUrl;
use crate::store::Store;
use crate::wire::Request;
use testnet::{MemLinks, MemNet};

const WAIT: Duration = Duration::from_secs(10);
const T0: u64 = 1_800_000_000_000;

struct TestClock(AtomicU64);

impl TestClock {
    fn advance_secs(&self, secs: u64) {
        self.0.fetch_add(secs * 1000, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn key(seed: u8) -> iroh::SecretKey {
    iroh::SecretKey::from_bytes(&[seed; 32])
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// What the spew server knows and was asked.
struct SpewState {
    peers: Value,
    etag: u32,
    log: Vec<String>,
    register_failures: u32,
    fail_patch: bool,
    patches: Vec<Value>,
}

impl SpewState {
    fn count(&self, line: &str) -> usize {
        self.log.iter().filter(|l| l.as_str() == line).count()
    }
}

/// A spew server on loopback that takes any bearer token.
struct Spew {
    base: String,
    state: Arc<Mutex<SpewState>>,
    task: JoinHandle<()>,
}

impl Drop for Spew {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn peers(me_devices: Vec<Value>) -> Value {
    json!({
        "version": "v1",
        "ttl_secs": 60,
        "me": { "username": "sam", "display": "Sam", "devices": me_devices },
        "rooms": []
    })
}

fn device_json(id: &EndpointId, name: &str) -> Value {
    json!({ "endpoint_id": id.to_string(), "name": name, "relay": null })
}

impl Spew {
    async fn start(peers: Value) -> Spew {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new(Mutex::new(SpewState {
            peers,
            etag: 1,
            log: Vec::new(),
            register_failures: 0,
            fail_patch: false,
            patches: Vec::new(),
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(answer(stream, shared.clone()));
            }
        });
        Spew { base, state, task }
    }

    fn with<R>(&self, f: impl FnOnce(&mut SpewState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    fn count(&self, line: &str) -> usize {
        self.with(|s| s.count(line))
    }
}

async fn answer(mut stream: TcpStream, state: Arc<Mutex<SpewState>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (status, extra, reply) = {
        let mut s = state.lock().unwrap();
        s.log.push(format!("{method} {path}"));
        route(&mut s, &method, &path, &headers, &body)
    };
    let text = if reply.is_null() {
        String::new()
    } else {
        reply.to_string()
    };
    let mut out = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n");
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    ));
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn api_error(status: u16, code: &str) -> (u16, Vec<(String, String)>, Value) {
    (
        status,
        Vec::new(),
        json!({ "error": { "code": code, "message": code } }),
    )
}

fn route(
    s: &mut SpewState,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &Value,
) -> (u16, Vec<(String, String)>, Value) {
    match (method, path) {
        ("GET", "/api/mesh/v1/peers") => {
            let etag = format!("\"{}\"", s.etag);
            if headers.get("if-none-match") == Some(&etag) {
                return (304, vec![("ETag".into(), etag)], Value::Null);
            }
            (200, vec![("ETag".into(), etag)], s.peers.clone())
        }
        ("POST", "/api/mesh/v1/devices/challenge") => (
            200,
            Vec::new(),
            json!({ "nonce": URL_SAFE_NO_PAD.encode([7u8; 32]), "expires_at": T0 + 300_000 }),
        ),
        ("POST", "/api/mesh/v1/devices") => {
            if s.register_failures > 0 {
                s.register_failures -= 1;
                return api_error(503, "unavailable");
            }
            let key = body["public_key"].as_str().unwrap_or("").to_string();
            let name = body["name"].as_str().unwrap_or("").to_string();
            if let Some(list) = s.peers["me"]["devices"].as_array_mut() {
                list.push(json!({ "endpoint_id": key, "name": name, "relay": null }));
            }
            s.etag += 1;
            (
                201,
                Vec::new(),
                json!({ "name": name, "public_key": key, "created_at": T0 }),
            )
        }
        ("PATCH", "/api/mesh/v1/devices/self") => {
            if s.fail_patch {
                return api_error(503, "unavailable");
            }
            s.patches.push(body["relay"].clone());
            (200, Vec::new(), json!({}))
        }
        _ => api_error(404, "not-found"),
    }
}

// Signs `home` in to `spew` as sam, the way a finished login leaves auth/spew.json.
fn sign_in(home: &Home, spew: &Spew) {
    let base = BaseUrl::parse(&spew.base).unwrap();
    let auth = json!({
        "url": base.as_str(),
        "client_id": "client-1",
        "access_token": "at-1",
        "refresh_token": "rt-1",
        "scope": "mesh",
        "username": "sam",
        "display": "Sam",
    });
    crate::paths::write_atomic(&home.spew_auth_file(), auth.to_string().as_bytes()).unwrap();
}

fn api_for(home: &Home, spew: &Spew, clock: Arc<TestClock>) -> SpewApi {
    crate::install_crypto_provider();
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    SpewApi::with_http(
        BaseUrl::parse(&spew.base).unwrap(),
        home.spew_auth_file(),
        clock,
        http,
    )
}

/// sam-mac signed in to a spew server, with its courier on an in-memory network and the roster task running.
struct Harness {
    _tmp: tempfile::TempDir,
    home: Home,
    spew: Spew,
    clock: Arc<TestClock>,
    net: MemNet,
    device: EndpointId,
    courier: Arc<Courier<MemLinks>>,
    api: SpewApi,
    pending: Arc<AtomicBool>,
    roster: mpsc::Sender<RosterCmd>,
    tasks: Vec<JoinHandle<()>>,
}

impl Harness {
    async fn start(spew: Spew) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        home.ensure().unwrap();
        sign_in(&home, &spew);
        let clock = Arc::new(TestClock(AtomicU64::new(T0)));
        let api = api_for(&home, &spew, clock.clone());
        let mut settings = Settings::defaults(&home);
        settings.roster.kind = RosterKind::Spew;
        let secret = key(1);
        let device = secret.public();
        let mut mgr = RosterManager::new(&settings, device, "sam-mac", Ok(api.clone()), None);
        let chosen = mgr.current(T0);
        let net = MemNet::new();
        let (links, inbound) = net.join(device);
        let cfg = CourierConfig {
            device: LocalDevice::new(device, "sam-mac", "linux"),
            default_room: None,
            send_branch: true,
            send_place: true,
            serve: serve_settings(&settings.serve),
            clock: clock.clone(),
            start_time: |_| None,
        };
        let courier = Arc::new(
            Courier::new(
                links,
                Store::open_in_memory().unwrap(),
                ActiveRoster::new(chosen.me, device, chosen.roster),
                chosen.info,
                cfg,
            )
            .unwrap(),
        );
        let weak: Weak<dyn InboundDyn> = Arc::downgrade(&courier) as Weak<dyn InboundDyn>;
        inbound.bind(weak);
        courier.start_links();
        let pending = Arc::new(AtomicBool::new(true));
        let registration = Registration {
            key: secret,
            name: "sam-mac".into(),
            platform: "linux".into(),
            pending: pending.clone(),
        };
        let (roster, rx) = mpsc::channel(8);
        let tasks = vec![tokio::spawn(roster_task(
            courier.clone(),
            mgr,
            registration,
            rx,
        ))];
        Harness {
            _tmp: tmp,
            home,
            spew,
            clock,
            net,
            device,
            courier,
            api,
            pending,
            roster,
            tasks,
        }
    }

    async fn refresh(&self) {
        let (tx, rx) = oneshot::channel();
        self.roster.send(RosterCmd::Refresh(tx)).await.unwrap();
        tokio::time::timeout(WAIT, rx).await.unwrap().unwrap();
    }

    fn peers_fetches(&self) -> usize {
        self.spew.count("GET /api/mesh/v1/peers")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_registration_is_tried_again_at_the_next_refresh() {
    let spew = Spew::start(peers(Vec::new())).await;
    spew.with(|s| s.register_failures = 1);
    let h = Harness::start(spew).await;
    // The first round fetches the roster, which doesn't list this device, and the registration fails.
    eventually("the first registration is tried", || {
        h.spew.count("POST /api/mesh/v1/devices") == 1
    })
    .await;
    h.refresh().await;
    assert_eq!(h.spew.count("POST /api/mesh/v1/devices"), 2);
    assert!(!h.pending.load(Ordering::Relaxed));
    let me = h.spew.with(|s| s.peers["me"]["devices"].clone());
    assert_eq!(me[0]["endpoint_id"], h.device.to_string());
    // The roster is read again right after, so this device shows as registered and nothing more is sent.
    let fetched = h.peers_fetches();
    assert!(fetched >= 3, "{fetched}");
    h.refresh().await;
    assert_eq!(h.spew.count("POST /api/mesh/v1/devices"), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_device_refreshes_the_roster_at_most_every_ten_seconds() {
    let sam = key(1).public();
    let spew = Spew::start(peers(vec![device_json(&sam, "sam-mac")])).await;
    let h = Harness::start(spew).await;
    eventually("the first roster fetch", || h.peers_fetches() == 1).await;
    assert!(!h.courier.links().is_admitted(&key(9).public()));

    // lee-linux's own roster says it shares a room with sam-mac; sam-mac's roster doesn't list it.
    let lee = key(9).public();
    let text = format!(
        "[[person]]\nname = \"lee\"\ndevices = [ {{ id = \"{lee}\", name = \"lee-linux\" }} ]\n\n\
         [[person]]\nname = \"sam\"\ndevices = [ {{ id = \"{sam}\", name = \"sam-mac\" }} ]\n\n\
         [[room]]\nname = \"ops\"\npeople = [\"lee\", \"sam\"]\n"
    );
    let (me, roster) = roster_file::parse(&text)
        .unwrap()
        .roster_for(Some("lee"), &lee, T0)
        .unwrap();
    let (lee_links, _inbound) = h.net.join(lee);
    lee_links.set_admission(Admission::compute(&me, &lee, &roster));

    assert!(lee_links.call(h.device, Request::Ping).await.is_err());
    eventually("an early roster fetch", || h.peers_fetches() == 2).await;
    for _ in 0..5 {
        assert!(lee_links.call(h.device, Request::Ping).await.is_err());
    }
    // The clock doesn't move, so none of these may fetch again; give the roster task time to see them.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.peers_fetches(), 2);

    h.clock.advance_secs(10);
    assert!(lee_links.call(h.device, Request::Ping).await.is_err());
    eventually("a second early fetch, ten seconds on", || {
        h.peers_fetches() == 3
    })
    .await;
}

#[tokio::test]
async fn the_home_relay_is_reported_when_it_changes_at_most_once_a_minute() {
    let spew = Spew::start(peers(Vec::new())).await;
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home")).unwrap();
    home.ensure().unwrap();
    let clock = Arc::new(TestClock(AtomicU64::new(T0)));
    let api = api_for(&home, &spew, clock);
    let a: RelayUrl = "https://relay-a.example.com".parse().unwrap();
    let b: RelayUrl = "https://relay-b.example.com".parse().unwrap();
    let mut report = RelayReport::default();

    assert_eq!(
        report.step(&api, Some(&a), T0).await,
        ReportOutcome::NotSignedIn
    );
    sign_in(&home, &spew);
    assert_eq!(report.step(&api, None, T0).await, ReportOutcome::NotDue);
    assert_eq!(
        report.step(&api, Some(&a), T0 + 1_000).await,
        ReportOutcome::Sent
    );
    assert_eq!(
        report.step(&api, Some(&a), T0 + 90_000).await,
        ReportOutcome::NotDue
    );
    assert_eq!(
        report.step(&api, Some(&b), T0 + 30_000).await,
        ReportOutcome::NotDue,
        "too soon"
    );
    assert_eq!(
        report.step(&api, Some(&b), T0 + 61_000).await,
        ReportOutcome::Sent
    );
    // A failure waits out the minute too.
    spew.with(|s| s.fail_patch = true);
    assert!(matches!(
        report.step(&api, None, T0 + 122_000).await,
        ReportOutcome::Failed(_)
    ));
    spew.with(|s| s.fail_patch = false);
    assert_eq!(
        report.step(&api, None, T0 + 150_000).await,
        ReportOutcome::NotDue
    );
    assert_eq!(
        report.step(&api, None, T0 + 182_000).await,
        ReportOutcome::Sent
    );
    let sent = spew.with(|s| s.patches.clone());
    assert_eq!(
        sent,
        [
            json!("https://relay-a.example.com/"),
            json!("https://relay-b.example.com/"),
            Value::Null
        ]
    );
    assert_eq!(spew.count("PATCH /api/mesh/v1/devices/self"), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_task_reports_what_the_links_say() {
    let sam = key(1).public();
    let spew = Spew::start(peers(vec![device_json(&sam, "sam-mac")])).await;
    let mut h = Harness::start(spew).await;
    h.tasks
        .push(tokio::spawn(relay_task(h.courier.clone(), h.api.clone())));
    let a: RelayUrl = "https://relay-a.example.com".parse().unwrap();
    h.net.set_home_relay(&h.device, Some(a));
    eventually("spew hears of the relay", || {
        h.spew.with(|s| s.patches.len()) == 1
    })
    .await;
    assert_eq!(
        h.spew.with(|s| s.patches[0].clone()),
        json!("https://relay-a.example.com/")
    );
    // The status shows it as the home relay of this device's network.
    assert_eq!(
        h.courier.links().status().relay.as_deref(),
        Some("https://relay-a.example.com/")
    );
    assert!(h.home.spew_auth_file().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_holds_the_lock_clears_run_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let home = Home::at(tmp.path().join("home")).unwrap();
    home.ensure().unwrap();
    home.mark_stopped().unwrap();
    let opts = || DaemonOptions {
        home: home.clone(),
        user_home: Some(tmp.path().to_path_buf()),
        env: Env {
            lan_off: true,
            ..Env::default()
        },
        runtime_dirs: Vec::new(),
        clock: Arc::new(crate::spew_api::SystemClock),
    };
    let net = MemNet::new();
    let n = net.clone();
    let daemon = start(opts(), move |id| n.join(id)).await.unwrap().unwrap();
    assert!(!home.is_stopped());
    // A second daemon that can't take the lock touches nothing, the stopped mark included.
    home.mark_stopped().unwrap();
    let n = net.clone();
    assert!(start(opts(), move |id| n.join(id)).await.unwrap().is_none());
    assert!(home.is_stopped());
    daemon.finish().await;
}
