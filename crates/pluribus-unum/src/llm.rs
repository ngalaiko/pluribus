pub mod openrouter;

use futures_lite::StreamExt;
use pluribus_frequency::state::State;

use std::{fmt, pin::Pin};

use futures_lite::Stream;
use pluribus_frequency::protocol::{ContentPart, Message, ToolCall, ToolDef};

/// An incremental event from an LLM streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    /// A chunk of text content.
    Text(String),
    /// A chunk of reasoning / chain-of-thought content.
    Reasoning(String),
    /// A chunk of a tool call being built.
    ToolCall {
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
    /// Enable chain-of-thought reasoning with a given effort level.
    pub reasoning: Option<ReasoningEffort>,
}

/// Reasoning effort level for chain-of-thought.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ReasoningEffort {
    XHigh,
    High,
    Medium,
    Low,
    Minimal,
}

/// Errors from LLM operations.
#[derive(Debug)]
pub enum LlmError {
    Api { status: u16, body: String },
    Network(String),
    StreamParse(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api { status, body } => write!(f, "API error ({status}): {body}"),
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::StreamParse(msg) => write!(f, "stream parse error: {msg}"),
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
            LlmEvent::Text(delta) => content.push_str(&delta),
            LlmEvent::Reasoning(delta) => reasoning.push_str(&delta),
            LlmEvent::ToolCall {
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

/// Resolve the `OpenRouter` LLM: try stored key, wait for peer sync, or
/// prompt the user.
pub async fn resolve(state: &State) -> openrouter::OpenRouter {
    let config = state.configuration();

    // Try stored key first.
    if let Some(llm) = openrouter::from_config(&config).await {
        return llm;
    }

    // Wait briefly for a peer to sync the key.
    {
        tracing::debug!("waiting for peers to sync OpenRouter API key");
        let mut receiver = state.new_broadcast_receiver();
        let timeout = async_io::Timer::after(std::time::Duration::from_secs(5));
        let got_key = futures_lite::future::or(
            async {
                futures_lite::pin!(timeout);
                (&mut timeout).await;
                false
            },
            async {
                loop {
                    if receiver.next().await.is_none() {
                        return false;
                    }
                    if openrouter::from_config(&config).await.is_some() {
                        return true;
                    }
                }
            },
        )
        .await;

        if got_key {
            if let Some(llm) = openrouter::from_config(&config).await {
                return llm;
            }
            tracing::warn!("OpenRouter API key from peer is invalid");
        } else {
            tracing::debug!("no peer sync received, will prompt for key");
        }
    }

    // Prompt the user.
    loop {
        eprint!("Enter OpenRouter API key: ");
        let key = read_line_trimmed().await;
        if key.is_empty() {
            continue;
        }
        tracing::debug!("validating OpenRouter API key");
        match openrouter::OpenRouter::new(&key).await {
            Ok(llm) => {
                tracing::info!("OpenRouter API key valid");
                let _ = openrouter::set_api_key(&config, &key);
                return llm;
            }
            Err(e) => tracing::warn!(%e, "OpenRouter API key invalid"),
        }
    }
}

async fn read_line_trimmed() -> String {
    blocking::unblock(|| {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).ok();
        buf.trim().to_owned()
    })
    .await
}
