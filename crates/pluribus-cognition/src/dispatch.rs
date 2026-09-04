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
use pluribus_plugin_package::ComponentManifest;
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

        let decision = authority.decide(
            &name,
            &provider,
            arguments.to_string().as_bytes(),
            &self.constraints,
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

    pub(crate) async fn timers(&self, page_size: usize) -> Result<Vec<PendingTimer>, RouterError> {
        let mut projection = self.projection.lock().await;
        projection
            .refresh(self.events.as_ref(), &self.stream_id, page_size)
            .await?;
        Ok(projection.timers())
    }
}

/// Timer requests a plugin proposed and the core has yet to answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTimer {
    pub request: EventId,
    pub due_at_ms: i64,
    pub instance_id: String,
}

/// Reads timer requests without a subsequent firing or cancellation.
///
/// `page_size` bounds each log read, not the number of pending timers.
/// Zero uses one event per page. Results retain request sequence order.
///
/// # Errors
///
/// Returns an error when the stream cannot be read.
pub async fn pending_timers(
    events: &dyn EventStore,
    stream_id: &StreamId,
    page_size: usize,
) -> Result<Vec<PendingTimer>, RouterError> {
    let mut projection = DispatchProjection::default();
    projection.refresh(events, stream_id, page_size).await?;
    Ok(projection.timers())
}

#[derive(Default)]
struct DispatchProjection {
    through: u64,
    results: BTreeMap<String, EventId>,
    pending: BTreeMap<String, (u64, PendingTimer)>,
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
                            "timer.set",
                            "timer.fired",
                            "timer.cancel",
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
                    "timer.set" => {
                        if let Some(due_at_ms) = json_number(&event, "dueAtMs") {
                            self.pending.insert(
                                event.event_id.as_str().into(),
                                (
                                    event.sequence,
                                    PendingTimer {
                                        request: event.event_id.clone(),
                                        due_at_ms,
                                        instance_id: event.request.actor.id.as_str().into(),
                                    },
                                ),
                            );
                        }
                    }
                    "timer.fired" | "timer.cancel" => {
                        if let Some(request) = payload_field(&event, "requestEventId")
                            && (event.request.event_type == "timer.fired"
                                || self.pending.get(&request).is_some_and(|(_, timer)| {
                                    timer.instance_id == event.request.actor.id.as_str()
                                }))
                        {
                            self.pending.remove(&request);
                        }
                    }
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

    fn timers(&self) -> Vec<PendingTimer> {
        let mut pending = self.pending.values().collect::<Vec<_>>();
        pending.sort_unstable_by_key(|(sequence, _)| *sequence);
        pending
            .into_iter()
            .map(|(_, timer)| timer.clone())
            .collect()
    }
}

fn json_number(event: &CommittedEvent, field: &str) -> Option<i64> {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        return None;
    };
    let value: Value = serde_json::from_slice(bytes).ok()?;
    value.get(field)?.as_i64()
}

/// Appends `timer.fired` for a due timer.
///
/// # Errors
///
/// Returns an error when the append fails.
pub async fn fire_timer(
    events: &dyn EventStore,
    stream_id: &StreamId,
    agent: &PrincipalRef,
    timer: &PendingTimer,
) -> Result<CommittedEvent, RouterError> {
    let payload = json!({
        "requestEventId": timer.request.as_str(),
        "dueAtMs": timer.due_at_ms,
    });
    events
        .append(AppendRequest {
            stream_id: stream_id.clone(),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "timer.fired".into(),
            payload_schema: "pluribus.timer-fired/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&payload)
                    .map_err(|error| RouterError::Storage(error.to_string()))?,
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, agent.id.as_str().to_owned()),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: Some(timer.request.clone()),
            deduplication_key: Some(format!("timer:{}", timer.request.as_str())),
        })
        .await
        .map_err(|error| RouterError::Storage(error.to_string()))
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
