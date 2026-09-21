//! The send capabilities, driven against scripted peers.
//!
//! The peers speak over Unix sockets. What the host does above the socket —
//! resolving a name, checking a certificate, the STARTTLS preamble — is the
//! transport's, and is tested in `pluribus-host-stream`. What is tested here
//! is the component: the bytes it puts on the wire and the events it emits.

use pluribus_core::*;
use pluribus_plugin_package::PluginPackage;
use pluribus_runtime_wasm::{
    Delivery, GrantedStream, PluginInstance, PluginServices, Principal, PrincipalKind as Kind,
    Runtime, RuntimeLimits,
};
use pluribus_store_sqlite::SqliteEventStore;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

#[derive(Default)]
struct Metadata(AtomicU64);
impl EventMetadataSource for Metadata {
    fn next_event_id(&self) -> EventId {
        EventId::new(format!("event-{}", self.0.fetch_add(1, Ordering::Relaxed)))
    }
    fn now_ms(&self) -> i64 {
        1_700_000_000_000
    }
}

/// A peer's transcript: every byte the component sent it.
type Transcript = Arc<Mutex<Vec<u8>>>;

fn text(transcript: &Transcript) -> String {
    String::from_utf8_lossy(&transcript.lock().unwrap()).into_owned()
}

/// An SMTP peer answering a fixed script, one reply per command, and holding
/// its replies back while a `DATA` payload arrives.
async fn smtp_peer(path: &Path, replies: Vec<&'static str>) -> Transcript {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let transcript: Transcript = Arc::default();
    let seen = Arc::clone(&transcript);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (reader, mut writer) = socket.into_split();
            let mut reader = tokio::io::BufReader::new(reader);
            let mut replies = replies.clone().into_iter();
            let mut in_data = false;
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(&line);
                if in_data && line != b".\r\n" {
                    continue;
                }
                let Some(reply) = replies.next() else {
                    break;
                };
                if writer.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
                in_data = reply.starts_with("354");
            }
        }
    });
    transcript
}

/// The replies a submission that works answers with.
fn accepting() -> Vec<&'static str> {
    vec![
        "250-peer.example.net\r\n250-SIZE 26214400\r\n250 AUTH PLAIN LOGIN\r\n",
        "235 authenticated\r\n",
        "250 sender ok\r\n",
        "250 recipient ok\r\n",
        "354 go ahead\r\n",
        "250 2.0.0 queued as ABC\r\n",
        "221 bye\r\n",
    ]
}

/// An IMAP peer holding one read-only session: it answers the commands the
/// connector issues and then idles, as the real endpoint does.
async fn imap_peer(path: &Path) -> Transcript {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let transcript: Transcript = Arc::default();
    let seen = Arc::clone(&transcript);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (reader, mut writer) = socket.into_split();
            let mut reader = tokio::io::BufReader::new(reader);
            if writer.write_all(b"* OK ready\r\n").await.is_err() {
                break;
            }
            let mut idle_tag = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                seen.lock().unwrap().extend_from_slice(line.as_bytes());
                let line = line.trim_end();
                if line == "DONE" {
                    let reply = format!("{idle_tag} OK idle done\r\n");
                    let _ = writer.write_all(reply.as_bytes()).await;
                    continue;
                }
                let mut words = line.split(' ');
                let tag = words.next().unwrap_or_default().to_owned();
                let reply = match words.next().unwrap_or_default() {
                    "CAPABILITY" => {
                        format!("* CAPABILITY IMAP4rev1 IDLE\r\n{tag} OK done\r\n")
                    }
                    "LOGIN" => format!("{tag} OK logged in\r\n"),
                    "EXAMINE" => format!(
                        "* 0 EXISTS\r\n* OK [UIDVALIDITY 1] valid\r\n\
                         * OK [UIDNEXT 1] next\r\n{tag} OK [READ-ONLY] done\r\n"
                    ),
                    "UID" => format!("* SEARCH\r\n{tag} OK done\r\n"),
                    "IDLE" => {
                        idle_tag = tag;
                        "+ idling\r\n".to_owned()
                    }
                    _ => format!("{tag} BAD unexpected\r\n"),
                };
                if writer.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });
    transcript
}

struct Harness {
    store: Arc<SqliteEventStore<Metadata>>,
    runtime: Runtime,
    directory: tempfile::TempDir,
}

impl Harness {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Arc::new(
            SqliteEventStore::open_in_memory(Metadata::default())
                .await
                .unwrap(),
        );
        store
            .replace_plugin_credential(
                &SecretHandle::new("email:account"),
                "dev.pluribus.email",
                None,
                serde_json::to_vec(&json!({
                    "username": "ada@example.com",
                    "password": "app-specific",
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let runtime = Runtime::new(
            RuntimeLimits::default(),
            store.clone(),
            store.clone(),
            Arc::new(InMemoryBlobStore::default()),
            store.clone(),
        )
        .unwrap();
        Self {
            store,
            runtime,
            directory,
        }
    }

    fn endpoint(&self, name: &str) -> PathBuf {
        self.directory.path().join(format!("{name}.sock"))
    }

    fn granted(&self, endpoints: &[&str]) -> BTreeMap<String, GrantedStream> {
        endpoints
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    GrantedStream {
                        service: Arc::new(
                            pluribus_host_stream::LocalStreamService::new(self.directory.path())
                                .unwrap(),
                        ),
                        grant: StreamGrant {
                            endpoint: StreamEndpoint::Unix {
                                path: self.endpoint(name),
                                peer_uids: vec![rustix::process::geteuid().as_raw()],
                            },
                            max_bytes: 4 * 1024 * 1024,
                            max_timeout_ms: 5_000,
                            max_connections: 1,
                        },
                    },
                )
            })
            .collect()
    }

    async fn instance(&self, endpoints: &[&str]) -> PluginInstance {
        self.instance_with(endpoints, ["email:account".into()].into())
            .await
    }

    async fn instance_with(
        &self,
        endpoints: &[&str],
        handles: std::collections::HashSet<String>,
    ) -> PluginInstance {
        let package = PluginPackage::load(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugins/email"),
        )
        .unwrap();
        self.runtime
            .instantiate(
                &package.components()[""],
                &json!({
                    "credentials": {"account": "email:account"},
                    "display_name": "Ada Lovelace",
                }),
                Delivery {
                    instance_id: "email".into(),
                    agent: principal(),
                    actor: principal(),
                    authority_id: "test".into(),
                    activity_id: "test".into(),
                    correlation_id: "test".into(),
                    origin_event_id: "init".into(),
                    depth: 0,
                    deadline_at_ms: None,
                    visible_blobs: vec![],
                },
                PluginServices {
                    credentials: Some(pluribus_runtime_wasm::CredentialAccess {
                        exports: BTreeMap::new(),
                        store: self.store.clone(),
                        provider: "dev.pluribus.email".into(),
                        handles,
                    }),
                    streams: self.granted(endpoints),
                    ..PluginServices::default()
                },
            )
            .await
            .unwrap()
    }

    async fn request(&self, capability: &str, arguments: Value) -> CommittedEvent {
        self.store
            .append(AppendRequest {
                stream_id: StreamId::new("test"),
                stream_kind: StreamKind::Agent,
                observed_at_ms: None,
                event_type: "capability.requested".into(),
                payload_schema: "pluribus.capability-request/1".into(),
                payload: EventPayload::CanonicalJson(
                    serde_json::to_vec(&json!({
                        "capability": capability,
                        "arguments": arguments,
                    }))
                    .unwrap(),
                ),
                actor: PrincipalRef::new(PrincipalKind::Agent, "test"),
                authority_id: None,
                activity_id: None,
                correlation_id: None,
                causation_id: None,
                deduplication_key: None,
            })
            .await
            .unwrap()
    }
}

fn principal() -> Principal {
    Principal {
        kind: Kind::Agent,
        id: "test".into(),
    }
}

/// The one terminal event a capability request produces: its type, and the
/// payload it carries.
fn terminal(outcome: &pluribus_runtime_wasm::Outcome) -> (String, Value) {
    let event = outcome
        .events
        .iter()
        .find(|event| {
            matches!(
                event.request.event_type.as_str(),
                "capability.completed" | "capability.failed"
            )
        })
        .expect("a capability request answers with a terminal event");
    let EventPayload::CanonicalJson(bytes) = &event.request.payload else {
        panic!("JSON payload")
    };
    (
        event.request.event_type.clone(),
        serde_json::from_slice(bytes).unwrap(),
    )
}

fn reply_arguments() -> Value {
    json!({
        "conversationId": "email:INBOX",
        "to": "bob@example.net",
        "messageId": "parent@example.net",
        "references": ["root@example.net"],
        "subject": "Lunch",
        // A line beginning with a dot ends DATA unless it is stuffed.
        "text": "yes\n.and one more thing\ncafé",
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_is_submitted_as_a_threaded_message() {
    let harness = Harness::new().await;
    let peer = smtp_peer(&harness.endpoint("smtp"), accepting()).await;
    let mut instance = harness.instance(&["smtp"]).await;
    instance.init().await.unwrap();
    let request = harness.request("email.reply", reply_arguments()).await;
    let outcome = instance.handle(&[request]).await.unwrap();

    let (event_type, payload) = terminal(&outcome);
    assert_eq!(event_type, "capability.completed", "{payload}");
    assert_eq!(payload["output"]["recipients"], json!(["bob@example.net"]));
    assert!(
        payload["output"]["messageId"]
            .as_str()
            .unwrap()
            .ends_with("@example.com")
    );

    let wire = text(&peer);
    for expected in [
        "EHLO example.com\r\n",
        // AUTH PLAIN carries \0user\0password, base64 encoded.
        "AUTH PLAIN AGFkYUBleGFtcGxlLmNvbQBhcHAtc3BlY2lmaWM=\r\n",
        "MAIL FROM:<ada@example.com>\r\n",
        "RCPT TO:<bob@example.net>\r\n",
        "DATA\r\n",
        "From: \"Ada Lovelace\" <ada@example.com>\r\n",
        "To: bob@example.net\r\n",
        "Subject: Re: Lunch\r\n",
        "In-Reply-To: <parent@example.net>\r\n",
        "References: <root@example.net> <parent@example.net>\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        // The dot that would have ended DATA, stuffed.
        "\r\n..and one more thing\r\n",
        "caf=C3=A9\r\n",
        "\r\n.\r\n",
        "QUIT\r\n",
    ] {
        assert!(wire.contains(expected), "{expected:?} missing from {wire}");
    }
    assert!(!wire.contains("app-specific"), "the password is not plain");
    // The password reaches the wire once, in the AUTH command, and the
    // transcript ends where the session does.
    assert!(wire.ends_with("QUIT\r\n"), "{wire}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_send_names_its_own_recipients() {
    let harness = Harness::new().await;
    let mut replies = accepting();
    // One more recipient, one more reply.
    replies.insert(4, "250 recipient ok\r\n");
    let peer = smtp_peer(&harness.endpoint("smtp"), replies).await;
    let mut instance = harness.instance(&["smtp"]).await;
    instance.init().await.unwrap();
    let request = harness
        .request(
            "email.send-message",
            json!({
                "to": ["bob@example.net"],
                "cc": ["carol@example.net"],
                "subject": "Notes",
                "text": "plain",
            }),
        )
        .await;
    let outcome = instance.handle(&[request]).await.unwrap();
    let (event_type, payload) = terminal(&outcome);
    assert_eq!(event_type, "capability.completed", "{payload}");

    let wire = text(&peer);
    assert!(wire.contains("RCPT TO:<bob@example.net>\r\n"), "{wire}");
    assert!(wire.contains("RCPT TO:<carol@example.net>\r\n"), "{wire}");
    assert!(wire.contains("Cc: carol@example.net\r\n"), "{wire}");
    assert!(wire.contains("Subject: Notes\r\n"), "{wire}");
    assert!(!wire.contains("In-Reply-To"), "{wire}");
    // Nothing needed encoding, so the body travels as it stands.
    assert!(
        wire.contains("Content-Transfer-Encoding: 7bit\r\n"),
        "{wire}"
    );
}

/// A `5xx` is refused identically on every attempt; a `4xx` is the endpoint
/// asking to be tried later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_is_reported_as_permanent_or_retryable() {
    for (refusal, code) in [
        ("550 no such user\r\n", "invalid-argument"),
        ("451 try again later\r\n", "unavailable"),
    ] {
        let harness = Harness::new().await;
        let mut replies = accepting();
        replies[3] = refusal;
        let peer = smtp_peer(&harness.endpoint("smtp"), replies).await;
        let mut instance = harness.instance(&["smtp"]).await;
        instance.init().await.unwrap();
        let request = harness.request("email.reply", reply_arguments()).await;
        let outcome = instance.handle(&[request]).await.unwrap();
        let (event_type, payload) = terminal(&outcome);
        assert_eq!(event_type, "capability.failed", "{payload}");
        assert_eq!(payload["code"], code, "{payload}");
        assert!(
            payload["reason"].as_str().unwrap().contains("RCPT TO"),
            "{payload}"
        );
        // A refused recipient means no message was offered.
        assert!(!text(&peer).contains("DATA\r\n"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_credential_is_permanent() {
    let harness = Harness::new().await;
    let mut replies = accepting();
    replies[1] = "535 authentication failed\r\n";
    let peer = smtp_peer(&harness.endpoint("smtp"), replies).await;
    let mut instance = harness.instance(&["smtp"]).await;
    instance.init().await.unwrap();
    let request = harness.request("email.reply", reply_arguments()).await;
    let outcome = instance.handle(&[request]).await.unwrap();
    let (event_type, payload) = terminal(&outcome);
    assert_eq!(event_type, "capability.failed", "{payload}");
    assert_eq!(payload["code"], "permission-denied", "{payload}");
    assert!(!text(&peer).contains("MAIL FROM"));
}

/// A capability request is owed an answer even when the instance cannot send:
/// rejecting the delivery would leave the agent waiting on nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unenrolled_credential_answers_the_request() {
    let harness = Harness::new().await;
    let peer = smtp_peer(&harness.endpoint("smtp"), accepting()).await;
    let mut instance = harness
        .instance_with(&["smtp"], std::collections::HashSet::new())
        .await;
    instance.init().await.unwrap();
    let request = harness.request("email.reply", reply_arguments()).await;
    let outcome = instance.handle(&[request]).await.unwrap();
    let (event_type, payload) = terminal(&outcome);
    assert_eq!(event_type, "capability.failed", "{payload}");
    assert_eq!(payload["code"], "permission-denied", "{payload}");
    assert!(text(&peer).is_empty(), "the endpoint was never reached");
}

/// The name selects among the grants. One the operator did not grant reaches
/// nothing, however the component spells it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endpoint_the_grant_does_not_name_is_denied() {
    let harness = Harness::new().await;
    let peer = smtp_peer(&harness.endpoint("smtp"), accepting()).await;
    // Only the IMAP endpoint is granted, so `connect("smtp")` names one that
    // is not there even though a peer is listening on it.
    let mut instance = harness.instance(&["imap"]).await;
    instance.init().await.unwrap();
    let request = harness.request("email.reply", reply_arguments()).await;
    let outcome = instance.handle(&[request]).await.unwrap();
    let (event_type, payload) = terminal(&outcome);
    assert_eq!(event_type, "capability.failed", "{payload}");
    assert_eq!(payload["code"], "permission-denied", "{payload}");
    assert!(text(&peer).is_empty(), "the endpoint was never reached");
}

/// One component holds both endpoints at once: the IMAP session stays where
/// it was while a send runs on its own connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_leaves_the_imap_session_where_it_was() {
    let harness = Harness::new().await;
    let mailbox = imap_peer(&harness.endpoint("imap")).await;
    let peer = smtp_peer(&harness.endpoint("smtp"), accepting()).await;
    let mut instance = harness.instance(&["imap", "smtp"]).await;
    instance.init().await.unwrap();
    instance.start();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !text(&mailbox).contains("IDLE") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the session reaches IDLE");
    let idling = text(&mailbox);

    let request = harness.request("email.reply", reply_arguments()).await;
    let outcome = instance.handle(&[request]).await.unwrap();
    let (event_type, payload) = terminal(&outcome);
    assert_eq!(event_type, "capability.completed", "{payload}");
    assert!(text(&peer).contains("MAIL FROM:<ada@example.com>\r\n"));

    // The session issued no command of its own while the send ran, and it is
    // still where it was: a stop finds it idling, not reconnecting.
    assert_eq!(text(&mailbox), idling);
    instance.stop(i64::MAX).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_initial_imap_cursor_is_stored_before_an_empty_catch_up() {
    let harness = Harness::new().await;
    let mailbox = imap_peer(&harness.endpoint("imap")).await;
    let mut instance = harness.instance(&["imap"]).await;
    instance.init().await.unwrap();
    instance.start();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !text(&mailbox).contains("IDLE") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the session reaches IDLE");

    let snapshot = StateStore::get(
        &*harness.store,
        &StateNamespace::new("email"),
        "mailbox/INBOX",
    )
    .await
    .unwrap();
    let value = snapshot.value.expect("the initial cursor is committed");
    let cursor: serde_json::Value = serde_json::from_slice(&value).unwrap();
    assert_eq!(cursor["uidvalidity"], 1);
    assert_eq!(cursor["last_uid"], 0);
    instance.stop(i64::MAX).await.unwrap();
}
