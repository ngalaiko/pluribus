use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    time::Duration,
};

use exn::ResultExt;
use serde::Deserialize;

use crate::protocol::NodeId;

use super::{Event, Network};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Errors returned by `Tailscale` operations.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// Tailscale
// ---------------------------------------------------------------------------

/// Peer discovery backed by `tailscale status --json`.
pub struct Tailscale {
    node_id: NodeId,
    name: String,
    port: u16,
}

impl Tailscale {
    /// Create a new discovery instance.
    ///
    /// Calls `tailscale status --json` once to determine this node's identity.
    pub async fn new(port: u16) -> exn::Result<Self, Error> {
        let status = run_status().await?;
        let self_node = &status.self_node;
        let node_id = node_id(&self_node.id);
        let name = hostname(&self_node.host_name);
        Ok(Self {
            node_id,
            name,
            port,
        })
    }
}

impl Network for Tailscale {
    type Events = Pin<Box<dyn futures_lite::Stream<Item = Event> + Send>>;

    fn id(&self) -> &NodeId {
        &self.node_id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn events(self) -> Self::Events {
        let state = StreamState {
            port: self.port,
            known: HashMap::new(),
            pending: Vec::new(),
            first: true,
        };

        Box::pin(futures_lite::stream::unfold(
            state,
            |mut state| async move {
                loop {
                    // Drain pending events.
                    if let Some(event) = state.pending.pop() {
                        return Some((event, state));
                    }

                    // Wait before polling (skip on first iteration).
                    if state.first {
                        state.first = false;
                    } else {
                        async_io::Timer::after(Duration::from_secs(10)).await;
                    }

                    // Poll tailscale.
                    let status = match run_status().await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(?e, "tailscale discover error");
                            continue;
                        }
                    };

                    // Build current peer set.
                    let mut current: HashMap<NodeId, Peer> = HashMap::new();
                    for peer in status.peer.values() {
                        if !peer.online {
                            continue;
                        }
                        if !peer.tags.iter().any(|t| t == "tag:pluribus") {
                            continue;
                        }
                        let Some(ip) = peer.tailscale_ips.first() else {
                            continue;
                        };
                        let id = node_id(&peer.id);
                        let name = hostname(&peer.host_name);
                        current.insert(id, Peer { name, ip: *ip });
                    }

                    // Diff: new peers → Online events.
                    for (id, peer) in &current {
                        if !state.known.contains_key(id) {
                            state.pending.push(Event::Online {
                                id: id.clone(),
                                name: peer.name.clone(),
                                addr: SocketAddr::new(peer.ip, state.port),
                            });
                        }
                    }

                    // Diff: disappeared peers → Offline events.
                    for id in state.known.keys() {
                        if !current.contains_key(id) {
                            state.pending.push(Event::Offline { id: id.clone() });
                        }
                    }

                    state.known = current;
                }
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// Internal stream state
// ---------------------------------------------------------------------------

struct StreamState {
    port: u16,
    known: HashMap<NodeId, Peer>,
    pending: Vec<Event>,
    first: bool,
}

struct Peer {
    name: String,
    ip: IpAddr,
}

// ---------------------------------------------------------------------------
// Internal: run tailscale, parse JSON
// ---------------------------------------------------------------------------

async fn run_status() -> exn::Result<Status, Error> {
    let output = async_process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
        .or_raise(|| Error("run tailscale status".into()))?;

    if !output.status.success() {
        exn::bail!(Error(format!("tailscale exited with {}", output.status)));
    }

    let status: Status = serde_json::from_slice(&output.stdout)
        .or_raise(|| Error("parse tailscale status JSON".into()))?;
    Ok(status)
}

fn hostname(raw: &str) -> String {
    raw.trim_end_matches('.').to_owned()
}

/// Derive a `NodeId` from a `Tailscale` stable node ID.
fn node_id(stable_id: &str) -> NodeId {
    NodeId::new(&format!("tailscale:{stable_id}"))
}

// ---------------------------------------------------------------------------
// Tailscale JSON schema (subset)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "Self")]
    self_node: Node,
    #[serde(rename = "Peer", default, deserialize_with = "null_as_default")]
    peer: std::collections::HashMap<String, Node>,
}

#[derive(Deserialize)]
struct Node {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "HostName")]
    host_name: String,
    #[serde(rename = "TailscaleIPs", default, deserialize_with = "null_as_empty")]
    tailscale_ips: Vec<IpAddr>,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "Tags", default, deserialize_with = "null_as_empty")]
    tags: Vec<String>,
}

fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}
