//! Policy-controlled HTTP under a per-component grant.

use futures_util::TryStreamExt;
use pluribus_core::is_public_address;
use pluribus_core::{
    BlobError, BlobStore, HttpError, HttpFrame, HttpFramePage, HttpGrant, HttpHeader, HttpRequest,
    HttpResponse, HttpService, HttpStreamProtocol, HttpStreamService, PrincipalRef,
};
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue, LOCATION};
use reqwest::redirect::Policy;
use reqwest::{Body, Client, RequestBuilder, Response};
use reqwest::{Method, StatusCode};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{self, Receiver, Sender, error::TryRecvError};

const MAX_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_REDIRECTS: u8 = 5;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;
const MAX_ERROR_DETAIL_CHARS: usize = 512;
const TRANSFER_CHUNK_BYTES: usize = 64 * 1024;
const STREAM_QUEUE_FRAMES: usize = 32;
const MAX_STREAM_FRAMES_PER_RECEIVE: u32 = 256;

pub struct PolicyHttpService {
    blobs: Arc<dyn BlobStore>,
    streams: Mutex<HashMap<String, Arc<ActiveStream>>>,
    next_stream_id: AtomicU64,
}

struct ActiveStream {
    component: PrincipalRef,
    receiver: AsyncMutex<Receiver<Result<HttpFrame, HttpError>>>,
    closed: AtomicBool,
    worker: tokio::task::AbortHandle,
}

impl Drop for ActiveStream {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl PolicyHttpService {
    #[must_use]
    pub fn new(blobs: Arc<dyn BlobStore>) -> Self {
        Self {
            blobs,
            streams: Mutex::new(HashMap::new()),
            next_stream_id: AtomicU64::new(1),
        }
    }

    fn start_stream(
        &self,
        grant: &HttpGrant,
        protocol: HttpStreamProtocol,
        response: Response,
    ) -> Result<String, HttpError> {
        if response
            .content_length()
            .is_some_and(|length| length > grant.max_response_bytes)
        {
            return Err(HttpError::ResourceExhausted(
                "stream response exceeds grant".into(),
            ));
        }
        let stream_id = format!(
            "http-stream-{}",
            self.next_stream_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = mpsc::channel(STREAM_QUEUE_FRAMES);
        let maximum_bytes = grant.max_response_bytes;
        let worker = tokio::spawn(async move {
            if protocol == HttpStreamProtocol::Bytes {
                if let Err(error) =
                    read_bytes(response_reader(response), maximum_bytes, &sender).await
                {
                    let _ = sender.send(Err(error)).await;
                }
            } else {
                read_sse(response, maximum_bytes, &sender).await;
            }
        });
        self.streams
            .lock()
            .map_err(|_| HttpError::Internal("stream registry lock failed".into()))?
            .insert(
                stream_id.clone(),
                Arc::new(ActiveStream {
                    component: grant.component.clone(),
                    receiver: AsyncMutex::new(receiver),
                    closed: AtomicBool::new(false),
                    worker: worker.abort_handle(),
                }),
            );
        Ok(stream_id)
    }

    async fn execute(
        &self,
        grant: &HttpGrant,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpError> {
        let response = self.open_response(grant, request).await?;
        self.store_response(response, grant.max_response_bytes)
            .await
    }

    async fn open_response(
        &self,
        grant: &HttpGrant,
        request: &HttpRequest,
    ) -> Result<Response, HttpError> {
        let (mut url, mut method, timeout) = prepare_request(grant, request)?;

        let deadline = Instant::now() + timeout;
        let mut redirect_count = 0;
        let mut body = request.body.clone();
        let mut strip_sensitive_headers = false;
        loop {
            remaining_time(deadline)?;
            validate_destination(grant, &url, &method)?;
            let resolved =
                tokio::time::timeout(remaining_time(deadline)?, resolve_destination(grant, &url))
                    .await
                    .map_err(|_| HttpError::Timeout)??;
            let request_url = url.clone();
            let client = build_client(&request_url, &resolved, remaining_time(deadline)?)?;
            let mut builder = prepare_headers(
                client.request(method.clone(), request_url.clone()),
                request,
                strip_sensitive_headers,
            )?;
            if let Some(blob) = &body {
                let state = (self.blobs.clone(), blob.clone(), 0_u64);
                let chunks =
                    futures_util::stream::try_unfold(state, |(store, blob, offset)| async move {
                        if offset == blob.size {
                            return Ok::<_, io::Error>(None);
                        }
                        let chunk = store
                            .read(&blob, offset, TRANSFER_CHUNK_BYTES)
                            .await
                            .map_err(|error| io_error(&error))?;
                        let next = offset
                            .checked_add(chunk.bytes.len() as u64)
                            .filter(|next| *next <= blob.size)
                            .ok_or_else(|| io::Error::other("blob exceeds declared size"))?;
                        if chunk.bytes.is_empty() {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "blob ended before its declared size",
                            ));
                        }
                        Ok(Some((chunk.bytes, (store, blob, next))))
                    });
                builder = builder
                    .header(reqwest::header::CONTENT_LENGTH, blob.size)
                    .body(Body::wrap_stream(chunks));
            }
            let response = builder.send().await.map_err(transport_error)?;
            if let Some(next) = redirect_target(&request_url, &response)? {
                if redirect_count == grant.max_redirects {
                    return Err(HttpError::ResourceExhausted(
                        "redirect limit exceeded".into(),
                    ));
                }
                let previous_origin = canonical_origin(&url)?;
                let next_origin = canonical_origin(&next)?;
                if previous_origin != next_origin {
                    strip_sensitive_headers = true;
                }
                if response.status() == StatusCode::SEE_OTHER
                    || ((response.status() == StatusCode::MOVED_PERMANENTLY
                        || response.status() == StatusCode::FOUND)
                        && method == Method::POST)
                {
                    method = Method::GET;
                    body = None;
                }
                redirect_count += 1;
                url = next;
                continue;
            }
            return Ok(response);
        }
    }

    async fn store_response(
        &self,
        mut response: Response,
        max_response_bytes: u64,
    ) -> Result<HttpResponse, HttpError> {
        if response
            .content_length()
            .is_some_and(|length| length > max_response_bytes)
        {
            return Err(HttpError::ResourceExhausted(
                "response body exceeds grant".into(),
            ));
        }
        let headers = response
            .headers()
            .iter()
            .filter(|(name, _)| !is_hop_by_hop(name))
            .map(|(name, value)| HttpHeader {
                name: name.as_str().to_owned(),
                value: value.as_bytes().to_vec(),
            })
            .collect::<Vec<_>>();
        if header_bytes(&headers) > MAX_HEADER_BYTES {
            return Err(HttpError::ResourceExhausted(
                "response headers exceed 64 KiB".into(),
            ));
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let store = self.blobs.clone();
        let expected = response.content_length();
        let id = store
            .begin_put(&media_type, expected)
            .await
            .map_err(|error| blob_error(&error))?;
        let upload = ResponseUpload {
            store,
            id,
            executor: tokio::runtime::Handle::current(),
        };
        let mut offset = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            let next = offset
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| HttpError::ResourceExhausted("response size overflow".into()))?;
            if next > max_response_bytes {
                return Err(HttpError::ResourceExhausted(
                    "response body exceeds grant".into(),
                ));
            }
            let store = upload.store.clone();
            let id = upload.id.clone();
            store
                .write(&id, offset, &chunk)
                .await
                .map_err(|e| blob_error(&e))?;
            offset = next;
        }
        let store = upload.store.clone();
        let id = upload.id.clone();
        let body = store.finish_put(&id).await.map_err(|e| blob_error(&e))?;
        Ok(HttpResponse {
            status: response.status().as_u16(),
            headers,
            body,
        })
    }
}

struct ResponseUpload {
    executor: tokio::runtime::Handle,
    store: Arc<dyn BlobStore>,
    id: pluribus_core::BlobUploadId,
}

impl Drop for ResponseUpload {
    fn drop(&mut self) {
        let store = self.store.clone();
        let id = self.id.clone();
        self.executor.spawn(async move {
            let _ = store.abort_put(&id).await;
        });
    }
}

fn response_reader(response: Response) -> impl AsyncRead + Unpin {
    tokio_util::io::StreamReader::new(response.bytes_stream().map_err(io::Error::other))
}

async fn limited_body(response: &mut Response, maximum: u64) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        let remaining = usize::try_from(maximum)
            .unwrap_or(usize::MAX)
            .saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() as u64 >= maximum {
            break;
        }
    }
    Ok(body)
}

fn prepare_headers(
    mut builder: RequestBuilder,
    request: &HttpRequest,
    strip_sensitive_headers: bool,
) -> Result<RequestBuilder, HttpError> {
    for header in &request.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| HttpError::Invalid("header name is malformed".into()))?;
        if is_hop_by_hop(&name) || (strip_sensitive_headers && is_credential_header(&name)) {
            continue;
        }
        let value = HeaderValue::from_bytes(&header.value)
            .map_err(|_| HttpError::Invalid("header value is malformed".into()))?;
        builder = builder.header(name, value);
    }
    Ok(builder)
}

fn remaining_time(deadline: Instant) -> Result<Duration, HttpError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(HttpError::Timeout)
}

#[async_trait::async_trait]
impl HttpService for PolicyHttpService {
    async fn send(
        &self,
        grant: &HttpGrant,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpError> {
        self.execute(grant, request).await
    }
}

#[async_trait::async_trait]
impl HttpStreamService for PolicyHttpService {
    fn with_body_store(&self, store: Arc<dyn BlobStore>) -> Option<Arc<dyn HttpStreamService>> {
        Some(Arc::new(Self::new(store)))
    }

    async fn start_response(
        &self,
        grant: &HttpGrant,
        request: &HttpRequest,
    ) -> Result<Option<pluribus_core::HttpStreamingResponse>, HttpError> {
        let response = self.open_response(grant, request).await?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter(|(name, _)| !is_hop_by_hop(name))
            .map(|(name, value)| HttpHeader {
                name: name.to_string(),
                value: value.as_bytes().to_vec(),
            })
            .collect::<Vec<_>>();
        if header_bytes(&headers) > MAX_HEADER_BYTES {
            return Err(HttpError::ResourceExhausted(
                "response headers exceed 64 KiB".into(),
            ));
        }
        let stream_id = self.start_stream(grant, HttpStreamProtocol::Bytes, response)?;
        Ok(Some(pluribus_core::HttpStreamingResponse {
            status,
            headers,
            stream_id,
        }))
    }

    async fn open_stream(
        &self,
        grant: &HttpGrant,
        protocol: HttpStreamProtocol,
        request: &HttpRequest,
    ) -> Result<String, HttpError> {
        if protocol == HttpStreamProtocol::WebSocket {
            return Err(HttpError::Unsupported(
                "WebSocket transport is not implemented".into(),
            ));
        }
        let mut response = self.open_response(grant, request).await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = limited_body(&mut response, MAX_ERROR_BODY_BYTES)
                .await
                .unwrap_or_default();
            return Err(stream_status_error(status, &body));
        }
        self.start_stream(grant, protocol, response)
    }

    async fn receive(
        &self,
        grant: &HttpGrant,
        stream_id: &str,
        max_frames: u32,
        timeout_ms: u32,
    ) -> Result<HttpFramePage, HttpError> {
        if max_frames == 0 || max_frames > MAX_STREAM_FRAMES_PER_RECEIVE {
            return Err(HttpError::Invalid(format!(
                "max frames must be between 1 and {MAX_STREAM_FRAMES_PER_RECEIVE}"
            )));
        }
        if timeout_ms > grant.max_timeout_ms {
            return Err(HttpError::PermissionDenied(
                "stream receive timeout exceeds grant".into(),
            ));
        }
        let stream = self
            .streams
            .lock()
            .map_err(|_| HttpError::Internal("stream registry lock failed".into()))?
            .get(stream_id)
            .cloned()
            .ok_or_else(|| HttpError::NotFound("stream".into()))?;
        ensure_stream_owner(&stream, grant)?;
        if stream.closed.load(Ordering::Acquire) {
            return Ok(HttpFramePage {
                frames: Vec::new(),
                closed: true,
            });
        }
        let mut receiver = stream.receiver.lock().await;
        let mut frames = Vec::new();
        let first = match receiver.try_recv() {
            Ok(frame) => Some(frame),
            Err(TryRecvError::Disconnected) => None,
            Err(TryRecvError::Empty) => match tokio::time::timeout(
                Duration::from_millis(u64::from(timeout_ms)),
                receiver.recv(),
            )
            .await
            {
                Ok(frame) => frame,
                Err(_) => {
                    return Ok(HttpFramePage {
                        frames,
                        closed: false,
                    });
                }
            },
        };
        match first {
            Some(result) => frames.push(result?),
            None => stream.closed.store(true, Ordering::Release),
        }
        while frames.len() < usize::try_from(max_frames).unwrap_or(usize::MAX)
            && !stream.closed.load(Ordering::Acquire)
        {
            match receiver.try_recv() {
                Ok(result) => frames.push(result?),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => stream.closed.store(true, Ordering::Release),
            }
        }
        Ok(HttpFramePage {
            frames,
            closed: stream.closed.load(Ordering::Acquire),
        })
    }

    async fn send_frame(
        &self,
        grant: &HttpGrant,
        stream_id: &str,
        _frame: &HttpFrame,
    ) -> Result<(), HttpError> {
        let stream = self
            .streams
            .lock()
            .map_err(|_| HttpError::Internal("stream registry lock failed".into()))?
            .get(stream_id)
            .cloned()
            .ok_or_else(|| HttpError::NotFound("stream".into()))?;
        ensure_stream_owner(&stream, grant)?;
        Err(HttpError::Unsupported(
            "SSE streams are receive-only".into(),
        ))
    }

    fn close_stream(&self, grant: &HttpGrant, stream_id: &str) {
        let Ok(mut streams) = self.streams.lock() else {
            return;
        };
        if streams
            .get(stream_id)
            .is_some_and(|stream| stream.component == grant.component)
            && let Some(stream) = streams.remove(stream_id)
        {
            stream.closed.store(true, Ordering::Release);
            stream.worker.abort();
        }
    }
}

fn ensure_stream_owner(stream: &ActiveStream, grant: &HttpGrant) -> Result<(), HttpError> {
    if stream.component == grant.component {
        Ok(())
    } else {
        Err(HttpError::PermissionDenied(
            "stream belongs to another component".into(),
        ))
    }
}

fn stream_status_error(status: u16, body: &[u8]) -> HttpError {
    if matches!(status, 401 | 403) {
        return HttpError::AuthenticationRequired;
    }
    let detail = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            ["/error/message", "/detail", "/message"]
                .into_iter()
                .find_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_str))
                .map(safe_error_detail)
        })
        .filter(|detail| !detail.is_empty());
    let suffix = detail.map_or_else(String::new, |detail| format!(": {detail}"));
    HttpError::Unavailable(format!("stream endpoint returned HTTP {status}{suffix}"))
}

fn safe_error_detail(detail: &str) -> String {
    detail
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .take(MAX_ERROR_DETAIL_CHARS)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

async fn read_bytes(
    mut reader: impl AsyncRead + Unpin,
    maximum_bytes: u64,
    sender: &Sender<Result<HttpFrame, HttpError>>,
) -> Result<(), HttpError> {
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer).await.map_err(transport_error)?;
        if read == 0 {
            return Ok(());
        }
        total = total
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| HttpError::ResourceExhausted("stream size overflow".into()))?;
        if total > maximum_bytes {
            return Err(HttpError::ResourceExhausted(
                "stream response exceeds grant".into(),
            ));
        }
        sender
            .send(Ok(HttpFrame {
                kind: "bytes".into(),
                data: buffer[..read].to_vec(),
            }))
            .await
            .map_err(|_| HttpError::Cancelled)?;
    }
}

async fn read_sse(
    response: Response,
    maximum_bytes: u64,
    sender: &Sender<Result<HttpFrame, HttpError>>,
) {
    if let Err(error) = parse_sse(response, maximum_bytes, sender).await {
        let _ = sender.send(Err(error)).await;
    }
}

async fn parse_sse(
    response: Response,
    maximum_bytes: u64,
    sender: &Sender<Result<HttpFrame, HttpError>>,
) -> Result<(), HttpError> {
    let mut reader = BufReader::new(response_reader(response));
    let mut line = Vec::new();
    let mut event = String::from("message");
    let mut data = Vec::new();
    let mut bytes_read = 0_u64;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(transport_error)?;
        bytes_read = bytes_read
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| HttpError::ResourceExhausted("stream size overflow".into()))?;
        if bytes_read > maximum_bytes {
            return Err(HttpError::ResourceExhausted(
                "stream response exceeds grant".into(),
            ));
        }
        if read == 0 {
            emit_sse_frame(&event, &mut data, sender).await?;
            return Ok(());
        }
        trim_line_ending(&mut line);
        if line.is_empty() {
            emit_sse_frame(&event, &mut data, sender).await?;
            event.clear();
            event.push_str("message");
            continue;
        }
        if line.first() == Some(&b':') {
            continue;
        }
        if let Some(value) = sse_field(&line, b"event") {
            event = String::from_utf8(value.to_vec())
                .map_err(|_| HttpError::Invalid("SSE event name is not UTF-8".into()))?;
        } else if let Some(value) = sse_field(&line, b"data") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value);
        }
    }
}

async fn emit_sse_frame(
    event: &str,
    data: &mut Vec<u8>,
    sender: &Sender<Result<HttpFrame, HttpError>>,
) -> Result<(), HttpError> {
    if data.is_empty() {
        return Ok(());
    }
    let frame = HttpFrame {
        kind: event.to_owned(),
        data: std::mem::take(data),
    };
    sender
        .send(Ok(frame))
        .await
        .map_err(|_| HttpError::Cancelled)
}

fn trim_line_ending(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

fn sse_field<'a>(line: &'a [u8], expected: &[u8]) -> Option<&'a [u8]> {
    let colon = line.iter().position(|byte| *byte == b':')?;
    if &line[..colon] != expected {
        return None;
    }
    let value = &line[colon + 1..];
    Some(value.strip_prefix(b" ").unwrap_or(value))
}

fn prepare_request(
    grant: &HttpGrant,
    request: &HttpRequest,
) -> Result<(Url, Method, Duration), HttpError> {
    let url =
        Url::parse(&request.url).map_err(|_| HttpError::Invalid("URL is malformed".into()))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(HttpError::Invalid(
            "URL credentials and fragments are forbidden".into(),
        ));
    }
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| HttpError::Invalid("method is malformed".into()))?;
    let timeout = Duration::from_millis(u64::from(request.timeout_ms));
    if timeout.is_zero() || timeout > MAX_TIMEOUT || request.timeout_ms > grant.max_timeout_ms {
        return Err(HttpError::Invalid(
            "timeout exceeds the invocation or host limit".into(),
        ));
    }
    if grant.max_redirects > MAX_REDIRECTS {
        return Err(HttpError::Invalid(
            "redirect grant exceeds hard limit".into(),
        ));
    }
    if header_bytes(&request.headers) > MAX_HEADER_BYTES {
        return Err(HttpError::ResourceExhausted(
            "request headers exceed 64 KiB".into(),
        ));
    }
    if request
        .body
        .as_ref()
        .is_some_and(|body| body.size > grant.max_request_bytes)
    {
        return Err(HttpError::ResourceExhausted(
            "request body exceeds grant".into(),
        ));
    }
    Ok((url, method, timeout))
}

fn validate_destination(
    grant: &HttpGrant,
    url: &Url,
    method: &Method,
) -> Result<String, HttpError> {
    match url.scheme() {
        "https" => {}
        "http" if grant.allow_http => {}
        "http" => {
            return Err(HttpError::PermissionDenied(
                "plaintext HTTP is not granted".into(),
            ));
        }
        _ => {
            return Err(HttpError::Invalid(
                "only HTTP and HTTPS are supported".into(),
            ));
        }
    }
    let origin = canonical_origin(url)?;
    let origin_allowed = grant.origins.iter().any(|allowed| {
        Url::parse(allowed)
            .ok()
            .and_then(|url| canonical_origin(&url).ok())
            .is_some_and(|allowed| allowed == origin)
    });
    if !origin_allowed {
        return Err(HttpError::PermissionDenied("origin is not granted".into()));
    }
    if !grant
        .methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(HttpError::PermissionDenied("method is not granted".into()));
    }
    Ok(origin)
}

fn canonical_origin(url: &Url) -> Result<String, HttpError> {
    if url.host_str().is_none() || url.cannot_be_a_base() {
        return Err(HttpError::Invalid("URL has no network origin".into()));
    }
    Ok(url.origin().ascii_serialization())
}

async fn resolve_destination(grant: &HttpGrant, url: &Url) -> Result<Vec<SocketAddr>, HttpError> {
    let host = url
        .host_str()
        .ok_or_else(|| HttpError::Invalid("URL has no host".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| HttpError::Invalid("URL has no port".into()))?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| HttpError::Unavailable("DNS resolution failed".into()))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(HttpError::Unavailable("DNS returned no addresses".into()));
    }
    if !grant.allow_private_network
        && addresses
            .iter()
            .any(|address| !is_public_address(address.ip()))
    {
        return Err(HttpError::PermissionDenied(
            "destination resolves to a non-public address".into(),
        ));
    }
    Ok(addresses)
}

fn build_client(
    url: &Url,
    resolved: &[SocketAddr],
    timeout: Duration,
) -> Result<Client, HttpError> {
    let host = url
        .host_str()
        .ok_or_else(|| HttpError::Invalid("URL has no host".into()))?;
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .timeout(timeout);
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve(host, resolved[0]);
    }
    builder
        .build()
        .map_err(|_| HttpError::Internal("cannot initialize HTTP client".into()))
}

fn redirect_target(current: &Url, response: &Response) -> Result<Option<Url>, HttpError> {
    if !response.status().is_redirection() {
        return Ok(None);
    }
    let Some(location) = response.headers().get(LOCATION) else {
        return Ok(None);
    };
    let location = location
        .to_str()
        .map_err(|_| HttpError::Invalid("redirect location is not UTF-8".into()))?;
    current
        .join(location)
        .map(Some)
        .map_err(|_| HttpError::Invalid("redirect location is malformed".into()))
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

fn header_bytes(headers: &[HttpHeader]) -> usize {
    headers.iter().fold(0, |total, header| {
        total
            .saturating_add(header.name.len())
            .saturating_add(header.value.len())
    })
}

fn is_credential_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie"
    )
}

fn blob_error(error: &BlobError) -> HttpError {
    match error {
        BlobError::NotFound => HttpError::NotFound("blob".into()),
        BlobError::Conflict { .. } | BlobError::Invalid(_) => {
            HttpError::Invalid("blob operation failed".into())
        }
        BlobError::ResourceExhausted(_) => HttpError::ResourceExhausted("blob store limit".into()),
        BlobError::Corrupt(_) => HttpError::Internal("blob is corrupt".into()),
        BlobError::Storage(_) => HttpError::Unavailable("blob store failed".into()),
    }
}

fn transport_error(error: impl std::fmt::Display) -> HttpError {
    let message = error.to_string();
    if message.to_ascii_lowercase().contains("timed out") {
        HttpError::Timeout
    } else {
        HttpError::Unavailable("transport failed".into())
    }
}

fn io_error(error: &BlobError) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pluribus_core::{BlobStore, InMemoryBlobStore, PrincipalKind, PrincipalRef};
    use std::io::Read as _;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[tokio::test]
    async fn response_upload_cleanup_survives_drop_outside_executor() {
        let blobs: Arc<dyn BlobStore> = Arc::new(pluribus_core::InMemoryBlobStore::default());
        let id = blobs.begin_put("text/plain", None).await.unwrap();
        let upload = ResponseUpload {
            store: blobs.clone(),
            id: id.clone(),
            executor: tokio::runtime::Handle::current(),
        };
        std::thread::spawn(move || drop(upload))
            .join()
            .expect("cleanup panicked outside executor");
        tokio::time::timeout(Duration::from_secs(1), async {
            while blobs.write(&id, 0, &[]).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("upload was not discarded");
    }

    #[test]
    fn authentication_errors_never_include_provider_prose() {
        let error = stream_status_error(401, br#"{"error":{"message":"Bearer fixture-secret"}}"#);
        assert!(!error.to_string().contains("fixture-secret"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn byte_stream_preserves_framing_and_enforces_size() {
        let bytes = b"event: response\r\ndata: {\"text\":\"hello\"}\r\n\r\n";
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        read_bytes(&bytes[..], 1024, &sender).await.unwrap();
        drop(sender);
        let mut actual = Vec::new();
        while let Some(frame) = receiver.recv().await {
            actual.extend(frame.unwrap().data);
        }
        assert_eq!(actual, bytes);
        let (sender, _) = tokio::sync::mpsc::channel(4);
        assert!(matches!(
            read_bytes(&bytes[..], 1, &sender).await,
            Err(HttpError::ResourceExhausted(_))
        ));
    }

    #[test]
    fn stream_error_includes_only_a_bounded_json_message() {
        let error = stream_status_error(
            400,
            br#"{"error":{"message":"unsupported field\nvalue","token":"secret"}}"#,
        );

        assert_eq!(
            error,
            HttpError::Unavailable(
                "stream endpoint returned HTTP 400: unsupported field value".into()
            )
        );
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires loopback sockets"]
    async fn sends_bounded_requests_with_guest_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.contains("authorization: bearer test-token"));
            assert!(request.contains("chatgpt-account-id: account-1"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .unwrap();
        });
        let origin = format!("http://{address}");
        let component = PrincipalRef::new(PrincipalKind::Component, "test-1");
        let blobs: Arc<dyn BlobStore> = Arc::new(InMemoryBlobStore::default());
        let service = PolicyHttpService::new(Arc::clone(&blobs));
        let response = service
            .send(
                &HttpGrant {
                    component,
                    origins: vec![origin.clone()],
                    methods: vec!["GET".into()],
                    allow_http: true,
                    allow_private_network: true,
                    max_request_bytes: 1024,
                    max_response_bytes: 1024,
                    max_redirects: 0,
                    max_timeout_ms: 1000,
                },
                &HttpRequest {
                    method: "GET".into(),
                    url: format!("{origin}/test"),
                    headers: vec![
                        HttpHeader {
                            name: "authorization".into(),
                            value: b"Bearer test-token".to_vec(),
                        },
                        HttpHeader {
                            name: "chatgpt-account-id".into(),
                            value: b"account-1".to_vec(),
                        },
                    ],
                    body: None,
                    timeout_ms: 1000,
                },
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(blobs.read(&response.body, 0, 2).await.unwrap().bytes, b"ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires loopback sockets"]
    async fn delivers_sse_frames_before_the_response_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0_u8; 4096];
            let _read = stream.read(&mut request).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .unwrap();
            let first = b"event: response.output_text.delta\ndata: {\"delta\":\"one\"}\n\n";
            write!(stream, "{:x}\r\n", first.len()).unwrap();
            stream.write_all(first).unwrap();
            stream.write_all(b"\r\n").unwrap();
            stream.flush().unwrap();
            wait.recv().unwrap();
            let second = b"data: {\"type\":\"response.completed\"}\n\n";
            write!(stream, "{:x}\r\n", second.len()).unwrap();
            stream.write_all(second).unwrap();
            stream.write_all(b"\r\n0\r\n\r\n").unwrap();
        });
        let origin = format!("http://{address}");
        let component = PrincipalRef::new(PrincipalKind::Component, "test-1");
        let service = PolicyHttpService::new(Arc::new(InMemoryBlobStore::default()));
        let grant = HttpGrant {
            component,
            origins: vec![origin.clone()],
            methods: vec!["GET".into()],
            allow_http: true,
            allow_private_network: true,
            max_request_bytes: 0,
            max_response_bytes: 1024,
            max_redirects: 0,
            max_timeout_ms: 2_000,
        };
        let stream_id = service
            .open_stream(
                &grant,
                HttpStreamProtocol::ServerSentEvents,
                &HttpRequest {
                    method: "GET".into(),
                    url: format!("{origin}/events"),
                    headers: Vec::new(),
                    body: None,
                    timeout_ms: 2_000,
                },
            )
            .await
            .unwrap();

        let first = service.receive(&grant, &stream_id, 1, 1_000).await.unwrap();
        assert_eq!(first.frames[0].kind, "response.output_text.delta");
        assert_eq!(first.frames[0].data, br#"{"delta":"one"}"#);
        assert!(!first.closed);
        release.send(()).unwrap();
        let last = service.receive(&grant, &stream_id, 1, 1_000).await.unwrap();
        assert_eq!(last.frames.len(), 1);
        service.close_stream(&grant, &stream_id);
        server.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn private_destinations_require_an_explicit_grant() {
        let blobs: Arc<dyn BlobStore> = Arc::new(InMemoryBlobStore::default());
        let service = PolicyHttpService::new(blobs);
        let error = service
            .send(
                &HttpGrant {
                    component: PrincipalRef::new(PrincipalKind::Component, "test-1"),
                    origins: vec!["http://127.0.0.1:9".into()],
                    methods: vec!["GET".into()],
                    allow_http: true,
                    allow_private_network: false,
                    max_request_bytes: 0,
                    max_response_bytes: 0,
                    max_redirects: 0,
                    max_timeout_ms: 100,
                },
                &HttpRequest {
                    method: "GET".into(),
                    url: "http://127.0.0.1:9/".into(),
                    headers: Vec::new(),
                    body: None,
                    timeout_ms: 100,
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(error, HttpError::PermissionDenied(_)));
    }
    #[tokio::test]
    #[ignore = "requires loopback sockets"]
    async fn closing_an_idle_stream_disconnects_upstream() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(socket.read_u8().await.unwrap());
                assert!(headers.len() < 4096);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            socket.read(&mut [0; 1]).await.unwrap()
        });
        let service = Arc::new(PolicyHttpService::new(Arc::new(
            InMemoryBlobStore::default(),
        )));
        let grant = HttpGrant {
            component: PrincipalRef::new(PrincipalKind::Component, "test"),
            origins: vec![origin.clone()],
            methods: vec!["GET".into()],
            allow_http: true,
            allow_private_network: true,
            max_request_bytes: 0,
            max_response_bytes: 1024,
            max_redirects: 0,
            max_timeout_ms: 2000,
        };
        let id = service
            .open_stream(
                &grant,
                HttpStreamProtocol::Bytes,
                &HttpRequest {
                    method: "GET".into(),
                    url: origin,
                    headers: vec![],
                    body: None,
                    timeout_ms: 2000,
                },
            )
            .await
            .unwrap();
        let reader_service = service.clone();
        let reader_grant = grant.clone();
        let reader_id = id.clone();
        let reader = tokio::spawn(async move {
            reader_service
                .receive(&reader_grant, &reader_id, 1, 1000)
                .await
        });
        tokio::task::yield_now().await;
        service.close_stream(&grant, &id);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), server)
                .await
                .expect("upstream remained open")
                .unwrap(),
            0
        );
        assert!(reader.await.unwrap().unwrap().closed);
    }
}

/// Exchanges sensitive payloads without putting them in durable blob storage.
pub async fn exchange_inline(
    grant: &HttpGrant,
    method: &str,
    url: &str,
    headers: Vec<HttpHeader>,
    bytes: Vec<u8>,
    timeout_ms: u32,
) -> Result<(u16, Vec<u8>), HttpError> {
    if bytes.len() as u64 > grant.max_request_bytes {
        return Err(HttpError::Invalid("request too large".into()));
    }
    let blobs = Arc::new(pluribus_core::InMemoryBlobStore::default());
    let body = if bytes.is_empty() {
        None
    } else {
        let upload = blobs
            .begin_put("application/json", Some(bytes.len() as u64))
            .await
            .map_err(|e| blob_error(&e))?;
        blobs
            .write(&upload, 0, &bytes)
            .await
            .map_err(|e| blob_error(&e))?;
        Some(
            blobs
                .finish_put(&upload)
                .await
                .map_err(|e| blob_error(&e))?,
        )
    };
    let service = PolicyHttpService::new(blobs.clone());
    let response = service
        .execute(
            grant,
            &HttpRequest {
                method: method.into(),
                url: url.into(),
                headers,
                body,
                timeout_ms,
            },
        )
        .await?;
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < response.body.size {
        let chunk = blobs
            .read(&response.body, bytes.len() as u64, 64 * 1024)
            .await
            .map_err(|e| blob_error(&e))?;
        bytes.extend(chunk.bytes);
    }
    Ok((response.status, bytes))
}

#[cfg(test)]
mod inline_tests {
    use super::*;
    #[tokio::test]
    async fn inline_exchange_applies_policy_and_returns_sensitive_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let mut grant = HttpGrant {
            component: PrincipalRef::new(pluribus_core::PrincipalKind::Component, "test"),
            origins: vec![origin.clone()],
            methods: vec!["POST".into()],
            allow_http: true,
            allow_private_network: false,
            max_request_bytes: 1024,
            max_response_bytes: 1024,
            max_redirects: 0,
            max_timeout_ms: 2000,
        };
        assert!(matches!(
            exchange_inline(
                &grant,
                "POST",
                &origin,
                vec![],
                b"private-request".to_vec(),
                2000
            )
            .await,
            Err(HttpError::PermissionDenied(_))
        ));
        grant.allow_private_network = true;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(socket.read_u8().await.unwrap());
            }
            let mut body = vec![0; 15];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(body, b"private-request");
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\nprivate-response").await.unwrap();
        });
        let result = exchange_inline(
            &grant,
            "POST",
            &origin,
            vec![],
            b"private-request".to_vec(),
            2000,
        )
        .await
        .unwrap();
        assert_eq!(result, (200, b"private-response".to_vec()));
        server.await.unwrap();
    }
}
