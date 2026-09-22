use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn service() -> (TempDir, LocalStreamService) {
    let dir = TempDir::new().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let service = LocalStreamService::new(dir.path()).unwrap();
    (dir, service)
}

#[tokio::test]
async fn concurrent_reads_cannot_overdraw_the_grant_budget() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    socket.set_nonblocking(true).unwrap();
    let stream = Stream {
        transport: Transport::Unix(AsyncFd::new(socket).unwrap()),
        remaining: Mutex::new(5),
        writes: tokio::sync::Mutex::new(()),
    };
    let first = stream.take_read_budget(4).unwrap();
    let second = stream.take_read_budget(4).unwrap();
    assert_eq!((first, second), (4, 4));
    assert_eq!(
        stream
            .finish_read(vec![1; first], first, first)
            .unwrap()
            .bytes
            .len(),
        4
    );
    assert_eq!(
        stream.finish_read(vec![1; second], second, second),
        Err(StreamError::LimitExceeded)
    );
    assert_eq!(stream.take_read_budget(1), Err(StreamError::LimitExceeded));
}

#[tokio::test]
async fn a_pending_read_does_not_block_a_write() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (_dir, service) = service();
    let (client, peer) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    service.streams.lock().unwrap().insert(
        "duplex".into(),
        Arc::new(Stream {
            transport: Transport::Unix(AsyncFd::new(client).unwrap()),
            remaining: Mutex::new(32),
            writes: tokio::sync::Mutex::new(()),
        }),
    );
    let mut peer = tokio::net::UnixStream::from_std(peer).unwrap();
    let peer_task = tokio::spawn(async move {
        let mut request = [0; 4];
        peer.read_exact(&mut request).await.unwrap();
        peer.write_all(b"pong").await.unwrap();
    });
    let (sent, page) = tokio::time::timeout(Duration::from_millis(100), async {
        tokio::join!(service.send("duplex", b"ping"), service.next("duplex", 16))
    })
    .await
    .unwrap();
    sent.unwrap();
    assert_eq!(page.unwrap().bytes, b"pong");
    peer_task.await.unwrap();
}

fn grant(path: &Path, peer_uid: u32) -> StreamGrant {
    StreamGrant {
        endpoint: StreamEndpoint::Unix {
            path: path.to_owned(),
            peer_uids: vec![peer_uid],
        },
        max_bytes: 1024,
        max_timeout_ms: 2000,
        max_connections: 1,
    }
}

#[test]
fn runtime_data_must_be_private() {
    let dir = TempDir::new().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(LocalStreamService::new(dir.path()).is_err());
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    LocalStreamService::new(dir.path()).unwrap();
}

#[tokio::test]
async fn shared_accounts_root_and_relative_paths_are_rejected() {
    let (_dir, service) = service();
    let own = rustix::process::geteuid().as_raw();
    for endpoint in [
        grant(Path::new("/tmp/endpoint.sock"), own),
        grant(Path::new("/tmp/endpoint.sock"), 0),
        grant(Path::new("endpoint.sock"), own + 1),
    ] {
        assert!(service.open(&endpoint).await.is_err());
    }
}

#[test]
fn socket_peer_credentials_accept_only_a_listed_uid() {
    let (client, _) = UnixStream::pair().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    verify_peer(&client, &[uid]).unwrap();
    verify_peer(&client, &[uid + 1, uid]).unwrap();
    assert!(verify_peer(&client, &[uid + 1]).is_err());
    assert!(verify_peer(&client, &[]).is_err());
}

#[tokio::test]
async fn unknown_streams_and_closed_streams_are_unavailable() {
    let (_dir, service) = service();
    assert!(matches!(
        service.send("stream:1", b"x").await,
        Err(StreamError::Unavailable(_))
    ));
    service.close("stream:1");
}

/// A breached ceiling tears the stream down, so each direction needs its own.
#[tokio::test]
async fn transfer_is_bounded_by_the_grant() {
    let (dir, _) = service();
    let service = LocalStreamService::new(dir.path()).unwrap();
    let mut peers = Vec::new();
    for id in ["read", "write"] {
        let (client, peer) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        service.streams.lock().unwrap().insert(
            id.into(),
            Arc::new(Stream {
                transport: Transport::Unix(AsyncFd::new(client).unwrap()),
                remaining: Mutex::new(4),
                writes: tokio::sync::Mutex::new(()),
            }),
        );
        peers.push(peer);
    }
    peers[0].write_all(b"abcd").unwrap();
    let page = service.next("read", 64).await.unwrap();
    assert_eq!(page.bytes, b"abcd");
    assert!(!page.closed);
    assert!(matches!(
        service.next("read", 64).await,
        Err(StreamError::LimitExceeded)
    ));
    assert!(matches!(
        service.send("write", b"toolong").await,
        Err(StreamError::LimitExceeded)
    ));
    assert!(matches!(
        service.send("write", b"ok").await,
        Err(StreamError::Unavailable(_))
    ));
}

#[tokio::test]
async fn reads_report_peer_close() {
    let (dir, _) = service();
    let service = LocalStreamService::new(dir.path()).unwrap();
    let (client, peer) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    service.streams.lock().unwrap().insert(
        "stream:1".into(),
        Arc::new(Stream {
            transport: Transport::Unix(AsyncFd::new(client).unwrap()),
            remaining: Mutex::new(1024),
            writes: tokio::sync::Mutex::new(()),
        }),
    );
    drop(peer);
    let page = service.next("stream:1", 64).await.unwrap();
    assert!(page.closed);
    assert!(page.bytes.is_empty());
}

/// Reads wait for the peer rather than an idle deadline, and stop at the
/// grant's byte budget.
#[tokio::test]
async fn reads_wait_for_the_peer_and_enforce_the_byte_budget() {
    let (_dir, service) = service();
    let (client, mut peer) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    service.streams.lock().unwrap().insert(
        "sub".into(),
        Arc::new(Stream {
            transport: Transport::Unix(AsyncFd::new(client).unwrap()),
            remaining: Mutex::new(3),
            writes: tokio::sync::Mutex::new(()),
        }),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), service.next("sub", 10))
            .await
            .is_err()
    );
    peer.write_all(b"abc").unwrap();
    assert_eq!(service.next("sub", 10).await.unwrap().bytes, b"abc");
    peer.write_all(b"d").unwrap();
    assert_eq!(
        service.next("sub", 10).await,
        Err(StreamError::LimitExceeded)
    );
    service.close("sub");
    assert!(service.streams.lock().unwrap().is_empty());
}

fn tls_grant(hostname: &str, port: u16, allow_private_network: bool) -> StreamGrant {
    StreamGrant {
        endpoint: StreamEndpoint::Tls {
            hostname: hostname.to_owned(),
            port,
            allow_private_network,
            starttls: None,
        },
        max_bytes: 1024,
        max_timeout_ms: 2000,
        max_connections: 1,
    }
}

#[tokio::test]
async fn remote_services_refuse_local_endpoints() {
    let service = LocalStreamService::remote();
    let own = rustix::process::geteuid().as_raw();
    assert!(matches!(
        service
            .open(&grant(Path::new("/tmp/endpoint.sock"), own))
            .await,
        Err(StreamError::Denied(_))
    ));
}

#[tokio::test]
async fn tls_endpoints_must_name_a_host_not_an_address() {
    let service = LocalStreamService::remote();
    for hostname in ["127.0.0.1", "::1", ""] {
        assert!(
            matches!(
                service.open(&tls_grant(hostname, 993, true)).await,
                Err(StreamError::Denied(_))
            ),
            "{hostname} must be denied"
        );
    }
}

/// Loopback is the address a rebinding answer aims at, so it needs the opt-in.
#[tokio::test]
async fn private_destinations_require_an_explicit_grant() {
    let service = LocalStreamService::remote();
    assert!(matches!(
        service.open(&tls_grant("localhost", 993, false)).await,
        Err(StreamError::Denied(_))
    ));
}

#[tokio::test]
async fn a_grant_of_no_connections_denies() {
    let service = LocalStreamService::remote();
    let grant = StreamGrant {
        max_connections: 0,
        ..tls_grant("localhost", 993, true)
    };
    assert!(matches!(
        service.open(&grant).await,
        Err(StreamError::Denied(_))
    ));
}

/// A self-signed endpoint chains to nothing the binary trusts.
#[tokio::test]
#[ignore = "requires loopback sockets"]
async fn an_untrusted_certificate_is_denied() {
    let (port, _server) = tls_echo_server().await;
    let service = LocalStreamService::remote();
    assert!(matches!(
        service.open(&tls_grant("localhost", port, true)).await,
        Err(StreamError::Denied(_))
    ));
}

#[tokio::test]
async fn connections_are_bounded_by_the_grant() {
    let (dir, service) = service();
    let path = dir.path().join("endpoint.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });
    let grant = grant(&path, rustix::process::geteuid().as_raw());
    let first = service.open(&grant).await.unwrap();
    assert!(matches!(
        service.open(&grant).await,
        Err(StreamError::LimitExceeded)
    ));
    // The ceiling counts live connections, not connections ever made.
    service.close(&first);
    service.open(&grant).await.unwrap();
}

/// A TLS listener with a self-signed certificate, and the task serving it.
#[cfg(test)]
async fn tls_echo_server() -> (u16, tokio::task::JoinHandle<()>) {
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(key.cert.der().to_vec())],
        PrivateKeyDer::try_from(key.signing_key.serialize_der()).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let task = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let Ok(mut stream) = acceptor.accept(socket).await else {
                    return;
                };
                let mut bytes = [0; 64];
                while let Ok(n) = stream.read(&mut bytes).await {
                    if n == 0 || stream.write_all(&bytes[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, task)
}

/// A plaintext peer answering a fixed script, one reply per command line,
/// and recording what it was asked. It never speaks TLS: these tests are
/// about the preamble the host owns.
async fn scripted_smtp(
    script: Vec<&'static str>,
) -> (u16, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let (reader, mut writer) = socket.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut script = script.into_iter();
        if let Some(greeting) = script.next()
            && writer.write_all(greeting.as_bytes()).await.is_err()
        {
            return;
        }
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            recorded.lock().unwrap().push(line.trim_end().to_owned());
            let Some(reply) = script.next() else {
                return;
            };
            if writer.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    (port, seen, task)
}

fn starttls_grant(port: u16) -> StreamGrant {
    StreamGrant {
        endpoint: StreamEndpoint::Tls {
            hostname: "localhost".into(),
            port,
            allow_private_network: true,
            starttls: Some(pluribus_core::StartTls::Smtp),
        },
        max_bytes: 1024,
        max_timeout_ms: 5000,
        max_connections: 1,
    }
}

/// The host speaks the whole preamble, multiline `250-` replies included, and
/// only then hands the connection to TLS. A self-signed peer fails the
/// handshake, which is proof the preamble got that far.
#[tokio::test]
#[ignore = "requires loopback sockets"]
async fn a_starttls_preamble_runs_to_the_handshake() {
    let (port, seen, _peer) = scripted_smtp(vec![
        "220 peer ESMTP\r\n",
        "250-peer\r\n250-SIZE 26214400\r\n250 STARTTLS\r\n",
        "220 go ahead\r\n",
    ])
    .await;
    let service = LocalStreamService::remote();
    let error = service.open(&starttls_grant(port)).await.unwrap_err();
    assert!(
        matches!(&error, StreamError::Denied(message) if message.contains("handshake")),
        "{error:?}"
    );
    let asked = seen.lock().unwrap().clone();
    assert!(asked[0].starts_with("EHLO "), "{asked:?}");
    assert_eq!(asked[1], "STARTTLS");
}

/// The guest may not be handed a connection that stayed plaintext.
#[tokio::test]
#[ignore = "requires loopback sockets"]
async fn an_endpoint_that_will_not_upgrade_is_denied() {
    let (port, _seen, _peer) = scripted_smtp(vec![
        "220 peer ESMTP\r\n",
        "250 peer\r\n",
        "502 command not implemented\r\n",
    ])
    .await;
    let service = LocalStreamService::remote();
    let error = service.open(&starttls_grant(port)).await.unwrap_err();
    assert!(
        matches!(&error, StreamError::Denied(message) if message.contains("STARTTLS")),
        "{error:?}"
    );
}

/// A peer asking to be tried later has not refused the upgrade, so the
/// failure stays retryable rather than ending the source.
#[tokio::test]
#[ignore = "requires loopback sockets"]
async fn a_busy_endpoint_stays_retryable() {
    let (port, _seen, _peer) = scripted_smtp(vec!["421 too many connections\r\n"]).await;
    let service = LocalStreamService::remote();
    assert!(matches!(
        service.open(&starttls_grant(port)).await,
        Err(StreamError::Unavailable(_))
    ));
}

/// Bytes arriving before the handshake would be indistinguishable from the
/// encrypted session's own output once it starts.
#[tokio::test]
#[ignore = "requires loopback sockets"]
async fn data_sent_before_the_handshake_is_refused() {
    let (port, _seen, _peer) = scripted_smtp(vec![
        "220 peer ESMTP\r\n",
        "250 peer\r\n",
        "220 go ahead\r\n250 injected\r\n",
    ])
    .await;
    let service = LocalStreamService::remote();
    let error = service.open(&starttls_grant(port)).await.unwrap_err();
    assert!(
        matches!(&error, StreamError::Denied(message) if message.contains("before the TLS handshake")),
        "{error:?}"
    );
}
