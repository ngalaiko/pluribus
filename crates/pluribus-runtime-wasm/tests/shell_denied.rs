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

#[derive(Default)]
struct ExecutorStream {
    opens: AtomicU64,
    sent: std::sync::Mutex<Vec<Vec<u8>>>,
}
#[async_trait::async_trait]
impl pluribus_core::StreamService for ExecutorStream {
    async fn open(
        &self,
        _: &pluribus_core::StreamGrant,
    ) -> Result<String, pluribus_core::StreamError> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok("executor".into())
    }
    async fn send(&self, _: &str, bytes: &[u8]) -> Result<(), pluribus_core::StreamError> {
        self.sent.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
    async fn receive(
        &self,
        _: &str,
        _: u32,
        _: u32,
        _: &std::sync::atomic::AtomicBool,
    ) -> Result<pluribus_core::StreamPage, pluribus_core::StreamError> {
        Ok(pluribus_core::StreamPage { bytes: b"{\"status\":\"completed\",\"stdout\":\"\",\"stderr\":\"\",\"exit_code\":0,\"truncated\":false}\n".to_vec(), closed: false })
    }
    fn shutdown_write(&self, _: &str) {}
    fn close(&self, _: &str) {}
}

#[tokio::test]
async fn shell_resolves_exports_before_connecting_without_persisting_values() {
    for mode in ["granted", "ungranted", "expired"] {
        export_delivery(mode).await;
    }
}

async fn export_delivery(mode: &str) {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    use pluribus_core::PluginCredentialStore;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    store.replace_plugin_credential(&pluribus_core::SecretHandle::new("github:personal"), "dev.pluribus.github", None,
        serde_json::to_vec(&json!({"private-key":"private-fixture", "exports":{"installation-token":{
            "value":"fixture-token", "expires_at_ms": if mode == "expired" { now - 1 } else { now + 120_000 }
        }}})).unwrap()).await.unwrap();
    let stream = Arc::new(ExecutorStream::default());
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

    let mut instance = runtime
        .instantiate(
            package.component("main").unwrap(),
            &json!({"credential_exports":{"GH_TOKEN":{"provider":"dev.pluribus.github","credential":"github:personal","export":"installation-token"}}}),
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
            PluginServices {
                credentials: Some(pluribus_runtime_wasm::CredentialAccess {
                    store: store.clone(), provider: "dev.pluribus.shell".into(), handles: Default::default(),
                    exports: if mode == "ungranted" { Default::default() } else {
                        [("GH_TOKEN".into(), pluribus_runtime_wasm::CredentialExport {
                            provider: "dev.pluribus.github".into(), credential: "github:personal".into(), export: "installation-token".into(),
                        })].into_iter().collect()
                    },
                }),
                stream: Some(stream.clone()),
                stream_grant: Some(pluribus_core::StreamGrant {
                    endpoint: pluribus_core::StreamEndpoint::Unix { path: "/unused/executor.sock".into(), peer_uids: vec![1001] },
                    max_bytes: 1024 * 1024, max_timeout_ms: 30_000,
                }),
                ..PluginServices::default()
            },
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

    assert_eq!(outcome.events.len(), 1);
    let event = &outcome.events[0].request;
    let EventPayload::CanonicalJson(bytes) = &event.payload else {
        panic!("expected JSON")
    };
    assert!(!String::from_utf8_lossy(bytes).contains("fixture"));
    let sent = stream.sent.lock().unwrap();
    if mode == "granted" {
        assert_eq!(event.event_type, "capability.completed");
        assert_eq!(stream.opens.load(Ordering::Relaxed), 1);
        assert_eq!(sent.len(), 1);
        let request: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        assert_eq!(request["env"]["GH_TOKEN"], "fixture-token");
        assert_eq!(request["version"], 2);
        assert!(!String::from_utf8_lossy(&sent[0]).contains("private-fixture"));
    } else {
        assert_eq!(event.event_type, "capability.failed");
        assert_eq!(stream.opens.load(Ordering::Relaxed), 0);
        assert!(sent.is_empty());
    }
}
