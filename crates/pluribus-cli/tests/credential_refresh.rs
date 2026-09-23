use pluribus_core::*;
use pluribus_host_http::PolicyHttpService;
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    CredentialAccess, Delivery, PluginServices, Principal, PrincipalKind as WasmPrincipalKind,
    Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicU64, Ordering},
};

const RESPONSES: &str = "https://chatgpt.com/backend-api/codex/responses";
const TOKEN: &str = "https://auth.openai.com/oauth/token";
const DEVICE_USER_CODE: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const NOW: i64 = 1_700_000_000_000;
const HANDLE: &str = "codex:fixture";
const OLD: &str = "e30.eyJleHAiOjE3MDAwMDM2MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJmaXh0dXJlIn19.old";
const NEW: &str = "e30.eyJleHAiOjE3MDAwMDM2MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJmaXh0dXJlIn19.new";

struct Metadata(AtomicU64);
impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::SeqCst)))
    }
    fn now_ms(&self) -> i64 {
        NOW
    }
}

/// Routes the provider's fixed URLs to the local HTTP fixture, keeping the
/// path so the fixture can tell a token exchange from a completion.
struct FixtureHttp {
    inner: PolicyHttpService,
    origin: String,
    concurrent_write: Option<Arc<SqliteEventStore<Metadata>>>,
    reenroll_during_exchange: bool,
}
impl FixtureHttp {
    fn mapped(&self, grant: &HttpGrant, request: &HttpRequest) -> (HttpGrant, HttpRequest) {
        let path = match request.url.as_str() {
            RESPONSES => "/backend-api/codex/responses",
            TOKEN => "/oauth/token",
            DEVICE_USER_CODE => "/api/accounts/deviceauth/usercode",
            DEVICE_TOKEN => "/api/accounts/deviceauth/token",
            other => panic!("unexpected destination: {other}"),
        };
        let mut grant = grant.clone();
        grant.origins = vec![self.origin.clone()];
        grant.allow_http = true;
        grant.allow_private_network = true;
        let mut request = request.clone();
        request.url = format!("{}{path}", self.origin);
        (grant, request)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires loopback sockets"]
async fn packaged_codex_starts_device_enrollment_and_seals_state() {
    device_enrollment(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires loopback sockets"]
async fn packaged_codex_preserves_a_new_enrollment_during_token_exchange() {
    device_enrollment(true).await;
}

#[allow(clippy::too_many_lines)]
async fn device_enrollment(reenroll_during_exchange: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let request = exchange(
            &listener,
            "200 OK",
            "application/json",
            r#"{"device_auth_id":"device-1","user_code":"ABCD-1234","interval":65}"#,
        );
        assert!(request.contains("POST /api/accounts/deviceauth/usercode"));
        assert!(request.contains("app_EMoamEEZ73f0CkXaXp7hrann"));
        let poll = exchange(
            &listener,
            "400 Bad Request",
            "application/json",
            r#"{"error":"slow_down"}"#,
        );
        assert!(poll.contains("POST /api/accounts/deviceauth/token"));
        let authorized = exchange(
            &listener,
            "200 OK",
            "application/json",
            r#"{"authorization_code":"code-1","code_verifier":"verifier-1"}"#,
        );
        assert!(authorized.contains("POST /api/accounts/deviceauth/token"));
        let failed_exchange = exchange(
            &listener,
            "503 Service Unavailable",
            "application/json",
            r#"{"error":"temporarily_unavailable"}"#,
        );
        assert!(failed_exchange.contains("POST /oauth/token"));
        let tokens = exchange(
            &listener,
            "200 OK",
            "application/json",
            r#"{"access_token":"access-device","refresh_token":"refresh-device"}"#,
        );
        assert!(tokens.contains("POST /oauth/token"));
    });

    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let package = PluginPackage::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/openai-codex"),
    )
    .unwrap();
    assert_eq!(
        package.manifest().credentials[0].flow_schema,
        "pluribus:credential/plugin@1"
    );
    let provider = package.manifest().id.clone();
    let handle = SecretHandle::new(HANDLE);
    store
        .replace_plugin_credential(
            &handle,
            &provider,
            None,
            serde_json::to_vec(&json!({
                "enrollment": {
                    "id": "enrollment-1",
                    "input": {},
                    "expires_at_ms": i64::MAX
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

    let blobs = Arc::new(InMemoryBlobStore::default());
    let http = Arc::new(FixtureHttp {
        inner: PolicyHttpService::new(blobs.clone()),
        origin,
        concurrent_write: Some(store.clone()),
        reenroll_during_exchange,
    });
    let clock = Arc::new(AtomicI64::new(NOW));
    let runtime_clock = clock.clone();
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        blobs,
        store.clone(),
    )
    .unwrap()
    .with_clock(Arc::new(move || runtime_clock.load(Ordering::SeqCst)));
    let principal = PrincipalRef::new(PrincipalKind::Component, "codex-1/main");
    let delivery = Delivery {
        instance_id: "codex-1/main".into(),
        agent: Principal {
            kind: WasmPrincipalKind::Agent,
            id: "personal".into(),
        },
        actor: Principal {
            kind: WasmPrincipalKind::Agent,
            id: "personal".into(),
        },
        authority_id: "test".into(),
        activity_id: "test".into(),
        correlation_id: "test".into(),
        origin_event_id: "origin".into(),
        depth: 0,
        deadline_at_ms: None,
        visible_blobs: vec![],
    };
    let services = PluginServices {
        credentials: Some(CredentialAccess {
            store: store.clone(),
            provider: provider.clone(),
            handles: HashSet::from([HANDLE.to_owned()]),
            exports: std::collections::BTreeMap::new(),
        }),
        http: Some(http),
        http_grant: Some(HttpGrant {
            component: principal,
            origins: vec!["https://auth.openai.com".into()],
            methods: vec!["POST".into()],
            allow_http: false,
            allow_private_network: false,
            max_request_bytes: 1_000_000,
            max_response_bytes: 1_000_000,
            max_redirects: 0,
            max_timeout_ms: 10000,
        }),
        ..PluginServices::default()
    };
    let config = json!({"credentials": {"subscription": HANDLE}, "models":["fixture-model"], "timeout_ms":10000});
    let mut instance = runtime
        .instantiate(
            package.component("main").unwrap(),
            &config,
            delivery.clone(),
            services.clone(),
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let request = store
        .append(AppendRequest {
            stream_id: StreamId::new("personal"),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "credential.enrollment.requested".into(),
            payload_schema: "pluribus.credential.enrollment.requested/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({
                    "component": "codex-1/main",
                    "credential": HANDLE,
                    "enrollment": "enrollment-1"
                }))
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
    let result = instance
        .handle(std::slice::from_ref(&request))
        .await
        .unwrap();
    let started = result
        .events
        .iter()
        .find(|event| event.request.event_type == "credential.enrollment.started")
        .expect("device enrollment response");
    let EventPayload::CanonicalJson(bytes) = &started.request.payload else {
        panic!()
    };
    let value: Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(value["url"], "https://auth.openai.com/codex/device");
    assert_eq!(value["userCode"], "ABCD-1234");
    assert!(value.get("device_auth_id").is_none());
    assert!(
        result
            .events
            .iter()
            .any(|event| event.request.event_type == "timer.set")
    );
    let device_due = store
        .read_plugin_credential(&handle, &provider)
        .await
        .unwrap()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|record| record["device"]["next_poll_at_ms"].as_i64())
        .unwrap();
    assert_eq!(device_due, NOW + 65_000);
    let timer = result
        .events
        .iter()
        .find(|event| event.request.event_type == "timer.set")
        .unwrap()
        .clone();
    clock.store(device_due, Ordering::SeqCst);
    for foreign_actor in [true, false] {
        let mut unrelated = timer.request.clone();
        if foreign_actor {
            unrelated.actor = PrincipalRef::new(PrincipalKind::Component, "other/main");
        } else {
            unrelated.payload = EventPayload::CanonicalJson(
                serde_json::to_vec(
                    &json!({"dueAtMs":device_due,"enrollmentId":"superseded-enrollment"}),
                )
                .unwrap(),
            );
        }
        unrelated.deduplication_key = None;
        let unrelated = store.append(unrelated).await.unwrap();
        let unrelated_fired = fire_timer(
            store.as_ref(),
            &StreamId::new("personal"),
            &PrincipalRef::new(PrincipalKind::Node, "scheduler"),
            &PendingTimer {
                request: unrelated.event_id,
                due_at_ms: device_due,
                instance_id: "codex-1/main".into(),
            },
        )
        .await
        .unwrap();
        assert!(
            instance
                .handle(&[unrelated_fired])
                .await
                .unwrap()
                .events
                .is_empty(),
            "foreign timers cannot poll credentials"
        );
    }
    instance.stop(i64::MAX).await.unwrap();
    drop(instance);
    clock.store(device_due + 1, Ordering::SeqCst);
    let mut instance = runtime
        .instantiate(
            package.component("main").unwrap(),
            &config,
            delivery,
            services,
        )
        .await
        .unwrap();
    let resumed = instance.init().await.unwrap();
    assert!(
        resumed
            .events
            .iter()
            .any(|event| event.event_id == timer.event_id),
        "restart must preserve the pending timer"
    );
    clock.store(device_due, Ordering::SeqCst);
    let fired = fire_timer(
        store.as_ref(),
        &StreamId::new("personal"),
        &PrincipalRef::new(PrincipalKind::Node, "scheduler"),
        &PendingTimer {
            request: timer.event_id,
            due_at_ms: device_due,
            instance_id: "codex-1/main".into(),
        },
    )
    .await
    .unwrap();
    let poll_result = instance.handle(std::slice::from_ref(&fired)).await.unwrap();
    assert!(poll_result.events.iter().any(|event| {
        if event.request.event_type != "timer.set" {
            return false;
        }
        let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
            return false;
        };
        serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|value| value["dueAtMs"].as_i64())
            .is_some_and(|due| due == NOW + 135_000)
    }));
    let record: Value = serde_json::from_slice(
        &store
            .read_plugin_credential(&handle, &provider)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["device"]["id"], "device-1");
    assert_eq!(record["device"]["user_code"], "ABCD-1234");
    assert_eq!(record["device"]["interval_seconds"], 70);
    assert!(record["enrollment"].is_null());
    let device_due = record["device"]["next_poll_at_ms"].as_i64().unwrap();
    let replay = instance.handle(&[fired]).await.unwrap();
    assert_eq!(
        replay.events, poll_result.events,
        "poll replay must restore its timer without polling twice"
    );
    let timer = poll_result
        .events
        .iter()
        .find(|event| event.request.event_type == "timer.set")
        .unwrap()
        .clone();
    clock.store(device_due, Ordering::SeqCst);
    let fired = fire_timer(
        store.as_ref(),
        &StreamId::new("personal"),
        &PrincipalRef::new(PrincipalKind::Node, "scheduler"),
        &PendingTimer {
            request: timer.event_id,
            due_at_ms: device_due,
            instance_id: "codex-1/main".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        instance.handle(std::slice::from_ref(&fired)).await.is_err(),
        "token exchange failure must remain retryable"
    );
    let poll_result = instance.handle(&[fired]).await.unwrap();
    assert!(
        !poll_result
            .events
            .iter()
            .any(|event| event.request.event_type == "timer.set")
    );
    let record: Value = serde_json::from_slice(
        &store
            .read_plugin_credential(&handle, &provider)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["concurrent_metadata"], "preserve");
    if reenroll_during_exchange {
        assert_eq!(record["enrollment"]["id"], "enrollment-2");
        assert!(record["access_token"].is_null());
    } else {
        assert_eq!(record["access_token"], "access-device");
        assert_eq!(record["refresh_token"], "refresh-device");
        assert!(record["device"]["id"].as_str().is_none_or(str::is_empty));
    }
    let request = store.append(request.request.clone()).await.unwrap();
    let replay = instance.handle(&[request]).await.unwrap();
    assert!(
        !replay
            .events
            .iter()
            .any(|event| event.request.event_type == "timer.set")
    );
    for event in store
        .query(&StreamId::new("personal"), &EventQuery::default(), 100)
        .await
        .unwrap()
    {
        if let EventPayload::CanonicalJson(bytes) = event.request.payload {
            let payload = String::from_utf8(bytes).unwrap();
            for secret in [
                "device-1",
                "code-1",
                "verifier-1",
                "access-device",
                "refresh-device",
            ] {
                assert!(
                    !payload.contains(secret),
                    "provider secrets must remain in private storage"
                );
            }
        }
    }
    server.join().unwrap();
}
#[async_trait::async_trait]
impl HttpService for FixtureHttp {
    async fn send(&self, g: &HttpGrant, r: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let (g, r) = self.mapped(g, r);
        self.inner.send(&g, &r).await
    }
}
#[async_trait::async_trait]
impl HttpStreamService for FixtureHttp {
    async fn start_response(
        &self,
        g: &HttpGrant,
        r: &HttpRequest,
    ) -> Result<Option<HttpStreamingResponse>, HttpError> {
        let token_exchange = r.url == TOKEN;
        let (g, r) = self.mapped(g, r);
        let response = self.inner.start_response(&g, &r).await?;
        if token_exchange
            && response
                .as_ref()
                .is_some_and(|response| response.status == 200)
            && let Some(store) = &self.concurrent_write
        {
            let handle = SecretHandle::new(HANDLE);
            let provider = "dev.pluribus.openai-codex";
            let previous = store
                .read_plugin_credential(&handle, provider)
                .await
                .unwrap()
                .unwrap();
            let mut record: Value = serde_json::from_slice(&previous).unwrap();
            record["concurrent_metadata"] = json!("preserve");
            if self.reenroll_during_exchange {
                record["enrollment"] =
                    json!({"id":"enrollment-2", "input":{}, "expires_at_ms":i64::MAX});
            }
            assert!(
                store
                    .replace_plugin_credential(
                        &handle,
                        provider,
                        Some(previous),
                        serde_json::to_vec(&record).unwrap()
                    )
                    .await
                    .unwrap()
            );
        }
        Ok(response)
    }
    async fn open_stream(
        &self,
        g: &HttpGrant,
        p: HttpStreamProtocol,
        r: &HttpRequest,
    ) -> Result<String, HttpError> {
        let (g, r) = self.mapped(g, r);
        self.inner.open_stream(&g, p, &r).await
    }
    async fn receive(
        &self,
        g: &HttpGrant,
        s: &str,
        b: u32,
        w: u32,
    ) -> Result<HttpFramePage, HttpError> {
        self.inner.receive(g, s, b, w).await
    }
    async fn send_frame(&self, g: &HttpGrant, s: &str, f: &HttpFrame) -> Result<(), HttpError> {
        self.inner.send_frame(g, s, f).await
    }
    fn close_stream(&self, g: &HttpGrant, s: &str) {
        self.inner.close_stream(g, s);
    }
}

/// Reads one request and answers it, returning the request head and body.
fn exchange(listener: &TcpListener, status: &str, content_type: &str, body: &str) -> String {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).unwrap();
        assert_ne!(read, 0);
        bytes.extend_from_slice(&chunk[..read]);
    }
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The host attaches nothing: the packaged component reads its sealed record,
/// answers a rejection by exchanging the refresh token itself, and seals the
/// replacement under the same handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires loopback sockets"]
#[allow(clippy::too_many_lines)]
async fn packaged_codex_refreshes_its_own_credential_after_401() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let rejected = exchange(
            &listener,
            "401 Unauthorized",
            "application/json",
            "{\"error\":{\"code\":\"token_expired\"}}",
        );
        assert!(rejected.contains(&format!("Bearer {OLD}")));
        assert!(
            rejected
                .to_lowercase()
                .contains("chatgpt-account-id: fixture")
        );

        let refresh = exchange(
            &listener,
            "200 OK",
            "application/json",
            &json!({"access_token":NEW, "refresh_token":"new-refresh"}).to_string(),
        );
        assert!(refresh.contains("POST /oauth/token"));
        assert!(refresh.contains("refresh_token=old-refresh"));

        let replayed = exchange(
            &listener,
            "200 OK",
            "text/event-stream",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        );
        assert!(replayed.contains(&format!("Bearer {NEW}")));
    });

    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let package = PluginPackage::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/openai-codex"),
    )
    .unwrap();
    assert_eq!(
        package.manifest().credentials[0].flow_schema,
        "pluribus:credential/plugin@1"
    );
    let provider = package.manifest().id.clone();
    let handle = SecretHandle::new(HANDLE);
    // Tokens and a staged enrollment share one private record.
    assert!(
        store
            .replace_plugin_credential(
                &handle,
                &provider,
                None,
                serde_json::to_vec(&json!({"access_token":OLD,"refresh_token":"old-refresh", "enrollment":{"id":"pending", "expires_at_ms":i64::MAX}}))
                    .unwrap(),
            )
            .await
            .unwrap()
    );

    let blobs = Arc::new(InMemoryBlobStore::default());
    let http = Arc::new(FixtureHttp {
        inner: PolicyHttpService::new(blobs.clone()),
        origin,
        concurrent_write: None,
        reenroll_during_exchange: false,
    });
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        blobs,
        store.clone(),
    )
    .unwrap();
    let principal = PrincipalRef::new(PrincipalKind::Component, "codex-1/main");
    let mut instance = runtime
        .instantiate(
            package.component("main").unwrap(),
            &json!({"credentials": {"subscription": HANDLE},"models":["fixture-model"],"timeout_ms":10000}),
            Delivery {
                instance_id: "codex-1/main".into(),
                agent: Principal {
                    kind: WasmPrincipalKind::Agent,
                    id: "personal".into(),
                },
                actor: Principal {
                    kind: WasmPrincipalKind::Agent,
                    id: "personal".into(),
                },
                authority_id: "test".into(),
                activity_id: "test".into(),
                correlation_id: "test".into(),
                origin_event_id: "origin".into(),
                depth: 0,
                deadline_at_ms: None,
                visible_blobs: vec![],
            },
            PluginServices {
                credentials: Some(CredentialAccess {
                    store: store.clone(),
                    provider: provider.clone(),
                    handles: HashSet::from([HANDLE.to_owned()]),
                    exports: std::collections::BTreeMap::new(),
                }),
                http: Some(http),
                http_grant: Some(HttpGrant {
                    component: principal,
                    origins: vec![
                        "https://chatgpt.com".into(),
                        "https://auth.openai.com".into(),
                    ],
                    methods: vec!["POST".into()],
                    allow_http: false,
                    allow_private_network: false,
                    max_request_bytes: 1_000_000,
                    max_response_bytes: 1_000_000,
                    max_redirects: 0,
                    max_timeout_ms: 10000,
                }),
                ..PluginServices::default()
            },
        )
        .await
        .unwrap();
    instance.init().await.unwrap();
    let event=store.append(AppendRequest{stream_id:StreamId::new("personal"),stream_kind:StreamKind::Agent,observed_at_ms:None,event_type:"model.requested".into(),payload_schema:"pluribus.model-request/1".into(),payload:EventPayload::CanonicalJson(serde_json::to_vec(&json!({"call_id":"call-1","model":"fixture-model","messages":[{"role":"user","content":[{"kind":"text","text":"hello"}]}],"tools":[]})).unwrap()),actor:PrincipalRef::new(PrincipalKind::Component,"rlm"),authority_id:None,activity_id:None,correlation_id:None,causation_id:None,deduplication_key:None}).await.unwrap();
    let result = instance.handle(&[event]).await.unwrap();
    assert_eq!(
        result
            .events
            .iter()
            .filter(|e| e.request.event_type == "model.completed")
            .count(),
        1,
        "{:?}",
        result.events
    );
    assert!(
        !result
            .events
            .iter()
            .any(|e| e.request.event_type == "model.failed")
    );
    let record: Value = serde_json::from_slice(
        &store
            .read_plugin_credential(&handle, &provider)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(record["access_token"], NEW);
    assert_eq!(record["refresh_token"], "new-refresh");
    assert_eq!(record["enrollment"]["id"], "pending");
    server.join().unwrap();
}

struct PendingTimer {
    request: pluribus_core::EventId,
    due_at_ms: i64,
    instance_id: String,
}
async fn fire_timer(
    events: &dyn pluribus_core::EventStore,
    stream: &pluribus_core::StreamId,
    _agent: &pluribus_core::PrincipalRef,
    timer: &PendingTimer,
) -> Result<pluribus_core::CommittedEvent, Box<dyn std::error::Error>> {
    let _ = &timer.instance_id;
    events.append(pluribus_core::AppendRequest {
        stream_id: stream.clone(), stream_kind: pluribus_core::StreamKind::Agent,
        observed_at_ms: None, event_type: "timer.fired".into(), payload_schema: "pluribus.timer-fired/1".into(),
        payload: pluribus_core::EventPayload::CanonicalJson(serde_json::to_vec(&serde_json::json!({"requestEventId":timer.request.as_str(),"dueAtMs":timer.due_at_ms})).unwrap()),
        actor: pluribus_core::PrincipalRef::new(pluribus_core::PrincipalKind::Component, "scheduler"),
        authority_id: None, activity_id: None, correlation_id: None, causation_id: Some(timer.request.clone()),
        deduplication_key: Some(format!("timer:{}", timer.request.as_str())),
    }).await.map_err(Into::into)
}
