use crate::{peer_uid, verify_peer};
use protocol::{MAX_MESSAGES, MAX_REQUEST, MAX_RESPONSE, MAX_TEXT, Message, Request, Response};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Input the person typed but the agent has not read yet.
#[derive(Default)]
struct Inbox {
    messages: Vec<Message>,
    next: u64,
    session_id: String,
}

type Shared = Arc<(Mutex<Inbox>, Condvar)>;

/// Serves one agent at a time, and the terminal for as long as it stays open.
///
/// # Errors
/// Returns socket, permission, and listener failures.
pub fn serve(socket: &Path, runtime_uid: u32) -> io::Result<()> {
    if !socket.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute socket path required",
        ));
    }
    if socket.symlink_metadata().is_ok() {
        // A stale socket from a previous run would otherwise block binding.
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;

    let shared: Shared = Arc::new((
        Mutex::new(Inbox {
            session_id: new_session_id()?,
            ..Inbox::default()
        }),
        Condvar::new(),
    ));
    let typing = Arc::clone(&shared);
    thread::spawn(move || read_terminal(&typing));

    info!(
        socket = %socket.display(),
        runtime_uid,
        "bridge listening"
    );
    println!("Connected to {}. Type a message.", socket.display());
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = stream?;
        if verify_peer(&stream, runtime_uid).is_err() {
            warn!(
                expected_uid = runtime_uid,
                peer_uid = peer_uid(&stream).ok(),
                "rejected socket peer"
            );
            continue;
        }
        spawn_connection(stream, shared.clone(), active.clone());
    }
    Ok(())
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
fn spawn_connection(mut stream: UnixStream, shared: Shared, active: Arc<AtomicUsize>) {
    if active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < 16).then_some(count + 1)
        })
        .is_err()
    {
        return;
    }
    thread::spawn(move || {
        let _active = Active(active);
        let response = handle(&mut stream, &shared).unwrap_or_else(|error| Response::Unavailable {
            message: error.to_string(),
        });
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let _ = stream.write_all(&encode_response(response));
    });
}

fn encode_response(response: Response) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&response).unwrap_or_default();
    if bytes.len() + 1 > MAX_RESPONSE {
        bytes = serde_json::to_vec(&Response::Unavailable {
            message: "bridge response exceeds limit".into(),
        })
        .unwrap_or_default();
    }
    bytes.push(b'\n');
    bytes
}

fn handle(stream: &mut UnixStream, shared: &Shared) -> io::Result<Response> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut bytes = Vec::new();
    BufReader::new((&mut *stream).take(MAX_REQUEST as u64 + 1)).read_until(b'\n', &mut bytes)?;
    if bytes.len() > MAX_REQUEST || bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid request frame",
        ));
    }
    let request: Request = serde_json::from_slice(&bytes)?;
    request.validate().map_err(io::Error::other)?;
    Ok(match request {
        Request::Poll {
            session_id,
            after,
            timeout_ms,
            ..
        } => {
            let (session_id, messages) = wait_for_input(
                shared,
                session_id.as_deref(),
                after,
                Duration::from_millis(timeout_ms.into()),
            );
            // Counts only: the text is the conversation, not a transport detail.
            if !messages.is_empty() {
                debug!(count = messages.len(), after, "poll returning input");
            }
            Response::Messages {
                session_id,
                messages,
            }
        }
        Request::Reply { text, .. } => {
            print_reply(&text);
            Response::Delivered
        }
    })
}

/// Waits for input newer than `after`, up to `timeout`.
fn wait_for_input(
    shared: &Shared,
    requested_session: Option<&str>,
    after: u64,
    timeout: Duration,
) -> (String, Vec<Message>) {
    let (inbox, signal) = &**shared;
    let deadline = Instant::now() + timeout;
    let mut guard = inbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let effective_after = if requested_session == Some(guard.session_id.as_str()) {
            after
        } else {
            0
        };
        let mut pending = Vec::new();
        for message in guard
            .messages
            .iter()
            .filter(|message| message.sequence > effective_after)
            .take(MAX_MESSAGES)
        {
            pending.push(message.clone());
            if !response_fits(&guard.session_id, &pending) {
                pending.pop();
                break;
            }
        }
        if !pending.is_empty() {
            let last = pending
                .last()
                .map_or(effective_after, |message| message.sequence);
            guard.messages.retain(|message| message.sequence > last);
            return (guard.session_id.clone(), pending);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return (guard.session_id.clone(), Vec::new());
        };
        guard = signal
            .wait_timeout(guard, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

fn response_fits(session_id: &str, messages: &[Message]) -> bool {
    serde_json::to_vec(&Response::Messages {
        session_id: session_id.to_owned(),
        messages: messages.to_vec(),
    })
    .is_ok_and(|bytes| bytes.len() + 1 <= MAX_RESPONSE)
}

fn new_session_id() -> io::Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| {
        io::Error::other(format!("random session identifier unavailable: {error}"))
    })?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn print_reply(text: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "\nagent: {text}");
    let _ = write!(out, "you: ");
    let _ = out.flush();
}

/// Numbers each line the person types and wakes whoever is polling.
fn read_terminal(shared: &Shared) {
    let (inbox, signal) = &**shared;
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        {
            let mut out = io::stdout().lock();
            let _ = write!(out, "you: ");
            let _ = out.flush();
        }
        let text = match read_terminal_line(&mut reader) {
            Ok(Some(text)) => text.trim().to_owned(),
            // A closed terminal stops the typing, not the agent: polls keep
            // waiting out their timeout rather than spinning.
            Ok(None) => return,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                eprintln!("input line exceeds {MAX_TEXT} bytes or is not UTF-8");
                continue;
            }
            Err(_) => return,
        };
        if text.is_empty() {
            continue;
        }
        let mut guard = inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.next += 1;
        let sequence = guard.next;
        guard.messages.push(Message {
            sequence,
            at_ms: now_ms(),
            text,
        });
        signal.notify_all();
    }
}

fn read_terminal_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let read = reader
        .take((MAX_TEXT + 2) as u64)
        .read_until(b'\n', &mut bytes)?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.last() != Some(&b'\n') && read >= MAX_TEXT + 2 {
        discard_line(reader)?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_TEXT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "line is not UTF-8"))
}

fn discard_line(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let found_newline = available.get(consumed.wrapping_sub(1)) == Some(&b'\n');
        reader.consume(consumed);
        if found_newline {
            return Ok(());
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
