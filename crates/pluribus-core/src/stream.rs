use std::path::PathBuf;

/// An endpoint a component reaches through an explicit grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEndpoint {
    /// Local socket authenticated by kernel-reported peer credentials.
    ///
    /// `peer_uids` lists the accounts allowed to answer it. Listing the
    /// runtime's own account gives the endpoint the runtime's authority, so
    /// only an endpoint that cannot act with it — a terminal a person reads,
    /// say — belongs there.
    Unix { path: PathBuf, peer_uids: Vec<u32> },
    /// Remote socket authenticated by a TLS server certificate chaining to a
    /// trusted root and matching `hostname`.
    ///
    /// The kernel vouches for nothing here, so the certificate is the whole
    /// of the peer's identity. `hostname` is both the name resolved and the
    /// name verified: the host connects to a checked address and presents
    /// that name for SNI, so a second resolution cannot move the target.
    Tls {
        hostname: String,
        port: u16,
        /// Permits an endpoint on a loopback, private, or otherwise
        /// non-public address. Off by default, as for HTTP.
        allow_private_network: bool,
        /// Upgrade an endpoint that answers in plaintext. The host speaks the
        /// preamble itself and hands the guest a stream that is already
        /// encrypted; there is no way to keep the plaintext one.
        starttls: Option<StartTls>,
    },
}

/// The plaintext preamble the host speaks before the handshake.
///
/// Only protocols the host can complete in full belong here: the guarantee is
/// that the guest never sees a plaintext byte, which needs the host to know
/// where the upgrade sits in the dialogue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartTls {
    /// RFC 3207: read the greeting, `EHLO`, `STARTTLS`, expect `220`.
    Smtp,
}

/// One endpoint and the ceilings the host enforces on it.
#[derive(Clone, Debug)]
pub struct StreamGrant {
    pub endpoint: StreamEndpoint,
    pub max_bytes: u64,
    pub max_timeout_ms: u32,
    /// Connections this grant may hold open at once.
    pub max_connections: u32,
}

/// Bytes read from a stream, and whether the peer closed it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamPage {
    pub bytes: Vec<u8>,
    pub closed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamError {
    Cancelled,
    DeadlineExceeded,
    LimitExceeded,
    /// The grant forbids the endpoint: a non-public address without the
    /// opt-in, or a peer whose identity does not check out. Retrying reaches
    /// the same verdict, so this is never retryable.
    Denied(String),
    Unavailable(String),
}

/// Transport for granted endpoints. Implementations carry no knowledge of the
/// protocol spoken over the stream.
#[async_trait::async_trait]
pub trait StreamService: Send + Sync {
    /// Connects and authenticates the peer. The grant's timeout bounds
    /// establishment; the connection itself has no idle deadline.
    ///
    /// # Errors
    /// Returns connection, authentication, or limit failures.
    async fn open(&self, grant: &StreamGrant) -> Result<String, StreamError>;

    /// Waits for bytes without polling. Dropping the future cancels the read;
    /// the caller must close its connection on teardown. Both directions
    /// draw on the grant's one byte budget.
    ///
    /// # Errors
    /// Returns transport or byte-budget failures.
    async fn next(&self, stream_id: &str, max_bytes: u32) -> Result<StreamPage, StreamError>;

    /// Writes the whole buffer.
    ///
    /// # Errors
    /// Returns limit or transport failures.
    async fn send(&self, stream_id: &str, bytes: &[u8]) -> Result<(), StreamError>;

    /// Closes the write direction, leaving reads open. The peer sees EOF
    /// and MAY treat it as cancellation. Unknown identifiers are ignored.
    fn shutdown_write(&self, stream_id: &str);

    /// Closes the stream. Unknown identifiers are ignored.
    ///
    /// A TLS connection is dropped without `close_notify`: the call sites are
    /// synchronous teardown paths, and an emergency stop is worth more than a
    /// clean shutdown record the peer discards anyway.
    fn close(&self, stream_id: &str);
}
