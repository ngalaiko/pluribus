use serde_json::Value;
use std::{
    io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::Path,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
pub const MAX_FRAME: usize = 40 * 1024 * 1024;
/// Binds a Unix socket with the requested permissions.
///
/// # Errors
/// Returns an error for an invalid or occupied path, or filesystem failure.
pub fn bind(path: &Path, mode: u32) -> io::Result<tokio::net::UnixListener> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute socket required"));
    }
    if let Ok(m) = path.symlink_metadata() {
        if !m.file_type().is_socket()
            || m.uid() != rustix::process::geteuid().as_raw()
            || UnixStream::connect(path).is_ok()
        {
            return Err(io::Error::other("socket path is occupied"));
        }
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}
/// Reads one JSON frame.
///
/// # Errors
/// Returns an error for a timeout, invalid frame, or I/O failure.
pub async fn read(stream: &mut tokio::net::UnixStream) -> io::Result<Value> {
    let mut bytes = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::io::BufReader::new(stream.take(MAX_FRAME as u64 + 1)).read_until(b'\n', &mut bytes),
    )
    .await??;
    if bytes.len() > MAX_FRAME || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid frame"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
/// Writes one JSON frame.
///
/// # Errors
/// Returns an error for an oversized frame, timeout, or I/O failure.
pub async fn write(stream: &mut tokio::net::UnixStream, value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::other("response too large"));
    }
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(&bytes)).await??;
    Ok(())
}
