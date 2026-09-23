use pluribus_core::*;
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as Kind, Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct GithubHttp {
    blobs: Arc<dyn BlobStore>,
}

impl GithubHttp {
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
impl HttpService for GithubHttp {
    async fn send(&self, _: &HttpGrant, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let repositories = request
            .url
            .as_str()
            .strip_prefix("https://api.github.com/")
            .map(|path| path == "installation/repositories?per_page=100&page=1");
        let authorization = request
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("authorization"))
            .map(|header| String::from_utf8_lossy(&header.value).into_owned());
        if repositories == Some(true) {
            assert_eq!(authorization.as_deref(), Some("Bearer ghs_fixture"));
        } else {
            assert_eq!(
                request.url,
                "https://api.github.com/app/installations?per_page=100&page=1"
            );
            assert!(
                authorization
                    .as_deref()
                    .is_some_and(|value| value.starts_with("Bearer ey"))
            );
        }
        let body = if repositories == Some(true) {
            &br#"{"total_count":1,"repositories":[{"id":42}]}"#[..]
        } else {
            &br#"[{"id":9,"app_id":7,"account":{"login":"me","type":"User"},"suspended_at":null}]"#
                [..]
        };
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: self.blob(body).await,
        })
    }
}

#[async_trait::async_trait]
impl HttpStreamService for GithubHttp {
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
#[derive(Default)]
struct Metadata(AtomicU64);
impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }
    fn now_ms(&self) -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
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
        streams: [(
            "default".to_owned(),
            pluribus_runtime_wasm::GrantedStream {
                service: Arc::new(pluribus_host_stream::LocalStreamService::new(dir).unwrap()),
                grant: StreamGrant {
                    endpoint: StreamEndpoint::Unix {
                        path: socket,
                        peer_uids: vec![rustix::process::geteuid().as_raw()],
                    },
                    max_bytes: 40 * 1024 * 1024,
                    max_timeout_ms: 5000,
                    max_connections: u32::MAX,
                },
            },
        )]
        .into(),
        ..PluginServices::default()
    }
}

struct HttpListener(Child);

impl HttpListener {
    async fn start(root: &Path, port: u16, uid: u32) -> Self {
        let socket = root.join("http.sock");
        let binary = std::env::var_os("PLURIBUS_HTTP_LISTENER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("pluribus-http-listener")
            });
        let child = Command::new(&binary)
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--socket")
            .arg(&socket)
            .arg("--runtime-uid")
            .arg(uid.to_string())
            .args([
                "--route",
                "github:POST:/events:github/receive",
                "--max-body-bytes",
                "1048576",
                "--max-queue-bytes",
                "1048576",
                "--response-timeout-seconds",
                "30",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|error| panic!("start {}: {error}", binary.display()));
        let mut listener = Self(child);
        for _ in 0..100 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                return listener;
            }
            assert!(
                listener.0.try_wait().unwrap().is_none(),
                "HTTP listener exited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("HTTP listener did not open {}", socket.display());
    }

    async fn restart(&mut self, root: &Path, port: u16, uid: u32) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
        *self = Self::start(root, port, uid).await;
    }
}

impl Drop for HttpListener {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
    let value = json!({"app":{"id":7,"slug":"fixture","pem":"unused by verification","webhook_secret":"fixture-secret"},"installation_id":9,"exports":{"installation-token":{"value":"ghs_fixture","expires_at_ms":9999999999999_i64}}});
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
    let mut http_listener = HttpListener::start(root, hp, uid).await;
    let blobs = Arc::new(InMemoryBlobStore::default());
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        blobs.clone(),
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
                http: Some(Arc::new(GithubHttp {
                    blobs: blobs.clone(),
                })),
                http_grant: Some(HttpGrant {
                    component: PrincipalRef::new(PrincipalKind::Component, "github/receive"),
                    origins: vec!["https://api.github.com".into()],
                    methods: vec!["GET".into(), "POST".into()],
                    allow_http: false,
                    allow_private_network: false,
                    max_request_bytes: 1024 * 1024,
                    max_response_bytes: 1024 * 1024,
                    max_redirects: 0,
                    max_timeout_ms: 15_000,
                }),
                ..PluginServices::default()
            },
        )
        .await
        .unwrap();
    http.init().await.unwrap();
    http.start();
    github.init().await.unwrap();
    for (index, (id, owner, valid, expected)) in [
        ("delivery-1", "me", true, 200),
        ("delivery-1", "me", true, 200),
        ("delivery-2", "me", false, 401),
        ("delivery-3", "other", true, 403),
        ("delivery-1", "me", true, 409),
        ("delivery-repository-only", "me", true, 200),
        ("delivery-repository-unknown", "me", true, 403),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 {
            http_listener.restart(root, hp, uid).await;
        }
        let repository_id = if id == "delivery-repository-unknown" {
            43
        } else {
            42
        };
        let mut body = if id == "delivery-repository-only" || id == "delivery-repository-unknown" {
            json!({"repository":{"id":repository_id,"full_name":format!("{owner}/repo"),"owner":{"login":owner,"type":"User"}},"sender":{"id":1}}).to_string()
        } else {
            json!({"repository":{"id":repository_id,"full_name":format!("{owner}/repo"),"owner":{"login":owner,"type":"User"}},"installation":{"id":9},"sender":{"id":1}}).to_string()
        };
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
                if !http.has_background_output() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
                let polled = http.background_output().unwrap();
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
        .unwrap_or_else(|e| panic!("HTTP delivery {index} was not delivered: {e}"));
        assert_eq!(events.len(), 1);
        let result = github.handle(&events).await.unwrap();
        let responses: Vec<_> = result
            .events
            .into_iter()
            .filter(|e| e.request.event_type == "http.response.requested")
            .collect();
        assert_eq!(responses.len(), 1);
        http.handle(&responses).await.unwrap();
        assert_eq!(
            request.await.unwrap().unwrap().status().as_u16(),
            expected,
            "delivery {id} at index {index}"
        );
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
    assert_eq!(observations.len(), 2);
    let EventPayload::CanonicalJson(bytes) = &observations[0].request.payload else {
        panic!("JSON")
    };
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).unwrap()["trusted"],
        false
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_backfills_webhook_secret_export_for_shell_access() {
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata::default())
            .await
            .unwrap(),
    );
    let blobs = Arc::new(InMemoryBlobStore::default());
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        blobs.clone(),
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
                http: Some(Arc::new(GithubHttp {
                    blobs: blobs.clone(),
                })),
                http_grant: Some(HttpGrant {
                    component: PrincipalRef::new(PrincipalKind::Component, "github/receive"),
                    origins: vec!["https://api.github.com".into()],
                    methods: vec!["GET".into(), "POST".into()],
                    allow_http: false,
                    allow_private_network: false,
                    max_request_bytes: 1024 * 1024,
                    max_response_bytes: 1024 * 1024,
                    max_redirects: 0,
                    max_timeout_ms: 15_000,
                }),
                ..PluginServices::default()
            },
        )
        .await
        .unwrap();
    plugin.init().await.unwrap();
    store
        .replace_plugin_credential(
            &SecretHandle::new("test"),
            "dev.pluribus.github",
            None,
            serde_json::to_vec(&json!({
                "app":{"id":7,"pem":include_str!("../../../plugins/github/tests/fixtures/test-app.pem"),"webhook_secret":"secret-fixture"},
                "installation_id":9,
                "exports":{"installation-token":{"value":"ghs_fixture","expires_at_ms":9999999999999_i64}}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let due = StateStore::get(
        store.as_ref(),
        &StateNamespace::new("github/receive"),
        "refresh/due",
    )
    .await
    .unwrap()
    .value
    .map(|value| serde_json::from_slice::<i64>(&value).unwrap())
    .unwrap();
    let timer = store
        .append(AppendRequest {
            stream_id: StreamId::new("test"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "timer.fired".into(),
            payload_schema: "test".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({"dueAtMs":due})).unwrap(),
            ),
            actor: PrincipalRef::new(PrincipalKind::Component, "clock"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap();
    plugin.handle(&[timer]).await.unwrap();
    let bytes = store
        .read_plugin_credential(&SecretHandle::new("test"), "dev.pluribus.github")
        .await
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["exports"]["webhook-secret"]["value"],
        "secret-fixture"
    );
    let shell = pluribus_runtime_wasm::CredentialAccess {
        exports: std::collections::BTreeMap::from([(
            "GITHUB_WEBHOOK_SECRET".into(),
            pluribus_runtime_wasm::CredentialExport {
                credential: "test".into(),
                provider: "dev.pluribus.github".into(),
                export: "webhook-secret".into(),
            },
        )]),
        store,
        provider: "dev.pluribus.shell".into(),
        handles: Default::default(),
    };
    assert_eq!(
        shell.resolve_export("GITHUB_WEBHOOK_SECRET").await.unwrap(),
        "secret-fixture"
    );
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
