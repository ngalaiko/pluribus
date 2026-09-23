//! The cursor, and the observations a fetched message becomes.
//!
//! A message is identified by mailbox, UIDVALIDITY and UID. Sequence numbers
//! are not identity: they shift under expunges and mean nothing across
//! reconnects. RFC 9051 §2.3.1.1.

use crate::pluribus::plugin::types::{
    BlobRef, Error, ErrorCode, Mutation, Payload, Proposal, StateEntry,
};
use crate::pluribus::plugin::{blobs, state};
use crate::wire::failure;
use crate::{Config, imap, mime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SCHEMA: &str = "dev.pluribus.email.observation.v1";
/// Ceiling on a header value carried into an observation.
const MAX_HEADER: usize = 4 * 1024;

/// Where this mailbox has been read to.
pub struct Cursor {
    pub mailbox: String,
    pub uid_validity: u64,
    pub last_uid: u64,
    /// Emitted once when the mailbox was renumbered under us.
    pub resynchronized: Option<Proposal>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct StoredCursor {
    uidvalidity: u64,
    last_uid: u64,
}

fn key(mailbox: &str) -> String {
    format!("mailbox/{mailbox}")
}

impl Cursor {
    /// # Errors
    /// Returns an error when the cursor cannot be encoded.
    pub fn mutation(&self) -> Result<Mutation, Error> {
        let stored = StoredCursor {
            uidvalidity: self.uid_validity,
            last_uid: self.last_uid,
        };
        Ok(Mutation::Set(StateEntry {
            key: key(&self.mailbox),
            value: serde_json::to_vec(&stored)
                .map_err(|_| failure(ErrorCode::Internal, "cannot encode the mailbox cursor"))?,
        }))
    }
}

/// Reads the stored cursor and reconciles it with the mailbox as opened.
///
/// # Errors
/// Returns an error when stored state cannot be read or decoded.
pub fn resolve_cursor(
    mailbox: &str,
    opened: &imap::Mailbox,
    import_history: bool,
    now_ms: i64,
) -> Result<Cursor, Error> {
    let stored: Option<StoredCursor> = state::get(&key(mailbox))?
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()
        .map_err(|_| failure(ErrorCode::Internal, "stored mailbox cursor is malformed"))?;
    reconcile(mailbox, stored, opened, import_history, now_ms)
}

/// Decides where to resume from.
///
/// A changed UIDVALIDITY invalidates every stored UID, so the old cursor is
/// discarded rather than reused. Discarding it silently would lose whatever
/// arrived in between, so the reset is itself observable.
fn reconcile(
    mailbox: &str,
    stored: Option<StoredCursor>,
    opened: &imap::Mailbox,
    import_history: bool,
    now_ms: i64,
) -> Result<Cursor, Error> {
    let boundary = opened.uid_next.saturating_sub(1);
    let Some(stored) = stored else {
        return Ok(Cursor {
            mailbox: mailbox.to_owned(),
            uid_validity: opened.uid_validity,
            // A first connection records where the mailbox stands. Importing
            // what is already there is a deliberate choice, not a default.
            last_uid: if import_history { 0 } else { boundary },
            resynchronized: None,
        });
    };
    if stored.uidvalidity == opened.uid_validity {
        return Ok(Cursor {
            mailbox: mailbox.to_owned(),
            uid_validity: opened.uid_validity,
            // A stored cursor above the mailbox boundary means the mailbox
            // shrank without renumbering; resuming above it would observe
            // nothing ever again.
            last_uid: stored.last_uid.min(boundary),
            resynchronized: None,
        });
    }
    let notice = proposal(
        &json!({
            "provider": "email",
            "kind": "mailbox-resynchronized",
            "conversationId": conversation_id(mailbox),
            "externalSenderId": "",
            "mailbox": mailbox,
            "previousUidValidity": stored.uidvalidity,
            "uidValidity": opened.uid_validity,
            "previousLastUid": stored.last_uid,
            "observedAtMs": now_ms,
            "trusted": false,
            "message": {
                "text": format!(
                    "Mailbox {mailbox} was renumbered. Messages that arrived before the reset were not observed."
                ),
            },
        }),
        format!("email:{mailbox}:resync:{}", opened.uid_validity),
    )?;
    Ok(Cursor {
        mailbox: mailbox.to_owned(),
        uid_validity: opened.uid_validity,
        last_uid: boundary,
        resynchronized: Some(notice),
    })
}

/// Turns a fetched message into one observation, storing its attachments.
///
/// # Errors
/// Returns an error when a blob cannot be written or the payload encoded.
pub fn from_message(
    config: &Config,
    cursor: &Cursor,
    fetched: &imap::Fetched,
    now_ms: i64,
) -> Result<Proposal, Error> {
    let message = mime::parse(&fetched.bytes);
    let headers = &message.headers;
    let mut attachments = Vec::new();
    for attachment in &message.attachments {
        attachments.push(store(config, attachment)?);
    }
    let mut text = message.text.clone();
    mime::truncate_utf8(&mut text, mime::MAX_BODY_BYTES);
    let from = headers.get("from").unwrap_or_default();
    let payload = json!({
        "provider": "email",
        "kind": "message",
        // The sender's address identifies the correspondent; the mailbox
        // identifies the thread this connector delivers into.
        "externalSenderId": address(&from),
        "conversationId": conversation_id(&cursor.mailbox),
        "mailbox": cursor.mailbox,
        "uidValidity": cursor.uid_validity,
        "uid": fetched.uid,
        "sizeBytes": fetched.size,
        "truncated": fetched.truncated,
        "internalDate": fetched.internal_date,
        "observedAtMs": now_ms,
        // Everything below is written by the sender.
        "trusted": false,
        "from": bounded(&from),
        "to": bounded_all(headers.all("to")),
        "cc": bounded_all(headers.all("cc")),
        "replyTo": headers.get("reply-to").map(|value| bounded(&value)),
        "subject": headers.get("subject").map(|value| bounded(&value)),
        "date": headers.get("date").map(|value| bounded(&value)),
        "messageId": headers.get("message-id").map(|value| bounded(&value)),
        "inReplyTo": headers.get("in-reply-to").map(|value| bounded(&value)),
        // The chain a reply carries forward so a long thread stays one.
        "references": identifiers(headers.get("references").as_deref()),
        "bodyFromHtml": message.text_from_html,
        "message": { "text": text },
        "attachments": attachments,
    });
    proposal(
        &payload,
        format!(
            "email:{}:{}:{}",
            cursor.mailbox, cursor.uid_validity, fetched.uid
        ),
    )
}

/// Stores one attachment, or describes it without its bytes when it exceeds
/// the configured ceiling.
fn store(config: &Config, attachment: &mime::Attachment) -> Result<Value, Error> {
    let described = json!({
        "fileName": attachment.file_name,
        "mediaType": attachment.media_type,
        "sizeBytes": attachment.bytes.len(),
    });
    if attachment.bytes.len() as u64 > config.max_attachment_bytes {
        let mut value = described;
        value["status"] = json!("too-large");
        return Ok(value);
    }
    let blob = blobs::put(&attachment.media_type, &attachment.bytes)?;
    let mut value = described;
    value["status"] = json!("ready");
    value["blob"] = blob_json(&blob);
    Ok(value)
}

fn blob_json(blob: &BlobRef) -> Value {
    pluribus_plugin_sdk::blob::BlobRef::from(blob.clone())
        .to_json(pluribus_plugin_sdk::blob::MediaTypeSpelling::Camel)
}

fn proposal(payload: &Value, idempotency_key: String) -> Result<Proposal, Error> {
    Ok(Proposal {
        event_type: "observation.received".into(),
        payload_schema: SCHEMA.into(),
        payload: Payload::Json(
            serde_json::to_vec(payload)
                .map_err(|_| failure(ErrorCode::Internal, "cannot encode the observation"))?,
        ),
        idempotency_key: Some(idempotency_key),
        causation_id: None,
    })
}

/// A mailbox is one conversation: mail has no thread the agent replies into
/// the way a chat does.
fn conversation_id(mailbox: &str) -> String {
    format!("email:{mailbox}")
}

/// The address out of a `Name <address>` header, lowercased.
fn address(value: &str) -> String {
    let inner = match (value.rfind('<'), value.rfind('>')) {
        (Some(open), Some(close)) if close > open => &value[open + 1..close],
        _ => value,
    };
    inner.trim().to_ascii_lowercase()
}

/// The message identifiers in a `References` header, brackets removed.
/// Whitespace is the only separator the header grammar allows between them.
fn identifiers(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|token| {
            let id = token.trim_start_matches('<').trim_end_matches('>');
            (!id.is_empty()).then(|| bounded(id))
        })
        .take(64)
        .collect()
}

fn bounded(value: &str) -> String {
    let mut value = value.to_owned();
    mime::truncate_utf8(&mut value, MAX_HEADER);
    value
}

fn bounded_all(values: Vec<String>) -> Vec<String> {
    values.iter().map(|value| bounded(value)).take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_extracted_from_a_display_name() {
        assert_eq!(address("Ada Lovelace <Ada@Example.COM>"), "ada@example.com");
        assert_eq!(address("plain@example.com"), "plain@example.com");
        assert_eq!(address("  Spaced <a@b.c> "), "a@b.c");
        // Malformed angle brackets fall back to the whole value.
        assert_eq!(address("> broken <"), "> broken <");
    }

    #[test]
    fn a_mailbox_is_one_conversation() {
        assert_eq!(conversation_id("INBOX"), "email:INBOX");
    }

    fn opened(uid_validity: u64, uid_next: u64) -> imap::Mailbox {
        imap::Mailbox {
            uid_validity,
            uid_next,
            exists: 0,
        }
    }

    #[test]
    fn a_first_connection_records_the_boundary_and_observes_arrivals() {
        let cursor = reconcile("INBOX", None, &opened(1, 101), false, 0).unwrap();
        assert_eq!(cursor.last_uid, 100);
        assert_eq!(cursor.uid_validity, 1);
        assert!(cursor.resynchronized.is_none());
    }

    #[test]
    fn importing_history_is_an_explicit_choice() {
        let cursor = reconcile("INBOX", None, &opened(1, 101), true, 0).unwrap();
        assert_eq!(cursor.last_uid, 0);
        assert!(cursor.resynchronized.is_none());
    }

    #[test]
    fn a_matching_uidvalidity_resumes_where_it_left_off() {
        let stored = StoredCursor {
            uidvalidity: 7,
            last_uid: 42,
        };
        let cursor = reconcile("INBOX", Some(stored), &opened(7, 60), false, 0).unwrap();
        assert_eq!(cursor.last_uid, 42);
        assert!(cursor.resynchronized.is_none());
    }

    /// The stored UID means nothing under a new UIDVALIDITY, so the boundary
    /// is re-recorded — and the gap that creates is said out loud.
    #[test]
    fn a_changed_uidvalidity_resynchronizes_and_says_so() {
        let stored = StoredCursor {
            uidvalidity: 7,
            last_uid: 42,
        };
        let cursor = reconcile("INBOX", Some(stored), &opened(8, 11), false, 0).unwrap();
        assert_eq!(cursor.uid_validity, 8);
        assert_eq!(cursor.last_uid, 10, "the old UID is not reused");
        let notice = cursor.resynchronized.expect("the reset is observable");
        assert_eq!(notice.event_type, "observation.received");
        assert_eq!(
            notice.idempotency_key.as_deref(),
            Some("email:INBOX:resync:8")
        );
    }

    /// A mailbox that shrank without renumbering would otherwise leave the
    /// cursor stranded above every UID that will ever exist.
    #[test]
    fn a_cursor_above_the_boundary_is_pulled_back_to_it() {
        let stored = StoredCursor {
            uidvalidity: 7,
            last_uid: 900,
        };
        let cursor = reconcile("INBOX", Some(stored), &opened(7, 11), false, 0).unwrap();
        assert_eq!(cursor.last_uid, 10);
        assert!(cursor.resynchronized.is_none());
    }

    #[test]
    fn a_cursor_round_trips_through_its_mutation() {
        let cursor = reconcile("INBOX", None, &opened(3, 15), false, 0).unwrap();
        let Mutation::Set(entry) = cursor.mutation().unwrap() else {
            panic!("a cursor is stored, never deleted")
        };
        assert_eq!(entry.key, "mailbox/INBOX");
        let stored: StoredCursor = serde_json::from_slice(&entry.value).unwrap();
        assert_eq!(stored.uidvalidity, 3);
        assert_eq!(stored.last_uid, 14);
    }

    #[test]
    fn a_reference_chain_is_split_on_whitespace_and_unbracketed() {
        assert_eq!(
            identifiers(Some("<a@x>\r\n <b@x> <c@x>")),
            ["a@x", "b@x", "c@x"]
        );
        assert!(identifiers(None).is_empty());
        assert!(identifiers(Some("  ")).is_empty());
    }

    #[test]
    fn headers_are_bounded() {
        let long = "x".repeat(MAX_HEADER * 2);
        assert_eq!(bounded(&long).len(), MAX_HEADER);
        assert_eq!(bounded_all(vec![long; 100]).len(), 64);
    }
}
