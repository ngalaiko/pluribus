//! Model request and completion payloads.
//!
//! These left the WIT package for JSON Schema: they were 40% of the ABI, and
//! pinning them there meant a new ABI version for every provider feature.
//! Capability arguments were already schema-identified JSON, so models now
//! travel the same way.
//!
//! The host and model plugins share this crate so both sides agree on the
//! encoding without the types being frozen into the component boundary.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const COMPLETION_SCHEMA: &str = "pluribus.model-completion/1";
pub const STREAM_SCHEMA: &str = "pluribus.model-stream/1";

/// A capability a model either honours for every request to it, or does not
/// advertise.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Feature {
    Vision,
    AudioInput,
    AudioOutput,
    Reasoning,
    PromptCaching,
    StructuredOutput,
    ParallelTools,
    Continuation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlobRef {
    pub algorithm: String,
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MediaPart {
    pub blob: BlobRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output_schema: String,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ContentPart {
    Text { text: String },
    Image(MediaPart),
    Audio(MediaPart),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    ProviderData { data: Value },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Message {
    pub role: MessageRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: Vec<ContentPart>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One model call. `call_id` correlates the request event with its stream and
/// completion events.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Request {
    pub call_id: String,
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub required_features: Vec<Feature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Opaque provider continuation from an earlier completion. Valid only for
    /// a compatible provider, account, model, and conversation lineage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<Value>,
}

/// Cumulative for the call. A missing field means unavailable, not zero.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub arguments_fragment: String,
}

/// One increment of a streaming completion. Argument fragments concatenate in
/// emission order and MUST form the final canonical JSON arguments.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Delta {
    Text { text: String },
    ReasoningSummary { text: String },
    ToolCall(ToolCallDelta),
    Usage(Usage),
}

/// Payload of a `model.stream` event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Stream {
    pub call_id: String,
    pub deltas: Vec<Delta>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StopReason {
    EndTurn,
    MaxOutput,
    ToolCall,
    ContentFilter,
    Other { reason: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Completion {
    pub call_id: String,
    /// The canonical accumulated assistant message. The host trusts this over
    /// the deltas for subsequent context.
    pub message: Message,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<Value>,
}

/// One model a provider serves, resolved from instance configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ModelEntry {
    Id(String),
    Descriptor {
        id: String,
        #[serde(default)]
        features: Vec<Feature>,
        #[serde(default)]
        context_tokens: Option<u64>,
        #[serde(default)]
        max_output_tokens: Option<u64>,
    },
}

impl ModelEntry {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Id(id) | Self::Descriptor { id, .. } => id,
        }
    }
}

impl Delta {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    #[must_use]
    pub fn reasoning_summary(text: impl Into<String>) -> Self {
        Self::ReasoningSummary { text: text.into() }
    }
}

impl ContentPart {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_round_trips() {
        let request = Request {
            call_id: "call-1".into(),
            model: "gpt-5.6-luna".into(),
            messages: vec![Message {
                role: MessageRole::User,
                name: None,
                content: vec![ContentPart::Text {
                    text: "hello".into(),
                }],
            }],
            tools: Vec::new(),
            required_features: vec![Feature::Reasoning],
            max_output_tokens: Some(256),
            output_schema: None,
            continuation: None,
            provider_options: None,
        };

        let bytes = serde_json::to_vec(&request).unwrap();
        let decoded: Request = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(decoded, request);
    }

    #[test]
    fn a_model_entry_is_a_string_or_a_descriptor() {
        let plain: ModelEntry = serde_json::from_value(json!("gpt-5.6-luna")).unwrap();
        let detailed: ModelEntry =
            serde_json::from_value(json!({"id": "gpt-5.6-luna", "features": ["vision"]})).unwrap();

        assert_eq!(plain.id(), "gpt-5.6-luna");
        assert_eq!(detailed.id(), "gpt-5.6-luna");
        assert!(matches!(
            detailed,
            ModelEntry::Descriptor { ref features, .. } if features == &[Feature::Vision]
        ));
    }

    #[test]
    fn an_unknown_provider_field_does_not_break_decoding() {
        let usage: Usage =
            serde_json::from_value(json!({"input_tokens": 10, "provider_metadata": {"x": 1}}))
                .unwrap();

        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, None, "absent means unavailable");
    }

    #[test]
    fn a_stop_reason_carries_its_provider_string() {
        let reason: StopReason =
            serde_json::from_value(json!({"kind": "other", "reason": "provider-specific"}))
                .unwrap();

        assert_eq!(
            reason,
            StopReason::Other {
                reason: "provider-specific".into()
            }
        );
    }
}
