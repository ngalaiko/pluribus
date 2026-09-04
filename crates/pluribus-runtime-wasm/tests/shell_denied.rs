//! The shell plugin without an endpoint grant must fail, not trap.

use pluribus_core::{
    AppendRequest, BlobStore, DeliveryStore, EventId, EventMetadataSource, EventPayload,
    EventStore, InMemoryBlobStore, PrincipalKind, PrincipalRef, StateStore, StreamId, StreamKind,
};
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as RuntimePrincipalKind, Runtime,
    RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::json;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_endpoint_grant_fails_the_request_without_trapping() {
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
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/shell"),
    )
    .unwrap();

    // No stream service and no grant: the endpoint was never granted.
    let mut instance = runtime
        .instantiate(
            package.component("main").unwrap(),
            &json!({}),
            Delivery {
                instance_id: "shell-1".into(),
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
            },
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();

    let event = store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "capability.requested".into(),
            payload_schema: "pluribus.capability-request/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({
                    "capability": "shell.execute",
                    "arguments": {"command": "echo hi"},
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
        .unwrap();

    let outcome = instance.handle(&[event]).await.unwrap();

    assert_eq!(
        outcome.events.len(),
        1,
        "the requester must get a terminal result"
    );
    assert_eq!(outcome.events[0].request.event_type, "capability.failed");
    let EventPayload::CanonicalJson(bytes) = &outcome.events[0].request.payload else {
        panic!("expected JSON")
    };
    let payload: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(payload["code"], json!("permission-denied"));
}
