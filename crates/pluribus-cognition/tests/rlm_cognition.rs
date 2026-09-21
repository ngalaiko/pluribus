//! A pinned session must keep its interpreter heap across deliveries.

#[path = "support/memory_eval_scoring.rs"]
mod memory_eval_scoring;

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
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
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
            grants: ["system.echo", "telegram.reply", "shell.execute"]
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

fn event_request(event_type: &str, payload: &Value) -> AppendRequest {
    AppendRequest {
        stream_id: StreamId::new("personal"),
        stream_kind: StreamKind::Agent,
        observed_at_ms: None,
        event_type: event_type.into(),
        payload_schema: "dev.pluribus.repl.cell/1".into(),
        payload: EventPayload::CanonicalJson(serde_json::to_vec(payload).unwrap()),
        actor: PrincipalRef::new(PrincipalKind::Agent, "personal"),
        authority_id: Some(AuthorityId::new("authority-1")),
        activity_id: Some("activity-1".into()),
        correlation_id: Some("correlation-1".into()),
        causation_id: None,
        deduplication_key: None,
    }
}

async fn append(
    store: &Arc<SqliteEventStore<Metadata>>,
    event_type: &str,
    payload: &Value,
) -> CommittedEvent {
    store
        .append(event_request(event_type, payload))
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
        ("cognition", "rlm", json!({"repl":{}})),
        ("code", "repl", json!({})),
    ] {
        agent
            .install_component(
                PluginPackage::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/plugins/{}",
                    if path == "repl" { "rlm" } else { path }
                )))
                .unwrap()
                .component(if path == "rlm" { "cognition" } else { "repl" })
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
                PluginServices {
                    model: Some("test".into()),
                    ..PluginServices::default()
                },
                &[],
            )
            .await
            .unwrap();
    }
    let mut origin = event_request(
        "observation.received",
        &json!({"provider":"telegram","externalSenderId":"7","message":{"text":"compute"}}),
    );
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
            assert_eq!(payload(request)["model"], "test");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_guest_oom_replans_and_completes_smaller_read() {
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
    let first = model_requests(&store).await.pop().unwrap();
    scripted(&store,&first,json!([{"kind":"tool-call","name":"js","call_id":"large","arguments":{"code":"await history.read({after:0,limit:1}); return 'x'.repeat(100000000);"}}])).await;
    drive(&mut agent, 1).await;
    let state = projection(&store).await;
    assert_eq!(
        state["tasks"][origin.event_id.as_str()]["resource_failures"],
        1,
        "{state}"
    );
    let retry = model_requests(&store).await.pop().unwrap();
    assert_ne!(first.event_id, retry.event_id);
    assert_eq!(
        state["tasks"][origin.event_id.as_str()]["context"]["resourceRecovery"]["readLimit"],
        16
    );
    scripted(&store,&retry,json!([{"kind":"tool-call","name":"js","call_id":"small","arguments":{"code":"return (await history.read({after:0,limit:1000})).events.length;"}}])).await;
    drive(&mut agent, 10000).await;
    let completed = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.request.event_type == "code.completed")
        .last()
        .expect("smaller read completed");
    assert!(
        payload(&completed)["value"]
            .as_u64()
            .is_some_and(|n| (1..=16).contains(&n)),
        "{}",
        payload(&completed)
    );
    let request = model_requests(&store).await.pop().unwrap();
    scripted(
        &store,
        &request,
        yield_control(json!({"action":"complete"})),
    )
    .await;
    drive(&mut agent, 10000).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "completed"
    );
    assert!(agent.failed().is_empty());
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
            json!({"credentials": {"api-key": "test"},"models":["test"]}),
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
            json!({"repl":{}}),
            PluginServices {
                model: Some("test".into()),
                ..PluginServices::default()
            },
            vec![],
        ),
    ] {
        agent
            .install_component(
                PluginPackage::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/plugins/{}",
                    if plugin == "repl" { "rlm" } else { plugin }
                )))
                .unwrap()
                .component(match plugin {
                    "rlm" => "cognition",
                    "repl" => "repl",
                    "echo" => "",
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
    let mut observation = event_request(
        "observation.received",
        &json!({"message":{"text":"independent"}}),
    );
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
        ("cognition", "rlm", json!({"repl":{}})),
        ("code", "repl", json!({})),
        ("echo", "echo", json!({})),
    ] {
        agent
            .install_component(
                PluginPackage::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                    "../../target/plugins/{}",
                    if plugin == "repl" { "rlm" } else { plugin }
                )))
                .unwrap()
                .component(match plugin {
                    "rlm" => "cognition",
                    "repl" => "repl",
                    "echo" => "",
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
                PluginServices {
                    model: Some("test".into()),
                    ..PluginServices::default()
                },
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
    let mut request = event_request("observation.received", &value);
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
    for _ in 0..64 {
        if agent.tick_wait(now).await.unwrap().is_idle() {
            return;
        }
    }
    panic!("agent did not become idle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_jobs_rebuild_corrections_and_wakes() {
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
        yield_control(json!({"action":"wait","dueAtMs":clock.load(Ordering::Relaxed) + 60_000,"note":"External deadline"})),
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
        yield_control(json!({"action":"wait","dueAtMs":clock.load(Ordering::Relaxed) + 120_000})),
    )
    .await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    clock.fetch_add(120_000, Ordering::Relaxed);
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "running"
    );
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
    assert_eq!(
        replies.len(),
        1,
        "projection: {}; events: {:?}",
        projection(&store).await,
        events
            .iter()
            .map(|e| (&e.request.event_type, payload(e)))
            .collect::<Vec<_>>()
    );
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
async fn packaged_history_filters_events() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({"message":{"text":"history"}})).await;
    drive(&mut agent, 1).await;
    scripted(&store, &model_requests(&store).await[0], json!([{"kind":"tool-call","name":"js","call_id":"history","arguments":{"code":"const observations=await history.read({after:0,limit:100,eventTypes:['observation.received']}); const internal=await history.read({after:0,limit:100,eventTypes:['cognition.checkpoint']}); return {observations,internal};"}}])).await;
    drive(&mut agent, 1).await;
    let rows = store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap();
    let resumed = rows
        .iter()
        .find(|e| e.request.event_type == "code.resumed")
        .unwrap();
    let result = payload(resumed);
    let events = result["response"]["value"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["eventId"], origin.event_id.as_str());
    assert_eq!(events[0]["actorId"], origin.request.actor.id.as_str());
    let internal = rows
        .iter()
        .filter(|e| e.request.event_type == "code.resumed")
        .nth(1)
        .unwrap();
    let value = payload(internal);
    let checkpoints = value["response"]["value"]["events"].as_array().unwrap();
    assert!(!checkpoints.is_empty());
    assert!(
        checkpoints
            .iter()
            .all(|e| e["type"] == "cognition.checkpoint")
    );
    assert!(
        serde_json::to_vec(&value["response"]["value"])
            .unwrap()
            .len()
            <= 64 * 1024
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_history_search_recovers_old_evidence_and_exhausts_pages() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut expected = Vec::new();
    for index in 0..12 {
        let conversation = if index % 2 == 0 {
            "chat:1"
        } else {
            "chat:other"
        };
        let event = append(&store, "test.history", &json!({
            "conversationId":conversation,"message":{"text":format!("deployment decision {index}")}
        })).await;
        if index % 2 == 0 {
            expected.push(event.sequence);
        }
    }
    expected.reverse();
    let mut agent = persistent_agent(&store).await;
    observation(
        &store,
        json!({"message":{"text":"Recover earlier deployment decisions"}}),
    )
    .await;
    drive(&mut agent, 1).await;
    scripted(&store, &model_requests(&store).await[0], json!([
        {"kind":"tool-call","name":"js","call_id":"search","arguments":{"code":
            r"
            for (const args of [{query:'x'.repeat(4097)}, {query:''}, {query:'x',limit:0}, {query:'x',before:-1}, {query:'x',recordedFromMs:2,recordedToMs:1}]) {
                let rejected=false;
                try { await history.search(args); } catch { rejected=true; }
                if (!rejected) throw Error('invalid search accepted');
            }
            const found=[]; let before;
            for (let pageNumber=0; pageNumber<10; pageNumber++) {
                const page=await history.search({query:'deployment',conversationId:'chat:1',eventTypes:['test.history'],limit:2,...(before===undefined?{}:{before})});
                found.push(...page.events.map(event=>event.sequence));
                if (page.nextBefore===null) return {found,exhausted:true};
                if (page.nextBefore===before) throw Error('search cursor did not advance');
                before=page.nextBefore;
            }
            throw Error('search did not exhaust');
            "
        }}
    ])).await;
    drive(&mut agent, 1).await;
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    let done = events
        .iter()
        .find(|event| event.request.event_type == "code.completed");
    assert!(
        done.is_some(),
        "history.search must finish with a terminal cursor"
    );
    assert_eq!(
        payload(done.unwrap())["value"],
        json!({"found":expected,"exhausted":true})
    );
    assert!(!events.iter().any(|event| matches!(
        event.request.event_type.as_str(),
        "code.failed" | "component.failed"
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_automatic_checkpoint_restores_state_after_restart() {
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
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.request.event_type == "code.completed" && payload(e)["value"] == 42)
            .count(),
        2,
        "state must survive restart without an explicit checkpoint"
    );
    assert!(!events.iter().any(|e| e.request.event_type == "code.failed"));
}

fn yield_control(arguments: Value) -> Value {
    let mut content = json!([{"kind":"tool-call","name":"yield","call_id":"decision"}]);
    content[0]["arguments"] = arguments;
    content
}

struct MemoryEvalCase {
    id: &'static str,
    key: &'static str,
    initial: &'static str,
    expected: &'static str,
    stale: &'static [&'static str],
}

fn memory_eval_cases() -> [MemoryEvalCase; 5] {
    [
        MemoryEvalCase {
            id: "recall-after-distraction",
            key: "launch codename",
            initial: "northstar",
            expected: "northstar",
            stale: &[],
        },
        MemoryEvalCase {
            id: "correction-resists-stale-fact",
            key: "deployment region",
            initial: "us-east",
            expected: "eu-north",
            stale: &["us-east"],
        },
        MemoryEvalCase {
            id: "compaction-preserves-memory",
            key: "storage decision",
            initial: "sqlite",
            expected: "sqlite",
            stale: &[],
        },
        MemoryEvalCase {
            id: "restart-restores-memory",
            key: "reply style",
            initial: "terse replies",
            expected: "terse replies",
            stale: &[],
        },
        MemoryEvalCase {
            id: "source-validity",
            key: "retained object",
            initial: "blue kettle",
            expected: "blue kettle",
            stale: &[],
        },
    ]
}

fn run_memory_provider(request: &CommittedEvent) -> Value {
    let command = std::env::var("PLURIBUS_MEMORY_EVAL_PROVIDER_CMD").unwrap();
    let mut body = payload(request);
    body["model"] =
        json!(std::env::var("PLURIBUS_MEMORY_EVAL_MODEL").expect("set PLURIBUS_MEMORY_EVAL_MODEL"));
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        serde_json::to_writer(&mut input, &body).unwrap();
        input.write_all(b"\n").unwrap();
    });
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let read = |stream: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stream.take(1_048_577).read_to_end(&mut bytes).unwrap();
            bytes
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("provider bridge exceeded 120 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    writer.join().unwrap();
    let output = out.join().unwrap();
    let error = err.join().unwrap();
    assert!(
        status.success(),
        "provider bridge failed: {}",
        String::from_utf8_lossy(&error)
    );
    assert!(
        output.len() <= 1_048_576,
        "provider completion exceeds 1 MiB"
    );
    let completion: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        completion["call_id"],
        payload(request)["call_id"],
        "provider returned wrong call_id"
    );
    let _: pluribus_model::Completion =
        serde_json::from_value(completion.clone()).expect("canonical provider completion");
    completion
}

#[derive(Default)]
struct MemoryEvalRun {
    handled: std::collections::BTreeSet<String>,
    completions: Vec<Value>,
    forced_cells: usize,
    compacted: bool,
    live: bool,
}

async fn drive_eval(agent: &mut Agent<AllowAll, TestAuthority>) {
    tokio::time::timeout(std::time::Duration::from_mins(1), drive(agent, 1))
        .await
        .expect("memory evaluation runtime did not become idle within 60 seconds");
}

async fn finish_eval_turn(
    store: &Arc<SqliteEventStore<Metadata>>,
    agent: &mut Agent<AllowAll, TestAuthority>,
    run: &mut MemoryEvalRun,
    origin: &CommittedEvent,
    scripted_reply: &str,
    force_compaction: bool,
) -> String {
    let mut compact_call = false;
    for step in 0..48 {
        drive_eval(agent).await;
        if compact_call {
            let state = projection(store).await;
            run.compacted |= state["tasks"].as_object().is_some_and(|tasks| {
                tasks.values().any(|task| {
                    task["context"]["workingSummaryProvenance"]["verified"] == true
                        && task["context"]["workingSummary"].is_object()
                })
            });
        }
        let events = store
            .read(&StreamId::new("personal"), origin.sequence, 10000)
            .await
            .unwrap();
        if let Some(reply) = events.iter().rev().find_map(|event| {
            let value = payload(event);
            (event.request.event_type == "capability.requested"
                && value["capability"] == "telegram.reply")
                .then(|| value["arguments"]["text"].as_str().map(str::to_owned))
                .flatten()
        }) {
            return reply;
        }
        let Some(request) = model_requests(store)
            .await
            .into_iter()
            .find(|event| !run.handled.contains(event.event_id.as_str()))
        else {
            panic!("evaluation stalled without reply or model request at step {step}");
        };
        run.handled.insert(request.event_id.as_str().to_owned());
        let body = payload(&request);
        let tools = body["tools"].as_array().unwrap();
        let compact = tools.iter().any(|tool| tool["name"] == "compact");
        let routing = tools.iter().any(|tool| tool["name"] == "associate");
        compact_call |= compact;
        let force_cell =
            force_compaction && !routing && !compact && !run.compacted && run.forced_cells < 12;
        let completion = if force_cell {
            run.forced_cells += 1;
            json!({"call_id":body["call_id"],"message":{"role":"assistant","content":[{"kind":"tool-call","name":"js","call_id":format!("pressure-{}",run.forced_cells),"arguments":{"code":format!("/*{}*/ return {{step:{}}};", "x".repeat(6000), run.forced_cells)}}]},"stop_reason":{"kind":"tool-call"}})
        } else if run.live {
            let request = request.clone();
            let completion = tokio::task::spawn_blocking(move || run_memory_provider(&request))
                .await
                .unwrap();
            run.completions.push(completion.clone());
            completion
        } else {
            let content = if routing {
                json!([{"kind":"tool-call","name":"associate","call_id":"route","arguments":{"action":"new","jobId":null}}])
            } else if compact {
                json!([{"kind":"tool-call","name":"compact","call_id":"compact","arguments":{"version":1,"objective":"answer the memory question","constraints":[],"decisions":[],"completedWork":[],"unresolvedQuestions":[],"durableFacts":[],"corrections":[],"sourceIds":[origin.event_id.as_str()]}}])
            } else {
                yield_control(json!({"action":"complete","reply":scripted_reply}))
            };
            json!({"call_id":body["call_id"],"message":{"role":"assistant","content":content},"stop_reason":{"kind":"tool-call"}})
        };
        let mut event = event_request("model.completed", &completion);
        event.causation_id = Some(request.event_id.clone());
        store.append(event).await.unwrap();
    }
    panic!("memory evaluation exceeded 48 model steps");
}

async fn run_memory_eval_case(case: &MemoryEvalCase, live: bool) -> Value {
    let started = std::time::Instant::now();
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let mut run = MemoryEvalRun {
        live,
        ..Default::default()
    };
    let source = observation(&store, json!({"message":{"chat":{"id":42},"text":format!("Remember this decision: {} is {}. Acknowledge briefly.",case.key,case.initial)}})).await;
    finish_eval_turn(&store, &mut agent, &mut run, &source, "acknowledged", false).await;
    let mut evidence = source.event_id.as_str().to_owned();
    if !case.stale.is_empty() {
        let correction = observation(&store, json!({"message":{"chat":{"id":42},"text":format!("Correction: {} is {}. This replaces the earlier decision. Acknowledge briefly.",case.key,case.expected)}})).await;
        finish_eval_turn(
            &store,
            &mut agent,
            &mut run,
            &correction,
            "corrected",
            false,
        )
        .await;
        evidence = correction.event_id.as_str().to_owned();
    }
    for index in 0..8 {
        let distraction = observation(&store, json!({"message":{"chat":{"id":42},"text":format!("Unrelated task {index}: what is {} plus 1? Reply with the number.",index+10)}})).await;
        finish_eval_turn(
            &store,
            &mut agent,
            &mut run,
            &distraction,
            &(index + 11).to_string(),
            false,
        )
        .await;
    }
    let restarted = case.id == "restart-restores-memory";
    if restarted {
        drop(agent);
        agent = persistent_agent(&store).await;
    }
    let question = observation(&store, json!({"message":{"chat":{"id":42},"text":format!("What is the current {}? Recover the original observation as evidence. Reply only with JSON: {{\"answer\":\"the exact value\",\"sourceIds\":[\"the supporting observation event ID\"]}}. Cite the correction when one exists.",case.key)}})).await;
    let fixture_reply = json!({"answer":case.expected,"sourceIds":[evidence]}).to_string();
    let requires_compaction = case.id == "compaction-preserves-memory";
    let reply = finish_eval_turn(
        &store,
        &mut agent,
        &mut run,
        &question,
        &fixture_reply,
        requires_compaction,
    )
    .await;
    let mut result = memory_eval_scoring::score_case(
        case.id,
        &reply,
        case.expected,
        case.stale,
        &[evidence],
        memory_eval_scoring::Transitions {
            requires_compaction,
            compacted: run.compacted,
            requires_restart: restarted,
            restarted,
        },
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        &run.completions,
    );
    result["scriptedPressureCells"] = json!(run.forced_cells);
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit live provider bridge and model configuration"]
async fn memory_evaluation_uses_packaged_cognition_and_provider_path() {
    if std::env::var("PLURIBUS_MEMORY_EVAL_LIVE").as_deref() != Ok("1") {
        eprintln!("memory evaluation disabled; set PLURIBUS_MEMORY_EVAL_LIVE=1");
        return;
    }
    for key in [
        "PLURIBUS_MEMORY_EVAL_PROVIDER_CMD",
        "PLURIBUS_MEMORY_EVAL_MODEL",
    ] {
        assert!(
            std::env::var(key).is_ok_and(|value| !value.trim().is_empty()),
            "set {key}"
        );
    }
    let mut cases = Vec::new();
    for case in memory_eval_cases() {
        cases.push(run_memory_eval_case(&case, true).await);
    }
    let report = json!({"schema":"pluribus.memory-evaluation/1","mode":"live","cases":cases});
    let encoded = serde_json::to_string_pretty(&report).unwrap();
    if let Ok(path) = std::env::var("PLURIBUS_MEMORY_EVAL_OUTPUT") {
        std::fs::write(path, &encoded).unwrap();
    }
    println!("{encoded}");
    assert!(
        cases.iter().all(|case| case["valid"] == true),
        "memory evaluation failed; inspect report"
    );
}

macro_rules! memory_workflow_test {
    ($name:ident, $index:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            let result = run_memory_eval_case(&memory_eval_cases()[$index], false).await;
            assert_eq!(result["valid"], true, "{result}");
        }
    };
}

memory_workflow_test!(memory_evaluation_recall_workflow, 0);
memory_workflow_test!(memory_evaluation_correction_workflow, 1);
memory_workflow_test!(memory_evaluation_compaction_workflow, 2);
memory_workflow_test!(memory_evaluation_restart_workflow, 3);
memory_workflow_test!(memory_evaluation_citation_workflow, 4);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_summary_sources_are_host_verified_without_mutating_checkpoint_state() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock.clone()))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), drive(&mut agent, 1))
        .await
        .expect("initial drive timeout");
    let request = model_requests(&store).await.pop().unwrap();
    let summary = |source: &str| json!({"version":1,"objective":"x","constraints":[],"decisions":[],"completedWork":[],"unresolvedQuestions":[],"durableFacts":[],"corrections":[],"sourceIds":[source]});
    let bad = summary("fabricated-source");
    scripted(&store, &request, json!([{"kind":"tool-call","call_id":"bad-js","name":"js","arguments":{"code":format!("var summary={}; checkpoint({{workingSummary:summary}}); return 1;", bad)}}])).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), drive(&mut agent, 1))
        .await
        .expect("bad drive timeout");
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    let checkpoint = events
        .iter()
        .find(|event| {
            event.request.event_type == "code.completed"
                && payload(event)["checkpoint"]["state"]["workingSummary"] == bad
        })
        .expect("checkpoint preserved");
    assert_eq!(
        payload(checkpoint)["checkpoint"]["state"]["workingSummary"],
        bad
    );
    let state = projection(&store).await;
    assert!(
        state["tasks"][origin.event_id.as_str()]["context"]
            .get("workingSummary")
            .is_none()
    );
    assert!(state["tasks"][origin.event_id.as_str()]["context"]["workingSummaryError"].is_string());
    let request = model_requests(&store).await.last().unwrap().clone();
    let good = summary(origin.event_id.as_str());
    scripted(&store, &request, json!([{"kind":"tool-call","call_id":"good-js","name":"js","arguments":{"code":format!("var summary={}; checkpoint({{workingSummary:summary}}); return 1;", good)}}])).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), drive(&mut agent, 1))
        .await
        .expect("good drive timeout");
    assert_eq!(
        projection(&store).await["tasks"][origin.event_id.as_str()]["context"]["workingSummary"],
        good
    );
    drop(agent);
    let _restored = persistent_agent(&store).await;
    assert_eq!(
        projection(&store).await["tasks"][origin.event_id.as_str()]["context"]["workingSummary"],
        good
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_unsaved_state_prevents_execution_after_restart() {
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
    scripted(&store, &model_requests(&store).await[0], json!([
        {"kind":"tool-call","name":"js","call_id":"save","arguments":{"code":"state.n=1; return 1;"}}
    ])).await;
    drive(&mut agent, 1).await;
    scripted(&store, &model_requests(&store).await.pop().unwrap(), json!([
        {"kind":"tool-call","name":"js","call_id":"invalidate","arguments":{"code":"state.n=2; state.callback=()=>42; return 42;"}}
    ])).await;
    drive(&mut agent, 1).await;
    let next = model_requests(&store).await.pop().unwrap();
    drop(agent);
    let mut agent = persistent_agent(&store).await;
    scripted(&store, &next, json!([
        {"kind":"tool-call","name":"js","call_id":"restore","arguments":{"code":"await capabilities.invoke('system.echo',{text:'must not run'}); return 42;"}}
    ])).await;
    drive(&mut agent, 1).await;
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    assert!(events.iter().any(|e| {
        e.request.event_type == "code.failed"
            && payload(e)["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("session was lost"))
    }));
    assert!(
        !events
            .iter()
            .any(|e| e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "system.echo")
    );
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
        yield_control(
            json!({"action":"wait","dueAtMs":1_800_000_000_000_i64,"note":"External deadline"}),
        ),
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
        yield_control(json!({"action":"wait","dueAtMs":1_800_000_000_000_i64})),
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
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        drive(&mut agent, clock.load(Ordering::Relaxed)),
    )
    .await;
    let before = projection(&store).await;
    assert!(
        store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .iter()
            .all(|event| event.request.event_type != "component.failed"),
        "large observations must fit the default Wasm memory budget"
    );
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
async fn packaged_concurrent_jobs_fit_default_memory_limit() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock.clone()))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    for chat in 0..8 {
        let incoming = observation(
            &store,
            json!({"conversationId":format!("chat:{chat}"),"message":{"chat":{"id":chat},"text":"work"},"archive":(0..1500).map(|id| json!({"id":id,"status":"completed","usage":{"input_tokens":100,"output_tokens":10}})).collect::<Vec<_>>()}),
        ).await;
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            drive(&mut agent, clock.load(Ordering::Relaxed)),
        )
        .await;
        assert!(
            projection(&store).await["jobs"][incoming.event_id.as_str()].is_object(),
            "job {chat} must be persisted within the default Wasm memory budget"
        );
        let failures: Vec<_> = store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.request.event_type == "component.failed")
            .map(|event| payload(&event))
            .collect();
        assert!(failures.is_empty(), "job {chat}: {failures:?}");
    }
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

#[derive(Default)]
struct CancellableExecutor {
    opens: AtomicU64,
    entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl pluribus_core::StreamService for CancellableExecutor {
    async fn open(
        &self,
        _: &pluribus_core::StreamGrant,
    ) -> Result<String, pluribus_core::StreamError> {
        Ok(self.opens.fetch_add(1, Ordering::SeqCst).to_string())
    }
    async fn send(&self, _: &str, _: &[u8]) -> Result<(), pluribus_core::StreamError> {
        Ok(())
    }
    async fn next(
        &self,
        id: &str,
        _: u32,
    ) -> Result<pluribus_core::StreamPage, pluribus_core::StreamError> {
        if id == "0" {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(pluribus_core::StreamPage {bytes:b"{\"status\":\"completed\",\"stdout\":\"ok\",\"stderr\":\"\",\"exit_code\":0,\"truncated\":false}\n".to_vec(),closed:false})
    }
    fn shutdown_write(&self, _: &str) {}
    fn close(&self, _: &str) {}
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_shell_call_preserves_the_provider() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let executor = Arc::new(CancellableExecutor::default());
    install_cancellable_shell(&mut agent, &executor).await;
    let request = append(
        &store,
        "capability.requested",
        &json!({"capability":"shell.execute","arguments":{"command":"blocked"}}),
    )
    .await;
    agent.tick(1).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        executor.entered.notified(),
    )
    .await
    .unwrap();
    append(
        &store,
        "cognition.cancel-requested",
        &json!({"requestEventId":request.event_id.as_str()}),
    )
    .await;
    for _ in 0..3 {
        agent.tick_wait(2).await.unwrap();
    }
    let rows = store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap();
    assert!(
        rows.iter()
            .any(|e| e.request.event_type == "capability.cancelled")
    );
    assert!(!agent.failed().contains_key("shell"));
    append(
        &store,
        "capability.requested",
        &json!({"capability":"shell.execute","arguments":{"command":"next"}}),
    )
    .await;
    for _ in 0..3 {
        agent.tick_wait(3).await.unwrap();
    }
    let rows = store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap();
    assert!(rows.iter().any(|e| e.request.actor.id.as_str() == "shell"
        && e.request.event_type == "capability.completed"));
    assert_eq!(
        executor.opens.load(Ordering::SeqCst),
        2,
        "cancelled commands must not replay"
    );
    let mut failed = event_request(
        "component.failed",
        &json!({"instanceId":"shell","reason":"call cancelled","deliveredThrough":request.sequence}),
    );
    failed.actor = PrincipalRef::new(PrincipalKind::Node, "personal");
    failed.causation_id = Some(request.event_id);
    store.append(failed).await.unwrap();
    drop(agent);
    let mut agent = persistent_agent(&store).await;
    install_cancellable_shell(&mut agent, &executor).await;
    agent.tick_wait(4).await.unwrap();
    assert!(
        !agent.failed().contains_key("shell"),
        "restart must recover a cancellation recorded as a crash"
    );
}

async fn install_cancellable_shell(
    agent: &mut Agent<AllowAll, TestAuthority>,
    executor: &Arc<CancellableExecutor>,
) {
    let package = PluginPackage::load(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/shell"),
    )
    .unwrap();
    agent
        .install_component(
            package.component("main").unwrap(),
            &json!({}),
            Delivery {
                instance_id: "shell".into(),
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
                origin_event_id: "shell-init".into(),
                depth: 0,
                deadline_at_ms: None,
                visible_blobs: vec![],
            },
            PluginServices {
                stream: Some(executor.clone()),
                stream_grant: Some(pluribus_core::StreamGrant {
                    endpoint: pluribus_core::StreamEndpoint::Unix {
                        path: "/tmp/executor.sock".into(),
                        peer_uids: vec![0],
                    },
                    max_bytes: 65536,
                    max_timeout_ms: 30000,
                }),
                ..Default::default()
            },
            &[],
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_reasoning_preserves_large_state_across_budget_pause() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock.clone()))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({})).await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    let js = |source: &str| json!([{"kind":"tool-call","name":"js","call_id":"cell","arguments":{"code":source}}]);
    scripted(
        &store,
        model_requests(&store).await.last().unwrap(),
        js("state.large = 'x'.repeat(250000); return state.large.length;"),
    )
    .await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    for n in 1..32 {
        scripted(
            &store,
            model_requests(&store).await.last().unwrap(),
            js(&format!("return {{step:{n},length:state.large.length}};")),
        )
        .await;
        drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    }
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "paused-budget"
    );
    clock.fetch_add(60_000, Ordering::Relaxed);
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    scripted(
        &store,
        model_requests(&store).await.last().unwrap(),
        js("return {retained:state.large.length};"),
    )
    .await;
    drive(&mut agent, clock.load(Ordering::Relaxed)).await;
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.request.event_type == "code.completed"
                && payload(e)["value"] == json!({"retained":250_000}))
    );
    assert!(
        !events
            .iter()
            .any(|e| e.request.event_type == "code.close-requested")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packaged_empty_reasoning_reports_stall_without_timer() {
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
    for _ in 0..3 {
        scripted(
            &store,
            model_requests(&store).await.last().unwrap(),
            json!([]),
        )
        .await;
        drive(&mut agent, 1).await;
    }
    assert_eq!(
        projection(&store).await["jobs"][origin.event_id.as_str()]["status"],
        "failed"
    );
    let events = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap();
    assert!(!events.iter().any(|e| e.request.event_type == "timer.set"));
    assert!(events.iter().any(|e| {
        e.request.event_type == "capability.requested"
            && payload(e)["capability"] == "telegram.reply"
            && payload(e)["arguments"]["text"]
                .as_str()
                .is_some_and(|s| s.contains("stalled"))
    }));
}

/// Records the provider request bodies instead of reaching the network.
struct CapturingHttp {
    blobs: Arc<InMemoryBlobStore>,
    bodies: std::sync::Mutex<Vec<Value>>,
}
impl CapturingHttp {
    async fn capture(&self, request: &pluribus_core::HttpRequest) {
        use pluribus_core::BlobStore;
        let blob = request.body.clone().expect("provider sends a body");
        let mut bytes = Vec::new();
        loop {
            let chunk = self
                .blobs
                .read(&blob, bytes.len() as u64, 64 * 1024)
                .await
                .unwrap();
            bytes.extend(chunk.bytes);
            if chunk.eof {
                break;
            }
        }
        self.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&bytes).unwrap());
    }
}
#[async_trait::async_trait]
impl pluribus_core::HttpService for CapturingHttp {
    async fn send(
        &self,
        _: &pluribus_core::HttpGrant,
        request: &pluribus_core::HttpRequest,
    ) -> Result<pluribus_core::HttpResponse, pluribus_core::HttpError> {
        self.capture(request).await;
        Err(pluribus_core::HttpError::PermissionDenied("fixture".into()))
    }
}
#[async_trait::async_trait]
impl pluribus_core::HttpStreamService for CapturingHttp {
    async fn open_stream(
        &self,
        _: &pluribus_core::HttpGrant,
        _: pluribus_core::HttpStreamProtocol,
        request: &pluribus_core::HttpRequest,
    ) -> Result<String, pluribus_core::HttpError> {
        self.capture(request).await;
        Err(pluribus_core::HttpError::PermissionDenied("fixture".into()))
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
#[allow(clippy::too_many_lines)]
async fn a_photo_observation_reaches_the_provider_as_an_image_part() {
    use base64::Engine as _;
    use pluribus_core::BlobStore;
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(
            AtomicU64::new(1),
            Arc::new(AtomicI64::new(1_700_000_000_000)),
        ))
        .await
        .unwrap(),
    );
    let blobs = Arc::new(InMemoryBlobStore::default());
    let image: &[u8] = b"\x89PNG\r\n\x1a\nfixture image bytes";
    let upload = blobs
        .begin_put("image/png", Some(image.len() as u64))
        .await
        .unwrap();
    blobs.write(&upload, 0, image).await.unwrap();
    let blob = blobs.finish_put(&upload).await.unwrap();
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        blobs.clone(),
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
    let http = Arc::new(CapturingHttp {
        blobs: blobs.clone(),
        bodies: std::sync::Mutex::new(Vec::new()),
    });
    for (id, plugin, config, services, models) in [
        (
            "provider",
            "openrouter",
            json!({"credentials": {"api-key": "test"},"models":["test"]}),
            PluginServices {
                http: Some(http.clone()),
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
            vec!["test".to_owned()],
        ),
        (
            "cognition",
            "rlm",
            json!({"repl":{}}),
            PluginServices {
                model: Some("test".into()),
                ..PluginServices::default()
            },
            vec![],
        ),
    ] {
        agent
            .install_component(
                PluginPackage::load(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join(format!("../../target/plugins/{plugin}")),
                )
                .unwrap()
                .component(if plugin == "rlm" { "cognition" } else { "main" })
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
    let mut observation = event_request(
        "observation.received",
        &json!({"provider":"telegram","externalSenderId":"7","conversationId":"chat:42",
            "message":{"chat":{"id":42},"caption":"what is in this picture?"},
            "media":[{"kind":"photo","status":"ready","fileName":"photo.png","metadata":{},
                "blob":{"algorithm":blob.algorithm,"digest":blob.digest,"size":blob.size,"mediaType":blob.media_type}}]}),
    );
    observation.actor = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
    store.append(observation).await.unwrap();
    for _ in 0..64 {
        if !http.bodies.lock().unwrap().is_empty() {
            break;
        }
        if agent.tick_wait(1).await.unwrap().is_idle() {
            break;
        }
    }
    let request = model_requests(&store).await.remove(0);
    assert_eq!(payload(&request)["required_features"], json!(["vision"]));
    let body = http
        .bodies
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("provider received a request");
    let user = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    let parts = user["content"].as_array().unwrap();
    assert!(
        parts[0]["text"]
            .as_str()
            .unwrap()
            .contains("what is in this picture?")
    );
    let url = parts[1]["image_url"]["url"].as_str().unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(url.strip_prefix("data:image/png;base64,").unwrap())
            .unwrap(),
        image
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_payloads_do_not_expand_cognition_working_set() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock))
            .await
            .unwrap(),
    );
    let namespace = pluribus_core::StateNamespace::new("cognition");
    let mut mutations = vec![];
    for n in 0..96 {
        let id = format!("archived-{n}");
        let record = json!({"field":"jobs","key":id,"value":{
            "id":id,"objective":"unrelated archive","origin":id,"sources":[id],
            "revision":1,"incorporated_sequence":0,"status":"completed",
            "completion_conditions":null,"completed_steps":null,"next_step":null,
            "blockers":null,"notes":"x".repeat(200_000),"checkpoint":null,
            "checkpoint_version":0,"cycle_started_ms":0,"retry_count":0,"wake":null
        }});
        for (part, bytes) in serde_json::to_vec(&record)
            .unwrap()
            .chunks(128 * 1024)
            .enumerate()
        {
            mutations.push(pluribus_core::StateMutation::Set {
                key: format!("engine/record/jobs/{n:064x}/{part:08}"),
                value: bytes.to_vec(),
            });
        }
    }
    StateStore::apply(store.as_ref(), &namespace, 0, &mutations)
        .await
        .unwrap();
    let mut agent = persistent_agent(&store).await;
    observation(&store, json!({"conversationId":"new-conversation"})).await;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for n in 0..20 {
            agent.tick_wait(1_700_000_000_000 + n * 5000).await.unwrap();
            if !model_requests(&store).await.is_empty() {
                break;
            }
        }
    })
    .await
    .expect("bounded legacy migration must finish");
    assert!(
        !model_requests(&store).await.is_empty(),
        "unrelated archived payloads blocked cognition"
    );
    assert!(agent.failed().is_empty());
    let unchanged=StateStore::get(store.as_ref(),&namespace,"engine/record/jobs/0000000000000000000000000000000000000000000000000000000000000000/00000000").await.unwrap();
    assert!(
        unchanged.value.is_some(),
        "partial delivery deleted an unrelated archive"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_active_job_recovers_after_restart() {
    let clock = Arc::new(AtomicI64::new(1_700_000_000_000));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1), clock))
            .await
            .unwrap(),
    );
    let mut agent = persistent_agent(&store).await;
    let origin = observation(&store, json!({"conversationId":"active"})).await;
    agent.tick_wait(1).await.unwrap();
    drop(agent);

    let namespace = pluribus_core::StateNamespace::new("cognition");
    let mut after = None;
    let mut target = None;
    loop {
        let page = StateStore::scan(
            store.as_ref(),
            &namespace,
            "engine/record/jobs/",
            after.as_deref(),
            100,
        )
        .await
        .unwrap();
        for entry in &page.entries {
            if let Ok(mut record) = serde_json::from_slice::<Value>(&entry.value)
                && record["value"]["id"].as_str() == Some(origin.event_id.as_str())
            {
                record["value"]["notes"] = json!("x".repeat(600 * 1024));
                target = Some((entry.key.rsplit_once('/').unwrap().0.to_owned(), record));
            }
        }
        match page.next_key {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let (prefix, record) = target.expect("active job record");
    let bytes = serde_json::to_vec(&record).unwrap();
    let mut mutations = Vec::new();
    for (part, chunk) in bytes.chunks(128 * 1024).enumerate() {
        mutations.push(pluribus_core::StateMutation::Set {
            key: format!("{prefix}/{part:08}"),
            value: chunk.to_vec(),
        });
    }
    let revision = StateStore::get(store.as_ref(), &namespace, &format!("{prefix}/00000000"))
        .await
        .unwrap()
        .revision;
    StateStore::apply(store.as_ref(), &namespace, revision, &mutations)
        .await
        .unwrap();

    let mut restarted = persistent_agent(&store).await;
    observation(&store, json!({"conversationId":"new"})).await;
    for _ in 0..20 {
        restarted.tick_wait(1).await.unwrap();
        if !restarted.failed().contains_key("cognition") {
            break;
        }
    }
    let state = projection(&store).await;
    assert!(state["jobs"][origin.event_id.as_str()].is_object());
    assert!(state["tasks"][origin.event_id.as_str()]["context"]["resourceRecovery"].is_object());
    assert!(state["inbox"].as_object().is_some_and(|inbox| {
        inbox
            .values()
            .any(|value| value["value"]["conversationId"] == "new")
    }));
    assert!(!restarted.failed().contains_key("cognition"));
}
