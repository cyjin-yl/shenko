//! The WeChat iLink bot API client — docs/specs/channel-adapters.md#spec-16, docs/specs/channel-adapters.md#spec-15-3.
//!
//! Every call here is a named endpoint of the provider's JSON protocol:
//!
//! | purpose | call |
//! | --- | --- |
//! | receive | `POST /ilink/bot/getupdates` |
//! | send | `POST /ilink/bot/sendmessage` |
//! | typing | `POST /ilink/bot/sendtyping` |
//! | typing ticket | `POST /ilink/bot/getconfig` |
//! | media upload ticket | `POST /ilink/bot/getuploadurl` |
//! | lifecycle | `POST /ilink/bot/msg/notifystart`, `…/notifystop` |
//!
//! The protocol has no edit endpoint and no retract endpoint. This client
//! therefore offers neither, and the capability record says so.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::common::{PlatformAnswer, PlatformHttp};

use super::config::ILinkConfig;

/// The media kind of an upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// An image.
    Image,
    /// A video.
    Video,
    /// A file.
    File,
    /// A voice message.
    Voice,
}

impl MediaKind {
    /// The numeric `media_type` the provider expects.
    pub fn as_number(self) -> u8 {
        match self {
            MediaKind::Image => 1,
            MediaKind::Video => 2,
            MediaKind::File => 3,
            MediaKind::Voice => 4,
        }
    }
}

/// The typing state the provider understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingState {
    /// The bot is typing.
    Typing,
    /// The bot stopped typing.
    Stopped,
}

impl TypingState {
    /// The numeric `status` the provider expects.
    pub fn as_number(self) -> u8 {
        match self {
            TypingState::Typing => 1,
            TypingState::Stopped => 2,
        }
    }
}

/// Why one iLink call did not produce an accepted result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ILinkError {
    /// The provider answered and refused.
    #[error("ilink refused the request: {message} (ret {ret:?})")]
    Rejected {
        /// The provider's `ret` value.
        ret: Option<i64>,
        /// The provider's message.
        message: String,
    },
    /// The answer was lost; the request may have been executed.
    #[error("ilink outcome unknown: {0}")]
    Unknown(String),
    /// The provider answered, but the answer did not carry what was promised.
    #[error("ilink answer was incomplete: {0}")]
    Incomplete(String),
}

/// The iLink bot API client.
#[derive(Clone)]
pub struct ILinkApi {
    config: Arc<ILinkConfig>,
    http: PlatformHttp,
    client_nonce: Arc<AtomicU32>,
}

impl ILinkApi {
    /// Build a client for one iLink configuration.
    pub fn new(config: Arc<ILinkConfig>, http: PlatformHttp) -> Self {
        Self {
            config,
            http,
            client_nonce: Arc::new(AtomicU32::new(0)),
        }
    }

    /// The configuration this client serves.
    pub fn config(&self) -> &ILinkConfig {
        &self.config
    }

    /// The HTTP client this API uses.
    pub fn http(&self) -> &PlatformHttp {
        &self.http
    }

    /// The headers every authenticated request carries.
    ///
    /// `AuthorizationType` names the credential kind, `Authorization` carries
    /// the credential itself, `X-WECHAT-UIN` is an opaque client id and
    /// `iLink-App-ClientVersion` is the encoded plugin version.
    fn headers(&self) -> Result<Vec<(String, String)>, ILinkError> {
        let token = self.config.bot_token.as_deref().ok_or_else(|| {
            ILinkError::Incomplete("no bot token configured for this iLink account".to_string())
        })?;
        Ok(vec![
            ("AuthorizationType".to_string(), "ilink_bot_token".to_string()),
            ("Authorization".to_string(), format!("Bearer {token}")),
            ("X-WECHAT-UIN".to_string(), self.wechat_uin()),
            ("iLink-App-Id".to_string(), self.config.app_id.clone()),
            (
                "iLink-App-ClientVersion".to_string(),
                self.config.client_version(),
            ),
        ])
    }

    /// The opaque client id of this process.
    fn wechat_uin(&self) -> String {
        use base64::Engine as _;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u32)
            .unwrap_or(0);
        let counter = self.client_nonce.fetch_add(1, Ordering::Relaxed);
        let value = nanos ^ counter.wrapping_mul(0x9e37_79b9);
        base64::engine::general_purpose::STANDARD.encode(value.to_string())
    }

    /// `POST` an authenticated JSON request.
    async fn post(&self, path: &str, body: &Value) -> Result<Value, ILinkError> {
        let url = format!("{}{path}", self.config.base_url());
        let headers = self.headers()?;
        let answer = self.http.post_json_with_headers(&url, &headers, body).await;
        match answer {
            PlatformAnswer::Answered { body, .. } => {
                let ret = body.get("ret").and_then(Value::as_i64);
                if let Some(ret) = ret {
                    if ret != 0 {
                        return Err(ILinkError::Rejected {
                            ret: Some(ret),
                            message: body
                                .get("errmsg")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        });
                    }
                }
                Ok(body)
            }
            PlatformAnswer::Rejected { status, code, message, .. } => Err(ILinkError::Rejected {
                ret: code.or(Some(status as i64)),
                message,
            }),
            PlatformAnswer::Unknown { detail } => Err(ILinkError::Unknown(detail)),
        }
    }

    /// Long-poll for inbound messages.
    ///
    /// The cursor is the provider's own `get_updates_buf`; it is opaque and
    /// must be handed back unchanged.
    pub async fn get_updates(&self, cursor: &str) -> Result<Updates, ILinkError> {
        let body = json!({
            "get_updates_buf": cursor,
            "base_info": self.config.base_info(),
        });
        let answer = self.post("/ilink/bot/getupdates", &body).await?;
        let messages = answer
            .get("msgs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let next_cursor = answer
            .get("get_updates_buf")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned);
        let timeout = answer
            .get("longpolling_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(self.config.long_poll_timeout_ms);
        Ok(Updates {
            messages,
            next_cursor,
            long_poll_timeout_ms: timeout,
        })
    }

    /// Send one text message into a conversation.
    pub async fn send_text(
        &self,
        to_user_id: &str,
        context_token: Option<&str>,
        client_id: &str,
        text: &str,
    ) -> Result<Value, ILinkError> {
        let mut message = json!({
            "from_user_id": "",
            "to_user_id": to_user_id,
            "client_id": client_id,
            "message_type": 2,
            "message_state": 2,
            "item_list": [
                { "type": 1, "text_item": { "text": text } }
            ],
        });
        if let Some(token) = context_token {
            message["context_token"] = Value::String(token.to_string());
        }
        let body = json!({
            "msg": message,
            "base_info": self.config.base_info(),
        });
        self.post("/ilink/bot/sendmessage", &body).await
    }

    /// Send one item into a conversation.
    ///
    /// The item is the provider's own message item, e.g. an `image_item`
    /// that carries the encrypted CDN descriptor of an uploaded file. The
    /// bytes were uploaded separately.
    pub async fn send_media(
        &self,
        to_user_id: &str,
        context_token: Option<&str>,
        client_id: &str,
        item: &Value,
    ) -> Result<Value, ILinkError> {
        let mut message = json!({
            "from_user_id": "",
            "to_user_id": to_user_id,
            "client_id": client_id,
            "message_type": 2,
            "message_state": 2,
            "item_list": [ item ],
        });
        if let Some(token) = context_token {
            message["context_token"] = Value::String(token.to_string());
        }
        let body = json!({
            "msg": message,
            "base_info": self.config.base_info(),
        });
        self.post("/ilink/bot/sendmessage", &body).await
    }

    /// Ask the provider for a typing ticket.
    pub async fn get_config(
        &self,
        ilink_user_id: &str,
        context_token: Option<&str>,
    ) -> Result<Option<String>, ILinkError> {
        let mut body = json!({
            "ilink_user_id": ilink_user_id,
            "base_info": self.config.base_info(),
        });
        if let Some(token) = context_token {
            body["context_token"] = Value::String(token.to_string());
        }
        let answer = self.post("/ilink/bot/getconfig", &body).await?;
        Ok(answer
            .get("typing_ticket")
            .and_then(Value::as_str)
            .filter(|ticket| !ticket.is_empty())
            .map(str::to_owned))
    }

    /// Publish or cancel the typing indicator.
    pub async fn send_typing(
        &self,
        ilink_user_id: &str,
        typing_ticket: &str,
        state: TypingState,
    ) -> Result<(), ILinkError> {
        let body = json!({
            "ilink_user_id": ilink_user_id,
            "typing_ticket": typing_ticket,
            "status": state.as_number(),
            "base_info": self.config.base_info(),
        });
        self.post("/ilink/bot/sendtyping", &body).await?;
        Ok(())
    }

    /// Ask the provider where to upload a media file.
    pub async fn get_upload_url(
        &self,
        filekey: &str,
        media_kind: MediaKind,
        to_user_id: &str,
        raw_size: usize,
        raw_md5: &str,
        cipher_size: usize,
        aes_key_hex: &str,
    ) -> Result<Value, ILinkError> {
        let body = json!({
            "filekey": filekey,
            "media_type": media_kind.as_number(),
            "to_user_id": to_user_id,
            "rawsize": raw_size,
            "rawfilemd5": raw_md5,
            "filesize": cipher_size,
            "no_need_thumb": true,
            "aeskey": aes_key_hex,
            "base_info": self.config.base_info(),
        });
        self.post("/ilink/bot/getuploadurl", &body).await
    }

    /// Tell the provider that this client started.
    pub async fn notify_start(&self) -> Result<(), ILinkError> {
        let body = json!({ "base_info": self.config.base_info() });
        self.post("/ilink/bot/msg/notifystart", &body).await?;
        Ok(())
    }

    /// Tell the provider that this client stopped.
    pub async fn notify_stop(&self) -> Result<(), ILinkError> {
        let body = json!({ "base_info": self.config.base_info() });
        self.post("/ilink/bot/msg/notifystop", &body).await?;
        Ok(())
    }

    /// Upload encrypted media bytes to the CDN.
    ///
    /// The provider answers with an `x-encrypted-param` header; that header is
    /// the download parameter of the stored object and is part of the media
    /// reference that travels with the message.
    pub async fn upload_cdn(
        &self,
        upload_url: &str,
        ciphertext: Vec<u8>,
    ) -> Result<String, ILinkError> {
        let response = self
            .http
            .client()
            .post(upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext)
            .send()
            .await
            .map_err(|error| {
                ILinkError::Unknown(crate::common::redact_transport_error(&error.to_string()))
            })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(ILinkError::Rejected {
                ret: Some(status as i64),
                message: "cdn upload refused".to_string(),
            });
        }
        let parameter = response
            .headers()
            .get("x-encrypted-param")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        parameter.ok_or_else(|| {
            ILinkError::Incomplete("cdn upload answer carried no x-encrypted-param".to_string())
        })
    }

    /// The download URL of an encrypted media object.
    pub fn download_url(&self, encrypt_query_param: &str) -> String {
        format!(
            "{}/download?encrypted_query_param={}",
            self.config.cdn_base_url(),
            urlencoding(encrypt_query_param)
        )
    }
}

/// One long-poll answer of the iLink provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updates {
    /// The inbound messages.
    pub messages: Vec<Value>,
    /// The cursor of the next poll.
    pub next_cursor: Option<String>,
    /// The long-poll timeout the provider suggests.
    pub long_poll_timeout_ms: u64,
}

/// Percent-encode a query parameter.
fn urlencoding(value: &str) -> String {
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

/// The timeout the provider's long poll should use.
pub fn long_poll_timeout(config: &ILinkConfig, suggested: u64) -> Duration {
    let millis = if suggested == 0 {
        config.long_poll_timeout_ms
    } else {
        suggested
    };
    Duration::from_millis(millis.clamp(1_000, 120_000))
}
