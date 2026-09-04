//! The terminal half of the cli plugin: it owns stdin and stdout, and answers
//! the component over a unix socket.
//!
//! The component always connects; this always listens, as the shell executor
//! does. Input the person types is numbered so a restart resumes in order.

mod bridge;

use clap::Parser;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// An endpoint lives in the agent's data directory, named for the instance and
/// component it answers.
const ENDPOINT: &str = "cli-main.sock";

#[derive(Parser)]
#[command(about = "Talk to a Pluribus agent from a terminal")]
struct Args {
    /// Directory holding the endpoint. Defaults to the agent's runtime
    /// directory, which the platform decides.
    #[arg(long, short = 'd')]
    data_dir: Option<PathBuf>,
    /// Endpoint the component connects to, when it is not the conventional one.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// UID the component's runtime runs as. Defaults to this account.
    #[arg(long)]
    runtime_uid: Option<u32>,
}

fn main() {
    pluribus_log::init();
    let args = Args::parse();
    let data = args.data_dir.unwrap_or_else(pluribus_paths::runtime);
    let socket = match args.socket {
        Some(socket) => socket,
        None => match data.canonicalize() {
            Ok(data) => data.join(ENDPOINT),
            Err(error) => {
                eprintln!("error: {}: {error}", data.display());
                std::process::exit(1);
            }
        },
    };
    let runtime_uid = args
        .runtime_uid
        .unwrap_or_else(|| rustix::process::geteuid().as_raw());
    if let Err(error) = bridge::serve(&socket, runtime_uid) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
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

pub(crate) fn verify_peer(stream: &UnixStream, expected: u32) -> io::Result<()> {
    if peer_uid(stream)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unexpected runtime socket peer",
        ));
    }
    Ok(())
}
