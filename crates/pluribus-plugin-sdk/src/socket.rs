// Byte channel to a granted socket endpoint. Framing is the caller's.

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
    /// Connects the endpoint granted under `endpoint`. The name selects
    /// among the grants; it does not name a destination.
    pub async fn connect(endpoint: &str) -> Result<Self, Error> {
        let (outgoing, peer) = crate::wit_stream::new::<u8>();
        let (incoming, completion) = socket::connect(endpoint.to_owned(), peer).await?;
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
    ///
    /// The end of the stream is taken from what the transport reported, not
    /// from an empty read: a timeout that races a transport ending returns
    /// no bytes either way, and starting another read on an ended stream
    /// traps.
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
        let (status, bytes) = {
            let mut read = std::pin::pin!(
                self.incoming
                    .read(Vec::with_capacity(max.min(64 * 1024) as usize))
            );
            if let Some(ms) = timeout_ms {
                let timer = std::pin::pin!(crate::wasi::clocks::monotonic_clock::wait_for(
                    u64::from(ms) * 1_000_000
                ));
                match futures_util::future::select(read.as_mut(), timer).await {
                    futures_util::future::Either::Left((result, _)) => result,
                    futures_util::future::Either::Right(((), read)) => read.cancel(),
                }
            } else {
                read.await
            }
        };
        let closed = !matches!(status, wit_bindgen::StreamResult::Cancelled) && bytes.is_empty();
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
