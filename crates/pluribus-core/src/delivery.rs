use crate::{
    AppendError, AppendRequest, CommittedEvent, StateError, StateMutation, StateNamespace, StreamId,
};
use std::error::Error;
use std::fmt;

/// Identifies one plugin instance's delivery cursor over one stream.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CursorKey {
    pub stream_id: StreamId,
    pub namespace: StateNamespace,
}

/// One atomic unit of work: the events a delivery proposed, the state it
/// mutated, and the cursor advance that records the delivery as handled.
///
/// A store commits all three or none. Partial commit would leave a projection
/// ahead of its cursor, which replays as duplicated work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryCommit {
    pub cursor: CursorKey,
    /// Cursor value read before the delivery. A mismatch commits nothing.
    pub expected_checkpoint: u64,
    /// Cursor value to store. `None` leaves the cursor unchanged, which
    /// redelivers the same events.
    pub checkpoint: Option<u64>,
    pub mutations: Vec<StateMutation>,
    pub events: Vec<AppendRequest>,
}

impl DeliveryCommit {
    /// Checks invariants shared by every store implementation.
    ///
    /// # Errors
    ///
    /// Returns an error when the cursor regresses, an event is invalid, or the
    /// batch repeats a deduplication key.
    pub fn validate(&self) -> Result<(), DeliveryError> {
        if self.cursor.stream_id.as_str().is_empty() {
            return Err(DeliveryError::Invalid("stream ID is empty".into()));
        }
        if self.cursor.namespace.as_str().is_empty() {
            return Err(DeliveryError::Invalid("namespace is empty".into()));
        }
        if let Some(checkpoint) = self.checkpoint
            && checkpoint < self.expected_checkpoint
        {
            return Err(DeliveryError::Regressed {
                from: self.expected_checkpoint,
                to: checkpoint,
            });
        }
        for event in &self.events {
            if event.stream_id != self.cursor.stream_id {
                return Err(DeliveryError::Invalid(
                    "event targets another stream".into(),
                ));
            }
            event.validate().map_err(DeliveryError::Append)?;
        }
        let mut keys = self
            .events
            .iter()
            .filter_map(|event| event.deduplication_key.as_deref())
            .collect::<Vec<_>>();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        if keys.len() != total {
            return Err(DeliveryError::Invalid(
                "batch repeats a deduplication key".into(),
            ));
        }
        Ok(())
    }
}

/// Outcome of one committed delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryReceipt {
    /// Committed events in proposal order. A deduplicated proposal yields the
    /// originally committed event.
    pub events: Vec<CommittedEvent>,
    pub checkpoint: u64,
    pub revision: u64,
}

/// Atomic commit of events, state, and delivery progress.
#[async_trait::async_trait]
pub trait DeliveryStore: Send + Sync {
    /// Reads the stored cursor value. Zero means nothing was delivered.
    ///
    /// # Errors
    ///
    /// Returns an error when the cursor cannot be read.
    async fn checkpoint(&self, cursor: &CursorKey) -> Result<u64, DeliveryError>;

    /// Commits one delivery atomically.
    ///
    /// # Errors
    ///
    /// Returns `Conflict` without mutation when the stored cursor differs from
    /// `expected_checkpoint`.
    async fn commit(&self, commit: DeliveryCommit) -> Result<DeliveryReceipt, DeliveryError>;

    /// Discards a namespace and its cursor, forcing a rebuild from events.
    ///
    /// # Errors
    ///
    /// Returns an error when the namespace cannot be cleared.
    async fn discard(&self, cursor: &CursorKey) -> Result<(), DeliveryError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryError {
    Conflict { expected: u64, actual: u64 },
    Regressed { from: u64, to: u64 },
    Invalid(String),
    Append(AppendError),
    State(StateError),
    Storage(String),
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { expected, actual } => write!(
                formatter,
                "delivery checkpoint conflict: expected {expected}, actual {actual}"
            ),
            Self::Regressed { from, to } => {
                write!(formatter, "delivery checkpoint regressed: {from} to {to}")
            }
            Self::Invalid(message) => write!(formatter, "invalid delivery: {message}"),
            Self::Append(error) => write!(formatter, "{error}"),
            Self::State(error) => write!(formatter, "{error}"),
            Self::Storage(message) => write!(formatter, "delivery storage failed: {message}"),
        }
    }
}

impl Error for DeliveryError {}

impl From<AppendError> for DeliveryError {
    fn from(error: AppendError) -> Self {
        Self::Append(error)
    }
}

impl From<StateError> for DeliveryError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventPayload, PrincipalKind, PrincipalRef, StreamKind};

    fn event(stream: &str, deduplication_key: Option<&str>) -> AppendRequest {
        AppendRequest {
            stream_id: StreamId::new(stream),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "capability.completed".into(),
            payload_schema: "test.result/1".into(),
            payload: EventPayload::CanonicalJson(b"{}".to_vec()),
            actor: PrincipalRef::new(PrincipalKind::Component, "shell-1"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: deduplication_key.map(str::to_owned),
        }
    }

    fn commit(checkpoint: Option<u64>, events: Vec<AppendRequest>) -> DeliveryCommit {
        DeliveryCommit {
            cursor: CursorKey {
                stream_id: StreamId::new("personal"),
                namespace: StateNamespace::new("shell-1"),
            },
            expected_checkpoint: 4,
            checkpoint,
            mutations: Vec::new(),
            events,
        }
    }

    #[test]
    fn checkpoint_may_not_regress() {
        let error = commit(Some(3), Vec::new()).validate().unwrap_err();

        assert_eq!(error, DeliveryError::Regressed { from: 4, to: 3 });
    }

    #[test]
    fn unchanged_checkpoint_is_valid() {
        commit(Some(4), Vec::new()).validate().unwrap();
        commit(None, Vec::new()).validate().unwrap();
    }

    #[test]
    fn batch_may_not_repeat_a_deduplication_key() {
        let error = commit(
            Some(5),
            vec![
                event("personal", Some("call-1")),
                event("personal", Some("call-1")),
            ],
        )
        .validate()
        .unwrap_err();

        assert_eq!(
            error,
            DeliveryError::Invalid("batch repeats a deduplication key".into())
        );
    }

    #[test]
    fn unkeyed_events_do_not_collide() {
        commit(
            Some(5),
            vec![event("personal", None), event("personal", None)],
        )
        .validate()
        .unwrap();
    }

    #[test]
    fn events_must_target_the_cursor_stream() {
        let error = commit(Some(5), vec![event("family", None)])
            .validate()
            .unwrap_err();

        assert_eq!(
            error,
            DeliveryError::Invalid("event targets another stream".into())
        );
    }
}
