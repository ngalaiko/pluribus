use std::fmt;

/// Stable identity understood by the core.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PrincipalRef {
    pub kind: PrincipalKind,
    pub id: PrincipalId,
}

impl PrincipalRef {
    #[must_use]
    pub fn new(kind: PrincipalKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: PrincipalId::new(id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PrincipalKind {
    Human,
    Agent,
    Node,
    Component,
    External,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PrincipalId(String);

impl PrincipalId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
