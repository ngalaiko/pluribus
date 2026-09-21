//! Telegram polling through the packaged receiver source loop and real event store.

use pluribus_core::{
    BlobRef, BlobStore, CommittedEvent, DeliveryStore, EventId, EventMetadataSource, EventPayload,
    EventStore, HttpError, HttpFrame, HttpFramePage, HttpGrant, HttpRequest, HttpResponse,
    HttpService, HttpStreamProtocol, HttpStreamService, InMemoryBlobStore, InMemoryCredentialStore,
    PluginCredentialStore, PrincipalKind, PrincipalRef, SecretHandle, StateMutation,
    StateNamespace, StateStore, StreamId,
};
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    CredentialAccess, Delivery, PluginInstance, PluginServices, Principal,
    PrincipalKind as RuntimePrincipalKind, Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// One trusted text update and one update from an unlisted sender.
const UPDATES: &[u8] = br#"{"ok":true,"result":[{"update_id":7,"message":{"message_id":1,"date":1700000000,"chat":{"id":1,"type":"private"},"from":{"id":2,"is_bot":false,"first_name":"User"},"text":"hi"}},{"update_id":8,"message":{"from":{"id":3},"chat":{"id":1},"text":"ignored","document":{"file_id":"ignored"}}}]}"#;
const EMPTY: &[u8] = br#"{"ok":true,"result":[]}"#;

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

fn namespace() -> StateNamespace {
    StateNamespace::new("telegram/receive")
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

/// Serves the fixture batch once, then empty long polls. Anything but
/// `getUpdates` is recorded and refused.
struct FakeHttp {
    blobs: Arc<dyn BlobStore>,
    polls: AtomicU64,
    downloads: AtomicU64,
    requested_offsets: Mutex<Vec<Value>>,
}

impl FakeHttp {
    async fn body(&self, blob: &BlobRef) -> Value {
        let chunk = self
            .blobs
            .read(blob, 0, usize::try_from(blob.size).unwrap())
            .await
            .unwrap();
        serde_json::from_slice(&chunk.bytes).unwrap()
    }

    async fn blob(&self, bytes: &[u8]) -> BlobRef {
        let upload = self
            .blobs
            .begin_put("application/json", Some(bytes.len() as u64))
            .await
            .unwrap();
        self.blobs.write(&upload, 0, bytes).await.unwrap();
        self.blobs.finish_put(&upload).await.unwrap()
    }
}

#[async_trait::async_trait]
impl HttpService for FakeHttp {
    async fn send(&self, _: &HttpGrant, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        if !request.url.ends_with("/getUpdates") {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            return Err(HttpError::NotFound(request.url.clone()));
        }
        let payload = self.body(request.body.as_ref().unwrap()).await;
        self.requested_offsets
            .lock()
            .unwrap()
            .push(payload["offset"].clone());
        let first = self.polls.fetch_add(1, Ordering::SeqCst) == 0;
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: self.blob(if first { UPDATES } else { EMPTY }).await,
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

struct Harness {
    runtime: Runtime,
    store: Arc<SqliteEventStore<Metadata>>,
    http: Arc<FakeHttp>,
    credentials: Arc<InMemoryCredentialStore>,
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
        polls: AtomicU64::new(0),
        downloads: AtomicU64::new(0),
        requested_offsets: Mutex::new(Vec::new()),
    });
    let credentials = Arc::new(InMemoryCredentialStore::default());
    credentials
        .replace_plugin_credential(
            &SecretHandle::new("fixture"),
            &package().manifest().id,
            None,
            br#"{"token":"123456:fixture"}"#.to_vec(),
        )
        .await
        .unwrap();
    Harness {
        runtime,
        store,
        http,
        credentials,
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
                &json!({"credentials": {"bot-token": "fixture"},"trusted_senders":senders,"poll_timeout_seconds":30}),
                delivery(),
                PluginServices {
                    credentials: Some(CredentialAccess {
                        store: self.credentials.clone(),
                        provider: package().manifest().id.clone(),
                        handles: std::collections::HashSet::from(["fixture".to_owned()]),
                        exports: std::collections::BTreeMap::new(),
                    }),
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

    /// Runs startup and releases the source loop.
    async fn started(&self, senders: Value) -> PluginInstance {
        let mut receiver = self.receiver_for(senders).await;
        assert!(
            receiver.init().await.unwrap().events.is_empty(),
            "startup must not begin polling"
        );
        receiver.start();
        receiver
    }

    async fn offset(&self) -> Option<Vec<u8>> {
        StateStore::get(self.store.as_ref(), &namespace(), "updates/offset")
            .await
            .unwrap()
            .value
    }

    /// Waits for the commit that carries the fixture batch's offset.
    async fn await_batch(&self) {
        deadline("the poll batch must commit", || async {
            self.offset().await.as_deref() == Some(b"9".as_slice())
        })
        .await;
    }

    async fn pending_media(&self) -> usize {
        StateStore::scan(
            self.store.as_ref(),
            &namespace(),
            "pending-media/",
            None,
            100,
        )
        .await
        .unwrap()
        .entries
        .len()
    }

    async fn observations(&self) -> Vec<CommittedEvent> {
        self.store
            .read(&StreamId::new("personal"), 0, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.request.event_type == "observation.received")
            .collect()
    }
}

async fn deadline<F>(what: &str, mut ready: impl FnMut() -> F)
where
    F: Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

fn payload(event: &CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("JSON required")
    };
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test]
async fn activation_starts_the_poll_chain() {
    let h = harness().await;
    let mut receiver = h.receiver().await;
    assert!(receiver.init().await.unwrap().events.is_empty());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        h.http.polls.load(Ordering::SeqCst),
        0,
        "a staged instance must not poll"
    );
    receiver.start();
    // A returned update must not leave a poll-timeout-long gap.
    deadline("the loop must repeat its poll", || async {
        h.http.polls.load(Ordering::SeqCst) >= 2
    })
    .await;
}

#[tokio::test]
async fn polling_resumes_from_the_committed_offset() {
    let h = harness().await;
    StateStore::apply(
        h.store.as_ref(),
        &namespace(),
        0,
        &[StateMutation::Set {
            key: "updates/offset".into(),
            value: b"42".to_vec(),
        }],
    )
    .await
    .unwrap();
    let _receiver = h.started(json!(["2"])).await;
    deadline("the loop must poll", || async {
        h.http.polls.load(Ordering::SeqCst) >= 1
    })
    .await;
    assert_eq!(h.http.requested_offsets.lock().unwrap()[0], json!(42));
}

#[tokio::test]
async fn ignored_senders_emit_nothing_and_advance_the_offset() {
    let h = harness().await;
    let _receiver = h.started(json!([])).await;
    h.await_batch().await;
    assert!(h.observations().await.is_empty());
    assert_eq!(h.pending_media().await, 0);
}

#[tokio::test]
async fn removed_sender_pending_media_is_deleted_without_download() {
    let h = harness().await;
    StateStore::apply(h.store.as_ref(), &namespace(), 0, &[StateMutation::Set {
        key: "pending-media/00000000000000000001".into(),
        value: serde_json::to_vec(&json!({"update_id":1,"externalSenderId":"8","media":[{"status":"pending","metadata":{"file_id":"ignored"}}]})).unwrap(),
    }]).await.unwrap();
    let _receiver = h.started(json!([])).await;
    h.await_batch().await;
    assert_eq!(h.pending_media().await, 0);
    assert!(h.observations().await.is_empty());
    assert_eq!(
        h.http.downloads.load(Ordering::SeqCst),
        0,
        "a removed sender's media must not be fetched"
    );
}

#[tokio::test]
async fn mixed_poll_exposes_only_admitted_updates() {
    let h = harness().await;
    let _receiver = h.started(json!(["2"])).await;
    h.await_batch().await;
    let observations = h.observations().await;
    assert_eq!(observations.len(), 1);
    let observation = payload(&observations[0]);
    assert_eq!(observation["externalSenderId"], "2");
    assert!(observation.get("raw").is_none());
    assert_eq!(h.pending_media().await, 0);
}
