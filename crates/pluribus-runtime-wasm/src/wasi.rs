//! WASI primitives under the instance's scheduling and resource limits.

use super::*;
use bindings::wasi::{
    clocks::{monotonic_clock, system_clock, types as clock_types},
    random::random,
};
use runner::HostData;
use std::sync::OnceLock;
use wasmtime::component::Accessor;

fn monotonic_now() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    u64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl clock_types::Host for HostState {}
impl system_clock::Host for HostState {
    async fn now(&mut self) -> system_clock::Instant {
        let ms = (self.clock)();
        system_clock::Instant {
            seconds: ms.div_euclid(1000),
            nanoseconds: (ms.rem_euclid(1000) as u32) * 1_000_000,
        }
    }
    async fn get_resolution(&mut self) -> u64 {
        1_000_000
    }
}
impl monotonic_clock::Host for HostState {
    async fn now(&mut self) -> u64 {
        monotonic_now()
    }
    async fn get_resolution(&mut self) -> u64 {
        1
    }
}
impl<T> monotonic_clock::HostWithStore<T> for HostData {
    async fn wait_for(accessor: &Accessor<T, Self>, duration: u64) {
        let (cancelled, stopping, source) = accessor.with(|mut access| {
            let host = access.get();
            (
                host.cancel_notify.clone(),
                host.runner.stopping || host.cancelled.load(Ordering::Acquire),
                host.runner.active,
            )
        });
        if stopping {
            return;
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_nanos(duration)) => {},
            () = cancelled.notified() => {},
        }
        if source {
            runner::resume(accessor);
        }
    }
    async fn wait_until(accessor: &Accessor<T, Self>, when: u64) {
        Self::wait_for(accessor, when.saturating_sub(monotonic_now())).await;
    }
}
impl random::Host for HostState {
    async fn get_random_bytes(&mut self, max_len: u64) -> wasmtime::Result<Vec<u8>> {
        let mut bytes = vec![0; max_len.min(1024) as usize];
        getrandom::fill(&mut bytes)
            .map_err(|_| wasmtime::format_err!("operating system randomness unavailable"))?;
        Ok(bytes)
    }
    async fn get_random_u64(&mut self) -> wasmtime::Result<u64> {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes)
            .map_err(|_| wasmtime::format_err!("operating system randomness unavailable"))?;
        Ok(u64::from_le_bytes(bytes))
    }
}

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use wasmtime::{
    StoreContextMut,
    component::{Destination, FutureReader, StreamProducer, StreamReader, StreamResult, VecBuffer},
};

type ReadFuture = Pin<Box<dyn Future<Output = Result<types::Chunk, types::Error>> + Send>>;

struct SocketBytes {
    service: Arc<dyn StreamService>,
    id: String,
    cancelled: Arc<tokio::sync::Notify>,
    read: Option<ReadFuture>,
    completion: Option<tokio::sync::oneshot::Sender<Result<(), types::Error>>>,
}

impl StreamProducer<HostState> for SocketBytes {
    type Item = u8;
    type Buffer = VecBuffer<u8>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, HostState>,
        mut dst: Destination<'a, u8, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if self.read.is_none() {
            let service = self.service.clone();
            let id = self.id.clone();
            let cancelled = self.cancelled.clone();
            self.read = Some(Box::pin(async move {
                loop {
                    let page = tokio::select! {
                        page = service.next(&id, 32 * 1024) => page.map_err(stream_plugin_error)?,
                        () = cancelled.notified() => return Err(host_error(types::ErrorCode::Cancelled, "source cancelled")),
                    };
                    if page.closed || !page.bytes.is_empty() {
                        return Ok(types::Chunk {
                            bytes: page.bytes,
                            closed: page.closed,
                        });
                    }
                    tokio::task::yield_now().await;
                }
            }));
        }
        let result =
            if store.data().runner.stopping || store.data().cancelled.load(Ordering::Acquire) {
                Poll::Ready(Err(host_error(
                    types::ErrorCode::Cancelled,
                    "source stopped",
                )))
            } else {
                self.read.as_mut().unwrap().as_mut().poll(cx)
            };
        match result {
            Poll::Pending if finish => {
                self.read = None;
                Poll::Ready(Ok(StreamResult::Cancelled))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.read = None;
                let host = store.data_mut();
                host.call_deadline = Some(Instant::now() + host.runner.timeout);
                host.stream_deadline = host
                    .stream_grant
                    .as_ref()
                    .map(|g| Instant::now() + Duration::from_millis(u64::from(g.max_timeout_ms)));
                let (bytes, closed, outcome) = match result {
                    Ok(chunk) => (chunk.bytes, chunk.closed, Ok(())),
                    Err(error) => (vec![], true, Err(error)),
                };
                dst.set_buffer(bytes.into());
                if closed {
                    if let Some(tx) = self.completion.take() {
                        let _ = tx.send(outcome);
                    }
                    Poll::Ready(Ok(StreamResult::Dropped))
                } else {
                    Poll::Ready(Ok(StreamResult::Completed))
                }
            }
        }
    }
}

impl reader::HostReaderWithStore<HostState> for HostData {
    async fn read_via_stream(
        accessor: &Accessor<HostState, Self>,
        read: Resource<reader::Reader>,
    ) -> Result<(StreamReader<u8>, FutureReader<Result<(), types::Error>>), types::Error> {
        let (_, cancelled) = runner::cancellation(accessor)?;
        accessor.with(|mut access| {
            let host = access.get();
            let (service, id) = match host.read_target(&read)?.1 {
                Target::Socket(id) => (host.stream_access()?.0, id),
            };
            if !host.streaming_readers.insert(read.rep()) {
                return Err(host_error(
                    types::ErrorCode::Conflict,
                    "reader already streaming",
                ));
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            let stream = StreamReader::new(
                &mut access,
                SocketBytes {
                    service,
                    id,
                    cancelled,
                    read: None,
                    completion: Some(tx),
                },
            )
            .map_err(|_| {
                host_error(
                    types::ErrorCode::ResourceExhausted,
                    "stream allocation failed",
                )
            })?;
            let completion = match FutureReader::new(&mut access, async move {
                wasmtime::error::Ok(rx.await.unwrap_or_else(|_| {
                    Err(host_error(types::ErrorCode::Cancelled, "stream dropped"))
                }))
            }) {
                Ok(future) => future,
                Err(_) => {
                    let mut stream = stream;
                    let _ = stream.close(&mut access);
                    return Err(host_error(
                        types::ErrorCode::ResourceExhausted,
                        "future allocation failed",
                    ));
                }
            };
            Ok((stream, completion))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::component::{Source, StreamConsumer};
    struct FirstBytes(Option<tokio::sync::oneshot::Sender<Vec<u8>>>);
    impl StreamConsumer<HostState> for FirstBytes {
        type Item = u8;
        fn poll_consume(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            store: StoreContextMut<HostState>,
            mut source: Source<'_, u8>,
            _: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            let mut bytes = Vec::with_capacity(32 * 1024);
            source.read(store, &mut bytes)?;
            if !bytes.is_empty() {
                let _ = self.0.take().unwrap().send(bytes);
                Poll::Ready(Ok(StreamResult::Dropped))
            } else {
                Poll::Ready(Ok(StreamResult::Completed))
            }
        }
    }
    #[tokio::test]
    async fn native_stream_is_lazy_and_exclusive() {
        let (mut store, _, _) = crate::runner::tests::source().await;
        assert!(socket::Host::listen(store.data_mut()).await.is_err());
        let service = Arc::new(crate::tests::SubscriptionFixture::default());
        store.data_mut().stream = Some(service.clone());
        store.data_mut().stream_grant = Some(StreamGrant {
            endpoint: pluribus_core::StreamEndpoint::Unix {
                path: "/unused".into(),
                peer_uids: vec![1],
            },
            max_bytes: 1024,
            max_timeout_ms: 1000,
        });
        let (read, _write) = socket::Host::listen(store.data_mut()).await.unwrap();
        store
            .run_concurrent(async |accessor| {
                let host_access = accessor.with_getter::<HostData>(|h| h);
                let (stream, mut done) = reader::HostReaderWithStore::read_via_stream(
                    &host_access,
                    Resource::new_borrow(read.rep()),
                )
                .await
                .unwrap();
                assert_eq!(service.reads.load(Ordering::Acquire), 0);
                assert!(
                    reader::HostReaderWithStore::read_via_stream(
                        &host_access,
                        Resource::new_borrow(read.rep())
                    )
                    .await
                    .is_err()
                );
                let (tx, rx) = tokio::sync::oneshot::channel();
                accessor.with(|access| stream.pipe(access, FirstBytes(Some(tx))).unwrap());
                assert_eq!(rx.await.unwrap(), vec![1]);
                assert_eq!(service.reads.load(Ordering::Acquire), 1);
                done.close_with(accessor).unwrap();
            })
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn standard_clocks_preserve_units_and_random_reads_are_bounded() {
        let mut host = crate::tests::host(vec![]).await;
        host.clock = Arc::new(|| -1);
        let time = system_clock::Host::now(&mut host).await;
        assert_eq!((time.seconds, time.nanoseconds), (-1, 999_000_000));
        assert!(
            random::Host::get_random_bytes(&mut host, 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            random::Host::get_random_bytes(&mut host, u64::MAX)
                .await
                .unwrap()
                .len(),
            1024
        );
        let before = monotonic_clock::Host::now(&mut host).await;
        let after = monotonic_clock::Host::now(&mut host).await;
        assert!(after >= before);
    }
}
