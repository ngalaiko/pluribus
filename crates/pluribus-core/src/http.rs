use crate::{BlobRef, PrincipalRef, SecretHandle};
use std::error::Error;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpHeader {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<HttpHeader>,
    pub body: Option<BlobRef>,
    pub credential_handle: Option<SecretHandle>,
    pub timeout_ms: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: BlobRef,
    pub credentials_used: Vec<SecretHandle>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpStreamingResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub stream_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpStreamProtocol {
    Bytes,
    ServerSentEvents,
    WebSocket,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpFrame {
    pub kind: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpFramePage {
    pub frames: Vec<HttpFrame>,
    pub closed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpGrant {
    pub component: PrincipalRef,
    pub origins: Vec<String>,
    pub methods: Vec<String>,
    pub allow_http: bool,
    pub allow_private_network: bool,
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
    pub max_redirects: u8,
    pub max_timeout_ms: u32,
}

#[async_trait::async_trait]
pub trait HttpService: Send + Sync {
    /// Executes one policy-controlled request.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, denied policy, transport failure, or exhausted limits.
    async fn send(
        &self,
        grant: &HttpGrant,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpError>;
}

#[async_trait::async_trait]
pub trait HttpStreamService: HttpService {
    /// Opens an HTTP body without buffering it or rejecting HTTP error statuses.
    /// `None` selects the buffered compatibility path.
    async fn start_response(
        &self,
        _grant: &HttpGrant,
        _request: &HttpRequest,
    ) -> Result<Option<HttpStreamingResponse>, HttpError> {
        Ok(None)
    }

    /// Creates an isolated transport using the supplied body store.
    /// In-memory fixtures may retain their existing store.
    fn with_body_store(
        &self,
        _store: std::sync::Arc<dyn crate::BlobStore>,
    ) -> Option<std::sync::Arc<dyn HttpStreamService>> {
        None
    }

    /// Opens one policy-controlled stream.
    ///
    /// # Errors
    ///
    /// Returns an error for denied policy, transport failure, or unsupported protocols.
    async fn open_stream(
        &self,
        grant: &HttpGrant,
        protocol: HttpStreamProtocol,
        request: &HttpRequest,
    ) -> Result<String, HttpError>;

    /// Waits for bounded stream frames.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown streams, ownership mismatch, or transport failure.
    async fn receive(
        &self,
        grant: &HttpGrant,
        stream_id: &str,
        max_frames: u32,
        timeout_ms: u32,
    ) -> Result<HttpFramePage, HttpError>;

    /// Sends one frame to a bidirectional stream.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown streams, ownership mismatch, or unsupported protocols.
    async fn send_frame(
        &self,
        grant: &HttpGrant,
        stream_id: &str,
        frame: &HttpFrame,
    ) -> Result<(), HttpError>;

    fn close_stream(&self, grant: &HttpGrant, stream_id: &str);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HttpError {
    Invalid(String),
    Unsupported(String),
    PermissionDenied(String),
    NotFound(String),
    ResourceExhausted(String),
    Timeout,
    Cancelled,
    AuthenticationRequired,
    Unavailable(String),
    Internal(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid HTTP request: {message}"),
            Self::Unsupported(message) => {
                write!(formatter, "unsupported HTTP operation: {message}")
            }
            Self::PermissionDenied(message) => write!(formatter, "HTTP request denied: {message}"),
            Self::NotFound(message) => write!(formatter, "HTTP dependency missing: {message}"),
            Self::ResourceExhausted(message) => {
                write!(formatter, "HTTP resource exhausted: {message}")
            }
            Self::Timeout => formatter.write_str("HTTP request timed out"),
            Self::AuthenticationRequired => formatter.write_str("HTTP authentication required"),
            Self::Cancelled => formatter.write_str("HTTP request cancelled"),
            Self::Unavailable(message) => write!(formatter, "HTTP service unavailable: {message}"),
            Self::Internal(message) => write!(formatter, "HTTP service failed: {message}"),
        }
    }
}

impl Error for HttpError {}
