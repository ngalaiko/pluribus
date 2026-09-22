use crate::protocol::{
    CoreOperation, CoreReply, CoreRequest, MAX_CORE_BYTES, MAX_CORE_FRAME_BYTES,
};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

pub fn run(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let command = args.next();
    if matches!(command.as_deref(), Some("--help" | "-h")) {
        println!("{}", help());
        return Ok(());
    }
    let operation = match command.as_deref() {
        Some("secret") => CoreOperation::Secret {
            binding: match args.next().as_deref() {
                Some("--help" | "-h") => {
                    println!(
                        "usage: pluribus-shell-cli secret BINDING\nRead an explicitly granted credential export to stdout."
                    );
                    return Ok(());
                }
                Some(binding) => binding.to_owned(),
                None => return Err(usage()),
            },
        },
        Some("attachment") => {
            let digest = match args.next().as_deref() {
                Some("--help" | "-h") => {
                    println!(
                        "usage: pluribus-shell-cli attachment SHA256_DIGEST\nWrite a visible attachment's raw bytes to stdout."
                    );
                    return Ok(());
                }
                Some(digest) => digest.to_owned(),
                None => return Err(usage()),
            };
            if args.next().is_some() {
                return Err(usage());
            }
            let socket = core_socket_path()?;
            return download_attachment(&digest, &mut std::io::stdout(), |operation| {
                core_call_reply(&socket, operation)
            });
        }
        Some("upload") => {
            let path = match args.next().as_deref() {
                Some("--help" | "-h") => {
                    println!(
                        "usage: pluribus-shell-cli upload PATH [MEDIA_TYPE]\nStore a workspace file as a blob and print its reference as JSON."
                    );
                    return Ok(());
                }
                Some(path) => path.to_owned(),
                None => return Err(usage()),
            };
            let media_type = args
                .next()
                .unwrap_or_else(|| "application/octet-stream".into());
            if args.next().is_some() {
                return Err(usage());
            }
            let socket = core_socket_path()?;
            return upload(
                Path::new(&path),
                &media_type,
                &mut std::io::stdout(),
                |operation| core_call_reply(&socket, operation),
            );
        }
        Some("--help" | "-h") => {
            println!("{}", help());
            return Ok(());
        }
        _ => return Err(usage()),
    };
    if args.next().is_some() {
        return Err(usage());
    }
    let socket = core_socket_path()?;
    std::io::stdout().write_all(&core_call(&socket, operation)?)
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: pluribus-shell-cli secret BINDING | pluribus-shell-cli attachment SHA256_DIGEST | pluribus-shell-cli upload PATH [MEDIA_TYPE]",
    )
}

fn help() -> &'static str {
    "usage: pluribus-shell-cli <secret|attachment|upload> ...\n\nCommands:\n  secret BINDING  Read an explicitly granted credential export to stdout\n  attachment SHA256_DIGEST  Read a current-delivery-visible attachment to stdout\n  upload PATH [MEDIA_TYPE]  Store a file as a blob and print its reference as JSON\n\nPipe output directly to a command. Keep secrets out of logs and output."
}

fn core_socket_path() -> io::Result<std::path::PathBuf> {
    std::env::var_os("PLURIBUS_CORE_SOCKET")
        .map(Into::into)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "core access unavailable"))
}

fn upload(
    path: &Path,
    media_type: &str,
    output: &mut impl Write,
    mut call: impl FnMut(crate::protocol::CoreOperation) -> io::Result<CoreReply>,
) -> io::Result<()> {
    use crate::protocol::{CoreOperation, MAX_UPLOAD_CHUNK};

    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let handle = core_bytes(
        &mut call,
        CoreOperation::AttachmentOpen {
            media_type: media_type.to_owned(),
            size,
        },
    )?;
    let handle = String::from_utf8(handle)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid upload handle"))?;
    let mut offset = 0_u64;
    let mut chunk = vec![0; MAX_UPLOAD_CHUNK];
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let next = core_bytes(
            &mut call,
            CoreOperation::AttachmentWrite {
                handle: handle.clone(),
                offset,
                bytes: chunk[..count].to_vec(),
            },
        )?;
        let reported = std::str::from_utf8(&next)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid upload offset"))?;
        offset = offset
            .checked_add(count as u64)
            .filter(|expected| *expected == reported)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "upload offset mismatch"))?;
    }
    if offset != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file changed during upload",
        ));
    }
    let reference = core_bytes(&mut call, CoreOperation::AttachmentFinish { handle })?;
    output.write_all(&reference)
}

fn core_call(socket: &Path, operation: crate::protocol::CoreOperation) -> io::Result<Vec<u8>> {
    core_bytes(
        &mut |operation| core_call_reply(socket, operation),
        operation,
    )
}

fn core_bytes(
    call: &mut impl FnMut(crate::protocol::CoreOperation) -> io::Result<CoreReply>,
    operation: crate::protocol::CoreOperation,
) -> io::Result<Vec<u8>> {
    let reply = call(operation)?;
    match (reply.bytes, reply.error) {
        (Some(bytes), None) if bytes.len() <= MAX_CORE_BYTES => Ok(bytes),
        (None, Some(error)) => Err(io::Error::new(io::ErrorKind::PermissionDenied, error)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid core reply",
        )),
    }
}

fn core_call_reply(
    socket: &Path,
    operation: crate::protocol::CoreOperation,
) -> io::Result<CoreReply> {
    operation.validate().map_err(io::Error::other)?;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let request = CoreRequest { id: 1, operation };
    let mut encoded = serde_json::to_vec(&request).map_err(io::Error::other)?;
    encoded.push(b'\n');
    stream.write_all(&encoded)?;
    let mut line = Vec::new();
    BufReader::new(stream)
        .take(MAX_CORE_FRAME_BYTES as u64)
        .read_until(b'\n', &mut line)?;
    let reply: CoreReply = serde_json::from_slice(&line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid core reply"))?;
    if reply.id != request.id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mismatched core reply",
        ));
    }
    Ok(reply)
}

fn download_attachment(
    digest: &str,
    output: &mut impl Write,
    mut call: impl FnMut(crate::protocol::CoreOperation) -> io::Result<CoreReply>,
) -> io::Result<()> {
    use crate::protocol::{CoreOperation, MAX_UPLOAD_CHUNK};

    let mut offset = 0_u64;
    loop {
        let reply = call(CoreOperation::Attachment {
            digest: digest.to_owned(),
            offset,
            max_bytes: MAX_UPLOAD_CHUNK as u32,
        })?;
        if let Some(error) = reply.error {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, error));
        }
        let bytes = reply.bytes.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing attachment bytes")
        })?;
        if bytes.len() > MAX_UPLOAD_CHUNK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attachment chunk too large",
            ));
        }
        output.write_all(&bytes)?;
        offset = offset.checked_add(bytes.len() as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "attachment size overflow")
        })?;
        match reply.closed {
            Some(true) => return Ok(()),
            Some(false) if !bytes.is_empty() => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "invalid attachment stream",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_core_reply_fits_the_reader_limit() {
        let reply = CoreReply {
            id: 1,
            bytes: Some(vec![255; MAX_CORE_BYTES]),
            error: None,
            closed: None,
        };
        let encoded = serde_json::to_vec(&reply).unwrap();
        assert!(encoded.len() <= MAX_CORE_FRAME_BYTES);
    }

    #[test]
    fn upload_streams_workspace_file_and_returns_blob_ref() {
        use crate::protocol::{CoreOperation, CoreReply};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("photo.jpg");
        let contents = vec![42_u8; 1024 * 1024 + 17];
        std::fs::write(&path, &contents).unwrap();
        let expected = contents.clone();
        let mut received = Vec::new();
        let mut offset = 0_u64;
        let mut call = |operation| {
            let bytes = match operation {
                CoreOperation::AttachmentOpen { media_type, size } => {
                    assert_eq!(media_type, "image/jpeg");
                    assert_eq!(size, expected.len() as u64);
                    b"upload-1".to_vec()
                }
                CoreOperation::AttachmentWrite {
                    handle,
                    offset: got,
                    bytes,
                } => {
                    assert_eq!(handle, "upload-1");
                    assert_eq!(got, offset);
                    offset += bytes.len() as u64;
                    received.extend(bytes);
                    offset.to_string().into_bytes()
                }
                CoreOperation::AttachmentFinish { handle } => {
                    assert_eq!(handle, "upload-1");
                    assert_eq!(received, expected);
                    br#"{"algorithm":"sha256","digest":"abc","size":1048593,"media_type":"image/jpeg"}"#.to_vec()
                }
                _ => panic!("unexpected operation"),
            };
            Ok(CoreReply {
                id: 1,
                bytes: Some(bytes),
                error: None,
                closed: None,
            })
        };
        let mut output = Vec::new();
        upload(&path, "image/jpeg", &mut output, &mut call).unwrap();
        let reference: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(reference["digest"], "abc");
        assert_eq!(received.len(), contents.len());
    }

    #[test]
    fn download_streams_large_attachment_without_buffer_limit() {
        use crate::protocol::{CoreOperation, CoreReply, MAX_UPLOAD_CHUNK};

        let expected = vec![91_u8; 1024 * 1024 + 29];
        let mut offset = 0_u64;
        let mut call = |operation| {
            let CoreOperation::Attachment {
                digest,
                offset: got,
                max_bytes,
            } = operation
            else {
                panic!("unexpected operation")
            };
            assert_eq!(digest, "a".repeat(64));
            assert_eq!(got, offset);
            assert_eq!(max_bytes, MAX_UPLOAD_CHUNK as u32);
            let end = (offset as usize + MAX_UPLOAD_CHUNK).min(expected.len());
            let bytes = expected[offset as usize..end].to_vec();
            offset = end as u64;
            Ok(CoreReply {
                id: 1,
                bytes: Some(bytes),
                error: None,
                closed: Some(end == expected.len()),
            })
        };
        let mut output = Vec::new();
        download_attachment(&"a".repeat(64), &mut output, &mut call).unwrap();
        assert_eq!(output, expected);
    }
}
