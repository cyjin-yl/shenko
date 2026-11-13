//! IM-channel plugin boundary tests: the reply grammar, reply-target
//! admission and ingress staging all resolve through the channel registry.
//! The real QQ plugin comes from `shenko-channel-napcat`; a local stub proves
//! the boundary is open to channels the subject core has never heard of.
use std::sync::Arc;

use shenko_channel_napcat::{QQ_CHANNEL_ID, qq_test_registry};
use shenko_identity::{DefaultIdentityPolicy, IdentityService};
use shenko_subject::output::parse_output;
use shenko_subject::{canonical_system_prompt, staged_message_text};
use shenko_types::channel::{
    BoxFuture, ChannelAction, ChannelDispatch, ChannelEnvelope, ChannelError, ChannelPlugin,
    ChannelReceipt, ChannelRegistry, ChannelReplyTarget, ReplyKind,
};
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::output::{OutputAction, PublicationTarget};

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

fn parse(
    raw: &str,
    channels: &ChannelRegistry,
) -> Result<shenko_types::output::ParsedOutput, shenko_types::error::ShenkoError> {
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
struct StubChannel {
    channel: String,
    source: String,
    kinds: Vec<ReplyKind>,
    prompt_names: &'static str,
}

impl ChannelPlugin for StubChannel {
    fn channel_id(&self) -> &str {
        &self.channel
    }

    fn account_scope(&self) -> String {
        "relay:local".to_string()
    }

    fn capabilities(&self) -> shenko_types::capabilities::ChannelCapabilities {
        shenko_types::capabilities::ChannelCapabilities {
            direct: false,
            group: true,
            thread: false,
            guild_channel: false,
            rich_text: false,
            attachments: false,
            edit: false,
            retract: false,
            typing: false,
            proactive: false,
            notes: vec!["a test stub that serves one room audience".to_string()],
        }
    }

    fn sources(&self) -> &[String] {
        std::slice::from_ref(&self.source)
    }

    fn kinds(&self) -> &[ReplyKind] {
        &self.kinds
    }

    fn prompt_names(&self) -> &str {
        self.prompt_names
    }

    fn classify(&self, source: &str, raw: &serde_json::Value) -> Option<ChannelEnvelope> {
        if source != "relay-bridge"
            || raw.get("channel_type").and_then(|value| value.as_str()) != Some("room")
        {
            return None;
        }
        let id = raw
            .get("room_id")
            .and_then(|value| value.as_u64())
            .filter(|id| *id > 0)?
            .to_string();
        let who = raw
            .get("nickname")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let text = raw
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        Some(ChannelEnvelope {
            schema_version: 1,
            provider: self.channel.clone(),
            account: "relay:local".to_string(),
            tenant: None,
            transport_identity_domain: "relay-bridge".to_string(),
            conversation: Some(id.clone()),
            thread: None,
            message_id: raw
                .get("message_id")
                .and_then(|value| value.as_u64())
                .map(|id| id.to_string())
                .unwrap_or_default(),
            source_event_revision: None,
            occurred_at: None,
            received_at: chrono::Utc::now(),
            actor: shenko_types::channel::ChannelActor {
                platform_id: who.clone(),
                display_name: if who.is_empty() { None } else { Some(who) },
                alias: None,
                person_ref: None,
                role_grant_ref: None,
                group_role: None,
                own_account: None,
            },
            content: shenko_types::channel::ChannelContent {
                text,
                segments: vec![],
                mentions: vec![],
                reply_to: None,
            },
            reply_target: Some(ChannelReplyTarget {
                kind: "room".to_string(),
                id: id.clone(),
                label: format!("room {id}"),
                account: "relay:local".to_string(),
                tenant: None,
                conversation: Some(id),
                shape: None,
            }),
            attachments: vec![],
            raw_ref: None,
        })
    }

    fn staged_text(&self, envelope: &ChannelEnvelope) -> String {
        let who = envelope
            .actor
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(envelope.actor.platform_id.as_str());
        match &envelope.reply_target {
            Some(target) => format!("[{who} · {}] {}", target.label, envelope.content.text),
            None => format!("[{who}] {}", envelope.content.text),
        }
    }

    fn resolve_reply(&self, kind: &str, id: &str) -> Option<PublicationTarget> {
        if kind != "room" || id.is_empty() {
            return None;
        }
        Some(
            PublicationTarget::new(self.channel.clone(), "room", id, "relay:local")
                .with_scope(None, Some(id.to_string())),
        )
    }

    fn describe(&self, target: &PublicationTarget) -> Result<ChannelDispatch, ChannelError> {
        Err(ChannelError::UnsupportedTarget {
            channel: target.channel.clone(),
            kind: target.kind.clone(),
        })
    }

    fn dispatch(
        &self,
        dispatch: &ChannelDispatch,
        _content: &str,
    ) -> BoxFuture<'static, Result<ChannelReceipt, ChannelError>> {
        let channel = dispatch.channel.clone();
        let kind = dispatch.platform_action.clone();
        Box::pin(async move {
            Err(ChannelError::UnsupportedTarget { channel, kind })
        })
    }
}

fn stub_channel() -> StubChannel {
    StubChannel {
        channel: "relay".to_string(),
        source: "relay-bridge".to_string(),
        kinds: vec![ReplyKind {
            tag: "room",
            id_hint: "房间号",
            audience: "房间号",
        }],
        prompt_names: "房间号",
    }
}

fn stub_registry() -> ChannelRegistry {
    let mut registry = ChannelRegistry::new();
    registry.register(Arc::new(stub_channel())).unwrap();
    registry
}

fn qq_group_target(id: &str) -> PublicationTarget {
    PublicationTarget::new(QQ_CHANNEL_ID, "group", id, "qq:10000")
        .with_scope(None, Some(id.to_string()))
}

/// Registry-driven parse: the registered kind resolves to a typed target
/// carrying its channel id and the connector account scope; an unregistered
/// kind stays a protocol error.
#[test]
fn registered_and_unknown_reply_kinds() {
    let channels = qq_test_registry();
    let parsed = parse("[reply:group:7] 大家好", &channels).unwrap();
    let OutputAction::Reply { target, .. } = &parsed.controls[0].action else {
        panic!("expected reply control")
    };
    assert_eq!(target, &qq_group_target("7"));
    let private = parse("[reply:private:9] 你好", &channels).unwrap();
    let OutputAction::Reply { target, .. } = &private.controls[0].action else {
        panic!("expected reply control")
    };
    assert_eq!(
        target,
        &PublicationTarget::new(QQ_CHANNEL_ID, "private", "9", "qq:10000")
            .with_scope(None, Some("9".to_string()))
    );

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
    assert_eq!(
        target,
        &PublicationTarget::new("relay", "room", "42", "relay:local")
            .with_scope(None, Some("42".to_string()))
    );
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

#[test]
fn inline_html_examples_remain_root_text() -> Result<(), shenko_types::error::ShenkoError> {
    let literal = concat!(
        "Example <!--\n",
        "[remember] literal memory\n",
        "[reply:room:a+b/c=] literal reply\n",
        "-->\n\n",
    );
    let raw = format!("{literal}[remember] actual memory\n");
    let parsed = parse(&raw, &stub_registry())?;
    assert_eq!(parsed.text, literal);
    assert_eq!(parsed.controls.len(), 1);
    assert!(matches!(
        &parsed.controls[0].action,
        OutputAction::Remember { text } if text == "actual memory"
    ));
    Ok(())
}

#[test]
fn opaque_reply_ids_reach_the_channel_without_reclassifying_root_literals()
    -> Result<(), shenko_types::error::ShenkoError>
{
    let literal = concat!(
        "[reply:room:a+b/c=](/example)\n\n",
        "[reply:room:reference+a/b=] linked example\n\n",
        "[reply:room:reference+a/b=]: /original\n\n",
        "\\[reply:room:a+b/c=] escaped example\n\n",
        "    [reply:room:a+b/c=] indented code\n\n",
        "> [reply:room:a+b/c=] quoted example\n\n",
    );
    let raw = format!("{literal}[reply:room:a+b/c=] message\n");
    let parsed = parse(&raw, &stub_registry())?;
    assert_eq!(parsed.text, format!("{literal}message\n"));
    assert_eq!(parsed.controls.len(), 1);
    let OutputAction::Reply { target, text } = &parsed.controls[0].action else {
        panic!("expected the channel to resolve the opaque reply identity")
    };
    assert_eq!(
        target,
        &PublicationTarget::new("relay", "room", "a+b/c=", "relay:local")
            .with_scope(None, Some("a+b/c=".to_string()))
    );
    assert_eq!(text, "message\n");
    Ok(())
}

/// The prompt contract is assembled from the registered plugins: the shipped
/// QQ grammar survives byte-identical, and an empty registry says plainly
/// that no reply can be delivered.
#[test]
fn contract_text_comes_from_the_registry() {
    let qq = canonical_system_prompt(&qq_test_registry());
    assert!(
        qq.contains("[reply:group:群号] 正文 或 [reply:private:QQ号] 正文"),
        "{qq}"
    );
    assert!(qq.contains("可回复的 QQ 号或群号"), "{qq}");

    let mut both = ChannelRegistry::new();
    both.register(Arc::new(shenko_channel_napcat::NapCatChannel::new(
        shenko_channel_napcat::config::NapCatConfig {
            bot_qq: 10000,
            ..Default::default()
        },
        {
            let config = shenko_channel_napcat::config::NapCatConfig {
                bot_qq: 10000,
                ..Default::default()
            };
            let (client, _events, _actions) =
                shenko_channel_napcat::NapCatClient::open_isolated(config).unwrap();
            Arc::new(client)
        },
    )))
    .unwrap();
    both.register(Arc::new(stub_channel())).unwrap();
    let multi = canonical_system_prompt(&both);
    assert!(multi.contains("[reply:room:房间号]"), "{multi}");
    assert!(multi.contains("房间号"), "{multi}");

    let none = canonical_system_prompt(&ChannelRegistry::new());
    assert!(none.contains("当前没有注册的回复通道"), "{none}");
    assert!(!none.contains("[reply:"), "{none}");
}

/// Ambiguous descriptors are refused at registration, not at parse time.
#[test]
fn registry_rejects_duplicate_and_invalid_descriptors() {
    use shenko_types::channel::ChannelRegistryError;
    let mut registry = qq_test_registry();
    let conflicting = StubChannel {
        channel: "relay".to_string(),
        source: "relay-bridge".to_string(),
        kinds: vec![ReplyKind {
            tag: "group",
            id_hint: "x",
            audience: "x",
        }],
        prompt_names: "x",
    };
    assert!(matches!(
        registry.register(Arc::new(conflicting)),
        Err(ChannelRegistryError::DuplicateTag)
    ));
    let same_source = StubChannel {
        channel: "relay2".to_string(),
        source: "napcat".to_string(),
        kinds: stub_channel().kinds.clone(),
        prompt_names: "房间号",
    };
    assert!(matches!(
        registry.register(Arc::new(same_source)),
        Err(ChannelRegistryError::DuplicateSource)
    ));
    let bad_tag = StubChannel {
        channel: "relay3".to_string(),
        source: "relay3".to_string(),
        kinds: vec![ReplyKind {
            tag: "bad:tag",
            id_hint: "x",
            audience: "x",
        }],
        prompt_names: "x",
    };
    assert!(matches!(
        registry.register(Arc::new(bad_tag)),
        Err(ChannelRegistryError::InvalidTag)
    ));
}

/// Staged headers use the plugin's own label; events from unregistered
/// sources, or registered-channel payloads the plugin does not classify,
/// keep the plain `[who]` header so no unparseable reply can be invited.
#[test]
fn staging_binds_labels_to_the_owning_channel() {
    let channels = qq_test_registry();
    let group = staged_message_text(
        &serde_json::json!({"post_type":"message","message_type":"group","group_id":7,"sender":{"nickname":"小明"},"raw_message":"在吗"}),
        "napcat",
        &channels,
    );
    assert_eq!(group, "[小明 · 群 7] 在吗");
    let private = staged_message_text(
        &serde_json::json!({"post_type":"message","message_type":"private","user_id":9,"sender":{"nickname":"小蓝"},"raw_message":"hi"}),
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
        &serde_json::json!({"post_type":"notice","raw_message":"系统消息"}),
        "napcat",
        &channels,
    );
    assert_eq!(no_account, "[napcat] 系统消息");
}

/// Records sealed before the channel boundary (`{"kind":"qq_…","id":N}`)
/// still read through the explicit legacy reader used by the Store
/// migration; the versioned deserializer itself accepts only v2. Admission
/// of new targets is the registry's job, so this reader only preserves the
/// old bytes.
#[test]
fn legacy_qq_publication_records_still_read() {
    let group = PublicationTarget::read_legacy(&serde_json::json!({"kind":"qq_group","id":7}))
        .expect("legacy group target reads");
    assert_eq!(group.channel, QQ_CHANNEL_ID);
    assert_eq!(group.kind, "group");
    assert_eq!(group.id, "7");
    assert!(group.account.is_empty());
    let private = PublicationTarget::read_legacy(&serde_json::json!({"kind":"qq_private","id":9}))
        .expect("legacy private target reads");
    assert_eq!(private.kind, "private");
    assert_eq!(private.id, "9");

    // The strict reader refuses the old shape: only the migration may read it.
    #[derive(serde::Deserialize)]
    struct Intent {
        target: PublicationTarget,
    }
    assert!(
        serde_json::from_str::<Intent>(r#"{"target":{"kind":"qq_group","id":7}}"#).is_err()
    );
}
