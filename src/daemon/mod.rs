//! The daemon process (3, 6.1, 12): lock, settings, identity, store, roster, control socket, links and the
//! background work that keeps them moving.
//!
//! Start order (6.1): take the lock, read settings, load or create the identity, open the store, choose the
//! roster, remove a stale socket, bind the new one 0600, record its path, then accept.

pub mod links;
pub mod roster;
pub mod testnet;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context as _, bail};
use iroh::EndpointId;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::control::place::{
    DaemonLock, bind_private, choose_socket_path, read_socket_path, remove_previous_socket,
    remove_stale_socket, take_lock, write_socket_path,
};
use crate::control::protocol::{
    self, DeviceList, DeviceStatus, Empty, LoginResult, LoginUrl, LogoutResult, Reason,
    RevokeParams, Revoked, RpcError, Status, TokenOnly, method,
};
use crate::control::server::{Conn, Handler, serve};
use crate::courier::outbox::CALL_TIMEOUT;
use crate::courier::{Courier, CourierConfig, LocalDevice};
use crate::identity::{DeviceIdentity, default_device_name, platform};
use crate::link::{LinkEvent, Links};
use crate::paths::Home;
use crate::presence::{DEBOUNCE_MS, REFRESH_MS};
use crate::roster::local::{local_username, os_user_name};
use crate::roster::{ActiveRoster, file::POLL_SECONDS};
use crate::settings::{Env, Settings};
use crate::spew_api::{ApiError, BaseUrl, Clock, SpewApi};
use crate::store::{RosterProvider, Store};
use crate::wire::Request;

use links::{InboundDyn, InboundHandle, OffLinks};
use roster::RosterManager;

/// How long `auth.login` waits for the browser (14.1).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long `directory.refresh` waits for the roster.
const REFRESH_WAIT: Duration = Duration::from_secs(15);
/// How long `daemon.shutdown` leaves for its answer to go out.
const SHUTDOWN_DELAY: Duration = Duration::from_millis(100);

/// What a daemon needs from the process that runs it.
pub struct DaemonOptions {
    /// The home directory.
    pub home: Home,
    /// The person's home directory, for `~` in settings.
    pub user_home: Option<PathBuf>,
    /// The spew mesh environment variables.
    pub env: Env,
    /// `$XDG_RUNTIME_DIR`, `$TMPDIR` and `/tmp`, for a long home path (6.1).
    pub runtime_dirs: Vec<Option<PathBuf>>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
}

// Registering this device with spew again after the try at sign-in failed (14.3). Registering is idempotent, so
// it is retried after each spew refresh until the roster lists this device.
struct Registration {
    key: iroh::SecretKey,
    name: String,
    platform: String,
    pending: Arc<AtomicBool>,
}

impl Registration {
    // Tries once when a retry is pending; true when this device was registered just now.
    async fn retry(&self, mgr: &RosterManager) -> bool {
        if !self.pending.load(Ordering::Relaxed) {
            return false;
        }
        if mgr.spew_lists_this_device() == Some(true) {
            self.pending.store(false, Ordering::Relaxed);
            return false;
        }
        let Some(api) = mgr.api() else {
            return false;
        };
        match api
            .register_device(&self.key, &self.name, &self.platform)
            .await
        {
            Ok(_) => {
                self.pending.store(false, Ordering::Relaxed);
                tracing::info!("registered this device with spew");
                true
            }
            Err(e) => {
                tracing::info!(error = %e, "device registration still fails; trying again at the next refresh");
                false
            }
        }
    }
}

enum RosterCmd {
    // Re-read the roster now (`directory.refresh`).
    Refresh(oneshot::Sender<()>),
    // The sign-in changed: forget the spew cache and re-read.
    SignInChanged(oneshot::Sender<()>),
}

/// A running daemon.
pub struct Daemon<L: Links> {
    /// Its courier, for tests that look inside.
    pub courier: Arc<Courier<L>>,
    /// The control socket.
    pub socket: PathBuf,
    /// This device.
    pub device: EndpointId,
    home: Home,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    _lock: DaemonLock,
}

impl<L: Links> Daemon<L> {
    /// Asks the daemon to stop.
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Resolves once something asked the daemon to stop.
    pub async fn stopped(&self) {
        let mut rx = self.shutdown.subscribe();
        let _ = rx.wait_for(|stop| *stop).await;
    }

    /// Stops the background work and removes the socket; the lock goes when this returns.
    pub async fn finish(self) {
        self.stop();
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks {
            let _ = task.await;
        }
        let _ = remove_stale_socket(&self.socket);
        if read_socket_path(&self.home).as_deref() == Some(self.socket.as_path()) {
            let _ = std::fs::remove_file(self.home.socket_file());
        }
        tracing::info!("daemon stopped");
    }
}

/// Runs the production daemon until `daemon.shutdown`, SIGTERM or SIGINT.
pub async fn run(opts: DaemonOptions) -> anyhow::Result<()> {
    let root = opts.home.root().display().to_string();
    let Some(daemon) = start(opts, |_| (OffLinks::default(), InboundHandle::new())).await? else {
        bail!("another spew mesh daemon is already running for {root}");
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("can't listen for SIGTERM")?;
    tokio::select! {
        _ = daemon.stopped() => {}
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
    }
    daemon.finish().await;
    Ok(())
}

/// Starts a daemon with the links `make_links` builds for this device. `Ok(None)` when another daemon holds
/// this home's lock; nothing is touched then.
pub async fn start<L, F>(opts: DaemonOptions, make_links: F) -> anyhow::Result<Option<Daemon<L>>>
where
    L: Links,
    F: FnOnce(EndpointId) -> (L, InboundHandle),
{
    let home = opts.home.clone();
    home.ensure()
        .with_context(|| format!("can't prepare {}", home.root().display()))?;
    let started_at = opts.clock.now_ms();
    let Some(lock) = take_lock(&home, started_at)
        .with_context(|| format!("can't take {}", home.lock_file().display()))?
    else {
        return Ok(None);
    };
    let mut settings = Settings::load(&home, opts.user_home.as_deref())?;
    settings.apply_env(&opts.env);
    let api = BaseUrl::parse(settings.spew.url.as_str())
        .and_then(|base| SpewApi::new(base, home.spew_auth_file(), opts.clock.clone()))
        .map_err(|e| e.to_string());

    let identity = load_identity(&home, &settings, started_at)?;
    let device = identity.endpoint_id();
    let store = Store::open(&home.store_file())
        .with_context(|| format!("can't open {}", home.store_file().display()))?;
    let cached = store.roster(RosterProvider::Spew).ok().flatten();
    let mut mgr = RosterManager::new(&settings, device, identity.name(), api.clone(), cached);
    let chosen = mgr.current(started_at);
    tracing::info!(
        device = %identity.name(),
        id = %identity.short_id(),
        roster = %chosen.info.provider,
        person = %chosen.me.username,
        "daemon starting"
    );
    let active = ActiveRoster::new(chosen.me, device, chosen.roster);

    // A socket an earlier daemon left outside the home would otherwise stay there for good.
    if let Err(e) = remove_previous_socket(&home) {
        tracing::warn!(error = %e, "could not remove the previous daemon's socket");
    }
    let socket =
        choose_socket_path(&home, &opts.runtime_dirs).context("can't choose a socket path")?;
    remove_stale_socket(&socket).with_context(|| format!("can't clear {}", socket.display()))?;
    let listener =
        bind_private(&socket).with_context(|| format!("can't bind {}", socket.display()))?;
    write_socket_path(&home, &socket).context("can't record the socket path")?;

    let (links, inbound) = make_links(device);
    // Subscribed before admission is set, so the first link ups aren't missed.
    let link_events = links.events();
    let cfg = CourierConfig {
        device: LocalDevice::new(device, identity.name(), platform()),
        default_room: settings.roster.default_room.clone(),
        send_branch: settings.presence.branch,
        send_place: settings.presence.place,
        serve: settings.serve.clone(),
        clock: opts.clock.clone(),
        start_time: crate::sessions::process::start_time,
    };
    let courier = Arc::new(Courier::new(links, store, active, chosen.info, cfg)?);
    let weak: Weak<dyn InboundDyn> = Arc::downgrade(&courier) as Weak<dyn InboundDyn>;
    inbound.bind(weak);
    courier.start_links();

    let (shutdown, shutdown_rx) = watch::channel(false);
    let (roster_tx, roster_rx) = mpsc::channel(8);
    let registration = Registration {
        key: identity.secret_key().clone(),
        name: identity.name().to_string(),
        platform: platform().to_string(),
        // Starts pending so a registration that failed before a restart is tried again; the first
        // refresh clears it when spew already lists this device.
        pending: Arc::new(AtomicBool::new(true)),
    };
    let registration_pending = registration.pending.clone();
    let control = Arc::new(Control {
        courier: courier.clone(),
        api,
        identity,
        registration_pending,
        roster: roster_tx,
        shutdown: shutdown.clone(),
        base: StatusBase {
            pid: std::process::id(),
            started_at,
            home: home.root().display().to_string(),
            socket: socket.display().to_string(),
        },
    });
    let tasks = vec![
        tokio::spawn(link_task(courier.clone(), link_events)),
        tokio::spawn(outbox_task(courier.clone())),
        tokio::spawn(presence_task(courier.clone())),
        tokio::spawn(roster_task(courier.clone(), mgr, registration, roster_rx)),
        tokio::spawn(serve(listener, control, shutdown_rx)),
    ];
    tracing::info!(socket = %socket.display(), "listening");
    Ok(Some(Daemon {
        courier,
        socket,
        device,
        home,
        shutdown,
        tasks,
        _lock: lock,
    }))
}

// The device file, created on the first start with `device_name` or `<person>-<platform>` (2, 4.1).
fn load_identity(home: &Home, settings: &Settings, now: u64) -> anyhow::Result<DeviceIdentity> {
    if let Some(identity) = DeviceIdentity::load(home)? {
        return Ok(identity);
    }
    let name = match &settings.device_name {
        Some(name) => name.clone(),
        None => {
            let person = local_username(settings.username.as_deref(), os_user_name().as_deref())
                .unwrap_or_else(|_| "local".into());
            default_device_name(&person, platform(), |_| false).unwrap_or_else(|| "device".into())
        }
    };
    let (identity, created) = DeviceIdentity::load_or_create(home, &name, now)?;
    if created {
        tracing::info!(device = %identity.name(), "created this device's identity");
    }
    Ok(identity)
}

// Sends presence to `devices` (every online device when `None`), each after its hold (5.6).
fn send_presence<L: Links>(courier: &Arc<Courier<L>>, devices: Option<&[EndpointId]>) {
    for (device, snapshot, hold) in courier.presence_round(devices) {
        let courier = courier.clone();
        tokio::spawn(async move {
            if !hold.is_zero() {
                tokio::time::sleep(hold).await;
            }
            if let Err(e) = courier
                .call(device, Request::Presence(snapshot), CALL_TIMEOUT)
                .await
            {
                tracing::debug!(device = %device.fmt_short(), error = %e, "presence not sent");
            }
        });
    }
}

fn spawn_pump<L: Links>(courier: &Arc<Courier<L>>, device: EndpointId, all: bool) {
    let courier = courier.clone();
    tokio::spawn(async move { courier.pump(device, all).await });
}

async fn link_task<L: Links>(courier: Arc<Courier<L>>, mut events: broadcast::Receiver<LinkEvent>) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if let Some(device) = courier.on_link_event(&event) {
                    send_presence(&courier, Some(&[device]));
                    spawn_pump(&courier, device, true);
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(missed = n, "link events lagged");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn outbox_task<L: Links>(courier: Arc<Courier<L>>) {
    let mut every = tokio::time::interval(Duration::from_secs(1));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = every.tick() => {}
            _ = courier.outbox_signal() => {}
        }
        for device in courier.tick() {
            spawn_pump(&courier, device, false);
        }
    }
}

async fn presence_task<L: Links>(courier: Arc<Courier<L>>) {
    loop {
        tokio::select! {
            _ = courier.presence_changed_signal() => {
                tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(REFRESH_MS)) => {}
        }
        send_presence(&courier, None);
    }
}

async fn roster_task<L: Links>(
    courier: Arc<Courier<L>>,
    mut mgr: RosterManager,
    registration: Registration,
    mut commands: mpsc::Receiver<RosterCmd>,
) {
    let mut every = tokio::time::interval(Duration::from_secs(POLL_SECONDS));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let mut reply = None;
        tokio::select! {
            _ = every.tick() => {}
            cmd = commands.recv() => match cmd {
                Some(RosterCmd::Refresh(tx)) => reply = Some(tx),
                Some(RosterCmd::SignInChanged(tx)) => {
                    mgr.reset_spew();
                    reply = Some(tx);
                }
                None => return,
            },
        }
        let now = courier.now();
        let forced = reply.is_some();
        if (forced || mgr.refresh_due(now))
            && let Some(cached) = mgr.refresh_spew(now).await
        {
            courier.save_roster_cache(&cached);
            // A device registered just now is in the next roster; fetch it at once.
            if registration.retry(&mgr).await
                && let Some(cached) = mgr.refresh_spew(courier.now()).await
            {
                courier.save_roster_cache(&cached);
            }
        }
        // A roster file is re-read only when it changed; a spew roster's view moves with time.
        let file_changed = mgr.file_changed();
        if forced || file_changed || mgr.uses_spew() {
            let chosen = mgr.current(courier.now());
            courier.apply_roster(chosen.me, chosen.roster, chosen.info);
        }
        if let Some(tx) = reply {
            let _ = tx.send(());
        }
    }
}

struct StatusBase {
    pid: u32,
    started_at: u64,
    home: String,
    socket: String,
}

// The control socket's request handler (6.2).
struct Control<L: Links> {
    courier: Arc<Courier<L>>,
    api: Result<SpewApi, String>,
    identity: DeviceIdentity,
    registration_pending: Arc<AtomicBool>,
    roster: mpsc::Sender<RosterCmd>,
    shutdown: watch::Sender<bool>,
    base: StatusBase,
}

fn api_err(e: ApiError) -> RpcError {
    let message = e.to_string();
    match e {
        ApiError::NotSignedIn | ApiError::SignedOut(_) => RpcError::forbidden(message),
        ApiError::Device(_) => RpcError::not_found(message),
        ApiError::GrantHasDevice => RpcError::conflict(message),
        ApiError::MeshUnsupported(_) | ApiError::Login(_) => RpcError::refused(message),
        _ => RpcError::unavailable(message),
    }
}

fn empty() -> Result<Value, RpcError> {
    protocol::result(&Empty {})
}

impl<L: Links> Control<L> {
    fn api(&self) -> Result<&SpewApi, RpcError> {
        self.api
            .as_ref()
            .map_err(|e| RpcError::unavailable(format!("the spew URL can't be used: {e}")))
    }

    async fn roster(&self, make: fn(oneshot::Sender<()>) -> RosterCmd) -> Result<(), RpcError> {
        let (tx, rx) = oneshot::channel();
        self.roster
            .send(make(tx))
            .await
            .map_err(|_| RpcError::unavailable("the roster task stopped"))?;
        match tokio::time::timeout(REFRESH_WAIT, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(RpcError::unavailable("the roster task stopped")),
            Err(_) => Err(RpcError::new(
                Reason::Timeout,
                "the roster refresh is taking long; it goes on in the background",
            )),
        }
    }

    fn status(&self) -> Status {
        let (counts, network) = self.courier.counts();
        let device = self.courier.device();
        Status {
            version: crate::control::client::own_version().to_string(),
            pid: self.base.pid,
            started_at: self.base.started_at,
            home: self.base.home.clone(),
            socket: self.base.socket.clone(),
            device: DeviceStatus {
                name: device.name.clone(),
                endpoint_id: device.hex.clone(),
                short_id: crate::identity::short_id(&device.id),
                platform: device.platform.clone(),
            },
            roster: self.courier.roster_status(),
            network,
            // The bound sockets come from the iroh endpoint, which the link layer owns (5.1).
            bound_sockets: Vec::new(),
            counts,
        }
    }

    async fn login(&self, conn: &Conn) -> Result<LoginResult, RpcError> {
        let api = self.api()?.clone();
        let flow = api.start_login().await.map_err(api_err)?;
        let url = LoginUrl {
            url: flow.authorize_url().to_string(),
        };
        conn.notify(method::AUTH_LOGIN_URL, &protocol::result(&url)?);
        let signed_in = tokio::select! {
            done = flow.complete(LOGIN_TIMEOUT) => done.map_err(api_err)?,
            _ = conn.closed() => return Err(RpcError::refused("the sign-in was abandoned")),
        };
        let device = self.courier.device();
        let (registered, note) = match api
            .register_device(self.identity.secret_key(), &device.name, &device.platform)
            .await
        {
            Ok(_) => (true, None),
            Err(e) => {
                tracing::warn!(error = %e, "device registration failed; retrying at each roster refresh");
                (
                    false,
                    Some(format!(
                        "this device isn't registered yet ({e}); the daemon keeps trying"
                    )),
                )
            }
        };
        self.registration_pending
            .store(!registered, Ordering::Relaxed);
        if let Err(e) = self.roster(RosterCmd::SignInChanged).await {
            tracing::info!(error = %e.message, "roster not refreshed after sign-in");
        }
        tracing::info!(person = %signed_in.me.username, "signed in to spew");
        Ok(LoginResult {
            username: signed_in.me.username,
            display: signed_in.me.display,
            projects: signed_in.projects.into_iter().map(|p| p.key).collect(),
            device: device.name.clone(),
            registered,
            note,
        })
    }

    async fn logout(&self) -> Result<LogoutResult, RpcError> {
        let report = self.api()?.logout().await.map_err(api_err)?;
        if let Err(e) = self.roster(RosterCmd::SignInChanged).await {
            tracing::info!(error = %e.message, "roster not refreshed after sign-out");
        }
        Ok(LogoutResult {
            was_signed_in: report.was_signed_in,
            device_unregistered: report.device_unregistered,
            token_revoked: report.token_revoked,
        })
    }

    async fn dispatch(&self, conn: Conn, name: &str, params: Value) -> Result<Value, RpcError> {
        use protocol::{params as p, result as r};
        let c = &self.courier;
        match name {
            method::SESSION_OPEN => r(&c.open_session(&conn, p(params)?)?),
            method::SESSION_UPDATE => {
                c.update_session(p(params)?)?;
                empty()
            }
            method::SESSION_CLOSE => {
                let t: TokenOnly = p(params)?;
                c.close_session(&t.token)?;
                empty()
            }
            method::HOOK_EVENT => r(&c.hook_event(p(params)?)?),
            method::MESSAGES_ACK => {
                c.ack(p(params)?)?;
                empty()
            }
            method::DIRECTORY_LIST => r(&c.directory(&p(params)?)?),
            method::MESSAGE_ASK => r(&c.ask(&conn, p(params)?).await?),
            method::MESSAGE_TELL => r(&c.tell(p(params)?).await?),
            method::MESSAGE_REPLY => r(&c.reply(&conn, p(params)?).await?),
            method::MESSAGES_FETCH => r(&c.fetch(&conn, p(params)?).await?),
            method::RESPONDER_TAKE => r(&c.take(&conn, p(params)?).await?),
            method::DAEMON_STATUS => {
                let _: Empty = p(params)?;
                r(&self.status())
            }
            method::DIRECTORY_REFRESH => {
                let _: Empty = p(params)?;
                self.roster(RosterCmd::Refresh).await?;
                empty()
            }
            method::DEVICES_LIST => {
                let _: Empty = p(params)?;
                let devices = self.api()?.list_devices().await.map_err(api_err)?;
                r(&DeviceList { devices })
            }
            method::DEVICES_REVOKE => {
                let q: RevokeParams = p(params)?;
                let d = self
                    .api()?
                    .revoke_device_named(&q.device)
                    .await
                    .map_err(api_err)?;
                if let Err(e) = self.roster(RosterCmd::Refresh).await {
                    tracing::info!(error = %e.message, "roster not refreshed after a revoke");
                }
                r(&Revoked {
                    name: d.name,
                    endpoint_id: d.public_key.to_string(),
                })
            }
            method::AUTH_LOGIN => {
                let _: Empty = p(params)?;
                r(&self.login(&conn).await?)
            }
            method::AUTH_LOGOUT => {
                let _: Empty = p(params)?;
                r(&self.logout().await?)
            }
            method::DAEMON_SHUTDOWN => {
                let _: Empty = p(params)?;
                tracing::info!("asked to shut down");
                let shutdown = self.shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(SHUTDOWN_DELAY).await;
                    let _ = shutdown.send(true);
                });
                empty()
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }
}

impl<L: Links> Handler for Control<L> {
    async fn handle(&self, conn: Conn, name: String, params: Value) -> Result<Value, RpcError> {
        let outcome = self.dispatch(conn, &name, params).await;
        if let Err(e) = &outcome {
            tracing::debug!(method = %name, reason = e.reason.as_str(), error = %e.message, "request failed");
        }
        outcome
    }

    fn closed(&self, conn: u64) {
        self.courier.connection_closed(conn);
    }
}
