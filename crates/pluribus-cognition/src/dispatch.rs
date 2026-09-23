//! Routes committed events to the instances that handle them.
//!
//! Capability and model calls were a synchronous Rust stack: a caller held the
//! result while the callee ran, so nothing could survive a restart mid-call.
//! Here a request is an event, the provider answers with an event, and the
//! correlation lives in the log rather than on a stack.

use pluribus_core::{
    AppendRequest, Authority, CapabilityName, CommittedEvent, ConstraintPolicy, Decision,
    DenialReason, EventId, EventPayload, EventQuery, EventStore, EventTypeRegistry, PrincipalKind,
    PrincipalRef, StreamId, StreamKind, matches_pattern,
};
use pluribus_plugin_package::{ComponentManifest, ConstraintBinding};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use tokio::sync::Mutex;

/// What an instance receives, derived from its manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Subscriptions {
    pub idempotency: BTreeMap<String, String>,
    /// Every event on the stream. Reserved for cognition, which filters
    /// internally rather than declaring types.
    pub whole_stream: bool,
    /// Event-type patterns from `subscribes`.
    pub event_types: Vec<String>,
    /// Capability names from `provides`, matched against the payload of a
    /// `capability.requested`.
    pub capabilities: Vec<String>,
    /// Model identifiers this instance serves, matched against the payload of
    /// a `model.requested`.
    pub models: Vec<String>,
    /// Provider-declared projections from request arguments to grant selectors.
    pub constraint_bindings: BTreeMap<String, BTreeMap<String, ConstraintBinding>>,
}

impl Subscriptions {
    /// Derives subscriptions from a manifest and the model identifiers its
    /// configuration resolved to.
    #[must_use]
    pub fn from_manifest(manifest: &ComponentManifest, models: &[String]) -> Self {
        Self {
            idempotency: manifest
                .provides
                .iter()
                .map(|p| {
                    (
                        p.capability.clone(),
                        match p.idempotency {
                            pluribus_plugin_package::Idempotency::Inherent => "inherent",
                            pluribus_plugin_package::Idempotency::Keyed => "keyed",
                            pluribus_plugin_package::Idempotency::NonIdempotent => "non-idempotent",
                        }
                        .into(),
                    )
                })
                .collect(),
            whole_stream: manifest.subscribes_to_stream(),
            event_types: manifest
                .subscribes
                .iter()
                .filter(|pattern| pattern.as_str() != "*")
                .cloned()
                .collect(),
            capabilities: manifest
                .provides
                .iter()
                .map(|provided| provided.capability.clone())
                .collect(),
            models: models.to_vec(),
            constraint_bindings: manifest
                .provides
                .iter()
                .map(|capability| {
                    (
                        capability.capability.clone(),
                        capability.constraint_bindings.clone(),
                    )
                })
                .collect(),
        }
    }

    /// Reports whether this instance should receive the event.
    #[must_use]
    pub fn accepts(&self, event: &CommittedEvent) -> bool {
        if self.whole_stream {
            return true;
        }
        let event_type = event.request.event_type.as_str();
        if self
            .event_types
            .iter()
            .any(|pattern| matches_pattern(pattern, event_type))
        {
            return true;
        }
        match event_type {
            "capability.requested" => payload_field(event, "capability")
                .is_some_and(|name| self.capabilities.iter().any(|owned| owned == &name)),
            "model.requested" => payload_field(event, "model")
                .is_some_and(|name| self.models.iter().any(|owned| owned == &name)),
            _ => false,
        }
    }
}

/// Reads one string field out of a committed event's JSON payload.
///
/// A blob payload never carries routing keys: routing must not depend on
/// fetching blob content.
#[must_use]
pub fn payload_field(event: &CommittedEvent, field: &str) -> Option<String> {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        return None;
    };
    let value: Value = serde_json::from_slice(bytes).ok()?;
    value.get(field)?.as_str().map(str::to_owned)
}

/// One registered plugin instance.
#[derive(Clone, Debug)]
pub struct Registration {
    pub instance_id: String,
    pub subscriptions: Subscriptions,
}

struct ProjectedConstraints<'a, P> {
    policy: &'a P,
    bindings: Option<&'a BTreeMap<String, ConstraintBinding>>,
}

impl<P: ConstraintPolicy> ConstraintPolicy for ProjectedConstraints<'_, P> {
    fn allows(&self, grant: &pluribus_core::Grant, request: &[u8]) -> Result<bool, String> {
        let constraints: Value = serde_json::from_slice(grant.constraints.as_bytes())
            .map_err(|error| error.to_string())?;
        let constraints = constraints
            .as_object()
            .ok_or("constraints must be an object")?;
        let request_value: Value =
            serde_json::from_slice(request).map_err(|error| error.to_string())?;
        let mut selectors = serde_json::Map::new();
        for (key, binding) in self.bindings.into_iter().flatten() {
            if !constraints.contains_key(key) {
                if binding.required {
                    return Ok(false);
                }
                continue;
            }
            let mut selector = String::new();
            if binding.parts.is_empty() {
                return Ok(false);
            }
            for part in &binding.parts {
                let Some(value) = request_value.pointer(&part.pointer) else {
                    if part.optional {
                        continue;
                    }
                    return Ok(false);
                };
                let value = match value {
                    Value::String(value) => value.clone(),
                    Value::Number(value) => value.to_string(),
                    _ => return Ok(false),
                };
                selector.push_str(&part.prefix);
                selector.push_str(&value);
            }
            selectors.insert(key.clone(), Value::String(selector));
        }
        self.policy.allows_projected(
            grant,
            request,
            &serde_json::to_vec(&selectors).map_err(|error| error.to_string())?,
        )
    }
}

/// The decision the router reached about one request event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Routed {
    /// Deliver to this instance.
    Deliver { instance_id: String },
    /// No instance provides it.
    NoProvider { reason: String },
    /// Authority refused it.
    Denied { reason: String },
}

/// Maps committed events onto instances, gating requests through authority.
pub struct Router<P> {
    stream_id: StreamId,
    agent: PrincipalRef,
    registrations: Vec<Registration>,
    events: Arc<dyn EventStore>,
    registry: Arc<EventTypeRegistry>,
    constraints: P,
    projection: Mutex<DispatchProjection>,
}

impl<P: ConstraintPolicy> Router<P> {
    #[must_use]
    pub fn new(
        stream_id: StreamId,
        agent: PrincipalRef,
        events: Arc<dyn EventStore>,
        registry: Arc<EventTypeRegistry>,
        constraints: P,
    ) -> Self {
        Self {
            stream_id,
            agent,
            registrations: Vec::new(),
            events,
            registry,
            constraints,
            projection: Mutex::new(DispatchProjection::default()),
        }
    }

    /// Registers an instance. A later registration for the same id replaces
    /// the earlier one.
    pub fn register(&mut self, registration: Registration) {
        self.registrations
            .retain(|existing| existing.instance_id != registration.instance_id);
        self.registrations.push(registration);
    }

    pub fn unregister(&mut self, instance_id: &str) {
        self.registrations
            .retain(|existing| existing.instance_id != instance_id);
    }

    #[must_use]
    pub fn registrations(&self) -> &[Registration] {
        &self.registrations
    }

    /// Instances that should receive the event, in registration order.
    #[must_use]
    pub async fn recipients(&self, event: &CommittedEvent) -> Vec<String> {
        let mut recipients = Vec::new();
        for registration in &self.registrations {
            if registration.subscriptions.accepts(event)
                && self.owns_timer(&registration.instance_id, event).await
            {
                recipients.push(registration.instance_id.clone());
            }
        }
        recipients
    }

    async fn owns_timer(&self, instance_id: &str, event: &CommittedEvent) -> bool {
        if event.request.event_type != "timer.fired" {
            return true;
        }
        let Some(id) = payload_field(event, "requestEventId") else {
            return false;
        };
        self.events
            .get(&EventId::new(id))
            .await
            .ok()
            .flatten()
            .is_some_and(|request| {
                request.request.stream_id == self.stream_id
                    && request.request.event_type == "timer.set"
                    && request.request.actor.id.as_str() == instance_id
            })
    }

    /// Reads events after `checkpoint` that the named instance subscribes to.
    ///
    /// A whole-stream subscriber reads sequentially; a narrow subscriber gets
    /// an indexed query so it is not charged for the whole stream.
    ///
    /// # Errors
    ///
    /// Returns an error when the instance is unknown or the stream cannot be
    /// read.
    pub async fn poll(
        &self,
        instance_id: &str,
        checkpoint: u64,
        limit: usize,
    ) -> Result<Vec<CommittedEvent>, RouterError> {
        let registration = self
            .registrations
            .iter()
            .find(|registration| registration.instance_id == instance_id)
            .ok_or_else(|| RouterError::UnknownInstance(instance_id.to_owned()))?;

        let mut event_types = registration.subscriptions.event_types.clone();
        if !registration.subscriptions.capabilities.is_empty() {
            event_types.push("capability.requested".to_owned());
        }
        if !registration.subscriptions.models.is_empty() {
            event_types.push("model.requested".to_owned());
        }
        let wildcard = registration.subscriptions.whole_stream
            || event_types.iter().any(|pattern| pattern.contains('*'));
        if !wildcard && event_types.is_empty() {
            return Ok(Vec::new());
        }
        let mut after = checkpoint;
        let mut matched = Vec::new();
        while matched.len() < limit {
            let page = if wildcard {
                self.events.read(&self.stream_id, after, limit).await
            } else {
                self.events
                    .query(
                        &self.stream_id,
                        &EventQuery {
                            after_sequence: Some(after),
                            event_types: event_types.clone(),
                            ..EventQuery::default()
                        },
                        limit,
                    )
                    .await
            }
            .map_err(|e| RouterError::Storage(e.to_string()))?;
            if page.is_empty() {
                break;
            }
            after = page.last().map_or(after, |event| event.sequence);
            for event in page {
                if registration.subscriptions.accepts(&event)
                    && self.owns_timer(instance_id, &event).await
                {
                    matched.push(event);
                    if matched.len() == limit {
                        break;
                    }
                }
            }
        }
        Ok(matched)
    }

    /// Gates a request event, appending `policy.decision` either way.
    ///
    /// The requester proposed the event, so it is already committed. Refusing
    /// it here records the refusal rather than erasing the attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the audit append fails.
    pub async fn route_request(
        &self,
        event: &CommittedEvent,
        authority: &Authority,
        _provider: &PrincipalRef,
        now_ms: i64,
    ) -> Result<Routed, RouterError> {
        let Some(capability) = payload_field(event, "capability") else {
            return Ok(Routed::NoProvider {
                reason: "request has no capability name".into(),
            });
        };
        let arguments = match &event.request.payload {
            EventPayload::CanonicalJson(bytes) => serde_json::from_slice::<Value>(bytes)
                .ok()
                .map_or(Value::Null, |v| v["arguments"].clone()),
            EventPayload::Blob(_) => Value::Null,
        };
        let candidates: Vec<_> = self
            .registrations
            .iter()
            .filter(|r| r.subscriptions.capabilities.contains(&capability))
            .collect();
        if candidates.len() != 1 {
            let routed = Routed::NoProvider {
                reason: if candidates.is_empty() {
                    format!("no instance provides {capability}")
                } else {
                    format!("multiple providers for {capability}")
                },
            };
            self.append_decision(event, &capability, &routed).await?;
            return Ok(routed);
        }
        let instance_id = candidates[0].instance_id.clone();
        let provider = PrincipalRef::new(PrincipalKind::Component, &instance_id);
        let name = CapabilityName::new(capability.clone());

        let bindings = candidates[0]
            .subscriptions
            .constraint_bindings
            .get(&capability);
        let policy = ProjectedConstraints {
            policy: &self.constraints,
            bindings,
        };
        let decision = authority.decide(
            &name,
            &provider,
            arguments.to_string().as_bytes(),
            &policy,
            now_ms,
        );
        let routed = match decision {
            Decision::Allow { .. } => Routed::Deliver { instance_id },
            Decision::Deny { reason } => Routed::Denied {
                reason: denial_reason(&reason),
            },
        };

        self.append_decision(event, &capability, &routed).await?;
        Ok(routed)
    }

    async fn append_decision(
        &self,
        event: &CommittedEvent,
        capability: &str,
        routed: &Routed,
    ) -> Result<(), RouterError> {
        let (allowed, reason) = match routed {
            Routed::Deliver { .. } => (true, Value::Null),
            Routed::NoProvider { reason } | Routed::Denied { reason } => {
                (false, Value::String(reason.clone()))
            }
        };
        let payload = json!({
            "capability": capability,
            "allowed": allowed,
            "provider": match routed { Routed::Deliver {instance_id} => Some(instance_id), _ => None },
            "reason": reason,
            "requestEventId": event.event_id.as_str(),
        });
        let request = AppendRequest {
            stream_id: self.stream_id.clone(),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "policy.decision".into(),
            payload_schema: "pluribus.policy-decision/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&payload)
                    .map_err(|error| RouterError::Storage(error.to_string()))?,
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, self.agent.id.as_str().to_owned()),
            authority_id: event.request.authority_id.clone(),
            activity_id: event.request.activity_id.clone(),
            correlation_id: event.request.correlation_id.clone(),
            causation_id: Some(event.event_id.clone()),
            deduplication_key: Some(format!("policy:{}", event.event_id.as_str())),
        };
        self.registry
            .validate(&request)
            .map_err(|error| RouterError::Storage(error.to_string()))?;
        self.events
            .append(request)
            .await
            .map(|_| ())
            .map_err(|error| RouterError::Storage(error.to_string()))
    }

    /// Records that a refused request terminated, so the requester resumes
    /// instead of waiting for a result that will never arrive.
    ///
    /// # Errors
    ///
    /// Returns an error when the append fails.
    pub async fn append_denial(
        &self,
        event: &CommittedEvent,
        reason: &str,
    ) -> Result<CommittedEvent, RouterError> {
        let payload = json!({
            "requestEventId": event.event_id.as_str(),
            "reason": reason,
        });
        let request = AppendRequest {
            stream_id: self.stream_id.clone(),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "capability.denied".into(),
            payload_schema: "pluribus.capability-denied/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&payload)
                    .map_err(|error| RouterError::Storage(error.to_string()))?,
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, self.agent.id.as_str().to_owned()),
            authority_id: event.request.authority_id.clone(),
            activity_id: event.request.activity_id.clone(),
            correlation_id: event.request.correlation_id.clone(),
            causation_id: Some(event.event_id.clone()),
            deduplication_key: Some(format!("denied:{}", event.event_id.as_str())),
        };
        self.events
            .append(request)
            .await
            .map_err(|error| RouterError::Storage(error.to_string()))
    }

    /// Records that a component died mid-delivery.
    ///
    /// Core-owned, and necessarily so: a trap leaves no outcome for the
    /// component to return, and the registry refuses a plugin that proposes
    /// `component.failed`. Only the host can witness this.
    ///
    /// # Errors
    /// Returns storage errors.
    pub async fn append_component_failure(
        &self,
        instance_id: &str,
        batch: &[CommittedEvent],
        reason: &str,
    ) -> Result<CommittedEvent, RouterError> {
        let last = batch.last();
        let payload = json!({
            "instanceId": instance_id,
            "reason": reason,
            "deliveredFrom": batch.first().map(|event| event.sequence),
            "deliveredThrough": last.map(|event| event.sequence),
        });
        let request = AppendRequest {
            stream_id: self.stream_id.clone(),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "component.failed".into(),
            payload_schema: "pluribus.component-failed/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&payload)
                    .map_err(|error| RouterError::Storage(error.to_string()))?,
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, self.agent.id.as_str().to_owned()),
            authority_id: last.and_then(|event| event.request.authority_id.clone()),
            activity_id: last.and_then(|event| event.request.activity_id.clone()),
            correlation_id: last.and_then(|event| event.request.correlation_id.clone()),
            causation_id: last.map(|event| event.event_id.clone()),
            deduplication_key: last
                .map(|event| format!("component-failed:{instance_id}:{}", event.sequence)),
        };
        self.events
            .append(request)
            .await
            .map_err(|error| RouterError::Storage(error.to_string()))
    }

    /// Records a host-observed cognition resource failure. The event is
    /// durable before the delivery cursor may move past the input.
    /// # Errors
    /// Returns storage or registry errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_resource_exhaustion(
        &self,
        instance_id: &str,
        source: &CommittedEvent,
        remaining_attempts: u32,
        resource: Value,
        effect_status: &str,
        checkpoint_status: &str,
        job_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<CommittedEvent, RouterError> {
        let request = self.resource_exhaustion_request(
            instance_id,
            source,
            remaining_attempts,
            resource,
            effect_status,
            checkpoint_status,
            job_id,
            session_id,
        )?;
        self.events
            .append(request)
            .await
            .map_err(|error| RouterError::Storage(error.to_string()))
    }

    /// Builds the host-owned deferred event for an atomic delivery commit.
    /// # Errors
    /// Returns a registry or serialization error.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    pub fn resource_exhaustion_request(
        &self,
        instance_id: &str,
        source: &CommittedEvent,
        remaining_attempts: u32,
        resource: Value,
        effect_status: &str,
        checkpoint_status: &str,
        job_id: Option<String>,
        session_id: Option<String>,
    ) -> Result<AppendRequest, RouterError> {
        let payload = json!({
            "instanceId": instance_id,
            "requestEventId": source.event_id.as_str(),
            "inputEventId": source.event_id.as_str(),
            "input": deferred_input(source),
            "jobId": job_id.or_else(|| payload_field(source, "jobId")),
            "sessionId": session_id.or_else(|| payload_field(source, "sessionId")),
            "code": "resource-exhausted",
            "resource": resource,
            "remainingAttempts": remaining_attempts,
            "effectStatus": effect_status,
            "checkpointStatus": checkpoint_status,
            "deferred": true,
        });
        let request = AppendRequest {
            stream_id: self.stream_id.clone(),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "cognition.resource-exhausted".into(),
            payload_schema: "pluribus.cognition-resource-exhausted/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&payload)
                    .map_err(|error| RouterError::Storage(error.to_string()))?,
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, self.agent.id.as_str()),
            authority_id: source.request.authority_id.clone(),
            activity_id: source.request.activity_id.clone(),
            correlation_id: source.request.correlation_id.clone(),
            causation_id: Some(source.event_id.clone()),
            deduplication_key: Some(format!(
                "cognition-resource-exhausted:{instance_id}:{}",
                source.event_id.as_str()
            )),
        };
        self.registry
            .validate(&request)
            .map_err(|error| RouterError::Storage(error.to_string()))?;
        Ok(request)
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(crate) async fn append_health(
        &self,
        kind: &str,
        payload: Value,
    ) -> Result<CommittedEvent, RouterError> {
        self.events
            .append(AppendRequest {
                stream_id: self.stream_id.clone(),
                stream_kind: StreamKind::Agent,
                observed_at_ms: None,
                event_type: kind.into(),
                payload_schema: "pluribus.component-health/1".into(),
                payload: EventPayload::CanonicalJson(
                    serde_json::to_vec(&payload)
                        .map_err(|e| RouterError::Storage(e.to_string()))?,
                ),
                actor: PrincipalRef::new(PrincipalKind::Node, self.agent.id.as_str()),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: None,
            })
            .await
            .map_err(|e| RouterError::Storage(e.to_string()))
    }

    /// Checks the latest coordinator revision at dispatch admission.
    pub(crate) async fn revision_current(
        &self,
        request: &CommittedEvent,
    ) -> Result<bool, RouterError> {
        let Some(job_id) = payload_field(request, "jobId") else {
            return Ok(true);
        };
        let Some(revision) = json_number(request, "revision") else {
            return Ok(false);
        };
        let mut projection = self.projection.lock().await;
        projection
            .refresh(self.events.as_ref(), &self.stream_id, 1000)
            .await?;
        for registration in self
            .registrations
            .iter()
            .filter(|r| r.subscriptions.whole_stream)
        {
            let actor = &registration.instance_id;
            if projection
                .observations
                .get(actor)
                .is_some_and(|pending| !pending.is_empty())
            {
                return Ok(false);
            }
            if let Some((current_revision, status)) =
                projection.jobs.get(&(actor.clone(), job_id.clone()))
            {
                return Ok(*current_revision == revision
                    && !["cancelled", "waiting-input"].contains(&status.as_str()));
            }
        }
        Ok(true)
    }

    pub(crate) async fn expired(
        &self,
        request: &CommittedEvent,
        now_ms: i64,
    ) -> Result<bool, RouterError> {
        if json_number(request, "deadlineAtMs").is_none_or(|deadline| deadline > now_ms) {
            return Ok(false);
        }
        let kind = match request.request.event_type.as_str() {
            "capability.requested" => "capability.timed-out",
            "model.requested" => "model.failed",
            "code.evaluate-requested" => "code.failed",
            _ => return Ok(false),
        };
        self.attempt_event(request,kind,json!({"requestEventId":request.event_id.as_str(),"call_id":payload_field(request,"call_id"),"sessionId":payload_field(request,"sessionId"),"code":"deadline-exceeded","reason":"request deadline expired before admission"}),&format!("expired:{}",request.event_id.as_str())).await?;
        Ok(true)
    }

    pub(crate) async fn reject_stale_code(
        &self,
        request: &CommittedEvent,
    ) -> Result<(), RouterError> {
        self.attempt_event(request,"code.failed",json!({"requestEventId":request.event_id.as_str(),"sessionId":payload_field(request,"sessionId"),"reason":"job revision changed before cell admission"}),&format!("stale:{}",request.event_id.as_str())).await.map(|_|())
    }

    pub(crate) async fn cancel_attempt(&self, request: &CommittedEvent) -> Result<(), RouterError> {
        let kind = match request.request.event_type.as_str() {
            "capability.requested" => "capability.cancelled",
            "model.requested" => "model.failed",
            "code.evaluate-requested" | "code.resumed" => "code.failed",
            _ => return Ok(()),
        };
        if self.result_for(&request.event_id).await?.is_some() {
            return Ok(());
        }
        self.attempt_event(request, kind, json!({
            "requestEventId":request.event_id.as_str(), "call_id":payload_field(request,"call_id"),
            "sessionId":payload_field(request,"sessionId"), "code":"cancelled", "outcome":"unknown",
            "reason":"activity cancelled; effects may have occurred; reconcile before retrying"
        }), &format!("cancelled:{}",request.event_id.as_str())).await?;
        Ok(())
    }

    pub(crate) async fn fail_sessions(
        &self,
        provider: &str,
        batch: &[CommittedEvent],
    ) -> Result<(), RouterError> {
        self.fail_sessions_with_resource(provider, batch, None)
            .await
    }

    pub(crate) async fn fail_sessions_with_resource(
        &self,
        provider: &str,
        batch: &[CommittedEvent],
        resource_error: Option<Value>,
    ) -> Result<(), RouterError> {
        let mut pending = std::collections::BTreeMap::new();
        let mut after = 0;
        loop {
            let page = self
                .events
                .query(
                    &self.stream_id,
                    &EventQuery {
                        after_sequence: Some(after),
                        event_types: vec![
                            "code.yielded".into(),
                            "code.completed".into(),
                            "code.failed".into(),
                        ],
                        ..Default::default()
                    },
                    100,
                )
                .await
                .map_err(|e| RouterError::Storage(e.to_string()))?;
            if page.is_empty() {
                break;
            }
            for event in page {
                after = event.sequence;
                let Some(session) = payload_field(&event, "sessionId") else {
                    continue;
                };
                if event.request.event_type == "code.yielded" {
                    if event.request.actor.id.as_str() == provider {
                        pending.insert(session, event);
                    }
                } else {
                    pending.remove(&session);
                }
            }
        }
        for event in batch {
            if matches!(
                event.request.event_type.as_str(),
                "code.evaluate-requested" | "code.resumed"
            ) {
                pending.insert(
                    payload_field(event, "sessionId").unwrap_or_else(|| "default".into()),
                    event.clone(),
                );
            }
        }
        for (session, source) in pending {
            let mut failure = json!({
                "requestEventId":source.event_id.as_str(), "sessionId":session,
                "code":"outcome-unknown", "reason":"session was lost after a component trap; reconcile effects before retrying"
            });
            if let Some(resource_error) = &resource_error {
                failure["resourceError"] = resource_error.clone();
                failure["effectStatus"] = json!("outcome-unknown");
            }
            self.attempt_event(
                &source,
                "code.failed",
                failure,
                &format!("session-lost:{}", source.event_id.as_str()),
            )
            .await?;
        }
        Ok(())
    }

    /// Records admission before a provider can cross an effect boundary.
    pub(crate) async fn admit_attempt(
        &self,
        request: &CommittedEvent,
        provider: &str,
    ) -> Result<bool, RouterError> {
        static CLAIM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let key = format!("attempt:{}", request.event_id.as_str());
        let mut after = 0;
        loop {
            let page = self
                .events
                .query(
                    &self.stream_id,
                    &EventQuery {
                        after_sequence: Some(after),
                        event_types: vec!["activity.attempted".into()],
                        ..EventQuery::default()
                    },
                    100,
                )
                .await
                .map_err(|e| RouterError::Storage(e.to_string()))?;
            if page.is_empty() {
                break;
            }
            after = page.last().unwrap().sequence;
            if page
                .iter()
                .any(|event| event.request.causation_id.as_ref() == Some(&request.event_id))
            {
                let inherent = self
                    .registrations
                    .iter()
                    .find(|registration| registration.instance_id == provider)
                    .and_then(|registration| {
                        payload_field(request, "capability").and_then(|capability| {
                            registration.subscriptions.idempotency.get(&capability)
                        })
                    })
                    .is_some_and(|kind| kind == "inherent");
                if inherent {
                    return Ok(true);
                }
                self.attempt_event(request, "activity.unknown", json!({"requestEventId":request.event_id.as_str(),"provider":provider,"attemptId":key,"status":"unknown"}), &format!("unknown:{}",request.event_id.as_str())).await?;
                let model = request.request.event_type == "model.requested";
                let code = request.request.event_type == "code.evaluate-requested";
                self.attempt_event(request, if model { "model.failed" } else if code { "code.failed" } else { "capability.failed" }, json!({"requestEventId":request.event_id.as_str(),"call_id":payload_field(request,"call_id"),"sessionId":payload_field(request,"sessionId"),"code":"outcome-unknown","message":"admitted attempt has no committed outcome; reconciliation required"}), &format!("interrupted:{}",request.event_id.as_str())).await?;
                return Ok(false);
            }
        }
        let classification = self
            .registrations
            .iter()
            .find(|r| r.instance_id == provider)
            .and_then(|r| {
                payload_field(request, "capability")
                    .and_then(|cap| r.subscriptions.idempotency.get(&cap))
            })
            .map_or("non-idempotent", String::as_str);
        let claim = format!(
            "{}:{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            CLAIM.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let admitted = self.attempt_event(request,"activity.attempted",json!({"admissionClaim":claim,"requestEventId":request.event_id.as_str(),"provider":provider,"attemptId":key,"status":"running","jobId":payload_field(request,"jobId"),"revision":json_number(request,"revision"),"idempotency":classification,"idempotencyKey":request.request.deduplication_key,"deadlineAtMs":json_number(request,"deadlineAtMs"),"cancellation":"not-requested"}), &key).await?;
        Ok(payload_field(&admitted, "admissionClaim").as_deref() == Some(claim.as_str()))
    }

    #[allow(clippy::needless_pass_by_value)]
    async fn attempt_event(
        &self,
        source: &CommittedEvent,
        kind: &str,
        payload: Value,
        key: &str,
    ) -> Result<CommittedEvent, RouterError> {
        let request = AppendRequest {
            stream_id: self.stream_id.clone(),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: kind.into(),
            payload_schema: if kind.starts_with("activity.") {
                "pluribus.activity-attempt/1"
            } else {
                "pluribus.activity-failure/1"
            }
            .into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&payload).map_err(|e| RouterError::Storage(e.to_string()))?,
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, self.agent.id.as_str()),
            authority_id: source.request.authority_id.clone(),
            activity_id: source.request.activity_id.clone(),
            correlation_id: source.request.correlation_id.clone(),
            causation_id: Some(source.event_id.clone()),
            deduplication_key: Some(key.into()),
        };
        self.registry
            .validate(&request)
            .map_err(|e| RouterError::Storage(e.to_string()))?;
        self.events
            .append(request)
            .await
            .map_err(|e| RouterError::Storage(e.to_string()))
    }

    /// Whether a request was authorized for this provider.
    ///
    /// # Errors
    /// Returns storage errors.
    pub async fn authorized_for(
        &self,
        request: &EventId,
        provider: &str,
    ) -> Result<bool, RouterError> {
        let mut after = 0;
        loop {
            let page = self
                .events
                .query(
                    &self.stream_id,
                    &EventQuery {
                        after_sequence: Some(after),
                        event_types: vec!["policy.decision".into()],
                        ..EventQuery::default()
                    },
                    1000,
                )
                .await
                .map_err(|e| RouterError::Storage(e.to_string()))?;
            if page.is_empty() {
                return Ok(false);
            }
            for event in &page {
                if event.request.causation_id.as_ref() == Some(request) {
                    return Ok(payload_field(event, "provider").as_deref() == Some(provider));
                }
            }
            after = page.last().map_or(after, |event| event.sequence);
        }
    }

    /// Finds the terminal result for a request, so a restart resumes from the
    /// log instead of a lost stack frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream cannot be read.
    pub async fn result_for(
        &self,
        request: &EventId,
    ) -> Result<Option<CommittedEvent>, RouterError> {
        let mut projection = self.projection.lock().await;
        projection
            .refresh(self.events.as_ref(), &self.stream_id, 1000)
            .await?;
        let Some(id) = projection.results.get(request.as_str()) else {
            return Ok(None);
        };
        self.events
            .get(id)
            .await
            .map_err(|e| RouterError::Storage(e.to_string()))
    }
}

fn deferred_input(source: &CommittedEvent) -> Value {
    let mut envelope = json!({
        "eventId": source.event_id.as_str(),
        "eventType": source.request.event_type,
        "sequence": source.sequence,
    });
    let EventPayload::CanonicalJson(bytes) = &source.request.payload else {
        return envelope;
    };
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return envelope;
    };
    let mut compact = serde_json::Map::new();
    for key in [
        "provider",
        "externalSenderId",
        "conversationId",
        "jobId",
        "sessionId",
        "rlmSession",
        "call_id",
        "callId",
        "requestEventId",
        "revision",
    ] {
        if let Some(value) = value.get(key) {
            match value {
                Value::String(text) => {
                    compact.insert(key.into(), Value::String(text.chars().take(256).collect()));
                }
                Value::Number(_) | Value::Bool(_) => {
                    compact.insert(key.into(), value.clone());
                }
                _ => {}
            }
        }
    }
    let message = value
        .pointer("/message/text")
        .and_then(Value::as_str)
        .or_else(|| value.get("text").and_then(Value::as_str))
        .map(|text| ("text", text))
        .or_else(|| {
            value
                .pointer("/message/caption")
                .and_then(Value::as_str)
                .or_else(|| value.get("caption").and_then(Value::as_str))
                .map(|text| ("caption", text))
        });
    if let Some((field, message)) = message {
        compact.insert(
            "message".into(),
            json!({(field):message.chars().take(4096).collect::<String>()}),
        );
        compact.insert(
            "source".into(),
            json!({"eventId":source.event_id.as_str(),"payloadOmitted":true}),
        );
    }
    envelope["payload"] = Value::Object(compact);
    envelope
}

#[derive(Default)]
struct DispatchProjection {
    through: u64,
    results: BTreeMap<String, EventId>,
    jobs: BTreeMap<(String, String), (i64, String)>,
    observations: BTreeMap<String, BTreeSet<String>>,
}

impl DispatchProjection {
    async fn refresh(
        &mut self,
        events: &dyn EventStore,
        stream: &StreamId,
        page_size: usize,
    ) -> Result<(), RouterError> {
        loop {
            let page = events
                .query(
                    stream,
                    &EventQuery {
                        after_sequence: Some(self.through),
                        event_types: [
                            "cognition.checkpoint",
                            "cognition.job-updated",
                            "cognition.observation-associated",
                            "code.completed",
                            "code.failed",
                            "code.yielded",
                            "model.completed",
                            "model.failed",
                            "capability.completed",
                            "capability.failed",
                            "capability.denied",
                            "capability.timed-out",
                            "capability.cancelled",
                        ]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                        ..EventQuery::default()
                    },
                    page_size.max(1),
                )
                .await
                .map_err(|e| RouterError::Storage(e.to_string()))?;
            if page.is_empty() {
                return Ok(());
            }
            for event in page {
                self.through = event.sequence;
                match event.request.event_type.as_str() {
                    "cognition.checkpoint"
                    | "cognition.job-updated"
                    | "cognition.observation-associated" => self.update_cognition(&event)?,
                    _ => {
                        if let Some(request) = &event.request.causation_id {
                            self.results
                                .entry(request.as_str().into())
                                .or_insert_with(|| event.event_id.clone());
                        }
                        if let Some(request) = payload_field(&event, "requestEventId") {
                            self.results.entry(request).or_insert(event.event_id);
                        }
                    }
                }
            }
        }
    }

    fn update_cognition(&mut self, event: &CommittedEvent) -> Result<(), RouterError> {
        let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
            return Ok(());
        };
        let value: Value =
            serde_json::from_slice(bytes).map_err(|e| RouterError::Storage(e.to_string()))?;
        let actor = event.request.actor.id.as_str();
        if event.request.actor.kind != PrincipalKind::Component {
            return Ok(());
        }
        match event.request.event_type.as_str() {
            "cognition.job-updated" => {
                if let Some(id) = value["job"]["id"].as_str() {
                    self.update_job(actor, id, &value["job"]);
                }
            }
            "cognition.observation-associated" => {
                if let Some(id) = value["observation"]["id"].as_str() {
                    self.update_observation(actor, id, &value["observation"]);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn update_job(&mut self, actor: &str, id: &str, job: &Value) {
        self.jobs.insert(
            (actor.into(), id.into()),
            (
                job["revision"].as_i64().unwrap_or(-1),
                job["status"].as_str().unwrap_or("").into(),
            ),
        );
    }

    fn update_observation(&mut self, actor: &str, id: &str, observation: &Value) {
        let pending = self.observations.entry(actor.into()).or_default();
        if observation["status"] == "pending" {
            pending.insert(id.into());
        } else {
            pending.remove(id);
        }
    }
}

fn json_number(event: &CommittedEvent, field: &str) -> Option<i64> {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        return None;
    };
    let value: Value = serde_json::from_slice(bytes).ok()?;
    value.get(field)?.as_i64()
}

fn denial_reason(reason: &DenialReason) -> String {
    match reason {
        DenialReason::MissingGrant => "no grant for this capability".into(),
        DenialReason::ConstraintMismatch => "constraints reject the request".into(),
        DenialReason::Expired => "authority expired".into(),
        DenialReason::DepthExceeded => "recursion depth exhausted".into(),
        DenialReason::PolicyError(message) => format!("policy error: {message}"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouterError {
    UnknownInstance(String),
    Storage(String),
}

impl fmt::Display for RouterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownInstance(id) => write!(formatter, "unknown instance: {id}"),
            Self::Storage(message) => write!(formatter, "router storage failed: {message}"),
        }
    }
}

impl Error for RouterError {}
