//! Authenticated local byte streams for granted endpoints.

use pluribus_core::{StreamEndpoint, StreamError, StreamGrant, StreamPage, StreamService};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;

const TICK: Duration = Duration::from_millis(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

struct Stream {
    socket: AsyncFd<UnixStream>,
    remaining: Mutex<u64>,
    writes: tokio::sync::Mutex<()>,
    deadline: Instant,
}

/// Connects granted endpoints and enforces their ceilings.
pub struct LocalStreamService {
    streams: Mutex<HashMap<String, Arc<Stream>>>,
    sequence: Mutex<u64>,
}

impl LocalStreamService {
    /// Validates the private runtime directory.
    ///
    /// # Errors
    /// Returns an error for an insecure directory.
    pub fn new(runtime_data: &Path) -> io::Result<Self> {
        validate_runtime_data(runtime_data)?;
        Ok(Self {
            streams: Mutex::new(HashMap::new()),
            sequence: Mutex::new(0),
        })
    }

    fn get(&self, id: &str) -> Result<Arc<Stream>, StreamError> {
        let stream = self
            .streams
            .lock()
            .map_err(|_| poisoned())?
            .get(id)
            .cloned()
            .ok_or_else(|| unavailable("unknown stream"))?;
        if Instant::now() >= stream.deadline {
            self.close(id);
            return Err(StreamError::DeadlineExceeded);
        }
        Ok(stream)
    }
}

#[async_trait::async_trait]
impl StreamService for LocalStreamService {
    async fn open(&self, grant: &StreamGrant) -> Result<String, StreamError> {
        let StreamEndpoint::Unix { path, peer_uids } = &grant.endpoint;
        validate_peers(peer_uids).map_err(unavailable)?;
        if !path.is_absolute() {
            return Err(unavailable("endpoint socket must be absolute"));
        }
        let timeout = Duration::from_millis(u64::from(grant.max_timeout_ms));
        let socket = tokio::time::timeout(timeout, tokio::net::UnixStream::connect(path))
            .await
            .map_err(|_| StreamError::DeadlineExceeded)?
            .map_err(unavailable)?
            .into_std()
            .map_err(unavailable)?;
        verify_peer(&socket, peer_uids).map_err(unavailable)?;
        let mut sequence = self.sequence.lock().map_err(|_| poisoned())?;
        *sequence += 1;
        let id = format!("stream:{sequence}");
        self.streams.lock().map_err(|_| poisoned())?.insert(
            id.clone(),
            Arc::new(Stream {
                socket: AsyncFd::new(socket).map_err(unavailable)?,
                remaining: Mutex::new(grant.max_bytes),
                writes: tokio::sync::Mutex::new(()),
                deadline: Instant::now() + timeout,
            }),
        );
        Ok(id)
    }

    async fn receive(
        &self,
        id: &str,
        max_bytes: u32,
        timeout_ms: u32,
        cancelled: &AtomicBool,
    ) -> Result<StreamPage, StreamError> {
        let stream = self.get(id)?;
        let until = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(StreamError::Cancelled);
            }
            if Instant::now() >= stream.deadline {
                self.close(id);
                return Err(StreamError::DeadlineExceeded);
            }
            // The budget lock covers only the nonblocking syscall.
            let result = {
                let mut remaining = stream.remaining.lock().map_err(|_| poisoned())?;
                let limit = usize::try_from(*remaining)
                    .unwrap_or(usize::MAX)
                    .min(max_bytes as usize);
                if limit == 0 {
                    self.close(id);
                    return Err(StreamError::LimitExceeded);
                }
                let mut bytes = vec![0; limit];
                match stream.socket.get_ref().read(&mut bytes) {
                    Ok(n) => {
                        *remaining -= n as u64;
                        bytes.truncate(n);
                        Some(Ok(StreamPage {
                            bytes,
                            closed: n == 0,
                        }))
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => None,
                    Err(e) => Some(Err(unavailable(e))),
                }
            };
            if let Some(result) = result {
                if result.is_err() {
                    self.close(id);
                }
                return result;
            }
            if Instant::now() >= until {
                return Ok(StreamPage {
                    bytes: Vec::new(),
                    closed: false,
                });
            }
            let wake = until.min(stream.deadline).min(Instant::now() + TICK);
            tokio::select! {
                ready = stream.socket.readable() => { ready.map_err(unavailable)?.clear_ready(); }
                () = tokio::time::sleep_until(wake.into()) => {}
            }
        }
    }

    async fn send(&self, id: &str, bytes: &[u8]) -> Result<(), StreamError> {
        let stream = self.get(id)?;
        let until = stream.deadline.min(Instant::now() + WRITE_TIMEOUT);
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
            let mut offset = 0;
            while offset < bytes.len() {
                let mut ready = stream.socket.writable().await.map_err(unavailable)?;
                match ready.try_io(|socket| socket.get_ref().write(&bytes[offset..])) {
                    Ok(Ok(0)) => return Err(unavailable("socket write returned zero")),
                    Ok(Ok(n)) => offset += n,
                    Ok(Err(e)) => return Err(unavailable(e)),
                    Err(_) => {}
                }
            }
            Ok(())
        };
        let result = tokio::time::timeout_at(until.into(), send)
            .await
            .unwrap_or(Err(StreamError::DeadlineExceeded));
        guard.complete = result.is_ok();
        result
    }

    fn shutdown_write(&self, id: &str) {
        if let Ok(stream) = self.get(id) {
            let _ = stream.socket.get_ref().shutdown(Shutdown::Write);
        }
    }

    fn close(&self, id: &str) {
        if let Ok(mut streams) = self.streams.lock()
            && let Some(stream) = streams.remove(id)
        {
            let _ = stream.socket.get_ref().shutdown(Shutdown::Both);
        }
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
