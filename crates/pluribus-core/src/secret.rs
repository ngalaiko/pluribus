use crate::PrincipalRef;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Mutex;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SecretHandle(String);

impl SecretHandle {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SecretHeader {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SecretPathPrefix(Vec<u8>);

impl SecretPathPrefix {
    #[must_use]
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretPathPrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[REDACTED; {} bytes]", self.0.len())
    }
}

impl fmt::Debug for SecretHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretHeader")
            .field("name", &self.name)
            .field(
                "value",
                &format_args!("[REDACTED; {} bytes]", self.value.len()),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpCredential {
    pub headers: Vec<SecretHeader>,
    pub path_prefix: Option<SecretPathPrefix>,
    pub allowed_origins: Vec<String>,
    pub allowed_components: Vec<PrincipalRef>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct OAuthCredential {
    pub refresh_recipe: Option<Vec<u8>>,
    pub access_token: Option<Vec<u8>>,
    pub provider: String,
    pub http: HttpCredential,
    pub refresh_token: Vec<u8>,
    pub expires_at_ms: i64,
    pub token_url: String,
    pub client_id: String,
}

impl fmt::Debug for OAuthCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCredential")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "refresh_recipe",
                &self.refresh_recipe.as_ref().map(|_| "[REDACTED]"),
            )
            .field("provider", &self.provider)
            .field("http", &self.http)
            .field(
                "refresh_token",
                &format_args!("[REDACTED; {} bytes]", self.refresh_token.len()),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedHttpCredential {
    pub credential: HttpCredential,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthRecovery {
    NotMatched,
    Refreshed { replay_allowed: bool },
}

#[async_trait::async_trait]

pub trait CredentialStore: Send + Sync {
    /// Resolves scoped injection and its generation.
    ///
    /// # Errors
    /// Returns an error for invalid state or unavailable storage.
    async fn resolve_http_versioned(
        &self,
        handle: &SecretHandle,
        component: &PrincipalRef,
        origin: &str,
        _deadline: std::time::Instant,
    ) -> Result<ResolvedHttpCredential, SecretError> {
        self.resolve_http(handle, component, origin)
            .await
            .map(|credential| ResolvedHttpCredential {
                credential,
                generation: None,
            })
    }
    #[allow(clippy::too_many_arguments)]
    /// Handles a bounded authentication rejection.
    ///
    /// # Errors
    /// Returns an error for invalid state or unavailable storage.
    async fn recover_http(
        &self,
        _handle: &SecretHandle,
        _component: &PrincipalRef,
        _origin: &str,
        _generation: u64,
        _status: u16,
        _body: &[u8],
        _method: &str,
        _path: &str,
        _deadline: std::time::Instant,
    ) -> Result<AuthRecovery, SecretError> {
        Ok(AuthRecovery::NotMatched)
    }

    /// Resolves a handle for one component and destination.
    ///
    /// # Errors
    ///
    /// Returns an error when the handle is missing, unavailable, or outside its scope.
    async fn resolve_http(
        &self,
        handle: &SecretHandle,
        component: &PrincipalRef,
        origin: &str,
    ) -> Result<HttpCredential, SecretError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialStatus {
    Usable,
    Refreshing { deadline_ms: i64 },
    Backoff { retry_at_ms: i64 },
    ReauthorizationRequired,
    UnknownOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthCredentialSnapshot {
    pub credential: OAuthCredential,
    pub generation: u64,
    pub status: CredentialStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialLifecycle {
    pub sequence: u64,
    pub handle: SecretHandle,
    pub generation: u64,
    pub outcome: String,
    pub at_ms: i64,
    pub deadline_ms: Option<i64>,
}

#[async_trait::async_trait]
pub trait OAuthCredentialStore: CredentialStore {
    /// Reads credential lifecycle records in sequence order, capped at 1000.
    ///
    /// # Errors
    /// Returns an error when storage is unavailable.
    async fn lifecycle_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<CredentialLifecycle>, SecretError>;

    /// Loads credential state.
    ///
    /// # Errors
    /// Returns an error for invalid state or unavailable storage.
    async fn load_oauth_snapshot(
        &self,
        handle: &SecretHandle,
    ) -> Result<OAuthCredentialSnapshot, SecretError>;
    /// Claims one generation; expired claims become unknown.
    ///
    /// # Errors
    /// Returns an error for invalid state or unavailable storage.
    async fn begin_refresh(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        now_ms: i64,
        deadline_ms: i64,
    ) -> Result<bool, SecretError>;
    /// Atomically replaces the claimed generation.
    ///
    /// # Errors
    /// Returns an error for invalid state or unavailable storage.
    async fn finish_refresh(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        credential: &OAuthCredential,
        completed_at_ms: i64,
    ) -> Result<bool, SecretError>;
    /// Adopts a validated recipe without replacing tokens or authority.
    ///
    /// # Errors
    /// Returns an error for invalid adoption or unavailable storage.
    async fn adopt_oauth_recipe(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        credential: &OAuthCredential,
        completed_at_ms: i64,
    ) -> Result<bool, SecretError>;

    /// Persists failure and invalidates the claim.
    ///
    /// # Errors
    /// Returns an error for invalid state or unavailable storage.
    async fn fail_refresh(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        status: CredentialStatus,
        completed_at_ms: i64,
    ) -> Result<bool, SecretError>;

    /// Adds or replaces a static HTTP credential.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata or unavailable storage.
    async fn put_http(
        &self,
        handle: &SecretHandle,
        credential: &HttpCredential,
    ) -> Result<(), SecretError>;

    /// Loads a renewable credential without exposing it to a plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the handle is missing or storage is unavailable.
    async fn load_oauth(&self, handle: &SecretHandle) -> Result<OAuthCredential, SecretError>;

    /// Adds or replaces a renewable credential.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata or unavailable storage.
    async fn put_oauth(
        &self,
        handle: &SecretHandle,
        credential: &OAuthCredential,
    ) -> Result<(), SecretError>;

    /// Removes any static or renewable credential under the handle.
    ///
    /// # Errors
    ///
    /// Returns an error when storage is unavailable.
    async fn remove_credential(&self, handle: &SecretHandle) -> Result<(), SecretError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretError {
    NotFound,
    ReauthorizationRequired,
    PermissionDenied,
    Invalid(String),
    Storage(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("credential handle not found"),
            Self::ReauthorizationRequired => {
                formatter.write_str("credential requires reauthorization")
            }
            Self::PermissionDenied => formatter.write_str("credential use is not permitted"),
            Self::Invalid(message) => write!(formatter, "invalid credential: {message}"),
            Self::Storage(message) => write!(formatter, "credential storage failed: {message}"),
        }
    }
}

impl Error for SecretError {}

#[derive(Default)]
struct MemoryCredentials {
    http: HashMap<SecretHandle, HttpCredential>,
    oauth: HashMap<SecretHandle, OAuthCredentialSnapshot>,
    generations: HashMap<SecretHandle, u64>,
    lifecycle: Vec<CredentialLifecycle>,
}

#[derive(Default)]
pub struct InMemoryCredentialStore {
    state: Mutex<MemoryCredentials>,
}

impl InMemoryCredentialStore {
    /// Updates a credential.
    ///
    /// # Errors
    /// Returns an error for invalid metadata or unavailable storage.
    pub fn put(&self, handle: SecretHandle, credential: HttpCredential) -> Result<(), SecretError> {
        validate_http_credential(&handle, &credential)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        *state.generations.entry(handle.clone()).or_default() += 1;
        state.oauth.remove(&handle);
        let generation = state.generations[&handle];
        record_memory_lifecycle(
            &mut state,
            &handle,
            generation,
            "enrolled",
            credential_time_ms(),
            None,
        );
        state.http.insert(handle, credential);
        Ok(())
    }

    /// Updates a credential.
    ///
    /// # Errors
    /// Returns an error for invalid metadata or unavailable storage.
    pub fn remove(&self, handle: &SecretHandle) -> Result<(), SecretError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        *state.generations.entry(handle.clone()).or_default() += 1;
        state.oauth.remove(handle);
        state.http.remove(handle);
        let generation = state.generations[handle];
        record_memory_lifecycle(
            &mut state,
            handle,
            generation,
            "revoked",
            credential_time_ms(),
            None,
        );
        Ok(())
    }
}

#[async_trait::async_trait]

impl CredentialStore for InMemoryCredentialStore {
    async fn resolve_http(
        &self,
        handle: &SecretHandle,
        component: &PrincipalRef,
        origin: &str,
    ) -> Result<HttpCredential, SecretError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        let credential = state.http.get(handle).ok_or(SecretError::NotFound)?;
        if !credential
            .allowed_origins
            .iter()
            .any(|value| value == origin)
            || !credential.allowed_components.contains(component)
        {
            return Err(SecretError::PermissionDenied);
        }
        Ok(credential.clone())
    }
}

#[async_trait::async_trait]
impl OAuthCredentialStore for InMemoryCredentialStore {
    async fn lifecycle_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<CredentialLifecycle>, SecretError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        Ok(state
            .lifecycle
            .iter()
            .filter(|entry| entry.sequence > after_sequence)
            .take(limit.min(1000))
            .cloned()
            .collect())
    }

    async fn put_http(
        &self,
        handle: &SecretHandle,
        credential: &HttpCredential,
    ) -> Result<(), SecretError> {
        self.put(handle.clone(), credential.clone())
    }

    async fn load_oauth(&self, handle: &SecretHandle) -> Result<OAuthCredential, SecretError> {
        Ok(self.load_oauth_snapshot(handle).await?.credential)
    }

    async fn load_oauth_snapshot(
        &self,
        handle: &SecretHandle,
    ) -> Result<OAuthCredentialSnapshot, SecretError> {
        self.state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?
            .oauth
            .get(handle)
            .cloned()
            .ok_or(SecretError::NotFound)
    }

    async fn put_oauth(
        &self,
        handle: &SecretHandle,
        credential: &OAuthCredential,
    ) -> Result<(), SecretError> {
        validate_oauth_credential(handle, credential)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        let generation = state.generations.entry(handle.clone()).or_default();
        *generation += 1;
        let snapshot = OAuthCredentialSnapshot {
            credential: credential.clone(),
            generation: *generation,
            status: CredentialStatus::Usable,
        };
        state.http.insert(handle.clone(), credential.http.clone());
        let generation = snapshot.generation;
        state.oauth.insert(handle.clone(), snapshot);
        record_memory_lifecycle(
            &mut state,
            handle,
            generation,
            "enrolled",
            credential_time_ms(),
            None,
        );
        Ok(())
    }

    async fn begin_refresh(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        now_ms: i64,
        deadline_ms: i64,
    ) -> Result<bool, SecretError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        let snapshot = state.oauth.get_mut(handle).ok_or(SecretError::NotFound)?;
        if snapshot.generation != expected_generation {
            return Ok(false);
        }
        match snapshot.status {
            CredentialStatus::Usable => {}
            CredentialStatus::Backoff { retry_at_ms } if retry_at_ms <= now_ms => {}
            CredentialStatus::Refreshing { deadline_ms } if deadline_ms <= now_ms => {
                snapshot.status = CredentialStatus::UnknownOutcome;
                record_memory_lifecycle(
                    &mut state,
                    handle,
                    expected_generation,
                    "unknown",
                    now_ms,
                    None,
                );
                return Ok(false);
            }
            _ => return Ok(false),
        }
        if deadline_ms <= now_ms {
            return Err(SecretError::Invalid("refresh deadline elapsed".into()));
        }
        snapshot.status = CredentialStatus::Refreshing { deadline_ms };
        record_memory_lifecycle(
            &mut state,
            handle,
            expected_generation,
            "refreshing",
            now_ms,
            Some(deadline_ms),
        );
        Ok(true)
    }

    async fn finish_refresh(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        credential: &OAuthCredential,
        completed_at_ms: i64,
    ) -> Result<bool, SecretError> {
        validate_oauth_credential(handle, credential)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        let Some(snapshot) = state.oauth.get_mut(handle) else {
            return Ok(false);
        };
        if snapshot.generation != expected_generation
            || !matches!(snapshot.status, CredentialStatus::Refreshing { .. })
        {
            return Ok(false);
        }
        validate_refresh_replacement(&snapshot.credential, credential)?;
        snapshot.credential = credential.clone();
        snapshot.generation += 1;
        snapshot.status = CredentialStatus::Usable;
        let generation = snapshot.generation;
        state.generations.insert(handle.clone(), generation);
        state.http.insert(handle.clone(), credential.http.clone());
        record_memory_lifecycle(
            &mut state,
            handle,
            generation,
            "refreshed",
            completed_at_ms,
            None,
        );
        Ok(true)
    }

    async fn adopt_oauth_recipe(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        credential: &OAuthCredential,
        completed_at_ms: i64,
    ) -> Result<bool, SecretError> {
        validate_oauth_credential(handle, credential)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        let Some(snapshot) = state.oauth.get_mut(handle) else {
            return Ok(false);
        };
        if snapshot.generation != expected_generation || snapshot.status != CredentialStatus::Usable
        {
            return Ok(false);
        }
        validate_recipe_adoption(&snapshot.credential, credential)?;
        if snapshot.credential.refresh_recipe.is_some() {
            return Ok(true);
        }
        snapshot.credential = credential.clone();
        snapshot.generation += 1;
        let generation = snapshot.generation;
        state.generations.insert(handle.clone(), generation);
        record_memory_lifecycle(
            &mut state,
            handle,
            generation,
            "recipe-adopted",
            completed_at_ms,
            None,
        );
        Ok(true)
    }

    async fn fail_refresh(
        &self,
        handle: &SecretHandle,
        expected_generation: u64,
        status: CredentialStatus,
        completed_at_ms: i64,
    ) -> Result<bool, SecretError> {
        if matches!(
            status,
            CredentialStatus::Usable | CredentialStatus::Refreshing { .. }
        ) {
            return Err(SecretError::Invalid(
                "invalid refresh failure status".into(),
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        let Some(snapshot) = state.oauth.get_mut(handle) else {
            return Ok(false);
        };
        if snapshot.generation != expected_generation
            || !matches!(snapshot.status, CredentialStatus::Refreshing { .. })
        {
            return Ok(false);
        }
        let (outcome, deadline_ms) = match status {
            CredentialStatus::Backoff { retry_at_ms } => ("backoff", Some(retry_at_ms)),
            CredentialStatus::ReauthorizationRequired => ("reauthorization", None),
            _ => ("unknown", None),
        };
        snapshot.status = status;
        snapshot.generation += 1;
        let generation = snapshot.generation;
        state.generations.insert(handle.clone(), generation);
        record_memory_lifecycle(
            &mut state,
            handle,
            generation,
            outcome,
            completed_at_ms,
            deadline_ms,
        );
        Ok(true)
    }

    async fn remove_credential(&self, handle: &SecretHandle) -> Result<(), SecretError> {
        self.remove(handle)
    }
}

fn record_memory_lifecycle(
    state: &mut MemoryCredentials,
    handle: &SecretHandle,
    generation: u64,
    outcome: &str,
    at_ms: i64,
    deadline_ms: Option<i64>,
) {
    state.lifecycle.push(CredentialLifecycle {
        sequence: state.lifecycle.len() as u64 + 1,
        handle: handle.clone(),
        generation,
        outcome: outcome.into(),
        at_ms,
        deadline_ms,
    });
}

/// Returns the host clock for enrollment and revocation records.
#[must_use]
pub fn credential_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

/// Validates a credential handle, header, and non-empty scope.
///
/// # Errors
///
/// Returns an error for an empty handle, malformed header, or empty scope.
pub fn validate_http_credential(
    handle: &SecretHandle,
    credential: &HttpCredential,
) -> Result<(), SecretError> {
    if handle.as_str().is_empty() {
        return Err(SecretError::Invalid("handle is empty".into()));
    }
    if credential.headers.is_empty() && credential.path_prefix.is_none() {
        return Err(SecretError::Invalid(
            "credential has no injection method".into(),
        ));
    }
    if let Some(prefix) = &credential.path_prefix {
        let bytes = prefix.as_bytes();
        if bytes.is_empty()
            || bytes[0] != b'/'
            || bytes
                .iter()
                .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b'?' | b'#' | b'\\'))
        {
            return Err(SecretError::Invalid(
                "credential path prefix is malformed".into(),
            ));
        }
    }
    let mut names = Vec::with_capacity(credential.headers.len());
    for header in &credential.headers {
        if header.name.is_empty() || !header.name.bytes().all(is_header_name_byte) {
            return Err(SecretError::Invalid("header name is malformed".into()));
        }
        if header.value.is_empty()
            || header
                .value
                .iter()
                .any(|byte| (*byte < 0x20 && *byte != b'\t') || *byte == 0x7f)
        {
            return Err(SecretError::Invalid("header value is malformed".into()));
        }
        let name = header.name.to_ascii_lowercase();
        if names.contains(&name) {
            return Err(SecretError::Invalid("credential repeats a header".into()));
        }
        names.push(name);
    }
    if credential.allowed_origins.is_empty() || credential.allowed_components.is_empty() {
        return Err(SecretError::Invalid("credential scope is empty".into()));
    }
    Ok(())
}

/// Validates a renewable credential and its HTTP scope.
///
/// # Errors
///
/// Returns an error for missing provider, token metadata, refresh token, or HTTP scope.
pub fn validate_oauth_credential(
    handle: &SecretHandle,
    credential: &OAuthCredential,
) -> Result<(), SecretError> {
    validate_http_credential(handle, &credential.http)?;
    if credential.provider.is_empty()
        || credential.refresh_token.is_empty()
        || credential.token_url.is_empty()
        || credential.client_id.is_empty()
    {
        return Err(SecretError::Invalid(
            "OAuth credential metadata is incomplete".into(),
        ));
    }
    Ok(())
}

/// Checks the immutable authority and recipe of a refresh replacement.
///
/// # Errors
/// Returns an error if refresh changes enrollment authority.
pub fn validate_refresh_replacement(
    previous: &OAuthCredential,
    replacement: &OAuthCredential,
) -> Result<(), SecretError> {
    if previous.provider != replacement.provider
        || previous.token_url != replacement.token_url
        || previous.client_id != replacement.client_id
        || previous.refresh_recipe != replacement.refresh_recipe
        || previous.http.allowed_origins != replacement.http.allowed_origins
        || previous.http.allowed_components != replacement.http.allowed_components
    {
        return Err(SecretError::Invalid(
            "refresh changes enrollment authority".into(),
        ));
    }
    Ok(())
}

/// Checks recipe adoption without changing enrollment state.
///
/// # Errors
/// Returns an error when tokens, authority, or an installed recipe change.
pub fn validate_recipe_adoption(
    previous: &OAuthCredential,
    replacement: &OAuthCredential,
) -> Result<(), SecretError> {
    let mut comparable = replacement.clone();
    comparable
        .refresh_recipe
        .clone_from(&previous.refresh_recipe);
    comparable.access_token.clone_from(&previous.access_token);
    if replacement.refresh_recipe.is_none()
        || comparable != *previous
        || (previous.refresh_recipe.is_some()
            && replacement.refresh_recipe != previous.refresh_recipe)
    {
        return Err(SecretError::Invalid(
            "recipe adoption changes enrollment".into(),
        ));
    }
    Ok(())
}

fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PrincipalKind;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn credentials_require_matching_component_and_origin() {
        let store = InMemoryCredentialStore::default();
        let handle = SecretHandle::new("telegram");
        let component = PrincipalRef::new(PrincipalKind::Component, "telegram-1");
        store
            .put(
                handle.clone(),
                HttpCredential {
                    headers: vec![SecretHeader {
                        name: "authorization".into(),
                        value: b"Bearer secret".to_vec(),
                    }],
                    path_prefix: None,
                    allowed_origins: vec!["https://api.telegram.org".into()],
                    allowed_components: vec![component.clone()],
                },
            )
            .unwrap();

        assert!(
            store
                .resolve_http(&handle, &component, "https://api.telegram.org")
                .await
                .is_ok()
        );
        assert_eq!(
            store
                .resolve_http(&handle, &component, "https://example.com")
                .await,
            Err(SecretError::PermissionDenied)
        );
    }
}
