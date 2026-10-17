//! An in-memory network for tests: several daemons in one process, linked without iroh.
//!
//! A link between two devices is up while both are on the net, each admits the other and the pair isn't cut,
//! the way the real link layer keeps a link up while the other device is admitted (5.3). Calls pass through the
//! wire encoding both ways, so what one courier sends is what another parses.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use iroh::{EndpointId, RelayUrl};
use tokio::sync::{broadcast, watch};

use super::links::{DaemonLinks, InboundHandle};
use crate::link::{CallError, Inbound, LinkEvent, LinkInfo, LinkStatus, Links, Peer};
use crate::roster::Admission;
use crate::wire::{Request, Response, parse_incoming};

struct Node {
    inbound: InboundHandle,
    admission: Option<Admission>,
    events: broadcast::Sender<LinkEvent>,
    refused: broadcast::Sender<EndpointId>,
    home_relay: watch::Sender<Option<RelayUrl>>,
    up: HashSet<EndpointId>,
}

#[derive(Default)]
struct NetState {
    nodes: HashMap<EndpointId, Node>,
    cut: HashSet<(EndpointId, EndpointId)>,
    clock: u64,
}

fn pair(a: EndpointId, b: EndpointId) -> (EndpointId, EndpointId) {
    if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    }
}

impl NetState {
    fn admits(&self, from: &EndpointId, to: &EndpointId) -> bool {
        self.nodes
            .get(from)
            .and_then(|n| n.admission.as_ref())
            .is_some_and(|a| a.is_admitted(to))
    }

    fn should_be_up(&self, a: &EndpointId, b: &EndpointId) -> bool {
        a != b
            && self.nodes.contains_key(a)
            && self.nodes.contains_key(b)
            && !self.cut.contains(&pair(*a, *b))
            && self.admits(a, b)
            && self.admits(b, a)
    }

    // Brings every link to the state the rules say, sending ups and downs.
    fn settle(&mut self) {
        let ids: Vec<EndpointId> = self.nodes.keys().copied().collect();
        let mut changes = Vec::new();
        for a in &ids {
            let up_now: Vec<EndpointId> = self.nodes[a].up.iter().copied().collect();
            for b in up_now {
                if !self.should_be_up(a, &b) {
                    changes.push((*a, b, false));
                }
            }
            for b in &ids {
                if !self.nodes[a].up.contains(b) && self.should_be_up(a, b) {
                    changes.push((*a, *b, true));
                }
            }
        }
        self.clock += 1;
        let at = self.clock;
        for (a, b, up) in changes {
            let Some(node) = self.nodes.get_mut(&a) else {
                continue;
            };
            let event = if up {
                node.up.insert(b);
                LinkEvent::Up(b)
            } else {
                node.up.remove(&b);
                LinkEvent::Down {
                    device: b,
                    last_seen_ms: at,
                }
            };
            let _ = node.events.send(event);
        }
    }
}

/// The shared in-memory network.
#[derive(Clone, Default)]
pub struct MemNet {
    state: Arc<Mutex<NetState>>,
}

impl MemNet {
    /// An empty network.
    pub fn new() -> MemNet {
        MemNet::default()
    }

    fn lock(&self) -> MutexGuard<'_, NetState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds a device and returns its links and the inbound handle to bind to its courier.
    pub fn join(&self, device: EndpointId) -> (MemLinks, InboundHandle) {
        let inbound = InboundHandle::new();
        let mut s = self.lock();
        s.nodes.insert(
            device,
            Node {
                inbound: inbound.clone(),
                admission: None,
                events: broadcast::channel(64).0,
                refused: broadcast::channel(64).0,
                home_relay: watch::channel(None).0,
                up: HashSet::new(),
            },
        );
        s.settle();
        (
            MemLinks {
                net: self.clone(),
                me: device,
            },
            inbound,
        )
    }

    /// Takes a device off the network, as when its daemon stops.
    pub fn leave(&self, device: &EndpointId) {
        let mut s = self.lock();
        s.nodes.remove(device);
        s.settle();
    }

    /// Cuts the link between two devices, as when one of them loses the network.
    pub fn cut(&self, a: EndpointId, b: EndpointId) {
        let mut s = self.lock();
        s.cut.insert(pair(a, b));
        s.settle();
    }

    /// Restores a link cut with [`MemNet::cut`].
    pub fn heal(&self, a: EndpointId, b: EndpointId) {
        let mut s = self.lock();
        s.cut.remove(&pair(a, b));
        s.settle();
    }

    /// Whether the link between two devices is up.
    pub fn is_up(&self, a: &EndpointId, b: &EndpointId) -> bool {
        self.lock().nodes.get(a).is_some_and(|n| n.up.contains(b))
    }

    /// Sets a device's home relay, as when its endpoint connects to one.
    pub fn set_home_relay(&self, device: &EndpointId, relay: Option<RelayUrl>) {
        if let Some(node) = self.lock().nodes.get(device) {
            node.home_relay.send_replace(relay);
        }
    }
}

/// One device's links on a [`MemNet`].
#[derive(Clone)]
pub struct MemLinks {
    net: MemNet,
    me: EndpointId,
}

impl MemLinks {
    /// This device.
    pub fn device(&self) -> EndpointId {
        self.me
    }
}

impl Links for MemLinks {
    async fn call(&self, device: EndpointId, request: Request) -> Result<Response, CallError> {
        let (inbound, peer) = {
            let s = self.net.lock();
            if !s.admits(&self.me, &device) {
                return Err(CallError::NotAdmitted);
            }
            // A dial the other device's admission closes: it hears of an unknown device, as iroh's hook reports.
            if let Some(callee) = s.nodes.get(&device)
                && !s.admits(&device, &self.me)
            {
                let _ = callee.refused.send(self.me);
            }
            let up = s
                .nodes
                .get(&self.me)
                .is_some_and(|n| n.up.contains(&device));
            let Some(callee) = s.nodes.get(&device).filter(|_| up) else {
                return Err(CallError::Unreachable("no link to that device".into()));
            };
            let Some(me) = callee.admission.as_ref().and_then(|a| a.get(&self.me)) else {
                return Err(CallError::Unreachable("that device closed the link".into()));
            };
            let peer = Peer {
                device: self.me,
                person: me.person.clone(),
                device_name: me.name.clone(),
                own: me.own,
            };
            (callee.inbound.clone(), peer)
        };
        let payload =
            serde_json::to_vec(&request).map_err(|e| CallError::Protocol(e.to_string()))?;
        let incoming = match parse_incoming(&payload) {
            Ok(incoming) => incoming,
            Err(e) => return Ok(e.response()),
        };
        let response = inbound.handle(peer, incoming).await;
        let bytes =
            serde_json::to_vec(&response).map_err(|e| CallError::Protocol(e.to_string()))?;
        Response::parse(&bytes).map_err(|e| CallError::Protocol(e.to_string()))
    }

    fn is_admitted(&self, device: &EndpointId) -> bool {
        self.net.lock().admits(&self.me, device)
    }

    fn events(&self) -> broadcast::Receiver<LinkEvent> {
        let s = self.net.lock();
        match s.nodes.get(&self.me) {
            Some(n) => n.events.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    fn set_admission(&self, admission: Admission) {
        let mut s = self.net.lock();
        if let Some(node) = s.nodes.get_mut(&self.me) {
            node.admission = Some(admission);
        }
        s.settle();
    }

    fn status(&self) -> LinkStatus {
        let s = self.net.lock();
        let Some(node) = s.nodes.get(&self.me) else {
            return LinkStatus {
                network: "off".into(),
                ..LinkStatus::default()
            };
        };
        let admits_others = node
            .admission
            .as_ref()
            .is_some_and(Admission::admits_others);
        let mut links: Vec<LinkInfo> = node
            .up
            .iter()
            .map(|d| LinkInfo {
                device: d.to_string(),
                path: "direct".into(),
                rtt_ms: Some(1),
                ..LinkInfo::default()
            })
            .collect();
        links.sort_by(|a, b| a.device.cmp(&b.device));
        LinkStatus {
            network: if admits_others { "local" } else { "off" }.into(),
            endpoint_id: admits_others.then(|| self.me.to_string()),
            relay: node.home_relay.borrow().as_ref().map(ToString::to_string),
            links,
            ..LinkStatus::default()
        }
    }
}

impl DaemonLinks for MemLinks {
    fn refused(&self) -> broadcast::Receiver<EndpointId> {
        let s = self.net.lock();
        match s.nodes.get(&self.me) {
            Some(n) => n.refused.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    fn home_relay(&self) -> watch::Receiver<Option<RelayUrl>> {
        let s = self.net.lock();
        match s.nodes.get(&self.me) {
            Some(n) => n.home_relay.subscribe(),
            None => watch::channel(None).1,
        }
    }

    async fn shutdown(&self) {}
}
