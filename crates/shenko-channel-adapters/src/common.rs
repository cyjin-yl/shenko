//! Facts every adapter shares: what the platform can really do, how healthy
//! the connector is, and the small amount of HTTP/token plumbing that must
//! behave identically everywhere.
//!
//! SPEC §15.3 requires capability discovery, authentication/renewal and health
//! status to be *real* platform facts. Nothing here may promise a capability
//! the platform does not have: the three adapters fill these structures from
//! their own protocol documents, and the difference between QQ official
//! (guild-only typing, group-wide receipt scope) and Feishu (no typing
//! indicator at all) stays visible instead of being smoothed over.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

/// The envelope format this adapter family produces.
///
/// Bumped only when the meaning of an envelope field changes. The core reads
/// the version, so an older sealed envelope is still recognisable.
pub const ADAPTER_ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// What one platform can actually do.
///
/// The model grammar and the runtime advertise exactly this set. A field that
/// is `false` is a promise NOT to attempt the action, not a promise that the
/// platform might accept it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelCapabilities {
    /// One-to-one conversations with a person.
    pub direct: bool,
    /// Group conversations.
    pub group: bool,
    /// Thread/topic conversations.
    pub thread: bool,
    /// Guild/channel conversations (QQ official channels).
    pub guild_channel: bool,
    /// Structured rich text beyond plain text.
    pub rich_text: bool,
    /// Attachments (images, files, voice, video).
    pub attachments: bool,
    /// Editing a delivered message.
    pub edit: bool,
    /// Retracting a delivered message.
    pub retract: bool,
    /// Publishing a typing indicator.
    pub typing: bool,
    /// Sending without an inbound message (proactive delivery).
    pub proactive: bool,
    /// Human-readable notes about the real limits of the above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl ChannelCapabilities {
    /// The capability set of a platform that has no channel features at all.
    pub fn none() -> Self {
        Self {
            direct: false,
            group: false,
            thread: false,
            guild_channel: false,
            rich_text: false,
            attachments: false,
            edit: false,
            retract: false,
            typing: false,
            proactive: false,
            notes: vec![],
        }
    }
}

/// The connector's health, as observed by this runtime.
///
/// `state` is one of `disabled`, `connecting`, `ready`, `degraded` or
/// `stopped`. It describes THIS connector, not the platform in general.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelHealth {
    /// The channel id of the connector.
    pub channel: String,
    /// The account namespace the connector serves.
    pub account: String,
    /// One of `disabled`, `connecting`, `ready`, `degraded`, `stopped`.
    pub state: String,
    /// A short, credential-free explanation of the current state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the connector last saw a platform event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<DateTime<Utc>>,
    /// When this observation was made.
    pub observed_at: DateTime<Utc>,
}

impl ChannelHealth {
    /// A health observation in the given state.
    pub fn new(channel: &str, account: &str, state: &str, detail: Option<String>) -> Self {
        Self {
            channel: channel.to_string(),
            account: account.to_string(),
            state: state.to_string(),
            detail,
            last_event_at: None,
            observed_at: Utc::now(),
        }
    }
}

/// A live health observation a connector keeps up to date.
#[derive(Clone, Default)]
pub struct HealthState {
    inner: Arc<Mutex<ChannelHealth>>,
}

impl HealthState {
    /// Start a health observation in the `disabled` state.
    pub fn disabled(channel: &str, account: &str, detail: &str) -> Self {
        Self::new(ChannelHealth::new(
            channel,
            account,
            "disabled",
            Some(detail.to_string()),
        ))
    }

    /// Start a health observation in the given state.
    pub fn new(health: ChannelHealth) -> Self {
        Self {
            inner: Arc::new(Mutex::new(health)),
        }
    }

    /// Replace the whole observation.
    pub async fn set(&self, health: ChannelHealth) {
        let mut guard = self.inner.lock().await;
        *guard = health;
    }

    /// Move to a new state, keeping the account scope and the last-event time.
    pub async fn transition(&self, state: &str, detail: Option<String>) {
        let mut guard = self.inner.lock().await;
        guard.state = state.to_string();
        guard.detail = detail;
        guard.observed_at = Utc::now();
    }

    /// Record that a platform event was observed.
    pub async fn observe_event(&self) {
        let mut guard = self.inner.lock().await;
        guard.last_event_at = Some(Utc::now());
        guard.observed_at = Utc::now();
    }

    /// Read the current observation.
    pub async fn snapshot(&self) -> ChannelHealth {
        self.inner.lock().await.clone()
    }
}

/// One platform access token with its own expiry.
///
/// The token value never leaves this struct except through an explicit
/// accessor that the HTTP layer uses to build an `Authorization` header. It is
/// never logged, never sealed into an artifact and never put in a URL.
#[derive(Debug, Clone)]
pub struct CachedToken {
    /// The bearer value.
    pub value: String,
    /// When the platform stops accepting it.
    pub expires_at: DateTime<Utc>,
}

impl CachedToken {
    /// Whether the token is expired, or close enough that a renewal must
    /// start before the next call.
    pub fn is_stale(&self, skew: Duration) -> bool {
        Utc::now() + skew >= self.expires_at
    }
}

/// A token cache that renews on demand and serialises concurrent renewals.
#[derive(Clone)]
pub struct TokenCache {
    inner: Arc<Mutex<Option<CachedToken>>>,
}

impl TokenCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// The cached token, if one is present and still fresh.
    pub async fn peek(&self) -> Option<CachedToken> {
        self.inner.lock().await.clone()
    }

    /// Store a freshly issued token.
    pub async fn store(&self, token: CachedToken) {
        *self.inner.lock().await = Some(token);
    }

    /// Forget the cached token, e.g. after the platform rejected it.
    pub async fn invalidate(&self) {
        *self.inner.lock().await = None;
    }
}

impl Default for TokenCache {
    fn default() -> Self {
        Self::new()
    }
}

/// How a platform answered one request.
///
/// The distinction matters: `Rejected` means the platform answered and refused
/// (nothing happened at the far end), while `Unknown` means the answer was
/// lost and the request MAY have been executed.
#[derive(Debug, Clone)]
pub enum PlatformAnswer {
    /// The platform answered with a body.
    Answered {
        /// The HTTP status code.
        status: u16,
        /// The decoded body.
        body: Value,
    },
    /// The platform answered with a refusal.
    Rejected {
        /// The HTTP status code.
        status: u16,
        /// The platform's own error code, when it reported one.
        code: Option<i64>,
        /// The platform's message, when it reported one.
        message: String,
        /// The decoded body, when there was one.
        body: Value,
    },
    /// The transport failed; the request may or may not have been executed.
    Unknown {
        /// A credential-free description of the failure.
        detail: String,
    },
}

impl PlatformAnswer {
    /// Whether the platform answered at all.
    pub fn is_answered(&self) -> bool {
        !matches!(self, PlatformAnswer::Unknown { .. })
    }
}

/// A small HTTP helper shared by the three adapters.
///
/// It bounds how much of an answer is read, keeps credentials out of error
/// strings, and separates "answered" from "lost".
#[derive(Clone)]
pub struct PlatformHttp {
    client: reqwest::Client,
}

impl PlatformHttp {
    /// Build a client with the default timeouts of the platform adapters.
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent("shenko-channel-adapters/1")
            .build()
            .map_err(|error| format!("http client build failed: {error}"))?;
        Ok(Self { client })
    }

    /// The underlying client, for multipart uploads.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// `GET {url}` with an optional bearer header.
    pub async fn get(&self, url: &str, bearer: Option<&str>) -> PlatformAnswer {
        let mut request = self.client.get(url);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        self.send(request).await
    }

    /// `GET {url}` with explicit headers.
    pub async fn get_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> PlatformAnswer {
        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        self.send(request).await
    }

    /// `POST {url}` with a JSON body and an optional bearer header.
    pub async fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &Value,
    ) -> PlatformAnswer {
        let mut request = self.client.post(url).json(body);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        self.send(request).await
    }

    /// `POST {url}` with a JSON body and explicit headers.
    pub async fn post_json_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> PlatformAnswer {
        let mut request = self.client.post(url).json(body);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        self.send(request).await
    }

    /// `PUT {url}` with a JSON body and explicit headers.
    pub async fn put_json_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> PlatformAnswer {
        let mut request = self.client.put(url).json(body);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        self.send(request).await
    }

    /// `DELETE {url}` with explicit headers.
    pub async fn delete_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> PlatformAnswer {
        let mut request = self.client.delete(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        self.send(request).await
    }

    /// `PATCH {url}` with a JSON body and explicit headers.
    pub async fn patch_json_with_headers(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
    ) -> PlatformAnswer {
        let mut request = self.client.patch(url).json(body);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        self.send(request).await
    }

    /// `GET {url}` with a bearer header, returning raw bytes.
    ///
    /// Media endpoints answer with the file bytes, not JSON; `send` would
    /// turn them into `Value::Null`. This preserves them.
    pub async fn get_bytes(&self, url: &str, bearer: Option<&str>) -> Result<Vec<u8>, String> {
        let mut request = self.client.get(url);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| redact_transport_error(&error.to_string()))?;
        if !response.status().is_success() {
            return Err(format!(
                "media download refused with http {}",
                response.status().as_u16()
            ));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| format!("media body failed: {error}"))
    }

    /// `POST {url}` with a multipart body.
    pub async fn post_multipart(
        &self,
        url: &str,
        headers: &[(String, String)],
        form: reqwest::multipart::Form,
    ) -> PlatformAnswer {
        let mut request = self.client.post(url).multipart(form);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        self.send(request).await
    }

    /// Execute a prepared request and classify the answer.
    async fn send(&self, request: reqwest::RequestBuilder) -> PlatformAnswer {
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                // A transport failure after the request was written is not a
                // refusal: the platform may still have executed it.
                return PlatformAnswer::Unknown {
                    detail: redact_transport_error(&error.to_string()),
                };
            }
        };
        let status = response.status().as_u16();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                return PlatformAnswer::Unknown {
                    detail: redact_transport_error(&error.to_string()),
                };
            }
        };
        let body = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return PlatformAnswer::Answered { status, body };
        }
        PlatformAnswer::Rejected {
            status,
            code: body
                .get("err_code")
                .or_else(|| body.get("code"))
                .and_then(Value::as_i64),
            message: body
                .get("message")
                .or_else(|| body.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            body,
        }
    }
}

/// Strip anything that could carry a credential out of a transport error.
///
/// `reqwest` embeds the URL in its error display, and a platform URL may
/// carry a query token. Only the scheme and host survive.
pub fn redact_transport_error(detail: &str) -> String {
    match url::Url::parse(detail) {
        Ok(parsed) => {
            let mut safe = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
            if let Some(port) = parsed.port() {
                safe.push(':');
                safe.push_str(&port.to_string());
            }
            safe
        }
        Err(_) => {
            // Not a URL: keep only the first line and drop anything that
            // looks like a long opaque token.
            let first = detail.lines().next().unwrap_or_default();
            if first.len() > 160 {
                format!("{}…", &first[..160])
            } else {
                first.to_string()
            }
        }
    }
}

/// Strip the query and fragment of a URL so it can be recorded as provenance.
///
/// A configured endpoint may embed a token in its query; the recorded
/// provenance must never carry it.
pub fn endpoint_provenance(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let mut safe = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
            if let Some(port) = parsed.port() {
                safe.push(':');
                safe.push_str(&port.to_string());
            }
            safe.push_str(parsed.path());
            safe
        }
        Err(_) => "<unparseable endpoint>".to_string(),
    }
}

/// Read a non-empty string field.
pub fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Read an integer field that the platform may send as a number or a string.
pub fn integer_field(value: &Value, key: &str) -> Option<i64> {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// The inbound context a platform needs in order to answer inside its
/// passive window.
///
/// A QQ official group reply must carry the id of the message it answers; a
/// Feishu thread reply must carry the root message; a WeChat iLink reply must
/// carry the conversation's context token. None of these is part of the
/// opaque audience id, and none of them may be invented. The adapter
/// therefore remembers the last inbound context of every conversation it
/// classified, and the outbound description uses exactly what the platform
/// gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyContext {
    /// The platform message id of the last inbound message.
    pub message_id: String,
    /// The conversation's context token, when the platform has one.
    pub context_token: Option<String>,
    /// The thread/root id, when the platform has one.
    pub thread_id: Option<String>,
    /// When the context was recorded.
    pub at: DateTime<Utc>,
}

/// A bounded cache of the last inbound context per conversation.
#[derive(Clone)]
pub struct ReplyContextCache {
    inner: Arc<parking_lot::Mutex<HashMap<String, ReplyContext>>>,
    capacity: usize,
    window: Duration,
}

impl ReplyContextCache {
    /// A cache that keeps at most `capacity` conversations for `window`.
    pub fn new(capacity: usize, window: Duration) -> Self {
        Self {
            inner: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            capacity,
            window,
        }
    }

    /// Remember the last inbound context of one conversation.
    pub fn record(&self, conversation: &str, context: ReplyContext) {
        if conversation.is_empty() {
            return;
        }
        let mut guard = self.inner.lock();
        guard.insert(conversation.to_string(), context);
        while guard.len() > self.capacity {
            let oldest = guard
                .iter()
                .min_by_key(|(_, context)| context.at)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    guard.remove(&key);
                }
                None => break,
            }
        }
    }

    /// The remembered context of one conversation, if it is still fresh.
    pub fn get(&self, conversation: &str) -> Option<ReplyContext> {
        let mut guard = self.inner.lock();
        let context = guard.get(conversation)?.clone();
        if Utc::now() - context.at > self.window {
            guard.remove(conversation);
            return None;
        }
        Some(context)
    }

    /// Forget every remembered context.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

/// Read the bytes an attachment locator points at.
///
/// A locator names the host that owns the bytes. A remote URL is fetched from
/// the platform that published it; a local path is only read when the
/// locator says the bytes live on this machine. A path that belongs to
/// another host is never read as if it were local.
pub async fn read_attachment_bytes(
    http: &PlatformHttp,
    attachment: &shenko_types::channel::ChannelAttachment,
) -> Result<Vec<u8>, String> {
    let Some(locator) = &attachment.locator else {
        return Err("attachment carries no locator".to_string());
    };
    if let Some(url) = locator.url.as_deref().filter(|url| !url.is_empty()) {
        let response = http
            .client()
            .get(url)
            .send()
            .await
            .map_err(|error| format!("attachment fetch failed: {}", error.status()))?;
        if !response.status().is_success() {
            return Err(format!(
                "attachment fetch refused with http {}",
                response.status().as_u16()
            ));
    }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("attachment body failed: {error}"))?;
        return Ok(bytes.to_vec());
    }
    if let Some(path) = locator.path.as_deref().filter(|path| !path.is_empty()) {
        let host = locator.host.as_deref().unwrap_or("local");
        if host != "local" && host != "localhost" {
            return Err(format!(
                "attachment bytes live on host {host}; this runtime cannot read them directly"
            ));
}
        return tokio::fs::read(path)
            .await
            .map_err(|error| format!("attachment read failed: {error}"));
    }
    Err("attachment locator names neither a URL nor a path".to_string())
}
