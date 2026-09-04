use base64::Engine as _;
use pluribus_core::{
    AuthRecovery, CredentialStatus, CredentialStore, InMemoryCredentialStore, OAuthCredentialStore,
    PrincipalKind, PrincipalRef, SecretError, SecretHandle,
};
use pluribus_host_oauth::{
    Clock, CredentialEnrollment, EnrollmentPolicy, OAuthError, OAuthHttpRequest, OAuthHttpResponse,
    OAuthTransport, RefreshingCredentialStore,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
struct Transport {
    calls: AtomicUsize,
    response: Mutex<Result<OAuthHttpResponse, OAuthError>>,
    entered: Option<mpsc::Sender<()>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}
#[async_trait::async_trait]
impl OAuthTransport for Transport {
    async fn post(&self, _request: &OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(response(
                &json!({"device_auth_id":"device","user_code":"code"}),
            )),
            1 => Ok(response(
                &json!({"authorization_code":"code","code_verifier":"verifier"}),
            )),
            2 => Ok(response(&tokens("old"))),
            _ => {
                if let Some(entered) = &self.entered {
                    entered.send(()).unwrap();
                }
                if let Some(release) = self.release.lock().unwrap().take() {
                    tokio::task::block_in_place(|| release.recv().unwrap());
                }
                self.response.lock().unwrap().clone()
            }
        }
    }
}
fn response(body: &Value) -> OAuthHttpResponse {
    OAuthHttpResponse {
        status: 200,
        body: serde_json::to_vec(body).unwrap(),
    }
}
fn tokens(token: &str) -> Value {
    let claims = json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"},"fixture":token});
    let access = format!(
        "e30.{}.sig",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap())
    );
    json!({"access_token":access,"refresh_token":format!("refresh-{token}"),"expires_in":3600})
}
fn component() -> PrincipalRef {
    PrincipalRef::new(PrincipalKind::Component, "codex")
}
fn handle() -> SecretHandle {
    SecretHandle::new("codex-fixture")
}
async fn enrollment(
    store: Arc<InMemoryCredentialStore>,
    transport: Arc<Transport>,
    clock: Arc<TestClock>,
    backoff: bool,
) {
    let enrollment = CredentialEnrollment::new(store, transport, clock);
    let mut flow: Value = serde_json::from_str(include_str!(
        "../../../plugins/openai-codex/flows/subscription.json"
    ))
    .unwrap();
    if backoff {
        flow["refresh"]["failure"] = json!({"retryableStatuses":[429],"backoffSeconds":3});
    }
    let mut session = enrollment
        .begin_device(
            &json!({"type":"object"}),
            &flow,
            &json!({}),
            handle(),
            EnrollmentPolicy {
                components: vec![component()],
                enrollment_origins: vec!["https://auth.openai.com".into()],
                injection_origins: vec!["https://chatgpt.com".into()],
            },
        )
        .await
        .unwrap();
    enrollment.poll_device(&mut session).await.unwrap();
}
async fn recover(
    refreshing: &RefreshingCredentialStore,
    generation: u64,
) -> Result<AuthRecovery, SecretError> {
    refreshing
        .recover_http(
            &handle(),
            &component(),
            "https://chatgpt.com",
            generation,
            401,
            b"{}",
            "POST",
            "/backend-api/codex/responses",
            Instant::now() + Duration::from_secs(5),
        )
        .await
}
fn transport(result: Result<OAuthHttpResponse, OAuthError>) -> Arc<Transport> {
    Arc::new(Transport {
        calls: AtomicUsize::new(0),
        response: Mutex::new(result),
        entered: None,
        release: Mutex::new(None),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_uncertainty_survives_resolver_restart_without_rotating_again() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let transport = transport(Err(OAuthError::Unavailable(
        "fixture transport unavailable".into(),
    )));
    enrollment(store.clone(), transport.clone(), clock.clone(), false).await;
    let generation = store
        .load_oauth_snapshot(&handle())
        .await
        .unwrap()
        .generation;
    let first = RefreshingCredentialStore::new(store.clone(), transport.clone(), clock.clone());
    assert!(recover(&first, generation).await.is_err());
    assert_eq!(
        store.load_oauth_snapshot(&handle()).await.unwrap().status,
        CredentialStatus::UnknownOutcome
    );
    let restarted = RefreshingCredentialStore::new(store, transport.clone(), clock);
    assert_eq!(
        restarted
            .resolve_http(&handle(), &component(), "https://chatgpt.com")
            .await,
        Err(SecretError::ReauthorizationRequired)
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_backoff_survives_resolver_restart_and_expires_on_clock() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let transport = transport(Ok(OAuthHttpResponse {
        status: 429,
        body: b"{}".to_vec(),
    }));
    enrollment(store.clone(), transport.clone(), clock.clone(), true).await;
    let generation = store
        .load_oauth_snapshot(&handle())
        .await
        .unwrap()
        .generation;
    let first = RefreshingCredentialStore::new(store.clone(), transport.clone(), clock.clone());
    assert!(recover(&first, generation).await.is_err());
    assert_eq!(
        store.load_oauth_snapshot(&handle()).await.unwrap().status,
        CredentialStatus::Backoff { retry_at_ms: 4000 }
    );
    let restarted = RefreshingCredentialStore::new(store.clone(), transport.clone(), clock.clone());
    assert!(
        restarted
            .resolve_http(&handle(), &component(), "https://chatgpt.com")
            .await
            .is_err()
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 4);
    clock.0.store(4000, Ordering::SeqCst);
    *transport.response.lock().unwrap() = Ok(response(&tokens("new")));
    restarted
        .resolve_http(&handle(), &component(), "https://chatgpt.com")
        .await
        .unwrap();
    assert_eq!(transport.calls.load(Ordering::SeqCst), 5);
    assert_eq!(
        store.load_oauth_snapshot(&handle()).await.unwrap().status,
        CredentialStatus::Usable
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_refresh_cannot_overwrite_concurrent_enrollment() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let transport = Arc::new(Transport {
        calls: AtomicUsize::new(0),
        response: Mutex::new(Ok(response(&tokens("rotated")))),
        entered: Some(entered_tx),
        release: Mutex::new(Some(release_rx)),
    });
    enrollment(store.clone(), transport.clone(), clock.clone(), false).await;
    let before = store.load_oauth_snapshot(&handle()).await.unwrap();
    let refreshing = RefreshingCredentialStore::new(store.clone(), transport.clone(), clock);
    let worker = tokio::spawn(async move { recover(&refreshing, before.generation).await });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut replacement = before.credential;
    replacement.refresh_token = b"new-enrollment".to_vec();
    store.put_oauth(&handle(), &replacement).await.unwrap();
    release_tx.send(()).unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(
        store.load_oauth(&handle()).await.unwrap().refresh_token,
        b"new-enrollment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_resolvers_share_storage_claim_and_old_generation() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let transport = transport(Ok(response(&tokens("rotated"))));
    enrollment(store.clone(), transport.clone(), clock.clone(), false).await;
    let generation = store
        .load_oauth_snapshot(&handle())
        .await
        .unwrap()
        .generation;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let workers = (0..2)
        .map(|_| {
            let refreshing =
                RefreshingCredentialStore::new(store.clone(), transport.clone(), clock.clone());
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                recover(&refreshing, generation).await
            })
        })
        .collect::<Vec<_>>();
    barrier.wait().await;
    for worker in workers {
        worker.await.unwrap().unwrap();
    }
    assert_eq!(transport.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_installation_endpoint_and_component_never_refresh() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let transport = transport(Ok(response(&tokens("rotated"))));
    enrollment(store.clone(), transport.clone(), clock.clone(), false).await;
    let generation = store
        .load_oauth_snapshot(&handle())
        .await
        .unwrap()
        .generation;
    let refreshing = RefreshingCredentialStore::new(store, transport.clone(), clock)
        .with_enrollment_policies(vec![(component(), Vec::new())]);
    assert_eq!(
        recover(&refreshing, generation).await,
        Err(SecretError::PermissionDenied)
    );
    assert_eq!(
        refreshing
            .resolve_http(
                &handle(),
                &PrincipalRef::new(PrincipalKind::Component, "other"),
                "https://chatgpt.com"
            )
            .await,
        Err(SecretError::PermissionDenied)
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_legacy_adoption_preserves_tokens_and_enables_recovery_without_enrollment() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let transport = transport(Ok(response(&tokens("rotated"))));
    enrollment(store.clone(), transport.clone(), clock.clone(), false).await;
    let mut legacy = store.load_oauth(&handle()).await.unwrap();
    legacy.refresh_recipe = None;
    legacy.access_token = None;
    legacy.client_id = "app_EMoamEEZ73f0CkXaXp7hrann".into();
    store.put_oauth(&handle(), &legacy).await.unwrap();
    let enrollment = CredentialEnrollment::new(store.clone(), transport.clone(), clock.clone());
    let flow: Value = serde_json::from_str(include_str!(
        "../../../plugins/openai-codex/flows/subscription.json"
    ))
    .unwrap();
    let policy = EnrollmentPolicy {
        components: vec![component()],
        enrollment_origins: vec!["https://auth.openai.com".into()],
        injection_origins: vec!["https://chatgpt.com".into()],
    };
    enrollment
        .adopt_device_recipe(
            &json!({"type":"object"}),
            &flow,
            &json!({}),
            &handle(),
            &policy,
        )
        .await
        .unwrap();
    assert_eq!(transport.calls.load(Ordering::SeqCst), 3);
    let adopted = store.load_oauth_snapshot(&handle()).await.unwrap();
    assert_eq!(adopted.credential.http, legacy.http);
    assert_eq!(adopted.credential.refresh_token, legacy.refresh_token);
    enrollment
        .adopt_device_recipe(
            &json!({"type":"object"}),
            &flow,
            &json!({}),
            &handle(),
            &policy,
        )
        .await
        .unwrap();
    let refreshing = RefreshingCredentialStore::new(store, transport.clone(), clock);
    recover(&refreshing, adopted.generation).await.unwrap();
    assert_eq!(transport.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_claim_during_rotation_cannot_report_recovery() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let transport = Arc::new(Transport {
        calls: AtomicUsize::new(0),
        response: Mutex::new(Ok(response(&tokens("rotated")))),
        entered: Some(entered_tx),
        release: Mutex::new(Some(release_rx)),
    });
    enrollment(store.clone(), transport.clone(), clock.clone(), false).await;
    let generation = store
        .load_oauth_snapshot(&handle())
        .await
        .unwrap()
        .generation;
    let refreshing = RefreshingCredentialStore::new(store.clone(), transport, clock);
    let worker = tokio::spawn(async move { recover(&refreshing, generation).await });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        !store
            .begin_refresh(&handle(), generation, 100_000, 101_000)
            .await
            .unwrap()
    );
    release_tx.send(()).unwrap();
    assert_eq!(
        worker.await.unwrap(),
        Err(SecretError::ReauthorizationRequired)
    );
    assert_eq!(
        store.load_oauth_snapshot(&handle()).await.unwrap().status,
        CredentialStatus::UnknownOutcome
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_recipe_adoption_checks_consumers_and_refresh_preserves_scope() {
    let store = Arc::new(InMemoryCredentialStore::default());
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let transport = transport(Ok(response(&tokens("rotated"))));
    enrollment(store.clone(), transport.clone(), clock.clone(), false).await;
    let sibling = PrincipalRef::new(PrincipalKind::Component, "codex/sibling");
    let mut legacy = store.load_oauth(&handle()).await.unwrap();
    legacy.refresh_recipe = None;
    legacy.access_token = None;
    legacy.client_id = "app_EMoamEEZ73f0CkXaXp7hrann".into();
    legacy.http.allowed_components.push(sibling.clone());
    store.put_oauth(&handle(), &legacy).await.unwrap();
    let generation = store
        .load_oauth_snapshot(&handle())
        .await
        .unwrap()
        .generation;
    let enrollment = CredentialEnrollment::new(store.clone(), transport.clone(), clock.clone());
    let flow: Value = serde_json::from_str(include_str!(
        "../../../plugins/openai-codex/flows/subscription.json"
    ))
    .unwrap();
    let mut policy = EnrollmentPolicy {
        components: vec![
            component(),
            PrincipalRef::new(PrincipalKind::Component, "foreign"),
        ],
        enrollment_origins: vec!["https://auth.openai.com".into()],
        injection_origins: vec!["https://chatgpt.com".into()],
    };
    assert_eq!(
        enrollment
            .adopt_device_recipe(
                &json!({"type":"object"}),
                &flow,
                &json!({}),
                &handle(),
                &policy
            )
            .await,
        Err(OAuthError::AuthorizationDenied)
    );
    assert_eq!(
        store
            .load_oauth_snapshot(&handle())
            .await
            .unwrap()
            .generation,
        generation
    );
    policy.components = legacy.http.allowed_components.clone();
    enrollment
        .adopt_device_recipe(
            &json!({"type":"object"}),
            &flow,
            &json!({}),
            &handle(),
            &policy,
        )
        .await
        .unwrap();
    let adopted = store.load_oauth_snapshot(&handle()).await.unwrap();
    let refreshing = RefreshingCredentialStore::new(store.clone(), transport.clone(), clock);
    refreshing
        .recover_http(
            &handle(),
            &sibling,
            "https://chatgpt.com",
            adopted.generation,
            401,
            b"{}",
            "POST",
            "/backend-api/codex/responses",
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .load_oauth(&handle())
            .await
            .unwrap()
            .http
            .allowed_components,
        policy.components
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 4);
}
