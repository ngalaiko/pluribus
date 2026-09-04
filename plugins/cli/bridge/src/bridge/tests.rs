use super::{Inbox, Shared, wait_for_input};
use protocol::{Message, Request, VERSION};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn shared(texts: &[&str]) -> Shared {
    let mut inbox = Inbox::default();
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
    let messages = wait_for_input(&shared, 0, Duration::from_millis(50));
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].sequence, 1);
    assert_eq!(messages[1].text, "second");
    // The agent read through sequence 2, so nothing repeats.
    assert!(wait_for_input(&shared, 2, Duration::from_millis(10)).is_empty());
}

#[test]
fn a_poll_without_input_waits_for_its_timeout() {
    let shared = shared(&[]);
    let started = Instant::now();
    assert!(wait_for_input(&shared, 0, Duration::from_millis(80)).is_empty());
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
    let messages = wait_for_input(&shared, 0, Duration::from_secs(5));
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
        timeout_ms: 1_000,
    };
    assert_eq!(stale.validate(), Err("unsupported protocol version"));
}
