//! The iroh endpoint (5.1) and the admission hook (5.2).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{
    AfterHandshakeOutcome, BeforeConnectOutcome, BindOpts, Connection, EndpointHooks,
    PortmapperConfig, QuicTransportConfig, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMap, RelayMode, RelayUrl};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tokio::sync::broadcast;

use super::config::{
    LinkConfig, MAX_INBOUND_STREAMS, MDNS_SERVICE, RECEIVE_WINDOW, STREAM_RECEIVE_WINDOW,
};
use crate::roster::{Admission, AdmittedDevice};
use crate::settings::NetworkMode;
use crate::wire::{ALPN, CloseCode};

/// How often at most an unknown endpoint id trying to connect is reported to the daemon.
const REFUSED_REPORT_GAP: Duration = Duration::from_secs(1);

/// The QUIC close code for one of our application close codes.
pub(crate) fn close_code(code: CloseCode) -> VarInt {
    VarInt::from_u32(code.code())
}

/// Closes a connection with one of our application close codes (5.2).
pub(crate) fn close(conn: &Connection, code: CloseCode) {
    conn.close(close_code(code), code.reason().as_bytes());
}

/// The admitted set, shared by the endpoint hook and the link layer. Writers hold the link state lock, so a
/// connection being registered and an admission change never interleave.
#[derive(Debug)]
pub(crate) struct Gate {
    admission: RwLock<Arc<Admission>>,
    refused: broadcast::Sender<EndpointId>,
    last_refused: Mutex<Option<Instant>>,
}

impl Gate {
    pub(crate) fn new(admission: Admission, refused: broadcast::Sender<EndpointId>) -> Self {
        Self {
            admission: RwLock::new(Arc::new(admission)),
            refused,
            last_refused: Mutex::new(None),
        }
    }

    /// The admitted set in force.
    pub(crate) fn admission(&self) -> Arc<Admission> {
        self.admission
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the admitted set and returns the old one.
    pub(crate) fn replace(&self, admission: Admission) -> Arc<Admission> {
        let mut guard = self
            .admission
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *guard, Arc::new(admission))
    }

    pub(crate) fn is_admitted(&self, id: &EndpointId) -> bool {
        self.admission().is_admitted(id)
    }

    // Tells the daemon an unknown device tried to connect, so a spew roster can refresh early (4.2). Throttled
    // because anyone who knows the endpoint id can trigger it.
    fn report_refused(&self, id: EndpointId) {
        let now = Instant::now();
        let mut last = self
            .last_refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|t| now.saturating_duration_since(t) < REFUSED_REPORT_GAP) {
            return;
        }
        *last = Some(now);
        // No receivers is fine.
        let _ = self.refused.send(id);
    }
}

/// The endpoint hook that closes every connection, inbound or outbound, whose remote isn't admitted (5.2). It
/// holds only the gate, never the endpoint, so it doesn't keep the endpoint alive.
#[derive(Debug)]
pub(crate) struct AdmissionHook(pub(crate) Arc<Gate>);

impl EndpointHooks for AdmissionHook {
    async fn before_connect<'a>(
        &'a self,
        remote_addr: &'a EndpointAddr,
        _alpn: &'a [u8],
    ) -> BeforeConnectOutcome {
        if self.0.is_admitted(&remote_addr.id) {
            BeforeConnectOutcome::Accept
        } else {
            BeforeConnectOutcome::Reject
        }
    }

    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let remote = conn.remote_id();
        if conn.alpn() == ALPN && self.0.is_admitted(&remote) {
            return AfterHandshakeOutcome::Accept;
        }
        if conn.side().is_server() {
            self.0.report_refused(remote);
        }
        AfterHandshakeOutcome::Reject {
            error_code: close_code(CloseCode::NotAdmitted),
            reason: CloseCode::NotAdmitted.reason().as_bytes().to_vec(),
        }
    }
}

/// The QUIC settings every link uses: keep-alive and idle timeout (5.3), 16 inbound bidirectional streams and no
/// unidirectional ones, and receive windows sized to the frame limit rather than QUIC's bulk-transfer defaults
/// (5.4).
fn transport_config(config: &LinkConfig) -> Result<QuicTransportConfig, String> {
    let idle = config
        .timing
        .idle_timeout
        .try_into()
        .map_err(|_| "the idle timeout is too long".to_string())?;
    Ok(QuicTransportConfig::builder()
        .keep_alive_interval(config.timing.keep_alive)
        .max_idle_timeout(Some(idle))
        .max_concurrent_bidi_streams(VarInt::from_u32(MAX_INBOUND_STREAMS))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .stream_receive_window(VarInt::from_u32(STREAM_RECEIVE_WINDOW))
        .receive_window(VarInt::from_u32(RECEIVE_WINDOW))
        .build())
}

/// Binds the endpoint for `[network]` (5.1).
pub(crate) async fn bind(
    config: &LinkConfig,
    gate: Arc<Gate>,
    hints: MemoryLookup,
) -> Result<Endpoint, String> {
    let network = &config.network;
    let mut builder = match network.mode {
        NetworkMode::N0 => Endpoint::builder(presets::N0),
        NetworkMode::Own => {
            let relays: Vec<RelayConfig> = network
                .relays
                .iter()
                .map(|url| RelayConfig::from(RelayUrl::from(url.clone())))
                .collect();
            if relays.is_empty() {
                // Settings validation refuses this; stay closed if it gets here anyway.
                return Err("[network] mode = \"own\" has no relays".into());
            }
            let mut map: RelayMap = relays.into_iter().collect();
            if let Some(token) = &network.relay_token {
                map = map.with_auth_token(token.expose());
            }
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Custom(map))
        }
        // LAN-only and air-gapped: no relays, and no UPnP or NAT-PMP probing of the gateway either.
        NetworkMode::Local => Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .portmapper_config(PortmapperConfig::Disabled),
    };
    builder = builder
        .secret_key(config.secret_key.clone())
        .alpns(vec![ALPN.to_vec()])
        .transport_config(transport_config(config)?)
        .hooks(AdmissionHook(gate))
        .address_lookup(hints);
    if network.mdns {
        builder = builder.address_lookup(MdnsAddressLookup::builder().service_name(MDNS_SERVICE));
    }
    // The bind rule of 5.1: the default sockets unless bind or port is set; then exactly `<bind>:<port>`, plus
    // `[::]:<port>` (allowed to fail) when bind is 0.0.0.0.
    let unspecified_v4 = network.bind == IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    if !unspecified_v4 || network.port != 0 {
        builder = builder
            .clear_ip_transports()
            .bind_addr(SocketAddr::new(network.bind, network.port))
            .map_err(|e| format!("can't bind {}:{}: {e}", network.bind, network.port))?;
        if unspecified_v4 {
            builder = builder
                .bind_addr_with_opts(
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), network.port),
                    BindOpts::default().set_is_required(false),
                )
                .map_err(|e| format!("can't bind [::]:{}: {e}", network.port))?;
        }
    }
    builder
        .bind()
        .await
        .map_err(|e| format!("can't start the network endpoint: {e}"))
}

/// The dialing hints for one admitted device (5.1), filtered for the mode: no relay hints in `local` (no
/// relays at all) and only our own relays in `own` (never n0's or anyone else's).
pub(crate) fn hint_addr(
    device: &AdmittedDevice,
    mode: NetworkMode,
    own_relays: &[url::Url],
) -> Option<EndpointAddr> {
    let relay = device.relay.as_ref().filter(|relay| match mode {
        NetworkMode::N0 => true,
        NetworkMode::Local => false,
        NetworkMode::Own => own_relays.iter().any(|own| own == &***relay),
    });
    let mut addr = EndpointAddr::new(device.endpoint_id);
    if let Some(relay) = relay {
        addr = addr.with_relay_url(relay.clone());
    }
    for ip in &device.addrs {
        addr = addr.with_ip_addr(*ip);
    }
    (!addr.is_empty()).then_some(addr)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn device(relay: Option<&str>, addrs: &[&str]) -> AdmittedDevice {
        AdmittedDevice {
            endpoint_id: iroh::SecretKey::from_bytes(&[7; 32]).public(),
            person: "maya".into(),
            display: "Maya".into(),
            name: "maya-mbp".into(),
            own: false,
            rooms: BTreeSet::new(),
            relay: relay.map(|r| r.parse().unwrap()),
            addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn receive_windows_track_the_frame_limit() {
        // One stream holds a whole frame and its 4-byte length prefix; all 16 streams fit the link's window.
        assert!(STREAM_RECEIVE_WINDOW as usize >= crate::wire::MAX_FRAME + 4);
        assert!(STREAM_RECEIVE_WINDOW as usize <= 2 * crate::wire::MAX_FRAME);
        assert_eq!(RECEIVE_WINDOW, MAX_INBOUND_STREAMS * STREAM_RECEIVE_WINDOW);
        let config = LinkConfig {
            secret_key: iroh::SecretKey::from_bytes(&[7; 32]),
            device_name: "sam-mac".into(),
            network: crate::settings::NetworkSettings {
                mode: NetworkMode::Local,
                bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                mdns: false,
                relays: Vec::new(),
                relay_token: None,
            },
            timing: crate::link::Timing::default(),
        };
        let shown = format!("{:?}", transport_config(&config).unwrap());
        for field in [
            "stream_receive_window: 262144",
            " receive_window: 4194304",
            "max_concurrent_bidi_streams: 16",
            "max_concurrent_uni_streams: 0",
        ] {
            assert!(shown.contains(field), "{field} in {shown}");
        }
    }

    #[test]
    fn hints_follow_the_mode() {
        let d = device(Some("https://relay.example.com"), &["192.168.1.20:7461"]);
        let n0 = hint_addr(&d, NetworkMode::N0, &[]).unwrap();
        assert_eq!(n0.relay_urls().count(), 1);
        assert_eq!(n0.ip_addrs().count(), 1);

        let local = hint_addr(&d, NetworkMode::Local, &[]).unwrap();
        assert_eq!(
            local.relay_urls().count(),
            0,
            "local mode never uses relays"
        );
        assert_eq!(local.ip_addrs().count(), 1);

        let own: Vec<url::Url> = vec!["https://relay.example.com".parse().unwrap()];
        let other: Vec<url::Url> = vec!["https://other.example.com".parse().unwrap()];
        assert_eq!(
            hint_addr(&d, NetworkMode::Own, &own)
                .unwrap()
                .relay_urls()
                .count(),
            1
        );
        assert_eq!(
            hint_addr(&d, NetworkMode::Own, &other)
                .unwrap()
                .relay_urls()
                .count(),
            0,
            "own mode uses only the configured relays"
        );

        assert!(hint_addr(&device(None, &[]), NetworkMode::N0, &[]).is_none());
        assert!(
            hint_addr(
                &device(Some("https://relay.example.com"), &[]),
                NetworkMode::Local,
                &[]
            )
            .is_none()
        );
    }
}
