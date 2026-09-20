#![allow(unsafe_op_in_unsafe_fn)]

// Each side uses a subset of the shared wire constants.
#[path = "../../protocol/src/lib.rs"]
#[allow(dead_code)]
mod protocol;

wit_bindgen::generate!({ generate_all, path: "../../wit", world: "plugin" });

use channel::Socket;
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::credentials;
use pluribus::plugin::types::{Error, ErrorCode, Event, Payload, Proposal};
use protocol::{MAX_RESPONSE, MAX_TIMEOUT_MS, Request, Response, VERSION};
use serde::Deserialize;
use serde_json::json;

/// Bytes requested per read. The executor caps its own output.
const READ_CHUNK: u32 = 64 * 1024;
/// Slack over the command timeout before the client gives up on a response.
const RESPONSE_GRACE_MS: u32 = 5_000;
const CAPABILITY: &str = "shell.execute";

thread_local! {
    static EXPORTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct Shell;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    command: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u32,
}

fn default_timeout() -> u32 {
    30_000
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../shared/run.rs"));

mod channel {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../shared/socket.rs"));
}

fn setup(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
    let config: serde_json::Value = serde_json::from_slice(&config)
        .map_err(|_| failure(ErrorCode::InvalidArgument, "invalid shell config"))?;
    let names: Vec<String> = config
        .get("credential_exports")
        .and_then(serde_json::Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if names.len() > 64 || names.iter().any(|n| !protocol::valid_env_name(n)) {
        return Err(failure(
            ErrorCode::InvalidArgument,
            "invalid environment binding",
        ));
    }
    EXPORTS.with_borrow_mut(|exports| *exports = names);
    Ok(empty_outcome())
}

impl Guest for Shell {
    async fn run(context: Context, config: Vec<u8>) -> Result<(), Error> {
        let outcome = setup(context.clone(), config)?;
        pluribus::plugin::runtime::ready(outcome.events, outcome.mutations).await?;

        serve::<Self>(context).await
    }

    async fn handle(context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let mut proposals = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            checkpoint = Some(event.sequence);
            if event.event_type != "capability.requested" {
                continue;
            }
            let request = json_payload(event)?;
            if request.get("capability").and_then(|value| value.as_str()) != Some(CAPABILITY) {
                continue;
            }
            let terminal = run(&context, event, &request)
                .await
                .and_then(|response| complete(event, response));
            proposals.push(match terminal {
                Ok(completed) => completed,
                Err(error) => proposal(
                    "capability.failed",
                    "dev.pluribus.shell-result/1",
                    &json!({
                        "requestEventId": event.event_id,
                        "code": code_name(error.code),
                        "reason": error.message,
                    }),
                    Some(event.event_id.clone()),
                )?,
            });
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

/// Sends one request to the executor and reads its single response.
///
/// Provenance comes from the delivered event, so the executor records the
/// durable event that authorized the command rather than a transient call id.
async fn run(
    context: &Context,
    event: &Event,
    payload: &serde_json::Value,
) -> Result<Response, Error> {
    let arguments: Arguments = serde_json::from_value(
        payload
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({})),
    )
    .map_err(|error| {
        failure(
            ErrorCode::InvalidArgument,
            format!("invalid arguments: {error}"),
        )
    })?;
    if arguments.timeout_ms == 0 || arguments.timeout_ms > MAX_TIMEOUT_MS {
        return Err(failure(
            ErrorCode::InvalidArgument,
            "invalid timeout (1..300000 ms)",
        ));
    }

    let env = EXPORTS.with_borrow(|names| {
        names
            .iter()
            .map(|name| credentials::resolve_export(name).map(|value| (name.clone(), value)))
            .collect::<Result<std::collections::BTreeMap<_, _>, Error>>()
    })?;
    let request = Request {
        env,
        version: VERSION,
        command: arguments.command,
        timeout_ms: arguments.timeout_ms,
        invocation_id: event.event_id.clone(),
        // The event id is real provenance, so it stands in for an absent
        // envelope field rather than an empty string the executor rejects.
        authority_id: event
            .authority_id
            .clone()
            .unwrap_or_else(|| event.event_id.clone()),
        activity_id: event
            .activity_id
            .clone()
            .unwrap_or_else(|| event.event_id.clone()),
        origin_event_id: event
            .causation_id
            .clone()
            .unwrap_or_else(|| event.event_id.clone()),
    };
    request
        .validate()
        .map_err(|message| failure(ErrorCode::InvalidArgument, message))?;
    let mut bytes = serde_json::to_vec(&request)
        .map_err(|_| failure(ErrorCode::Internal, "cannot encode shell request"))?;
    if bytes.len() >= protocol::MAX_REQUEST {
        return Err(failure(
            ErrorCode::ResourceExhausted,
            "shell request exceeds limit",
        ));
    }
    bytes.push(b'\n');

    let _ = context;
    // The channel stays open across the read: the executor reads a half-close
    // as a cancellation, and dropping it ends the command.
    let mut channel = Socket::connect().await?;
    exchange(&mut channel, &bytes, request.timeout_ms).await
}

async fn exchange(
    channel: &mut Socket,
    request: &[u8],
    timeout_ms: u32,
) -> Result<Response, Error> {
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
                "executor response exceeds limit",
            ));
        }
        if bytes.last() == Some(&b'\n') {
            break;
        }
        if chunk.closed {
            return Err(failure(ErrorCode::Unavailable, "executor disconnected"));
        }
        if chunk.bytes.is_empty() {
            return Err(failure(
                ErrorCode::DeadlineExceeded,
                "executor did not respond",
            ));
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| failure(ErrorCode::Unavailable, "malformed executor response"))
}

/// Turns the executor's response into a terminal event. A non-completion is a
/// failure event, not a trap: the requester needs a result either way.
fn complete(event: &Event, response: Response) -> Result<Proposal, Error> {
    match response {
        Response::Completed {
            stdout,
            stderr,
            exit_code,
            truncated,
        } => proposal(
            "capability.completed",
            "dev.pluribus.shell-result/1",
            &json!({
                "requestEventId": event.event_id,
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "truncated": truncated,
            }),
            Some(event.event_id.clone()),
        ),
        Response::Cancelled => Err(failure(ErrorCode::Cancelled, "shell command cancelled")),
        Response::DeadlineExceeded => Err(failure(
            ErrorCode::DeadlineExceeded,
            "shell command deadline exceeded",
        )),
        Response::Unavailable { message } => Err(failure(ErrorCode::Unavailable, message)),
    }
}

fn json_payload(event: &Event) -> Result<serde_json::Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes).map_err(|error| {
            failure(
                ErrorCode::InvalidArgument,
                format!("invalid payload: {error}"),
            )
        }),
        Payload::Blob(_) => Err(failure(
            ErrorCode::InvalidArgument,
            "shell does not read blob payloads",
        )),
    }
}

fn proposal(
    event_type: &str,
    payload_schema: &str,
    value: &serde_json::Value,
    causation_id: Option<String>,
) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: event_type.to_owned(),
        payload_schema: payload_schema.to_owned(),
        payload: Payload::Json(
            serde_json::to_vec(value)
                .map_err(|_| failure(ErrorCode::Internal, "cannot encode payload"))?,
        ),
        idempotency_key: None,
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

fn failure(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
        retryable: false,
        details: None,
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

export!(Shell);
