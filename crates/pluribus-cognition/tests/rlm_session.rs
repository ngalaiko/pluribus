//! A pinned session must keep its interpreter heap across deliveries.

use pluribus_cognition::{Agent, AuthorityResolver, Router};
use pluribus_core::{
    AppendRequest, Audience, Authority, AuthorityId, BlobStore, CommittedEvent, ConstraintPolicy,
    DeliveryStore, EventId, EventMetadataSource, EventPayload, EventStore, EventTypeRegistry,
    Grant, InMemoryBlobStore, Origin, OriginKind, PrincipalKind, PrincipalRef, StateStore,
    StreamId, StreamKind,
};
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as RuntimePrincipalKind, Runtime,
    RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
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

struct AllowAll;

impl ConstraintPolicy for AllowAll {
    fn allows(&self, _grant: &Grant, _request: &[u8]) -> Result<bool, String> {
        Ok(true)
    }
}

struct NoGrants;

#[async_trait::async_trait]
impl AuthorityResolver for NoGrants {
    async fn resolve(&self, _event: &CommittedEvent) -> Result<Authority, String> {
        Ok(Authority {
            schema: Authority::SCHEMA.into(),
            authority_id: AuthorityId::new("authority-1"),
            agent: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            origin: Origin {
                kind: OriginKind::Autonomous,
                principal: None,
                connector: None,
                conversation_id: None,
                source_event_id: EventId::new("origin-1"),
                trusted: true,
            },
            delegation_chain: Vec::new(),
            grants: BTreeMap::new(),
            audiences: Vec::<Audience>::new(),
            parent_authority: None,
            issued_at_ms: 0,
            expires_at_ms: None,
            max_depth: 4,
            current_depth: 0,
        })
    }
}

async fn append(
    store: &Arc<SqliteEventStore<Metadata>>,
    event_type: &str,
    payload: &Value,
) -> CommittedEvent {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: event_type.into(),
            payload_schema: "dev.pluribus.repl.cell/1".into(),
            payload: EventPayload::CanonicalJson(serde_json::to_vec(payload).unwrap()),
            actor: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            authority_id: Some(AuthorityId::new("authority-1")),
            activity_id: Some("activity-1".into()),
            correlation_id: Some("correlation-1".into()),
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap()
}

fn payload(event: &CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("expected JSON")
    };
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn a_cell_yields_a_request_then_resumes_with_its_result() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let blobs: Arc<dyn BlobStore> = Arc::new(InMemoryBlobStore::default());
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(&store) as Arc<dyn StateStore>,
        Arc::clone(&store) as Arc<dyn EventStore>,
        blobs,
        Arc::clone(&store) as Arc<dyn DeliveryStore>,
    )
    .unwrap();
    let package = PluginPackage::load(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/rlm"),
    )
    .unwrap();
    assert!(
        package.component("repl").unwrap().manifest().pinned_session,
        "rlm must declare a pinned session"
    );
    assert_eq!(
        package.component("repl").unwrap().manifest().imports,
        ["pluribus:plugin/runtime@3.0.0"],
        "the interpreter imports only lifecycle coordination"
    );

    let router = Router::new(
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::new(EventTypeRegistry::core()),
        AllowAll,
    );
    let mut agent = Agent::new(
        router,
        NoGrants,
        runtime,
        Arc::clone(&store) as Arc<dyn EventStore>,
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
    );
    agent
        .install_component(
            package.component("repl").unwrap(),
            &json!({}),
            Delivery {
                instance_id: "rlm-1".into(),
                agent: Principal {
                    kind: RuntimePrincipalKind::Agent,
                    id: "personal".into(),
                },
                actor: Principal {
                    kind: RuntimePrincipalKind::Agent,
                    id: "personal".into(),
                },
                authority_id: "authority-1".into(),
                activity_id: "activity-1".into(),
                correlation_id: "correlation-1".into(),
                origin_event_id: "origin-1".into(),
                depth: 0,
                deadline_at_ms: None,
                visible_blobs: Vec::new(),
            },
            PluginServices::default(),
            &[],
        )
        .await
        .unwrap();

    // The cell awaits a host request, so it must park rather than complete.
    append(
        &store,
        "code.evaluate-requested",
        &json!({
            "context": {"objective": "count rows"},
            "source": "state.rows = await history.read({after: 0}); return state.rows.length;",
        }),
    )
    .await;
    agent.tick_wait(1).await.unwrap();

    let yielded = store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.request.event_type == "code.yielded")
        .expect("the cell must yield a host request");
    assert_eq!(payload(&yielded)["method"], json!("history.read"));

    // Resuming carries the result back into the paused async function. That
    // only works if the JS heap survived the first delivery.
    append(
        &store,
        "code.resumed",
        &json!({
            "response": {"id": payload(&yielded)["id"], "value": [{"text": "a"}, {"text": "b"}]},
        }),
    )
    .await;
    agent.tick_wait(2).await.unwrap();

    let completed = store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.request.event_type == "code.completed")
        .expect("the resumed cell must complete");
    assert_eq!(
        payload(&completed)["value"],
        json!(2),
        "the cell saw the resumed value, so its suspended stack survived"
    );
    append(
        &store,
        "code.evaluate-requested",
        &json!({
            "context": {"turn": 2, "job": {"revision": 1}},
            "source":"return {rows: state.rows.length, turn: context.turn, revision: context.job?.revision, objective: context.objective ?? null};",
        }),
    ).await;
    agent.tick_wait(3).await.unwrap();
    let last = store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(last.request.event_type, "code.completed");
    assert_eq!(
        payload(&last)["value"],
        json!({"rows": 2, "turn": 2, "revision": 1, "objective": null})
    );
}
