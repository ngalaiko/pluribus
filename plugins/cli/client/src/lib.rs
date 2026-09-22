#![allow(unsafe_op_in_unsafe_fn)]

//! A terminal connector. Subscribed bridge input produces observations.

use pluribus_plugin_sdk::export;
pub use pluribus_plugin_sdk::{exports, pluribus, wasi};

use channel::Socket;
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::state;
use pluribus::plugin::types::{Error, ErrorCode, Event, Mutation, Payload, Proposal, StateEntry};
use protocol::{MAX_RESPONSE, Message, Request, Response, VERSION};
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;

/// Bytes requested per read. The bridge caps its own response.
const READ_CHUNK: u32 = 64 * 1024;
/// Bytes requested per read on the input subscription.
const INPUT_CHUNK: u32 = 32 * 1024;
/// Slack over the bridge's own timeout before the client gives up.
const RESPONSE_GRACE_MS: u32 = 5_000;
const CAPABILITY: &str = "cli.reply";
/// Key holding the last delivered input. State is a rebuildable projection: an
/// empty namespace replays whatever the bridge still holds.
const CURSOR_KEY: &str = "input/cursor";

use pluribus_plugin_sdk::socket as channel;

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

fn setup(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
    let parsed = parse_config(&config)?;
    CONFIG.with_borrow_mut(|slot| *slot = Some(parsed));
    Ok(Outcome {
        events: Vec::new(),
        mutations: Vec::new(),
        checkpoint: None,
    })
}

impl Guest for Cli {
    async fn run(mut context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        use pluribus::plugin::runtime;
        let mut delay: u32 = 0;
        loop {
            if Self::waiting(&mut context, async {
                crate::wasi::clocks::monotonic_clock::wait_for(u64::from(delay) * 1_000_000).await;
                Ok(())
            })
            .await?
            .is_none()
            {
                return Ok(());
            }
            INPUT.with_borrow_mut(Vec::clear);
            let result: Result<bool, Error> = async {
                let mut input = Socket::connect("default").await?;
                let current_cursor = cursor()?;
                input
                    .send(&poll_request(&configuration()?, &current_cursor)?)
                    .await?;
                loop {
                    let Some(chunk) =
                        Self::waiting(&mut context, input.read(INPUT_CHUNK, None)).await?
                    else {
                        return Ok(true);
                    };
                    if !chunk.bytes.is_empty() {
                        let out = Self::process_bytes(chunk.bytes)?;
                        runtime::commit(&out.events, &out.mutations, None)?;
                        if INPUT.with_borrow(|b| b.is_empty()) {
                            break;
                        }
                    }
                    if chunk.closed {
                        return Err(unavailable("bridge disconnected"));
                    }
                }
                #[allow(unreachable_code)]
                Ok(false)
            }
            .await;
            match result {
                Ok(true) => return Ok(()),
                Ok(false) => delay = 100,
                Err(error)
                    if !error.retryable
                        && !matches!(
                            error.code,
                            pluribus::plugin::types::ErrorCode::DeadlineExceeded
                        ) =>
                {
                    return Err(error);
                }
                Err(_) => delay = delay.saturating_mul(2).clamp(100, 30_000),
            }
        }
    }

    async fn handle(context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let mut proposals = Vec::new();
        let mutations = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            checkpoint = Some(event.sequence);
            if event.event_type == "capability.requested" {
                let request = json_payload(event)?;
                if request.get("capability").and_then(Value::as_str) != Some(CAPABILITY) {
                    continue;
                }
                proposals.push(dispatch(event, &request).await);
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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, serde::Serialize)]
struct Cursor {
    session_id: Option<String>,
    sequence: u64,
}

fn poll_request(config: &Config, cursor: &Cursor) -> Result<Vec<u8>, Error> {
    let mut request = serde_json::to_vec(&Request::Poll {
        version: VERSION,
        after: cursor.sequence,
        session_id: cursor.session_id.clone(),
        timeout_ms: config.poll_timeout_seconds.saturating_mul(1000),
    })
    .map_err(|e| internal(e.to_string()))?;
    request.push(b'\n');
    Ok(request)
}

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

async fn dispatch(event: &Event, request: &Value) -> Proposal {
    let outcome = match reply(request).await {
        Ok(()) => proposal(
            "capability.completed",
            "dev.pluribus.cli.result.v1",
            &json!({"requestEventId": event.event_id, "output": true}),
            Some(event.event_id.clone()),
        ),
        Err(error) => Err(error),
    };
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

async fn reply(request: &Value) -> Result<(), Error> {
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
    match exchange(&request, 30_000).await? {
        Response::Delivered => Ok(()),
        Response::Messages { .. } => Err(invalid("bridge answered a reply with input")),
        Response::Unavailable { message } => Err(unavailable(message)),
    }
}

/// One newline-terminated request and one response per connection.
async fn exchange(request: &Request, timeout_ms: u32) -> Result<Response, Error> {
    request.validate().map_err(invalid)?;
    let mut bytes = serde_json::to_vec(request).map_err(|_| internal("cannot encode request"))?;
    bytes.push(b'\n');
    let mut channel = Socket::connect("default").await?;
    send(&mut channel, &bytes, timeout_ms).await
}

async fn send(channel: &mut Socket, request: &[u8], timeout_ms: u32) -> Result<Response, Error> {
    channel.send(request).await?;
    let mut bytes = Vec::new();
    loop {
        let chunk = channel
            .read(
                READ_CHUNK,
                Some(timeout_ms.saturating_add(RESPONSE_GRACE_MS)),
            )
            .await?;
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
        if chunk.bytes.is_empty() {
            return Err(failure(
                ErrorCode::DeadlineExceeded,
                "bridge did not respond",
            ));
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| unavailable(format!("invalid response: {error}")))
}

fn observation(config: &Config, session_id: &str, message: &Message) -> Result<Proposal, Error> {
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
    proposal.idempotency_key = Some(format!("cli:input:{session_id}:{}", message.sequence));
    Ok(proposal)
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

fn cursor() -> Result<Cursor, Error> {
    let Some(bytes) = state::get(CURSOR_KEY)? else {
        return Ok(Cursor::default());
    };
    decode_cursor(&bytes)
}

fn decode_cursor(bytes: &[u8]) -> Result<Cursor, Error> {
    if let Ok(cursor) = serde_json::from_slice(&bytes) {
        return Ok(cursor);
    }
    // Older releases persisted only the sequence. It cannot identify a
    // restarted bridge, so the next response establishes a fresh session.
    std::str::from_utf8(&bytes)
        .map_err(|_| invalid("stored cursor is not UTF-8"))?
        .parse::<u64>()
        .map(|sequence| Cursor {
            session_id: None,
            sequence,
        })
        .map_err(|_| invalid("stored cursor is invalid"))
}

fn cursor_after(current: &Cursor, session_id: &str, messages: &[Message]) -> Cursor {
    let same_session = current.session_id.as_deref() == Some(session_id);
    Cursor {
        session_id: Some(session_id.to_owned()),
        sequence: messages.last().map_or_else(
            || if same_session { current.sequence } else { 0 },
            |m| m.sequence,
        ),
    }
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

impl Cli {
    fn process_bytes(bytes: Vec<u8>) -> Result<SourceOutput, Error> {
        let mut out = SourceOutput {
            events: vec![],
            mutations: vec![],
        };
        let frame = INPUT.with_borrow_mut(|buffer| {
            if buffer.len().saturating_add(bytes.len()) > MAX_RESPONSE {
                buffer.clear();
                return Err(invalid("bridge response exceeds limit"));
            }
            buffer.extend(bytes);
            Ok(if buffer.last() == Some(&b'\n') {
                Some(std::mem::take(buffer))
            } else {
                None
            })
        })?;
        let Some(frame) = frame else {
            return Ok(out);
        };
        let config = configuration()?;
        let current_cursor = cursor()?;
        let response: Response =
            serde_json::from_slice(&frame).map_err(|e| invalid(e.to_string()))?;
        match response {
            Response::Messages {
                session_id,
                messages,
            } => {
                let next_cursor = cursor_after(&current_cursor, &session_id, &messages);
                for message in &messages {
                    out.events.push(observation(&config, &session_id, message)?);
                }
                if next_cursor.session_id != current_cursor.session_id
                    || next_cursor.sequence != current_cursor.sequence
                {
                    out.mutations.push(Mutation::Set(StateEntry {
                        key: CURSOR_KEY.into(),
                        value: serde_json::to_vec(&next_cursor)
                            .map_err(|_| internal("cannot encode input cursor"))?,
                    }));
                }
            }
            Response::Unavailable { message } => return Err(unavailable(message)),
            Response::Delivered => {
                return Err(invalid("unexpected reply receipt on input subscription"));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{Cursor, cursor_after, decode_cursor, observation};
    use protocol::Message;
    use serde_json::Value;

    #[test]
    fn old_sequence_cursor_migrates_without_assuming_a_bridge_session() {
        assert_eq!(
            decode_cursor(b"12").unwrap(),
            Cursor {
                session_id: None,
                sequence: 12,
            }
        );
    }

    #[test]
    fn empty_response_from_new_session_resets_old_cursor() {
        let next = cursor_after(
            &Cursor {
                session_id: Some("old".into()),
                sequence: 42,
            },
            "new",
            &[],
        );
        assert_eq!(next.session_id.as_deref(), Some("new"));
        assert_eq!(next.sequence, 0);
    }

    #[test]
    fn input_idempotency_keys_are_scoped_to_bridge_session() {
        let config = super::Config {
            conversation_id: "local".into(),
            sender: "operator".into(),
            poll_timeout_seconds: 20,
        };
        let message = Message {
            sequence: 1,
            at_ms: 0,
            text: "hello".into(),
        };
        let old = observation(&config, "old", &message).unwrap();
        let new = observation(&config, "new", &message).unwrap();
        assert_ne!(old.idempotency_key, new.idempotency_key);
    }
}

struct SourceOutput {
    events: Vec<pluribus::plugin::types::Proposal>,
    mutations: Vec<pluribus::plugin::types::Mutation>,
}

impl Cli {
    /// Services internal deliveries while external work is suspended.
    async fn waiting<T>(
        context: &mut Context,
        work: impl std::future::Future<Output = Result<T, Error>>,
    ) -> Result<Option<T>, Error> {
        use futures_util::future::{Either, select};
        use pluribus::plugin::runtime::{self, Wake};
        futures_util::pin_mut!(work);
        loop {
            match select(Box::pin(runtime::next()), work.as_mut()).await {
                Either::Left((wake, _)) => match wake? {
                    Wake::Stop(_) => {
                        // Finish cancelled imports before dropping their borrowed resources.
                        let _ = work.await;
                        return Ok(None);
                    }
                    Wake::Events(events) => match Self::handle(context.clone(), events).await {
                        Ok(out) => {
                            runtime::commit(&out.events, &out.mutations, out.checkpoint)?;
                            context.state_checkpoint =
                                out.checkpoint.unwrap_or(context.state_checkpoint);
                        }
                        Err(error) => runtime::reject(&error)?,
                    },
                },
                Either::Right((result, _)) => return result.map(Some),
            }
        }
    }
}
