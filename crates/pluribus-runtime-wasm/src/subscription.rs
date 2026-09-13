//! Instance-scoped subscriptions with one uncommitted input at a time.

use super::{
    Arc, BlobRef, Duration, HostState, HttpGrant, HttpRequest, HttpStreamService, StreamGrant,
    StreamService, Value,
};
use tokio::sync::{mpsc, oneshot};

pub(super) enum Request {
    Socket(Vec<u8>),
    Http(HttpRequest),
}
pub(super) struct Input {
    pub payload: Value,
    pub blob: Option<BlobRef>,
    pub id: [u8; 16],
    pub committed: oneshot::Sender<()>,
}
pub(super) struct Subscription {
    task: tokio::task::JoinHandle<()>,
    pub inbox: mpsc::Receiver<Input>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Connection {
    service: Arc<dyn StreamService>,
    id: String,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.service.close(&self.id);
    }
}
impl Subscription {
    pub fn start(host: &HostState, request: Request) -> Self {
        let (sender, inbox) = mpsc::channel(1);
        let pump = Pump {
            sender,
            progress: host.progress.clone(),
        };
        let socket = host.stream.clone().zip(host.stream_grant.clone());
        let http = host.http.clone().zip(host.http_grant.clone());
        let task = tokio::spawn(async move {
            let mut delay = Duration::from_millis(100);
            loop {
                let result = match &request {
                    Request::Socket(bytes) => {
                        let (service, grant) = socket.as_ref().unwrap();
                        pump.socket(service.clone(), grant, bytes, &mut delay).await
                    }
                    Request::Http(request) => {
                        let (service, grant) = http.as_ref().unwrap();
                        pump.http(service.as_ref(), grant, request).await
                    }
                };
                if let Err(reason) = result {
                    if pump
                        .deliver(serde_json::json!({"kind":"error","reason":reason}), None)
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                } else {
                    delay = Duration::from_millis(100);
                }
            }
        });
        Self { task, inbox }
    }
}
struct Pump {
    sender: mpsc::Sender<Input>,
    progress: Arc<tokio::sync::Notify>,
}
impl Pump {
    async fn socket(
        &self,
        service: Arc<dyn StreamService>,
        grant: &StreamGrant,
        request: &[u8],
        retry_delay: &mut Duration,
    ) -> Result<(), String> {
        let connection = Connection {
            id: service
                .subscribe(grant)
                .await
                .map_err(|e| format!("{e:?}"))?,
            service: service.clone(),
        };
        service
            .send(&connection.id, request)
            .await
            .map_err(|e| format!("{e:?}"))?;
        loop {
            let page = service
                .next(&connection.id, 32 * 1024)
                .await
                .map_err(|e| format!("{e:?}"))?;
            if !page.bytes.is_empty() {
                *retry_delay = Duration::from_millis(100);
                self.deliver(
                    serde_json::json!({"kind":"socket","bytes":page.bytes}),
                    None,
                )
                .await?;
            }
            if page.closed {
                return Err("subscription peer disconnected".into());
            }
        }
    }

    async fn http(
        &self,
        service: &dyn HttpStreamService,
        grant: &HttpGrant,
        request: &HttpRequest,
    ) -> Result<(), String> {
        let response = service
            .send(grant, request)
            .await
            .map_err(|e| e.to_string())?;
        if !(200..300).contains(&response.status) {
            return Err(format!("subscription HTTP status {}", response.status));
        }
        self.deliver(serde_json::json!({"kind":"http","status":response.status,"body":{
            "digest":response.body.digest,"size":response.body.size,"mediaType":response.body.media_type
        }}), Some(response.body)).await
    }

    async fn deliver(&self, payload: Value, blob: Option<BlobRef>) -> Result<(), String> {
        let (committed, done) = oneshot::channel();
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|e| e.to_string())?;
        self.sender
            .send(Input {
                payload,
                blob,
                id,
                committed,
            })
            .await
            .map_err(|e| e.to_string())?;
        self.progress.notify_one();
        done.await.map_err(|e| e.to_string())
    }
}
