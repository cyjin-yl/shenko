//! The QQ channel declares what it can really do — docs/specs/channels.md#spec-57-4, RT19.
//!
//! The table is a promise in both directions: a `false` field means this
//! plugin will refuse the action instead of sending a request the platform
//! cannot keep, and a `true` field means `describe` can really produce it.
//! These cases check both halves against the plugin's own dispatch.

use std::sync::Arc;

use shenko_types::capabilities::ChannelCapabilities;
use shenko_types::channel::{ChannelAction, ChannelPlugin};
use shenko_types::output::PublicationTarget;

use crate::channel::{NapCatChannel, QQ_CHANNEL_ID};
use crate::client::NapCatClient;
use crate::config::NapCatConfig;

fn plugin() -> NapCatChannel {
    let config = NapCatConfig {
        bot_qq: 10000,
        ..Default::default()
    };
    let (client, _events, _actions) =
        NapCatClient::open_isolated(config.clone()).expect("isolated connector opens");
    NapCatChannel::new(config, Arc::new(client))
}

/// The table lists what the plugin can really produce — and nothing more.
#[test]
fn the_capability_table_only_lists_reachable_actions() {
    let plugin = plugin();
    let capabilities = plugin.capabilities();
    assert!(capabilities.direct, "private chats are reachable");
    assert!(capabilities.group, "group chats are reachable");
    assert!(capabilities.attachments, "the plugin resolves what it receives");
    assert!(capabilities.proactive, "the platform accepts an unsolicited send");
    assert!(!capabilities.edit, "this plugin never edits a message");
    assert!(!capabilities.retract, "this plugin never retracts a message");
    assert!(!capabilities.typing, "there is no typing action it calls");
    assert!(
        !capabilities.rich_text,
        "QQ renders no markup: the body is plain text and Markdown is downgraded"
    );
    assert!(
        capabilities
            .notes
            .iter()
            .any(|note| note.contains("send_group_msg")),
        "the reason is stated, not hidden: {:?}",
        capabilities.notes
    );
}

/// A declared action is really producible: the send kinds describe to a
/// concrete platform action, and the target is the bot's own account.
#[test]
fn a_declared_action_describes_to_a_real_platform_action() {
    let plugin = plugin();
    for (kind, id, action) in [
        ("group", "12345", "send_group_msg"),
        ("private", "10001", "send_private_msg"),
    ] {
        let target = PublicationTarget::new(
            QQ_CHANNEL_ID,
            kind,
            id.to_string(),
            plugin.account_scope(),
        );
        let dispatch = plugin
            .describe(&target)
            .expect("the declared audience is describable");
        assert_eq!(dispatch.action, ChannelAction::Send);
        assert_eq!(dispatch.platform_action, action);
        assert_eq!(dispatch.account, plugin.account_scope());
    }
}

/// The account the plugin serves is its own QQ number, and it says so.
#[test]
fn the_account_scope_is_the_bots_own_number() {
    let plugin = plugin();
    assert_eq!(plugin.account_scope(), "qq:10000");
}

/// An action the table does not offer is not addressable either: the plugin
/// refuses before the platform sees a request.
#[test]
fn an_unoffered_action_is_not_addressable() {
    let plugin = plugin();
    for kind in ["edit", "retract", "typing"] {
        assert!(
            plugin.resolve_reply(kind, "12345").is_none(),
            "{kind} is not offered, so it does not resolve"
        );
    }
}

/// The capability table is part of the plugin contract, so the runtime can
/// read it for every installed channel without knowing the platform.
#[test]
fn the_capability_table_is_part_of_the_plugin_contract() {
    let plugin = plugin();
    let table: ChannelCapabilities = ChannelPlugin::capabilities(&plugin);
    assert_eq!(table, plugin.capabilities());
}
