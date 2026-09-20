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

fn grant(path: &Path, peer_uid: u32) -> StreamGrant {
    StreamGrant {
        endpoint: StreamEndpoint::Unix {
            path: path.to_owned(),
            peer_uids: vec![peer_uid],
        },
        max_bytes: 1024,
        max_timeout_ms: 2000,
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
                socket: AsyncFd::new(client).unwrap(),
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
            socket: AsyncFd::new(client).unwrap(),
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
            socket: AsyncFd::new(client).unwrap(),
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
