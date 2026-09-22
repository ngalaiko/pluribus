use super::*;
use crate::protocol::{MAX_OUTPUT, Request, Response};
use std::fs;
use std::io::Write;
use std::net::Shutdown;
use std::thread;
use std::time::Instant;
use tempfile::TempDir;

fn request(command: &str) -> Request {
    Request {
        version: crate::protocol::VERSION,
        env: Default::default(),
        command: command.into(),
        timeout_ms: 2000,
        invocation_id: "invocation".into(),
        authority_id: "authority".into(),
        activity_id: "activity".into(),
        origin_event_id: "origin".into(),
    }
}

#[test]
fn captures_output_exit_status_and_workspace_without_ambient_environment() {
    let dir = TempDir::new().unwrap();
    let response = executor::execute(
        &request("printf hello; printf problem >&2; pwd > location; env > environment; exit 7"),
        dir.path(),
        || false,
    )
    .unwrap();
    let Response::Completed {
        stdout,
        stderr,
        exit_code,
        truncated,
    } = response
    else {
        panic!("{response:?}")
    };
    assert_eq!(
        (stdout.as_str(), stderr.as_str(), exit_code, truncated),
        ("hello", "problem", Some(7), false)
    );
    let location = fs::read_to_string(dir.path().join("location")).unwrap();
    assert_eq!(
        fs::canonicalize(location.trim()).unwrap(),
        fs::canonicalize(dir.path()).unwrap()
    );
    let environment = fs::read_to_string(dir.path().join("environment")).unwrap();
    for line in environment.lines() {
        assert!(
            [
                "HOME=",
                "PATH=",
                "LANG=",
                "PWD=",
                "SHLVL=",
                "_=",
                "PLURIBUS_CORE_SOCKET=",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix)),
            "{line}"
        );
    }
}

#[test]
fn timeout_and_cancellation_kill_the_process_group() {
    for cancel in [false, true] {
        let dir = TempDir::new().unwrap();
        let mut request = request("(sleep 0.3; touch escaped) & wait");
        request.timeout_ms = if cancel { 2000 } else { 50 };
        let started = Instant::now();
        let response = executor::execute(&request, dir.path(), || {
            cancel && started.elapsed() > Duration::from_millis(50)
        })
        .unwrap();
        assert!(
            matches!(
                (&response, cancel),
                (Response::Cancelled, true) | (Response::DeadlineExceeded, false)
            ),
            "{response:?}"
        );
        thread::sleep(Duration::from_millis(400));
        assert!(!dir.path().join("escaped").exists());
    }
}

#[test]
fn disconnect_cancels_an_active_socket_request() {
    let dir = TempDir::new().unwrap();
    let workspace = dir.path().to_owned();
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || executor::handle(&mut server, &workspace).unwrap());
    let mut bytes =
        serde_json::to_vec(&request("touch started; sleep 0.3; touch escaped")).unwrap();
    bytes.push(b'\n');
    client.write_all(&bytes).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !dir.path().join("started").exists() {
        assert!(Instant::now() < until);
        thread::sleep(TICK);
    }
    client.shutdown(Shutdown::Both).unwrap();
    assert!(matches!(worker.join().unwrap(), Response::Cancelled));
    thread::sleep(Duration::from_millis(350));
    assert!(!dir.path().join("escaped").exists());
}

#[test]
fn output_is_bounded_without_blocking_the_child() {
    let dir = TempDir::new().unwrap();
    // Draining megabytes competes with the sleeping tests, so bound generously.
    let mut request = request("head -c 2097152 /dev/zero; printf done >&2");
    request.timeout_ms = 30_000;
    let response = executor::execute(&request, dir.path(), || false).unwrap();
    let Response::Completed {
        stdout,
        stderr,
        truncated,
        exit_code,
    } = response
    else {
        panic!("{response:?}")
    };
    assert!(truncated);
    assert!(stdout.len() + stderr.len() <= MAX_OUTPUT);
    assert_eq!(exit_code, Some(0));
}

#[test]
fn invalid_requests_and_shared_accounts_are_rejected() {
    for command in ["", "bad\0command"] {
        assert!(request(command).validate().is_err());
    }
    let mut version = request("printf hello");
    version.version = crate::protocol::VERSION + 1;
    assert!(version.validate().is_err());
    let own = rustix::process::geteuid().as_raw();
    assert!(validate_separation(own, false).is_err());
    // Sharing the account is the operator's call, and never applies to root.
    assert!(validate_separation(own, true).is_ok());
    assert!(validate_separation(0, true).is_err());
}

#[test]
fn socket_peer_credentials_reject_another_uid() {
    let (client, _) = UnixStream::pair().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    verify_peer(&client, uid).unwrap();
    assert!(verify_peer(&client, uid + 1).is_err());
}

#[test]
fn request_environment_reaches_child() {
    let dir = TempDir::new().unwrap();
    let mut request = request("test \"$GH_TOKEN\" = fixture-token");
    request
        .env
        .insert("GH_TOKEN".into(), "fixture-token".into());
    let response = executor::execute(&request, dir.path(), || false).unwrap();
    assert!(matches!(
        response,
        Response::Completed {
            exit_code: Some(0),
            ..
        }
    ));
}

#[test]
fn invalid_environment_prevents_spawn() {
    let dir = TempDir::new().unwrap();
    for (name, value) in [
        ("PATH", "/tmp"),
        ("HOME", "/tmp"),
        ("LANG", "x"),
        ("1TOKEN", "x"),
        ("TOKEN=bad", "x"),
        ("TOKEN", "bad\0token"),
    ] {
        let mut request = request("touch started");
        request.env.insert(name.into(), value.into());
        assert!(executor::execute(&request, dir.path(), || false).is_err());
        assert!(!dir.path().join("started").exists());
    }
    let mut request = request("touch started");
    request
        .env
        .insert("TOKEN".into(), "x".repeat(16 * 1024 + 1));
    assert!(executor::execute(&request, dir.path(), || false).is_err());
    assert!(!dir.path().join("started").exists());
}

#[test]
fn malformed_frames_do_not_echo_values() {
    let dir = TempDir::new().unwrap();
    let (mut client, mut server) = UnixStream::pair().unwrap();
    client
        .write_all(b"{\"version\":\"fixture-token\"}\n")
        .unwrap();
    let error = executor::handle(&mut server, dir.path()).unwrap_err();
    assert!(!error.to_string().contains("fixture-token"));
}
