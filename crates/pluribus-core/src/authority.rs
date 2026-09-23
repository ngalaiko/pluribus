use crate::{EventId, PrincipalRef};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AuthorityId(String);

impl AuthorityId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityName(String);

impl CapabilityName {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CapabilityName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Canonical JSON interpreted by the capability provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConstraintSet(Vec<u8>);

impl ConstraintSet {
    #[must_use]
    pub fn canonical_json(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub capability: CapabilityName,
    pub provider: Option<PrincipalRef>,
    pub constraints: ConstraintSet,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Audience {
    pub connector: PrincipalRef,
    pub conversation_id: String,
    pub recipients: Vec<PrincipalRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginKind {
    Autonomous,
    /// An observation a connector plugin delivered. `Origin::connector` names
    /// the component; the principal carries the provider it came from.
    Connector,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin {
    pub kind: OriginKind,
    pub principal: Option<PrincipalRef>,
    pub connector: Option<PrincipalRef>,
    pub conversation_id: Option<String>,
    pub source_event_id: EventId,
    pub trusted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delegation {
    pub from: PrincipalRef,
    pub to: PrincipalRef,
    pub on_behalf_of: Option<PrincipalRef>,
    pub purpose: String,
    pub source_event_id: EventId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Authority {
    pub schema: String,
    pub authority_id: AuthorityId,
    pub agent: PrincipalRef,
    pub origin: Origin,
    pub delegation_chain: Vec<Delegation>,
    pub grants: BTreeMap<CapabilityName, Vec<Grant>>,
    pub audiences: Vec<Audience>,
    pub parent_authority: Option<AuthorityId>,
    pub issued_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub max_depth: u32,
    pub current_depth: u32,
}

impl Authority {
    pub const SCHEMA: &str = "pluribus.authority/1";

    #[must_use]
    pub fn permits(&self, capability: &CapabilityName) -> bool {
        self.grants.contains_key(capability)
    }

    #[must_use]
    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|expiry| now_ms >= expiry)
    }

    #[must_use]
    pub fn can_descend(&self) -> bool {
        self.current_depth < self.max_depth
    }

    /// Evaluates a capability request against immutable activity authority.
    #[must_use]
    pub fn decide(
        &self,
        capability: &CapabilityName,
        provider: &PrincipalRef,
        request: &[u8],
        policy: &dyn ConstraintPolicy,
        now_ms: i64,
    ) -> Decision {
        if self.is_expired_at(now_ms) {
            return Decision::Deny {
                reason: DenialReason::Expired,
            };
        }
        if self.current_depth > self.max_depth {
            return Decision::Deny {
                reason: DenialReason::DepthExceeded,
            };
        }
        let Some(grants) = self.grants.get(capability) else {
            return Decision::Deny {
                reason: DenialReason::MissingGrant,
            };
        };
        let matching = grants.iter().filter(|grant| {
            grant
                .provider
                .as_ref()
                .is_none_or(|value| value == provider)
        });
        let mut saw_provider = false;
        let mut policy_error = None;
        for grant in matching {
            saw_provider = true;
            match policy.allows(grant, request) {
                Ok(true) => {
                    return Decision::Allow {
                        grant: grant.clone(),
                    };
                }
                Ok(false) => {}
                Err(error) => policy_error = Some(error),
            }
        }
        let reason = if let Some(error) = policy_error {
            DenialReason::PolicyError(error)
        } else if saw_provider {
            DenialReason::ConstraintMismatch
        } else {
            DenialReason::MissingGrant
        };
        Decision::Deny { reason }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow { grant: Grant },
    Deny { reason: DenialReason },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DenialReason {
    MissingGrant,
    ConstraintMismatch,
    Expired,
    DepthExceeded,
    PolicyError(String),
}

/// Capability-owned constraint semantics.
pub trait ConstraintPolicy: Send + Sync {
    /// Checks a request against one grant.
    ///
    /// # Errors
    ///
    /// Returns an error when the constraint or request cannot be interpreted.
    fn allows(&self, grant: &Grant, request: &[u8]) -> Result<bool, String>;

    /// Checks host-projected selectors from the selected provider's contract.
    /// Policies without declarative bindings retain access to the original request.
    ///
    /// # Errors
    /// Returns an error when the grant, request, or selectors cannot be interpreted.
    fn allows_projected(
        &self,
        grant: &Grant,
        request: &[u8],
        _selectors: &[u8],
    ) -> Result<bool, String> {
        self.allows(grant, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority(expires_at_ms: Option<i64>, current_depth: u32) -> Authority {
        let capability = CapabilityName::new("battery.read");
        let grant = Grant {
            capability: capability.clone(),
            provider: None,
            constraints: ConstraintSet::canonical_json(b"{}"),
        };

        Authority {
            schema: Authority::SCHEMA.into(),
            authority_id: AuthorityId::new("authority"),
            agent: PrincipalRef::new(crate::PrincipalKind::Agent, "personal"),
            origin: Origin {
                kind: OriginKind::Autonomous,
                principal: None,
                connector: None,
                conversation_id: None,
                source_event_id: EventId::new("event"),
                trusted: true,
            },
            delegation_chain: Vec::new(),
            grants: BTreeMap::from([(capability, vec![grant])]),
            audiences: Vec::new(),
            parent_authority: None,
            issued_at_ms: 10,
            expires_at_ms,
            max_depth: 4,
            current_depth,
        }
    }

    #[test]
    fn authority_denies_unknown_capabilities() {
        let active = authority(None, 0);

        assert!(active.permits(&CapabilityName::new("battery.read")));
        assert!(!active.permits(&CapabilityName::new("shell.exec")));
    }

    #[test]
    fn expiry_is_exclusive() {
        let authority = authority(Some(20), 0);

        assert!(!authority.is_expired_at(19));
        assert!(authority.is_expired_at(20));
    }

    #[test]
    fn depth_limit_is_enforced() {
        assert!(authority(None, 3).can_descend());
        assert!(!authority(None, 4).can_descend());
    }

    struct FixedPolicy(Result<bool, &'static str>);

    impl ConstraintPolicy for FixedPolicy {
        fn allows(&self, _grant: &Grant, _request: &[u8]) -> Result<bool, String> {
            self.0.map_err(str::to_owned)
        }
    }

    #[test]
    fn decision_checks_expiry_provider_and_constraints() {
        let active = authority(None, 0);
        let capability = CapabilityName::new("battery.read");
        let provider = PrincipalRef::new(crate::PrincipalKind::Component, "battery-1");

        assert!(matches!(
            active.decide(&capability, &provider, b"{}", &FixedPolicy(Ok(true)), 10),
            Decision::Allow { .. }
        ));
        assert_eq!(
            active.decide(&capability, &provider, b"{}", &FixedPolicy(Ok(false)), 10),
            Decision::Deny {
                reason: DenialReason::ConstraintMismatch
            }
        );
        let expired = authority(Some(20), 0);
        assert_eq!(
            expired.decide(&capability, &provider, b"{}", &FixedPolicy(Ok(true)), 20),
            Decision::Deny {
                reason: DenialReason::Expired
            }
        );
    }
}
