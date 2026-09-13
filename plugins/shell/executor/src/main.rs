//! Runs shell commands under the workspace account, one connection at a time.

// Each side uses a subset of the shared wire constants.
#[path = "../../protocol/src/lib.rs"]
#[allow(dead_code)]
mod protocol;

mod config;
mod executor;

use clap::Parser;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const TICK: Duration = Duration::from_millis(10);

/// An endpoint lives in the agent's data directory, named for the instance and
/// component it answers.
const ENDPOINT: &str = "shell-main.sock";

#[derive(Parser)]
#[command(about = "Execute shell activities for a Pluribus agent")]
struct Args {
    #[arg(long, default_value = "/usr/local/bin:/usr/bin:/bin")]
    path: String,
    /// Directory holding the endpoint. Defaults to the agent's runtime
    /// directory, which the platform decides.
    #[arg(long, short = 'd')]
    data_dir: Option<PathBuf>,
    /// Endpoint the component connects to, when it is not the conventional one.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// UID the agent runs as. Defaults to this account, which lets commands run
    /// with this account's authority.
    #[arg(long)]
    runtime_uid: Option<u32>,
    /// Directory commands start in. Defaults to the current one.
    #[arg(long)]
    workspace: Option<PathBuf>,
}

fn main() {
    pluribus_log::init();
    let args = Args::parse();
    let own_uid = rustix::process::geteuid().as_raw();
    let runtime_uid = args.runtime_uid.unwrap_or(own_uid);
    if runtime_uid == own_uid {
        tracing::warn!(
            runtime_uid,
            "serving a runtime on this account; commands run with its authority. \
             A deployment runs the agent as another account and names it with --runtime-uid."
        );
    }
    // The endpoint does not exist yet, so its directory is what resolves.
    let directory = |path: &Path, name: &str| {
        path.canonicalize()
            .map_err(|error| format!("{name}: {}: {error}", path.display()))
    };
    let data = args
        .data_dir
        .clone()
        .unwrap_or_else(pluribus_paths::runtime);
    let result = args
        .socket
        .clone()
        .map_or_else(
            || directory(&data, "data directory").map(|data| data.join(ENDPOINT)),
            Ok,
        )
        .and_then(|socket| {
            let workspace = match args.workspace.clone() {
                Some(workspace) => workspace,
                None => directory(Path::new("."), "workspace")?,
            };
            let config =
                config::Config::from_flags(args.path.clone()).map_err(|error| error.to_string())?;
            executor::serve(
                &socket,
                runtime_uid,
                &workspace,
                runtime_uid == own_uid,
                &config,
            )
            .map_err(|error| error.to_string())
        });
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Rejects a root account, and a peer sharing the executor's account unless
/// the operator asked for that.
pub(crate) fn validate_separation(peer_uid: u32, same_account: bool) -> io::Result<()> {
    let own_uid = rustix::process::geteuid().as_raw();
    if own_uid == 0 || peer_uid == 0 || (own_uid == peer_uid && !same_account) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime and executor require distinct non-root accounts",
        ));
    }
    Ok(())
}

pub(crate) fn verify_peer(stream: &UnixStream, expected: u32) -> io::Result<()> {
    if peer_uid(stream)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected runtime socket peer",
        ));
    }
    Ok(())
}

/// Reads the peer's effective UID from the kernel, which the peer cannot forge.
///
/// Hosts without a peer-credential call fail to build rather than accepting
/// unverified peers.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let (uid, _) = nix::unistd::getpeereid(stream)?;
    Ok(uid.as_raw())
}

#[cfg(test)]
mod tests;
