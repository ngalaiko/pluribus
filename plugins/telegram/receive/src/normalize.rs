use serde_json::{Map, Value, json};
use telegram::api;
use telegram::pluribus::plugin::types::{BlobRef, Error};

/// One normalized update, including attachment download descriptors.
pub struct Observation {
    pub payload: Value,
}

pub fn updates(
    result: &Value,
    trusted_senders: &[String],
    limit: usize,
) -> Result<Vec<Observation>, Error> {
    result
        .as_array()
        .ok_or_else(|| api::unavailable("Telegram getUpdates result is not an array"))?
        .iter()
        .take(limit)
        .filter(|update| {
            let (kind, content, _) = update_content(update);
            kind != "unknown" && allowed_sender(sender_id(content).as_deref(), trusted_senders)
        })
        .map(normalize_update)
        .collect()
}

pub(crate) fn allowed_sender(sender: Option<&str>, trusted_senders: &[String]) -> bool {
    sender.is_some_and(|sender| trusted_senders.iter().any(|allowed| allowed == sender))
}

fn normalize_update(update: &Value) -> Result<Observation, Error> {
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
    Some(format!("chat:{chat}"))
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
            "media_group_id",
            "text",
            "caption",
            "entities",
            "caption_entities",
            "from",
            "sender_chat",
            "chat",
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
    if let Some(nested) = message.get("message") {
        normalized.insert("message".into(), normalize_message(nested));
    }
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
    fn thread_ids_do_not_change_observation_identity() {
        let result = updates(
            &json!([
                {"update_id":1,"message":{"from":{"id":7},"chat":{"id":9},"message_thread_id":10}},
                {"update_id":2,"message":{"from":{"id":7},"chat":{"id":9},"message_thread_id":20}}
            ]),
            &["7".into()],
            100,
        )
        .unwrap();
        for observation in result {
            assert_eq!(observation.payload["conversationId"], "chat:9");
            assert!(
                observation.payload["message"]
                    .get("message_thread_id")
                    .is_none()
            );
        }
    }

    #[test]
    fn ignored_malformed_update_does_not_block_admitted_sender() {
        let batch = json!([
            {"message":{"from":{"id":8},"document":{"file_id":"ignored"}}},
            {"update_id":2,"message":{"from":{"id":7},"text":"allowed"}},
            {"update_id":3,"from":{"id":7}}
        ]);
        assert_eq!(updates(&batch, &["7".into()], 100).unwrap().len(), 1);
    }

    #[test]
    fn only_listed_senders_are_admitted() {
        for kind in [
            "edited_message",
            "channel_post",
            "edited_channel_post",
            "message_reaction",
            "callback_query",
        ] {
            let sender_field = if kind == "message_reaction" {
                "user"
            } else {
                "from"
            };
            let batch = json!([
                {"update_id":1, kind: {sender_field: {"id":7}, "text":"allowed"}},
                {"update_id":2, kind: {sender_field: {"id":8}, "text":"ignored"}},
                {"update_id":3, kind: {"text":"anonymous"}}
            ]);
            let admitted = updates(&batch, &["7".into()], 100).unwrap();
            assert_eq!(admitted.len(), 1, "{kind}");
            assert_eq!(admitted[0].payload["externalSenderId"], "7");
            assert!(admitted[0].payload.get("raw").is_none());
            assert!(updates(&batch, &[], 100).unwrap().is_empty());
        }
    }

    #[test]
    fn malformed_attachment_does_not_block_later_updates() {
        let observations = updates(
            &json!([
                {"update_id":1,"message":{"from":{"id":7},"photo":[{}]}},
                {"update_id":2,"message":{"from":{"id":7},"text":"next"}}
            ]),
            &["7".into()],
            100,
        )
        .unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].payload["media"][0]["status"], "failed");
        assert_eq!(observations[1].payload["message"]["text"], "next");
    }
}
