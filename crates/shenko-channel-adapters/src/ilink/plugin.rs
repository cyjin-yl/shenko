//! The WeChat iLink channel plugin — SPEC §15.3, §16.
//!
//! The plugin owns everything iLink-specific: the long-poll ingress, the CDN
//! media flow, the per-conversation context tokens and the acceptance verdict
//! of a send. The core only ever sees the normalized envelope and the opaque
//! target.
//!
//! The protocol's real boundary is narrower than a general chat platform and
//! this plugin does not pretend otherwise:
//!
//! * there is no edit endpoint and no retract endpoint;
//! * there is no typing indicator;
//! * a group send does not exist, so only a direct conversation is served;
//! * a reply must carry the conversation's own `context_token`, which the
//!   connector remembers per conversation instead of inventing.
//!
//! Ordinary reachability is open; administration is a separate owner-set fact
//! (`config.admin_user_ids`), never derived from familiarity.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use shenko_types::channel::{
    BoxFuture, ChannelAction, ChannelDispatch, ChannelEnvelope, ChannelError, ChannelPlugin,
    ChannelReceipt, ChannelRegistry, ChannelRegistryError, ReplyKind,
};
use shenko_types::object::Digest;
use shenko_types::output::PublicationTarget;

use crate::common::{ChannelCapabilities, ChannelHealth, HealthState, ReplyContextCache};

use super::api::{ILinkApi, ILinkError, MediaKind};
use super::config::ILinkConfig;
use super::events::{ILINK_CHANNEL_ID, ILINK_KIND_DIRECT, ILINK_SOURCE, classify};
use super::media;

/// The platform action of a direct send.
pub const ACTION_SEND_DIRECT: &str = "ilink_send_message";

/// The contract digest of one iLink send.
///
/// It states the real boundary: the platform answers with `ret`, a human does
/// not read, and a lost answer is reconciled instead of repeated.
pub const ILINK_SEND_CONTRACT: &[u8] =
    b"shenko.ilink/ilink-bot-send/1;no-blind-replay;provider-ret-zero-not-read";

/// The reply audience kinds this plugin serves.
pub const ILINK_REPLY_KINDS: &[ReplyKind] = &[ReplyKind {
    tag: "ilink-direct",
    id_hint: "微信用户ID",
    audience: "微信用户ID",
}];

/// The WeChat iLink channel plugin.
pub struct ILinkChannel {
    config: Arc<ILinkConfig>,
    api: ILinkApi,
    health: HealthState,
    replies: ReplyContextCache,
}

impl ILinkChannel {
    /// Bind the plugin to one iLink account.
    pub fn new(
        config: Arc<ILinkConfig>,
        api: ILinkApi,
        health: HealthState,
        replies: ReplyContextCache,
    ) -> Self {
        Self {
            config,
            api,
            health,
            replies,
        }
    }

    /// The configuration this plugin serves.
    pub fn config(&self) -> &ILinkConfig {
        &self.config
    }

    /// The API client this plugin dispatches through.
    pub fn api(&self) -> &ILinkApi {
        &self.api
    }

    /// What the iLink protocol can really do for this bot.
    pub fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            direct: true,
            group: false,
            thread: false,
            guild_channel: false,
            rich_text: false,
            attachments: true,
            edit: false,
            retract: false,
            typing: false,
            proactive: true,
            notes: vec![
                "the protocol has no edit endpoint and no retract endpoint".to_string(),
                "the protocol has no typing indicator".to_string(),
                "a group send does not exist; only a direct conversation is served".to_string(),
                "a reply must carry the conversation's own context_token".to_string(),
                "media is stored on the provider CDN encrypted with AES-128-ECB".to_string(),
            ],
        }
    }

    /// The current health of this connector.
    pub async fn health(&self) -> ChannelHealth {
        self.health.snapshot().await
    }

    /// The remembered reply contexts of this connector.
    pub fn replies(&self) -> &ReplyContextCache {
        &self.replies
    }

    /// The reply window this connector honours.
    pub fn reply_window(&self) -> Duration {
        Duration::from_secs(self.config.reply_window_seconds)
    }
}

/// The source labels of the iLink plugin.
fn owned_sources() -> Vec<String> {
    vec![ILINK_SOURCE.to_string()]
}

impl ChannelPlugin for ILinkChannel {
    fn channel_id(&self) -> &str {
        ILINK_CHANNEL_ID
    }

    fn sources(&self) -> &[String] {
        static SOURCES: std::sync::LazyLock<Vec<String>> =
            std::sync::LazyLock::new(owned_sources);
        &SOURCES
    }

    fn kinds(&self) -> &[ReplyKind] {
        ILINK_REPLY_KINDS
    }

    fn prompt_names(&self) -> &str {
        "微信用户ID"
    }

    fn classify(&self, source: &str, raw: &Value) -> Option<ChannelEnvelope> {
        if source != ILINK_SOURCE {
            return None;
        }
        classify(&self.config, raw)
    }

    fn staged_text(&self, envelope: &ChannelEnvelope) -> String {
        let who = envelope
            .actor
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(envelope.actor.platform_id.as_str());
        let body = envelope.content.text.trim();
        let attachments = if envelope.attachments.is_empty() {
            String::new()
        } else {
            let kinds = envelope
                .attachments
                .iter()
                .map(|attachment| {
                    attachment
                        .name
                        .clone()
                        .unwrap_or_else(|| attachment.kind.clone())
                })
                .collect::<Vec<_>>()
                .join("、");
            format!("（附 {kinds}）")
        };
        let text = if body.is_empty() {
            attachments
        } else {
            format!("{body}{attachments}")
        };
        match &envelope.reply_target {
            Some(target) => format!("[{who} · {}] {text}", target.label),
            None => format!("[{who}] {text}"),
        }
    }

    fn resolve_reply(&self, kind: &str, id: &str) -> Option<PublicationTarget> {
        if id.is_empty() {
            return None;
        }
        let scope = self.config.account_scope();
        if kind != ILINK_KIND_DIRECT {
            return None;
        }
        Some(
            PublicationTarget::new(ILINK_CHANNEL_ID, ILINK_KIND_DIRECT, id, scope)
                .with_scope(None, Some(id.to_string())),
        )
    }

    fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError> {
        if target.channel != ILINK_CHANNEL_ID {
            return Err(ChannelError::UnsupportedTarget {
                channel: target.channel.clone(),
                kind: target.kind.clone(),
            });
        }
        if target.tenant.is_some() {
            return Err(ChannelError::UnsupportedTarget {
                channel: target.channel.clone(),
                kind: target.kind.clone(),
            });
        }
        let scope = self.config.account_scope();
        if !target.account.is_empty() && target.account != scope {
            return Err(ChannelError::UnsupportedTarget {
                channel: target.channel.clone(),
                kind: target.kind.clone(),
            });
        }
        if target.id.is_empty() {
            return Err(ChannelError::UnresolvedReply {
                channel: target.channel.clone(),
                id: target.id.clone(),
            });
        }
        if target.kind != ILINK_KIND_DIRECT {
            return Err(ChannelError::UnsupportedTarget {
                channel: target.channel.clone(),
                kind: target.kind.clone(),
            });
        }
        // A reply must carry the conversation's own context token; the
        // connector remembers it from the inbound message instead of
        // inventing one.
        let conversation = target
            .conversation
            .clone()
            .unwrap_or_else(|| target.id.clone());
        let mut params = json!({ "to_user_id": target.id.clone() });
        if let Some(context) = self.replies.get(&conversation) {
            if let Some(token) = context.context_token {
                params["context_token"] = Value::String(token);
            }
        }
        // A client id the provider can use to deduplicate one send.
        params["client_id"] = Value::String(new_client_id());
        Ok(ChannelDispatch {
            channel: ILINK_CHANNEL_ID.to_string(),
            action: ChannelAction::Send,
            platform_action: ACTION_SEND_DIRECT.to_string(),
            params,
            account: scope,
            tenant: None,
            conversation: Some(conversation),
            attachments: vec![],
            contract_digest: Digest::blake3(ILINK_SEND_CONTRACT),
            acceptance_predicate: "ilink_ret_zero".to_string(),
            echo: None,
        })
    }

    fn dispatch(
        &self,
        dispatch: &ChannelDispatch,
        content: &str,
    ) -> BoxFuture<'static, Result<ChannelReceipt, ChannelError>> {
        let api = self.api.clone();
        let dispatch = dispatch.clone();
        let content = content.to_string();
        Box::pin(async move {
            if dispatch.channel != ILINK_CHANNEL_ID {
                return Err(ChannelError::UnsupportedTarget {
                    channel: dispatch.channel.clone(),
                    kind: dispatch.platform_action.clone(),
                });
            }
            match dispatch.action {
                ChannelAction::Send => {
                    let to_user_id = dispatch
                        .params
                        .get("to_user_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let context_token = dispatch
                        .params
                        .get("context_token")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let client_id = dispatch
                        .params
                        .get("client_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(new_client_id);
                    if let Some(attachment) = dispatch.attachments.first() {
                        let kind = match attachment.kind.as_str() {
                            "image" => MediaKind::Image,
                            "video" => MediaKind::Video,
                            "voice" => MediaKind::Voice,
                            _ => MediaKind::File,
                        };
                        let bytes = crate::common::read_attachment_bytes(api.http(), attachment)
                            .await
                            .map_err(|detail| ChannelError::Rejected {
                                channel: dispatch.channel.clone(),
                                detail,
                            })?;
                        let uploaded = media::upload(&api, &to_user_id, kind, &bytes)
                            .await
                            .map_err(|error| map_api_error(&dispatch.channel, error))?;
                        let item = media_item(&attachment.kind, &uploaded, attachment);
                        let answer = api
                            .send_media(&to_user_id, context_token.as_deref(), &client_id, &item)
                            .await
                            .map_err(|error| map_api_error(&dispatch.channel, error))?;
                        return Ok(receipt_of(dispatch, answer));
                    }
                    let answer = api
                        .send_text(
                            &to_user_id,
                            context_token.as_deref(),
                            &client_id,
                            &content,
                        )
                        .await
                        .map_err(|error| map_api_error(&dispatch.channel, error))?;
                    Ok(receipt_of(dispatch, answer))
                }
                other => Err(ChannelError::UnsupportedTarget {
                    channel: dispatch.channel.clone(),
                    kind: format!("{other:?}"),
                }),
            }
        })
    }
}

/// The media item of one uploaded attachment, in the provider's own shape.
fn media_item(
    kind: &str,
    uploaded: &media::UploadedFile,
    attachment: &shenko_types::channel::ChannelAttachment,
) -> Value {
    let reference = uploaded.media.as_reference();
    match kind {
        "image" => json!({ "type": 2, "image_item": { "media": reference } }),
        "video" => json!({
            "type": 5,
            "video_item": { "media": reference, "video_size": uploaded.cipher_size as i64 },
        }),
        "voice" => json!({ "type": 3, "voice_item": { "media": reference } }),
        _ => json!({
            "type": 4,
            "file_item": {
                "media": reference,
                "file_name": attachment.name.clone().unwrap_or_else(|| "file".to_string()),
                "len": uploaded.raw_size.to_string(),
            },
        }),
    }
}

/// Turn a platform answer into a receipt.
///
/// The acceptance verdict is the provider's own `ret` value: only `ret == 0`
/// means the provider accepted the send.
fn receipt_of(dispatch: ChannelDispatch, answer: Value) -> ChannelReceipt {
    let retcode = answer.get("ret").and_then(Value::as_i64).unwrap_or(0);
    let accepted = retcode == 0;
    ChannelReceipt {
        status: if accepted { "ok" } else { "failed" }.to_string(),
        retcode,
        accepted,
        provider_message_id: answer
            .get("message_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        raw: answer,
        observed_at: chrono::Utc::now(),
    }
}

/// Map an API failure onto the channel error vocabulary.
pub fn map_api_error(channel: &str, error: ILinkError) -> ChannelError {
    match error {
        ILinkError::Rejected { message, .. } => ChannelError::Rejected {
            channel: channel.to_string(),
            detail: message,
        },
        ILinkError::Unknown(detail) => ChannelError::Unknown {
            channel: channel.to_string(),
            detail,
        },
        ILinkError::Incomplete(detail) => ChannelError::Transport {
            channel: channel.to_string(),
            detail,
        },
    }
}

/// Register the iLink plugin into a registry.
pub fn register(
    registry: &mut ChannelRegistry,
    plugin: Arc<ILinkChannel>,
) -> Result<(), ChannelRegistryError> {
    registry.register(plugin)
}

/// The reply kinds of the iLink plugin, for callers that build their own
/// registry.
pub fn reply_kinds() -> &'static [ReplyKind] {
    ILINK_REPLY_KINDS
}

/// A fresh client-generated request id.
fn new_client_id() -> String {
    media::random_bytes(8)
        .map(|bytes| hex::encode(bytes))
        .unwrap_or_else(|_| format!("{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)))
}
