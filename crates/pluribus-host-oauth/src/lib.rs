//! Host-owned credential enrollment and OAuth refresh.

mod flow;

pub use flow::{CredentialEnrollment, DeviceEnrollmentSession, EnrollmentPolicy};

use base64::Engine as _;
use pluribus_core::{
    AuthRecovery, CredentialStatus, CredentialStore, HttpCredential, OAuthCredential,
    OAuthCredentialStore, PrincipalRef, ResolvedHttpCredential, SecretError, SecretHandle,
};
use reqwest::Client;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::Value;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;
use url::form_urlencoded::Serializer;

const REFRESH_SKEW: Duration = Duration::from_mins(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceCodePrompt {
    pub user_code: String,
    pub verification_url: String,
    pub interval: Duration,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceCodeStatus {
    Pending,
    SlowDown,
    Authorized,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OAuthError {
    InvalidRequest(String),
    InvalidResponse(String),
    AuthorizationDenied,
    Expired,
    Unavailable(String),
    Storage,
    Backoff(i64),
}

impl fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(formatter, "invalid OAuth request: {message}"),
            Self::InvalidResponse(message) => {
                write!(formatter, "invalid OAuth response: {message}")
            }
            Self::AuthorizationDenied => formatter.write_str("OAuth authorization denied"),
            Self::Expired => formatter.write_str("OAuth authorization expired"),
            Self::Unavailable(message) => write!(formatter, "OAuth service unavailable: {message}"),
            Self::Storage => formatter.write_str("OAuth credential storage failed"),
            Self::Backoff(_) => formatter.write_str("OAuth credential refresh backoff"),
        }
    }
}

impl Error for OAuthError {}

#[derive(Clone, Eq, PartialEq)]
pub struct OAuthHttpRequest {
    pub url: String,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
}

impl fmt::Debug for OAuthHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthHttpRequest")
            .field("url", &self.url)
            .field("content_type", &self.content_type)
            .field(
                "body",
                &format_args!("[REDACTED; {} bytes]", self.body.len()),
            )
            .field("headers", &"[REDACTED]")
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct OAuthHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl fmt::Debug for OAuthHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthHttpResponse")
            .field("status", &self.status)
            .field(
                "body",
                &format_args!("[REDACTED; {} bytes]", self.body.len()),
            )
            .finish()
    }
}

#[async_trait::async_trait]

pub trait OAuthTransport: Send + Sync {
    /// Sends one request to a host-approved OAuth endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when transport fails.
    async fn post(&self, request: &OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError>;
}

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX)
    }
}

pub struct ReqwestOAuthTransport {
    client: Client,
}

impl ReqwestOAuthTransport {
    /// Builds a transport without proxies or redirects.
    ///
    /// # Errors
    ///
    /// Returns an error when the TLS client cannot be built.
    pub fn new() -> Result<Self, OAuthError> {
        let client = Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| OAuthError::Unavailable("cannot build HTTP client".into()))?;
        Ok(Self { client })
    }
}

#[async_trait::async_trait]

impl OAuthTransport for ReqwestOAuthTransport {
    async fn post(&self, request: &OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError> {
        validate_endpoint(&request.url)?;
        let mut builder = self
            .client
            .post(&request.url)
            .header(reqwest::header::CONTENT_TYPE, request.content_type)
            .body(request.body.clone())
            .timeout(request.timeout);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|_| OAuthError::Unavailable("HTTP request failed".into()))?;
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES)
        {
            return Err(OAuthError::InvalidResponse("response exceeds 1 MiB".into()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| OAuthError::Unavailable("cannot read HTTP response".into()))?
        {
            if body.len() as u64 + chunk.len() as u64 > MAX_RESPONSE_BYTES {
                return Err(OAuthError::InvalidResponse("response exceeds 1 MiB".into()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(OAuthHttpResponse { status, body })
    }
}

/// Resolves HTTP credentials and refreshes standard bearer OAuth tokens.
pub struct RefreshingCredentialStore {
    store: Arc<dyn OAuthCredentialStore>,
    transport: Arc<dyn OAuthTransport>,
    clock: Arc<dyn Clock>,
    enrollment_policies: Option<Vec<(PrincipalRef, Vec<String>)>>,
}

impl RefreshingCredentialStore {
    #[must_use]
    pub fn new(
        store: Arc<dyn OAuthCredentialStore>,
        transport: Arc<dyn OAuthTransport>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            transport,
            clock,
            enrollment_policies: None,
        }
    }

    #[must_use]
    pub fn with_enrollment_policies(mut self, policies: Vec<(PrincipalRef, Vec<String>)>) -> Self {
        self.enrollment_policies = Some(policies);
        self
    }

    fn authorize_refresh(
        &self,
        component: &PrincipalRef,
        credential: &OAuthCredential,
    ) -> Result<(), SecretError> {
        if let Some(policies) = &self.enrollment_policies {
            let origin = Url::parse(&credential.token_url)
                .map_err(|_| SecretError::PermissionDenied)?
                .origin()
                .ascii_serialization();
            if !policies
                .iter()
                .any(|(owner, origins)| owner == component && origins.contains(&origin))
            {
                return Err(SecretError::PermissionDenied);
            }
        }
        Ok(())
    }

    async fn refresh(
        &self,
        handle: &SecretHandle,
        generation: u64,
        deadline: Instant,
    ) -> Result<(), OAuthError> {
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| OAuthError::Unavailable("credential deadline exceeded".into()))?;
            let snapshot = self
                .store
                .load_oauth_snapshot(handle)
                .await
                .map_err(secret_error)?;
            if snapshot.generation != generation {
                return credential_status(&snapshot.status);
            }
            match snapshot.status {
                CredentialStatus::ReauthorizationRequired | CredentialStatus::UnknownOutcome => {
                    return Err(OAuthError::AuthorizationDenied);
                }
                CredentialStatus::Backoff { retry_at_ms } if self.clock.now_ms() < retry_at_ms => {
                    return Err(OAuthError::Unavailable("credential refresh backoff".into()));
                }
                _ => {}
            }
            let timeout = remaining.min(REQUEST_TIMEOUT);
            let deadline_ms = add_duration(self.clock.now_ms(), timeout)?;
            if !self
                .store
                .begin_refresh(handle, generation, self.clock.now_ms(), deadline_ms)
                .await
                .map_err(secret_error)?
            {
                tokio::time::sleep(Duration::from_millis(2).min(remaining)).await;
                continue;
            }
            let current = snapshot.credential;
            let recipe = current
                .refresh_recipe
                .as_deref()
                .map(flow::StoredRecipe::decode)
                .transpose();
            let request = recipe
                .as_ref()
                .map_err(|_| OAuthError::Storage)
                .and_then(|recipe| {
                    if let Some(recipe) = recipe {
                        recipe.request(&current, timeout)
                    } else {
                        legacy_request(&current, timeout)
                    }
                });
            let request = match request {
                Ok(request) => request,
                Err(error) => {
                    self.store
                        .fail_refresh(
                            handle,
                            generation,
                            CredentialStatus::ReauthorizationRequired,
                            self.clock.now_ms(),
                        )
                        .await
                        .map_err(secret_error)?;
                    return Err(error);
                }
            };
            let result = self.transport.post(&request).await.and_then(|response| {
                self.apply_response(
                    &current,
                    recipe.as_ref().map_err(|_| OAuthError::Storage)?.as_ref(),
                    &response,
                )
            });
            match result {
                Ok(refreshed) => {
                    return self
                        .commit_refreshed(handle, generation, &refreshed, deadline)
                        .await;
                }
                Err(error) => {
                    let status = match error {
                        OAuthError::AuthorizationDenied | OAuthError::Expired => {
                            CredentialStatus::ReauthorizationRequired
                        }
                        OAuthError::Backoff(retry_at_ms) => {
                            CredentialStatus::Backoff { retry_at_ms }
                        }
                        _ => CredentialStatus::UnknownOutcome,
                    };
                    self.store
                        .fail_refresh(handle, generation, status, self.clock.now_ms())
                        .await
                        .map_err(secret_error)?;
                    return Err(error);
                }
            }
        }
    }
    async fn commit_refreshed(
        &self,
        handle: &SecretHandle,
        generation: u64,
        refreshed: &OAuthCredential,
        deadline: Instant,
    ) -> Result<(), OAuthError> {
        match self
            .store
            .finish_refresh(handle, generation, refreshed, self.clock.now_ms())
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                let snapshot = self
                    .store
                    .load_oauth_snapshot(handle)
                    .await
                    .map_err(secret_error)?;
                credential_status(&snapshot.status)?;
            }
            Err(error) => {
                let _ = self
                    .store
                    .fail_refresh(
                        handle,
                        generation,
                        CredentialStatus::UnknownOutcome,
                        self.clock.now_ms(),
                    )
                    .await;
                return Err(secret_error(error));
            }
        }
        if Instant::now() >= deadline {
            return Err(OAuthError::Unavailable(
                "credential deadline exceeded".into(),
            ));
        }
        Ok(())
    }

    fn apply_response(
        &self,
        current: &OAuthCredential,
        recipe: Option<&flow::StoredRecipe>,
        response: &OAuthHttpResponse,
    ) -> Result<OAuthCredential, OAuthError> {
        if !(200..300).contains(&response.status) {
            let invalid_grant = serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
                .is_some_and(|v| {
                    matches!(
                        v.as_str(),
                        "invalid_grant" | "invalid_token" | "unauthorized_client"
                    )
                });
            if invalid_grant {
                return Err(OAuthError::AuthorizationDenied);
            }
            if let Some(recipe) = recipe
                && let Some(until) = recipe.backoff(response.status, self.clock.now_ms())?
            {
                return Err(OAuthError::Backoff(until));
            }
            return Err(OAuthError::Unavailable(
                "credential refresh rejected".into(),
            ));
        }
        if let Some(recipe) = recipe {
            recipe.apply(current, &response.body, self.clock.now_ms())
        } else {
            refresh_legacy(current.clone(), response, self.clock.now_ms())
        }
    }
}

fn legacy_request(
    current: &OAuthCredential,
    timeout: Duration,
) -> Result<OAuthHttpRequest, OAuthError> {
    Ok(OAuthHttpRequest {
        url: current.token_url.clone(),
        content_type: "application/x-www-form-urlencoded",
        headers: Vec::new(),
        timeout,
        body: form(&[
            ("grant_type", "refresh_token"),
            (
                "refresh_token",
                std::str::from_utf8(&current.refresh_token).map_err(|_| OAuthError::Storage)?,
            ),
            ("client_id", &current.client_id),
        ]),
    })
}

fn credential_status(status: &CredentialStatus) -> Result<(), OAuthError> {
    match status {
        CredentialStatus::Usable => Ok(()),
        CredentialStatus::ReauthorizationRequired | CredentialStatus::UnknownOutcome => {
            Err(OAuthError::AuthorizationDenied)
        }
        CredentialStatus::Refreshing { .. } | CredentialStatus::Backoff { .. } => Err(
            OAuthError::Unavailable("credential refresh unavailable".into()),
        ),
    }
}

fn refresh_legacy(
    mut current: OAuthCredential,
    response: &OAuthHttpResponse,
    now_ms: i64,
) -> Result<OAuthCredential, OAuthError> {
    let token = parse_refresh_response(response, now_ms)?;
    let authorization = current
        .http
        .headers
        .iter_mut()
        .find(|header| header.name.eq_ignore_ascii_case("authorization"))
        .ok_or_else(|| {
            OAuthError::InvalidRequest("renewable credential has no authorization header".into())
        })?;
    authorization.value = format!("Bearer {}", token.access_token).into_bytes();
    current.access_token = Some(token.access_token.into_bytes());
    if let Some(token) = token.refresh_token {
        current.refresh_token = token.into_bytes();
    }
    current.expires_at_ms = token.expires_at_ms;
    Ok(current)
}

#[async_trait::async_trait]

impl CredentialStore for RefreshingCredentialStore {
    async fn resolve_http(
        &self,
        handle: &SecretHandle,
        component: &PrincipalRef,
        origin: &str,
    ) -> Result<HttpCredential, SecretError> {
        self.resolve_http_versioned(handle, component, origin, Instant::now() + REQUEST_TIMEOUT)
            .await
            .map(|resolved| resolved.credential)
    }

    async fn resolve_http_versioned(
        &self,
        handle: &SecretHandle,
        component: &PrincipalRef,
        origin: &str,
        deadline: Instant,
    ) -> Result<ResolvedHttpCredential, SecretError> {
        let resolved = self.store.resolve_http(handle, component, origin).await?;
        match self.store.load_oauth_snapshot(handle).await {
            Ok(snapshot) => {
                // Authorization is checked on the same snapshot used for refresh.
                if !snapshot
                    .credential
                    .http
                    .allowed_origins
                    .iter()
                    .any(|v| v == origin)
                    || !snapshot
                        .credential
                        .http
                        .allowed_components
                        .contains(component)
                {
                    return Err(SecretError::PermissionDenied);
                }
                if needs_refresh(&snapshot.credential, self.clock.now_ms()).map_err(oauth_secret)?
                    || !matches!(snapshot.status, CredentialStatus::Usable)
                {
                    self.authorize_refresh(component, &snapshot.credential)?;
                    self.refresh(handle, snapshot.generation, deadline)
                        .await
                        .map_err(oauth_secret)?;
                }
                let snapshot = self.store.load_oauth_snapshot(handle).await?;
                credential_status(&snapshot.status).map_err(oauth_secret)?;
                if !snapshot
                    .credential
                    .http
                    .allowed_origins
                    .iter()
                    .any(|v| v == origin)
                    || !snapshot
                        .credential
                        .http
                        .allowed_components
                        .contains(component)
                {
                    return Err(SecretError::PermissionDenied);
                }
                Ok(ResolvedHttpCredential {
                    credential: snapshot.credential.http,
                    generation: Some(snapshot.generation),
                })
            }
            Err(SecretError::NotFound) => Ok(ResolvedHttpCredential {
                credential: resolved,
                generation: None,
            }),
            Err(error) => Err(error),
        }
    }

    async fn recover_http(
        &self,
        handle: &SecretHandle,
        component: &PrincipalRef,
        origin: &str,
        generation: u64,
        status: u16,
        body: &[u8],
        method: &str,
        path: &str,
        deadline: Instant,
    ) -> Result<AuthRecovery, SecretError> {
        self.store.resolve_http(handle, component, origin).await?;
        let snapshot = self.store.load_oauth_snapshot(handle).await?;
        if !snapshot
            .credential
            .http
            .allowed_origins
            .iter()
            .any(|v| v == origin)
            || !snapshot
                .credential
                .http
                .allowed_components
                .contains(component)
        {
            return Err(SecretError::PermissionDenied);
        }
        let Some(recipe) = snapshot
            .credential
            .refresh_recipe
            .as_deref()
            .map(flow::StoredRecipe::decode)
            .transpose()
            .map_err(oauth_secret)?
        else {
            return Ok(AuthRecovery::NotMatched);
        };
        if !recipe.matches(status, body) {
            return Ok(AuthRecovery::NotMatched);
        }
        self.authorize_refresh(component, &snapshot.credential)?;
        self.refresh(handle, generation, deadline)
            .await
            .map_err(oauth_secret)?;
        self.store.resolve_http(handle, component, origin).await?;
        Ok(AuthRecovery::Refreshed {
            replay_allowed: recipe.replay(origin, method, path),
        })
    }
}

struct ParsedRefreshToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: i64,
}

#[derive(Deserialize)]
struct RefreshTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

fn parse_refresh_response(
    response: &OAuthHttpResponse,
    now_ms: i64,
) -> Result<ParsedRefreshToken, OAuthError> {
    require_success(response, "token refresh")?;
    let token: RefreshTokenResponse = serde_json::from_slice(&response.body)
        .map_err(|_| OAuthError::InvalidResponse("token refresh fields are missing".into()))?;
    if token.access_token.is_empty()
        || token.refresh_token.as_ref().is_some_and(String::is_empty)
        || token.expires_in == 0
    {
        return Err(OAuthError::InvalidResponse(
            "token refresh fields are empty".into(),
        ));
    }
    Ok(ParsedRefreshToken {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at_ms: add_duration(now_ms, Duration::from_secs(token.expires_in))?,
    })
}

fn account_id_claim(access_token: &str, pointer: &str) -> Result<String, OAuthError> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or_else(|| OAuthError::InvalidResponse("access token is not a JWT".into()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| OAuthError::InvalidResponse("access token payload is invalid".into()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| OAuthError::InvalidResponse("access token payload is not JSON".into()))?;
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| OAuthError::InvalidResponse("access token has no declared claim".into()))
}

fn require_success(response: &OAuthHttpResponse, operation: &str) -> Result<(), OAuthError> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        Err(OAuthError::Unavailable(format!(
            "{operation} failed with status {}",
            response.status
        )))
    }
}

fn form(values: &[(&str, &str)]) -> Vec<u8> {
    let mut serializer = Serializer::new(String::new());
    serializer.extend_pairs(values.iter().copied());
    serializer.finish().into_bytes()
}

fn needs_refresh(credential: &OAuthCredential, now_ms: i64) -> Result<bool, OAuthError> {
    let skew = credential
        .refresh_recipe
        .as_deref()
        .map(flow::StoredRecipe::decode)
        .transpose()?
        .map_or(
            i64::try_from(REFRESH_SKEW.as_millis()).unwrap_or(i64::MAX),
            |recipe| recipe.skew_ms(),
        );
    let refresh_at = credential
        .expires_at_ms
        .checked_sub(skew)
        .unwrap_or(i64::MIN);
    Ok(now_ms >= refresh_at)
}

fn add_duration(now_ms: i64, duration: Duration) -> Result<i64, OAuthError> {
    let duration_ms = i64::try_from(duration.as_millis())
        .map_err(|_| OAuthError::InvalidResponse("token lifetime overflow".into()))?;
    now_ms
        .checked_add(duration_ms)
        .ok_or_else(|| OAuthError::InvalidResponse("token expiry overflow".into()))
}

fn validate_endpoint(value: &str) -> Result<(), OAuthError> {
    let url = Url::parse(value)
        .map_err(|_| OAuthError::InvalidRequest("OAuth endpoint is malformed".into()))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.port_or_known_default() != Some(443)
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(OAuthError::InvalidRequest(
            "OAuth endpoint is not approved".into(),
        ));
    }
    Ok(())
}

fn secret_error(error: SecretError) -> OAuthError {
    match error {
        SecretError::Invalid(message) => OAuthError::InvalidRequest(message),
        SecretError::NotFound
        | SecretError::PermissionDenied
        | SecretError::ReauthorizationRequired
        | SecretError::Storage(_) => OAuthError::Storage,
    }
}

fn oauth_secret(error: OAuthError) -> SecretError {
    match error {
        OAuthError::InvalidRequest(message) | OAuthError::InvalidResponse(message) => {
            SecretError::Invalid(message)
        }
        OAuthError::AuthorizationDenied | OAuthError::Expired => {
            SecretError::ReauthorizationRequired
        }
        OAuthError::Unavailable(message) => SecretError::Storage(message),
        OAuthError::Storage => SecretError::Storage("OAuth service failed".into()),
        OAuthError::Backoff(_) => SecretError::Storage("credential refresh backoff".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pluribus_core::{InMemoryCredentialStore, PrincipalKind, SecretHeader};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    struct FakeTransport {
        responses: Mutex<VecDeque<OAuthHttpResponse>>,
        calls: AtomicUsize,
    }

    impl FakeTransport {
        fn new(responses: Vec<OAuthHttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]

    impl OAuthTransport for FakeTransport {
        async fn post(&self, _request: &OAuthHttpRequest) -> Result<OAuthHttpResponse, OAuthError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| OAuthError::Unavailable("missing fake response".into()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generic_refresh_preserves_provider_headers_and_old_refresh_token() {
        let transport = Arc::new(FakeTransport::new(vec![response(
            200,
            &json!({
                "access_token": "new-access",
                "expires_in": 3600
            }),
        )]));
        let store = Arc::new(InMemoryCredentialStore::default());
        let handle = SecretHandle::new("example");
        let component = PrincipalRef::new(PrincipalKind::Component, "example-1");
        store
            .put_oauth(
                &handle,
                &OAuthCredential {
                    provider: "example".into(),
                    http: HttpCredential {
                        headers: vec![
                            SecretHeader {
                                name: "authorization".into(),
                                value: b"Bearer old-access".to_vec(),
                            },
                            SecretHeader {
                                name: "account-id".into(),
                                value: b"account-1".to_vec(),
                            },
                        ],
                        path_prefix: None,
                        allowed_origins: vec!["https://api.example.com".into()],
                        allowed_components: vec![component.clone()],
                    },
                    refresh_token: b"old-refresh".to_vec(),
                    expires_at_ms: 1_000,
                    token_url: "https://auth.example.com/token".into(),
                    client_id: "client".into(),
                    refresh_recipe: None,
                    access_token: None,
                },
            )
            .await
            .unwrap();
        let credential_store: Arc<dyn OAuthCredentialStore> = store.clone();
        let refreshing = Arc::new(RefreshingCredentialStore::new(
            credential_store,
            transport.clone(),
            Arc::new(FixedClock(2_000)),
        ));
        let credentials = (0..2)
            .map(|_| {
                let refreshing = Arc::clone(&refreshing);
                let handle = handle.clone();
                let component = component.clone();
                tokio::spawn(async move {
                    refreshing
                        .resolve_http(&handle, &component, "https://api.example.com")
                        .await
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let mut resolved = Vec::new();
        for task in credentials {
            resolved.push(task.await.unwrap());
        }
        let credentials = resolved;

        assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
        assert!(
            credentials
                .iter()
                .all(|credential| credential.headers[0].value == b"Bearer new-access")
        );
        assert!(
            credentials
                .iter()
                .all(|credential| credential.headers[1].value == b"account-1")
        );
        assert_eq!(
            store.load_oauth(&handle).await.unwrap().refresh_token,
            b"old-refresh"
        );
    }

    #[test]
    fn provider_error_does_not_echo_response_body() {
        let response = OAuthHttpResponse {
            status: 500,
            body: b"refresh-super-secret".to_vec(),
        };
        let Err(error) = parse_refresh_response(&response, 0) else {
            panic!("expected token refresh failure");
        };

        assert!(!error.to_string().contains("refresh-super-secret"));
        assert!(error.to_string().contains("status 500"));
    }

    fn response(status: u16, body: &Value) -> OAuthHttpResponse {
        OAuthHttpResponse {
            status,
            body: serde_json::to_vec(&body).unwrap(),
        }
    }
}
