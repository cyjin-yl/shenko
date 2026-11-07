//! Configuration of the Feishu/Lark connector — SPEC §16.
//!
//! Feishu/Lark is an open platform: the account is an *app* identified by an
//! `app_id` and an `app_secret`. The secret is exchanged for a tenant ticket,
//! and every open-API call carries that ticket. The credentials therefore
//! have the ordinary app lifecycle: they are issued once, they are renewed on
//! expiry, and they are local to this deployment.
//!
//! The facts that live here:
//!
//! * `app_id` / `app_secret` — the app credentials. The secret is sent only
//!   to the token endpoint and never appears in a URL or a log line.
//! * `base_url` — the open-API host (`open.feishu.cn` or the Lark host
//!   `open.larksuite.com`). Changing the host is a deployment decision, not
//!   a wire parameter.
//! * `bot_open_id` — the bot's own open id, reported by the platform.
//! * `admin_open_ids` — the OWNER-SET administrative accounts, independent
//!   of the app ticket and of any group role.
//!
//! The account namespace is `feishu:<app_id>`. The platform serves chats
//! (`chat_id`) and, inside a chat, reply threads (`thread_id`).

use serde::{Deserialize, Serialize};

/// The default Feishu open-API host.
pub const DEFAULT_BASE_URL: &str = "https://open.feishu.cn";

/// The Lark (international) open-API host.
pub const LARK_BASE_URL: &str = "https://open.larksuite.com";

/// One Feishu/Lark connector's configuration.
///
/// `Debug` is implemented by hand so the app secret can never be printed by a
/// tracing field or a configuration dump.
#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuConfig {
    /// The open-API host.
    pub base_url: String,
    /// The app id.
    pub app_id: String,
    /// The app secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_secret: Option<String>,
    /// The bot's own open id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_open_id: Option<String>,
    /// Owner-set administrative accounts.
    #[serde(default)]
    pub admin_open_ids: Vec<String>,
    /// How many conversations keep a remembered reply context.
    pub reply_context_capacity: usize,
    /// How long a remembered reply context stays usable.
    pub reply_window_seconds: u64,
}

impl std::fmt::Debug for FeishuConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuConfig")
            .field("base_url", &self.base_url)
            .field("app_id", &self.app_id)
            .field("app_secret", &self.app_secret.as_ref().map(|_| "<redacted>"))
            .field("bot_open_id", &self.bot_open_id)
            .field("admin_open_ids", &self.admin_open_ids)
            .field("reply_context_capacity", &self.reply_context_capacity)
            .field("reply_window_seconds", &self.reply_window_seconds)
            .finish()
    }
}

impl Default for FeishuConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            app_id: String::new(),
            app_secret: None,
            bot_open_id: None,
            admin_open_ids: Vec::new(),
            reply_context_capacity: 256,
            reply_window_seconds: 1_800,
        }
    }
}

impl FeishuConfig {
    /// A config for one app.
    pub fn for_app(app_id: impl Into<String>, app_secret: Option<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret,
            ..Self::default()
        }
    }

    /// The account namespace of this connector.
    pub fn account_scope(&self) -> String {
        format!("feishu:{}", self.app_id)
    }

    /// Whether one open id is an owner-set administrator.
    pub fn is_admin(&self, open_id: &str) -> bool {
        self.admin_open_ids
            .iter()
            .any(|admin| admin == open_id)
    }
}
