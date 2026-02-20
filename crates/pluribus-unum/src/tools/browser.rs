mod click;
mod close;
mod content;
mod evaluate;
mod navigate;
mod type_text;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use async_lock::Mutex as AsyncMutex;

use async_channel::Sender;
use async_tungstenite::tungstenite::Message as WsMessage;
use futures_lite::future::poll_fn;
use futures_lite::StreamExt;
use futures_sink::Sink;
use serde_json::{json, Value};

use pluribus_frequency::state::Configuration;

use super::{Provider, Tools};

/// Produce a valid JavaScript string literal (including quotes) from a Rust
/// string.  JSON encoding handles all special characters — backslashes,
/// quotes, newlines, unicode — and the result is a valid JS string literal.
pub fn js_string_literal(s: &str) -> String {
    serde_json::to_string(s).expect("string is always valid JSON")
}

/// An open browser tab with its Chrome target ID and CDP session.
struct OpenTab {
    target_id: String,
    session: Arc<Session>,
}

/// Check whether Google Chrome is installed at any known path.
#[must_use]
pub fn chrome_available() -> bool {
    find_chrome_path().is_some()
}

/// Try known Chrome/Chromium paths and return the first one that exists.
fn find_chrome_path() -> Option<&'static str> {
    const CANDIDATES: &[&str] = &[
        // macOS
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        // Linux
        "/usr/bin/google-chrome-stable",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
    ];
    let path = CANDIDATES
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).exists());
    if path.is_none() {
        tracing::info!("Google Chrome not found, browser tools disabled");
    }
    path
}

/// A single Chrome process started at boot.
///
/// Owns the child process and provides an HTTP API for creating/closing tabs.
struct Chrome {
    child: Mutex<Option<std::process::Child>>,
    port: u16,
}

impl Chrome {
    /// Spawn a headless Chrome process.
    ///
    /// Returns `None` if Google Chrome is not installed at any known path.
    async fn launch() -> Option<Self> {
        tracing::info!("launching Chrome");

        let chrome_path = find_chrome_path()?;

        // Bind to port 0 to let the OS assign a free port, then release it
        // so Chrome can bind to it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);

        let data_dir = std::env::temp_dir().join("pluribus-chrome");
        let data_dir_str = data_dir.display().to_string();

        let mut child = match std::process::Command::new(chrome_path)
            .args([
                // "--headless=new",
                "--disable-gpu",
                "--disable-dev-shm-usage",
                "--disable-extensions",
                "--disable-background-networking",
                "--disable-default-apps",
                "--disable-sync",
                "--disable-translate",
                "--no-first-run",
                &format!("--user-data-dir={data_dir_str}"),
                &format!("--remote-debugging-port={port}"),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::info!("Google Chrome not available: {e}");
                return None;
            }
        };

        tracing::debug!(port, "waiting for CDP to be ready");
        if let Err(e) = wait_for_cdp_ready(port).await {
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::error!("Chrome exited with status: {status}");
                }
                Ok(None) => {
                    tracing::warn!("Chrome is still running but CDP is not reachable");
                }
                Err(err) => {
                    tracing::error!("failed to check Chrome process status: {err}");
                }
            }
            tracing::error!("Chrome CDP not ready: {e}");
            let _ = child.kill();
            return None;
        }

        tracing::info!(port, "Chrome ready");
        Some(Self {
            child: Mutex::new(Some(child)),
            port,
        })
    }

    /// Create a new tab via Chrome's HTTP API.
    ///
    /// Returns `(target_id, ws_url)`.
    async fn new_tab(&self) -> Result<(String, String), String> {
        let url = format!("http://127.0.0.1:{}/json/new?about:blank", self.port);
        let mut response = isahc::put_async(&url, ())
            .await
            .map_err(|e| format!("new tab request failed: {e}"))?;
        let body = isahc::AsyncReadResponseExt::text(&mut response)
            .await
            .map_err(|e| format!("new tab response read failed: {e}"))?;
        let json: Value =
            serde_json::from_str(&body).map_err(|e| format!("new tab response parse: {e}"))?;
        let target_id = json
            .get("id")
            .and_then(Value::as_str)
            .ok_or("missing id in new tab response")?
            .to_owned();
        let ws_url = json
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or("missing webSocketDebuggerUrl in new tab response")?
            .to_owned();
        tracing::debug!(target_id, ws_url, "new tab created");
        Ok((target_id, ws_url))
    }

    /// Close a tab via Chrome's HTTP API.
    async fn close_tab(&self, target_id: &str) -> Result<(), String> {
        let url = format!("http://127.0.0.1:{}/json/close/{target_id}", self.port);
        isahc::get_async(&url)
            .await
            .map_err(|e| format!("close tab request failed: {e}"))?;
        tracing::debug!(target_id, "tab closed");
        Ok(())
    }
}

impl Chrome {
    /// Check whether the Chrome child process is still running.
    fn is_alive(&self) -> bool {
        self.child
            .lock()
            .expect("chrome lock")
            .as_mut()
            .is_some_and(|c| c.try_wait().ok().flatten().is_none())
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let child = self.child.lock().expect("chrome lock").take();
        if let Some(mut child) = child {
            tracing::debug!("killing Chrome process");
            let _ = child.kill();
        }
    }
}

/// Poll Chrome's CDP HTTP endpoint until it responds successfully.
async fn wait_for_cdp_ready(port: u16) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    for _ in 0..300 {
        async_io::Timer::after(std::time::Duration::from_millis(100)).await;
        if isahc::get_async(&url).await.is_ok() {
            return Ok(());
        }
    }
    tracing::warn!(port, "Chrome did not start within 30s");
    Err(format!("Chrome did not start on port {port} within 30s"))
}

/// Single-tab browser controller.
///
/// At most one tab is open at a time. Opening a new tab closes the previous
/// one automatically.
#[derive(Clone)]
pub struct Sessions {
    tab: Arc<Mutex<Option<OpenTab>>>,
    chrome: Arc<AsyncMutex<Option<Arc<Chrome>>>>,
    executor: Arc<async_executor::Executor<'static>>,
}

impl Sessions {
    /// Create a new single-tab controller that lazily launches Chrome on first use.
    #[must_use]
    pub fn new(executor: Arc<async_executor::Executor<'static>>) -> Self {
        Self {
            tab: Arc::new(Mutex::new(None)),
            chrome: Arc::new(AsyncMutex::new(None)),
            executor,
        }
    }

    /// Ensure Chrome is running, launching it on first call.
    ///
    /// If Chrome was previously launched but has since crashed, the stale
    /// reference is cleared and a fresh instance is started.
    async fn ensure_chrome(&self) -> Result<Arc<Chrome>, String> {
        let mut guard = self.chrome.lock().await;
        if let Some(c) = guard.as_ref() {
            if c.is_alive() {
                return Ok(c.clone());
            }
            tracing::warn!("Chrome process died, relaunching");
            *guard = None;
        }
        let chrome = Arc::new(Chrome::launch().await.ok_or("Chrome not available")?);
        *guard = Some(chrome.clone());
        drop(guard);
        Ok(chrome)
    }

    /// Get the current tab's session, or error if no tab is open.
    pub fn session(&self) -> Result<Arc<Session>, String> {
        self.tab
            .lock()
            .expect("tab lock")
            .as_ref()
            .map(|t| t.session.clone())
            .ok_or_else(|| "no browser tab open".to_owned())
    }

    /// Open a new browser tab.
    ///
    /// If a tab is already open it is closed first.
    pub async fn open_tab(&self) -> Result<(), String> {
        let chrome = self.ensure_chrome().await?;

        // Close the previous tab if one exists.
        let old = { self.tab.lock().expect("tab lock").take() };
        if let Some(old) = old {
            tracing::info!(target_id = %old.target_id, "closing previous tab");
            let _ = chrome.close_tab(&old.target_id).await;
        }

        let (target_id, ws_url) = chrome.new_tab().await?;

        // Connect TCP then upgrade to WebSocket.
        let addr = ws_url.strip_prefix("ws://").ok_or("expected ws:// URL")?;
        let host_port = addr.split('/').next().ok_or("malformed ws URL")?;
        let stream = async_net::TcpStream::connect(host_port)
            .await
            .map_err(|e| format!("TCP connect failed: {e}"))?;
        let (ws, _) = async_tungstenite::client_async(&ws_url, stream)
            .await
            .map_err(|e| format!("websocket connect failed: {e}"))?;

        // Channel for writing to the WebSocket.
        let (ws_tx, ws_rx) = async_channel::bounded::<String>(64);

        // Event broadcast.
        let (mut events_tx, events_rx) = async_broadcast::broadcast(64);
        events_tx.set_await_active(false);
        events_tx.set_overflow(true);
        let events_keep_alive = events_rx.deactivate();

        let session = Arc::new(Session {
            ws_tx,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            events_tx: events_tx.clone(),
            _events_keep_alive: events_keep_alive,
        });

        // Single I/O task: handles both reading from WS and writing to WS.
        let session_weak = Arc::downgrade(&session);
        self.executor
            .spawn(async move {
                ws_io_loop(ws, ws_rx, session_weak, events_tx).await;
            })
            .detach();

        {
            let mut tab = self.tab.lock().expect("tab lock");
            *tab = Some(OpenTab {
                target_id: target_id.clone(),
                session,
            });
        }

        tracing::info!(target_id = %target_id, "browser tab opened");
        Ok(())
    }

    /// Close the current browser tab.
    ///
    /// Errors if no tab is open.
    pub async fn close_tab(&self) -> Result<(), String> {
        let old = { self.tab.lock().expect("tab lock").take() };
        let old = old.ok_or("no browser tab open")?;
        tracing::info!(target_id = %old.target_id, "closing browser tab");
        let chrome = self.ensure_chrome().await?;
        chrome.close_tab(&old.target_id).await?;
        tracing::info!(target_id = %old.target_id, "browser tab closed");
        Ok(())
    }
}

/// A browser tab connected via CDP over `WebSocket`.
pub struct Session {
    /// Channel for sending messages to the `WebSocket` writer task.
    ws_tx: Sender<String>,
    /// Next CDP request ID.
    next_id: AtomicU64,
    /// Pending request ID -> oneshot response channel.
    pending: Mutex<HashMap<u64, async_channel::Sender<Value>>>,
    /// Event broadcast sender.
    events_tx: async_broadcast::Sender<(String, Value)>,
    /// Keeps the broadcast channel alive even when no active receivers exist.
    _events_keep_alive: async_broadcast::InactiveReceiver<(String, Value)>,
}

impl Session {
    /// Send a CDP command and await its response.
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        tracing::debug!(method, "sending CDP command");
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = async_channel::bounded(1);
        {
            let mut pending = self.pending.lock().expect("pending lock");
            pending.insert(id, tx);
        }
        let msg = json!({"id": id, "method": method, "params": params}).to_string();
        self.ws_tx
            .send(msg)
            .await
            .map_err(|e| format!("ws send failed: {e}"))?;

        let response = rx
            .recv()
            .await
            .map_err(|e| format!("ws recv failed: {e}"))?;

        if let Some(error) = response.get("error") {
            tracing::warn!(method, %error, "CDP command error");
            return Err(format!("CDP error: {error}"));
        }

        tracing::debug!(method, "CDP command succeeded");
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Create a subscription for a specific CDP event.
    ///
    /// The returned [`EventSubscription`] captures events from the moment of
    /// creation, so subscribing *before* sending the command that triggers the
    /// event eliminates the race where the event fires before the receiver
    /// exists.
    pub fn subscribe(&self, target_method: &str) -> EventSubscription {
        EventSubscription {
            rx: self.events_tx.new_receiver(),
            target_method: target_method.to_owned(),
        }
    }
}

/// A pre-created event subscription that can be awaited later.
///
/// Holds a broadcast receiver that captures events from the moment the
/// subscription is created.
pub struct EventSubscription {
    rx: async_broadcast::Receiver<(String, Value)>,
    target_method: String,
}

impl EventSubscription {
    /// Wait until the subscribed event fires and return its parameters.
    pub async fn wait(mut self) -> Result<Value, String> {
        tracing::debug!(event = %self.target_method, "waiting for CDP event");
        while let Ok((method, params)) = self.rx.recv().await {
            if method == self.target_method {
                tracing::debug!(event = %self.target_method, "CDP event received");
                return Ok(params);
            }
        }
        tracing::warn!(
            event = %self.target_method,
            "CDP event stream closed while waiting"
        );
        Err("event stream closed".to_owned())
    }
}

/// Combined WS read/write loop using `poll_fn` to multiplex.
async fn ws_io_loop(
    mut ws: async_tungstenite::WebSocketStream<async_net::TcpStream>,
    write_rx: async_channel::Receiver<String>,
    session: std::sync::Weak<Session>,
    events_tx: async_broadcast::Sender<(String, Value)>,
) {
    let mut write_rx = std::pin::pin!(write_rx);
    loop {
        // Poll both the write channel and the WS stream.
        let action = poll_fn(|cx| {
            // Check for inbound WS messages first — CDP responses unblock
            // pending requests and are more latency-sensitive than outbound
            // writes.
            if let Poll::Ready(result) = Pin::new(&mut ws).poll_next(cx) {
                return Poll::Ready(Action::Read(result));
            }
            // Check for outbound messages.
            if let Poll::Ready(result) = write_rx.as_mut().poll_next(cx) {
                return Poll::Ready(Action::Write(result));
            }
            Poll::Pending
        })
        .await;

        match action {
            Action::Write(Some(msg)) => {
                let message = WsMessage::Text(msg);
                if ws_send(&mut ws, message).await.is_err() {
                    break;
                }
            }
            Action::Write(None) | Action::Read(None | Some(Err(_))) => break,
            Action::Read(Some(Ok(WsMessage::Text(text)))) => {
                handle_ws_message(&text, &session, &events_tx).await;
            }
            Action::Read(Some(Ok(_))) => {} // ignore non-text
        }
    }
}

enum Action {
    Write(Option<String>),
    Read(Option<Result<WsMessage, async_tungstenite::tungstenite::Error>>),
}

/// Send a single WS message using the `Sink` protocol.
async fn ws_send(
    ws: &mut async_tungstenite::WebSocketStream<async_net::TcpStream>,
    message: WsMessage,
) -> Result<(), ()> {
    poll_fn(|cx| Pin::new(&mut *ws).poll_ready(cx))
        .await
        .map_err(|_| ())?;
    Pin::new(&mut *ws).start_send(message).map_err(|_| ())?;
    poll_fn(|cx| Pin::new(&mut *ws).poll_flush(cx))
        .await
        .map_err(|_| ())
}

/// Route an inbound CDP message to the appropriate handler.
async fn handle_ws_message(
    text: &str,
    session: &std::sync::Weak<Session>,
    events_tx: &async_broadcast::Sender<(String, Value)>,
) {
    let Ok(json) = serde_json::from_str::<Value>(text) else {
        return;
    };

    if let Some(id) = json.get("id").and_then(Value::as_u64) {
        // Response to a request.
        let Some(session) = session.upgrade() else {
            return;
        };
        let tx = {
            let mut pending = session.pending.lock().expect("pending lock");
            pending.remove(&id)
        };
        drop(session);
        if let Some(tx) = tx {
            let _ = tx.send(json).await;
        }
    } else if let Some(method) = json.get("method").and_then(Value::as_str) {
        // CDP event.
        let params = json.get("params").cloned().unwrap_or(Value::Null);
        let _ = events_tx.broadcast((method.to_owned(), params)).await;
    }
}

/// Result of navigating to a URL.
pub struct NavigateResult {
    pub title: String,
    pub url: String,
}

/// Navigate a session to a URL, wait for the page to load, and return the
/// page title and final URL.
pub async fn navigate(session: &Session, url: &str) -> Result<NavigateResult, String> {
    session.send("Page.enable", json!({})).await?;

    let load_sub = session.subscribe("Page.domContentEventFired");

    let nav_result = session.send("Page.navigate", json!({"url": url})).await?;

    if let Some(error_text) = nav_result.get("errorText").and_then(Value::as_str) {
        return Err(format!("navigation failed: {error_text}"));
    }

    let _ = load_sub.wait().await;

    let result = session
        .send("Runtime.evaluate", json!({"expression": "document.title"}))
        .await?;
    let title = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    let result = session
        .send(
            "Runtime.evaluate",
            json!({"expression": "window.location.href"}),
        )
        .await?;
    let final_url = result
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    Ok(NavigateResult {
        title,
        url: final_url,
    })
}

pub struct Browser {
    sessions: Option<Sessions>,
}

impl Browser {
    /// Create a browser provider that lazily launches Chrome on first use.
    #[must_use]
    pub fn new(executor: Arc<async_executor::Executor<'static>>) -> Self {
        Self {
            sessions: Some(Sessions::new(executor)),
        }
    }

    /// Create a browser provider with no Chrome available.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self { sessions: None }
    }
}

impl Provider for Browser {
    fn resolve(&self, _config: &Configuration) -> Tools {
        let mut tools = Tools::new();
        let Some(s) = &self.sessions else {
            return tools;
        };
        tools.register(close::new(s.clone()));
        tools.register(navigate::new(s.clone()));
        tools.register(content::new(s.clone()));
        tools.register(click::new(s.clone()));
        tools.register(type_text::new(s.clone()));
        tools.register(evaluate::new(s.clone()));
        tools
    }
}
