//! Uploading media to Telegram as multipart form data.

use serde::Deserialize;
use telegram::api::{
    CHUNK_BYTES, CREDENTIAL_MARKER, ORIGIN, TelegramResponse, decode_response, internal, write,
};
use telegram::http::{self, Header, Request};
use telegram::pluribus::plugin::blobs;
use telegram::pluribus::plugin::types::{BlobRef, Error};

#[derive(Clone, Deserialize)]
pub struct BlobArgument {
    pub algorithm: String,
    pub digest: String,
    pub size: u64,
    #[serde(rename = "media_type", alias = "mediaType")]
    pub media_type: String,
}

impl From<BlobArgument> for BlobRef {
    fn from(value: BlobArgument) -> Self {
        Self {
            algorithm: value.algorithm,
            digest: value.digest,
            size: value.size,
            media_type: value.media_type,
        }
    }
}

pub fn call(
    method: &str,
    fields: &[(String, String)],
    files: &[(String, String, BlobRef)],
    credential: &str,
) -> Result<TelegramResponse, Error> {
    let boundary = "pluribus-telegram-boundary-7d9f1a";
    let upload = blobs::open_write("multipart/form-data", None)?;
    let mut offset = 0_u64;
    for (name, value) in fields {
        write(
            &upload,
            &mut offset,
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        )?;
    }
    for (field, file_name, blob) in files {
        let safe_name = safe_file_name(file_name);
        write(
            &upload,
            &mut offset,
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{safe_name}\"\r\nContent-Type: {}\r\n\r\n",
                blob.media_type
            )
            .as_bytes(),
        )?;
        let mut read_offset = 0_u64;
        loop {
            let chunk = blobs::read(blob, read_offset, CHUNK_BYTES)?;
            if !chunk.bytes.is_empty() {
                write(&upload, &mut offset, &chunk.bytes)?;
                read_offset = read_offset
                    .checked_add(u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX))
                    .ok_or_else(|| internal("media size overflow"))?;
            }
            if chunk.closed {
                break;
            }
        }
        write(&upload, &mut offset, b"\r\n")?;
    }
    write(
        &upload,
        &mut offset,
        format!("--{boundary}--\r\n").as_bytes(),
    )?;
    let body = blobs::finish(&upload)?;
    let response = http::send(&Request {
        method: "POST".into(),
        url: format!("{ORIGIN}/{CREDENTIAL_MARKER}/{method}"),
        headers: vec![Header {
            name: "content-type".into(),
            value: format!("multipart/form-data; boundary={boundary}").into_bytes(),
        }],
        body: Some(body),
        credential: Some(credential.into()),
        timeout_ms: 120_000,
    })?;
    decode_response(response.status, response.body)
}

fn safe_file_name(name: &str) -> String {
    let value = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .take(120)
        .collect::<String>();
    if value.is_empty() {
        "file".into()
    } else {
        value
    }
}
