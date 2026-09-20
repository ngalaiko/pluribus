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
    AsContextMut, StoreContextMut,
    component::{
        Destination, FutureReader, Source, StreamConsumer, StreamProducer, StreamReader,
        StreamResult, VecBuffer,
    },
};

const READ_CHUNK_BYTES: u32 = 32 * 1024;
const WRITE_CHUNK_BYTES: usize = 32 * 1024;

type ReadFuture = Pin<Box<dyn Future<Output = Result<types::Chunk, types::Error>> + Send>>;

/// Incoming half: peer bytes, read on demand and without an idle deadline.
/// Ends on EOF, transport failure, byte-budget exhaustion, or cancellation.
struct SocketBytes {
    transport: Arc<Transport>,
    cancelled: Arc<tokio::sync::Notify>,
    read: Option<ReadFuture>,
    completion: Option<tokio::sync::oneshot::Sender<Result<(), types::Error>>>,
}

impl Drop for SocketBytes {
    fn drop(&mut self) {
        self.transport.finish();
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(self.transport.outcome());
        }
    }
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
            let transport = self.transport.clone();
            let cancelled = self.cancelled.clone();
            self.read = Some(Box::pin(async move {
                loop {
                    let page = tokio::select! {
                        page = transport.service.next(&transport.stream_id, READ_CHUNK_BYTES) => page.map_err(stream_plugin_error)?,
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
                // A source loop spends its call budget waiting for the peer;
                // a delivery may not extend its own deadline that way.
                if host.runner.active && !host.runner.has_pending_delivery() {
                    host.call_deadline = Some(Instant::now() + host.runner.timeout);
                }
                let (bytes, closed) = match result {
                    Ok(chunk) => (chunk.bytes, chunk.closed),
                    Err(error) => {
                        self.transport.fail(error);
                        (vec![], true)
                    }
                };
                dst.set_buffer(bytes.into());
                if closed {
                    Poll::Ready(Ok(StreamResult::Dropped))
                } else {
                    Poll::Ready(Ok(StreamResult::Completed))
                }
            }
        }
    }
}

type SendFuture = Pin<Box<dyn Future<Output = Result<(), StreamError>> + Send>>;

/// Outgoing half: guest bytes forwarded to the peer. The guest ending or
/// dropping the stream half-closes the connection.
struct SocketSend {
    transport: Arc<Transport>,
    send: Option<SendFuture>,
}

impl Drop for SocketSend {
    fn drop(&mut self) {
        self.transport
            .service
            .shutdown_write(&self.transport.stream_id);
        self.transport.finish();
    }
}

impl StreamConsumer<HostState> for SocketSend {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<HostState>,
        mut source: Source<'_, u8>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if self.send.is_none() {
            if store.data().runner.stopping || store.data().cancelled.load(Ordering::Acquire) {
                self.transport
                    .fail(host_error(types::ErrorCode::Cancelled, "source stopped"));
                return Poll::Ready(Ok(StreamResult::Dropped));
            }
            let mut bytes = Vec::with_capacity(WRITE_CHUNK_BYTES);
            source.read(store.as_context_mut(), &mut bytes)?;
            if bytes.is_empty() {
                return Poll::Ready(Ok(if finish {
                    StreamResult::Cancelled
                } else {
                    StreamResult::Completed
                }));
            }
            let transport = self.transport.clone();
            self.send = Some(Box::pin(async move {
                transport.service.send(&transport.stream_id, &bytes).await
            }));
        }
        // The bytes have left `source`, so the write runs to completion even
        // when the writer cancels.
        match self.send.as_mut().unwrap().as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                self.send = None;
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(Err(error)) => {
                self.send = None;
                self.transport.fail(stream_plugin_error(error));
                Poll::Ready(Ok(StreamResult::Dropped))
            }
        }
    }
}

impl socket::HostWithStore<HostState> for HostData {
    async fn connect(
        accessor: &Accessor<HostState, Self>,
        mut outgoing: StreamReader<u8>,
    ) -> Result<(StreamReader<u8>, FutureReader<Result<(), types::Error>>), types::Error> {
        let (transport, cancelled) = match Self::open_transport(accessor).await {
            Ok(opened) => opened,
            Err(error) => {
                accessor.with(|mut access| {
                    let _ = outgoing.close(&mut access);
                });
                return Err(error);
            }
        };
        accessor.with(|mut access| {
            // Piping disposes of the guest's stream, so it goes first: every
            // failure after this point only has to close the transport.
            outgoing
                .pipe(
                    &mut access,
                    SocketSend {
                        transport: transport.clone(),
                        send: None,
                    },
                )
                .map_err(|_| {
                    transport.close();
                    host_error(types::ErrorCode::ResourceExhausted, "stream pipe failed")
                })?;
            let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
            let mut incoming = StreamReader::new(
                &mut access,
                SocketBytes {
                    transport: transport.clone(),
                    cancelled,
                    read: None,
                    completion: Some(completion_tx),
                },
            )
            .map_err(|_| {
                transport.close();
                host_error(
                    types::ErrorCode::ResourceExhausted,
                    "stream allocation failed",
                )
            })?;
            let Ok(completion) = FutureReader::new(&mut access, async move {
                wasmtime::error::Ok(completion_rx.await.unwrap_or_else(|_| {
                    Err(host_error(types::ErrorCode::Cancelled, "stream dropped"))
                }))
            }) else {
                transport.close();
                let _ = incoming.close(&mut access);
                return Err(host_error(
                    types::ErrorCode::ResourceExhausted,
                    "future allocation failed",
                ));
            };
            access.get().transports.push(transport);
            Ok((incoming, completion))
        })
    }
}

impl HostData {
    /// Connects the granted endpoint under the call's cancellation and
    /// deadline. Connection establishment is bounded by the grant.
    async fn open_transport(
        accessor: &Accessor<HostState, Self>,
    ) -> Result<(Arc<Transport>, Arc<tokio::sync::Notify>), types::Error> {
        let (service, grant, interrupt, deadline, cancelled) = accessor.with(|mut access| {
            let host = access.get();
            if host.replaying || host.runner.stopping || host.interrupted() {
                return Err(stream_denied());
            }
            let (service, grant) = host.stream_access()?;
            let grant = StreamGrant {
                max_timeout_ms: host.connect_budget(&grant)?,
                ..grant
            };
            Ok((
                service,
                grant,
                Arc::clone(host.interrupt_flag()),
                host.effective_deadline(),
                host.cancel_notify.clone(),
            ))
        })?;
        let stream_id = cancellable(&interrupt, deadline, service.open(&grant))
            .await?
            .map_err(stream_plugin_error)?;
        let transport = Arc::new(Transport::new(service, stream_id));
        accessor.with(|mut access| access.get().transports.retain(|t| t.is_open()));
        Ok((transport, cancelled))
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
    /// Emits one buffer, then ends the stream.
    struct Once(Option<Vec<u8>>);
    impl StreamProducer<HostState> for Once {
        type Item = u8;
        type Buffer = VecBuffer<u8>;
        fn poll_produce<'a>(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: StoreContextMut<'a, HostState>,
            mut dst: Destination<'a, u8, Self::Buffer>,
            _: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            Poll::Ready(Ok(match self.0.take() {
                Some(bytes) => {
                    dst.set_buffer(bytes.into());
                    StreamResult::Completed
                }
                None => StreamResult::Dropped,
            }))
        }
    }

    fn granted(host: &mut HostState, service: &Arc<crate::tests::SubscriptionFixture>) {
        host.stream = Some(service.clone());
        host.stream_grant = Some(StreamGrant {
            endpoint: pluribus_core::StreamEndpoint::Unix {
                path: "/unused".into(),
                peer_uids: vec![1],
            },
            max_bytes: 1024,
            max_timeout_ms: 1000,
        });
    }

    #[tokio::test]
    async fn connect_is_denied_without_a_grant() {
        let (mut store, _, _) = crate::runner::tests::source().await;
        store
            .run_concurrent(async |accessor| {
                let host_access = accessor.with_getter::<HostData>(|h| h);
                let outgoing =
                    accessor.with(|mut access| StreamReader::new(&mut access, Once(None)).unwrap());
                assert!(
                    socket::HostWithStore::connect(&host_access, outgoing)
                        .await
                        .is_err()
                );
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn connect_reads_lazily_forwards_writes_and_closes_once_both_ends_finish() {
        let (mut store, _, _) = crate::runner::tests::source().await;
        let service = Arc::new(crate::tests::SubscriptionFixture::default());
        granted(store.data_mut(), &service);
        store
            .run_concurrent(async |accessor| {
                let host_access = accessor.with_getter::<HostData>(|h| h);
                let outgoing = accessor.with(|mut access| {
                    StreamReader::new(&mut access, Once(Some(b"ping".to_vec()))).unwrap()
                });
                let (incoming, mut done) = socket::HostWithStore::connect(&host_access, outgoing)
                    .await
                    .unwrap();
                assert_eq!(service.opens.load(Ordering::Acquire), 1);
                assert_eq!(service.reads.load(Ordering::Acquire), 0, "reads are lazy");
                let (tx, rx) = tokio::sync::oneshot::channel();
                accessor.with(|access| incoming.pipe(access, FirstBytes(Some(tx))).unwrap());
                assert_eq!(rx.await.unwrap(), vec![1]);
                assert_eq!(service.reads.load(Ordering::Acquire), 1);
                service.closed.notified().await;
                assert_eq!(service.sent.lock().unwrap().as_slice(), [b"ping".to_vec()]);
                assert_eq!(
                    service.half_closed.load(Ordering::Acquire),
                    1,
                    "ending the outgoing stream half-closes the connection"
                );
                assert_eq!(service.closes.load(Ordering::Acquire), 1);
                done.close_with(accessor).unwrap();
            })
            .await
            .unwrap();
    }

    /// Consumes bytes without ever ending the stream.
    struct Hold;
    impl StreamConsumer<HostState> for Hold {
        type Item = u8;
        fn poll_consume(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            store: StoreContextMut<HostState>,
            mut source: Source<'_, u8>,
            _: bool,
        ) -> Poll<wasmtime::Result<StreamResult>> {
            let mut bytes = Vec::with_capacity(32 * 1024);
            source.read(store, &mut bytes)?;
            Poll::Ready(Ok(StreamResult::Completed))
        }
    }
    struct Settle(Option<tokio::sync::oneshot::Sender<Result<(), types::Error>>>);
    impl wasmtime::component::FutureConsumer<HostState> for Settle {
        type Item = Result<(), types::Error>;
        fn poll_consume(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            store: StoreContextMut<HostState>,
            mut source: Source<'_, Self::Item>,
            _: bool,
        ) -> Poll<wasmtime::Result<()>> {
            let mut outcome = Vec::with_capacity(1);
            source.read(store, &mut outcome)?;
            if let Some(outcome) = outcome.pop() {
                let _ = self.0.take().unwrap().send(outcome);
            }
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn completion_settles_when_the_peer_ends_while_the_reader_is_held() {
        let (mut store, _, _) = crate::runner::tests::source().await;
        let service = Arc::new(crate::tests::SubscriptionFixture::default());
        service.fail.store(true, Ordering::Release);
        granted(store.data_mut(), &service);
        store
            .run_concurrent(async |accessor| {
                let host_access = accessor.with_getter::<HostData>(|h| h);
                let outgoing =
                    accessor.with(|mut access| StreamReader::new(&mut access, Once(None)).unwrap());
                let (incoming, done) = socket::HostWithStore::connect(&host_access, outgoing)
                    .await
                    .unwrap();
                let (tx, rx) = tokio::sync::oneshot::channel();
                accessor.with(|access| incoming.pipe(access, Hold).unwrap());
                accessor.with(|access| done.pipe(access, Settle(Some(tx))).unwrap());
                let outcome = tokio::time::timeout(Duration::from_secs(2), rx)
                    .await
                    .expect("completion settles without dropping the reader")
                    .unwrap();
                assert_eq!(outcome.unwrap_err().code, types::ErrorCode::Unavailable);
                assert_eq!(service.closes.load(Ordering::Acquire), 1);
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
