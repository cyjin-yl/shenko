//! QQ/NapCat connector configuration — docs/specs/channel-adapters.md#spec-16, docs/specs/channels.md#spec-35-6.
//!
//! Four different facts live here and must not be confused:
//!
//! * `ws_url` — where the OneBot WebSocket endpoint is;
//! * `access_token` — the TRANSPORT credential of that endpoint. It proves
//!   nothing about who may talk to the agent;
//! * `bot_qq` — the account this connector serves;
//! * `admin_qq` — the OWNER-SET administrative accounts. They are configured
//!   by the owner, are independent of the transport credential, and are never
//!   derived from a group role, a voiceprint or familiarity.
//!
//! The media root names the directory on the NapCat host that holds the
//! attachment files the platform reports, so a locator can be resolved by the
//! file capability of that host instead of being mistaken for a local path.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One NapCat/OneBot v11 connector's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NapCatConfig {
    /// The OneBot v11 WebSocket endpoint.
    pub ws_url: String,
    /// The endpoint's transport token. It is sent as an `Authorization`
    /// header and never appears in a URL, a log line, a receipt or a binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// The account this connector serves. It binds the ingress database.
    pub bot_qq: u64,
    /// Owner-set administrator accounts. Group ownership is NOT administration.
    #[serde(default)]
    pub admin_qq: Vec<u64>,
    /// Reconnect backoff in milliseconds.
    pub reconnect_interval_ms: u64,
    /// The directory on the NapCat host that contains the reported media
    /// files. Attachment locators are resolved inside it and nowhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_root: Option<PathBuf>,
    /// The name of the host that owns the attachment bytes. Defaults to the
    /// local machine; a remote NapCat host must name itself so the locator is
    /// not mistaken for a local path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_host: Option<String>,
}

impl Default for NapCatConfig {
    fn default() -> Self {
        Self {
            ws_url: "ws://127.0.0.1:3001".to_string(),
            access_token: None,
            bot_qq: 0,
            admin_qq: vec![],
            reconnect_interval_ms: 5000,
            file_root: None,
            file_host: None,
        }
    }
}

impl NapCatConfig {
    /// The account namespace this connector's conversations belong to.
    pub fn account_scope(&self) -> String {
        format!("qq:{}", self.bot_qq)
    }

    /// The host that owns the attachment bytes.
    pub fn file_host(&self) -> &str {
        self.file_host.as_deref().unwrap_or("local")
    }

    /// Whether this account was set as an administrator by the owner.
    pub fn is_admin(&self, account: u64) -> bool {
        self.admin_qq.contains(&account)
    }
}
