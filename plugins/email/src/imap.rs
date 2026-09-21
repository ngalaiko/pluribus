//! The IMAP commands this connector needs, and the responses they return.
//!
//! Read-only throughout: `EXAMINE` opens the mailbox without write access and
//! `BODY.PEEK` leaves `\Seen` alone. That is a property of these commands, not
//! of the grant — the credential permits mutation and nothing here performs
//! one. See the README.

use crate::pluribus::plugin::types::{Error, ErrorCode};
use crate::wire::{Completion, Connection, Response, Status, failure, unavailable};

/// What the server said about the mailbox when it was opened.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Mailbox {
    pub uid_validity: u64,
    /// The UID the next arriving message will be given. The boundary a first
    /// connection records is one below it.
    pub uid_next: u64,
    pub exists: u64,
}

/// One message as the server returned it.
pub struct Fetched {
    pub uid: u64,
    pub internal_date: Option<String>,
    pub size: u64,
    /// The message bytes: the whole message, or its header alone when the
    /// message exceeded the configured ceiling.
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

/// Completes the server greeting.
pub async fn greet(connection: &mut Connection) -> Result<(), Error> {
    let Some(response) = connection.read(None).await? else {
        return Err(unavailable("endpoint sent no greeting"));
    };
    let text = response.text();
    if text.starts_with("* OK") {
        return Ok(());
    }
    if text.starts_with("* PREAUTH") {
        return Err(failure(
            ErrorCode::PermissionDenied,
            "endpoint pre-authenticated a session this connector did not open",
        ));
    }
    Err(unavailable(format!("endpoint refused the session: {text}")))
}

pub async fn capabilities(connection: &mut Connection) -> Result<Vec<String>, Error> {
    let tag = connection.next_tag();
    connection.send_line(&format!("{tag} CAPABILITY")).await?;
    let completion = connection.read_until_tagged(&tag).await?;
    require_ok(&completion, "CAPABILITY")?;
    Ok(completion
        .untagged
        .iter()
        .filter_map(|response| {
            response.text().strip_prefix("* CAPABILITY ").map(|list| {
                list.split(' ')
                    .map(str::to_ascii_uppercase)
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect())
}

/// Authenticates with a username and password.
///
/// The credential reaches the wire as a quoted string or a literal, never
/// spliced raw: a password containing a quote or a newline must not be able to
/// become part of the command.
pub async fn login(connection: &mut Connection, user: &str, password: &str) -> Result<(), Error> {
    if user.is_empty() || password.is_empty() {
        return Err(failure(
            ErrorCode::InvalidArgument,
            "credential is missing a username or password",
        ));
    }
    let tag = connection.next_tag();
    connection
        .send_line(&format!(
            "{tag} LOGIN {} {}",
            quoted(user)?,
            quoted(password)?
        ))
        .await?;
    let completion = connection.read_until_tagged(&tag).await?;
    match completion.status {
        Status::Ok => Ok(()),
        // A rejected password is rejected on every retry. Saying so keeps the
        // reconnect loop from hammering the endpoint with a bad credential.
        Status::No | Status::Bad => Err(failure(
            ErrorCode::PermissionDenied,
            "endpoint rejected the credential",
        )),
    }
}

/// Opens a mailbox read-only.
pub async fn examine(connection: &mut Connection, mailbox: &str) -> Result<Mailbox, Error> {
    let tag = connection.next_tag();
    connection
        .send_line(&format!("{tag} EXAMINE {}", quoted(mailbox)?))
        .await?;
    let completion = connection.read_until_tagged(&tag).await?;
    require_ok(&completion, "EXAMINE")?;
    let mut state = Mailbox::default();
    for response in &completion.untagged {
        let text = response.text();
        if let Some(value) = bracketed(&text, "UIDVALIDITY") {
            state.uid_validity = value;
        }
        if let Some(value) = bracketed(&text, "UIDNEXT") {
            state.uid_next = value;
        }
        if let Some(count) = text
            .strip_prefix("* ")
            .and_then(|rest| rest.strip_suffix(" EXISTS"))
            .and_then(|count| count.parse().ok())
        {
            state.exists = count;
        }
    }
    if state.uid_validity == 0 {
        return Err(unavailable("endpoint reported no UIDVALIDITY"));
    }
    if state.uid_next == 0 {
        // Servers may omit UIDNEXT; one past the highest UID is equivalent for
        // the boundary this connector records.
        state.uid_next = highest_uid(connection).await?.saturating_add(1);
    }
    Ok(state)
}

/// The UIDs above `after`, ascending, at most `limit` of them.
pub async fn search_since(
    connection: &mut Connection,
    after: u64,
    limit: usize,
) -> Result<Vec<u64>, Error> {
    let tag = connection.next_tag();
    connection
        .send_line(&format!(
            "{tag} UID SEARCH UID {}:*",
            after.saturating_add(1)
        ))
        .await?;
    let completion = connection.read_until_tagged(&tag).await?;
    require_ok(&completion, "UID SEARCH")?;
    let mut uids: Vec<u64> = completion
        .untagged
        .iter()
        .filter_map(|response| {
            let text = response.text();
            let rest = text.strip_prefix("* SEARCH")?;
            Some(
                rest.split_whitespace()
                    .filter_map(|uid| uid.parse::<u64>().ok())
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        // `UID n:*` always returns at least one UID even when none is above
        // `after`, because the range is anchored at the highest existing UID.
        .filter(|uid| *uid > after)
        .collect();
    uids.sort_unstable();
    uids.dedup();
    uids.truncate(limit);
    Ok(uids)
}

/// Fetches one message, its header alone when it exceeds `max_bytes`.
pub async fn fetch(
    connection: &mut Connection,
    uid: u64,
    max_bytes: u64,
) -> Result<Option<Fetched>, Error> {
    let tag = connection.next_tag();
    connection
        .send_line(&format!("{tag} UID FETCH {uid} (UID RFC822.SIZE)"))
        .await?;
    let completion = connection.read_until_tagged(&tag).await?;
    require_ok(&completion, "UID FETCH")?;
    let Some(size) = completion
        .untagged
        .iter()
        .filter_map(|response| parse_fetch(response).ok().flatten())
        .find(|items| items.uid == Some(uid))
        .and_then(|items| items.size)
    else {
        // Expunged between the search and the fetch. Nothing to observe.
        return Ok(None);
    };
    let truncated = size > max_bytes;
    let section = if truncated { "HEADER" } else { "" };
    let tag = connection.next_tag();
    connection
        .send_line(&format!(
            "{tag} UID FETCH {uid} (UID INTERNALDATE BODY.PEEK[{section}])"
        ))
        .await?;
    let completion = connection.read_until_tagged(&tag).await?;
    require_ok(&completion, "UID FETCH")?;
    let Some(items) = completion
        .untagged
        .iter()
        .filter_map(|response| parse_fetch(response).ok().flatten())
        .find(|items| items.uid == Some(uid))
    else {
        return Ok(None);
    };
    Ok(Some(Fetched {
        uid,
        internal_date: items.internal_date,
        size,
        bytes: items.body.unwrap_or_default(),
        truncated,
    }))
}

/// Enters IDLE. The server answers with a continuation before it will push.
pub async fn idle_begin(connection: &mut Connection) -> Result<String, Error> {
    let tag = connection.next_tag();
    connection.send_line(&format!("{tag} IDLE")).await?;
    loop {
        let Some(response) = connection.read(None).await? else {
            return Err(unavailable("endpoint closed before accepting IDLE"));
        };
        if response.is_continuation() {
            return Ok(tag);
        }
        if response.text().starts_with(&tag) {
            return Err(unavailable("endpoint refused IDLE"));
        }
    }
}

/// Leaves IDLE and drains the command's completion.
pub async fn idle_end(connection: &mut Connection, tag: &str) -> Result<(), Error> {
    connection.send_line("DONE").await?;
    let completion = connection.read_until_tagged(tag).await?;
    require_ok(&completion, "IDLE")
}

/// Whether a pushed response means the mailbox gained something to look at.
///
/// `EXISTS` and `RECENT` announce arrivals. `EXPUNGE` and `FETCH` describe
/// messages this connector has already passed, and a read-only session has
/// nothing to do about either.
pub fn announces_arrival(response: &Response) -> bool {
    let text = response.text();
    let Some(rest) = text.strip_prefix("* ") else {
        return false;
    };
    let mut words = rest.split(' ');
    let (Some(count), Some(kind)) = (words.next(), words.next()) else {
        return false;
    };
    count.parse::<u64>().is_ok() && matches!(kind, "EXISTS" | "RECENT")
}

/// The highest UID in the mailbox, or zero when it is empty.
async fn highest_uid(connection: &mut Connection) -> Result<u64, Error> {
    let tag = connection.next_tag();
    connection
        .send_line(&format!("{tag} UID SEARCH ALL"))
        .await?;
    let completion = connection.read_until_tagged(&tag).await?;
    require_ok(&completion, "UID SEARCH")?;
    Ok(completion
        .untagged
        .iter()
        .filter_map(|response| {
            let text = response.text();
            let rest = text.strip_prefix("* SEARCH")?;
            rest.split_whitespace()
                .filter_map(|uid| uid.parse::<u64>().ok())
                .max()
        })
        .max()
        .unwrap_or(0))
}

fn require_ok(completion: &Completion, command: &str) -> Result<(), Error> {
    match completion.status {
        Status::Ok => Ok(()),
        Status::No => Err(unavailable(format!("endpoint refused {command}"))),
        Status::Bad => Err(failure(
            ErrorCode::Internal,
            format!("endpoint rejected {command} as malformed"),
        )),
    }
}

/// The unsigned value of a response code such as `[UIDVALIDITY 1234]`.
fn bracketed(text: &str, code: &str) -> Option<u64> {
    let start = text.find(&format!("[{code} "))? + code.len() + 2;
    let rest = &text[start..];
    let end = rest.find(']')?;
    rest[..end].trim().parse().ok()
}

/// A string safe to place in a command. Anything a quoted string cannot
/// carry is refused rather than escaped into something else.
fn quoted(value: &str) -> Result<String, Error> {
    if value.contains(['\r', '\n']) || value.contains('\0') {
        return Err(failure(
            ErrorCode::InvalidArgument,
            "value cannot appear in an IMAP command",
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// The fields of one FETCH response this connector reads.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FetchItems {
    pub uid: Option<u64>,
    pub size: Option<u64>,
    pub internal_date: Option<String>,
    pub body: Option<Vec<u8>>,
}

/// Parses `* <n> FETCH (...)`. Returns `None` for any other response.
pub fn parse_fetch(response: &Response) -> Result<Option<FetchItems>, Error> {
    let bytes = &response.bytes;
    let Some(rest) = bytes.strip_prefix(b"* ") else {
        return Ok(None);
    };
    let Some(open) = rest.iter().position(|byte| *byte == b'(') else {
        return Ok(None);
    };
    if !String::from_utf8_lossy(&rest[..open])
        .to_ascii_uppercase()
        .contains("FETCH")
    {
        return Ok(None);
    }
    let Token::List(items) = parse_list(&rest[open..])? else {
        return Ok(None);
    };
    let mut fetched = FetchItems::default();
    let mut index = 0;
    while index < items.len() {
        let Token::Atom(name) = &items[index] else {
            index += 1;
            continue;
        };
        let name = name.to_ascii_uppercase();
        let value = items.get(index + 1);
        match (name.as_str(), value) {
            ("UID", Some(Token::Atom(value))) => fetched.uid = value.parse().ok(),
            ("RFC822.SIZE", Some(Token::Atom(value))) => fetched.size = value.parse().ok(),
            ("INTERNALDATE", Some(Token::Quoted(value))) => {
                fetched.internal_date = Some(value.clone());
            }
            (name, Some(token)) if name.starts_with("BODY[") || name == "RFC822" => {
                fetched.body = Some(match token {
                    Token::Literal(bytes) => bytes.clone(),
                    Token::Quoted(text) | Token::Atom(text) => text.as_bytes().to_vec(),
                    Token::List(_) => Vec::new(),
                });
            }
            _ => {}
        }
        index += 2;
    }
    Ok(Some(fetched))
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Atom(String),
    Quoted(String),
    Literal(Vec<u8>),
    List(Vec<Token>),
}

/// Parses one parenthesised list starting at `bytes[0] == b'('`.
///
/// A literal appears as its `{n}` marker followed by exactly `n` octets, the
/// shape the framing layer preserves.
fn parse_list(bytes: &[u8]) -> Result<Token, Error> {
    let mut cursor = 0;
    let token = parse_token(bytes, &mut cursor)?;
    Ok(token)
}

fn parse_token(bytes: &[u8], cursor: &mut usize) -> Result<Token, Error> {
    skip_spaces(bytes, cursor);
    match bytes.get(*cursor) {
        Some(b'(') => {
            *cursor += 1;
            let mut items = Vec::new();
            loop {
                skip_spaces(bytes, cursor);
                match bytes.get(*cursor) {
                    Some(b')') => {
                        *cursor += 1;
                        return Ok(Token::List(items));
                    }
                    None => return Err(unavailable("endpoint sent an unterminated list")),
                    Some(_) => items.push(parse_token(bytes, cursor)?),
                }
            }
        }
        Some(b'"') => {
            *cursor += 1;
            let mut text = String::new();
            loop {
                match bytes.get(*cursor) {
                    Some(b'\\') => {
                        *cursor += 1;
                        if let Some(byte) = bytes.get(*cursor) {
                            text.push(char::from(*byte));
                            *cursor += 1;
                        }
                    }
                    Some(b'"') => {
                        *cursor += 1;
                        return Ok(Token::Quoted(text));
                    }
                    Some(byte) => {
                        text.push(char::from(*byte));
                        *cursor += 1;
                    }
                    None => return Err(unavailable("endpoint sent an unterminated string")),
                }
            }
        }
        Some(b'{') => {
            let close = bytes[*cursor..]
                .iter()
                .position(|byte| *byte == b'}')
                .ok_or_else(|| unavailable("endpoint sent an unterminated literal"))?;
            let digits = &bytes[*cursor + 1..*cursor + close];
            let digits = digits.strip_suffix(b"+").unwrap_or(digits);
            let length: usize = std::str::from_utf8(digits)
                .ok()
                .and_then(|digits| digits.parse().ok())
                .ok_or_else(|| unavailable("endpoint sent a malformed literal length"))?;
            let start = *cursor + close + 1;
            let end = start
                .checked_add(length)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(|| unavailable("endpoint sent a truncated literal"))?;
            *cursor = end;
            Ok(Token::Literal(bytes[start..end].to_vec()))
        }
        Some(_) => {
            let start = *cursor;
            while let Some(byte) = bytes.get(*cursor) {
                if matches!(byte, b' ' | b'(' | b')' | b'"' | b'{' | b'\r' | b'\n') {
                    break;
                }
                *cursor += 1;
            }
            Ok(Token::Atom(
                String::from_utf8_lossy(&bytes[start..*cursor]).into_owned(),
            ))
        }
        None => Err(unavailable("endpoint sent an empty response")),
    }
}

fn skip_spaces(bytes: &[u8], cursor: &mut usize) {
    while bytes.get(*cursor) == Some(&b' ') {
        *cursor += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(bytes: &[u8]) -> Response {
        Response {
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn response_codes_are_read_from_anywhere_in_the_line() {
        assert_eq!(
            bracketed("* OK [UIDVALIDITY 1234] UIDs valid", "UIDVALIDITY"),
            Some(1234)
        );
        assert_eq!(bracketed("* OK [UIDNEXT 9]", "UIDNEXT"), Some(9));
        assert_eq!(bracketed("* OK [UIDNEXT 9]", "UIDVALIDITY"), None);
        // A prefix must not match: UIDNEXT is not UIDVALIDITY.
        assert_eq!(bracketed("* OK [UIDNEXTISH 9]", "UIDNEXT"), None);
    }

    #[test]
    fn a_fetch_with_a_literal_body_keeps_its_exact_bytes() {
        let mut bytes =
            b"* 1 FETCH (UID 42 INTERNALDATE \"01-Jan-2026 00:00:00 +0000\" BODY[] {12}".to_vec();
        bytes.extend_from_slice(b"Subject: hi\n");
        bytes.extend_from_slice(b")");
        let items = parse_fetch(&response(&bytes)).unwrap().unwrap();
        assert_eq!(items.uid, Some(42));
        assert_eq!(
            items.internal_date.as_deref(),
            Some("01-Jan-2026 00:00:00 +0000")
        );
        assert_eq!(items.body.as_deref(), Some(&b"Subject: hi\n"[..]));
    }

    /// A body whose bytes contain a closing paren, a quote, or a brace must
    /// not end the response early: the literal length is the only boundary.
    #[test]
    fn a_literal_body_is_delimited_by_its_length_alone() {
        let body = b") \"quoted\" {9} trailing";
        let mut bytes = format!("* 1 FETCH (UID 7 BODY[] {{{}}}", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b")");
        let items = parse_fetch(&response(&bytes)).unwrap().unwrap();
        assert_eq!(items.uid, Some(7));
        assert_eq!(items.body.as_deref(), Some(&body[..]));
    }

    #[test]
    fn sizes_and_header_sections_are_read() {
        let items = parse_fetch(&response(b"* 3 FETCH (UID 9 RFC822.SIZE 4096)"))
            .unwrap()
            .unwrap();
        assert_eq!(items.uid, Some(9));
        assert_eq!(items.size, Some(4096));
        assert!(items.body.is_none());
        let mut header = b"* 3 FETCH (UID 9 BODY[HEADER] {5}".to_vec();
        header.extend_from_slice(b"From:");
        header.push(b')');
        assert_eq!(
            parse_fetch(&response(&header)).unwrap().unwrap().body,
            Some(b"From:".to_vec())
        );
    }

    #[test]
    fn other_untagged_responses_are_not_fetches() {
        assert!(parse_fetch(&response(b"* 4 EXISTS")).unwrap().is_none());
        assert!(
            parse_fetch(&response(b"* OK [UIDNEXT 9]"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn only_arrivals_wake_the_loop() {
        assert!(announces_arrival(&response(b"* 4 EXISTS")));
        assert!(announces_arrival(&response(b"* 1 RECENT")));
        assert!(!announces_arrival(&response(b"* 4 EXPUNGE")));
        assert!(!announces_arrival(&response(b"* 4 FETCH (FLAGS (\\Seen))")));
        assert!(!announces_arrival(&response(b"* OK still here")));
    }

    #[test]
    fn credentials_are_quoted_and_control_characters_refused() {
        assert_eq!(quoted("user@example.com").unwrap(), "\"user@example.com\"");
        assert_eq!(quoted("pa\"ss\\word").unwrap(), "\"pa\\\"ss\\\\word\"");
        assert!(quoted("bad\r\nLOGOUT").is_err());
        assert!(quoted("bad\0").is_err());
    }
}
