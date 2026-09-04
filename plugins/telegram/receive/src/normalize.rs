use serde_json::{Map, Value, json};
use telegram::api;
use telegram::pluribus::plugin::types::{BlobRef, Error};

/// One candidate observation: the dedup key the host needs and the payload it
/// carries.
pub struct Observation {
    pub deduplication_key: String,
    pub payload: Value,
}

pub fn updates(
    result: &Value,
    raw: &BlobRef,
    _credential: &str,
    limit: usize,
) -> Result<Vec<Observation>, Error> {
    result
        .as_array()
        .ok_or_else(|| api::unavailable("Telegram getUpdates result is not an array"))?
        .iter()
        .take(limit)
        .map(|update| normalize_update(update, raw))
        .collect()
}

fn normalize_update(update: &Value, raw: &BlobRef) -> Result<Observation, Error> {
    let update_id = integer(update, "update_id")?;
    let (kind, content, edited) = update_content(update);
    let sender = sender_id(content);
    let conversation = conversation_id(content);
    let reply_to = content
        .get("reply_to_message")
        .and_then(|reply| reply.get("message_id"))
        .and_then(Value::as_i64)
        .map(|id| id.to_string());
    let mut media = Vec::new();
    if is_message_kind(kind) {
        collect_media(content, &mut media);
    }
    // observed_at_ms is provider data and untrusted, so it lives in the
    // payload rather than the host-stamped envelope.
    Ok(Observation {
        deduplication_key: format!("telegram:update:{update_id}"),
        payload: json!({
            "provider": "telegram",
            "update_id": update_id,
            "update_kind": kind,
            "edited": edited,
            "observedAtMs": content
                .get("date")
                .and_then(Value::as_i64)
                .and_then(|seconds| seconds.checked_mul(1_000)),
            "externalSenderId": sender,
            "conversationId": conversation,
            "replyToExternalId": reply_to,
            "message": normalize_message(content),
            "raw": blob_json(raw),
            "media": media,
        }),
    })
}

/// A blob reference as JSON, so an event payload can name content without
/// inlining it.
pub(crate) fn blob_json(blob: &BlobRef) -> Value {
    json!({
        "algorithm": blob.algorithm,
        "digest": blob.digest,
        "size": blob.size,
        "mediaType": blob.media_type,
    })
}

fn update_content(update: &Value) -> (&str, &Value, bool) {
    for (field, edited) in [
        ("message", false),
        ("edited_message", true),
        ("channel_post", false),
        ("edited_channel_post", true),
        ("message_reaction", false),
        ("callback_query", false),
    ] {
        if let Some(content) = update.get(field) {
            return (field, content, edited);
        }
    }
    ("unknown", update, false)
}

fn is_message_kind(kind: &str) -> bool {
    matches!(
        kind,
        "message" | "edited_message" | "channel_post" | "edited_channel_post"
    )
}

fn sender_id(content: &Value) -> Option<String> {
    content
        .get("from")
        .or_else(|| content.get("user"))
        .and_then(|sender| sender.get("id"))
        .and_then(Value::as_i64)
        .map(|id| id.to_string())
}

fn conversation_id(content: &Value) -> Option<String> {
    let message = content.get("message").unwrap_or(content);
    let chat = message
        .get("chat")
        .and_then(|chat| chat.get("id"))
        .and_then(Value::as_i64)?;
    let thread = message.get("message_thread_id").and_then(Value::as_i64);
    Some(thread.map_or_else(
        || format!("chat:{chat}"),
        |thread| format!("chat:{chat}:thread:{thread}"),
    ))
}

fn normalize_message(message: &Value) -> Value {
    let mut normalized = Map::new();
    copy_fields(
        message,
        &mut normalized,
        &[
            "id",
            "message_id",
            "date",
            "message_thread_id",
            "media_group_id",
            "text",
            "caption",
            "entities",
            "caption_entities",
            "from",
            "sender_chat",
            "chat",
            "message",
            "user",
            "actor_chat",
            "chat_instance",
            "data",
            "game_short_name",
            "inline_message_id",
            "old_reaction",
            "new_reaction",
            "quote",
            "forward_origin",
            "location",
            "venue",
            "contact",
            "poll",
            "dice",
            "sticker",
            "photo",
            "document",
            "audio",
            "voice",
            "video",
            "video_note",
            "animation",
            "new_chat_members",
            "left_chat_member",
            "forum_topic_created",
            "forum_topic_edited",
        ],
    );
    if let Some(reply) = message.get("reply_to_message") {
        let mut summary = Map::new();
        copy_fields(
            reply,
            &mut summary,
            &[
                "message_id",
                "date",
                "from",
                "sender_chat",
                "text",
                "caption",
            ],
        );
        normalized.insert("reply_to".into(), Value::Object(summary));
    }
    Value::Object(normalized)
}

fn copy_fields(source: &Value, destination: &mut Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = source.get(*field) {
            destination.insert((*field).into(), value.clone());
        }
    }
}

fn collect_media(message: &Value, media: &mut Vec<Value>) {
    if let Some(photo) = message
        .get("photo")
        .and_then(Value::as_array)
        .and_then(|sizes| sizes.last())
    {
        describe("photo", photo, None, media);
    }
    for (field, default_name) in [
        ("document", None),
        ("audio", Some("audio")),
        ("voice", Some("voice.ogg")),
        ("video", Some("video.mp4")),
        ("video_note", Some("video-note.mp4")),
        ("animation", Some("animation.mp4")),
        ("sticker", Some("sticker")),
    ] {
        if let Some(value) = message.get(field) {
            describe(field, value, default_name, media);
        }
    }
}

fn describe(kind: &str, value: &Value, default_name: Option<&str>, media: &mut Vec<Value>) {
    let status = if value.get("file_id").and_then(Value::as_str).is_some() {
        "pending"
    } else {
        "failed"
    };
    let file_name = value
        .get("file_name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| default_name.map(str::to_owned));
    media.push(json!({
        "kind": kind,
        "status": status,
        "fileName": file_name,
        "metadata": value,
    }));
}

fn integer(value: &Value, field: &str) -> Result<i64, Error> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| api::unavailable(format!("Telegram update has no {field}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_attachment_does_not_block_later_updates() {
        let raw = BlobRef {
            algorithm: "sha256".into(),
            digest: "fixture".into(),
            size: 0,
            media_type: "application/json".into(),
        };
        let observations = updates(
            &json!([
                {"update_id":1,"message":{"photo":[{}]}},
                {"update_id":2,"message":{"text":"next"}}
            ]),
            &raw,
            "fixture",
            100,
        )
        .unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].payload["media"][0]["status"], "failed");
        assert_eq!(observations[1].payload["message"]["text"], "next");
    }
}
