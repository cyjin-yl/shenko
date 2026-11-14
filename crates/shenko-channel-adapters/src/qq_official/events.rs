//! Classification of official QQ bot events — docs/specs/channels.md#spec-15-1, docs/specs/channel-adapters.md#spec-15-3, docs/specs/channel-adapters.md#spec-16.
//!
//! The platform pushes one gateway frame per event. The frame is
//! `{"id":…,"op":0,"d":{…},"s":42,"t":"EVENT_NAME"}`. The raw frame is the
//! evidence; the envelope is the classified view the subject works with.
//!
//! Every identity in this module is an OpenID that the platform issued for
//! THIS application. The numeric QQ number is not available to an official
//! bot, and the adapter never pretends otherwise: the model sees the OpenID
//! the platform reported.

use serde_json::Value;

use crate::common::{ADAPTER_ENVELOPE_SCHEMA_VERSION, integer_field, text_field};
use shenko_types::channel::{
    AudienceShape, ChannelActor, ChannelAttachment, ChannelContent, ChannelEnvelope,
    ChannelReplyTarget, ChannelSegment,
};

use super::config::QqOfficialConfig;

/// The channel id of the official QQ bot connector.
pub const QQ_OFFICIAL_CHANNEL_ID: &str = "qq-official";

/// The `InboxEvent.source` label of this connector.
pub const QQ_OFFICIAL_SOURCE: &str = "qqofficial";

/// The transport identity domain of the official QQ gateway.
pub const QQ_OFFICIAL_TRANSPORT_IDENTITY_DOMAIN: &str = "qq_official_gateway_v2";

/// The event names this connector classifies.
pub const QQ_OFFICIAL_EVENTS: &[&str] = &[
    "C2C_MESSAGE_CREATE",
    "GROUP_AT_MESSAGE_CREATE",
    "GROUP_MESSAGE_CREATE",
    "AT_MESSAGE_CREATE",
    "DIRECT_MESSAGE_CREATE",
];

/// The reply audience kinds this connector serves.
pub const QQ_OFFICIAL_KIND_GROUP: &str = "qqo-group";
/// The reply audience kind of a single chat.
pub const QQ_OFFICIAL_KIND_PRIVATE: &str = "qqo-private";
/// The reply audience kind of a guild channel.
pub const QQ_OFFICIAL_KIND_CHANNEL: &str = "qqo-channel";

/// The reply kind that edits the bot's own last message in one conversation.
///
/// The platform edits a message by its id, and the bot may only edit its own
/// message. The subject therefore names the CONVERSATION, and the plugin
/// addresses the last message it sent there.
/// The label of the edit kind.
///
/// The official platform has NO message-edit endpoint (its API index offers
/// send, retract and media upload), so this kind is never offered: it is
/// named here so the capability table can prove that the kind the platform
/// cannot serve is absent.
pub const QQ_OFFICIAL_KIND_EDIT: &str = "qqo-edit";

/// The reply kind that retracts the bot's own last message in one
/// conversation.
pub const QQ_OFFICIAL_KIND_RETRACT: &str = "qqo-retract";

/// The classified view of one official QQ event.
///
/// `None` means the frame is not a message this connector owns; the frame
/// stays in the ingress ledger as raw bytes.
pub fn classify(config: &QqOfficialConfig, raw: &Value) -> Option<ChannelEnvelope> {
    // An envelope that was already normalized classifies to itself, so the
    // model projection is identical at admission and at replay.
    if raw.get("provider").and_then(Value::as_str) == Some(QQ_OFFICIAL_CHANNEL_ID)
        && raw.get("schema_version").and_then(Value::as_u64)
            == Some(u64::from(ADAPTER_ENVELOPE_SCHEMA_VERSION))
    {
        return serde_json::from_value::<ChannelEnvelope>(raw.clone()).ok();
    }
    let event = raw.get("t").and_then(Value::as_str)?;
    if !QQ_OFFICIAL_EVENTS.contains(&event) {
        return None;
    }
    let data = raw.get("d").cloned().unwrap_or(Value::Null);
    let author = data.get("author").cloned().unwrap_or(Value::Null);

    let platform_id = text_field(&author, "user_openid")
        .or_else(|| text_field(&author, "member_openid"))
        .or_else(|| text_field(&author, "id"))
        .unwrap_or_default();
    let display_name = text_field(&author, "username");
    let group_role = text_field(&author, "member_role");

    let account = config.account_scope();
    let (conversation, reply_target) = match event {
        "C2C_MESSAGE_CREATE" | "DIRECT_MESSAGE_CREATE" => {
            let peer = platform_id.clone();
            (
                Some(peer.clone()),
                Some(ChannelReplyTarget {
                    kind: QQ_OFFICIAL_KIND_PRIVATE.to_string(),
                    id: peer,
                    label: format!("单聊 {platform_id}"),
                    account: account.clone(),
                    tenant: None,
                    conversation: None,
                    shape: Some(AudienceShape::OneToOne),
                }),
            )
        }
        "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" => {
            let group = text_field(&data, "group_openid").unwrap_or_default();
            (
                Some(group.clone()),
                Some(ChannelReplyTarget {
                    kind: QQ_OFFICIAL_KIND_GROUP.to_string(),
                    id: group.clone(),
                    label: format!("群 {group}"),
                    account: account.clone(),
                    tenant: None,
                    conversation: Some(group),
                    shape: Some(AudienceShape::Group),
                }),
            )
        }
        "AT_MESSAGE_CREATE" => {
            let channel = text_field(&data, "channel_id").unwrap_or_default();
            (
                Some(channel.clone()),
                Some(ChannelReplyTarget {
                    kind: QQ_OFFICIAL_KIND_CHANNEL.to_string(),
                    id: channel.clone(),
                    label: format!("频道 {channel}"),
                    account: account.clone(),
                    tenant: None,
                    conversation: Some(channel),
                    shape: Some(AudienceShape::Group),
                }),
            )
        }
        _ => (None, None),
    };

    let text = text_field(&data, "content").unwrap_or_default();
    let mut segments = vec![ChannelSegment {
        kind: "text".to_string(),
        text: Some(text.clone()),
        attachment_id: None,
        url: None,
    }];
    let attachments = parse_attachments(&data);
    for attachment in &attachments {
        segments.push(ChannelSegment {
            kind: attachment.kind.clone(),
            text: attachment.name.clone(),
            attachment_id: Some(attachment.attachment_id.clone()),
            url: attachment.locator.url.clone(),
        });
    }
    let mentions = data
        .get("mentions")
        .and_then(Value::as_array)
        .map(|mentions| {
            mentions
                .iter()
                .filter_map(|mention| {
                    text_field(mention, "user_openid")
                        .or_else(|| text_field(mention, "member_openid"))
                        .or_else(|| text_field(mention, "id"))
                })
                .collect()
        })
        .unwrap_or_default();
    let scene = data.get("message_scene").cloned().unwrap_or(Value::Null);
    let reply_to = scene_ext(&scene, "ref_msg_idx");

    Some(ChannelEnvelope {
        schema_version: ADAPTER_ENVELOPE_SCHEMA_VERSION,
        provider: QQ_OFFICIAL_CHANNEL_ID.to_string(),
        account,
        tenant: None,
        transport_identity_domain: QQ_OFFICIAL_TRANSPORT_IDENTITY_DOMAIN.to_string(),
        conversation,
        thread: None,
        message_id: text_field(&data, "id").unwrap_or_default(),
        source_event_revision: raw.get("s").and_then(Value::as_i64).map(|s| s.to_string()),
        occurred_at: text_field(&data, "timestamp")
            .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(&timestamp).ok())
            .map(|timestamp| timestamp.with_timezone(&chrono::Utc)),
        received_at: chrono::Utc::now(),
        actor: ChannelActor {
            platform_id: platform_id.clone(),
            display_name,
            alias: None,
            person_ref: None,
            role_grant_ref: config
                .is_admin(&platform_id)
                .then(|| format!("qq-official-admin:{platform_id}")),
            group_role,
            // The platform drops the bot's own messages before they reach here.
            own_account: Some(false),
        },
        content: ChannelContent {
            text,
            segments,
            mentions,
            reply_to,
        },
        reply_target,
        attachments,
        raw_ref: None,
    })
}

/// Read one `key=value` entry of the platform's message scene extensions.
pub fn scene_ext(scene: &Value, key: &str) -> Option<String> {
    let entries = scene.get("ext")?.as_array()?;
    entries.iter().find_map(|entry| {
        let entry = entry.as_str()?;
        let (name, value) = entry.split_once('=')?;
        (name == key && !value.is_empty()).then(|| value.to_string())
    })
}

/// The attachments of one official QQ event.
///
/// The platform publishes a temporary download URL per attachment. The bytes
/// do not travel with the message: the locator names the platform host, so a
/// media worker can fetch them through the controlled attachment handle.
pub fn parse_attachments(data: &Value) -> Vec<ChannelAttachment> {
    let Some(entries) = data.get("attachments").and_then(Value::as_array) else {
        return vec![];
    };
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let content_type = text_field(entry, "content_type").unwrap_or_default();
            let kind = attachment_kind(&content_type);
            ChannelAttachment {
                attachment_id: format!("qq-{index}"),
                kind,
                name: text_field(entry, "filename"),
                mime: (!content_type.is_empty()).then_some(content_type),
                bytes: integer_field(entry, "size").and_then(|size| u64::try_from(size).ok()),
                hash: None,
                locator: shenko_types::channel::AttachmentLocator {
                    kind: "url".to_string(),
                    host: Some("qq-platform".to_string()),
                    path: None,
                    url: text_field(entry, "url"),
                    // The platform's own temporary URL is the whole fetch
                    // descriptor: the CDN needs no second fact from the
                    // message, so nothing is captured here.
                    params: None,
                },
                object: None,
                expires_at: None,
            }
        })
        .collect()
}

/// The attachment kind of a platform content type.
fn attachment_kind(content_type: &str) -> String {
    if content_type.starts_with("image/") {
        "image".to_string()
    } else if content_type.starts_with("video/") {
        "video".to_string()
    } else if content_type.starts_with("audio/") || content_type == "voice" {
        "voice".to_string()
    } else {
        "file".to_string()
    }
}
