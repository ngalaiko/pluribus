//! Telegram scheduling through the packaged receiver and real event store.

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
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram");
    PluginPackage::load(&path).expect("the packaged Telegram plugin must load")
}

fn delivery() -> Delivery {
    Delivery {
        instance_id: "telegram/receive".into(),
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
    http: Arc<FakeHttp>,
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
        blobs.clone(),
        Arc::clone(&store) as Arc<dyn DeliveryStore>,
    )
    .unwrap();
    let http = Arc::new(FakeHttp {
        blobs,
        calls: AtomicU64::new(0),
    });
    Harness {
        runtime,
        store,
        http,
    }
}

use pluribus_core::{
    CommittedEvent, HttpError, HttpFrame, HttpFramePage, HttpGrant, HttpRequest, HttpResponse,
    HttpService, HttpStreamProtocol, HttpStreamService,
};
use pluribus_runtime_wasm::PluginInstance;

struct FakeHttp {
    blobs: Arc<dyn BlobStore>,
    calls: AtomicU64,
}

#[async_trait::async_trait]
impl HttpService for FakeHttp {
    async fn send(&self, _: &HttpGrant, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        assert!(request.url.ends_with("/getUpdates"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bytes = br#"{"ok":true,"result":[{"update_id":7,"message":{"message_id":1,"date":1700000000,"chat":{"id":1,"type":"private"},"from":{"id":2,"is_bot":false,"first_name":"User"},"text":"hi"}},{"update_id":8,"message":{"from":{"id":3},"chat":{"id":1},"text":"ignored","document":{"file_id":"ignored"}}}]}"#;
        let upload = self
            .blobs
            .begin_put("application/json", Some(bytes.len() as u64))
            .await
            .unwrap();
        self.blobs.write(&upload, 0, bytes).await.unwrap();
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: self.blobs.finish_put(&upload).await.unwrap(),
            credentials_used: vec![],
        })
    }
}

#[async_trait::async_trait]
impl HttpStreamService for FakeHttp {
    async fn open_stream(
        &self,
        _: &HttpGrant,
        _: HttpStreamProtocol,
        _: &HttpRequest,
    ) -> Result<String, HttpError> {
        unreachable!()
    }
    async fn receive(
        &self,
        _: &HttpGrant,
        _: &str,
        _: u32,
        _: u32,
    ) -> Result<HttpFramePage, HttpError> {
        unreachable!()
    }
    async fn send_frame(&self, _: &HttpGrant, _: &str, _: &HttpFrame) -> Result<(), HttpError> {
        unreachable!()
    }
    fn close_stream(&self, _: &HttpGrant, _: &str) {
        unreachable!()
    }
}

impl Harness {
    async fn receiver(&self) -> PluginInstance {
        self.receiver_for(json!(["2"])).await
    }

    async fn receiver_for(&self, senders: Value) -> PluginInstance {
        self.runtime
            .instantiate(
                package().component("receive").unwrap(),
                &json!({"credential_handle":"fixture","trusted_senders":senders,"poll_timeout_seconds":30}),
                delivery(),
                PluginServices {
                    http: Some(self.http.clone()),
                    http_grant: Some(HttpGrant {
                        component: PrincipalRef::new(PrincipalKind::Component, "telegram/receive"),
                        origins: vec!["https://api.telegram.org".into()],
                        methods: vec!["POST".into()],
                        allow_http: false,
                        allow_private_network: false,
                        max_request_bytes: 1024 * 1024,
                        max_response_bytes: 1024 * 1024,
                        max_redirects: 0,
                        max_timeout_ms: 60_000,
                    }),
                    ..PluginServices::default()
                },
            )
            .await
            .unwrap()
    }

    async fn fire(&self, timer: &CommittedEvent) -> CommittedEvent {
        self.store
            .append(AppendRequest {
                stream_id: StreamId::new("personal"),
                stream_kind: StreamKind::Agent,
                observed_at_ms: None,
                event_type: "timer.fired".into(),
                payload_schema: "pluribus.timer-fired/1".into(),
                payload: EventPayload::CanonicalJson(
                    serde_json::to_vec(&json!({
                        "requestEventId": timer.event_id.as_str(),
                        "dueAtMs": payload(timer)["dueAtMs"],
                    }))
                    .unwrap(),
                ),
                actor: PrincipalRef::new(PrincipalKind::Node, "personal"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: None,
            })
            .await
            .unwrap()
    }
}

fn payload(event: &CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("JSON required")
    };
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test]
async fn early_poll_response_schedules_the_next_poll_immediately() {
    let h = harness().await;
    let mut receiver = h.receiver().await;
    let start = receiver.init().await.unwrap().events.remove(0);
    let fired = h.fire(&start).await;
    let now = fired.recorded_at_ms;
    let result = receiver.handle(&[fired]).await.unwrap();
    assert!(
        result
            .events
            .iter()
            .any(|e| e.request.event_type == "observation.received")
    );
    let next = result
        .events
        .iter()
        .find(|e| e.request.event_type == "timer.set")
        .unwrap();
    assert!(
        payload(next)["dueAtMs"].as_i64().unwrap() <= now,
        "a returned update must not leave a 30-second polling gap"
    );
}

#[tokio::test]
async fn restart_ignores_old_timers_and_keeps_one_polling_chain() {
    let h = harness().await;
    let mut receiver = h.receiver().await;
    let old = receiver.init().await.unwrap().events.remove(0);
    let stale = h.fire(&old).await;
    drop(receiver);
    let mut receiver = h.receiver().await;
    let current = receiver.init().await.unwrap().events.remove(0);
    let result = receiver.handle(&[stale]).await.unwrap();
    assert_eq!(
        h.http.calls.load(Ordering::SeqCst),
        0,
        "restart must invalidate the outstanding poll"
    );
    assert!(
        result.events.is_empty(),
        "stale timers must not schedule successors"
    );
    let fired = h.fire(&current).await;
    let result = receiver.handle(&[fired]).await.unwrap();
    assert_eq!(h.http.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        result
            .events
            .iter()
            .filter(|e| e.request.event_type == "timer.set")
            .count(),
        1
    );
}

#[tokio::test]
async fn ignored_senders_emit_nothing_and_advance_the_offset() {
    let h = harness().await;
    let mut receiver = h.receiver_for(json!([])).await;
    let start = receiver.init().await.unwrap().events.remove(0);
    let fired = h.fire(&start).await;
    let result = receiver.handle(&[fired]).await.unwrap();
    assert!(
        result
            .events
            .iter()
            .all(|e| e.request.event_type == "timer.set")
    );
    let namespace = pluribus_core::StateNamespace::new("telegram/receive");
    let offset = StateStore::get(h.store.as_ref(), &namespace, "updates/offset")
        .await
        .unwrap();
    assert_eq!(offset.value.as_deref(), Some(b"9".as_slice()));
    assert!(
        StateStore::scan(h.store.as_ref(), &namespace, "pending-media/", None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn removed_sender_pending_media_is_deleted_without_download() {
    let h = harness().await;
    let namespace = pluribus_core::StateNamespace::new("telegram/receive");
    StateStore::apply(h.store.as_ref(), &namespace, 0, &[pluribus_core::StateMutation::Set {
        key: "pending-media/00000000000000000001".into(),
        value: serde_json::to_vec(&json!({"update_id":1,"externalSenderId":"8","media":[{"status":"pending","metadata":{"file_id":"ignored"}}]})).unwrap(),
    }]).await.unwrap();
    let mut receiver = h.receiver_for(json!([])).await;
    let start = receiver.init().await.unwrap().events.remove(0);
    let fired = h.fire(&start).await;
    let result = receiver.handle(&[fired]).await.unwrap();
    assert!(
        result
            .events
            .iter()
            .all(|e| e.request.event_type == "timer.set")
    );
    assert!(
        StateStore::scan(h.store.as_ref(), &namespace, "pending-media/", None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(h.http.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mixed_poll_exposes_only_admitted_updates() {
    let h = harness().await;
    let mut receiver = h.receiver().await;
    let start = receiver.init().await.unwrap().events.remove(0);
    let fired = h.fire(&start).await;
    let result = receiver.handle(&[fired]).await.unwrap();
    let observations: Vec<_> = result
        .events
        .iter()
        .filter(|e| e.request.event_type == "observation.received")
        .collect();
    assert_eq!(observations.len(), 1);
    let observation = payload(observations[0]);
    assert_eq!(observation["externalSenderId"], "2");
    assert!(observation.get("raw").is_none());
    let namespace = pluribus_core::StateNamespace::new("telegram/receive");
    assert!(
        StateStore::scan(h.store.as_ref(), &namespace, "pending-media/", None, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}
