#![allow(unsafe_op_in_unsafe_fn)]

//! A person at a terminal, as a connector. Input arrives by polling the bridge
//! over the granted byte stream; replies go back the same way.

wit_bindgen::generate!({ path: "../../wit", world: "plugin" });

use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::reader::Reader;
use pluribus::plugin::socket;
use pluribus::plugin::state;
use pluribus::plugin::types::{Error, ErrorCode, Event, Mutation, Payload, Proposal, StateEntry};
use pluribus::plugin::writer::Writer;
use protocol::{MAX_RESPONSE, Message, Request, Response, VERSION};
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;

/// Bytes requested per read. The bridge caps its own response.
const READ_CHUNK: u32 = 64 * 1024;
const CAPABILITY: &str = "cli.reply";
/// Key holding the last delivered input. State is a rebuildable projection: an
/// empty namespace replays whatever the bridge still holds.
const CURSOR_KEY: &str = "input/cursor";
/// How long to wait before polling again after the bridge refuses a connection.
const RETRY_SECONDS: u32 = 5;

struct Cli;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_conversation")]
    conversation_id: String,
    #[serde(default = "default_sender")]
    sender: String,
    #[serde(default = "default_poll_timeout")]
    poll_timeout_seconds: u32,
}

fn default_conversation() -> String {
    "local".into()
}

fn default_sender() -> String {
    "operator".into()
}

fn default_poll_timeout() -> u32 {
    20
}

impl Guest for Cli {
    fn init(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
        let parsed = parse_config(&config)?;
        CONFIG.with_borrow_mut(|slot| *slot = Some(parsed));
        // Polling is a timer, so the host decides when this instance runs.
        Ok(Outcome {
            events: vec![timer(0, 0)?],
            mutations: Vec::new(),
            checkpoint: None,
        })
    }

    fn handle(context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let config = configuration()?;
        let mut proposals = Vec::new();
        let mut mutations = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            checkpoint = Some(event.sequence);
            match event.event_type.as_str() {
                "timer.fired" => match poll(&config) {
                    Ok(messages) => {
                        if let Some(last) = messages.last() {
                            mutations.push(Mutation::Set(StateEntry {
                                key: CURSOR_KEY.to_owned(),
                                value: last.sequence.to_string().into_bytes(),
                            }));
                        }
                        for message in &messages {
                            proposals.push(observation(&config, message)?);
                        }
                        // The poll itself waited, so the next one starts now.
                        proposals.push(timer(event.recorded_at_ms, 0)?);
                    }
                    // A bridge nobody is running is a state to wait out, not a
                    // failure that quarantines the component.
                    Err(_) => proposals.push(timer(event.recorded_at_ms, RETRY_SECONDS)?),
                },
                "capability.requested" => {
                    let request = json_payload(event)?;
                    if request.get("capability").and_then(Value::as_str) != Some(CAPABILITY) {
                        continue;
                    }
                    proposals.push(dispatch(event, &request));
                }
                _ => {}
            }
        }

        let _ = context;
        Ok(Outcome {
            events: proposals,
            mutations,
            checkpoint,
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: Vec::new(),
            mutations: Vec::new(),
            checkpoint: None,
        })
    }
}

/// Waits for input newer than the stored cursor.
fn poll(config: &Config) -> Result<Vec<Message>, Error> {
    let timeout_ms = config.poll_timeout_seconds.saturating_mul(1_000);
    let request = Request::Poll {
        version: VERSION,
        after: cursor()?,
        timeout_ms,
    };
    match exchange(&request, timeout_ms)? {
        Response::Messages { messages } => Ok(messages),
        Response::Delivered => Err(invalid("bridge answered a poll with a delivery")),
        Response::Unavailable { message } => Err(unavailable(message)),
    }
}

fn dispatch(event: &Event, request: &Value) -> Proposal {
    let outcome = reply(request).and_then(|()| {
        proposal(
            "capability.completed",
            "dev.pluribus.cli.result.v1",
            &json!({"requestEventId": event.event_id, "output": true}),
            Some(event.event_id.clone()),
        )
    });
    outcome.unwrap_or_else(|error| {
        proposal(
            "capability.failed",
            "dev.pluribus.cli.result.v1",
            &json!({
                "requestEventId": event.event_id,
                "code": code_name(error.code),
                "reason": error.message,
            }),
            Some(event.event_id.clone()),
        )
        .expect("failure proposal")
    })
}

fn reply(request: &Value) -> Result<(), Error> {
    let arguments = request
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let conversation_id = arguments["conversationId"]
        .as_str()
        .ok_or_else(|| invalid("conversationId is required"))?
        .to_owned();
    let text = arguments["text"]
        .as_str()
        .ok_or_else(|| invalid("text is required"))?
        .to_owned();
    let request = Request::Reply {
        version: VERSION,
        conversation_id,
        text,
    };
    match exchange(&request, 30_000)? {
        Response::Delivered => Ok(()),
        Response::Messages { .. } => Err(invalid("bridge answered a reply with input")),
        Response::Unavailable { message } => Err(unavailable(message)),
    }
}

/// One newline-terminated request and one response per connection.
fn exchange(request: &Request, timeout_ms: u32) -> Result<Response, Error> {
    request.validate().map_err(invalid)?;
    let mut bytes = serde_json::to_vec(request).map_err(|_| internal("cannot encode request"))?;
    bytes.push(b'\n');
    let (read, write) = socket::connect()?;
    send(&read, &write, &bytes, timeout_ms)
}

fn send(read: &Reader, write: &Writer, request: &[u8], timeout_ms: u32) -> Result<Response, Error> {
    write.send(request)?;
    let mut bytes = Vec::new();
    loop {
        let chunk = read.receive(READ_CHUNK, timeout_ms.saturating_add(5_000))?;
        bytes.extend_from_slice(&chunk.bytes);
        if bytes.len() > MAX_RESPONSE {
            return Err(failure(
                ErrorCode::ResourceExhausted,
                "bridge response exceeds limit",
            ));
        }
        if bytes.last() == Some(&b'\n') {
            break;
        }
        if chunk.closed {
            return Err(unavailable("bridge disconnected"));
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| unavailable(format!("invalid response: {error}")))
}

fn observation(config: &Config, message: &Message) -> Result<Proposal, Error> {
    let mut proposal = proposal(
        "observation.received",
        "dev.pluribus.cli.observation.v1",
        &json!({
            "provider": "cli",
            "conversationId": config.conversation_id,
            "externalSenderId": config.sender,
            "observedAtMs": message.at_ms,
            "message": {"text": message.text},
        }),
        None,
    )?;
    proposal.idempotency_key = Some(format!("cli:input:{}", message.sequence));
    Ok(proposal)
}

/// Asks the core to wake this instance after `seconds`.
fn timer(recorded_at_ms: i64, seconds: u32) -> Result<Proposal, Error> {
    proposal(
        "timer.set",
        "pluribus.timer-set/1",
        &json!({"dueAtMs": recorded_at_ms.saturating_add(i64::from(seconds) * 1_000)}),
        None,
    )
}

fn proposal(
    event_type: &str,
    payload_schema: &str,
    value: &Value,
    causation_id: Option<String>,
) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: event_type.to_owned(),
        payload_schema: payload_schema.to_owned(),
        payload: Payload::Json(serde_json::to_vec(value).map_err(|_| internal("cannot encode"))?),
        idempotency_key: None,
        causation_id,
    })
}

fn cursor() -> Result<u64, Error> {
    let Some(bytes) = state::get(CURSOR_KEY)? else {
        return Ok(0);
    };
    std::str::from_utf8(&bytes)
        .map_err(|_| invalid("stored cursor is not UTF-8"))?
        .parse()
        .map_err(|_| invalid("stored cursor is not an integer"))
}

// Configuration arrives in `init` and no import returns it later, so the
// instance holds it. A restart runs `init` again.
thread_local! {
    static CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
}

fn configuration() -> Result<Config, Error> {
    CONFIG.with_borrow(|slot| {
        slot.clone()
            .ok_or_else(|| unavailable("plugin is not initialized"))
    })
}

fn parse_config(bytes: &[u8]) -> Result<Config, Error> {
    if bytes.is_empty() {
        return Ok(Config {
            conversation_id: default_conversation(),
            sender: default_sender(),
            poll_timeout_seconds: default_poll_timeout(),
        });
    }
    serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("invalid configuration: {error}")))
}

fn json_payload(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| invalid(format!("invalid payload: {error}"))),
        Payload::Blob(_) => Err(invalid("cli does not read blob payloads")),
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

fn failure(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
        retryable: matches!(code, ErrorCode::Unavailable),
        details: None,
    }
}

fn invalid(message: impl Into<String>) -> Error {
    failure(ErrorCode::InvalidArgument, message)
}

fn unavailable(message: impl Into<String>) -> Error {
    failure(ErrorCode::Unavailable, message)
}

fn internal(message: impl Into<String>) -> Error {
    failure(ErrorCode::Internal, message)
}

export!(Cli);
