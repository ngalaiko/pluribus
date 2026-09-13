use pluribus_core::*;
#[path = "../../http/listener/src/listener.rs"]
mod listener;
#[path = "../../http/listener/src/native.rs"]
mod native;
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as Kind, Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
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
fn port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn delivery(instance: &str) -> Delivery {
    Delivery {
        instance_id: instance.into(),
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
        origin_event_id: "init".into(),
        depth: 0,
        deadline_at_ms: None,
        visible_blobs: vec![],
    }
}
fn services(dir: &Path, socket: PathBuf, _instance: &str) -> PluginServices {
    PluginServices {
        stream: Some(Arc::new(
            pluribus_host_stream::LocalStreamService::new(dir).unwrap(),
        )),
        stream_grant: Some(StreamGrant {
            endpoint: StreamEndpoint::Unix {
                path: socket,
                peer_uids: vec![rustix::process::geteuid().as_raw()],
            },
            max_bytes: 40 * 1024 * 1024,
            max_timeout_ms: 5000,
        }),
        ..PluginServices::default()
    }
}
async fn timer(store: &SqliteEventStore<Metadata>) -> CommittedEvent {
    store
        .append(AppendRequest {
            stream_id: StreamId::new("test"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "timer.fired".into(),
            payload_schema: "test".into(),
            payload: EventPayload::CanonicalJson(b"{}".to_vec()),
            actor: PrincipalRef::new(PrincipalKind::Node, "timer"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_delivery_crosses_listener_and_wasm_with_core_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let uid = rustix::process::geteuid().as_raw();
    let store = Arc::new(
        SqliteEventStore::open(root.join("events.sqlite"), Metadata::default())
            .await
            .unwrap(),
    );
    let value = json!({"app":{"id":7,"slug":"fixture","pem":"unused by verification","webhook_secret":"fixture-secret"},"installation_id":9});
    store
        .replace_plugin_credential(
            &SecretHandle::new("github:personal"),
            "dev.pluribus.github",
            None,
            serde_json::to_vec(&value).unwrap(),
        )
        .await
        .unwrap();
    let hp = port();
    let listener_config = || listener::Config {
        listen: format!("127.0.0.1:{hp}").parse().unwrap(),
        socket: root.join("http.sock"),
        runtime_uid: uid,
        routes: vec![listener::Route {
            path: "/events".into(),
            id: "github".into(),
            consumer: "github/receive".into(),
            methods: vec!["POST".into()],
        }],
        max_body_bytes: 1024 * 1024,
        max_queue_bytes: 1024 * 1024,
        response_timeout_seconds: 30,
    };
    let mut http_task = tokio::spawn(listener::serve(listener_config()));
    for _ in 0..100 {
        if root.join("http.sock").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!http_task.is_finished());
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        Arc::new(InMemoryBlobStore::default()),
        store.clone(),
    )
    .unwrap();
    let packages = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins");
    let http_package = PluginPackage::load(packages.join("http")).unwrap();
    let github_package = PluginPackage::load(packages.join("github")).unwrap();
    let mut http = runtime
        .instantiate(
            &http_package.components()["listen"],
            &json!({}),
            delivery("http/listen"),
            services(root, root.join("http.sock"), "http/listen"),
        )
        .await
        .unwrap();
    let config = json!({"credentials": {"app": "github:personal"},"http_instance":"http/listen","route_id":"github","owner":"me"});
    let mut github = runtime
        .instantiate(
            &github_package.components()["receive"],
            &config,
            delivery("github/receive"),
            PluginServices {
                credentials: Some(pluribus_runtime_wasm::CredentialAccess {
                    exports: Default::default(),
                    store: store.clone(),
                    provider: "dev.pluribus.github".into(),
                    handles: std::collections::HashSet::from(["github:personal".into()]),
                }),
                ..PluginServices::default()
            },
        )
        .await
        .unwrap();
    http.init().await.unwrap();
    github.init().await.unwrap();
    for (index, (id, owner, valid, expected)) in [
        ("delivery-1", "me", true, 200),
        ("delivery-1", "me", true, 200),
        ("delivery-2", "me", false, 401),
        ("delivery-3", "other", true, 403),
        ("delivery-1", "me", true, 409),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 {
            http_task.abort();
            let _ = http_task.await;
            http_task = tokio::spawn(listener::serve(listener_config()));
            for _ in 0..100 {
                if tokio::net::UnixStream::connect(root.join("http.sock"))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(!http_task.is_finished());
        }
        let mut body=json!({"repository":{"full_name":format!("{owner}/repo"),"owner":{"login":owner,"type":"User"}},"installation":{"id":9},"sender":{"id":1}}).to_string();
        if expected == 409 {
            body.push(' ');
        }
        let signature = ring::hmac::sign(
            &ring::hmac::Key::new(
                ring::hmac::HMAC_SHA256,
                if valid {
                    b"fixture-secret"
                } else {
                    b"wrong-secret"
                },
            ),
            body.as_bytes(),
        );
        let signature = format!(
            "sha256={}",
            signature
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let request = tokio::spawn(
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{hp}/events"))
                .header("x-github-event", "push")
                .header("x-github-delivery", id)
                .header("x-hub-signature-256", signature)
                .body(body)
                .send(),
        );
        let events = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let input = timer(&store).await;
                let polled = http.handle(&[input]).await.unwrap();
                let events: Vec<_> = polled
                    .events
                    .into_iter()
                    .filter(|e| e.request.event_type == "http.request.received")
                    .collect();
                if !events.is_empty() {
                    break events;
                }
                assert!(!request.is_finished(), "HTTP request ended before delivery");
            }
        })
        .await
        .expect("HTTP request was not delivered");
        assert_eq!(events.len(), 1);
        let result = github.handle(&events).await.unwrap();
        let responses: Vec<_> = result
            .events
            .into_iter()
            .filter(|e| e.request.event_type == "http.response.requested")
            .collect();
        assert_eq!(responses.len(), 1);
        http.handle(&responses).await.unwrap();
        assert_eq!(request.await.unwrap().unwrap().status().as_u16(), expected);
    }
    let observations = store
        .query(
            &StreamId::new("test"),
            &EventQuery {
                event_types: vec!["observation.received".into()],
                ..EventQuery::default()
            },
            100,
        )
        .await
        .unwrap();
    assert_eq!(observations.len(), 1);
    let EventPayload::CanonicalJson(bytes) = &observations[0].request.payload else {
        panic!("JSON")
    };
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).unwrap()["trusted"],
        false
    );
    http_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wasm_manual_enrollment_replays_without_exposing_secrets() {
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
    let package = PluginPackage::load(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/github"),
    )
    .unwrap();
    let config = json!({"credentials":{"app":"test"},"http_instance":"http/listen","route_id":"github","owner":"me"});
    let mut plugin = runtime
        .instantiate(
            &package.components()["receive"],
            &config,
            delivery("github/receive"),
            PluginServices {
                credentials: Some(pluribus_runtime_wasm::CredentialAccess {
                    exports: Default::default(),
                    store: store.clone(),
                    provider: "dev.pluribus.github".into(),
                    handles: std::collections::HashSet::from(["test".into()]),
                }),
                ..PluginServices::default()
            },
        )
        .await
        .unwrap();
    plugin.init().await.unwrap();
    store.replace_plugin_credential(&SecretHandle::new("test"), "dev.pluribus.github", None,
        serde_json::to_vec(&json!({"app":{"id":7,"pem":"private-fixture","webhook_secret":"secret-fixture"},
            "enrollment_result":{"id":"enrollment-1","url":"https://github.com/apps/fixture/installations/new"}})).unwrap()).await.unwrap();
    let request = store
        .append(AppendRequest {
            stream_id: StreamId::new("test"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "credential.enrollment.requested".into(),
            payload_schema: "test".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({"component":"github/receive","credential":"test","enrollment":"enrollment-1"}))
                    .unwrap(),
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, "credential-cli"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap();
    let outcome = plugin.handle(&[request.clone()]).await.unwrap();
    let response = outcome
        .events
        .iter()
        .find(|e| e.request.event_type == "credential.enrollment.started")
        .unwrap();
    let EventPayload::CanonicalJson(bytes) = &response.request.payload else {
        panic!()
    };
    let value: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(
        value["url"],
        "https://github.com/apps/fixture/installations/new"
    );
    assert!(!String::from_utf8_lossy(bytes).contains("private-fixture"));
    assert!(!String::from_utf8_lossy(bytes).contains("secret-fixture"));
    assert!(
        store
            .read_plugin_credential(&SecretHandle::new("test"), "other-plugin")
            .await
            .unwrap()
            .is_none()
    );

    let http=store.append(AppendRequest{
        stream_id:StreamId::new("test"),stream_kind:StreamKind::Agent,observed_at_ms:None,
        event_type:"http.request.received".into(),payload_schema:"test".into(),
        payload:EventPayload::CanonicalJson(serde_json::to_vec(&json!({"routeId":"github","consumer":"github/receive","method":"GET","target":"/setup/removed"})).unwrap()),
        actor:PrincipalRef::new(PrincipalKind::Component,"http/listen"),authority_id:None,activity_id:None,correlation_id:None,causation_id:None,deduplication_key:None,
    }).await.unwrap();
    let result = plugin.handle(&[http]).await.unwrap();
    let EventPayload::CanonicalJson(bytes) = &result.events[0].request.payload else {
        panic!()
    };
    let value: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(value["status"], 404);

    let mut denied = runtime
        .instantiate(
            &package.components()["receive"],
            &config,
            delivery("denied"),
            PluginServices {
                credentials: Some(pluribus_runtime_wasm::CredentialAccess {
                    exports: Default::default(),
                    store: store.clone(),
                    provider: "dev.pluribus.github".into(),
                    handles: Default::default(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    denied.init().await.unwrap();
    let mut denied_request = request.request;
    denied_request.payload = EventPayload::CanonicalJson(
        serde_json::to_vec(
            &json!({"component":"denied","credential":"test","enrollment":"enrollment-1"}),
        )
        .unwrap(),
    );
    let denied_request = store.append(denied_request).await.unwrap();
    let result = denied.handle(&[denied_request]).await.unwrap();
    let EventPayload::CanonicalJson(bytes) = &result.events[0].request.payload else {
        panic!()
    };
    let value: Value = serde_json::from_slice(bytes).unwrap();
    assert!(value.get("error").is_some());
}
