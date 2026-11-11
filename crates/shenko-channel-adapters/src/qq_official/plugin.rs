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

use shenko_types::capabilities::ChannelCapabilities;
use shenko_types::channel::{
    AttachmentError, BoxFuture, ChannelAction, ChannelAttachment, ChannelDispatch, ChannelEnvelope,
    ChannelError, ChannelPlugin, ChannelReceipt, ChannelRegistry, ChannelRegistryError, ReplyKind,
    ResolvedAttachment,
};
use shenko_types::object::Digest;
use shenko_types::output::PublicationTarget;

use crate::common::{ChannelHealth, HealthState, ReplyContextCache};

use super::api::{MediaScope, QqApiError, QqOfficialApi};
use super::config::QqOfficialConfig;
use super::events::{
    QQ_OFFICIAL_CHANNEL_ID, QQ_OFFICIAL_KIND_CHANNEL, QQ_OFFICIAL_KIND_GROUP,
    QQ_OFFICIAL_KIND_PRIVATE, QQ_OFFICIAL_KIND_RETRACT, QQ_OFFICIAL_SOURCE,
};

/// The platform action of a single-chat send.
pub const ACTION_SEND_PRIVATE: &str = "send_private_message";
/// The platform action of a group send.
pub const ACTION_SEND_GROUP: &str = "send_group_message";
/// The platform action of a channel send.
pub const ACTION_SEND_CHANNEL: &str = "send_channel_message";
/// How long the platform still accepts a retract for a message it sent.
///
/// The official docs state it plainly: a bot can retract its own message,
/// but not one that was sent more than two minutes ago.
pub const RETRACT_WINDOW: chrono::Duration = chrono::Duration::minutes(2);

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
    ReplyKind {
        tag: QQ_OFFICIAL_KIND_RETRACT,
        id_hint: "会话ID",
        audience: "撤回本账号在该会话的最后一条消息",
    },
];

/// The official QQ bot channel plugin.
pub struct QqOfficialChannel {
    config: Arc<QqOfficialConfig>,
    api: QqOfficialApi,
    health: HealthState,
    replies: ReplyContextCache,
    /// The bot's own last message per conversation: what an edit or a
    /// retract addresses. The platform edits and retracts by message id, and
    /// the bot may only touch its own message.
    own: Arc<parking_lot::Mutex<std::collections::HashMap<String, OwnMessage>>>,
}

/// The bot's own last message in one conversation.
#[derive(Clone, Debug)]
pub(crate) struct OwnMessage {
    message_id: String,
    /// "group" or "private": which audience the message belongs to.
    scope: String,
    /// The group openid or the user openid the message was sent to.
    audience: String,
    /// When the bot sent it. The platform's own windows decide how long an
    /// edit or a retract can still succeed.
    at: chrono::DateTime<chrono::Utc>,
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
            own: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Remember the message this bot just sent, so a later retract
    /// has something real to address.
    fn remember_own(
        own: &Arc<parking_lot::Mutex<std::collections::HashMap<String, OwnMessage>>>,
        window: chrono::Duration,
        conversation: &str,
        message_id: &str,
        scope: &str,
        audience: &str,
    ) {
        Self::remember_own_at(
            own,
            window,
            conversation,
            message_id,
            scope,
            audience,
            chrono::Utc::now(),
        );
    }

    /// Remember a message with the moment it was sent.
    ///
    /// The platform's own rules are about AGE ("a message sent more than two
    /// minutes ago can no longer be retracted"), so the age has to be part of
    /// what the plugin remembers — and testable without waiting.
    fn remember_own_at(
        own: &Arc<parking_lot::Mutex<std::collections::HashMap<String, OwnMessage>>>,
        window: chrono::Duration,
        conversation: &str,
        message_id: &str,
        scope: &str,
        audience: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        if conversation.is_empty() || message_id.is_empty() {
            return;
        }
        let mut guard = own.lock();
        guard.insert(
            conversation.to_string(),
            OwnMessage {
                message_id: message_id.to_string(),
                scope: scope.to_string(),
                audience: audience.to_string(),
                at: now,
            },
        );
        // The cache stays as small as the platform's own answer window: an
        // retract that targets a message from hours ago is not something this
        // bot can still change, so keeping it would promise something the
        // platform will refuse.
        guard.retain(|_, message| now - message.at <= window);
        while guard.len() > 64 {
            let oldest = guard
                .iter()
                .min_by_key(|(_, own)| own.at)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    guard.remove(&key);
                }
                None => break,
            }
        }
    }

    /// The cache of the bot's own messages, for this crate's tests.
    pub(crate) fn own_cache(
        &self,
    ) -> &Arc<parking_lot::Mutex<std::collections::HashMap<String, OwnMessage>>> {
        &self.own
    }

    /// The bot's own last message in one conversation, if it is still within
    /// the platform's window.
    fn own_message(&self, conversation: &str) -> Option<OwnMessage> {
        let guard = self.own.lock();
        guard.get(conversation).cloned()
    }

    /// The configuration this plugin serves.
    pub fn config(&self) -> &QqOfficialConfig {
        &self.config
    }

    /// The API client this plugin dispatches through.
    pub fn api(&self) -> &QqOfficialApi {
        &self.api
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

    fn account_scope(&self) -> String {
        self.config.account_scope()
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
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            direct: true,
            group: true,
            thread: false,
            guild_channel: true,
            rich_text: true,
            attachments: true,
            // The official platform has no message-edit endpoint: its API
            // index offers send, retract and media upload for private and
            // group chats, and nothing that changes a message that was
            // already sent. Advertising an edit would promise an ability
            // the platform does not have (SPEC §57.4, RT19).
            edit: false,
            retract: true,
            // The platform has a guild-only typing endpoint, and this adapter
            // can call it — but one reply is one effect, so a typing indicator
            // could only be sent instead of the answer, or after it (when it
            // says nothing). The capability table lists what `describe` can
            // really produce (SPEC §57.4), so typing is not advertised.
            typing: false,
            proactive: true,
            notes: vec![
                "typing is a channel-only endpoint, and one reply is one effect: the indicator would arrive after the answer, so it is not offered as a capability".to_string(),
                "the platform has no message-edit endpoint: a message that was sent cannot be changed, only retracted".to_string(),
                "a retract only works within 2 minutes of the message, and only for the bot's own message".to_string(),
                "the passive reply window belongs to the platform: 60 minutes and 4 replies in a private chat, 5 minutes and 5 replies in a group, 5 minutes in a guild channel".to_string(),
                "active messages are the platform's business: a user can switch off the bot's active messages, a group owner has to allow the bot to speak in the group first, and the rate limits are 60 per minute per bot (30 unverified), 20 per minute per conversation, 1000 per day per conversation".to_string(),
                "full group traffic needs a platform-side permission; by default only messages that mention the bot arrive".to_string(),
                "the official platform issues per-application OpenIDs; the numeric QQ number is not available here".to_string(),
            ],
        }
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
            // A retract names the CONVERSATION; the plugin addresses the
            // last message it sent there. The platform retracts by message
            // id, and only the bot's own message can be touched.
            QQ_OFFICIAL_KIND_RETRACT => PublicationTarget::new(
                QQ_OFFICIAL_CHANNEL_ID,
                QQ_OFFICIAL_KIND_RETRACT,
                id.to_string(),
                self.config.account_scope(),
            )
            .with_scope(None, Some(id.to_string())),
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
        if matches!(target.kind.as_str(), QQ_OFFICIAL_KIND_RETRACT) {
            let conversation = target
                .conversation
                .clone()
                .unwrap_or_else(|| target.id.clone());
            let own = self.own_message(&conversation).ok_or_else(|| {
                // Nothing of the bot's own is addressable there: the
                // platform's window has passed, or the bot never spoke in
                // that conversation. Say so instead of pretending.
                ChannelError::UnresolvedReply {
                    channel: target.channel.clone(),
                    id: target.id.clone(),
                }
            })?;
            // The platform only retracts a message within two minutes of the
            // send, and only the bot's own message. A retract that is older
            // than that is refused here, with the platform's own rule as the
            // reason, instead of a request that could only fail.
            if chrono::Utc::now() - own.at > RETRACT_WINDOW {
                return Err(ChannelError::Rejected {
                    channel: target.channel.clone(),
                    detail: "the platform only retracts a message within 2 minutes of the send"
                        .to_string(),
                });
            }
            let mut params = json!({
                "message_id": own.message_id,
                "scope": own.scope,
            });
            if own.scope == "group" {
                params["group_openid"] = Value::String(own.audience.clone());
            } else {
                params["user_openid"] = Value::String(own.audience.clone());
            }
            let (action, platform_action, contract) = (
                ChannelAction::Retract,
                ACTION_RETRACT,
                QQ_OFFICIAL_RETRACT_CONTRACT,
            );
            return Ok(ChannelDispatch {
                channel: QQ_OFFICIAL_CHANNEL_ID.to_string(),
                action,
                platform_action: platform_action.to_string(),
                params,
                account: scope,
                tenant: None,
                conversation: Some(conversation),
                attachments: vec![],
                contract_digest: Digest::blake3(contract),
                acceptance_predicate: "qq_official_err_code_zero".to_string(),
                echo: None,
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
        let own = Arc::clone(&self.own);
        let own_window = chrono::Duration::seconds(self.config.reply_window_seconds.max(1) as i64);
        Box::pin(async move {
            // The send path needs the audience and the conversation after the
            // action was moved out, so they are read first.
            let send_conversation = dispatch.conversation.clone();
            let send_action = dispatch.platform_action.clone();
            let send_params = dispatch.params.clone();
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
                    // The plugin's own surface syntax decides what the wire
                    // carries: a leading mention becomes the platform's
                    // `<@member_openid>` markup, a quoted message becomes a
                    // `message_reference`, and Markdown becomes the
                    // platform's Markdown message type where the platform
                    // renders it. A body without markers is sent as the same
                    // plain text message as before.
                    let answer = match dispatch.platform_action.as_str() {
                        ACTION_SEND_PRIVATE => {
                            let openid = dispatch
                                .params
                                .get("user_openid")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let outbound = crate::qq_official::outbound::build_outbound(
                                &content,
                                crate::qq_official::outbound::OutboundScope::Private,
                            );
                            api.send_private(
                                openid,
                                &outbound,
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
                            let outbound = crate::qq_official::outbound::build_outbound(
                                &content,
                                crate::qq_official::outbound::OutboundScope::Group,
                            );
                            api.send_group(
                                group,
                                &outbound,
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
                            let outbound = crate::qq_official::outbound::build_outbound(
                                &content,
                                crate::qq_official::outbound::OutboundScope::Channel,
                            );
                            api.send_channel(
                                channel,
                                &outbound,
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
                    let answer = answer.map_err(|error| map_api_error(&dispatch.channel, error))?;
                    let receipt = receipt_of(dispatch, answer);
                    // The bot's own message id is what a later retract
                    // addresses, so it is remembered with the
                    // audience it went to.
                    if let (Some(conversation), Some(message_id)) = (
                        send_conversation.as_deref(),
                        receipt.provider_message_id.as_deref(),
                    ) {
                        let (scope, audience) = match send_action.as_str() {
                            ACTION_SEND_GROUP => (
                                "group",
                                send_params
                                    .get("group_openid")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                            ),
                            ACTION_SEND_CHANNEL => (
                                "channel",
                                send_params
                                    .get("channel_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                            ),
                            _ => (
                                "private",
                                send_params
                                    .get("user_openid")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                            ),
                        };
                        Self::remember_own(
                            &own,
                            own_window,
                            conversation,
                            message_id,
                            scope,
                            audience,
                        );
                    }
                    Ok(receipt)
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
                            api.retract_group(group, message_id).await
                        }
                        _ => {
                            let openid = dispatch
                                .params
                                .get("user_openid")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            api.retract_private(openid, message_id).await
                        }
                    }.map_err(|error| map_api_error(&dispatch.channel, error))?;
                    Ok(receipt_of(dispatch, answer))
                }
                ChannelAction::Edit => {
                    // The official platform has no message-edit endpoint.
                    // A dispatch that asks for one is refused with the
                    // platform's own fact, never faked into a re-send.
                    Err(ChannelError::Rejected {
                        channel: dispatch.channel.clone(),
                        detail: "the official platform has no message-edit endpoint; a sent message can only be retracted"
                            .to_string(),
                    })
                }
                ChannelAction::Typing => {
                    let channel = dispatch
                        .params
                        .get("channel_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let answer = api.typing(channel).await
                        .map_err(|error| map_api_error(&dispatch.channel, error))?;
                    Ok(receipt_of(dispatch, answer))
                }
            }
        })
    }

    /// Fetch one published media locator from the platform's own CDN.
    ///
    /// The official platform publishes a temporary download URL per
    /// attachment. That URL already carries its own authority, so it is
    /// fetched as published; if the CDN refuses it, the installed account's
    /// access token is tried exactly once, because a bot-attached resource can
    /// need the account's credential. Both answers stay the platform's own:
    /// an aged-out locator is reported as expired, a refused credential as
    /// unauthenticated, and anything else as unavailable.
    fn resolve_attachment(
        &self,
        attachment: &ChannelAttachment,
    ) -> BoxFuture<'static, Result<ResolvedAttachment, AttachmentError>> {
        let attachment = attachment.clone();
        let api = self.api.clone();
        Box::pin(async move {
            let Some(url) = attachment
                .locator
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
            else {
                return Err(AttachmentError::Refused {
                    attachment_id: attachment.attachment_id.clone(),
                    detail: "the platform published no download URL for this attachment".to_string(),
                });
            };
            let answer =
                crate::common::fetch_media_bounded(api.http(), &url, None, &attachment.attachment_id)
                    .await?;
            let (media_type, bytes) = match answer {
                crate::common::MediaAnswer::Delivered { media_type, bytes } => (media_type, bytes),
                crate::common::MediaAnswer::Refused { status, detail } => {
                    if is_expired_statement(&detail) {
                        return Err(AttachmentError::Expired {
                            attachment_id: attachment.attachment_id.clone(),
                            detail,
                        });
                    }
                    if matches!(status, 401 | 403) {
                        // One authenticated retry with the installed account:
                        // the same locator, the account's own credential, and
                        // the platform's answer to that credential.
                        let token = api.access_token().await.map_err(|error| {
                            AttachmentError::Unauthenticated {
                                channel: QQ_OFFICIAL_CHANNEL_ID.to_string(),
                                attachment_id: attachment.attachment_id.clone(),
                                detail: format!("no usable access token: {error}"),
                            }
                        })?;
                        match crate::common::fetch_media_bounded(
                            api.http(),
                            &url,
                            Some(&token),
                            &attachment.attachment_id,
                        )
                        .await?
                        {
                            crate::common::MediaAnswer::Delivered { media_type, bytes } => {
                                (media_type, bytes)
                            }
                            crate::common::MediaAnswer::Refused { status, detail } => {
                                if is_expired_statement(&detail) {
                                    return Err(AttachmentError::Expired {
                                        attachment_id: attachment.attachment_id.clone(),
                                        detail,
                                    });
                                }
                                return Err(crate::common::refused_attachment(
                                    QQ_OFFICIAL_CHANNEL_ID,
                                    &attachment.attachment_id,
                                    status,
                                    detail,
                                ));
                            }
                        }
                    } else {
                        return Err(crate::common::refused_attachment(
                            QQ_OFFICIAL_CHANNEL_ID,
                            &attachment.attachment_id,
                            status,
                            detail,
                        ));
                    }
                }
            };
            crate::common::resolved_from_media(&attachment, media_type, bytes)
        })
    }
}

/// Whether the platform's own answer says the signed locator aged out.
///
/// This is the provider's protocol vocabulary, not a subject-side judgement:
/// the CDN answers an expired signature with its own statement, and reporting
/// it as anything else would misattribute a locator lifetime to a credential.
fn is_expired_statement(detail: &str) -> bool {
    let lowered = detail.to_ascii_lowercase();
    lowered.contains("expired") || lowered.contains("过期")
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
pub fn reply_kinds() -> &'static [ReplyKind] {
    QQ_OFFICIAL_REPLY_KINDS
}

#[cfg(test)]
#[path = "capability_tests.rs"]
mod capability_tests;
