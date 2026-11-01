//! QQ IM-channel plugin descriptor — SPEC §35.6 boundary side.
//!
//! Every QQ/OneBot-specific string the subject prompt and the staged message
//! header used to hardcode lives here: the reply audience kinds
//! (`group`/`private`), the localized grammar hints, and the payload→label
//! projection that binds an ingress event to the account the model can reply
//! to. The subject core only sees `ChannelPlugin` values.
use serde_json::Value;
use shenko_types::channel::{ChannelPlugin, ChannelRegistry, ReplyKind};
use shenko_types::output::PublicationTarget;

use crate::outbound::OutboundKind;

/// `PublicationTarget.channel` id and registration identity of this plugin.
pub const QQ_CHANNEL_ID: &str = "qq";

/// `InboxEvent.source` values this channel emits (the napcat transport).
pub const QQ_SOURCES: &[&str] = &["napcat"];

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

/// The QQ channel descriptor registered into a `ChannelRegistry`.
pub fn qq_channel_plugin() -> ChannelPlugin {
    ChannelPlugin {
        channel: QQ_CHANNEL_ID,
        sources: QQ_SOURCES,
        kinds: QQ_REPLY_KINDS,
        prompt_names: "QQ 号或群号",
        staged_target: qq_staged_target,
    }
}

/// A registry containing exactly the QQ channel — the default live profile.
pub fn qq_registry() -> ChannelRegistry {
    let mut registry = ChannelRegistry::new();
    registry
        .register(qq_channel_plugin())
        .expect("qq channel descriptor validates");
    registry
}

/// OneBot v11 payload → staged `[who · label]` target label.
///
/// Group messages name the group; private messages name the sender account.
/// An event without a positive account id yields no label, so the header
/// stays `[who]` and no reply control can address it.
fn qq_staged_target(payload: &Value) -> Option<String> {
    let sender = payload.get("sender");
    let account = |key: &str| {
        payload
            .get(key)
            .or_else(|| sender.and_then(|sender| sender.get(key)))
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
    };
    match payload.get("message_type").and_then(Value::as_str) {
        Some("group") => account("group_id").map(|id| format!("群 {id}")),
        Some("private") => account("user_id").map(|id| format!("私聊 QQ {id}")),
        _ => None,
    }
}

/// Map a resolved reply target to the OneBot send action, or `None` when the
/// target does not belong to this channel or names a kind QQ has no action
/// for. Publication intents for other channels are another plugin's work.
pub fn qq_outbound(target: &PublicationTarget) -> Option<(OutboundKind, u64)> {
    if target.channel != QQ_CHANNEL_ID {
        return None;
    }
    let kind = match target.kind.as_str() {
        "group" => OutboundKind::SendGroup,
        "private" => OutboundKind::SendPrivate,
        _ => return None,
    };
    Some((kind, target.id))
}
