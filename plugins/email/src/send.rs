//! The send capabilities.
//!
//! Composition is separated from submission: a message is built and checked
//! whole before the endpoint is reached, so a malformed request costs no
//! connection and a refused connection cannot half-send anything.

use crate::pluribus::plugin::types::{Error, ErrorCode, Event, Payload, Proposal};
use crate::wire::failure;
use crate::{Account, Config, mime, smtp};
use serde_json::{Value, json};

const SCHEMA: &str = "dev.pluribus.email.result.v1";
pub const CAPABILITIES: &[&str] = &["email.reply", "email.send-message"];
/// Ceiling on the reference chain a reply carries forward. RFC 5322 allows
/// trimming a long one; the first message and the most recent ancestors are
/// what threading needs.
const MAX_REFERENCES: usize = 32;

thread_local! {
    /// Distinguishes two messages composed in the same millisecond.
    static NONCE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn nonce() -> u64 {
    NONCE.with(|counter| {
        counter.set(counter.get().wrapping_add(1));
        counter.get()
    })
}

/// Composes one message, submits it, and answers with the terminal event.
pub async fn dispatch(
    event: &Event,
    capability: &str,
    request: &Value,
    config: &Config,
    now_ms: i64,
) -> Proposal {
    let outcome = run(capability, request, config, now_ms).await;
    match outcome {
        Ok(result) => proposal(
            "capability.completed",
            &json!({"requestEventId": event.event_id, "output": result}),
            event,
        ),
        Err(error) => proposal(
            "capability.failed",
            &json!({
                "requestEventId": event.event_id,
                "code": code_name(error.code),
                "reason": error.message,
            }),
            event,
        ),
    }
}

async fn run(
    capability: &str,
    request: &Value,
    config: &Config,
    now_ms: i64,
) -> Result<Value, Error> {
    let arguments = request
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    // An unenrolled credential answers the request rather than rejecting the
    // delivery: the agent asked for something and is owed a reason.
    let account = crate::load_credential(config)?;
    let from = sender(config, &account)?;
    let message = compose(capability, &arguments, config, &from, now_ms, nonce())?;
    let recipients = message.recipients();
    let bytes = mime::write(&message).map_err(|_| {
        failure(
            ErrorCode::InvalidArgument,
            "message contains a header that cannot be folded safely",
        )
    })?;
    submit(&from, &recipients, &bytes, &account).await?;
    Ok(json!({"messageId": message.message_id, "recipients": recipients}))
}

/// The address outgoing mail claims. The credential's username is the
/// account's own address wherever submission authenticates with it.
fn sender(config: &Config, account: &Account) -> Result<String, Error> {
    let from = config
        .from
        .clone()
        .unwrap_or_else(|| account.username.clone());
    if !mime::valid_address(&from) {
        return Err(failure(
            ErrorCode::InvalidArgument,
            "configured sender is not an address this connector can send from",
        ));
    }
    Ok(from)
}

/// Builds the message a capability asks for.
///
/// Pure, so the whole of the shaping — threading, the `Re:` prefix, address
/// validation — is exercised without an endpoint.
pub fn compose(
    capability: &str,
    arguments: &Value,
    config: &Config,
    from: &str,
    now_ms: i64,
    nonce: u64,
) -> Result<mime::Outgoing, Error> {
    let text = required_string(arguments, "text")?.to_owned();
    let mut message = mime::Outgoing {
        message_id: message_id(from, now_ms, nonce),
        date: mime::rfc5322_date(now_ms),
        from: from.to_owned(),
        display_name: config.display_name.clone(),
        text,
        ..mime::Outgoing::default()
    };
    match capability {
        "email.reply" => {
            required_string(arguments, "conversationId")?;
            let parent = required_string(arguments, "messageId")?.to_owned();
            message.to = vec![address(required_string(arguments, "to")?)?];
            message.subject = answered(arguments.get("subject").and_then(Value::as_str));
            message.references = references(arguments.get("references"), &parent);
            message.in_reply_to = Some(parent);
        }
        "email.send-message" => {
            message.to = addresses(arguments.get("to"), "to")?;
            if message.to.is_empty() {
                return Err(invalid("to requires at least one recipient"));
            }
            message.cc = addresses(arguments.get("cc"), "cc")?;
            message.subject = arguments
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
        }
        other => return Err(invalid(format!("unknown capability: {other}"))),
    }
    Ok(message)
}

async fn submit(
    from: &str,
    recipients: &[String],
    message: &[u8],
    account: &Account,
) -> Result<(), Error> {
    // The client has no domain of its own, so it presents the sender's.
    let client_name = from.rsplit('@').next().unwrap_or("localhost");
    let mut session = smtp::Session::open(client_name).await?;
    let result = async {
        session
            .authenticate(&account.username, &account.password)
            .await?;
        session.submit(from, recipients, message).await
    }
    .await;
    session.quit().await;
    result
}

/// The subject an answer carries. A thread that already says `Re:` does not
/// gain a second one.
fn answered(subject: Option<&str>) -> String {
    let subject = subject.unwrap_or_default().trim();
    if subject
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("re:"))
    {
        return subject.to_owned();
    }
    format!("Re: {subject}").trim_end().to_owned()
}

/// The reference chain an answer carries: what the observation carried, then
/// the message being answered.
fn references(existing: Option<&Value>, parent: &str) -> Vec<String> {
    let mut chain: Vec<String> = existing
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if chain.last().map(String::as_str) != Some(parent) {
        chain.push(parent.to_owned());
    }
    if chain.len() > MAX_REFERENCES {
        // The first message identifies the thread; the rest that matter are
        // the most recent.
        let tail = chain.split_off(chain.len() - (MAX_REFERENCES - 1));
        chain.truncate(1);
        chain.extend(tail);
    }
    chain
}

/// A message identifier for one outgoing message. The clock and a counter are
/// the only uniqueness available here, and together they are enough: two
/// messages cannot share both.
fn message_id(from: &str, now_ms: i64, nonce: u64) -> String {
    let domain = from.rsplit('@').next().unwrap_or("localhost");
    format!("{now_ms}.{nonce}.pluribus@{domain}")
}

fn addresses(value: Option<&Value>, field: &str) -> Result<Vec<String>, Error> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| invalid(format!("{field} must be an array of addresses")))?;
    items
        .iter()
        .map(|item| {
            address(
                item.as_str()
                    .ok_or_else(|| invalid(format!("{field} must be an array of addresses")))?,
            )
        })
        .collect()
}

/// One address, lowercased in its domain and refused unless it is a bare
/// addr-spec. A display name reaching the envelope would be a second
/// recipient the caller did not name.
fn address(value: &str) -> Result<String, Error> {
    let value = value.trim();
    if !mime::valid_address(value) {
        return Err(invalid(format!("not a usable address: {value}")));
    }
    Ok(value.to_owned())
}

fn proposal(event_type: &str, payload: &Value, event: &Event) -> Proposal {
    Proposal {
        event_type: event_type.to_owned(),
        payload_schema: SCHEMA.to_owned(),
        // The payload is two strings and an object this module built, so it
        // encodes or the component is broken.
        payload: Payload::Json(serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec())),
        idempotency_key: None,
        causation_id: Some(event.event_id.clone()),
    }
}

pub fn json_payload(event: &Event) -> Result<Value, Error> {
    match &event.payload {
        Payload::Json(bytes) => serde_json::from_slice(bytes)
            .map_err(|error| invalid(format!("invalid payload: {error}"))),
        Payload::Blob(_) => Err(invalid("a capability request is not a blob")),
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid(format!("{field} is required")))
}

fn invalid(message: impl Into<String>) -> Error {
    failure(ErrorCode::InvalidArgument, message)
}

const fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidArgument => "invalid-argument",
        ErrorCode::NotFound => "not-found",
        ErrorCode::PermissionDenied => "permission-denied",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Unavailable => "unavailable",
        ErrorCode::ResourceExhausted => "resource-exhausted",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::DeadlineExceeded => "deadline-exceeded",
        ErrorCode::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        crate::setup(
            br#"{"credentials":{"account":"mail:account"},"display_name":"Ada"}"#.to_vec(),
        )
        .unwrap();
        crate::config().unwrap()
    }

    fn compose_reply(arguments: Value) -> Result<mime::Outgoing, Error> {
        compose(
            "email.reply",
            &arguments,
            &config(),
            "ada@example.com",
            1_700_000_000_000,
            1,
        )
    }

    #[test]
    fn a_reply_threads_onto_the_message_it_answers() {
        let message = compose_reply(json!({
            "conversationId": "email:INBOX",
            "to": "bob@example.net",
            "messageId": "parent@example.net",
            "subject": "Lunch",
            "references": ["root@example.net"],
            "text": "yes",
        }))
        .unwrap();
        assert_eq!(message.to, ["bob@example.net"]);
        assert_eq!(message.subject, "Re: Lunch");
        assert_eq!(message.in_reply_to.as_deref(), Some("parent@example.net"));
        assert_eq!(
            message.references,
            ["root@example.net", "parent@example.net"]
        );
    }

    #[test]
    fn a_subject_that_already_answers_is_not_answered_twice() {
        assert_eq!(answered(Some("Re: Lunch")), "Re: Lunch");
        assert_eq!(answered(Some("RE: Lunch")), "RE: Lunch");
        assert_eq!(answered(Some("Lunch")), "Re: Lunch");
        assert_eq!(answered(None), "Re:");
    }

    #[test]
    fn a_non_ascii_subject_is_prefixed_without_panicking() {
        assert_eq!(answered(Some("Привет")), "Re: Привет");
    }

    #[test]
    fn a_chain_already_ending_in_the_parent_does_not_repeat_it() {
        assert_eq!(
            references(Some(&json!(["a@x", "b@x"])), "b@x"),
            ["a@x", "b@x"]
        );
        assert_eq!(references(None, "b@x"), ["b@x"]);
    }

    /// A long thread keeps the message that started it and the ancestors
    /// closest to the reply.
    #[test]
    fn a_long_chain_is_trimmed_around_its_ends() {
        let long: Vec<Value> = (0..100).map(|n| json!(format!("m{n}@x"))).collect();
        let chain = references(Some(&Value::Array(long)), "parent@x");
        assert_eq!(chain.len(), MAX_REFERENCES);
        assert_eq!(chain[0], "m0@x");
        assert_eq!(chain[MAX_REFERENCES - 1], "parent@x");
    }

    #[test]
    fn an_address_that_could_end_a_header_or_a_command_is_refused() {
        for bad in [
            "Bob <bob@example.net>",
            "bob@example.net, eve@example.net",
            "bob@example.net>\r\nRCPT TO:<eve@example.net",
            "bob",
            "bob@localhost",
            "@example.net",
        ] {
            assert!(
                compose_reply(json!({
                    "conversationId": "email:INBOX",
                    "to": bad,
                    "messageId": "p@x.net",
                    "text": "hi",
                }))
                .is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_reply_requires_the_message_it_answers() {
        for missing in ["conversationId", "messageId", "to", "text"] {
            let mut arguments = json!({
                "conversationId": "email:INBOX",
                "to": "bob@example.net",
                "messageId": "p@x.net",
                "text": "hi",
            });
            arguments.as_object_mut().unwrap().remove(missing);
            assert!(compose_reply(arguments).is_err(), "{missing} is required");
        }
    }

    #[test]
    fn a_send_names_its_recipients_and_carries_no_thread() {
        let message = compose(
            "email.send-message",
            &json!({
                "to": ["bob@example.net"],
                "cc": ["carol@example.net"],
                "subject": "Notes",
                "text": "body",
            }),
            &config(),
            "ada@example.com",
            1_700_000_000_000,
            7,
        )
        .unwrap();
        assert_eq!(message.subject, "Notes");
        assert!(message.in_reply_to.is_none());
        assert!(message.references.is_empty());
        assert_eq!(
            message.recipients(),
            ["bob@example.net", "carol@example.net"]
        );
        assert!(message.message_id.ends_with("@example.com"));
        assert!(
            compose(
                "email.send-message",
                &json!({"to": [], "text": "body"}),
                &config(),
                "ada@example.com",
                0,
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn two_messages_in_one_millisecond_get_different_identifiers() {
        assert_ne!(
            message_id("a@b.com", 1, nonce()),
            message_id("a@b.com", 1, nonce())
        );
    }
}
