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
use shenko_store::store::Store;
use shenko_types::channel::{
    BoxFuture, ChannelAction, ChannelAttachment, ChannelDispatch, ChannelEnvelope, ChannelError,
    ChannelPlugin, ChannelReceipt, ChannelReplyTarget, ChannelRegistry, ReplyKind,
};
use shenko_types::object::Digest;
use shenko_types::output::PublicationTarget;

use crate::attachments::{parse_message, resolve_attachment, AttachmentError, ResolvedAttachment};
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
    store: Option<Arc<Store>>,
}

impl NapCatChannel {
    /// Bind the plugin to a live connector.
    pub fn new(config: NapCatConfig, client: Arc<NapCatClient>) -> Self {
        Self {
            config: Arc::new(config),
            client,
            store: None,
        }
    }

    /// Bind the plugin to the Store, so a resolved attachment can be sealed
    /// into the content-addressed store instead of staying a local path.
    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// The account namespace this connector's conversations belong to.
    pub fn account_scope(&self) -> String {
        self.config.account_scope()
    }

    /// Read one attachment on the NapCat host and seal its bytes into the
    /// store. The bytes are then reachable by content digest from every
    /// node; the remote path itself never becomes a local file.
    pub fn resolve_attachment_bytes(
        &self,
        attachment: &ChannelAttachment,
    ) -> Result<ResolvedAttachment, String> {
        let resolved = resolve_attachment(attachment, &self.config).map_err(|error| match error {
            AttachmentError::Read { detail } => format!("attachment read failed: {detail}"),
            other => other.to_string(),
        })?;
        if let Some(store) = &self.store {
            let mime = resolved
                .mime
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let sealed = store
                .cas
                .seal_object(
                    &resolved.bytes,
                    &mime,
                    "channel_attachments",
                    "bytes",
                    1,
                )
                .map_err(|error| format!("attachment seal failed: {error}"))?;
            return Ok(ResolvedAttachment {
                attachment_id: resolved.attachment_id,
                bytes: resolved.bytes,
                mime: resolved.mime,
                object: Some(sealed.object),
            });
        }
        Ok(resolved)
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
}

impl ChannelPlugin for NapCatChannel {
    fn channel_id(&self) -> &str {
        QQ_CHANNEL_ID
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
        if post_type != "message" {
            return None;
        }
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
                let peer = raw
                    .get("user_id")
                    .and_then(platform_id)
                    .or_else(|| sender_id.clone())?;
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
            if let Some(object) = params.as_object_mut() {
                object.insert("message".to_string(), Value::String(content.clone()));
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
    ) -> Result<ChannelAttachment, String> {
        let resolved = self.resolve_attachment_bytes(attachment)?;
        let mut attachment = attachment.clone();
        attachment.object = resolved.object;
        Ok(attachment)
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
