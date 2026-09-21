//! A component that dies mid-delivery must not take the agent loop with it.

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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A loop that outlives the call deadline below. Nested so each loop stays
/// under the interpreter's own iteration limit and the wall clock is what
/// runs out. The component cannot report this: the host kills it
/// mid-instruction, and no amount of plugin hardening removes the class.
const OUTER: usize = 1_000;
const INNER: usize = 1_000;

/// Short enough to keep the test quick, long enough that the deadline is
/// what fires rather than anything in the interpreter. Far below the
/// runtime default, so only a per-instance ceiling can produce it.
const DEADLINE: Duration = Duration::from_millis(250);

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
            grants: [(
                pluribus_core::CapabilityName::new("system.echo"),
                vec![Grant {
                    capability: pluribus_core::CapabilityName::new("system.echo"),
                    provider: None,
                    constraints: pluribus_core::ConstraintSet::canonical_json(b"{}".to_vec()),
                }],
            )]
            .into_iter()
            .collect(),
            audiences: Vec::<Audience>::new(),
            parent_authority: None,
            issued_at_ms: 0,
            expires_at_ms: None,
            max_depth: 4,
            current_depth: 0,
        })
    }
}

async fn append(store: &Arc<SqliteEventStore<Metadata>>, event_type: &str, payload: &Value) {
    append_bytes(store, event_type, serde_json::to_vec(payload).unwrap()).await;
}

async fn append_bytes(store: &Arc<SqliteEventStore<Metadata>>, event_type: &str, payload: Vec<u8>) {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: event_type.into(),
            payload_schema: "dev.pluribus.repl.cell/1".into(),
            payload: EventPayload::CanonicalJson(payload),
            actor: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            authority_id: Some(AuthorityId::new("authority-1")),
            activity_id: Some("activity-1".into()),
            correlation_id: Some("correlation-1".into()),
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap();
}

fn payload(event: &CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("expected JSON")
    };
    serde_json::from_slice(bytes).unwrap()
}

async fn events(store: &Arc<SqliteEventStore<Metadata>>) -> Vec<CommittedEvent> {
    store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap()
}

/// Everything but the events: one agent with the interpreter installed.
async fn agent_with_js(store: &Arc<SqliteEventStore<Metadata>>) -> Agent<AllowAll, NoGrants> {
    agent_with_limits(store, None).await
}

/// The runtime keeps its defaults; `limits` is what this one instance is
/// granted, so a passing test proves the override reached the guest.
async fn agent_with_limits(
    store: &Arc<SqliteEventStore<Metadata>>,
    limits: Option<RuntimeLimits>,
) -> Agent<AllowAll, NoGrants> {
    let blobs: Arc<dyn BlobStore> = Arc::new(InMemoryBlobStore::default());
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(store) as Arc<dyn StateStore>,
        Arc::clone(store) as Arc<dyn EventStore>,
        blobs,
        Arc::clone(store) as Arc<dyn DeliveryStore>,
    )
    .unwrap();
    let package = PluginPackage::load(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/rlm"),
    )
    .unwrap();
    let router = Router::new(
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
        Arc::clone(store) as Arc<dyn EventStore>,
        Arc::new(EventTypeRegistry::core()),
        AllowAll,
    );
    let mut agent = Agent::new(
        router,
        NoGrants,
        runtime,
        Arc::clone(store) as Arc<dyn EventStore>,
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
    );
    agent
        .install_component(
            package.component("repl").unwrap(),
            &json!({}),
            Delivery {
                instance_id: "code-1".into(),
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
            PluginServices {
                limits,
                ..PluginServices::default()
            },
            &[],
        )
        .await
        .unwrap();
    agent
}

async fn echo_agent(store: &Arc<SqliteEventStore<Metadata>>) -> Agent<AllowAll, NoGrants> {
    let blobs: Arc<dyn BlobStore> = Arc::new(InMemoryBlobStore::default());
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(store) as Arc<dyn StateStore>,
        Arc::clone(store) as Arc<dyn EventStore>,
        blobs,
        Arc::clone(store) as Arc<dyn DeliveryStore>,
    )
    .unwrap();
    let package = PluginPackage::load(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/echo"),
    )
    .unwrap();
    let router = Router::new(
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
        Arc::clone(store) as Arc<dyn EventStore>,
        Arc::new(EventTypeRegistry::core()),
        AllowAll,
    );
    let mut agent = Agent::new(
        router,
        NoGrants,
        runtime,
        Arc::clone(store) as Arc<dyn EventStore>,
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
    );
    agent
        .install_component(
            package.component("").unwrap(),
            &json!({}),
            Delivery {
                instance_id: "echo-1".into(),
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
            PluginServices {
                limits: Some(RuntimeLimits {
                    memory_bytes: 4 * 1024 * 1024,
                    ..RuntimeLimits::default()
                }),
                ..PluginServices::default()
            },
            &[],
        )
        .await
        .unwrap();
    agent
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trapped_session_component_fails_the_call_and_recovers() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let mut agent = agent_with_limits(
        &store,
        Some(RuntimeLimits {
            call_timeout: DEADLINE,
            ..RuntimeLimits::default()
        }),
    )
    .await;

    append(
        &store,
        "code.evaluate-requested",
        &json!({
            "sessionId":"waiting", "context":{}, "source":"return await history.read({});"
        }),
    )
    .await;
    agent.tick_wait(1).await.unwrap();
    append(&store, "code.evaluate-requested", &json!({
        "sessionId":"crashing", "context":{},
        "source":format!("await history.read({{}}); let total=0; for(let i=0;i<{OUTER};i++) {{ for(let j=0;j<{INNER};j++) {{ total+=JSON.stringify([i,j,total]).length; }} }} return total;")
    })).await;
    agent.tick_wait(1).await.unwrap();
    append(
        &store,
        "code.resumed",
        &json!({"sessionId":"crashing", "response":{"id":1,"value":[]}}),
    )
    .await;
    agent
        .tick_wait(1)
        .await
        .expect("a trap must not fail the pass");

    let recorded = events(&store).await;
    let failure = recorded
        .iter()
        .find(|event| event.request.event_type == "component.failed")
        .unwrap_or_else(|| {
            panic!(
                "the host must record the component's death: {:?}",
                recorded
                    .iter()
                    .map(|e| (&e.request.event_type, payload(e)))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(payload(failure)["instanceId"], json!("code-1"));
    assert_eq!(
        failure.request.actor.kind,
        PrincipalKind::Node,
        "the host reports the failure, not the component"
    );

    assert!(
        agent.instance_ids().contains(&"code-1".to_owned()),
        "a fresh component accepts subsequent work"
    );
    assert!(!agent.failed().contains_key("code-1"));
    assert!(
        events(&store)
            .await
            .iter()
            .any(|e| e.request.event_type == "code.failed"),
        "the waiting caller receives a terminal failure"
    );
    let failures = events(&store)
        .await
        .into_iter()
        .filter(|e| e.request.event_type == "code.failed")
        .map(|e| payload(&e)["sessionId"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(failures, vec!["crashing", "waiting"]);
    append(
        &store,
        "code.evaluate-requested",
        &json!({"sessionId":"fresh", "context":{}, "source":"return 42;"}),
    )
    .await;
    agent.tick_wait(2).await.unwrap();
    assert!(
        events(&store)
            .await
            .iter()
            .any(|e| e.request.event_type == "code.completed" && payload(e)["value"] == 42)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reported_failure_leaves_the_instance_installed() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let mut agent = agent_with_js(&store).await;

    // Not JSON, so the interpreter rejects the delivery outright. That is a
    // returned error, not a death: it stays retryable.
    append_bytes(&store, "code.evaluate-requested", b"not json".to_vec()).await;

    agent
        .tick_wait(1)
        .await
        .expect("reported failures leave the runner alive");
    drop(agent);
    let mut agent = agent_with_js(&store).await;
    let before = events(&store).await.len();
    agent.tick_wait(2).await.unwrap();
    assert_eq!(
        events(&store).await.len(),
        before,
        "backoff suppresses immediate retries"
    );
    assert!(
        events(&store)
            .await
            .iter()
            .any(|event| event.request.event_type == "component.backoff")
    );

    assert!(
        agent.failed().is_empty(),
        "a component that reports a failure is not quarantined"
    );
    assert!(agent.instance_ids().contains(&"code-1".to_owned()));
    agent.tick_wait(1001).await.unwrap();
    agent.tick_wait(1002).await.unwrap();
    assert!(
        events(&store)
            .await
            .iter()
            .any(|event| event.request.event_type == "activity.unknown")
    );
    assert!(
        events(&store)
            .await
            .iter()
            .any(|event| event.request.event_type == "component.recovered")
    );

    assert!(
        !events(&store)
            .await
            .iter()
            .any(|event| event.request.event_type == "component.failed"),
        "component.failed marks a death, not a rejected delivery"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_session_component_recovers_persisted_failure() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let mut agent = agent_with_js(&store).await;
    append(
        &store,
        "code.evaluate-requested",
        &json!({"sessionId":"lost", "context":{}, "source":"return await history.read({});"}),
    )
    .await;
    agent.tick_wait(1).await.unwrap();
    let source = events(&store)
        .await
        .into_iter()
        .find(|e| e.request.event_type == "code.evaluate-requested")
        .unwrap();
    store.append(AppendRequest {
        stream_id:StreamId::new("personal"), stream_kind:StreamKind::Agent,
        observed_at_ms:None, event_type:"component.failed".into(), payload_schema:"pluribus.component-failed/1".into(),
        payload:EventPayload::CanonicalJson(serde_json::to_vec(&json!({"instanceId":"code-1", "reason":"trap", "deliveredThrough":source.sequence})).unwrap()),
        actor:PrincipalRef::new(PrincipalKind::Node,"personal"), authority_id:None, activity_id:None, correlation_id:None, causation_id:Some(source.event_id), deduplication_key:None,
    }).await.unwrap();
    drop(agent);
    let mut agent = agent_with_js(&store).await;
    append(
        &store,
        "code.evaluate-requested",
        &json!({"sessionId":"fresh", "context":{}, "source":"return 42;"}),
    )
    .await;
    agent.tick_wait(2).await.unwrap();
    assert!(!agent.failed().contains_key("code-1"));
    assert!(
        events(&store)
            .await
            .iter()
            .any(|e| e.request.event_type == "code.completed" && payload(e)["value"] == 42)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resource_failure_survives_restart_and_isolates_following_request() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let mut agent = echo_agent(&store).await;
    append(
        &store,
        "capability.requested",
        &json!({
            "capability":"system.echo",
            "arguments":{"message":"x".repeat(8 * 1024 * 1024)}
        }),
    )
    .await;
    append(
        &store,
        "capability.requested",
        &json!({"capability":"system.echo","arguments":{"message":"small"}}),
    )
    .await;
    agent.tick_wait(1).await.unwrap();
    drop(agent);

    let mut agent = echo_agent(&store).await;
    agent.tick_wait(1001).await.unwrap();
    agent.tick_wait(3001).await.unwrap();
    agent.tick_wait(5001).await.unwrap();
    agent.tick_wait(7001).await.unwrap();
    let recorded = events(&store).await;
    assert!(recorded.iter().any(|event| {
        event.request.event_type == "cognition.resource-exhausted"
            && payload(event)["inputEventId"] == "event-1"
    }));
    agent.tick_wait(7002).await.unwrap();
    let recorded = events(&store).await;
    let small_id = recorded
        .iter()
        .find(|input| {
            input.request.event_type == "capability.requested"
                && payload(input)["arguments"]["message"] == "small"
        })
        .map(|input| input.event_id.as_str().to_owned())
        .unwrap();
    assert!(recorded.iter().any(|event| {
        event.request.event_type == "capability.completed"
            && payload(event)["requestEventId"] == small_id
    }));
}
