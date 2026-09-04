use std::error::Error;
use std::fmt;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StateNamespace(String);

impl StateNamespace {
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
pub struct StateEntry {
    pub key: String,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateSnapshot {
    pub revision: u64,
    pub value: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatePage {
    pub entries: Vec<StateEntry>,
    pub next_key: Option<String>,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateMutation {
    Set { key: String, value: Vec<u8> },
    Delete { key: String },
}

#[async_trait::async_trait]
pub trait StateStore: Send + Sync {
    /// Reads one value and its namespace revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the namespace cannot be read.
    async fn get(&self, namespace: &StateNamespace, key: &str)
    -> Result<StateSnapshot, StateError>;

    /// Reads lexicographically ordered keys from one namespace revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the namespace cannot be read.
    async fn scan(
        &self,
        namespace: &StateNamespace,
        prefix: &str,
        after_key: Option<&str>,
        limit: usize,
    ) -> Result<StatePage, StateError>;

    /// Atomically applies mutations when the expected revision matches.
    ///
    /// # Errors
    ///
    /// Returns `Conflict` without mutation on a revision mismatch.
    async fn apply(
        &self,
        namespace: &StateNamespace,
        expected_revision: u64,
        mutations: &[StateMutation],
    ) -> Result<u64, StateError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateError {
    Conflict { expected: u64, actual: u64 },
    Invalid(String),
    Storage(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { expected, actual } => {
                write!(
                    formatter,
                    "state revision conflict: expected {expected}, actual {actual}"
                )
            }
            Self::Invalid(message) => write!(formatter, "invalid state operation: {message}"),
            Self::Storage(message) => write!(formatter, "state storage failed: {message}"),
        }
    }
}

impl Error for StateError {}
