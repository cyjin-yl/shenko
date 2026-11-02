//! QR-code authorisation of the WeChat iLink connector — SPEC §16.
//!
//! The iLink provider does not use an app ticket. A human scans a QR code,
//! the provider issues a bot token, and the token is stored locally. The
//! token is a credential: it is written to a file that only the owner can
//! read and it never appears in a log line, a receipt or a URL.
//!
//! The flow is the provider's own:
//!
//! 1. `POST /ilink/bot/get_bot_qrcode?bot_type=3` — ask for a code.
//! 2. `GET /ilink/bot/get_qrcode_status?qrcode=…` — poll until the human
//!    scanned (`scaned`), confirmed (`confirmed`), must enter a verification
//!    code (`need_verifycode`), the code expired (`expired`) or the provider
//!    moved to another host (`scaned_but_redirect`).
//! 3. On `confirmed` the answer carries the bot token, the bot id and the
//!    human account that authorised it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::common::{PlatformAnswer, PlatformHttp, text_field};

use super::config::ILinkConfig;

/// The credentials the provider issued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ILinkCredentials {
    /// The bot token.
    pub bot_token: String,
    /// The bot account id.
    pub ilink_bot_id: String,
    /// The human account that authorised the bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ilink_user_id: Option<String>,
    /// The API host the provider pointed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// The state of one QR login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrStatus {
    /// The code was issued and nobody scanned it yet.
    Waiting,
    /// The code was scanned; the provider is still verifying.
    Scanned,
    /// The provider confirmed the login.
    Confirmed(ILinkCredentials),
    /// The human must type the displayed verification code.
    NeedVerifyCode,
    /// Too many wrong verification codes.
    VerifyCodeBlocked,
    /// The code expired.
    Expired,
    /// The provider moved the login to another host.
    Redirect(String),
    /// The provider reported a state this adapter does not know.
    Unknown(String),
}

/// The QR code the provider issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrCode {
    /// The opaque code value that is polled.
    pub qrcode: String,
    /// The content the owner shows as a QR image.
    pub qrcode_img_content: String,
}

/// Why one QR login step failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QrError {
    /// The provider answered and refused.
    #[error("ilink refused the login request: {0}")]
    Rejected(String),
    /// The answer was lost.
    #[error("ilink login outcome unknown: {0}")]
    Unknown(String),
    /// The provider answered, but the answer was incomplete.
    #[error("ilink login answer was incomplete: {0}")]
    Incomplete(String),
}

/// The QR login client.
pub struct QrLogin {
    config: Arc<ILinkConfig>,
    http: PlatformHttp,
}

impl QrLogin {
    /// Build a login client.
    pub fn new(config: Arc<ILinkConfig>, http: PlatformHttp) -> Self {
        Self { config, http }
    }

    /// The application headers the QR endpoints expect.
    ///
    /// The QR endpoints do NOT carry the bot credential: there is no token
    /// yet when the code is requested.
    fn headers(&self) -> Vec<(String, String)> {
        vec![
            ("iLink-App-Id".to_string(), self.config.app_id.clone()),
            (
                "iLink-App-ClientVersion".to_string(),
                self.config.client_version(),
            ),
        ]
    }

    /// Ask the provider for a QR code.
    pub async fn request_qrcode(&self) -> Result<QrCode, QrError> {
        let url = format!(
            "{}/ilink/bot/get_bot_qrcode?bot_type=3",
            self.config.base_url()
        );
        let headers = self.headers();
        let answer = self
            .http
            .post_json_with_headers(&url, &headers, &json!({ "local_token_list": [] }))
            .await;
        let body = match answer {
            PlatformAnswer::Answered { body, .. } => body,
            PlatformAnswer::Rejected { status, message, .. } => {
                return Err(QrError::Rejected(format!(
                    "http {status}: {message}"
                )));
            }
            PlatformAnswer::Unknown { detail } => return Err(QrError::Unknown(detail)),
        };
        let qrcode = text_field(&body, "qrcode").ok_or_else(|| {
            QrError::Incomplete("the QR answer carried no qrcode value".to_string())
        })?;
        let qrcode_img_content =
            text_field(&body, "qrcode_img_content").unwrap_or_else(|| qrcode.clone());
        Ok(QrCode {
            qrcode,
            qrcode_img_content,
        })
    }

    /// Poll the status of one QR code.
    ///
    /// `verify_code` is the code the provider asked the human to type.
    pub async fn poll_status(
        &self,
        qrcode: &str,
        verify_code: Option<&str>,
    ) -> Result<QrStatus, QrError> {
        let mut url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={}",
            self.config.base_url(),
            encode_query(qrcode)
        );
        if let Some(code) = verify_code {
            url.push_str(&format!("&verify_code={}", encode_query(code)));
        }
        let headers = self.headers();
        let answer = self.http.get_with_headers(&url, &headers).await;
        let body = match answer {
            PlatformAnswer::Answered { body, .. } => body,
            PlatformAnswer::Rejected { status, message, .. } => {
                return Err(QrError::Rejected(format!("http {status}: {message}")));
            }
            PlatformAnswer::Unknown { detail } => return Err(QrError::Unknown(detail)),
        };
        Ok(read_status(&body))
    }
}

/// Read the provider's status answer.
fn read_status(body: &Value) -> QrStatus {
    match body.get("status").and_then(Value::as_str).unwrap_or("") {
        "wait" => QrStatus::Waiting,
        "scaned" => QrStatus::Scanned,
        "confirmed" => {
            let bot_token = text_field(body, "bot_token").unwrap_or_default();
            let ilink_bot_id = text_field(body, "ilink_bot_id").unwrap_or_default();
            if bot_token.is_empty() || ilink_bot_id.is_empty() {
                return QrStatus::Unknown("confirmed without credentials".to_string());
            }
            QrStatus::Confirmed(ILinkCredentials {
                bot_token,
                ilink_bot_id,
                ilink_user_id: text_field(body, "ilink_user_id"),
                base_url: text_field(body, "baseurl"),
            })
        }
        "need_verifycode" => QrStatus::NeedVerifyCode,
        "verify_code_blocked" => QrStatus::VerifyCodeBlocked,
        "expired" => QrStatus::Expired,
        "scaned_but_redirect" => QrStatus::Redirect(
            text_field(body, "redirect_host").unwrap_or_else(|| "baseurl".to_string()),
        ),
        other => QrStatus::Unknown(other.to_string()),
    }
}

/// Where the iLink credentials of one account are stored.
pub fn credentials_path(data_dir: &Path, account: &str) -> PathBuf {
    data_dir.join(format!("ilink-credentials-{account}.json"))
}

/// Write the credentials locally, readable by the owner only.
pub async fn store_credentials(
    path: &Path,
    credentials: &ILinkCredentials,
) -> Result<(), String> {
    let text = serde_json::to_string_pretty(credentials)
        .map_err(|error| format!("ilink credential encode failed: {error}"))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("ilink credential directory failed: {error}"))?;
    }
    tokio::fs::write(path, text)
        .await
        .map_err(|error| format!("ilink credential write failed: {error}"))?;
    restrict(path).await
}

/// Read stored credentials.
pub async fn load_credentials(path: &Path) -> Option<ILinkCredentials> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

/// Restrict a credential file to the owner.
async fn restrict(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(path, permissions)
            .await
            .map_err(|error| format!("ilink credential permission failed: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Percent-encode a query parameter.
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
