//! IMAP framing over the granted byte channel.
//!
//! The transport delivers bytes with no boundaries, so this module owns the
//! whole of IMAP's framing: CRLF-terminated lines, and the length-prefixed
//! literals that may appear anywhere in one. A caller sees complete logical
//! responses and never a partial read.

use crate::channel::Socket;
use crate::pluribus::plugin::types::{Error, ErrorCode};

/// Bytes asked of the transport per read.
const READ_CHUNK: u32 = 64 * 1024;
/// Ceiling on one logical response, literals included. A message larger than
/// this is fetched in parts, never buffered whole.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
/// Ceiling on what may accumulate unconsumed. A peer that never sends CRLF
/// cannot make the component grow without bound.
const MAX_BUFFERED: usize = MAX_RESPONSE + 64 * 1024;

/// A connection carrying complete IMAP responses.
pub struct Connection {
    socket: Socket,
    framer: Framer,
    tag: u32,
    closed: bool,
}

/// Bytes waiting to become responses. Separated from the transport so the
/// framing — the part with all the edge cases — is exercised directly.
#[derive(Default)]
pub struct Framer {
    buffer: Vec<u8>,
    /// Consumed prefix of `buffer`, compacted rather than copied per line.
    start: usize,
}

/// One logical response: a line with every literal resolved in place.
pub struct Response {
    pub bytes: Vec<u8>,
}

impl Response {
    /// The response as text, lossily. IMAP is ASCII; message bodies reach the
    /// caller as bytes through `literals`, not through this.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    pub fn is_continuation(&self) -> bool {
        self.bytes.starts_with(b"+")
    }
}

impl Connection {
    pub fn new(socket: Socket) -> Self {
        Self {
            socket,
            framer: Framer::default(),
            tag: 0,
            closed: false,
        }
    }

    /// The next command tag. Tags are per-connection and never reused.
    pub fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("p{:04}", self.tag)
    }

    pub async fn send_line(&mut self, line: &str) -> Result<(), Error> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.extend_from_slice(b"\r\n");
        self.socket.send(&bytes).await
    }

    /// Reads one logical response, waiting up to `timeout_ms` for its first
    /// byte. `None` means the wait elapsed with nothing buffered, which is the
    /// normal state of an idle connection.
    ///
    /// Once a response has started, the remainder is awaited without a further
    /// timeout: a half-read literal has no useful partial meaning.
    pub async fn read(&mut self, timeout_ms: Option<u32>) -> Result<Option<Response>, Error> {
        if let Some(response) = self.framer.take_response()? {
            return Ok(Some(response));
        }
        if !self.fill(timeout_ms).await? {
            return Ok(None);
        }
        loop {
            if let Some(response) = self.framer.take_response()? {
                return Ok(Some(response));
            }
            if !self.fill(None).await? {
                return Err(unavailable("endpoint closed mid-response"));
            }
        }
    }

    /// Reads until the response tagged `tag`, collecting untagged lines.
    ///
    /// # Errors
    /// Returns an error for a `NO` or `BAD` completion, carrying its text.
    pub async fn read_until_tagged(&mut self, tag: &str) -> Result<Completion, Error> {
        let mut untagged = Vec::new();
        loop {
            let Some(response) = self.read(None).await? else {
                return Err(unavailable("endpoint closed before completing a command"));
            };
            if let Some(status) = tagged_status(&response, tag)? {
                return Ok(Completion { status, untagged });
            }
            untagged.push(response);
        }
    }

    /// Pulls one chunk from the transport. `false` means the wait elapsed.
    async fn fill(&mut self, timeout_ms: Option<u32>) -> Result<bool, Error> {
        let chunk = self.socket.read(READ_CHUNK, timeout_ms).await?;
        if chunk.closed {
            self.closed = true;
            return Err(unavailable("endpoint closed the connection"));
        }
        if chunk.bytes.is_empty() {
            return Ok(false);
        }
        self.framer.push(&chunk.bytes)?;
        Ok(true)
    }
}

impl Framer {
    /// Adds transport bytes.
    ///
    /// # Errors
    /// Returns an error once unconsumed bytes pass the buffer ceiling: a peer
    /// that never sends CRLF must not grow the component without bound.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.buffer.len() - self.start > MAX_BUFFERED {
            return Err(failure(
                ErrorCode::ResourceExhausted,
                "endpoint response exceeds the buffer ceiling",
            ));
        }
        if self.start > 0 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Takes one complete response from the buffer, if one is there.
    ///
    /// # Errors
    /// Returns an error for a response past the response ceiling.
    pub fn take_response(&mut self) -> Result<Option<Response>, Error> {
        let mut cursor = self.start;
        let mut line = Vec::new();
        loop {
            let Some(end) = find_crlf(&self.buffer[cursor..]) else {
                return Ok(None);
            };
            let text = &self.buffer[cursor..cursor + end];
            match literal_length(text) {
                None => {
                    line.extend_from_slice(text);
                    self.start = cursor + end + 2;
                    if line.len() > MAX_RESPONSE {
                        return Err(failure(
                            ErrorCode::ResourceExhausted,
                            "endpoint response exceeds the response ceiling",
                        ));
                    }
                    return Ok(Some(Response { bytes: line }));
                }
                Some(length) => {
                    let body = cursor + end + 2;
                    if line.len().saturating_add(length) > MAX_RESPONSE {
                        return Err(failure(
                            ErrorCode::ResourceExhausted,
                            "endpoint literal exceeds the response ceiling",
                        ));
                    }
                    if self.buffer.len() < body + length {
                        return Ok(None);
                    }
                    line.extend_from_slice(text);
                    line.extend_from_slice(&self.buffer[body..body + length]);
                    cursor = body + length;
                }
            }
        }
    }
}

/// A completed command: its status and the untagged responses it produced.
pub struct Completion {
    pub status: Status,
    pub untagged: Vec<Response>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Ok,
    No,
    Bad,
}

/// The status of `response` when it is the completion of `tag`.
fn tagged_status(response: &Response, tag: &str) -> Result<Option<Status>, Error> {
    let text = response.text();
    let Some(rest) = text.strip_prefix(tag) else {
        return Ok(None);
    };
    let Some(rest) = rest.strip_prefix(' ') else {
        return Ok(None);
    };
    let word = rest.split(' ').next().unwrap_or_default();
    match word.to_ascii_uppercase().as_str() {
        "OK" => Ok(Some(Status::Ok)),
        "NO" => Ok(Some(Status::No)),
        "BAD" => Ok(Some(Status::Bad)),
        _ => Err(unavailable("endpoint sent an unknown completion status")),
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|pair| pair == b"\r\n")
}

/// The octet count of a literal introduced at the end of `line`, as in
/// `... {1234}` or the non-synchronising `... {1234+}`.
fn literal_length(line: &[u8]) -> Option<usize> {
    let line = line.strip_suffix(b"}")?;
    let open = line.iter().rposition(|byte| *byte == b'{')?;
    let digits = &line[open + 1..];
    let digits = digits.strip_suffix(b"+").unwrap_or(digits);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

pub fn failure(code: ErrorCode, message: impl Into<String>) -> Error {
    Error {
        code,
        message: message.into(),
        retryable: matches!(code, ErrorCode::Unavailable | ErrorCode::DeadlineExceeded),
        details: None,
    }
}

pub fn unavailable(message: impl Into<String>) -> Error {
    failure(ErrorCode::Unavailable, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `bytes` one chunk at a time and collects whole responses.
    fn frame(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut framer = Framer::default();
        let mut out = Vec::new();
        for chunk in chunks {
            framer.push(chunk).unwrap();
            while let Some(response) = framer.take_response().unwrap() {
                out.push(response.bytes);
            }
        }
        out
    }

    #[test]
    fn lines_are_split_on_crlf_and_the_terminator_dropped() {
        assert_eq!(
            frame(&[b"* OK ready\r\np0001 OK done\r\n"]),
            [b"* OK ready".to_vec(), b"p0001 OK done".to_vec()]
        );
    }

    /// The transport gives no boundaries, so a response may arrive in any
    /// number of pieces, including one byte at a time.
    #[test]
    fn a_response_split_across_chunks_is_reassembled() {
        let whole = b"* OK [UIDVALIDITY 1] ready\r\n";
        let byte_at_a_time: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(
            frame(&byte_at_a_time),
            [b"* OK [UIDVALIDITY 1] ready".to_vec()]
        );
        assert_eq!(
            frame(&[&whole[..7], &whole[7..]]),
            [b"* OK [UIDVALIDITY 1] ready".to_vec()]
        );
        // A CRLF split across the boundary is still one terminator.
        assert_eq!(frame(&[b"* OK\r", b"\n"]), [b"* OK".to_vec()]);
    }

    /// A literal's bytes are data, not framing: CRLF inside one ends nothing.
    #[test]
    fn a_literal_carrying_crlf_does_not_end_the_response() {
        let body = b"line one\r\nline two\r\n";
        let mut wire = format!("* 1 FETCH (BODY[] {{{}}}\r\n", body.len()).into_bytes();
        wire.extend_from_slice(body);
        wire.extend_from_slice(b")\r\n");
        let framed = frame(&[&wire]);
        assert_eq!(framed.len(), 1, "one response, not three");
        let mut expected = format!("* 1 FETCH (BODY[] {{{}}}", body.len()).into_bytes();
        expected.extend_from_slice(body);
        expected.push(b')');
        assert_eq!(framed[0], expected);
    }

    #[test]
    fn a_literal_split_across_chunks_waits_for_all_its_bytes() {
        let mut wire = b"* 1 FETCH (BODY[] {6}\r\n".to_vec();
        wire.extend_from_slice(b"abcdef)\r\n");
        let mut framer = Framer::default();
        framer.push(&wire[..24]).unwrap();
        assert!(
            framer.take_response().unwrap().is_none(),
            "literal incomplete"
        );
        framer.push(&wire[24..]).unwrap();
        let response = framer.take_response().unwrap().unwrap();
        assert!(response.bytes.ends_with(b"abcdef)"));
    }

    /// Two literals in one response, the shape a multi-item FETCH takes.
    #[test]
    fn several_literals_in_one_response_are_all_resolved() {
        let mut wire = b"* 1 FETCH (BODY[HEADER] {4}\r\n".to_vec();
        wire.extend_from_slice(b"From");
        wire.extend_from_slice(b" BODY[TEXT] {4}\r\n");
        wire.extend_from_slice(b"body");
        wire.extend_from_slice(b")\r\n");
        let framed = frame(&[&wire]);
        assert_eq!(framed.len(), 1);
        assert_eq!(
            framed[0],
            b"* 1 FETCH (BODY[HEADER] {4}From BODY[TEXT] {4}body)".to_vec()
        );
    }

    #[test]
    fn a_partial_response_yields_nothing_and_stays_buffered() {
        let mut framer = Framer::default();
        framer.push(b"* OK part").unwrap();
        assert!(framer.take_response().unwrap().is_none());
        framer.push(b"ial\r\n").unwrap();
        assert_eq!(
            framer.take_response().unwrap().unwrap().bytes,
            b"* OK partial".to_vec()
        );
        assert!(framer.take_response().unwrap().is_none());
    }

    #[test]
    fn a_peer_that_never_terminates_a_line_hits_the_buffer_ceiling() {
        let mut framer = Framer::default();
        let chunk = vec![b'x'; 1024 * 1024];
        let mut pushes = 0;
        loop {
            if framer.push(&chunk).is_err() {
                break;
            }
            assert!(framer.take_response().unwrap().is_none());
            pushes += 1;
            assert!(pushes < 64, "the ceiling must stop an unterminated line");
        }
    }

    #[test]
    fn literals_are_recognised_only_at_the_end_of_a_line() {
        assert_eq!(literal_length(b"* 1 FETCH (BODY[] {42}"), Some(42));
        assert_eq!(literal_length(b"* 1 FETCH (BODY[] {42+}"), Some(42));
        assert_eq!(literal_length(b"* OK [UIDVALIDITY 1] {}"), None);
        assert_eq!(literal_length(b"* OK {12} trailing"), None);
        assert_eq!(literal_length(b"* OK [UIDNEXT 9]"), None);
    }

    #[test]
    fn a_tag_matches_only_its_own_completion() {
        let ok = Response {
            bytes: b"p0001 OK FETCH completed".to_vec(),
        };
        assert_eq!(tagged_status(&ok, "p0001").unwrap(), Some(Status::Ok));
        assert_eq!(tagged_status(&ok, "p0002").unwrap(), None);
        // A tag is not a prefix match: p0001 must not answer for p00011.
        let other = Response {
            bytes: b"p00011 OK done".to_vec(),
        };
        assert_eq!(tagged_status(&other, "p0001").unwrap(), None);
        let untagged = Response {
            bytes: b"* 4 EXISTS".to_vec(),
        };
        assert_eq!(tagged_status(&untagged, "p0001").unwrap(), None);
    }

    #[test]
    fn statuses_are_case_insensitive_and_bounded() {
        for (line, expected) in [
            ("p0001 no [AUTHENTICATIONFAILED] bad password", Status::No),
            ("p0001 BAD syntax", Status::Bad),
        ] {
            let response = Response {
                bytes: line.as_bytes().to_vec(),
            };
            assert_eq!(tagged_status(&response, "p0001").unwrap(), Some(expected));
        }
        let unknown = Response {
            bytes: b"p0001 MAYBE".to_vec(),
        };
        assert!(tagged_status(&unknown, "p0001").is_err());
    }
}
