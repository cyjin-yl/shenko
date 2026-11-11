//! Classification of WeChat iLink messages — docs/specs/channels.md#spec-15-1, docs/specs/channel-adapters.md#spec-15-3, docs/specs/channel-adapters.md#spec-16.
//!
//! The provider pushes `WeixinMessage` objects. The fields the adapter uses
//! are the provider's own:
//!
//! * `message_id`, `seq`, `from_user_id`, `to_user_id` — the identity of the
//!   message and of its participants;
//! * `context_token` — the conversation context a reply must carry;
//! * `item_list` — the content, whose items are typed (`1` text, `2` image,
//!   `3` voice, `4` file, `5` video);
//! * `message_type` (`1` user, `2` bot) and `message_state`.
//!
//! The provider also reports a `group_id` on some messages. The protocol
//! exposes no group send, so this adapter serves direct conversations only
//! and does not advertise a group audience.

use serde_json::Value;

use crate::common::{ADAPTER_ENVELOPE_SCHEMA_VERSION, integer_field, text_field};
use shenko_types::channel::{
    ChannelActor, ChannelAttachment, ChannelContent, ChannelEnvelope, ChannelReplyTarget,
    ChannelSegment,
};

use super::config::ILinkConfig;

/// The channel id of the WeChat iLink connector.
pub const ILINK_CHANNEL_ID: &str = "ilink";

/// The `InboxEvent.source` label of this connector.
pub const ILINK_SOURCE: &str = "ilink";

/// The transport identity domain of the iLink provider.
pub const ILINK_TRANSPORT_IDENTITY_DOMAIN: &str = "ilink_bot_json_v1";

/// The reply audience kind this connector serves.
pub const ILINK_KIND_DIRECT: &str = "ilink-direct";

/// The classified view of one iLink message.
///
/// `None` means the message is not one this connector owns.
pub fn classify(config: &ILinkConfig, raw: &Value) -> Option<ChannelEnvelope> {
    // An envelope that was already normalized classifies to itself.
    if raw.get("provider").and_then(Value::as_str) == Some(ILINK_CHANNEL_ID)
        && raw.get("schema_version").and_then(Value::as_u64)
            == Some(u64::from(ADAPTER_ENVELOPE_SCHEMA_VERSION))
    {
        return serde_json::from_value::<ChannelEnvelope>(raw.clone()).ok();
    }
    // The provider wraps the message in its own envelope when it pushes it.
    let message = match raw.get("msg") {
        Some(message) => message.clone(),
        None => raw.clone(),
    };
    let message_id = text_field(&message, "message_id")?;
    if message_id.is_empty() {
        return None;
    }
    // A message the bot itself produced is not an inbound event.
    if integer_field(&message, "message_type") == Some(2) {
        return None;
    }
    let from_user_id = text_field(&message, "from_user_id").unwrap_or_default();
    let account = config.account_scope();
    let items = message
        .get("item_list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut text = String::new();
    let mut segments = vec![];
    let mut attachments = vec![];
    for (index, item) in items.iter().enumerate() {
        let item_type = integer_field(item, "type").unwrap_or(0);
        match item_type {
            1 => {
                let body = item
                    .get("text_item")
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                text.push_str(&body);
                segments.push(ChannelSegment {
                    kind: "text".to_string(),
                    text: Some(body),
                    attachment_id: None,
                    url: None,
                });
            }
            2 | 3 | 4 | 5 => {
                let (kind, holder) = match item_type {
                    2 => ("image", "image_item"),
                    3 => ("voice", "voice_item"),
                    4 => ("file", "file_item"),
                    _ => ("video", "video_item"),
                };
                let payload = item.get(holder).cloned().unwrap_or(Value::Null);
                let media = payload.get("media").cloned().unwrap_or(Value::Null);
                let name = text_field(&payload, "file_name");
                let size = integer_field(&payload, "len")
                    .or_else(|| integer_field(&payload, "mid_size"))
                    .or_else(|| integer_field(&payload, "video_size"))
                    .and_then(|size| u64::try_from(size).ok());
                let attachment = ChannelAttachment {
                    attachment_id: format!("ilink-{index}"),
                    kind: kind.to_string(),
                    name: name.clone(),
                    mime: None,
                    bytes: size,
                    hash: None,
                    locator: shenko_types::channel::AttachmentLocator {
                        kind: "remote_path".to_string(),
                        host: Some("ilink-cdn".to_string()),
                        path: media
                            .get("encrypt_query_param")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        url: media
                            .get("full_url")
                            .and_then(Value::as_str)
                            .filter(|url| !url.is_empty())
                            .map(str::to_owned),
                        // The CDN descriptor travels with the message: the
                        // download parameter and the symmetric key are the
                        // provider's own facts, captured verbatim here so the
                        // owning plugin can resolve the object later without
                        // guessing a location.
                        params: (!media.is_null()).then(|| media.clone()),
                    },
                    object: None,
                    expires_at: None,
                };
                segments.push(ChannelSegment {
                    kind: kind.to_string(),
                    text: name.clone(),
                    attachment_id: Some(attachment.attachment_id.clone()),
                    url: attachment.locator.url.clone(),
                });
                attachments.push(attachment);
            }
            _ => {}
        }
    }

    let reply_target = if from_user_id.is_empty() {
        None
    } else {
        Some(ChannelReplyTarget {
            kind: ILINK_KIND_DIRECT.to_string(),
            id: from_user_id.clone(),
            label: format!("微信 {from_user_id}"),
            account: account.clone(),
            tenant: None,
            conversation: Some(from_user_id.clone()),
        })
    };

    Some(ChannelEnvelope {
        schema_version: ADAPTER_ENVELOPE_SCHEMA_VERSION,
        provider: ILINK_CHANNEL_ID.to_string(),
        account,
        tenant: None,
        transport_identity_domain: ILINK_TRANSPORT_IDENTITY_DOMAIN.to_string(),
        conversation: Some(from_user_id.clone()),
        thread: text_field(&message, "group_id"),
        message_id,
        source_event_revision: integer_field(&message, "seq").map(|seq| seq.to_string()),
        occurred_at: integer_field(&message, "create_time_ms")
            .and_then(|millis| chrono::DateTime::from_timestamp_millis(millis)),
        received_at: chrono::Utc::now(),
        actor: ChannelActor {
            platform_id: from_user_id.clone(),
            display_name: None,
            alias: None,
            person_ref: None,
            role_grant_ref: config
                .is_admin(&from_user_id)
                .then(|| format!("ilink-admin:{from_user_id}")),
            group_role: None,
        },
        content: ChannelContent {
            text,
            segments,
            mentions: vec![],
            reply_to: None,
        },
        reply_target,
        attachments,
        raw_ref: None,
    })
}

/// The conversation context token of one inbound message.
///
/// A reply must carry the token of the conversation it answers. The token is
/// the provider's own; it is never invented and never sent to a different
/// conversation.
pub fn context_token(raw: &Value) -> Option<String> {
    let message = raw.get("msg").unwrap_or(raw);
    message
        .get("context_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}
