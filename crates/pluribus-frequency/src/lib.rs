pub mod network;
pub mod protocol;
pub mod state;
mod sync;

use std::{
    collections::HashMap,
    fmt,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use futures_lite::StreamExt;

use network::{Event, Network};
use protocol::{Manifest, NodeId, ToolDef};
use state::State;

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Handle to a running network session.
///
/// Dropping the handle cancels all networking tasks.
pub struct Handle {
    manifest: Arc<Mutex<Manifest>>,
    manifest_tx: async_broadcast::Sender<Manifest>,
    /// Keep-alive so the broadcast channel stays open even when no receivers exist yet.
    _manifest_rx: async_broadcast::InactiveReceiver<Manifest>,
    _shutdown_tx: async_channel::Sender<()>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
}

impl Handle {
    /// This node's manifest (owned clone).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn self_manifest(&self) -> Manifest {
        self.manifest.lock().expect("poisoned").clone()
    }

    /// Update the local tool list and broadcast the change to all peers.
    ///
    /// No-op if the tools haven't changed.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn update_tools(&self, tools: &[ToolDef]) {
        let mut inner = self.manifest.lock().expect("poisoned");
        if inner.tools == tools {
            return;
        }
        inner.tools = tools.to_vec();
        let manifest = inner.clone();
        drop(inner);
        tracing::debug!(tools = manifest.tools.len(), "broadcasting manifest update");
        let _ = self.manifest_tx.try_broadcast(manifest);
    }

    /// Snapshot of currently connected peers.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn others(&self) -> Vec<Manifest> {
        self.peers
            .lock()
            .expect("poisoned")
            .values()
            .cloned()
            .collect()
    }
}

/// Start networking: bind a TCP listener, spawn accept + discovery loops.
///
/// Builds a [`Manifest`] from the network identity and the given tool
/// definitions. Returns a [`Handle`] that provides access to the self
/// manifest and currently connected peers. Dropping the handle shuts
/// everything down.
pub async fn join<N: Network>(
    executor: Arc<async_executor::Executor<'static>>,
    network: N,
    log: &State,
    tools: Vec<ToolDef>,
) -> Result<Handle, Error> {
    let manifest = Manifest {
        id: network.id().clone(),
        name: network.name().into(),
        tools,
    };
    let port = network.port();

    let (shutdown_tx, shutdown_rx) = async_channel::bounded::<()>(1);
    let peers: Arc<Mutex<HashMap<NodeId, Manifest>>> = Arc::new(Mutex::new(HashMap::new()));

    let (mut manifest_tx, manifest_rx) = async_broadcast::broadcast::<Manifest>(16);
    manifest_tx.set_overflow(true);
    let manifest_keep_alive = manifest_rx.deactivate();
    let manifest = Arc::new(Mutex::new(manifest));

    // TCP listener.
    let listener = async_net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .map_err(|e| Error(format!("bind TCP listener on port {port}: {e}")))?;
    tracing::info!(port, "listening");

    // Accept loop.
    {
        let ex = Arc::clone(&executor);
        let log = log.clone();
        let manifest = Arc::clone(&manifest);
        let shutdown_rx = shutdown_rx.clone();
        let peers = Arc::clone(&peers);
        let manifest_tx = manifest_tx.clone();
        executor
            .spawn(async move {
                accept_loop(ex, listener, log, manifest, peers, shutdown_rx, manifest_tx).await;
            })
            .detach();
    }

    // Network event loop.
    {
        let ex = Arc::clone(&executor);
        let log = log.clone();
        let manifest = Arc::clone(&manifest);
        let shutdown_rx = shutdown_rx.clone();
        let peers = Arc::clone(&peers);
        let manifest_tx = manifest_tx.clone();
        executor
            .spawn(async move {
                network_event_loop(ex, network, log, manifest, peers, shutdown_rx, manifest_tx)
                    .await;
            })
            .detach();
    }

    Ok(Handle {
        manifest,
        manifest_tx,
        _manifest_rx: manifest_keep_alive,
        _shutdown_tx: shutdown_tx,
        peers,
    })
}

async fn accept_loop(
    executor: Arc<async_executor::Executor<'static>>,
    listener: async_net::TcpListener,
    log: State,
    manifest: Arc<Mutex<Manifest>>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
    shutdown_rx: async_channel::Receiver<()>,
    manifest_tx: async_broadcast::Sender<Manifest>,
) {
    loop {
        let accepted = futures_lite::future::or(async { listener.accept().await.ok() }, async {
            let _ = shutdown_rx.recv().await;
            None
        })
        .await;

        let Some((stream, addr)) = accepted else {
            break;
        };
        tracing::debug!(%addr, "accepted connection");

        let ex = Arc::clone(&executor);
        let log = log.clone();
        let manifest = Arc::clone(&manifest);
        let peers = Arc::clone(&peers);
        let shutdown_rx = shutdown_rx.clone();
        let manifest_tx = manifest_tx.clone();
        executor
            .spawn(async move {
                handle_inbound(ex, stream, &log, &manifest, peers, shutdown_rx, manifest_tx).await;
            })
            .detach();
    }
}

async fn handle_inbound(
    executor: Arc<async_executor::Executor<'static>>,
    stream: async_net::TcpStream,
    log: &State,
    manifest: &Arc<Mutex<Manifest>>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
    shutdown_rx: async_channel::Receiver<()>,
    manifest_tx: async_broadcast::Sender<Manifest>,
) {
    let manifest_snapshot = manifest.lock().expect("poisoned").clone();
    let manifest_rx = manifest_tx.new_receiver();
    let (remote, handle) = match sync::accept(
        executor,
        log,
        &manifest_snapshot,
        stream,
        manifest_rx,
        Arc::clone(&peers),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(?e, "handshake/sync error");
            return;
        }
    };

    tracing::info!(peer_name = %remote.name, peer_tools = remote.tools.len(), "synced");
    peers
        .lock()
        .expect("poisoned")
        .insert(remote.id.clone(), remote.clone());

    // Wait for sync to finish or shutdown.
    futures_lite::future::or(
        async {
            if let Err(e) = handle.done().await {
                tracing::warn!(peer_name = %remote.name, ?e, "sync error");
            }
        },
        async {
            let _ = shutdown_rx.recv().await;
        },
    )
    .await;

    peers.lock().expect("poisoned").remove(&remote.id);
    tracing::info!(peer_name = %remote.name, "disconnected");
}

// ---------------------------------------------------------------------------
// network_event_loop
// ---------------------------------------------------------------------------

async fn network_event_loop<N: Network>(
    executor: Arc<async_executor::Executor<'static>>,
    network: N,
    log: State,
    manifest: Arc<Mutex<Manifest>>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
    shutdown_rx: async_channel::Receiver<()>,
    manifest_tx: async_broadcast::Sender<Manifest>,
) {
    let mut events = network.events();
    let self_id = manifest.lock().expect("poisoned").id.clone();

    loop {
        let event = futures_lite::future::or(async { events.next().await }, async {
            let _ = shutdown_rx.recv().await;
            None
        })
        .await;

        let Some(event) = event else {
            break;
        };

        match event {
            Event::Online { id, name, addr } => {
                if id == self_id {
                    continue;
                }

                // Already connected — skip.
                {
                    let inner = peers.lock().expect("poisoned");
                    if inner.contains_key(&id) {
                        drop(inner);
                        continue;
                    }
                    drop(inner);
                }

                // Tie-break: only the node with the smaller ID dials out.
                if self_id > id {
                    continue;
                }

                let ex = Arc::clone(&executor);
                let log = log.clone();
                let manifest = Arc::clone(&manifest);
                let peers = Arc::clone(&peers);
                let manifest_tx = manifest_tx.clone();
                executor
                    .spawn(async move {
                        tracing::info!(%name, %addr, "connecting to peer");
                        connect_to_peer(ex, &log, &manifest, &peers, &id, &name, addr, manifest_tx)
                            .await;
                    })
                    .detach();
            }
            Event::Offline { .. } => {
                // Peer disconnection is handled naturally when the sync
                // handle terminates (in handle_inbound / connect_to_peer).
            }
        }
    }
}

// ---------------------------------------------------------------------------
// connect_to_peer
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn connect_to_peer(
    executor: Arc<async_executor::Executor<'static>>,
    log: &State,
    manifest: &Arc<Mutex<Manifest>>,
    peers: &Arc<Mutex<HashMap<NodeId, Manifest>>>,
    peer_id: &NodeId,
    peer_name: &str,
    addr: SocketAddr,
    manifest_tx: async_broadcast::Sender<Manifest>,
) {
    let manifest_snapshot = manifest.lock().expect("poisoned").clone();
    let manifest_rx = manifest_tx.new_receiver();
    let (remote, handle) = match sync::dial(
        executor,
        log,
        &manifest_snapshot,
        addr,
        manifest_rx,
        Arc::clone(peers),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(%peer_name, ?e, "connection failed");
            return;
        }
    };

    tracing::info!(peer_name = %remote.name, peer_tools = remote.tools.len(), "synced");
    peers
        .lock()
        .expect("poisoned")
        .insert(peer_id.clone(), remote.clone());

    if let Err(e) = handle.done().await {
        tracing::warn!(peer_name = %remote.name, ?e, "sync error");
    }

    peers.lock().expect("poisoned").remove(peer_id);
    tracing::info!(peer_name = %remote.name, "disconnected");
}
