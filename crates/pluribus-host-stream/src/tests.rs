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
                deadline: Instant::now() + Duration::from_secs(2),
            }),
        );
        peers.push(peer);
    }
    peers[0].write_all(b"abcd").unwrap();
    let page = service
        .receive("read", 64, 500, &AtomicBool::new(false))
        .await
        .unwrap();
    assert_eq!(page.bytes, b"abcd");
    assert!(!page.closed);
    assert!(matches!(
        service
            .receive("read", 64, 100, &AtomicBool::new(false))
            .await,
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
async fn receive_reports_peer_close_and_honours_cancellation() {
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
            deadline: Instant::now() + Duration::from_secs(2),
        }),
    );
    assert!(matches!(
        service
            .receive("stream:1", 64, 200, &AtomicBool::new(true))
            .await,
        Err(StreamError::Cancelled)
    ));
    drop(peer);
    let page = service
        .receive("stream:1", 64, 500, &AtomicBool::new(false))
        .await
        .unwrap();
    assert!(page.closed);
    assert!(page.bytes.is_empty());
}

#[tokio::test]
async fn idle_stream_does_not_block_another_stream() {
    let (_dir, service) = service();
    let service = std::sync::Arc::new(service);
    let mut peers = Vec::new();
    for id in ["idle", "ready"] {
        let (client, peer) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        service.streams.lock().unwrap().insert(
            id.into(),
            Arc::new(Stream {
                socket: AsyncFd::new(client).unwrap(),
                remaining: Mutex::new(1024),
                writes: tokio::sync::Mutex::new(()),
                deadline: Instant::now() + Duration::from_secs(2),
            }),
        );
        peers.push(peer);
    }
    let reader = service.clone();
    let worker = tokio::spawn(async move {
        reader
            .receive("idle", 64, 400, &AtomicBool::new(false))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    peers[1].write_all(b"ready").unwrap();
    let started = Instant::now();
    let result = service
        .receive("ready", 64, 100, &AtomicBool::new(false))
        .await;
    let elapsed = started.elapsed();
    eprintln!("ready stream latency with one idle stream: {elapsed:?}");
    worker.await.unwrap().unwrap();
    assert_eq!(result.unwrap().bytes, b"ready");
    assert!(
        elapsed < Duration::from_millis(100),
        "ready stream blocked for {elapsed:?}"
    );
}

#[tokio::test]
async fn many_idle_streams_leave_the_executor_responsive() {
    let (_dir, service) = service();
    let service = Arc::new(service);
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut peers = Vec::new();
    let mut workers = Vec::new();
    for index in 0..128 {
        let (socket, peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        let id = index.to_string();
        service.streams.lock().unwrap().insert(
            id.clone(),
            Arc::new(Stream {
                socket: AsyncFd::new(socket).unwrap(),
                remaining: Mutex::new(1024),
                writes: tokio::sync::Mutex::new(()),
                deadline: Instant::now() + Duration::from_secs(2),
            }),
        );
        peers.push(peer);
        let service = service.clone();
        let cancelled = cancelled.clone();
        workers.push(tokio::spawn(async move {
            service.receive(&id, 64, 1000, &cancelled).await
        }));
    }
    tokio::task::yield_now().await;
    let started = Instant::now();
    peers[127].write_all(b"ready").unwrap();
    let ready = tokio::time::timeout(Duration::from_millis(100), workers.pop().unwrap())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(ready.bytes, b"ready");
    cancelled.store(true, Ordering::Release);
    for worker in workers {
        assert!(matches!(worker.await.unwrap(), Err(StreamError::Cancelled)));
    }
    eprintln!("ready stream latency with 127 idle streams: {elapsed:?}");
}
