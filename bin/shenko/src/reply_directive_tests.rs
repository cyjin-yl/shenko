use shenko_types::{ids::EntityId, output::*};
#[test]
fn identity_lines_are_actions_not_reply_text() {
    let out = "先想一下\n[reply:group:7]  你好\n[identity:name] 岚\n继续说\n[identity:soul] 记住关系；不要把 delegate 和委派当指令\n[remember] 一整段记忆\n[delegate] {\"goal\":\"完整有限目标\",\"system_prompt\":\"独立子角色\",\"model\":\"fixture-child\",\"max_tokens\":32}\n> [delegate] 引用，不执行\n[remember] 懒续行仍是引用\n\n[identity:personality] 这是链接旁的说明\n\n[identity:personality]: https://example.invalid/\n\n[delegate](https://example.invalid/)\n[1] 引用编号不是控制行\n```text\n[identity:name] 代码，不执行\n[remember] 代码例子\n```\n[reply:private:9] 第二条";
    let dir = std::env::temp_dir().join(format!("shenko-controls-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = shenko_store::store::Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
    let raw = store
        .cas
        .seal_object(out.as_bytes(), "text/plain", "test", "text", 1)
        .unwrap()
        .object;
    let identity = shenko_identity::IdentityService::new(std::sync::Arc::new(
        shenko_identity::DefaultIdentityPolicy,
    ));
    let parsed =
        shenko_subject::output::parse_output(EntityId::new(), raw.clone(), out, &identity, &shenko_channel_napcat::qq_test_registry(), None)
            .unwrap();
    assert_eq!(parsed.controls.len(), 6); // two replies, two identity, one memory, one task
    let first = &parsed.controls[0];
    let OutputAction::Reply { target, text } = &first.action else {
        panic!("reply")
    };
    assert_eq!(
        *target,
        PublicationTarget::new(shenko_channel_napcat::QQ_CHANNEL_ID, "group", "7", "qq:10000")
            .with_scope(None, Some("7".to_string()))
    );
    assert_eq!(
        text,
        " 你好\n继续说\n> [delegate] 引用，不执行\n[remember] 懒续行仍是引用\n\n[identity:personality] 这是链接旁的说明\n\n[identity:personality]: https://example.invalid/\n\n[delegate](https://example.invalid/)\n[1] 引用编号不是控制行\n```text\n[identity:name] 代码，不执行\n[remember] 代码例子\n```\n"
    );
    assert!(!parsed.text.contains("一整段记忆") && !parsed.text.contains("完整有限目标"));
    assert_eq!(
        &out[first.spans[0].start..first.spans[0].end],
        "[reply:group:7]  你好\n"
    );
    assert_eq!(
        store.cas.read_object(&parsed.raw_ref.digest).unwrap(),
        out.as_bytes()
    );
    // Unsupported old syntax is a diagnosed protocol error, not a fallback.
    let obsolete = store
        .cas
        .seal_object(b"[memory] obsolete", "text/plain", "test", "text", 1)
        .unwrap()
        .object;
    assert!(
        shenko_subject::output::parse_output(
            EntityId::new(),
            obsolete,
            "[memory] obsolete",
            &identity,
            &shenko_channel_napcat::qq_test_registry(),
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("CONTROL_PROTOCOL_INVALID")
    );
}

/// The semantic wait protocol is one typed control per result.
///
/// Malformed or duplicate wait controls reject the whole result, while
/// a quoted or fenced wait line remains literal prose.
#[test]
fn semantic_wait_controls_are_typed_and_atomic() {
    use shenko_types::subject::SemanticWaitChange;

    let dir = std::env::temp_dir().join(format!("shenko-wait-{}", EntityId::new()));
    std::fs::create_dir(&dir).unwrap();
    let store = shenko_store::store::Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
    let identity = shenko_identity::IdentityService::new(std::sync::Arc::new(
        shenko_identity::DefaultIdentityPolicy,
    ));
    let parse = |raw: &str| {
        let raw_ref = store
            .cas
            .seal_object(raw.as_bytes(), "text/plain", "test", "text", 1)
            .unwrap()
            .object;
        shenko_subject::output::parse_output(EntityId::new(), raw_ref, raw, &identity, &shenko_channel_napcat::qq_test_registry(), None)
    };

    // A start names the condition and its cap, next to an ordinary action.
    let started = parse(
        "先想清楚再等\n[wait] {\"condition\":\"实验出现足够判断的结果\",\"max_wait_secs\":120}\n[remember] 记住等待条件",
    )
    .unwrap();
    assert_eq!(started.controls.len(), 2);
    let OutputAction::SemanticWait { change } = &started.controls[0].action else {
        panic!("the wait control must be a typed semantic wait action")
    };
    assert_eq!(
        change,
        &SemanticWaitChange::Start {
            condition: "实验出现足够判断的结果".into(),
            max_wait_secs: 120,
        }
    );
    assert!(!started.text.contains("max_wait_secs"));

    // An out-of-range cap rejects the whole result, not just this action.
    let invalid = parse("[remember] 先记下\n[wait] {\"condition\":\"x\",\"max_wait_secs\":601}");
    assert!(invalid.unwrap_err().to_string().contains("CONTROL_PROTOCOL_INVALID"));

    // One wait control per result: a second one rejects the whole result.
    let duplicate = parse(
        "[wait] {\"condition\":\"x\",\"max_wait_secs\":10}\n[wait] {\"condition\":\"y\",\"max_wait_secs\":10}",
    );
    assert!(
        duplicate
            .unwrap_err()
            .to_string()
            .contains("CONTROL_PROTOCOL_INVALID")
    );

    // A quoted wait line is prose, not a control.
    let quoted = parse("> [wait] {\"condition\":\"引用不执行\",\"max_wait_secs\":10}\n").unwrap();
    assert!(quoted.controls.is_empty());
    assert!(quoted.text.contains("[wait] {\"condition\":\"引用不执行\""));

    // A fenced wait line is prose, not a control.
    let fenced = parse("```text\n[wait:end] 代码不执行\n```\n")
    .unwrap();
    assert!(fenced.controls.is_empty());
    assert!(fenced.text.contains("[wait:end]"));
}
