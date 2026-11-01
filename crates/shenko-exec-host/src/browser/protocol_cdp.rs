//! Chrome DevTools Protocol client (Chromium family).
//!
//! The browser-level WebSocket is used; each tab is attached as a flattened
//! session so page commands never leak into another tab. Every reply is the
//! engine's own reply: an engine refusal is reported as a refusal.
use super::protocol::{
    AxNode, BrowserProtocol, Cookie, NavOutcome, TabHandle, TabInfo, WsClient, http_get_json,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::sync::Mutex;

/// A real CDP client bound to one browser process.
pub struct CdpClient {
    ws: WsClient,
    endpoint: String,
    product: String,
    /// target id -> flattened session id
    attached: Mutex<HashMap<String, String>>,
}

impl CdpClient {
    pub async fn connect(endpoint: String, product: String) -> Result<Self> {
        let ws = WsClient::connect(&endpoint).await?;
        Ok(Self {
            ws,
            endpoint,
            product,
            attached: Mutex::new(HashMap::new()),
        })
    }

    /// Read the browser-level endpoint from the DevTools HTTP endpoint.
    /// The port is the one the browser actually bound, not one we guessed.
    pub async fn discover_endpoint(port: u16) -> Result<(String, String)> {
        let version = http_get_json("127.0.0.1", port, "/json/version")
            .await
            .context("read devtools /json/version")?;
        let endpoint = version
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("devtools /json/version carried no webSocketDebuggerUrl"))?
            .to_string();
        let product = version
            .get("Browser")
            .and_then(Value::as_str)
            .unwrap_or("chromium")
            .to_string();
        Ok((endpoint, product))
    }

    async fn attach(&self, target_id: &str) -> Result<String> {
        if let Some(session) = self.attached.lock().await.get(target_id) {
            return Ok(session.clone());
        }
        let result = self
            .ws
            .request(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
                None,
            )
            .await?;
        let session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("CDP attachToTarget returned no sessionId"))?
            .to_string();
        self.attached
            .lock()
            .await
            .insert(target_id.to_string(), session.clone());
        Ok(session)
    }

    async fn page(&self, tab: &TabHandle) -> Result<String> {
        self.attach(&tab.id).await
    }

    async fn call(&self, tab: &TabHandle, method: &str, params: Value) -> Result<Value> {
        let session = self.page(tab).await?;
        self.ws.request(method, params, Some(&session)).await
    }
}

#[async_trait::async_trait]
impl BrowserProtocol for CdpClient {
    fn kind(&self) -> &'static str {
        "cdp"
    }

    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    fn product(&self) -> String {
        self.product.clone()
    }

    async fn create_tab(&self, url: &str) -> Result<TabHandle> {
        let result = self
            .ws
            .request("Target.createTarget", json!({"url": url}), None)
            .await?;
        let target_id = result
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("CDP createTarget returned no targetId"))?
            .to_string();
        let session = self.attach(&target_id).await?;
        // Enable the domains this adapter reads. A refusal is not fatal, but
        // a domain that stays off is reported by the observation instead of
        // being silently missing.
        let _ = self.ws.request("Page.enable", json!({}), Some(&session)).await;
        let _ = self
            .ws
            .request("Runtime.enable", json!({}), Some(&session))
            .await;
        Ok(TabHandle {
            id: target_id,
            kind: "cdp-target".into(),
        })
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        let result = self.ws.request("Target.getTargets", json!({}), None).await?;
        let mut tabs = Vec::new();
        for info in result
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if info.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            tabs.push(TabInfo {
                tab_id: info
                    .get("targetId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                url: info
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title: info
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                group: String::new(),
            });
        }
        Ok(tabs)
    }

    async fn close_tab(&self, tab: &TabHandle) -> Result<()> {
        self.attached.lock().await.remove(&tab.id);
        self.ws
            .request("Target.closeTarget", json!({"targetId": tab.id}), None)
            .await?;
        Ok(())
    }

    async fn navigate(&self, tab: &TabHandle, url: &str) -> Result<NavOutcome> {
        let result = self
            .call(tab, "Page.navigate", json!({"url": url}))
            .await
            .context("CDP Page.navigate failed")?;
        let error_text = result
            .get("errorText")
            .and_then(Value::as_str)
            .map(str::to_string);
        let ready_state = self
            .call(
                tab,
                "Runtime.evaluate",
                json!({"expression": "document.readyState", "returnByValue": true}),
            )
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("result")
                    .and_then(|inner| inner.get("value"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        Ok(NavOutcome {
            url: url.to_string(),
            loader: result
                .get("loaderId")
                .and_then(Value::as_str)
                .map(str::to_string),
            error_text,
            ready_state,
        })
    }

    async fn evaluate_json(&self, tab: &TabHandle, script: &str) -> Result<Value> {
        let result = self
            .call(
                tab,
                "Runtime.evaluate",
                json!({
                    "expression": script,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await
            .context("CDP Runtime.evaluate failed")?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details
                .get("exception")
                .and_then(|exception| exception.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("page script raised an exception");
            bail!("{text}");
        }
        let value = result
            .get("result")
            .and_then(|inner| inner.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        match value {
            Value::String(text) => {
                serde_json::from_str(&text).context("page script did not return a JSON document")
            }
            other => Ok(other),
        }
    }

    async fn screenshot(&self, tab: &TabHandle) -> Result<Vec<u8>> {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as B64;
        let result = self
            .call(tab, "Page.captureScreenshot", json!({"format": "png"}))
            .await
            .context("CDP Page.captureScreenshot failed")?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("CDP screenshot carried no image data"))?;
        B64.decode(data).context("CDP screenshot was not base64")
    }

    async fn ax_nodes(&self, tab: &TabHandle) -> Result<Option<Vec<AxNode>>> {
        let session = self.page(tab).await?;
        let _ = self
            .ws
            .request("Accessibility.enable", json!({}), Some(&session))
            .await;
        let Ok(result) = self
            .ws
            .request("Accessibility.getFullAXTree", json!({}), Some(&session))
            .await
        else {
            return Ok(None);
        };
        let mut nodes = Vec::new();
        for node in result
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if nodes.len() >= super::protocol::MAX_AX_NODES {
                break;
            }
            let role = node
                .get("role")
                .and_then(|role| role.get("value"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if role.is_empty() {
                continue;
            }
            nodes.push(AxNode {
                role,
                name: node
                    .get("name")
                    .and_then(|name| name.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                value: node
                    .get("value")
                    .and_then(|value| value.get("value"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        Ok(Some(nodes))
    }

    async fn cookies(&self) -> Result<Vec<Cookie>> {
        let result = self.ws.request("Storage.getCookies", json!({}), None).await?;
        let mut cookies = Vec::new();
        for cookie in result
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            cookies.push(Cookie {
                name: cookie
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                value: cookie
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                domain: cookie
                    .get("domain")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                path: cookie
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("/")
                    .to_string(),
                expires: cookie.get("expires").and_then(Value::as_f64).unwrap_or(-1.0),
                http_only: cookie
                    .get("httpOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                secure: cookie
                    .get("secure")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                same_site: cookie
                    .get("sameSite")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        Ok(cookies)
    }

    async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()> {
        let payload: Vec<Value> = cookies
            .iter()
            .map(|cookie| {
                let mut value = json!({
                    "name": cookie.name,
                    "value": cookie.value,
                    "domain": cookie.domain,
                    "path": cookie.path,
                });
                if cookie.expires >= 0.0 {
                    value["expires"] = json!(cookie.expires);
                }
                if cookie.http_only {
                    value["httpOnly"] = json!(true);
                }
                if cookie.secure {
                    value["secure"] = json!(true);
                }
                if let Some(same_site) = &cookie.same_site {
                    value["sameSite"] = json!(same_site);
                }
                value
            })
            .collect();
        self.ws
            .request("Storage.setCookies", json!({"cookies": payload}), None)
            .await?;
        Ok(())
    }

    async fn insert_text(&self, tab: &TabHandle, text: &str) -> Result<()> {
        self.call(tab, "Input.insertText", json!({"text": text}))
            .await
            .context("CDP Input.insertText failed")?;
        Ok(())
    }

    async fn press_key(&self, tab: &TabHandle, key: &str) -> Result<()> {
        let (code, key_code, text) = key_descriptor(key);
        self.call(
            tab,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown",
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": key_code,
                "nativeVirtualKeyCode": key_code,
                "text": text,
            }),
        )
        .await
        .context("CDP keyDown failed")?;
        self.call(
            tab,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": key_code,
                "nativeVirtualKeyCode": key_code,
            }),
        )
        .await
        .context("CDP keyUp failed")?;
        Ok(())
    }

    async fn scroll(&self, tab: &TabHandle, delta_y: f64) -> Result<()> {
        self.call(
            tab,
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": 0,
                "y": 0,
                "deltaX": 0,
                "deltaY": delta_y,
            }),
        )
        .await
        .context("CDP mouseWheel failed")?;
        Ok(())
    }

    async fn shutdown(&self) {
        self.ws.close().await;
    }
}

/// Map a named key to the CDP key fields. Unknown names fall back to a plain
/// character key so a caller is never silently ignored.
fn key_descriptor(key: &str) -> (&'static str, u32, String) {
    match key {
        "Enter" => ("Enter", 13, "\r".into()),
        "Tab" => ("Tab", 9, String::new()),
        "Escape" => ("Escape", 27, String::new()),
        "Backspace" => ("Backspace", 8, String::new()),
        "Delete" => ("Delete", 46, String::new()),
        "ArrowUp" => ("ArrowUp", 38, String::new()),
        "ArrowDown" => ("ArrowDown", 40, String::new()),
        "ArrowLeft" => ("ArrowLeft", 37, String::new()),
        "ArrowRight" => ("ArrowRight", 39, String::new()),
        "PageUp" => ("PageUp", 33, String::new()),
        "PageDown" => ("PageDown", 34, String::new()),
        "Home" => ("Home", 36, String::new()),
        "End" => ("End", 35, String::new()),
        "Space" => ("Space", 32, " ".into()),
        other => {
            let mut chars = other.chars();
            let first = chars.next().unwrap_or(' ');
            ("KeyUnknown", first as u32, first.to_string())
        }
    }
}
