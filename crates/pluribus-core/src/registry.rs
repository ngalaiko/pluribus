use crate::AppendRequest;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// Event types the core owns. A plugin may never propose one directly; the
/// host stamps them while committing a delivery or enforcing a gate.
///
/// Kept in step with `docs/events-and-authority.md`.
pub const RESERVED_EVENT_TYPES: &[&str] = &[
    // Instances and registry
    "component.failed",
    "component.backoff",
    "component.recovered",
    "operator.job-control",
    "operator.attempt-reconciled",
    // Observations
    "observation.received",
    // Gates
    "policy.decision",
    "credential.lifecycle",
    "credential.enrollment.requested",
    // Capability and model dispatch. The request itself is plugin-emitted;
    // the core owns the decision and the dispatch record.
    "activity.attempted",
    "activity.unknown",
    "capability.denied",
    "capability.timed-out",
    "capability.cancelled",
    // Timers, owned by the core scheduler
    "timer.fired",
    // Transport audit
    "stream.closed",
    // Inter-agent calls
    "agent.failed",
];

/// Event types a granted plugin may propose. Each is a result or a request
/// the core routes onward.
pub const PLUGIN_EVENT_TYPES: &[&str] = &[
    "credential.enrollment.started",
    "http.request.received",
    "http.response.requested",
    "telegram.media-ready",
    "telegram.media-failed",
    // Requests. A requester proposes; the core gates dispatch and records
    // policy.decision either way, so a refused attempt stays auditable.
    "capability.requested",
    "model.requested",
    // Capability results
    "capability.output",
    "capability.completed",
    "capability.failed",
    // Model results
    "model.stream",
    "model.completed",
    "model.failed",
    // Cognition
    "cognition.checkpoint",
    "cognition.job-updated",
    "cognition.observation-associated",
    "cognition.cancel-requested",
    "cognition.completed",
    "cognition.failed",
    // Memory projections
    "memory.remembered",
    "memory.superseded",
    "memory.forgotten",
    // Sandboxed code sessions
    "code.close-requested",
    "code.closed",
    "code.evaluate-requested",
    "code.yielded",
    "code.resumed",
    "code.completed",
    "code.failed",
    // Timers, requested by a plugin and answered by the core
    "timer.set",
    "timer.cancel",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ownership {
    /// Only the core may propose it.
    Core,
    /// A granted plugin may propose it.
    Plugin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EventTypeDeclaration {
    ownership: Ownership,
    /// Payload schema every event of this type must carry. `None` accepts any
    /// schema.
    payload_schema: Option<String>,
}

/// The closed vocabulary of event types.
///
/// A free-form event type is a typo waiting to silently create a second,
/// never-read stream of events. Every proposal is checked against this.
#[derive(Clone, Debug)]
pub struct EventTypeRegistry {
    types: BTreeMap<String, EventTypeDeclaration>,
}

impl EventTypeRegistry {
    /// Builds the registry holding the core vocabulary and nothing else.
    #[must_use]
    pub fn core() -> Self {
        let mut types = BTreeMap::new();
        for event_type in RESERVED_EVENT_TYPES {
            types.insert(
                (*event_type).to_owned(),
                EventTypeDeclaration {
                    ownership: Ownership::Core,
                    payload_schema: None,
                },
            );
        }
        for event_type in PLUGIN_EVENT_TYPES {
            types.insert(
                (*event_type).to_owned(),
                EventTypeDeclaration {
                    ownership: Ownership::Plugin,
                    payload_schema: None,
                },
            );
        }
        for (kind, schema) in [
            ("telegram.media-ready", "dev.pluribus.telegram.media.v1"),
            ("telegram.media-failed", "dev.pluribus.telegram.media.v1"),
            ("operator.job-control", "pluribus.operator-job-control/1"),
            (
                "operator.attempt-reconciled",
                "pluribus.operator-attempt-reconciled/1",
            ),
            ("component.backoff", "pluribus.component-health/1"),
            ("component.recovered", "pluribus.component-health/1"),
            ("credential.lifecycle", "pluribus.credential-lifecycle/1"),
            ("activity.attempted", "pluribus.activity-attempt/1"),
            ("activity.unknown", "pluribus.activity-attempt/1"),
            ("cognition.checkpoint", "pluribus.cognition-checkpoint/1"),
            ("cognition.job-updated", "pluribus.job/1"),
            (
                "cognition.observation-associated",
                "pluribus.observation-association/1",
            ),
            ("cognition.cancel-requested", "pluribus.cognition-cancel/1"),
        ] {
            if let Some(declaration) = types.get_mut(kind) {
                declaration.payload_schema = Some(schema.into());
            }
        }
        Self { types }
    }

    /// Checks a core-originated proposal against the vocabulary.
    ///
    /// # Errors
    ///
    /// Returns an error when the type is unknown or the payload schema differs
    /// from the declared one.
    pub fn validate(&self, request: &AppendRequest) -> Result<(), RegistryError> {
        let declaration = self
            .types
            .get(&request.event_type)
            .ok_or_else(|| RegistryError::Unknown(request.event_type.clone()))?;
        if let Some(schema) = &declaration.payload_schema
            && schema != &request.payload_schema
        {
            return Err(RegistryError::SchemaMismatch {
                event_type: request.event_type.clone(),
                expected: schema.clone(),
                actual: request.payload_schema.clone(),
            });
        }
        Ok(())
    }

    /// Checks a plugin proposal against the vocabulary and the instance's
    /// declared emissions.
    ///
    /// # Errors
    ///
    /// Returns an error when the type is unknown, core-owned, or absent from
    /// `granted`.
    pub fn authorize(
        &self,
        request: &AppendRequest,
        granted: &[String],
    ) -> Result<(), RegistryError> {
        self.validate(request)?;
        let declaration = self
            .types
            .get(&request.event_type)
            .ok_or_else(|| RegistryError::Unknown(request.event_type.clone()))?;
        // Connectors require an exact observation grant; wildcards confer no intake authority.
        let connector_observation = request.event_type == "observation.received"
            && granted
                .iter()
                .any(|event_type| event_type == "observation.received");
        if declaration.ownership == Ownership::Core && !connector_observation {
            return Err(RegistryError::Reserved(request.event_type.clone()));
        }
        if !granted
            .iter()
            .any(|pattern| matches_pattern(pattern, &request.event_type))
        {
            return Err(RegistryError::NotGranted(request.event_type.clone()));
        }
        Ok(())
    }
}

/// Matches an event type against a manifest pattern: an exact name, a
/// `prefix.*` wildcard, or `*`.
#[must_use]
pub fn matches_pattern(pattern: &str, event_type: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return event_type
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'));
    }
    pattern == event_type
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistryError {
    Unknown(String),
    Reserved(String),
    NotGranted(String),
    SchemaMismatch {
        event_type: String,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(event_type) => write!(formatter, "unknown event type: {event_type}"),
            Self::Reserved(event_type) => {
                write!(formatter, "event type is core-owned: {event_type}")
            }
            Self::NotGranted(event_type) => {
                write!(formatter, "instance may not emit {event_type}")
            }
            Self::SchemaMismatch {
                event_type,
                expected,
                actual,
            } => write!(
                formatter,
                "event type {event_type} carries schema {expected}, proposal used {actual}"
            ),
        }
    }
}

impl Error for RegistryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventPayload, PrincipalKind, PrincipalRef, StreamId, StreamKind};

    fn request(event_type: &str, payload_schema: &str) -> AppendRequest {
        AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: event_type.into(),
            payload_schema: payload_schema.into(),
            payload: EventPayload::CanonicalJson(b"{}".to_vec()),
            actor: PrincipalRef::new(PrincipalKind::Component, "shell-1"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        }
    }

    #[test]
    fn an_unknown_event_type_is_rejected() {
        let registry = EventTypeRegistry::core();

        let error = registry
            .validate(&request("capability.complete", "x/1"))
            .unwrap_err();

        assert_eq!(
            error,
            RegistryError::Unknown("capability.complete".into()),
            "a typo must not silently create a new event type"
        );
    }

    #[test]
    fn a_plugin_may_not_emit_a_core_owned_type() {
        let registry = EventTypeRegistry::core();
        let granted = vec!["*".to_owned()];

        let error = registry
            .authorize(&request("policy.decision", "x/1"), &granted)
            .unwrap_err();

        assert_eq!(error, RegistryError::Reserved("policy.decision".into()));
    }

    #[test]
    fn plugins_cannot_emit_operator_or_health_events() {
        let registry = EventTypeRegistry::core();
        for (kind, schema) in [
            ("operator.job-control", "pluribus.operator-job-control/1"),
            (
                "operator.attempt-reconciled",
                "pluribus.operator-attempt-reconciled/1",
            ),
            ("component.backoff", "pluribus.component-health/1"),
            ("component.recovered", "pluribus.component-health/1"),
        ] {
            assert_eq!(
                registry.authorize(&request(kind, schema), &["*".into()]),
                Err(RegistryError::Reserved(kind.into()))
            );
        }
    }

    #[test]
    fn a_plugin_may_emit_only_what_it_declared() {
        let registry = EventTypeRegistry::core();
        let granted = vec!["capability.completed".to_owned()];

        registry
            .authorize(&request("capability.completed", "x/1"), &granted)
            .unwrap();
        let error = registry
            .authorize(&request("capability.failed", "x/1"), &granted)
            .unwrap_err();

        assert_eq!(error, RegistryError::NotGranted("capability.failed".into()));
    }

    #[test]
    fn a_wildcard_grant_covers_a_plugin_type() {
        let registry = EventTypeRegistry::core();
        let granted = vec!["capability.*".to_owned()];

        registry
            .authorize(&request("capability.completed", "x/1"), &granted)
            .unwrap();
    }

    #[test]
    fn a_pinned_payload_schema_is_enforced() {
        let registry = EventTypeRegistry::core();

        let error = registry
            .validate(&request("cognition.checkpoint", "x/1"))
            .unwrap_err();

        assert_eq!(
            error,
            RegistryError::SchemaMismatch {
                event_type: "cognition.checkpoint".into(),
                expected: "pluribus.cognition-checkpoint/1".into(),
                actual: "x/1".into(),
            }
        );
    }

    #[test]
    fn wildcards_match_only_on_a_segment_boundary() {
        assert!(matches_pattern("capability.*", "capability.completed"));
        assert!(!matches_pattern("capability.*", "capabilities.completed"));
        assert!(!matches_pattern("capability.*", "capability"));
        assert!(matches_pattern("*", "anything.at.all"));
        assert!(matches_pattern("timer.set", "timer.set"));
        assert!(!matches_pattern("timer.set", "timer.setx"));
    }
}
