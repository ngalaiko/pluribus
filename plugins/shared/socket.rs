// Byte channel to the one granted socket endpoint. Framing is the caller's.

use crate::pluribus::plugin::{
    socket,
    types::{Chunk, Error, ErrorCode},
};

/// One connected endpoint: the outgoing writer, the peer's bytes, and the
/// transport completion.
pub struct Socket {
    outgoing: Option<wit_bindgen::StreamWriter<u8>>,
    incoming: wit_bindgen::StreamReader<u8>,
    completion: Option<wit_bindgen::FutureReader<Result<(), Error>>>,
    ended: bool,
}

impl Socket {
    pub async fn connect() -> Result<Self, Error> {
        let (outgoing, peer) = crate::wit_stream::new::<u8>();
        let (incoming, completion) = socket::connect(peer).await?;
        Ok(Self {
            outgoing: Some(outgoing),
            incoming,
            completion: Some(completion),
            ended: false,
        })
    }

    /// Writes `bytes` to the peer.
    pub async fn send(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let outgoing = self
            .outgoing
            .as_mut()
            .ok_or_else(|| endpoint_error(ErrorCode::Unavailable, "outgoing stream is closed"))?;
        if outgoing.write_all(bytes.to_vec()).await.is_empty() {
            Ok(())
        } else {
            Err(endpoint_error(
                ErrorCode::Unavailable,
                "peer stopped reading",
            ))
        }
    }

    /// Reads at most `max` bytes. A timeout yields an empty open chunk. End of
    /// stream yields a closed chunk and surfaces the transport error, if any.
    pub async fn read(&mut self, max: u32, timeout_ms: Option<u32>) -> Result<Chunk, Error> {
        if self.ended {
            return Ok(Chunk {
                bytes: vec![],
                closed: true,
            });
        }
        if max == 0 {
            return Err(endpoint_error(
                ErrorCode::InvalidArgument,
                "read size must be positive",
            ));
        }
        let (bytes, timed_out) = {
            let mut read = std::pin::pin!(
                self.incoming
                    .read(Vec::with_capacity(max.min(64 * 1024) as usize))
            );
            if let Some(ms) = timeout_ms {
                let timer = std::pin::pin!(crate::wasi::clocks::monotonic_clock::wait_for(
                    u64::from(ms) * 1_000_000
                ));
                match futures_util::future::select(read.as_mut(), timer).await {
                    futures_util::future::Either::Left(((_, bytes), _)) => (bytes, false),
                    futures_util::future::Either::Right(((), read)) => {
                        let (_, bytes) = read.cancel();
                        (bytes, true)
                    }
                }
            } else {
                let (_, bytes) = read.await;
                (bytes, false)
            }
        };
        let closed = bytes.is_empty() && !timed_out;
        if closed {
            self.ended = true;
            self.completion.take().unwrap().await?;
        }
        Ok(Chunk { bytes, closed })
    }
}

fn endpoint_error(code: ErrorCode, message: &str) -> Error {
    Error {
        code,
        message: message.to_owned(),
        retryable: matches!(code, ErrorCode::Unavailable),
        details: None,
    }
}
