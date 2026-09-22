use crate::protocol::{
    ComponentFrame, CoreReply, CoreRequest, ExecutorFrame, MAX_CORE_BYTES, MAX_OUTPUT, MAX_REQUEST,
    Request, Response,
};
use crate::{TICK, peer_uid, validate_separation, verify_peer};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, Signal, kill_process_group};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Serves one shell activity at a time for the configured runtime UID.
///
/// # Errors
/// Returns identity, workspace, socket, or listener failures.
pub fn serve(
    socket: &Path,
    runtime_uid: u32,
    workspace: &Path,
    same_account: bool,
    config: &crate::config::Config,
) -> io::Result<()> {
    validate_separation(runtime_uid, same_account)?;
    if !socket.is_absolute() || !workspace.is_absolute() || !workspace.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "absolute socket and workspace directory required",
        ));
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o660))?;
    info!(
        socket = %socket.display(),
        runtime_uid,
        workspace = %workspace.display(),
        "executor listening"
    );
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
        let result = handle_configured(&mut stream, workspace, config);
        let response = result.unwrap_or_else(|error| Response::Unavailable {
            message: error.to_string(),
        });
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        if let Ok(mut bytes) = serde_json::to_vec(&ExecutorFrame::Result { response }) {
            bytes.push(b'\n');
            let _ = stream.write_all(&bytes);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn handle(stream: &mut UnixStream, workspace: &Path) -> io::Result<Response> {
    handle_configured(stream, workspace, &crate::config::Config::default())
}

fn handle_configured(
    stream: &mut UnixStream,
    workspace: &Path,
    config: &crate::config::Config,
) -> io::Result<Response> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut bytes = Vec::new();
    BufReader::new((&mut *stream).take(MAX_REQUEST as u64 + 1)).read_until(b'\n', &mut bytes)?;
    if bytes.len() > MAX_REQUEST || bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process frame",
        ));
    }
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|_| io::Error::other("invalid process request"))?;
    request.validate().map_err(io::Error::other)?;
    stream.set_nonblocking(true)?;
    let mut cancel_stream = stream.try_clone()?;
    execute_stream(&request, workspace, config, stream, move || {
        disconnected(&mut cancel_stream)
    })
}

fn disconnected(stream: &mut UnixStream) -> bool {
    !matches!(stream.read(&mut [0; 1]), Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted))
}

struct Activity(Child);

impl Drop for Activity {
    fn drop(&mut self) {
        if let Some(pid) = i32::try_from(self.0.id()).ok().and_then(Pid::from_raw) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct PendingCore {
    request: CoreRequest,
    reply: mpsc::Sender<CoreReply>,
}

struct CoreBridge {
    path: PathBuf,
    requests: mpsc::Receiver<PendingCore>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CoreBridge {
    fn start() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "pluribus-core-{}-{}-{}.sock",
            rustix::process::geteuid().as_raw(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (sender, requests) = mpsc::channel();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let (mut client, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = client.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = Vec::new();
                let read = BufReader::new((&mut client).take((MAX_REQUEST + 1) as u64))
                    .read_until(b'\n', &mut line);
                if read.is_err() || line.len() > MAX_REQUEST || line.last() != Some(&b'\n') {
                    continue;
                }
                let Ok(request) = serde_json::from_slice::<CoreRequest>(&line) else {
                    continue;
                };
                if request.operation.validate().is_err() {
                    continue;
                }
                let (reply, result) = mpsc::channel();
                if sender.send(PendingCore { request, reply }).is_err() {
                    break;
                }
                let result = loop {
                    match result.recv_timeout(Duration::from_millis(100)) {
                        Ok(result) => break result,
                        Err(mpsc::RecvTimeoutError::Timeout)
                            if !worker_stop.load(Ordering::Relaxed) =>
                        {
                            continue;
                        }
                        _ => {
                            break CoreReply {
                                id: 0,
                                bytes: None,
                                error: Some("core access closed".into()),
                                closed: None,
                            };
                        }
                    }
                };
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                let _ = send_core_reply(&mut client, &result);
            }
        });
        Ok(Self {
            path,
            requests,
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for CoreBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn core_round_trip(
    stream: &mut UnixStream,
    request: CoreRequest,
    deadline: Instant,
) -> io::Result<CoreReply> {
    let id = request.id;
    let mut frame =
        serde_json::to_vec(&ExecutorFrame::CoreRequest { request }).map_err(io::Error::other)?;
    if frame.len() > MAX_REQUEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "core request too large",
        ));
    }
    frame.push(b'\n');
    stream.set_nonblocking(false).map_err(|error| {
        io::Error::new(error.kind(), format!("make core stream blocking: {error}"))
    })?;
    let result = (|| {
        stream
            .set_write_timeout(Some(deadline.saturating_duration_since(Instant::now())))
            .map_err(|error| {
                io::Error::new(error.kind(), format!("set core write timeout: {error}"))
            })?;
        stream.write_all(&frame)?;
        stream
            .set_read_timeout(Some(deadline.saturating_duration_since(Instant::now())))
            .map_err(|error| {
                io::Error::new(error.kind(), format!("set core read timeout: {error}"))
            })?;
        let mut line = Vec::new();
        BufReader::new(&mut *stream)
            .take((MAX_CORE_BYTES * 5 + 4096) as u64)
            .read_until(b'\n', &mut line)?;
        let ComponentFrame::CoreReply(reply) = serde_json::from_slice(&line).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid core response frame")
        })?;
        if reply.id != id
            || reply
                .bytes
                .as_ref()
                .is_some_and(|bytes| bytes.len() > MAX_CORE_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid core response",
            ));
        }
        Ok(reply)
    })();
    stream.set_nonblocking(true)?;
    result
}

fn send_core_reply(stream: &mut UnixStream, reply: &CoreReply) -> io::Result<()> {
    let mut encoded = serde_json::to_vec(reply).map_err(io::Error::other)?;
    encoded.push(b'\n');
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut written = 0;
    while written < encoded.len() {
        match stream.write(&encoded[written..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(TICK);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::ErrorKind::TimedOut.into());
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn execute(
    request: &Request,
    workspace: &Path,
    cancelled: impl FnMut() -> bool,
) -> io::Result<Response> {
    execute_configured(
        request,
        workspace,
        &crate::config::Config::default(),
        cancelled,
    )
}

#[cfg(test)]
pub(super) fn execute_configured(
    request: &Request,
    workspace: &Path,
    config: &crate::config::Config,
    cancelled: impl FnMut() -> bool,
) -> io::Result<Response> {
    execute_configured_inner(request, workspace, config, None, cancelled)
}

fn execute_stream(
    request: &Request,
    workspace: &Path,
    config: &crate::config::Config,
    stream: &mut UnixStream,
    cancelled: impl FnMut() -> bool,
) -> io::Result<Response> {
    execute_configured_inner(request, workspace, config, Some(stream), cancelled)
}

fn execute_configured_inner(
    request: &Request,
    workspace: &Path,
    config: &crate::config::Config,
    mut runtime: Option<&mut UnixStream>,
    mut cancelled: impl FnMut() -> bool,
) -> io::Result<Response> {
    request.validate().map_err(io::Error::other)?;
    if cancelled() {
        return Ok(Response::Cancelled);
    }
    let deadline = Instant::now() + Duration::from_millis(u64::from(request.timeout_ms));
    config.validate()?;
    if cancelled() {
        return Ok(Response::Cancelled);
    }
    if Instant::now() >= deadline {
        return Ok(Response::DeadlineExceeded);
    }
    let bridge = CoreBridge::start()?;
    let mut activity = Activity(
        Command::new("/bin/sh")
            .arg("-c")
            .arg(&request.command)
            .current_dir(workspace)
            .env_clear()
            .env("HOME", workspace)
            .env("PATH", &config.path)
            .env("LANG", "C.UTF-8")
            .envs(&request.env)
            .env("PLURIBUS_CORE_SOCKET", &bridge.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?,
    );
    let mut stdout = activity
        .0
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout"))?;
    let mut stderr = activity
        .0
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut remaining = MAX_OUTPUT;
    let mut truncated = false;
    loop {
        if cancelled() {
            return Ok(Response::Cancelled);
        }
        if Instant::now() >= deadline {
            return Ok(Response::DeadlineExceeded);
        }
        drain(&mut stdout, &mut out, &mut remaining, &mut truncated)?;
        drain(&mut stderr, &mut err, &mut remaining, &mut truncated)?;
        if let Ok(pending) = bridge.requests.try_recv() {
            let Some(runtime) = runtime.as_deref_mut() else {
                pending
                    .reply
                    .send(CoreReply {
                        id: pending.request.id,
                        bytes: None,
                        error: Some("core access unavailable".into()),
                        closed: None,
                    })
                    .ok();
                continue;
            };
            let reply = core_round_trip(runtime, pending.request, deadline)?;
            let _ = pending.reply.send(reply);
        }
        if let Some(status) = activity.0.try_wait()? {
            drop(activity);
            drain(&mut stdout, &mut out, &mut remaining, &mut truncated)?;
            drain(&mut stderr, &mut err, &mut remaining, &mut truncated)?;
            return Ok(Response::Completed {
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                exit_code: status.code(),
                truncated,
            });
        }
        thread::sleep(TICK);
    }
}

fn nonblocking(fd: impl AsFd) -> io::Result<()> {
    fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
    Ok(())
}

fn drain(
    reader: &mut impl Read,
    output: &mut Vec<u8>,
    remaining: &mut usize,
    truncated: &mut bool,
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    for _ in 0..32 {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(*remaining);
                output.extend_from_slice(&buffer[..keep]);
                *remaining -= keep;
                *truncated |= keep != count;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod core_tests {
    use super::*;
    use crate::protocol::CoreOperation;

    #[test]
    fn helper_requests_are_forwarded_and_replied_to() {
        let bridge = CoreBridge::start().unwrap();
        let mut helper = UnixStream::connect(&bridge.path).unwrap();
        let request = CoreRequest {
            id: 1,
            operation: CoreOperation::Secret {
                binding: "GH_TOKEN".into(),
            },
        };
        let mut encoded = serde_json::to_vec(&request).unwrap();
        encoded.push(b'\n');
        helper.write_all(&encoded).unwrap();

        let pending = bridge
            .requests
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(pending.request.id, request.id);
        pending
            .reply
            .send(CoreReply {
                id: request.id,
                bytes: Some(b"secret-value".to_vec()),
                error: None,
                closed: None,
            })
            .unwrap();
        let mut reply = Vec::new();
        BufReader::new(helper)
            .read_until(b'\n', &mut reply)
            .unwrap();
        let reply: CoreReply = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply.bytes.as_deref(), Some(b"secret-value".as_slice()));
        assert_eq!(reply.error, None);
    }

    #[test]
    fn runtime_stream_round_trip_preserves_core_reply_id() {
        let (mut host, mut component) = UnixStream::pair().unwrap();
        let responder = thread::spawn(move || {
            let mut line = Vec::new();
            BufReader::new(&mut component)
                .read_until(b'\n', &mut line)
                .unwrap();
            let frame: ExecutorFrame = serde_json::from_slice(&line).unwrap();
            let ExecutorFrame::CoreRequest { request } = frame else {
                panic!("expected core request")
            };
            let reply = ComponentFrame::CoreReply(CoreReply {
                id: request.id,
                bytes: Some(b"ok".to_vec()),
                error: None,
                closed: None,
            });
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            component.write_all(&bytes).unwrap();
        });
        host.set_nonblocking(true).unwrap();
        let reply = core_round_trip(
            &mut host,
            CoreRequest {
                id: 17,
                operation: CoreOperation::Secret {
                    binding: "GH_TOKEN".into(),
                },
            },
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        responder.join().unwrap();
        assert_eq!(reply.id, 17);
        assert_eq!(reply.bytes.as_deref(), Some(b"ok".as_slice()));
    }

    #[test]
    fn core_reply_write_is_bounded_when_peer_does_not_read() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        rustix::net::sockopt::set_socket_recv_buffer_size(&reader, 4096).unwrap();
        let reply = CoreReply {
            id: 1,
            bytes: Some(vec![b'x'; MAX_CORE_BYTES]),
            error: None,
            closed: None,
        };
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let _ = done.send(send_core_reply(&mut writer, &reply));
        });
        let error = finished
            .recv_timeout(Duration::from_secs(3))
            .expect("bounded response write")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
