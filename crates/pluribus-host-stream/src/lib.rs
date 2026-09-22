//! Authenticated byte streams for granted endpoints.
//!
//! Two endpoint kinds, one contract. A Unix endpoint is authenticated by the
//! kernel's report of the peer's account; a TLS endpoint by a certificate
//! chaining to a trusted root and matching the configured hostname. Both
//! share the grant's byte budget, its establishment timeout, and its
//! connection ceiling.

pub mod ipc;
mod tls;

use pluribus_core::{StreamEndpoint, StreamError, StreamGrant, StreamPage, StreamService};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;

/// Write bound for a local endpoint, which is either reading or gone.
const LOCAL_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// Write bound for a remote endpoint. Generous, because a slow network is not
/// a dead peer. Reads stay unbounded: an idle connection is the normal state
/// of a server-push session.
const REMOTE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// One connection. `remaining` is the grant's byte budget, shared by both
/// directions.
struct Stream {
    transport: Transport,
    remaining: Mutex<u64>,
    writes: tokio::sync::Mutex<()>,
}

enum Transport {
    Unix(AsyncFd<UnixStream>),
    Tls(tls::Connection),
}

impl Stream {
    fn write_timeout(&self) -> Duration {
        match self.transport {
            Transport::Unix(_) => LOCAL_WRITE_TIMEOUT,
            Transport::Tls(_) => REMOTE_WRITE_TIMEOUT,
        }
    }
}

/// Connects granted endpoints and enforces their ceilings.
pub struct LocalStreamService {
    streams: Mutex<HashMap<String, Arc<Stream>>>,
    sequence: Mutex<u64>,
    /// Whether this service validated a private runtime directory. Local
    /// endpoints require one; a service built for remote endpoints refuses
    /// them rather than connecting without the check.
    local_endpoints: bool,
}

impl LocalStreamService {
    /// Validates the private runtime directory local endpoints live in.
    ///
    /// # Errors
    /// Returns an error for an insecure directory.
    pub fn new(runtime_data: &Path) -> io::Result<Self> {
        validate_runtime_data(runtime_data)?;
        Ok(Self {
            streams: Mutex::new(HashMap::new()),
            sequence: Mutex::new(0),
            local_endpoints: true,
        })
    }

    /// A service for remote endpoints only. No local socket path is involved,
    /// so no runtime directory is required or accepted.
    #[must_use]
    pub fn remote() -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
            sequence: Mutex::new(0),
            local_endpoints: false,
        }
    }

    fn get(&self, id: &str) -> Result<Arc<Stream>, StreamError> {
        self.streams
            .lock()
            .map_err(|_| poisoned())?
            .get(id)
            .cloned()
            .ok_or_else(|| unavailable("unknown stream"))
    }

    /// Registers a connection, refusing one past the grant's ceiling.
    fn register(&self, grant: &StreamGrant, transport: Transport) -> Result<String, StreamError> {
        let mut streams = self.streams.lock().map_err(|_| poisoned())?;
        if streams.len() as u64 >= u64::from(grant.max_connections) {
            return Err(StreamError::LimitExceeded);
        }
        let mut sequence = self.sequence.lock().map_err(|_| poisoned())?;
        *sequence += 1;
        let id = format!("stream:{sequence}");
        streams.insert(
            id.clone(),
            Arc::new(Stream {
                transport,
                remaining: Mutex::new(grant.max_bytes),
                writes: tokio::sync::Mutex::new(()),
            }),
        );
        Ok(id)
    }

    async fn open_unix(
        &self,
        grant: &StreamGrant,
        path: &Path,
        peer_uids: &[u32],
    ) -> Result<Transport, StreamError> {
        if !self.local_endpoints {
            return Err(StreamError::Denied(
                "local endpoint requires a validated runtime directory".into(),
            ));
        }
        validate_peers(peer_uids).map_err(denied)?;
        if !path.is_absolute() {
            return Err(StreamError::Denied(
                "endpoint socket must be absolute".into(),
            ));
        }
        let timeout = Duration::from_millis(u64::from(grant.max_timeout_ms));
        let socket = tokio::time::timeout(timeout, tokio::net::UnixStream::connect(path))
            .await
            .map_err(|_| StreamError::DeadlineExceeded)?
            .map_err(unavailable)?
            .into_std()
            .map_err(unavailable)?;
        verify_peer(&socket, peer_uids).map_err(denied)?;
        Ok(Transport::Unix(AsyncFd::new(socket).map_err(unavailable)?))
    }
}

#[async_trait::async_trait]
impl StreamService for LocalStreamService {
    async fn open(&self, grant: &StreamGrant) -> Result<String, StreamError> {
        if grant.max_connections == 0 {
            return Err(StreamError::Denied("grant permits no connections".into()));
        }
        let transport = match &grant.endpoint {
            StreamEndpoint::Unix { path, peer_uids } => {
                self.open_unix(grant, path, peer_uids).await?
            }
            StreamEndpoint::Tls {
                hostname,
                port,
                allow_private_network,
                starttls,
            } => Transport::Tls(
                tls::connect(
                    hostname,
                    *port,
                    *allow_private_network,
                    *starttls,
                    Duration::from_millis(u64::from(grant.max_timeout_ms)),
                )
                .await?,
            ),
        };
        self.register(grant, transport)
    }

    async fn next(&self, id: &str, max_bytes: u32) -> Result<StreamPage, StreamError> {
        let stream = self.get(id)?;
        match &stream.transport {
            Transport::Unix(socket) => loop {
                let mut ready = socket.readable().await.map_err(unavailable)?;
                let limit = stream.take_read_budget(max_bytes)?;
                let mut bytes = vec![0; limit];
                match ready.try_io(|socket| socket.get_ref().read(&mut bytes)) {
                    Ok(Ok(n)) => return stream.finish_read(bytes, limit, n),
                    Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                    Ok(Err(e)) => return Err(unavailable(e)),
                    Err(_) => {}
                }
            },
            Transport::Tls(connection) => {
                let limit = stream.take_read_budget(max_bytes)?;
                let mut bytes = vec![0; limit];
                let n = connection.read(&mut bytes).await?;
                stream.finish_read(bytes, limit, n)
            }
        }
    }

    async fn send(&self, id: &str, bytes: &[u8]) -> Result<(), StreamError> {
        let stream = self.get(id)?;
        let until = Instant::now() + stream.write_timeout();
        let _writer = tokio::time::timeout_at(until.into(), stream.writes.lock())
            .await
            .map_err(|_| StreamError::DeadlineExceeded)?;
        {
            let mut remaining = stream.remaining.lock().map_err(|_| poisoned())?;
            if bytes.len() as u64 > *remaining {
                self.close(id);
                return Err(StreamError::LimitExceeded);
            }
            *remaining -= bytes.len() as u64;
        }
        let mut guard = WriteGuard {
            service: self,
            id,
            complete: false,
        };
        let send = async {
            match &stream.transport {
                Transport::Unix(socket) => {
                    let mut offset = 0;
                    while offset < bytes.len() {
                        let mut ready = socket.writable().await.map_err(unavailable)?;
                        match ready.try_io(|socket| socket.get_ref().write(&bytes[offset..])) {
                            Ok(Ok(0)) => return Err(unavailable("socket write returned zero")),
                            Ok(Ok(n)) => offset += n,
                            Ok(Err(e)) => return Err(unavailable(e)),
                            Err(_) => {}
                        }
                    }
                    Ok(())
                }
                Transport::Tls(connection) => connection.write_all(bytes).await,
            }
        };
        let result = tokio::time::timeout_at(until.into(), send)
            .await
            .unwrap_or(Err(StreamError::DeadlineExceeded));
        guard.complete = result.is_ok();
        result
    }

    fn shutdown_write(&self, id: &str) {
        if let Ok(stream) = self.get(id) {
            match &stream.transport {
                Transport::Unix(socket) => {
                    let _ = socket.get_ref().shutdown(Shutdown::Write);
                }
                Transport::Tls(connection) => connection.shutdown_write(),
            }
        }
    }

    fn close(&self, id: &str) {
        if let Ok(mut streams) = self.streams.lock()
            && let Some(stream) = streams.remove(id)
        {
            match &stream.transport {
                Transport::Unix(socket) => {
                    let _ = socket.get_ref().shutdown(Shutdown::Both);
                }
                Transport::Tls(connection) => connection.close(),
            }
        }
    }
}

impl Stream {
    /// Caps a read at the grant's current remainder.
    fn take_read_budget(&self, max_bytes: u32) -> Result<usize, StreamError> {
        let remaining = *self.remaining.lock().map_err(|_| poisoned())?;
        let limit = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(max_bytes as usize);
        if limit == 0 {
            return Err(StreamError::LimitExceeded);
        }
        Ok(limit)
    }

    /// Charges what was actually read and shapes the page.
    fn finish_read(
        &self,
        mut bytes: Vec<u8>,
        limit: usize,
        read: usize,
    ) -> Result<StreamPage, StreamError> {
        debug_assert!(read <= limit);
        let mut remaining = self.remaining.lock().map_err(|_| poisoned())?;
        if read as u64 > *remaining {
            *remaining = 0;
            return Err(StreamError::LimitExceeded);
        }
        *remaining -= read as u64;
        drop(remaining);
        bytes.truncate(read);
        Ok(StreamPage {
            bytes,
            closed: read == 0,
        })
    }
}

struct WriteGuard<'a> {
    service: &'a LocalStreamService,
    id: &'a str,
    complete: bool,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.service.close(self.id);
        }
    }
}

fn poisoned() -> StreamError {
    StreamError::Unavailable("stream transport poisoned".into())
}

fn unavailable(error: impl std::fmt::Display) -> StreamError {
    StreamError::Unavailable(error.to_string())
}

fn denied(error: impl std::fmt::Display) -> StreamError {
    StreamError::Denied(error.to_string())
}

fn validate_runtime_data(path: &Path) -> io::Result<()> {
    let metadata = path.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime data must be owned by the runtime account with directory mode 0700",
        ));
    }
    Ok(())
}

/// Rejects an endpoint nobody may answer, and any root account on either side.
///
/// Which accounts may answer is the grant's decision: an endpoint listing the
/// runtime's own account holds the runtime's authority.
fn validate_peers(peer_uids: &[u32]) -> io::Result<()> {
    let own_uid = rustix::process::geteuid().as_raw();
    if own_uid == 0 || peer_uids.is_empty() || peer_uids.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "endpoint requires non-root accounts and at least one allowed peer",
        ));
    }
    Ok(())
}

fn verify_peer(stream: &UnixStream, expected: &[u32]) -> io::Result<()> {
    if !expected.contains(&peer_uid(stream)?) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected endpoint socket peer",
        ));
    }
    Ok(())
}

/// Reads the peer's effective UID from the kernel, which the peer cannot forge.
///
/// Hosts without a peer-credential call fail to build rather than accepting
/// unverified peers.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let (uid, _) = nix::unistd::getpeereid(stream)?;
    Ok(uid.as_raw())
}

#[cfg(test)]
mod tests;
