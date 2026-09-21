//! SMTP submission over the granted `smtp` endpoint.
//!
//! The host hands this module a connection that is already encrypted. The
//! endpoint's grant names a `starttls` preamble, so the plaintext greeting,
//! `EHLO` and `STARTTLS` are the host's; the first byte written here is this
//! module's own `EHLO`, which RFC 3207 requires after the handshake anyway.
//!
//! One connection per send. A submission endpoint is not a session worth
//! holding: reusing one would mean keeping a second long-lived connection
//! alive beside the IMAP one to save a handshake on a rare command.

use crate::channel::Socket;
use crate::pluribus::plugin::types::{Error, ErrorCode};
use crate::wire::{failure, unavailable};
use base64::Engine as _;

/// Bytes asked of the transport per read.
const READ_CHUNK: u32 = 16 * 1024;
/// How long one reply may take. A submission endpoint that has gone quiet is
/// a failed send, not an idle session.
const REPLY_TIMEOUT_MS: u32 = 60_000;
/// Ceiling on one reply, continuation lines included.
const MAX_REPLY: usize = 64 * 1024;

/// One reply: its code and the text of every line it spanned.
#[derive(Debug, Eq, PartialEq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    fn text(&self) -> String {
        self.lines.join(" ")
    }
}

/// One connected submission session.
pub struct Session {
    socket: Socket,
    buffer: Vec<u8>,
    /// What `EHLO` advertised, uppercased, one entry per line.
    extensions: Vec<String>,
}

impl Session {
    /// Connects the `smtp` endpoint and greets it.
    ///
    /// `client_name` is what this client calls itself. It has no domain of
    /// its own, so the sender's domain stands in.
    pub async fn open(client_name: &str) -> Result<Self, Error> {
        let mut session = Self {
            socket: Socket::connect("smtp").await?,
            buffer: Vec::new(),
            extensions: Vec::new(),
        };
        let reply = session.command(&format!("EHLO {client_name}")).await?;
        if reply.code != 250 {
            return Err(refused("EHLO", &reply));
        }
        session.extensions = reply
            .lines
            .iter()
            .map(|line| line.trim().to_ascii_uppercase())
            .collect();
        Ok(session)
    }

    /// Authenticates with the enrolled credential.
    ///
    /// `PLAIN` is preferred and `LOGIN` is the fallback; an endpoint offering
    /// neither is refused rather than silently submitting unauthenticated.
    pub async fn authenticate(&mut self, user: &str, password: &str) -> Result<(), Error> {
        if user.is_empty()
            || password.is_empty()
            || [user, password]
                .iter()
                .any(|value| value.contains(['\0', '\r', '\n']))
        {
            return Err(failure(
                ErrorCode::InvalidArgument,
                "credential cannot appear in an SMTP command",
            ));
        }
        let mechanisms = self.extension("AUTH").unwrap_or_default();
        let reply = if mechanisms.contains("PLAIN") {
            let secret = encode(&format!("\0{user}\0{password}"));
            self.command(&format!("AUTH PLAIN {secret}")).await?
        } else if mechanisms.contains("LOGIN") {
            let reply = self
                .command(&format!("AUTH LOGIN {}", encode(user)))
                .await?;
            if reply.code != 334 {
                return Err(refused("AUTH LOGIN", &reply));
            }
            self.command(&encode(password)).await?
        } else {
            return Err(failure(
                ErrorCode::Unsupported,
                "endpoint offers no password authentication",
            ));
        };
        if reply.code == 235 {
            return Ok(());
        }
        Err(refused("AUTH", &reply))
    }

    /// The largest message the endpoint advertised, if it advertised one.
    #[must_use]
    pub fn size_limit(&self) -> Option<u64> {
        self.extension("SIZE")?.trim().parse().ok()
    }

    /// Submits one message. The recipients are the envelope's, which need not
    /// be the addresses in the headers.
    pub async fn submit(
        &mut self,
        from: &str,
        recipients: &[String],
        message: &[u8],
    ) -> Result<(), Error> {
        if recipients.is_empty() {
            return Err(failure(ErrorCode::InvalidArgument, "no recipients"));
        }
        if self.size_limit().is_some_and(|limit| {
            // A zero SIZE parameter means the endpoint declares no limit.
            limit > 0 && message.len() as u64 > limit
        }) {
            return Err(failure(
                ErrorCode::ResourceExhausted,
                "message exceeds the size the endpoint advertised",
            ));
        }
        let reply = self.command(&format!("MAIL FROM:<{from}>")).await?;
        if reply.code != 250 {
            return Err(refused("MAIL FROM", &reply));
        }
        for recipient in recipients {
            let reply = self.command(&format!("RCPT TO:<{recipient}>")).await?;
            // A partly accepted recipient set would deliver a message the
            // caller did not ask for, so one refusal ends the submission.
            if !matches!(reply.code, 250 | 251) {
                return Err(refused("RCPT TO", &reply));
            }
        }
        let reply = self.command("DATA").await?;
        if reply.code != 354 {
            return Err(refused("DATA", &reply));
        }
        self.socket.send(&dot_stuff(message)).await?;
        let reply = self.read_reply().await?;
        if reply.code != 250 {
            return Err(refused("the message", &reply));
        }
        Ok(())
    }

    /// Ends the session. A refused `QUIT` says nothing about the message the
    /// endpoint already accepted, so the reply is read and discarded.
    pub async fn quit(&mut self) {
        if self.socket.send(b"QUIT\r\n").await.is_ok() {
            let _ = self.read_reply().await;
        }
    }

    /// The parameters of one advertised extension, uppercased.
    fn extension(&self, name: &str) -> Option<String> {
        self.extensions.iter().find_map(|line| {
            let rest = line.strip_prefix(name)?;
            match rest.chars().next() {
                None => Some(String::new()),
                Some(' ') => Some(rest[1..].to_owned()),
                Some(_) => None,
            }
        })
    }

    async fn command(&mut self, line: &str) -> Result<Reply, Error> {
        self.socket.send(format!("{line}\r\n").as_bytes()).await?;
        self.read_reply().await
    }

    /// Reads one reply, following `250-` continuation lines to the `250 `
    /// that ends them.
    async fn read_reply(&mut self) -> Result<Reply, Error> {
        let mut code = None;
        let mut lines = Vec::new();
        loop {
            let line = self.read_line().await?;
            let digits = line
                .get(..3)
                .and_then(|digits| digits.parse::<u16>().ok())
                .ok_or_else(|| unavailable("endpoint sent a malformed SMTP reply"))?;
            if *code.get_or_insert(digits) != digits {
                return Err(unavailable("endpoint changed code mid-reply"));
            }
            lines.push(line.get(4..).unwrap_or_default().to_owned());
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok(Reply {
                    code: digits,
                    lines,
                });
            }
            if lines.len() * 4 > MAX_REPLY {
                return Err(failure(
                    ErrorCode::ResourceExhausted,
                    "endpoint reply exceeds the reply ceiling",
                ));
            }
        }
    }

    /// One CRLF-terminated line, the terminator dropped.
    async fn read_line(&mut self) -> Result<String, Error> {
        loop {
            if let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
                self.buffer.drain(..end + 2);
                return Ok(line);
            }
            if self.buffer.len() > MAX_REPLY {
                return Err(failure(
                    ErrorCode::ResourceExhausted,
                    "endpoint reply exceeds the reply ceiling",
                ));
            }
            let chunk = self.socket.read(READ_CHUNK, Some(REPLY_TIMEOUT_MS)).await?;
            if chunk.closed {
                return Err(unavailable("endpoint closed the connection"));
            }
            if chunk.bytes.is_empty() {
                return Err(failure(
                    ErrorCode::DeadlineExceeded,
                    "endpoint did not answer",
                ));
            }
            self.buffer.extend_from_slice(&chunk.bytes);
        }
    }
}

/// Escapes a leading `.` on every line and appends the terminator.
///
/// RFC 5321 §4.5.2: a message line beginning with `.` would otherwise end the
/// `DATA` command early, and the rest of the message would be read as
/// commands.
#[must_use]
pub fn dot_stuff(message: &[u8]) -> Vec<u8> {
    let body = message.strip_suffix(b"\r\n").unwrap_or(message);
    let mut out = Vec::with_capacity(body.len() + 16);
    let mut start = 0;
    loop {
        let end = body[start..]
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .map_or(body.len(), |at| start + at);
        if body[start..end].first() == Some(&b'.') {
            out.push(b'.');
        }
        out.extend_from_slice(&body[start..end]);
        out.extend_from_slice(b"\r\n");
        if end == body.len() {
            break;
        }
        start = end + 2;
    }
    out.extend_from_slice(b".\r\n");
    out
}

fn encode(value: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
}

/// Maps a refusal onto an error the caller can act on.
///
/// A `4xx` is the endpoint asking to be tried later, so it stays retryable
/// the way a dropped connection is. A `5xx` is refused identically on every
/// attempt, and retrying one only wastes the endpoint's patience.
fn refused(step: &str, reply: &Reply) -> Error {
    let code = match reply.code {
        400..=499 => ErrorCode::Unavailable,
        530 | 534 | 535 | 538 => ErrorCode::PermissionDenied,
        523 | 552 => ErrorCode::ResourceExhausted,
        _ => ErrorCode::InvalidArgument,
    };
    failure(
        code,
        format!(
            "endpoint refused {step}: {} {}",
            reply.code,
            reply.text().trim()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_beginning_with_a_dot_is_stuffed_and_the_body_terminated() {
        assert_eq!(
            dot_stuff(b"Subject: hi\r\n\r\n.hidden\r\n"),
            b"Subject: hi\r\n\r\n..hidden\r\n.\r\n".to_vec()
        );
        // The terminator alone must not be mistaken for content.
        assert_eq!(dot_stuff(b".\r\n"), b"..\r\n.\r\n".to_vec());
        // A dot inside a line is data.
        assert_eq!(dot_stuff(b"a.b\r\n"), b"a.b\r\n.\r\n".to_vec());
        // Blank lines survive, and a body without a final CRLF gains one.
        assert_eq!(
            dot_stuff(b"one\r\n\r\ntwo"),
            b"one\r\n\r\ntwo\r\n.\r\n".to_vec()
        );
    }

    #[test]
    fn refusals_are_retryable_only_when_the_endpoint_asked_to_be_tried_later() {
        let reply = |code| Reply {
            code,
            lines: vec!["reason".into()],
        };
        assert!(refused("MAIL FROM", &reply(421)).retryable);
        assert!(refused("MAIL FROM", &reply(451)).retryable);
        assert!(!refused("AUTH", &reply(535)).retryable);
        assert_eq!(
            refused("AUTH", &reply(535)).code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            refused("the message", &reply(552)).code,
            ErrorCode::ResourceExhausted
        );
        assert_eq!(
            refused("RCPT TO", &reply(550)).code,
            ErrorCode::InvalidArgument
        );
        assert!(!refused("RCPT TO", &reply(550)).retryable);
    }
}
