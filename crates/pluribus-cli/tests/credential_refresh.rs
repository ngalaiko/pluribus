use pluribus_core::*;
use pluribus_host_http::PolicyHttpService;
use pluribus_host_oauth::{
    Clock, CredentialEnrollment, DeviceCodeStatus, EnrollmentPolicy, OAuthError, OAuthHttpRequest,
    OAuthHttpResponse, OAuthTransport, RefreshingCredentialStore,
};
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, PluginServices, Principal, PrincipalKind as WasmPrincipalKind, Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

const ORIGIN: &str = "https://chatgpt.com";
const PATH: &str = "/backend-api/codex/responses";
const NOW: i64 = 1_700_000_000_000;
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
struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        NOW
    }
}
struct TokenEndpoint(AtomicU64);
#[async_trait::async_trait]
impl OAuthTransport for TokenEndpoint {
    async fn post(&self, request: &OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError> {
        let count = self.0.fetch_add(1, Ordering::SeqCst);
        let response = match count {
            0 => json!({"device_auth_id":"device", "user_code":"fixture", "interval":1}),
            1 => json!({"authorization_code":"code", "code_verifier":"verifier"}),
            2 => json!({"access_token":OLD, "refresh_token":"old-refresh", "expires_in":3600}),
            3 => {
                assert_eq!(request.url, "https://auth.openai.com/oauth/token");
                assert!(
                    String::from_utf8_lossy(&request.body).contains("refresh_token=old-refresh")
                );
                json!({"access_token":NEW, "refresh_token":"new-refresh", "expires_in":3600})
            }
            _ => panic!("unexpected token request"),
        };
        Ok(OAuthHttpResponse {
            status: 200,
            body: serde_json::to_vec(&response).unwrap(),
        })
    }
}

// Maps the fixture endpoint to the provider identity at the credential boundary.
struct FixtureCredentials(Arc<RefreshingCredentialStore>);
#[async_trait::async_trait]
impl CredentialStore for FixtureCredentials {
    async fn resolve_http(
        &self,
        h: &SecretHandle,
        c: &PrincipalRef,
        _: &str,
    ) -> Result<HttpCredential, SecretError> {
        self.0.resolve_http(h, c, ORIGIN).await
    }
    async fn resolve_http_versioned(
        &self,
        h: &SecretHandle,
        c: &PrincipalRef,
        _: &str,
        d: Instant,
    ) -> Result<ResolvedHttpCredential, SecretError> {
        self.0.resolve_http_versioned(h, c, ORIGIN, d).await
    }
    async fn recover_http(
        &self,
        handle: &SecretHandle,
        component: &PrincipalRef,
        _: &str,
        generation: u64,
        status: u16,
        body: &[u8],
        method: &str,
        path: &str,
        deadline: Instant,
    ) -> Result<AuthRecovery, SecretError> {
        self.0
            .recover_http(
                handle, component, ORIGIN, generation, status, body, method, path, deadline,
            )
            .await
    }
}

// Routes only the packaged provider's fixed URL to the local HTTP fixture.
struct FixtureHttp {
    inner: PolicyHttpService,
    origin: String,
}
impl FixtureHttp {
    fn mapped(&self, g: &HttpGrant, r: &HttpRequest) -> (HttpGrant, HttpRequest) {
        assert_eq!(r.url, format!("{ORIGIN}{PATH}"));
        let mut grant = g.clone();
        grant.origins = vec![self.origin.clone()];
        grant.allow_http = true;
        grant.allow_private_network = true;
        let mut request = r.clone();
        request.url = format!("{}{PATH}", self.origin);
        (grant, request)
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires loopback sockets"]
#[allow(clippy::too_many_lines)]
async fn packaged_codex_refreshes_future_expiry_after_401() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (token, status, body) in [
            (
                OLD,
                "401 Unauthorized",
                "{\"error\":{\"code\":\"token_expired\"}}",
            ),
            (
                NEW,
                "200 OK",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    assert!(headers.contains(&format!("Bearer {token}")));
                    break (end + 4, length);
                }
            };
            while bytes.len() < header_end + length {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            write!(stream,"HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let store = Arc::new(
        SqliteEventStore::open_in_memory(Metadata(AtomicU64::new(1)))
            .await
            .unwrap(),
    );
    let tokens = Arc::new(TokenEndpoint(AtomicU64::new(0)));
    let enrollment = CredentialEnrollment::new(store.clone(), tokens.clone(), Arc::new(FixedClock));
    let flow: Value = serde_json::from_str(include_str!(
        "../../../target/plugins/openai-codex/flows/subscription.json"
    ))
    .unwrap();
    let principal = PrincipalRef::new(PrincipalKind::Component, "codex-1/main");
    let handle = SecretHandle::new("codex:fixture");
    let mut session = enrollment
        .begin_device(
            &json!({"type":"object","additionalProperties":false}),
            &flow,
            &json!({}),
            handle.clone(),
            EnrollmentPolicy {
                components: vec![principal.clone()],
                enrollment_origins: vec!["https://auth.openai.com".into()],
                injection_origins: vec![ORIGIN.into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        enrollment.poll_device(&mut session).await.unwrap(),
        DeviceCodeStatus::Authorized
    );
    assert!(store.load_oauth(&handle).await.unwrap().expires_at_ms > NOW);
    let before = store.load_oauth_snapshot(&handle).await.unwrap().generation;
    let blobs = Arc::new(InMemoryBlobStore::default());
    let credentials = Arc::new(FixtureCredentials(Arc::new(
        RefreshingCredentialStore::new(store.clone(), tokens.clone(), Arc::new(FixedClock)),
    )));
    let http = Arc::new(FixtureHttp {
        inner: PolicyHttpService::new(blobs.clone(), credentials),
        origin,
    });
    let runtime = Runtime::new(
        RuntimeLimits::default(),
        store.clone(),
        store.clone(),
        blobs,
        store.clone(),
    )
    .unwrap();
    let package = PluginPackage::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/openai-codex"),
    )
    .unwrap();
    assert_eq!(
        package.manifest().credentials[0].flow_schema,
        "pluribus:credential/oauth-device@1"
    );
    let mut instance = runtime
        .instantiate(
            package.component("main").unwrap(),
            &json!({"credentials": {"subscription": "codex:fixture"},"models":["fixture-model"],"timeout_ms":10000}),
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
                http: Some(http),
                http_grant: Some(HttpGrant {
                    component: principal,
                    origins: vec![ORIGIN.into()],
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
    assert_eq!(tokens.0.load(Ordering::SeqCst), 4);
    let after = store.load_oauth_snapshot(&handle).await.unwrap();
    assert!(after.generation > before);
    assert_eq!(after.credential.refresh_token, b"new-refresh");
    server.join().unwrap();
}
