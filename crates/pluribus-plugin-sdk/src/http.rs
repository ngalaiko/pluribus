//! HTTP helpers over WASI resources and native body streams.
use crate::pluribus::plugin::{
    blobs,
    types::{BlobRef, Chunk, Error, ErrorCode},
};
use crate::wasi::http::{client, types as wasi};
use std::cell::RefCell;

pub struct Header {
    pub name: String,
    pub value: Vec<u8>,
}
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<Header>,
    pub body: Option<BlobRef>,
    pub timeout_ms: u32,
}
pub struct Response {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: BlobRef,
}
pub struct InlineRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
    pub timeout_ms: u32,
}
pub struct InlineResponse {
    pub status: u16,
    pub body: Vec<u8>,
}
fn invalid(message: impl ToString) -> Error {
    Error {
        code: ErrorCode::InvalidArgument,
        message: message.to_string(),
        retryable: false,
        details: None,
    }
}
fn error(e: wasi::ErrorCode) -> Error {
    let (code, retryable) = match e {
        wasi::ErrorCode::HttpRequestDenied | wasi::ErrorCode::DestinationIpProhibited => {
            (ErrorCode::PermissionDenied, false)
        }
        wasi::ErrorCode::ConnectionTimeout
        | wasi::ErrorCode::ConnectionReadTimeout
        | wasi::ErrorCode::HttpResponseTimeout => (ErrorCode::DeadlineExceeded, true),
        wasi::ErrorCode::HttpRequestUriInvalid | wasi::ErrorCode::HttpRequestMethodInvalid => {
            (ErrorCode::InvalidArgument, false)
        }
        wasi::ErrorCode::HttpRequestBodySize(_) | wasi::ErrorCode::HttpResponseBodySize(_) => {
            (ErrorCode::ResourceExhausted, false)
        }
        _ => (ErrorCode::Unavailable, true),
    };
    Error {
        code,
        message: format!("HTTP failed: {e:?}"),
        retryable,
        details: None,
    }
}
fn blob_bytes(blob: &BlobRef) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < blob.size {
        let chunk = blobs::read(blob, bytes.len() as u64, 64 * 1024)?;
        if chunk.bytes.is_empty() {
            return Err(invalid("incomplete HTTP body blob"));
        }
        bytes.extend(chunk.bytes);
    }
    if bytes.len() as u64 != blob.size {
        return Err(invalid("invalid HTTP body size"));
    }
    Ok(bytes)
}
async fn request(
    method: &str,
    url: &str,
    headers: &[Header],
    bytes: Vec<u8>,
    timeout_ms: u32,
) -> Result<wasi::Response, Error> {
    let url = url::Url::parse(url).map_err(invalid)?;
    let fields = wasi::Fields::from_list(
        &headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect::<Vec<_>>(),
    )
    .map_err(invalid)?;
    let options = wasi::RequestOptions::new();
    let duration = Some(u64::from(timeout_ms) * 1_000_000);
    options.set_connect_timeout(duration).map_err(invalid)?;
    options.set_first_byte_timeout(duration).map_err(invalid)?;
    options
        .set_between_bytes_timeout(duration)
        .map_err(invalid)?;
    let contents = if bytes.is_empty() {
        None
    } else {
        let (mut writer, reader) = crate::wit_stream::new::<u8>();
        wit_bindgen::spawn_local(async move {
            let _ = writer.write_all(bytes).await;
        });
        Some(reader)
    };
    let (trailers, trailers_read) = crate::wit_future::new(|| Ok(None));
    drop(trailers);
    let (request, sent) = wasi::Request::new(fields, contents, trailers_read, Some(options));
    request
        .set_method(&match method {
            "GET" => wasi::Method::Get,
            "POST" => wasi::Method::Post,
            "PUT" => wasi::Method::Put,
            "DELETE" => wasi::Method::Delete,
            "PATCH" => wasi::Method::Patch,
            "HEAD" => wasi::Method::Head,
            "OPTIONS" => wasi::Method::Options,
            other => wasi::Method::Other(other.into()),
        })
        .map_err(|()| invalid("invalid HTTP method"))?;
    request
        .set_scheme(Some(&match url.scheme() {
            "https" => wasi::Scheme::Https,
            "http" => wasi::Scheme::Http,
            other => wasi::Scheme::Other(other.into()),
        }))
        .map_err(|()| invalid("invalid HTTP scheme"))?;
    request
        .set_authority(Some(
            &url[url::Position::BeforeHost..url::Position::AfterPort],
        ))
        .map_err(|()| invalid("invalid HTTP authority"))?;
    request
        .set_path_with_query(Some(
            &url[url::Position::BeforePath..url::Position::AfterQuery],
        ))
        .map_err(|()| invalid("invalid HTTP path"))?;
    let response = client::send(request).await.map_err(error)?;
    sent.await.map_err(error)?;
    Ok(response)
}
struct Body {
    bytes: wit_bindgen::StreamReader<u8>,
    completion: Option<wit_bindgen::FutureReader<Result<Option<wasi::Fields>, wasi::ErrorCode>>>,
    ended: bool,
}
pub struct Reader {
    status: u16,
    body: RefCell<Body>,
}
impl Reader {
    fn new(response: wasi::Response) -> Self {
        let status = response.get_status_code();
        let (done, done_read) = crate::wit_future::new(|| Ok(()));
        drop(done);
        let (bytes, completion) = wasi::Response::consume_body(response, done_read);
        Self {
            status,
            body: RefCell::new(Body {
                bytes,
                completion: Some(completion),
                ended: false,
            }),
        }
    }
    pub fn status(&self) -> u16 {
        self.status
    }
    async fn read(&self, max: u32, timeout_ms: Option<u32>) -> Result<Chunk, Error> {
        let mut body = self.body.borrow_mut();
        if body.ended {
            return Ok(Chunk {
                bytes: vec![],
                closed: true,
            });
        }
        if max == 0 {
            return Err(invalid("read size must be positive"));
        }
        let (bytes, timed_out) = {
            let mut read = std::pin::pin!(
                body.bytes
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
            body.ended = true;
            body.completion.take().unwrap().await.map_err(error)?;
        }
        Ok(Chunk { bytes, closed })
    }
    pub fn receive(&self, max: u32, timeout_ms: u32) -> Result<Chunk, Error> {
        wit_bindgen::block_on(self.read(max, Some(timeout_ms)))
    }
}
async fn response_body(response: wasi::Response) -> Result<Response, Error> {
    let status = response.get_status_code();
    let headers = response
        .get_headers()
        .copy_all()
        .into_iter()
        .map(|(name, value)| Header { name, value })
        .collect::<Vec<_>>();
    let media_type = headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-type"))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
        .unwrap_or("application/octet-stream");
    let upload = blobs::open_write(media_type, None)?;
    let reader = Reader::new(response);
    let mut offset = 0;
    loop {
        let chunk = reader.read(64 * 1024, None).await?;
        if !chunk.bytes.is_empty() {
            offset = blobs::write(&upload, offset, &chunk.bytes)?;
        }
        if chunk.closed {
            break;
        }
    }
    Ok(Response {
        status,
        headers,
        body: blobs::finish(&upload)?,
    })
}
pub async fn fetch(input: Request) -> Result<Response, Error> {
    let bytes = input
        .body
        .as_ref()
        .map(blob_bytes)
        .transpose()?
        .unwrap_or_default();
    let response = request(
        &input.method,
        &input.url,
        &input.headers,
        bytes,
        input.timeout_ms,
    )
    .await?;
    response_body(response).await
}
pub fn send(input: &Request) -> Result<Response, Error> {
    wit_bindgen::block_on(async {
        let bytes = input
            .body
            .as_ref()
            .map(blob_bytes)
            .transpose()?
            .unwrap_or_default();
        let response = request(
            &input.method,
            &input.url,
            &input.headers,
            bytes,
            input.timeout_ms,
        )
        .await?;
        response_body(response).await
    })
}
pub fn sse(input: &Request) -> Result<Reader, Error> {
    wit_bindgen::block_on(async {
        let bytes = input
            .body
            .as_ref()
            .map(blob_bytes)
            .transpose()?
            .unwrap_or_default();
        let response = request(
            &input.method,
            &input.url,
            &input.headers,
            bytes,
            input.timeout_ms,
        )
        .await?;
        Ok(Reader::new(response))
    })
}
pub fn exchange(input: &InlineRequest) -> Result<InlineResponse, Error> {
    wit_bindgen::block_on(async {
        let response = request(
            &input.method,
            &input.url,
            &input.headers,
            input.body.clone(),
            input.timeout_ms,
        )
        .await?;
        let reader = Reader::new(response);
        let status = reader.status();
        let mut body = Vec::new();
        loop {
            let chunk = reader.read(64 * 1024, None).await?;
            body.extend(chunk.bytes);
            if body.len() > 1024 * 1024 {
                return Err(invalid("inline HTTP response too large"));
            }
            if chunk.closed {
                break;
            }
        }
        Ok(InlineResponse { status, body })
    })
}
