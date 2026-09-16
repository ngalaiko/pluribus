//! WASI HTTP over the policy-controlled host transport.
use super::*;
use ::http::Response;
use bindings::wasi::http::client;
use bytes::Bytes;
use http_body_util::{BodyExt, Limited, StreamBody};
use runner::HostData;
use wasmtime::component::Accessor;
use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::{RequestOptions, WasiBody, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

// Consumed by this adapter before any network request, including redirects.
pub(super) const CREDENTIAL_HEADER: &str = "x-pluribus-credential-handle";
pub(super) fn resource_table() -> wasmtime::component::ResourceTable {
    let mut table = wasmtime::component::ResourceTable::new();
    table.set_max_capacity(256);
    table
}
pub(super) struct Hooks;
impl WasiHttpHooks for Hooks {
    fn send_request(
        &mut self,
        _: ::http::Request<WasiBody>,
        _: Option<RequestOptions>,
        _: Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        Box::new(async { Err(wasmtime_wasi_http::Error::HttpRequestDenied) })
    }
}
impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.wasi_http,
            table: &mut self.wasi_table,
            hooks: &mut self.wasi_hooks,
        }
    }
}

pub(super) fn add_to_linker(linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
    macro_rules! link { ($($m:ident),*) => { $( $m::add_to_linker::<_, HostData>(linker, |h| h)?; )* }; }
    link!(
        blobs,
        credentials,
        events,
        reader,
        execution,
        socket,
        state,
        types,
        writer
    );
    use bindings::wasi::{
        clocks::{monotonic_clock, system_clock, types as clock_types},
        random::random,
    };
    link!(monotonic_clock, system_clock, clock_types, random, client);
    wasmtime_wasi_http::p3::bindings::http::types::add_to_linker::<_, wasmtime_wasi_http::WasiHttp>(
        linker,
        HostState::http,
    )?;
    Ok(())
}

fn map_error(error: HttpError) -> ErrorCode {
    match error {
        HttpError::PermissionDenied(_) | HttpError::AuthenticationRequired => {
            ErrorCode::HttpRequestDenied
        }
        HttpError::Timeout => ErrorCode::ConnectionTimeout,
        HttpError::Invalid(_) => ErrorCode::HttpRequestUriInvalid,
        HttpError::ResourceExhausted(_) => ErrorCode::HttpResponseBodySize(None),
        HttpError::Cancelled => ErrorCode::ConnectionTerminated,
        HttpError::Unsupported(_) => ErrorCode::HttpProtocolError,
        HttpError::NotFound(_) => ErrorCode::DestinationNotFound,
        HttpError::Unavailable(_) => ErrorCode::DestinationUnavailable,
        HttpError::Internal(_) => ErrorCode::InternalError(None),
    }
}
fn plugin_error(error: types::Error) -> ErrorCode {
    match error.code {
        types::ErrorCode::PermissionDenied => ErrorCode::HttpRequestDenied,
        types::ErrorCode::DeadlineExceeded => ErrorCode::ConnectionTimeout,
        types::ErrorCode::Cancelled => ErrorCode::ConnectionTerminated,
        _ => ErrorCode::InternalError(None),
    }
}

impl client::Host for HostState {}
impl client::HostWithStore<HostState> for HostData {
    async fn send(
        accessor: &Accessor<HostState, Self>,
        req: Resource<wasmtime_wasi_http::p3::Request>,
    ) -> Result<Resource<wasmtime_wasi_http::p3::Response>, ErrorCode> {
        let (request, service, grant, blobs, cancelled, timeout, source) =
            accessor.with(|mut access| {
                let host = access.get();
                // Consume even when authorization fails; WASI passes ownership.
                let request = host
                    .wasi_table
                    .delete(req)
                    .map_err(|_| ErrorCode::HttpRequestDenied)?;
                let options = request.options.clone();
                let (request, _) = request
                    .into_http(&mut access, async { Ok(()) })
                    .map_err(|_| ErrorCode::HttpRequestUriInvalid)?;
                let host = access.get();
                if host.replaying || host.cancelled.load(Ordering::Acquire) || host.runner.stopping
                {
                    return Err(ErrorCode::HttpRequestDenied);
                }
                let grant = host.http_grant().map_err(plugin_error)?.clone();
                let service = host.http.clone().ok_or(ErrorCode::DestinationUnavailable)?;
                let source = host.runner.active;
                let budget = if source {
                    grant.max_timeout_ms
                } else {
                    host.call_budget()
                        .map_err(plugin_error)?
                        .min(grant.max_timeout_ms)
                };
                let timeout = options
                    .as_ref()
                    .and_then(|o| {
                        [
                            o.connect_timeout,
                            o.first_byte_timeout,
                            o.between_bytes_timeout,
                        ]
                        .into_iter()
                        .flatten()
                        .min()
                    })
                    .unwrap_or(Duration::from_millis(u64::from(budget)))
                    .min(Duration::from_millis(u64::from(budget)));
                let blobs = host.blob_store.clone();
                let cancelled = host.cancel_notify.clone();
                Ok((request, service, grant, blobs, cancelled, timeout, source))
            })?;
        let result = tokio::select! {
            response = tokio::time::timeout(timeout, exchange(request, service, grant, blobs, timeout, cancelled.clone())) => response.map_err(|_| ErrorCode::ConnectionTimeout).and_then(|r| r),
            () = cancelled.notified() => Err(ErrorCode::ConnectionTerminated),
        };
        if source {
            runner::resume(accessor);
        }
        let response = result?;
        accessor.with(|mut access| {
            let host = access.get();
            let response = response.map(|body| ResumeBody {
                body,
                deadline: source.then(|| host.io_completion_deadline.clone()),
                budget: host.runner.timeout,
            });
            let (response, _completion) =
                wasmtime_wasi_http::p3::Response::from_http(&mut host.wasi_hooks, response);
            host.wasi_table
                .push(response)
                .map_err(|_| ErrorCode::InternalError(None))
        })
    }
}

struct ResumeBody {
    body: ResponseBody,
    deadline: Option<Arc<std::sync::Mutex<Option<Instant>>>>,
    budget: Duration,
}
impl http_body::Body for ResumeBody {
    type Data = Bytes;
    type Error = wasmtime_wasi_http::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let result = std::pin::Pin::new(&mut self.body).poll_frame(cx);
        if result.is_ready() {
            if let Some(deadline) = &self.deadline {
                *deadline.lock().unwrap() = Some(Instant::now() + self.budget);
            }
        }
        result
    }
}

type ResponseBody = http_body_util::combinators::UnsyncBoxBody<Bytes, wasmtime_wasi_http::Error>;
async fn exchange(
    request: ::http::Request<WasiBody>,
    service: Arc<dyn HttpStreamService>,
    grant: HttpGrant,
    blobs: Arc<dyn BlobStore>,
    timeout: Duration,
    cancelled: Arc<tokio::sync::Notify>,
) -> Result<Response<ResponseBody>, ErrorCode> {
    let transient: Arc<dyn BlobStore> = Arc::new(pluribus_core::InMemoryBlobStore::default());
    let (service, blobs) = match service.with_body_store(transient.clone()) {
        Some(service) => (service, transient),
        None => (service, blobs),
    };
    let (mut parts, body) = request.into_parts();
    let credential = parts
        .headers
        .remove(CREDENTIAL_HEADER)
        .map(|h| {
            h.to_str()
                .map(|s| SecretHandle::new(s.to_owned()))
                .map_err(|_| ErrorCode::HttpRequestDenied)
        })
        .transpose()?;
    let media_type = parts
        .headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let bytes = Limited::new(
        body,
        usize::try_from(grant.max_request_bytes).unwrap_or(usize::MAX),
    )
    .collect()
    .await
    .map_err(|_| ErrorCode::HttpRequestBodySize(Some(grant.max_request_bytes)))?
    .to_bytes();
    let body = if bytes.is_empty() {
        None
    } else {
        let upload = blobs
            .begin_put(&media_type, Some(bytes.len() as u64))
            .await
            .map_err(|_| ErrorCode::InternalError(None))?;
        if blobs.write(&upload, 0, &bytes).await.is_err() {
            let _ = blobs.abort_put(&upload).await;
            return Err(ErrorCode::InternalError(None));
        }
        Some(
            blobs
                .finish_put(&upload)
                .await
                .map_err(|_| ErrorCode::InternalError(None))?,
        )
    };
    let request = HttpRequest {
        method: parts.method.to_string(),
        url: parts.uri.to_string(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| HttpHeader {
                name: name.to_string(),
                value: value.as_bytes().to_vec(),
            })
            .collect(),
        body,
        credential_handle: credential,
        timeout_ms: u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX),
    };
    let head = service
        .start_response(&grant, &request)
        .await
        .map_err(map_error)?;
    if let Some(head) = head {
        return streaming_response(service, grant, head, cancelled, timeout);
    }
    if parts
        .headers
        .get("accept")
        .is_some_and(|v| v == "text/event-stream")
    {
        let stream_id = service
            .open_stream(&grant, HttpStreamProtocol::Bytes, &request)
            .await
            .map_err(map_error)?;
        return streaming_response(
            service,
            grant,
            pluribus_core::HttpStreamingResponse {
                status: 200,
                headers: vec![HttpHeader {
                    name: "content-type".into(),
                    value: b"text/event-stream".to_vec(),
                }],
                stream_id,
            },
            cancelled,
            timeout,
        );
    }
    let response = service.send(&grant, &request).await.map_err(map_error)?;
    let mut builder = Response::builder().status(response.status);
    for header in response.headers {
        builder = builder.header(header.name, header.value);
    }
    let body_ref = response.body;
    let stream = futures_util::stream::unfold(
        (blobs, body_ref, 0, false),
        |(blobs, blob, offset, ended)| async move {
            if ended {
                return None;
            }
            match blobs.read(&blob, offset, 64 * 1024).await {
                Ok(page) => {
                    let next = offset + page.bytes.len() as u64;
                    Some((
                        Ok::<_, wasmtime_wasi_http::Error>(http_body::Frame::data(Bytes::from(
                            page.bytes,
                        ))),
                        (blobs, blob, next, page.eof),
                    ))
                }
                Err(_) => Some((
                    Err(wasmtime_wasi_http::Error::InternalError(None)),
                    (blobs, blob, offset, true),
                )),
            }
        },
    );
    builder
        .body(StreamBody::new(stream).boxed_unsync())
        .map_err(|_| ErrorCode::InternalError(None))
}
fn streaming_response(
    service: Arc<dyn HttpStreamService>,
    grant: HttpGrant,
    head: pluribus_core::HttpStreamingResponse,
    cancelled: Arc<tokio::sync::Notify>,
    timeout: Duration,
) -> Result<Response<ResponseBody>, ErrorCode> {
    let mut builder = Response::builder().status(head.status);
    for h in head.headers {
        builder = builder.header(h.name, h.value);
    }
    let state = Sse {
        service,
        grant,
        id: head.stream_id,
        frames: VecDeque::new(),
        closed: false,
        cancelled,
        deadline: Instant::now() + timeout,
    };
    let stream = futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(bytes) = state.frames.pop_front() {
                return Some((
                    Ok::<_, wasmtime_wasi_http::Error>(http_body::Frame::data(Bytes::from(bytes))),
                    state,
                ));
            }
            if state.closed {
                return None;
            }
            let remaining = state.deadline.saturating_duration_since(Instant::now());
            let result = tokio::select! {
                result = tokio::time::timeout(remaining, state.service.receive(&state.grant, &state.id, 32, 1000)) => result.unwrap_or(Err(HttpError::Timeout)),
                () = state.cancelled.notified() => Err(HttpError::Cancelled),
            };
            match result {
                Ok(page) => {
                    state.closed = page.closed;
                    state.frames.extend(page.frames.into_iter().map(|f| f.data));
                }
                Err(error) => {
                    state.closed = true;
                    return Some((
                        Err(wasmtime_wasi_http::Error::from(map_error(error))),
                        state,
                    ));
                }
            }
        }
    });
    builder
        .body(StreamBody::new(stream).boxed_unsync())
        .map_err(|_| ErrorCode::InternalError(None))
}
struct Sse {
    deadline: Instant,
    service: Arc<dyn HttpStreamService>,
    grant: HttpGrant,
    id: String,
    frames: VecDeque<Vec<u8>>,
    closed: bool,
    cancelled: Arc<tokio::sync::Notify>,
}
impl Drop for Sse {
    fn drop(&mut self) {
        self.service.close_stream(&self.grant, &self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use pluribus_core::{HttpFrame, HttpFramePage, HttpResponse, HttpService};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fixture {
        seen: Mutex<Vec<HttpRequest>>,
        denied: bool,
        pending: bool,
        status: Option<u16>,
        delay: Duration,
        fail_body: bool,
        closed: AtomicBool,
    }
    #[async_trait::async_trait]
    impl HttpService for Fixture {
        async fn send(
            &self,
            _: &HttpGrant,
            request: &HttpRequest,
        ) -> Result<HttpResponse, HttpError> {
            self.seen.lock().unwrap().push(request.clone());
            if self.pending {
                std::future::pending::<()>().await;
            }
            Err(if self.denied {
                HttpError::PermissionDenied("denied".into())
            } else {
                HttpError::Timeout
            })
        }
    }
    #[async_trait::async_trait]
    impl HttpStreamService for Fixture {
        async fn start_response(
            &self,
            _: &HttpGrant,
            _: &HttpRequest,
        ) -> Result<Option<pluribus_core::HttpStreamingResponse>, HttpError> {
            Ok(self
                .status
                .map(|status| pluribus_core::HttpStreamingResponse {
                    status,
                    headers: vec![HttpHeader {
                        name: "x-fixture".into(),
                        value: b"present".to_vec(),
                    }],
                    stream_id: "stream".into(),
                }))
        }
        async fn open_stream(
            &self,
            _: &HttpGrant,
            protocol: HttpStreamProtocol,
            request: &HttpRequest,
        ) -> Result<String, HttpError> {
            assert_eq!(protocol, HttpStreamProtocol::Bytes);
            self.seen.lock().unwrap().push(request.clone());
            Ok("stream".into())
        }
        async fn receive(
            &self,
            _: &HttpGrant,
            _: &str,
            _: u32,
            _: u32,
        ) -> Result<HttpFramePage, HttpError> {
            tokio::time::sleep(self.delay).await;
            if self.fail_body {
                return Err(HttpError::Timeout);
            }
            Ok(HttpFramePage {
                frames: vec![HttpFrame {
                    kind: "bytes".into(),
                    data: b"data: hello\n\n".to_vec(),
                }],
                closed: true,
            })
        }
        async fn send_frame(&self, _: &HttpGrant, _: &str, _: &HttpFrame) -> Result<(), HttpError> {
            unreachable!()
        }
        fn close_stream(&self, _: &HttpGrant, _: &str) {
            self.closed.store(true, Ordering::Release);
        }
    }
    async fn setup(fixture: Arc<Fixture>) -> Store<HostState> {
        let (mut store, _, _) = crate::runner::tests::source().await;
        store.data_mut().http = Some(fixture);
        store.data_mut().http_grant = Some(HttpGrant {
            component: PrincipalRef::new(CorePrincipalKind::Component, "shell-1"),
            origins: vec!["https://example.com".into()],
            methods: vec!["GET".into()],
            allow_http: false,
            allow_private_network: false,
            max_request_bytes: 1024,
            max_response_bytes: 1024,
            max_redirects: 0,
            max_timeout_ms: 1000,
        });
        store
    }
    fn request(host: &mut HostState, sse: bool) -> Resource<wasmtime_wasi_http::p3::Request> {
        let mut request = ::http::Request::builder().uri("https://example.com/");
        if sse {
            request = request.header("accept", "text/event-stream");
        }
        let (request, _) = wasmtime_wasi_http::p3::Request::from_http(
            &mut host.wasi_hooks,
            request.body(Full::new(Bytes::new())).unwrap(),
        );
        host.wasi_table.push(request).unwrap()
    }
    #[tokio::test]
    async fn credentials_are_removed_from_headers_and_policy_errors_remain_denials() {
        let fixture = Arc::new(Fixture {
            denied: true,
            ..Default::default()
        });
        let mut store = setup(fixture.clone()).await;
        let request = request(store.data_mut(), false);
        credentials::Host::authorize_http(
            store.data_mut(),
            Resource::new_borrow(request.rep()),
            "opaque-token".into(),
        )
        .await
        .unwrap();
        store
            .run_concurrent(async |accessor| {
                let error =
                    client::HostWithStore::send(&accessor.with_getter::<HostData>(|h| h), request)
                        .await
                        .unwrap_err();
                assert!(matches!(error, ErrorCode::HttpRequestDenied));
            })
            .await
            .unwrap();
        let seen = fixture.seen.lock().unwrap();
        assert_eq!(
            seen[0].credential_handle,
            Some(SecretHandle::new("opaque-token"))
        );
        assert!(!seen[0].headers.iter().any(|h| h.name == CREDENTIAL_HEADER));
    }
    #[tokio::test]
    async fn native_http_cancellation_and_deadlines() {
        let fixture = Arc::new(Fixture {
            pending: true,
            ..Default::default()
        });
        let mut store = setup(fixture.clone()).await;
        store.data_mut().runner.active = false;
        store.data_mut().call_deadline = Some(Instant::now() + Duration::from_millis(20));
        let req = request(store.data_mut(), false);
        store
            .run_concurrent(async |accessor| {
                assert!(matches!(
                    client::HostWithStore::send(&accessor.with_getter::<HostData>(|h| h), req)
                        .await
                        .unwrap_err(),
                    ErrorCode::ConnectionTimeout
                ));
            })
            .await
            .unwrap();
        assert!(fixture.seen.lock().unwrap()[0].timeout_ms <= 20);
        store.data_mut().runner.active = true;
        let req = request(store.data_mut(), false);
        let cancel = CancellationHandle::of(store.data());
        store
            .run_concurrent(async |accessor| {
                let host_access = accessor.with_getter::<HostData>(|h| h);
                let (result, ()) =
                    tokio::join!(client::HostWithStore::send(&host_access, req), async {
                        tokio::task::yield_now().await;
                        cancel.cancel();
                    });
                assert!(matches!(
                    result.unwrap_err(),
                    ErrorCode::ConnectionTerminated
                ));
            })
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn response_stream_preserves_sse_bytes_and_closes_transport() {
        let fixture = Arc::new(Fixture::default());
        let mut store = setup(fixture.clone()).await;
        let req = request(store.data_mut(), true);
        store
            .run_concurrent(async |accessor| {
                let response =
                    client::HostWithStore::send(&accessor.with_getter::<HostData>(|h| h), req)
                        .await
                        .unwrap();
                let response = accessor.with(|mut access| {
                    let response = access.get().wasi_table.delete(response).unwrap();
                    response.into_http(&mut access, async { Ok(()) }).unwrap()
                });
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                assert_eq!(&bytes[..], b"data: hello\n\n");
            })
            .await
            .unwrap();
        assert!(fixture.closed.load(Ordering::Acquire));
    }
    #[tokio::test]
    async fn streamed_status_headers_errors_and_compute_budget_survive_body_waits() {
        for fail_body in [false, true] {
            let fixture = Arc::new(Fixture {
                status: Some(418),
                delay: Duration::from_millis(30),
                fail_body,
                ..Default::default()
            });
            let mut store = setup(fixture.clone()).await;
            store.data_mut().runner.timeout = Duration::from_millis(100);
            let req = request(store.data_mut(), false);
            store
                .run_concurrent(async |accessor| {
                    let response =
                        client::HostWithStore::send(&accessor.with_getter::<HostData>(|h| h), req)
                            .await
                            .unwrap();
                    let response = accessor.with(|mut access| {
                        access.get().call_deadline =
                            Some(Instant::now() + Duration::from_millis(1));
                        let response = access.get().wasi_table.delete(response).unwrap();
                        response.into_http(&mut access, async { Ok(()) }).unwrap()
                    });
                    assert_eq!(response.status(), 418);
                    assert_eq!(response.headers()["x-fixture"], "present");
                    assert_eq!(response.into_body().collect().await.is_err(), fail_body);
                    accessor.with(|mut access| {
                        assert!(access.get().effective_deadline().unwrap() > Instant::now())
                    });
                })
                .await
                .unwrap();
            assert!(fixture.closed.load(Ordering::Acquire));
        }
    }

    #[tokio::test]
    async fn wasi_policy_transport_streams_private_bodies_without_persisting_them() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(socket.read_u8().await.unwrap());
            }
            assert!(!String::from_utf8_lossy(&headers).contains(CREDENTIAL_HEADER));
            let mut body = [0; 15];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"private-request");
            socket.write_all(b"HTTP/1.1 418 Teapot\r\nContent-Length: 16\r\nX-Private: present\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            socket.write_all(b"private-response").await.unwrap();
        });
        let mut store = setup(Arc::new(Fixture::default())).await;
        let durable = Arc::new(pluribus_core::InMemoryBlobStore::default());
        store.data_mut().blob_store = durable.clone();
        store.data_mut().http = Some(Arc::new(pluribus_host_http::PolicyHttpService::new(
            durable.clone(),
            Arc::new(pluribus_core::InMemoryCredentialStore::default()),
        )));
        let grant = store.data_mut().http_grant.as_mut().unwrap();
        grant.origins = vec![origin.clone()];
        grant.methods = vec!["POST".into()];
        grant.allow_http = true;
        grant.allow_private_network = true;
        let (req, _) = wasmtime_wasi_http::p3::Request::from_http(
            &mut store.data_mut().wasi_hooks,
            ::http::Request::builder()
                .method("POST")
                .uri(&origin)
                .body(Full::new(Bytes::from_static(b"private-request")))
                .unwrap(),
        );
        let req = store.data_mut().wasi_table.push(req).unwrap();
        store
            .run_concurrent(async |accessor| {
                let response =
                    client::HostWithStore::send(&accessor.with_getter::<HostData>(|h| h), req)
                        .await
                        .unwrap();
                let response = accessor.with(|mut access| {
                    let response = access.get().wasi_table.delete(response).unwrap();
                    response.into_http(&mut access, async { Ok(()) }).unwrap()
                });
                assert_eq!(response.status(), 418);
                assert_eq!(response.headers()["x-private"], "present");
                assert_eq!(
                    &response.into_body().collect().await.unwrap().to_bytes()[..],
                    b"private-response"
                );
            })
            .await
            .unwrap();
        server.await.unwrap();
        let probe = pluribus_core::InMemoryBlobStore::default();
        for (media_type, bytes) in [
            ("application/octet-stream", &b"private-request"[..]),
            ("text/plain", &b"private-response"[..]),
        ] {
            let upload = probe
                .begin_put(media_type, Some(bytes.len() as u64))
                .await
                .unwrap();
            probe.write(&upload, 0, bytes).await.unwrap();
            let blob = probe.finish_put(&upload).await.unwrap();
            assert!(durable.read(&blob, 0, 1024).await.is_err());
        }
    }
}
