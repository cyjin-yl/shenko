//! The Feishu/Lark open API — SPEC §16.
//!
//! Every egress call is the platform's own: a message is built from typed
//! blocks and posted to `im/v1/messages`, a reply names the message it
//! answers, an edit is a `PATCH`, a recall is a `DELETE`, and a typing
//! indicator is its own endpoint. Media is uploaded to the drive in two
//! steps (`upload_prepare` → `upload_part` → `upload_finish`) and sent by
//! `file_key`.
//!
//! The platform answers every call with `{code, msg, data}`; only `code == 0`
//! means the platform accepted the call. A non-zero code is a refusal
//! (nothing happened at the far end); a lost transport answer is `Unknown`,
//! because the platform may still have executed it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use shenko_types::channel::AttachmentError;

use crate::common::{
    MediaAnswer, PlatformAnswer, PlatformHttp, endpoint_provenance, fetch_media_bounded,
    refused_attachment, text_field,
};

use super::auth::{AuthError, TenantAuth};
use super::config::FeishuConfig;

/// Why one Feishu call did not produce an accepted result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeishuError {
    /// The platform answered and refused.
    #[error("feishu refused the request: {message} (code {code})")]
    Rejected {
        /// The platform result code.
        code: i64,
        /// The platform's own message.
        message: String,
    },
    /// The answer was lost; the request may have been executed.
    #[error("feishu outcome unknown: {0}")]
    Unknown(String),
    /// The platform answered, but the answer did not carry what was promised.
    #[error("feishu answer was incomplete: {0}")]
    Incomplete(String),
}

impl From<AuthError> for FeishuError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Rejected(detail) => FeishuError::Rejected {
                code: -1,
                message: detail,
            },
            AuthError::Unknown(detail) => FeishuError::Unknown(detail),
            AuthError::Incomplete(detail) => FeishuError::Incomplete(detail),
        }
    }
}

/// The audience of one send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveIdType {
    /// A chat (`chat_id`).
    Chat,
    /// A user (`open_id`).
    User,
}

impl ReceiveIdType {
    /// The wire value of the id type.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReceiveIdType::Chat => "chat_id",
            ReceiveIdType::User => "open_id",
        }
    }
}

/// One uploaded media reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedMedia {
    /// The platform file key.
    pub file_key: String,
}

/// The Feishu open API client.
#[derive(Clone)]
pub struct FeishuApi {
    config: Arc<FeishuConfig>,
    http: PlatformHttp,
    auth: Arc<TenantAuth>,
    sequence: Arc<AtomicU64>,
}

impl FeishuApi {
    /// Build a client for one app.
    pub fn new(config: Arc<FeishuConfig>, http: PlatformHttp, auth: Arc<TenantAuth>) -> Self {
        Self {
            config,
            http,
            auth,
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The configuration this client serves.
    pub fn config(&self) -> &FeishuConfig {
        &self.config
    }

    /// The HTTP transport, for attachment fetches.
    pub fn http(&self) -> &PlatformHttp {
        &self.http
    }

    /// Where sends come from, for a health fact.
    pub fn endpoint(&self) -> String {
        endpoint_provenance(&format!("{}/open-apis/im/v1/messages", self.config.base_url))
    }

    /// A fresh idempotency key (`X-Tt-Logid`-style client token).
    fn next_idempotency_key(&self) -> String {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!("shenko-{}-{seq}", self.config.app_id)
    }

    /// The bearer ticket for one call.
    async fn token(&self) -> Result<String, FeishuError> {
        Ok(self.auth.token().await?)
    }

    /// Send one message built from blocks to one audience.
    pub async fn send_message(
        &self,
        receive_id_type: ReceiveIdType,
        receive_id: &str,
        content: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, FeishuError> {
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type={}",
            self.config.base_url,
            receive_id_type.as_str()
        );
        let body = json!({
            "receive_id": receive_id,
            "msg_type": "post",
            "content": content,
        });
        self.authorized_post(&url, &body, idempotency_key).await
    }

    /// Reply in the thread of one message the platform delivered.
    pub async fn reply_message(
        &self,
        message_id: &str,
        content: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, FeishuError> {
        let url = format!(
            "{}/open-apis/im/v1/messages/{}/reply",
            self.config.base_url,
            encode_path(message_id)
        );
        let body = json!({ "msg_type": "post", "content": content });
        let answer = self
            .authorized_post(&url, &body, idempotency_key)
            .await?;
        Ok(answer)
    }

    /// Replace the blocks of one message.
    pub async fn edit_message(
        &self,
        message_id: &str,
        content: &Value,
    ) -> Result<Value, FeishuError> {
        let url = format!(
            "{}/open-apis/im/v1/messages/{}",
            self.config.base_url,
            encode_path(message_id)
        );
        let token = self.token().await?;
        let headers = self.json_headers(&token);
        let body = json!({ "msg_type": "post", "content": content });
        let answer = self
            .http
            .patch_json_with_headers(&url, &headers, &body)
            .await;
        self.read("edit_message", answer).await
    }

    /// Recall one message.
    pub async fn recall_message(&self, message_id: &str) -> Result<Value, FeishuError> {
        let url = format!(
            "{}/open-apis/im/v1/messages/{}",
            self.config.base_url,
            encode_path(message_id)
        );
        let token = self.token().await?;
        let headers = self.json_headers(&token);
        let answer = self.http.delete_with_headers(&url, &headers).await;
        self.read("recall_message", answer).await
    }

    /// Send a typing indicator into a chat.
    pub async fn typing(&self, chat_id: &str) -> Result<Value, FeishuError> {
        let url = format!("{}/open-apis/im/v1/messages/typing", self.config.base_url);
        let body = json!({ "chat_id": chat_id });
        let answer = self.authorized_post(&url, &body, None).await?;
        Ok(answer)
    }

    /// Upload bytes to the drive and return the `file_key`.
    pub async fn upload_media(
        &self,
        file_name: &str,
        kind: &str,
        bytes: &[u8],
    ) -> Result<UploadedMedia, FeishuError> {
        // The platform accepts a single-part upload for small media; the
        // multipart endpoint carries the bytes and the metadata together.
        let url = format!(
            "{}/open-apis/drive/v1/medias/upload_all",
            self.config.base_url
        );
        let token = self.token().await?;
        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name(file_name.to_string())
            .mime_str(mime_of(kind))
            .map_err(|error| FeishuError::Incomplete(format!("media mime failed: {error}")))?;
        let form = reqwest::multipart::Form::new()
            .text("file_name", file_name.to_string())
            .text("parent_type", parent_type_of(kind).to_string())
            .text("size", bytes.len().to_string())
            .part("file", part);
        let headers = self.bearer_headers(&token);
        let answer = self.http.post_multipart(&url, &headers, form).await;
        let body = self.read("upload_media", answer).await?;
        let file_key = body
            .get("data")
            .and_then(|data| text_field(data, "file_key"))
            .ok_or_else(|| FeishuError::Incomplete("upload carried no file_key".to_string()))?;
        Ok(UploadedMedia { file_key })
    }

    /// Download one message resource in bounded chunks.
    ///
    /// The resource endpoint needs both the message id the resource arrived
    /// with and the tenant ticket, so this is the installed app's own call, not
    /// a bare fetch of a published URL. The ticket is one real fact about the
    /// app: a refused ticket is reported as such, distinct from a resource the
    /// platform will not serve.
    pub async fn download_media(
        &self,
        channel: &str,
        attachment_id: &str,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<MediaAnswer, AttachmentError> {
        let token = self.token().await.map_err(|error| AttachmentError::Unauthenticated {
            channel: channel.to_string(),
            attachment_id: attachment_id.to_string(),
            detail: format!("tenant ticket refused: {error}"),
        })?;
        let url = format!(
            "{}/open-apis/im/v1/messages/{}/resources/{}?type={}",
            self.config.base_url,
            encode_path(message_id),
            encode_path(file_key),
            encode_query(resource_type)
        );
        match fetch_media_bounded(&self.http, &url, Some(&token), attachment_id).await? {
            delivered @ MediaAnswer::Delivered { .. } => Ok(delivered),
            MediaAnswer::Refused { status, detail } => Err(refused_attachment(
                channel,
                attachment_id,
                status,
                detail,
            )),
        }
    }

    /// One authorized JSON POST.
    async fn authorized_post(
        &self,
        url: &str,
        body: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<Value, FeishuError> {
        let token = self.token().await?;
        let mut headers = self.json_headers(&token);
        let key = idempotency_key
            .map(str::to_string)
            .unwrap_or_else(|| self.next_idempotency_key());
        headers.push(("X-Tt-Logid".to_string(), key));
        let answer = self.http.post_json_with_headers(url, &headers, body).await;
        self.read("post", answer).await
    }

    /// The JSON headers of one authorized call.
    fn json_headers(&self, token: &str) -> Vec<(String, String)> {
        let mut headers = self.bearer_headers(token);
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        headers
    }

    /// The bearer headers of one authorized call.
    fn bearer_headers(&self, token: &str) -> Vec<(String, String)> {
        vec![(
            "Authorization".to_string(),
            format!("Bearer {token}"),
        )]
    }

    /// Read one platform answer, separating refusal from lost answer.
    async fn read(&self, action: &str, answer: PlatformAnswer) -> Result<Value, FeishuError> {
        let body = match answer {
            PlatformAnswer::Answered { body, .. } => body,
            PlatformAnswer::Rejected { code, message, .. } => {
                return Err(FeishuError::Rejected {
                    code: code.unwrap_or(-1),
                    message: if message.is_empty() {
                        format!("http refusal during {action}")
                    } else {
                        message
                    },
                });
            }
            PlatformAnswer::Unknown { detail } => return Err(FeishuError::Unknown(detail)),
        };
        let code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code != 0 {
            return Err(FeishuError::Rejected {
                code,
                message: text_field(&body, "msg")
                    .unwrap_or_else(|| format!("{action} refused with code {code}")),
            });
        }
        // The full `{code, msg, data}` body travels on so a receipt can read
        // both the verdict and the issued message id.
        Ok(body)
    }
}

/// The drive parent type of one media kind.
fn parent_type_of(kind: &str) -> &'static str {
    match kind {
        "image" => "message_image",
        "video" => "message_video",
        "file" => "message_file",
        _ => "message_file",
    }
}

/// The MIME of one media kind.
fn mime_of(kind: &str) -> &'static str {
    match kind {
        "image" => "image/png",
        "video" => "video/mp4",
        "voice" => "audio/opus",
        _ => "application/octet-stream",
    }
}

/// Percent-encode one path segment.
fn encode_path(value: &str) -> String {
    encode_query(value)
}

/// Percent-encode one query parameter.
fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}
