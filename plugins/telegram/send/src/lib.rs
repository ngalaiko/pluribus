#![allow(unsafe_op_in_unsafe_fn)]

mod multipart;

use multipart::BlobArgument;
use serde::Deserialize;
use serde_json::{Value, json};
use telegram::exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use telegram::pluribus::plugin::types::{Error, ErrorCode, Event, Payload, Proposal};
use telegram::{Slot, parse_config, proposal};

const CAPABILITIES: &[&str] = &[
    "telegram.reply",
    "telegram.send-message",
    "telegram.send-draft",
    "telegram.send-media",
    "telegram.send-media-group",
    "telegram.send-location",
    "telegram.react",
    "telegram.edit-message",
    "telegram.delete-message",
];

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(rename = "bot-token")]
    bot_token: String,
}

#[derive(Clone, Deserialize)]
struct Config {
    credentials: Credentials,
}

thread_local! {
    static CONFIG: Slot<Config> = const { Slot::empty() };
}

struct Telegram;

impl Guest for Telegram {
    fn init(_context: Context, config: Vec<u8>) -> Result<Outcome, Error> {
        let parsed = parse_config(&config)?;
        CONFIG.with(|slot| slot.store(parsed));
        Ok(Outcome {
            events: Vec::new(),
            mutations: Vec::new(),
            checkpoint: None,
        })
    }

    fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let config = CONFIG.with(Slot::load)?;
        let mut proposals = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            checkpoint = Some(event.sequence);
            if event.event_type != "capability.requested" {
                continue;
            }
            let request = json_payload(event)?;
            let Some(capability) = request.get("capability").and_then(Value::as_str) else {
                continue;
            };
            if !CAPABILITIES.contains(&capability) {
                continue;
            }
            proposals.push(dispatch(event, capability, &request, &config)?);
        }

        Ok(Outcome {
            events: proposals,
            mutations: Vec::new(),
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

fn dispatch(
    event: &Event,
    capability: &str,
    request: &Value,
    config: &Config,
) -> Result<Proposal, Error> {
    let arguments = request
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let outcome = match capability {
        "telegram.reply" => reply(&arguments, config),
        "telegram.send-message" => send_message(&arguments, config),
        "telegram.send-draft" => send_draft(&arguments, config),
        "telegram.send-media" => send_media(&arguments, config),
        "telegram.send-media-group" => send_media_group(&arguments, config),
        "telegram.send-location" => send_location(&arguments, config),
        "telegram.react" => simple_call("setMessageReaction", &arguments, config),
        "telegram.edit-message" => simple_call("editMessageText", &arguments, config),
        "telegram.delete-message" => simple_call("deleteMessage", &arguments, config),
        other => Err(telegram::api::invalid(format!(
            "unknown capability: {other}"
        ))),
    };
    match outcome {
        Ok(result) => proposal(
            "capability.completed",
            "dev.pluribus.telegram.result.v1",
            &json!({"requestEventId": event.event_id, "output": result}),
            None,
            Some(event.event_id.clone()),
        ),
        Err(error) => proposal(
            "capability.failed",
            "dev.pluribus.telegram.result.v1",
            &json!({
                "requestEventId": event.event_id,
                "code": code_name(error.code),
                "reason": error.message,
            }),
            None,
            Some(event.event_id.clone()),
        ),
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

/// The provider-neutral answer every connector provides. The conversation ID
/// is the one its observations carry: `chat:<id>` or `chat:<id>:thread:<id>`.
fn reply(arguments: &Value, config: &Config) -> Result<Value, Error> {
    let conversation = required_string(arguments, "conversationId")?;
    let text = required_string(arguments, "text")?;
    let mut parts = conversation.split(':');
    let chat = match (parts.next(), parts.next()) {
        (Some("chat"), Some(chat)) => chat.to_owned(),
        _ => return Err(telegram::api::invalid("unsupported conversation")),
    };
    let thread = match (parts.next(), parts.next()) {
        (Some("thread"), Some(thread)) => Some(thread.to_owned()),
        (None, None) => None,
        _ => return Err(telegram::api::invalid("unsupported conversation")),
    };
    let mut message = json!({"chat_id": chat, "text": text});
    if let Some(thread) = thread {
        message["message_thread_id"] = json!(thread);
    }
    simple_call("sendMessage", &message, config)
}

fn send_message(arguments: &Value, config: &Config) -> Result<Value, Error> {
    required(arguments, "chat_id")?;
    required_string(arguments, "text")?;
    simple_call("sendMessage", arguments, config)
}

fn send_draft(arguments: &Value, config: &Config) -> Result<Value, Error> {
    required(arguments, "chat_id")?;
    required(arguments, "draft_id")?;
    required_string(arguments, "text")?;
    simple_call("sendMessageDraft", arguments, config)
}

fn send_location(arguments: &Value, config: &Config) -> Result<Value, Error> {
    required(arguments, "chat_id")?;
    required(arguments, "latitude")?;
    required(arguments, "longitude")?;
    let method = if arguments.get("title").is_some() && arguments.get("address").is_some() {
        "sendVenue"
    } else {
        "sendLocation"
    };
    simple_call(method, arguments, config)
}

fn send_media(arguments: &Value, config: &Config) -> Result<Value, Error> {
    let kind = required_string(arguments, "kind")?;
    let (method, field) = media_method(kind)?;
    let blob: BlobArgument = serde_json::from_value(required(arguments, "blob")?.clone())
        .map_err(|error| telegram::api::invalid(format!("invalid blob: {error}")))?;
    let file_name = arguments
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or(kind)
        .to_owned();
    let mut fields = common_fields(arguments);
    if let Some(caption) = arguments.get("caption").and_then(Value::as_str) {
        fields.push(("caption".into(), caption.into()));
    }
    if arguments.get("spoiler").and_then(Value::as_bool) == Some(true) {
        fields.push(("has_spoiler".into(), "true".into()));
    }
    let response = multipart::call(
        method,
        &fields,
        &[(field.into(), file_name, blob.into())],
        &config.credentials.bot_token,
    )?;
    Ok(response.value)
}

fn send_media_group(arguments: &Value, config: &Config) -> Result<Value, Error> {
    let items = arguments
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| telegram::api::invalid("items must be an array"))?;
    if !(2..=10).contains(&items.len()) {
        return Err(telegram::api::invalid(
            "media groups require two to ten items",
        ));
    }
    let mut media = Vec::with_capacity(items.len());
    let mut files = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let kind = required_string(item, "kind")?;
        if !matches!(kind, "photo" | "video" | "audio" | "document") {
            return Err(telegram::api::invalid("unsupported media-group kind"));
        }
        let blob: BlobArgument = serde_json::from_value(required(item, "blob")?.clone())
            .map_err(|error| telegram::api::invalid(format!("invalid blob: {error}")))?;
        let attachment = format!("file{index}");
        let mut entry = json!({"type": kind, "media": format!("attach://{attachment}")});
        if let Some(caption) = item.get("caption").and_then(Value::as_str) {
            entry["caption"] = Value::String(caption.into());
        }
        media.push(entry);
        let file_name = item
            .get("file_name")
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .to_owned();
        files.push((attachment, file_name, blob.into()));
    }
    let mut fields = common_fields(arguments);
    fields.push((
        "media".into(),
        serde_json::to_string(&media).map_err(internal)?,
    ));
    Ok(multipart::call(
        "sendMediaGroup",
        &fields,
        &files,
        &config.credentials.bot_token,
    )?
    .value)
}

fn simple_call(method: &str, arguments: &Value, config: &Config) -> Result<Value, Error> {
    Ok(telegram::api::call_json(method, arguments, &config.credentials.bot_token, 60_000)?.value)
}

fn common_fields(arguments: &Value) -> Vec<(String, String)> {
    ["chat_id", "message_thread_id", "reply_to_message_id"]
        .iter()
        .filter_map(|field| {
            arguments.get(*field).and_then(|value| match value {
                Value::String(value) => Some(((*field).to_string(), value.clone())),
                Value::Number(value) => Some(((*field).to_string(), value.to_string())),
                _ => None,
            })
        })
        .collect()
}

fn media_method(kind: &str) -> Result<(&'static str, &'static str), Error> {
    match kind {
        "photo" => Ok(("sendPhoto", "photo")),
        "document" => Ok(("sendDocument", "document")),
        "audio" => Ok(("sendAudio", "audio")),
        "voice" => Ok(("sendVoice", "voice")),
        "video" => Ok(("sendVideo", "video")),
        "video_note" => Ok(("sendVideoNote", "video_note")),
        "animation" => Ok(("sendAnimation", "animation")),
        "sticker" => Ok(("sendSticker", "sticker")),
        _ => Err(telegram::api::invalid("unsupported media kind")),
    }
}

fn json_payload(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| telegram::api::invalid(format!("invalid payload: {error}"))),
        Payload::Blob(_) => Err(telegram::api::invalid(
            "telegram does not read blob payloads",
        )),
    }
}

fn required<'a>(value: &'a Value, field: &str) -> Result<&'a Value, Error> {
    value
        .get(field)
        .ok_or_else(|| telegram::api::invalid(format!("{field} is required")))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    required(value, field)?
        .as_str()
        .ok_or_else(|| telegram::api::invalid(format!("{field} must be a string")))
}

fn internal(error: impl std::fmt::Display) -> Error {
    telegram::api::internal(error)
}

impl telegram::exports::pluribus::plugin::ingress::Guest for Telegram {
    fn receive(_: Vec<u8>) -> Result<telegram::pluribus::plugin::types::IngressOutcome, Error> {
        Err(telegram::api::invalid("sender has no ingress subscription"))
    }
}

telegram::export!(Telegram);

#[cfg(test)]
mod role_tests {
    use super::*;
    use telegram::pluribus::plugin::types::{Principal, PrincipalKind};

    fn context() -> Context {
        Context {
            instance_id: "fixture".into(),
            agent: Principal {
                kind: PrincipalKind::Agent,
                id: "fixture".into(),
            },
            state_checkpoint: 0,
            depth: 0,
            deadline_at_ms: None,
        }
    }

    #[test]
    fn role_initialization_and_irrelevant_deliveries() {
        let init = Telegram::init(
            context(),
            br#"{"credentials":{"bot-token":"fixture"}}"#.to_vec(),
        )
        .unwrap();
        assert!(init.events.is_empty());
        let event = Event {
            event_id: "event".into(),
            sequence: 1,
            recorded_at_ms: 0,
            event_type: "timer.fired".into(),
            payload_schema: "fixture".into(),
            payload: Payload::Json(b"null".to_vec()),
            actor: Principal {
                kind: PrincipalKind::Agent,
                id: "fixture".into(),
            },
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
        };
        let outcome = Telegram::handle(context(), vec![event]).unwrap();
        assert!(outcome.events.is_empty());
        assert!(outcome.mutations.is_empty());
        assert_eq!(outcome.checkpoint, Some(1));
    }
}
