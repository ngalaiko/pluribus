pub mod deepseek;
mod openai_compat;

use std::{fmt, pin::Pin};

use futures_lite::Stream;
use pluribus_frequency::protocol::{ContentPart, Message, ToolCall, ToolDef};

/// An incremental event from an LLM streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    /// A chunk of text content.
    TextDelta(String),
    /// A chunk of reasoning / chain-of-thought content.
    ReasoningDelta(String),
    /// A chunk of a tool call being built.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        function_name: Option<String>,
        arguments_delta: String,
    },
}

/// A stream of incremental LLM events.
pub type LlmEventStream<'a> =
    Pin<Box<dyn Stream<Item = exn::Result<LlmEvent, LlmError>> + Send + 'a>>;

/// An LLM provider that supports streaming completions.
pub trait Provider: Send + Sync {
    /// Human-readable provider name (e.g. "deepseek").
    fn name(&self) -> &str;

    /// The model identifier used in API requests.
    fn model_id(&self) -> &str;

    /// Start a streaming completion and return incremental events.
    fn complete_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        options: &'a GenOptions,
    ) -> LlmEventStream<'a>;
}

/// Options controlling LLM generation.
#[derive(Debug, Clone, Default)]
pub struct GenOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub stop: Option<Vec<String>>,
    pub seed: Option<i64>,
    /// Enable chain-of-thought reasoning (model decides depth).
    pub thinking: bool,
}

/// Errors from LLM operations.
#[derive(Debug)]
pub enum LlmError {
    Api { status: u16, body: String },
    Network(String),
    StreamParse(String),
    Truncated,
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api { status, body } => write!(f, "API error ({status}): {body}"),
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::StreamParse(msg) => write!(f, "stream parse error: {msg}"),
            Self::Truncated => f.write_str("response truncated"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Accumulates tool call deltas into complete [`ToolCall`] values.
#[derive(Default)]
pub struct ToolCallBuilder {
    entries: Vec<ToolCallEntry>,
}

#[derive(Default)]
struct ToolCallEntry {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallBuilder {
    /// Apply a tool call delta.
    pub fn push(&mut self, index: usize, id: Option<&str>, name: Option<&str>, args: &str) {
        if index >= self.entries.len() {
            self.entries.resize_with(index + 1, ToolCallEntry::default);
        }
        let entry = &mut self.entries[index];
        if let Some(id) = id {
            entry.id.push_str(id);
        }
        if let Some(name) = name {
            entry.name.push_str(name);
        }
        entry.arguments.push_str(args);
    }

    /// Consume the builder and return finished tool calls.
    #[must_use]
    pub fn finish(self) -> Vec<ToolCall> {
        self.entries
            .into_iter()
            .map(|e| ToolCall {
                id: e.id,
                name: e.name,
                arguments: e.arguments,
            })
            .collect()
    }
}

/// Collect a full response from a stream, returning an assistant [`Message`].
pub async fn collect_response(mut stream: LlmEventStream<'_>) -> exn::Result<Message, LlmError> {
    use futures_lite::StreamExt;

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut builder = ToolCallBuilder::default();

    while let Some(event) = stream.next().await {
        match event? {
            LlmEvent::TextDelta(delta) => content.push_str(&delta),
            LlmEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
            LlmEvent::ToolCallDelta {
                index,
                id,
                function_name,
                arguments_delta,
            } => {
                builder.push(
                    index,
                    id.as_deref(),
                    function_name.as_deref(),
                    &arguments_delta,
                );
            }
        }
    }

    let content_parts = if content.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(content)]
    };

    let reasoning_content = if reasoning.is_empty() {
        None
    } else {
        Some(reasoning)
    };

    Ok(Message::assistant_with_tool_calls(
        content_parts,
        builder.finish(),
        reasoning_content,
    ))
}

/// Create a new `deepseek-chat` LLM backend, validating the API key.
pub async fn deepseek_chat(api_key: &str) -> exn::Result<deepseek::DeepSeek, deepseek::Error> {
    deepseek::DeepSeek::new(api_key, deepseek::DeepSeekModel::Chat).await
}

/// Create a new `deepseek-reasoner` LLM backend, validating the API key.
pub async fn deepseek_reasoner(api_key: &str) -> exn::Result<deepseek::DeepSeek, deepseek::Error> {
    deepseek::DeepSeek::new(api_key, deepseek::DeepSeekModel::Reasoner).await
}
