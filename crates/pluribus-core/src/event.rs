use crate::{AuthorityId, PrincipalRef};
use std::error::Error;
use std::fmt;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EventId(String);

impl EventId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StreamId(String);

impl StreamId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StreamKind {
    Agent,
    Node,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BlobRef {
    pub algorithm: String,
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventPayload {
    CanonicalJson(Vec<u8>),
    Blob(BlobRef),
}

/// An event before the store assigns ordering and identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendRequest {
    pub stream_id: StreamId,
    pub stream_kind: StreamKind,
    pub observed_at_ms: Option<i64>,
    pub event_type: String,
    pub payload_schema: String,
    pub payload: EventPayload,
    pub actor: PrincipalRef,
    pub authority_id: Option<AuthorityId>,
    pub activity_id: Option<String>,
    pub correlation_id: Option<String>,
    pub causation_id: Option<EventId>,
    pub deduplication_key: Option<String>,
}

impl AppendRequest {
    /// Checks invariants shared by every store implementation.
    ///
    /// # Errors
    ///
    /// Returns an error when a required identifier or type is empty.
    pub fn validate(&self) -> Result<(), AppendError> {
        if self.stream_id.as_str().is_empty() {
            return Err(AppendError::InvalidEvent("stream ID is empty".into()));
        }
        if self.event_type.is_empty() {
            return Err(AppendError::InvalidEvent("event type is empty".into()));
        }
        if self.payload_schema.is_empty() {
            return Err(AppendError::InvalidEvent("payload schema is empty".into()));
        }
        if self
            .deduplication_key
            .as_ref()
            .is_some_and(String::is_empty)
        {
            return Err(AppendError::InvalidEvent(
                "deduplication key is empty".into(),
            ));
        }
        if let EventPayload::Blob(blob) = &self.payload {
            crate::validate_blob_ref(blob)
                .map_err(|error| AppendError::InvalidEvent(error.to_string()))?;
        }
        Ok(())
    }
}

/// Immutable event returned after an atomic append.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedEvent {
    pub schema: String,
    pub event_id: EventId,
    pub sequence: u64,
    pub recorded_at_ms: i64,
    pub request: AppendRequest,
}

impl CommittedEvent {
    pub const SCHEMA: &str = "pluribus.event/1";
}

/// Filter over one stream for history reads and indexed search.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EventQuery {
    pub after_sequence: Option<u64>,
    /// Excludes this sequence and all newer events.
    pub before_sequence: Option<u64>,
    pub event_types: Vec<String>,
    /// Literal text to search in the indexed event type and JSON payload.
    pub text_query: Option<String>,
    /// Sort by descending sequence when set. The default preserves history.read.
    pub descending: bool,
    pub correlation_id: Option<String>,
    pub activity_id: Option<String>,
    pub recorded_from_ms: Option<i64>,
    pub recorded_to_ms: Option<i64>,
}

impl EventQuery {
    /// Tests metadata predicates. Text matching belongs to the store index.
    #[must_use]
    pub fn matches(&self, event: &CommittedEvent) -> bool {
        if self
            .after_sequence
            .is_some_and(|after| event.sequence <= after)
        {
            return false;
        }
        if self
            .before_sequence
            .is_some_and(|before| event.sequence >= before)
        {
            return false;
        }
        if !self.event_types.is_empty()
            && !self
                .event_types
                .iter()
                .any(|event_type| event_type == &event.request.event_type)
        {
            return false;
        }
        if self.correlation_id.is_some() && self.correlation_id != event.request.correlation_id {
            return false;
        }
        if self.activity_id.is_some() && self.activity_id != event.request.activity_id {
            return false;
        }
        if self
            .recorded_from_ms
            .is_some_and(|from| event.recorded_at_ms < from)
        {
            return false;
        }
        if self
            .recorded_to_ms
            .is_some_and(|to| event.recorded_at_ms > to)
        {
            return false;
        }
        true
    }
}

/// Authoritative append-only event storage.
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    /// Atomically assigns the next gapless sequence and commits the event.
    /// A repeated deduplication key returns the original committed event.
    ///
    /// # Errors
    ///
    /// Returns an error when validation or the atomic commit fails.
    async fn append(&self, request: AppendRequest) -> Result<CommittedEvent, AppendError>;

    /// Reads committed events in ascending sequence order.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream cannot be read.
    async fn read(
        &self,
        stream: &StreamId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<CommittedEvent>, AppendError>;

    /// Reads one event by identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream cannot be read.
    async fn get(&self, event_id: &EventId) -> Result<Option<CommittedEvent>, AppendError>;

    /// Reads matching events in sequence order, descending when requested.
    ///
    /// Implementations must serve this from an index. A scan over the whole
    /// stream is not an acceptable implementation: plugins call it directly.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream cannot be read.
    async fn query(
        &self,
        stream: &StreamId,
        query: &EventQuery,
        limit: usize,
    ) -> Result<Vec<CommittedEvent>, AppendError>;
}

/// Supplies commit metadata which production stores derive from `UUIDv7` and a clock.
pub trait EventMetadataSource: Send + Sync {
    fn next_event_id(&self) -> EventId;
    fn now_ms(&self) -> i64;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppendError {
    InvalidEvent(String),
    Storage(String),
}

impl fmt::Display for AppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEvent(message) => write!(formatter, "invalid event: {message}"),
            Self::Storage(message) => write!(formatter, "event storage failed: {message}"),
        }
    }
}

impl Error for AppendError {}
