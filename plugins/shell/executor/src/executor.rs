use crate::protocol::{MAX_OUTPUT, MAX_REQUEST, Request, Response};
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
use std::process::{Child, Command, Stdio};
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
        if let Ok(mut bytes) = serde_json::to_vec(&response) {
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
    execute_configured(&request, workspace, config, || disconnected(stream))
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

pub(super) fn execute_configured(
    request: &Request,
    workspace: &Path,
    config: &crate::config::Config,
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
