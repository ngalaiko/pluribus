//! Downloading an attachment Telegram holds.

use serde_json::{Value, json};
use telegram::api::{ORIGIN, call_json, invalid, unavailable};
use telegram::http::{self, Request};
use telegram::pluribus::plugin::types::{BlobRef, Error};

pub fn download(file_id: &str, token: &str) -> Result<(BlobRef, Option<String>), Error> {
    let response = call_json("getFile", &json!({"file_id": file_id}), token, 30_000)?;
    let path = response
        .value
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Telegram getFile returned no file_path"))?;
    if path.is_empty()
        || path.starts_with('/')
        || path.contains("..")
        || !path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.'))
    {
        return Err(invalid("Telegram returned an unsafe file_path"));
    }
    let response = http::send(&Request {
        method: "GET".into(),
        url: format!("{ORIGIN}/file/bot{token}/{path}"),
        headers: Vec::new(),
        body: None,
        timeout_ms: 60_000,
    })?;
    if response.status != 200 {
        return Err(unavailable(format!(
            "Telegram file download returned HTTP {}",
            response.status
        )));
    }
    Ok((response.body, path.rsplit('/').next().map(str::to_owned)))
}
