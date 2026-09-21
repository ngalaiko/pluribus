//! TLS endpoints.
//!
//! The kernel vouches for nothing across a network, so the certificate is the
//! peer's whole identity. The host resolves the configured hostname, judges
//! every address it gets, connects to one it accepted, and verifies the
//! certificate against that same hostname: a second resolution cannot move
//! the target between the check and the connection.

use super::{StreamError, denied, unavailable};
use pluribus_core::StartTls;
use std::net::{Shutdown, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, pki_types::ServerName};

/// Ceiling on one preamble reply line. A peer that never sends CRLF must not
/// grow the host without bound.
const MAX_PREAMBLE_LINE: u64 = 8 * 1024;
/// Ceiling on lines in one multiline preamble reply.
const MAX_PREAMBLE_LINES: usize = 128;

/// One established TLS connection.
///
/// The halves are separately locked so a read parked on an idle connection
/// never blocks a write. `socket` is a second descriptor for the same TCP
/// connection, held only to force a shutdown from a synchronous teardown
/// path; nothing reads or writes through it.
pub struct Connection {
    reader: tokio::sync::Mutex<ReadHalf<TlsStream<TcpStream>>>,
    writer: tokio::sync::Mutex<WriteHalf<TlsStream<TcpStream>>>,
    socket: std::net::TcpStream,
}

impl Connection {
    pub async fn read(&self, bytes: &mut [u8]) -> Result<usize, StreamError> {
        self.reader
            .lock()
            .await
            .read(bytes)
            .await
            .map_err(unavailable)
    }

    pub async fn write_all(&self, bytes: &[u8]) -> Result<(), StreamError> {
        self.writer
            .lock()
            .await
            .write_all(bytes)
            .await
            .map_err(unavailable)
    }

    /// Half-closes: `close_notify`, then FIN. The peer reads a clean EOF while
    /// this side keeps reading.
    ///
    /// The shutdown needs the writer, and the caller is synchronous, so it
    /// runs detached. Without a runtime to detach into it degrades to a bare
    /// FIN, which every peer still reads as end of input.
    pub fn shutdown_write(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            let _ = self.socket.shutdown(Shutdown::Write);
            return;
        };
        let Ok(socket) = self.socket.try_clone() else {
            return;
        };
        // Cloning the lock is not possible, so the detached task takes the
        // descriptor and the writer half is left to close with the connection.
        handle.spawn_blocking(move || {
            let _ = socket.shutdown(Shutdown::Write);
        });
    }

    /// Drops the connection without `close_notify`. Reads parked on the socket
    /// wake immediately, which is what a stop needs.
    pub fn close(&self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

/// Connects and completes the handshake, both inside `timeout`.
///
/// `starttls` names a plaintext preamble the host speaks before the
/// handshake. The guest is handed the connection only once it is encrypted,
/// so an endpoint whose preamble does not reach the upgrade is refused rather
/// than downgraded.
pub async fn connect(
    hostname: &str,
    port: u16,
    allow_private_network: bool,
    starttls: Option<StartTls>,
    timeout: Duration,
) -> Result<Connection, StreamError> {
    let name = ServerName::try_from(hostname.to_owned())
        .map_err(|_| denied("endpoint hostname is not a valid DNS name"))?;
    if matches!(name, ServerName::IpAddress(_)) {
        return Err(denied("endpoint hostname must be a name, not an address"));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let addresses =
        tokio::time::timeout_at(deadline, resolve(hostname, port, allow_private_network))
            .await
            .map_err(|_| StreamError::DeadlineExceeded)??;
    let mut socket = tokio::time::timeout_at(deadline, tcp_connect(&addresses))
        .await
        .map_err(|_| StreamError::DeadlineExceeded)??;
    // Line-oriented protocols pay for Nagle on every command.
    socket.set_nodelay(true).map_err(unavailable)?;
    if let Some(preamble) = starttls {
        match preamble {
            StartTls::Smtp => tokio::time::timeout_at(deadline, smtp_preamble(&mut socket))
                .await
                .map_err(|_| StreamError::DeadlineExceeded)??,
        }
    }
    let shutdown_handle = socket
        .into_std()
        .and_then(|socket| {
            let handle = socket.try_clone()?;
            TcpStream::from_std(socket).map(|socket| (socket, handle))
        })
        .map_err(unavailable)?;
    let (socket, handle) = shutdown_handle;
    let stream = tokio::time::timeout_at(
        deadline,
        TlsConnector::from(config()?).connect(name, socket),
    )
    .await
    .map_err(|_| StreamError::DeadlineExceeded)?
    .map_err(|error| denied(format!("TLS handshake failed: {error}")))?;
    let (reader, writer) = tokio::io::split(stream);
    Ok(Connection {
        reader: tokio::sync::Mutex::new(reader),
        writer: tokio::sync::Mutex::new(writer),
        socket: handle,
    })
}

/// Speaks RFC 3207 up to the `220` after which the next byte is a handshake.
///
/// The host, not the guest, drives this: the grant's promise is that the
/// guest never holds a plaintext connection, and only the host knows where in
/// the dialogue the upgrade sits.
async fn smtp_preamble(socket: &mut TcpStream) -> Result<(), StreamError> {
    let name = ehlo_name(socket);
    let mut reader = BufReader::new(socket);
    expect(&mut reader, 220, "the greeting").await?;
    send_line(&mut reader, &format!("EHLO {name}")).await?;
    expect(&mut reader, 250, "EHLO").await?;
    send_line(&mut reader, "STARTTLS").await?;
    expect(&mut reader, 220, "STARTTLS").await?;
    // Bytes already buffered arrived before the handshake and would be
    // indistinguishable from the encrypted session's own output afterwards.
    if !reader.buffer().is_empty() {
        return Err(denied("endpoint sent data before the TLS handshake"));
    }
    Ok(())
}

/// The address literal RFC 5321 §4.1.3 allows a client with no domain of its
/// own. The guest re-issues `EHLO` after the handshake, as RFC 3207 requires,
/// so this name only has to be well formed.
fn ehlo_name(socket: &TcpStream) -> String {
    match socket.local_addr().map(|address| address.ip()) {
        Ok(std::net::IpAddr::V4(address)) => format!("[{address}]"),
        Ok(std::net::IpAddr::V6(address)) => format!("[IPv6:{address}]"),
        Err(_) => "[127.0.0.1]".into(),
    }
}

async fn send_line(reader: &mut BufReader<&mut TcpStream>, line: &str) -> Result<(), StreamError> {
    let socket = reader.get_mut();
    socket
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(unavailable)?;
    socket.flush().await.map_err(unavailable)
}

/// Reads one reply — `250-` continuation lines included — and checks its code.
///
/// A `4xx` refusal is the endpoint being busy, so it stays retryable. Anything
/// else, a server that will not upgrade included, is the grant's promise being
/// unmeetable and is denied.
async fn expect(
    reader: &mut BufReader<&mut TcpStream>,
    expected: u16,
    step: &str,
) -> Result<(), StreamError> {
    for _ in 0..MAX_PREAMBLE_LINES {
        let mut line = Vec::new();
        tokio::io::AsyncReadExt::take(&mut *reader, MAX_PREAMBLE_LINE)
            .read_until(b'\n', &mut line)
            .await
            .map_err(unavailable)?;
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let code = text
            .get(..3)
            .and_then(|digits| digits.parse::<u16>().ok())
            .ok_or_else(|| {
                denied(format!(
                    "endpoint answered {step} with a malformed SMTP reply"
                ))
            })?;
        if code != expected {
            let reason = format!("endpoint answered {step} with {text}");
            return Err(if (400..500).contains(&code) {
                unavailable(reason)
            } else {
                denied(reason)
            });
        }
        if text.as_bytes().get(3) != Some(&b'-') {
            return Ok(());
        }
    }
    Err(denied(format!("endpoint answered {step} without end")))
}

/// Connects to the first address that answers. A name with both families
/// resolves to both, and only one of them may be reachable.
async fn tcp_connect(addresses: &[SocketAddr]) -> Result<TcpStream, StreamError> {
    let mut last = unavailable("no address answered");
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(socket) => return Ok(socket),
            Err(error) => last = unavailable(error),
        }
    }
    Err(last)
}

/// Resolves and judges. One non-public answer denies the whole set: a split
/// answer is how a rebinding attack reaches a local service.
async fn resolve(
    hostname: &str,
    port: u16,
    allow_private_network: bool,
) -> Result<Vec<SocketAddr>, StreamError> {
    let addresses = tokio::net::lookup_host((hostname, port))
        .await
        .map_err(|_| unavailable("DNS resolution failed"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(unavailable("DNS returned no addresses"));
    }
    if !allow_private_network
        && !addresses
            .iter()
            .all(|address| pluribus_core::is_public_address(address.ip()))
    {
        return Err(denied("endpoint resolves to a non-public address"));
    }
    Ok(addresses)
}

/// The trust anchors, built once. Roots ship with the binary rather than
/// coming from the machine, so an installation verifies the same chains
/// wherever it runs.
fn config() -> Result<Arc<ClientConfig>, StreamError> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(Arc::clone(config));
    }
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| unavailable("TLS provider rejected the default protocol versions"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::clone(CONFIG.get_or_init(|| Arc::new(config))))
}
