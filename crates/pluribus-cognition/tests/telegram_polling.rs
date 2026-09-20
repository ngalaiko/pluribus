//! Telegram subscriptions discard empty responses and preserve intake state.
use pluribus_cognition::{Agent, AuthorityResolver, OriginConstraints, Router};
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
    polls: tokio::sync::Semaphore,
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
            assert!(request.url.ends_with("/photos/file_2.jpg"));
            return Ok(self.response(200, b"attachment contents").await);
        }
        assert_eq!(request.method, "POST");
        if request.url.ends_with("/getFile") {
            let attempt = self.files.fetch_add(1, Ordering::SeqCst);
            if self.recover && attempt > 0 {
                return Ok(self
                    .response(
                        200,
                        br#"{"ok":true,"result":{"file_path":"photos/file_2.jpg"}}"#,
                    )
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
        self.polls.acquire().await.unwrap().forget();
        let number = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut result = match number {
            1 => {
                json!([{"update_id":47,"message":{"message_id":9,"date":1,"chat":{"id":42,"type":"private"},"from":{"id":7,"is_bot":false,"first_name":"Fixture"},"text":"pong"}}])
            }
            _ => json!([]),
        };
        if self.with_media && number == 1 {
            let message = result[0]["message"].as_object_mut().unwrap();
            // Photo sizes carry no file name or MIME type.
            message.insert(
                "photo".into(),
                json!([{"file_id":"tiny","width":90,"height":88},{"file_id":"broken","width":320,"height":312}]),
            );
            // An attachment carries its text in a caption.
            let text = message.remove("text").unwrap();
            message.insert("caption".into(), text);
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
async fn telegram_subscription_discards_empty_responses_and_receives_next_message() {
    run_polling(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_attachment_preserves_observation_offset_and_polling() {
    run_polling(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_retry_emits_one_complete_observation() {
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
        polls: tokio::sync::Semaphore::new(0),
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
    let source_clock = clock.clone();
    let runtime = runtime.with_clock(Arc::new(move || source_clock.load(Ordering::SeqCst)));
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
            &json!({"credentials": {"bot-token": "fabricated"},"trusted_senders":["7"],"poll_timeout_seconds":1}),
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
    settle_poll(&mut agent, &http, clock.load(Ordering::SeqCst)).await;
    assert_eq!(http.calls.load(Ordering::SeqCst), 1);
    assert!(
        store.read(&stream, 0, 100).await.unwrap().is_empty(),
        "empty long polls must not append events"
    );
    clock.store(1_800_000_001_000, Ordering::SeqCst);
    settle_poll(&mut agent, &http, clock.load(Ordering::SeqCst)).await;
    let observations: Vec<_> = store
        .read(&stream, 0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.request.event_type == "observation.received")
        .collect();
    assert_eq!(observations.len(), usize::from(!with_media));
    if !with_media {
        assert_eq!(payload(&observations[0])["message"]["text"], "pong");
    }
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
        settle_poll(&mut agent, &http, clock.load(Ordering::SeqCst)).await;
        assert!(
            !store
                .read(&stream, 0, 100)
                .await
                .unwrap()
                .iter()
                .any(|event| event.request.event_type == "observation.received")
        );
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
            settle_poll(&mut agent, &http, now).await;
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
            .find(|event| event.request.event_type == "observation.received")
            .unwrap();
        assert_eq!(payload(failure)["message"]["caption"], "pong");
        assert!(payload(failure)["message"].get("text").is_none());
        assert_eq!(
            failure.request.deduplication_key.as_deref(),
            Some("telegram:update:47")
        );
        assert!(!events.iter().any(|event| matches!(
            event.request.event_type.as_str(),
            "telegram.media-ready" | "telegram.media-failed"
        )));
        assert_eq!(
            payload(failure)["media"][0]["status"],
            if recover { "ready" } else { "failed" }
        );
        if recover {
            let media = &payload(failure)["media"][0];
            assert_eq!(media["kind"], "photo");
            assert_eq!(media["fileName"], "file_2.jpg");
            assert_eq!(media["blob"]["size"], 19);
            assert_eq!(
                media["blob"]["mediaType"], "image/jpeg",
                "Telegram serves files without a content type"
            );
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

async fn settle_poll(agent: &mut Agent<OriginConstraints, Deny>, http: &TelegramFixture, now: i64) {
    let before = http.calls.load(Ordering::SeqCst);
    http.polls.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while http.calls.load(Ordering::SeqCst) == before {
            tokio::task::yield_now().await;
        }
        for _ in 0..10 {
            agent.tick_wait(now).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
