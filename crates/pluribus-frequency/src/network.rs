pub mod tailscale;

use std::net::SocketAddr;

use crate::protocol::NodeId;

/// A peer appeared or disappeared on the network.
pub enum Event {
    Online {
        id: NodeId,
        name: String,
        addr: SocketAddr,
    },
    Offline {
        id: NodeId,
    },
}

/// Event-driven peer discovery.
///
/// Implementations poll the underlying network for peer changes and emit
/// [`Event`]s. `events()` consumes `self` — the returned stream owns the
/// discovery resources.
pub trait Network: Send + 'static {
    /// Stream of discovery events.
    type Events: futures_lite::Stream<Item = Event> + Send + Unpin;

    /// This node's identity.
    fn id(&self) -> &NodeId;
    /// This node's human-readable name.
    fn name(&self) -> &str;
    /// The port this node listens on.
    fn port(&self) -> u16;
    /// Consume the network and return a stream of events.
    fn events(self) -> Self::Events;
}

/// Create a [`Tailscale`](tailscale::Tailscale) network backend.
///
/// Calls `tailscale status --json` once to determine this node's identity.
pub async fn tailscale(port: u16) -> exn::Result<tailscale::Tailscale, tailscale::Error> {
    tailscale::Tailscale::new(port).await
}
