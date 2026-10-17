//! The daemon's side of the link boundary: [`DaemonLinks`], what the daemon needs from a link layer beyond
//! [`Links`]; the iroh links it runs in production; [`OffLinks`], which reach nobody; and [`InboundHandle`],
//! which lets the link layer reach a courier built after it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::Context as _;
use iroh::{EndpointId, RelayUrl};
use tokio::sync::{broadcast, watch};

use crate::courier::Courier;
use crate::link::{
    CallError, Inbound, IrohLinks, LinkConfig, LinkEvent, LinkStatus, Links, Peer, Timing,
};
use crate::roster::Admission;
use crate::wire::{ErrorCode, Incoming, Request, Response};

/// What the daemon needs from its link layer besides [`Links`]: unknown devices that tried to connect, the home
/// relay, and a clean shutdown. The courier sees only [`Links`].
pub trait DaemonLinks: Links {
    /// Endpoint ids that tried to connect without being admitted, so the roster can refresh early (4.2). The link
    /// layer reports at most one a second.
    fn refused(&self) -> broadcast::Receiver<EndpointId>;

    /// The home relay URL, `None` while there is none, for reporting to spew (5.1, 14.3).
    fn home_relay(&self) -> watch::Receiver<Option<RelayUrl>>;

    /// Closes every link with code 4 (`shutdown`) and the endpoint, and waits for it (5.2).
    fn shutdown(&self) -> impl Future<Output = ()> + Send;
}

impl DaemonLinks for IrohLinks {
    fn refused(&self) -> broadcast::Receiver<EndpointId> {
        IrohLinks::refused(self)
    }

    fn home_relay(&self) -> watch::Receiver<Option<RelayUrl>> {
        IrohLinks::home_relay(self)
    }

    fn shutdown(&self) -> impl Future<Output = ()> + Send {
        IrohLinks::shutdown(self)
    }
}

/// What a link layer is built from: this device and its link settings.
#[derive(Debug, Clone)]
pub struct LinkSetup {
    /// This device's endpoint id.
    pub device: EndpointId,
    /// The device key and name, `[network]` with the environment applied, and the spec's timings.
    pub config: LinkConfig,
}

impl LinkSetup {
    /// The setup for this device's key and name and `[network]` settings.
    pub fn new(
        secret_key: iroh::SecretKey,
        device_name: &str,
        network: crate::settings::NetworkSettings,
    ) -> LinkSetup {
        LinkSetup {
            device: secret_key.public(),
            config: LinkConfig {
                secret_key,
                device_name: device_name.to_string(),
                network,
                timing: Timing::default(),
            },
        }
    }
}

/// The production links: iroh, admitting nobody until the courier puts the roster in force, so no endpoint
/// exists until the roster names another device (5.1).
pub async fn iroh_links(setup: LinkSetup) -> anyhow::Result<(IrohLinks, InboundHandle)> {
    let inbound = InboundHandle::new();
    let links = IrohLinks::start(
        setup.config,
        Admission::default(),
        Arc::new(inbound.clone()),
    )
    .await
    .context("can't start the network")?;
    Ok((links, inbound))
}

/// Links that admit nobody and report the network `off` (5.1): the daemon runs on its own.
pub struct OffLinks {
    events: broadcast::Sender<LinkEvent>,
    refused: broadcast::Sender<EndpointId>,
    home_relay: watch::Sender<Option<RelayUrl>>,
    admission: Mutex<Option<Admission>>,
}

impl Default for OffLinks {
    fn default() -> Self {
        OffLinks {
            events: broadcast::channel(16).0,
            refused: broadcast::channel(1).0,
            home_relay: watch::channel(None).0,
            admission: Mutex::new(None),
        }
    }
}

impl OffLinks {
    /// How many devices the roster in force names besides this one; none of them are reached.
    pub fn named_devices(&self) -> usize {
        self.admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, Admission::len)
    }
}

impl Links for OffLinks {
    async fn call(&self, _device: EndpointId, _request: Request) -> Result<Response, CallError> {
        Err(CallError::NotAdmitted)
    }

    fn is_admitted(&self, _device: &EndpointId) -> bool {
        false
    }

    fn events(&self) -> broadcast::Receiver<LinkEvent> {
        self.events.subscribe()
    }

    fn set_admission(&self, admission: Admission) {
        *self.admission.lock().unwrap_or_else(|e| e.into_inner()) = Some(admission);
    }

    fn status(&self) -> LinkStatus {
        LinkStatus {
            network: "off".to_string(),
            ..LinkStatus::default()
        }
    }
}

impl DaemonLinks for OffLinks {
    fn refused(&self) -> broadcast::Receiver<EndpointId> {
        self.refused.subscribe()
    }

    fn home_relay(&self) -> watch::Receiver<Option<RelayUrl>> {
        self.home_relay.subscribe()
    }

    async fn shutdown(&self) {}
}

/// A boxed inbound handler, so the link layer's type needn't name the courier's.
pub trait InboundDyn: Send + Sync + 'static {
    /// [`Inbound::handle`], boxed.
    fn handle_dyn(
        &self,
        peer: Peer,
        call: Incoming,
    ) -> Pin<Box<dyn Future<Output = Response> + Send + '_>>;
}

impl<L: Links> InboundDyn for Courier<L> {
    fn handle_dyn(
        &self,
        peer: Peer,
        call: Incoming,
    ) -> Pin<Box<dyn Future<Output = Response> + Send + '_>> {
        Box::pin(self.handle_inbound(peer, call))
    }
}

/// The [`Inbound`] given to the link layer before the courier exists; [`InboundHandle::bind`] points it at the
/// courier once built. Calls before that, or after the courier is gone, get `unknown-call`.
#[derive(Clone, Default)]
pub struct InboundHandle {
    target: Arc<OnceLock<Weak<dyn InboundDyn>>>,
}

impl InboundHandle {
    /// A handle bound to nothing yet.
    pub fn new() -> InboundHandle {
        InboundHandle::default()
    }

    /// Points the handle at its handler; only the first bind counts.
    pub fn bind(&self, target: Weak<dyn InboundDyn>) {
        let _ = self.target.set(target);
    }
}

impl Inbound for InboundHandle {
    async fn handle(&self, peer: Peer, call: Incoming) -> Response {
        match self.target.get().and_then(Weak::upgrade) {
            Some(target) => target.handle_dyn(peer, call).await,
            None => Response::Error {
                code: ErrorCode::UnknownCall,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn off_links_reach_nobody() {
        let links = OffLinks::default();
        let id = iroh::SecretKey::from_bytes(&[7u8; 32]).public();
        assert!(matches!(
            links.call(id, Request::Ping).await,
            Err(CallError::NotAdmitted)
        ));
        assert!(!links.is_admitted(&id));
        assert_eq!(links.status().network, "off");
        assert_eq!(links.named_devices(), 0);
    }

    #[tokio::test]
    async fn an_unbound_handle_answers_unknown_call() {
        let handle = InboundHandle::new();
        let peer = Peer {
            device: iroh::SecretKey::from_bytes(&[8u8; 32]).public(),
            person: "maya".into(),
            device_name: "maya-mbp".into(),
            own: false,
        };
        assert_eq!(
            handle.handle(peer, Incoming::Ping).await,
            Response::Error {
                code: ErrorCode::UnknownCall
            }
        );
    }
}
