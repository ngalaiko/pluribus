use super::dispatch::*;
use pluribus_core::{
    AppendRequest, Audience, Authority, AuthorityId, CapabilityName, CommittedEvent,
    ConstraintPolicy, ConstraintSet, EventId, EventMetadataSource, EventPayload, EventStore,
    EventTypeRegistry, Grant, Origin, OriginKind, PrincipalKind, PrincipalRef, StreamId,
    StreamKind,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct Metadata(AtomicU64);

impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }

    fn now_ms(&self) -> i64 {
        1_700_000_000_000
    }
}

/// Accepts every request, so tests exercise grants rather than constraints.
struct AllowAll;

impl ConstraintPolicy for AllowAll {
    fn allows(&self, _grant: &Grant, _request: &[u8]) -> Result<bool, String> {
        Ok(true)
    }
}

async fn store() -> Arc<SqliteEventStore<Metadata>> {
    Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    )
}

fn agent() -> PrincipalRef {
    PrincipalRef::new(PrincipalKind::Agent, "personal")
}

async fn append(
    store: &Arc<SqliteEventStore<Metadata>>,
    event_type: &str,
    payload: &serde_json::Value,
) -> CommittedEvent {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: event_type.into(),
            payload_schema: "test/1".into(),
            payload: EventPayload::CanonicalJson(serde_json::to_vec(payload).unwrap()),
            actor: PrincipalRef::new(PrincipalKind::Component, "rlm-1"),
            authority_id: Some(AuthorityId::new("authority-1")),
            activity_id: Some("activity-1".into()),
            correlation_id: Some("correlation-1".into()),
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap()
}

fn router(store: &Arc<SqliteEventStore<Metadata>>) -> Router<AllowAll> {
    Router::new(
        StreamId::new("personal"),
        agent(),
        Arc::clone(store) as Arc<dyn EventStore>,
        Arc::new(EventTypeRegistry::core()),
        AllowAll,
    )
}

fn authority(grants: &[&str]) -> Authority {
    let mut table: BTreeMap<CapabilityName, Vec<Grant>> = BTreeMap::new();
    for name in grants {
        let capability = CapabilityName::new(*name);
        table.insert(
            capability.clone(),
            vec![Grant {
                capability,
                provider: None,
                constraints: ConstraintSet::canonical_json(b"{}".to_vec()),
            }],
        );
    }
    Authority {
        schema: Authority::SCHEMA.into(),
        authority_id: AuthorityId::new("authority-1"),
        agent: agent(),
        origin: Origin {
            kind: OriginKind::Autonomous,
            principal: None,
            connector: None,
            conversation_id: None,
            source_event_id: EventId::new("origin-1"),
            trusted: true,
        },
        delegation_chain: Vec::new(),
        grants: table,
        audiences: Vec::<Audience>::new(),
        parent_authority: None,
        issued_at_ms: 0,
        expires_at_ms: None,
        max_depth: 4,
        current_depth: 0,
    }
}

fn capability_provider(id: &str, capabilities: &[&str]) -> Registration {
    Registration {
        instance_id: id.into(),
        subscriptions: Subscriptions {
            capabilities: capabilities.iter().map(|name| (*name).to_owned()).collect(),
            ..Subscriptions::default()
        },
    }
}

#[tokio::test]
async fn a_request_routes_by_its_payload_capability_name() {
    let store = store().await;
    let mut router = router(&store);
    router.register(capability_provider("shell-1", &["shell.execute"]));
    router.register(capability_provider("echo-1", &["system.echo"]));
    let event = append(
        &store,
        "capability.requested",
        &json!({"capability": "shell.execute", "arguments": "{}"}),
    )
    .await;

    let recipients = router.recipients(&event).await;

    assert_eq!(recipients, ["shell-1"]);
}

#[tokio::test]
async fn a_whole_stream_subscriber_receives_everything() {
    let store = store().await;
    let mut router = router(&store);
    router.register(Registration {
        instance_id: "rlm-1".into(),
        subscriptions: Subscriptions {
            whole_stream: true,
            ..Subscriptions::default()
        },
    });
    let event = append(&store, "observation.received", &json!({"text": "hi"})).await;

    assert_eq!(router.recipients(&event).await, ["rlm-1"]);
}

#[tokio::test]
async fn a_narrow_subscriber_ignores_unrelated_events() {
    let store = store().await;
    let mut router = router(&store);
    router.register(capability_provider("shell-1", &["shell.execute"]));
    let event = append(&store, "observation.received", &json!({"text": "hi"})).await;

    assert!(router.recipients(&event).await.is_empty());
}

#[tokio::test]
async fn a_granted_request_is_delivered_and_audited() {
    let store = store().await;
    let mut router = router(&store);
    router.register(capability_provider("shell-1", &["shell.execute"]));
    let event = append(
        &store,
        "capability.requested",
        &json!({"capability": "shell.execute", "arguments": "{}"}),
    )
    .await;

    let outcome = router
        .route_request(&event, &authority(&["shell.execute"]), &agent(), 1)
        .await
        .unwrap();

    assert_eq!(
        outcome,
        Routed::Deliver {
            instance_id: "shell-1".into()
        }
    );
    let audited = store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|committed| committed.request.event_type == "policy.decision")
        .collect::<Vec<_>>();
    assert_eq!(audited.len(), 1, "every decision is audited");
    assert_eq!(
        payload_field(&audited[0], "requestEventId").as_deref(),
        Some(event.event_id.as_str())
    );
}

#[tokio::test]
async fn an_ungranted_request_is_denied_and_still_audited() {
    let store = store().await;
    let mut router = router(&store);
    router.register(capability_provider("shell-1", &["shell.execute"]));
    let event = append(
        &store,
        "capability.requested",
        &json!({"capability": "shell.execute", "arguments": "{}"}),
    )
    .await;

    let outcome = router
        .route_request(&event, &authority(&[]), &agent(), 1)
        .await
        .unwrap();

    assert!(matches!(outcome, Routed::Denied { .. }), "{outcome:?}");
    let audited = store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|committed| committed.request.event_type == "policy.decision")
        .collect::<Vec<_>>();
    assert_eq!(
        audited.len(),
        1,
        "a refused attempt stays in the log, not only the ones that passed"
    );
    assert!(
        payload_field(&audited[0], "reason").is_some(),
        "the refusal records why"
    );
}

#[tokio::test]
async fn a_granted_capability_with_no_provider_is_not_delivered() {
    let store = store().await;
    let router = router(&store);
    let event = append(
        &store,
        "capability.requested",
        &json!({"capability": "shell.execute", "arguments": "{}"}),
    )
    .await;

    let outcome = router
        .route_request(&event, &authority(&["shell.execute"]), &agent(), 1)
        .await
        .unwrap();

    assert_eq!(
        outcome,
        Routed::NoProvider {
            reason: "no instance provides shell.execute".into()
        }
    );
}

#[tokio::test]
async fn a_result_is_found_by_the_request_it_answers() {
    let store = store().await;
    let router = router(&store);
    let request = append(
        &store,
        "capability.requested",
        &json!({"capability": "shell.execute"}),
    )
    .await;
    append(
        &store,
        "capability.completed",
        &json!({"requestEventId": request.event_id.as_str(), "output": "ok"}),
    )
    .await;

    let result = router.result_for(&request.event_id).await.unwrap().unwrap();

    assert_eq!(result.request.event_type, "capability.completed");
}

#[tokio::test]
async fn a_request_without_a_result_resumes_nothing() {
    let store = store().await;
    let router = router(&store);
    let request = append(
        &store,
        "capability.requested",
        &json!({"capability": "shell.execute"}),
    )
    .await;

    assert!(
        router
            .result_for(&request.event_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn registering_the_same_instance_twice_replaces_it() {
    let store = store().await;
    let mut router = router(&store);
    router.register(capability_provider("shell-1", &["shell.execute"]));
    router.register(capability_provider("shell-1", &["shell.other"]));

    assert_eq!(router.registrations().len(), 1);
    assert_eq!(
        router.registrations()[0].subscriptions.capabilities,
        ["shell.other"]
    );
}

#[tokio::test]
async fn a_blob_payload_never_routes() {
    let store = store().await;
    let mut router = router(&store);
    router.register(capability_provider("shell-1", &["shell.execute"]));
    let event = store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "capability.requested".into(),
            payload_schema: "test/1".into(),
            payload: EventPayload::Blob(pluribus_core::BlobRef {
                algorithm: "sha256".into(),
                digest: "a".repeat(64),
                size: 1,
                media_type: "application/json".into(),
            }),
            actor: PrincipalRef::new(PrincipalKind::Component, "rlm-1"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap();

    assert!(
        router.recipients(&event).await.is_empty(),
        "routing must not depend on fetching blob content"
    );
}

#[tokio::test]
async fn route_uses_the_capability_provider_and_object_arguments() {
    let store = store().await;
    let mut router = router(&store);
    router.register(Registration {
        instance_id: "cognition".into(),
        subscriptions: Subscriptions {
            whole_stream: true,
            ..Subscriptions::default()
        },
    });
    router.register(Registration {
        instance_id: "shell-1".into(),
        subscriptions: Subscriptions {
            capabilities: vec!["shell.execute".into()],
            ..Subscriptions::default()
        },
    });
    let mut authority = authority(&["shell.execute"]);
    authority
        .grants
        .get_mut(&CapabilityName::new("shell.execute"))
        .unwrap()[0]
        .provider = Some(PrincipalRef::new(PrincipalKind::Component, "shell-1"));
    let event = append(
        &store,
        "capability.requested",
        &json!({"capability":"shell.execute","arguments":{"command":"pwd"}}),
    )
    .await;
    assert_eq!(
        router
            .route_request(&event, &authority, &agent(), 0)
            .await
            .unwrap(),
        Routed::Deliver {
            instance_id: "shell-1".into()
        }
    );
}

#[tokio::test]
async fn origin_grants_follow_causation_and_confine_replies() {
    use crate::{AuthorityResolver, Connector, OriginAuthority, OriginConstraints};
    let store = store().await;
    let resolver = OriginAuthority {
        agent: agent(),
        events: Arc::clone(&store) as _,
        connectors: vec![Connector {
            provider: "telegram".into(),
            inherits_origin: false,
            ingress: "telegram-1/receive".into(),
            reply: "telegram-1/send".into(),
            reply_capabilities: vec![CapabilityName::new("telegram.send-message")],
        }],
        grants: authority(&["shell.execute"]).grants,
        max_depth: 4,
    };
    for (sender, trusted) in [("7", true), ("8", true)] {
        let mut origin=append(&store,"observation.received",&json!({"provider":"telegram","externalSenderId":sender,"conversationId":"chat:9:thread:3"})).await.request;
        origin.actor = PrincipalRef::new(PrincipalKind::Component, "telegram-1/receive");
        let origin = store.append(origin).await.unwrap();
        let mut request = append(&store, "capability.requested", &json!({}))
            .await
            .request;
        request.causation_id = Some(origin.event_id.clone());
        let request = store.append(request).await.unwrap();
        let issued = resolver.resolve(&request).await.unwrap();
        assert_eq!(issued.origin.trusted, trusted);
        assert_eq!(
            issued.permits(&CapabilityName::new("shell.execute")),
            trusted
        );
        for capability in ["telegram.edit-message", "telegram.delete-message"] {
            assert!(!issued.permits(&CapabilityName::new(capability)));
        }
        let grant = &issued.grants[&CapabilityName::new("telegram.send-message")][0];
        assert_eq!(
            grant.provider.as_ref().unwrap().id.as_str(),
            "telegram-1/send"
        );
        assert!(
            OriginConstraints
                .allows(grant, br#"{"conversation_ids":"chat:9:thread:3"}"#)
                .unwrap()
        );
        assert!(
            !OriginConstraints
                .allows(grant, br#"{"conversation_ids":"chat:10:thread:3"}"#)
                .unwrap()
        );
        assert!(
            !OriginConstraints
                .allows(grant, br#"{"conversation_ids":"chat:9"}"#)
                .unwrap()
        );
        assert!(
            !OriginConstraints
                .allows(
                    grant,
                    br#"{"chat_id":10,"conversationId":"chat:9:thread:3"}"#
                )
                .unwrap()
        );
        let mut spoof = origin.request.clone();
        spoof.actor = PrincipalRef::new(PrincipalKind::Component, "unknown");
        let spoof = store.append(spoof).await.unwrap();
        assert!(resolver.resolve(&spoof).await.is_err());
    }
}

#[tokio::test]
async fn terminal_lookup_spans_multiple_pages() {
    let store = store().await;
    let router = router(&store);
    for _ in 0..1000 {
        append(
            &store,
            "capability.completed",
            &json!({"requestEventId":"other"}),
        )
        .await;
    }
    let request = append(
        &store,
        "capability.requested",
        &json!({"capability":"system.echo"}),
    )
    .await;
    append(
        &store,
        "capability.denied",
        &json!({"requestEventId":request.event_id.as_str()}),
    )
    .await;
    assert!(
        router
            .result_for(&request.event_id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn timer_delivery_uses_the_request_actor_not_payload_target() {
    let store = store().await;
    let mut router = router(&store);
    for id in ["rlm-1", "telegram-1"] {
        router.register(Registration {
            instance_id: id.into(),
            subscriptions: Subscriptions {
                event_types: vec!["timer.fired".into()],
                ..Subscriptions::default()
            },
        });
    }
    let request = append(
        &store,
        "timer.set",
        &json!({"dueAtMs":0,"target":"telegram-1"}),
    )
    .await;
    let fired = append(
        &store,
        "timer.fired",
        &json!({"requestEventId":request.event_id.as_str(),"dueAtMs":0}),
    )
    .await;
    assert_eq!(router.recipients(&fired).await, vec!["rlm-1"]);
    assert!(router.poll("telegram-1", 0, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn completed_model_requests_are_terminal_for_dispatch() {
    let store = store().await;
    let request = append(
        &store,
        "model.requested",
        &json!({"call_id":"c","model":"test"}),
    )
    .await;
    append(
        &store,
        "model.completed",
        &json!({"requestEventId":request.event_id.as_str(),"call_id":"c"}),
    )
    .await;
    assert!(
        router(&store)
            .result_for(&request.event_id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn admitted_attempt_without_result_is_unknown_and_never_replayed() {
    let store = store().await;
    let request = append(
        &store,
        "capability.requested",
        &json!({"capability":"shell.execute"}),
    )
    .await;
    assert!(
        router(&store)
            .admit_attempt(&request, "shell-1")
            .await
            .unwrap()
    );
    assert!(
        !router(&store)
            .admit_attempt(&request, "shell-1")
            .await
            .unwrap()
    );
    let result = router(&store)
        .result_for(&request.event_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        payload_field(&result, "code").as_deref(),
        Some("outcome-unknown")
    );
}

#[tokio::test]
async fn interrupted_code_admission_reports_a_lost_cell() {
    let store = store().await;
    let request = append(
        &store,
        "code.evaluate-requested",
        &json!({"sessionId":"s","source":"state.counter++"}),
    )
    .await;
    assert!(
        router(&store)
            .admit_attempt(&request, "code")
            .await
            .unwrap()
    );
    assert!(
        !router(&store)
            .admit_attempt(&request, "code")
            .await
            .unwrap()
    );
    let result = router(&store)
        .result_for(&request.event_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.request.event_type, "code.failed");
    assert_eq!(payload_field(&result, "sessionId").as_deref(), Some("s"));
}

struct AdmissionRace {
    store: Arc<SqliteEventStore<Metadata>>,
    barrier: tokio::sync::Barrier,
}
#[async_trait::async_trait]
impl EventStore for AdmissionRace {
    async fn append(&self, r: AppendRequest) -> Result<CommittedEvent, pluribus_core::AppendError> {
        self.store.append(r).await
    }
    async fn read(
        &self,
        s: &StreamId,
        after: u64,
        n: usize,
    ) -> Result<Vec<CommittedEvent>, pluribus_core::AppendError> {
        self.store.read(s, after, n).await
    }
    async fn get(
        &self,
        id: &EventId,
    ) -> Result<Option<CommittedEvent>, pluribus_core::AppendError> {
        EventStore::get(self.store.as_ref(), id).await
    }
    async fn query(
        &self,
        s: &StreamId,
        q: &pluribus_core::EventQuery,
        n: usize,
    ) -> Result<Vec<CommittedEvent>, pluribus_core::AppendError> {
        let result = self.store.query(s, q, n).await?;
        if q.event_types == ["activity.attempted"] && result.is_empty() {
            self.barrier.wait().await;
        }
        Ok(result)
    }
}
#[tokio::test]
async fn concurrent_admission_claims_execute_once() {
    let store = store().await;
    let request = append(
        &store,
        "capability.requested",
        &json!({"capability":"shell.execute"}),
    )
    .await;
    let race = Arc::new(AdmissionRace {
        store,
        barrier: tokio::sync::Barrier::new(2),
    });
    let handles = (0..2)
        .map(|_| {
            let race = race.clone();
            let request = request.clone();
            tokio::spawn(async move {
                Router::new(
                    StreamId::new("personal"),
                    agent(),
                    race,
                    Arc::new(EventTypeRegistry::core()),
                    AllowAll,
                )
                .admit_attempt(&request, "shell")
                .await
                .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let mut owners = 0;
    for handle in handles {
        owners += usize::from(handle.await.unwrap());
    }
    assert_eq!(owners, 1);
}

struct CountedStore {
    inner: Arc<SqliteEventStore<Metadata>>,
    returned: AtomicU64,
}
#[async_trait::async_trait]
impl EventStore for CountedStore {
    async fn append(
        &self,
        request: AppendRequest,
    ) -> Result<CommittedEvent, pluribus_core::AppendError> {
        self.inner.append(request).await
    }
    async fn read(
        &self,
        stream: &StreamId,
        after: u64,
        limit: usize,
    ) -> Result<Vec<CommittedEvent>, pluribus_core::AppendError> {
        self.inner.read(stream, after, limit).await
    }
    async fn get(
        &self,
        id: &EventId,
    ) -> Result<Option<CommittedEvent>, pluribus_core::AppendError> {
        self.inner.get(id).await
    }
    async fn query(
        &self,
        stream: &StreamId,
        query: &pluribus_core::EventQuery,
        limit: usize,
    ) -> Result<Vec<CommittedEvent>, pluribus_core::AppendError> {
        let page = self.inner.query(stream, query, limit).await?;
        self.returned
            .fetch_add(page.len() as u64, Ordering::Relaxed);
        Ok(page)
    }
}
#[tokio::test]
async fn terminal_lookup_only_reads_new_events() {
    let inner = store().await;
    let counted = Arc::new(CountedStore {
        inner: inner.clone(),
        returned: AtomicU64::new(0),
    });
    let router = Router::new(
        StreamId::new("personal"),
        agent(),
        counted.clone() as _,
        Arc::new(EventTypeRegistry::core()),
        AllowAll,
    );
    for _ in 0..10 {
        append(
            &inner,
            "capability.completed",
            &json!({"requestEventId":"old"}),
        )
        .await;
    }
    router.result_for(&EventId::new("missing")).await.unwrap();
    counted.returned.store(0, Ordering::Relaxed);
    router.result_for(&EventId::new("missing")).await.unwrap();
    assert_eq!(counted.returned.load(Ordering::Relaxed), 0);
    let result = append(
        &inner,
        "capability.completed",
        &json!({"requestEventId":"missing"}),
    )
    .await;
    assert_eq!(
        router.result_for(&EventId::new("missing")).await.unwrap(),
        Some(result)
    );
    assert_eq!(counted.returned.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn admission_checks_job_updates_without_whole_engine_checkpoints() {
    let store = store().await;
    let mut router = router(&store);
    router.register(Registration {
        instance_id: "rlm-1".into(),
        subscriptions: Subscriptions {
            whole_stream: true,
            ..Default::default()
        },
    });
    let request = append(
        &store,
        "capability.requested",
        &json!({"jobId":"job-1","revision":1}),
    )
    .await;
    append(
        &store,
        "cognition.job-updated",
        &json!({"job":{"id":"job-1","revision":2,"status":"running"}}),
    )
    .await;
    assert!(!router.revision_current(&request).await.unwrap());
    append(
        &store,
        "cognition.job-updated",
        &json!({"job":{"id":"job-1","revision":1,"status":"running"}}),
    )
    .await;
    assert!(router.revision_current(&request).await.unwrap());
    append(
        &store,
        "cognition.observation-associated",
        &json!({"observation":{"id":"obs-1","status":"pending"}}),
    )
    .await;
    assert!(!router.revision_current(&request).await.unwrap());
    append(
        &store,
        "cognition.observation-associated",
        &json!({"observation":{"id":"obs-1","status":"incorporated"}}),
    )
    .await;
    assert!(router.revision_current(&request).await.unwrap());
}

#[tokio::test]
async fn resource_exhaustion_is_a_host_owned_durable_event() {
    let store = store().await;
    let router = router(&store);
    let source = append(
        &store,
        "model.requested",
        &json!({"requestEventId":"request-1","jobId":"job-1","sessionId":"session-1"}),
    )
    .await;

    let recorded = router
        .append_resource_exhaustion(
            "rlm-1",
            &source,
            1,
            json!({
                "resource":"memory",
                "currentBytes":1024,
                "requestedBytes":2048,
                "limitBytes":1536,
                "phase":"delivery"
            }),
            "not-started",
            "checkpoint-committed",
            Some("job-from-runtime".into()),
            Some("session-from-runtime".into()),
        )
        .await
        .unwrap();

    assert_eq!(recorded.request.event_type, "cognition.resource-exhausted");
    assert_eq!(recorded.request.actor.kind, PrincipalKind::Node);
    assert_eq!(recorded.request.actor.id.as_str(), "personal");
    let payload = match recorded.request.payload {
        EventPayload::CanonicalJson(bytes) => {
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        }
        EventPayload::Blob(_) => panic!("resource event must be inline"),
    };
    assert_eq!(payload["requestEventId"], "event-1");
    assert_eq!(payload["inputEventId"], "event-1");
    assert_eq!(payload["remainingAttempts"], 1);
    assert_eq!(payload["effectStatus"], "not-started");
    assert_eq!(payload["jobId"], "job-from-runtime");
    assert_eq!(payload["sessionId"], "session-from-runtime");
    assert_eq!(payload["input"]["eventId"], "event-1");
    assert_eq!(payload["input"]["eventType"], "model.requested");
}

#[tokio::test]
async fn untrusted_observations_do_not_inherit_standing_or_reply_grants() {
    use crate::{AuthorityResolver, Connector, OriginAuthority};
    let store = store().await;
    let resolver = OriginAuthority {
        agent: agent(),
        events: Arc::clone(&store) as _,
        connectors: vec![Connector {
            provider: "github".into(),
            inherits_origin: false,
            ingress: "github/receive".into(),
            reply: "github/send".into(),
            reply_capabilities: vec![CapabilityName::new("github.reply")],
        }],
        grants: authority(&["shell.execute"]).grants,
        max_depth: 4,
    };
    let mut request = append(&store,"observation.received",&json!({"provider":"github","trusted":false,"externalSenderId":"7","conversationId":"github:me/repo"})).await.request;
    request.actor = PrincipalRef::new(PrincipalKind::Component, "github/receive");
    let origin = store.append(request).await.unwrap();
    let issued = resolver.resolve(&origin).await.unwrap();
    assert!(!issued.origin.trusted);
    assert!(issued.grants.is_empty());
    assert!(issued.audiences.is_empty());
}

#[tokio::test]
async fn deferred_photo_preserves_caption_and_original_reference() {
    let store = store().await;
    let router = router(&store);
    let source=append(&store,"observation.received",&json!({"provider":"telegram","conversationId":"chat","externalSenderId":"u","message":{"caption":"x".repeat(5000)}})).await;
    let deferred = router
        .resource_exhaustion_request(
            "cognition",
            &source,
            0,
            json!({}),
            "not-started",
            "committed",
            None,
            None,
        )
        .unwrap();
    let EventPayload::CanonicalJson(bytes) = deferred.payload else {
        panic!("inline")
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["input"]["payload"]["message"]["caption"]
            .as_str()
            .map(str::len),
        Some(4096)
    );
    assert_eq!(
        value["input"]["payload"]["source"]["eventId"],
        source.event_id.as_str()
    );
    assert_eq!(value["input"]["payload"]["source"]["payloadOmitted"], true);
}

#[test]
fn origin_constraints_support_declared_selectors_without_plugin_names() {
    let grant = Grant {
        capability: CapabilityName::new("vendor.search"),
        provider: None,
        constraints: ConstraintSet::canonical_json(br#"{"tenants":["team:7"]}"#.to_vec()),
    };
    assert!(
        crate::OriginConstraints
            .allows(&grant, br#"{"tenants":"team:7"}"#)
            .unwrap()
    );
    assert!(
        !crate::OriginConstraints
            .allows(&grant, br#"{"tenants":"team:8"}"#)
            .unwrap()
    );
}

#[tokio::test]
async fn routing_projects_constraints_from_the_selected_provider_contract() {
    use pluribus_plugin_package::{ConstraintBinding, ConstraintPart};
    let store = store().await;
    let mut router = Router::new(
        StreamId::new("personal"),
        agent(),
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::new(EventTypeRegistry::core()),
        crate::OriginConstraints,
    );
    let binding = ConstraintBinding {
        parts: vec![
            ConstraintPart {
                pointer: "/destination".into(),
                prefix: "room:".into(),
                optional: false,
            },
            ConstraintPart {
                pointer: "/thread".into(),
                prefix: ":thread:".into(),
                optional: true,
            },
        ],
        required: true,
    };
    router.register(Registration {
        instance_id: "external-provider".into(),
        subscriptions: Subscriptions {
            capabilities: vec!["vendor.send".into()],
            constraint_bindings: BTreeMap::from([(
                "vendor.send".into(),
                BTreeMap::from([("destinations".into(), binding)]),
            )]),
            ..Default::default()
        },
    });
    let mut auth = authority(&["vendor.send"]);
    let set_constraints = |auth: &mut Authority, value| {
        auth.grants
            .get_mut(&CapabilityName::new("vendor.send"))
            .unwrap()[0]
            .constraints = ConstraintSet::canonical_json(serde_json::to_vec(&value).unwrap());
    };
    set_constraints(&mut auth, json!({"destinations":["room:7:thread:3"]}));
    for (args, permitted) in [
        (json!({"destination":7,"thread":3}), true),
        (
            json!({"destination":8,"thread":3,"conversationId":"room:7:thread:3"}),
            false,
        ),
        (json!({"destination":7}), false),
        (json!({"destination":7,"thread":null}), false),
        (json!({"destinations":"room:7:thread:3"}), false),
    ] {
        let event = append(
            &store,
            "capability.requested",
            &json!({"capability":"vendor.send","arguments":args}),
        )
        .await;
        let result = router
            .route_request(&event, &auth, &agent(), 0)
            .await
            .unwrap();
        assert_eq!(
            matches!(result, Routed::Deliver { .. }),
            permitted,
            "{result:?}"
        );
    }
    set_constraints(&mut auth, json!({"destinations":["room:7"]}));
    let event = append(
        &store,
        "capability.requested",
        &json!({"capability":"vendor.send","arguments":{"destination":7}}),
    )
    .await;
    assert!(matches!(
        router
            .route_request(&event, &auth, &agent(), 0)
            .await
            .unwrap(),
        Routed::Deliver { .. }
    ));
    let malformed = append(
        &store,
        "capability.requested",
        &json!({"capability":"vendor.send","arguments":{"destination":7,"thread":null}}),
    )
    .await;
    assert!(matches!(
        router
            .route_request(&malformed, &auth, &agent(), 0)
            .await
            .unwrap(),
        Routed::Denied { .. }
    ));
    for constraints in [
        json!({}),
        json!({"destinations":[]}),
        json!({"destinations":["room:7"],"unknown":["x"]}),
    ] {
        set_constraints(&mut auth, constraints);
        assert!(matches!(
            router
                .route_request(&event, &auth, &agent(), 0)
                .await
                .unwrap(),
            Routed::Denied { .. }
        ));
    }
    // Replacing a provider replaces its selector contract, including wildcard grants.
    router.unregister("external-provider");
    router.register(Registration {
        instance_id: "replacement".into(),
        subscriptions: Subscriptions {
            capabilities: vec!["vendor.send".into()],
            ..Default::default()
        },
    });
    set_constraints(&mut auth, json!({"destinations":["room:7"]}));
    assert!(matches!(
        router
            .route_request(&event, &auth, &agent(), 0)
            .await
            .unwrap(),
        Routed::Denied { .. }
    ));
}

#[tokio::test]
async fn derived_observations_preserve_origin_authority_and_reject_identity_changes() {
    use crate::{AuthorityResolver, Connector, OriginAuthority};
    let store = store().await;
    let resolver = OriginAuthority {
        agent: agent(),
        events: store.clone(),
        max_depth: 4,
        connectors: vec![
            Connector {
                provider: "cli".into(),
                inherits_origin: false,
                ingress: "cli".into(),
                reply: "cli".into(),
                reply_capabilities: vec![CapabilityName::new("cli.reply")],
            },
            Connector {
                provider: "scheduler".into(),
                inherits_origin: true,
                ingress: "scheduler".into(),
                reply: String::new(),
                reply_capabilities: vec![],
            },
        ],
        grants: authority(&["shell.execute"]).grants,
    };
    for trusted in [true, false] {
        let identity =
            json!({"provider":"cli","externalSenderId":"u","conversationId":"c","trusted":trusted});
        let mut original = append(&store, "observation.received", &identity)
            .await
            .request;
        original.actor = PrincipalRef::new(PrincipalKind::Component, "cli");
        let original = store.append(original).await.unwrap();
        let mut request = append(
            &store,
            "capability.requested",
            &json!({"capability":"schedule.create"}),
        )
        .await
        .request;
        request.causation_id = Some(original.event_id.clone());
        let request = store.append(request).await.unwrap();
        for changed in [
            None,
            Some("conversationId"),
            Some("externalSenderId"),
            Some("provider"),
            Some("originEventId"),
            Some("trusted"),
        ] {
            let mut value = identity.clone();
            value["originEventId"] = json!(original.event_id.as_str());
            if let Some(field) = changed {
                value[field] = json!("forged");
            }
            let mut wake = append(&store, "observation.received", &value).await.request;
            wake.actor = PrincipalRef::new(PrincipalKind::Component, "scheduler");
            wake.causation_id = Some(request.event_id.clone());
            let wake = store.append(wake).await.unwrap();
            let resolved = resolver.resolve(&wake).await;
            // Non-true trust values are untrusted, so changing false to a string cannot expand authority.
            if changed.is_some() && !(changed == Some("trusted") && !trusted) {
                assert!(resolved.is_err(), "accepted changed {changed:?}");
            } else {
                let resolved = resolved.unwrap();
                assert_eq!(resolved.origin.source_event_id, original.event_id);
                assert_eq!(
                    resolved.permits(&CapabilityName::new("shell.execute")),
                    trusted
                );
                assert_eq!(resolved.origin.conversation_id.as_deref(), Some("c"));
            }
        }
    }
}
