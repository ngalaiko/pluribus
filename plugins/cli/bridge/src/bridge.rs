use crate::{peer_uid, verify_peer};
use protocol::{MAX_MESSAGES, MAX_REQUEST, Message, Request, Response};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// Input the person typed but the agent has not read yet.
#[derive(Default)]
struct Inbox {
    messages: Vec<Message>,
    next: u64,
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

    let shared: Shared = Arc::new((Mutex::new(Inbox::default()), Condvar::new()));
    let typing = Arc::clone(&shared);
    thread::spawn(move || read_terminal(&typing));

    info!(
        socket = %socket.display(),
        runtime_uid,
        "bridge listening"
    );
    println!("Connected to {}. Type a message.", socket.display());
    for stream in listener.incoming() {
        let mut stream = stream?;
        if verify_peer(&stream, runtime_uid).is_err() {
            warn!(
                expected_uid = runtime_uid,
                peer_uid = peer_uid(&stream).ok(),
                "rejected socket peer"
            );
            continue;
        }
        let response = handle(&mut stream, &shared).unwrap_or_else(|error| Response::Unavailable {
            message: error.to_string(),
        });
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        if let Ok(mut bytes) = serde_json::to_vec(&response) {
            bytes.push(b'\n');
            let _ = stream.write_all(&bytes);
        }
    }
    Ok(())
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
            after, timeout_ms, ..
        } => {
            let messages = wait_for_input(shared, after, Duration::from_millis(timeout_ms.into()));
            // Counts only: the text is the conversation, not a transport detail.
            if !messages.is_empty() {
                debug!(count = messages.len(), after, "poll returning input");
            }
            Response::Messages { messages }
        }
        Request::Reply { text, .. } => {
            print_reply(&text);
            Response::Delivered
        }
    })
}

/// Waits for input newer than `after`, up to `timeout`.
fn wait_for_input(shared: &Shared, after: u64, timeout: Duration) -> Vec<Message> {
    let (inbox, signal) = &**shared;
    let deadline = Instant::now() + timeout;
    let mut guard = inbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let pending: Vec<Message> = guard
            .messages
            .iter()
            .filter(|message| message.sequence > after)
            .take(MAX_MESSAGES)
            .cloned()
            .collect();
        if !pending.is_empty() {
            // Everything the agent has read can go.
            guard.messages.retain(|message| message.sequence > after);
            return pending;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Vec::new();
        };
        guard = signal
            .wait_timeout(guard, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
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
    let mut line = String::new();
    loop {
        {
            let mut out = io::stdout().lock();
            let _ = write!(out, "you: ");
            let _ = out.flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            // A closed terminal stops the typing, not the agent: polls keep
            // waiting out their timeout rather than spinning.
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = line.trim().to_owned();
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
