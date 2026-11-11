//! The Feishu channel's declared capabilities must be reachable —
//! docs/specs/channels.md#spec-57-4, acceptance RT19.
//!
//! These cases exercise the plugin's own dispatch description: what it can
//! really produce, and what it honestly refuses.

use std::sync::Arc;

use shenko_types::channel::{ChannelAction, ChannelError, ChannelPlugin};
use shenko_types::output::PublicationTarget;

use crate::common::{HealthState, PlatformHttp, ReplyContextCache};
use crate::feishu::api::FeishuApi;
use crate::feishu::auth::TenantAuth;
use crate::feishu::config::FeishuConfig;
use crate::feishu::events::{FEISHU_KIND_CHAT, FEISHU_KIND_EDIT, FEISHU_KIND_RECALL};
use crate::feishu::plugin::FeishuChannel;

fn plugin() -> FeishuChannel {
    let config = Arc::new(FeishuConfig::for_app("app", Some("secret".to_string())));
    let http = PlatformHttp::new().expect("the platform client builds");
    let auth = Arc::new(TenantAuth::new(Arc::clone(&config), PlatformHttp::new().expect("client")));
    let api = FeishuApi::new(Arc::clone(&config), http, auth);
    FeishuChannel::new(
        config,
        api,
        HealthState::disabled("feishu", "app", "test fixture"),
        ReplyContextCache::new(16, std::time::Duration::from_secs(1800)),
    )
}

/// A target that names one chat of the bot.
fn chat_target(plugin: &FeishuChannel, kind: &str, id: &str) -> PublicationTarget {
    PublicationTarget::new(
        "feishu",
        kind,
        id.to_string(),
        plugin.config().account_scope(),
    )
    .with_scope(None, Some(id.to_string()))
}

/// An edit or a recall with nothing of the bot's own to address says so
/// instead of pretending.
#[test]
fn an_edit_without_anything_to_edit_is_refused() {
    let plugin = plugin();
    let target = chat_target(&plugin, FEISHU_KIND_EDIT, "chat-1");
    let error = plugin.describe(&target).expect_err("nothing to edit");
    assert!(
        matches!(error, ChannelError::UnresolvedReply { .. }),
        "the refusal names the audience it could not resolve: {error:?}"
    );
}

/// Once the bot has spoken in a chat, an edit addresses that post.
#[test]
fn an_edit_addresses_the_bots_own_last_post() {
    let plugin = plugin();
    FeishuChannel::remember_own(
        &plugin.own_cache(),
        chrono::Duration::minutes(30),
        "chat-2",
        "om-9",
    );
    let target = chat_target(&plugin, FEISHU_KIND_EDIT, "chat-2");
    let dispatch = plugin.describe(&target).expect("the edit is describable");
    assert_eq!(dispatch.action, ChannelAction::Edit);
    assert_eq!(dispatch.platform_action, "edit_message");
    assert_eq!(
        dispatch.params["message_id"].as_str(),
        Some("om-9"),
        "the edit addresses the bot's own post"
    );
}

/// A recall names the post it withdraws.
#[test]
fn a_recall_addresses_the_bots_own_last_post() {
    let plugin = plugin();
    FeishuChannel::remember_own(
        &plugin.own_cache(),
        chrono::Duration::minutes(30),
        "chat-3",
        "om-3",
    );
    let target = chat_target(&plugin, FEISHU_KIND_RECALL, "chat-3");
    let dispatch = plugin.describe(&target).expect("the recall is describable");
    assert_eq!(dispatch.action, ChannelAction::Retract);
    assert_eq!(dispatch.platform_action, "recall_message");
    assert_eq!(dispatch.params["message_id"].as_str(), Some("om-3"));
}

/// The capability table lists what the plugin can really produce.
#[test]
fn the_capability_table_only_lists_reachable_actions() {
    let plugin = plugin();
    let capabilities = plugin.capabilities();
    assert!(capabilities.edit, "the plugin can really edit");
    assert!(capabilities.retract, "the plugin can really recall");
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
    let target = plugin
        .resolve_reply(FEISHU_KIND_CHAT, "chat-4")
        .expect("the ordinary kind resolves");
    assert_eq!(target.kind, FEISHU_KIND_CHAT);
    assert_eq!(target.id, "chat-4");
}
