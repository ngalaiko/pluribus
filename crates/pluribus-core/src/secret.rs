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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretError {
    NotFound,
    PermissionDenied,
    Invalid(String),
    Storage(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("credential handle not found"),
            Self::PermissionDenied => formatter.write_str("credential use is not permitted"),
            Self::Invalid(message) => write!(formatter, "invalid credential: {message}"),
            Self::Storage(message) => write!(formatter, "credential storage failed: {message}"),
        }
    }
}

impl Error for SecretError {}

/// Host-only opaque credential state. Guest state and event APIs cannot read it.
#[async_trait::async_trait]
pub trait PluginCredentialStore: Send + Sync {
    async fn read_plugin_credential(
        &self,
        handle: &SecretHandle,
        provider: &str,
    ) -> Result<Option<Vec<u8>>, SecretError>;
    /// Atomically replaces the exact previous value; None requires absence.
    async fn replace_plugin_credential(
        &self,
        handle: &SecretHandle,
        provider: &str,
        expected: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<bool, SecretError>;
}

#[derive(Default)]
pub struct InMemoryCredentialStore {
    state: Mutex<HashMap<(SecretHandle, String), Vec<u8>>>,
}

#[async_trait::async_trait]
impl PluginCredentialStore for InMemoryCredentialStore {
    async fn read_plugin_credential(
        &self,
        handle: &SecretHandle,
        provider: &str,
    ) -> Result<Option<Vec<u8>>, SecretError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?
            .get(&(handle.clone(), provider.to_owned()))
            .cloned())
    }
    async fn replace_plugin_credential(
        &self,
        handle: &SecretHandle,
        provider: &str,
        expected: Option<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<bool, SecretError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecretError::Storage("credential lock poisoned".into()))?;
        let key = (handle.clone(), provider.to_owned());
        if state.get(&key) != expected.as_ref() {
            return Ok(false);
        }
        state.insert(key, value);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_record_is_scoped_to_its_provider_and_replaces_atomically() {
        let store = InMemoryCredentialStore::default();
        let handle = SecretHandle::new("mail:account");
        assert!(
            store
                .replace_plugin_credential(&handle, "dev.pluribus.email", None, b"first".to_vec())
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .read_plugin_credential(&handle, "dev.pluribus.email")
                .await
                .unwrap(),
            Some(b"first".to_vec())
        );
        assert_eq!(
            store
                .read_plugin_credential(&handle, "dev.pluribus.other")
                .await
                .unwrap(),
            None
        );
        assert!(
            !store
                .replace_plugin_credential(&handle, "dev.pluribus.email", None, b"second".to_vec())
                .await
                .unwrap()
        );
    }
}
