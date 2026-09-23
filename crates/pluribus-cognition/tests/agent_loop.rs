//! Drives the real echo plugin through the agent loop.

use pluribus_cognition::{Agent, AuthorityResolver, Router, Subscriptions};
use pluribus_core::{
    AppendRequest, Audience, Authority, AuthorityId, BlobStore, CapabilityName, CommittedEvent,
    ConstraintPolicy, ConstraintSet, DeliveryStore, EventId, EventMetadataSource, EventPayload,
    EventStore, EventTypeRegistry, Grant, InMemoryBlobStore, Origin, OriginKind, PrincipalKind,
    PrincipalRef, StateStore, StreamId, StreamKind,
};
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, Principal, PrincipalKind as RuntimePrincipalKind, Runtime, RuntimeLimits,
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

struct AllowAll;

impl ConstraintPolicy for AllowAll {
    fn allows(&self, _grant: &Grant, _request: &[u8]) -> Result<bool, String> {
        Ok(true)
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
                    constraints: ConstraintSet::canonical_json(b"{}".to_vec()),
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
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/echo");
    PluginPackage::load(&path).unwrap()
}

fn delivery() -> Delivery {
    Delivery {
        instance_id: "echo-1".into(),
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

type TestAgent = Agent<AllowAll, Standing>;

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
        AllowAll,
    );
    let mut agent = Agent::new(
        router,
        Standing(grants.iter().map(|name| (*name).to_owned()).collect()),
        runtime,
        Arc::clone(store) as Arc<dyn EventStore>,
        StreamId::new("personal"),
        agent_principal(),
    );
    let installed = agent
        .install_package(
            &package(),
            &json!({}),
            delivery(),
            std::collections::BTreeMap::from([(
                String::new(),
                pluribus_cognition::ComponentInstall::default(),
            )]),
        )
        .await
        .unwrap();
    assert_eq!(installed, ["echo-1"]);
    agent
}

async fn request(store: &Arc<SqliteEventStore<Metadata>>, message: &str) -> CommittedEvent {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "capability.requested".into(),
            payload_schema: "pluribus.capability-request/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({
                    "capability": "system.echo",
                    "arguments": {"message": message},
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

async fn types(store: &Arc<SqliteEventStore<Metadata>>) -> Vec<String> {
    store
        .read(&StreamId::new("personal"), 0, 200)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.request.event_type)
        .collect()
}

fn payload(event: &CommittedEvent) -> Value {
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("expected JSON")
    };
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_granted_request_reaches_the_provider_and_completes() {
    let (mut agent, store) = build(&["system.echo"]).await;
    let event = request(&store, "hello").await;

    let progress = agent.tick_wait(1).await.unwrap();

    assert_eq!(progress.requests_gated, 1);
    assert_eq!(progress.requests_denied, 0);
    assert_eq!(progress.deliveries, 1);
    let result = agent.result_for(&event.event_id).await.unwrap().unwrap();
    assert_eq!(result.request.event_type, "capability.completed");
    assert_eq!(payload(&result)["output"]["message"], json!("hello"));
    assert!(types(&store).await.contains(&"policy.decision".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ungranted_request_never_reaches_the_provider() {
    let (mut agent, store) = build(&[]).await;
    let event = request(&store, "hello").await;

    let progress = agent.tick_wait(1).await.unwrap();

    assert_eq!(progress.requests_denied, 1);
    let result = agent.result_for(&event.event_id).await.unwrap().unwrap();
    assert_eq!(result.request.event_type, "capability.denied");
    let committed = types(&store).await;
    assert!(
        !committed.contains(&"capability.completed".to_owned()),
        "a denied request must not execute: {committed:?}"
    );
    assert!(
        !committed.contains(&"capability.output".to_owned()),
        "a denied request must not even emit progress: {committed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_pass_does_not_repeat_a_completed_request() {
    let (mut agent, store) = build(&["system.echo"]).await;
    request(&store, "hello").await;

    agent.tick_wait(1).await.unwrap();
    let second = agent.tick_wait(2).await.unwrap();

    assert!(
        second.is_idle(),
        "a settled request must not be delivered again: {second:?}"
    );
    let completions = types(&store)
        .await
        .into_iter()
        .filter(|event_type| event_type == "capability.completed")
        .count();
    assert_eq!(completions, 1, "the effect must not repeat");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loop_is_idle_with_nothing_to_do() {
    let (mut agent, _store) = build(&["system.echo"]).await;

    assert!(agent.tick_wait(1).await.unwrap().is_idle());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_does_not_execute_timers() {
    let (mut agent, store) = build(&["system.echo"]).await;
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "timer.set".into(),
            payload_schema: "pluribus.timer-set/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({"dueAtMs": 500})).unwrap(),
            ),
            actor: PrincipalRef::new(PrincipalKind::Component, "echo-1"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap();

    agent.tick_wait(900).await.unwrap();
    let events = store
        .read(&StreamId::new("personal"), 0, 100)
        .await
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|event| event.request.event_type == "timer.fired")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_requests_settle_in_serialized_passes() {
    let (mut agent, store) = build(&["system.echo"]).await;
    let first = request(&store, "one").await;
    let second = request(&store, "two").await;

    agent.tick_wait(1).await.unwrap();
    agent.tick_wait(1).await.unwrap();

    assert_eq!(
        agent
            .result_for(&first.event_id)
            .await
            .unwrap()
            .unwrap()
            .request
            .event_type,
        "capability.completed"
    );
    assert_eq!(
        agent
            .result_for(&second.event_id)
            .await
            .unwrap()
            .unwrap()
            .request
            .event_type,
        "capability.completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_an_instance_stops_delivering_to_it() {
    let (mut agent, store) = build(&["system.echo"]).await;
    agent.remove("echo-1", 0).await.unwrap();
    let event = request(&store, "hello").await;

    let progress = agent.tick_wait(1).await.unwrap();

    assert_eq!(progress.deliveries, 0);
    let result = agent.result_for(&event.event_id).await.unwrap().unwrap();
    assert_eq!(
        result.request.event_type, "capability.denied",
        "with no provider the request terminates instead of hanging"
    );
}

#[test]
fn subscriptions_come_from_the_manifest() {
    let subscriptions =
        Subscriptions::from_manifest(package().component("").unwrap().manifest(), &[]);

    assert_eq!(subscriptions.capabilities, ["system.echo"]);
    assert!(!subscriptions.whole_stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_request_does_not_hide_later_requests() {
    let (agent, store) = build(&["system.echo"]).await;
    let mut agent = agent.with_batch(1);
    let first = request(&store, "first").await;
    let mut denied = first.request.clone();
    denied.event_type = "capability.denied".into();
    denied.causation_id = Some(first.event_id.clone());
    store.append(denied).await.unwrap();
    let second = request(&store, "second").await;
    for _ in 0..10 {
        agent.tick_wait(1).await.unwrap();
    }
    assert_eq!(
        agent
            .result_for(&second.event_id)
            .await
            .unwrap()
            .unwrap()
            .request
            .event_type,
        "capability.completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_wait_until_the_gate_reaches_them() {
    let (agent, store) = build(&["system.echo"]).await;
    let mut agent = agent.with_batch(1);
    let first = request(&store, "first").await;
    let second = request(&store, "second").await;
    agent.tick_wait(1).await.unwrap();
    assert!(agent.result_for(&first.event_id).await.unwrap().is_some());
    assert!(agent.result_for(&second.event_id).await.unwrap().is_none());
    agent.tick_wait(1).await.unwrap();
    assert!(agent.result_for(&second.event_id).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_checkpoints_do_not_delay_capability_admission() {
    let (mut agent, store) = build(&["system.echo"]).await;
    for _ in 0..300 {
        store
            .append(AppendRequest {
                stream_id: StreamId::new("personal"),
                stream_kind: StreamKind::Agent,
                observed_at_ms: None,
                event_type: "cognition.checkpoint".into(),
                payload_schema: "pluribus.cognition-checkpoint/1".into(),
                payload: EventPayload::CanonicalJson(b"{}".to_vec()),
                actor: PrincipalRef::new(PrincipalKind::Component, "rlm-1"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: None,
            })
            .await
            .unwrap();
    }
    let event = request(&store, "reply after restart").await;
    let progress = agent.tick_wait(1).await.unwrap();
    assert_eq!(
        progress.requests_gated, 1,
        "unrelated history must not consume the gate batch"
    );
    assert_eq!(
        agent
            .result_for(&event.event_id)
            .await
            .unwrap()
            .unwrap()
            .request
            .event_type,
        "capability.completed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reinstalled_provider_serves_requests_after_a_restart() {
    let (agent, store) = build(&["system.echo"]).await;
    drop(agent);
    let mut agent = build_on(&store, &["system.echo"]).await;
    let event = request(&store, "hello").await;

    agent.tick_wait(1).await.unwrap();

    let result = agent.result_for(&event.event_id).await.unwrap().unwrap();
    assert_eq!(result.request.event_type, "capability.completed");
    assert!(
        !types(&store).await.contains(&"component.failed".to_owned()),
        "a restart must not report the previous source loop as a failure"
    );
}

async fn install_scheduler(agent: &mut TestAgent) {
    let package = PluginPackage::load(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/scheduler"),
    )
    .unwrap();
    let mut delivery = delivery();
    delivery.instance_id = "scheduler".into();
    agent
        .install_package(
            &package,
            &json!({}),
            delivery,
            BTreeMap::from([(
                String::new(),
                pluribus_cognition::ComponentInstall::default(),
            )]),
        )
        .await
        .unwrap();
}

async fn schedule_event(
    store: &Arc<SqliteEventStore<Metadata>>,
    kind: &str,
    actor: &str,
    value: Value,
    cause: Option<EventId>,
) -> CommittedEvent {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: kind.into(),
            payload_schema: format!("test.{kind}/1"),
            payload: EventPayload::CanonicalJson(serde_json::to_vec(&value).unwrap()),
            actor: PrincipalRef::new(PrincipalKind::Component, actor),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: cause,
            deduplication_key: None,
        })
        .await
        .unwrap()
}

async fn schedule_result(agent: &mut TestAgent, request: &CommittedEvent) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            agent.tick_wait(1_700_000_000_000).await.unwrap();
            if let Some(result) = agent.result_for(&request.event_id).await.unwrap() {
                assert_eq!(
                    result.request.event_type,
                    "capability.completed",
                    "{}",
                    payload(&result)
                );
                return payload(&result)["output"].clone();
            }
            agent
                .wait_for_progress(std::time::Duration::from_millis(10))
                .await;
        }
    })
    .await
    .expect("schedule request did not settle")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_plugin_emits_one_time_observations_and_manages_cron() {
    let grants = [
        "schedule.create",
        "schedule.list",
        "schedule.get",
        "schedule.pause",
        "schedule.resume",
        "schedule.update",
        "schedule.delete",
    ];
    let (mut agent, store) = build(&grants).await;
    install_scheduler(&mut agent).await;
    let origin = schedule_event(&store,"observation.received","cli",json!({"provider":"cli","externalSenderId":"u","conversationId":"c","message":{"text":"remind me"}}),None).await;
    let create = schedule_event(&store,"capability.requested","rlm-1",json!({"capability":"schedule.create","arguments":{"name":"reminder","prompt":"check status","timing":{"kind":"after","milliseconds":100}}}),Some(origin.event_id.clone())).await;
    let created = schedule_result(&mut agent, &create).await;
    assert!(created["nextAtMs"].is_i64());
    let wake = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            agent.tick_wait(1_700_000_000_000).await.unwrap();
            let events = store
                .read(&StreamId::new("personal"), 0, 1000)
                .await
                .unwrap();
            if let Some(event) = events.into_iter().find(|e| {
                e.request.event_type == "observation.received"
                    && e.request.actor.id.as_str() == "scheduler"
            }) {
                break event;
            }
            agent
                .wait_for_progress(std::time::Duration::from_millis(10))
                .await;
        }
    })
    .await
    .expect("schedule did not fire");
    assert_eq!(payload(&wake)["message"]["text"], "check status");
    assert_eq!(payload(&wake)["originEventId"], origin.event_id.as_str());
    assert_eq!(wake.request.causation_id, Some(create.event_id));
    drop(agent);
    clear_scheduler_projection(&store).await;
    let mut agent = build_on(&store, &grants).await;
    install_scheduler(&mut agent).await;
    let list = schedule_event(
        &store,
        "capability.requested",
        "rlm-1",
        json!({"capability":"schedule.list","arguments":{}}),
        Some(origin.event_id.clone()),
    )
    .await;
    let listed = schedule_result(&mut agent, &list).await;
    assert_eq!(listed["schedules"].as_array().unwrap().len(), 1);
    assert!(listed["schedules"][0]["nextAtMs"].is_null());
    let id = created["id"].clone();
    for (capability, args) in [
        (
            "schedule.update",
            json!({"id":id,"timing":{"kind":"cron","expression":"0 9 * * MON-FRI","timezone":"Europe/Stockholm"}}),
        ),
        ("schedule.pause", json!({"id":id})),
        ("schedule.get", json!({"id":id})),
        ("schedule.resume", json!({"id":id})),
        ("schedule.delete", json!({"id":id})),
    ] {
        let request = schedule_event(
            &store,
            "capability.requested",
            "rlm-1",
            json!({"capability":capability,"arguments":args}),
            Some(origin.event_id.clone()),
        )
        .await;
        let result = schedule_result(&mut agent, &request).await;
        if capability == "schedule.get" {
            assert_eq!(result["paused"], true);
        }
    }
    let events = store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.request.event_type == "observation.received"
                && e.request.actor.id.as_str() == "scheduler")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_plugin_serves_requests_without_redelivering_timer_history() {
    let (mut agent, store) = build(&["schedule.list"]).await;
    for _ in 0..256 {
        let timer = schedule_event(&store, "timer.set", "owner", json!({"dueAtMs":1}), None).await;
        schedule_event(
            &store,
            "timer.fired",
            "old-host",
            json!({"requestEventId":timer.event_id.as_str(),"dueAtMs":1}),
            Some(timer.event_id.clone()),
        )
        .await;
    }
    install_scheduler(&mut agent).await;
    let origin = schedule_event(
        &store,
        "observation.received",
        "cli",
        json!({"provider":"cli","externalSenderId":"u","conversationId":"c","message":{"text":"list schedules"}}),
        None,
    )
    .await;
    let request = schedule_event(
        &store,
        "capability.requested",
        "rlm-1",
        json!({"capability":"schedule.list","arguments":{}}),
        Some(origin.event_id),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for _ in 0..16 {
            agent.tick_wait(1_700_000_000_000).await.unwrap();
            if let Some(result) = agent.result_for(&request.event_id).await.unwrap() {
                assert_eq!(result.request.event_type, "capability.completed");
                assert_eq!(payload(&result)["output"]["schedules"], json!([]));
                return;
            }
            agent
                .wait_for_progress(std::time::Duration::from_millis(10))
                .await;
        }
        panic!("timer history blocked schedule.list");
    })
    .await
    .expect("scheduler stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_plugin_recovers_timers_and_respects_cancellation_ownership() {
    let (mut agent, store) = build(&[]).await;
    let fired = schedule_event(&store, "timer.set", "owner", json!({"dueAtMs":1}), None).await;
    schedule_event(
        &store,
        "timer.fired",
        "old-host",
        json!({"requestEventId":fired.event_id.as_str(),"dueAtMs":1}),
        Some(fired.event_id.clone()),
    )
    .await;
    let cancelled = schedule_event(&store, "timer.set", "owner", json!({"dueAtMs":1}), None).await;
    schedule_event(
        &store,
        "timer.cancel",
        "owner",
        json!({"requestEventId":cancelled.event_id.as_str()}),
        None,
    )
    .await;
    for _ in 0..70 {
        let timer = schedule_event(&store, "timer.set", "owner", json!({"dueAtMs":1}), None).await;
        schedule_event(
            &store,
            "timer.cancel",
            "owner",
            json!({"requestEventId":timer.event_id.as_str()}),
            None,
        )
        .await;
    }
    let pending = schedule_event(&store, "timer.set", "owner", json!({"dueAtMs":1}), None).await;
    schedule_event(
        &store,
        "timer.cancel",
        "stranger",
        json!({"requestEventId":pending.event_id.as_str()}),
        None,
    )
    .await;
    install_scheduler(&mut agent).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            agent.tick_wait(1_700_000_000_000).await.unwrap();
            let events = store
                .read(&StreamId::new("personal"), 0, 1000)
                .await
                .unwrap();
            if events.iter().any(|e| {
                e.request.event_type == "timer.fired"
                    && payload(e)["requestEventId"] == pending.event_id.as_str()
            }) {
                break;
            }
            agent
                .wait_for_progress(std::time::Duration::from_millis(10))
                .await;
        }
    })
    .await
    .expect("timer did not fire");
    drop(agent);
    let mut agent = build_on(&store, &[]).await;
    install_scheduler(&mut agent).await;
    agent.tick_wait(1_700_000_000_000).await.unwrap();
    let events = store
        .read(&StreamId::new("personal"), 0, 1000)
        .await
        .unwrap();
    let fired: Vec<_> = events
        .iter()
        .filter(|e| e.request.event_type == "timer.fired")
        .collect();
    assert_eq!(fired.len(), 2);
    assert!(
        !fired
            .iter()
            .any(|e| payload(e)["requestEventId"] == cancelled.event_id.as_str())
    );
}

async fn clear_scheduler_projection(store: &Arc<SqliteEventStore<Metadata>>) {
    let namespace = pluribus_core::StateNamespace::new("scheduler");
    let state = store.scan(&namespace, "", None, 1000).await.unwrap();
    store
        .apply(
            &namespace,
            state.revision,
            &state
                .entries
                .iter()
                .map(|entry| pluribus_core::StateMutation::Delete {
                    key: entry.key.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
}
