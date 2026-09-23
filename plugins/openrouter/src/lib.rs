#![allow(unsafe_op_in_unsafe_fn)]

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::BTreeMap;

use pluribus_plugin_sdk::export;
pub use pluribus_plugin_sdk::{exports, http, pluribus, wasi};

use crate::http::Reader;
use crate::http::{Header, Request};
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::blobs;
use pluribus::plugin::credentials;
use pluribus::plugin::events;
use pluribus::plugin::types::{self, Error, ErrorCode, Event, Payload, Proposal};

use pluribus_model::{
    BlobRef, COMPLETION_SCHEMA, Completion, ContentPart, Delta, Feature, MediaPart, Message,
    MessageRole, ModelEntry, Request as ModelRequest, STREAM_SCHEMA, StopReason, Stream, ToolCall,
    ToolCallDelta, ToolDefinition, Usage,
};

// Configuration arrives in `init`; no import returns it later.
thread_local! {
    static CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
}

const URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEFAULT_TIMEOUT_MS: u32 = 300_000;
const CHUNK_BYTES: u32 = 1024 * 1024;
const MAX_PROVIDER_TOOL_NAME_BYTES: usize = 64;

/// Request knobs copied verbatim from `provider_options`. Anything else a
/// caller sets is ignored rather than forwarded blind.
const PASSTHROUGH: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "frequency_penalty",
    "presence_penalty",
    "repetition_penalty",
    "seed",
    "stop",
    "reasoning",
    "provider",
    "transforms",
    "models",
    "route",
];

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(rename = "api-key")]
    api_key: String,
}

/// The enrolled record behind the `api-key` handle.
#[derive(Deserialize)]
struct ApiKey {
    api_key: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    credentials: Credentials,
    models: Vec<ModelEntry>,
    #[serde(default = "default_timeout")]
    timeout_ms: u32,
}

struct OpenRouter;

use pluribus_plugin_sdk::serve;

fn setup(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
    let parsed: Config = serde_json::from_slice(&config)
        .map_err(|error| invalid(format!("invalid configuration: {error}")))?;
    if parsed.credentials.api_key.is_empty() {
        return Err(invalid("credential handle is empty"));
    }
    if parsed.models.is_empty() {
        return Err(invalid("at least one model is required"));
    }
    CONFIG.with_borrow_mut(|slot| *slot = Some(parsed));
    Ok(empty_outcome())
}

impl Guest for OpenRouter {
    async fn run(context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        serve::<Self>(context).await
    }

    async fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
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
            if !config
                .models
                .iter()
                .any(|model| model.id() == request.model)
            {
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
                Err(error) => proposals.push(failure_event(event, Some(&request.call_id), &error)?),
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

/// Runs one completion against the OpenRouter chat-completions endpoint.
fn complete(request: &ModelRequest, config: &Config) -> Result<Completion, Error> {
    reject_unsupported_features(&request.required_features)?;
    if request.continuation.is_some() {
        return Err(unsupported(
            "chat completions carry no provider continuation",
        ));
    }
    let (body, tool_names) = build_request(request)?;
    let body = put_blob("application/json", &body)?;
    let mut read = http::sse(&Request {
        method: "POST".to_owned(),
        url: URL.to_owned(),
        headers: vec![
            header("accept", "text/event-stream"),
            header("content-type", "application/json"),
            header("authorization", &format!("Bearer {}", api_key(config)?)),
        ],
        body: Some(body),
        timeout_ms: config.timeout_ms,
    })?;
    // The reader closes when it drops, ending the transfer.
    parse_stream(&request.call_id, &mut read, tool_names)
}

fn decode_request(event: &Event) -> Result<ModelRequest, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| invalid(format!("invalid model request: {error}"))),
        Payload::Blob(_) => Err(invalid("model requests must be inline JSON")),
    }
}

fn build_request(request: &ModelRequest) -> Result<(Vec<u8>, ToolNames), Error> {
    let tool_names = ToolNames::new(&request.tools);
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool_names.provider(&tool.name),
                    "description": tool.description,
                    "parameters": tool.input_schema,
                },
            })
        })
        .collect::<Vec<_>>();
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("stream".into(), Value::Bool(true)),
        // OpenRouter reports token accounting only when asked.
        ("usage".into(), json!({"include": true})),
        (
            "messages".into(),
            Value::Array(convert_messages(&request.messages, &tool_names)?),
        ),
    ]);
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), Value::String("auto".into()));
        body.insert("parallel_tool_calls".into(), Value::Bool(true));
    }
    if let Some(limit) = request.max_output_tokens {
        body.insert("max_tokens".into(), json!(limit));
    }
    if let Some(schema) = &request.output_schema {
        body.insert(
            "response_format".into(),
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "output",
                    "strict": true,
                    "schema": schema,
                },
            }),
        );
    }
    if let Some(options) = request.provider_options.as_ref().and_then(Value::as_object) {
        for key in PASSTHROUGH {
            if let Some(value) = options.get(*key) {
                body.insert((*key).to_owned(), value.clone());
            }
        }
    }
    let body = serde_json::to_vec(&body)
        .map_err(|error| internal(format!("cannot encode request: {error}")))?;
    Ok((body, tool_names))
}

/// Maps the canonical message list onto chat-completions messages. Tool
/// results become their own `tool` messages, which the wire format requires.
fn convert_messages(messages: &[Message], tool_names: &ToolNames) -> Result<Vec<Value>, Error> {
    let mut result = Vec::new();
    for message in messages {
        let mut content = Vec::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text { text } => content.push(json!({"type": "text", "text": text})),
                ContentPart::Image(media) => content.push(image_part(media)?),
                ContentPart::ToolCall(call) => calls.push(json!({
                    "id": call.call_id,
                    "type": "function",
                    "function": {
                        "name": tool_names.provider(&call.name),
                        "arguments": serde_json::to_string(&call.arguments).map_err(internal)?,
                    },
                })),
                ContentPart::ToolResult(tool_result) => results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_result.call_id,
                    "content": serde_json::to_string(&tool_result.output).map_err(internal)?,
                })),
                ContentPart::ProviderData { data } => result.push(data.clone()),
                ContentPart::Audio(_) => {
                    return Err(unsupported("audio input is not implemented"));
                }
            }
        }
        if !content.is_empty() || !calls.is_empty() {
            let mut converted = Map::from_iter([("role".into(), json!(role_name(message.role)))]);
            if !content.is_empty() {
                // A system prompt fans out to every upstream provider, so it
                // travels as a plain string rather than as content parts.
                converted.insert(
                    "content".into(),
                    if matches!(message.role, MessageRole::System) {
                        Value::String(joined_text(&content))
                    } else {
                        Value::Array(content)
                    },
                );
            }
            if !calls.is_empty() {
                converted.insert("tool_calls".into(), Value::Array(calls));
            }
            if let Some(name) = &message.name {
                converted.insert("name".into(), Value::String(name.clone()));
            }
            result.push(Value::Object(converted));
        }
        result.extend(results);
    }
    Ok(result)
}

/// Concatenates the text parts, dropping anything a string cannot carry.
fn joined_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n")
}

const fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn image_part(media: &MediaPart) -> Result<Value, Error> {
    let encoded = STANDARD.encode(read_blob(&media.blob)?);
    Ok(json!({
        "type": "image_url",
        "image_url": {
            "url": format!("data:{};base64,{encoded}", media.blob.media_type),
            "detail": media.detail.as_deref().unwrap_or("auto"),
        },
    }))
}

/// One tool call assembled from its fragments. The provider sends the name
/// once and the arguments across many chunks.
#[derive(Default)]
struct PartialCall {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamState {
    text: String,
    reasoning: String,
    calls: BTreeMap<u32, PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    metadata: Map<String, Value>,
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
fn parse_stream(
    call_id: &str,
    read: &mut Reader,
    tool_names: ToolNames,
) -> Result<Completion, Error> {
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
        if apply(&records, &mut state)? {
            flush(call_id, &mut state)?;
            return finish(call_id, state);
        }
        // One batch per read, so progress is observable without a durable
        // event per token.
        flush(call_id, &mut state)?;
        if chunk.closed {
            // A final record may arrive without its terminating blank line.
            apply(&sse_data(&buffered)?, &mut state)?;
            flush(call_id, &mut state)?;
            if state.finish_reason.is_none() {
                return Err(unavailable("stream ended without a finish reason"));
            }
            return finish(call_id, state);
        }
    }
}

/// Applies complete records, reporting whether the stream terminated.
fn apply(records: &[Vec<u8>], state: &mut StreamState) -> Result<bool, Error> {
    for record in records {
        if record == b"[DONE]" {
            return Ok(true);
        }
        process_chunk(&parse_json(record)?, state)?;
    }
    Ok(false)
}

fn process_chunk(chunk: &Value, state: &mut StreamState) -> Result<(), Error> {
    if let Some(error) = chunk.get("error").filter(|value| !value.is_null()) {
        return Err(chunk_error(error));
    }
    for key in ["id", "model", "provider"] {
        if let Some(value) = chunk.get(key).filter(|value| !value.is_null())
            && !state.metadata.contains_key(key)
        {
            state.metadata.insert(key.to_owned(), value.clone());
        }
    }
    for choice in chunk
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                state.text.push_str(text);
                state.pending.push(Delta::text(text));
            }
            if let Some(text) = delta.get("reasoning").and_then(Value::as_str)
                && !text.is_empty()
            {
                state.reasoning.push_str(text);
                state.pending.push(Delta::reasoning_summary(text));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                accumulate_call(call, state);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            state.finish_reason = Some(reason.to_owned());
        }
    }
    if let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) {
        let usage = parse_usage(usage);
        state.pending.push(Delta::Usage(usage.clone()));
        state.usage = Some(usage);
    }
    Ok(())
}

fn accumulate_call(call: &Value, state: &mut StreamState) {
    let index = call
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default();
    let fragment = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .map(|name| state.tool_names.original(name).to_owned());
    let call_id = call.get("id").and_then(Value::as_str);
    let partial = state.calls.entry(index).or_default();
    if let Some(call_id) = call_id
        && !call_id.is_empty()
    {
        partial.call_id = call_id.to_owned();
    }
    if let Some(name) = name.clone() {
        partial.name = name;
    }
    partial.arguments.push_str(fragment);
    state.pending.push(Delta::ToolCall(ToolCallDelta {
        index,
        call_id: call_id.map(str::to_owned),
        name,
        arguments_fragment: fragment.to_owned(),
    }));
}

fn finish(call_id: &str, mut state: StreamState) -> Result<Completion, Error> {
    let mut content = Vec::new();
    if !state.text.is_empty() {
        content.push(ContentPart::text(std::mem::take(&mut state.text)));
    }
    let calls = std::mem::take(&mut state.calls);
    for (_, partial) in calls {
        content.push(ContentPart::ToolCall(ToolCall {
            call_id: partial.call_id,
            name: partial.name,
            arguments: serde_json::from_str(&partial.arguments).unwrap_or_else(|_| json!({})),
        }));
    }
    let has_calls = content
        .iter()
        .any(|part| matches!(part, ContentPart::ToolCall(_)));
    let stop_reason = match state.finish_reason.as_deref() {
        Some("tool_calls") => StopReason::ToolCall,
        Some("length") => StopReason::MaxOutput,
        Some("content_filter") => StopReason::ContentFilter,
        Some("stop") | None if has_calls => StopReason::ToolCall,
        Some("stop") | None => StopReason::EndTurn,
        Some(other) => StopReason::Other {
            reason: other.to_owned(),
        },
    };
    if !state.reasoning.is_empty() {
        state
            .metadata
            .insert("reasoning".into(), Value::String(state.reasoning.clone()));
    }
    Ok(Completion {
        call_id: call_id.to_owned(),
        message: Message {
            role: MessageRole::Assistant,
            name: None,
            content,
        },
        stop_reason,
        usage: state.usage.clone(),
        continuation: None,
        provider_metadata: Some(Value::Object(state.metadata)),
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

/// Appends one batch of deltas so a completion is visible while it streams.
///
/// The key is the call and the batch ordinal, so a redelivered request
/// deduplicates instead of doubling the stream.
fn emit_stream(call_id: &str, ordinal: u64, deltas: Vec<Delta>) -> Result<(), Error> {
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

/// Extracts the `data:` payloads from complete SSE records. Comment lines,
/// which OpenRouter sends as keep-alives, carry no data and drop out.
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

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64),
        output_tokens: usage.get("completion_tokens").and_then(Value::as_u64),
        reasoning_tokens: usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        cached_input_tokens: usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        provider_metadata: Some(usage.clone()),
    }
}

/// Provider tool names are restricted to a short identifier alphabet, so the
/// canonical name is mapped both ways rather than passed through.
#[derive(Default)]
struct ToolNames {
    provider_by_original: BTreeMap<String, String>,
    original_by_provider: BTreeMap<String, String>,
}

impl ToolNames {
    fn new(tools: &[ToolDefinition]) -> Self {
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

fn reject_unsupported_features(features: &[Feature]) -> Result<(), Error> {
    for feature in features {
        if matches!(
            feature,
            Feature::AudioInput | Feature::AudioOutput | Feature::Continuation
        ) {
            return Err(unsupported("requested model feature is unavailable"));
        }
    }
    Ok(())
}

/// The transport and the model payload use different `blob-ref` types, so the
/// boundary converts rather than aliasing them.
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
    let blob = to_wit_blob(blob);
    let capacity =
        usize::try_from(blob.size).map_err(|_| resource_exhausted("blob is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    loop {
        let chunk = blobs::read(&blob, offset, CHUNK_BYTES)?;
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

fn config() -> Result<Config, Error> {
    CONFIG.with_borrow(|slot| {
        slot.clone()
            .ok_or_else(|| unavailable("plugin is not initialized"))
    })
}

fn parse_json(bytes: &[u8]) -> Result<Value, Error> {
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("invalid JSON: {error}")))
}

fn chunk_error(error: &Value) -> Error {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenRouter request failed");
    match error.get("code").and_then(Value::as_u64) {
        Some(402) => resource_exhausted(message),
        Some(429) => unavailable(message),
        Some(400 | 404 | 422) => invalid(message),
        _ => unavailable(message),
    }
}

/// Reads the enrolled key. The host attaches nothing, so the request carries
/// whatever this returns.
fn api_key(config: &Config) -> Result<String, Error> {
    let bytes = credentials::get(&config.credentials.api_key)?
        .ok_or_else(|| invalid("OpenRouter key is not enrolled"))?;
    let record: ApiKey =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid credential record"))?;
    Ok(record.api_key)
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.to_owned(),
        value: value.as_bytes().to_vec(),
    }
}

const fn default_timeout() -> u32 {
    DEFAULT_TIMEOUT_MS
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

export!(OpenRouter);

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> ModelRequest {
        serde_json::from_value(value).unwrap()
    }

    fn state() -> StreamState {
        StreamState::default()
    }

    #[test]
    fn an_output_token_limit_becomes_max_tokens() {
        let (bytes, _) = build_request(&request(json!({
            "call_id": "call-1",
            "model": "anthropic/claude-sonnet-4.5",
            "messages": [],
            "max_output_tokens": 4096,
        })))
        .unwrap();

        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["max_tokens"], json!(4096));
        assert_eq!(body["usage"], json!({"include": true}));
    }

    #[test]
    fn only_allowlisted_provider_options_reach_the_wire() {
        let (bytes, _) = build_request(&request(json!({
            "call_id": "call-1",
            "model": "openai/gpt-5.1",
            "messages": [],
            "provider_options": {"temperature": 0.2, "api_key": "leak"},
        })))
        .unwrap();

        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(body["temperature"], json!(0.2));
        assert!(body.get("api_key").is_none());
    }

    #[test]
    fn a_system_prompt_travels_as_a_plain_string() {
        let messages = convert_messages(
            &[Message {
                role: MessageRole::System,
                name: None,
                content: vec![ContentPart::text("be terse"), ContentPart::text("be exact")],
            }],
            &ToolNames::default(),
        )
        .unwrap();

        assert_eq!(messages[0]["content"], "be terse\n\nbe exact");
    }

    #[test]
    fn a_tool_result_becomes_its_own_message() {
        let messages = convert_messages(
            &[
                Message {
                    role: MessageRole::Assistant,
                    name: None,
                    content: vec![ContentPart::ToolCall(ToolCall {
                        call_id: "call-a".into(),
                        name: "read file".into(),
                        arguments: json!({"path": "/tmp/x"}),
                    })],
                },
                Message {
                    role: MessageRole::Tool,
                    name: None,
                    content: vec![ContentPart::ToolResult(pluribus_model::ToolResult {
                        call_id: "call-a".into(),
                        output_schema: "x/1".into(),
                        output: json!({"ok": true}),
                        error: None,
                    })],
                },
            ],
            &ToolNames::new(&[ToolDefinition {
                name: "read file".into(),
                description: String::new(),
                input_schema: json!({}),
            }]),
        )
        .unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["name"],
            "read_file_t0"
        );
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call-a");
    }

    #[test]
    fn tool_call_fragments_concatenate_into_one_call() {
        let mut state = state();
        state.tool_names = ToolNames::new(&[ToolDefinition {
            name: "read file".into(),
            description: String::new(),
            input_schema: json!({}),
        }]);
        for fragment in [
            json!({"id": "call-a", "index": 0, "function": {"name": "read_file_t0", "arguments": "{\"pa"}}),
            json!({"index": 0, "function": {"arguments": "th\":\"/tmp/x\"}"}}),
        ] {
            accumulate_call(&fragment, &mut state);
        }
        state.finish_reason = Some("tool_calls".into());

        let completion = finish("call-1", state).unwrap();

        assert_eq!(completion.stop_reason, StopReason::ToolCall);
        assert_eq!(
            completion.message.content,
            vec![ContentPart::ToolCall(ToolCall {
                call_id: "call-a".into(),
                name: "read file".into(),
                arguments: json!({"path": "/tmp/x"}),
            })]
        );
    }

    #[test]
    fn a_keep_alive_comment_carries_no_record() {
        let (records, rest) = split_sse(
            b": OPENROUTER PROCESSING\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        )
        .unwrap();

        assert_eq!(records.len(), 1);
        assert!(rest.is_empty());
    }

    #[test]
    fn a_record_split_across_reads_is_not_truncated() {
        let partial = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}";

        let (records, rest) = split_sse(partial).unwrap();

        assert!(records.is_empty());
        assert_eq!(rest, partial);
    }

    #[test]
    fn usage_maps_provider_token_names() {
        let mut state = state();
        process_chunk(
            &json!({
                "id": "gen-1",
                "model": "openai/gpt-5.1",
                "usage": {"prompt_tokens": 10, "completion_tokens": 4},
            }),
            &mut state,
        )
        .unwrap();

        assert_eq!(state.usage.as_ref().unwrap().input_tokens, Some(10));
        assert_eq!(state.usage.as_ref().unwrap().output_tokens, Some(4));
        assert_eq!(state.metadata["id"], "gen-1");
    }

    #[test]
    fn a_payment_error_is_not_retryable() {
        let error = process_chunk(
            &json!({"error": {"code": 402, "message": "insufficient credits"}}),
            &mut state(),
        )
        .expect_err("an error chunk fails the call");

        assert_eq!(error.code, ErrorCode::ResourceExhausted);
        assert!(!error.retryable);
    }
}
