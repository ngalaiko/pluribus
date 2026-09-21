use crate::pluribus::plugin::types::{Error, ErrorCode, Event, Payload, Proposal};
use serde_json::Value;

/// Bound on one listener response.
const RESPONSE_TIMEOUT_MS: u32 = 2000;
pub fn error(message: &str) -> Error {
    Error {
        code: ErrorCode::Unavailable,
        message: message.into(),
        retryable: true,
        details: None,
    }
}
pub fn value(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes).map_err(|_| error("invalid event")),
        _ => Err(error("JSON event required")),
    }
}
pub fn proposal(kind: &str, value: Value, cause: Option<String>) -> Proposal {
    Proposal {
        event_type: kind.into(),
        payload_schema: format!("pluribus.{kind}/1"),
        payload: Payload::Json(serde_json::to_vec(&value).unwrap()),
        idempotency_key: None,
        causation_id: cause,
    }
}
pub async fn exchange(value: Value) -> Result<Value, Error> {
    let mut channel = crate::Socket::connect("default").await?;
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    channel.send(&bytes).await?;
    let mut bytes = Vec::new();
    loop {
        let chunk = channel.read(64 * 1024, Some(RESPONSE_TIMEOUT_MS)).await?;
        let empty = chunk.bytes.is_empty();
        bytes.extend(chunk.bytes);
        if bytes.len() > 40 * 1024 * 1024 {
            return Err(error("native response exceeds limit"));
        }
        if bytes.last() == Some(&b'\n') {
            break;
        }
        if chunk.closed {
            return Err(error("native peer disconnected"));
        }
        if empty {
            return Err(error("native peer did not respond"));
        }
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| error("invalid native response"))?;
    if v.get("error").is_some() {
        return Err(error("native operation unavailable"));
    }
    Ok(v)
}
