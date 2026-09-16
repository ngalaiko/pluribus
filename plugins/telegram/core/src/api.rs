//! The Telegram HTTP surface both components call.

use crate::http::{self, Header, Request};
use crate::pluribus::plugin::blobs;
use crate::pluribus::plugin::types::{BlobRef, Error, ErrorCode};
use serde_json::Value;

pub const ORIGIN: &str = "https://api.telegram.org";
pub const CREDENTIAL_MARKER: &str = "_pluribus_credential_";
pub const CHUNK_BYTES: u32 = 1024 * 1024;

pub struct TelegramResponse {
    pub value: Value,
    /// The undecoded body. Receiving keeps it; sending drops it.
    #[allow(dead_code)]
    pub raw: BlobRef,
}

pub fn call_json(
    method: &str,
    payload: &Value,
    credential: &str,
    timeout_ms: u32,
) -> Result<TelegramResponse, Error> {
    let response = http::send(&request(method, payload, credential, timeout_ms)?)?;
    decode_response(response.status, response.body)
}

pub fn request(
    method: &str,
    payload: &Value,
    credential: &str,
    timeout_ms: u32,
) -> Result<Request, Error> {
    let body = put_blob(
        "application/json",
        &serde_json::to_vec(payload).map_err(internal)?,
    )?;
    Ok(Request {
        method: "POST".into(),
        url: format!("{ORIGIN}/{CREDENTIAL_MARKER}/{method}"),
        headers: vec![Header {
            name: "content-type".into(),
            value: b"application/json".to_vec(),
        }],
        body: Some(body),
        credential: Some(credential.into()),
        timeout_ms,
    })
}

pub fn read_blob(blob: &BlobRef) -> Result<Vec<u8>, Error> {
    let capacity = usize::try_from(blob.size).map_err(|_| internal("blob is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut offset = 0_u64;
    loop {
        let chunk = blobs::read(blob, offset, CHUNK_BYTES)?;
        offset = offset
            .checked_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| internal("blob size overflow"))?;
        bytes.extend_from_slice(&chunk.bytes);
        if chunk.closed {
            return Ok(bytes);
        }
    }
}

fn put_blob(media_type: &str, bytes: &[u8]) -> Result<BlobRef, Error> {
    let upload = blobs::open_write(
        media_type,
        Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
    )?;
    let mut offset = 0_u64;
    for chunk in bytes.chunks(usize::try_from(CHUNK_BYTES).unwrap_or(usize::MAX)) {
        write(&upload, &mut offset, chunk)?;
    }
    blobs::finish(&upload)
}

pub fn write(upload: &str, offset: &mut u64, bytes: &[u8]) -> Result<(), Error> {
    *offset = blobs::write(upload, *offset, bytes)?;
    Ok(())
}

pub fn decode_response(status: u16, body: BlobRef) -> Result<TelegramResponse, Error> {
    let bytes = read_blob(&body)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| unavailable(format!("Telegram returned invalid JSON: {error}")))?;
    if !(200..300).contains(&status) || value.get("ok").and_then(Value::as_bool) != Some(true) {
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("Telegram API request failed");
        return Err(unavailable(format!("{description} (HTTP {status})")));
    }
    Ok(TelegramResponse {
        value: value.get("result").cloned().unwrap_or(Value::Null),
        raw: body,
    })
}

pub fn invalid(message: impl Into<String>) -> Error {
    Error {
        code: ErrorCode::InvalidArgument,
        message: message.into(),
        retryable: false,
        details: None,
    }
}

pub fn unavailable(message: impl Into<String>) -> Error {
    Error {
        code: ErrorCode::Unavailable,
        message: message.into(),
        retryable: true,
        details: None,
    }
}

pub fn internal(error: impl std::fmt::Display) -> Error {
    Error {
        code: ErrorCode::Internal,
        message: error.to_string(),
        retryable: false,
        details: None,
    }
}
