//! Configuration of the official QQ bot connector — docs/specs/channel-adapters.md#spec-16, docs/specs/channels.md#spec-35-6.
//!
//! Four different facts live here and must not be confused:
//!
//! * `app_id` / `app_secret` — the developer's own bot ticket. The secret
//!   never appears in a log line, a receipt, a sealed artifact or a URL.
//! * `api_base` / `token_base` — where the platform lives. The sandbox and
//!   the production host are different deployments, not different privileges.
//! * `intents` — which event families this bot is allowed to subscribe to.
//!   The platform refuses an unauthorised intent and closes the connection,
//!   so the default is the narrowest set that still carries messages.
//! * `admin_openids` — the OWNER-SET administrative accounts. They are
//!   configured by the owner and are independent of the app ticket, of the
//!   group role and of the transport.
//!
//! The account namespace is `qq:<app_id>`: the official platform's OpenIDs
//! are scoped to one application, so two bots of the same owner still see
//! different user ids. That difference is real and is not smoothed over.

use serde::{Deserialize, Serialize};

/// The production host of the official QQ bot OpenAPI.
pub const DEFAULT_API_BASE: &str = "https://api.bot.qq.com";

/// The production host that issues the app access token.
pub const DEFAULT_TOKEN_BASE: &str = "https://bots.qq.com";

/// `GROUP_AND_C2C_EVENT` — group and single-chat messages.
pub const INTENT_GROUP_AND_C2C: u32 = 1 << 25;

/// `PUBLIC_GUILD_MESSAGES` — messages that mention the bot in a channel.
pub const INTENT_PUBLIC_GUILD_MESSAGES: u32 = 1 << 30;

/// `DIRECT_MESSAGE` — direct messages to the bot.
pub const INTENT_DIRECT_MESSAGE: u32 = 1 << 12;

/// `GUILDS` — guild lifecycle events.
pub const INTENT_GUILDS: u32 = 1 << 0;

/// `GUILD_MESSAGES` — every channel message; only private-domain bots hold
/// this permission.
pub const INTENT_GUILD_MESSAGES: u32 = 1 << 9;

/// The default intent set: the platform's basic permissions plus the message
/// families a conversational agent needs.
pub const DEFAULT_INTENTS: u32 =
    INTENT_GUILDS | INTENT_DIRECT_MESSAGE | INTENT_GROUP_AND_C2C | INTENT_PUBLIC_GUILD_MESSAGES;

/// One official QQ bot connector's configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct QqOfficialConfig {
    /// The bot's AppID.
    pub app_id: String,
    /// The bot's AppSecret. It is only ever used to request an access token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_secret: Option<String>,
    /// The OpenAPI host.
    pub api_base: String,
    /// The host that issues the access token.
    pub token_base: String,
    /// A fixed gateway URL, when the deployment must not discover one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_url: Option<String>,
    /// The event families this connector subscribes to.
    pub intents: u32,
    /// The shard of this connection.
    pub shard: [u32; 2],
    /// Owner-set administrative accounts.
    #[serde(default)]
    pub admin_openids: Vec<String>,
    /// How long a remembered reply context stays usable.
    pub reply_window_seconds: u64,
    /// How many conversations keep a remembered reply context.
    pub reply_context_capacity: usize,
}

impl std::fmt::Debug for QqOfficialConfig {
    /// The app ticket is a credential: a debug line names the account and the
    /// hosts, never the secret that buys the access token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QqOfficialConfig")
            .field("app_id", &self.app_id)
            .field(
                "app_secret",
                &self.app_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("api_base", &self.api_base)
            .field("token_base", &self.token_base)
            .field("gateway_url", &self.gateway_url)
            .field("intents", &self.intents)
            .field("shard", &self.shard)
            .field("admin_openids", &self.admin_openids)
            .field("reply_window_seconds", &self.reply_window_seconds)
            .field("reply_context_capacity", &self.reply_context_capacity)
            .finish()
    }
}

impl QqOfficialConfig {
    /// A configuration with the production hosts and the default intents.
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: Some(app_secret.into()),
            api_base: DEFAULT_API_BASE.to_string(),
            token_base: DEFAULT_TOKEN_BASE.to_string(),
            gateway_url: None,
            intents: DEFAULT_INTENTS,
            shard: [0, 1],
            admin_openids: vec![],
            reply_window_seconds: 300,
            reply_context_capacity: 512,
        }
    }

    /// The account namespace this connector's conversations belong to.
    pub fn account_scope(&self) -> String {
        format!("qq:{}", self.app_id)
    }

    /// Whether the owner set this account as an administrator.
    pub fn is_admin(&self, openid: &str) -> bool {
        self.admin_openids.iter().any(|admin| admin == openid)
    }

    /// The platform's OpenAPI host without a trailing slash.
    pub fn api_base(&self) -> &str {
        self.api_base.trim_end_matches('/')
    }

    /// The token host without a trailing slash.
    pub fn token_base(&self) -> &str {
        self.token_base.trim_end_matches('/')
    }
}
