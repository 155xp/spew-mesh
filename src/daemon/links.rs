//! The daemon's side of the link boundary: [`OffLinks`], used until the iroh links exist, and
//! [`InboundHandle`], which lets the link layer reach a courier built after it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use iroh::EndpointId;
use tokio::sync::broadcast;

use crate::courier::Courier;
use crate::link::{CallError, Inbound, LinkEvent, LinkStatus, Links, Peer};
use crate::roster::Admission;
use crate::wire::{ErrorCode, Incoming, Request, Response};

/// Links that admit nobody and report the network `off` (5.1): the daemon runs on its own.
pub struct OffLinks {
    events: broadcast::Sender<LinkEvent>,
    admission: Mutex<Option<Admission>>,
}

impl Default for OffLinks {
    fn default() -> Self {
        OffLinks {
            events: broadcast::channel(16).0,
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
