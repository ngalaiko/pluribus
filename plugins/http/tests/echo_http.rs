#[path = "../listener/src/listener.rs"]
mod listener;
#[path = "../listener/src/native.rs"]
mod native;
use pluribus_cognition::{Agent, AuthorityResolver, OriginConstraints, Router};
use pluribus_core::*;
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as Kind, Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct Metadata(AtomicU64);
impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }
    fn now_ms(&self) -> i64 {
        native::now() * 1000
    }
}
struct Deny;
#[async_trait::async_trait]
impl AuthorityResolver for Deny {
    async fn resolve(&self, _: &CommittedEvent) -> Result<Authority, String> {
        Err("no capabilities".into())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_echo_crosses_listener_wasm_and_agent() {
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("http.sock");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = tcp.local_addr().unwrap();
    drop(tcp);
    let config = listener::Config {
        listen: address,
        socket: socket.clone(),
        runtime_uid: rustix::process::geteuid().as_raw(),
        routes: vec![listener::Route {
            path: "/*".into(),
            id: "echo".into(),
            consumer: "echo".into(),
            methods: vec!["GET".into(), "POST".into()],
        }],
        max_body_bytes: 1024,
        max_queue_bytes: 8192,
        response_timeout_seconds: 5,
    };
    let mut listener = tokio::spawn(listener::serve(config.clone()));
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata::default())
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
    let stream = StreamId::new("test");
    let principal = PrincipalRef::new(PrincipalKind::Agent, "test");
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
    let packages = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    for (name, component, id) in [("http", "listen", "http/listen"), ("echo", "", "echo")] {
        let package = PluginPackage::load(packages.join(name)).unwrap();
        let services = if name == "http" {
            PluginServices {
                streams: [(
                    "default".to_owned(),
                    pluribus_runtime_wasm::GrantedStream {
                        service: Arc::new(
                            pluribus_host_stream::LocalStreamService::new(dir.path()).unwrap(),
                        ),
                        grant: StreamGrant {
                            endpoint: StreamEndpoint::Unix {
                                path: socket.clone(),
                                peer_uids: vec![rustix::process::geteuid().as_raw()],
                            },
                            max_bytes: 16 * 1024 * 1024,
                            max_timeout_ms: 1000,
                            max_connections: u32::MAX,
                        },
                    },
                )]
                .into(),
                ..Default::default()
            }
        } else {
            PluginServices::default()
        };
        agent
            .install_component(
                package.component(component).unwrap(),
                &json!({}),
                Delivery {
                    instance_id: id.into(),
                    agent: Principal {
                        kind: Kind::Agent,
                        id: "test".into(),
                    },
                    actor: Principal {
                        kind: Kind::Agent,
                        id: "test".into(),
                    },
                    authority_id: "test".into(),
                    activity_id: "test".into(),
                    correlation_id: "test".into(),
                    origin_event_id: format!("init-{id}"),
                    depth: 0,
                    deadline_at_ms: None,
                    visible_blobs: vec![],
                },
                services,
                &[],
            )
            .await
            .unwrap();
    }
    for _ in 0..3 {
        agent.tick_wait(native::now() * 1000).await.unwrap();
    }
    assert!(
        store.read(&stream, 0, 100).await.unwrap().is_empty(),
        "idle listener must not append events"
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    for (index, (method, body)) in [
        (reqwest::Method::GET, vec![]),
        (reqwest::Method::POST, vec![0, 1, 255, 10]),
    ]
    .into_iter()
    .enumerate()
    {
        let request = tokio::spawn(
            reqwest::Client::new()
                .request(method, format!("http://{address}/echo"))
                .body(body.clone())
                .send(),
        );
        tokio::time::timeout(Duration::from_secs(8), async {
            while !request.is_finished() {
                agent.tick_wait(native::now() * 1000).await.unwrap();
            }
        })
        .await
        .unwrap();
        let response = request.await.unwrap().unwrap();
        assert_eq!(
            response.status(),
            200,
            "events: {:?}; failed: {:?}",
            store.read(&stream, 0, 100).await.unwrap(),
            agent.failed()
        );
        assert_eq!(response.bytes().await.unwrap().as_ref(), body.as_slice());
        if index == 0 {
            listener.abort();
            let _ = (&mut listener).await;
            listener = tokio::spawn(listener::serve(config.clone()));
            for _ in 0..10 {
                agent.tick_wait(native::now() * 1000).await.unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    let events = store
        .query(
            &stream,
            &EventQuery {
                event_types: vec![
                    "http.request.received".into(),
                    "http.response.requested".into(),
                ],
                ..Default::default()
            },
            100,
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 4);
    assert!(
        store
            .query(
                &stream,
                &EventQuery {
                    event_types: vec!["timer.set".into(), "timer.fired".into()],
                    ..Default::default()
                },
                100
            )
            .await
            .unwrap()
            .is_empty()
    );
    listener.abort();
}
