//! The official QQ bot channel plugin — SPEC §15.3, §16.
//!
//! The plugin owns everything QQ-specific: the OpenAPI endpoints, the
//! OpenID namespaces, the reply audience kinds and the acceptance predicate
//! of a send. The core only ever sees the normalized envelope and the opaque
//! target.
//!
//! Ordinary reachability is open: any user who can reach the account may
//! talk. Administration is a separate, owner-set fact
//! (`config.admin_openids`); it is never derived from a group role, the app
//! ticket or familiarity.

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

use super::api::{MediaScope, QqApiError, QqOfficialApi};
use super::config::QqOfficialConfig;
use super::events::{
    QQ_OFFICIAL_CHANNEL_ID, QQ_OFFICIAL_KIND_CHANNEL, QQ_OFFICIAL_KIND_GROUP,
    QQ_OFFICIAL_KIND_PRIVATE, QQ_OFFICIAL_SOURCE,
};

/// The platform action of a single-chat send.
pub const ACTION_SEND_PRIVATE: &str = "send_private_message";
/// The platform action of a group send.
pub const ACTION_SEND_GROUP: &str = "send_group_message";
/// The platform action of a channel send.
pub const ACTION_SEND_CHANNEL: &str = "send_channel_message";
/// The platform action of an edit.
pub const ACTION_EDIT: &str = "edit_message";
/// The platform action of a retract.
pub const ACTION_RETRACT: &str = "retract_message";
/// The platform action of a typing indicator.
pub const ACTION_TYPING: &str = "send_typing";

/// The contract digest of one official QQ send.
///
/// It states the real boundary: the platform answers, a human does not read,
/// and a lost answer is reconciled instead of repeated.
pub const QQ_OFFICIAL_SEND_CONTRACT: &[u8] =
    b"shenko.qq-official/openapi-v2-send/1;no-blind-replay;provider-accepted-not-read";

/// The contract digest of one official QQ edit.
pub const QQ_OFFICIAL_EDIT_CONTRACT: &[u8] =
    b"shenko.qq-official/openapi-v2-edit/1;edit-window-is-platform-owned";

/// The contract digest of one official QQ retract.
pub const QQ_OFFICIAL_RETRACT_CONTRACT: &[u8] =
    b"shenko.qq-official/openapi-v2-retract/1;retract-window-is-platform-owned";

/// The contract digest of one official QQ typing indicator.
pub const QQ_OFFICIAL_TYPING_CONTRACT: &[u8] =
    b"shenko.qq-official/openapi-v2-typing/1;channel-only";

/// The reply audience kinds this plugin serves.
pub const QQ_OFFICIAL_REPLY_KINDS: &[ReplyKind] = &[
    ReplyKind {
        tag: QQ_OFFICIAL_KIND_GROUP,
        id_hint: "群开放ID",
        audience: "QQ 群",
    },
    ReplyKind {
        tag: QQ_OFFICIAL_KIND_PRIVATE,
        id_hint: "用户开放ID",
        audience: "QQ 单聊",
    },
    ReplyKind {
        tag: QQ_OFFICIAL_KIND_CHANNEL,
        id_hint: "频道ID",
        audience: "QQ 频道",
    },
];

/// The official QQ bot channel plugin.
pub struct QqOfficialChannel {
    config: Arc<QqOfficialConfig>,
    api: QqOfficialApi,
    health: HealthState,
    replies: ReplyContextCache,
}

impl QqOfficialChannel {
    /// Bind the plugin to one bot account.
    pub fn new(
        config: Arc<QqOfficialConfig>,
        api: QqOfficialApi,
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
    pub fn config(&self) -> &QqOfficialConfig {
        &self.config
    }

    /// The API client this plugin dispatches through.
    pub fn api(&self) -> &QqOfficialApi {
        &self.api
    }

    /// What the official platform can really do for this bot.
    ///
    /// The differences are platform facts, not preferences:
    ///
    /// * typing exists only in a guild channel;
    /// * a group send answers with a message id that covers the whole group
    ///   audience, while a single chat answers for one person;
    /// * the passive reply window belongs to the platform and expires;
    /// * whether every group message arrives (not only the ones that mention
    ///   the bot) depends on a platform-side permission.
    pub fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            direct: true,
            group: true,
            thread: false,
            guild_channel: true,
            rich_text: true,
            attachments: true,
            edit: true,
            retract: true,
            typing: true,
            proactive: true,
            notes: vec![
                "typing is a channel-only endpoint; a single chat or group has none".to_string(),
                "the passive reply window and the rate limits belong to the platform".to_string(),
                "full group traffic needs a platform-side permission; by default only messages that mention the bot arrive".to_string(),
                "the official platform issues per-application OpenIDs; the numeric QQ number is not available here".to_string(),
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

    /// The scope an upload belongs to, derived from the reply kind.
    fn media_scope(kind: &str, id: &str) -> MediaScope {
        match kind {
            QQ_OFFICIAL_KIND_GROUP => MediaScope::Group(id.to_string()),
            _ => MediaScope::User(id.to_string()),
        }
    }
}

/// The source labels of the official QQ plugin.
fn owned_sources() -> Vec<String> {
    vec![QQ_OFFICIAL_SOURCE.to_string()]
}

impl ChannelPlugin for QqOfficialChannel {
    fn channel_id(&self) -> &str {
        QQ_OFFICIAL_CHANNEL_ID
    }

    fn sources(&self) -> &[String] {
        // The registry keeps the labels unique; the value is computed once.
        static SOURCES: std::sync::LazyLock<Vec<String>> =
            std::sync::LazyLock::new(owned_sources);
        &SOURCES
    }

    fn kinds(&self) -> &[ReplyKind] {
        QQ_OFFICIAL_REPLY_KINDS
    }

    fn prompt_names(&self) -> &str {
        "QQ 群开放ID、用户开放ID 或频道ID"
    }

    fn classify(&self, source: &str, raw: &Value) -> Option<ChannelEnvelope> {
        if source != QQ_OFFICIAL_SOURCE {
            return None;
        }
        super::events::classify(&self.config, raw)
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
            QQ_OFFICIAL_KIND_GROUP => PublicationTarget::new(
                QQ_OFFICIAL_CHANNEL_ID,
                QQ_OFFICIAL_KIND_GROUP,
                id,
                scope,
            )
            .with_scope(None, Some(id.to_string())),
            QQ_OFFICIAL_KIND_PRIVATE => PublicationTarget::new(
                QQ_OFFICIAL_CHANNEL_ID,
                QQ_OFFICIAL_KIND_PRIVATE,
                id,
                scope,
            )
            .with_scope(None, Some(id.to_string())),
            QQ_OFFICIAL_KIND_CHANNEL => PublicationTarget::new(
                QQ_OFFICIAL_CHANNEL_ID,
                QQ_OFFICIAL_KIND_CHANNEL,
                id,
                scope,
            )
            .with_scope(None, Some(id.to_string())),
            _ => return None,
        };
        Some(target)
    }

    fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError> {
        if target.channel != QQ_OFFICIAL_CHANNEL_ID {
            return Err(ChannelError::UnsupportedTarget {
                channel: target.channel.clone(),
                kind: target.kind.clone(),
            });
        }
        if target.tenant.is_some() {
            // The official platform has no tenant namespace; a target that
            // carries one belongs to another platform.
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
            QQ_OFFICIAL_KIND_PRIVATE => (
                ACTION_SEND_PRIVATE,
                json!({ "user_openid": target.id.clone() }),
            ),
            QQ_OFFICIAL_KIND_GROUP => (
                ACTION_SEND_GROUP,
                json!({ "group_openid": target.id.clone() }),
            ),
            QQ_OFFICIAL_KIND_CHANNEL => (
                ACTION_SEND_CHANNEL,
                json!({ "channel_id": target.id.clone() }),
            ),
            other => {
                return Err(ChannelError::UnsupportedTarget {
                    channel: target.channel.clone(),
                    kind: other.to_string(),
                });
            }
        };
        // A passive answer must carry the id of the message it answers. The
        // platform gives that id with the inbound event; the connector
        // remembers it per conversation instead of inventing one.
        let mut params = params;
        let conversation = target
            .conversation
            .clone()
            .unwrap_or_else(|| target.id.clone());
        if let Some(context) = self.replies.get(&conversation) {
            params["msg_id"] = Value::String(context.message_id.clone());
        }
        params["msg_seq"] = json!(1);
        Ok(ChannelDispatch {
            channel: QQ_OFFICIAL_CHANNEL_ID.to_string(),
            action: ChannelAction::Send,
            platform_action: platform_action.to_string(),
            params,
            account: scope,
            tenant: None,
            conversation: Some(conversation),
            attachments: vec![],
            contract_digest: Digest::blake3(QQ_OFFICIAL_SEND_CONTRACT),
            acceptance_predicate: "qq_official_err_code_zero".to_string(),
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
            if dispatch.channel != QQ_OFFICIAL_CHANNEL_ID {
                return Err(ChannelError::UnsupportedTarget {
                    channel: dispatch.channel.clone(),
                    kind: dispatch.platform_action.clone(),
                });
            }
            match dispatch.action {
                ChannelAction::Send => {
                    // A dispatch that carries attachments sends media, not
                    // text: the platform needs an uploaded `file_info`
                    // before it accepts the message.
                    if let Some(attachment) = dispatch.attachments.first() {
                        let scope = match dispatch.platform_action.as_str() {
                            ACTION_SEND_GROUP => MediaScope::Group(
                                dispatch
                                    .params
                                    .get("group_openid")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            ),
                            _ => MediaScope::User(
                                dispatch
                                    .params
                                    .get("user_openid")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            ),
                        };
                        let bytes = crate::common::read_attachment_bytes(api.http(), attachment)
                            .await
                            .map_err(|detail| ChannelError::Rejected {
                                channel: dispatch.channel.clone(),
                                detail,
                            })?;
                        let file_name = attachment
                            .name
                            .clone()
                            .unwrap_or_else(|| "attachment".to_string());
                        let file_type = match attachment.kind.as_str() {
                            "image" => 1,
                            "video" => 2,
                            "voice" => 3,
                            _ => 4,
                        };
                        let uploaded = api
                            .upload_media(&scope, &bytes, &file_name, file_type)
                            .await
                            .map_err(|error| map_api_error(&dispatch.channel, error))?;
                        let answer = api
                            .send_media(
                                &scope,
                                &uploaded.file_info,
                                dispatch.params.get("msg_id").and_then(Value::as_str),
                                dispatch
                                    .params
                                    .get("msg_seq")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(1) as u32,
                            )
                            .await
                            .map_err(|error| map_api_error(&dispatch.channel, error))?;
                        return Ok(receipt_of(dispatch, answer));
                    }
                    let answer = match dispatch.platform_action.as_str() {
                        ACTION_SEND_PRIVATE => {
                            let openid = dispatch
                                .params
                                .get("user_openid")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            api.send_private(
                                openid,
                                &content,
                                dispatch.params.get("msg_id").and_then(Value::as_str),
                                dispatch
                                    .params
                                    .get("msg_seq")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(1) as u32,
                            )
                            .await
                        }
                        ACTION_SEND_GROUP => {
                            let group = dispatch
                                .params
                                .get("group_openid")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            api.send_group(
                                group,
                                &content,
                                dispatch.params.get("msg_id").and_then(Value::as_str),
                                dispatch
                                    .params
                                    .get("msg_seq")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(1) as u32,
                            )
                            .await
                        }
                        ACTION_SEND_CHANNEL => {
                            let channel = dispatch
                                .params
                                .get("channel_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            api.send_channel(
                                channel,
                                &content,
                                dispatch.params.get("msg_id").and_then(Value::as_str),
                            )
                            .await
                        }
                        other => {
                            return Err(ChannelError::UnsupportedTarget {
                                channel: dispatch.channel.clone(),
                                kind: other.to_string(),
                            });
                        }
                    };
                    Ok(receipt_of(dispatch, answer?))
                }
                ChannelAction::Edit => {
                    let message_id = dispatch
                        .params
                        .get("message_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let answer = api
                        .edit_message(
                            message_id,
                            &content,
                            dispatch
                                .params
                                .get("msg_seq")
                                .and_then(Value::as_u64)
                                .unwrap_or(1) as u32,
                        )
                        .await?;
                    Ok(receipt_of(dispatch, answer))
                }
                ChannelAction::Retract => {
                    let message_id = dispatch
                        .params
                        .get("message_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let scope = dispatch
                        .params
                        .get("scope")
                        .and_then(Value::as_str)
                        .unwrap_or("private");
                    let answer = match scope {
                        "group" => {
                            let group = dispatch
                                .params
                                .get("group_openid")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            api.retract_group(group, message_id).await?
                        }
                        _ => {
                            let openid = dispatch
                                .params
                                .get("user_openid")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            api.retract_private(openid, message_id).await?
                        }
                    };
                    Ok(receipt_of(dispatch, answer))
                }
                ChannelAction::Typing => {
                    let channel = dispatch
                        .params
                        .get("channel_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let answer = api.typing(channel).await?;
                    Ok(receipt_of(dispatch, answer))
                }
            }
        })
    }
}

/// Turn a platform answer into a receipt.
///
/// The acceptance verdict is the platform's own error code: the official API
/// answers `200 OK` with `err_code` inside the body, and only `err_code == 0`
/// means the platform accepted the request.
fn receipt_of(dispatch: ChannelDispatch, answer: Value) -> ChannelReceipt {
    let retcode = answer
        .get("err_code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let accepted = retcode == 0;
    ChannelReceipt {
        status: if accepted { "ok" } else { "failed" }.to_string(),
        retcode,
        accepted,
        provider_message_id: answer
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        raw: answer,
        observed_at: chrono::Utc::now(),
    }
}

/// Map an API failure onto the channel error vocabulary.
pub fn map_api_error(channel: &str, error: QqApiError) -> ChannelError {
    match error {
        QqApiError::Rejected { message, .. } => ChannelError::Rejected {
            channel: channel.to_string(),
            detail: message,
        },
        QqApiError::Unknown(detail) => ChannelError::Unknown {
            channel: channel.to_string(),
            detail,
        },
        QqApiError::Incomplete(detail) => ChannelError::Transport {
            channel: channel.to_string(),
            detail,
        },
    }
}

/// Register the official QQ plugin into a registry.
pub fn register(
    registry: &mut ChannelRegistry,
    plugin: Arc<QqOfficialChannel>,
) -> Result<(), ChannelRegistryError> {
    registry.register(plugin)
}

/// The reply kinds of the official QQ plugin, for callers that build their
/// own registry.
pub fn reply_kinds() -> &[ReplyKind] {
    QQ_OFFICIAL_REPLY_KINDS
}
