use crate::pluribus::plugin::types::{Error, ErrorCode, Event, Payload, Proposal};
use serde_json::{Value, json};
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
pub fn timer(at: i64, cause: Option<String>) -> Proposal {
    let mut p = proposal("timer.set", json!({"dueAtMs":at}), cause);
    p.payload_schema = "pluribus.timer-set/1".into();
    p
}
