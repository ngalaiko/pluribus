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
async fn an_unanswered_timer_is_pending_and_fires_once() {
    let store = store().await;
    let stream = StreamId::new("personal");
    let request = append(&store, "timer.set", &json!({"dueAtMs": 500})).await;

    let pending = pending_timers(store.as_ref(), &stream, 100).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].due_at_ms, 500);
    assert_eq!(pending[0].request, request.event_id);

    fire_timer(store.as_ref(), &stream, &agent(), &pending[0])
        .await
        .unwrap();

    assert!(
        pending_timers(store.as_ref(), &stream, 100)
            .await
            .unwrap()
            .is_empty(),
        "a fired timer stops being pending, so a restart does not re-fire it"
    );
}

#[tokio::test]
async fn a_cancelled_timer_never_becomes_pending() {
    let store = store().await;
    let stream = StreamId::new("personal");
    let request = append(&store, "timer.set", &json!({"dueAtMs": 500})).await;
    append(
        &store,
        "timer.cancel",
        &json!({"requestEventId": request.event_id.as_str()}),
    )
    .await;

    assert!(
        pending_timers(store.as_ref(), &stream, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn timer_lookup_reaches_requests_beyond_settled_pages() {
    let store = store().await;
    let stream = StreamId::new("personal");
    for n in 0..105 {
        let timer = append(&store, "timer.set", &json!({"dueAtMs": n})).await;
        append(
            &store,
            if n % 2 == 0 {
                "timer.fired"
            } else {
                "timer.cancel"
            },
            &json!({"requestEventId": timer.event_id.as_str()}),
        )
        .await;
    }
    let timer = append(&store, "timer.set", &json!({"dueAtMs": 500})).await;
    let pending = pending_timers(store.as_ref(), &stream, 100).await.unwrap();
    assert_eq!(
        pending.iter().map(|t| &t.request).collect::<Vec<_>>(),
        vec![&timer.event_id]
    );
    fire_timer(store.as_ref(), &stream, &agent(), &pending[0])
        .await
        .unwrap();
    assert!(
        pending_timers(store.as_ref(), &stream, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn timer_lookup_reads_terminal_events_beyond_the_first_page() {
    let store = store().await;
    let stream = StreamId::new("personal");
    let cancelled = append(&store, "timer.set", &json!({"dueAtMs": 100})).await;
    let fired = append(&store, "timer.set", &json!({"dueAtMs": 200})).await;
    for n in 0..105 {
        append(
            &store,
            "timer.fired",
            &json!({"requestEventId": format!("other-{n}")}),
        )
        .await;
    }
    append(
        &store,
        "timer.cancel",
        &json!({"requestEventId": cancelled.event_id.as_str()}),
    )
    .await;
    append(
        &store,
        "timer.fired",
        &json!({"requestEventId": fired.event_id.as_str()}),
    )
    .await;
    assert!(
        pending_timers(store.as_ref(), &stream, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn timer_lookup_preserves_request_order_across_pages() {
    let store = store().await;
    let stream = StreamId::new("personal");
    let mut expected = Vec::new();
    for n in 0..12 {
        expected.push(
            append(&store, "timer.set", &json!({"dueAtMs": 12-n}))
                .await
                .event_id,
        );
    }
    let pending = pending_timers(store.as_ref(), &stream, 3).await.unwrap();
    assert_eq!(
        pending.into_iter().map(|t| t.request).collect::<Vec<_>>(),
        expected
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
                .allows(grant, br#"{"chat_id":9,"message_thread_id":3}"#)
                .unwrap()
        );
        assert!(
            !OriginConstraints
                .allows(grant, br#"{"chat_id":10,"message_thread_id":3}"#)
                .unwrap()
        );
        assert!(
            !OriginConstraints
                .allows(grant, br#"{"chat_id":9}"#)
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
    append(
        &store,
        "timer.set",
        &json!({"dueAtMs":0,"target":"telegram-1"}),
    )
    .await;
    let timer = pending_timers(store.as_ref(), &StreamId::new("personal"), 100)
        .await
        .unwrap()
        .remove(0);
    let fired = fire_timer(store.as_ref(), &StreamId::new("personal"), &agent(), &timer)
        .await
        .unwrap();
    assert_eq!(router.recipients(&fired).await, vec!["rlm-1"]);
    assert!(router.poll("telegram-1", 0, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn another_plugin_cannot_cancel_an_owned_timer() {
    let store = store().await;
    let timer = append(&store, "timer.set", &json!({"dueAtMs":1})).await;
    let mut cancel = append(
        &store,
        "timer.cancel",
        &json!({"requestEventId":"unrelated"}),
    )
    .await
    .request;
    cancel.actor = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
    cancel.payload = EventPayload::CanonicalJson(
        serde_json::to_vec(&json!({"requestEventId":timer.event_id.as_str()})).unwrap(),
    );
    store.append(cancel).await.unwrap();
    assert_eq!(
        pending_timers(store.as_ref(), &StreamId::new("personal"), 100)
            .await
            .unwrap()
            .len(),
        1
    );
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
async fn timer_projection_only_reads_new_events() {
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
    let request = append(&inner, "timer.set", &json!({"dueAtMs":5})).await;
    assert_eq!(router.timers(1).await.unwrap().len(), 1);
    counted.returned.store(0, Ordering::Relaxed);
    assert_eq!(router.timers(1).await.unwrap().len(), 1);
    assert_eq!(counted.returned.load(Ordering::Relaxed), 0);
    append(
        &inner,
        "timer.cancel",
        &json!({"requestEventId":request.event_id.as_str()}),
    )
    .await;
    assert!(router.timers(1).await.unwrap().is_empty());
    assert_eq!(counted.returned.load(Ordering::Relaxed), 1);
}
