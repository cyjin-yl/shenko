//! Real browser protocol adapters: Chrome DevTools Protocol (Chromium family)
//! and WebDriver BiDi (Firefox family), both spoken over a real WebSocket
//! connection to a real running browser process.
//!
//! docs/specs/browser.md#spec-24-6, docs/specs/repl.md#spec-19-1, docs/specs/architecture.md#spec-41. The adapter layer is deliberately thin and honest:
//! it speaks the engine's own protocol and reports what actually came back.
//! There is no "simulated page" and no DOM fixture here — a page that cannot
//! be reached is an error, not an empty success.
//!
//! Two rules from docs/specs/browser.md#spec-24-6-1 are encoded structurally:
//! - A persistent profile directory is real profile state (cookies,
//!   localStorage, cache). It is NOT live page state: tabs, in-memory form
//!   state and open WebSockets are never restored by reopening a profile.
//!   `RestoreCoverage` states this per session instead of implying it.
//! - The protocol client is bound to one browser process. A crashed engine is
//!   a dead session, not a resumable one; recovery re-opens the profile and
//!   reports a NEW run identity.
use anyhow::{Context, Result, anyhow, ensure};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Largest page payload the adapter returns as text (bytes).
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Largest script an action may carry.
pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;
/// Largest typed text an action may carry.
pub const MAX_TYPED_CHARS: usize = 4096;
/// Largest number of structured targets an observation carries.
pub const MAX_TARGETS: usize = 256;
/// Largest number of accessibility nodes an observation carries.
pub const MAX_AX_NODES: usize = 256;

// ---------------------------------------------------------------------------
// Shared protocol types
// ---------------------------------------------------------------------------

/// One tab of a live browser session. `id` is the engine's own handle: a
/// CDP target id or a BiDi browsing context id. It is only meaningful
/// inside the session that produced it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabHandle {
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub url: String,
    pub title: String,
    pub group: String,
}

/// Result of one navigation request. `error_text` is the engine's own
/// refusal (e.g. `net::ERR_NAME_NOT_RESOLVED`); it is never rewritten into a
/// success.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavOutcome {
    pub url: String,
    pub loader: Option<String>,
    pub error_text: Option<String>,
    pub ready_state: Option<String>,
}

/// A browser cookie in a protocol-neutral shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; negative means a session cookie.
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: Option<String>,
}

/// What a reopened profile actually restores, and what it never restores.
/// docs/specs/browser.md#spec-24-6-1: storageState is not live page continuity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreCoverage {
    /// Cookies / localStorage / cache written by the real profile directory.
    pub persistent_profile: bool,
    /// Cookies + localStorage can be exported and re-imported as
    /// `storageState` (a weaker, origin-scoped form).
    pub storage_state_exportable: bool,
    /// Live tabs are never restored by reopening a profile.
    pub live_tabs_restored: bool,
    /// In-page memory (forms, JS state, open WebSockets) is never restored.
    pub in_page_state_restored: bool,
    pub note: String,
}

impl Default for RestoreCoverage {
    fn default() -> Self {
        Self {
            persistent_profile: true,
            storage_state_exportable: true,
            live_tabs_restored: false,
            in_page_state_restored: false,
            note: "a reopened profile restores persistent profile state only; live tabs, in-page memory and open sockets are not restored and are never claimed to be".into(),
        }
    }
}

/// A structured page target (link, button, input) as the page itself
/// reports it. No coordinates are invented: a target without a verified
/// bounding box is still returned, marked as such.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageTarget {
    pub role: String,
    pub name: String,
    pub selector: String,
    pub href: Option<String>,
    pub input_type: Option<String>,
    pub value_len: Option<usize>,
    pub visible: bool,
    pub rect: Option<[f64; 4]>,
}

/// One accessibility node in the browser's own AX tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxNode {
    pub role: String,
    pub name: String,
    pub value: Option<String>,
}

/// The page viewport as the page itself reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
}

/// The protocol-neutral browser surface used by the provider.
#[async_trait::async_trait]
pub trait BrowserProtocol: Send + Sync {
    /// "cdp" or "bidi".
    fn kind(&self) -> &'static str;
    /// The real WebSocket endpoint this client is bound to.
    fn endpoint(&self) -> String;
    /// The browser's own reported product/version string.
    fn product(&self) -> String;
    async fn create_tab(&self, url: &str) -> Result<TabHandle>;
    async fn list_tabs(&self) -> Result<Vec<TabInfo>>;
    async fn close_tab(&self, tab: &TabHandle) -> Result<()>;
    async fn navigate(&self, tab: &TabHandle, url: &str) -> Result<NavOutcome>;
    /// Evaluate a script that must produce a JSON string; the adapter parses
    /// it and returns real JSON.
    async fn evaluate_json(&self, tab: &TabHandle, script: &str) -> Result<Value>;
    async fn screenshot(&self, tab: &TabHandle) -> Result<Vec<u8>>;
    /// The browser's own accessibility tree, when the engine exposes one.
    async fn ax_nodes(&self, tab: &TabHandle) -> Result<Option<Vec<AxNode>>>;
    async fn cookies(&self) -> Result<Vec<Cookie>>;
    async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()>;
    /// Insert text at the focused element through the engine's real input
    /// channel (not a scripted value assignment).
    async fn insert_text(&self, tab: &TabHandle, text: &str) -> Result<()>;
    /// Press one named key through the engine's real input channel.
    async fn press_key(&self, tab: &TabHandle, key: &str) -> Result<()>;
    /// Scroll the viewport by a real wheel/gesture amount.
    async fn scroll(&self, tab: &TabHandle, delta_y: f64) -> Result<()>;
    async fn shutdown(&self);
}

// ---------------------------------------------------------------------------
// WebSocket JSON transport
// ---------------------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// A multiplexed JSON-over-WebSocket client. Requests are correlated by the
/// protocol's own numeric id; unsolicited events are consumed, never guessed
/// at, and a closed connection fails every outstanding request instead of
/// hanging it.
pub struct WsClient {
    sink: Mutex<futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>,
    pending: Pending,
    next_id: AtomicU64,
}

impl WsClient {
    pub async fn connect(url: &str) -> Result<Self> {
        let request = url
            .into_client_request()
            .context("browser protocol endpoint is not a valid websocket URL")?;
        let (stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .context("browser protocol websocket handshake failed")?;
        let (sink, mut source) = stream.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(message) = source.next().await {
                let Ok(message) = message else { break };
                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                // Only replies carry an id. Events are consumed here and never
                // mistaken for an answer.
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let waiter = reader_pending.lock().await.remove(&id);
                if let Some(waiter) = waiter {
                    let payload = protocol_result(&value);
                    let _ = waiter.send(payload);
                }
            }
            let mut guard = reader_pending.lock().await;
            for (_, waiter) in guard.drain() {
                let _ = waiter.send(Err(anyhow!("browser protocol connection closed")));
            }
        });
        Ok(Self {
            sink: Mutex::new(sink),
            pending,
            next_id: AtomicU64::new(1),
        })
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let send = self
            .sink
            .lock()
            .await
            .send(Message::text(message.to_string()))
            .await;
        if let Err(error) = send {
            self.pending.lock().await.remove(&id);
            return Err(anyhow!("browser protocol send failed: {error}"));
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("browser protocol connection closed before a reply")),
        }
    }

    pub async fn close(&self) {
        let _ = self.sink.lock().await.close().await;
    }
}

/// Extract the protocol's own error, if the reply carries one.
fn protocol_result(value: &Value) -> Result<Value> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("browser protocol error");
        let detail = error
            .get("data")
            .and_then(Value::as_str)
            .filter(|data| !data.is_empty());
        return Err(anyhow!(
            "{message}{}",
            detail.map(|data| format!(" ({data})")).unwrap_or_default()
        ));
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

// ---------------------------------------------------------------------------
// Minimal HTTP GET for the DevTools HTTP endpoint
// ---------------------------------------------------------------------------

/// Fetch a small JSON document from a loopback HTTP endpoint. Only
/// `127.0.0.1` is ever used: the DevTools endpoint is a local socket owned
/// by the browser process this provider started.
pub async fn http_get_json(host: &str, port: u16, path: &str) -> Result<Value> {
    let mut stream = TcpStream::connect((host, port))
        .await
        .context("connect to browser devtools http endpoint")?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\
         User-Agent: shenko-browser/1\r\nAccept: application/json\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("write devtools http request")?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .context("read devtools http response")?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("devtools http response had no header/body split"))?;
    let status = head.lines().next().unwrap_or_default();
    ensure!(
        status.contains(" 200"),
        "devtools http endpoint answered {status}"
    );
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        decode_chunked(body)?
    } else {
        body.to_string()
    };
    serde_json::from_str(body.trim()).context("devtools http response was not JSON")
}

fn decode_chunked(body: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let (size_line, tail) = rest
            .split_once("\r\n")
            .ok_or_else(|| anyhow!("truncated chunked body"))?;
        let size = usize::from_str_radix(size_line.trim(), 16)
            .context("chunk size was not hexadecimal")?;
        if size == 0 {
            break;
        }
        if tail.len() < size {
            return Err(anyhow!("truncated chunked body"));
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].strip_prefix("\r\n").unwrap_or(&tail[size..]);
    }
    Ok(out)
}
