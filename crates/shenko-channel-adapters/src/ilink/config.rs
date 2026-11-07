//! Configuration of the WeChat iLink connector — SPEC §16.
//!
//! The iLink provider is reached with a *bot token* that a human obtains by
//! scanning a QR code. The credentials therefore have a lifecycle the other
//! adapters do not have: they are obtained interactively, they expire, and
//! they may be re-bound to another deployment.
//!
//! Four facts live here:
//!
//! * `bot_token` — the bearer credential of the bot API. It is sent as
//!   `Authorization: Bearer …` and never appears in a URL or a log line.
//! * `ilink_bot_id` / `ilink_user_id` — the account identities the provider
//!   reported. They are different namespaces: one names the bot, the other
//!   the human who scanned the code.
//! * `base_url` / `cdn_base_url` — where the provider and its media CDN live.
//!   The QR flow may answer with a different API host, which the connector
//!   records instead of guessing.
//! * `admin_user_ids` — the OWNER-SET administrative accounts, independent
//!   of the token and of the provider.
//!
//! The account namespace is `ilink:<ilink_bot_id>`. The provider's direct
//! conversations are the only audience this adapter serves: the protocol
//! exposes no group send, so no group capability is claimed.

use serde::{Deserialize, Serialize};

/// The default iLink API host.
pub const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

/// The default iLink media CDN host.
pub const DEFAULT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// The application id the provider expects in `iLink-App-Id`.
pub const DEFAULT_APP_ID: &str = "bot";

/// The channel version this adapter reports to the provider.
pub const DEFAULT_CHANNEL_VERSION: &str = "2.4.8";

/// One iLink connector's configuration.
///
/// `Debug` is implemented by hand: the bot token is a credential, so a debug
/// line names the account and the hosts and says `<redacted>` instead of the
/// token value.
#[derive(Clone, Serialize, Deserialize)]
pub struct ILinkConfig {
    /// The API host.
    pub base_url: String,
    /// The media CDN host.
    pub cdn_base_url: String,
    /// The application id sent as `iLink-App-Id`.
    pub app_id: String,
    /// The bot token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    /// The bot account id the provider reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ilink_bot_id: Option<String>,
    /// The human account that authorised this bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ilink_user_id: Option<String>,
    /// The channel version reported in `base_info`.
    pub channel_version: String,
    /// The sanitized observability identifier reported in `base_info`.
    pub bot_agent: String,
    /// Owner-set administrative accounts.
    #[serde(default)]
    pub admin_user_ids: Vec<String>,
    /// The long-poll timeout the provider suggests, in milliseconds.
    pub long_poll_timeout_ms: u64,
    /// How many conversations keep a remembered reply context.
    pub reply_context_capacity: usize,
    /// How long a remembered reply context stays usable.
    pub reply_window_seconds: u64,
}

impl std::fmt::Debug for ILinkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ILinkConfig")
            .field("base_url", &self.base_url)
            .field("cdn_base_url", &self.cdn_base_url)
            .field("app_id", &self.app_id)
            .field("bot_token", &self.bot_token.as_ref().map(|_| "<redacted>"))
            .field("ilink_bot_id", &self.ilink_bot_id)
            .field("ilink_user_id", &self.ilink_user_id)
            .field("channel_version", &self.channel_version)
            .field("bot_agent", &self.bot_agent)
            .field("admin_user_ids", &self.admin_user_ids)
            .field("long_poll_timeout_ms", &self.long_poll_timeout_ms)
            .field("reply_context_capacity", &self.reply_context_capacity)
            .field("reply_window_seconds", &self.reply_window_seconds)
            .finish()
    }
}

impl Default for ILinkConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            cdn_base_url: DEFAULT_CDN_BASE_URL.to_string(),
            app_id: DEFAULT_APP_ID.to_string(),
            bot_token: None,
            ilink_bot_id: None,
            ilink_user_id: None,
            channel_version: DEFAULT_CHANNEL_VERSION.to_string(),
            bot_agent: "Shenko".to_string(),
            admin_user_ids: vec![],
            long_poll_timeout_ms: 35_000,
            reply_context_capacity: 512,
            reply_window_seconds: 900,
        }
    }
}

impl ILinkConfig {
    /// A configuration with the default hosts.
    pub fn new(bot_token: impl Into<String>, ilink_bot_id: impl Into<String>) -> Self {
        Self {
            bot_token: Some(bot_token.into()),
            ilink_bot_id: Some(ilink_bot_id.into()),
            ..Self::default()
        }
    }

    /// The account namespace this connector's conversations belong to.
    pub fn account_scope(&self) -> String {
        format!("ilink:{}", self.ilink_bot_id.clone().unwrap_or_default())
    }

    /// Whether the owner set this account as an administrator.
    pub fn is_admin(&self, user_id: &str) -> bool {
        self.admin_user_ids.iter().any(|admin| admin == user_id)
    }

    /// The API host without a trailing slash.
    pub fn base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// The media CDN host without a trailing slash.
    pub fn cdn_base_url(&self) -> &str {
        self.cdn_base_url.trim_end_matches('/')
    }

    /// The `base_info` block every authenticated request carries.
    pub fn base_info(&self) -> serde_json::Value {
        serde_json::json!({
            "channel_version": self.channel_version,
            "bot_agent": self.bot_agent,
        })
    }

    /// The `iLink-App-ClientVersion` value.
    ///
    /// The provider expects the plugin version encoded as `0x00MMNNPP` and
    /// sent as a decimal string. The channel version `2.4.8` therefore
    /// becomes `0x020408`.
    pub fn client_version(&self) -> String {
        let mut parts = self.channel_version.split('.');
        let major = parts.next().and_then(|part| part.parse::<u32>().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|part| part.parse::<u32>().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|part| part.parse::<u32>().ok()).unwrap_or(0);
        let encoded = (major << 16) | (minor << 8) | patch;
        encoded.to_string()
    }
}
