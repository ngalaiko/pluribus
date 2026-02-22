use exn::ResultExt;
use futures_lite::io::{AsyncBufReadExt, BufReader};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use pluribus_frequency::protocol::{Message, ToolDef};
use serde::{Deserialize, Serialize};

use crate::llm::{GenOptions, LlmError, LlmEvent};

// -- Frequency → OpenAI wire conversion -------------------------------------

#[must_use]
pub fn to_wire_message(msg: &Message) -> WireMessage<'_> {
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
pub fn to_wire_tools(tools: &[ToolDef]) -> Vec<WireTool<'_>> {
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

/// Read the next SSE event from the buffered reader.
///
/// Returns `None` when the stream is finished (`[DONE]` sentinel or EOF).
pub async fn read_next_sse_event(
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

        if let Some(reason) = choice.finish_reason.as_deref() {
            if reason == "length" {
                tracing::warn!("response truncated (finish_reason=length)");
                exn::bail!(LlmError::Truncated);
            }
            tracing::debug!(reason, "SSE stream finish_reason received");
            return Ok(None);
        }

        if let Some(reasoning) = choice.delta.reasoning_content {
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
    }
}

/// Build a chat request body and send it as an SSE streaming HTTP request.
///
/// Returns a buffered reader over the SSE response body.
pub async fn start_sse_stream(
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

    let thinking = if options.thinking {
        Some(Thinking {
            thinking_type: "enabled",
        })
    } else {
        None
    };

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
        thinking,
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

// -- Wire types (OpenAI chat completions format) ----------------------------

/// Helper to serialize a string that may be borrowed or owned.
#[derive(Debug)]
pub enum WireContent<'a> {
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
pub enum WireContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireImageUrl },
}

/// Image URL payload for the `OpenAI` vision API.
#[derive(Serialize)]
pub struct WireImageUrl {
    pub url: String,
}

/// Thinking mode configuration for models that support chain-of-thought.
#[derive(Serialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub thinking_type: &'static str,
}

#[derive(Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [WireMessage<'a>],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool<'a>>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
}

#[derive(Serialize)]
#[serde(tag = "role")]
pub enum WireMessage<'a> {
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
pub struct WireToolCall<'a> {
    pub id: &'a str,
    #[serde(rename = "type")]
    pub call_type: &'static str,
    pub function: WireFunctionCall<'a>,
}

#[derive(Serialize)]
pub struct WireFunctionCall<'a> {
    pub name: &'a str,
    pub arguments: &'a str,
}

#[derive(Serialize)]
pub struct WireTool<'a> {
    #[serde(rename = "type")]
    pub tool_type: &'static str,
    pub function: WireFunction<'a>,
}

#[derive(Serialize)]
pub struct WireFunction<'a> {
    pub name: &'a str,
    pub description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<&'a schemars::Schema>,
}

// -- SSE streaming wire types -----------------------------------------------

/// An inline error returned inside an SSE stream (e.g. from `OpenRouter`).
#[derive(Deserialize)]
pub struct StreamError {
    pub error: StreamErrorBody,
}

/// Body of an inline SSE stream error.
#[derive(Deserialize)]
pub struct StreamErrorBody {
    pub message: String,
    #[serde(default)]
    pub code: u16,
}

#[derive(Deserialize)]
pub struct StreamChunk {
    pub choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
pub struct StreamChoice {
    pub delta: StreamDelta,
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub struct StreamDelta {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Deserialize)]
pub struct StreamToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize)]
pub struct StreamFunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}
