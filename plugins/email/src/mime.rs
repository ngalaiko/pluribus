//! Enough of RFC 5322 and MIME to turn a fetched message into an observation,
//! and to turn an outgoing one into bytes.
//!
//! Every field read here is attacker-controlled: a message is written by
//! whoever sent it. Nothing on the reading side trusts a declared length, a
//! declared charset, or a boundary appearing where it should. Malformed input
//! yields a poorer observation, never a failure to observe.
//!
//! The writing side is the mirror: a header value is encoded rather than
//! trusted, so nothing a caller supplies can become a header of its own.

use base64::Engine as _;

/// Ceiling on one decoded text body kept in an observation.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Ceiling on parts walked in one message, so a deeply nested or wide
/// multipart cannot cost unbounded work.
const MAX_PARTS: usize = 64;
const MAX_DEPTH: usize = 8;

/// A parsed message.
#[derive(Debug, Default)]
pub struct Message {
    pub headers: Headers,
    /// The best text body found, decoded and bounded.
    pub text: String,
    /// Whether `text` came from an HTML part with its tags stripped.
    pub text_from_html: bool,
    pub attachments: Vec<Attachment>,
}

/// One non-text part, described but not decoded.
#[derive(Debug)]
pub struct Attachment {
    pub file_name: Option<String>,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// Header fields, in order, with names lowercased.
#[derive(Debug, Default)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// The first value for `name`, decoded from any encoded words.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| decode_words(value))
    }

    /// Every value for `name`, decoded. Addresses are not split here: one
    /// header may list many, and splitting them needs the address grammar.
    #[must_use]
    pub fn all(&self, name: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(field, _)| field == name)
            .map(|(_, value)| decode_words(value))
            .collect()
    }

    fn raw(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Parses a message, or its header alone when the body was not fetched.
#[must_use]
pub fn parse(bytes: &[u8]) -> Message {
    let (headers, body) = split(bytes);
    let headers = parse_headers(headers);
    let mut message = Message::default();
    let mut budget = MAX_PARTS;
    walk(&headers, body, 0, &mut budget, &mut message);
    message.headers = headers;
    if message.text.len() > MAX_BODY_BYTES {
        truncate_utf8(&mut message.text, MAX_BODY_BYTES);
    }
    message
}

/// Splits a part into its header block and its body at the first blank line.
fn split(bytes: &[u8]) -> (&[u8], &[u8]) {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"\r\n\r\n") {
            return (&bytes[..index], &bytes[index + 4..]);
        }
        if bytes[index..].starts_with(b"\n\n") {
            return (&bytes[..index], &bytes[index + 2..]);
        }
        index += 1;
    }
    (bytes, &[])
}

/// Unfolds continuation lines and splits each field at its first colon.
fn parse_headers(bytes: &[u8]) -> Headers {
    let text = String::from_utf8_lossy(bytes);
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim_start());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            fields.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Headers(fields)
}

/// Descends into a part, collecting the best text body and every attachment.
fn walk(headers: &Headers, body: &[u8], depth: usize, budget: &mut usize, out: &mut Message) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    *budget -= 1;
    let content_type = headers.raw("content-type").unwrap_or("text/plain");
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("text/plain")
        .trim()
        .to_ascii_lowercase();
    if let Some(boundary) = parameter(content_type, "boundary")
        && media_type.starts_with("multipart/")
    {
        for part in split_multipart(body, &boundary) {
            let (part_headers, part_body) = split(part);
            let part_headers = parse_headers(part_headers);
            walk(&part_headers, part_body, depth + 1, budget, out);
        }
        return;
    }
    let decoded = decode_transfer(headers.raw("content-transfer-encoding"), body);
    let disposition = headers
        .raw("content-disposition")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let attached = disposition.starts_with("attachment")
        || (!media_type.starts_with("text/") && !media_type.starts_with("multipart/"));
    if attached {
        out.attachments.push(Attachment {
            file_name: parameter(headers.raw("content-disposition").unwrap_or(""), "filename")
                .or_else(|| parameter(content_type, "name"))
                .map(|name| decode_words(&name)),
            media_type: if media_type.is_empty() {
                "application/octet-stream".into()
            } else {
                media_type
            },
            bytes: decoded,
        });
        return;
    }
    let charset = parameter(content_type, "charset").unwrap_or_else(|| "utf-8".into());
    let text = decode_text(&decoded, &charset);
    // A plain part always wins; an HTML part stands in only until one appears.
    if media_type == "text/html" {
        if out.text.is_empty() {
            out.text = strip_html(&text);
            out.text_from_html = true;
        }
    } else if out.text.is_empty() || out.text_from_html {
        out.text = text;
        out.text_from_html = false;
    }
}

/// The parts between `--boundary` delimiters.
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delimiter = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut offset = 0;
    let mut start: Option<usize> = None;
    while offset < body.len() {
        let at_line_start = offset == 0 || body[offset - 1] == b'\n';
        if at_line_start && body[offset..].starts_with(&delimiter) {
            if let Some(from) = start {
                parts.push(trim_crlf(&body[from..offset]));
            }
            let after = offset + delimiter.len();
            if body[after..].starts_with(b"--") {
                return parts;
            }
            start = Some(skip_line(body, after));
            offset = start.unwrap();
            continue;
        }
        offset += 1;
    }
    if let Some(from) = start.filter(|from| *from < body.len()) {
        parts.push(trim_crlf(&body[from..]));
    }
    parts
}

fn skip_line(body: &[u8], from: usize) -> usize {
    body[from..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(body.len(), |index| from + index + 1)
}

fn trim_crlf(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes)
}

/// A `name=value` parameter from a structured header, quoted or bare.
fn parameter(header: &str, name: &str) -> Option<String> {
    let lowered = header.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lowered[search..].find(name) {
        let at = search + found;
        let before_ok = at == 0
            || lowered[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c == ';' || c == ' ' || c == '\t');
        let rest = &header[at + name.len()..];
        let trimmed = rest.trim_start();
        // RFC 2231 continuations are not decoded; the unadorned form is taken.
        if before_ok && let Some(value) = trimmed.strip_prefix('=') {
            let value = value.trim_start();
            return Some(if let Some(quoted) = value.strip_prefix('"') {
                quoted
                    .find('"')
                    .map_or_else(|| quoted.to_owned(), |end| quoted[..end].to_owned())
            } else {
                value
                    .split([';', ' ', '\t'])
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            });
        }
        search = at + name.len();
    }
    None
}

/// Reverses the content transfer encoding. An undecodable body is kept as it
/// arrived rather than dropped.
fn decode_transfer(encoding: Option<&str>, body: &[u8]) -> Vec<u8> {
    match encoding
        .unwrap_or("7bit")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "base64" => {
            let compact: Vec<u8> = body
                .iter()
                .copied()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(&compact)
                .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&compact))
                .unwrap_or_else(|_| body.to_vec())
        }
        "quoted-printable" => decode_quoted_printable(body),
        _ => body.to_vec(),
    }
}

fn decode_quoted_printable(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut index = 0;
    while index < body.len() {
        if body[index] != b'=' {
            out.push(body[index]);
            index += 1;
            continue;
        }
        // A soft break: `=` at end of line joins the next one.
        if body[index..].starts_with(b"=\r\n") {
            index += 3;
            continue;
        }
        if body[index..].starts_with(b"=\n") {
            index += 2;
            continue;
        }
        match body
            .get(index + 1..index + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(body[index]);
                index += 1;
            }
        }
    }
    out
}

/// Decodes body bytes to text. UTF-8 and the Latin-1 family are handled;
/// anything else is read lossily as UTF-8, which keeps ASCII intact.
fn decode_text(bytes: &[u8], charset: &str) -> String {
    match charset.trim().to_ascii_lowercase().as_str() {
        "iso-8859-1" | "latin1" | "iso8859-1" | "windows-1252" | "us-ascii" | "ascii" => {
            bytes.iter().map(|byte| char::from(*byte)).collect()
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Decodes RFC 2047 encoded words in a header value.
fn decode_words(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    // Whitespace between two adjacent encoded words is not part of the text.
    let mut previous_was_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, tail) = rest.split_at(start);
        if !(previous_was_word && before.trim().is_empty()) {
            out.push_str(before);
        }
        let Some(word) = encoded_word(tail) else {
            out.push_str("=?");
            rest = &tail[2..];
            previous_was_word = false;
            continue;
        };
        out.push_str(&word.text);
        rest = &tail[word.length..];
        previous_was_word = true;
    }
    out.push_str(rest);
    out
}

struct EncodedWord {
    text: String,
    length: usize,
}

/// Decodes one `=?charset?encoding?text?=` at the start of `tail`.
fn encoded_word(tail: &str) -> Option<EncodedWord> {
    let end = tail.find("?=")? + 2;
    let inner = &tail[2..end - 2];
    let mut fields = inner.splitn(3, '?');
    let charset = fields.next()?;
    let encoding = fields.next()?;
    let payload = fields.next()?;
    // A `?` inside the payload would have ended the word early; reject rather
    // than decode a fragment.
    if payload.contains('?') {
        return None;
    }
    let bytes = match encoding.to_ascii_uppercase().as_str() {
        "B" => base64::engine::general_purpose::STANDARD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
            .ok()?,
        // In an encoded word, `_` stands for a space.
        "Q" => decode_quoted_printable(payload.replace('_', " ").as_bytes()),
        _ => return None,
    };
    Some(EncodedWord {
        text: decode_text(&bytes, charset),
        length: end,
    })
}

/// Reduces HTML to its text. Not a parser: a body is not a document to be
/// rendered, only a string a person might read.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0usize;
    let mut skipping: Option<&str> = None;
    let mut rest = html;
    while !rest.is_empty() {
        if let Some(tag) = skipping {
            let close = format!("</{tag}");
            match rest.to_ascii_lowercase().find(&close) {
                Some(at) => {
                    rest = &rest[at..];
                    skipping = None;
                }
                None => break,
            }
            continue;
        }
        let Some(at) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let lowered = rest.to_ascii_lowercase();
        for tag in ["script", "style"] {
            if lowered.starts_with(&format!("<{tag}")) {
                skipping = Some(tag);
            }
        }
        if skipping.is_some() {
            continue;
        }
        depth += 1;
        match rest.find('>') {
            Some(close) => {
                if matches!(
                    lowered.get(..3),
                    Some("<br") | Some("</p") | Some("<p>") | Some("<tr")
                ) {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                rest = &rest[close + 1..];
            }
            None => break,
        }
        if depth > 100_000 {
            break;
        }
    }
    collapse(&out)
}

/// Collapses runs of whitespace, keeping paragraph breaks.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    let mut spaced = false;
    for character in text.chars() {
        if character == '\n' {
            newlines += 1;
            spaced = true;
            continue;
        }
        if character.is_whitespace() {
            spaced = true;
            continue;
        }
        if newlines > 0 {
            out.push_str(if newlines > 1 { "\n\n" } else { "\n" });
        } else if spaced && !out.is_empty() {
            out.push(' ');
        }
        newlines = 0;
        spaced = false;
        out.push(character);
    }
    out
}

/// One message to be written out.
#[derive(Debug, Default)]
pub struct Outgoing {
    pub message_id: String,
    /// The `Date` header, already formatted. See `rfc5322_date`.
    pub date: String,
    pub from: String,
    pub display_name: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub text: String,
}

impl Outgoing {
    /// The envelope recipients, in the order they are offered to the endpoint.
    #[must_use]
    pub fn recipients(&self) -> Vec<String> {
        self.to.iter().chain(&self.cc).cloned().collect()
    }
}

/// Renders a message as RFC 5322, CRLF throughout.
///
/// Header values are encoded, never interpolated: a value carrying a newline
/// would otherwise become a header the caller did not ask for.
#[must_use]
pub fn write(message: &Outgoing) -> Result<Vec<u8>, WriteError> {
    let mut out = String::new();
    out.push_str(&fold("Date", &message.date)?);
    out.push_str(&fold(
        "From",
        &mailbox(message.display_name.as_deref(), &message.from),
    )?);
    out.push_str(&fold("To", &address_list(&message.to))?);
    if !message.cc.is_empty() {
        out.push_str(&fold("Cc", &address_list(&message.cc))?);
    }
    out.push_str(&fold("Subject", &encode_header(&message.subject))?);
    out.push_str(&fold("Message-Id", &angle(&message.message_id))?);
    if let Some(parent) = &message.in_reply_to {
        out.push_str(&fold("In-Reply-To", &angle(parent))?);
    }
    if !message.references.is_empty() {
        let chain: Vec<String> = message.references.iter().map(|id| angle(id)).collect();
        out.push_str(&fold("References", &chain.join(" "))?);
    }
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=\"utf-8\"\r\n");
    let seven_bit = is_seven_bit(&message.text);
    out.push_str(if seven_bit {
        "Content-Transfer-Encoding: 7bit\r\n"
    } else {
        "Content-Transfer-Encoding: quoted-printable\r\n"
    });
    out.push_str("\r\n");
    out.push_str(&if seven_bit {
        crlf(&message.text)
    } else {
        quoted_printable(&message.text)
    });
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    Ok(out.into_bytes())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteError {
    HeaderTooLong,
}

/// Whether a body can travel as it stands: printable ASCII, short lines, no
/// trailing whitespace a transfer agent would strip.
fn is_seven_bit(text: &str) -> bool {
    text.split('\n').all(|line| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        line.len() <= 78
            && !line.ends_with([' ', '\t'])
            && line
                .bytes()
                .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
    })
}

/// Normalises line endings to CRLF.
fn crlf(text: &str) -> String {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Encodes a body as quoted-printable with CRLF endings.
///
/// Lines stay inside the 76-character limit RFC 2045 sets, and a `=XX`
/// escape is never split across a soft break.
#[must_use]
pub fn quoted_printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        encode_line(line.strip_suffix('\r').unwrap_or(line), &mut out);
    }
    out
}

fn encode_line(line: &str, out: &mut String) {
    let bytes = line.as_bytes();
    let mut column = 0;
    for (index, byte) in bytes.iter().enumerate() {
        let last = index + 1 == bytes.len();
        let escaped = match byte {
            // Trailing whitespace is stripped in transit unless it is encoded.
            b' ' | b'\t' => last,
            b'=' => true,
            0x21..=0x7e => false,
            _ => true,
        };
        let width = if escaped { 3 } else { 1 };
        // 76 includes the trailing `=` a soft break adds.
        if column + width > 75 {
            out.push_str("=\r\n");
            column = 0;
        }
        if escaped {
            out.push_str(&format!("={byte:02X}"));
        } else {
            out.push(char::from(*byte));
        }
        column += width;
    }
}

/// A header value safe to place in a header: ASCII as it stands, anything
/// else as RFC 2047 encoded words.
#[must_use]
pub fn encode_header(value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|character| !matches!(character, '\r' | '\n'))
        .collect();
    if value.is_ascii() {
        return value;
    }
    // An encoded word is at most 75 characters including its delimiters, so
    // each carries at most 45 source bytes of base64.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for character in value.chars() {
        if chunk.len() + character.len_utf8() > 45 {
            words.push(encode_word(&chunk));
            chunk.clear();
        }
        chunk.push(character);
    }
    if !chunk.is_empty() {
        words.push(encode_word(&chunk));
    }
    words.join(" ")
}

/// One RFC 2047 encoded word around `text`.
fn encode_word(text: &str) -> String {
    format!(
        "=?utf-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    )
}

/// `Display Name <address>`, or the bare address when there is no name.
fn mailbox(display_name: Option<&str>, address: &str) -> String {
    match display_name.filter(|name| !name.trim().is_empty()) {
        None => address.to_owned(),
        Some(name) if name.is_ascii() => {
            format!("\"{}\" <{address}>", escape_quoted(name))
        }
        Some(name) => format!("{} <{address}>", encode_header(name)),
    }
}

/// Escapes a display name for a quoted string, and drops what one cannot
/// carry rather than letting it end the string early.
fn escape_quoted(name: &str) -> String {
    name.chars()
        .filter(|character| !matches!(character, '\r' | '\n'))
        .flat_map(|character| match character {
            '"' | '\\' => vec!['\\', character],
            other => vec![other],
        })
        .collect()
}

fn address_list(addresses: &[String]) -> String {
    addresses.join(", ")
}

/// A message identifier in angle brackets, whichever form it arrived in.
fn angle(id: &str) -> String {
    let trimmed = id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    let cleaned: String = trimmed
        .chars()
        .filter(|character| {
            !character.is_whitespace() && !matches!(character, '<' | '>' | '\r' | '\n')
        })
        .collect();
    format!("<{cleaned}>")
}

/// Folds one header onto continuation lines at spaces, RFC 5322 §2.2.3.
///
/// A token longer than the transport limit is rejected: an encoded word or a
/// message identifier means something different once it is split. A run of
/// spaces yields empty tokens and so survives: unfolding restores what was
/// written.
fn fold(name: &str, value: &str) -> Result<String, WriteError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(format!("{name}:\r\n"));
    }
    let mut out = format!("{name}:");
    let mut column = out.len();
    for token in value.split(' ') {
        if token.len() > 997 || (column == name.len() + 1 && column + 1 + token.len() > 998) {
            return Err(WriteError::HeaderTooLong);
        }
        if column + 1 + token.len() > 78 && column > name.len() + 1 {
            out.push_str("\r\n ");
            column = 1;
        } else {
            out.push(' ');
            column += 1;
        }
        out.push_str(token);
        column += token.len();
    }
    out.push_str("\r\n");
    Ok(out)
}

/// Whether a string is one addr-spec this connector will put on the wire.
///
/// Deliberately narrow: no display names, no groups, no quoted local parts.
/// A caller's address becomes both a header and an SMTP command, so anything
/// that could end either early is refused rather than escaped.
#[must_use]
pub fn valid_address(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    if domain.contains('@') || value.len() > 320 || local.is_empty() || domain.len() < 3 {
        return false;
    }
    !value.chars().any(|character| {
        character.is_whitespace()
            || character.is_control()
            || !character.is_ascii()
            || matches!(
                character,
                '<' | '>' | ',' | ';' | ':' | '"' | '\\' | '(' | ')'
            )
    }) && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// The `Date` header for an epoch millisecond, always in UTC.
///
/// The host clock is the only calendar available here, and a connector has no
/// reason to claim a local zone it cannot know.
#[must_use]
pub fn rfc5322_date(ms: i64) -> String {
    const DAY_MS: i64 = 86_400_000;
    let days = ms.div_euclid(DAY_MS);
    let rest = ms.rem_euclid(DAY_MS) / 1000;
    let (year, month, day) = civil_from_days(days);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][month as usize - 1];
    format!(
        "{weekday}, {day} {name} {year} {:02}:{:02}:{:02} +0000",
        rest / 3600,
        (rest / 60) % 60,
        rest % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, by Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Truncates to at most `limit` bytes without splitting a character.
pub fn truncate_utf8(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_headers_are_unfolded_and_decoded() {
        let message = parse(
            b"Subject: =?utf-8?B?SGVsbG8s?=\r\n =?utf-8?B?IHdvcmxk?=\r\nFrom: a@b.c\r\n\r\nbody\r\n",
        );
        assert_eq!(
            message.headers.get("subject").as_deref(),
            Some("Hello, world")
        );
        assert_eq!(message.headers.get("from").as_deref(), Some("a@b.c"));
        assert_eq!(message.text.trim(), "body");
    }

    #[test]
    fn quoted_printable_words_and_bodies_decode() {
        let message = parse(
            b"Subject: =?iso-8859-1?Q?caf=E9_break?=\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nlong=\r\nline =3D done\r\n",
        );
        assert_eq!(
            message.headers.get("subject").as_deref(),
            Some("café break")
        );
        assert_eq!(message.text.trim(), "longline = done");
    }

    #[test]
    fn a_header_only_fetch_still_parses() {
        let message = parse(b"From: a@b.c\r\nSubject: only headers\r\n");
        assert_eq!(
            message.headers.get("subject").as_deref(),
            Some("only headers")
        );
        assert!(message.text.is_empty());
        assert!(message.attachments.is_empty());
    }

    #[test]
    fn the_plain_part_wins_over_html_whatever_the_order() {
        for (first, second) in [("text/plain", "text/html"), ("text/html", "text/plain")] {
            let raw = format!(
                "Content-Type: multipart/alternative; boundary=\"B\"\r\n\r\n\
                 --B\r\nContent-Type: {first}\r\n\r\n{}\r\n\
                 --B\r\nContent-Type: {second}\r\n\r\n{}\r\n--B--\r\n",
                if first == "text/plain" {
                    "plain body"
                } else {
                    "<p>html body</p>"
                },
                if second == "text/plain" {
                    "plain body"
                } else {
                    "<p>html body</p>"
                },
            );
            let message = parse(raw.as_bytes());
            assert_eq!(message.text.trim(), "plain body", "{first} then {second}");
            assert!(!message.text_from_html);
        }
    }

    #[test]
    fn an_html_only_message_is_reduced_to_its_text() {
        let message = parse(
            b"Content-Type: text/html\r\n\r\n<style>p{color:red}</style><p>Hello <b>there</b></p><script>alert(1)</script>",
        );
        assert!(message.text_from_html);
        assert!(message.text.contains("Hello"), "{:?}", message.text);
        assert!(message.text.contains("there"));
        assert!(!message.text.contains("alert"));
        assert!(!message.text.contains("color:red"));
    }

    #[test]
    fn attachments_are_described_and_decoded() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"X\"\r\n\r\n\
--X\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
--X\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--X--\r\n";
        let message = parse(raw);
        assert_eq!(message.text.trim(), "see attached");
        assert_eq!(message.attachments.len(), 1);
        let attachment = &message.attachments[0];
        assert_eq!(attachment.file_name.as_deref(), Some("report.pdf"));
        assert_eq!(attachment.media_type, "application/pdf");
        assert_eq!(attachment.bytes, b"hello");
    }

    /// A boundary string may appear inside a body; only a line that starts
    /// with the delimiter separates parts.
    #[test]
    fn a_boundary_inside_a_body_does_not_split_it() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"SEP\"\r\n\r\n\
--SEP\r\nContent-Type: text/plain\r\n\r\ntalking about --SEP inline\r\n--SEP--\r\n";
        let message = parse(raw);
        assert_eq!(message.text.trim(), "talking about --SEP inline");
    }

    #[test]
    fn malformed_input_yields_a_poorer_observation_not_a_failure() {
        for raw in [
            &b""[..],
            b"not a header at all",
            b"Content-Type: multipart/mixed; boundary=\"Z\"\r\n\r\n--Z\r\n",
            b"Content-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\n!!!not base64!!!",
            b"Subject: =?utf-8?B?bad",
        ] {
            let _ = parse(raw);
        }
    }

    fn outgoing() -> Outgoing {
        Outgoing {
            message_id: "1.2.pluribus@example.com".into(),
            date: rfc5322_date(1_700_000_000_000),
            from: "ada@example.com".into(),
            to: vec!["bob@example.net".into()],
            subject: "Lunch".into(),
            text: "yes".into(),
            ..Outgoing::default()
        }
    }

    fn written(message: &Outgoing) -> String {
        String::from_utf8(write(message).unwrap()).unwrap()
    }

    #[test]
    fn a_written_message_is_crlf_throughout_and_ends_its_header_block() {
        let text = written(&outgoing());
        // Every newline is part of a CRLF; a bare LF would end a line for
        // some peers and not others.
        assert_eq!(text.matches('\n').count(), text.matches("\r\n").count());
        assert!(text.contains("\r\n\r\nyes\r\n"), "{text:?}");
        assert!(text.contains("Date: Tue, 14 Nov 2023 22:13:20 +0000\r\n"));
        assert!(text.contains("From: ada@example.com\r\n"));
        assert!(text.contains("To: bob@example.net\r\n"));
        assert!(text.contains("Subject: Lunch\r\n"));
        assert!(text.contains("Message-Id: <1.2.pluribus@example.com>\r\n"));
        assert!(text.contains("Content-Transfer-Encoding: 7bit\r\n"));
    }

    #[test]
    fn threading_headers_carry_the_chain_in_angle_brackets() {
        let mut message = outgoing();
        message.in_reply_to = Some("parent@example.net".into());
        message.references = vec!["root@example.net".into(), "<parent@example.net>".into()];
        message.cc = vec!["carol@example.net".into()];
        let text = written(&message);
        assert!(text.contains("In-Reply-To: <parent@example.net>\r\n"));
        assert!(text.contains("References: <root@example.net> <parent@example.net>\r\n"));
        assert!(text.contains("Cc: carol@example.net\r\n"));
    }

    /// A non-ASCII subject travels as encoded words, and a word is never
    /// longer than RFC 2047 allows.
    #[test]
    fn a_non_ascii_subject_is_encoded_in_bounded_words() {
        let mut message = outgoing();
        message.subject = "café ☕ ".repeat(12);
        let text = written(&message);
        let header = text
            .lines()
            .skip_while(|line| !line.starts_with("Subject:"))
            .take_while(|line| line.starts_with("Subject:") || line.starts_with(' '))
            .collect::<Vec<_>>()
            .join("");
        assert!(!header.contains("café"), "{header}");
        for word in header.split(' ').filter(|word| word.starts_with("=?")) {
            assert!(word.len() <= 75, "{word} is {} characters", word.len());
            assert!(word.ends_with("?="));
        }
        // Every written line stays inside the 998-octet limit.
        for line in text.split("\r\n") {
            assert!(line.len() <= 998);
        }
    }

    #[test]
    fn a_maximum_length_subject_does_not_exceed_the_wire_limit() {
        let mut message = outgoing();
        message.subject = "x".repeat(998);
        assert_eq!(write(&message), Err(WriteError::HeaderTooLong));
    }

    #[test]
    fn unbreakable_identifiers_and_display_names_are_rejected() {
        let mut message = outgoing();
        message.message_id = "x".repeat(998);
        assert_eq!(write(&message), Err(WriteError::HeaderTooLong));

        let mut message = outgoing();
        message.display_name = Some("x".repeat(998));
        assert_eq!(write(&message), Err(WriteError::HeaderTooLong));
    }

    /// Folding is a transport detail: unfolding must give back what was
    /// written, spacing included.
    #[test]
    fn folding_preserves_the_value_it_folds() {
        assert_eq!(fold("Subject", "a  b").unwrap(), "Subject: a  b\r\n");
        assert_eq!(
            fold("Subject", "  padded  ").unwrap(),
            "Subject: padded\r\n"
        );
        assert_eq!(fold("Subject", "").unwrap(), "Subject:\r\n");
        let long = fold("References", &"<0123456789@example.net>".repeat(6)).unwrap();
        // A token within the limit is left whole rather than split.
        assert_eq!(long.matches("\r\n").count(), 1);
        let recipients = ["someone@example.net"; 8].join(", ");
        let folded = fold("To", &recipients).unwrap();
        for line in folded.trim_end().split("\r\n") {
            assert!(line.len() <= 78, "{line}");
        }
        assert_eq!(
            folded.replace("\r\n ", " ").trim_end(),
            format!("To: {recipients}")
        );
    }

    #[test]
    fn a_display_name_is_quoted_or_encoded_but_never_interpolated() {
        assert_eq!(mailbox(None, "a@b.com"), "a@b.com");
        assert_eq!(mailbox(Some("Ada"), "a@b.com"), "\"Ada\" <a@b.com>");
        assert_eq!(
            mailbox(Some("Ada \"The\" L"), "a@b.com"),
            "\"Ada \\\"The\\\" L\" <a@b.com>"
        );
        // A newline in a name would otherwise start a header of its own.
        let injected = mailbox(Some("Ada\r\nBcc: eve@x.com"), "a@b.com");
        assert!(!injected.contains('\n'), "{injected}");
        assert!(mailbox(Some("Ada Λ"), "a@b.com").starts_with("=?utf-8?B?"));
    }

    #[test]
    fn a_header_value_cannot_carry_a_line_of_its_own() {
        let mut message = outgoing();
        message.subject = "hello\r\nBcc: eve@example.net".into();
        let text = written(&message);
        assert!(
            !text.contains("Bcc:\r\n") && !text.contains("\r\nBcc:"),
            "{text}"
        );
        assert!(
            text.contains("Subject: helloBcc: eve@example.net\r\n"),
            "{text}"
        );
    }

    #[test]
    fn a_body_that_cannot_travel_as_it_stands_is_quoted_printable() {
        let mut message = outgoing();
        message.text = "café = 1\ntrailing \n".into();
        let text = written(&message);
        assert!(text.contains("Content-Transfer-Encoding: quoted-printable\r\n"));
        assert!(text.contains("caf=C3=A9 =3D 1\r\n"), "{text}");
        assert!(text.contains("trailing=20\r\n"), "{text}");
    }

    /// A quoted-printable line never passes 76 characters, and a `=XX`
    /// escape is never split across the soft break that keeps it there.
    #[test]
    fn quoted_printable_lines_are_bounded_and_escapes_stay_whole() {
        for body in [
            "é".repeat(200),
            "x".repeat(200),
            format!("{}é", "x".repeat(74)),
        ] {
            let encoded = quoted_printable(&body);
            for line in encoded.split("\r\n") {
                assert!(line.len() <= 76, "{line:?} is {} long", line.len());
            }
            for (index, _) in encoded.match_indices('=') {
                let rest = &encoded[index + 1..];
                assert!(
                    rest.starts_with("\r\n")
                        || rest.len() >= 2 && rest[..2].chars().all(|c| c.is_ascii_hexdigit()),
                    "a split escape at {index} in {encoded:?}"
                );
            }
        }
    }

    #[test]
    fn only_a_bare_addr_spec_is_usable() {
        for good in ["a@b.com", "first.last+tag@sub.example.co.uk"] {
            assert!(valid_address(good), "{good}");
        }
        for bad in [
            "Ada <a@b.com>",
            "a@b.com, c@d.com",
            "a@b.com\r\nRCPT TO:<e@f.com>",
            "a@localhost",
            "@b.com",
            "a@",
            "a@b.",
            "a@.b",
            "a b@c.com",
            "café@b.com",
            "a@b@c.com",
        ] {
            assert!(!valid_address(bad), "{bad}");
        }
    }

    #[test]
    fn dates_are_utc_and_name_the_right_day() {
        assert_eq!(rfc5322_date(0), "Thu, 1 Jan 1970 00:00:00 +0000");
        assert_eq!(
            rfc5322_date(1_700_000_000_000),
            "Tue, 14 Nov 2023 22:13:20 +0000"
        );
        // A leap day, and the last second before one.
        assert_eq!(
            rfc5322_date(1_709_164_799_000),
            "Wed, 28 Feb 2024 23:59:59 +0000"
        );
        assert_eq!(
            rfc5322_date(1_709_164_800_000),
            "Thu, 29 Feb 2024 00:00:00 +0000"
        );
    }

    #[test]
    fn a_long_body_is_truncated_on_a_character_boundary() {
        let mut body = "é".repeat(MAX_BODY_BYTES);
        let raw = format!("Content-Type: text/plain\r\n\r\n{body}");
        let message = parse(raw.as_bytes());
        assert!(message.text.len() <= MAX_BODY_BYTES);
        truncate_utf8(&mut body, 3);
        assert_eq!(body, "é");
    }
}
