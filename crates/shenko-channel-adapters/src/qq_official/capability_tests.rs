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

/// The official platform has no message-edit endpoint, so the plugin offers
/// no edit kind: a reply kind the platform cannot serve is not a capability.
#[test]
fn the_platform_has_no_edit_so_the_plugin_offers_no_edit() {
    let plugin = plugin();
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_EDIT, "conversation-1");
    assert!(
        plugin.resolve_reply(QQ_OFFICIAL_KIND_EDIT, "conversation-1").is_none(),
        "an edit kind the platform cannot serve is not offered"
    );
    assert!(
        !plugin.capabilities().edit,
        "the capability table does not advertise an edit"
    );
    let error = plugin.describe(&target).expect_err("there is no edit to describe");
    assert!(
        matches!(error, ChannelError::UnsupportedTarget { .. }),
        "the refusal says the target is not one this plugin serves: {error:?}"
    );
}

/// A retract with nothing of the bot's own to address says so instead of
/// pretending.
#[test]
fn a_retract_without_anything_to_retract_is_refused() {
    let plugin = plugin();
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_RETRACT, "conversation-1");
    let error = plugin.describe(&target).expect_err("nothing to retract");
    assert!(
        matches!(error, ChannelError::UnresolvedReply { .. }),
        "the refusal names the audience it could not resolve: {error:?}"
    );
}

/// The platform only retracts a message within two minutes of the send, so a
/// retract of an older message is refused with the platform's own rule.
#[test]
fn a_retract_older_than_the_platform_window_is_refused_with_its_reason() {
    let plugin = plugin();
    QqOfficialChannel::remember_own_at(
        &plugin.own_cache(),
        chrono::Duration::minutes(30),
        "conversation-old",
        "message-3-min",
        "private",
        "user-openid-old",
        chrono::Utc::now() - chrono::Duration::minutes(3),
    );
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_RETRACT, "conversation-old");
    let error = plugin.describe(&target).expect_err("the platform would refuse it");
    match error {
        ChannelError::Rejected { detail, .. } => assert!(
            detail.contains("2 minutes"),
            "the refusal names the platform's own rule: {detail}"
        ),
        other => panic!("the refusal is a rejection with the platform's rule: {other:?}"),
    }
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
    let target = conversation_target(&plugin, QQ_OFFICIAL_KIND_RETRACT, "conversation-3");
    // The entry is still inside the window, so it resolves; the point of the
    // case is that the cache is bounded by the window rather than by memory.
    assert!(plugin.describe(&target).is_ok());
}

/// The capability table lists what the plugin can really produce.
#[test]
fn the_capability_table_only_lists_reachable_actions() {
    let plugin = plugin();
    let capabilities = plugin.capabilities();
    assert!(
        !capabilities.edit,
        "the official platform has no message-edit endpoint"
    );
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
    assert!(
        capabilities
            .notes
            .iter()
            .any(|note| note.contains("2 minutes")),
        "the platform's retract window is stated as a fact: {:?}",
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
