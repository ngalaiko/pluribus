use super::*;
use pluribus_core::{EventMetadataSource, InMemoryBlobStore, RegistryError};
use pluribus_plugin_package::PluginPackage;
use pluribus_store_sqlite::SqliteEventStore;
use std::sync::atomic::AtomicU64;
use wit_component::ComponentEncoder;
use wit_parser::{ManglingAndAbi, Resolve};

struct Metadata(AtomicU64);

impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!(
            "event-{}",
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn now_ms(&self) -> i64 {
        1_700_000_000_000
    }
}

async fn store() -> Arc<SqliteEventStore<Metadata>> {
    Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    )
}

fn delivery() -> Delivery {
    Delivery {
        instance_id: "shell-1".into(),
        agent: Principal {
            kind: PrincipalKind::Agent,
            id: "personal".into(),
        },
        actor: Principal {
            kind: PrincipalKind::Human,
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

async fn host(emits: Vec<String>) -> HostState {
    let store = store().await;
    HostState::new(
        1024 * 1024,
        delivery(),
        Arc::new(AtomicBool::new(false)),
        HostServices {
            state: Arc::clone(&store) as Arc<dyn StateStore>,
            events: Arc::clone(&store) as Arc<dyn EventStore>,
            blobs: Arc::new(InMemoryBlobStore::default()),
            registry: Arc::new(EventTypeRegistry::core()),
            progress: Arc::new(tokio::sync::Notify::new()),
        },
        PluginServices::default(),
        emits,
    )
}

fn proposal(event_type: &str) -> types::Proposal {
    types::Proposal {
        event_type: event_type.into(),
        payload_schema: "test/1".into(),
        payload: types::Payload::Json(b"{}".to_vec()),
        idempotency_key: None,
        causation_id: None,
    }
}

#[tokio::test]
async fn a_proposal_is_stamped_with_host_owned_provenance() {
    let host = host(vec!["capability.completed".into()]).await;

    let request = host
        .append_request(
            &proposal("capability.completed"),
            Some(EventId::new("cause-1")),
            Some("derived".into()),
        )
        .unwrap();

    assert_eq!(request.stream_id.as_str(), "personal");
    assert_eq!(request.actor.kind, CorePrincipalKind::Component);
    assert_eq!(
        request.actor.id.as_str(),
        "shell-1",
        "the component is the actor, not the human who triggered it"
    );
    assert_eq!(
        request.authority_id.as_ref().unwrap().as_str(),
        "authority-1"
    );
    assert_eq!(request.activity_id.as_deref(), Some("activity-1"));
    assert_eq!(request.correlation_id.as_deref(), Some("correlation-1"));
    assert_eq!(request.causation_id.as_ref().unwrap().as_str(), "cause-1");
    assert_eq!(request.deduplication_key.as_deref(), Some("derived"));
}

#[tokio::test]
async fn a_plugin_supplied_idempotency_key_wins_over_the_derived_one() {
    let host = host(vec!["capability.completed".into()]).await;
    let mut proposal = proposal("capability.completed");
    proposal.idempotency_key = Some("call-7".into());

    let request = host
        .append_request(
            &proposal,
            Some(EventId::new("cause-1")),
            Some("derived".into()),
        )
        .unwrap();

    assert_eq!(request.deduplication_key.as_deref(), Some("call-7"));
}

#[tokio::test]
async fn an_undeclared_event_type_is_refused() {
    let host = host(vec!["capability.completed".into()]).await;

    let error = host
        .append_request(
            &proposal("capability.failed"),
            Some(EventId::new("c")),
            None,
        )
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        RegistryError::NotGranted("capability.failed".into()).to_string()
    );
}

#[tokio::test]
async fn a_core_owned_event_type_is_refused_even_under_a_wildcard() {
    let host = host(vec!["*".into()]).await;

    let error = host
        .append_request(&proposal("policy.decision"), Some(EventId::new("c")), None)
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        RegistryError::Reserved("policy.decision".into()).to_string()
    );
}

#[tokio::test]
async fn a_multi_event_batch_needs_explicit_causation() {
    let host = host(vec!["capability.completed".into()]).await;

    let error = host
        .append_request(&proposal("capability.completed"), None, None)
        .unwrap_err();

    assert!(error.to_string().contains("explicit causation"), "{error}");
}

#[test]
fn mutation_limits_are_enforced() {
    let too_many = (0..=MAX_STATE_MUTATIONS)
        .map(|index| {
            types::Mutation::Set(types::StateEntry {
                key: format!("k{index}"),
                value: vec![1],
            })
        })
        .collect::<Vec<_>>();
    let long_key = vec![types::Mutation::Set(types::StateEntry {
        key: "k".repeat(MAX_STATE_KEY_BYTES + 1),
        value: vec![1],
    })];
    let big_value = vec![types::Mutation::Set(types::StateEntry {
        key: "k".into(),
        value: vec![0; MAX_STATE_VALUE_BYTES + 1],
    })];

    assert!(
        convert_mutations(&too_many)
            .unwrap_err()
            .to_string()
            .contains("too many mutations")
    );
    assert!(
        convert_mutations(&long_key)
            .unwrap_err()
            .to_string()
            .contains("512 bytes")
    );
    assert!(
        convert_mutations(&big_value)
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
    assert!(convert_mutations(&[]).unwrap().is_empty());
}

#[test]
fn a_delete_mutation_converts() {
    let mutations = convert_mutations(&[types::Mutation::Delete("gone".into())]).unwrap();

    assert_eq!(mutations, [StateMutation::Delete { key: "gone".into() }]);
}

async fn runtime() -> (Runtime, Arc<SqliteEventStore<Metadata>>) {
    let store = store().await;
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(&store) as Arc<dyn StateStore>,
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::new(InMemoryBlobStore::default()),
        Arc::clone(&store) as Arc<dyn DeliveryStore>,
    )
    .unwrap();
    (runtime, store)
}

#[tokio::test]
async fn memory_above_the_abi_maximum_is_refused() {
    let store = store().await;
    let error = Runtime::new(
        RuntimeLimits {
            memory_bytes: MAX_MEMORY_BYTES + 1,
            ..RuntimeLimits::default()
        },
        Arc::clone(&store) as Arc<dyn StateStore>,
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::new(InMemoryBlobStore::default()),
        Arc::clone(&store) as Arc<dyn DeliveryStore>,
    );

    let Err(error) = error else {
        panic!("memory above the ABI maximum must be refused")
    };
    assert!(error.to_string().contains("exceeds the ABI maximum"));
}

/// Builds a component for the `plugin` world whose exports trap when called.
/// Enough to prove the five host imports link.
fn stub_component() -> Vec<u8> {
    let mut resolve = Resolve::default();
    let (package, _) = resolve
        .push_dir(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../wit"
        )))
        .unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    let mut module = wit_component::dummy_module(&resolve, world, ManglingAndAbi::Standard32);
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )
    .unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

#[tokio::test]
async fn every_host_import_links() {
    let (runtime, _store) = runtime().await;
    let component = stub_component();
    let mut linker = Linker::new(&runtime.ticker.engine);

    bindings::Plugin::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)
        .unwrap();
    let compiled = Component::new(&runtime.ticker.engine, &component).unwrap();

    assert!(
        linker.instantiate_pre(&compiled).is_ok(),
        "the five host imports must satisfy the component"
    );
}

#[tokio::test]
async fn a_fresh_cursor_starts_at_zero() {
    let (_runtime, store) = runtime().await;
    let cursor = CursorKey {
        stream_id: StreamId::new("personal"),
        namespace: StateNamespace::new("shell-1"),
    };

    let checkpoint = DeliveryStore::checkpoint(store.as_ref(), &cursor)
        .await
        .unwrap();

    assert_eq!(checkpoint, 0);
}

struct ByteHttp;
#[async_trait::async_trait]
impl pluribus_core::HttpService for ByteHttp {
    async fn send(
        &self,
        _: &HttpGrant,
        _: &HttpRequest,
    ) -> Result<pluribus_core::HttpResponse, HttpError> {
        unreachable!()
    }
}
#[async_trait::async_trait]
impl HttpStreamService for ByteHttp {
    async fn open_stream(
        &self,
        _: &HttpGrant,
        protocol: HttpStreamProtocol,
        _: &HttpRequest,
    ) -> Result<String, HttpError> {
        assert_eq!(format!("{protocol:?}"), "Bytes");
        Ok("bytes".into())
    }
    async fn receive(
        &self,
        _: &HttpGrant,
        _: &str,
        _: u32,
        _: u32,
    ) -> Result<pluribus_core::HttpFramePage, HttpError> {
        Ok(pluribus_core::HttpFramePage {
            frames: vec![pluribus_core::HttpFrame {
                kind: "bytes".into(),
                data: b"data: hello\n\n".to_vec(),
            }],
            closed: true,
        })
    }
    async fn send_frame(
        &self,
        _: &HttpGrant,
        _: &str,
        _: &pluribus_core::HttpFrame,
    ) -> Result<(), HttpError> {
        Err(HttpError::Unsupported(
            "SSE streams are receive-only".into(),
        ))
    }
    fn close_stream(&self, _: &HttpGrant, _: &str) {}
}
/// Records nothing; the halves' bookkeeping is what these tests check.
struct NullStream;
#[async_trait::async_trait]
impl StreamService for NullStream {
    async fn open(&self, _: &StreamGrant) -> Result<String, StreamError> {
        Ok("socket".into())
    }
    async fn receive(
        &self,
        _: &str,
        _: u32,
        _: u32,
        _: &AtomicBool,
    ) -> Result<pluribus_core::StreamPage, StreamError> {
        Ok(pluribus_core::StreamPage {
            bytes: vec![],
            closed: false,
        })
    }
    async fn send(&self, _: &str, _: &[u8]) -> Result<(), StreamError> {
        Ok(())
    }
    fn shutdown_write(&self, _: &str) {}
    fn close(&self, _: &str) {}
}
fn byte_request() -> http::Request {
    http::Request {
        method: "GET".into(),
        url: "https://example.com".into(),
        headers: vec![],
        body: None,
        credential: None,
        timeout_ms: 1000,
    }
}
/// Host-side stand-in for the borrow the guest would pass.
fn borrow<T: 'static>(handle: &Resource<T>) -> Resource<T> {
    Resource::new_borrow(handle.rep())
}
async fn byte_host() -> HostState {
    let mut host = host(vec![]).await;
    host.http = Some(Arc::new(ByteHttp));
    host.http_grant = Some(HttpGrant {
        component: PrincipalRef::new(CorePrincipalKind::Component, "shell-1"),
        origins: vec!["https://example.com".into()],
        methods: vec!["GET".into()],
        allow_http: false,
        allow_private_network: false,
        max_request_bytes: 1000,
        max_response_bytes: 1000,
        max_redirects: 0,
        max_timeout_ms: 1000,
    });
    host
}
#[tokio::test]
async fn sse_requests_unframed_bytes() {
    let mut host = byte_host().await;
    http::Host::sse(&mut host, byte_request()).await.unwrap();
}
#[tokio::test]
async fn a_reader_keeps_unread_bytes_until_the_last_chunk() {
    let mut host = byte_host().await;
    let read = http::Host::sse(&mut host, byte_request()).await.unwrap();
    let mut all = vec![];
    loop {
        let chunk = reader::HostReader::receive(&mut host, borrow(&read), 3, 1000)
            .await
            .unwrap();
        all.extend(chunk.bytes);
        if chunk.closed {
            break;
        }
    }
    assert_eq!(all, b"data: hello\n\n");
}
#[tokio::test]
async fn an_exhausted_reader_rejects_further_reads() {
    let mut host = byte_host().await;
    let read = http::Host::sse(&mut host, byte_request()).await.unwrap();
    while !reader::HostReader::receive(&mut host, borrow(&read), 64, 1000)
        .await
        .unwrap()
        .closed
    {}
    let error = reader::HostReader::receive(&mut host, borrow(&read), 64, 1000)
        .await
        .expect_err("a closed channel has no transport left");
    assert_eq!(error.code, types::ErrorCode::InvalidArgument);
}
#[tokio::test]
async fn dropping_one_half_keeps_the_transport_for_the_other() {
    let mut host = byte_host().await;
    host.stream = Some(Arc::new(NullStream));
    host.stream_grant = Some(StreamGrant {
        endpoint: pluribus_core::StreamEndpoint::Unix {
            path: "/run/endpoint.sock".into(),
            peer_uids: vec![1002],
        },
        max_bytes: 1024,
        max_timeout_ms: 1000,
    });
    host.stream_deadline = Some(Instant::now() + Duration::from_secs(1));
    let (read, write) = socket::Host::connect(&mut host).await.unwrap();
    writer::HostWriter::drop(&mut host, write).await.unwrap();
    assert_eq!(host.transports.len(), 1, "the reader still holds it");
    reader::HostReader::drop(&mut host, read).await.unwrap();
    assert!(host.transports.is_empty(), "the last half closes it");
}

#[tokio::test]
async fn event_get_cannot_read_another_agent_stream() {
    let mut host = host(vec!["memory.remembered".into()]).await;
    let mut request = host
        .append_request(
            &proposal("memory.remembered"),
            Some(EventId::new("cause")),
            Some("foreign".into()),
        )
        .unwrap();
    request.stream_id = StreamId::new("family");
    let event = host.event_store.append(request).await.unwrap();
    assert!(
        events::Host::get(&mut host, event.event_id.as_str().into())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn provider_http_timeout_cannot_exceed_the_lifecycle_deadline() {
    let mut config = Config::new();
    config.epoch_interruption(true);
    let engine = Engine::new(&config).unwrap();
    let mut store = Store::new(&engine, host(vec![]).await);
    prepare_call(
        &mut store,
        &RuntimeLimits {
            memory_bytes: 1024 * 1024,
            call_timeout: Duration::from_millis(100),
        },
    );
    let request = store
        .data()
        .http_request(http::Request {
            method: "GET".into(),
            url: "https://test.invalid".into(),
            headers: vec![],
            body: None,
            credential: None,
            timeout_ms: 300_000,
        })
        .unwrap();
    assert!(request.timeout_ms <= 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_telegram_subscription_requires_http_access() {
    let (runtime, _) = runtime().await;
    let package = PluginPackage::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram"),
    )
    .unwrap();
    let mut instance = runtime
        .instantiate(
            package.component("receive").unwrap(),
            &serde_json::json!({"credentials": {"bot-token": "telegram:test"},"poll_timeout_seconds":30}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    let error = instance.init().await.unwrap_err();
    assert!(error.to_string().contains("HTTP service is unavailable"));
}

#[tokio::test]
async fn explicitly_granted_connector_can_emit_observations() {
    let host = host(vec!["observation.received".into()]).await;
    let request = host
        .append_request(
            &proposal("observation.received"),
            Some(EventId::new("poll-1")),
            None,
        )
        .unwrap();
    assert_eq!(request.actor.id.as_str(), "shell-1");
}

#[tokio::test]
async fn wildcard_does_not_grant_connector_observations() {
    let host = host(vec!["*".into()]).await;
    assert!(
        host.append_request(
            &proposal("observation.received"),
            Some(EventId::new("poll-1")),
            None
        )
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_outcomes_have_distinct_deduplication_domains() {
    let (runtime, _) = runtime().await;
    let package = PluginPackage::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram"),
    )
    .unwrap();
    let mut instance = runtime
        .instantiate(
            package.component("receive").unwrap(),
            &serde_json::json!({"credentials": {"bot-token": "telegram:test"},"poll_timeout_seconds":30}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.store.data_mut().emits.push("timer.set".into());
    let initialization = guest::Outcome {
        events: vec![proposal("timer.set")],
        mutations: vec![],
        checkpoint: None,
    };
    let initialized = instance
        .commit(
            &initialization,
            Some(&EventId::new("origin-1")),
            CommitPhase::Init,
        )
        .await
        .unwrap();
    let repeated = instance
        .commit(
            &initialization,
            Some(&EventId::new("origin-1")),
            CommitPhase::Init,
        )
        .await
        .unwrap();
    assert_eq!(initialized.events[0].event_id, repeated.events[0].event_id);
    let origin = EventId::new("origin-1");
    let handled = instance
        .commit(
            &guest::Outcome {
                events: vec![proposal("timer.set")],
                mutations: vec![],
                checkpoint: Some(2),
            },
            Some(&origin),
            CommitPhase::Handle,
        )
        .await
        .unwrap();
    assert_ne!(initialized.events[0].event_id, handled.events[0].event_id);
    let stopped = instance
        .commit(
            &guest::Outcome {
                events: vec![proposal("timer.set")],
                mutations: vec![],
                checkpoint: None,
            },
            Some(&origin),
            CommitPhase::Stop,
        )
        .await
        .unwrap();
    let handled_again = instance
        .commit(
            &guest::Outcome {
                events: vec![proposal("timer.set")],
                mutations: vec![],
                checkpoint: Some(3),
            },
            Some(&origin),
            CommitPhase::Handle,
        )
        .await
        .unwrap();
    assert_ne!(stopped.events[0].event_id, handled_again.events[0].event_id);
}

struct PendingHttp;
#[async_trait::async_trait]
impl pluribus_core::HttpService for PendingHttp {
    async fn send(
        &self,
        _: &HttpGrant,
        _: &HttpRequest,
    ) -> Result<pluribus_core::HttpResponse, HttpError> {
        std::future::pending().await
    }
}
#[async_trait::async_trait]
impl HttpStreamService for PendingHttp {
    async fn open_stream(
        &self,
        _: &HttpGrant,
        _: HttpStreamProtocol,
        _: &HttpRequest,
    ) -> Result<String, HttpError> {
        std::future::pending().await
    }
    async fn receive(
        &self,
        _: &HttpGrant,
        _: &str,
        _: u32,
        _: u32,
    ) -> Result<pluribus_core::HttpFramePage, HttpError> {
        std::future::pending().await
    }
    async fn send_frame(
        &self,
        _: &HttpGrant,
        _: &str,
        _: &pluribus_core::HttpFrame,
    ) -> Result<(), HttpError> {
        unreachable!()
    }
    fn close_stream(&self, _: &HttpGrant, _: &str) {}
}

#[tokio::test]
async fn cancellation_interrupts_pending_http() {
    let mut host = byte_host().await;
    host.http = Some(Arc::new(PendingHttp));
    let cancelled = host.cancelled.clone();
    let (result, ()) = tokio::join!(
        tokio::time::timeout(
            Duration::from_millis(100),
            http::Host::send(&mut host, byte_request())
        ),
        async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancelled.store(true, Ordering::Release);
        }
    );
    assert!(matches!(
        result
            .expect("pending HTTP ignored cancellation")
            .unwrap_err()
            .code,
        types::ErrorCode::Cancelled
    ));
}

struct GatedDelivery {
    store: Arc<dyn DeliveryStore>,
    entered: Arc<tokio::sync::Notify>,
    completed: Arc<tokio::sync::Notify>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
#[async_trait::async_trait]
impl DeliveryStore for GatedDelivery {
    async fn checkpoint(&self, cursor: &CursorKey) -> Result<u64, pluribus_core::DeliveryError> {
        self.store.checkpoint(cursor).await
    }
    async fn discard(&self, cursor: &CursorKey) -> Result<(), pluribus_core::DeliveryError> {
        self.store.discard(cursor).await
    }
    async fn commit(
        &self,
        commit: DeliveryCommit,
    ) -> Result<pluribus_core::DeliveryReceipt, pluribus_core::DeliveryError> {
        let release = self.release.lock().unwrap().take();
        let store = self.store.clone();
        let entered = self.entered.clone();
        let completed = self.completed.clone();
        tokio::spawn(async move {
            if let Some(release) = release {
                entered.notify_one();
                tokio::task::spawn_blocking(move || release.recv().unwrap())
                    .await
                    .unwrap();
            }
            let result = store.commit(commit).await;
            completed.notify_one();
            result
        })
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn interrupted_lifecycle_requires_reinstantiation() {
    let (runtime, _) = runtime().await;
    let package = PluginPackage::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/telegram"),
    )
    .unwrap();
    let mut instance = runtime
        .instantiate(
            package.component("receive").unwrap(),
            &serde_json::json!({"credentials": {"bot-token": "test"},"poll_timeout_seconds":30}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(tokio::sync::Notify::new());
    let (release, wait) = std::sync::mpsc::channel();
    instance.delivery_store = Arc::new(GatedDelivery {
        store: instance.delivery_store.clone(),
        entered: entered.clone(),
        completed: completed.clone(),
        release: std::sync::Mutex::new(Some(wait)),
    });
    let mut pending = Box::pin(instance.skip(1));
    tokio::select! {
        () = entered.notified() => {},
        result = &mut pending => panic!("commit did not suspend: {result:?}"),
    }
    drop(pending);
    release.send(()).unwrap();
    completed.notified().await;
    let result = instance.handle(&[]).await;
    assert!(
        result
            .expect_err("interrupted instance was reused")
            .trapped()
    );
}

#[tokio::test]
async fn model_requests_use_the_host_model_when_omitted() {
    let mut host = host(vec!["model.requested".into()]).await;
    host.model = Some("host-model".into());
    let mut proposal = proposal("model.requested");
    proposal.payload =
        types::Payload::Json(br#"{"call_id":"c","messages":[],"tools":[]}"#.to_vec());
    let request = host
        .append_request(&proposal, Some(EventId::new("cause")), None)
        .unwrap();
    let EventPayload::CanonicalJson(bytes) = request.payload else {
        panic!("expected JSON")
    };
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["model"], "host-model");
    assert_eq!(payload["call_id"], "c");
}

#[tokio::test]
async fn model_requests_receive_core_identity() {
    let mut host = host(vec!["model.requested".into()]).await;
    host.model = Some("test".into());
    host.identity = Some("Core identity".into());
    let mut proposal = proposal("model.requested");
    proposal.payload = types::Payload::Json(br#"{"messages":[{"role":"system","content":[{"kind":"text","text":"Plugin instructions"}]}]}"#.to_vec());
    let request = host
        .append_request(&proposal, Some(EventId::new("cause")), None)
        .unwrap();
    let EventPayload::CanonicalJson(bytes) = request.payload else {
        panic!("expected JSON")
    };
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        payload["messages"][0]["content"][0]["text"],
        "Core identity"
    );
    assert_eq!(
        payload["messages"][0]["content"][1]["text"],
        "Plugin instructions"
    );
}

#[tokio::test]
async fn http_request_reads_are_confined_to_consumer_and_listener() {
    let mut host = host(vec!["http.request.received".into()]).await;
    let mut request = proposal("http.request.received");
    request.payload =
        types::Payload::Json(br#"{"consumer":"github/receive","body":"private"}"#.to_vec());
    request.idempotency_key = Some("request".into());
    events::Host::append(&mut host, request).await.unwrap();
    let stream = StreamId::new(host.delivery.agent.id.clone());
    let event = host
        .event_store
        .query(&stream, &EventQuery::default(), 10)
        .await
        .unwrap()
        .remove(0);
    assert!(http_request_visible(&event, &host.delivery.instance_id));
    host.delivery.instance_id = "unrelated".into();
    assert!(
        events::Host::get(&mut host, event.event_id.as_str().into())
            .await
            .is_err()
    );
    host.delivery.instance_id = "github/receive".into();
    assert!(
        events::Host::get(&mut host, event.event_id.as_str().into())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn credential_exports_are_scoped_without_raw_record_access() {
    use pluribus_core::{InMemoryCredentialStore, PluginCredentialStore};
    let store = Arc::new(InMemoryCredentialStore::default());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let doc = serde_json::json!({
        "private-key": "never-export",
        "exports": {
            "token": {"value": "allowed", "expires_at_ms": now + 120_000},
            "expired": {"value": "expired", "expires_at_ms": now - 1},
            "expiring": {"value": "expiring", "expires_at_ms": now + 20_000}
        }
    });
    store
        .replace_plugin_credential(
            &SecretHandle::new("test"),
            "provider",
            None,
            serde_json::to_vec(&doc).unwrap(),
        )
        .await
        .unwrap();
    let mut host = host(vec![]).await;
    host.credentials = Some(CredentialAccess {
        store,
        provider: "consumer".into(),
        handles: Default::default(),
        exports: [
            ("TOKEN", "provider", "test", "token"),
            ("EXPIRED", "provider", "test", "expired"),
            ("EXPIRING", "provider", "test", "expiring"),
            ("PRIVATE", "provider", "test", "private-key"),
            ("WRONG_PROVIDER", "other", "test", "token"),
            ("WRONG_HANDLE", "provider", "other", "token"),
        ]
        .into_iter()
        .map(|(name, provider, credential, export)| {
            (
                name.into(),
                CredentialExport {
                    provider: provider.into(),
                    credential: credential.into(),
                    export: export.into(),
                },
            )
        })
        .collect(),
    });
    assert_eq!(
        credentials::Host::resolve_export(&mut host, "TOKEN".into())
            .await
            .unwrap(),
        "allowed"
    );
    for binding in [
        "UNGRANTED",
        "EXPIRED",
        "EXPIRING",
        "PRIVATE",
        "WRONG_PROVIDER",
        "WRONG_HANDLE",
    ] {
        assert!(
            credentials::Host::resolve_export(&mut host, binding.into())
                .await
                .is_err()
        );
    }
    assert!(
        credentials::Host::get(&mut host, "test".into())
            .await
            .is_err()
    );
    assert!(
        credentials::Host::compare_and_swap(&mut host, "test".into(), None, vec![])
            .await
            .is_err()
    );
}

#[derive(Default)]
struct SubscriptionFixture {
    opens: AtomicU64,
    reads: AtomicU64,
    closed: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl StreamService for SubscriptionFixture {
    async fn open(&self, _: &StreamGrant) -> Result<String, StreamError> {
        unreachable!()
    }
    async fn subscribe(&self, _: &StreamGrant) -> Result<String, StreamError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok("input".into())
    }
    async fn next(&self, _: &str, _: u32) -> Result<pluribus_core::StreamPage, StreamError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(pluribus_core::StreamPage {
            bytes: vec![1],
            closed: false,
        })
    }
    async fn receive(
        &self,
        _: &str,
        _: u32,
        _: u32,
        _: &AtomicBool,
    ) -> Result<pluribus_core::StreamPage, StreamError> {
        unreachable!()
    }
    async fn send(&self, _: &str, _: &[u8]) -> Result<(), StreamError> {
        Ok(())
    }
    fn shutdown_write(&self, _: &str) {}
    fn close(&self, _: &str) {
        self.closed.notify_one();
    }
}

#[tokio::test]
async fn subscriptions_require_grants_and_wait_for_callback_commit() {
    let mut host = host(vec![]).await;
    assert!(socket::Host::subscribe(&mut host, vec![]).await.is_err());
    let service = Arc::new(SubscriptionFixture::default());
    host.stream = Some(service.clone());
    host.stream_grant = Some(StreamGrant {
        endpoint: pluribus_core::StreamEndpoint::Unix {
            path: "/unused".into(),
            peer_uids: vec![1],
        },
        max_bytes: 1024,
        max_timeout_ms: 1000,
    });
    assert!(socket::Host::subscribe(&mut host, vec![]).await.is_err());
    host.ingress_supported = true;
    host.replaying = true;
    assert!(socket::Host::subscribe(&mut host, vec![]).await.is_err());
    host.replaying = false;
    socket::Host::subscribe(&mut host, b"subscribe".to_vec())
        .await
        .unwrap();
    assert_eq!(
        service.opens.load(Ordering::SeqCst),
        0,
        "subscription is staged until commit"
    );
    let request = host.pending_subscription.take().unwrap();
    let mut subscription = subscription::Subscription::start(&host, request);
    let first = subscription.inbox.recv().await.unwrap();
    assert_eq!(first.payload["bytes"], serde_json::json!([1]));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), subscription.inbox.recv())
            .await
            .is_err()
    );
    assert_eq!(service.reads.load(Ordering::SeqCst), 1);
    first.committed.send(()).unwrap();
    let _second = subscription.inbox.recv().await.unwrap();
    assert_eq!(service.reads.load(Ordering::SeqCst), 2);
    assert!(
        host.event_store
            .read(&StreamId::new("personal"), 0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    drop(subscription);
    tokio::time::timeout(Duration::from_secs(1), service.closed.notified())
        .await
        .unwrap();
}

#[derive(Default)]
struct CliSubscriptionFixture {
    sequence: AtomicU64,
    frames: std::sync::Mutex<HashMap<String, VecDeque<Vec<u8>>>>,
    offsets: std::sync::Mutex<Vec<u64>>,
}
#[async_trait::async_trait]
impl StreamService for CliSubscriptionFixture {
    async fn open(&self, _: &StreamGrant) -> Result<String, StreamError> {
        Ok("reply".into())
    }
    async fn subscribe(&self, _: &StreamGrant) -> Result<String, StreamError> {
        Ok(format!(
            "input-{}",
            self.sequence.fetch_add(1, Ordering::SeqCst)
        ))
    }
    async fn send(&self, id: &str, bytes: &[u8]) -> Result<(), StreamError> {
        if id == "reply" {
            return Ok(());
        }
        let request: Value = serde_json::from_slice(bytes).unwrap();
        let after = request["after"].as_u64().unwrap();
        self.offsets.lock().unwrap().push(after);
        let frame = if after == 0 {
            b"{\"status\":\"messages\",\"messages\":[{\"sequence\":1,\"at_ms\":1,\"text\":\"hello\"}]}\n".to_vec()
        } else {
            b"{\"status\":\"messages\",\"messages\":[]}\n".to_vec()
        };
        let middle = frame.len() / 2;
        self.frames.lock().unwrap().insert(
            id.into(),
            VecDeque::from([frame[..middle].to_vec(), frame[middle..].to_vec()]),
        );
        Ok(())
    }
    async fn next(&self, id: &str, _: u32) -> Result<pluribus_core::StreamPage, StreamError> {
        let bytes = self
            .frames
            .lock()
            .unwrap()
            .get_mut(id)
            .and_then(VecDeque::pop_front);
        if let Some(bytes) = bytes {
            return Ok(pluribus_core::StreamPage {
                bytes,
                closed: false,
            });
        }
        std::future::pending().await
    }
    async fn receive(
        &self,
        _: &str,
        _: u32,
        _: u32,
        _: &AtomicBool,
    ) -> Result<pluribus_core::StreamPage, StreamError> {
        Ok(pluribus_core::StreamPage {
            bytes: b"{\"status\":\"delivered\"}\n".to_vec(),
            closed: false,
        })
    }
    fn shutdown_write(&self, _: &str) {}
    fn close(&self, id: &str) {
        self.frames.lock().unwrap().remove(id);
    }
}

#[tokio::test]
async fn packaged_cli_subscription_frames_input_and_commits_offsets() {
    let (runtime, store) = runtime().await;
    let package = PluginPackage::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/cli"),
    )
    .unwrap();
    let stream = Arc::new(CliSubscriptionFixture::default());
    let mut instance = runtime
        .instantiate(
            package.component("main").unwrap(),
            &serde_json::json!({}),
            delivery(),
            PluginServices {
                stream: Some(stream.clone()),
                stream_grant: Some(StreamGrant {
                    endpoint: pluribus_core::StreamEndpoint::Unix {
                        path: "/unused".into(),
                        peer_uids: vec![1],
                    },
                    max_bytes: 1024,
                    max_timeout_ms: 1000,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(instance.init().await.unwrap().events.is_empty());
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !instance.has_stream_input() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        instance.receive_input(1000).await.unwrap();
    }
    let events = store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "fragmented input and empty responses must not create extra events"
    );
    assert_eq!(events[0].request.event_type, "observation.received");
    assert_eq!(&stream.offsets.lock().unwrap()[..2], &[0, 1]);
    assert_eq!(
        StateStore::get(
            store.as_ref(),
            &StateNamespace::new("shell-1"),
            "input/cursor"
        )
        .await
        .unwrap()
        .value
        .unwrap(),
        b"1"
    );
    instance.stop(1000).await.unwrap();
    tokio::task::yield_now().await;
    assert!(stream.frames.lock().unwrap().is_empty());
}
