//! WebDriver BiDi client (Firefox family).
//!
//! BiDi is a different protocol from CDP: commands are `method` + `params`
//! and replies arrive as `success`/`error` envelopes. BiDi has no
//! accessibility domain in this adapter, so the AX path honestly reports
//! "unavailable" instead of inventing nodes.
use super::protocol::{AxNode, BrowserProtocol, Cookie, NavOutcome, TabHandle, TabInfo, WsClient};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

/// A real WebDriver BiDi client bound to one Firefox-family browser process.
pub struct BidiClient {
    ws: WsClient,
    endpoint: String,
    product: String,
    session_id: String,
}

impl BidiClient {
    pub async fn connect(endpoint: String, product: String) -> Result<Self> {
        let ws = WsClient::connect(&endpoint).await?;
        let result = ws
            .request("session.new", json!({"capabilities": {}}), None)
            .await
            .context("BiDi session.new failed")?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("BiDi session.new returned no sessionId"))?
            .to_string();
        Ok(Self {
            ws,
            endpoint,
            product,
            session_id,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Every BiDi command after session.new carries the session id in the
    /// message root, as the protocol requires.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.ws.request(method, params, Some(&self.session_id)).await
    }
}

#[async_trait::async_trait]
impl BrowserProtocol for BidiClient {
    fn kind(&self) -> &'static str {
        "bidi"
    }

    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    fn product(&self) -> String {
        self.product.clone()
    }

    async fn create_tab(&self, url: &str) -> Result<TabHandle> {
        let result = self
            .call("browsingContext.create", json!({"type": "tab"}))
            .await
            .context("BiDi browsingContext.create failed")?;
        let context = result
            .get("context")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("BiDi create returned no context id"))?
            .to_string();
        if !url.is_empty() && url != "about:blank" {
            let _ = self
                .call(
                    "browsingContext.navigate",
                    json!({"context": context, "url": url, "wait": "complete"}),
                )
                .await;
        }
        Ok(TabHandle {
            id: context,
            kind: "bidi-context".into(),
        })
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        let result = self
            .call("browsingContext.getTree", json!({}))
            .await
            .context("BiDi browsingContext.getTree failed")?;
        let mut tabs = Vec::new();
        for context in result
            .get("contexts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let id = context
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                continue;
            }
            tabs.push(TabInfo {
                tab_id: id,
                url: context
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title: context
                    .get("originalOpener")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                group: String::new(),
            });
        }
        Ok(tabs)
    }

    async fn close_tab(&self, tab: &TabHandle) -> Result<()> {
        self.call("browsingContext.close", json!({"context": tab.id}))
            .await?;
        Ok(())
    }

    async fn navigate(&self, tab: &TabHandle, url: &str) -> Result<NavOutcome> {
        let result = self
            .call(
                "browsingContext.navigate",
                json!({"context": tab.id, "url": url, "wait": "complete"}),
            )
            .await
            .context("BiDi browsingContext.navigate failed")?;
        let error_text = result
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string);
        // The navigate result names the document url at the top level; the
        // requested url is only a fallback.
        let navigated = result
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(url)
            .to_string();
        let ready_state = self
            .evaluate_json(tab, "JSON.stringify({readyState: document.readyState})")
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("readyState")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        Ok(NavOutcome {
            url: navigated,
            loader: None,
            error_text,
            ready_state,
        })
    }

    async fn evaluate_json(&self, tab: &TabHandle, script: &str) -> Result<Value> {
        let result = self
            .call(
                "script.evaluate",
                json!({
                    "expression": script,
                    "target": {"context": tab.id},
                    "awaitPromise": true,
                    "resultOwnership": "none",
                    "userActivation": true,
                }),
            )
            .await
            .context("BiDi script.evaluate failed")?;
        if let Some(details) = result.get("exceptionDetails") {
            let text = details
                .get("text")
                .and_then(Value::as_str)
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
            .call("browsingContext.captureScreenshot", json!({"context": tab.id}))
            .await
            .context("BiDi browsingContext.captureScreenshot failed")?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("BiDi screenshot carried no image data"))?;
        B64.decode(data).context("BiDi screenshot was not base64")
    }

    async fn ax_nodes(&self, tab: &TabHandle) -> Result<Option<Vec<AxNode>>> {
        let _ = tab;
        Ok(None)
    }

    async fn cookies(&self) -> Result<Vec<Cookie>> {
        let result = self
            .call("storage.getCookies", json!({}))
            .await
            .context("BiDi storage.getCookies failed")?;
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
                expires: cookie.get("expiry").and_then(Value::as_f64).unwrap_or(-1.0),
                http_only: cookie
                    .get("http")
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
                    "http": cookie.http_only,
                    "secure": cookie.secure,
                });
                if cookie.expires >= 0.0 {
                    value["expiry"] = json!(cookie.expires);
                }
                if let Some(same_site) = &cookie.same_site {
                    value["sameSite"] = json!(same_site);
                }
                value
            })
            .collect();
        self.call("storage.setCookies", json!({"cookies": payload}))
            .await?;
        Ok(())
    }

    async fn insert_text(&self, tab: &TabHandle, text: &str) -> Result<()> {
        // BiDi has no "insert text" primitive: the real input channel is a
        // key sequence, which is what a human produces.
        let actions: Vec<Value> = text
            .chars()
            .map(|character| {
                json!({
                    "type": "key",
                    "id": "shenko-text",
                    "actions": [
                        {"type": "keyDown", "value": character.to_string()},
                        {"type": "keyUp", "value": character.to_string()},
                    ],
                })
            })
            .collect();
        self.call("input.performActions", json!({"context": tab.id, "actions": actions}))
            .await
            .context("BiDi input.performActions failed")?;
        Ok(())
    }

    async fn press_key(&self, tab: &TabHandle, key: &str) -> Result<()> {
        let value = bidi_key_value(key);
        self.call(
            "input.performActions",
            json!({
                "context": tab.id,
                "actions": [{
                    "type": "key",
                    "id": "shenko-key",
                    "actions": [
                        {"type": "keyDown", "value": value},
                        {"type": "keyUp", "value": value},
                    ],
                }],
            }),
        )
        .await
        .context("BiDi input.performActions failed")?;
        Ok(())
    }

    async fn scroll(&self, tab: &TabHandle, delta_y: f64) -> Result<()> {
        self.call(
            "input.performActions",
            json!({
                "context": tab.id,
                "actions": [{
                    "type": "wheel",
                    "id": "shenko-wheel",
                    "actions": [{"type": "scroll", "x": 0, "y": 0, "deltaX": 0, "deltaY": delta_y}],
                }],
            }),
        )
        .await
        .context("BiDi wheel action failed")?;
        Ok(())
    }

    async fn shutdown(&self) {
        let _ = self.call("session.end", json!({})).await;
        self.ws.close().await;
    }
}

/// BiDi key values are WebDriver key names or single characters.
fn bidi_key_value(key: &str) -> String {
    match key {
        "Enter" => "\u{e007}".into(),
        "Tab" => "\u{e004}".into(),
        "Escape" => "\u{e00c}".into(),
        "Backspace" => "\u{e003}".into(),
        "Delete" => "\u{e006}".into(),
        "ArrowUp" => "\u{e013}".into(),
        "ArrowDown" => "\u{e015}".into(),
        "ArrowLeft" => "\u{e012}".into(),
        "ArrowRight" => "\u{e014}".into(),
        "PageUp" => "\u{e011}".into(),
        "PageDown" => "\u{e017}".into(),
        "Home" => "\u{e010}".into(),
        "End" => "\u{e018}".into(),
        "Space" => " ".into(),
        other => other.to_string(),
    }
}
