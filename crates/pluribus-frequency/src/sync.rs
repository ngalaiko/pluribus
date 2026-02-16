use std::{
    collections::HashMap,
    fmt,
    pin::Pin,
    sync::{Arc, Mutex},
};

use exn::ResultExt;
use futures_lite::io::{AsyncRead, AsyncWrite};
use futures_lite::StreamExt;
use futures_sink::Sink;
use serde::{Deserialize, Serialize};

use crate::protocol::{Manifest, NodeId};
use crate::state::State;

/// Errors returned by sync operations.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

#[derive(Serialize, Deserialize)]
enum WireMessage {
    Hello {
        manifest: Manifest,
        vv: loro::VersionVector,
    },
    Sync(#[serde(with = "serde_bytes")] Vec<u8>),
    Update(#[serde(with = "serde_bytes")] Vec<u8>),
    ManifestUpdate(Manifest),
}

impl fmt::Debug for WireMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello { manifest, .. } => write!(f, "Hello({manifest})"),
            Self::Sync(data) => write!(f, "Sync({}B)", data.len()),
            Self::Update(data) => write!(f, "Update({}B)", data.len()),
            Self::ManifestUpdate(manifest) => write!(f, "ManifestUpdate({manifest})"),
        }
    }
}

/// Handle to a running `WebSocket` sync session.
pub struct SyncHandle {
    task: async_executor::Task<exn::Result<(), Error>>,
}

impl SyncHandle {
    /// Wait for the sync to finish (connection dropped or error).
    pub async fn done(self) -> exn::Result<(), Error> {
        self.task.await
    }
}

async fn ws_send<S>(
    ws: &mut async_tungstenite::WebSocketStream<S>,
    msg: &WireMessage,
) -> exn::Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use async_tungstenite::tungstenite;
    use futures_lite::future::poll_fn;

    let bytes = rmp_serde::to_vec_named(msg).or_raise(|| Error("serialize wire message".into()))?;
    let message = tungstenite::Message::Binary(bytes);

    poll_fn(|cx| Pin::new(&mut *ws).poll_ready(cx))
        .await
        .or_raise(|| Error("WS poll_ready".into()))?;
    Pin::new(&mut *ws)
        .start_send(message)
        .or_raise(|| Error("WS start_send".into()))?;
    poll_fn(|cx| Pin::new(&mut *ws).poll_flush(cx))
        .await
        .or_raise(|| Error("WS poll_flush".into()))?;

    Ok(())
}

async fn ws_recv<S>(
    ws: &mut async_tungstenite::WebSocketStream<S>,
) -> exn::Result<Option<WireMessage>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use async_tungstenite::tungstenite;

    loop {
        let Some(result) = ws.next().await else {
            return Ok(None);
        };
        let msg = result.or_raise(|| Error("WS recv".into()))?;
        match msg {
            tungstenite::Message::Binary(data) => {
                let wire: WireMessage = rmp_serde::from_slice(&data)
                    .or_raise(|| Error("deserialize wire message".into()))?;
                return Ok(Some(wire));
            }
            tungstenite::Message::Close(_) => return Ok(None),
            // Ping/Pong handled automatically by tungstenite.
            _ => {}
        }
    }
}

/// Exchange `Hello` messages with a remote peer.
///
/// Sends our manifest and version vector, receives theirs.
/// Returns the remote manifest, remote version vector, and the `WebSocket`
/// for further use.
async fn exchange_hello<S>(
    log: &State,
    manifest: &Manifest,
    mut ws: async_tungstenite::WebSocketStream<S>,
) -> exn::Result<
    (
        Manifest,
        loro::VersionVector,
        async_tungstenite::WebSocketStream<S>,
    ),
    Error,
>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let vv = log.oplog_vv();
    ws_send(
        &mut ws,
        &WireMessage::Hello {
            manifest: manifest.clone(),
            vv,
        },
    )
    .await?;

    let Some(remote_hello) = ws_recv(&mut ws).await? else {
        exn::bail!(Error("connection closed before Hello".into()));
    };
    let WireMessage::Hello {
        manifest: remote_manifest,
        vv: remote_vv,
    } = remote_hello
    else {
        exn::bail!(Error("expected Hello".into()));
    };

    Ok((remote_manifest, remote_vv, ws))
}

/// Exchange deltas and start the background sync loop.
///
/// Call after [`exchange_hello`]. Computes the delta from our local state
/// since the remote's version vector, sends it, receives the remote's delta,
/// and then starts the bidirectional sync loop.
async fn sync_after_hello<S>(
    executor: Arc<async_executor::Executor<'static>>,
    log: &State,
    mut ws: async_tungstenite::WebSocketStream<S>,
    remote_vv: &loro::VersionVector,
    manifest_rx: async_broadcast::Receiver<Manifest>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
    remote_id: NodeId,
) -> exn::Result<SyncHandle, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Send our delta (updates the remote doesn't have).
    let delta = log
        .export_updates(remote_vv)
        .or_raise(|| Error("export CRDT updates for sync".into()))?;
    tracing::debug!(size = delta.len(), "sending sync delta");
    ws_send(&mut ws, &WireMessage::Sync(delta)).await?;

    // Receive remote's delta.
    let Some(remote_sync) = ws_recv(&mut ws).await? else {
        exn::bail!(Error("connection closed before Sync".into()));
    };
    match remote_sync {
        WireMessage::Sync(data) => {
            if !data.is_empty() {
                tracing::debug!(size = data.len(), "imported sync delta");
                log.import_remote(&data)
                    .or_raise(|| Error("import remote sync data".into()))?;
            }
        }
        _ => {
            exn::bail!(Error("expected Sync".into()));
        }
    }

    // Start the background sync loop.
    let log = log.clone();
    let peer_vv = log.oplog_vv();
    let ex = Arc::clone(&executor);
    let task = executor.spawn(sync_loop(ex, log, ws, peer_vv, manifest_rx, peers, remote_id));

    Ok(SyncHandle { task })
}

/// Exchange hello and start sync in one call.
///
/// Returns the remote manifest and a [`SyncHandle`].
async fn join_websocket<S>(
    executor: Arc<async_executor::Executor<'static>>,
    log: &State,
    manifest: &Manifest,
    ws: async_tungstenite::WebSocketStream<S>,
    manifest_rx: async_broadcast::Receiver<Manifest>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
) -> exn::Result<(Manifest, SyncHandle), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (remote, remote_vv, ws) = exchange_hello(log, manifest, ws).await?;
    let remote_id = remote.id.clone();
    let handle = sync_after_hello(executor, log, ws, &remote_vv, manifest_rx, peers, remote_id)
        .await?;
    Ok((remote, handle))
}

/// Connect to a remote peer at `addr`, perform the `WebSocket` handshake,
/// exchange hello, and start the sync loop.
///
/// Returns the remote manifest and a [`SyncHandle`].
pub async fn dial(
    executor: Arc<async_executor::Executor<'static>>,
    log: &State,
    manifest: &Manifest,
    addr: std::net::SocketAddr,
    manifest_rx: async_broadcast::Receiver<Manifest>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
) -> exn::Result<(Manifest, SyncHandle), Error> {
    let stream = async_net::TcpStream::connect(addr)
        .await
        .or_raise(|| Error(format!("TCP connect to {addr}")))?;
    let url = format!("ws://{addr}/");
    let (ws, _) = async_tungstenite::client_async(&url, stream)
        .await
        .or_raise(|| Error(format!("WS handshake with {addr}")))?;
    join_websocket(executor, log, manifest, ws, manifest_rx, peers).await
}

/// Accept an inbound TCP connection, upgrade to `WebSocket`, exchange hello,
/// and start the sync loop.
///
/// Returns the remote manifest and a [`SyncHandle`].
pub async fn accept(
    executor: Arc<async_executor::Executor<'static>>,
    log: &State,
    manifest: &Manifest,
    stream: async_net::TcpStream,
    manifest_rx: async_broadcast::Receiver<Manifest>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
) -> exn::Result<(Manifest, SyncHandle), Error> {
    let ws = async_tungstenite::accept_async(stream)
        .await
        .or_raise(|| Error("WS accept".into()))?;
    let (remote, remote_vv, ws) = exchange_hello(log, manifest, ws).await?;
    let remote_id = remote.id.clone();
    let handle = sync_after_hello(executor, log, ws, &remote_vv, manifest_rx, peers, remote_id)
        .await?;
    Ok((remote, handle))
}

/// Bidirectional sync loop.
///
/// Uses two concurrent tasks sharing the `WebSocket` via a channel:
/// - A reader task pulls messages from the WS and forwards to a channel.
/// - The main loop selects between local broadcast, manifest broadcast,
///   and remote channel.
#[allow(clippy::too_many_lines)]
async fn sync_loop<S>(
    executor: Arc<async_executor::Executor<'static>>,
    log: State,
    mut ws: async_tungstenite::WebSocketStream<S>,
    mut peer_vv: loro::VersionVector,
    mut manifest_rx: async_broadcast::Receiver<Manifest>,
    peers: Arc<Mutex<HashMap<NodeId, Manifest>>>,
    remote_id: NodeId,
) -> exn::Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Channel for remote messages received from the WS reader task.
    let (remote_tx, remote_rx) = async_channel::bounded::<Option<WireMessage>>(16);

    // Channel for outbound messages to send over WS.
    let (outbound_tx, outbound_rx) = async_channel::bounded::<WireMessage>(16);

    // Task: WS reader + writer. Owns the WebSocket.
    let ws_task: async_executor::Task<exn::Result<(), Error>> = executor.spawn(async move {
        // We need to handle both reading and writing concurrently on the same ws.
        // Use a simple poll loop: try to read, try to send pending outbound.
        loop {
            enum Action {
                Received(Option<WireMessage>),
                Outbound(WireMessage),
            }

            let action = futures_lite::future::or(
                async { Action::Received(ws_recv(&mut ws).await.unwrap_or(None)) },
                async {
                    outbound_rx
                        .recv()
                        .await
                        .map_or(Action::Received(None), Action::Outbound)
                },
            )
            .await;

            match action {
                Action::Received(msg) => {
                    let done = msg.is_none();
                    let _ = remote_tx.send(msg).await;
                    if done {
                        return Ok(());
                    }
                }
                Action::Outbound(msg) => {
                    ws_send(&mut ws, &msg).await?;
                }
            }
        }
    });

    // Main loop: react to local changes, manifest changes, and remote messages.
    let mut receiver = log.new_broadcast_receiver();

    let result: exn::Result<(), Error> = async {
        loop {
            enum Branch {
                LocalChange,
                ManifestChange(Manifest),
                Remote(Option<WireMessage>),
            }

            let branch = futures_lite::future::or(
                futures_lite::future::or(
                    async {
                        let _ = receiver.next().await;
                        Branch::LocalChange
                    },
                    async {
                        manifest_rx
                            .recv()
                            .await
                            .map_or(Branch::Remote(None), Branch::ManifestChange)
                    },
                ),
                async {
                    remote_rx
                        .recv()
                        .await
                        .map_or(Branch::Remote(None), Branch::Remote)
                },
            )
            .await;

            match branch {
                Branch::LocalChange => {
                    let updates = log
                        .export_updates(&peer_vv)
                        .or_raise(|| Error("export local updates for peer".into()))?;
                    if !updates.is_empty() {
                        tracing::trace!(size = updates.len(), "sending update to peer");
                        let _ = outbound_tx.send(WireMessage::Update(updates)).await;
                        peer_vv = log.oplog_vv();
                    }
                }
                Branch::ManifestChange(manifest) => {
                    tracing::debug!("sending manifest update to peer");
                    let _ = outbound_tx
                        .send(WireMessage::ManifestUpdate(manifest))
                        .await;
                }
                Branch::Remote(Some(WireMessage::Update(data))) => {
                    tracing::trace!(size = data.len(), "imported update from peer");
                    log.import_remote(&data)
                        .or_raise(|| Error("import remote update".into()))?;
                    peer_vv = log.oplog_vv();
                }
                Branch::Remote(Some(WireMessage::ManifestUpdate(manifest))) => {
                    tracing::debug!(peer = %manifest, "received manifest update from peer");
                    peers
                        .lock()
                        .expect("poisoned")
                        .insert(remote_id.clone(), manifest);
                }
                Branch::Remote(Some(WireMessage::Hello { .. } | WireMessage::Sync(_))) => {
                    exn::bail!(Error("unexpected Hello/Sync after handshake".into(),));
                }
                Branch::Remote(None) => return Ok(()),
            }
        }
    }
    .await;

    // Clean up: close channels so ws_task finishes.
    drop(outbound_tx);
    drop(remote_rx);
    let _ = ws_task.await;

    result
}
