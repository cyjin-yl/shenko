//! App authentication of the Feishu/Lark connector — SPEC §16.
//!
//! The account is an app, so the credential is an *app ticket*. The secret is
//! exchanged for a `tenant_access_token` at the token endpoint and the token
//! is cached until shortly before the provider's own expiry. The secret is
//! sent only to the token endpoint; it never appears in a URL or a log line.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::common::{CachedToken, PlatformAnswer, PlatformHttp, TokenCache, endpoint_provenance};

use super::config::FeishuConfig;

/// The refresh skew: renew this long before the provider's expiry.
const REFRESH_SKEW: Duration = Duration::from_secs(300);

/// The cached tenant ticket of one app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantToken {
    /// The ticket that authorizes open-API calls.
    pub access_token: String,
    /// When the ticket expires, as seconds since the epoch.
    pub expires_at: i64,
}

/// The app credentials behind one ticket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppTicket {
    /// The app id.
    pub app_id: String,
    /// The app secret; local only.
    #[serde(skip_serializing)]
    pub app_secret: String,
}

impl AppTicket {
    /// The app credentials of one config.
    pub fn from_config(config: &FeishuConfig) -> Option<Self> {
        Some(Self {
            app_id: config.app_id.clone(),
            app_secret: config.app_secret.clone()?,
        })
    }
}

/// Why one ticket request failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The platform answered and refused.
    #[error("feishu refused the ticket request: {0}")]
    Rejected(String),
    /// The answer was lost; the request may have been executed.
    #[error("feishu ticket outcome unknown: {0}")]
    Unknown(String),
    /// The platform answered, but the answer did not carry a ticket.
    #[error("feishu ticket answer was incomplete: {0}")]
    Incomplete(String),
}

/// The token endpoint answer.
#[derive(Debug, Deserialize)]
struct TokenAnswer {
    /// The platform result code; `0` is success.
    code: i64,
    /// The platform's own message.
    #[serde(default)]
    msg: String,
    /// The ticket, on success.
    #[serde(default)]
    tenant_access_token: Option<String>,
    /// The ticket lifetime in seconds.
    #[serde(default)]
    expire: Option<i64>,
}

/// A tenant-ticket provider with a cache.
pub struct TenantAuth {
    config: Arc<FeishuConfig>,
    http: PlatformHttp,
    cache: TokenCache,
    app_cache: TokenCache,
}

impl TenantAuth {
    /// Build an auth provider for one app.
    pub fn new(config: Arc<FeishuConfig>, http: PlatformHttp) -> Self {
        Self {
            config,
            http,
            cache: TokenCache::new(),
            app_cache: TokenCache::new(),
        }
    }

    /// The cached ticket, or a fresh one when the cache is stale.
    pub async fn token(&self) -> Result<String, AuthError> {
        if let Some(cached) = self.cache.peek().await {
            if !cached.is_stale(REFRESH_SKEW) {
                return Ok(cached.value);
            }
        }
        let credentials = AppTicket::from_config(&self.config).ok_or_else(|| {
            AuthError::Incomplete("no app secret configured for this feishu app".to_string())
        })?;
        let token = self.request(&credentials).await?;
        self.cache
            .store(CachedToken {
                value: token.access_token.clone(),
                expires_at: chrono::Utc::now()
                    + Duration::from_secs(token.expires_at.max(0) as u64),
            })
            .await;
        Ok(token.access_token)
    }

    /// Force a fresh ticket, ignoring the cache.
    pub async fn refresh(&self) -> Result<String, AuthError> {
        self.cache.invalidate().await;
        self.token().await
    }

    /// The app ticket that authorizes the event-dispatcher WebSocket.
    ///
    /// This is the platform's `app_access_token`, a different credential from
    /// the open-API tenant ticket; it is cached on its own lifetime.
    pub async fn app_token(&self) -> Result<String, AuthError> {
        if let Some(cached) = self.app_cache.peek().await {
            if !cached.is_stale(REFRESH_SKEW) {
                return Ok(cached.value);
            }
        }
        let credentials = AppTicket::from_config(&self.config).ok_or_else(|| {
            AuthError::Incomplete("no app secret configured for this feishu app".to_string())
        })?;
        let answer = self.request_value(&credentials).await?;
        let token = answer
            .get("app_access_token")
            .and_then(|value| value.as_str())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                AuthError::Incomplete("feishu answer carried no app_access_token".to_string())
            })?
            .to_string();
        let expires = answer
            .get("expire")
            .and_then(|value| value.as_i64())
            .unwrap_or(7_200);
        self.app_cache
            .store(CachedToken {
                value: token.clone(),
                expires_at: chrono::Utc::now() + Duration::from_secs(expires.max(0) as u64),
            })
            .await;
        Ok(token)
    }

    /// Where the ticket comes from, for a health fact.
    pub fn endpoint(&self) -> String {
        endpoint_provenance(&self.url())
    }

    /// Exchange the app credentials for a ticket.
    async fn request(&self, credentials: &AppTicket) -> Result<TenantToken, AuthError> {
        let body = self.request_value(credentials).await?;
        let parsed: TokenAnswer = serde_json::from_value(body.clone()).map_err(|error| {
            AuthError::Unknown(format!("feishu ticket answer was not json: {error}"))
        })?;
        if parsed.code != 0 {
            return Err(AuthError::Rejected(format!(
                "feishu ticket refused: {} {}",
                parsed.code, parsed.msg
            )));
        }
        let access_token = parsed
            .tenant_access_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| AuthError::Incomplete("feishu ticket answer carried no token".into()))?;
        Ok(TenantToken {
            access_token,
            expires_at: parsed.expire.unwrap_or(7_200),
        })
    }

    /// The raw token-endpoint answer.
    async fn request_value(
        &self,
        credentials: &AppTicket,
    ) -> Result<serde_json::Value, AuthError> {
        let answer = self
            .http
            .post_json(
                &self.url(),
                None,
                &serde_json::json!({
                    "app_id": credentials.app_id,
                    "app_secret": credentials.app_secret,
                }),
            )
            .await;
        let body = match answer {
            PlatformAnswer::Answered { body, .. } => body,
            PlatformAnswer::Rejected { status, message, .. } => {
                return Err(AuthError::Rejected(format!("http {status}: {message}")));
            }
            PlatformAnswer::Unknown { detail } => return Err(AuthError::Unknown(detail)),
        };
        Ok(body)
    }

    /// The token endpoint.
    fn url(&self) -> String {
        format!("{}/open-apis/auth/v3/tenant_access_token/internal", self.config.base_url)
    }
}
