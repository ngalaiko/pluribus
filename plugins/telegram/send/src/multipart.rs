//! Uploading media to Telegram as multipart form data.

use telegram::api::{CHUNK_BYTES, ORIGIN, TelegramResponse, decode_response, internal, write};
use telegram::http::{self, Header, Request};
use telegram::pluribus::plugin::blobs;
use telegram::pluribus::plugin::types::{BlobRef, Error};

pub fn call(
    method: &str,
    fields: &[(String, String)],
    files: &[(String, String, BlobRef)],
    token: &str,
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
        url: format!("{ORIGIN}/bot{token}/{method}"),
        headers: vec![Header {
            name: "content-type".into(),
            value: format!("multipart/form-data; boundary={boundary}").into_bytes(),
        }],
        body: Some(body),
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

#[cfg(test)]
mod tests {
    use pluribus_plugin_sdk::blob::BlobRef;

    #[test]
    fn accepts_blob_reference_media_type_spellings() {
        for key in ["media_type", "mediaType", "media-type"] {
            let value = serde_json::json!({
                "algorithm": "sha256",
                "digest": "a".repeat(64),
                "size": 4,
                key: "image/png"
            });
            let blob: BlobRef = serde_json::from_value(value).unwrap();
            assert_eq!(blob.media_type, "image/png");
        }
    }

    #[test]
    fn media_capability_schemas_accept_supported_blob_refs() {
        for schema in [
            include_str!("../../schemas/send-media.arguments.json"),
            include_str!("../../schemas/send-media-group.arguments.json"),
        ] {
            let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
            let validator = jsonschema::validator_for(&schema).unwrap();
            for field in ["media_type", "mediaType", "media-type"] {
                let blob = serde_json::json!({
                    "algorithm": "sha256",
                    "digest": "a".repeat(64),
                    "size": 4,
                    field: "image/png"
                });
                let arguments = if schema["required"]
                    .as_array()
                    .unwrap()
                    .contains(&"items".into())
                {
                    serde_json::json!({"chat_id":1,"items":[{"kind":"photo","blob":blob},{"kind":"document","blob":blob}]})
                } else {
                    serde_json::json!({"chat_id":1,"kind":"photo","blob":blob})
                };
                assert!(validator.is_valid(&arguments), "{field}: {arguments}");
            }

            let bad_blob = serde_json::json!({
                "algorithm": "sha256",
                "digest": "bad",
                "size": 4,
                "media_type": "image/png"
            });
            let missing_type = serde_json::json!({
                "algorithm": "sha256",
                "digest": "a".repeat(64),
                "size": 4
            });
            let wrap = |blob| {
                if schema["required"]
                    .as_array()
                    .unwrap()
                    .contains(&"items".into())
                {
                    serde_json::json!({"chat_id":1,"items":[{"kind":"photo","blob":blob},{"kind":"document","blob":blob}]})
                } else {
                    serde_json::json!({"chat_id":1,"kind":"photo","blob":blob})
                }
            };
            assert!(!validator.is_valid(&wrap(bad_blob)));
            assert!(!validator.is_valid(&wrap(missing_type)));
        }
    }

    #[test]
    fn manifest_requests_media_sized_http_grants() {
        let manifest: toml::Value = toml::from_str(include_str!("../../plugin.toml")).unwrap();
        for (component, request_bytes, response_bytes) in [
            ("receive", 1024 * 1024, 8 * 1024 * 1024),
            ("send", 8 * 1024 * 1024, 1024 * 1024),
        ] {
            let requested = &manifest["components"][component]["requested_capabilities"][0];
            assert_eq!(requested["name"].as_str(), Some("net.http"));
            let constraints = &requested["constraints"];
            assert_eq!(
                constraints
                    .get("max_request_bytes")
                    .and_then(toml::Value::as_integer)
                    .unwrap_or(1024 * 1024),
                request_bytes
            );
            assert_eq!(
                constraints
                    .get("max_response_bytes")
                    .and_then(toml::Value::as_integer)
                    .unwrap_or(1024 * 1024),
                response_bytes
            );
            assert_eq!(
                constraints
                    .get("max_timeout_ms")
                    .and_then(toml::Value::as_integer)
                    .unwrap_or(30_000),
                120_000
            );
        }
    }
}
