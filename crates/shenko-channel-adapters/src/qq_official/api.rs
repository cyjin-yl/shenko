//! The official QQ bot OpenAPI client — SPEC §16, §15.3.
//!
//! Every call in this module is a real, named endpoint of the official
//! platform:
//!
//! | purpose | call |
//! | --- | --- |
//! | access token | `POST {token_base}/app/getAppAccessToken` |
//! | gateway | `GET {api_base}/gateway` |
//! | single chat send | `POST /v2/users/{user_openid}/messages` |
//! | group send | `POST /v2/groups/{group_openid}/messages` |
//! | channel send | `POST /v2/channels/{channel_id}/messages` |
//! | edit | `PUT /v2/messages/{message_id}` |
//! | retract | `DELETE /v2/{users\|groups}/{openid}/messages/{message_id}` |
//! | typing | `POST /v2/channels/{channel_id}/typing` |
//! | media upload | `POST /v2/{users\|groups}/{openid}/files` |
//! | media pre-upload | `POST /v2/{users\|groups}/{openid}/upload_prepare` |
//! | media part finish | `POST /v2/{users\|groups}/{openid}/upload_part_finish` |
//!
//! The access token is requested with the app ticket and renewed before it
//! expires. It is sent as `Authorization: QQBot {token}` and never appears in
//! a URL, a log line or a receipt.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::common::{
    CachedToken, PlatformAnswer, PlatformHttp, TokenCache, endpoint_provenance, integer_field,
    text_field,
};

use super::config::QqOfficialConfig;

/// Renew the access token this long before it expires.
pub const TOKEN_RENEW_SKEW: Duration = Duration::from_secs(300);

/// Why one official QQ call did not produce an accepted result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QqApiError {
    /// The platform answered and refused.
    #[error("qq official refused the request: {message} (err_code {code:?}, http {status})")]
    Rejected {
        /// The HTTP status.
        status: u16,
        /// The platform's own error code.
        code: Option<i64>,
        /// The platform's message.
        message: String,
    },
    /// The answer was lost; the request may have been executed.
    #[error("qq official outcome unknown: {0}")]
    Unknown(String),
    /// The platform answered, but the answer did not carry what was promised.
    #[error("qq official answer was incomplete: {0}")]
    Incomplete(String),
}

/// The official QQ bot API client.
#[derive(Clone)]
pub struct QqOfficialApi {
    config: Arc<QqOfficialConfig>,
    http: PlatformHttp,
    tokens: TokenCache,
}

impl QqOfficialApi {
    /// Build a client for one bot configuration.
    pub fn new(config: Arc<QqOfficialConfig>, http: PlatformHttp) -> Self {
        Self {
            config,
            http,
            tokens: TokenCache::new(),
        }
    }

    /// The configuration this client serves.
    pub fn config(&self) -> &QqOfficialConfig {
        &self.config
    }

    /// The HTTP client this API uses, for attachment fetches.
    pub fn http(&self) -> &PlatformHttp {
        &self.http
    }

    /// The cached access token, if one is present.
    pub async fn cached_token(&self) -> Option<CachedToken> {
        self.tokens.peek().await
    }

    /// A valid access token, renewed when it is missing or close to expiry.
    pub async fn access_token(&self) -> Result<String, QqApiError> {
        if let Some(token) = self.tokens.peek().await {
            if !token.is_stale(TOKEN_RENEW_SKEW) {
                return Ok(token.value);
            }
        }
        let token = self.request_token().await?;
        self.tokens.store(token.clone()).await;
        Ok(token.value)
    }

    /// Ask the platform for a fresh access token.
    async fn request_token(&self) -> Result<CachedToken, QqApiError> {
        let secret = self.config.app_secret.as_deref().ok_or_else(|| {
            QqApiError::Incomplete("no app secret configured for this bot".to_string())
        })?;
        let url = format!("{}/app/getAppAccessToken", self.config.token_base());
        let body = json!({
            "appId": self.config.app_id,
            "clientSecret": secret,
        });
        let answer = self.http.post_json(&url, None, &body).await;
        let body = match answer {
            PlatformAnswer::Answered { body, .. } => body,
            PlatformAnswer::Rejected { status, code, message, .. } => {
                return Err(QqApiError::Rejected {
                    status,
                    code,
                    message,
                });
            }
            PlatformAnswer::Unknown { detail } => return Err(QqApiError::Unknown(detail)),
        };
        // The token endpoint answers with `code`; the message endpoints answer
        // with `err_code`. Both must be zero.
        let code = integer_field(&body, "code").unwrap_or(0);
        if code != 0 {
            return Err(QqApiError::Rejected {
                status: 200,
                code: Some(code),
                message: text_field(&body, "message").unwrap_or_default(),
            });
        }
        let value = text_field(&body, "access_token").ok_or_else(|| {
            QqApiError::Incomplete("access token response carried no access_token".to_string())
        })?;
        let expires_in = integer_field(&body, "expires_in").unwrap_or(7200);
        Ok(CachedToken {
            value,
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(expires_in),
        })
    }

    /// The `Authorization` header value of this bot.
    async fn authorization(&self) -> Result<String, QqApiError> {
        Ok(format!("QQBot {}", self.access_token().await?))
    }

    /// The WebSocket gateway address of this bot.
    ///
    /// The platform publishes a generic entry point (`GET /gateway`) and a
    /// sharded one (`GET /gateway/bot`). The generic one is tried first; a
    /// deployment that must shard configures the URL directly.
    pub async fn gateway_url(&self) -> Result<String, QqApiError> {
        if let Some(url) = &self.config.gateway_url {
            return Ok(url.clone());
        }
        let token = self.access_token().await?;
        for path in ["/gateway", "/gateway/bot"] {
            let url = format!("{}{path}", self.config.api_base());
            let answer = self.http.get(&url, Some(&token)).await;
            if let PlatformAnswer::Answered { body, .. } = answer {
                if let Some(gateway) = text_field(&body, "url") {
                    return Ok(gateway);
                }
            }
        }
        Err(QqApiError::Incomplete(
            "gateway discovery returned no url".to_string(),
        ))
    }

    /// The credential-free provenance of the gateway endpoint.
    pub async fn gateway_provenance(&self) -> String {
        match self.gateway_url().await {
            Ok(url) => endpoint_provenance(&url),
            Err(_) => "<unavailable>".to_string(),
        }
    }

    /// Send one message to a single chat.
    pub async fn send_private(
        &self,
        openid: &str,
        outbound: &crate::qq_official::outbound::OfficialOutbound,
        reply_to: Option<&str>,
        msg_seq: u32,
    ) -> Result<Value, QqApiError> {
        let url = format!(
            "{}/v2/users/{openid}/messages",
            self.config.api_base()
        );
        self.send_message(&url, outbound, reply_to, msg_seq, None).await
    }

    /// Send one message to a group.
    pub async fn send_group(
        &self,
        group_openid: &str,
        outbound: &crate::qq_official::outbound::OfficialOutbound,
        reply_to: Option<&str>,
        msg_seq: u32,
    ) -> Result<Value, QqApiError> {
        let url = format!(
            "{}/v2/groups/{group_openid}/messages",
            self.config.api_base()
        );
        self.send_message(&url, outbound, reply_to, msg_seq, None).await
    }

    /// Send one message to a guild channel.
    pub async fn send_channel(
        &self,
        channel_id: &str,
        outbound: &crate::qq_official::outbound::OfficialOutbound,
        reply_to: Option<&str>,
    ) -> Result<Value, QqApiError> {
        let url = format!(
            "{}/v2/channels/{channel_id}/messages",
            self.config.api_base()
        );
        self.send_message(&url, outbound, reply_to, 1, None).await
    }

    /// Send one media message; the platform needs the `file_info` of an
    /// uploaded file.
    pub async fn send_media(
        &self,
        scope: &MediaScope,
        file_info: &str,
        reply_to: Option<&str>,
        msg_seq: u32,
    ) -> Result<Value, QqApiError> {
        let url = scope.files_url(self.config.api_base());
        let mut body = json!({
            "msg_type": 7,
            "media": { "file_info": file_info },
        });
        if let Some(reply) = reply_to {
            body["msg_id"] = Value::String(reply.to_string());
            body["msg_seq"] = json!(msg_seq);
        }
        self.post_authorized(&url, &body).await
    }

    /// The shared body of the three message endpoints.
    async fn send_message(
        &self,
        url: &str,
        outbound: &crate::qq_official::outbound::OfficialOutbound,
        reply_to: Option<&str>,
        msg_seq: u32,
        extra: Option<Value>,
    ) -> Result<Value, QqApiError> {
        // The plugin's own surface syntax already decided the message type,
        // the content field and the quoted message; the platform refuses a
        // body that carries both `content` and `markdown`.
        let mut body = outbound.wire_body();
        if let Some(extra) = extra {
            for (key, value) in extra.as_object().cloned().unwrap_or_default() {
                body[key] = value;
            }
        }
        if let Some(reply) = reply_to {
            body["msg_id"] = Value::String(reply.to_string());
            body["msg_seq"] = json!(msg_seq);
        }
        self.post_authorized(url, &body).await
    }


    /// Retract one message of a single chat.
    pub async fn retract_private(
        &self,
        openid: &str,
        message_id: &str,
    ) -> Result<Value, QqApiError> {
        let url = format!(
            "{}/v2/users/{openid}/messages/{message_id}",
            self.config.api_base()
        );
        self.delete_authorized(&url).await
    }

    /// Retract one message of a group.
    pub async fn retract_group(
        &self,
        group_openid: &str,
        message_id: &str,
    ) -> Result<Value, QqApiError> {
        let url = format!(
            "{}/v2/groups/{group_openid}/messages/{message_id}",
            self.config.api_base()
        );
        self.delete_authorized(&url).await
    }

    /// Publish a typing indicator in a guild channel.
    ///
    /// The platform offers this only for channels. A single chat or a group
    /// has no typing endpoint, and this adapter does not pretend otherwise.
    pub async fn typing(&self, channel_id: &str) -> Result<Value, QqApiError> {
        let url = format!(
            "{}/v2/channels/{channel_id}/typing",
            self.config.api_base()
        );
        self.post_authorized(&url, &json!({})).await
    }

    /// Upload media and return the platform's `file_info`.
    ///
    /// Small payloads are uploaded directly as base64 (`file_data`); larger
    /// ones use the platform's pre-upload / part-finish / merge flow. Both
    /// flows end in the same `POST /v2/.../files` call and the same
    /// `file_info`, which the message endpoint consumes as `media.file_info`.
    pub async fn upload_media(
        &self,
        scope: &MediaScope,
        bytes: &[u8],
        file_name: &str,
        file_type: u8,
    ) -> Result<UploadedMedia, QqApiError> {
        let upload_id = if bytes.len() > DIRECT_UPLOAD_LIMIT {
            Some(self.prepare_upload(scope, bytes, file_name, file_type).await?)
        } else {
            None
        };
        let mut body = json!({
            "file_type": file_type,
            "file_name": file_name,
            "srv_send_msg": false,
        });
        match &upload_id {
            Some(upload_id) => {
                body["upload_id"] = Value::String(upload_id.clone());
            }
            None => {
                body["file_data"] = Value::String(base64_encode(bytes));
            }
        }
        let url = scope.files_url(self.config.api_base());
        let answer = self.post_authorized(&url, &body).await?;
        let file_info = text_field(&answer, "file_info").ok_or_else(|| {
            QqApiError::Incomplete("media upload answer carried no file_info".to_string())
        })?;
        Ok(UploadedMedia {
            file_info,
            ttl: integer_field(&answer, "ttl"),
            upload_id,
        })
    }

    /// Start the chunked upload and push every part to its pre-signed URL.
    async fn prepare_upload(
        &self,
        scope: &MediaScope,
        bytes: &[u8],
        file_name: &str,
        file_type: u8,
    ) -> Result<String, QqApiError> {
        let url = scope.prepare_url(self.config.api_base());
        let body = json!({
            "file_type": file_type,
            "file_size": bytes.len().to_string(),
            "file_name": file_name,
            "md5": md5_hex(bytes),
            "sha1": sha1_hex(bytes),
            "md5_10m": md5_hex(&bytes[..bytes.len().min(10_002_432)]),
        });
        let answer = self.post_authorized(&url, &body).await?;
        let upload_id = text_field(&answer, "upload_id").ok_or_else(|| {
            QqApiError::Incomplete("upload_prepare answer carried no upload_id".to_string())
        })?;
        let block_size = integer_field(&answer, "block_size").unwrap_or(5 * 1024 * 1024) as usize;
        let parts = answer
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for part in parts {
            let index = integer_field(&part, "index").unwrap_or(0) as usize;
            let Some(presigned) = text_field(&part, "presigned_url") else {
                continue;
            };
            let start = index * block_size;
            let end = (start + block_size).min(bytes.len());
            let chunk = &bytes[start.min(bytes.len())..end];
            let part_url = presigned;
            let response = self
                .http
                .client()
                .put(&part_url)
                .header("Content-Type", "application/octet-stream")
                .body(chunk.to_vec())
                .send()
                .await;
            let status = match response {
                Ok(response) => response.status().as_u16(),
                Err(error) => {
                    return Err(QqApiError::Unknown(crate::common::redact_transport_error(
                        &error.to_string(),
                    )));
                }
            };
            if !(200..300).contains(&status) {
                return Err(QqApiError::Rejected {
                    status,
                    code: None,
                    message: "media part upload refused".to_string(),
                });
            }
            let finish_url = scope.part_finish_url(self.config.api_base());
            let finish_body = json!({
                "upload_id": upload_id,
                "part_index": index,
                "block_size": block_size.to_string(),
                "md5": md5_hex(chunk),
            });
            self.post_authorized(&finish_url, &finish_body).await?;
        }
        Ok(upload_id)
    }

    /// `POST` with the bot's authorization header.
    async fn post_authorized(&self, url: &str, body: &Value) -> Result<Value, QqApiError> {
        let authorization = self.authorization().await?;
        let headers = vec![("Authorization".to_string(), authorization)];
        match self.http.post_json_with_headers(url, &headers, body).await {
            PlatformAnswer::Answered { body, .. } => {
                if let Some(code) = integer_field(&body, "err_code") {
                    if code != 0 {
                        return Err(QqApiError::Rejected {
                            status: 200,
                            code: Some(code),
                            message: text_field(&body, "message").unwrap_or_default(),
                        });
                    }
                }
                Ok(body)
            }
            PlatformAnswer::Rejected { status, code, message, .. } => {
                Err(QqApiError::Rejected {
                    status,
                    code,
                    message,
                })
            }
            PlatformAnswer::Unknown { detail } => Err(QqApiError::Unknown(detail)),
        }
    }

    /// `DELETE` with the bot's authorization header.
    async fn delete_authorized(&self, url: &str) -> Result<Value, QqApiError> {
        let authorization = self.authorization().await?;
        let headers = vec![("Authorization".to_string(), authorization)];
        match self.http.delete_with_headers(url, &headers).await {
            PlatformAnswer::Answered { body, .. } => Ok(body),
            PlatformAnswer::Rejected { status, code, message, .. } => {
                Err(QqApiError::Rejected {
                    status,
                    code,
                    message,
                })
            }
            PlatformAnswer::Unknown { detail } => Err(QqApiError::Unknown(detail)),
        }
    }
}

/// The largest payload the platform accepts in the direct `file_data` upload.
pub const DIRECT_UPLOAD_LIMIT: usize = 5 * 1024 * 1024;

/// Which conversation scope an upload belongs to.
///
/// The official platform keeps the single-chat and the group upload
/// namespaces apart: a file uploaded for a single chat can never be sent to
/// a group. The scope therefore travels with the upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaScope {
    /// A single chat of one user OpenID.
    User(String),
    /// A group of one group OpenID.
    Group(String),
}

impl MediaScope {
    fn files_url(&self, base: &str) -> String {
        match self {
            MediaScope::User(openid) => format!("{base}/v2/users/{openid}/files"),
            MediaScope::Group(openid) => format!("{base}/v2/groups/{openid}/files"),
        }
    }

    fn prepare_url(&self, base: &str) -> String {
        match self {
            MediaScope::User(openid) => format!("{base}/v2/users/{openid}/upload_prepare"),
            MediaScope::Group(openid) => format!("{base}/v2/groups/{openid}/upload_prepare"),
        }
    }

    fn part_finish_url(&self, base: &str) -> String {
        match self {
            MediaScope::User(openid) => format!("{base}/v2/users/{openid}/upload_part_finish"),
            MediaScope::Group(openid) => format!("{base}/v2/groups/{openid}/upload_part_finish"),
        }
    }
}

/// What the platform answered after an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedMedia {
    /// The opaque `file_info` the message endpoint consumes.
    pub file_info: String,
    /// The lifetime of `file_info` in seconds, when the platform reported it.
    pub ttl: Option<i64>,
    /// The chunked upload id, when the chunked flow was used.
    pub upload_id: Option<String>,
}

/// Standard base64 with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The MD5 of the bytes, lowercase hexadecimal.
pub fn md5_hex(bytes: &[u8]) -> String {
    format!("{:x}", md5::compute(bytes))
}

/// The SHA-1 of the bytes, lowercase hexadecimal.
pub fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::Digest as _;
    let digest = sha1::Sha1::digest(bytes);
    hex::encode(digest)
}
