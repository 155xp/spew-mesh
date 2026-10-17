//! Links between devices over iroh: endpoint, admission and calls (5).
//!
//! This file is the fixed boundary of section 3. The daemon and the courier see only [`Links`] and
//! [`Inbound`]; the iroh endpoint, dialing, admission, `hello`, frames, stream limits and per-peer rate limits
//! live behind them in this module.

use std::future::Future;

use iroh::EndpointId;
use serde::Serialize;

use crate::roster::Admission;
use crate::wire::{Incoming, Request, Response};

/// A link coming up or going down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// A link to this device finished `hello` and can carry calls.
    Up(EndpointId),
    /// The link closed; `last_seen_ms` is when the device last answered.
    Down {
        /// The device.
        device: EndpointId,
        /// Unix ms of the last response from it.
        last_seen_ms: u64,
    },
}

/// Why an outgoing call failed.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The roster doesn't admit the device (4.2).
    #[error("that device isn't admitted")]
    NotAdmitted,
    /// No link could be opened, or it closed mid-call.
    #[error("can't reach that device: {0}")]
    Unreachable(String),
    /// No response in time.
    #[error("the call timed out")]
    Timeout,
    /// The response broke the protocol (bad frame, wrong type).
    #[error("bad response: {0}")]
    Protocol(String),
}

/// The admitted peer behind an incoming call. Person and device come from the link and the roster, never from
/// the payload (5.5 step 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The device's endpoint id.
    pub device: EndpointId,
    /// Its person's username.
    pub person: String,
    /// The device name from the roster.
    pub device_name: String,
    /// Whether it is one of my own devices.
    pub own: bool,
}

/// Network state for `daemon.status` and `doctor` (5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LinkStatus {
    /// `off`, `local`, `n0` or `own`.
    pub network: String,
    /// This device's endpoint id, when an endpoint is up.
    pub endpoint_id: Option<String>,
    /// The home relay, when connected to one.
    pub relay: Option<String>,
    /// One entry per open link.
    pub links: Vec<LinkInfo>,
    /// `Endpoint::bound_sockets()` (5.1).
    pub bound_sockets: Vec<String>,
    /// Home relay status from `Endpoint::home_relay_status()`, for `doctor` (5.1).
    pub relays: Vec<RelayInfo>,
    /// Why the endpoint isn't up although another device is admitted (a bind failure; it is retried).
    pub error: Option<String>,
    /// The last thing that kept each admitted device from linking, cleared when a link comes up: `doctor`
    /// shows these (5.2, 5.4).
    pub problems: Vec<LinkProblem>,
}

/// One open link.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LinkInfo {
    /// The peer's endpoint id.
    pub device: String,
    /// `direct` or `relay`.
    pub path: String,
    /// Round-trip time of the selected path, in ms.
    pub rtt_ms: Option<u64>,
    /// The device name from the roster.
    pub name: String,
    /// The device's person.
    pub person: String,
    /// Unix ms when the link came up.
    pub since_ms: u64,
    /// The protocol version `hello` settled on.
    pub version: u32,
    /// Whether this device dialed the link.
    pub dialed: bool,
}

/// What the link layer offers the rest of the daemon.
pub trait Links: Send + Sync + 'static {
    /// Makes one call on `device` and returns its response, dialing first when no link is up. `hello` is the
    /// link's own business; callers never send it.
    fn call(
        &self,
        device: EndpointId,
        request: Request,
    ) -> impl Future<Output = Result<Response, CallError>> + Send;

    /// Whether the current roster admits `device`.
    fn is_admitted(&self, device: &EndpointId) -> bool;

    /// Subscribes to link ups and downs.
    fn events(&self) -> tokio::sync::broadcast::Receiver<LinkEvent>;

    /// Replaces the admitted set after a roster change (4.3): links to devices no longer admitted close with
    /// code 1, new devices may be dialed, and hints update. With nobody else admitted the endpoint goes down
    /// (5.1).
    fn set_admission(&self, admission: Admission);

    /// Current network state.
    fn status(&self) -> LinkStatus;
}

/// What the link layer needs from the daemon: answers to admitted calls that arrive. `hello` never reaches it,
/// and rate limits have already been applied.
pub trait Inbound: Send + Sync + 'static {
    /// Handles one call from `peer` and returns the response frame.
    fn handle(&self, peer: Peer, call: Incoming) -> impl Future<Output = Response> + Send;
}

// ---- Implementation (below the boundary) ----

mod config;
mod endpoint;
mod limits;
mod links;
mod stream;
#[cfg(test)]
mod tests;

pub use config::{LinkConfig, MAX_INBOUND_STREAMS, MDNS_SERVICE, Timing};
pub use limits::{DELIVER_PER_MINUTE, DELIVER_WINDOW, PRESENCE_GAP};
pub use links::{IrohLinks, LinkError};

/// One home relay's state (5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RelayInfo {
    /// The relay URL.
    pub url: String,
    /// Whether the endpoint is connected to it.
    pub connected: bool,
    /// The last connection error, if any.
    pub error: Option<String>,
    /// Why the relay refused our token, if it did (`own` mode with `relay_token`).
    pub auth_denied: Option<String>,
}

/// What last kept a device from linking (5.2, 5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkProblem {
    /// The device's endpoint id.
    pub device: String,
    /// `unreachable`, `refused` (its roster doesn't admit this device), `version` (no common protocol version)
    /// or `protocol` (a broken `hello`).
    pub problem: String,
    /// One line for people.
    pub detail: String,
    /// Unix ms when it happened.
    pub at_ms: u64,
}
