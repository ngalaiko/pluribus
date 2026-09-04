//! Origin-scoped standing grants.
use crate::AuthorityResolver;
use pluribus_core::{
    Audience, Authority, AuthorityId, CapabilityName, CommittedEvent, ConstraintPolicy,
    ConstraintSet, EventPayload, EventStore, Grant, Origin, OriginKind, PrincipalKind,
    PrincipalRef, StreamId,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

/// One configured way in and out: the component that delivers observations and
/// the component that answers them.
pub struct Connector {
    /// The plugin name the ingress component stamps on its observations.
    pub provider: String,
    /// Component principal of the ingress half, such as `telegram-1/receive`.
    pub ingress: String,
    /// Component principal of the reply half, such as `telegram-1/send`.
    pub reply: String,
    /// What the reply component provides, confined to the conversation the
    /// observation arrived in.
    pub reply_capabilities: Vec<CapabilityName>,
}

pub struct OriginAuthority {
    pub agent: PrincipalRef,
    pub events: Arc<dyn EventStore>,
    /// Every connector the configuration installs, resolved per observation.
    pub connectors: Vec<Connector>,
    pub trusted_senders: Vec<String>,
    pub grants: BTreeMap<CapabilityName, Vec<Grant>>,
    pub max_depth: u32,
}
#[async_trait::async_trait]
impl AuthorityResolver for OriginAuthority {
    async fn resolve(&self, event: &CommittedEvent) -> Result<Authority, String> {
        let mut origin = event.clone();
        for _ in 0..4096 {
            if origin.request.stream_id != StreamId::new(self.agent.id.as_str()) {
                return Err("origin belongs to another agent".into());
            }
            if origin.request.event_type == "observation.received" {
                return self.issue(&origin);
            }
            let cause = origin
                .request
                .causation_id
                .as_ref()
                .ok_or("request has no observation origin")?;
            let parent = self
                .events
                .get(cause)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("origin not found")?;
            if parent.sequence >= origin.sequence {
                return Err("invalid origin chain".into());
            }
            origin = parent;
        }
        Err("origin chain exceeds limit".into())
    }
}
impl OriginAuthority {
    fn issue(&self, event: &CommittedEvent) -> Result<Authority, String> {
        // The component that appended the observation picks the connector; a
        // component cannot answer for another provider.
        let connector = self
            .connectors
            .iter()
            .find(|connector| {
                event.request.actor
                    == PrincipalRef::new(PrincipalKind::Component, &connector.ingress)
            })
            .ok_or("unrecognized observation connector")?;
        let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
            return Err("invalid observation".into());
        };
        let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if value["provider"] != connector.provider.as_str() {
            return Err("unsupported origin".into());
        }
        let conversation = value["conversationId"]
            .as_str()
            .ok_or("missing conversation")?;
        let sender = value["externalSenderId"].as_str().ok_or("missing sender")?;
        let trusted = self.trusted_senders.iter().any(|id| id == sender);
        let principal = PrincipalRef::new(
            if trusted {
                PrincipalKind::Human
            } else {
                PrincipalKind::External
            },
            format!("{}:{sender}", connector.provider),
        );
        let actor = event.request.actor.clone();
        let mut grants = if trusted {
            self.grants.clone()
        } else {
            BTreeMap::new()
        };
        // Replies remain confined to the originating conversation.
        for capability in &connector.reply_capabilities {
            let capability = capability.clone();
            grants.insert(
                capability.clone(),
                vec![Grant {
                    capability,
                    provider: Some(PrincipalRef::new(
                        PrincipalKind::Component,
                        &connector.reply,
                    )),
                    constraints: ConstraintSet::canonical_json(
                        serde_json::to_vec(&json!({"conversation_ids":[conversation]})).unwrap(),
                    ),
                }],
            );
        }
        Ok(Authority {
            schema: Authority::SCHEMA.into(),
            authority_id: AuthorityId::new(format!("origin:{}", event.event_id.as_str())),
            agent: self.agent.clone(),
            origin: Origin {
                kind: OriginKind::Connector,
                principal: Some(principal.clone()),
                connector: Some(actor.clone()),
                conversation_id: Some(conversation.into()),
                source_event_id: event.event_id.clone(),
                trusted,
            },
            delegation_chain: vec![],
            grants,
            audiences: vec![Audience {
                connector: PrincipalRef::new(PrincipalKind::Component, &connector.reply),
                conversation_id: conversation.into(),
                recipients: vec![principal],
            }],
            parent_authority: None,
            issued_at_ms: event.recorded_at_ms,
            expires_at_ms: None,
            max_depth: self.max_depth,
            current_depth: 0,
        })
    }
}

pub struct OriginConstraints;
impl ConstraintPolicy for OriginConstraints {
    fn allows(&self, grant: &Grant, request: &[u8]) -> Result<bool, String> {
        let constraints: Value =
            serde_json::from_slice(grant.constraints.as_bytes()).map_err(|e| e.to_string())?;
        if grant.capability.as_str().starts_with("memory.") {
            let request: Value = serde_json::from_slice(request).map_err(|e| e.to_string())?;
            return Ok(request["scope"].as_str().is_some_and(|scope| {
                constraints["scopes"].as_array().is_some_and(|scopes| {
                    scopes.iter().any(|allowed| allowed.as_str() == Some(scope))
                })
            }));
        }
        if constraints == json!({}) {
            return Ok(true);
        }
        let request: Value = serde_json::from_slice(request).map_err(|e| e.to_string())?;
        // The reply contract names its conversation; a provider's own sends
        // may name the chat instead.
        let conversation = if let Some(conversation) = request["conversationId"].as_str() {
            conversation.to_owned()
        } else if grant.capability.as_str().starts_with("telegram.") {
            let chat = match &request["chat_id"] {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => return Ok(false),
            };
            request["message_thread_id"].as_i64().map_or_else(
                || format!("chat:{chat}"),
                |thread| format!("chat:{chat}:thread:{thread}"),
            )
        } else {
            // An unrecognised request shape cannot be checked against a
            // constraint, so it is not allowed.
            return Ok(false);
        };
        Ok(constraints["conversation_ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&conversation))))
    }
}
