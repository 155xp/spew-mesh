//! [`IrohLinks`]: the iroh-backed [`Links`] (5.1 to 5.4).
//!
//! One actor task owns the endpoint's lifetime: it binds the endpoint while the admission admits another device
//! and closes it when it admits none. Each link is one QUIC connection that counts as up once `hello` has been
//! exchanged; a close watcher and a stream server run per connection, and the designated dialer of each pair runs
//! a dialer task per peer. All shared state sits behind one mutex that is never held across an await, and every
//! [`LinkEvent`] is sent while holding it, so events come out in the order the state changed.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream, VarInt};
use iroh::{Endpoint, EndpointId, RelayUrl, Watcher};
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::config::{LinkConfig, Timing};
use super::endpoint::{self, Gate, close, hint_addr};
use super::limits::{Backoff, Gap, RateWindow};
use super::stream::CallHead;
use super::{
    CallError, Inbound, LinkEvent, LinkInfo, LinkProblem, LinkStatus, Links, Peer, RelayInfo,
};
use crate::address::{is_device_name, is_ulid};
use crate::roster::Admission;
use crate::wire::{
    ALPN, CallKind, CloseCode, DeliverReceipt, DeliverState, ErrorCode, FrameError, Hello,
    Incoming, RefusalCode, Request, Response, encode_frame, parse_incoming, read_frame,
    write_frame,
};

/// Capacity of the link event channel; a receiver that falls this far behind gets `Lagged`.
const EVENT_CAPACITY: usize = 1024;
/// How long [`IrohLinks::shutdown`] waits for the endpoint to close.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// QUIC close code for a link whose `hello` failed for a reason other than versions. The spec lists only codes 1
/// to 4; 0 is QUIC's "no error".
const HELLO_FAILED: (u32, &[u8]) = (0, b"hello-failed");

/// Why [`IrohLinks::start`] refused the configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LinkError(pub String);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`Inbound`] without its generic future, so [`IrohLinks`] needn't be generic.
trait DynInbound: Send + Sync {
    fn handle(&self, peer: Peer, call: Incoming) -> BoxFuture<'_, Response>;
}

impl<T: Inbound> DynInbound for T {
    fn handle(&self, peer: Peer, call: Incoming) -> BoxFuture<'_, Response> {
        Box::pin(Inbound::handle(self, peer, call))
    }
}

/// The link layer over an iroh endpoint. Build it with [`IrohLinks::start`]; dropping it shuts the network down
/// in the background, [`IrohLinks::shutdown`] does so and waits.
pub struct IrohLinks {
    shared: Arc<Shared>,
    actor: Mutex<Option<JoinHandle<()>>>,
    /// Subscribed before anything could happen; the first [`Links::events`] call gets it.
    first_events: Mutex<Option<broadcast::Receiver<LinkEvent>>>,
}

impl std::fmt::Debug for IrohLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohLinks")
            .field("endpoint_id", &self.shared.me.fmt_short().to_string())
            .finish_non_exhaustive()
    }
}

impl IrohLinks {
    /// Starts the link layer with the initial admission. The endpoint is bound before this returns when the
    /// admission admits another device; a bind failure doesn't fail the start but shows in
    /// [`LinkStatus::error`] and is retried with backoff. Must run inside a tokio runtime.
    pub async fn start<I: Inbound>(
        config: LinkConfig,
        admission: Admission,
        inbound: Arc<I>,
    ) -> Result<IrohLinks, LinkError> {
        if !is_device_name(&config.device_name) {
            return Err(LinkError(format!(
                "\"{}\" isn't a valid device name",
                config.device_name
            )));
        }
        let hello = Hello::ours(&config.device_name);
        hello
            .validate()
            .map_err(|field| LinkError(format!("this build's hello has an invalid {field}")))?;
        if config.timing.keep_alive >= config.timing.idle_timeout {
            return Err(LinkError(
                "the keep-alive must be shorter than the idle timeout".into(),
            ));
        }
        let (events, first_events) = broadcast::channel(EVENT_CAPACITY);
        let (refused, _) = broadcast::channel(64);
        let (home_relay, _) = watch::channel(None);
        let shared = Arc::new(Shared {
            me: config.secret_key.public(),
            hello,
            gate: Arc::new(Gate::new(Admission::default(), refused.clone())),
            hints: MemoryLookup::new(),
            inbound,
            state: Mutex::new(State::default()),
            events,
            refused,
            home_relay,
            wake: Notify::new(),
            shutdown: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            config,
        });
        shared.apply_admission(admission);
        let (ready_tx, mut ready_rx) = watch::channel(false);
        let actor = tokio::spawn(shared.clone().run(ready_tx));
        // The actor reports after its first pass, which binds the endpoint when anyone else is admitted.
        let _ = ready_rx.wait_for(|ready| *ready).await;
        Ok(IrohLinks {
            shared,
            actor: Mutex::new(Some(actor)),
            first_events: Mutex::new(Some(first_events)),
        })
    }

    /// This device's endpoint id.
    pub fn endpoint_id(&self) -> EndpointId {
        self.shared.me
    }

    /// The sockets the endpoint is bound to; empty while the network is off (5.1).
    pub fn bound_sockets(&self) -> Vec<SocketAddr> {
        self.shared
            .lock()
            .endpoint
            .as_ref()
            .map(Endpoint::bound_sockets)
            .unwrap_or_default()
    }

    /// The home relay URL, updated when it changes and `None` while there is none, for the daemon's
    /// `PATCH /api/mesh/v1/devices/self` (14.3). It stays `None` in `local` mode.
    pub fn home_relay(&self) -> watch::Receiver<Option<RelayUrl>> {
        self.shared.home_relay.subscribe()
    }

    /// Endpoint ids that tried to connect without being admitted, at most one a second, so a spew roster can
    /// refresh early (4.2).
    pub fn refused(&self) -> broadcast::Receiver<EndpointId> {
        self.shared.refused.subscribe()
    }

    /// Closes every link with code 4 (`shutdown`), closes the endpoint and waits for it, up to a few seconds.
    pub async fn shutdown(&self) {
        self.shared.begin_shutdown();
        let actor = self
            .actor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(actor) = actor {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, actor).await;
        }
    }
}

impl Drop for IrohLinks {
    fn drop(&mut self) {
        self.shared.begin_shutdown();
    }
}

impl Links for IrohLinks {
    fn call(
        &self,
        device: EndpointId,
        request: Request,
    ) -> impl Future<Output = Result<Response, CallError>> + Send {
        let shared = self.shared.clone();
        async move { shared.call(device, request).await }
    }

    fn is_admitted(&self, device: &EndpointId) -> bool {
        self.shared.gate.is_admitted(device)
    }

    /// The first call returns a receiver holding every event since [`IrohLinks::start`] began, so links that
    /// came up during the start aren't missed; later calls see events from then on.
    fn events(&self) -> broadcast::Receiver<LinkEvent> {
        self.first_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| self.shared.events.subscribe())
    }

    fn set_admission(&self, admission: Admission) {
        self.shared.apply_admission(admission);
    }

    fn status(&self) -> LinkStatus {
        self.shared.status()
    }
}

struct Shared {
    me: EndpointId,
    config: LinkConfig,
    hello: Hello,
    gate: Arc<Gate>,
    hints: MemoryLookup,
    inbound: Arc<dyn DynInbound>,
    state: Mutex<State>,
    events: broadcast::Sender<LinkEvent>,
    refused: broadcast::Sender<EndpointId>,
    home_relay: watch::Sender<Option<RelayUrl>>,
    /// Wakes the actor: the admission changed or shutdown began.
    wake: Notify,
    shutdown: AtomicBool,
    /// Ids for connections and peer states.
    next_id: AtomicU64,
}

#[derive(Default)]
struct State {
    endpoint: Option<Endpoint>,
    /// Goes up each time the endpoint is replaced, so a stale relay watcher can tell.
    endpoint_gen: u64,
    endpoint_error: Option<String>,
    /// One entry per admitted device.
    peers: HashMap<EndpointId, PeerState>,
}

struct PeerState {
    /// Tells this state apart from an earlier one for the same device (removed, then admitted again).
    serial: u64,
    /// The link that carries calls, once `hello` is done.
    link: Option<Link>,
    /// Since when no link has been up; the non-designated side dials only after `lazy_dial_after` of it.
    quiet_since: Instant,
    /// One dial at a time per device.
    dial_lock: Arc<tokio::sync::Mutex<()>>,
    last_dial_failure: Option<(Instant, String)>,
    dialer_running: bool,
    /// Wakes this device's dialer: link down, new hints, removal, shutdown.
    notify: Arc<Notify>,
    deliver: RateWindow,
    presence: Gap,
    problem: Option<LinkProblem>,
}

impl PeerState {
    fn new(serial: u64) -> Self {
        Self {
            serial,
            link: None,
            quiet_since: Instant::now(),
            dial_lock: Arc::new(tokio::sync::Mutex::new(())),
            last_dial_failure: None,
            dialer_running: false,
            notify: Arc::new(Notify::new()),
            deliver: RateWindow::deliver(),
            presence: Gap::presence(),
            problem: None,
        }
    }
}

struct Link {
    id: u64,
    conn: Connection,
    dialed_by_me: bool,
    version: u32,
    since_ms: u64,
}

/// The device isn't admitted (any more).
struct NotAdmitted;

enum Registered {
    /// The connection is now the device's link.
    New,
    /// An existing link won (5.3); the new connection was closed with code 2.
    Kept(Connection),
}

/// What went wrong on the way to a link, for `doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Problem {
    Unreachable,
    Refused,
    Version,
    Protocol,
}

impl Problem {
    fn as_str(self) -> &'static str {
        match self {
            Problem::Unreachable => "unreachable",
            Problem::Refused => "refused",
            Problem::Version => "version",
            Problem::Protocol => "protocol",
        }
    }
}

struct DialError {
    problem: Problem,
    detail: String,
}

impl DialError {
    fn new(problem: Problem, detail: impl Into<String>) -> Self {
        Self {
            problem,
            detail: detail.into(),
        }
    }

    fn from_close(reason: &ConnectionError) -> Self {
        match close_code_of(reason) {
            Some(CloseCode::NotAdmitted) => Self::new(
                Problem::Refused,
                "it refused the link: its roster doesn't admit this device",
            ),
            Some(CloseCode::Version) => Self::new(
                Problem::Version,
                "it shares no protocol version with this build",
            ),
            Some(CloseCode::Shutdown) => Self::new(Problem::Unreachable, "it is shutting down"),
            _ => Self::new(Problem::Unreachable, describe_close(reason)),
        }
    }
}

/// Our close code in a close the peer sent, if any.
fn close_code_of(reason: &ConnectionError) -> Option<CloseCode> {
    match reason {
        ConnectionError::ApplicationClosed(close) => {
            CloseCode::from_code(close.error_code.into_inner())
        }
        _ => None,
    }
}

fn describe_close(reason: &ConnectionError) -> String {
    match close_code_of(reason) {
        Some(code) => format!("closed by the peer: {}", code.reason()),
        None => reason.to_string(),
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn short(id: &EndpointId) -> String {
    crate::identity::short_id(id)
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn timing(&self) -> &Timing {
        &self.config.timing
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// The designated dialer of a pair is the device with the greater endpoint id, in byte order (5.3).
    fn is_designated(&self, peer: &EndpointId) -> bool {
        self.me.as_bytes() > peer.as_bytes()
    }

    fn emit(&self, event: LinkEvent) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }

    /// The admitted peer behind `id`, from the roster (5.5 step 1).
    fn peer(&self, id: &EndpointId) -> Option<Peer> {
        let admission = self.gate.admission();
        admission.get(id).map(|d| Peer {
            device: d.endpoint_id,
            person: d.person.clone(),
            device_name: d.name.clone(),
            own: d.own,
        })
    }

    // ---- admission (5.2, 4.3) ----

    fn apply_admission(&self, admission: Admission) {
        let mut st = self.lock();
        let old = self.gate.replace(admission.clone());
        let now_ms = unix_ms();
        let removed: Vec<EndpointId> = old
            .devices()
            .map(|d| d.endpoint_id)
            .filter(|id| !admission.is_admitted(id))
            .collect();
        for id in removed {
            self.hints.remove_endpoint_info(id);
            if let Some(mut ps) = st.peers.remove(&id) {
                if let Some(link) = ps.link.take() {
                    info!(device = %id.fmt_short(), "closing the link to a device no longer admitted");
                    close(&link.conn, CloseCode::NotAdmitted);
                    self.emit(LinkEvent::Down {
                        device: id,
                        last_seen_ms: now_ms,
                    });
                }
                ps.notify.notify_one();
            }
        }
        let network = &self.config.network;
        for device in admission.devices() {
            let id = device.endpoint_id;
            let hints_changed = old
                .get(&id)
                .is_none_or(|before| before.relay != device.relay || before.addrs != device.addrs);
            if hints_changed {
                match hint_addr(device, network.mode, &network.relays) {
                    Some(addr) => {
                        self.hints.set_endpoint_info(addr);
                    }
                    None => {
                        self.hints.remove_endpoint_info(id);
                    }
                }
            }
            let serial = self.next_id();
            let ps = st.peers.entry(id).or_insert_with(|| PeerState::new(serial));
            if hints_changed {
                ps.notify.notify_one();
            }
        }
        drop(st);
        self.wake.notify_one();
    }

    // ---- endpoint lifetime (5.1) ----

    async fn run(self: Arc<Self>, ready: watch::Sender<bool>) {
        let timing = *self.timing();
        let mut bind_backoff = Backoff::new(timing.backoff_base, timing.backoff_max);
        let mut retry_at: Option<tokio::time::Instant> = None;
        loop {
            let shutting = self.is_shutting_down();
            let want = !shutting && self.gate.admission().admits_others();
            let have = self.lock().endpoint.is_some();
            if want && !have {
                if retry_at.is_none_or(|t| tokio::time::Instant::now() >= t) {
                    match endpoint::bind(&self.config, self.gate.clone(), self.hints.clone()).await
                    {
                        Ok(ep) => {
                            self.install(ep);
                            bind_backoff.reset();
                            retry_at = None;
                        }
                        Err(error) => {
                            warn!(%error, "the network endpoint didn't start");
                            self.lock().endpoint_error = Some(error);
                            retry_at =
                                Some(tokio::time::Instant::now() + bind_backoff.next_delay());
                        }
                    }
                    continue;
                }
            } else if !want && have {
                let code = if shutting {
                    CloseCode::Shutdown
                } else {
                    CloseCode::NotAdmitted
                };
                self.take_down(code).await;
                continue;
            } else if want {
                self.ensure_dialers();
            } else {
                retry_at = None;
                self.lock().endpoint_error = None;
            }
            ready.send_replace(true);
            if shutting {
                return;
            }
            match retry_at {
                Some(at) => {
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep_until(at) => {}
                    }
                }
                None => self.wake.notified().await,
            }
        }
    }

    fn install(self: &Arc<Self>, ep: Endpoint) {
        let generation = {
            let mut st = self.lock();
            st.endpoint = Some(ep.clone());
            st.endpoint_gen += 1;
            st.endpoint_error = None;
            let now = Instant::now();
            for ps in st.peers.values_mut() {
                ps.quiet_since = now;
            }
            st.endpoint_gen
        };
        info!(id = %self.me.fmt_short(), mode = self.config.network.mode.as_str(), "network endpoint up");
        tokio::spawn(self.clone().watch_home_relay(ep.clone(), generation));
        tokio::spawn(self.clone().accept_loop(ep));
    }

    async fn take_down(&self, code: CloseCode) {
        let ep = {
            let mut st = self.lock();
            let now_ms = unix_ms();
            for (id, ps) in st.peers.iter_mut() {
                if let Some(link) = ps.link.take() {
                    close(&link.conn, code);
                    ps.quiet_since = Instant::now();
                    self.emit(LinkEvent::Down {
                        device: *id,
                        last_seen_ms: now_ms,
                    });
                }
                ps.notify.notify_one();
            }
            st.endpoint_gen += 1;
            st.endpoint.take()
        };
        self.home_relay.send_replace(None);
        if let Some(ep) = ep {
            info!("network endpoint down");
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, ep.close()).await;
        }
    }

    fn begin_shutdown(&self) {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut st = self.lock();
        let now_ms = unix_ms();
        for (id, ps) in st.peers.iter_mut() {
            if let Some(link) = ps.link.take() {
                close(&link.conn, CloseCode::Shutdown);
                self.emit(LinkEvent::Down {
                    device: *id,
                    last_seen_ms: now_ms,
                });
            }
            ps.notify.notify_one();
        }
        drop(st);
        self.wake.notify_one();
    }

    async fn watch_home_relay(self: Arc<Self>, ep: Endpoint, generation: u64) {
        let mut watcher = ep.home_relay_status();
        let closed = ep.closed();
        // Holding the endpoint here would keep it alive after close.
        drop(ep);
        tokio::pin!(closed);
        let mut statuses = watcher.get();
        loop {
            let url = statuses
                .iter()
                .find(|s| s.is_connected())
                .or_else(|| statuses.first())
                .map(|s| s.url().clone());
            {
                let st = self.lock();
                if st.endpoint_gen != generation {
                    return;
                }
                self.home_relay.send_if_modified(|current| {
                    if *current == url {
                        false
                    } else {
                        *current = url;
                        true
                    }
                });
            }
            tokio::select! {
                _ = &mut closed => return,
                next = watcher.updated() => match next {
                    Ok(next) => statuses = next,
                    Err(_) => return,
                },
            }
        }
    }

    // ---- links (5.3) ----

    async fn accept_loop(self: Arc<Self>, ep: Endpoint) {
        while let Some(incoming) = ep.accept().await {
            let shared = self.clone();
            tokio::spawn(async move {
                // The admission hook has run by the time the handshake completes.
                match incoming.await {
                    Ok(conn) => shared.on_accepted(conn),
                    Err(error) => debug!(%error, "incoming connection failed"),
                }
            });
        }
    }

    fn on_accepted(self: &Arc<Self>, conn: Connection) {
        let peer = conn.remote_id();
        // The accept path checks admission again (5.2).
        if self.is_shutting_down() {
            close(&conn, CloseCode::Shutdown);
            return;
        }
        if !self.gate.is_admitted(&peer) {
            close(&conn, CloseCode::NotAdmitted);
            return;
        }
        debug!(device = %peer.fmt_short(), "accepted a connection");
        self.spawn_link_tasks(conn, self.next_id(), false);
    }

    fn ensure_dialers(self: &Arc<Self>) {
        let mut st = self.lock();
        if st.endpoint.is_none() || self.is_shutting_down() {
            return;
        }
        let mut start = Vec::new();
        for (id, ps) in st.peers.iter_mut() {
            if self.is_designated(id) && !ps.dialer_running {
                ps.dialer_running = true;
                start.push((*id, ps.serial));
            }
        }
        drop(st);
        for (id, serial) in start {
            tokio::spawn(self.clone().dialer(id, serial));
        }
    }

    /// Keeps a link to `peer` up while it is admitted, as its designated dialer (5.3).
    async fn dialer(self: Arc<Self>, peer: EndpointId, serial: u64) {
        let timing = *self.timing();
        let mut backoff = Backoff::new(timing.backoff_base, timing.backoff_max);
        loop {
            let (notify, linked) = {
                let mut st = self.lock();
                let has_endpoint = st.endpoint.is_some();
                let Some(ps) = st.peers.get_mut(&peer).filter(|ps| ps.serial == serial) else {
                    return;
                };
                if self.is_shutting_down() || !has_endpoint {
                    ps.dialer_running = false;
                    return;
                }
                (ps.notify.clone(), ps.link.is_some())
            };
            if linked {
                notify.notified().await;
                backoff.reset();
                continue;
            }
            match self.ensure_link(peer).await {
                Ok(_) => backoff.reset(),
                Err(error) => {
                    let delay = backoff.next_delay();
                    debug!(device = %peer.fmt_short(), %error, ?delay, "dial failed");
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = notify.notified() => {}
                    }
                }
            }
        }
    }

    /// The link to `peer`, dialing when there is none. Concurrent callers share one dial.
    async fn ensure_link(self: &Arc<Self>, peer: EndpointId) -> Result<Connection, CallError> {
        let asked = Instant::now();
        let (dial_lock, ep) = {
            let st = self.lock();
            let ps = st.peers.get(&peer).ok_or(CallError::NotAdmitted)?;
            if let Some(link) = &ps.link {
                return Ok(link.conn.clone());
            }
            let ep = st
                .endpoint
                .clone()
                .ok_or_else(|| CallError::Unreachable("the network endpoint isn't up".into()))?;
            (ps.dial_lock.clone(), ep)
        };
        let _dialing = dial_lock.lock().await;
        {
            let st = self.lock();
            let ps = st.peers.get(&peer).ok_or(CallError::NotAdmitted)?;
            if let Some(link) = &ps.link {
                return Ok(link.conn.clone());
            }
            // A dial that failed while this caller waited answers for it too.
            if let Some((at, why)) = &ps.last_dial_failure
                && *at >= asked
            {
                return Err(CallError::Unreachable(why.clone()));
            }
        }
        let timeout = self.timing().dial_timeout;
        let result = match tokio::time::timeout(timeout, self.dial(&ep, peer)).await {
            Ok(result) => result,
            Err(_) => Err(DialError::new(
                Problem::Unreachable,
                format!("no link within {} s", timeout.as_secs_f32()),
            )),
        };
        result.map_err(|error| {
            let mut st = self.lock();
            if let Some(ps) = st.peers.get_mut(&peer) {
                ps.last_dial_failure = Some((Instant::now(), error.detail.clone()));
                ps.problem = Some(LinkProblem {
                    device: peer.to_string(),
                    problem: error.problem.as_str().into(),
                    detail: error.detail.clone(),
                    at_ms: unix_ms(),
                });
            }
            match error.problem {
                Problem::Protocol => CallError::Protocol(error.detail),
                _ => CallError::Unreachable(error.detail),
            }
        })
    }

    async fn dial(
        self: &Arc<Self>,
        ep: &Endpoint,
        peer: EndpointId,
    ) -> Result<Connection, DialError> {
        let conn = ep
            .connect(peer, ALPN)
            .await
            .map_err(|e| DialError::new(Problem::Unreachable, format!("can't connect: {e}")))?;
        let version = self.client_hello(&conn).await?;
        let id = self.next_id();
        match self.register(&conn, id, true, version) {
            Ok(Registered::New) => {
                self.spawn_link_tasks(conn.clone(), id, true);
                Ok(conn)
            }
            Ok(Registered::Kept(existing)) => Ok(existing),
            Err(error) => Err(DialError::new(Problem::Unreachable, error.to_string())),
        }
    }

    /// The dialer's side of `hello`: send ours on the link's first stream and read theirs (5.4).
    async fn client_hello(&self, conn: &Connection) -> Result<u32, DialError> {
        let exchange = async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
            write_frame(&mut send, &Request::Hello(self.hello.clone()))
                .await
                .map_err(|e| e.to_string())?;
            send.finish().map_err(|e| e.to_string())?;
            let payload = read_frame(&mut recv).await.map_err(|e| e.to_string())?;
            Response::parse(&payload).map_err(|e| e.to_string())
        };
        match exchange.await {
            Ok(Response::Hello(theirs)) => match self.hello.negotiate(&theirs) {
                Some(version) => Ok(version),
                None => {
                    close(conn, CloseCode::Version);
                    Err(DialError::new(
                        Problem::Version,
                        version_detail(&self.hello, &theirs),
                    ))
                }
            },
            Ok(other) => {
                conn.close(VarInt::from_u32(HELLO_FAILED.0), HELLO_FAILED.1);
                Err(DialError::new(
                    Problem::Protocol,
                    format!("hello was answered with {other:?}"),
                ))
            }
            Err(error) => match conn.close_reason() {
                Some(reason) => Err(DialError::from_close(&reason)),
                None => {
                    conn.close(VarInt::from_u32(HELLO_FAILED.0), HELLO_FAILED.1);
                    Err(DialError::new(
                        Problem::Protocol,
                        format!("hello failed: {error}"),
                    ))
                }
            },
        }
    }

    /// Makes `conn` the device's link once `hello` is done, keeping one link per pair (5.3).
    fn register(
        &self,
        conn: &Connection,
        id: u64,
        dialed_by_me: bool,
        version: u32,
    ) -> Result<Registered, CallError> {
        let peer = conn.remote_id();
        let mut st = self.lock();
        if self.is_shutting_down() || st.endpoint.is_none() {
            close(conn, CloseCode::Shutdown);
            return Err(CallError::Unreachable(
                "the network endpoint is closing".into(),
            ));
        }
        if !self.gate.is_admitted(&peer) {
            close(conn, CloseCode::NotAdmitted);
            return Err(CallError::NotAdmitted);
        }
        let me_designated = self.is_designated(&peer);
        let Some(ps) = st.peers.get_mut(&peer) else {
            close(conn, CloseCode::NotAdmitted);
            return Err(CallError::NotAdmitted);
        };
        let now_ms = unix_ms();
        let new = Link {
            id,
            conn: conn.clone(),
            dialed_by_me,
            version,
            since_ms: now_ms,
        };
        match ps.link.take() {
            None => {
                ps.link = Some(new);
                self.emit(LinkEvent::Up(peer));
            }
            Some(old) => {
                // Both sides keep the link the designated dialer opened; between two links from the same dialer
                // the newer wins, since the older one is most likely left over from before a restart.
                let new_by_designated = dialed_by_me == me_designated;
                let old_by_designated = old.dialed_by_me == me_designated;
                if old_by_designated && !new_by_designated {
                    close(conn, CloseCode::Duplicate);
                    let kept = old.conn.clone();
                    ps.link = Some(old);
                    return Ok(Registered::Kept(kept));
                }
                close(&old.conn, CloseCode::Duplicate);
                ps.link = Some(new);
                // A new link may mean the peer restarted, so the daemon hears Down then Up and resends presence.
                self.emit(LinkEvent::Down {
                    device: peer,
                    last_seen_ms: now_ms,
                });
                self.emit(LinkEvent::Up(peer));
            }
        }
        ps.problem = None;
        ps.last_dial_failure = None;
        info!(device = %peer.fmt_short(), dialed_by_me, "link up");
        Ok(Registered::New)
    }

    fn spawn_link_tasks(self: &Arc<Self>, conn: Connection, id: u64, dialed_by_me: bool) {
        // The dialer has finished hello before its link is served; the other side waits for it.
        let hello_done = Arc::new(AtomicBool::new(dialed_by_me));
        let shared = self.clone();
        let watched = conn.clone();
        tokio::spawn(async move {
            let reason = watched.closed().await;
            shared.on_closed(watched.remote_id(), id, &reason);
        });
        if !dialed_by_me {
            let waiting = conn.clone();
            let done = hello_done.clone();
            let grace = self.timing().dial_timeout;
            tokio::spawn(async move {
                if tokio::time::timeout(grace, waiting.closed()).await.is_err()
                    && !done.load(Ordering::Acquire)
                {
                    debug!(device = %waiting.remote_id().fmt_short(), "no hello in time");
                    waiting.close(VarInt::from_u32(HELLO_FAILED.0), b"hello-timeout");
                }
            });
        }
        tokio::spawn(self.clone().serve(conn, id, hello_done));
    }

    fn on_closed(&self, peer: EndpointId, id: u64, reason: &ConnectionError) {
        let mut st = self.lock();
        let Some(ps) = st.peers.get_mut(&peer) else {
            return;
        };
        if ps.link.as_ref().is_none_or(|link| link.id != id) {
            return;
        }
        let Some(link) = ps.link.take() else {
            return;
        };
        let now_ms = unix_ms();
        // A timed-out link was last heard from about one idle timeout ago.
        let last_seen_ms = match reason {
            ConnectionError::TimedOut => {
                let idle = u64::try_from(self.timing().idle_timeout.as_millis()).unwrap_or(0);
                now_ms.saturating_sub(idle).max(link.since_ms)
            }
            _ => now_ms,
        };
        ps.quiet_since = Instant::now();
        if let Some(code @ (CloseCode::NotAdmitted | CloseCode::Version)) = close_code_of(reason) {
            let error = DialError::from_close(reason);
            ps.problem = Some(LinkProblem {
                device: peer.to_string(),
                problem: error.problem.as_str().into(),
                detail: error.detail,
                at_ms: now_ms,
            });
            debug!(device = %peer.fmt_short(), code = code.reason(), "link closed by the peer");
        }
        info!(device = %peer.fmt_short(), reason = %describe_close(reason), "link down");
        self.emit(LinkEvent::Down {
            device: peer,
            last_seen_ms,
        });
        ps.notify.notify_one();
    }

    fn set_problem(&self, peer: &EndpointId, problem: Problem, detail: String) {
        if let Some(ps) = self.lock().peers.get_mut(peer) {
            ps.problem = Some(LinkProblem {
                device: peer.to_string(),
                problem: problem.as_str().into(),
                detail,
                at_ms: unix_ms(),
            });
        }
    }

    // ---- calls (5.4) ----

    /// The link for a call: the one that is up, else a dial. The device that isn't the designated dialer dials
    /// only after no link has come up for `lazy_dial_after` (5.3); until then the call fails and the courier
    /// retries, normally when the peer's own dial brings the link up.
    async fn link_for_call(self: &Arc<Self>, device: EndpointId) -> Result<Connection, CallError> {
        let quiet_for = {
            let st = self.lock();
            let ps = st.peers.get(&device).ok_or(CallError::NotAdmitted)?;
            if let Some(link) = &ps.link {
                return Ok(link.conn.clone());
            }
            ps.quiet_since.elapsed()
        };
        if self.is_designated(&device) || quiet_for >= self.timing().lazy_dial_after {
            self.ensure_link(device).await
        } else {
            Err(CallError::Unreachable(format!(
                "no link yet; waiting for {} to connect",
                short(&device)
            )))
        }
    }

    async fn call(
        self: Arc<Self>,
        device: EndpointId,
        request: Request,
    ) -> Result<Response, CallError> {
        let kind = request.kind();
        if kind == CallKind::Hello {
            return Err(CallError::Protocol("hello is the link's own call".into()));
        }
        if !self.gate.is_admitted(&device) {
            return Err(CallError::NotAdmitted);
        }
        let frame = encode_frame(&request).map_err(|e| CallError::Protocol(e.to_string()))?;
        let conn = self.link_for_call(device).await?;
        let lost = |error: String| match conn.close_reason() {
            Some(reason) => CallError::Unreachable(describe_close(&reason)),
            None => CallError::Unreachable(error),
        };
        let exchange = async {
            let (mut send, mut recv) = conn.open_bi().await.map_err(|e| lost(e.to_string()))?;
            send.write_all(&frame)
                .await
                .map_err(|e| lost(e.to_string()))?;
            send.finish().map_err(|e| lost(e.to_string()))?;
            read_frame(&mut recv).await.map_err(|e| match e {
                FrameError::Io(e) => lost(e.to_string()),
                other => CallError::Protocol(other.to_string()),
            })
        };
        let payload = tokio::time::timeout(self.timing().call_timeout, exchange)
            .await
            .map_err(|_| CallError::Timeout)??;
        let response = Response::parse(&payload).map_err(|e| CallError::Protocol(e.to_string()))?;
        if !response.answers(kind) {
            return Err(CallError::Protocol(format!(
                "{} was answered with {response:?}",
                kind.as_str()
            )));
        }
        Ok(response)
    }

    async fn serve(self: Arc<Self>, conn: Connection, id: u64, hello_done: Arc<AtomicBool>) {
        // QUIC caps these at 16 open streams (5.4), so this spawns at most 16 handlers per link at once.
        while let Ok((send, recv)) = conn.accept_bi().await {
            let opened = tokio::time::Instant::now();
            tokio::spawn(self.clone().handle_stream(
                conn.clone(),
                id,
                hello_done.clone(),
                send,
                recv,
                opened,
            ));
        }
    }

    async fn handle_stream(
        self: Arc<Self>,
        conn: Connection,
        id: u64,
        hello_done: Arc<AtomicBool>,
        mut send: SendStream,
        mut recv: RecvStream,
        opened: tokio::time::Instant,
    ) {
        let deadline = opened + self.timing().first_frame;
        let payload = match tokio::time::timeout_at(deadline, read_frame(&mut recv)).await {
            Ok(Ok(payload)) => payload,
            Err(_) => {
                debug!(device = %conn.remote_id().fmt_short(), "no request in time; resetting the stream");
                let _ = recv.stop(VarInt::from_u32(0));
                let _ = send.reset(VarInt::from_u32(0));
                return;
            }
            // Reset by the peer, or the link is gone: nobody to answer.
            Ok(Err(FrameError::Io(_))) => return,
            Ok(Err(error)) => {
                // Don't read the rest of an oversized or broken frame.
                let _ = recv.stop(VarInt::from_u32(0));
                respond(&mut send, &Response::from(&error)).await;
                return;
            }
        };
        if let Some(response) = self.answer(&conn, id, &hello_done, &payload).await {
            respond(&mut send, &response).await;
        }
    }

    async fn answer(
        self: &Arc<Self>,
        conn: &Connection,
        id: u64,
        hello_done: &AtomicBool,
        payload: &[u8],
    ) -> Option<Response> {
        let device = conn.remote_id();
        // Every call re-checks admission before acting (5.2).
        let Some(peer) = self.peer(&device) else {
            close(conn, CloseCode::NotAdmitted);
            return None;
        };
        // Read once, so a call racing the acceptor's hello can't skip the rate check below and still be served.
        let ready = hello_done.load(Ordering::Acquire);
        // Calls are counted before their bodies are parsed (5.5 step 2): only `t` and `id` are read first, so a
        // call over the limit never has its JSON built.
        let head = CallHead::read(payload);
        if ready {
            match self.check_rate(&device, &head) {
                Ok(None) => {}
                Ok(Some(early)) => return Some(early),
                Err(NotAdmitted) => {
                    close(conn, CloseCode::NotAdmitted);
                    return None;
                }
            }
        }
        let call = match parse_incoming(payload) {
            Ok(call) => call,
            Err(error) => {
                debug!(device = %device.fmt_short(), %error, "bad request");
                return Some(error.response());
            }
        };
        // The head picked the limit, so a frame whose full parse names another call is refused, never served. The
        // two read repeated keys alike; this only guards against them drifting apart.
        if head.kind != Some(call.kind()) {
            return Some(Response::Error {
                code: ErrorCode::Malformed,
            });
        }
        if let Incoming::Hello(theirs) = call {
            return self.server_hello(conn, id, hello_done, theirs);
        }
        if !ready {
            return Some(Response::Error {
                code: ErrorCode::HelloRequired,
            });
        }
        Some(self.inbound.handle(peer, call).await)
    }

    /// The other side of `hello` (5.4): answer with ours; with no common version, let the dialer see ours and
    /// close with code 3.
    fn server_hello(
        self: &Arc<Self>,
        conn: &Connection,
        id: u64,
        hello_done: &AtomicBool,
        theirs: Hello,
    ) -> Option<Response> {
        let ours = Response::Hello(self.hello.clone());
        if hello_done.load(Ordering::Acquire) {
            return Some(ours);
        }
        let device = conn.remote_id();
        match self.hello.negotiate(&theirs) {
            None => {
                self.set_problem(
                    &device,
                    Problem::Version,
                    version_detail(&self.hello, &theirs),
                );
                let conn = conn.clone();
                let grace = self.timing().first_frame;
                tokio::spawn(async move {
                    if tokio::time::timeout(grace, conn.closed()).await.is_err() {
                        close(&conn, CloseCode::Version);
                    }
                });
                Some(ours)
            }
            Some(version) => match self.register(conn, id, false, version) {
                // Marked before answering, so a call the dialer sends right after our hello isn't refused.
                Ok(Registered::New) => {
                    hello_done.store(true, Ordering::Release);
                    Some(ours)
                }
                Ok(Registered::Kept(_)) | Err(_) => None,
            },
        }
    }

    /// The per-peer limits (5.5 step 2, 5.6), checked on the call's head before the body is parsed.
    /// `Ok(Some(_))` answers the call here; `Err` means the device was removed meanwhile.
    fn check_rate(
        &self,
        device: &EndpointId,
        head: &CallHead,
    ) -> Result<Option<Response>, NotAdmitted> {
        let now = Instant::now();
        let mut st = self.lock();
        let ps = st.peers.get_mut(device).ok_or(NotAdmitted)?;
        Ok(match head.kind {
            Some(CallKind::Deliver) if !ps.deliver.allow(now) => {
                Some(rate_limited(head.id.as_deref()))
            }
            // Dropped, but answered `ok` so the sender doesn't retry it.
            Some(CallKind::Presence) if !ps.presence.allow(now) => Some(Response::Ok),
            _ => None,
        })
    }

    // ---- status (5.1, 5.3, 6.2) ----

    fn status(&self) -> LinkStatus {
        let st = self.lock();
        let mut problems: Vec<LinkProblem> = st
            .peers
            .values()
            .filter_map(|ps| ps.problem.clone())
            .collect();
        problems.sort_by(|a, b| a.device.cmp(&b.device));
        let Some(ep) = st.endpoint.clone() else {
            return LinkStatus {
                network: "off".into(),
                error: st.endpoint_error.clone(),
                problems,
                ..LinkStatus::default()
            };
        };
        let admission = self.gate.admission();
        let mut links: Vec<LinkInfo> = st
            .peers
            .iter()
            .filter_map(|(device, ps)| {
                let link = ps.link.as_ref()?;
                let (path, rtt_ms) = selected_path(&link.conn);
                let known = admission.get(device);
                Some(LinkInfo {
                    device: device.to_string(),
                    path,
                    rtt_ms,
                    name: known.map(|d| d.name.clone()).unwrap_or_default(),
                    person: known.map(|d| d.person.clone()).unwrap_or_default(),
                    since_ms: link.since_ms,
                    version: link.version,
                    dialed: link.dialed_by_me,
                })
            })
            .collect();
        drop(st);
        links.sort_by(|a, b| a.device.cmp(&b.device));
        let statuses = ep.home_relay_status().get();
        let relays: Vec<RelayInfo> = statuses
            .iter()
            .map(|s| RelayInfo {
                url: s.url().to_string(),
                connected: s.is_connected(),
                error: s.last_error().map(|e| e.to_string()),
                auth_denied: s.auth_denied_reason().map(str::to_string),
            })
            .collect();
        LinkStatus {
            network: self.config.network.mode.as_str().into(),
            endpoint_id: Some(self.me.to_string()),
            relay: statuses
                .iter()
                .find(|s| s.is_connected())
                .map(|s| s.url().to_string()),
            links,
            bound_sockets: ep.bound_sockets().iter().map(ToString::to_string).collect(),
            relays,
            error: None,
            problems,
        }
    }
}

/// The selected path of a link: `direct` or `relay` and its round-trip time (5.3).
fn selected_path(conn: &Connection) -> (String, Option<u64>) {
    let paths = conn.paths();
    let chosen = paths
        .iter()
        .find(|p| p.is_selected())
        .or_else(|| paths.iter().next());
    match chosen {
        Some(path) => (
            if path.is_relay() { "relay" } else { "direct" }.into(),
            Some(u64::try_from(path.rtt().as_millis()).unwrap_or(u64::MAX)),
        ),
        None => ("none".into(), None),
    }
}

fn version_detail(ours: &Hello, theirs: &Hello) -> String {
    format!(
        "no protocol version in common: it speaks {} to {} ({}), this build {} to {}",
        theirs.proto_min, theirs.proto_max, theirs.app, ours.proto_min, ours.proto_max
    )
}

/// The receipt for a `deliver` over the rate limit (5.5 step 2), with the id from the call's head; a call without
/// a valid id can't be answered with a receipt and gets `malformed`.
fn rate_limited(id: Option<&str>) -> Response {
    match id.filter(|id| is_ulid(id)) {
        Some(id) => Response::Receipt(DeliverReceipt {
            id: id.to_string(),
            state: DeliverState::Refused,
            code: Some(RefusalCode::RateLimited),
        }),
        None => Response::Error {
            code: ErrorCode::Malformed,
        },
    }
}

async fn respond(send: &mut SendStream, response: &Response) {
    if write_frame(send, response).await.is_ok() {
        let _ = send.finish();
    }
}
