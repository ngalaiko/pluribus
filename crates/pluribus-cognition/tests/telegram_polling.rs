//! Packaged Telegram polling survives an empty first response.
use pluribus_cognition::{Agent, AuthorityResolver, OriginConstraints, Router, pending_timers};
use pluribus_core::*;
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as WasmKind, Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
};

#[derive(Default)]
struct Metadata(AtomicU64, Arc<AtomicI64>);
impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }
    fn now_ms(&self) -> i64 {
        self.1.load(Ordering::SeqCst)
    }
}
struct Deny;
#[async_trait::async_trait]
impl AuthorityResolver for Deny {
    async fn resolve(&self, _: &CommittedEvent) -> Result<Authority, String> {
        Err("polling requires no capabilities".into())
    }
}
struct TelegramFixture {
    calls: AtomicU64,
    files: AtomicU64,
    with_media: bool,
    recover: bool,
    blobs: Arc<InMemoryBlobStore>,
}
impl TelegramFixture {
    async fn response(&self, status: u16, bytes: &[u8]) -> HttpResponse {
        let upload = self
            .blobs
            .begin_put("application/octet-stream", Some(bytes.len() as u64))
            .await
            .unwrap();
        self.blobs.write(&upload, 0, bytes).await.unwrap();
        HttpResponse {
            status,
            headers: vec![],
            body: self.blobs.finish_put(&upload).await.unwrap(),
            credentials_used: vec![],
        }
    }
}
#[async_trait::async_trait]
impl HttpService for TelegramFixture {
    async fn send(&self, _: &HttpGrant, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        if request.method == "GET" {
            assert!(self.recover);
            assert!(request.url.ends_with("/document.txt"));
            return Ok(self.response(200, b"attachment contents").await);
        }
        assert_eq!(request.method, "POST");
        if request.url.ends_with("/getFile") {
            let attempt = self.files.fetch_add(1, Ordering::SeqCst);
            if self.recover && attempt > 0 {
                return Ok(self
                    .response(200, br#"{"ok":true,"result":{"file_path":"document.txt"}}"#)
                    .await);
            }
            let bytes = br#"{"ok":false,"description":"temporary failure"}"#;
            let upload = self
                .blobs
                .begin_put("application/json", Some(bytes.len() as u64))
                .await
                .unwrap();
            self.blobs.write(&upload, 0, bytes).await.unwrap();
            return Ok(HttpResponse {
                status: 503,
                headers: vec![],
                body: self.blobs.finish_put(&upload).await.unwrap(),
                credentials_used: vec![],
            });
        }
        assert!(request.url.ends_with("/getUpdates"));
        let number = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut result = match number {
            1 => {
                json!([{"update_id":47,"message":{"message_id":9,"date":1,"chat":{"id":42,"type":"private"},"from":{"id":7,"is_bot":false,"first_name":"Fixture"},"text":"pong"}}])
            }
            _ => json!([]),
        };
        if self.with_media && number == 1 {
            result[0]["message"]["document"] = json!({"file_id":"broken","file_name":"file.txt"});
        }
        let bytes = serde_json::to_vec(&json!({"ok":true,"result":result})).unwrap();
        let upload = self
            .blobs
            .begin_put("application/json", Some(bytes.len() as u64))
            .await
            .unwrap();
        self.blobs.write(&upload, 0, &bytes).await.unwrap();
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: self.blobs.finish_put(&upload).await.unwrap(),
            credentials_used: vec![],
        })
    }
}
#[async_trait::async_trait]
impl HttpStreamService for TelegramFixture {
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
fn payload(event: &CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("JSON required")
    };
    serde_json::from_slice(bytes).unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn fresh_telegram_rearms_empty_poll_and_receives_next_message() {
    run_polling(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_attachment_preserves_observation_offset_and_polling() {
    run_polling(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_retry_emits_ready_without_duplicate_observation() {
    run_polling(true, true).await;
}

#[allow(clippy::too_many_lines)]
async fn run_polling(with_media: bool, recover: bool) {
    let clock = Arc::new(AtomicI64::new(1_800_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(0), clock.clone()))
            .await
            .unwrap(),
    );
    let blobs = Arc::new(InMemoryBlobStore::default());
    let http = Arc::new(TelegramFixture {
        calls: AtomicU64::new(0),
        files: AtomicU64::new(0),
        with_media,
        recover,
        blobs: blobs.clone(),
    });
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        blobs,
        store.clone(),
    )
    .unwrap();
    let principal = PrincipalRef::new(PrincipalKind::Agent, "fixture");
    let stream = StreamId::new("fixture");
    let mut agent = Agent::new(
        Router::new(
            stream.clone(),
            principal.clone(),
            store.clone(),
            Arc::new(EventTypeRegistry::core()),
            OriginConstraints,
        ),
        Deny,
        runtime,
        store.clone(),
        stream.clone(),
        principal,
    );
    agent
        .install_component(
            PluginPackage::load(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram"),
            )
            .unwrap()
            .component("receive")
            .unwrap(),
            &json!({"credential_handle":"fabricated","poll_timeout_seconds":1}),
            Delivery {
                instance_id: "telegram-1".into(),
                agent: Principal {
                    kind: WasmKind::Agent,
                    id: "fixture".into(),
                },
                actor: Principal {
                    kind: WasmKind::Agent,
                    id: "fixture".into(),
                },
                authority_id: "standing".into(),
                activity_id: "fixture".into(),
                correlation_id: "fixture".into(),
                origin_event_id: "init".into(),
                depth: 0,
                deadline_at_ms: None,
                visible_blobs: vec![],
            },
            PluginServices {
                http: Some(http.clone()),
                http_grant: Some(HttpGrant {
                    component: PrincipalRef::new(PrincipalKind::Component, "telegram-1"),
                    origins: vec!["https://api.telegram.org".into()],
                    methods: vec!["POST".into(), "GET".into()],
                    allow_http: false,
                    allow_private_network: false,
                    max_request_bytes: 1024 * 1024,
                    max_response_bytes: 1024 * 1024,
                    max_redirects: 0,
                    max_timeout_ms: 120_000,
                }),
                ..PluginServices::default()
            },
            &[],
        )
        .await
        .unwrap();
    for _ in 0..4 {
        agent.tick_wait(1_800_000_000_000).await.unwrap();
    }
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        pending_timers(store.as_ref(), &stream, 100)
            .await
            .unwrap()
            .len(),
        1,
        "empty first poll must rearm its timer"
    );
    let timers: Vec<_> = store
        .read(&stream, 0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.request.event_type == "timer.set")
        .collect();
    assert_eq!(timers.len(), 2);
    assert_eq!(payload(&timers[1])["dueAtMs"], 1_800_000_001_000_i64);
    assert_ne!(
        timers[0].request.deduplication_key,
        timers[1].request.deduplication_key
    );
    clock.store(1_800_000_001_000, Ordering::SeqCst);
    for _ in 0..4 {
        agent.tick_wait(1_800_000_001_000).await.unwrap();
    }
    let observations: Vec<_> = store
        .read(&stream, 0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.request.event_type == "observation.received")
        .collect();
    assert_eq!(observations.len(), 1);
    assert_eq!(payload(&observations[0])["message"]["text"], "pong");
    let offset = StateStore::get(
        store.as_ref(),
        &StateNamespace::new("telegram-1"),
        "updates/offset",
    )
    .await
    .unwrap();
    assert_eq!(offset.value.unwrap(), b"48");
    if with_media {
        assert_eq!(
            http.files.load(Ordering::SeqCst),
            0,
            "intake commits before downloading attachments"
        );
        clock.store(1_800_000_002_000, Ordering::SeqCst);
        for _ in 0..4 {
            agent.tick_wait(1_800_000_002_000).await.unwrap();
        }
        assert_eq!(payload(&observations[0])["media"][0]["status"], "pending");
        assert!(http.files.load(Ordering::SeqCst) > 0);
        assert!(http.calls.load(Ordering::SeqCst) > 2);
        let pending = StateStore::get(
            store.as_ref(),
            &StateNamespace::new("telegram-1"),
            "pending-media/00000000000000000047",
        )
        .await
        .unwrap()
        .value
        .unwrap();
        let pending: Value = serde_json::from_slice(&pending).unwrap();
        assert_eq!(pending["media"][0]["status"], "pending");
        assert_eq!(pending["media"][0]["attempts"], 1);
        for attempt in 1..=10 {
            let now = 1_800_000_002_000 + attempt * 31_000;
            clock.store(now, Ordering::SeqCst);
            for _ in 0..4 {
                agent.tick_wait(now).await.unwrap();
            }
        }
        assert_eq!(
            http.files.load(Ordering::SeqCst),
            if recover { 2 } else { 5 }
        );
        assert!(
            StateStore::get(
                store.as_ref(),
                &StateNamespace::new("telegram-1"),
                "pending-media/00000000000000000047"
            )
            .await
            .unwrap()
            .value
            .is_none()
        );
        let events = store.read(&stream, 0, 500).await.unwrap();
        let failure = events
            .iter()
            .find(|event| {
                event.request.event_type
                    == if recover {
                        "telegram.media-ready"
                    } else {
                        "telegram.media-failed"
                    }
            })
            .unwrap();
        assert_eq!(
            payload(failure)["observationDeduplicationKey"],
            "telegram:update:47"
        );
        assert_eq!(
            payload(failure)["media"][0]["status"],
            if recover { "ready" } else { "failed" }
        );
        if recover {
            assert_eq!(payload(failure)["media"][0]["blob"]["size"], 19);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| event.request.event_type == "observation.received")
                .count(),
            1
        );
    }
}
