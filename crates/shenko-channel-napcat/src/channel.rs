//! QQ channel plugin — inbound classification, envelope projection and
//! outbound description/execution. SPEC §15–16, §35.6, §41.6.
//!
//! Everything QQ-specific lives here: the OneBot v11 payload shape, the
//! numeric account ids, the reply audience kinds (`group`/`private`), the
//! attachment segments and the acceptance predicate of a send. The subject
//! core only ever sees the normalized envelope and the opaque target.
//!
//! Ordinary reachability is open: any user who can reach the account may
//! talk. Administration is a separate, owner-set fact (`config.admin_qq`);
//! it is never derived from a group role, the transport token or a
//! voiceprint.
use std::sync::Arc;

use serde_json::Value;
use shenko_types::capabilities::ChannelCapabilities;
use shenko_types::channel::{
    AttachmentError, BoxFuture, ChannelAction, ChannelAttachment, ChannelDispatch, ChannelEnvelope,
    ChannelError, ChannelPlugin, ChannelReceipt, ChannelReplyTarget, ChannelRegistry, ReplyKind,
    ResolvedAttachment,
};
use shenko_types::object::Digest;
use shenko_types::output::PublicationTarget;

use crate::attachments::parse_message;
use crate::client::{NapCatClient, SendError};
use crate::config::NapCatConfig;

/// `PublicationTarget.channel` id and registration identity of this plugin.
pub const QQ_CHANNEL_ID: &str = "qq";

/// `InboxEvent.source` values this channel emits (the napcat transport).
pub const QQ_SOURCE: &str = "napcat";

/// The transport identity domain of the OneBot v11 wire protocol.
pub const QQ_TRANSPORT_IDENTITY_DOMAIN: &str = "onebot_v11";

/// The contract digest of the QQ send operation. It states the real
/// boundary: the provider answers, a human does not read, and a missing
/// answer is reconciled instead of repeated.
pub const QQ_SEND_CONTRACT: &[u8] =
    b"shenko.qq/onebot11-send/1;no-blind-replay;provider-accepted-not-read";

/// Reply audience kinds the model may address over this channel.
pub const QQ_REPLY_KINDS: &[ReplyKind] = &[
    ReplyKind {
        tag: "group",
        id_hint: "群号",
        audience: "群号",
    },
    ReplyKind {
        tag: "private",
        id_hint: "QQ号",
        audience: "QQ号",
    },
];

/// The QQ channel plugin.
pub struct NapCatChannel {
    config: Arc<NapCatConfig>,
    client: Arc<NapCatClient>,
}

impl NapCatChannel {
    /// Bind the plugin to a live connector.
    pub fn new(config: NapCatConfig, client: Arc<NapCatClient>) -> Self {
        Self {
            config: Arc::new(config),
            client,
        }
    }

    /// The QQ number of a platform id, when it is one.
    ///
    /// QQ ids are decimal numbers. The conversion stays inside this plugin:
    /// no other layer parses them.
    fn qq_number(id: &str) -> Option<u64> {
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        id.parse::<u64>().ok().filter(|number| *number > 0)
    }

    /// Classify a OneBot notice into the observation it is.
    ///
    /// Only the notices that carry a fact about the conversation are
    /// classified: a recall, a member change, a poke. Everything else stays
    /// unclassified and keeps the plain-header fallback, so an unknown event
    /// never invents a meaning.
    fn classify_notice(&self, raw: &Value) -> Option<ChannelEnvelope> {
        let notice_type = raw.get("notice_type").and_then(Value::as_str)?;
        let account = self.config.account_scope();
        let actor_id = raw
            .get("user_id")
            .and_then(platform_id)
            .or_else(|| raw.get("operator_id").and_then(platform_id));
        let occurred_at = raw
            .get("time")
            .and_then(Value::as_i64)
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0));
        let stamp = raw
            .get("time")
            .and_then(Value::as_i64)
            .map(|seconds| seconds.to_string())
            .unwrap_or_default();

        let (text, group_id, peer) = match notice_type {
            "group_recall" => {
                let group = raw.get("group_id").and_then(platform_id)?;
                let recalled = raw.get("message_id").and_then(platform_id)?;
                (format!("撤回了消息 {recalled}"), Some(group.clone()), Some(group))
            }
            "friend_recall" => {
                let recalled = raw.get("message_id").and_then(platform_id)?;
                let peer = raw.get("user_id").and_then(platform_id)?;
                (format!("撤回了消息 {recalled}"), None, Some(peer))
            }
            "group_increase" => {
                let group = raw.get("group_id").and_then(platform_id)?;
                (format!("加入了群 {group}"), Some(group.clone()), Some(group))
            }
            "group_decrease" => {
                let group = raw.get("group_id").and_then(platform_id)?;
                (format!("离开了群 {group}"), Some(group.clone()), Some(group))
            }
            "poke_notice" => {
                let peer = raw.get("user_id").and_then(platform_id)?;
                ("戳了戳这个账号".to_string(), None, Some(peer))
            }
            _ => return None,
        };

        let conversation = group_id.clone().or_else(|| peer.clone());
        let reply_target = match (&group_id, &peer) {
            (Some(group), _) => Some(ChannelReplyTarget {
                kind: "group".to_string(),
                id: group.clone(),
                label: format!("群 {group}"),
                account: account.clone(),
                tenant: None,
                conversation: Some(group.clone()),
            }),
            (None, Some(peer)) => Some(ChannelReplyTarget {
                kind: "private".to_string(),
                id: peer.clone(),
                label: format!("私聊 QQ {peer}"),
                account: account.clone(),
                tenant: None,
                conversation: Some(peer.clone()),
            }),
            (None, None) => None,
        };
        let actor = shenko_types::channel::ChannelActor {
            platform_id: actor_id.clone().unwrap_or_default(),
            display_name: None,
            alias: None,
            person_ref: None,
            role_grant_ref: actor_id
                .as_deref()
                .and_then(NapCatChannel::qq_number)
                .filter(|number| self.config.is_admin(*number))
                .map(|number| format!("qq-admin:{number}")),
            group_role: None,
        };
        // The event identity must be stable across a redelivery and distinct
        // per event: the same recall seen twice is one observation.
        let message_id = format!(
            "notice:{notice_type}:{}:{}:{}",
            actor_id.clone().unwrap_or_default(),
            group_id.clone().or_else(|| peer.clone()).unwrap_or_default(),
            stamp
        );
        Some(ChannelEnvelope {
            schema_version: 1,
            provider: QQ_CHANNEL_ID.to_string(),
            account,
            tenant: None,
            transport_identity_domain: QQ_TRANSPORT_IDENTITY_DOMAIN.to_string(),
            conversation,
            thread: None,
            message_id,
            source_event_revision: None,
            occurred_at,
            received_at: chrono::Utc::now(),
            actor,
            content: shenko_types::channel::ChannelContent {
                text,
                segments: Vec::new(),
                mentions: Vec::new(),
                reply_to: None,
            },
            reply_target,
            attachments: Vec::new(),
            raw_ref: None,
        })
    }
}

impl ChannelPlugin for NapCatChannel {
    fn channel_id(&self) -> &str {
        QQ_CHANNEL_ID
    }

    fn account_scope(&self) -> String {
        self.config.account_scope()
    }

    /// What the NapCat (OneBot v11) platform really does for this bot.
    ///
    /// The table lists what `describe` can really produce, and a field that
    /// is `false` is a promise NOT to attempt the action (SPEC §57.4, RT19).
    ///
    /// This plugin calls exactly two OneBot actions — `send_group_msg` and
    /// `send_private_msg` — so it offers neither editing nor retracting: a
    /// capability the plugin cannot produce must not be advertised.
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            direct: true,
            group: true,
            thread: false,
            guild_channel: false,
            // QQ renders no markup: the body is plain text, and this plugin
            // downgrades Markdown markers before sending because the platform
            // would show them literally. A mention and a quote travel as their
            // own segments, which is structure, not styled text.
            rich_text: false,
            attachments: true,
            edit: false,
            retract: false,
            typing: false,
            proactive: true,
            notes: vec![
                "this plugin calls only send_group_msg and send_private_msg: it offers neither editing nor retracting, because it cannot produce either".to_string(),
                "there is no typing action in the OneBot actions this plugin calls, and one reply is one effect".to_string(),
                "QQ renders no markup: the body is plain text and Markdown markers are downgraded before the send; a mention and a quote travel as their own segments".to_string(),
                "the account scope is this bot's own QQ number: the platform addresses messages by number, and the plugin only ever acts as itself".to_string(),
                "QQ groups and private chats are the two scopes; there are no threads and no guild channels here".to_string(),
            ],
        }
    }

    fn sources(&self) -> &[String] {
        // One owned source label; the registry keeps it unique.
        static SOURCES: std::sync::LazyLock<Vec<String>> =
            std::sync::LazyLock::new(|| vec![QQ_SOURCE.to_string()]);
        &SOURCES
    }

    fn kinds(&self) -> &[ReplyKind] {
        QQ_REPLY_KINDS
    }

    fn prompt_names(&self) -> &str {
        "QQ 号或群号"
    }

    fn classify(&self, source: &str, raw: &Value) -> Option<ChannelEnvelope> {
        if source != QQ_SOURCE {
            return None;
        }
        // An already normalized envelope classifies to itself, so the model
        // projection is identical at admission and at replay.
        if raw.get("provider").and_then(Value::as_str) == Some(QQ_CHANNEL_ID)
            && raw.get("schema_version").and_then(Value::as_u64) == Some(1)
        {
            return serde_json::from_value::<ChannelEnvelope>(raw.clone()).ok();
        }
        let post_type = raw.get("post_type").and_then(Value::as_str)?;
        // A platform notice is an event the account observed, not a message
        // somebody wrote. It still belongs to the subject: a recall means a
        // message the subject may have quoted no longer exists, and a member
        // change is exactly the kind of thing a personal agent is supposed to
        // notice. SPEC §57.4.
        if post_type == "notice" {
            return self.classify_notice(raw);
        }
        // `message_sent` is the NapCat/go-cqhttp extension that reports the
        // bot's own message, including one sent from another client of the same
        // account. It is a real conversation fact: the account spoke in that
        // audience. It is staged as the account's own line (see
        // `staged_text`), never as a person's message.
        if !matches!(post_type, "message" | "message_sent") {
            return None;
        }
        let own_message = post_type == "message_sent";
        let message_type = raw
            .get("message_type")
            .and_then(Value::as_str)
            .unwrap_or("private");
        let sender = raw.get("sender");
        let sender_id = sender
            .and_then(|sender| sender.get("user_id"))
            .or_else(|| raw.get("user_id"))
            .and_then(platform_id);
        let display_name = sender
            .and_then(|sender| sender.get("card"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .or_else(|| {
                sender
                    .and_then(|sender| sender.get("nickname"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        let group_role = sender
            .and_then(|sender| sender.get("role"))
            .and_then(Value::as_str)
            .filter(|role| !role.is_empty())
            .map(str::to_owned);
        let account = self.config.account_scope();
        let (reply_target, conversation) = match message_type {
            "group" => {
                let group_id = raw.get("group_id").and_then(platform_id)?;
                (
                    Some(ChannelReplyTarget {
                        kind: "group".to_string(),
                        id: group_id.clone(),
                        label: format!("群 {group_id}"),
                        account: account.clone(),
                        tenant: None,
                        conversation: Some(group_id.clone()),
                    }),
                    Some(group_id),
                )
            }
            "private" | "friend" | "group_temp" => {
                // For an own `message_sent` frame the peer is `target_id`; a
                // frame without it cannot name the audience unambiguously, so
                // it is left unclassified rather than inventing a conversation.
                let peer = if own_message {
                    raw.get("target_id").and_then(platform_id)?
                } else {
                    raw.get("user_id")
                        .and_then(platform_id)
                        .or_else(|| sender_id.clone())?
                };
                (
                    Some(ChannelReplyTarget {
                        kind: "private".to_string(),
                        id: peer.clone(),
                        label: format!("私聊 QQ {peer}"),
                        account: account.clone(),
                        tenant: None,
                        conversation: Some(peer.clone()),
                    }),
                    Some(peer),
                )
            }
            _ => (None, None),
        };
        let parsed = parse_message(
            raw.get("message")
                .filter(|value| !value.is_null())
                .unwrap_or(&Value::Null),
            &self.config,
        );
        let text = parsed
            .segments
            .iter()
            .filter_map(|segment| segment.text.clone())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("");
        let text = if text.is_empty() {
            raw.get("raw_message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        } else {
            text
        };
        let reply_to = parsed.reply_to.clone().or_else(|| {
                raw.get("source")
                    .and_then(|source| source.get("message_id"))
                    .and_then(platform_id)
            });
        let message_id = raw
            .get("message_id")
            .and_then(platform_id)
            .or_else(|| {
                raw.get("message_id")
                    .and_then(Value::as_u64)
                    .map(|id| id.to_string())
            })
            .unwrap_or_default();
        let actor = shenko_types::channel::ChannelActor {
            platform_id: sender_id.clone().unwrap_or_default(),
            display_name,
            alias: None,
            person_ref: None,
            role_grant_ref: sender_id
                .as_deref()
                .and_then(NapCatChannel::qq_number)
                .filter(|account| self.config.is_admin(*account))
                .map(|account| format!("qq-admin:{account}")),
            group_role,
        };
        Some(ChannelEnvelope {
            schema_version: 1,
            provider: QQ_CHANNEL_ID.to_string(),
            account,
            tenant: None,
            transport_identity_domain: QQ_TRANSPORT_IDENTITY_DOMAIN.to_string(),
            conversation,
            thread: None,
            message_id,
            source_event_revision: None,
            occurred_at: raw
                .get("time")
                .and_then(Value::as_i64)
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
            received_at: chrono::Utc::now(),
            actor,
            content: shenko_types::channel::ChannelContent {
                text,
                segments: parsed.segments,
                mentions: parsed.mentions,
                reply_to,
            },
            reply_target,
            attachments: parsed.attachments,
            raw_ref: None,
        })
    }

    fn staged_text(&self, envelope: &ChannelEnvelope) -> String {
        let who = envelope
            .actor
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(envelope.actor.platform_id.as_str());
        // The account's own line (a `message_sent` event, e.g. sent from
        // another client) is attributed to the account, never to a person: a
        // multi-speaker group must not read the bot's own words as somebody
        // else's message.
        let own = NapCatChannel::qq_number(&envelope.actor.platform_id)
            == Some(self.config.bot_qq);
        let who = if own {
            format!("{who}（本账号发出）")
        } else {
            who.to_string()
        };
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
            format!("（附 {}）", kinds)
        };
        let text = if body.is_empty() {
            attachments.clone()
        } else {
            format!("{body}{attachments}")
        };
        match &envelope.reply_target {
            Some(target) => format!("[{who} · {}] {text}", target.label),
            None => format!("[{who}] {text}"),
        }
    }

    fn resolve_reply(&self, kind: &str, id: &str) -> Option<PublicationTarget> {
        let number = NapCatChannel::qq_number(id)?;
        let target = match kind {
            "group" => PublicationTarget::new(
                QQ_CHANNEL_ID,
                "group",
                number.to_string(),
                self.config.account_scope(),
            )
            .with_scope(None, Some(number.to_string())),
            "private" => PublicationTarget::new(
                QQ_CHANNEL_ID,
                "private",
                number.to_string(),
                self.config.account_scope(),
            )
            .with_scope(None, Some(number.to_string())),
            _ => return None,
        };
        Some(target)
    }

    fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError> {
        if target.channel != QQ_CHANNEL_ID {
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
        let number = NapCatChannel::qq_number(&target.id).ok_or_else(|| {
            ChannelError::UnresolvedReply {
                channel: target.channel.clone(),
                id: target.id.clone(),
            }
        })?;
        let (platform_action, params) = match target.kind.as_str() {
            "group" => (
                "send_group_msg",
                serde_json::json!({"group_id": number.to_string(), "message": ""}),
            ),
            "private" => (
                "send_private_msg",
                serde_json::json!({"user_id": number.to_string(), "message": ""}),
            ),
            other => {
                return Err(ChannelError::UnsupportedTarget {
                    channel: target.channel.clone(),
                    kind: other.to_string(),
                });
            }
        };
        Ok(ChannelDispatch {
            channel: QQ_CHANNEL_ID.to_string(),
            action: ChannelAction::Send,
            platform_action: platform_action.to_string(),
            params,
            account: scope,
            tenant: None,
            conversation: target.conversation.clone(),
            attachments: vec![],
            contract_digest: Digest::blake3(QQ_SEND_CONTRACT),
            acceptance_predicate: "onebot_retcode_zero".to_string(),
            echo: None,
        })
    }

    fn dispatch(
        &self,
        dispatch: &ChannelDispatch,
        content: &str,
    ) -> BoxFuture<'static, Result<ChannelReceipt, ChannelError>> {
        let client = Arc::clone(&self.client);
        let dispatch = dispatch.clone();
        let content = content.to_string();
        Box::pin(async move {
            if dispatch.channel != QQ_CHANNEL_ID || dispatch.action != ChannelAction::Send {
                return Err(ChannelError::UnsupportedTarget {
                    channel: dispatch.channel.clone(),
                    kind: dispatch.platform_action.clone(),
                });
            }
            let echo = dispatch
                .echo
                .clone()
                .ok_or_else(|| ChannelError::Transport {
                    channel: dispatch.channel.clone(),
                    detail: "missing durable correlation token".to_string(),
                })?;
            let mut params = dispatch.params.clone();
            // The plugin's own surface syntax decides what the wire carries:
            // a leading mention and a quoted message become real segments,
            // and Markdown markers are downgraded to plain text a QQ reader
            // can actually see. A body without markers is sent as the same
            // single text segment as before.
            let outbound = crate::outbound::build_outbound(&content);
            if let Some(object) = params.as_object_mut() {
                object.insert("message".to_string(), outbound.wire_message());
            }
            let receipt = match dispatch.platform_action.as_str() {
                "send_group_msg" => {
                    let group_id = params
                        .get("group_id")
                        .and_then(Value::as_str)
                        .and_then(|id| id.parse::<u64>().ok())
                        .unwrap_or_default();
                    client
                        .send_action_with_echo("send_group_msg", params, echo)
                        .await
                }
                "send_private_msg" => {
                    let _user_id = params
                        .get("user_id")
                        .and_then(Value::as_str)
                        .and_then(|id| id.parse::<u64>().ok())
                        .unwrap_or_default();
                    client
                        .send_action_with_echo("send_private_msg", params, echo)
                        .await
                }
                other => {
                    return Err(ChannelError::UnsupportedTarget {
                        channel: dispatch.channel.clone(),
                        kind: other.to_string(),
                    });
                }
            };
            match receipt {
                Ok(receipt) => {
                    // The plugin judges acceptance with its own predicate
                    // (`onebot_retcode_zero`); the core records this verdict
                    // and never re-derives it from the status word.
                    let accepted = receipt.is_success();
                    Ok(ChannelReceipt {
                        status: receipt.status.clone(),
                        retcode: receipt.retcode,
                        accepted,
                        provider_message_id: receipt
                            .data
                            .get("message_id")
                            .map(|value| match value {
                                Value::String(text) => text.clone(),
                                other => other.to_string(),
                            }),
                        raw: serde_json::to_value(&receipt).unwrap_or(Value::Null),
                        observed_at: chrono::Utc::now(),
                    })
                }
                Err(SendError::Disconnected) => Err(ChannelError::Transport {
                    channel: dispatch.channel.clone(),
                    detail: "connector is not connected".to_string(),
                }),
                Err(SendError::Unknown(detail)) => Err(ChannelError::Unknown {
                    channel: dispatch.channel.clone(),
                    detail,
                }),
            }
        })
    }

    fn resolve_attachment(
        &self,
        attachment: &ChannelAttachment,
    ) -> BoxFuture<'static, Result<ResolvedAttachment, AttachmentError>> {
        let attachment = attachment.clone();
        let config = Arc::clone(&self.config);
        let client = Arc::clone(&self.client);
        Box::pin(async move {
            // The connector is the installed account: its own media actions and
            // the configured media root are the real mechanisms. Sealing the
            // bytes into the content-addressed store belongs to the caller that
            // owns the Store, so the plugin never needs one.
            crate::attachments::resolve(&attachment, &config, &client).await
        })
    }
}

/// The platform id of a JSON value, as an opaque string.
///
/// OneBot ids may exceed the signed 64-bit range; the exact decimal text is
/// kept and never converted to a signed number.
fn platform_id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// A registry containing exactly the QQ channel.
pub fn qq_registry(config: NapCatConfig, client: Arc<NapCatClient>) -> ChannelRegistry {
    let mut registry = ChannelRegistry::new();
    registry
        .register(Arc::new(NapCatChannel::new(config, client)))
        .expect("qq channel descriptor validates");
    registry
}

/// A registry with the QQ channel bound to an isolated on-disk connector.
///
/// Tests and non-canonical embedders use this; the subject runtime uses
/// `qq_registry` with its configured endpoint and the canonical ingress spool
/// in its data directory. The isolated spool is still a real file — reception
/// never degrades to memory.
pub fn qq_test_registry() -> ChannelRegistry {
    let config = NapCatConfig {
        bot_qq: 10000,
        ..Default::default()
    };
    let (client, _events, _actions) = NapCatClient::open_isolated(config.clone())
        .expect("isolated napcat connector opens");
    qq_registry(config, Arc::new(client))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn channel(config: NapCatConfig) -> NapCatChannel {
        let (client, _events, _actions) =
            NapCatClient::open_isolated(config.clone()).expect("isolated connector opens");
        NapCatChannel::new(config, Arc::new(client))
    }

    fn group_config() -> NapCatConfig {
        NapCatConfig {
            bot_qq: 10000,
            // The owner names the administrator explicitly; group roles and
            // familiarity never grant administration.
            admin_qq: vec![20002],
            ..Default::default()
        }
    }

    /// A message the account itself sent (from another client) is attributed to
    /// the account, keeps the same conversation identity as the peer's side,
    /// and never becomes a person's line in the staged text.
    #[test]
    fn own_sent_messages_are_the_accounts_own_line() {
        let plugin = channel(group_config());
        let group = json!({
            "post_type": "message_sent",
            "message_type": "group",
            "group_id": 7,
            "user_id": 10000,
            "self_id": 10000,
            "message_id": 900,
            "sender": {"user_id": 10000, "nickname": "深刻", "card": ""},
            "message": [{"type": "text", "data": {"text": "我先看一下"}}]
        });
        let envelope = plugin.classify(QQ_SOURCE, &group).expect("own group message classifies");
        assert_eq!(envelope.provider, QQ_CHANNEL_ID);
        assert_eq!(envelope.account, "qq:10000");
        assert_eq!(envelope.conversation.as_deref(), Some("7"));
        assert_eq!(envelope.message_id, "900");
        assert_eq!(envelope.actor.platform_id, "10000");
        assert_eq!(
            plugin.staged_text(&envelope),
            "[深刻（本账号发出） · 群 7] 我先看一下"
        );
        // It is not administration: the account is not an owner-set admin.
        assert_eq!(envelope.actor.role_grant_ref, None);
        // The peer's own messages keep the original wording.
        let incoming = json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 7,
            "user_id": 20001,
            "self_id": 10000,
            "message_id": 901,
            "sender": {"user_id": 20001, "nickname": "甲", "role": "owner"},
            "message": [{"type": "text", "data": {"text": "在吗"}}]
        });
        let envelope = plugin.classify(QQ_SOURCE, &incoming).unwrap();
        assert_eq!(plugin.staged_text(&envelope), "[甲 · 群 7] 在吗");
        // A platform group role is not an administrative grant either; only
        // the owner-configured admin list is.
        assert_eq!(envelope.actor.role_grant_ref, None);
        assert_eq!(envelope.actor.group_role.as_deref(), Some("owner"));
        let admin = json!({
            "post_type": "message",
            "message_type": "private",
            "user_id": 20002,
            "self_id": 10000,
            "message_id": 902,
            "sender": {"user_id": 20002, "nickname": "管理员"},
            "message": [{"type": "text", "data": {"text": "改一下"}}]
        });
        let envelope = plugin.classify(QQ_SOURCE, &admin).unwrap();
        assert_eq!(
            envelope.actor.role_grant_ref.as_deref(),
            Some("qq-admin:20002")
        );
    }

    /// Rich OneBot content keeps a readable rendering: a shared card, a
    /// mention, a face and a merged forward are not silently dropped from the
    /// message the model reads, and the structured segments survive.
    #[test]
    fn rich_segments_keep_readable_content() {
        let plugin = channel(group_config());
        let event = json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 7,
            "user_id": 20001,
            "self_id": 10000,
            "message_id": 910,
            "sender": {"user_id": 20001, "nickname": "甲"},
            "message": [
                {"type": "at", "data": {"qq": "10000"}},
                {"type": "text", "data": {"text": " 看这个 "}},
                {"type": "json", "data": {"data": "{\"prompt\":\"[分享]标题\",\"meta\":{\"title\":\"标题\"}}"}},
                {"type": "forward", "data": {"id": "fwd-1"}},
                {"type": "face", "data": {"id": "14"}},
                {"type": "share", "data": {"title": "一篇好文章", "url": "https://example.invalid/a"}}
            ]
        });
        let envelope = plugin.classify(QQ_SOURCE, &event).unwrap();
        assert_eq!(envelope.content.mentions, vec!["10000".to_string()]);
        let kinds: Vec<&str> = envelope
            .content
            .segments
            .iter()
            .map(|segment| segment.kind.as_str())
            .collect();
        assert_eq!(kinds, ["at", "text", "json", "forward", "face", "share"]);
        let text = plugin.staged_text(&envelope);
        assert!(text.contains("@10000"), "{text}");
        assert!(text.contains("[分享]标题"), "{text}");
        assert!(text.contains("[合并转发 fwd-1，未展开]"), "{text}");
        assert!(text.contains("[表情 14]"), "{text}");
        assert!(text.contains("一篇好文章"), "{text}");
        // The raw CQ-string form keeps media as attachments, as before.
        let cq = json!({
            "post_type": "message",
            "message_type": "private",
            "user_id": 9,
            "self_id": 10000,
            "message_id": 911,
            "sender": {"user_id": 9, "nickname": "乙"},
            "message": "[CQ:image,file=cat.png]在吗"
        });
        let envelope = plugin.classify(QQ_SOURCE, &cq).unwrap();
        assert_eq!(envelope.attachments.len(), 1);
        assert_eq!(envelope.attachments[0].kind, "image");
        assert!(plugin.staged_text(&envelope).contains("在吗"));
    }

    /// A private own message names its peer by `target_id`; without that field
    /// it is left unclassified instead of inventing a conversation.
    #[test]
    fn private_own_messages_need_an_explicit_peer() {
        let plugin = channel(group_config());
        let with_target = json!({
            "post_type": "message_sent",
            "message_type": "private",
            "user_id": 10000,
            "target_id": 20001,
            "self_id": 10000,
            "message_id": 903,
            "sender": {"user_id": 10000, "nickname": "深刻"},
            "message": "收到"
        });
        let envelope = plugin.classify(QQ_SOURCE, &with_target).unwrap();
        assert_eq!(envelope.conversation.as_deref(), Some("20001"));
        assert_eq!(
            plugin.resolve_reply("private", "20001").unwrap().id,
            "20001"
        );
        assert_eq!(
            plugin.staged_text(&envelope),
            "[深刻（本账号发出） · 私聊 QQ 20001] 收到"
        );
        let mut missing = with_target.clone();
        missing.as_object_mut().unwrap().remove("target_id");
        assert!(plugin.classify(QQ_SOURCE, &missing).is_none());
    }
}
