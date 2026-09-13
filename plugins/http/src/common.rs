use crate::pluribus::plugin::{
    socket,
    types::{Error, ErrorCode, Event, Payload, Proposal},
};
use serde_json::Value;
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
pub fn exchange(value: Value) -> Result<Value, Error> {
    let (read, write) = socket::connect()?;
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    write.send(&bytes)?;
    let mut bytes = Vec::new();
    loop {
        let chunk = read.receive(64 * 1024, 2000)?;
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
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| error("invalid native response"))?;
    if v.get("error").is_some() {
        return Err(error("native operation unavailable"));
    }
    Ok(v)
}
