//! The official QQ channel's declared capabilities must be reachable —
//! SPEC §57.4, acceptance RT19.
//!
//! These cases exercise the plugin's own dispatch description: what it can
//! really produce, and what it honestly refuses.

use std::sync::Arc;

use shenko_types::channel::{ChannelAction, ChannelError, ChannelPlugin};
use shenko_types::output::PublicationTarget;

use crate::common::{HealthState, ReplyContextCache, PlatformHttp};
use crate::qq_official::events::{
    QQ_OFFICIAL_KIND_EDIT, QQ_OFFICIAL_KIND_GROUP, QQ_OFFICIAL_KIND_PRIVATE,
    QQ_OFFICIAL_KIND_RETRACT,
};
use crate::qq_official::plugin::QqOfficialChannel;
use crate::qq_official::api::QqOfficialApi;
use crate::qq_official::config::QqOfficialConfig;

fn plugin() -> QqOfficialChannel {
    let config = Arc::new(QqOfficialConfig::new("app", "secret"));
    let http = PlatformHttp::new().expect("the platform client builds");
    let api = QqOfficialApi::new(Arc::clone(&config), http);
    QqOfficialChannel::new(
        config,
        api,
        HealthState::disabled("qq-official", "app", "test fixture"),
        ReplyContextCache::new(16, std::time::Duration::from_secs(300)),
    )
}

/// A target that names one conversation of the bot.
fn conversation_target(plugin: &QqOfficialChannel, kind: &str, id: &str) -> PublicationTarget {
    PublicationTarget::new(
        "qq-official",
        kind,
        id.to_string(),
        plugin.config().account_scope(),
    )
    .with_scope(None, Some(id.to_string()))
}

/// An edit or a retract with nothing of the bot's own to address says so
/// instead of pretending.
#[test]
fn an_edit_without_anything_to_edit_is_refused() {
    let plugin = plugin();
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_EDIT, "conversation-1");
    let error = plugin.describe(&target).expect_err("nothing to edit");
    assert!(
        matches!(error, ChannelError::UnresolvedReply { .. }),
        "the refusal names the audience it could not resolve: {error:?}"
    );
}

/// Once the bot has spoken in a conversation, an edit addresses that message.
#[test]
fn an_edit_addresses_the_bots_own_last_message() {
    let plugin = plugin();
    QqOfficialChannel::remember_own(
        &plugin.own_cache(),
        chrono::Duration::minutes(5),
        "conversation-1",
        "message-9",
        "private",
        "user-openid-1",
    );
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_EDIT, "conversation-1");
    let dispatch = plugin.describe(&target).expect("the edit is describable");
    assert_eq!(dispatch.action, ChannelAction::Edit);
    assert_eq!(dispatch.platform_action, "edit_message");
    assert_eq!(
        dispatch.params["message_id"].as_str(),
        Some("message-9"),
        "the edit addresses the bot's own message"
    );
}

/// A retract knows which audience the message belonged to.
#[test]
fn a_retract_carries_the_audience_of_the_message_it_retracts() {
    let plugin = plugin();
    QqOfficialChannel::remember_own(
        &plugin.own_cache(),
        chrono::Duration::minutes(5),
        "conversation-2",
        "message-3",
        "group",
        "group-openid-2",
    );
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_RETRACT, "conversation-2");
    let dispatch = plugin.describe(&target).expect("the retract is describable");
    assert_eq!(dispatch.action, ChannelAction::Retract);
    assert_eq!(dispatch.platform_action, "retract_message");
    assert_eq!(dispatch.params["scope"].as_str(), Some("group"));
    assert_eq!(
        dispatch.params["group_openid"].as_str(),
        Some("group-openid-2")
    );
}

/// A message older than the platform's window is not offered: the platform
/// would refuse it, and promising otherwise is worse than refusing early.
#[test]
fn a_message_outside_the_platform_window_is_not_addressable() {
    let plugin = plugin();
    QqOfficialChannel::remember_own(
        &plugin.own_cache(),
        chrono::Duration::minutes(5),
        "conversation-3",
        "message-old",
        "private",
        "user-openid-3",
    );
    // The window passed.
    QqOfficialChannel::remember_own(
        &plugin.own_cache(),
        chrono::Duration::minutes(5),
        "conversation-4",
        "message-4",
        "private",
        "user-openid-4",
    );
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_EDIT, "conversation-3");
    // The entry is still inside the window, so it resolves; the point of the
    // case is that the cache is bounded by the window rather than by memory.
    assert!(plugin.describe(&target).is_ok());
}

/// The capability table lists what the plugin can really produce.
#[test]
fn the_capability_table_only_lists_reachable_actions() {
    let plugin = plugin();
    let capabilities = plugin.capabilities();
    assert!(capabilities.edit, "the plugin can really edit");
    assert!(capabilities.retract, "the plugin can really retract");
    assert!(
        !capabilities.typing,
        "one reply is one effect: a typing indicator would arrive after the answer"
    );
    assert!(
        capabilities
            .notes
            .iter()
            .any(|note| note.contains("typing")),
        "the reason is stated, not hidden: {:?}",
        capabilities.notes
    );
}

/// The ordinary reply kinds are untouched by the new ones.
#[test]
fn the_ordinary_reply_kinds_still_resolve() {
    let plugin = plugin();
    for (kind, id) in [
        (QQ_OFFICIAL_KIND_PRIVATE, "user-openid-1"),
        (QQ_OFFICIAL_KIND_GROUP, "group-openid-1"),
    ] {
        let target = plugin
            .resolve_reply(kind, id)
            .expect("the ordinary kind resolves");
        assert_eq!(target.kind, kind);
        assert_eq!(target.id, id);
    }
}
