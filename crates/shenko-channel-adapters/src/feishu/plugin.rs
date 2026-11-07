//! The Feishu/Lark channel plugin — SPEC §15.3, §16.
//!
//! The plugin owns everything Feishu-specific: the app-ticket auth, the
//! event-dispatcher ingress, the typed message builder and the edit, recall
//! and typing endpoints. The core only ever sees the normalized envelope and
//! the opaque target.
//!
//! The platform's real boundary:
//!
//! * a message is a `post` document of typed blocks, not a plain string;
//! * a send names a `receive_id` (a chat or a user), a reply names the
//!   message it answers, an edit is a `PATCH`, a recall is a `DELETE`;
//! * the typing indicator is its own endpoint, scoped to a chat;
//! * a thread audience is the thread's root message: answering it posts into
//!   the thread.
//!
//! Ordinary reachability is open; administration is a separate owner-set fact
//! (`config.admin_open_ids`), never derived from a group role or the app
//! ticket.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use shenko_types::channel::{
    AttachmentError, BoxFuture, ChannelAction, ChannelAttachment, ChannelDispatch, ChannelEnvelope,
    ChannelError, ChannelPlugin, ChannelReceipt, ChannelRegistry, ChannelRegistryError, ReplyKind,
    ResolvedAttachment,
};
use shenko_types::object::Digest;
use shenko_types::output::PublicationTarget;

use crate::common::{
    ChannelCapabilities, ChannelHealth, HealthState, ReplyContextCache, text_field,
};

use super::api::{FeishuApi, FeishuError, ReceiveIdType};
use super::config::FeishuConfig;
use super::events::{
    FEISHU_CHANNEL_ID, FEISHU_KIND_CHAT, FEISHU_KIND_THREAD, FEISHU_SOURCE, classify,
};
use super::media::{MediaBlock, post_content};

/// The platform action of a chat send.
pub const ACTION_SEND_MESSAGE: &str = "feishu_send_message";
/// The platform action of a thread reply.
pub const ACTION_REPLY_MESSAGE: &str = "feishu_reply_message";
/// The platform action of an edit.
pub const ACTION_EDIT: &str = "feishu_edit_message";
/// The platform action of a recall.
pub const ACTION_RETRACT: &str = "feishu_retract_message";
/// The platform action of a typing indicator.
pub const ACTION_TYPING: &str = "feishu_typing";

/// The contract digest of one Feishu send.
///
/// It states the real boundary: the platform answers with `code`, a human
/// does not read, and a lost answer is reconciled instead of repeated.
pub const FEISHU_SEND_CONTRACT: &[u8] =
    b"shenko.feishu/openapi-v1-send/1;no-blind-replay;provider-code-zero-not-read";

/// The contract digest of one Feishu edit.
pub const FEISHU_EDIT_CONTRACT: &[u8] =
    b"shenko.feishu/openapi-v1-edit/1;edit-replaces-blocks;window-platform-owned";

/// The contract digest of one Feishu retract.
pub const FEISHU_RETRACT_CONTRACT: &[u8] =
    b"shenko.feishu/openapi-v1-retract/1;recall-is-message-scoped";

/// The contract digest of one Feishu typing indicator.
pub const FEISHU_TYPING_CONTRACT: &[u8] =
    b"shenko.feishu/openapi-v1-typing/1;chat-scoped";

/// The reply audience kinds this plugin serves.
pub const FEISHU_REPLY_KINDS: &[ReplyKind] = &[
    ReplyKind {
        tag: "feishu-chat",
        id_hint: "飞书会话ID",
        audience: "飞书会话ID",
    },
    ReplyKind {
        tag: "feishu-thread",
        id_hint: "飞书话题消息ID",
        audience: "飞书话题消息ID",
    },
];

/// The Feishu/Lark channel plugin.
pub struct FeishuChannel {
    config: Arc<FeishuConfig>,
    api: FeishuApi,
    health: HealthState,
    replies: ReplyContextCache,
}

impl FeishuChannel {
    /// Bind the plugin to one app.
    pub fn new(
        config: Arc<FeishuConfig>,
        api: FeishuApi,
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
    pub fn config(&self) -> &FeishuConfig {
        &self.config
    }

    /// The API client this plugin dispatches through.
    pub fn api(&self) -> &FeishuApi {
        &self.api
    }

    /// What the Feishu platform can really do for this bot.
    pub fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            direct: true,
            group: true,
            thread: true,
            guild_channel: false,
            rich_text: true,
            attachments: true,
            edit: true,
            retract: true,
            typing: true,
            proactive: true,
            notes: vec![
                "a message is a post document of typed blocks, not a plain string".to_string(),
                "a thread audience is the thread's root message; answering it posts into the thread".to_string(),
                "an edit replaces the blocks and a recall removes the message; both are message-scoped".to_string(),
                "the typing indicator is a chat-scoped endpoint of its own".to_string(),
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

/// The source labels of the Feishu plugin.
fn owned_sources() -> Vec<String> {
    vec![FEISHU_SOURCE.to_string()]
}

impl ChannelPlugin for FeishuChannel {
    fn channel_id(&self) -> &str {
        FEISHU_CHANNEL_ID
    }

    fn sources(&self) -> &[String] {
        static SOURCES: std::sync::LazyLock<Vec<String>> =
            std::sync::LazyLock::new(owned_sources);
        &SOURCES
    }

    fn kinds(&self) -> &[ReplyKind] {
        FEISHU_REPLY_KINDS
    }

    fn prompt_names(&self) -> &str {
        "飞书会话ID 或话题消息ID"
    }

    fn classify(&self, source: &str, raw: &Value) -> Option<ChannelEnvelope> {
        if source != FEISHU_SOURCE {
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
        let target = match kind {
            FEISHU_KIND_CHAT => PublicationTarget::new(
                FEISHU_CHANNEL_ID,
                FEISHU_KIND_CHAT,
                id,
                scope,
            )
            .with_scope(None, Some(id.to_string())),
            FEISHU_KIND_THREAD => PublicationTarget::new(
                FEISHU_CHANNEL_ID,
                FEISHU_KIND_THREAD,
                id,
                scope,
            )
            .with_scope(None, Some(id.to_string())),
            _ => return None,
        };
        Some(target)
    }

    fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError> {
        if target.channel != FEISHU_CHANNEL_ID {
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
        let (platform_action, params) = match target.kind.as_str() {
            FEISHU_KIND_CHAT => (
                ACTION_SEND_MESSAGE,
                json!({
                    "receive_id_type": "chat_id",
                    "receive_id": target.id.clone(),
                }),
            ),
            FEISHU_KIND_THREAD => (
                ACTION_REPLY_MESSAGE,
                json!({ "reply_to": target.id.clone() }),
            ),
            other => {
                return Err(ChannelError::UnsupportedTarget {
                    channel: target.channel.clone(),
                    kind: other.to_string(),
                });
            }
        };
        let conversation = target
            .conversation
            .clone()
            .unwrap_or_else(|| target.id.clone());
        Ok(ChannelDispatch {
            channel: FEISHU_CHANNEL_ID.to_string(),
            action: ChannelAction::Send,
            platform_action: platform_action.to_string(),
            params,
            account: scope,
            tenant: None,
            conversation: Some(conversation),
            attachments: vec![],
            contract_digest: Digest::blake3(FEISHU_SEND_CONTRACT),
            acceptance_predicate: "feishu_code_zero".to_string(),
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
            if dispatch.channel != FEISHU_CHANNEL_ID {
                return Err(ChannelError::UnsupportedTarget {
                    channel: dispatch.channel.clone(),
                    kind: dispatch.platform_action.clone(),
                });
            }
            match dispatch.action {
                ChannelAction::Send => {
                    let media = upload_attachments(&api, &dispatch).await?;
                    let body = post_content(&content, &media);
                    let answer = match dispatch.platform_action.as_str() {
                        ACTION_SEND_MESSAGE => {
                            let receive_id_type = match dispatch
                                .params
                                .get("receive_id_type")
                                .and_then(Value::as_str)
                            {
                                Some("open_id") => ReceiveIdType::User,
                                _ => ReceiveIdType::Chat,
                            };
                            let receive_id = dispatch
                                .params
                                .get("receive_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            api.send_message(
                                receive_id_type,
                                &receive_id,
                                &body,
                                dispatch
                                    .params
                                    .get("idempotency_key")
                                    .and_then(Value::as_str),
                            )
                            .await
                        }
                        ACTION_REPLY_MESSAGE => {
                            let reply_to = dispatch
                                .params
                                .get("reply_to")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            api.reply_message(&reply_to, &body, None).await
                        }
                        other => {
                            return Err(ChannelError::UnsupportedTarget {
                                channel: dispatch.channel.clone(),
                                kind: other.to_string(),
                            });
                        }
                    }
                    .map_err(|error| map_api_error(&dispatch.channel, error))?;
                    Ok(receipt_of(dispatch, answer))
                }
                ChannelAction::Edit => {
                    let message_id = dispatch
                        .params
                        .get("message_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let body = post_content(&content, &[]);
                    let answer = api
                        .edit_message(&message_id, &body)
                        .await
                        .map_err(|error| map_api_error(&dispatch.channel, error))?;
                    Ok(receipt_of(dispatch, answer))
                }
                ChannelAction::Retract => {
                    let message_id = dispatch
                        .params
                        .get("message_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let answer = api
                        .recall_message(&message_id)
                        .await
                        .map_err(|error| map_api_error(&dispatch.channel, error))?;
                    Ok(receipt_of(dispatch, answer))
                }
                ChannelAction::Typing => {
                    let chat_id = dispatch
                        .params
                        .get("chat_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let answer = api
                        .typing(&chat_id)
                        .await
                        .map_err(|error| map_api_error(&dispatch.channel, error))?;
                    Ok(receipt_of(dispatch, answer))
                }
            }
        })
    }

    /// Download one drive object through the message it arrived with.
    ///
    /// Feishu does not publish a fetchable URL for a message resource: the
    /// bytes are served by the resource endpoint, keyed by the message id, the
    /// `file_key` the platform named and the resource kind. All three were
    /// captured at classification time, and the call carries the app's own
    /// tenant ticket, so this is the installed account's authenticated read
    /// rather than a fetch of an arbitrary address.
    fn resolve_attachment(
        &self,
        attachment: &ChannelAttachment,
    ) -> BoxFuture<'static, Result<ResolvedAttachment, AttachmentError>> {
        let attachment = attachment.clone();
        let api = self.api.clone();
        Box::pin(async move {
            let params = attachment.locator.params.as_ref();
            let message_id = params
                .and_then(|params| text_field(params, "message_id"));
            let Some(message_id) = message_id else {
                return Err(AttachmentError::Refused {
                    attachment_id: attachment.attachment_id.clone(),
                    detail: "the message carried no platform message id for its resource".to_string(),
                });
            };
            let Some(file_key) = attachment
                .locator
                .path
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_owned)
            else {
                return Err(AttachmentError::Refused {
                    attachment_id: attachment.attachment_id.clone(),
                    detail: "the platform named no resource key for this attachment".to_string(),
                });
            };
            // The endpoint serves exactly two resource kinds. The kind captured
            // with the locator wins; a locator without one falls back to its
            // own attachment kind, which the platform's `type` accepts.
            let resource_type = params
                .and_then(|params| text_field(params, "resource_type"))
                .unwrap_or_else(|| {
                    if attachment.kind == "image" {
                        "image".to_string()
                    } else {
                        "file".to_string()
                    }
                });
            // `download_media` already maps platform refusal into `Err`, so
            // every `Ok` is a delivered payload.
            let crate::common::MediaAnswer::Delivered { media_type, bytes } = api
                .download_media(
                    FEISHU_CHANNEL_ID,
                    &attachment.attachment_id,
                    &message_id,
                    &file_key,
                    &resource_type,
                )
                .await?
            else {
                // `download_media` never returns `Ok(Refused{..})`.
                unreachable!()
            };
            crate::common::resolved_from_media(&attachment, media_type, bytes)
        })
    }
}

/// Upload every attachment of one dispatch and return the media blocks.
async fn upload_attachments(
    api: &FeishuApi,
    dispatch: &ChannelDispatch,
) -> Result<Vec<MediaBlock>, ChannelError> {
    let mut media = Vec::new();
    for attachment in &dispatch.attachments {
        let bytes = crate::common::read_attachment_bytes(api.http(), attachment)
            .await
            .map_err(|detail| ChannelError::Rejected {
                channel: dispatch.channel.clone(),
                detail,
            })?;
        let file_name = attachment
            .name
            .clone()
            .unwrap_or_else(|| attachment.kind.clone());
        let uploaded = api
            .upload_media(&file_name, &attachment.kind, &bytes)
            .await
            .map_err(|error| map_api_error(&dispatch.channel, error))?;
        media.push(MediaBlock {
            file_key: uploaded.file_key,
            kind: attachment.kind.clone(),
            name: attachment.name.clone(),
        });
    }
    Ok(media)
}

/// Turn a platform answer into a receipt.
///
/// The acceptance verdict is the platform's own `code`: only `code == 0`
/// means the platform accepted the call.
fn receipt_of(dispatch: ChannelDispatch, answer: Value) -> ChannelReceipt {
    let retcode = answer.get("code").and_then(Value::as_i64).unwrap_or(0);
    let accepted = retcode == 0;
    let provider_message_id = answer
        .get("data")
        .and_then(|data| text_field(data, "message_id"));
    ChannelReceipt {
        status: if accepted { "ok" } else { "failed" }.to_string(),
        retcode,
        accepted,
        provider_message_id,
        raw: answer,
        observed_at: chrono::Utc::now(),
    }
}

/// Map an API failure onto the channel error vocabulary.
pub fn map_api_error(channel: &str, error: FeishuError) -> ChannelError {
    match error {
        FeishuError::Rejected { message, .. } => ChannelError::Rejected {
            channel: channel.to_string(),
            detail: message,
        },
        FeishuError::Unknown(detail) => ChannelError::Unknown {
            channel: channel.to_string(),
            detail,
        },
        FeishuError::Incomplete(detail) => ChannelError::Transport {
            channel: channel.to_string(),
            detail,
        },
    }
}

/// Register the Feishu plugin into a registry.
pub fn register(
    registry: &mut ChannelRegistry,
    plugin: Arc<FeishuChannel>,
) -> Result<(), ChannelRegistryError> {
    registry.register(plugin)
}

/// The reply kinds of the Feishu plugin, for callers that build their own
/// registry.
pub fn reply_kinds() -> &'static [ReplyKind] {
    FEISHU_REPLY_KINDS
}
