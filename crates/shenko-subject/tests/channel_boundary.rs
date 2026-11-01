//! IM-channel plugin boundary tests: the reply grammar, reply-target
//! admission and ingress staging all resolve through the channel registry.
//! The real QQ plugin comes from `shenko-channel-napcat`; a local stub proves
//! the boundary is open to channels the subject core has never heard of.
use shenko_channel_napcat::{QQ_CHANNEL_ID, qq_registry};
use shenko_identity::{DefaultIdentityPolicy, IdentityService};
use shenko_subject::{canonical_system_prompt, staged_message_text};
use shenko_subject::output::parse_output;
use shenko_types::channel::{ChannelPlugin, ChannelRegistry, ReplyKind};
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::output::{OutputAction, PublicationTarget};
use std::sync::Arc;

fn raw_object(raw: &str) -> ObjectRef {
    ObjectRef {
        digest: Digest::blake3(raw.as_bytes()),
        bytes: raw.len() as u64,
        media_type: "text/plain".into(),
        store_namespace: "channel-boundary".into(),
        locator: "channel-boundary:test".into(),
        codec: shenko_types::object::SchemaRef {
            name: "raw".into(),
            major: 1,
            digest: Digest::blake3(b"raw"),
        },
    }
}

fn parse(raw: &str, channels: &ChannelRegistry) -> Result<shenko_types::output::ParsedOutput, shenko_types::error::ShenkoError> {
    let identity = IdentityService::new(Arc::new(DefaultIdentityPolicy));
    parse_output(
        EntityId::new(),
        raw_object(raw),
        raw,
        &identity,
        channels,
        None,
    )
}

/// A minimal second channel the subject core knows nothing about: one "room"
/// audience and its own staged-target projection keyed off `room_id`.
fn stub_channel() -> ChannelPlugin {
    ChannelPlugin {
        channel: "relay",
        sources: &["relay-bridge"],
        kinds: &[ReplyKind {
            tag: "room",
            id_hint: "房间号",
            audience: "房间号",
        }],
        prompt_names: "房间号",
        staged_target: |payload| {
            (payload.get("channel_type").and_then(|t| t.as_str()) == Some("room"))
                .then(|| {
                    payload
                        .get("room_id")
                        .and_then(|id| id.as_u64())
                        .filter(|id| *id > 0)
                })
                .flatten()
                .map(|id| format!("room {id}"))
        },
    }
}

fn stub_registry() -> ChannelRegistry {
    let mut registry = ChannelRegistry::new();
    registry.register(stub_channel()).unwrap();
    registry
}

/// Registry-driven parse: the registered kind resolves to a typed target
/// carrying its channel id; an unregistered kind stays a protocol error.
#[test]
fn registered_and_unknown_reply_kinds() {
    let channels = qq_registry();
    let parsed = parse("[reply:group:7] 大家好", &channels).unwrap();
    let OutputAction::Reply { target, .. } = &parsed.controls[0].action else {
        panic!("expected reply control")
    };
    assert_eq!(target, &PublicationTarget::new(QQ_CHANNEL_ID, "group", 7));
    let private = parse("[reply:private:9] 你好", &channels).unwrap();
    let OutputAction::Reply { target, .. } = &private.controls[0].action else {
        panic!("expected reply control")
    };
    assert_eq!(target, &PublicationTarget::new(QQ_CHANNEL_ID, "private", 9));

    for raw in ["[reply:bogus:5] x", "[reply:matrix:5] x"] {
        let error = parse(raw, &channels).unwrap_err().to_string();
        assert!(error.contains("CONTROL_PROTOCOL_INVALID"), "{raw}: {error}");
    }
    // Registration is authoritative: a different channel's tag is not admitted
    // just because the model happened to emit it.
    let stub_only = stub_registry();
    let error = parse("[reply:group:5] x", &stub_only)
        .unwrap_err()
        .to_string();
    assert!(error.contains("CONTROL_PROTOCOL_INVALID"), "{error}");
}

/// A channel the core never heard of drives grammar, parse and staging on its
/// own descriptor — the boundary, not the platform, is what is registered.
#[test]
fn stub_channel_is_a_first_class_reply_target() {
    let channels = stub_registry();
    let parsed = parse("[reply:room:42] 到了", &channels).unwrap();
    let OutputAction::Reply { target, text } = &parsed.controls[0].action else {
        panic!("expected reply control")
    };
    assert_eq!(target, &PublicationTarget::new("relay", "room", 42));
    assert!(text.contains("到了"));

    let staged = staged_message_text(
        &serde_json::json!({"channel_type":"room","room_id":42,"nickname":"桥","text":"hi"}),
        "relay-bridge",
        &channels,
    );
    assert_eq!(staged, "[桥 · room 42] hi");

    let prompt = canonical_system_prompt(&channels);
    assert!(prompt.contains("[reply:room:房间号]"), "{prompt}");
    assert!(!prompt.contains("[reply:group:"), "{prompt}");
}

/// The prompt contract is assembled from the registered plugins: the shipped
/// QQ grammar survives byte-identical, and an empty registry says plainly
/// that no reply can be delivered.
#[test]
fn contract_text_comes_from_the_registry() {
    let qq = canonical_system_prompt(&qq_registry());
    assert!(
        qq.contains("[reply:group:群号] 正文 或 [reply:private:QQ号] 正文"),
        "{qq}"
    );
    assert!(qq.contains("可回复的 QQ 号或群号"), "{qq}");

    let mut both = ChannelRegistry::new();
    both.register(shenko_channel_napcat::qq_channel_plugin()).unwrap();
    both.register(stub_channel()).unwrap();
    let multi = canonical_system_prompt(&both);
    assert!(multi.contains("[reply:room:房间号]"), "{multi}");
    assert!(multi.contains("房间号"), "{multi}");

    let none = canonical_system_prompt(&ChannelRegistry::new());
    assert!(none.contains("没有注册的回复通道"), "{none}");
    assert!(!none.contains("[reply:"), "{none}");
}

/// Ambiguous descriptors are refused at registration, not at parse time.
#[test]
fn registry_rejects_duplicate_and_invalid_descriptors() {
    let mut registry = qq_registry();
    let mut conflicting = stub_channel();
    conflicting.kinds = &[ReplyKind {
        tag: "group",
        id_hint: "x",
        audience: "x",
    }];
    assert!(registry.register(conflicting).is_err());
    let mut same_source = stub_channel();
    same_source.channel = "relay2";
    same_source.sources = &["napcat"];
    assert!(registry.register(same_source).is_err());
    let mut bad_tag = stub_channel();
    bad_tag.channel = "relay3";
    bad_tag.sources = &["relay3"];
    bad_tag.kinds = &[ReplyKind {
        tag: "bad:tag",
        id_hint: "x",
        audience: "x",
    }];
    assert!(registry.register(bad_tag).is_err());
}

/// Staged headers use the plugin's own label; events from unregistered
/// sources, or registered-channel events without a replyable account, keep
/// the plain `[who]` header so no unparseable reply can be invited.
#[test]
fn staging_binds_labels_to_the_owning_channel() {
    let channels = qq_registry();
    let group = staged_message_text(
        &serde_json::json!({"message_type":"group","group_id":7,"sender":{"nickname":"小明"},"raw_message":"在吗"}),
        "napcat",
        &channels,
    );
    assert_eq!(group, "[小明 · 群 7] 在吗");
    let private = staged_message_text(
        &serde_json::json!({"message_type":"private","user_id":9,"nickname":"小蓝","raw_message":"hi"}),
        "napcat",
        &channels,
    );
    assert_eq!(private, "[小蓝 · 私聊 QQ 9] hi");
    let unknown_source = staged_message_text(
        &serde_json::json!({"message_type":"group","group_id":7,"raw_message":"x"}),
        "nowhere",
        &channels,
    );
    assert_eq!(unknown_source, "[nowhere] x");
    let no_account = staged_message_text(
        &serde_json::json!({"message_type":"notice","raw_message":"系统消息"}),
        "napcat",
        &channels,
    );
    assert_eq!(no_account, "[napcat] 系统消息");
}

/// Records sealed before the channel boundary (`{"kind":"qq_…","id":N}`)
/// still deserialize into the new shape; admission of new targets is the
/// registry's job, so this reader only preserves the old bytes.
#[test]
fn legacy_qq_publication_records_still_read() {
    #[derive(serde::Deserialize)]
    struct Intent {
        target: PublicationTarget,
    }
    let group: Intent =
        serde_json::from_str(r#"{"target":{"kind":"qq_group","id":7}}"#).unwrap();
    assert_eq!(
        group.target,
        PublicationTarget::new(QQ_CHANNEL_ID, "group", 7)
    );
    let private: Intent =
        serde_json::from_str(r#"{"target":{"kind":"qq_private","id":9}}"#).unwrap();
    assert_eq!(
        private.target,
        PublicationTarget::new(QQ_CHANNEL_ID, "private", 9)
    );
}
