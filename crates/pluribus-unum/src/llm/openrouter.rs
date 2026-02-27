use exn::ResultExt;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use futures_lite::stream;
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use pluribus_frequency::protocol::{Message, ToolDef};
use pluribus_frequency::state::Configuration;
use serde::{Deserialize, Serialize};

use crate::llm::{GenOptions, LlmError, LlmEvent, LlmEventStream, Provider, ReasoningEffort};

const BASE_URL: &str = "https://openrouter.ai/api/v1";
const KEY_API_KEY: &str = "openrouter.api_key";
const MODEL: &str = "openrouter/auto";

/// `OpenRouter` LLM backend.
pub struct OpenRouter {
    client: HttpClient,
    api_key: String,
}

#[derive(Debug)]
pub enum Error {
    Network(String),
    InvalidAPIKey,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(msg) => write!(f, "Network error: {msg}"),
            Self::InvalidAPIKey => write!(f, "Invalid API key"),
        }
    }
}

impl std::error::Error for Error {}

/// Store an `OpenRouter` API key in configuration.
pub fn set_api_key(
    config: &Configuration,
    key: &str,
) -> exn::Result<(), pluribus_frequency::state::Error> {
    config.set(KEY_API_KEY, &key)
}

/// Build an `OpenRouter` backend from stored configuration.
///
/// Returns `None` if no API key is stored or the stored key is invalid.
pub async fn from_config(config: &Configuration) -> Option<OpenRouter> {
    let key: String = config.get(KEY_API_KEY)?;
    OpenRouter::new(&key).await.ok()
}

impl OpenRouter {
    /// Create a new `OpenRouter` LLM backend, validating the API key.
    pub async fn new(api_key: &str) -> exn::Result<Self, Error> {
        let client = HttpClient::new().or_raise(|| Error::Network("create HTTP client".into()))?;
        let request = Request::get(format!("{BASE_URL}/auth/key"))
            .header("Authorization", format!("Bearer {api_key}"))
            .body(())
            .or_raise(|| Error::Network("build HTTP request".into()))?;
        let response = client
            .send_async(request)
            .await
            .or_raise(|| Error::Network("send HTTP request".into()))?;
        if response.status().is_success() {
            Ok(Self {
                client,
                api_key: api_key.to_owned(),
            })
        } else {
            Err(Error::InvalidAPIKey.into())
        }
    }
}

/// Internal state for the streaming unfold.
enum State {
    New,
    Streaming(BufReader<isahc::AsyncBody>),
    Finished,
}

impl Provider for OpenRouter {
    fn complete_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        options: &'a GenOptions,
    ) -> LlmEventStream<'a> {
        let url = format!("{BASE_URL}/chat/completions");
        Box::pin(stream::unfold(State::New, move |state| {
            let url = url.clone();
            async move {
                match state {
                    State::New => {
                        tracing::debug!(model = MODEL, "starting OpenRouter SSE stream");
                        match start_sse_stream(
                            &self.client,
                            &url,
                            &self.api_key,
                            MODEL,
                            messages,
                            tools,
                            options,
                        )
                        .await
                        {
                            Ok(mut reader) => {
                                tracing::debug!("OpenRouter SSE stream connected");
                                match read_next_sse_event(&mut reader).await {
                                    Ok(Some(event)) => {
                                        Some((exn::Ok(event), State::Streaming(reader)))
                                    }
                                    Ok(None) => None,
                                    Err(e) => {
                                        tracing::warn!(%e, "OpenRouter SSE stream error on first event");
                                        Some((Err(e), State::Finished))
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%e, "failed to start OpenRouter SSE stream");
                                Some((Err(e), State::Finished))
                            }
                        }
                    }
                    State::Streaming(mut reader) => {
                        match read_next_sse_event(&mut reader).await {
                            Ok(Some(event)) => Some((exn::Ok(event), State::Streaming(reader))),
                            Ok(None) => {
                                tracing::debug!("OpenRouter SSE stream finished");
                                None
                            }
                            Err(e) => {
                                tracing::warn!(%e, "OpenRouter SSE stream error");
                                Some((Err(e), State::Finished))
                            }
                        }
                    }
                    State::Finished => None,
                }
            }
        }))
    }
}

// -- Frequency → OpenRouter wire conversion -----------------------------------

#[must_use]
fn to_wire_message(msg: &Message) -> WireMessage<'_> {
    match msg {
        Message::System { content } => {
            let text: String = content
                .iter()
                .filter_map(|p| p.as_text())
                .collect::<Vec<_>>()
                .join("");
            WireMessage::System {
                content: WireContent::Owned(text),
            }
        }
        Message::Scheduled { content } | Message::User { content } => {
            let parts = content
                .iter()
                .map(|p| match p {
                    pluribus_frequency::protocol::ContentPart::Text { text } => {
                        WireContentPart::Text { text: text.clone() }
                    }
                    pluribus_frequency::protocol::ContentPart::Image { data, .. } => {
                        WireContentPart::ImageUrl {
                            image_url: WireImageUrl { url: data.clone() },
                        }
                    }
                })
                .collect();
            WireMessage::User { content: parts }
        }
        Message::Assistant {
            content,
            tool_calls,
            reasoning_content,
        } => {
            let text = if content.is_empty() {
                None
            } else {
                Some(
                    content
                        .iter()
                        .filter_map(|p| p.as_text())
                        .collect::<Vec<_>>()
                        .join(""),
                )
            };
            let wire_tool_calls: Vec<WireToolCall<'_>> = tool_calls
                .iter()
                .map(|tc| WireToolCall {
                    id: &tc.id,
                    call_type: "function",
                    function: WireFunctionCall {
                        name: &tc.name,
                        arguments: &tc.arguments,
                    },
                })
                .collect();
            WireMessage::Assistant {
                content: text,
                tool_calls: if wire_tool_calls.is_empty() {
                    None
                } else {
                    Some(wire_tool_calls)
                },
                reasoning_content: reasoning_content.as_deref(),
            }
        }
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => {
            let text: String = content
                .iter()
                .filter_map(|p| p.as_text())
                .collect::<Vec<_>>()
                .join("");
            WireMessage::Tool {
                tool_call_id,
                content: WireContent::Owned(text),
            }
        }
    }
}

#[must_use]
fn to_wire_tools(tools: &[ToolDef]) -> Vec<WireTool<'_>> {
    tools
        .iter()
        .map(|t| WireTool {
            tool_type: "function",
            function: WireFunction {
                name: t.name().as_str(),
                description: t.description(),
                parameters: t.input_schema(),
            },
        })
        .collect()
}

// -- SSE streaming ------------------------------------------------------------

/// Read the next SSE event from the buffered reader.
///
/// Returns `None` when the stream is finished (`[DONE]` sentinel or EOF).
async fn read_next_sse_event(
    reader: &mut BufReader<isahc::AsyncBody>,
) -> exn::Result<Option<LlmEvent>, LlmError> {
    loop {
        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .await
            .or_raise(|| LlmError::Network("read SSE line".into()))?;

        if bytes_read == 0 {
            return Ok(None);
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let data = if let Some(d) = line.strip_prefix("data: ") {
            d
        } else if let Some(d) = line.strip_prefix("data:") {
            d
        } else {
            continue;
        };

        if data == "[DONE]" {
            return Ok(None);
        }

        // Check for inline error responses (e.g. OpenRouter sends these for
        // upstream provider failures like expired image URLs).
        if let Ok(err) = serde_json::from_str::<StreamError>(data) {
            exn::bail!(LlmError::Api {
                status: err.error.code,
                body: err.error.message,
            });
        }

        let chunk: StreamChunk = serde_json::from_str(data).or_raise(|| {
            LlmError::StreamParse(format!("parse SSE chunk JSON: {data}"))
        })?;
        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };

        let finished = choice.finish_reason.as_deref();

        // Mid-stream error: OpenRouter signals with finish_reason "error" and
        // a top-level error object.
        if matches!(finished, Some("error")) {
            if let Some(err) = chunk.error {
                exn::bail!(LlmError::Api {
                    status: err.code,
                    body: err.message,
                });
            }
            exn::bail!(LlmError::StreamParse(
                "finish_reason=error with no error body".into()
            ));
        }

        if matches!(finished, Some("length")) {
            tracing::warn!("response truncated (finish_reason=length)");
        }

        // Extract reasoning from the delta, preferring structured
        // reasoning_details over the plaintext fields.
        let reasoning_text = extract_reasoning(&choice.delta);
        if let Some(reasoning) = reasoning_text {
            if !reasoning.is_empty() {
                return Ok(Some(LlmEvent::Reasoning(reasoning)));
            }
        }

        if let Some(content) = choice.delta.content {
            if !content.is_empty() {
                return Ok(Some(LlmEvent::Text(content)));
            }
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            if let Some(tc) = tool_calls.into_iter().next() {
                let func_name = tc.function.as_ref().and_then(|f| f.name.clone());
                let args = tc
                    .function
                    .as_ref()
                    .and_then(|f| f.arguments.clone())
                    .unwrap_or_default();
                return Ok(Some(LlmEvent::ToolCall {
                    index: tc.index,
                    id: tc.id,
                    function_name: func_name,
                    arguments_delta: args,
                }));
            }
        }

        // No content in this chunk — if the stream is finished, signal end.
        if finished.is_some() {
            return Ok(None);
        }
    }
}

/// Extract reasoning text from a stream delta.
///
/// Prefers structured `reasoning_details` (extracting `.text` from
/// `reasoning.text` entries), falling back to `reasoning` /
/// `reasoning_content`.
fn extract_reasoning(delta: &StreamDelta) -> Option<String> {
    // Prefer structured reasoning_details.
    if let Some(details) = &delta.reasoning_details {
        let text: String = details
            .iter()
            .filter_map(|d| {
                if d.detail_type.as_deref() == Some("reasoning.text") {
                    d.text.as_deref()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return Some(text);
        }
    }

    // Fall back to plaintext reasoning fields.
    if let Some(r) = &delta.reasoning {
        return Some(r.clone());
    }
    if let Some(r) = &delta.reasoning_content {
        return Some(r.clone());
    }

    None
}

/// Build a chat request body and send it as an SSE streaming HTTP request.
///
/// Returns a buffered reader over the SSE response body.
async fn start_sse_stream(
    client: &HttpClient,
    url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    options: &GenOptions,
) -> exn::Result<BufReader<isahc::AsyncBody>, LlmError> {
    let wire_messages: Vec<WireMessage<'_>> = messages.iter().map(to_wire_message).collect();
    let wire_tools = to_wire_tools(tools);

    let reasoning = options.reasoning.as_ref().map(|effort| Reasoning {
        effort: match effort {
            ReasoningEffort::XHigh => "xhigh",
            ReasoningEffort::High => "high",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Minimal => "minimal",
        },
    });

    let body = ChatRequest {
        model,
        messages: &wire_messages,
        tools: wire_tools,
        stream: true,
        temperature: options.temperature,
        max_tokens: options.max_tokens,
        top_p: options.top_p,
        stop: options.stop.as_deref(),
        seed: options.seed,
        reasoning,
    };

    let json_body = serde_json::to_vec(&body)
        .or_raise(|| LlmError::Network("serialize chat request".into()))?;

    tracing::debug!(
        model,
        messages = messages.len(),
        tools = tools.len(),
        "sending chat completion request"
    );

    let request = Request::post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("X-OpenRouter-Title", "pluribus")
        .body(json_body)
        .or_raise(|| LlmError::Network("build HTTP request".into()))?;
    let response = client
        .send_async(request)
        .await
        .or_raise(|| LlmError::Network("send HTTP request".into()))?;

    let status = response.status();
    if !status.is_success() {
        let mut response = response;
        let body = response.text().await.unwrap_or_default();
        tracing::warn!(status = status.as_u16(), "chat completion API error");
        exn::bail!(LlmError::Api {
            status: status.as_u16(),
            body,
        });
    }

    tracing::debug!("chat completion SSE stream started");
    Ok(BufReader::new(response.into_body()))
}

// -- Wire types (OpenRouter chat completions format) --------------------------

/// Helper to serialize a string that may be borrowed or owned.
#[derive(Debug)]
enum WireContent<'a> {
    Owned(String),
    #[allow(dead_code)]
    Borrowed(&'a str),
}

impl Serialize for WireContent<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Owned(s) => serializer.serialize_str(s),
            Self::Borrowed(s) => serializer.serialize_str(s),
        }
    }
}

/// A single content part in a user message.
#[derive(Serialize)]
#[serde(tag = "type")]
enum WireContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireImageUrl },
}

/// Image URL payload for the vision API.
#[derive(Serialize)]
struct WireImageUrl {
    url: String,
}

/// Reasoning configuration for `OpenRouter`.
#[derive(Serialize)]
struct Reasoning {
    effort: &'static str,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [WireMessage<'a>],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
}

#[derive(Serialize)]
#[serde(tag = "role")]
enum WireMessage<'a> {
    #[serde(rename = "system")]
    System { content: WireContent<'a> },

    #[serde(rename = "user")]
    User { content: Vec<WireContentPart> },

    #[serde(rename = "assistant")]
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<WireToolCall<'a>>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<&'a str>,
    },

    #[serde(rename = "tool")]
    Tool {
        tool_call_id: &'a str,
        content: WireContent<'a>,
    },
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: WireFunctionCall<'a>,
}

#[derive(Serialize)]
struct WireFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: WireFunction<'a>,
}

#[derive(Serialize)]
struct WireFunction<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<&'a schemars::Schema>,
}

// -- SSE streaming wire types -------------------------------------------------

/// An inline error returned inside an SSE stream.
#[derive(Deserialize)]
struct StreamError {
    error: StreamErrorBody,
}

/// Body of an inline SSE stream error.
#[derive(Deserialize)]
struct StreamErrorBody {
    message: String,
    #[serde(default)]
    code: u16,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
    /// Top-level error object sent alongside `finish_reason: "error"`.
    error: Option<StreamErrorBody>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    /// Plaintext reasoning (some providers).
    reasoning: Option<String>,
    /// Alias for `reasoning` (some providers).
    reasoning_content: Option<String>,
    /// Structured reasoning details array.
    reasoning_details: Option<Vec<ReasoningDetail>>,
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

/// A single entry in the `reasoning_details` array.
#[derive(Deserialize)]
struct ReasoningDetail {
    #[serde(rename = "type")]
    detail_type: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct StreamToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize)]
struct StreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}
