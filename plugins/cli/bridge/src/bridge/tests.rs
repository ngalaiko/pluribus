use super::{Inbox, Shared, wait_for_input};
use protocol::{Message, Request, VERSION};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn shared(texts: &[&str]) -> Shared {
    let mut inbox = Inbox {
        session_id: "test-session".into(),
        ..Inbox::default()
    };
    for text in texts {
        inbox.next += 1;
        inbox.messages.push(Message {
            sequence: inbox.next,
            at_ms: 0,
            text: (*text).to_owned(),
        });
    }
    Arc::new((Mutex::new(inbox), Condvar::new()))
}

#[test]
fn a_poll_returns_pending_input_once() {
    let shared = shared(&["first", "second"]);
    let messages = wait_for_input(&shared, Some("test-session"), 0, Duration::from_millis(50)).1;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].sequence, 1);
    assert_eq!(messages[1].text, "second");
    // The agent read through sequence 2, so nothing repeats.
    assert!(
        wait_for_input(&shared, Some("test-session"), 2, Duration::from_millis(10))
            .1
            .is_empty()
    );
}

#[test]
fn a_bridge_restart_does_not_hide_new_input_behind_an_old_cursor() {
    let restarted = shared(&["first after restart"]);
    let messages = wait_for_input(
        &restarted,
        Some("old-session"),
        12,
        Duration::from_millis(50),
    )
    .1;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].sequence, 1);
}

#[test]
fn a_poll_batch_fits_the_protocol_response_limit() {
    use protocol::{MAX_RESPONSE, Response};

    let shared = shared(&[]);
    {
        let (inbox, _) = &*shared;
        let mut inbox = inbox.lock().unwrap();
        for sequence in 1..=protocol::MAX_MESSAGES as u64 {
            inbox.next = sequence;
            inbox.messages.push(Message {
                sequence,
                at_ms: 0,
                text: "\"".repeat(protocol::MAX_TEXT),
            });
        }
    }
    let (session_id, messages) = wait_for_input(&shared, Some("test-session"), 0, Duration::ZERO);
    let encoded = serde_json::to_vec(&Response::Messages {
        session_id,
        messages,
    })
    .unwrap();
    assert!(encoded.len() < MAX_RESPONSE);
}

#[test]
fn a_poll_without_input_waits_for_its_timeout() {
    let shared = shared(&[]);
    let started = Instant::now();
    assert!(
        wait_for_input(&shared, Some("test-session"), 0, Duration::from_millis(80))
            .1
            .is_empty()
    );
    assert!(started.elapsed() >= Duration::from_millis(70));
}

#[test]
fn a_poll_wakes_as_soon_as_someone_types() {
    let shared = shared(&[]);
    let typing = Arc::clone(&shared);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        let (inbox, signal) = &*typing;
        let mut guard = inbox.lock().unwrap();
        guard.next += 1;
        let sequence = guard.next;
        guard.messages.push(Message {
            sequence,
            at_ms: 0,
            text: "typed".into(),
        });
        signal.notify_all();
    });
    let messages = wait_for_input(&shared, Some("test-session"), 0, Duration::from_secs(5)).1;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "typed");
}

#[test]
fn requests_carry_their_version() {
    let reply = Request::Reply {
        version: VERSION,
        conversation_id: "local".into(),
        text: "hello".into(),
    };
    assert!(reply.validate().is_ok());
    let stale = Request::Poll {
        version: VERSION + 1,
        after: 0,
        session_id: None,
        timeout_ms: 1_000,
    };
    assert_eq!(stale.validate(), Err("unsupported protocol version"));
}

#[test]
fn oversized_terminal_line_is_drained_before_the_next_line() {
    use super::read_terminal_line;
    use protocol::MAX_TEXT;
    use std::io::Cursor;

    let mut input = Cursor::new(format!("{}\nok\n", "x".repeat(MAX_TEXT + 100)).into_bytes());
    assert_eq!(
        read_terminal_line(&mut input).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        read_terminal_line(&mut input).unwrap().as_deref(),
        Some("ok")
    );
}

#[test]
fn waiting_input_does_not_block_another_connection() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicUsize;
    let shared = shared(&[]);
    let active = Arc::new(AtomicUsize::new(0));
    let (mut blocked, server) = UnixStream::pair().unwrap();
    super::spawn_connection(server, shared.clone(), active.clone());
    blocked
        .write_all(b"{\"kind\":\"poll\",\"version\":2,\"after\":0,\"session_id\":\"test-session\",\"timeout_ms\":1000}\n")
        .unwrap();
    let (mut fast, server) = UnixStream::pair().unwrap();
    super::spawn_connection(server, shared, active);
    fast.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    fast.write_all(b"{\"kind\":\"poll\",\"version\":2,\"after\":0,\"session_id\":\"test-session\",\"timeout_ms\":1}\n")
        .unwrap();
    let mut result = String::new();
    fast.read_to_string(&mut result).unwrap();
    assert!(result.contains("messages"));
}
