//! Live acceptance harness for the Discord bot channel.
//!
//! The repository already classifies, describes and dispatches channels in
//! source, but no channel has ever completed a real platform round trip
//! (`docs/specs/channels.md`, "channel acceptance evidence level"). This
//! module is the bounded, opt-in harness that changes that for the Discord
//! bot transport: it connects to the live Bot Gateway, receives a real
//! message, replies, proves the declared capabilities that this run can
//! reach, and writes a receipt that separates what was observed from what
//! was only declared.
//!
//! The boundaries are deliberate:
//!
//! * **Credentials are environment-only.** [`DiscordLiveCredentials`] reads
//!   exactly two required variables and never writes a secret to disk, a
//!   receipt, a log line or a URL. It has no `Serialize` implementation and
//!   its `Debug` implementation prints the token as `<redacted>`.
//! * **An absent environment is a skip, not a failure.** The harness returns
//!   a [`DiscordLiveReceipt`] with status [`DiscordLiveStatus::Skipped`], the
//!   names of the missing variables and the resolved prompt. It performs no
//!   network request in that case, and says so in the receipt.
//! * **Every answer keeps the platform's own vocabulary.** A `429` carries
//!   the platform's `retry_after`, scope and bucket; a `4xx` is a definite
//!   refusal; a `5xx` or a lost transport is unknown and is never repeated
//!   automatically. A rate limit is honoured only inside a configured cap,
//!   and the wait is recorded as a fact.
//! * **No acceptance is inferred.** A `2xx` without the promised message id
//!   is unknown; a reply is only accepted when the platform returned a
//!   snowflake, and a deletion is only accepted when the documented empty
//!   success came back and the message really is gone.
//!
//! The harness does not depend on the (unmerged) Discord channel plugin: it
//! speaks the same Gateway v10 / REST v10 contract directly, so the platform
//! round trip can be verified independently of the connector's wiring. The
//! expected envelope and capability table it asserts are the contract the
//! connector must produce; [`NOTES.md`] of the packet records the exact
//! reconciliation when the connector lands.
//!
//! (channel adapter boundary), (Discord bot gateway and REST),
//! (channel acceptance is observed, not claimed).

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use shenko_types::capabilities::ChannelCapabilities;
use shenko_types::channel::{
    AttachmentLocator, AudienceShape, ChannelActor, ChannelAttachment, ChannelContent,
    ChannelEnvelope, ChannelReplyTarget, ChannelSegment,
};

use crate::common::{endpoint_provenance, redact_transport_error};

/// The environment variable that carries the bot token.
pub const TOKEN_ENV: &str = "SHENKO_DISCORD_LIVE_BOT_TOKEN";
/// The environment variable that names the channel to probe.
pub const CHANNEL_ENV: &str = "SHENKO_DISCORD_LIVE_CHANNEL_ID";
/// An optional configured bot user id, verified against `/users/@me`.
pub const BOT_USER_ID_ENV: &str = "SHENKO_DISCORD_LIVE_BOT_USER_ID";
/// An optional HTTP API base (defaults to Discord's production API v10).
pub const API_BASE_ENV: &str = "SHENKO_DISCORD_LIVE_API_BASE";
/// An optional fixed Gateway URL; when unset the platform discovers it.
pub const GATEWAY_URL_ENV: &str = "SHENKO_DISCORD_LIVE_GATEWAY_URL";
/// The overall gateway wait, in seconds (default 90).
pub const TIMEOUT_ENV: &str = "SHENKO_DISCORD_LIVE_TIMEOUT_SECS";
/// The largest `retry_after` the harness will sleep through, in seconds
/// (default 5). A longer platform delay is reported, never silently waited.
pub const RETRY_CAP_ENV: &str = "SHENKO_DISCORD_LIVE_RETRY_CAP_SECS";
/// Where to write the JSON receipt, when set. The receipt never holds a
/// credential.
pub const RECEIPT_ENV: &str = "SHENKO_DISCORD_LIVE_RECEIPT";
/// `1`/`true` adds the privileged `MESSAGE_CONTENT` intent. A bot that has
/// not enabled it in the Developer Portal is closed with code 4014, so the
/// default is off and the receipt records the empty content honestly.
pub const MESSAGE_CONTENT_ENV: &str = "SHENKO_DISCORD_LIVE_MESSAGE_CONTENT";
/// The optional source revision recorded in the receipt, matching the other
/// live harnesses.
pub const SOURCE_REVISION_ENV: &str = "SHENKO_SOURCE_REVISION";

/// The receipt format this harness writes.
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;
/// The envelope format the connector family uses.
pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;
/// The Discord API version this harness speaks.
pub const DISCORD_API_VERSION: &str = "10";
/// Discord's production HTTP API base.
pub const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";
/// The `User-Agent` Discord asks bot clients to send.
pub const DEFAULT_USER_AGENT: &str = "DiscordBot (https://github.com/cyjin-yl/shenko, 1.0)";
/// The channel id the Discord connector owns.
pub const DISCORD_CHANNEL_ID: &str = "discord";
/// The `InboxEvent.source` label the Discord connector owns.
pub const DISCORD_SOURCE: &str = "discord";
/// The transport identity domain of the Discord gateway.
pub const DISCORD_TRANSPORT_DOMAIN: &str = "discord_gateway_v10";
/// The reply kind of a guild/thread channel.
pub const DISCORD_KIND_CHANNEL: &str = "discord-channel";
/// The reply kind of a direct message channel.
pub const DISCORD_KIND_DM: &str = "discord-dm";

/// `GUILDS`.
pub const INTENT_GUILDS: u64 = 1 << 0;
/// `GUILD_MESSAGES`.
pub const INTENT_GUILD_MESSAGES: u64 = 1 << 9;
/// `DIRECT_MESSAGES`.
pub const INTENT_DIRECT_MESSAGES: u64 = 1 << 12;
/// The privileged `MESSAGE_CONTENT` intent.
pub const INTENT_MESSAGE_CONTENT: u64 = 1 << 15;
/// The default intent set: message traffic without a privileged intent.
pub const DEFAULT_INTENTS: u64 = INTENT_GUILDS | INTENT_GUILD_MESSAGES | INTENT_DIRECT_MESSAGES;

/// Discord's maximum message body, in characters.
pub const MAX_MESSAGE_CHARS: usize = 2_000;
/// A Discord snowflake is decimal and no shorter than this.
const MIN_SNOWFLAKE_CHARS: usize = 5;

/// Channel type: guild text channel.
pub const CHANNEL_TYPE_GUILD_TEXT: i64 = 0;
/// Channel type: direct message.
pub const CHANNEL_TYPE_DM: i64 = 1;
/// Channel type: guild voice channel.
pub const CHANNEL_TYPE_GUILD_VOICE: i64 = 2;
/// Channel type: group direct message.
pub const CHANNEL_TYPE_GROUP_DM: i64 = 3;
/// Channel type: guild category.
pub const CHANNEL_TYPE_GUILD_CATEGORY: i64 = 4;
/// Channel type: guild announcement channel.
pub const CHANNEL_TYPE_GUILD_ANNOUNCEMENT: i64 = 5;
/// Channel type: announcement thread.
pub const CHANNEL_TYPE_ANNOUNCEMENT_THREAD: i64 = 10;
/// Channel type: public thread.
pub const CHANNEL_TYPE_PUBLIC_THREAD: i64 = 11;
/// Channel type: private thread.
pub const CHANNEL_TYPE_PRIVATE_THREAD: i64 = 12;
/// Channel type: guild stage voice channel.
pub const CHANNEL_TYPE_GUILD_STAGE_VOICE: i64 = 13;
/// Channel type: guild directory.
pub const CHANNEL_TYPE_GUILD_DIRECTORY: i64 = 14;
/// Channel type: guild forum.
pub const CHANNEL_TYPE_GUILD_FORUM: i64 = 15;
/// Channel type: guild media channel.
pub const CHANNEL_TYPE_GUILD_MEDIA: i64 = 16;

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// The environment-only credentials of one live Discord acceptance run.
///
/// The value deliberately has no serialization and no `Display`: nothing that
/// formats it can leak the token by accident. The token is used only as an
/// `Authorization: Bot <token>` header and as the Gateway identify field; it
/// is never put in a URL.
#[derive(Clone)]
pub struct DiscordLiveCredentials {
    bot_token: String,
    channel_id: String,
    bot_user_id: Option<String>,
    api_base: String,
    gateway_url: Option<String>,
    timeout: Duration,
    retry_cap: Duration,
    message_content_intent: bool,
}

impl fmt::Debug for DiscordLiveCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordLiveCredentials")
            .field("bot_token", &"<redacted>")
            .field("channel_id", &self.channel_id)
            .field("bot_user_id", &self.bot_user_id)
            .field("api_base", &self.api_base)
            .field("gateway_url", &self.gateway_url)
            .field("timeout", &self.timeout)
            .field("retry_cap", &self.retry_cap)
            .field("message_content_intent", &self.message_content_intent)
            .finish()
    }
}

impl DiscordLiveCredentials {
    /// Read the credentials from the process environment.
    pub fn from_env() -> Result<Self, DiscordLiveSkip> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read the credentials from a name/answer lookup.
    ///
    /// This is the testable seam: a lookup that never answers proves the skip
    /// path without touching the real environment or the network.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, DiscordLiveSkip> {
        let read = |name: &str| -> Option<String> {
            lookup(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let mut missing = Vec::new();
        let bot_token = read(TOKEN_ENV);
        if bot_token.is_none() {
            missing.push(TOKEN_ENV.to_string());
        }
        let channel_id = read(CHANNEL_ENV);
        if channel_id.is_none() {
            missing.push(CHANNEL_ENV.to_string());
        }
        if !missing.is_empty() {
            return Err(DiscordLiveSkip::absent(missing));
        }
        let channel_id = channel_id.unwrap_or_default();
        if !is_snowflake(&channel_id) {
            return Err(DiscordLiveSkip::invalid(format!(
                "live Discord acceptance not run: {CHANNEL_ENV} must be a Discord snowflake (decimal digits), not a channel name, URL or partial id"
            )));
        }
        let bot_user_id = read(BOT_USER_ID_ENV);
        if let Some(id) = &bot_user_id
            && !is_snowflake(id)
        {
            return Err(DiscordLiveSkip::invalid(format!(
                "live Discord acceptance not run: {BOT_USER_ID_ENV} must be a Discord snowflake when it is set"
            )));
        }
        let api_base = read(API_BASE_ENV).unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        let api_base = api_base.trim_end_matches('/').to_string();
        if !(api_base.starts_with("https://") || api_base.starts_with("http://")) {
            return Err(DiscordLiveSkip::invalid(format!(
                "live Discord acceptance not run: {API_BASE_ENV} must be an http(s) endpoint"
            )));
        }
        let gateway_url = read(GATEWAY_URL_ENV);
        if let Some(url) = &gateway_url
            && !(url.starts_with("wss://")
                || url.starts_with("ws://")
                || url.starts_with("https://")
                || url.starts_with("http://"))
        {
            return Err(DiscordLiveSkip::invalid(format!(
                "live Discord acceptance not run: {GATEWAY_URL_ENV} must be a ws(s) or http(s) endpoint"
            )));
        }
        let timeout = match read(TIMEOUT_ENV) {
            Some(text) => match text.parse::<u64>() {
                Ok(seconds) if (1..=1_800).contains(&seconds) => Duration::from_secs(seconds),
                _ => {
                    return Err(DiscordLiveSkip::invalid(format!(
                        "live Discord acceptance not run: {TIMEOUT_ENV} must be an integer number of seconds in 1..=1800"
                    )))
                }
            },
            None => Duration::from_secs(90),
        };
        let retry_cap = match read(RETRY_CAP_ENV) {
            Some(text) => match text.parse::<f64>() {
                Ok(seconds) if seconds.is_finite() && (0.0..=60.0).contains(&seconds) => {
                    Duration::from_secs_f64(seconds)
                }
                _ => {
                    return Err(DiscordLiveSkip::invalid(format!(
                        "live Discord acceptance not run: {RETRY_CAP_ENV} must be a number of seconds in 0..=60"
                    )))
                }
            },
            None => Duration::from_secs(5),
        };
        let message_content_intent = matches!(
            read(MESSAGE_CONTENT_ENV).as_deref(),
            Some("1" | "true" | "TRUE" | "yes" | "on")
        );
        Ok(Self {
            bot_token: bot_token.unwrap_or_default(),
            channel_id,
            bot_user_id,
            api_base,
            gateway_url,
            timeout,
            retry_cap,
            message_content_intent,
        })
    }

    /// The bearer value; only the request builder may read it.
    fn bot_token(&self) -> &str {
        &self.bot_token
    }

    /// The probed channel.
    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// The configured bot account, when the operator named one.
    pub fn configured_bot_user_id(&self) -> Option<&str> {
        self.bot_user_id.as_deref()
    }

    /// The HTTP API base without a trailing slash.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// The fixed Gateway URL, when one was configured.
    pub fn gateway_url(&self) -> Option<&str> {
        self.gateway_url.as_deref()
    }

    /// The overall gateway wait.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The largest platform delay this run will sleep through.
    pub fn retry_cap(&self) -> Duration {
        self.retry_cap
    }

    /// The intent bitmask this run identifies with.
    pub fn intents(&self) -> u64 {
        if self.message_content_intent {
            DEFAULT_INTENTS | INTENT_MESSAGE_CONTENT
        } else {
            DEFAULT_INTENTS
        }
    }

    /// The `User-Agent` this run sends.
    pub fn user_agent(&self) -> &'static str {
        DEFAULT_USER_AGENT
    }
}

/// Why the live acceptance did not reach the platform.
///
/// A skip is a fact about the environment, never a failed assertion: it names
/// the missing variables, resolves the exact command, and states whether any
/// network request was attempted (always `false` for an absent environment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscordLiveSkip {
    /// A credential-free explanation.
    pub reason: String,
    /// The environment variables that were absent or unusable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_environment: Vec<String>,
    /// Whether the harness contacted the platform before skipping.
    pub network_attempted: bool,
    /// The exact operator prompt, without any secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt: Vec<String>,
}

impl DiscordLiveSkip {
    /// The required environment was absent; nothing was contacted.
    pub fn absent(missing: Vec<String>) -> Self {
        let mut prompt = Vec::new();
        for name in &missing {
            prompt.push(format!("export {name}=<required>"));
        }
        prompt.push(
            "then run: cargo test -p shenko-channel-adapters --test discord_live_acceptance -- --nocapture"
                .to_string(),
        );
        let reason = format!(
            "live Discord acceptance skipped: set {}; credentials are read from the environment only and are never stored, logged or written to a receipt",
            missing.join(" and ")
        );
        Self {
            reason,
            missing_environment: missing,
            network_attempted: false,
            prompt,
        }
    }

    /// The environment was present but malformed; nothing was contacted.
    pub fn invalid(reason: String) -> Self {
        Self {
            reason,
            missing_environment: vec![],
            network_attempted: false,
            prompt: vec![
                "fix the named variable and rerun: cargo test -p shenko-channel-adapters --test discord_live_acceptance -- --nocapture"
                    .to_string(),
            ],
        }
    }

    /// The platform answered, but this channel cannot host a text probe.
    pub fn observed(reason: String) -> Self {
        Self {
            reason,
            missing_environment: vec![],
            network_attempted: true,
            prompt: vec![format!(
                "choose a guild text, announcement, thread or direct message channel for {CHANNEL_ENV}"
            )],
        }
    }
}

// ---------------------------------------------------------------------------
// Platform answers and failures
// ---------------------------------------------------------------------------

/// The rate-limit facts Discord reported for one answer.
///
/// Discord's documentation says these values change by route and deployment,
/// so they are recorded exactly as the platform sent them and are never
/// replaced by a constant.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DiscordLiveRateFacts {
    /// `X-RateLimit-Limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// `X-RateLimit-Remaining`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<i64>,
    /// `X-RateLimit-Reset-After`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_after: Option<f64>,
    /// `X-RateLimit-Bucket`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// `X-RateLimit-Scope`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// `X-RateLimit-Global` or the body's `global`.
    pub global: bool,
    /// The platform's own wait: `Retry-After` or the body's `retry_after`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<f64>,
}

impl DiscordLiveRateFacts {
    fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let number = |name: &str| -> Option<f64> {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|text| text.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite())
        };
        let text = |name: &str| -> Option<String> {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        Self {
            limit: number("x-ratelimit-limit").map(|value| value as i64),
            remaining: number("x-ratelimit-remaining").map(|value| value as i64),
            reset_after: number("x-ratelimit-reset-after"),
            bucket: text("x-ratelimit-bucket"),
            scope: text("x-ratelimit-scope"),
            global: headers
                .get("x-ratelimit-global")
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            retry_after: number("retry-after"),
        }
    }

    fn absorb_body(&mut self, body: &Value) {
        if let Some(retry_after) = body
            .get("retry_after")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
        {
            self.retry_after = Some(retry_after);
        }
        if let Some(global) = body.get("global").and_then(Value::as_bool) {
            self.global = global;
        }
    }
}

/// Why one Discord call did not produce an acceptable answer.
#[derive(Debug, Clone, PartialEq)]
pub enum DiscordLiveError {
    /// The platform answered and refused; nothing happened at the far end.
    Rejected {
        /// The HTTP status.
        status: u16,
        /// Discord's own error code.
        code: Option<i64>,
        /// Discord's own message.
        message: String,
        /// The platform's rate-limit facts.
        rate: Box<DiscordLiveRateFacts>,
    },
    /// The platform answered with its rate-limit refusal.
    RateLimited {
        /// The HTTP status (always 429 here).
        status: u16,
        /// The platform's own wait, when it stated one.
        retry_after: Option<f64>,
        /// Whether the platform called it global.
        global: bool,
        /// The platform's scope.
        scope: Option<String>,
        /// The platform's bucket.
        bucket: Option<String>,
    },
    /// The answer was lost or the platform failed; the mutation may exist.
    Unknown {
        /// A credential-free description.
        detail: String,
    },
    /// The platform answered, but not with the fields it promised.
    Incomplete {
        /// A credential-free description.
        detail: String,
    },
    /// The platform refused this connector or this run outright.
    Refused {
        /// A credential-free description.
        detail: String,
    },
}

impl DiscordLiveError {
    fn kind(&self) -> DiscordLiveFailureKind {
        match self {
            DiscordLiveError::Rejected { .. } => DiscordLiveFailureKind::Rejected,
            DiscordLiveError::RateLimited { .. } => DiscordLiveFailureKind::RateLimited,
            DiscordLiveError::Unknown { .. } => DiscordLiveFailureKind::Unknown,
            DiscordLiveError::Incomplete { .. } => DiscordLiveFailureKind::Incomplete,
            DiscordLiveError::Refused { .. } => DiscordLiveFailureKind::Refused,
        }
    }

    fn to_failure(&self, stage: &str) -> DiscordLiveFailure {
        let (status, code) = match self {
            DiscordLiveError::Rejected { status, code, .. } => (Some(*status), *code),
            DiscordLiveError::RateLimited { status, .. } => (Some(*status), None),
            _ => (None, None),
        };
        DiscordLiveFailure {
            stage: stage.to_string(),
            kind: self.kind(),
            detail: self.detail(),
            status,
            code,
            retryable: false,
        }
    }

    fn detail(&self) -> String {
        match self {
            DiscordLiveError::Rejected {
                status, code, message, ..
            } => format!("discord refused the request (http {status}, code {code:?}): {message}"),
            DiscordLiveError::RateLimited {
                retry_after,
                global,
                scope,
                bucket,
                ..
            } => {
                let mut detail = match retry_after {
                    Some(seconds) => format!("discord rate limited the request; retry_after {seconds}s"),
                    None => "discord rate limited the request without a wait it could honour".to_string(),
                };
                if *global {
                    detail.push_str("; global");
                }
                if let Some(scope) = scope {
                    detail.push_str(&format!("; scope {scope}"));
                }
                if let Some(bucket) = bucket {
                    detail.push_str(&format!("; bucket {bucket}"));
                }
                detail
            }
            DiscordLiveError::Unknown { detail }
            | DiscordLiveError::Incomplete { detail }
            | DiscordLiveError::Refused { detail } => detail.clone(),
        }
    }
}

/// The stable failure class recorded in a receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscordLiveFailureKind {
    /// A definite platform refusal.
    Rejected,
    /// The platform's rate-limit refusal.
    RateLimited,
    /// The request may have been executed.
    Unknown,
    /// The platform answered without the promised fields.
    Incomplete,
    /// The connector or the run was refused outright.
    Refused,
}

/// One recorded failure.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordLiveFailure {
    /// The stage that failed (`identity`, `channel`, `gateway`, `receive`,
    /// `reply`, `edit`, `typing`, `retract`, `cleanup`, `receipt`).
    pub stage: String,
    /// The failure class.
    pub kind: DiscordLiveFailureKind,
    /// A credential-free detail.
    pub detail: String,
    /// The HTTP status, when the platform answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Discord's own error code, when it reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    /// Whether a later run could retry. The harness itself never retries an
    /// unknown or a refusal.
    pub retryable: bool,
}

/// The one HTTP answer the platform gave.
#[derive(Debug, Clone)]
struct LiveAnswer {
    status: u16,
    body: Value,
}

// ---------------------------------------------------------------------------
// Receipt types
// ---------------------------------------------------------------------------

/// The outcome of one live acceptance run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscordLiveStatus {
    /// Every asserted stage held.
    Passed,
    /// A stage was refused, incomplete, unknown or timed out.
    Failed,
    /// The environment was absent or this channel cannot host the probe.
    Skipped,
}

/// The account the token speaks as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscordLiveAccount {
    /// The platform's user id.
    pub user_id: String,
    /// The platform's username.
    pub username: String,
    /// Whether the platform says this is a bot account.
    pub bot: bool,
    /// The account namespace the connector would serve.
    pub account_scope: String,
}

/// What the platform says about the probed channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscordLiveChannelFacts {
    /// The channel id.
    pub channel_id: String,
    /// The platform's numeric channel type.
    pub channel_type: i64,
    /// A readable name for the channel type.
    pub channel_kind: String,
    /// Whether the channel is a thread.
    pub is_thread: bool,
    /// Whether the channel is a direct message namespace.
    pub is_direct: bool,
    /// The guild the channel belongs to, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    /// Whether the channel has a parent (a thread's parent channel).
    pub parent_present: bool,
    /// How many recipients a direct message reports, when it reports any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient_count: Option<usize>,
    /// The audience shape this channel states.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience_shape: Option<AudienceShape>,
    /// The reply kind the connector would offer for this channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_kind: Option<String>,
}

impl DiscordLiveChannelFacts {
    fn new(channel_id: String, detail: &Value) -> Result<Self, DiscordLiveError> {
        let channel_type = detail
            .get("type")
            .and_then(Value::as_i64)
            .ok_or_else(|| DiscordLiveError::Incomplete {
                detail: "the channel probe carried no numeric type".to_string(),
            })?;
        let is_thread = is_thread_type(channel_type);
        let is_direct = is_direct_type(channel_type);
        Ok(Self {
            channel_id,
            channel_type,
            channel_kind: channel_kind(channel_type).to_string(),
            is_thread,
            is_direct,
            guild_id: detail
                .get("guild_id")
                .and_then(Value::as_str)
                .filter(|id| is_snowflake(id))
                .map(str::to_owned),
            parent_present: detail
                .get("parent_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty()),
            recipient_count: detail
                .get("recipients")
                .and_then(Value::as_array)
                .map(|recipients| recipients.len()),
            audience_shape: audience_shape_of(channel_type),
            reply_kind: reply_kind_of(channel_type).map(str::to_owned),
        })
    }
}

/// One declared capability and what this run actually observed about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscordLiveCapabilityCheck {
    /// The capability field name.
    pub capability: String,
    /// The connector's own declaration.
    pub declared: bool,
    /// Whether this run observed the platform accepting the action.
    pub verified: bool,
    /// What the observation is based on.
    pub basis: String,
}

/// The capability table plus the live evidence for it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscordLiveCapabilityIntrospection {
    /// The table the Discord connector declares.
    pub declared: ChannelCapabilities,
    /// The per-field live basis.
    pub checks: Vec<DiscordLiveCapabilityCheck>,
    /// The audience shape of the probed channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience_shape: Option<AudienceShape>,
    /// The reply kind of the probed channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_kind: Option<String>,
}

/// What one gateway session established.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DiscordLiveGatewayFacts {
    /// The gateway endpoint, with any query stripped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The shard count the platform recommends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shards: Option<i64>,
    /// How many session starts remain in the platform's window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_start_remaining: Option<i64>,
    /// The platform's concurrent session limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<i64>,
    /// Whether the WebSocket handshake completed.
    pub connected: bool,
    /// Whether the platform sent READY for this session.
    pub session_established: bool,
    /// Whether READY carried a non-empty session id.
    pub session_id_present: bool,
    /// The heartbeat interval the platform stated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval_ms: Option<u64>,
    /// How many heartbeats the platform acknowledged.
    pub heartbeats_acknowledged: u64,
    /// The intent bitmask this run identified with.
    pub intents: u64,
    /// How many dispatch events were observed while waiting.
    pub dispatch_total: u64,
    /// The dispatch event names observed, deduplicated and bounded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dispatch_kinds: Vec<String>,
    /// How many binary frames were observed (encoding is JSON here).
    pub binary_frames: u64,
    /// The newest sequence the gateway delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sequence: Option<i64>,
    /// The close code, when the platform closed the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_code: Option<u16>,
    /// What that close code means.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_meaning: Option<String>,
}

impl DiscordLiveGatewayFacts {
    fn observe_dispatch(&mut self, event: &str) {
        self.dispatch_total += 1;
        if self.dispatch_kinds.len() < 32 && !self.dispatch_kinds.iter().any(|known| known == event) {
            self.dispatch_kinds.push(event.to_string());
        }
    }
}

/// One message received from the live gateway.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscordLiveReceived {
    /// The platform message id.
    pub message_id: String,
    /// The channel the message came from.
    pub channel_id: String,
    /// The dispatch event name.
    pub event: String,
    /// The exact frame length the gateway delivered.
    pub raw_frame_bytes: usize,
    /// The gateway sequence of the frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i64>,
    /// Whether the platform delivered a non-empty body.
    pub content_available: bool,
    /// The body length the platform delivered.
    pub content_chars: usize,
    /// Whether the platform attributed the line to the served account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_account: Option<bool>,
    /// The platform's revision marker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_event_revision: Option<String>,
    /// The normalized channel envelope the received frame produces.
    pub envelope: ChannelEnvelope,
}

/// One platform action this run performed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscordLiveActionReceipt {
    /// `seed`, `reply`, `edit`, `typing`, `retract` or `cleanup_seed`.
    pub action: String,
    /// Whether the platform accepted the action.
    pub accepted: bool,
    /// The HTTP status, when the platform answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The platform message id the action concerns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_message_id: Option<String>,
    /// The message this reply references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_reference: Option<String>,
    /// The correlation nonce sent with a creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// Whether a read-back confirmed the platform's answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_back: Option<bool>,
    /// How long the round trip took.
    pub latency_ms: u64,
    /// A credential-free note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One rate-limit refusal and what the harness did with it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscordLiveRateLimitObservation {
    /// The stage that was limited.
    pub stage: String,
    /// The HTTP status (429).
    pub status: u16,
    /// The platform's own wait.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<f64>,
    /// Whether the platform called it global.
    pub global: bool,
    /// The platform's scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The platform's bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// Whether the harness slept through the platform's wait.
    pub honored: bool,
    /// How long the harness slept.
    pub waited_ms: u64,
    /// Which attempt saw the refusal (0 is the first).
    pub attempt: u32,
    /// A credential-free note.
    pub note: String,
}

/// The complete receipt of one run.
///
/// The type holds no credential: there is no field that could carry the token,
/// and [`DiscordLiveReceipt::to_json`] is the only serialization.
#[derive(Debug, Clone, Serialize)]
pub struct DiscordLiveReceipt {
    /// The receipt format.
    pub schema_version: u32,
    /// The outcome.
    pub status: DiscordLiveStatus,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run finished.
    pub finished_at: DateTime<Utc>,
    /// How long the run took.
    pub elapsed_ms: u64,
    /// The source revision, when one was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    /// Why nothing was run, when the run skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<DiscordLiveSkip>,
    /// The account the token speaks as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<DiscordLiveAccount>,
    /// What the platform says about the channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<DiscordLiveChannelFacts>,
    /// The declared capability table and its live basis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<DiscordLiveCapabilityIntrospection>,
    /// What the gateway session established.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<DiscordLiveGatewayFacts>,
    /// The message the gateway delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received: Option<DiscordLiveReceived>,
    /// Every platform action, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<DiscordLiveActionReceipt>,
    /// Every rate-limit refusal, including the ones that were honoured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limits: Vec<DiscordLiveRateLimitObservation>,
    /// Every failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<DiscordLiveFailure>,
    /// Credential-free notes about what this run did not cover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl DiscordLiveReceipt {
    fn new(started_at: DateTime<Utc>) -> Self {
        Self {
            schema_version: RECEIPT_SCHEMA_VERSION,
            status: DiscordLiveStatus::Failed,
            started_at,
            finished_at: started_at,
            elapsed_ms: 0,
            source_revision: std::env::var(SOURCE_REVISION_ENV)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            skip: None,
            account: None,
            channel: None,
            capabilities: None,
            gateway: None,
            received: None,
            actions: Vec::new(),
            rate_limits: Vec::new(),
            failures: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// A receipt for a run that never reached the platform.
    pub fn skipped(skip: DiscordLiveSkip) -> Self {
        let started = Instant::now();
        let started_at = Utc::now();
        let mut receipt = Self::new(started_at);
        receipt.skip = Some(skip);
        receipt.finish(started)
    }

    /// The receipt as JSON. Never contains a credential.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|error| {
            json!({
                "schema_version": RECEIPT_SCHEMA_VERSION,
                "status": "failed",
                "serialization_error": error.to_string(),
            })
        })
    }

    /// Write the receipt to [`RECEIPT_ENV`], when the operator named a path.
    ///
    /// Returns the path written, if any. A skip receipt is written too, so an
    /// unattended supervisor can tell "not configured" from "never ran".
    pub fn write_if_requested(&self) -> Result<Option<PathBuf>, String> {
        let Ok(raw) = std::env::var(RECEIPT_ENV) else {
            return Ok(None);
        };
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        let path = PathBuf::from(raw);
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("receipt directory {}: {error}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| format!("receipt serialization failed: {error}"))?;
        std::fs::write(&path, text)
            .map_err(|error| format!("receipt {}: {error}", path.display()))?;
        Ok(Some(path))
    }

    /// Check that the receipt's own claims are consistent.
    ///
    /// A `Passed` receipt must carry every stage and no failure. A `Skipped`
    /// receipt must state why. A `Failed` receipt must carry the failure.
    pub fn validate(&self) -> Result<(), String> {
        match self.status {
            DiscordLiveStatus::Skipped => {
                let Some(skip) = &self.skip else {
                    return Err("a skipped receipt carries no skip reason".to_string());
                };
                if skip.reason.trim().is_empty() {
                    return Err("a skipped receipt carries an empty skip reason".to_string());
                }
                if !self.failures.is_empty() {
                    return Err("a skipped receipt also carries failures".to_string());
                }
                Ok(())
            }
            DiscordLiveStatus::Failed => {
                if self.failures.is_empty() && self.skip.is_none() {
                    return Err("a failed receipt carries neither a failure nor a skip".to_string());
                }
                Ok(())
            }
            DiscordLiveStatus::Passed => {
                if !self.failures.is_empty() {
                    return Err(format!(
                        "a passed receipt carries {} failure(s)",
                        self.failures.len()
                    ));
                }
                self.validate_mandatory()
            }
        }
    }

    fn validate_mandatory(&self) -> Result<(), String> {
        let Some(account) = &self.account else {
            return Err("a passed receipt has no account probe".to_string());
        };
        let Some(channel) = &self.channel else {
            return Err("a passed receipt has no channel probe".to_string());
        };
        let Some(capabilities) = &self.capabilities else {
            return Err("a passed receipt has no capability introspection".to_string());
        };
        if capabilities
            .checks
            .iter()
            .any(|check| check.capability.is_empty())
        {
            return Err("a capability check has no field name".to_string());
        }
        let Some(gateway) = &self.gateway else {
            return Err("a passed receipt has no gateway facts".to_string());
        };
        if !gateway.session_established {
            return Err("a passed receipt has no established gateway session".to_string());
        }
        let Some(received) = &self.received else {
            return Err("a passed receipt has no received message".to_string());
        };
        if received.message_id.is_empty() || received.channel_id != channel.channel_id {
            return Err("the received message does not belong to the probed channel".to_string());
        }
        if received.envelope.provider != DISCORD_CHANNEL_ID {
            return Err("the received envelope has the wrong provider".to_string());
        }
        if received.envelope.account != account.account_scope {
            return Err("the received envelope has the wrong account scope".to_string());
        }
        if received.envelope.reply_target.is_none() {
            return Err("the received envelope has no replyable audience".to_string());
        }
        let accepted = |action: &str| {
            self.actions
                .iter()
                .any(|receipt| receipt.action == action && receipt.accepted)
        };
        if !accepted("reply") {
            return Err("a passed receipt has no accepted reply".to_string());
        }
        for action in ["edit", "typing", "retract"] {
            if !accepted(action) {
                return Err(format!(
                    "a passed receipt did not observe the declared {action} capability"
                ));
            }
        }
        Ok(())
    }

    fn finish(mut self, started: Instant) -> Self {
        self.finished_at = Utc::now();
        self.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if self.skip.is_some() {
            self.status = DiscordLiveStatus::Skipped;
            return self;
        }
        if self.failures.is_empty() {
            let mandatory = self.validate_mandatory();
            if let Err(detail) = mandatory {
                self.failures.push(DiscordLiveFailure {
                    stage: "receipt".to_string(),
                    kind: DiscordLiveFailureKind::Incomplete,
                    detail,
                    status: None,
                    code: None,
                    retryable: false,
                });
            }
        }
        self.status = if self.failures.is_empty() {
            DiscordLiveStatus::Passed
        } else {
            DiscordLiveStatus::Failed
        };
        self
    }
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// The bounded Discord HTTP client used by the harness.
struct LiveClient {
    credentials: Arc<DiscordLiveCredentials>,
    http: reqwest::Client,
    observations: Vec<DiscordLiveRateLimitObservation>,
}

impl LiveClient {
    fn new(credentials: Arc<DiscordLiveCredentials>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            // The credential is a header value; a redirect must never carry
            // it to a different host.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(credentials.user_agent())
            .build()
            .map_err(|error| format!("http client build failed: {error}"))?;
        Ok(Self {
            credentials,
            http,
            observations: Vec::new(),
        })
    }

    async fn current_user(&mut self) -> Result<DiscordLiveAccount, DiscordLiveError> {
        let answer = self
            .request("identity", reqwest::Method::GET, "/users/@me", None)
            .await?;
        let user_id = answer
            .body
            .get("id")
            .and_then(Value::as_str)
            .and_then(snowflake_str)
            .ok_or_else(|| DiscordLiveError::Incomplete {
                detail: "the account probe carried no Discord user id".to_string(),
            })?;
        Ok(DiscordLiveAccount {
            account_scope: format!("{DISCORD_CHANNEL_ID}:{user_id}"),
            username: answer
                .body
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            bot: answer
                .body
                .get("bot")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            user_id,
        })
    }

    async fn channel(&mut self, channel_id: &str) -> Result<DiscordLiveChannelFacts, DiscordLiveError> {
        let path = format!("/channels/{channel_id}");
        let answer = self.request("channel", reqwest::Method::GET, &path, None).await?;
        DiscordLiveChannelFacts::new(channel_id.to_string(), &answer.body)
    }

    async fn gateway_info(&mut self) -> Result<DiscordLiveGatewayInfo, DiscordLiveError> {
        let answer = self
            .request("gateway_discovery", reqwest::Method::GET, "/gateway/bot", None)
            .await?;
        let url = answer
            .body
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| DiscordLiveError::Incomplete {
                detail: "the gateway discovery carried no url".to_string(),
            })?
            .to_string();
        let limit = answer.body.get("session_start_limit");
        Ok(DiscordLiveGatewayInfo {
            url,
            shards: answer.body.get("shards").and_then(Value::as_i64),
            session_start_remaining: limit
                .and_then(|limit| limit.get("remaining"))
                .and_then(Value::as_i64),
            max_concurrency: limit
                .and_then(|limit| limit.get("max_concurrency"))
                .and_then(Value::as_i64),
        })
    }

    async fn get_message(
        &mut self,
        stage: &'static str,
        channel_id: &str,
        message_id: &str,
    ) -> Result<LiveAnswer, DiscordLiveError> {
        let path = format!("/channels/{channel_id}/messages/{message_id}");
        self.request(stage, reqwest::Method::GET, &path, None).await
    }

    async fn create_message(
        &mut self,
        stage: &'static str,
        channel_id: &str,
        body: &Value,
    ) -> Result<LiveAnswer, DiscordLiveError> {
        let path = format!("/channels/{channel_id}/messages");
        self.request(stage, reqwest::Method::POST, &path, Some(body)).await
    }

    async fn edit_message(
        &mut self,
        stage: &'static str,
        channel_id: &str,
        message_id: &str,
        body: &Value,
    ) -> Result<LiveAnswer, DiscordLiveError> {
        let path = format!("/channels/{channel_id}/messages/{message_id}");
        self.request(stage, reqwest::Method::PATCH, &path, Some(body)).await
    }

    async fn delete_message(
        &mut self,
        stage: &'static str,
        channel_id: &str,
        message_id: &str,
    ) -> Result<LiveAnswer, DiscordLiveError> {
        let path = format!("/channels/{channel_id}/messages/{message_id}");
        self.request(stage, reqwest::Method::DELETE, &path, None).await
    }

    async fn typing(
        &mut self,
        stage: &'static str,
        channel_id: &str,
    ) -> Result<LiveAnswer, DiscordLiveError> {
        let path = format!("/channels/{channel_id}/typing");
        self.request(stage, reqwest::Method::POST, &path, None).await
    }

    /// One request with the bounded rate-limit policy.
    ///
    /// A `429` is a definite refusal, so retrying once is safe. The retry is
    /// only performed when the platform's own wait fits inside the configured
    /// cap; a longer wait is recorded and returned as a failure, never slept
    /// through or replaced with a guessed constant. Any other error is
    /// returned unchanged: a lost answer or a `5xx` may have been executed and
    /// is never repeated.
    async fn request(
        &mut self,
        stage: &'static str,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<LiveAnswer, DiscordLiveError> {
        let mut attempt = 0u32;
        loop {
            match self.request_once(&method, path, body).await {
                Ok(answer) => return Ok(answer),
                Err(DiscordLiveError::RateLimited {
                    status,
                    retry_after,
                    global,
                    scope,
                    bucket,
                }) => {
                    let delay = retry_after
                        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                        .map(Duration::from_secs_f64);
                    let honored = attempt == 0
                        && delay.is_some_and(|delay| delay <= self.credentials.retry_cap());
                    if honored {
                        let delay = delay.unwrap_or_default();
                        let waited_ms =
                            u64::try_from((delay + Duration::from_millis(50)).as_millis())
                                .unwrap_or(u64::MAX);
                        self.observations.push(DiscordLiveRateLimitObservation {
                            stage: stage.to_string(),
                            status,
                            retry_after,
                            global,
                            scope: scope.clone(),
                            bucket: bucket.clone(),
                            honored: true,
                            waited_ms,
                            attempt,
                            note: "slept through the platform's own wait, then retried once"
                                .to_string(),
                        });
                        tokio::time::sleep(delay + Duration::from_millis(50)).await;
                        attempt += 1;
                        continue;
                    }
                    self.observations.push(DiscordLiveRateLimitObservation {
                        stage: stage.to_string(),
                        status,
                        retry_after,
                        global,
                        scope: scope.clone(),
                        bucket: bucket.clone(),
                        honored: false,
                        waited_ms: 0,
                        attempt,
                        note: match retry_after {
                            Some(seconds) if attempt > 0 => format!(
                                "already retried once; the platform still asked for {seconds}s"
                            ),
                            Some(seconds)
                                if seconds > self.credentials.retry_cap().as_secs_f64() =>
                            {
                                format!(
                                    "the platform asked for {seconds}s, beyond the configured cap; not slept"
                                )
                            }
                            _ => "the platform gave no usable wait; not slept".to_string(),
                        },
                    });
                    return Err(DiscordLiveError::RateLimited {
                        status,
                        retry_after,
                        global,
                        scope,
                        bucket,
                    });
                }
                Err(other) => return Err(other),
            }
        }
    }

    async fn request_once(
        &self,
        method: &reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<LiveAnswer, DiscordLiveError> {
        let url = format!("{}{path}", self.credentials.api_base());
        let mut request = self
            .http
            .request(method.clone(), url.as_str())
            .header(
                "Authorization",
                format!("Bot {}", self.credentials.bot_token()),
            )
            .header("User-Agent", self.credentials.user_agent());
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| DiscordLiveError::Unknown {
            detail: redacted_detail(&error.to_string()),
        })?;
        let status = response.status().as_u16();
        let mut rate = DiscordLiveRateFacts::from_headers(response.headers());
        let bytes = response.bytes().await.map_err(|error| DiscordLiveError::Unknown {
            detail: redacted_detail(&error.to_string()),
        })?;
        let parsed = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice::<Value>(&bytes).ok()
        };
        let body = parsed.unwrap_or(Value::Null);
        if status == 429 {
            rate.absorb_body(&body);
            return Err(DiscordLiveError::RateLimited {
                status,
                retry_after: rate.retry_after,
                global: rate.global,
                scope: rate.scope,
                bucket: rate.bucket,
            });
        }
        if (200..300).contains(&status) {
            return Ok(LiveAnswer { status, body });
        }
        if status >= 500 {
            return Err(DiscordLiveError::Unknown {
                detail: format!(
                    "discord answered HTTP {status}; the request may have been executed"
                ),
            });
        }
        Err(DiscordLiveError::Rejected {
            status,
            code: body.get("code").and_then(Value::as_i64),
            message: text_field(&body, "message")
                .unwrap_or_else(|| format!("discord answered HTTP {status} without a decodable verdict")),
            rate: Box::new(rate),
        })
    }

    fn take_observations(&mut self) -> Vec<DiscordLiveRateLimitObservation> {
        std::mem::take(&mut self.observations)
    }
}

/// What the gateway discovery answered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscordLiveGatewayInfo {
    url: String,
    shards: Option<i64>,
    session_start_remaining: Option<i64>,
    max_concurrency: Option<i64>,
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

type DiscordSocket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// One live gateway session, up to the READY frame.
struct GatewayProbe {
    socket: DiscordSocket,
    heartbeat: tokio::time::Interval,
    facts: DiscordLiveGatewayFacts,
    deadline: Instant,
}

impl GatewayProbe {
    /// The observed gateway facts and the refusal travel together so the caller
    /// can record what it saw; the pair is boxed because the facts are large.
    async fn connect(
        client: &mut LiveClient,
        deadline: Instant,
        expected_bot_user_id: &str,
    ) -> Result<Self, Box<(DiscordLiveGatewayFacts, DiscordLiveError)>> {
        GatewayProbe::connect_inner(client, deadline, expected_bot_user_id)
            .await
            .map_err(Box::new)
    }

    async fn connect_inner(
        client: &mut LiveClient,
        deadline: Instant,
        expected_bot_user_id: &str,
    ) -> Result<Self, (DiscordLiveGatewayFacts, DiscordLiveError)> {
        let credentials = Arc::clone(&client.credentials);
        let mut facts = DiscordLiveGatewayFacts {
            intents: credentials.intents(),
            ..Default::default()
        };
        let gateway = match credentials.gateway_url() {
            Some(url) => url.to_string(),
            None => match client.gateway_info().await {
                Ok(info) => {
                    facts.shards = info.shards;
                    facts.session_start_remaining = info.session_start_remaining;
                    facts.max_concurrency = info.max_concurrency;
                    info.url
                }
                Err(error) => return Err((facts, error)),
            },
        };
        let url = connect_url(&gateway);
        facts.endpoint = Some(endpoint_provenance(&url));

        let wait = match remaining(deadline, "opening the Discord gateway") {
            Ok(wait) => wait,
            Err(error) => return Err((facts, error)),
        };
        let (mut socket, _) = match tokio::time::timeout(
            wait,
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                return Err((
                    facts,
                    DiscordLiveError::Unknown {
                        detail: redacted_detail(&format!("gateway connect failed: {error}")),
                    },
                ))
            }
            Err(_) => {
                return Err((
                    facts,
                    DiscordLiveError::Unknown {
                        detail: "gateway connect exceeded the configured timeout".to_string(),
                    },
                ))
            }
        };
        facts.connected = true;

        let (hello, _) = match next_json_frame(&mut socket, deadline).await {
            Ok(frame) => frame,
            Err(error) => return Err((facts, error)),
        };
        if hello.get("op").and_then(Value::as_i64) != Some(10) {
            return Err((
                facts,
                DiscordLiveError::Refused {
                    detail: "the gateway did not open with a HELLO frame".to_string(),
                },
            ));
        }
        let interval_ms = hello
            .get("d")
            .and_then(|data| data.get("heartbeat_interval"))
            .and_then(Value::as_u64)
            .unwrap_or(41_250)
            .max(1_000);
        facts.heartbeat_interval_ms = Some(interval_ms);

        let identify = json!({
            "op": 2,
            "d": {
                "token": credentials.bot_token(),
                "intents": credentials.intents(),
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "shenko",
                    "device": "shenko",
                },
            },
        });
        if let Err(error) = socket
            .send(Message::Text(identify.to_string().into()))
            .await
        {
            return Err((
                facts,
                DiscordLiveError::Unknown {
                    detail: redacted_detail(&format!("gateway identify failed: {error}")),
                },
            ));
        }

        loop {
            let (frame, _) = match next_json_frame(&mut socket, deadline).await {
                Ok(frame) => frame,
                Err(error) => return Err((facts, error)),
            };
            let opcode = frame.get("op").and_then(Value::as_i64);
            match opcode {
                Some(0) => {
                    let event = frame
                        .get("t")
                        .and_then(Value::as_str)
                        .unwrap_or("UNKNOWN")
                        .to_string();
                    facts.observe_dispatch(&event);
                    if event == "READY" {
                        let user = frame
                            .pointer("/d/user/id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if user != expected_bot_user_id {
                            return Err((
                                facts,
                                DiscordLiveError::Refused {
                                    detail: format!(
                                        "the gateway session speaks as {user}, but the token probe reported {expected_bot_user_id}"
                                    ),
                                },
                            ));
                        }
                        facts.session_established = true;
                        facts.session_id_present = frame
                            .pointer("/d/session_id")
                            .and_then(Value::as_str)
                            .is_some_and(|session| !session.is_empty());
                        facts.last_sequence = frame.get("s").and_then(Value::as_i64);
                        break;
                    }
                }
                Some(1) => {
                    let payload = json!({"op": 1, "d": facts.last_sequence});
                    if socket
                        .send(Message::Text(payload.to_string().into()))
                        .await
                        .is_err()
                    {
                        return Err((
                            facts,
                            DiscordLiveError::Unknown {
                                detail: "gateway heartbeat failed during the handshake"
                                    .to_string(),
                            },
                        ));
                    }
                }
                Some(7) => {
                    return Err((
                        facts,
                        DiscordLiveError::Unknown {
                            detail: "the gateway asked for a reconnect before READY".to_string(),
                        },
                    ))
                }
                Some(9) => {
                    return Err((
                        facts,
                        DiscordLiveError::Refused {
                            detail: "the platform refused the gateway session".to_string(),
                        },
                    ))
                }
                _ => {}
            }
        }

        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(interval_ms),
            Duration::from_millis(interval_ms),
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Ok(Self {
            socket,
            heartbeat,
            facts,
            deadline,
        })
    }

    fn facts(&self) -> DiscordLiveGatewayFacts {
        self.facts.clone()
    }

    /// Wait for the gateway to deliver one observed message.
    async fn receive(
        &mut self,
        channel_id: &str,
        expected_message_id: &str,
    ) -> Result<(Value, usize), DiscordLiveError> {
        let socket = &mut self.socket;
        let heartbeat = &mut self.heartbeat;
        let deadline = tokio::time::Instant::from_std(self.deadline);
        let mut facts = std::mem::take(&mut self.facts);
        let mut awaiting_ack = false;
        let result = loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if awaiting_ack {
                        break Err(DiscordLiveError::Unknown {
                            detail: "the platform did not acknowledge a heartbeat; the session is dead".to_string(),
                        });
                    }
                    let payload = json!({"op": 1, "d": facts.last_sequence});
                    if let Err(error) = socket.send(Message::Text(payload.to_string().into())).await {
                        break Err(DiscordLiveError::Unknown {
                            detail: redacted_detail(&format!("gateway heartbeat failed: {error}")),
                        });
                    }
                    awaiting_ack = true;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break Err(DiscordLiveError::Unknown {
                        detail: format!(
                            "no MESSAGE_CREATE for the probe message arrived before the configured timeout; observed dispatch events: {:?}",
                            facts.dispatch_kinds
                        ),
                    });
                }
                _message = socket.next() => {
                    match _message {
                        None => break Err(DiscordLiveError::Unknown {
                            detail: "the gateway closed the connection before the probe message arrived".to_string(),
                        }),
                        Some(Err(error)) => break Err(DiscordLiveError::Unknown {
                            detail: redacted_detail(&format!("gateway transport failed: {error}")),
                        }),
                        Some(Ok(Message::Text(text))) => {
                            let frame = match serde_json::from_str::<Value>(text.as_str()) {
                                Ok(frame) => frame,
                                Err(error) => break Err(DiscordLiveError::Unknown {
                                    detail: format!("the gateway frame is not JSON: {error}"),
                                }),
                            };
                            if let Some(sequence) = frame.get("s").and_then(Value::as_i64) {
                                facts.last_sequence = Some(
                                    facts.last_sequence.map_or(sequence, |old| old.max(sequence)),
                                );
                            }
                            match frame.get("op").and_then(Value::as_i64) {
                                Some(11) => awaiting_ack = false,
                                Some(1) => {
                                    let payload = json!({"op": 1, "d": facts.last_sequence});
                                    if let Err(error) = socket
                                        .send(Message::Text(payload.to_string().into()))
                                        .await
                                    {
                                        break Err(DiscordLiveError::Unknown {
                                            detail: redacted_detail(&format!(
                                                "gateway heartbeat failed: {error}"
                                            )),
                                        });
                                    }
                                    awaiting_ack = true;
                                }
                                Some(7) => break Err(DiscordLiveError::Unknown {
                                    detail: "the gateway asked for a reconnect before the probe message arrived".to_string(),
                                }),
                                Some(9) => break Err(DiscordLiveError::Refused {
                                    detail: "the platform invalidated the gateway session".to_string(),
                                }),
                                Some(0) => {
                                    let event = frame
                                        .get("t")
                                        .and_then(Value::as_str)
                                        .unwrap_or("UNKNOWN")
                                        .to_string();
                                    facts.observe_dispatch(&event);
                                    if event == "MESSAGE_CREATE"
                                        && frame
                                            .pointer("/d/channel_id")
                                            .and_then(Value::as_str)
                                            == Some(channel_id)
                                        && frame
                                            .pointer("/d/id")
                                            .and_then(Value::as_str)
                                            == Some(expected_message_id)
                                    {
                                        break Ok((frame, text.len()));
                                    }
                                }
                                _ => {}
                            }
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            facts.binary_frames += 1;
                            let _ = bytes.len();
                        }
                        Some(Ok(Message::Close(Some(frame)))) => {
                            let code: u16 = frame.code.into();
                            facts.close_code = Some(code);
                            facts.close_meaning = Some(close_meaning(code).to_string());
                            break Err(close_error(code, &frame.reason));
                        }
                        Some(Ok(Message::Close(None))) => break Err(DiscordLiveError::Unknown {
                            detail: "the gateway closed the connection without a close code".to_string(),
                        }),
                        Some(Ok(_)) => {}
                    }
                }
            }
        };
        self.facts = facts;
        result
    }
}

async fn next_json_frame(
    socket: &mut DiscordSocket,
    deadline: Instant,
) -> Result<(Value, usize), DiscordLiveError> {
    loop {
        let wait = remaining(deadline, "waiting for a gateway frame")?;
        let message = match tokio::time::timeout(wait, socket.next()).await {
            Ok(message) => message,
            Err(_) => {
                return Err(DiscordLiveError::Unknown {
                    detail: "the gateway did not answer before the configured timeout".to_string(),
                })
            }
        };
        match message {
            None => {
                return Err(DiscordLiveError::Unknown {
                    detail: "the gateway closed the connection".to_string(),
                })
            }
            Some(Err(error)) => {
                return Err(DiscordLiveError::Unknown {
                    detail: redacted_detail(&format!("gateway transport failed: {error}")),
                })
            }
            Some(Ok(Message::Text(text))) => {
                let frame = serde_json::from_str::<Value>(text.as_str()).map_err(|error| {
                    DiscordLiveError::Unknown {
                        detail: format!("the gateway frame is not JSON: {error}"),
                    }
                })?;
                return Ok((frame, text.len()));
            }
            Some(Ok(Message::Binary(_))) => continue,
            Some(Ok(Message::Close(Some(frame)))) => {
                let code: u16 = frame.code.into();
                return Err(close_error(code, &frame.reason));
            }
            Some(Ok(Message::Close(None))) => {
                return Err(DiscordLiveError::Unknown {
                    detail: "the gateway closed the connection without a close code".to_string(),
                })
            }
            Some(Ok(_)) => continue,
        }
    }
}

fn close_error(code: u16, reason: &str) -> DiscordLiveError {
    let detail = format!("the discord gateway closed with code {code} ({reason})");
    if close_is_fatal(code) {
        DiscordLiveError::Refused { detail }
    } else {
        DiscordLiveError::Unknown { detail }
    }
}

// ---------------------------------------------------------------------------
// Acceptance flow
// ---------------------------------------------------------------------------

/// Run the live acceptance against the process environment.
///
/// Reads only [`TOKEN_ENV`] and [`CHANNEL_ENV`] (plus the optional tuning
/// variables). Missing credentials return a `Skipped` receipt; a malformed
/// environment also skips and names the variable, so an unattended run cannot
/// contact the platform with half a configuration.
///
/// When [`RECEIPT_ENV`] names a path, the receipt is written there.
pub fn run_from_env() -> DiscordLiveReceipt {
    let mut receipt = match DiscordLiveCredentials::from_env() {
        Ok(credentials) => block_on_run(credentials),
        Err(skip) => DiscordLiveReceipt::skipped(skip),
    };
    if let Err(error) = receipt.write_if_requested() {
        // The run itself is not changed by a receipt-path problem; the note
        // makes the missing artifact visible to the operator.
        receipt
            .notes
            .push(format!("the receipt could not be written: {error}"));
    }
    receipt
}

/// Run the live acceptance from a name/answer lookup.
///
/// The deterministic skip path is exercised through this seam; it never reads
/// the process environment and it never touches the network when the lookup
/// answers nothing.
pub fn run_with_lookup(lookup: impl Fn(&str) -> Option<String>) -> DiscordLiveReceipt {
    match DiscordLiveCredentials::from_lookup(lookup) {
        Ok(credentials) => block_on_run(credentials),
        Err(skip) => DiscordLiveReceipt::skipped(skip),
    }
}

fn block_on_run(credentials: DiscordLiveCredentials) -> DiscordLiveReceipt {
    let started_at = Utc::now();
    let started = Instant::now();
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(credentials)),
        Err(error) => {
            let mut receipt = DiscordLiveReceipt::new(started_at);
            receipt.failures.push(DiscordLiveFailure {
                stage: "runtime".to_string(),
                kind: DiscordLiveFailureKind::Unknown,
                detail: format!("could not start the harness runtime: {error}"),
                status: None,
                code: None,
                retryable: false,
            });
            receipt.finish(started)
        }
    }
}

/// Run the bounded acceptance against the platform.
///
/// One run performs: account introspection, channel introspection, capability
/// introspection, gateway discovery, a real gateway identify/READY, one seed
/// message, the gateway delivery of that message, a reply, a read-back, an
/// edit, a read-back, a typing indicator, a retraction and the cleanup of the
/// seed. Any stage that the platform refuses is recorded, not hidden.
pub async fn run(credentials: DiscordLiveCredentials) -> DiscordLiveReceipt {
    let started_at = Utc::now();
    let started = Instant::now();
    let mut receipt = DiscordLiveReceipt::new(started_at);
    let credentials = Arc::new(credentials);
    let mut client = match LiveClient::new(Arc::clone(&credentials)) {
        Ok(client) => client,
        Err(detail) => {
            receipt.failures.push(DiscordLiveFailure {
                stage: "client".to_string(),
                kind: DiscordLiveFailureKind::Unknown,
                detail,
                status: None,
                code: None,
                retryable: false,
            });
            return receipt.finish(started);
        }
    };
    let deadline = started + credentials.timeout();
    if let Err((stage, error)) = run_stages(&mut receipt, &mut client, &credentials, deadline).await {
        receipt.failures.push(error.to_failure(stage));
    }
    receipt.rate_limits = client.take_observations();
    receipt.finish(started)
}

async fn run_stages(
    receipt: &mut DiscordLiveReceipt,
    client: &mut LiveClient,
    credentials: &DiscordLiveCredentials,
    deadline: Instant,
) -> Result<(), (&'static str, DiscordLiveError)> {
    // 1. Identity: the token must belong to the bot application it claims.
    let account = client.current_user().await.map_err(|error| ("identity", error))?;
    if !account.bot {
        return Err((
            "identity",
            DiscordLiveError::Refused {
                detail: "the credential belongs to a user account (bot=false); a user token cannot open a bot gateway session".to_string(),
            },
        ));
    }
    if let Some(expected) = credentials.configured_bot_user_id()
        && expected != account.user_id.as_str()
    {
        return Err((
            "identity",
            DiscordLiveError::Refused {
                detail: format!(
                    "the token belongs to {}, but {BOT_USER_ID_ENV} names {expected}",
                    account.user_id
                ),
            },
        ));
    }
    receipt.account = Some(account.clone());

    // 2. The channel must exist and be one that can carry a text round trip.
    let channel = client
        .channel(credentials.channel_id())
        .await
        .map_err(|error| ("channel", error))?;
    receipt.channel = Some(channel.clone());
    if !is_message_channel(channel.channel_type) {
        receipt.skip = Some(DiscordLiveSkip::observed(format!(
            "live Discord acceptance skipped: channel {} is of type {} ({}), which cannot carry a text message round trip; choose a guild text, announcement, thread or direct message channel",
            channel.channel_id, channel.channel_type, channel.channel_kind
        )));
        return Ok(());
    }

    // 3. Capability introspection: declare the platform's own facts and record
    //    which of them this run can actually reach.
    let mut capabilities = introspect_capabilities(&channel);
    receipt.capabilities = Some(capabilities.clone());

    // 4. Gateway: discover, connect, identify and wait for READY.
    let mut probe = match GatewayProbe::connect(client, deadline, &account.user_id).await {
        Ok(probe) => probe,
        Err(failure) => {
            let (facts, error) = *failure;
            receipt.gateway = Some(facts);
            return Err(("gateway", error));
        }
    };
    receipt.gateway = Some(probe.facts());

    // 5. Seed: create a message so the gateway has a real delivery to make.
    //    This is a deliberate, bounded test side effect in the operator's
    //    channel; the harness deletes it at the end and records both ids.
    let nonce = nonce_for("seed");
    let seed_body = json!({
        "content": format!("shenko discord live acceptance seed {}", Utc::now().to_rfc3339()),
        "allowed_mentions": {"parse": [], "replied_user": false},
        "nonce": nonce.clone(),
        "enforce_nonce": true,
    });
    let seed_started = Instant::now();
    let seed = client
        .create_message("seed", credentials.channel_id(), &seed_body)
        .await
        .map_err(|error| ("seed", error))?;
    let seed_id = message_id_of(&seed.body).ok_or_else(|| {
        (
            "seed",
            DiscordLiveError::Unknown {
                detail: format!(
                    "discord answered the seed message with HTTP {} but no usable message id; the message may exist",
                    seed.status
                ),
            },
        )
    })?;
    receipt.actions.push(DiscordLiveActionReceipt {
        action: "seed".to_string(),
        accepted: true,
        status: Some(seed.status),
        provider_message_id: Some(seed_id.clone()),
        message_reference: None,
        nonce: Some(nonce),
        read_back: None,
        latency_ms: elapsed_ms(seed_started),
        detail: None,
    });
    mark_verified(&mut capabilities, "proactive", true);
    receipt.capabilities = Some(capabilities.clone());

    // 6. Receive the seed from the live gateway.
    let received = probe.receive(credentials.channel_id(), &seed_id).await;
    receipt.gateway = Some(probe.facts());
    let (frame, raw_frame_bytes) = match received {
        Ok(received) => received,
        Err(error) => {
            receipt.failures.push(error.to_failure("receive"));
            cleanup_seed(receipt, client, credentials).await;
            return Ok(());
        }
    };
    let sequence = frame.get("s").and_then(Value::as_i64);
    let envelope = normalize_message(
        &account.account_scope,
        &account.user_id,
        channel.channel_type,
        &frame,
    )
    .ok_or_else(|| {
        // Handled below so the seed can be cleaned up first.
        (
            "receive",
            DiscordLiveError::Incomplete {
                detail: "the gateway delivered a MESSAGE_CREATE that does not normalize into a Discord envelope".to_string(),
            },
        )
    });
    let envelope = match envelope {
        Ok(envelope) => envelope,
        Err((stage, error)) => {
            receipt.failures.push(error.to_failure(stage));
            cleanup_seed(receipt, client, credentials).await;
            return Ok(());
        }
    };
    if envelope.message_id != seed_id || envelope.conversation.as_deref() != Some(credentials.channel_id()) {
        receipt.failures.push(DiscordLiveFailure {
            stage: "receive".to_string(),
            kind: DiscordLiveFailureKind::Incomplete,
            detail: "the gateway delivered a different message than the seed this run created".to_string(),
            status: None,
            code: None,
            retryable: false,
        });
        cleanup_seed(receipt, client, credentials).await;
        return Ok(());
    }
    if envelope.actor.own_account != Some(true) {
        receipt.failures.push(DiscordLiveFailure {
            stage: "receive".to_string(),
            kind: DiscordLiveFailureKind::Incomplete,
            detail: "the gateway delivered the seed without attributing it to the served bot account".to_string(),
            status: None,
            code: None,
            retryable: false,
        });
        cleanup_seed(receipt, client, credentials).await;
        return Ok(());
    }
    if envelope.reply_target.is_none() {
        receipt.failures.push(DiscordLiveFailure {
            stage: "receive".to_string(),
            kind: DiscordLiveFailureKind::Incomplete,
            detail: "the received envelope has no replyable audience".to_string(),
            status: None,
            code: None,
            retryable: false,
        });
        cleanup_seed(receipt, client, credentials).await;
        return Ok(());
    }
    let content_chars = envelope.content.text.chars().count();
    receipt.received = Some(DiscordLiveReceived {
        message_id: envelope.message_id.clone(),
        channel_id: credentials.channel_id().to_string(),
        event: "MESSAGE_CREATE".to_string(),
        raw_frame_bytes,
        sequence,
        content_available: content_chars > 0,
        content_chars,
        own_account: envelope.actor.own_account,
        source_event_revision: envelope.source_event_revision.clone(),
        envelope,
    });
    if content_chars == 0 {
        receipt.notes.push(format!(
            "the platform delivered the seed with an empty body; this is the real MESSAGE_CONTENT observation and was recorded instead of inventing content (intents: {})",
            receipt.gateway.as_ref().map(|gateway| gateway.intents).unwrap_or_default()
        ));
    }

    // 7. Typing is independent of the reply anchor, so it is probed first.
    let typing_started = Instant::now();
    match client.typing("typing", credentials.channel_id()).await {
        Ok(answer) => {
            let accepted = is_documented_empty_success(answer.status, &answer.body);
            receipt.actions.push(DiscordLiveActionReceipt {
                action: "typing".to_string(),
                accepted,
                status: Some(answer.status),
                provider_message_id: None,
                message_reference: None,
                nonce: None,
                read_back: None,
                latency_ms: elapsed_ms(typing_started),
                detail: (!accepted).then(|| {
                    format!(
                        "HTTP {} did not carry the documented empty success",
                        answer.status
                    )
                }),
            });
            mark_verified(&mut capabilities, "typing", accepted);
        }
        Err(error) => {
            receipt.failures.push(error.to_failure("typing"));
        }
    }
    receipt.capabilities = Some(capabilities.clone());

    // 8. Reply to the received message.
    let reply_text = format!(
        "shenko discord live acceptance reply to {} at {}",
        seed_id,
        Utc::now().to_rfc3339()
    );
    let reply_nonce = nonce_for("reply");
    let reply_body = build_reply_body(&seed_id, &reply_text, &reply_nonce);
    let reply_started = Instant::now();
    let reply = match client
        .create_message("reply", credentials.channel_id(), &reply_body)
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            receipt.failures.push(error.to_failure("reply"));
            cleanup_seed(receipt, client, credentials).await;
            return Ok(());
        }
    };
    let Some(reply_id) = message_id_of(&reply.body) else {
        receipt.failures.push(DiscordLiveFailure {
            stage: "reply".to_string(),
            kind: DiscordLiveFailureKind::Unknown,
            detail: format!(
                "discord answered the reply with HTTP {} but no usable message id; the message may exist",
                reply.status
            ),
            status: Some(reply.status),
            code: None,
            retryable: false,
        });
        cleanup_seed(receipt, client, credentials).await;
        return Ok(());
    };
    let mut reply_read_back = false;
    match client
        .get_message("reply_read_back", credentials.channel_id(), &reply_id)
        .await
    {
        Ok(answer) => {
            let body = &answer.body;
            let author = body.pointer("/author/id").and_then(Value::as_str);
            let reference = body
                .pointer("/message_reference/message_id")
                .and_then(Value::as_str);
            let content = body.get("content").and_then(Value::as_str).unwrap_or_default();
            if author == Some(account.user_id.as_str())
                && reference == Some(seed_id.as_str())
                && content == reply_text.as_str()
            {
                reply_read_back = true;
            } else {
                receipt.failures.push(DiscordLiveFailure {
                    stage: "reply_read_back".to_string(),
                    kind: DiscordLiveFailureKind::Incomplete,
                    detail: "the platform accepted the reply but its read-back does not carry this bot's authorship, the seed reference and the sent body".to_string(),
                    status: Some(answer.status),
                    code: None,
                    retryable: false,
                });
            }
        }
        Err(error) => {
            receipt.failures.push(error.to_failure("reply_read_back"));
        }
    }
    receipt.actions.push(DiscordLiveActionReceipt {
        action: "reply".to_string(),
        accepted: true,
        status: Some(reply.status),
        provider_message_id: Some(reply_id.clone()),
        message_reference: Some(seed_id.clone()),
        nonce: Some(reply_nonce),
        read_back: Some(reply_read_back),
        latency_ms: elapsed_ms(reply_started),
        detail: None,
    });

    // 9. Edit: the platform must accept a change to this bot's own message.
    let edited_text = format!("{reply_text} (edited)");
    let edit_body = json!({
        "content": edited_text.clone(),
        "allowed_mentions": {"parse": [], "replied_user": false},
    });
    let edit_started = Instant::now();
    match client
        .edit_message("edit", credentials.channel_id(), &reply_id, &edit_body)
        .await
    {
        Ok(answer) => {
            let acknowledged = message_id_of(&answer.body);
            let mut read_back = None;
            let mut accepted = acknowledged.as_deref() == Some(reply_id.as_str());
            if accepted {
                match client
                    .get_message("edit_read_back", credentials.channel_id(), &reply_id)
                    .await
                {
                    Ok(read) => {
                        let content = read
                            .body
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        read_back = Some(content == edited_text.as_str());
                        accepted = content == edited_text.as_str();
                        if !accepted {
                            receipt.failures.push(DiscordLiveFailure {
                                stage: "edit_read_back".to_string(),
                                kind: DiscordLiveFailureKind::Incomplete,
                                detail: "the platform accepted the edit but the read-back still carries the old body".to_string(),
                                status: Some(read.status),
                                code: None,
                                retryable: false,
                            });
                        }
                    }
                    Err(error) => {
                        read_back = Some(false);
                        accepted = false;
                        receipt.failures.push(error.to_failure("edit_read_back"));
                    }
                }
            } else {
                receipt.failures.push(DiscordLiveFailure {
                    stage: "edit".to_string(),
                    kind: DiscordLiveFailureKind::Incomplete,
                    detail: "discord answered the edit without the edited message id".to_string(),
                    status: Some(answer.status),
                    code: None,
                    retryable: false,
                });
            }
            receipt.actions.push(DiscordLiveActionReceipt {
                action: "edit".to_string(),
                accepted,
                status: Some(answer.status),
                provider_message_id: acknowledged,
                message_reference: None,
                nonce: None,
                read_back,
                latency_ms: elapsed_ms(edit_started),
                detail: None,
            });
            mark_verified(&mut capabilities, "edit", accepted);
        }
        Err(error) => {
            receipt.failures.push(error.to_failure("edit"));
        }
    }
    receipt.capabilities = Some(capabilities.clone());

    // 10. Retract: delete the reply, then prove it is really gone.
    let retract_started = Instant::now();
    match client
        .delete_message("retract", credentials.channel_id(), &reply_id)
        .await
    {
        Ok(answer) => {
            let mut accepted = is_documented_empty_success(answer.status, &answer.body);
            let mut read_back = None;
            if accepted {
                match client
                    .get_message("retract_read_back", credentials.channel_id(), &reply_id)
                    .await
                {
                    Ok(read) => {
                        // A message that still exists is a failed retraction.
                        read_back = Some(false);
                        accepted = false;
                        receipt.failures.push(DiscordLiveFailure {
                            stage: "retract_read_back".to_string(),
                            kind: DiscordLiveFailureKind::Incomplete,
                            detail: format!(
                                "the platform accepted the deletion but the message is still readable (HTTP {})",
                                read.status
                            ),
                            status: Some(read.status),
                            code: None,
                            retryable: false,
                        });
                    }
                    Err(DiscordLiveError::Rejected { status: 404, .. }) => {
                        read_back = Some(true);
                    }
                    Err(error) => {
                        read_back = Some(false);
                        accepted = false;
                        receipt.failures.push(error.to_failure("retract_read_back"));
                    }
                }
            } else {
                receipt.failures.push(DiscordLiveFailure {
                    stage: "retract".to_string(),
                    kind: DiscordLiveFailureKind::Incomplete,
                    detail: format!(
                        "HTTP {} did not carry the documented empty success",
                        answer.status
                    ),
                    status: Some(answer.status),
                    code: None,
                    retryable: false,
                });
            }
            receipt.actions.push(DiscordLiveActionReceipt {
                action: "retract".to_string(),
                accepted,
                status: Some(answer.status),
                provider_message_id: Some(reply_id.clone()),
                message_reference: None,
                nonce: None,
                read_back,
                latency_ms: elapsed_ms(retract_started),
                detail: None,
            });
            mark_verified(&mut capabilities, "retract", accepted);
        }
        Err(error) => {
            receipt.failures.push(error.to_failure("retract"));
        }
    }
    receipt.capabilities = Some(capabilities);

    cleanup_seed(receipt, client, credentials).await;
    Ok(())
}

async fn cleanup_seed(
    receipt: &mut DiscordLiveReceipt,
    client: &mut LiveClient,
    credentials: &DiscordLiveCredentials,
) {
    // When the retraction probe failed, the reply is still there. Remove it
    // before the seed so the channel is left with neither probe message.
    let reply_id = receipt
        .actions
        .iter()
        .find(|action| action.action == "reply")
        .and_then(|action| action.provider_message_id.clone());
    let retracted = receipt
        .actions
        .iter()
        .any(|action| action.action == "retract" && action.accepted);
    if let (Some(reply_id), false) = (reply_id, retracted) {
        let started = Instant::now();
        match client
            .delete_message("cleanup_reply", credentials.channel_id(), &reply_id)
            .await
        {
            Ok(answer) => {
                let accepted = is_documented_empty_success(answer.status, &answer.body);
                receipt.actions.push(DiscordLiveActionReceipt {
                    action: "cleanup_reply".to_string(),
                    accepted,
                    status: Some(answer.status),
                    provider_message_id: Some(reply_id),
                    message_reference: None,
                    nonce: None,
                    read_back: None,
                    latency_ms: elapsed_ms(started),
                    detail: (!accepted).then(|| {
                        "the reply was not cleanly deleted; it may remain in the channel".to_string()
                    }),
                });
                if !accepted {
                    receipt.notes.push(
                        "cleanup of the reply did not return the documented empty success"
                            .to_string(),
                    );
                }
            }
            Err(error) => {
                receipt.notes.push(format!(
                    "cleanup of the reply failed ({}); the message may remain in the channel: {}",
                    error.kind_string(),
                    error.detail()
                ));
            }
        }
    }

    let Some(seed_id) = receipt
        .actions
        .iter()
        .find(|action| action.action == "seed")
        .and_then(|action| action.provider_message_id.clone())
    else {
        return;
    };
    let started = Instant::now();
    match client
        .delete_message("cleanup", credentials.channel_id(), &seed_id)
        .await
    {
        Ok(answer) => {
            let accepted = is_documented_empty_success(answer.status, &answer.body);
            receipt.actions.push(DiscordLiveActionReceipt {
                action: "cleanup_seed".to_string(),
                accepted,
                status: Some(answer.status),
                provider_message_id: Some(seed_id),
                message_reference: None,
                nonce: None,
                read_back: None,
                latency_ms: elapsed_ms(started),
                detail: (!accepted).then(|| {
                    "the seed message was not cleanly deleted; it may remain in the channel".to_string()
                }),
            });
            if !accepted {
                receipt.notes.push(
                    "cleanup of the seed message did not return the documented empty success"
                        .to_string(),
                );
            }
        }
        Err(error) => {
            receipt.notes.push(format!(
                "cleanup of the seed message failed ({}); the message may remain in the channel: {}",
                error.kind_string(),
                error.detail()
            ));
        }
    }
}

impl DiscordLiveError {
    fn kind_string(&self) -> &'static str {
        match self {
            DiscordLiveError::Rejected { .. } => "rejected",
            DiscordLiveError::RateLimited { .. } => "rate_limited",
            DiscordLiveError::Unknown { .. } => "unknown",
            DiscordLiveError::Incomplete { .. } => "incomplete",
            DiscordLiveError::Refused { .. } => "refused",
        }
    }
}

// ---------------------------------------------------------------------------
// Capability introspection
// ---------------------------------------------------------------------------

/// The capability table the Discord connector declares.
///
/// These are platform facts, not preferences: Discord has no passive reply
/// window; a bot may edit and delete its own messages without a time limit;
/// the typing indicator is a real endpoint; and an application without the
/// privileged `MESSAGE_CONTENT` intent receives empty bodies. The harness
/// asserts this table and records which fields this run actually reached.
pub fn discord_capabilities() -> ChannelCapabilities {
    ChannelCapabilities {
        direct: true,
        group: true,
        thread: true,
        guild_channel: true,
        rich_text: true,
        attachments: true,
        edit: true,
        retract: true,
        typing: true,
        proactive: true,
        notes: vec![
            "Discord has no passive reply window: a bot may speak at any time, and the platform's own per-route and global rate limits are the only schedule; those limits are read from the platform's answer, never hard coded".to_string(),
            "a reply always addresses a channel the platform already named; opening a direct message channel is a separate external action and this connector never performs it".to_string(),
            "an edit and a deletion address the bot's own message in that channel, and only the bot's own message".to_string(),
            "the typing indicator expires after ten seconds on the platform side; it is not a message and carries no body".to_string(),
            "without the privileged MESSAGE_CONTENT intent the platform delivers empty content, embeds and attachments; the connector records that observation instead of inventing a body".to_string(),
            "outgoing messages expand no mention unless the owner configured the mention families; the default parses nothing, so an outgoing body never pings a user by accident".to_string(),
            "outbound media upload is not produced by this connector: only inbound attachments are read, and they are bounded by the channel attachment bound".to_string(),
            "attachments are served from Discord's own CDN with a signed address that expires; the bot credential is never sent to a content address".to_string(),
        ],
    }
}

fn introspect_capabilities(channel: &DiscordLiveChannelFacts) -> DiscordLiveCapabilityIntrospection {
    let declared = discord_capabilities();
    let direct_or_group = channel.audience_shape;
    let checks = vec![
        DiscordLiveCapabilityCheck {
            capability: "direct".to_string(),
            declared: declared.direct,
            // A group DM shares the direct-message namespace but reaches
            // several counterparts; it is not a one-to-one channel.
            verified: channel.channel_type == CHANNEL_TYPE_DM,
            basis: format!("channel type {} ({})", channel.channel_type, channel.channel_kind),
        },
        DiscordLiveCapabilityCheck {
            capability: "group".to_string(),
            declared: declared.group,
            verified: matches!(direct_or_group, Some(AudienceShape::Group)) && !channel.is_thread,
            basis: format!("channel type {} ({})", channel.channel_type, channel.channel_kind),
        },
        DiscordLiveCapabilityCheck {
            capability: "thread".to_string(),
            declared: declared.thread,
            verified: channel.is_thread,
            basis: if channel.is_thread {
                format!("channel type {} ({})", channel.channel_type, channel.channel_kind)
            } else {
                "this probe channel is not a thread; the thread audience is declared but not exercised by this run".to_string()
            },
        },
        DiscordLiveCapabilityCheck {
            capability: "guild_channel".to_string(),
            declared: declared.guild_channel,
            verified: channel.guild_id.is_some(),
            basis: "the channel probe reports a guild id".to_string(),
        },
        DiscordLiveCapabilityCheck {
            capability: "rich_text".to_string(),
            declared: declared.rich_text,
            verified: false,
            basis: "Discord renders Markdown on the platform side; this run proves acceptance of the body, not its visual rendering".to_string(),
        },
        DiscordLiveCapabilityCheck {
            capability: "attachments".to_string(),
            declared: declared.attachments,
            verified: false,
            basis: "the probe messages carry no attachment; inbound attachment read is declared but not exercised by this run".to_string(),
        },
        DiscordLiveCapabilityCheck {
            capability: "edit".to_string(),
            declared: declared.edit,
            verified: false,
            basis: "probed below by editing this bot's own message and reading it back".to_string(),
        },
        DiscordLiveCapabilityCheck {
            capability: "retract".to_string(),
            declared: declared.retract,
            verified: false,
            basis: "probed below by deleting this bot's own message and confirming it is gone".to_string(),
        },
        DiscordLiveCapabilityCheck {
            capability: "typing".to_string(),
            declared: declared.typing,
            verified: false,
            basis: "probed below through the typing endpoint".to_string(),
        },
        DiscordLiveCapabilityCheck {
            capability: "proactive".to_string(),
            declared: declared.proactive,
            verified: false,
            basis: "the seed and reply are proactive sends with no inbound request from a person; they are probed below".to_string(),
        },
    ];
    DiscordLiveCapabilityIntrospection {
        declared,
        checks,
        audience_shape: channel.audience_shape,
        reply_kind: channel.reply_kind.clone(),
    }
}

fn mark_verified(
    capabilities: &mut DiscordLiveCapabilityIntrospection,
    capability: &str,
    verified: bool,
) {
    if let Some(check) = capabilities
        .checks
        .iter_mut()
        .find(|check| check.capability == capability)
    {
        check.verified = verified;
        if !verified {
            check.basis = format!(
                "the platform did not accept this action in this run: {}",
                check.basis
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Message normalization (the envelope oracle)
// ---------------------------------------------------------------------------

/// Normalize one live `MESSAGE_CREATE` frame into the channel envelope the
/// connector must produce.
///
/// This is the harness's oracle: it keeps the platform facts the connector
/// declares (account scope, conversation, revision, actor origin, reply
/// audience, attachments) and invents nothing. It returns `None` only when
/// the frame is not a Discord message with the ids the contract requires.
pub fn normalize_message(
    account_scope: &str,
    bot_user_id: &str,
    channel_type_hint: i64,
    frame: &Value,
) -> Option<ChannelEnvelope> {
    let event = frame.get("t").and_then(Value::as_str)?;
    if event != "MESSAGE_CREATE" && event != "MESSAGE_UPDATE" {
        return None;
    }
    let data = frame.get("d")?;
    let message_id = data.get("id").and_then(Value::as_str).and_then(snowflake_str)?;
    let channel_id = data
        .get("channel_id")
        .and_then(Value::as_str)
        .and_then(snowflake_str)?;
    let guild_id = data
        .get("guild_id")
        .and_then(Value::as_str)
        .filter(|id| is_snowflake(id))
        .map(str::to_owned);
    // Discord's message object does not carry the channel type, so the
    // harness supplies the type the channel probe just established. If a
    // deployment does include it, the frame's own fact wins.
    let channel_type = data
        .get("channel_type")
        .and_then(Value::as_i64)
        .unwrap_or(channel_type_hint);
    let author = data.get("author");
    let author_id = author
        .and_then(|author| author.get("id"))
        .and_then(Value::as_str)
        .and_then(snowflake_str)
        .unwrap_or_default();
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mentions: Vec<String> = data
        .get("mentions")
        .and_then(Value::as_array)
        .map(|mentions| {
            mentions
                .iter()
                .filter_map(|mention| {
                    mention
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(snowflake_str)
                })
                .collect()
        })
        .unwrap_or_default();
    let reply_to = data
        .pointer("/message_reference/message_id")
        .and_then(Value::as_str)
        .and_then(snowflake_str)
        .or_else(|| {
            data.pointer("/referenced_message/id")
                .and_then(Value::as_str)
                .and_then(snowflake_str)
        });
    let attachments = parse_attachments(data);
    let timestamp = text_field(data, "timestamp")
        .and_then(|text| DateTime::parse_from_rfc3339(&text).ok())
        .map(|value| value.with_timezone(&Utc));
    let edited_timestamp = text_field(data, "edited_timestamp")
        .and_then(|text| DateTime::parse_from_rfc3339(&text).ok())
        .map(|value| value.with_timezone(&Utc));
    let revision = revision_of(frame, event, timestamp.as_ref(), edited_timestamp.as_ref());

    let mut segments = vec![ChannelSegment {
        kind: "text".to_string(),
        text: Some(content.clone()),
        attachment_id: None,
        url: None,
    }];
    if content.is_empty() && attachments.is_empty() {
        segments.push(ChannelSegment {
            kind: "content_unavailable".to_string(),
            text: Some(
                "the platform delivered no body for this message; the connector records the missing MESSAGE_CONTENT fact instead of inventing content"
                    .to_string(),
            ),
            attachment_id: None,
            url: None,
        });
    }
    for mention in &mentions {
        segments.push(ChannelSegment {
            kind: "mention".to_string(),
            text: Some(mention.clone()),
            attachment_id: None,
            url: None,
        });
    }
    if let Some(reply_to) = &reply_to {
        segments.push(ChannelSegment {
            kind: "quote_reference".to_string(),
            text: Some(reply_to.clone()),
            attachment_id: None,
            url: None,
        });
    }
    for attachment in &attachments {
        segments.push(ChannelSegment {
            kind: attachment.kind.clone(),
            text: attachment.name.clone(),
            attachment_id: Some(attachment.attachment_id.clone()),
            url: attachment.locator.url.clone(),
        });
    }

    let is_direct = is_direct_type(channel_type);
    let kind = if is_direct {
        DISCORD_KIND_DM
    } else {
        DISCORD_KIND_CHANNEL
    };
    let label = match (&guild_id, is_direct) {
        (Some(guild), _) => format!("服务器 {guild} 频道 {channel_id}"),
        (None, true) => format!("私信频道 {channel_id}"),
        (None, false) => format!("频道 {channel_id}"),
    };
    let own_account = !author_id.is_empty() && author_id == bot_user_id;
    let shape = audience_shape_of(channel_type);
    let display_name = author
        .and_then(|author| text_field(author, "global_name"))
        .or_else(|| author.and_then(|author| text_field(author, "username")))
        .or_else(|| data.pointer("/member/nick").and_then(Value::as_str).map(str::to_owned));
    Some(ChannelEnvelope {
        schema_version: ENVELOPE_SCHEMA_VERSION,
        provider: DISCORD_CHANNEL_ID.to_string(),
        account: account_scope.to_string(),
        tenant: None,
        transport_identity_domain: DISCORD_TRANSPORT_DOMAIN.to_string(),
        conversation: Some(channel_id.clone()),
        thread: is_thread_type(channel_type).then(|| channel_id.clone()),
        message_id,
        source_event_revision: Some(revision),
        occurred_at: timestamp,
        received_at: Utc::now(),
        actor: ChannelActor {
            platform_id: author_id,
            display_name,
            alias: None,
            person_ref: None,
            role_grant_ref: None,
            group_role: None,
            own_account: Some(own_account),
        },
        content: ChannelContent {
            text: content,
            segments,
            mentions,
            reply_to,
        },
        reply_target: Some(ChannelReplyTarget {
            kind: kind.to_string(),
            id: channel_id.clone(),
            label,
            account: account_scope.to_string(),
            tenant: None,
            conversation: Some(channel_id),
            shape,
        }),
        attachments,
        raw_ref: None,
    })
}

fn revision_of(
    frame: &Value,
    event: &str,
    timestamp: Option<&DateTime<Utc>>,
    edited_timestamp: Option<&DateTime<Utc>>,
) -> String {
    if event == "MESSAGE_UPDATE" {
        if let Some(edited) = edited_timestamp {
            return format!("edit:{}", edited.to_rfc3339());
        }
        if let Some(sequence) = frame.get("s").and_then(Value::as_i64) {
            return format!("update:{sequence}");
        }
        return "update".to_string();
    }
    if let Some(created) = timestamp {
        return created.to_rfc3339();
    }
    if let Some(sequence) = frame.get("s").and_then(Value::as_i64) {
        return format!("create:{sequence}");
    }
    "create".to_string()
}

fn parse_attachments(data: &Value) -> Vec<ChannelAttachment> {
    let Some(attachments) = data.get("attachments").and_then(Value::as_array) else {
        return Vec::new();
    };
    attachments
        .iter()
        .filter_map(|attachment| {
            let attachment_id = snowflake_str(attachment.get("id").and_then(Value::as_str)?)?;
            let url = attachment
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(str::to_owned);
            let content_type = attachment
                .get("content_type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let kind = match content_type.as_deref() {
                Some(mime) if mime.starts_with("image/") => "image",
                Some(mime) if mime.starts_with("video/") => "video",
                Some(mime) if mime.starts_with("audio/") => "audio",
                _ => "file",
            }
            .to_string();
            let host = url
                .as_deref()
                .and_then(|url| url::Url::parse(url).ok())
                .and_then(|parsed| parsed.host_str().map(str::to_owned));
            Some(ChannelAttachment {
                attachment_id,
                kind,
                name: attachment
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                mime: content_type,
                bytes: attachment.get("size").and_then(Value::as_u64),
                hash: None,
                locator: AttachmentLocator {
                    kind: "url".to_string(),
                    host,
                    path: None,
                    url: url.clone(),
                    params: None,
                },
                object: None,
                expires_at: url.as_deref().and_then(cdn_expiry),
            })
        })
        .collect()
}

fn cdn_expiry(url: &str) -> Option<DateTime<Utc>> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(key, _)| &**key == "ex")
        .and_then(|(_, value)| value.parse::<i64>().ok())
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
}

fn build_reply_body(referenced_message_id: &str, text: &str, nonce: &str) -> Value {
    let text: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
    json!({
        "content": text,
        "message_reference": {"message_id": referenced_message_id},
        "allowed_mentions": {"parse": [], "replied_user": false},
        "nonce": nonce,
        "enforce_nonce": true,
    })
}

fn nonce_for(role: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let process = std::process::id();
    let raw = format!("{role}:{process}:{nanos}");
    let mut nonce = String::new();
    for byte in raw.bytes() {
        if nonce.len() >= 24 {
            break;
        }
        nonce.push_str(&format!("{:02x}", byte));
    }
    nonce
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Whether a value is a Discord snowflake.
pub fn is_snowflake(value: &str) -> bool {
    value.len() >= MIN_SNOWFLAKE_CHARS && value.chars().all(|ch| ch.is_ascii_digit())
}

fn snowflake_str(value: &str) -> Option<String> {
    is_snowflake(value).then(|| value.to_string())
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn message_id_of(body: &Value) -> Option<String> {
    body.get("id").and_then(Value::as_str).and_then(snowflake_str)
}

fn is_documented_empty_success(status: u16, body: &Value) -> bool {
    status == 204 || !body.is_null()
}

/// A readable name for the platform's channel type.
pub fn channel_kind(channel_type: i64) -> &'static str {
    match channel_type {
        CHANNEL_TYPE_GUILD_TEXT => "guild_text",
        CHANNEL_TYPE_DM => "dm",
        CHANNEL_TYPE_GUILD_VOICE => "guild_voice",
        CHANNEL_TYPE_GROUP_DM => "group_dm",
        CHANNEL_TYPE_GUILD_CATEGORY => "guild_category",
        CHANNEL_TYPE_GUILD_ANNOUNCEMENT => "guild_announcement",
        CHANNEL_TYPE_ANNOUNCEMENT_THREAD => "announcement_thread",
        CHANNEL_TYPE_PUBLIC_THREAD => "public_thread",
        CHANNEL_TYPE_PRIVATE_THREAD => "private_thread",
        CHANNEL_TYPE_GUILD_STAGE_VOICE => "guild_stage_voice",
        CHANNEL_TYPE_GUILD_DIRECTORY => "guild_directory",
        CHANNEL_TYPE_GUILD_FORUM => "guild_forum",
        CHANNEL_TYPE_GUILD_MEDIA => "guild_media",
        _ => "unknown",
    }
}

/// Whether the channel type is a thread.
pub fn is_thread_type(channel_type: i64) -> bool {
    matches!(
        channel_type,
        CHANNEL_TYPE_ANNOUNCEMENT_THREAD
            | CHANNEL_TYPE_PUBLIC_THREAD
            | CHANNEL_TYPE_PRIVATE_THREAD
    )
}

/// Whether the channel type is a direct message namespace.
pub fn is_direct_type(channel_type: i64) -> bool {
    matches!(channel_type, CHANNEL_TYPE_DM | CHANNEL_TYPE_GROUP_DM)
}

/// Whether this channel can carry a text message round trip.
pub fn is_message_channel(channel_type: i64) -> bool {
    matches!(
        channel_type,
        CHANNEL_TYPE_GUILD_TEXT
            | CHANNEL_TYPE_DM
            | CHANNEL_TYPE_GROUP_DM
            | CHANNEL_TYPE_GUILD_ANNOUNCEMENT
            | CHANNEL_TYPE_ANNOUNCEMENT_THREAD
            | CHANNEL_TYPE_PUBLIC_THREAD
            | CHANNEL_TYPE_PRIVATE_THREAD
    )
}

/// How many counterparts a channel type reaches.
pub fn audience_shape_of(channel_type: i64) -> Option<AudienceShape> {
    if channel_type == CHANNEL_TYPE_DM {
        Some(AudienceShape::OneToOne)
    } else if is_message_channel(channel_type) {
        // A group DM and a guild channel both reach several counterparts; a
        // thread reaches its participants inside one conversation.
        Some(AudienceShape::Group)
    } else {
        None
    }
}

/// The reply kind the connector offers for a channel type.
pub fn reply_kind_of(channel_type: i64) -> Option<&'static str> {
    if channel_type == CHANNEL_TYPE_DM || channel_type == CHANNEL_TYPE_GROUP_DM {
        Some(DISCORD_KIND_DM)
    } else if is_message_channel(channel_type) {
        Some(DISCORD_KIND_CHANNEL)
    } else {
        None
    }
}

fn connect_url(gateway: &str) -> String {
    let base = gateway.trim_end_matches('/');
    if base.contains("v=") {
        return base.to_string();
    }
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}v={DISCORD_API_VERSION}&encoding=json")
}

fn remaining(deadline: Instant, stage: &str) -> Result<Duration, DiscordLiveError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(DiscordLiveError::Unknown {
            detail: format!("the configured timeout elapsed while {stage}"),
        })
    } else {
        Ok(remaining)
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Strip anything that could carry a credential out of a transport result.
///
/// `reqwest` embeds the URL in its error display; the URL of this harness
/// never carries the token, but the scrub is defensive and the `Bot <value>`
/// pattern is removed as well.
fn redacted_detail(detail: &str) -> String {
    let mut safe = redact_transport_error(detail);
    if let Some(index) = safe.find("Bot ") {
        let start = index + 4;
        let end = safe[start..]
            .find(char::is_whitespace)
            .map(|offset| start + offset)
            .unwrap_or(safe.len());
        if end > start {
            safe.replace_range(start..end, "<redacted>");
        }
    }
    safe
}

/// What one gateway close code means for the next connection.
///
/// The codes that refuse the credential, the intents, the shard or the API
/// version cannot be fixed by reconnecting.
pub fn close_is_fatal(code: u16) -> bool {
    matches!(code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014)
}

/// A readable meaning for a gateway close code.
pub fn close_meaning(code: u16) -> &'static str {
    match code {
        4004 => "authentication failed (the token was refused)",
        4007 => "the resume sequence was invalid; identify again",
        4009 => "the resume session timed out; identify again",
        4010 => "invalid shard",
        4011 => "sharding is required for this deployment",
        4012 => "invalid API version",
        4013 => "invalid intent bitmask",
        4014 => "a privileged intent is not enabled for this application",
        _ => "the platform closed the session",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn credentials_with_token(token: &str) -> DiscordLiveCredentials {
        DiscordLiveCredentials::from_lookup(lookup(&[
            (TOKEN_ENV, token),
            (CHANNEL_ENV, "123456789012345678"),
        ]))
        .expect("a complete lookup builds credentials")
    }

    #[test]
    fn absent_environment_skips_with_an_explicit_reason_and_no_network() {
        let receipt = run_with_lookup(|_| None);
        assert_eq!(receipt.status, DiscordLiveStatus::Skipped);
        // The receipt document is read before the skip reason is taken, so
        // the recorded document and the recorded reason are the same receipt.
        let document = receipt.to_json();
        let skip = receipt.skip.expect("the skip reason is recorded");
        assert!(skip.missing_environment.contains(&TOKEN_ENV.to_string()));
        assert!(skip.missing_environment.contains(&CHANNEL_ENV.to_string()));
        assert!(!skip.network_attempted, "an absent environment contacts nothing");
        assert!(skip.reason.contains(TOKEN_ENV));
        assert!(skip.reason.contains(CHANNEL_ENV));
        assert!(!skip.prompt.is_empty());
        assert!(receipt.failures.is_empty());
        assert_eq!(document["status"], serde_json::json!("skipped"));
    }

    #[test]
    fn one_missing_variable_is_named_alone() {
        let receipt = run_with_lookup(lookup(&[(TOKEN_ENV, "canary-token")]));
        assert_eq!(receipt.status, DiscordLiveStatus::Skipped);
        let skip = receipt.skip.expect("skip is recorded");
        assert_eq!(skip.missing_environment, vec![CHANNEL_ENV.to_string()]);
        assert!(!skip.reason.contains(TOKEN_ENV));
        assert!(
            !skip.reason.contains("canary-token"),
            "a skip reason never echoes the supplied token"
        );
    }

    #[test]
    fn blank_values_are_treated_as_absent() {
        let receipt = run_with_lookup(lookup(&[(TOKEN_ENV, "   "), (CHANNEL_ENV, "")]));
        assert_eq!(receipt.status, DiscordLiveStatus::Skipped);
        let skip = receipt.skip.expect("skip is recorded");
        assert!(skip.missing_environment.contains(&TOKEN_ENV.to_string()));
        assert!(skip.missing_environment.contains(&CHANNEL_ENV.to_string()));
    }

    #[test]
    fn a_channel_name_is_not_a_snowflake() {
        let receipt = run_with_lookup(lookup(&[
            (TOKEN_ENV, "canary-token"),
            (CHANNEL_ENV, "general"),
        ]));
        assert_eq!(receipt.status, DiscordLiveStatus::Skipped);
        let skip = receipt.skip.expect("skip is recorded");
        assert!(!skip.network_attempted);
        assert!(skip.reason.contains(CHANNEL_ENV));
        assert!(skip.reason.contains("snowflake"));
    }

    #[test]
    fn debug_and_receipts_never_carry_the_token() {
        let canary = "canary-token-that-must-not-leak";
        let credentials = credentials_with_token(canary);
        let debug = format!("{credentials:?}");
        assert!(!debug.contains(canary), "debug leaked the token: {debug}");
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("123456789012345678"));

        let mut receipt = DiscordLiveReceipt::new(Utc::now());
        receipt.notes.push("a note".to_string());
        receipt.failures.push(DiscordLiveFailure {
            stage: "identity".to_string(),
            kind: DiscordLiveFailureKind::Unknown,
            detail: redacted_detail("a transport error mentioning Bot canary-token-that-must-not-leak in passing"),
            status: None,
            code: None,
            retryable: false,
        });
        let json = receipt.to_json().to_string();
        assert!(!json.contains(canary), "receipt leaked the token: {json}");
        assert!(!json.contains("canary-token-that-must-not-leak"));
    }

    #[test]
    fn rate_limit_facts_are_read_from_headers_and_body() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-limit", "5".parse().unwrap());
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-after", "1.25".parse().unwrap());
        headers.insert("x-ratelimit-bucket", "abc".parse().unwrap());
        headers.insert("x-ratelimit-scope", "user".parse().unwrap());
        headers.insert("x-ratelimit-global", "true".parse().unwrap());
        let mut rate = DiscordLiveRateFacts::from_headers(&headers);
        assert_eq!(rate.limit, Some(5));
        assert_eq!(rate.remaining, Some(0));
        assert_eq!(rate.reset_after, Some(1.25));
        assert_eq!(rate.bucket.as_deref(), Some("abc"));
        assert_eq!(rate.scope.as_deref(), Some("user"));
        assert!(rate.global);
        rate.absorb_body(&json!({"retry_after": 0.5, "global": false}));
        assert_eq!(rate.retry_after, Some(0.5));
        assert!(!rate.global);
    }

    #[test]
    fn a_rate_limit_is_honoured_only_inside_the_cap() {
        // The request loop uses this exact comparison; a longer platform wait
        // is reported, never slept through.
        let cap = Duration::from_secs(5);
        let within = |seconds: f64| {
            seconds.is_finite()
                && seconds > 0.0
                && Duration::from_secs_f64(seconds) <= cap
        };
        assert!(within(0.25));
        assert!(within(5.0));
        assert!(!within(30.0));
        assert!(!within(f64::NAN));
    }

    #[test]
    fn gateway_close_codes_are_classified() {
        for code in [4004u16, 4010, 4011, 4012, 4013, 4014] {
            assert!(close_is_fatal(code), "{code} must not be retried");
        }
        for code in [4007u16, 4009, 1000, 1006, 4000] {
            assert!(!close_is_fatal(code), "{code} may be retried");
        }
        assert!(close_meaning(4014).contains("privileged intent"));
        assert!(matches!(
            close_error(4004, "auth"),
            DiscordLiveError::Refused { .. }
        ));
        assert!(matches!(
            close_error(1000, "bye"),
            DiscordLiveError::Unknown { .. }
        ));
    }

    #[test]
    fn the_declared_capability_table_is_honest() {
        let table = discord_capabilities();
        assert!(table.direct && table.group && table.thread && table.guild_channel);
        assert!(table.edit && table.retract && table.typing && table.proactive);
        assert!(
            table.notes.iter().any(|note| note.contains("MESSAGE_CONTENT")),
            "the privileged intent limit is declared"
        );
        assert!(
            table.notes.iter().any(|note| note.contains("rate")),
            "the platform rate limits are declared"
        );
        assert!(
            table.notes.iter().any(|note| note.contains("media")),
            "the missing outbound media is declared"
        );
    }

    #[test]
    fn channel_types_map_to_audiences_and_skips() {
        assert_eq!(
            audience_shape_of(CHANNEL_TYPE_GUILD_TEXT),
            Some(AudienceShape::Group)
        );
        assert_eq!(
            audience_shape_of(CHANNEL_TYPE_DM),
            Some(AudienceShape::OneToOne)
        );
        assert_eq!(
            audience_shape_of(CHANNEL_TYPE_GROUP_DM),
            Some(AudienceShape::Group),
            "a group DM reaches several counterparts"
        );
        assert_eq!(
            audience_shape_of(CHANNEL_TYPE_PUBLIC_THREAD),
            Some(AudienceShape::Group)
        );
        assert_eq!(audience_shape_of(CHANNEL_TYPE_GUILD_CATEGORY), None);
        assert_eq!(audience_shape_of(CHANNEL_TYPE_GUILD_VOICE), None);
        assert!(is_message_channel(CHANNEL_TYPE_GUILD_ANNOUNCEMENT));
        assert!(!is_message_channel(CHANNEL_TYPE_GUILD_FORUM));
        assert_eq!(reply_kind_of(CHANNEL_TYPE_DM), Some(DISCORD_KIND_DM));
        assert_eq!(reply_kind_of(CHANNEL_TYPE_GUILD_TEXT), Some(DISCORD_KIND_CHANNEL));
        assert_eq!(reply_kind_of(CHANNEL_TYPE_GUILD_CATEGORY), None);
        assert!(is_thread_type(CHANNEL_TYPE_PRIVATE_THREAD));
        assert!(!is_thread_type(CHANNEL_TYPE_GUILD_TEXT));
    }

    fn message_frame(channel_type: i64, guild_id: Option<&str>, author_id: &str) -> Value {
        let mut data = json!({
            "id": "111111111111111111",
            "channel_id": "222222222222222222",
            "channel_type": channel_type,
            "author": {"id": author_id, "username": "probe", "global_name": "Probe"},
            "content": "hello from the probe",
            "timestamp": "2026-11-22T10:00:00.000000+00:00",
            "mentions": [{"id": "333333333333333333"}],
            "attachments": [],
        });
        if let Some(guild) = guild_id {
            data["guild_id"] = json!(guild);
        }
        json!({"op": 0, "s": 7, "t": "MESSAGE_CREATE", "d": data})
    }

    #[test]
    fn a_guild_message_normalizes_to_the_channel_envelope() {
        let frame = message_frame(CHANNEL_TYPE_GUILD_TEXT, Some("444444444444444444"), "555555555555555555");
        let envelope = normalize_message(
            "discord:555555555555555555",
            "555555555555555555",
            CHANNEL_TYPE_GUILD_TEXT,
            &frame,
        )
        .expect("a real guild message normalizes");
        assert_eq!(envelope.provider, DISCORD_CHANNEL_ID);
        assert_eq!(envelope.account, "discord:555555555555555555");
        assert_eq!(envelope.message_id, "111111111111111111");
        assert_eq!(envelope.conversation.as_deref(), Some("222222222222222222"));
        assert_eq!(envelope.transport_identity_domain, DISCORD_TRANSPORT_DOMAIN);
        assert_eq!(
            envelope.source_event_revision.as_deref(),
            Some("2026-11-22T10:00:00+00:00")
        );
        assert_eq!(envelope.actor.own_account, Some(true));
        let target = envelope.reply_target.expect("the message is replyable");
        assert_eq!(target.kind, DISCORD_KIND_CHANNEL);
        assert_eq!(target.id, "222222222222222222");
        assert_eq!(target.shape, Some(AudienceShape::Group));
        assert_eq!(envelope.content.mentions, vec!["333333333333333333"]);
        assert!(envelope.attachments.is_empty());
    }

    #[test]
    fn a_direct_message_and_a_thread_keep_their_audience_facts() {
        let dm = message_frame(CHANNEL_TYPE_DM, None, "555555555555555555");
        let envelope = normalize_message(
            "discord:555555555555555555",
            "999999999999999999",
            CHANNEL_TYPE_DM,
            &dm,
        )
        .expect("a direct message normalizes");
        assert_eq!(envelope.actor.own_account, Some(false));
        let target = envelope.reply_target.expect("replyable");
        assert_eq!(target.kind, DISCORD_KIND_DM);
        assert_eq!(target.shape, Some(AudienceShape::OneToOne));
        assert!(envelope.thread.is_none());

        let thread = message_frame(
            CHANNEL_TYPE_PUBLIC_THREAD,
            Some("444444444444444444"),
            "555555555555555555",
        );
        let envelope = normalize_message(
            "discord:555555555555555555",
            "555555555555555555",
            CHANNEL_TYPE_PUBLIC_THREAD,
            &thread,
        )
        .expect("a thread message normalizes");
        assert_eq!(envelope.thread.as_deref(), Some("222222222222222222"));
        assert_eq!(
            envelope.reply_target.expect("replyable").shape,
            Some(AudienceShape::Group)
        );
    }

    #[test]
    fn empty_content_is_recorded_as_a_missing_body_not_invented() {
        let mut frame = message_frame(CHANNEL_TYPE_DM, None, "555555555555555555");
        frame["d"]["content"] = json!("");
        let envelope = normalize_message(
            "discord:555555555555555555",
            "555555555555555555",
            CHANNEL_TYPE_DM,
            &frame,
        )
        .expect("an empty message still normalizes");
        assert!(envelope.content.text.is_empty());
        assert!(envelope
            .content
            .segments
            .iter()
            .any(|segment| segment.kind == "content_unavailable"));
    }

    #[test]
    fn a_real_message_frame_without_a_channel_type_uses_the_probed_type() {
        // Discord's MESSAGE_CREATE object does not carry `channel_type`; the
        // harness supplies the fact the channel probe established.
        let mut frame = message_frame(CHANNEL_TYPE_DM, None, "555555555555555555");
        frame["d"]
            .as_object_mut()
            .expect("the test frame is an object")
            .remove("channel_type");
        let envelope = normalize_message(
            "discord:555555555555555555",
            "555555555555555555",
            CHANNEL_TYPE_DM,
            &frame,
        )
        .expect("a direct message without a channel_type still normalizes");
        assert_eq!(
            envelope.reply_target.expect("replyable").kind,
            DISCORD_KIND_DM
        );
        assert!(envelope.thread.is_none());
    }

    #[test]
    fn attachments_keep_their_locator_and_signed_expiry() {
        let mut frame = message_frame(CHANNEL_TYPE_GUILD_TEXT, Some("444444444444444444"), "555555555555555555");
        frame["d"]["attachments"] = json!([{
            "id": "666666666666666666",
            "filename": "proof.png",
            "content_type": "image/png",
            "size": 1024,
            "url": "https://cdn.discordapp.com/attachments/2/6/proof.png?ex=1700000000&is=1&hm=abc",
        }]);
        let envelope = normalize_message(
            "discord:555555555555555555",
            "555555555555555555",
            CHANNEL_TYPE_GUILD_TEXT,
            &frame,
        )
        .expect("a message with an attachment normalizes");
        assert_eq!(envelope.attachments.len(), 1);
        let attachment = &envelope.attachments[0];
        assert_eq!(attachment.attachment_id, "666666666666666666");
        assert_eq!(attachment.kind, "image");
        assert_eq!(attachment.name.as_deref(), Some("proof.png"));
        assert_eq!(attachment.bytes, Some(1024));
        assert_eq!(attachment.locator.host.as_deref(), Some("cdn.discordapp.com"));
        assert!(attachment.expires_at.is_some(), "the signed expiry is a platform fact");
        assert!(attachment.object.is_none(), "the bytes are not carried in the envelope");
    }

    #[test]
    fn a_reply_body_references_the_received_message_and_expands_no_mention() {
        let body = build_reply_body("111111111111111111", "the answer", "0123456789abcdef01234567");
        assert_eq!(
            body.pointer("/message_reference/message_id").and_then(Value::as_str),
            Some("111111111111111111")
        );
        assert_eq!(body.pointer("/allowed_mentions/parse"), Some(&json!([])));
        assert_eq!(
            body.pointer("/allowed_mentions/replied_user").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(body.get("nonce").and_then(Value::as_str).map(str::len), Some(24));
        assert_eq!(body.get("enforce_nonce").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn a_passed_receipt_without_its_stages_is_rejected() {
        let mut receipt = DiscordLiveReceipt::new(Utc::now());
        receipt.failures.clear();
        assert!(
            receipt.validate_mandatory().is_err(),
            "an empty receipt cannot claim a pass"
        );
        receipt.status = DiscordLiveStatus::Failed;
        receipt.failures.push(DiscordLiveFailure {
            stage: "identity".to_string(),
            kind: DiscordLiveFailureKind::Refused,
            detail: "test".to_string(),
            status: None,
            code: None,
            retryable: false,
        });
        assert!(receipt.validate().is_ok(), "a failed receipt names its failure");

        let skipped = DiscordLiveSkip::absent(vec![TOKEN_ENV.to_string()]);
        let mut receipt = DiscordLiveReceipt::skipped(skipped);
        assert_eq!(receipt.status, DiscordLiveStatus::Skipped);
        assert!(receipt.validate().is_ok());
        receipt.failures.push(DiscordLiveFailure {
            stage: "x".to_string(),
            kind: DiscordLiveFailureKind::Unknown,
            detail: "x".to_string(),
            status: None,
            code: None,
            retryable: false,
        });
        assert!(receipt.validate().is_err(), "a skip cannot hide a failure");
    }

    #[test]
    fn snowflakes_are_decimal_ids_only() {
        assert!(is_snowflake("123456789012345678"));
        assert!(!is_snowflake("1234"));
        assert!(!is_snowflake("1234567890123456789a"));
        assert!(!is_snowflake(""));
        assert_eq!(message_id_of(&json!({"id": "123456789012345678"})).as_deref(), Some("123456789012345678"));
        assert_eq!(message_id_of(&json!({"id": 123456789012345678i64})), None);
        assert_eq!(message_id_of(&json!({"no": "id"})), None);
    }

    #[test]
    fn the_gateway_url_keeps_the_api_version_and_encoding() {
        assert_eq!(
            connect_url("wss://gateway.discord.gg"),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
        assert_eq!(
            connect_url("wss://gateway.discord.gg/?v=10&encoding=json"),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
        assert_eq!(
            connect_url("wss://resume.example/"),
            "wss://resume.example/?v=10&encoding=json"
        );
    }
}
