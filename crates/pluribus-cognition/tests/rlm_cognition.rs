//! A pinned session must keep its interpreter heap across deliveries.

use pluribus_cognition::{Agent, AuthorityResolver, Router};
use pluribus_core::{
    AppendRequest, Audience, Authority, AuthorityId, CapabilityName, CommittedEvent,
    ConstraintPolicy, ConstraintSet, DeliveryStore, EventId, EventMetadataSource, EventPayload,
    EventStore, EventTypeRegistry, Grant, InMemoryBlobStore, Origin, OriginKind, PrincipalKind,
    PrincipalRef, StateStore, StreamId, StreamKind,
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
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

struct Metadata(AtomicU64, Arc<AtomicI64>);

impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }

    fn now_ms(&self) -> i64 {
        self.1.load(Ordering::Relaxed)
    }
}

struct AllowAll;

impl ConstraintPolicy for AllowAll {
    fn allows(&self, _grant: &Grant, _request: &[u8]) -> Result<bool, String> {
        Ok(true)
    }
}

struct TestAuthority;

#[async_trait::async_trait]
impl AuthorityResolver for TestAuthority {
    async fn resolve(&self, _event: &CommittedEvent) -> Result<Authority, String> {
        Ok(Authority {
            schema: Authority::SCHEMA.into(),
            authority_id: AuthorityId::new("authority-1"),
            agent: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            origin: Origin {
                kind: OriginKind::Autonomous,
                principal: None,
                connector: None,
                conversation_id: None,
                source_event_id: EventId::new("origin-1"),
                trusted: true,
            },
            delegation_chain: Vec::new(),
            grants: ["system.echo", "telegram.reply"]
                .into_iter()
                .map(|name| {
                    let capability = CapabilityName::new(name);
                    (
                        capability.clone(),
                        vec![Grant {
                            capability,
                            provider: None,
                            constraints: ConstraintSet::canonical_json(b"{}".to_vec()),
                        }],
                    )
                })
                .collect(),
            audiences: Vec::<Audience>::new(),
            parent_authority: None,
            issued_at_ms: 0,
            expires_at_ms: None,
            max_depth: 4,
            current_depth: 0,
        })
    }
}

async fn append(
    store: &Arc<SqliteEventStore<Metadata>>,
    event_type: &str,
    payload: &Value,
) -> CommittedEvent {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: event_type.into(),
            payload_schema: "dev.pluribus.js.cell/1".into(),
            payload: EventPayload::CanonicalJson(serde_json::to_vec(payload).unwrap()),
            actor: PrincipalRef::new(PrincipalKind::Agent, "personal"),
            authority_id: Some(AuthorityId::new("authority-1")),
            activity_id: Some("activity-1".into()),
            correlation_id: Some("correlation-1".into()),
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap()
}

fn payload(event: &CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("expected JSON")
    };
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn packaged_cognition_recurses_through_events_and_preserves_parent_state() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let runtime = Runtime::new(
        RuntimeLimits {
            ..RuntimeLimits::default()
        },
        Arc::clone(&store) as Arc<dyn StateStore>,
        Arc::clone(&store) as Arc<dyn EventStore>,
        Arc::new(InMemoryBlobStore::default()),
        Arc::clone(&store) as Arc<dyn DeliveryStore>,
    )
    .unwrap();
    let mut agent = Agent::new(
        Router::new(
            StreamId::new("personal"),
            PrincipalRef::new(PrincipalKind::Agent, "personal"),
            Arc::clone(&store) as _,
            Arc::new(EventTypeRegistry::core()),
            AllowAll,
        ),
        TestAuthority,
        runtime,
        Arc::clone(&store) as _,
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
    );
    for (id, path, config) in [
        (
            "cognition",
            "rlm",
            json!({"model":"test","identity":"test","js":{},"connectors":["telegram-1"],"trusted_users":["7"]}),
        ),
        ("code", "js", json!({})),
    ] {
        agent
            .install_component(
                PluginPackage::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/plugins/{}",
                    if path == "js" { "rlm" } else { path }
                )))
                .unwrap()
                .component(if path == "rlm" { "cognition" } else { "js" })
                .unwrap(),
                &config,
                Delivery {
                    instance_id: id.into(),
                    agent: Principal {
                        kind: RuntimePrincipalKind::Agent,
                        id: "personal".into(),
                    },
                    actor: Principal {
                        kind: RuntimePrincipalKind::Agent,
                        id: "personal".into(),
                    },
                    authority_id: "authority".into(),
                    activity_id: "activity".into(),
                    correlation_id: "correlation".into(),
                    origin_event_id: "origin".into(),
                    depth: 0,
                    deadline_at_ms: None,
                    visible_blobs: vec![],
                },
                PluginServices::default(),
                &[],
            )
            .await
            .unwrap();
    }
    let mut origin = append(
        &store,
        "observation.received",
        &json!({"provider":"telegram","externalSenderId":"7","message":{"text":"compute"}}),
    )
    .await
    .request;
    origin.actor = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
    store.append(origin).await.unwrap();
    let scripts = [
        (
            "js",
            "state.seed=40; state.answer=await rlm.query({question:'child',context:{n:2}}); return state.answer;",
        ),
        (
            "js",
            "return await rlm.query({question:'grandchild',context:context.context});",
        ),
        ("js", "return context.context.n;"),
        ("child", "2"),
        ("child", "2"),
        ("js", "return state.seed + Number(state.answer);"),
        ("root", "42"),
    ];
    let mut answered = std::collections::HashSet::new();
    let mut calls = 0;
    for _ in 0..80 {
        agent.tick_wait(1).await.unwrap();
        let events = store
            .read(&StreamId::new("personal"), 0, 1000)
            .await
            .unwrap();
        for request in events
            .iter()
            .filter(|e| e.request.event_type == "model.requested")
        {
            if !answered.insert(request.event_id.clone()) {
                continue;
            }
            let value = payload(request);
            let _: pluribus_model::Request = serde_json::from_value(value.clone()).unwrap();
            assert!(serde_json::to_vec(&value).unwrap().len() < 64 * 1024);
            let (kind, text) = scripts[calls];
            calls += 1;
            let content = match kind {
                "root" => yield_control(json!({"action":"complete","note":text})),
                "child" => yield_control(json!({"result":text})),
                "js" => {
                    json!([{"kind":"tool-call","call_id":format!("js{calls}"),"name":"js","arguments":{"code":text}}])
                }
                _ => unreachable!(),
            };
            let mut result=append(&store,"model.completed",&json!({"call_id":value["call_id"],"message":{"role":"assistant","content":content},"stop_reason":{"kind":"end-turn"}})).await.request;
            result.causation_id = Some(request.event_id.clone());
            store.append(result).await.unwrap();
        }
        if events
            .iter()
            .any(|e| e.request.event_type == "cognition.completed")
        {
            break;
        }
    }
    let events = store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap();
    assert_eq!(calls, 7);
    assert!(
        events
            .iter()
            .any(|e| e.request.event_type == "code.completed" && payload(e)["value"] == 42)
    );
    assert!(
        events
            .iter()
            .any(|e| e.request.event_type == "cognition.completed")
    );
    assert!(
        !events
            .iter()
            .any(|e| e.request.event_type == "capability.requested")
    );
}

struct BlockedHttp {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
#[async_trait::async_trait]
impl pluribus_core::HttpService for BlockedHttp {
    async fn send(
        &self,
        _: &pluribus_core::HttpGrant,
        _: &pluribus_core::HttpRequest,
    ) -> Result<pluribus_core::HttpResponse, pluribus_core::HttpError> {
        unreachable!()
    }
}
#[async_trait::async_trait]
impl pluribus_core::HttpStreamService for BlockedHttp {
    async fn open_stream(
        &self,
        _: &pluribus_core::HttpGrant,
        _: pluribus_core::HttpStreamProtocol,
        _: &pluribus_core::HttpRequest,
    ) -> Result<String, pluribus_core::HttpError> {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        Err(pluribus_core::HttpError::Cancelled)
    }
    async fn receive(
        &self,
        _: &pluribus_core::HttpGrant,
        _: &str,
        _: u32,
        _: u32,
    ) -> Result<pluribus_core::HttpFramePage, pluribus_core::HttpError> {
        unreachable!()
    }
    async fn send_frame(
        &self,
        _: &pluribus_core::HttpGrant,
        _: &str,
        _: &pluribus_core::HttpFrame,
    ) -> Result<(), pluribus_core::HttpError> {
        unreachable!()
    }
    fn close_stream(&self, _: &pluribus_core::HttpGrant, _: &str) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_packaged_provider_does_not_block_coordinator_intake() {
    blocked_provider(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_signals_a_blocked_provider_and_prevents_admission() {
    blocked_provider(true).await;
}
#[allow(clippy::too_many_lines)]
async fn blocked_provider(stopping: bool) {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        Arc::new(InMemoryBlobStore::default()),
        store.clone(),
    )
    .unwrap();
    let mut agent = Agent::new(
        Router::new(
            StreamId::new("personal"),
            PrincipalRef::new(PrincipalKind::Agent, "personal"),
            store.clone(),
            Arc::new(EventTypeRegistry::core()),
            AllowAll,
        ),
        TestAuthority,
        runtime,
        store.clone(),
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
    );
    let (entered_tx, entered) = sync_channel(1);
    let (release, release_rx) = sync_channel(1);
    let http = Arc::new(BlockedHttp {
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    for (id, plugin, config, services, models) in [
        (
            "provider",
            "openrouter",
            json!({"credential":"test","models":["test"]}),
            PluginServices {
                http: Some(http),
                http_grant: Some(pluribus_core::HttpGrant {
                    component: PrincipalRef::new(PrincipalKind::Component, "provider"),
                    origins: vec!["https://openrouter.ai".into()],
                    methods: vec!["POST".into()],
                    allow_http: false,
                    allow_private_network: false,
                    max_request_bytes: 1_000_000,
                    max_response_bytes: 1_000_000,
                    max_redirects: 0,
                    max_timeout_ms: 300_000,
                }),
                ..PluginServices::default()
            },
            vec!["test".into()],
        ),
        (
            "cognition",
            "rlm",
            json!({"model":"test","identity":"test","js":{},"connectors":["telegram-1"]}),
            PluginServices::default(),
            vec![],
        ),
    ] {
        agent
            .install_component(
                PluginPackage::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/plugins/{}",
                    if plugin == "js" { "rlm" } else { plugin }
                )))
                .unwrap()
                .component(match plugin {
                    "rlm" => "cognition",
                    "js" => "js",
                    _ => "main",
                })
                .unwrap(),
                &config,
                Delivery {
                    instance_id: id.into(),
                    agent: Principal {
                        kind: RuntimePrincipalKind::Agent,
                        id: "personal".into(),
                    },
                    actor: Principal {
                        kind: RuntimePrincipalKind::Agent,
                        id: "personal".into(),
                    },
                    authority_id: "a".into(),
                    activity_id: "a".into(),
                    correlation_id: "a".into(),
                    origin_event_id: "a".into(),
                    depth: 0,
                    deadline_at_ms: None,
                    visible_blobs: vec![],
                },
                services,
                &models,
            )
            .await
            .unwrap();
    }
    append(
        &store,
        "model.requested",
        &json!({"call_id":"blocked","model":"test","messages":[{"role":"user","content":[{"kind":"text","text":"test"}]}],"tools":[]}),
    ).await;
    let (returned_tx, returned) = sync_channel(1);
    let worker = tokio::spawn(async move {
        agent.tick(1).await.unwrap();
        returned_tx.send(agent).unwrap();
    });
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    let available = returned.recv_timeout(Duration::from_secs(2));
    if available.is_err() {
        release.send(()).unwrap();
        worker.await.unwrap();
        panic!("scheduler waited for the provider");
    }
    let mut agent = available.unwrap();
    let mut observation = append(
        &store,
        "observation.received",
        &json!({"message":{"text":"independent"}}),
    )
    .await
    .request;
    observation.actor = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
    let observation = store.append(observation).await.unwrap();
    agent.tick(2).await.unwrap();
    let events = store
        .read(&StreamId::new("personal"), observation.sequence, 100)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.request.event_type == "model.requested")
    );
    assert!(
        !events
            .iter()
            .any(|event| event.request.event_type == "model.failed")
    );
    if stopping {
        agent.stop_signal().store(true, Ordering::Release);
        for handle in agent.cancellation_handles() {
            handle.cancel();
            assert!(handle.is_cancelled());
        }
        agent.tick(3).await.unwrap();
        assert_eq!(
            store
                .read(&StreamId::new("personal"), 0, 1000)
                .await
                .unwrap()
                .iter()
                .filter(|e| e.request.event_type == "activity.attempted")
                .count(),
            1
        );
    }
    release.send(()).unwrap();
    worker.await.unwrap();
}

async fn persistent_agent(
    store: &Arc<SqliteEventStore<Metadata>>,
) -> Agent<AllowAll, TestAuthority> {
    persistent_agent_on(store, store.clone()).await
}
async fn persistent_agent_on(
    store: &Arc<SqliteEventStore<Metadata>>,
    deliveries: Arc<dyn DeliveryStore>,
) -> Agent<AllowAll, TestAuthority> {
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        Arc::new(InMemoryBlobStore::default()),
        deliveries,
    )
    .unwrap();
    let mut agent = Agent::new(
        Router::new(
            StreamId::new("personal"),
            PrincipalRef::new(PrincipalKind::Agent, "personal"),
            store.clone(),
            Arc::new(EventTypeRegistry::core()),
            AllowAll,
        ),
        TestAuthority,
        runtime,
        store.clone(),
        StreamId::new("personal"),
        PrincipalRef::new(PrincipalKind::Agent, "personal"),
    );
    for (id, plugin, config) in [
        (
            "cognition",
            "rlm",
            json!({"model":"test","identity":"test","js":{},"connectors":["telegram-1"],"trusted_users":["7"],"background_model_calls_per_hour":1}),
        ),
        ("code", "js", json!({})),
        ("echo", "echo", json!({})),
    ] {
        agent
            .install_component(
                PluginPackage::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/plugins/{}",
                    if plugin == "js" { "rlm" } else { plugin }
                )))
                .unwrap()
                .component(match plugin {
                    "rlm" => "cognition",
                    "js" => "js",
                    _ => "main",
                })
                .unwrap(),
                &config,
                Delivery {
                    instance_id: id.into(),
                    agent: Principal {
                        kind: RuntimePrincipalKind::Agent,
                        id: "personal".into(),
                    },
                    actor: Principal {
                        kind: RuntimePrincipalKind::Agent,
                        id: "personal".into(),
                    },
                    authority_id: "a".into(),
                    activity_id: "a".into(),
                    correlation_id: "a".into(),
                    origin_event_id: "a".into(),
                    depth: 0,
                    deadline_at_ms: None,
                    visible_blobs: vec![],
                },
                PluginServices::default(),
                &[],
            )
            .await
            .unwrap();
    }
    agent
}
#[allow(clippy::needless_pass_by_value)]
async fn observation(store: &Arc<SqliteEventStore<Metadata>>, extra: Value) -> CommittedEvent {
    let mut value = json!({"provider":"telegram","externalSenderId":"7","conversationId":"chat:42","message":{"chat":{"id":42},"text":"Investigate build, fix it, open a PR"}});
    for (k, v) in extra.as_object().unwrap() {
        value[k] = v.clone();
    }
    let mut request = append(store, "observation.received", &value).await.request;
    request.actor = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
    store.append(request).await.unwrap()
}
#[allow(clippy::needless_pass_by_value)]
async fn scripted(
    store: &Arc<SqliteEventStore<Metadata>>,
    request: &CommittedEvent,
    content: Value,
) {
    let mut result=append(store,"model.completed",&json!({"call_id":payload(request)["call_id"],"message":{"role":"assistant","content":content},"stop_reason":{"kind":"end-turn"}})).await.request;
    result.causation_id = Some(request.event_id.clone());
    store.append(result).await.unwrap();
}
async fn model_requests(store: &Arc<SqliteEventStore<Metadata>>) -> Vec<CommittedEvent> {
    store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.request.event_type == "model.requested")
        .collect()
}
#[allow(clippy::too_many_lines)]
async fn projection(store: &Arc<SqliteEventStore<Metadata>>) -> Value {
    let namespace = pluribus_core::StateNamespace::new("cognition");
    let mut engine =
        json!({"jobs":{},"inbox":{},"tasks":{},"calls":{},"budgets":{},"seen_results":[]});
    let count = StateStore::get(store.as_ref(), &namespace, "engine/count")
        .await
        .unwrap();
    if let Some(count) = count.value {
        let count: usize = serde_json::from_slice(&count).unwrap();
        let mut bytes = vec![];
        for index in 0..count {
            bytes.extend(
                StateStore::get(
                    store.as_ref(),
                    &namespace,
                    &format!("engine/part/{index:08}"),
                )
                .await
                .unwrap()
                .value
                .unwrap(),
            );
        }
        engine = serde_json::from_slice(&bytes).unwrap();
    } else if let Some(bytes) = StateStore::get(store.as_ref(), &namespace, "engine")
        .await
        .unwrap()
        .value
    {
        engine = serde_json::from_slice(&bytes).unwrap();
    }
    let mut after = None;
    let mut records: std::collections::BTreeMap<String, Vec<u8>> =
        std::collections::BTreeMap::new();
    loop {
        let page = store
            .scan(&namespace, "engine/record/", after.as_deref(), 100)
            .await
            .unwrap();
        for entry in page.entries {
            let key = entry.key.rsplit_once('/').unwrap().0.to_owned();
            let bytes = records.entry(key).or_default();
            if serde_json::from_slice::<Value>(bytes).is_err() {
                bytes.extend(entry.value);
            }
        }
        match page.next_key {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    for bytes in records.into_values() {
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        let field = record["field"].as_str().unwrap();
        let key = record["key"].as_str().unwrap();
        if record["value"].is_null() {
            if let Some(values) = engine[field].as_object_mut() {
                values.remove(key);
            }
        } else if field == "seen_results" {
            engine[field].as_array_mut().unwrap().push(json!(key));
        } else if field == "observations_seen" {
            engine["inbox"][key] = record["value"].clone();
        } else if key.is_empty() {
            engine[field] = record["value"].clone();
        } else {
            engine[field][key] = record["value"].clone();
        }
    }
    engine
}
async fn drive(agent: &mut Agent<AllowAll, TestAuthority>, now: i64) {
    for _ in 0..8 {
        agent.tick_wait(now).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_jobs_rebuild_corrections_wakes_and_rolling_budgets() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock.clone()))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({})).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert!(
        store
            .read(&StreamId::new("personal"), 0, 1000)
            .await
            .unwrap()
            .iter()
            .any(|event| event.request.event_type == "cognition.job-updated"
                && payload(event)["job"]["id"] == origin.event_id.as_str())
    );
    let first = model_requests(&store).await.remove(0);
    observation(
        &store,
        json!({"jobId":origin.event_id.as_str(),"constraint":"Leave authentication alone"}),
    )
    .await;
    scripted(
        &store,
        &first,
        json!([{"kind":"tool-call","name":"js","call_id":"stale","arguments":{"code":"throw Error('stale source executed');"}}]),
    ).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert!(
        !store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .iter()
            .any(|e| e.request.event_type == "code.evaluate-requested")
    );
    let second = model_requests(&store).await.pop().unwrap();
    scripted(
        &store,
        &second,
        yield_control(json!({"action":"continue","note":"Tests next"})),
    )
    .await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    let state = projection(&store).await;
    assert_eq!(state["jobs"][origin.event_id.as_str()]["revision"], 1);
    assert_eq!(
        state["jobs"][origin.event_id.as_str()]["status"],
        "waiting-time"
    );
    drop(agent);
    let namespace = pluribus_core::StateNamespace::new("cognition");
    let page = store.scan(&namespace, "", None, 1000).await.unwrap();
    store
        .apply(
            &namespace,
            page.revision,
            &page
                .entries
                .iter()
                .map(|e| pluribus_core::StateMutation::Delete { key: e.key.clone() })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let count = model_requests(&store).await.len();
    let mut agent = persistent_agent(&store).await;
    assert_eq!(projection(&store).await["jobs"], state["jobs"]);
    assert_eq!(model_requests(&store).await.len(), count);
    clock.fetch_add(60_000, Ordering::Relaxed);
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(model_requests(&store).await.len(), count + 1);
    let background = model_requests(&store).await.pop().unwrap();
    scripted(
        &store,
        &background,
        yield_control(json!({"action":"continue"})),
    )
    .await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    clock.fetch_add(120_000, Ordering::Relaxed);
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "paused-budget"
    );
    assert_eq!(model_requests(&store).await.len(), count + 1);
    drop(agent);
    let mut agent = persistent_agent(&store).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(model_requests(&store).await.len(), count + 1);
    clock.fetch_add(3_600_000, Ordering::Relaxed);
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(model_requests(&store).await.len(), count + 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_checkpoint_restores_values_after_interpreter_restart() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    observation(&store, json!({})).await;
    drive(&mut agent, 1).await;
    scripted(
        &store,
        &model_requests(&store).await[0],
        json!([{"kind":"tool-call","name":"js","call_id":"save","arguments":{"code":"state.n=42; checkpoint(state); return state.n;"}}]),
    ).await;
    drive(&mut agent, 1).await;
    let next = model_requests(&store).await.pop().unwrap();
    drop(agent);
    let mut agent = persistent_agent(&store).await;
    scripted(
        &store,
        &next,
        json!([{"kind":"tool-call","name":"js","call_id":"restore","arguments":{"code":"return {n:state.n,recovered:context.recovered};"}}]),
    ).await;
    drive(&mut agent, 1).await;
    assert!(
        store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .iter()
            .any(|e| e.request.event_type == "code.completed"
                && payload(e)["value"] == json!({"n":42,"recovered":true}))
    );
}

struct CommitBarrier {
    store: Arc<SqliteEventStore<Metadata>>,
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
#[async_trait::async_trait]
impl DeliveryStore for CommitBarrier {
    async fn checkpoint(
        &self,
        cursor: &pluribus_core::CursorKey,
    ) -> Result<u64, pluribus_core::DeliveryError> {
        self.store.checkpoint(cursor).await
    }
    async fn discard(
        &self,
        cursor: &pluribus_core::CursorKey,
    ) -> Result<(), pluribus_core::DeliveryError> {
        self.store.discard(cursor).await
    }
    async fn commit(
        &self,
        commit: pluribus_core::DeliveryCommit,
    ) -> Result<pluribus_core::DeliveryReceipt, pluribus_core::DeliveryError> {
        if commit.cursor.namespace.as_str() == "echo"
            && commit
                .events
                .iter()
                .any(|e| e.event_type == "capability.completed")
            && self.armed.swap(false, Ordering::AcqRel)
        {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        self.store.commit(commit).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn packaged_reference_scenario_corrects_running_tests_and_restarts_before_pr() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let (release, release_rx) = std::sync::mpsc::sync_channel(1);
    let barrier = Arc::new(CommitBarrier {
        store: store.clone(),
        armed: std::sync::atomic::AtomicBool::new(true),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let mut agent = persistent_agent_on(&store, barrier).await;
    let origin = observation(&store, json!({})).await;
    drive(&mut agent, 1).await;
    let js = |source: &str| json!([{"kind":"tool-call","name":"js","call_id":"cell","arguments":{"code":source}}]);
    scripted(
        &store,
        &model_requests(&store).await[0],
        js(
            "state.stage='failure reproduced; parser edited'; checkpoint(state); return await capabilities.invoke('system.echo',{message:'tests passed'});",
        ),
    ).await;
    agent.tick_wait(1).await.unwrap();
    agent.tick(1).await.unwrap();
    entered
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let correction = observation(
        &store,
        json!({"jobId":origin.event_id.as_str(),"constraint":"Leave authentication alone"}),
    )
    .await;
    agent.tick(1).await.unwrap();
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["revision"],
        1
    );
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["sources"],
        json!([origin.event_id.as_str(), correction.event_id.as_str()])
    );
    release.send(()).unwrap();
    drive(&mut agent, 1).await;
    let reconsider = model_requests(&store).await.pop().unwrap();
    drop(agent);
    let mut agent = persistent_agent(&store).await;
    scripted(
        &store,
        &reconsider,
        js(
            "return (await history.read({after:0,limit:100})).events.filter(e=>e.type==='capability.completed');",
        ),
    ).await;
    drive(&mut agent, 1).await;
    let evidence = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    assert_eq!(
        evidence
            .iter()
            .filter(|e| e.request.event_type == "capability.completed"
                && payload(e)["output"]["message"] == "tests passed")
            .count(),
        1
    );
    let after_tests = model_requests(&store).await.pop().unwrap();
    scripted(
        &store,
        &after_tests,
        js("return await capabilities.invoke('system.echo',{message:'PR opened after tests'});"),
    )
    .await;
    drive(&mut agent, 1).await;
    let after_pr = model_requests(&store).await.pop().unwrap();
    scripted(
        &store,
        &after_pr,
        yield_control(
            json!({"action":"complete","note":"Tests passed; PR opened; authentication untouched","reply":"Fixed; PR opened."}),
        ),
    ).await;
    drive(&mut agent, 1).await;
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    let replies = events
        .iter()
        .filter(|e| {
            e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "telegram.reply"
        })
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].request.causation_id.as_ref(),
        Some(&origin.event_id)
    );
    assert_eq!(
        payload(replies[0])["arguments"]["conversationId"],
        "chat:42"
    );
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_duplicate_history_yield_emits_one_resume() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    observation(&store, json!({})).await;
    drive(&mut agent, 1).await;
    scripted(
        &store,
        &model_requests(&store).await[0],
        json!([{"kind":"tool-call","name":"js","call_id":"history","arguments":{"code":"return await history.read({after:0,limit:1});"}}]),
    ).await;
    agent.tick_wait(1).await.unwrap();
    let mut duplicate = store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.request.event_type == "code.yielded")
        .unwrap()
        .request;
    duplicate.deduplication_key = None;
    store.append(duplicate).await.unwrap();
    drive(&mut agent, 1).await;
    assert_eq!(
        store
            .read(&StreamId::new("personal"), 0, 1000)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.request.event_type == "code.resumed")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_missing_checkpoint_reports_session_loss_before_executing_source() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    observation(&store, json!({})).await;
    drive(&mut agent, 1).await;
    scripted(
        &store,
        &model_requests(&store).await[0],
        json!([{"kind":"tool-call","name":"js","call_id":"save","arguments":{"code":"state.n=42; return state.n;"}}]),
    ).await;
    drive(&mut agent, 1).await;
    let next = model_requests(&store).await.pop().unwrap();
    drop(agent);
    let mut agent = persistent_agent(&store).await;
    scripted(
        &store,
        &next,
        json!([{"kind":"tool-call","name":"js","call_id":"restore","arguments":{"code":"return state.n;"}}]),
    ).await;
    drive(&mut agent, 1).await;
    assert!(
        store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .iter()
            .any(|e| e.request.event_type == "code.failed"
                && payload(e)["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("session was lost")))
    );
}

fn yield_control(arguments: Value) -> Value {
    let mut content = json!([{"kind":"tool-call","name":"yield","call_id":"decision"}]);
    content[0]["arguments"] = arguments;
    content
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_yield_completes_pong_with_one_reply() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Reply with pong"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    let request = &model_requests(&store).await[0];
    let envelope: Value = serde_json::from_str(
        payload(request)["messages"][1]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(envelope["schema"], "pluribus.turn/1");
    assert_eq!(envelope["message"], "Reply with pong");
    assert_eq!(envelope["trigger"]["kind"], "observation");
    let model: pluribus_model::Request = serde_json::from_value(payload(request)).unwrap();
    let control = model
        .tools
        .iter()
        .find(|tool| tool.name == "yield")
        .expect("root must expose its control schema");
    assert!(control.input_schema["properties"]["action"].is_object());
    assert!(model.tools.iter().any(|tool| tool.name == "js"));
    scripted(
        &store,
        request,
        yield_control(json!({"action":"complete","reply":"pong"})),
    )
    .await;
    drive(&mut agent, 1).await;
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    let replies: Vec<_> = events
        .iter()
        .filter(|e| {
            e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "telegram.reply"
        })
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(payload(replies[0])["arguments"]["text"], "pong");
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "completed"
    );
    scripted(
        &store,
        request,
        yield_control(json!({"action":"complete","reply":"pong"})),
    )
    .await;
    drive(&mut agent, 1).await;
    assert_eq!(
        store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "telegram.reply")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_invalid_root_output_retries_before_waiting_with_reply() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({})).await;
    drive(&mut agent, 1).await;
    scripted(
        &store,
        &model_requests(&store).await[0],
        json!([{"kind":"text","text":"pong"}]),
    )
    .await;
    drive(&mut agent, 1).await;
    let requests = model_requests(&store).await;
    assert_eq!(requests.len(), 2);
    assert_ne!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "completed"
    );
    assert!(
        !store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(
                e.request.event_type.as_str(),
                "cognition.completed" | "capability.requested"
            ))
    );
    scripted(
        &store,
        &requests[1],
        yield_control(
            json!({"action":"complete","waitFor":"input","reply":"Do not send","note":"Do not persist"}),
        ),
    ).await;
    drive(&mut agent, 1).await;
    let requests = model_requests(&store).await;
    assert_eq!(requests.len(), 3);
    let job = &projection(&store).await["jobs"][origin.event_id.as_str()];
    assert_ne!(job["status"], "completed");
    assert!(job["notes"].is_null());
    assert!(
        !store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(
                e.request.event_type.as_str(),
                "cognition.completed" | "capability.requested"
            ))
    );
    scripted(
        &store,
        &requests[2],
        yield_control(
            json!({"action":"wait","waitFor":"input","question":"Which repository?","reply":"Which repository?"}),
        ),
    ).await;
    drive(&mut agent, 1).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "waiting-input"
    );
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    let replies: Vec<_> = events
        .iter()
        .filter(|e| {
            e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "telegram.reply"
        })
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        payload(replies[0])["arguments"]["text"],
        "Which repository?"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_associate_resumes_waiting_job_before_reply() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({})).await;
    drive(&mut agent, 1).await;
    scripted(
        &store,
        &model_requests(&store).await[0],
        yield_control(json!({"action":"wait","waitFor":"input","question":"Which repository?"})),
    )
    .await;
    drive(&mut agent, 1).await;
    let followup = observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Use this repository and reply pong"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    let requests = model_requests(&store).await;
    assert_eq!(requests.len(), 2);
    let model: pluribus_model::Request = serde_json::from_value(payload(&requests[1])).unwrap();
    assert_eq!(model.tools.len(), 1);
    assert_eq!(model.tools[0].name, "associate");

    scripted(
        &store,
        &requests[1],
        json!([{"kind":"tool-call","name":"associate","call_id":"association","arguments":{"action":"amend","jobId":origin.event_id.as_str()}}]),
    ).await;
    drive(&mut agent, 1).await;
    let requests = model_requests(&store).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["sources"],
        json!([origin.event_id.as_str(), followup.event_id.as_str()])
    );
    scripted(
        &store,
        &requests[2],
        yield_control(json!({"action":"complete","reply":"pong"})),
    )
    .await;
    drive(&mut agent, 1).await;
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    let replies: Vec<_> = events
        .iter()
        .filter(|e| {
            e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "telegram.reply"
        })
        .collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(
        payload(replies[0])["arguments"]["text"],
        "Which repository?"
    );
    assert_eq!(payload(replies[1])["arguments"]["text"], "pong");
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_provider_failure_is_not_a_user_question() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({"message":{"chat":{"id":42},"text":"hi"}})).await;
    drive(&mut agent, 1).await;
    let request = &model_requests(&store).await[0];
    let mut failed = append(&store,"model.failed", &json!({"call_id":payload(request)["call_id"],"code":"outcome-unknown","message":"provider-secret-diagnostic"})).await.request;
    failed.causation_id = Some(request.event_id.clone());
    store.append(failed).await.unwrap();
    drive(&mut agent, 1).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["wait_reason"],
        "provider-error"
    );
    observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Reply with pong"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    let request = payload(model_requests(&store).await.last().unwrap());
    assert!(!request.to_string().contains("provider-secret-diagnostic"));
    let tools = request["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "yield"));
    assert!(!tools.iter().any(|tool| tool["name"] == "associate"));
    assert_eq!(
        projection(&store).await["jobs"].as_object().unwrap().len(),
        2
    );
    assert_eq!(model_requests(&store).await.len(), 2);
}

async fn routing_case(
    store: &Arc<SqliteEventStore<Metadata>>,
    agent: &mut Agent<AllowAll, TestAuthority>,
    name: &str,
    text: &str,
    expected: Value,
) -> Value {
    observation(store, json!({"message":{"chat":{"id":42},"text":text}})).await;
    drive(agent, 1).await;
    let request = model_requests(store).await.pop().unwrap();
    let mut result = json!({"name":name,"question_terms":["ci","readme","tests","documentation"],"request":payload(&request)});
    result["expected"] = expected;
    assert!(
        !result["request"]["tools"][0]["input_schema"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "ignore")
    );
    scripted(
        store,
        &request,
        json!([{"kind":"tool-call","name":"associate","call_id":"new-fixture","arguments":{"action":"new","jobId":null}}]),
    ).await;
    drive(agent, 1).await;
    scripted(
        store,
        model_requests(store).await.last().unwrap(),
        yield_control(json!({"action":"complete"})),
    )
    .await;
    drive(agent, 1).await;
    result
}

async fn routing_waiting_job(
    store: &Arc<SqliteEventStore<Metadata>>,
    agent: &mut Agent<AllowAll, TestAuthority>,
    objective: &str,
    question: &str,
) -> CommittedEvent {
    let origin = observation(
        store,
        json!({"message":{"chat":{"id":42},"text":objective}}),
    )
    .await;
    drive(agent, 1).await;
    if payload(model_requests(store).await.last().unwrap())["tools"][0]["name"] == "associate" {
        scripted(
            store,
            model_requests(store).await.last().unwrap(),
            json!([{"kind":"tool-call","name":"associate","call_id":"new-fixture","arguments":{"action":"new","jobId":null}}]),
        ).await;
        drive(agent, 1).await;
    }
    scripted(
        store,
        model_requests(store).await.last().unwrap(),
        yield_control(
            json!({"action":"wait","waitFor":"input","question":question,"reply":question}),
        ),
    )
    .await;
    drive(agent, 1).await;
    origin
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "exports packaged requests for separate real-provider evaluation"]
#[allow(clippy::too_many_lines)]
async fn export_packaged_routing_cases() {
    let Ok(output) = std::env::var("PLURIBUS_ROUTING_EVAL_OUTPUT") else {
        eprintln!("Set PLURIBUS_ROUTING_EVAL_OUTPUT to export routing cases");
        return;
    };
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    observation(&store, json!({"message":{"chat":{"id":42},"text":"hi"}})).await;
    drive(&mut agent, 1).await;
    let request = model_requests(&store).await.pop().unwrap();
    let mut failed = append(&store, "model.failed", &json!({"call_id":payload(&request)["call_id"],"code":"outcome-unknown","message":"Expired provider token"})).await.request;
    failed.causation_id = Some(request.event_id.clone());
    store.append(failed).await.unwrap();
    drive(&mut agent, 1).await;
    observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Reply with pong"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    let direct = model_requests(&store).await.pop().unwrap();
    let direct_payload = payload(&direct);
    let tools = direct_payload["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "yield"));
    assert!(!tools.iter().any(|tool| tool["name"] == "associate"));
    scripted(&store, &direct, yield_control(json!({"action":"complete"}))).await;
    drive(&mut agent, 1).await;
    let mut cases = Vec::new();
    let readme = routing_waiting_job(
        &store,
        &mut agent,
        "Review README documentation style",
        "Which README should I review?",
    )
    .await;
    let ci = routing_waiting_job(
        &store,
        &mut agent,
        "Fix the CI tests",
        "Which branch should I use for the CI tests?",
    )
    .await;
    cases.push(
        routing_case(
            &store,
            &mut agent,
            "clarification-answer",
            "Use the main branch for the CI tests.",
            json!({"action":"amend","jobId":ci.event_id.as_str()}),
        )
        .await,
    );
    cases.push(
        routing_case(
            &store,
            &mut agent,
            "unrelated-new",
            "What is 2 + 2?",
            json!({"action":"new","jobId":null}),
        )
        .await,
    );
    cases.push(
        routing_case(
            &store,
            &mut agent,
            "explicit-correction",
            "For the README review, check only spelling; leave the structure alone.",
            json!({"action":"amend","jobId":readme.event_id.as_str()}),
        )
        .await,
    );
    cases.push(
        routing_case(
            &store,
            &mut agent,
            "ambiguous-cancel",
            "Cancel it.",
            json!({"action":"clarify","jobId":null}),
        )
        .await,
    );
    assert_eq!(cases.len(), 4);
    for case in &cases {
        let context: Value = serde_json::from_str(
            case["request"]["messages"][1]["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(
            context["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|job| job["waitReason"] != "provider-error")
        );
    }
    assert!(
        !cases[0]["request"]
            .to_string()
            .contains("Expired provider token")
    );
    std::fs::write(&output, serde_json::to_vec_pretty(&cases).unwrap()).unwrap();
    let variants = [
        (
            "standalone-translation",
            "Translate good morning into Swedish.",
            json!({"action":"new","jobId":null}),
        ),
        (
            "different-branch-answer",
            "For CI, work from the release branch.",
            json!({"action":"amend","jobId":ci.event_id.as_str()}),
        ),
        (
            "unrelated-conversion",
            "How many minutes are in three hours?",
            json!({"action":"new","jobId":null}),
        ),
        (
            "paraphrased-scope-change",
            "For the documentation work, fix typos only.",
            json!({"action":"amend","jobId":readme.event_id.as_str()}),
        ),
        (
            "ambiguous-stop",
            "Stop one of those tasks, please.",
            json!({"action":"clarify","jobId":null}),
        ),
    ];
    let mut heldout = Vec::new();
    for (name, text, expected) in variants {
        heldout.push(routing_case(&store, &mut agent, name, text, expected).await);
    }
    std::fs::write(
        PathBuf::from(output).with_extension("heldout.json"),
        serde_json::to_vec_pretty(&heldout).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_clarification_preserves_other_jobs_and_sends_specific_question() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let first = observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Fix CI tests"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    scripted(
        &store,
        model_requests(&store).await.last().unwrap(),
        yield_control(json!({"action":"continue","note":"Run tests"})),
    )
    .await;
    drive(&mut agent, 1).await;
    observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Cancel it"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    let before = projection(&store).await["jobs"][first.event_id.as_str()].clone();
    scripted(
        &store,
        model_requests(&store).await.last().unwrap(),
        json!([{"kind":"tool-call","name":"associate","call_id":"clarify","arguments":{"action":"clarify","jobId":null,"question":"Cancel the CI test work or only the pending test run?"}}]),
    ).await;
    drive(&mut agent, 1).await;
    assert_eq!(
        projection(&store).await["jobs"][first.event_id.as_str()]["status"],
        before["status"]
    );
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    let replies: Vec<_> = events
        .iter()
        .filter(|event| {
            event.request.event_type == "capability.requested"
                && payload(event)["capability"] == "telegram.reply"
        })
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        payload(replies[0])["arguments"]["text"],
        "Cancel the CI test work or only the pending test run?"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_router_cannot_ignore_user_input() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Fix CI tests"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    scripted(
        &store,
        model_requests(&store).await.last().unwrap(),
        yield_control(json!({"action":"continue"})),
    )
    .await;
    drive(&mut agent, 1).await;
    let incoming = observation(
        &store,
        json!({"message":{"chat":{"id":42},"text":"Reply with pong"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    let count = model_requests(&store).await.len();
    scripted(
        &store,
        model_requests(&store).await.last().unwrap(),
        json!([{"kind":"tool-call","name":"associate","call_id":"invalid-ignore","arguments":{"action":"ignore","jobId":null}}]),
    ).await;
    drive(&mut agent, 1).await;
    assert_eq!(model_requests(&store).await.len(), count + 1);
    assert_eq!(
        projection(&store).await["inbox"][incoming.event_id.as_str()]["status"],
        "pending"
    );
    let request = payload(model_requests(&store).await.last().unwrap());
    assert!(
        !request["tools"][0]["input_schema"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "ignore")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_record_partitions_rebuild_large_observations() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock.clone()))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({"archive":"x".repeat(600_000)})).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    let before = projection(&store).await;
    assert!(serde_json::to_vec(&before).unwrap().len() > 1024 * 1024);
    assert!(before["jobs"][origin.event_id.as_str()].is_object());
    drop(agent);
    let namespace = pluribus_core::StateNamespace::new("cognition");
    let page = store.scan(&namespace, "", None, 1000).await.unwrap();
    assert!(
        page.entries
            .iter()
            .all(|entry| entry.value.len() <= 128 * 1024)
    );
    store
        .apply(
            &namespace,
            page.revision,
            &page
                .entries
                .iter()
                .map(|entry| pluribus_core::StateMutation::Delete {
                    key: entry.key.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let _agent = persistent_agent(&store).await;
    assert_eq!(projection(&store).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_observation_burst_stays_within_mutation_limits() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock.clone()))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await.with_batch(200);
    for index in 0..100 {
        observation(
            &store,
            json!({"message":{"chat":{"id":index},"text":"work"}}),
        )
        .await;
    }
    for _ in 0..32 {
        agent
            .tick_wait(clock.load(Ordering::Relaxed))
            .await
            .unwrap();
    }
    assert_eq!(
        projection(&store).await["inbox"].as_object().unwrap().len(),
        100
    );
}

async fn operator(
    store: &Arc<SqliteEventStore<Metadata>>,
    kind: &str,
    value: &Value,
    authorized: bool,
) {
    let mut request = append(store, kind, value).await.request;
    request.actor = PrincipalRef::new(
        PrincipalKind::Node,
        if authorized {
            "operator:personal"
        } else {
            "operator:foreign"
        },
    );
    store.append(request).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_operator_controls_preserve_waits_and_require_identity() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock.clone()))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({})).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    let control = json!({"version":1,"jobId":origin.event_id.as_str(),"action":"cancel","revision":0,"reason":"stop"});
    operator(&store, "operator.job-control", &control, false).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "running"
    );
    let first = model_requests(&store).await.remove(0);
    scripted(
        &store,
        &first,
        yield_control(json!({"action":"wait","dueAtMs":1_700_000_060_000_i64})),
    )
    .await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "waiting-time"
    );
    let count = model_requests(&store).await.len();
    append(
        &store,
        "activity.unknown",
        &json!({"requestEventId":first.event_id.as_str(),"reason":"connection lost"}),
    )
    .await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    operator(
        &store,
        "operator.job-control",
        &json!({"version":1,"jobId":origin.event_id.as_str(),"action":"resume","revision":0,"reason":"continue"}),
        true,
    ).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(model_requests(&store).await.len(), count);
    operator(
        &store,
        "operator.attempt-reconciled",
        &json!({"version":1,"requestEventId":first.event_id.as_str(),"outcome":"completed","output":{"verified":true},"reason":"checked"}),
        true,
    ).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(model_requests(&store).await.len(), count);
    operator(
        &store,
        "operator.job-control",
        &json!({"version":1,"jobId":origin.event_id.as_str(),"action":"resume","revision":0,"reason":"continue"}),
        true,
    ).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    let requests = model_requests(&store).await;
    assert_eq!(requests.len(), count + 1);
    assert_eq!(
        requests.last().unwrap().request.causation_id,
        Some(origin.event_id.clone())
    );
    operator(
        &store,
        "operator.job-control",
        &json!({"version":1,"jobId":origin.event_id.as_str(),"action":"cancel","revision":1,"reason":"stop"}),
        true,
    ).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "cancelled"
    );
}
