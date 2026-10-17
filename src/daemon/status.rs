//! The network part of `daemon.status` (5.1, 5.3, 10): the mode, endpoint id, home relay and its status, bound
//! sockets, each admitted device's link and what keeps devices apart. It is a superset of the protocol's
//! `NetworkStatus`, so a client that reads only that still parses it.

use serde::{Deserialize, Serialize};

use crate::control::protocol::{DeviceRow, LinkRow};
use crate::link::{LinkProblem, LinkStatus, RelayInfo};

/// The `network` object of `daemon.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkDetails {
    /// `off`, `local`, `n0` or `own`.
    pub network: String,
    /// This device's endpoint id, while an endpoint is up.
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// The home relay, while connected to one.
    #[serde(default)]
    pub relay: Option<String>,
    /// One entry per open link.
    #[serde(default)]
    pub links: Vec<LinkRow>,
    /// Home relay status from `Endpoint::home_relay_status()` (5.1).
    #[serde(default)]
    pub relays: Vec<RelayRow>,
    /// `Endpoint::bound_sockets()` (5.1).
    #[serde(default)]
    pub bound_sockets: Vec<String>,
    /// Every admitted device: online or not, path and round trip (5.3).
    #[serde(default)]
    pub devices: Vec<DeviceLink>,
    /// Why the endpoint isn't up although another device is admitted.
    #[serde(default)]
    pub error: Option<String>,
    /// The last thing that kept each device from linking (5.2, 5.4).
    #[serde(default)]
    pub problems: Vec<ProblemRow>,
}

/// One home relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayRow {
    /// The relay URL.
    pub url: String,
    /// Whether the endpoint is connected to it.
    pub connected: bool,
    /// The last connection error.
    #[serde(default)]
    pub error: Option<String>,
    /// Why the relay refused our token (`own` mode with `relay_token`).
    #[serde(default)]
    pub auth_denied: Option<String>,
}

/// One admitted device and its link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLink {
    /// Endpoint id.
    pub endpoint_id: String,
    /// The device name from the roster.
    pub name: String,
    /// Its person.
    pub person: String,
    /// Whether a link is up.
    pub online: bool,
    /// When it was last seen, ms, while offline.
    #[serde(default)]
    pub last_seen: Option<u64>,
    /// `direct` or `relay`, while online.
    #[serde(default)]
    pub path: Option<String>,
    /// Round-trip time in ms, while online.
    #[serde(default)]
    pub rtt_ms: Option<u64>,
}

/// What last kept a device from linking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemRow {
    /// The device's endpoint id.
    pub device: String,
    /// `unreachable`, `refused`, `version` or `protocol`.
    pub problem: String,
    /// One line for people.
    pub detail: String,
    /// Unix ms when it happened.
    pub at_ms: u64,
}

impl ProblemRow {
    /// Whether a person has something to fix: a peer that refuses us or speaks another version. An unreachable
    /// device is most often just switched off.
    pub fn needs_fixing(&self) -> bool {
        self.problem != "unreachable"
    }
}

impl From<RelayInfo> for RelayRow {
    fn from(r: RelayInfo) -> RelayRow {
        RelayRow {
            url: r.url,
            connected: r.connected,
            error: r.error,
            auth_denied: r.auth_denied,
        }
    }
}

impl From<LinkProblem> for ProblemRow {
    fn from(p: LinkProblem) -> ProblemRow {
        ProblemRow {
            device: p.device,
            problem: p.problem,
            detail: p.detail,
            at_ms: p.at_ms,
        }
    }
}

impl NetworkDetails {
    /// The details from the link layer's status and the directory's device rows (this device is left out).
    pub fn new(status: LinkStatus, devices: &[DeviceRow]) -> NetworkDetails {
        NetworkDetails {
            network: status.network,
            endpoint_id: status.endpoint_id,
            relay: status.relay,
            links: status.links.into_iter().map(LinkRow::from).collect(),
            relays: status.relays.into_iter().map(RelayRow::from).collect(),
            bound_sockets: status.bound_sockets,
            devices: devices
                .iter()
                .filter(|d| !d.this)
                .map(|d| DeviceLink {
                    endpoint_id: d.endpoint_id.clone(),
                    name: d.name.clone(),
                    person: d.person.clone(),
                    online: d.online,
                    last_seen: d.last_seen,
                    path: d.path.clone(),
                    rtt_ms: d.rtt_ms,
                })
                .collect(),
            error: status.error,
            problems: status.problems.into_iter().map(ProblemRow::from).collect(),
        }
    }

    /// The device name for an endpoint id, else its short form.
    pub fn device_label(&self, endpoint_id: &str) -> String {
        self.devices
            .iter()
            .find(|d| d.endpoint_id == endpoint_id)
            .map(|d| format!("{}@{}", d.person, d.name))
            .unwrap_or_else(|| endpoint_id.get(..10).unwrap_or(endpoint_id).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::NetworkStatus;
    use crate::link::LinkInfo;

    fn row(id: &str, this: bool, online: bool) -> DeviceRow {
        DeviceRow {
            endpoint_id: id.into(),
            name: format!("{id}-dev"),
            person: "maya".into(),
            own: false,
            this,
            online,
            last_seen: (!online).then_some(5),
            path: online.then(|| "direct".to_string()),
            rtt_ms: online.then_some(3),
        }
    }

    #[test]
    fn details_carry_everything_and_still_read_as_the_protocols_status() {
        let status = LinkStatus {
            network: "n0".into(),
            endpoint_id: Some("aa".into()),
            relay: Some("https://relay.example.com/".into()),
            links: vec![LinkInfo {
                device: "bb".into(),
                path: "relay".into(),
                rtt_ms: Some(40),
                ..LinkInfo::default()
            }],
            bound_sockets: vec!["0.0.0.0:7461".into()],
            relays: vec![RelayInfo {
                url: "https://relay.example.com/".into(),
                connected: true,
                error: None,
                auth_denied: None,
            }],
            error: None,
            problems: vec![LinkProblem {
                device: "cc".into(),
                problem: "refused".into(),
                detail: "it refused the link".into(),
                at_ms: 9,
            }],
        };
        let details = NetworkDetails::new(
            status,
            &[
                row("aa", true, true),
                row("bb", false, true),
                row("cc", false, false),
            ],
        );
        assert_eq!(details.devices.len(), 2, "this device is left out");
        assert!(details.devices[0].online);
        assert_eq!(details.devices[1].last_seen, Some(5));
        assert!(details.problems[0].needs_fixing());
        assert_eq!(details.device_label("cc"), "maya@cc-dev");
        assert_eq!(details.device_label("dd"), "dd");

        let value = serde_json::to_value(&details).unwrap();
        let plain: NetworkStatus = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(plain.network, "n0");
        assert_eq!(plain.links[0].path, "relay");
        let back: NetworkDetails = serde_json::from_value(value).unwrap();
        assert_eq!(back, details);
        // An older daemon's network object reads as details with nothing extra.
        let old: NetworkDetails =
            serde_json::from_value(serde_json::json!({"network": "off"})).unwrap();
        assert_eq!(old.network, "off");
        assert!(old.devices.is_empty() && old.problems.is_empty());
    }

    #[test]
    fn an_unreachable_device_is_no_fix() {
        let p = ProblemRow {
            device: "bb".into(),
            problem: "unreachable".into(),
            detail: "no link within 15 s".into(),
            at_ms: 1,
        };
        assert!(!p.needs_fixing());
    }
}
