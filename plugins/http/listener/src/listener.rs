use crate::native;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, Response, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Semaphore, oneshot};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub path: String,
    pub id: String,
    pub consumer: String,
    pub methods: Vec<String>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub socket: PathBuf,
    pub runtime_uid: u32,
    pub routes: Vec<Route>,
    #[serde(default = "body_limit")]
    pub max_body_bytes: usize,
    #[serde(default = "queue_limit")]
    pub max_queue_bytes: usize,
    #[serde(default = "deadline")]
    pub response_timeout_seconds: u64,
}
fn body_limit() -> usize {
    1024 * 1024
}
fn queue_limit() -> usize {
    256 * 1024 * 1024
}
fn deadline() -> u64 {
    8
}
struct Inbox {
    session: String,
    sequence: u64,
    bytes: usize,
    messages: VecDeque<(u64, Value, usize)>,
}
impl Default for Inbox {
    fn default() -> Self {
        Self {
            session: native::random(),
            sequence: 0,
            bytes: 0,
            messages: VecDeque::new(),
        }
    }
}
impl Inbox {
    fn enqueue(&mut self, value: Value, limit: usize) -> Result<(), ()> {
        let size = value.to_string().len();
        if self.bytes.saturating_add(size) > limit {
            return Err(());
        }
        self.sequence = self.sequence.checked_add(1).ok_or(())?;
        self.bytes += size;
        self.messages.push_back((self.sequence, value, size));
        Ok(())
    }
    fn poll(&mut self, session: Option<&str>, after: u64) -> Value {
        if session == Some(self.session.as_str()) {
            while self
                .messages
                .front()
                .is_some_and(|(sequence, _, _)| *sequence <= after)
            {
                self.bytes -= self.messages.pop_front().unwrap().2;
            }
        }
        let messages: Vec<_> = self
            .messages
            .front()
            .map(|(sequence, request, _)| json!({"sequence":sequence,"request":request}))
            .into_iter()
            .collect();
        json!({"session":self.session,"messages":messages})
    }
    fn remove(&mut self, id: &str) {
        if let Some(index) = self
            .messages
            .iter()
            .position(|(_, value, _)| value["requestId"] == id)
        {
            self.bytes -= self.messages.remove(index).unwrap().2;
        }
    }
}

struct Shared {
    config: Config,
    inbox: Mutex<Inbox>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    capacity: Arc<Semaphore>,
    changed: tokio::sync::watch::Sender<u64>,
}

struct Pending {
    shared: Arc<Shared>,
    id: String,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.shared.pending.lock().unwrap().remove(&self.id);
        self.shared.inbox.lock().unwrap().remove(&self.id);
    }
}

pub async fn serve(config: Config) -> io::Result<()> {
    if !config.listen.ip().is_loopback()
        || !(1..=25 * 1024 * 1024).contains(&config.max_body_bytes)
        || !(1..=60).contains(&config.response_timeout_seconds)
    {
        return Err(io::Error::other("invalid listener limits or address"));
    }
    let mut paths = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    for route in &config.routes {
        if !route.path.starts_with('/')
            || route.path.contains(['?', '#'])
            || route.consumer.is_empty()
            || route.id.is_empty()
            || route.methods.is_empty()
            || !paths.insert(&route.path)
            || !ids.insert(&route.id)
        {
            return Err(io::Error::other("invalid or duplicate HTTP route"));
        }
    }
    let tcp = tokio::net::TcpListener::bind(config.listen).await?;
    let socket = native::bind(&config.socket, 0o600)?;
    let shared = Arc::new(Shared {
        config,
        inbox: Mutex::new(Inbox::default()),
        pending: Mutex::new(HashMap::new()),
        capacity: Arc::new(Semaphore::new(64)),
        changed: tokio::sync::watch::channel(0).0,
    });
    let unix = unix_loop(socket, shared.clone());
    let http = axum::serve(tcp, Router::new().fallback(receive).with_state(shared));
    tokio::select! { result=unix => result, result=http => result }
}
fn status(code: u16) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .unwrap()
}
async fn receive(State(shared): State<Arc<Shared>>, request: Request<Body>) -> Response<Body> {
    let started = tokio::time::Instant::now();
    let received_at = native::now() * 1000;
    let Some(route) = shared.config.routes.iter().find(|r| {
        r.path == request.uri().path()
            || r.path
                .strip_suffix('*')
                .is_some_and(|prefix| request.uri().path().starts_with(prefix))
    }) else {
        return status(404);
    };
    if !route.methods.iter().any(|m| m == request.method().as_str()) {
        return status(405);
    }
    let Ok(_permit) = shared.capacity.clone().try_acquire_owned() else {
        return status(503);
    };
    let (parts, body) = request.into_parts();
    if parts
        .headers
        .iter()
        .map(|(k, v)| k.as_str().len() + v.as_bytes().len())
        .sum::<usize>()
        > 32 * 1024
    {
        return status(431);
    }
    let body = match tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(body, shared.config.max_body_bytes),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return status(413),
        Err(_) => return status(408),
    };
    let id = native::random();
    let (tx, rx) = oneshot::channel();
    let headers: Vec<_> = parts
        .headers
        .iter()
        .map(|(k, v)| json!([k.as_str(), STANDARD.encode(v.as_bytes())]))
        .collect();
    let value = json!({"requestId":id,"routeId":route.id,"consumer":route.consumer,"method":parts.method.as_str(),"target":parts.uri.to_string(),"headers":headers,"body":STANDARD.encode(&body),"receivedAtMs":received_at,"deadlineAtMs":received_at+shared.config.response_timeout_seconds as i64*1000});
    shared.pending.lock().unwrap().insert(id.clone(), tx);
    let _pending = Pending {
        shared: shared.clone(),
        id: id.clone(),
    };
    let accepted = {
        shared
            .inbox
            .lock()
            .unwrap()
            .enqueue(value, shared.config.max_queue_bytes)
    };
    if accepted.is_err() {
        shared.pending.lock().unwrap().remove(&id);
        return status(503);
    }
    shared
        .changed
        .send_modify(|version| *version = version.wrapping_add(1));
    let result = tokio::time::timeout(
        Duration::from_secs(shared.config.response_timeout_seconds)
            .saturating_sub(started.elapsed()),
        rx,
    )
    .await;
    shared.pending.lock().unwrap().remove(&id);
    match result {
        Ok(Ok(value)) => make_response(&value).unwrap_or_else(|| status(502)),
        _ => status(504),
    }
}
fn make_response(v: &Value) -> Option<Response<Body>> {
    let code = u16::try_from(v["status"].as_u64()?).ok()?;
    if !(200..=599).contains(&code) {
        return None;
    }
    let body = STANDARD.decode(v["body"].as_str().unwrap_or("")).ok()?;
    if body.len() > 1024 * 1024 {
        return None;
    }
    let mut response = Response::builder().status(StatusCode::from_u16(code).ok()?);
    if let Some(headers) = v["headers"].as_array() {
        if headers.len() > 64 {
            return None;
        }
        for pair in headers {
            let name = pair[0].as_str()?;
            let value = pair[1].as_str()?;
            if [
                "connection",
                "transfer-encoding",
                "content-length",
                "upgrade",
                "keep-alive",
                "trailer",
            ]
            .contains(&name.to_ascii_lowercase().as_str())
                || name.len() + value.len() > 8192
            {
                return None;
            }
            response = response.header(name, value);
        }
    }
    response.body(Body::from(body)).ok()
}
async fn unix_loop(listener: tokio::net::UnixListener, shared: Arc<Shared>) -> io::Result<()> {
    let capacity = Arc::new(Semaphore::new(16));
    let mut clients = tokio::task::JoinSet::new();
    loop {
        while clients.try_join_next().is_some() {}
        let permit = capacity
            .clone()
            .acquire_owned()
            .await
            .map_err(io::Error::other)?;
        let (mut stream, _) = listener.accept().await?;
        // A peer that disconnects before it is accepted fails `peer_cred`; that
        // drops the connection, not the loop.
        let Ok(peer) = stream.peer_cred() else {
            continue;
        };
        if peer.uid() != shared.config.runtime_uid {
            continue;
        }
        let shared = shared.clone();
        clients.spawn(async move {
            let _permit = permit;
            let response = match native::read(&mut stream).await {
                Ok(v) if v["op"] == "subscribe" => {
                    let _ = subscribe(&mut stream, &shared).await;
                    return;
                }
                Ok(v) => dispatch(&shared, &v).await,
                Err(_) => json!({"error":"invalid request"}),
            };
            let _ = native::write(&mut stream, &response).await;
        });
    }
}
async fn subscribe(stream: &mut tokio::net::UnixStream, shared: &Shared) -> io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut changed = shared.changed.subscribe();
    if !shared.inbox.lock().unwrap().messages.is_empty() {
        stream.write_all(b"\n").await?;
    }
    loop {
        let mut byte = [0];
        tokio::select! {
            result = changed.changed() => {
                result.map_err(io::Error::other)?;
                stream.write_all(b"\n").await?;
            }
            _ = stream.read(&mut byte) => return Ok(()),
        }
    }
}
async fn dispatch(shared: &Shared, v: &Value) -> Value {
    match v["op"].as_str() {
        Some("poll") => {
            let Some(after) = v["after"].as_u64() else {
                return json!({"error":"invalid cursor"});
            };
            let mut inbox = shared.inbox.lock().unwrap();
            let result = inbox.poll(v["session"].as_str(), after);
            if inbox.messages.len() > 1 {
                shared
                    .changed
                    .send_modify(|version| *version = version.wrapping_add(1));
            }
            result
        }
        Some("respond") => {
            if make_response(v).is_none() {
                return json!({"error":"invalid response"});
            }
            let Some(id) = v["requestId"].as_str() else {
                return json!({"error":"missing request"});
            };
            let pending = shared.pending.lock().unwrap().remove(id);
            match pending {
                Some(tx) => json!({"sent":tx.send(v.clone()).is_ok()}),
                None => json!({"sent":false}),
            }
        }
        _ => json!({"error":"unsupported operation"}),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn accepts_deployed_response_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: directory.path().join("listener.sock"),
            runtime_uid: rustix::process::geteuid().as_raw(),
            routes: vec![],
            max_body_bytes: body_limit(),
            max_queue_bytes: queue_limit(),
            response_timeout_seconds: 60,
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(100), serve(config))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_disconnected_peer_does_not_stop_the_socket_loop() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("listener.sock");
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            socket: socket.clone(),
            runtime_uid: rustix::process::geteuid().as_raw(),
            routes: vec![],
            max_body_bytes: body_limit(),
            max_queue_bytes: queue_limit(),
            response_timeout_seconds: 30,
        };
        let served = tokio::spawn(serve(config));
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        // Closed before the loop accepts it: `peer_cred` then reports ENOTCONN.
        drop(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let mut client = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match tokio::net::UnixStream::connect(&socket).await {
                    Ok(stream) => return stream,
                    Err(_) => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .unwrap();
        native::write(&mut client, &json!({"op":"poll","after":0}))
            .await
            .unwrap();
        assert!(
            native::read(&mut client).await.unwrap()["session"].is_string(),
            "listener stopped serving after a peer disconnected"
        );
        served.abort();
    }

    #[test]
    fn inbox_bounds_acknowledges_and_resets_on_restart() {
        let mut inbox = Inbox::default();
        let request = json!({"requestId":"first","body":"raw"});
        let size = request.to_string().len();
        inbox.enqueue(request.clone(), size).unwrap();
        assert!(inbox.enqueue(json!({"requestId":"second"}), size).is_err());
        let session = inbox.session.clone();
        assert_eq!(inbox.poll(None, 99)["messages"][0]["request"], request);
        assert_eq!(
            inbox.poll(Some(&session), 0)["messages"][0]["request"],
            request
        );
        assert_eq!(inbox.poll(Some(&session), 1)["messages"], json!([]));
        assert_eq!(inbox.bytes, 0);
        let mut restarted = Inbox::default();
        restarted.enqueue(request.clone(), size).unwrap();
        assert_ne!(restarted.session, session);
        assert_eq!(
            restarted.poll(Some(&session), 99)["messages"][0]["request"],
            request
        );
        restarted.remove("first");
        assert_eq!(restarted.bytes, 0);
        assert!(restarted.messages.is_empty());
    }
    #[test]
    fn response_rejects_informational_status_and_header_injection() {
        for v in [
            json!({"status":101}),
            json!({"status":200,"headers":[["x-test","a\r\nb"]]}),
            json!({"status":200,"headers":[["Content-Length","9"]]}),
        ] {
            assert!(make_response(&v).is_none());
        }
        assert!(make_response(&json!({"status":204})).is_some());
    }

    #[tokio::test]
    async fn response_is_once_and_timed_out_connections_stay_closed() {
        let shared = Arc::new(Shared {
            config: Config {
                listen: "127.0.0.1:0".parse().unwrap(),
                socket: PathBuf::from("/unused"),
                runtime_uid: 1,
                routes: vec![Route {
                    path: "/events".into(),
                    id: "test".into(),
                    consumer: "consumer".into(),
                    methods: vec!["POST".into()],
                }],
                max_body_bytes: 1024,
                max_queue_bytes: 8192,
                response_timeout_seconds: 1,
            },
            inbox: Mutex::new(Inbox::default()),
            pending: Mutex::new(HashMap::new()),
            capacity: Arc::new(Semaphore::new(2)),
            changed: tokio::sync::watch::channel(0).0,
        });
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/events")
                .body(Body::from("raw"))
                .unwrap()
        };
        let mut changed = shared.changed.subscribe();
        let first = tokio::spawn(receive(State(shared.clone()), request()));
        changed.changed().await.unwrap();
        let inbox = dispatch(&shared, &json!({"op":"poll","after":0})).await;
        let id = inbox["messages"][0]["request"]["requestId"].clone();
        assert!(id.is_string());
        let response = json!({"op":"respond","requestId":id,"status":204});
        assert_eq!(dispatch(&shared, &response).await["sent"], true);
        assert_eq!(dispatch(&shared, &response).await["sent"], false);
        assert_eq!(first.await.unwrap().status(), 204);
        let second = tokio::spawn(receive(State(shared.clone()), request()));
        changed.changed().await.unwrap();
        let inbox = dispatch(&shared, &json!({"op":"poll","after":1})).await;
        let id = inbox["messages"][0]["request"]["requestId"].clone();
        assert_eq!(second.await.unwrap().status(), 504);
        assert_eq!(
            dispatch(
                &shared,
                &json!({"op":"respond","requestId":id,"status":200})
            )
            .await["sent"],
            false
        );
        assert!(shared.inbox.lock().unwrap().messages.is_empty());
        assert_eq!(shared.inbox.lock().unwrap().bytes, 0);
    }
}
