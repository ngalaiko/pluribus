use pluribus_core::{
    AppendRequest, EventId, EventMetadataSource, EventPayload, EventStore, PrincipalKind,
    PrincipalRef, StreamId, StreamKind,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

#[derive(Default)]
struct Metadata(AtomicU64);
impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }
    fn now_ms(&self) -> i64 {
        42
    }
}

fn command(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pluribus"));
    command
        .arg("--data-dir")
        .arg(directory)
        .arg("--config-dir")
        .arg(directory)
        .arg("logs");
    command
}

async fn fixture(directory: &Path) -> SqliteEventStore<Metadata> {
    fs::write(directory.join("config.json"), br#"{"agent_id":"test"}"#).unwrap();
    SqliteEventStore::open(directory.join("pluribus.sqlite3"), Metadata::default())
        .await
        .unwrap()
}

async fn append(store: &SqliteEventStore<Metadata>, stream: &str, number: u64) {
    store
        .append(AppendRequest {
            stream_id: StreamId::new(stream),
            stream_kind: StreamKind::Agent,
            observed_at_ms: None,
            event_type: "test.event".into(),
            payload_schema: "test/1".into(),
            payload: EventPayload::CanonicalJson(
                serde_json::to_vec(&json!({"number":number})).unwrap(),
            ),
            actor: PrincipalRef::new(PrincipalKind::Node, "test"),
            authority_id: None,
            activity_id: None,
            correlation_id: None,
            causation_id: None,
            deduplication_key: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn logs_show_recent_agent_events_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let store = fixture(directory.path()).await;
    for number in 1..=3 {
        append(&store, "test", number).await;
    }
    append(&store, "other", 99).await;
    let output = command(directory.path())
        .args(["--limit", "2"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["sequence"], 2);
    assert_eq!(events[1]["payload"], json!({"number":3}));
    assert_eq!(events[1]["event_type"], "test.event");
    assert_eq!(events[1]["recorded_at_ms"], 42);
}

struct Follower(Child);
impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn follow_prints_existing_and_new_events_once_across_batches() {
    let directory = tempfile::tempdir().unwrap();
    let store = fixture(directory.path()).await;
    append(&store, "test", 1).await;
    let mut follower = Follower(
        command(directory.path())
            .args(["-f", "-n", "1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = follower.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let read = || {
        serde_json::from_str::<Value>(&receiver.recv_timeout(Duration::from_secs(10)).unwrap())
            .unwrap()
    };
    assert_eq!(read()["sequence"], 1);
    for number in 2..=106 {
        append(&store, "test", number).await;
    }
    for number in 2..=106 {
        assert_eq!(read()["sequence"], number);
    }
    assert!(receiver.recv_timeout(Duration::from_millis(400)).is_err());
}

#[test]
fn logs_do_not_create_a_missing_database() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("config.json"), b"{}").unwrap();
    let output = command(directory.path()).output().unwrap();
    assert!(!output.status.success());
    assert!(!directory.path().join("pluribus.sqlite3").exists());
}
