//! Platform notices become observations the subject can see — docs/specs/channels.md#spec-57-4.
//!
//! A recall means a message the subject may have quoted no longer exists; a
//! member event is exactly the kind of thing a personal agent is supposed to
//! notice. Before this, a notice reached the subject as a raw JSON payload
//! with a plain header, so the subject could neither read it nor tell a
//! retraction from a join.

use std::sync::Arc;

use serde_json::json;

use crate::channel::{NapCatChannel, QQ_CHANNEL_ID, QQ_SOURCE};
use crate::client::NapCatClient;
use crate::config::NapCatConfig;
use shenko_types::channel::ChannelPlugin;

fn channel(config: NapCatConfig) -> NapCatChannel {
    let (client, _events, _actions) =
        NapCatClient::open_isolated(config.clone()).expect("isolated connector opens");
    NapCatChannel::new(config, Arc::new(client))
}

fn group_config() -> NapCatConfig {
    NapCatConfig {
        bot_qq: 10000,
        admin_qq: vec![20002],
        ..Default::default()
    }
}

/// A recall is an observation the subject must see: the message it may have
/// quoted no longer exists, and the subject has to know that before it
/// answers.
#[test]
fn a_recall_is_classified_as_an_observation() {
    let plugin = channel(group_config());
    let notice = json!({
        "post_type": "notice",
        "notice_type": "group_recall",
        "group_id": 7,
        "user_id": 20001,
        "operator_id": 20001,
        "message_id": 901,
        "time": 1_700_000_000
    });
    let envelope = plugin.classify(QQ_SOURCE, &notice).expect("a recall classifies");
    assert_eq!(envelope.provider, QQ_CHANNEL_ID);
    assert_eq!(envelope.conversation.as_deref(), Some("7"));
    assert_eq!(envelope.actor.platform_id, "20001");
    assert!(
        envelope.content.text.contains("撤回了消息"),
        "the observation says what happened: {}",
        envelope.content.text
    );
    assert!(
        envelope.message_id.starts_with("notice:group_recall:"),
        "the event identity is namespaced: {}",
        envelope.message_id
    );
    // The same notice seen twice is one observation, not two.
    let again = plugin.classify(QQ_SOURCE, &notice).expect("a recall classifies");
    assert_eq!(again.message_id, envelope.message_id);
    // The reply audience is still available: the group can be addressed.
    let target = envelope
        .reply_target
        .as_ref()
        .expect("the group stays addressable");
    assert_eq!(target.kind, "group");
    assert_eq!(target.id, "7");
    // The staged text is what the model reads.
    let staged = plugin.staged_text(&envelope);
    assert!(staged.contains("撤回了消息"), "staged text: {staged}");
}

/// A member event is an observation too, and an unknown notice stays
/// unclassified so nothing invents a meaning for it.
#[test]
fn member_events_classify_and_unknown_notices_do_not() {
    let plugin = channel(group_config());
    let increase = json!({
        "post_type": "notice",
        "notice_type": "group_increase",
        "group_id": 7,
        "user_id": 30003,
        "operator_id": 20001,
        "time": 1_700_000_001
    });
    let envelope = plugin
        .classify(QQ_SOURCE, &increase)
        .expect("a member event classifies");
    assert!(envelope.content.text.contains("加入"));
    let unknown = json!({
        "post_type": "notice",
        "notice_type": "some_future_event",
        "group_id": 7,
        "user_id": 20001,
        "time": 1_700_000_002
    });
    assert!(
        plugin.classify(QQ_SOURCE, &unknown).is_none(),
        "an unknown notice keeps the plain-header fallback"
    );
}

/// An ordinary message is still a message: the notice branch does not
/// swallow the conversation itself.
#[test]
fn ordinary_messages_still_classify() {
    let plugin = channel(group_config());
    let message = json!({
        "post_type": "message",
        "message_type": "group",
        "group_id": 7,
        "user_id": 20001,
        "message_id": 902,
        "sender": {"user_id": 20001, "nickname": "甲"},
        "message": [{"type": "text", "data": {"text": "在吗"}}]
    });
    let envelope = plugin.classify(QQ_SOURCE, &message).expect("a message classifies");
    assert_eq!(envelope.message_id, "902");
    assert_eq!(plugin.staged_text(&envelope), "[甲 · 群 7] 在吗");
}
