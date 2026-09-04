//! Bundle preflight, worker separation, and origin-scoped replies.
use pluribus_cognition::{Agent, ComponentInstall, OriginAuthority, OriginConstraints, Router};
use pluribus_core::*;
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as WasmKind, Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Metadata(AtomicU64);
impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }
    fn now_ms(&self) -> i64 {
        0
    }
}
struct Telegram {
    blobs: Arc<InMemoryBlobStore>,
    entered: AtomicBool,
    sent: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
}
impl Telegram {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
#[async_trait::async_trait]
impl HttpService for Telegram {
    async fn send(
        &self,
        grant: &HttpGrant,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpError> {
        let result = if request.url.ends_with("/getUpdates") {
            assert_eq!(grant.component.id.as_str(), "telegram-1/receive");
            self.entered.store(true, Ordering::SeqCst);
            let guard = self.released.lock().unwrap();
            let (released, timeout) = self
                .wake
                .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
                .unwrap();
            assert!(
                *released && !timeout.timed_out(),
                "receiver release timed out"
            );
            json!([])
        } else {
            assert!(request.url.ends_with("/sendMessage"));
            assert_eq!(grant.component.id.as_str(), "telegram-1/send");
            self.sent.store(true, Ordering::SeqCst);
            json!({"message_id":9,"chat":{"id":42}})
        };
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
impl HttpStreamService for Telegram {
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
struct Release(Arc<Telegram>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
type FixtureAgent = Agent<OriginConstraints, OriginAuthority>;
async fn setup() -> (FixtureAgent, Arc<SqliteEventStore<Metadata>>, Arc<Telegram>) {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata::default())
            .await
            .unwrap(),
    );
    let blobs = Arc::new(InMemoryBlobStore::default());
    let http = Arc::new(Telegram {
        blobs: blobs.clone(),
        entered: AtomicBool::new(false),
        sent: AtomicBool::new(false),
        released: Mutex::new(false),
        wake: Condvar::new(),
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
    let router = Router::new(
        stream.clone(),
        principal.clone(),
        store.clone(),
        Arc::new(EventTypeRegistry::core()),
        OriginConstraints,
    );
    let authority = OriginAuthority {
        agent: principal.clone(),
        events: store.clone(),
        connectors: vec![pluribus_cognition::Connector {
            provider: "telegram".into(),
            ingress: "telegram-1/receive".into(),
            reply: "telegram-1/send".into(),
            reply_capabilities: vec![CapabilityName::new("telegram.send-message")],
        }],
        grants: BTreeMap::new(),
        max_depth: 4,
    };
    (
        Agent::new(router, authority, runtime, store.clone(), stream, principal),
        store,
        http,
    )
}
fn delivery() -> Delivery {
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
    }
}
fn package() -> PluginPackage {
    PluginPackage::load(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram"),
    )
    .unwrap()
}
fn settings(http: &Arc<Telegram>) -> BTreeMap<String, ComponentInstall> {
    ["receive", "send"]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                ComponentInstall {
                    models: vec![],
                    services: PluginServices {
                        http: Some(http.clone()),
                        http_grant: Some(HttpGrant {
                            component: PrincipalRef::new(
                                PrincipalKind::Component,
                                format!("telegram-1/{name}"),
                            ),
                            origins: vec!["https://api.telegram.org".into()],
                            methods: vec!["POST".into()],
                            allow_http: false,
                            allow_private_network: false,
                            max_request_bytes: 1024 * 1024,
                            max_response_bytes: 1024 * 1024,
                            max_redirects: 0,
                            max_timeout_ms: 30_000,
                        }),
                        ..PluginServices::default()
                    },
                },
            )
        })
        .collect()
}
fn config() -> serde_json::Value {
    json!({"credential_handle":"fixture","poll_timeout_seconds":1})
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_component_preflight_failure_leaves_no_initialization_or_registration() {
    let (mut agent, store, http) = setup().await;
    let mut components = settings(&http);
    components.get_mut("send").unwrap().services.limits = Some(RuntimeLimits {
        memory_bytes: usize::MAX,
        ..RuntimeLimits::default()
    });
    assert!(
        agent
            .install_package(&package(), &config(), delivery(), components)
            .await
            .is_err()
    );
    assert!(agent.instance_ids().is_empty());
    assert!(
        store
            .read(&StreamId::new("fixture"), 0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!http.entered.load(Ordering::SeqCst));
    agent
        .install_package(&package(), &config(), delivery(), settings(&http))
        .await
        .unwrap();
    assert_eq!(
        agent.instance_ids(),
        vec!["telegram-1/receive", "telegram-1/send"]
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_receiver_does_not_block_sender_and_keeps_separate_grants_and_state() {
    let (mut agent, store, http) = setup().await;
    let _release = Release(http.clone());
    agent
        .install_package(&package(), &config(), delivery(), settings(&http))
        .await
        .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !http.entered.load(Ordering::SeqCst) && Instant::now() < until {
        agent.tick(0).await.unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(http.entered.load(Ordering::SeqCst));
    let origin=store.append(AppendRequest{stream_id:StreamId::new("fixture"),stream_kind:StreamKind::Agent,observed_at_ms:None,event_type:"observation.received".into(),payload_schema:"pluribus.observation/1".into(),payload:EventPayload::CanonicalJson(serde_json::to_vec(&json!({"provider":"telegram","externalSenderId":"7","conversationId":"chat:42","message":{"chat":{"id":42},"text":"pong"}})).unwrap()),actor:PrincipalRef::new(PrincipalKind::Component,"telegram-1/receive"),authority_id:None,activity_id:None,correlation_id:None,causation_id:None,deduplication_key:None}).await.unwrap();
    let mut request = origin.request.clone();
    request.event_type = "capability.requested".into();
    request.payload_schema = "pluribus.capability-request/1".into();
    request.actor = PrincipalRef::new(PrincipalKind::Component, "rlm-1/cognition");
    request.causation_id = Some(origin.event_id);
    request.payload = EventPayload::CanonicalJson(
        serde_json::to_vec(
            &json!({"capability":"telegram.send-message","arguments":{"chat_id":42,"text":"pong"}}),
        )
        .unwrap(),
    );
    let request = store.append(request).await.unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !http.sent.load(Ordering::SeqCst) && Instant::now() < until {
        agent.tick(0).await.unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        http.sent.load(Ordering::SeqCst),
        "sender was blocked by receiver"
    );
    assert!(!*http.released.lock().unwrap());
    assert!(
        StateStore::get(
            store.as_ref(),
            &StateNamespace::new("telegram-1/send"),
            "updates/offset"
        )
        .await
        .unwrap()
        .value
        .is_none()
    );
    http.release();
    for _ in 0..4 {
        agent.tick_wait(0).await.unwrap();
    }
    assert_eq!(
        agent
            .result_for(&request.event_id)
            .await
            .unwrap()
            .unwrap()
            .request
            .event_type,
        "capability.completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn component_cannot_reuse_a_sibling_host_grant() {
    let (mut agent, store, http) = setup().await;
    let mut components = settings(&http);
    components
        .get_mut("send")
        .unwrap()
        .services
        .http_grant
        .as_mut()
        .unwrap()
        .component = PrincipalRef::new(PrincipalKind::Component, "telegram-1/receive");
    assert!(
        agent
            .install_package(&package(), &config(), delivery(), components)
            .await
            .is_err()
    );
    assert!(
        store
            .read(&StreamId::new("fixture"), 0, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_wait_keeps_the_provider_owned_by_the_agent() {
    let (mut agent, _store, http) = setup().await;
    let _release = Release(http.clone());
    agent
        .install_package(&package(), &config(), delivery(), settings(&http))
        .await
        .unwrap();
    let waited = tokio::time::timeout(Duration::from_millis(50), agent.tick_wait(0)).await;
    assert!(waited.is_err(), "fixture provider must remain blocked");
    assert!(
        agent
            .instance_ids()
            .contains(&"telegram-1/receive".to_owned()),
        "cancelled wait lost the provider"
    );
    http.release();
    agent.tick_wait(0).await.unwrap();
    assert!(
        agent
            .instance_ids()
            .contains(&"telegram-1/receive".to_owned())
    );
}
