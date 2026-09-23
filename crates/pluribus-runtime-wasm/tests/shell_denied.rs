//! The shell plugin without an endpoint grant must fail, not trap.

use pluribus_core::PluginCredentialStore;
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
    core_request: std::sync::Mutex<Option<Vec<u8>>>,
    core_requests: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
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
    async fn next(
        &self,
        _: &str,
        _: u32,
    ) -> Result<pluribus_core::StreamPage, pluribus_core::StreamError> {
        let count = self.sent.lock().unwrap().len();
        let scripted = self.core_requests.lock().unwrap().pop_front();
        let bytes = if let Some(bytes) = scripted {
            bytes
        } else if count == 1 {
            self.core_request.lock().unwrap().clone().unwrap_or_else(|| {
                b"{\"type\":\"core-request\",\"request\":{\"id\":1,\"operation\":{\"operation\":\"secret\",\"binding\":\"GH_TOKEN\"}}}\n".to_vec()
            })
        } else {
            b"{\"type\":\"result\",\"response\":{\"status\":\"completed\",\"stdout\":\"\",\"stderr\":\"\",\"exit_code\":0,\"truncated\":false}}\n".to_vec()
        };
        Ok(pluribus_core::StreamPage {
            bytes,
            closed: false,
        })
    }
    fn shutdown_write(&self, _: &str) {}
    fn close(&self, _: &str) {}
}

#[tokio::test]
async fn shell_keeps_secrets_out_of_requests_until_a_helper_asks_for_one() {
    for mode in [
        "granted",
        "ungranted",
        "expired",
        "attachment-visible",
        "attachment-hidden",
        "attachment-ambiguous",
        "attachment-import",
    ] {
        export_delivery(mode).await;
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep scenario setup and assertions together."
)]
async fn export_delivery(mode: &str) {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    store.replace_plugin_credential(&pluribus_core::SecretHandle::new("github:personal"), "dev.pluribus.github", None,
        serde_json::to_vec(&json!({"private-key":"private-fixture", "exports":{"installation-token":{
            "value":"fixture-token", "expires_at_ms": if mode == "expired" { now - 1 } else { now + 120_000 }
        }}})).unwrap()).await.unwrap();
    let stream = Arc::new(ExecutorStream::default());
    let blob_store = Arc::new(InMemoryBlobStore::default());
    let blobs: Arc<dyn BlobStore> = blob_store.clone();
    let visible_blobs = Vec::new();
    let mut attachments = Vec::new();
    let command = if mode == "attachment-import" {
        let content = vec![137_u8; 1024 * 1024 + 73];
        let mut operations = vec![json!({
            "operation":"attachment-open","media_type":"image/jpeg","size":content.len()
        })];
        for (index, chunk) in content.chunks(32 * 1024).enumerate() {
            operations.push(json!({
                "operation":"attachment-write","handle":"memory-upload-1",
                "offset":index * 32 * 1024,"bytes":chunk
            }));
        }
        operations.push(json!({"operation":"attachment-finish","handle":"memory-upload-1"}));
        let requests = operations
            .into_iter()
            .map(|operation| {
                serde_json::to_vec(
                    &json!({"type":"core-request","request":{"id":1,"operation":operation}}),
                )
                .unwrap()
                .into_iter()
                .chain([b'\n'])
                .collect()
            })
            .collect();
        *stream.core_requests.lock().unwrap() = requests;
        "pluribus-shell-cli upload image.jpg image/jpeg".into()
    } else if mode.starts_with("attachment-") {
        let contents = b"attachment fixture";
        let upload = blob_store
            .begin_put("text/plain", Some(contents.len() as u64))
            .await
            .unwrap();
        blob_store.write(&upload, 0, contents).await.unwrap();
        let blob = blob_store.finish_put(&upload).await.unwrap();
        let blob_json = json!({
            "algorithm": blob.algorithm,
            "digest": blob.digest,
            "size": blob.size,
            "media-type": blob.media_type,
        });
        if mode == "attachment-visible" || mode == "attachment-ambiguous" {
            attachments.push(blob_json.clone());
        }
        if mode == "attachment-ambiguous" {
            let mut conflicting = blob_json;
            conflicting["size"] = json!(contents.len() + 1);
            attachments.push(conflicting);
        }
        *stream.core_request.lock().unwrap() = Some(
            serde_json::to_vec(&json!({
                "type": "core-request",
                "request": {
                    "id": 1,
                    "operation": {
                        "operation": "attachment",
                        "digest": blob.digest,
                    }
                }
            }))
            .unwrap()
            .into_iter()
            .chain([b'\n'])
            .collect(),
        );
        format!("pluribus-shell-cli attachment {}", blob.digest)
    } else {
        "pluribus-shell-cli secret GH_TOKEN".into()
    };
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
    if mode.starts_with("attachment-") {
        let capability = package
            .component("main")
            .unwrap()
            .manifest()
            .provides
            .iter()
            .find(|capability| capability.capability == "shell.execute")
            .unwrap();
        let schema: serde_json::Value = serde_json::from_slice(
            &std::fs::read(package.root().join(&capability.arguments_schema)).unwrap(),
        )
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let args = json!({"command": command.clone()});
        assert!(
            validator.is_valid(&args),
            "public shell.execute schema rejected {args}"
        );
        let invalid = json!({"command": command.clone(), "attachments": [{"digest": "untrusted"}]});
        assert!(
            !validator.is_valid(&invalid),
            "schema accepted a fabricated reference"
        );
    }

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
                visible_blobs,
            },
            PluginServices {
                credentials: Some(pluribus_runtime_wasm::CredentialAccess {
                    store: store.clone(), provider: "dev.pluribus.shell".into(), handles: std::collections::HashSet::default(),
                    exports: if mode == "ungranted" { std::collections::BTreeMap::default() } else {
                        [("GH_TOKEN".into(), pluribus_runtime_wasm::CredentialExport {
                            provider: "dev.pluribus.github".into(), credential: "github:personal".into(), export: "installation-token".into(),
                        })].into_iter().collect()
                    },
                }),
                streams: [("default".to_owned(), pluribus_runtime_wasm::GrantedStream {
                    service: stream.clone(),
                    grant: pluribus_core::StreamGrant {
                        endpoint: pluribus_core::StreamEndpoint::Unix { path: "/unused/executor.sock".into(), peer_uids: vec![1001] },
                        max_bytes: 16 * 1024 * 1024, max_timeout_ms: 30_000, max_connections: u32::MAX,
                    },
                })].into(),
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
                    "arguments": {"command": command},
                    "attachments": attachments,
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
    let sent = stream.sent.lock().unwrap().clone();
    {
        assert_eq!(event.event_type, "capability.completed", "{event:?}");
        assert_eq!(stream.opens.load(Ordering::Relaxed), 1);
        assert_eq!(
            sent.len(),
            if mode == "attachment-import" {
                2 + (1024_usize * 1024 + 73).div_ceil(32 * 1024) + 1
            } else {
                2
            }
        );
        let request: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        assert_eq!(request["env"], json!({}));
        assert_eq!(request["version"], 3);
        assert!(!String::from_utf8_lossy(&sent[0]).contains("private-fixture"));
        assert!(!String::from_utf8_lossy(&sent[0]).contains("fixture-token"));
        let reply: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
        if mode == "attachment-import" {
            let reply: serde_json::Value = serde_json::from_slice(sent.last().unwrap()).unwrap();
            let encoded: Vec<u8> = serde_json::from_value(reply["bytes"].clone()).unwrap();
            let reference: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(reference["media_type"], "image/jpeg");
            assert_eq!(reference["size"], 1024 * 1024 + 73);
            let blob = pluribus_core::BlobRef {
                algorithm: reference["algorithm"].as_str().unwrap().to_owned(),
                digest: reference["digest"].as_str().unwrap().to_owned(),
                size: reference["size"].as_u64().unwrap(),
                media_type: reference["media_type"].as_str().unwrap().to_owned(),
            };
            let mut imported = Vec::new();
            let mut offset = 0;
            while offset < blob.size {
                let page = blob_store.read(&blob, offset, 64 * 1024).await.unwrap();
                assert!(!page.bytes.is_empty());
                offset += page.bytes.len() as u64;
                imported.extend(page.bytes);
            }
            assert_eq!(imported, vec![137; 1024 * 1024 + 73]);
            assert!(!reply["error"].is_string());
        } else if mode.starts_with("attachment-") {
            if mode == "attachment-visible" {
                assert_eq!(reply["bytes"], json!(b"attachment fixture"));
                assert!(reply["error"].is_null());
            } else if mode == "attachment-ambiguous" {
                assert!(reply["bytes"].is_null());
                assert!(reply["error"].as_str().unwrap().contains("ambiguous"));
            } else {
                assert!(reply["bytes"].is_null());
                assert!(reply["error"].as_str().unwrap().contains("not visible"));
            }
        } else if mode == "granted" {
            assert_eq!(reply["bytes"], json!(b"fixture-token"));
            assert!(reply["error"].is_null());
        } else {
            assert!(reply["bytes"].is_null());
            assert!(!reply["error"].is_null());
        }
    }
}
