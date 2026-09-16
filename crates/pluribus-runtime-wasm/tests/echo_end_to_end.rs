//! Drives the packaged echo plugin through the real runtime and store.

use pluribus_core::{
    AppendRequest, BlobStore, CursorKey, DeliveryStore, EventId, EventMetadataSource, EventPayload,
    EventStore, InMemoryBlobStore, PrincipalKind, PrincipalRef, StateNamespace, StateStore,
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

struct Metadata(AtomicU64);

impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }

    fn now_ms(&self) -> i64 {
        1_700_000_000_000
    }
}

fn package() -> PluginPackage {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/echo");
    PluginPackage::load(&path).expect("the packaged echo plugin must load")
}

fn delivery() -> Delivery {
    Delivery {
        instance_id: "echo-1".into(),
        agent: Principal {
            kind: RuntimePrincipalKind::Agent,
            id: "personal".into(),
        },
        actor: Principal {
            kind: RuntimePrincipalKind::Human,
            id: "operator".into(),
        },
        authority_id: "authority-1".into(),
        activity_id: "activity-1".into(),
        correlation_id: "correlation-1".into(),
        origin_event_id: "origin-1".into(),
        depth: 0,
        deadline_at_ms: None,
        visible_blobs: Vec::new(),
    }
}

struct Harness {
    runtime: Runtime,
    store: Arc<SqliteEventStore<Metadata>>,
}

async fn harness() -> Harness {
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
    Harness { runtime, store }
}

async fn request(
    store: &Arc<SqliteEventStore<Metadata>>,
    message: Option<&str>,
) -> pluribus_core::CommittedEvent {
    let arguments = message.map_or_else(|| json!({}), |text| json!({"message": text}));
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "capability.requested".into(),
            payload_schema: "pluribus.capability-request/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({
                    "capability": "system.echo",
                    "arguments": arguments,
                }))
                .unwrap(),
            ),
            actor: PrincipalRef::new(PrincipalKind::Component, "rlm-1"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap()
}

fn payload(event: &pluribus_core::CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("expected a JSON payload")
    };
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_loop_starts_after_ready_and_stops_cleanly() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    instance.start();
    let event = request(&harness.store, Some("run loop")).await;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        instance.handle(std::slice::from_ref(&event)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.checkpoint, event.sequence);
    assert!(
        outcome
            .events
            .iter()
            .any(|e| e.request.event_type == "capability.completed")
    );
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    instance.stop(now + 2000).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_produces_a_completion_and_advances_the_cursor() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let event = request(&harness.store, Some("hello")).await;

    let outcome = instance.handle(std::slice::from_ref(&event)).await.unwrap();

    assert_eq!(outcome.checkpoint, event.sequence);
    let completed = outcome
        .events
        .iter()
        .find(|committed| committed.request.event_type == "capability.completed")
        .expect("echo must complete the request");
    assert_eq!(
        payload(completed)["output"]["message"],
        json!("hello"),
        "the arguments come back verbatim"
    );
    assert_eq!(
        payload(completed)["requestEventId"],
        json!(event.event_id.as_str())
    );

    let cursor = CursorKey {
        stream_id: StreamId::new("personal"),
        namespace: StateNamespace::new("echo-1"),
    };
    assert_eq!(
        DeliveryStore::checkpoint(harness.store.as_ref(), &cursor)
            .await
            .unwrap(),
        event.sequence
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_component_is_the_actor_not_the_requester() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let event = request(&harness.store, Some("hello")).await;

    let outcome = instance.handle(&[event]).await.unwrap();

    let completed = &outcome.events[0];
    assert_eq!(completed.request.actor.kind, PrincipalKind::Component);
    assert_eq!(completed.request.actor.id.as_str(), "echo-1");
    assert_eq!(
        completed.request.authority_id.as_ref().unwrap().as_str(),
        "authority-1",
        "authority comes from the delivery, not from the guest"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_terminal_output_is_visible_before_the_delivery_commits() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let event = request(&harness.store, Some("hello")).await;

    instance.handle(&[event]).await.unwrap();

    let outputs = harness
        .store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|committed| committed.request.event_type == "capability.output")
        .collect::<Vec<_>>();
    assert_eq!(
        outputs.len(),
        1,
        "events::append commits progress outside the delivery transaction"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_survives_across_deliveries() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();

    let first = request(&harness.store, Some("one")).await;
    let first_outcome = instance.handle(&[first]).await.unwrap();
    let second = request(&harness.store, Some("two")).await;
    let second_outcome = instance.handle(&[second]).await.unwrap();

    assert_eq!(
        payload(&first_outcome.events[0])["invocationCount"],
        json!(1)
    );
    assert_eq!(
        payload(&second_outcome.events[0])["invocationCount"],
        json!(2),
        "the counter came back from committed state, not linear memory"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_argument_produces_a_failure_not_a_trap() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let event = request(&harness.store, None).await;

    let outcome = instance.handle(&[event]).await.unwrap();

    assert_eq!(outcome.events[0].request.event_type, "capability.failed");
    assert_eq!(
        payload(&outcome.events[0])["reason"],
        json!("message must be a string")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unrelated_event_is_ignored_but_still_checkpointed() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let event = harness
        .store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "observation.received".into(),
            payload_schema: "test/1".into(),
            payload: EventPayload::CanonicalJson(b"{}".to_vec()),
            actor: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap();

    let outcome = instance.handle(std::slice::from_ref(&event)).await.unwrap();

    assert!(outcome.events.is_empty());
    assert_eq!(
        outcome.checkpoint, event.sequence,
        "an ignored event must still advance the cursor or it redelivers forever"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_configuration_fails_init() {
    let harness = harness().await;
    let mut instance = harness
        .runtime
        .instantiate(
            package().component("").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();

    // The manifest schema allows only an object, so reach past it by handing
    // init a non-object directly through a second instance.
    instance.init().await.unwrap();
    assert!(
        harness
            .runtime
            .instantiate(
                package().component("").unwrap(),
                &json!([]),
                delivery(),
                PluginServices::default()
            )
            .await
            .is_err()
    );
}
