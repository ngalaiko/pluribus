//! Memory dispatch, reconstruction, and scope enforcement.

use pluribus_cognition::{Agent, AuthorityResolver, Router};
use pluribus_core::{
    AppendRequest, Audience, Authority, AuthorityId, BlobStore, CapabilityName, CommittedEvent,
    ConstraintSet, DeliveryStore, EventId, EventMetadataSource, EventPayload, EventStore,
    EventTypeRegistry, Grant, InMemoryBlobStore, Origin, OriginKind, PrincipalKind, PrincipalRef,
    StateStore, StreamId, StreamKind,
};
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as RuntimePrincipalKind, Runtime,
    RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::collections::BTreeMap;
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

/// Grants exactly the listed capabilities to every request.
struct Standing(Vec<String>);

#[async_trait::async_trait]
impl AuthorityResolver for Standing {
    async fn resolve(&self, _event: &CommittedEvent) -> Result<Authority, String> {
        let mut grants: BTreeMap<CapabilityName, Vec<Grant>> = BTreeMap::new();
        for name in &self.0 {
            let capability = CapabilityName::new(name.clone());
            grants.insert(
                capability.clone(),
                vec![Grant {
                    capability,
                    provider: None,
                    constraints: ConstraintSet::canonical_json(
                        br#"{"scopes":["project:p"]}"#.to_vec(),
                    ),
                }],
            );
        }
        Ok(Authority {
            schema: Authority::SCHEMA.into(),
            authority_id: AuthorityId::new("authority-1"),
            agent: agent_principal(),
            origin: Origin {
                kind: OriginKind::Autonomous,
                principal: None,
                connector: None,
                conversation_id: None,
                source_event_id: EventId::new("origin-1"),
                trusted: true,
            },
            delegation_chain: Vec::new(),
            grants,
            audiences: Vec::<Audience>::new(),
            parent_authority: None,
            issued_at_ms: 0,
            expires_at_ms: None,
            max_depth: 4,
            current_depth: 0,
        })
    }
}

fn agent_principal() -> PrincipalRef {
    PrincipalRef::new(PrincipalKind::Agent, "personal")
}

fn package() -> PluginPackage {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/memory");
    PluginPackage::load(&path).unwrap()
}

fn delivery() -> Delivery {
    Delivery {
        instance_id: "memory-1".into(),
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

type TestAgent = Agent<pluribus_cognition::OriginConstraints, Standing>;

async fn build(grants: &[&str]) -> (TestAgent, Arc<SqliteEventStore<Metadata>>) {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let agent = build_on(&store, grants).await;
    (agent, store)
}

async fn build_on(store: &Arc<SqliteEventStore<Metadata>>, grants: &[&str]) -> TestAgent {
    let blobs: Arc<dyn BlobStore> = Arc::new(InMemoryBlobStore::default());
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(store) as Arc<dyn StateStore>,
        Arc::clone(store) as Arc<dyn EventStore>,
        blobs,
        Arc::clone(store) as Arc<dyn DeliveryStore>,
    )
    .unwrap();
    let router = Router::new(
        StreamId::new("personal"),
        agent_principal(),
        Arc::clone(store) as Arc<dyn EventStore>,
        Arc::new(EventTypeRegistry::core()),
        pluribus_cognition::OriginConstraints,
    );
    let mut agent = Agent::new(
        router,
        Standing(grants.iter().map(|name| (*name).to_owned()).collect()),
        runtime,
        Arc::clone(store) as Arc<dyn EventStore>,
        StreamId::new("personal"),
        agent_principal(),
    );
    agent
        .install_component(
            package().component("main").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
            &[],
        )
        .await
        .unwrap();
    agent
}

async fn request(
    store: &Arc<SqliteEventStore<Metadata>>,
    capability: &str,
    args: &Value,
) -> CommittedEvent {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "capability.requested".into(),
            payload_schema: "pluribus.capability-request/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({
                    "capability": capability,
                    "arguments": args,
                }))
                .unwrap(),
            ),
            actor: PrincipalRef::new(PrincipalKind::Component, "rlm-1"),
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

const CAPS: &[&str] = &[
    "memory.remember",
    "memory.recall",
    "memory.get",
    "memory.supersede",
    "memory.forget",
];
fn args(op: &str, source: &str) -> Value {
    json!({"operationId":op,"scope":"project:p","kind":"procedure","content":"Use Jujutsu", "sources":[source],"basis":"explicit"})
}
async fn settle(agent: &mut TestAgent, request: &CommittedEvent) -> CommittedEvent {
    for _ in 0..200 {
        agent.tick_wait(1_700_000_000_000).await.unwrap();
        if let Some(result) = agent.result_for(&request.event_id).await.unwrap() {
            return result;
        }
    }
    panic!("request did not settle");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn memory_round_trip_rebuild_and_constraints() {
    let (mut agent, store) = build(CAPS).await;
    let source = request(&store, "other", &json!({})).await;
    let source_id = source.event_id.as_str();
    let write = request(&store, "memory.remember", &args("first", source_id)).await;
    let first = settle(&mut agent, &write).await;
    assert_eq!(
        first.request.event_type,
        "capability.completed",
        "{}",
        payload(&first)
    );
    let id = payload(&first)["output"]["id"].clone();
    // More mutation events than one replay page.
    for n in 0..70 {
        let write = request(
            &store,
            "memory.remember",
            &args(&format!("op-{n}"), source_id),
        )
        .await;
        assert_eq!(
            settle(&mut agent, &write).await.request.event_type,
            "capability.completed"
        );
    }
    let denied = request(
        &store,
        "memory.recall",
        &json!({"scope":"family","query":"Jujutsu"}),
    )
    .await;
    assert_eq!(
        settle(&mut agent, &denied).await.request.event_type,
        "capability.denied"
    );
    let foreign = {
        let mut r = source.request.clone();
        r.stream_id = StreamId::new("family");
        store.append(r).await.unwrap()
    };
    let bad = request(
        &store,
        "memory.remember",
        &args("foreign", foreign.event_id.as_str()),
    )
    .await;
    assert_eq!(
        payload(&settle(&mut agent, &bad).await)["code"],
        "source-unavailable"
    );
    let mut replacement = args("correct", source_id);
    replacement["expectedId"] = id.clone();
    replacement["content"] = json!("Never push");
    let correction = request(&store, "memory.supersede", &replacement).await;
    let current = payload(&settle(&mut agent, &correction).await)["output"]["id"].clone();
    let forget = request(
        &store,
        "memory.forget",
        &json!({"operationId":"forget","scope":"project:p","expectedId":current}),
    )
    .await;
    settle(&mut agent, &forget).await;
    let event = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.request.event_type == "memory.remembered")
        .unwrap();
    let mut spoof = event.request.clone();
    spoof.actor = PrincipalRef::new(PrincipalKind::Component, "impostor");
    spoof.deduplication_key = None;
    let mut change = payload(&event);
    change["operationId"] = json!("forged");
    change["record"]["record"]["id"] = json!("forged-id");
    change["record"]["root"] = json!("forged-id");
    spoof.payload = EventPayload::CanonicalJson(serde_json::to_vec(&change).unwrap());
    store.append(spoof).await.unwrap();
    drop(agent);
    store
        .discard(&pluribus_core::CursorKey {
            stream_id: StreamId::new("personal"),
            namespace: pluribus_core::StateNamespace::new("memory-1"),
        })
        .await
        .unwrap();
    // The queued read must observe replayed tombstones and records.
    let read = request(
        &store,
        "memory.get",
        &json!({"scope":"project:p","ids":[id,current,"forged-id"]}),
    )
    .await;
    let before = store
        .read(&StreamId::new("personal"), 0, 10000)
        .await
        .unwrap()
        .len();
    let commits = Arc::new(RejectCommit {
        store: Arc::clone(&store),
        reject: std::sync::atomic::AtomicBool::new(false),
        remaining: AtomicU64::new(u64::MAX),
    });
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(&store) as _,
        Arc::clone(&store) as _,
        Arc::new(InMemoryBlobStore::default()),
        Arc::clone(&commits) as _,
    )
    .unwrap();
    let mut recovering = runtime
        .instantiate(
            package().component("main").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    recovering.init().await.unwrap();
    commits.remaining.store(1, Ordering::Relaxed);
    assert!(
        recovering
            .rebuild(&package().component("main").unwrap().manifest().rebuilds)
            .await
            .is_err()
    );
    assert_eq!(recovering.checkpoint(), 0);
    assert!(
        StateStore::get(
            store.as_ref(),
            &pluribus_core::StateNamespace::new("memory-1"),
            "__host/rebuild"
        )
        .await
        .unwrap()
        .value
        .is_some()
    );
    commits.remaining.store(u64::MAX, Ordering::Relaxed);
    recovering
        .rebuild(&package().component("main").unwrap().manifest().rebuilds)
        .await
        .unwrap();
    drop(recovering);
    let mut agent = build_on(&store, CAPS).await;
    assert_eq!(
        store
            .read(&StreamId::new("personal"), 0, 10000)
            .await
            .unwrap()
            .len(),
        before
    );
    let output = payload(&settle(&mut agent, &read).await)["output"].clone();
    assert_eq!(output["records"], json!([]));
    assert_eq!(output["unavailableIds"], json!([id, current, "forged-id"]));
    let retry = request(&store, "memory.remember", &args("first", source_id)).await;
    assert_eq!(
        payload(&settle(&mut agent, &retry).await)["output"]["id"],
        id
    );
    let recall = request(
        &store,
        "memory.recall",
        &json!({"scope":"project:p","query":"Jujutsu"}),
    )
    .await;
    assert_eq!(
        payload(&settle(&mut agent, &recall).await)["output"]["records"]
            .as_array()
            .unwrap()
            .len(),
        8
    );
    assert!(
        agent
            .install_component(
                package().component("main").unwrap(),
                &json!({}),
                delivery(),
                PluginServices::default(),
                &[]
            )
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn rlm_recalls_remembers_and_replies_through_memory_events() {
    let (mut agent, store) = build(CAPS).await;
    for (id, plugin, config) in [
        ("rlm", "rlm", json!({"repl":{}})),
        ("repl", "repl", json!({})),
    ] {
        let mut d = delivery();
        d.instance_id = id.into();
        let package = PluginPackage::load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/plugins/{}",
            if plugin == "repl" { "rlm" } else { plugin }
        )))
        .unwrap();
        agent
            .install_component(
                package
                    .component(if plugin == "rlm" { "cognition" } else { "repl" })
                    .unwrap(),
                &config,
                d,
                PluginServices {
                    model: Some("test".into()),
                    ..PluginServices::default()
                },
                &[],
            )
            .await
            .unwrap();
    }
    let mut origin = request(&store, "unused", &json!({})).await.request;
    origin.event_type = "observation.received".into();
    origin.actor = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
    origin.payload = EventPayload::CanonicalJson(serde_json::to_vec(&json!({"provider":"telegram","externalSenderId":"7","conversationId":"chat:7","message":{"chat":{"id":7},"text":"Remember to use Jujutsu"}})).unwrap());
    store.append(origin).await.unwrap();
    let scripts = [
        Some(
            "return (await capabilities.invoke('memory.recall',{scope:'project:p',query:'Jujutsu'})).output;",
        ),
        Some(
            "state.saved=(await capabilities.invoke('memory.remember',{operationId:context.observationEventId,scope:'project:p',kind:'procedure',content:'Use Jujutsu',sources:[context.observationEventId],basis:'explicit'})).output; return state.saved;",
        ),
        Some(
            "const found=(await capabilities.invoke('memory.recall',{scope:'project:p',query:'Jujutsu'})).output; if(found.records.length!==1 || found.records[0].record.id!==state.saved.id) throw Error('memory mismatch'); return 'verified';",
        ),
        None,
    ];
    let mut answered = std::collections::HashSet::new();
    let mut calls = 0;
    for _ in 0..100 {
        agent.tick_wait(1_700_000_000_000).await.unwrap();
        let events = store
            .read(&StreamId::new("personal"), 0, 2000)
            .await
            .unwrap();
        for event in events
            .iter()
            .filter(|e| e.request.event_type == "model.requested")
        {
            if !answered.insert(event.event_id.clone()) {
                continue;
            }
            let content = if let Some(code) = scripts[calls] {
                json!([{"kind":"tool-call","call_id":format!("js-{calls}"),"name":"js","arguments":{"code":code}}])
            } else {
                json!([{"kind":"tool-call","name":"yield","call_id":"decision","arguments":{"action":"complete","note":"verified","reply":"Saved. Use Jujutsu."}}])
            };
            calls += 1;
            let mut result = event.request.clone();
            result.event_type = "model.completed".into();
            result.causation_id = Some(event.event_id.clone());
            result.deduplication_key = None;
            result.payload = EventPayload::CanonicalJson(serde_json::to_vec(&json!({"call_id":payload(event)["call_id"],"message":{"role":"assistant","content":content}})).unwrap());
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
        .read(&StreamId::new("personal"), 0, 2000)
        .await
        .unwrap();
    assert_eq!(calls, 4);
    assert!(
        events
            .iter()
            .any(|e| e.request.event_type == "code.completed" && payload(e)["value"] == "verified")
    );
    let remembered = events
        .iter()
        .find(|e| e.request.event_type == "memory.remembered")
        .unwrap();
    let reply = events
        .iter()
        .find(|e| {
            e.request.event_type == "capability.requested"
                && payload(e)["capability"] == "telegram.reply"
        })
        .unwrap();
    assert!(remembered.sequence < reply.sequence);
    assert!(!events.iter().any(
        |e| e.request.event_type == "code.failed" || e.request.event_type == "cognition.failed"
    ));
}

struct RejectCommit {
    store: Arc<SqliteEventStore<Metadata>>,
    reject: std::sync::atomic::AtomicBool,
    remaining: AtomicU64,
}
#[async_trait::async_trait]
impl DeliveryStore for RejectCommit {
    async fn checkpoint(
        &self,
        cursor: &pluribus_core::CursorKey,
    ) -> Result<u64, pluribus_core::DeliveryError> {
        self.store.checkpoint(cursor).await
    }
    async fn commit(
        &self,
        commit: pluribus_core::DeliveryCommit,
    ) -> Result<pluribus_core::DeliveryReceipt, pluribus_core::DeliveryError> {
        if self.reject.load(Ordering::Relaxed)
            || self
                .remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_err()
        {
            return Err(pluribus_core::DeliveryError::Storage(
                "injected failure".into(),
            ));
        }
        self.store.commit(commit).await
    }
    async fn discard(
        &self,
        cursor: &pluribus_core::CursorKey,
    ) -> Result<(), pluribus_core::DeliveryError> {
        self.store.discard(cursor).await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_memory_commit_leaves_no_record_result_or_cursor() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let commits = Arc::new(RejectCommit {
        store: Arc::clone(&store),
        reject: std::sync::atomic::AtomicBool::new(false),
        remaining: AtomicU64::new(u64::MAX),
    });
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        Arc::clone(&store) as _,
        Arc::clone(&store) as _,
        Arc::new(InMemoryBlobStore::default()),
        Arc::clone(&commits) as _,
    )
    .unwrap();
    let mut instance = runtime
        .instantiate(
            package().component("main").unwrap(),
            &json!({}),
            delivery(),
            PluginServices::default(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let source = request(&store, "unused", &json!({})).await;
    let write = request(
        &store,
        "memory.remember",
        &args("atomic", source.event_id.as_str()),
    )
    .await;
    commits.reject.store(true, Ordering::Relaxed);
    assert!(instance.handle(std::slice::from_ref(&write)).await.is_err());
    assert_eq!(instance.checkpoint(), 0);
    assert!(
        store
            .scan(
                &pluribus_core::StateNamespace::new("memory-1"),
                "",
                None,
                100
            )
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(
        store
            .read(&StreamId::new("personal"), 0, 100)
            .await
            .unwrap()
            .len(),
        2
    );
    commits.reject.store(false, Ordering::Relaxed);
    let result = instance.handle(&[write]).await.unwrap();
    assert_eq!(result.events.len(), 2);
    assert_eq!(result.events[0].request.event_type, "memory.remembered");
    assert_eq!(result.events[1].request.event_type, "capability.completed");
}
