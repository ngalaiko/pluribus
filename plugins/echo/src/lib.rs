#![allow(unsafe_op_in_unsafe_fn)]

wit_bindgen::generate!({
    path: "../../wit",
    world: "plugin",
});

use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::events;
use pluribus::plugin::state;
use pluribus::plugin::types::{Error, ErrorCode, Event, Mutation, Payload, Proposal, StateEntry};

/// Answers `capability.requested` for `system.echo`, returning the arguments
/// and a running invocation count.
struct Echo;

const CAPABILITY: &str = "system.echo";
const COUNT_KEY: &str = "invocations";

impl Guest for Echo {
    fn init(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
        let config: serde_json::Value = serde_json::from_slice(&config)
            .map_err(|error| invalid_argument(format!("invalid configuration: {error}")))?;
        if !config.is_object() {
            return Err(invalid_argument("configuration must be an object"));
        }
        Ok(empty_outcome())
    }

    fn handle(context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let mut count = stored_count()?;
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
            count += 1;

            // Non-terminal output commits immediately so a caller sees progress
            // while the delivery is still running.
            let progress = proposal(
                "capability.output",
                "dev.pluribus.echo-progress/1",
                &serde_json::json!({
                    "requestEventId": event.event_id,
                    "invocationCount": count,
                }),
                Some(format!("{}:progress", event.event_id)),
                Some(event.event_id.clone()),
            )?;
            events::append(&progress)?;

            let arguments = request
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            if arguments
                .get("message")
                .and_then(|value| value.as_str())
                .is_none()
            {
                proposals.push(proposal(
                    "capability.failed",
                    "dev.pluribus.echo-result/1",
                    &serde_json::json!({
                        "requestEventId": event.event_id,
                        "reason": "message must be a string",
                    }),
                    None,
                    Some(event.event_id.clone()),
                )?);
                continue;
            }
            proposals.push(proposal(
                "capability.completed",
                "dev.pluribus.echo-result/1",
                &serde_json::json!({
                    "requestEventId": event.event_id,
                    "output": arguments,
                    "invocationCount": count,
                }),
                None,
                Some(event.event_id.clone()),
            )?);
        }

        let mutations = if count == stored_count()? {
            Vec::new()
        } else {
            vec![Mutation::Set(StateEntry {
                key: COUNT_KEY.to_owned(),
                value: count.to_string().into_bytes(),
            })]
        };
        let _ = context;
        Ok(Outcome {
            events: proposals,
            mutations,
            checkpoint,
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(empty_outcome())
    }
}

/// Reads the counter, treating a cleared namespace as a fresh start: state is
/// a rebuildable projection, not durable truth.
fn stored_count() -> Result<u64, Error> {
    let Some(bytes) = state::get(COUNT_KEY)? else {
        return Ok(0);
    };
    let text =
        String::from_utf8(bytes).map_err(|error| internal(format!("invalid counter: {error}")))?;
    text.parse()
        .map_err(|error| internal(format!("invalid counter: {error}")))
}

fn json_payload(event: &Event) -> Result<serde_json::Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| invalid_argument(format!("invalid payload: {error}"))),
        Payload::Blob(_) => Err(invalid_argument("echo does not read blob payloads")),
    }
}

fn proposal(
    event_type: &str,
    payload_schema: &str,
    value: &serde_json::Value,
    idempotency_key: Option<String>,
    causation_id: Option<String>,
) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: event_type.to_owned(),
        payload_schema: payload_schema.to_owned(),
        payload: Payload::Json(
            serde_json::to_vec(value)
                .map_err(|error| internal(format!("cannot encode payload: {error}")))?,
        ),
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

fn invalid_argument(message: impl Into<String>) -> Error {
    Error {
        code: ErrorCode::InvalidArgument,
        message: message.into(),
        retryable: false,
        details: None,
    }
}

fn internal(message: impl Into<String>) -> Error {
    Error {
        code: ErrorCode::Internal,
        message: message.into(),
        retryable: false,
        details: None,
    }
}

export!(Echo);
