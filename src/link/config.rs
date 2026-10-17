//! What [`super::IrohLinks`] needs to start: the device key and name, `[network]`, and the protocol timings.

use std::fmt;
use std::time::Duration;

use iroh::SecretKey;

use crate::settings::NetworkSettings;

/// mDNS service name (5.1); never iroh's default, which would show every iroh app on the LAN.
pub const MDNS_SERVICE: &str = "spewmesh";
/// Most inbound bidirectional streams open at once on one link (5.4).
pub const MAX_INBOUND_STREAMS: u32 = 16;

/// Everything the link layer takes from the daemon at start.
#[derive(Clone)]
pub struct LinkConfig {
    /// The device secret key; its public key is the endpoint id (4.1).
    pub secret_key: SecretKey,
    /// This device's name, sent in `hello`.
    pub device_name: String,
    /// `[network]` with environment overrides applied (`SPEW_MESH_LAN=0` turns `mdns` off).
    pub network: NetworkSettings,
    /// Protocol timings; [`Timing::default`] is the spec's.
    pub timing: Timing,
}

impl fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkConfig")
            .field(
                "endpoint_id",
                &self.secret_key.public().fmt_short().to_string(),
            )
            .field("device_name", &self.device_name)
            .field("network", &self.network)
            .field("timing", &self.timing)
            .finish()
    }
}

/// Protocol timings (5.3, 5.4). The defaults are the spec's values; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// A request frame must arrive this long after its stream opens (5.4: 10 s).
    pub first_frame: Duration,
    /// A dial attempt, `hello` included, gives up after this (5.3: 15 s). An accepted connection that sends no
    /// `hello` in this time is closed too.
    pub dial_timeout: Duration,
    /// First dial backoff delay (5.3: 3 s).
    pub backoff_base: Duration,
    /// Longest dial backoff delay (5.3: 5 minutes).
    pub backoff_max: Duration,
    /// The device that isn't the designated dialer dials only after no link has come up for this long (5.3: 30 s).
    pub lazy_dial_after: Duration,
    /// How long a call waits for its response once the link is up. The spec sets no value; 20 s is ours.
    pub call_timeout: Duration,
    /// QUIC keep-alive (5.3: 5 s).
    pub keep_alive: Duration,
    /// QUIC idle timeout (5.3: 30 s).
    pub idle_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            first_frame: Duration::from_secs(10),
            dial_timeout: Duration::from_secs(15),
            backoff_base: Duration::from_secs(3),
            backoff_max: Duration::from_secs(300),
            lazy_dial_after: Duration::from_secs(30),
            call_timeout: Duration::from_secs(20),
            keep_alive: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(30),
        }
    }
}
