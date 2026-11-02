//! Classification of Feishu/Lark events — SPEC §15.1, §15.3, §16.
//!
//! The event dispatcher pushes one frame per event. The frame the adapter
//! uses carries `header.event_id`, `header.create_time` and
//! `event.message`: the message id, the chat it arrived in, its type, its
//! content and its sender. The content of a non-text message names a media
//! key, so the bytes are a remote resource the attachment handle fetches,
//! never bytes that travel with the event.
//!
//! Feishu serves two reply audiences: a whole chat (`chat_id`) and, inside a
//! chat, the thread of one message (`thread_id`). A message that carries no
//! thread belongs to the chat audience.

use serde_json::Value;

use crate::common::{ADAPTER_ENVELOPE_SCHEMA_VERSION, integer_field, text_field};
use shenko_types::channel::{
    ChannelActor, ChannelAttachment, ChannelContent, ChannelEnvelope, ChannelReplyTarget,
    ChannelSegment,
};

use super::config::FeishuConfig;

/// The channel id of the Feishu/Lark connector.
pub const FEISHU_CHANNEL_ID: &str = "feishu";

/// The `InboxEvent.source` label of this connector.
pub const FEISHU_SOURCE: &str = "feishu";

/// The transport identity domain of the Feishu platform.
pub const FEISHU_TRANSPORT_IDENTITY_DOMAIN: &str = "feishu_open_platform_v1";

/// The reply audience kind of a whole chat.
pub const FEISHU_KIND_CHAT: &str = "feishu-chat";

/// The reply audience kind of a thread inside a chat.
pub const FEISHU_KIND_THREAD: &str = "feishu-thread";

/// The event names this connector classifies.
pub const FEISHU_EVENTS: &[&str] = &["im.message.receive_v1"];

/// The classified view of one Feishu message event.
///
/// `None` means the frame is not a message this connector owns.
pub fn classify(config: &FeishuConfig, raw: &Value) -> Option<ChannelEnvelope> {
    // An envelope that was already normalized classifies to itself.
    if raw.get("provider").and_then(Value::as_str) == Some(FEISHU_CHANNEL_ID)
        && raw.get("schema_version").and_then(Value::as_u64)
            == Some(u64::from(ADAPTER_ENVELOPE_SCHEMA_VERSION))
    {
        return serde_json::from_value::<ChannelEnvelope>(raw.clone()).ok();
    }
    let header = raw.get("header").cloned().unwrap_or(Value::Null);
    let event_name = header
        .get("event_type")
        .and_then(Value::as_str)
        .or_else(|| raw.get("schema").and_then(Value::as_str).map(|_| "im.message.receive_v1"));
    let event_name = event_name?;
    if !FEISHU_EVENTS.contains(&event_name) {
        return None;
    }
    let event = raw.get("event").cloned().unwrap_or(Value::Null);
    let message = event.get("message").cloned().unwrap_or(Value::Null);
    let message_id = text_field(&message, "message_id")?;
    if message_id.is_empty() {
        return None;
    }
    // A message the bot itself produced is not an inbound event.
    let sender = event.get("sender").cloned().unwrap_or(Value::Null);
    if text_field(&sender, "sender_type").as_deref() == Some("app") {
        return None;
    }

    let account = config.account_scope();
    let chat_id = text_field(&message, "chat_id").unwrap_or_default();
    let chat_type = text_field(&message, "chat_type").unwrap_or_default();
    let thread_id = text_field(&message, "thread_id")
        .or_else(|| text_field(&message, "root_id"));
    let platform_id = sender
        .get("sender_id")
        .and_then(|id| text_field(id, "open_id"))
        .unwrap_or_default();

    let (kind, audience_id) = match thread_id.as_deref() {
        Some(thread) => (FEISHU_KIND_THREAD, thread),
        None => (FEISHU_KIND_CHAT, chat_id.as_str()),
    };
    let label = match kind {
        FEISHU_KIND_THREAD => format!("话题 {audience_id}"),
        _ if chat_type == "p2p" => format!("单聊 {platform_id}"),
        _ => format!("群 {audience_id}"),
    };
    let reply_target = if audience_id.is_empty() {
        None
    } else {
        Some(ChannelReplyTarget {
            kind: kind.to_string(),
            id: audience_id.to_string(),
            label,
            account: account.clone(),
            tenant: None,
            conversation: Some(audience_id.to_string()),
        })
    };

    let content = message
        .get("content")
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or(Value::Null);
    let message_type = text_field(&message, "message_type").unwrap_or_default();
    let (text, attachments) = parse_content(&message_id, &message_type, &content);

    let mut segments = vec![ChannelSegment {
        kind: "text".to_string(),
        text: Some(text.clone()),
        attachment_id: None,
        url: None,
    }];
    for attachment in &attachments {
        segments.push(ChannelSegment {
            kind: attachment.kind.clone(),
            text: attachment.name.clone(),
            attachment_id: Some(attachment.attachment_id.clone()),
            url: None,
        });
    }
    let mentions = message
        .get("mentions")
        .and_then(Value::as_array)
        .map(|mentions| {
            mentions
                .iter()
                .filter_map(|mention| text_field(mention, "open_id"))
                .collect()
        })
        .unwrap_or_default();

    Some(ChannelEnvelope {
        schema_version: ADAPTER_ENVELOPE_SCHEMA_VERSION,
        provider: FEISHU_CHANNEL_ID.to_string(),
        account,
        tenant: None,
        transport_identity_domain: FEISHU_TRANSPORT_IDENTITY_DOMAIN.to_string(),
        conversation: Some(chat_id),
        thread: thread_id,
        message_id,
        source_event_revision: text_field(&header, "event_id"),
        occurred_at: integer_field(&header, "create_time")
            .and_then(|millis| chrono::DateTime::from_timestamp_millis(millis)),
        received_at: chrono::Utc::now(),
        actor: ChannelActor {
            platform_id: platform_id.clone(),
            display_name: None,
            alias: None,
            person_ref: None,
            role_grant_ref: config
                .is_admin(&platform_id)
                .then(|| format!("feishu-admin:{platform_id}")),
            group_role: None,
        },
        content: ChannelContent {
            text,
            segments,
            mentions,
            reply_to: text_field(&message, "root_id"),
        },
        reply_target,
        attachments,
        raw_ref: None,
    })
}

/// Read the text and attachments of one message content.
fn parse_content(
    message_id: &str,
    message_type: &str,
    content: &Value,
) -> (String, Vec<ChannelAttachment>) {
    match message_type {
        "text" => (text_field(content, "text").unwrap_or_default(), vec![]),
        "post" => (post_text(content), vec![]),
        "image" | "video" | "file" | "audio" => {
            let (kind, key) = match message_type {
                "image" => ("image", text_field(content, "image_key")),
                "video" => ("video", text_field(content, "file_key")),
                "audio" => ("voice", text_field(content, "file_key")),
                _ => ("file", text_field(content, "file_key")),
            };
            let Some(file_key) = key else {
                return (String::new(), vec![]);
            };
            (
                String::new(),
                vec![ChannelAttachment {
                    attachment_id: format!("feishu-{message_id}-0"),
                    kind: kind.to_string(),
                    name: text_field(content, "file_name"),
                    mime: None,
                    bytes: integer_field(content, "size").and_then(|size| u64::try_from(size).ok()),
                    hash: None,
                    locator: shenko_types::channel::AttachmentLocator {
                        kind: "remote_path".to_string(),
                        host: Some("feishu-drive".to_string()),
                        path: Some(file_key),
                        url: None,
                    },
                    object: None,
                    expires_at: None,
                }],
            )
        }
        _ => (String::new(), vec![]),
    }
}

/// The plain text of a `post` document.
fn post_text(content: &Value) -> String {
    content
        .get("content")
        .and_then(Value::as_object)
        .map(|languages| {
            languages
                .values()
                .flat_map(|body| body.get("content").and_then(Value::as_array).cloned().unwrap_or_default())
                .flat_map(|row| row.as_array().cloned().unwrap_or_default())
                .filter_map(|block| text_field(&block, "text"))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}
