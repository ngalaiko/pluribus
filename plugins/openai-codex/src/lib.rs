#![allow(unsafe_op_in_unsafe_fn)]

use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;

wit_bindgen::generate!({
    path: "../../wit",
    world: "plugin",
});

use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::blobs;
use pluribus::plugin::events;
use pluribus::plugin::http::{self, Header, Request};
use pluribus::plugin::reader::Reader;
use pluribus::plugin::types::{self, Error, ErrorCode, Event, Payload, Proposal};

use pluribus_model::{
    BlobRef, COMPLETION_SCHEMA, Completion, ContentPart, Delta, Feature, MediaPart, Message,
    MessageRole, Request as ModelRequest, STREAM_SCHEMA, StopReason, Stream, ToolCall,
    ToolCallDelta, Usage,
};

// Configuration arrives in `init`; no import returns it later.
thread_local! {
    static CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
}

const URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_MODEL: &str = "gpt-5.6-luna";
const DEFAULT_TIMEOUT_MS: u32 = 300_000;
const CHUNK_BYTES: u32 = 1024 * 1024;
const MAX_PROVIDER_TOOL_NAME_BYTES: usize = 64;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(rename = "subscription")]
    subscription: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    credentials: Credentials,
    #[serde(default = "default_models")]
    models: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u32,
}

struct Codex;

impl Guest for Codex {
    fn init(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
        let parsed: Config = serde_json::from_slice(&config)
            .map_err(|error| invalid_argument(format!("invalid configuration: {error}")))?;
        if parsed.credentials.subscription.is_empty() {
            return Err(invalid_argument("credential handle is empty"));
        }
        if parsed.models.is_empty() {
            return Err(invalid_argument("at least one model is required"));
        }
        CONFIG.with_borrow_mut(|slot| *slot = Some(parsed));
        Ok(empty_outcome())
    }

    fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let config = config()?;
        let mut proposals = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            checkpoint = Some(event.sequence);
            if event.event_type != "model.requested" {
                continue;
            }
            let request: ModelRequest = match decode_request(event) {
                Ok(request) => request,
                Err(error) => {
                    proposals.push(failure_event(event, None, &error)?);
                    continue;
                }
            };
            if !config.models.iter().any(|model| model == &request.model) {
                continue;
            }
            match complete(&request, &config) {
                Ok(completion) => proposals.push(proposal(
                    "model.completed",
                    COMPLETION_SCHEMA,
                    &serde_json::to_value(&completion).map_err(internal)?,
                    None,
                    Some(event.event_id.clone()),
                )?),
                Err(error) => {
                    proposals.push(failure_event(event, Some(&request.call_id), &error)?);
                }
            }
        }

        Ok(Outcome {
            events: proposals,
            mutations: Vec::new(),
            checkpoint,
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(empty_outcome())
    }
}

/// Runs one completion against the Codex Responses endpoint.
fn complete(request: &ModelRequest, config: &Config) -> Result<Completion, Error> {
    reject_unsupported_features(&request.required_features)?;
    let (body, tool_names) = build_request(request)?;
    let body = put_blob("application/json", &body)?;
    let read = http::sse(&Request {
        method: "POST".to_owned(),
        url: URL.to_owned(),
        headers: vec![
            header("accept", "text/event-stream"),
            header("content-type", "application/json"),
            header("OpenAI-Beta", "responses=experimental"),
            header("originator", "pluribus"),
        ],
        body: Some(body),
        credential: Some(config.credentials.subscription.clone()),
        timeout_ms: config.timeout_ms,
    })?;
    // The reader closes when it drops, ending the transfer.
    parse_stream(&request.call_id, &read, tool_names)
}

fn decode_request(event: &Event) -> Result<ModelRequest, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| invalid_argument(format!("invalid model request: {error}"))),
        Payload::Blob(_) => Err(invalid_argument("model requests must be inline JSON")),
    }
}

fn failure_event(event: &Event, call_id: Option<&str>, error: &Error) -> Result<Proposal, Error> {
    proposal(
        "model.failed",
        "pluribus.model-failure/1",
        &json!({
            "requestEventId": event.event_id,
            "callId": call_id,
            "code": code_name(error.code),
            "reason": error.message,
        }),
        None,
        Some(event.event_id.clone()),
    )
}

/// Appends one batch of deltas so a completion is visible while it streams.
///
/// The key is the call and the batch ordinal, so a redelivered request
/// deduplicates instead of doubling the stream.
fn emit_stream(call_id: &str, ordinal: u64, deltas: Vec<Delta>) -> Result<(), Error> {
    if deltas.is_empty() {
        return Ok(());
    }
    let stream = Stream {
        call_id: call_id.to_owned(),
        deltas,
    };
    let proposal = proposal(
        "model.stream",
        STREAM_SCHEMA,
        &serde_json::to_value(&stream).map_err(internal)?,
        Some(format!("model:{call_id}:stream:{ordinal}")),
        None,
    )?;
    events::append(&proposal).map(|_| ())
}

fn proposal(
    event_type: &str,
    payload_schema: &str,
    value: &Value,
    idempotency_key: Option<String>,
    causation_id: Option<String>,
) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: event_type.to_owned(),
        payload_schema: payload_schema.to_owned(),
        payload: Payload::Json(serde_json::to_vec(value).map_err(internal)?),
        idempotency_key,
        causation_id,
    })
}

const fn empty_outcome() -> Outcome {
    Outcome {
        events: Vec::new(),
        mutations: Vec::new(),
        checkpoint: None,
    }
}

const fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidArgument => "invalid-argument",
        ErrorCode::NotFound => "not-found",
        ErrorCode::PermissionDenied => "permission-denied",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::ResourceExhausted => "resource-exhausted",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::DeadlineExceeded => "deadline-exceeded",
        ErrorCode::Internal => "internal",
    }
}

fn config() -> Result<Config, Error> {
    CONFIG.with_borrow(|slot| {
        slot.clone()
            .ok_or_else(|| unavailable("plugin is not initialized"))
    })
}

fn build_request(request: &ModelRequest) -> Result<(Vec<u8>, ToolNames), Error> {
    let options = request
        .provider_options
        .clone()
        .unwrap_or_else(|| json!({}));
    let instructions = system_instructions(&request.messages);
    let tool_names = ToolNames::new(&request.tools);
    let mut input = continuation_items(request.continuation.as_ref())?;
    input.extend(convert_messages(&request.messages, &tool_names)?);
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            Ok(json!({
                "type": "function",
                "name": tool_names.provider(&tool.name),
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": false,
            }))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("store".into(), Value::Bool(false)),
        ("stream".into(), Value::Bool(true)),
        (
            "instructions".into(),
            Value::String(if instructions.is_empty() {
                "You are a helpful assistant.".into()
            } else {
                instructions
            }),
        ),
        ("input".into(), Value::Array(input)),
        ("parallel_tool_calls".into(), Value::Bool(true)),
        ("tool_choice".into(), Value::String("auto".into())),
        ("include".into(), json!(["reasoning.encrypted_content"])),
        (
            "text".into(),
            json!({"verbosity": option_string(&options, "text_verbosity").unwrap_or("low")}),
        ),
    ]);
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    // Codex rejects per-request output-token limits.
    if let Some(key) = option_string(&options, "prompt_cache_key") {
        body.insert("prompt_cache_key".into(), Value::String(key.to_owned()));
    }
    if let Some(effort) = option_string(&options, "reasoning_effort") {
        body.insert(
            "reasoning".into(),
            json!({
                "effort": effort,
                "summary": option_string(&options, "reasoning_summary").unwrap_or("auto")
            }),
        );
    }
    if let Some(schema) = &request.output_schema {
        body.insert(
            "text".into(),
            json!({
                "verbosity": option_string(&options, "text_verbosity").unwrap_or("low"),
                "format": {
                    "type": "json_schema",
                    "name": "output",
                    "strict": true,
                    "schema": schema,
                }
            }),
        );
    }
    let body = serde_json::to_vec(&body)
        .map_err(|error| internal(format!("cannot encode request: {error}")))?;
    Ok((body, tool_names))
}

fn system_instructions(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|message| matches!(message.role, MessageRole::System))
        .flat_map(|message| message.content.iter())
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn convert_messages(messages: &[Message], tool_names: &ToolNames) -> Result<Vec<Value>, Error> {
    let mut result = Vec::new();
    for message in messages {
        if matches!(message.role, MessageRole::System) {
            continue;
        }
        let role = match message.role {
            MessageRole::System => unreachable!(),
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let mut content = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text { text } => content.push(json!({
                    "type": if role == "assistant" { "output_text" } else { "input_text" },
                    "text": text,
                })),
                ContentPart::ToolCall(call) => result.push(json!({
                    "type": "function_call",
                    "call_id": call.call_id,
                    "name": tool_names.provider(&call.name),
                    "arguments": serde_json::to_string(&call.arguments)
                        .map_err(internal)?,
                })),
                ContentPart::ToolResult(tool_result) => {
                    result.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_result.call_id,
                        "output": serde_json::to_string(&tool_result.output).map_err(internal)?,
                    }));
                }
                ContentPart::ProviderData { data } => result.push(data.clone()),
                ContentPart::Image(media) => content.push(image_part(media)?),
                ContentPart::Audio(_) => {
                    return Err(unsupported("audio input is not implemented"));
                }
            }
        }
        if !content.is_empty() {
            result.push(json!({"role": role, "content": content}));
        }
    }
    Ok(result)
}

fn image_part(media: &MediaPart) -> Result<Value, Error> {
    let bytes = read_blob(&media.blob)?;
    let encoded = base64(&bytes);
    Ok(json!({
        "type": "input_image",
        "image_url": format!("data:{};base64,{encoded}", media.blob.media_type),
        "detail": media.detail.as_deref().unwrap_or("auto"),
    }))
}

fn continuation_items(continuation: Option<&BlobRef>) -> Result<Vec<Value>, Error> {
    let Some(blob) = continuation else {
        return Ok(Vec::new());
    };
    let value = parse_json(&read_blob(blob)?)?;
    value
        .get("reasoning")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| invalid("continuation has no reasoning array"))
}

#[derive(Default)]
struct StreamState {
    text: String,
    reasoning_summary: String,
    tools: Vec<ToolCall>,
    terminal: Option<Value>,
    tool_names: ToolNames,
    /// Deltas awaiting their next `model.stream` batch. Batching keeps a long
    /// completion from writing one durable event per token.
    pending: Vec<Delta>,
    batch: u64,
}

/// Reads the response stream and accumulates one completion.
///
/// The host hands over raw bytes rather than parsed frames, so SSE record
/// framing lives here: the transport moves bytes, the plugin owns the
/// protocol. The core enforces cancellation through epoch interruption, so
/// there is no cancellation flag to poll.
fn parse_stream(call_id: &str, read: &Reader, tool_names: ToolNames) -> Result<Completion, Error> {
    let mut state = StreamState {
        tool_names,
        ..StreamState::default()
    };
    let mut buffered = Vec::new();
    loop {
        let chunk = read.receive(CHUNK_BYTES, 1_000)?;
        buffered.extend_from_slice(&chunk.bytes);
        let (records, rest) = split_sse(&buffered)?;
        buffered = rest;
        for record in records {
            if record == b"[DONE]" {
                continue;
            }
            let event = parse_json(&record)?;
            if process_event(&event, &mut state)? {
                return finish_response(call_id, state);
            }
        }
        // One batch per read, so progress is observable without a durable
        // event per token.
        flush(call_id, &mut state)?;
        if chunk.closed {
            return Err(unavailable("stream ended without a terminal event"));
        }
    }
}

/// Splits complete SSE records out of the buffer, returning the unconsumed
/// tail so a record spanning two reads is not truncated.
fn split_sse(buffered: &[u8]) -> Result<(Vec<Vec<u8>>, Vec<u8>), Error> {
    let text = std::str::from_utf8(buffered).map_err(|_| invalid("SSE response is not UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    let Some(boundary) = normalized.rfind("\n\n") else {
        return Ok((Vec::new(), buffered.to_vec()));
    };
    let (complete, tail) = normalized.split_at(boundary + 2);
    Ok((sse_data(complete.as_bytes())?, tail.as_bytes().to_vec()))
}

fn process_event(event: &Value, state: &mut StreamState) -> Result<bool, Error> {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "response.output_text.delta" => {
            let delta = required_string(event, "delta")?;
            state.text.push_str(delta);
            state.pending.push(Delta::text(delta));
        }
        "response.reasoning_summary_text.delta" => {
            let delta = required_string(event, "delta")?;
            state.reasoning_summary.push_str(delta);
            state.pending.push(Delta::reasoning_summary(delta));
        }
        "response.function_call_arguments.delta" => {
            let index = event
                .get("output_index")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default();
            let delta = required_string(event, "delta")?;
            state.pending.push(Delta::ToolCall(ToolCallDelta {
                index,
                call_id: event
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                name: event
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| state.tool_names.original(name).to_owned()),
                arguments_fragment: delta.to_owned(),
            }));
        }
        "response.output_item.done" => {
            if let Some(call) = event
                .get("item")
                .and_then(|item| parse_tool_call(item, &state.tool_names))
            {
                state.tools.push(call);
            }
        }
        "response.completed" | "response.incomplete" | "response.done" => {
            state.terminal = event.get("response").cloned();
            return Ok(true);
        }
        "response.failed" | "error" => return Err(event_error(event)),
        _ => {}
    }
    Ok(false)
}

fn finish_response(call_id: &str, mut state: StreamState) -> Result<Completion, Error> {
    let response = state
        .terminal
        .take()
        .ok_or_else(|| unavailable("stream ended without a terminal event"))?;
    if state.text.is_empty() {
        state.text = response_text(&response);
    }
    if state.tools.is_empty() {
        state.tools = response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| parse_tool_call(item, &state.tool_names))
            .collect();
    }
    let usage = response.get("usage").map(parse_usage);
    if let Some(usage) = &usage {
        state.pending.push(Delta::Usage(usage.clone()));
    }
    flush(call_id, &mut state)?;
    let mut content = Vec::new();
    if !state.text.is_empty() {
        content.push(ContentPart::text(std::mem::take(&mut state.text)));
    }
    content.extend(state.tools.iter().cloned().map(ContentPart::ToolCall));
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let stop_reason = if !state.tools.is_empty() {
        StopReason::ToolCall
    } else if status == "incomplete" {
        StopReason::MaxOutput
    } else {
        StopReason::EndTurn
    };
    let continuation = reasoning_continuation(&response)?;
    let metadata = json!({
        "id": response.get("id"),
        "status": status,
        "reasoningSummary": state.reasoning_summary,
    });
    Ok(Completion {
        call_id: call_id.to_owned(),
        message: Message {
            role: MessageRole::Assistant,
            name: None,
            content,
        },
        stop_reason,
        usage,
        continuation,
        provider_metadata: Some(metadata),
    })
}

/// Appends whatever deltas have accumulated as one `model.stream` event.
fn flush(call_id: &str, state: &mut StreamState) -> Result<(), Error> {
    if state.pending.is_empty() {
        return Ok(());
    }
    let deltas = std::mem::take(&mut state.pending);
    state.batch += 1;
    emit_stream(call_id, state.batch, deltas)
}

/// Extracts the `data:` payloads from complete SSE records.
fn sse_data(bytes: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("SSE response is not UTF-8"))?;
    let normalized = text.replace("\r\n", "\n");
    let mut records = normalized.split("\n\n").collect::<Vec<_>>();
    if normalized.ends_with("\n\n") {
        records.pop();
    }
    Ok(records
        .into_iter()
        .filter_map(|record| {
            let lines = record
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>();
            (!lines.is_empty()).then(|| lines.join("\n").into_bytes())
        })
        .collect())
}

fn response_text(response: &Value) -> String {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn parse_tool_call(item: &Value, tool_names: &ToolNames) -> Option<ToolCall> {
    if item.get("type")?.as_str()? != "function_call" {
        return None;
    }
    Some(ToolCall {
        call_id: item.get("call_id")?.as_str()?.to_owned(),
        name: tool_names.original(item.get("name")?.as_str()?).to_owned(),
        arguments: serde_json::from_str(
            item.get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}"),
        )
        .unwrap_or_else(|_| json!({})),
    })
}

#[derive(Default)]
struct ToolNames {
    provider_by_original: BTreeMap<String, String>,
    original_by_provider: BTreeMap<String, String>,
}

impl ToolNames {
    fn new(tools: &[pluribus_model::ToolDefinition]) -> Self {
        let mut names = Self::default();
        for (index, tool) in tools.iter().enumerate() {
            let provider = provider_tool_name(&tool.name, index);
            names
                .provider_by_original
                .insert(tool.name.clone(), provider.clone());
            names
                .original_by_provider
                .insert(provider, tool.name.clone());
        }
        names
    }

    fn provider<'a>(&'a self, original: &'a str) -> &'a str {
        self.provider_by_original
            .get(original)
            .map_or(original, String::as_str)
    }

    fn original<'a>(&'a self, provider: &'a str) -> &'a str {
        self.original_by_provider
            .get(provider)
            .map_or(provider, String::as_str)
    }
}

fn provider_tool_name(name: &str, index: usize) -> String {
    let suffix = format!("_t{index}");
    let maximum = MAX_PROVIDER_TOOL_NAME_BYTES.saturating_sub(suffix.len());
    let mut result = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(maximum)
        .collect::<String>();
    if result.is_empty() {
        result.push_str("tool");
    }
    result.push_str(&suffix);
    result
}

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        cached_input_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        provider_metadata: Some(usage.clone()),
    }
}

fn reasoning_continuation(response: &Value) -> Result<Option<BlobRef>, Error> {
    let reasoning = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        .cloned()
        .collect::<Vec<_>>();
    if reasoning.is_empty() {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(&json!({"reasoning": reasoning}))
        .map_err(|error| internal(format!("cannot encode continuation: {error}")))?;
    put_blob("application/json", &bytes).map(|blob| Some(to_model_blob(&blob)))
}

/// The transport and the model payload use different `blob-ref` types, so the
/// boundary converts rather than aliasing them.
fn to_model_blob(blob: &types::BlobRef) -> BlobRef {
    BlobRef {
        algorithm: blob.algorithm.clone(),
        digest: blob.digest.clone(),
        size: blob.size,
        media_type: blob.media_type.clone(),
    }
}

fn to_wit_blob(blob: &BlobRef) -> types::BlobRef {
    types::BlobRef {
        algorithm: blob.algorithm.clone(),
        digest: blob.digest.clone(),
        size: blob.size,
        media_type: blob.media_type.clone(),
    }
}

fn put_blob(media_type: &str, bytes: &[u8]) -> Result<types::BlobRef, Error> {
    let expected =
        u64::try_from(bytes.len()).map_err(|_| resource_exhausted("blob is too large"))?;
    let upload = blobs::open_write(media_type, Some(expected))?;
    // The host reaps an abandoned upload when the delivery ends, so a failed
    // write needs no explicit abort.
    blobs::write(&upload, 0, bytes)?;
    blobs::finish(&upload)
}

fn read_blob(blob: &BlobRef) -> Result<Vec<u8>, Error> {
    read_wit_blob(&to_wit_blob(blob))
}

fn read_wit_blob(blob: &types::BlobRef) -> Result<Vec<u8>, Error> {
    let capacity =
        usize::try_from(blob.size).map_err(|_| resource_exhausted("blob is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    loop {
        let chunk = blobs::read(blob, offset, CHUNK_BYTES)?;
        offset = offset
            .checked_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| resource_exhausted("blob size overflow"))?;
        bytes.extend_from_slice(&chunk.bytes);
        if chunk.closed {
            break;
        }
        if chunk.bytes.is_empty() {
            return Err(internal("blob read made no progress"));
        }
    }
    Ok(bytes)
}

fn reject_unsupported_features(features: &[Feature]) -> Result<(), Error> {
    for feature in features {
        if matches!(feature, Feature::AudioInput | Feature::AudioOutput) {
            return Err(unsupported("requested model feature is unavailable"));
        }
    }
    Ok(())
}

fn parse_json(bytes: &[u8]) -> Result<Value, Error> {
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("invalid JSON: {error}")))
}

fn option_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("event has no {key}")))
}

fn event_error(event: &Value) -> Error {
    let message = event
        .pointer("/response/error/message")
        .or_else(|| event.pointer("/error/message"))
        .or_else(|| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Codex response failed");
    unavailable(message)
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.to_owned(),
        value: value.as_bytes().to_vec(),
    }
}

fn default_models() -> Vec<String> {
    vec![DEFAULT_MODEL.to_owned()]
}

const fn default_timeout() -> u32 {
    DEFAULT_TIMEOUT_MS
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(char::from(TABLE[usize::from(a >> 2)]));
        output.push(char::from(TABLE[usize::from(((a & 0x03) << 4) | (b >> 4))]));
        output.push(if chunk.len() > 1 {
            char::from(TABLE[usize::from(((b & 0x0f) << 2) | (c >> 6))])
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            char::from(TABLE[usize::from(c & 0x3f)])
        } else {
            '='
        });
    }
    output
}

fn invalid_argument(message: impl Into<String>) -> Error {
    invalid(message)
}

fn invalid(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::InvalidArgument, message, false)
}

fn unsupported(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::Unsupported, message, false)
}

fn unavailable(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::Unavailable, message, true)
}

fn resource_exhausted(message: impl Into<String>) -> Error {
    plugin_error(ErrorCode::ResourceExhausted, message, false)
}

fn internal(message: impl std::fmt::Display) -> Error {
    plugin_error(ErrorCode::Internal, message.to_string(), false)
}

fn plugin_error(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Error {
    Error {
        code,
        message: message.into(),
        retryable,
        details: None,
    }
}

export!(Codex);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_tool_schema_and_name_round_trip() {
        let schema = json!({
            "type":"object",
            "properties":{"action":{"type":"string","enum":["complete","wait"]}},
            "required":["action"],
            "additionalProperties":false
        });
        let request: ModelRequest = serde_json::from_value(json!({
            "call_id":"test", "model":"gpt-5.6-luna", "messages":[],
            "tools":[
                {"name":"js","description":"Compute","input_schema":{"type":"object"}},
                {"name":"yield","description":"Return control","input_schema":schema}
            ]
        }))
        .unwrap();
        let (bytes, names) = build_request(&request).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["tools"][1]["parameters"], schema);
        let arguments = json!({"action":"complete"});
        let call = parse_tool_call(
            &json!({
                "type":"function_call", "call_id":"control",
                "name":body["tools"][1]["name"],
                "arguments":arguments.to_string()
            }),
            &names,
        )
        .unwrap();
        assert_eq!(call.name, "yield");
        assert_eq!(call.arguments, arguments);
    }

    #[test]
    fn codex_omits_unsupported_output_token_parameter() {
        let request: ModelRequest = serde_json::from_value(json!({
            "call_id":"test", "model":"gpt-5.6-luna", "messages":[],
            "max_output_tokens":4096
        }))
        .unwrap();
        let (bytes, _) = build_request(&request).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn parses_unterminated_terminal_sse_record() {
        let records = sse_data(
            br#"data: {"type":"response.output_text.delta","delta":"hello"}

data: {"type":"response.completed","response":{"status":"completed"}}"#,
        )
        .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(
            parse_json(&records[1]).unwrap()["type"],
            "response.completed"
        );
    }

    #[test]
    fn encodes_base64_padding() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
