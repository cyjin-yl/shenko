//! 本地 SQLite/CAS 恢复验证，不代表真实 provider 或部署验收。
use super::{SubjectLoopState, execute_step};
use crate::inbox::Inbox;
use shenko_store::store::Store;
use shenko_types::error::ShenkoError;
use shenko_types::{commit::CommitRef, ids::*, subject::*};
use std::{path::PathBuf, sync::Arc};

fn fixture() -> (PathBuf, Arc<Store>, EntityId) {
    let dir = std::env::temp_dir().join(format!("shenko-recovery-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(b"seed", "text/plain", "test", "raw", 1)
        .unwrap()
        .object;
    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(),
        initial_profile_ref: object.clone(),
        home_manifest_ref: object.clone(),
        bootstrap_event_ref: object,
        created_commit: precommit,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(7),
        software_generation: RuntimeGeneration("fixture-generation".into()),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: precommit,
        next_step_id: first_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first = SubjectStep {
        step_id: first_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: precommit,
        controller_epoch: head.controller_epoch,
        software_generation: head.software_generation.clone(),
        state: SubjectStepState::Planned,
        input_plan_ref: None,
        context_revision: None,
        active_request_id: None,
        active_attempt_id: None,
        model_result_ref: None,
        model_outcome_ref: None,
        decision_ref: None,
        commit_ref: None,
        retry_reason_ref: None,
        record_revision: RecordRevision(0),
        created_by_step_id: None,
    };
    store.initialize_genesis(&genesis, &head, &first).unwrap();
    (dir, store, agent)
}

fn state(
    store: Arc<Store>,
    agent: EntityId,
) -> Result<SubjectLoopState, shenko_types::error::ShenkoError> {
    let ctx = cordis_core::Context::new();
    let _publication = ctx
        .provide(Arc::new(shenko_identity::IdentityService::new(Arc::new(
            shenko_identity::DefaultIdentityPolicy,
        ))))
        .unwrap();
    let (head, _) = store.load_subject(agent)?;
    let _context = ctx.provide(Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
        shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(Arc::clone(&store), EntityId(head.branch_id.0)),
        )),
    ))).unwrap();
    SubjectLoopState::new(agent, Arc::new(Inbox::new(8)), store, ctx)
}

#[test]
fn reopening_awaiting_model_keeps_original_request_blocked() {
    let (dir, store, agent) = fixture();
    let (head, mut step) = store.load_subject(agent).unwrap();
    step.state = SubjectStepState::Preparing;
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(store.clone(), EntityId(head.branch_id.0)),
    ));
    let profile = shenko_types::object::Digest::blake3(b"unknown-request-fixture");
    ledger.rebind_model(1, &profile.0).unwrap();
    let window = ledger.current().unwrap();
    let scope_id = EntityId(head.branch_id.0);
    store.stage_subject_inputs(&head, &step, &[]).unwrap();
    let plan = serde_json::json!({
        "schema_version":10, "step_id":step.step_id, "step_no":step.step_no,
        "events":[], "semantic_wait":null,
        "context":{"source":{"scope_id":scope_id,
            "source_commit_seq":store.current_commit_seq().unwrap(), "leaf_seq":null,
            "input_raw_event_ids":[], "window_id":window.id,
            "window_generation":window.generation, "model_generation":1},
        "profile_digest":profile,
        "estimated_input_tokens":null, "budget_input_tokens":null,
        "trigger_budget_input_tokens":null,
        "budget_provenance":"recovery fixture without measured current input",
        "reserved_output_tokens":4096, "unfinished_call_ids":[],
        },
        "payload":{"system_prompt":"unknown request fixture", "messages":[],
            "max_tokens":4096, "tools":[]},
    });
    step.input_plan_ref = Some(store.cas.seal_object(
        &serde_json::to_vec(&plan).unwrap(), "application/json", "input_plans",
        "subject_step_input", 10).unwrap().object);
    step.record_revision = RecordRevision(1);
    store.upsert_step(&step, &head, RecordRevision(0)).unwrap();
    let request = EntityId::new();
    let attempt = EntityId::new();
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(request);
    step.active_attempt_id = Some(attempt);
    step.record_revision = RecordRevision(2);
    store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
    drop(store);

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let error = state(reopened.clone(), agent)
        .err()
        .expect("未知请求不得构造可运行主体");
    assert!(error.to_string().contains("recovery_blocked"));
    let (restored, unresolved) = reopened.load_subject(agent).unwrap();
    assert_eq!(restored.committed_at, head.committed_at);
    assert_eq!(restored.next_step_id, step.step_id);
    assert_eq!(unresolved.state, SubjectStepState::AwaitingModel);
    assert_eq!(unresolved.active_request_id, Some(request));
    assert_eq!(unresolved.active_attempt_id, Some(attempt));
    assert_eq!(unresolved.record_revision, RecordRevision(2));
    assert!(unresolved.commit_ref.is_none());
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

fn message(content: &str) -> crate::inbox::InboxEvent {
    crate::inbox::InboxEvent {
        event_id: EntityId::new(), kind: crate::inbox::InboxEventKind::Message,
        payload: serde_json::json!({"content":content}), created_at: chrono::Utc::now(),
        source: "web_ui".into(),
    }
}

#[tokio::test]
async fn idle_history_reopens_as_new_anthropic_turn_not_assistant_prefill() {
    use shenko_broker::{provider::AnthropicProvider, request::*};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn wire(payload: InferencePayload) -> serde_json::Value {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (start, length) = loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length: usize = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                    }).unwrap();
                    if bytes.len() >= end + 4 + length { break (end + 4, length); }
                }
            };
            let body = serde_json::from_slice(&bytes[start..start + length]).unwrap();
            let raw = r#"{"content":[{"type":"text","text":"fixture"}],"stop_reason":"end_turn"}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}", raw.len(), raw).as_bytes()).await.unwrap();
            body
        });
        let provider = AnthropicProvider { api_key: "offline-fixture".into(), base_url: base_url.clone(), model: "fixture".into() };
        let (result_tx, _) = tokio::sync::oneshot::channel();
        provider.complete(&InferenceRequest {
            request_id: EntityId::new(), agent_id: EntityId::new(), conversation_id: EntityId::new(),
            provider_binding: ProviderBinding::Anthropic { base_url, model: "fixture".into(),
                api_version: "2026-10-21".into(), serializer_revision: 3, default_max_tokens: 8192 },
            priority: InferencePriority::Subject, epoch: ControllerEpoch(7), payload, result_tx,
        }).await.unwrap();
        server.await.unwrap()
    }

    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    store.append_subject_raw_fact(&subject.head, EntityId::new(), "user", &serde_json::json!({"v":1,"text":"earlier question"})).unwrap();
    store.append_subject_raw_fact(&subject.head, EntityId::new(), "assistant", &serde_json::json!({"v":1,"text":"completed answer"})).unwrap();
    let prepared = crate::context_projection::prepare_context(&mut subject, &[]).unwrap();
    assert!(prepared.events.is_empty());
    assert!(prepared.context.source.input_raw_event_ids.is_empty());
    let estimate = crate::context_projection::estimate_payload_tokens(&prepared.payload, 0);
    assert_eq!(prepared.context.estimated_input_tokens, estimate);
    let body = wire(prepared.payload).await;
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"], "completed answer");
    assert_eq!(body["messages"][2]["role"], "user");
    assert!(body["messages"][2]["content"].as_str().unwrap().contains("不是新外部消息"));
    drop(subject);
    drop(store);
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let mut subject = state(reopened.clone(), agent).unwrap();
    let resumed = crate::context_projection::prepare_context(&mut subject, &[]).unwrap();
    assert_eq!(wire(resumed.payload).await, body);
    let fresh = crate::context_projection::prepare_context(&mut subject, &[message("new question")]).unwrap();
    let fresh_body = wire(fresh.payload).await;
    assert_eq!(fresh_body["messages"].as_array().unwrap().len(), 3);
    assert!(fresh_body["messages"][2]["content"].as_str().unwrap().contains("new question"));
    reopened.append_subject_raw_fact(&subject.head, EntityId::new(), "tool_result", &serde_json::json!({
        "v":1,"text":"observed tool output","call_id":EntityId::new().to_string()
    })).unwrap();
    let tool = crate::context_projection::prepare_context(&mut subject, &[]).unwrap();
    let tool_body = wire(tool.payload).await;
    let last = tool_body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(last["content"].as_str().unwrap().contains("runtime tool_result"));
    assert!(last["content"].as_str().unwrap().contains("observed tool output"));
}

fn sealed_preparing_step(subject: &mut SubjectLoopState) -> SubjectStep {
    let prepared = crate::context_projection::prepare_context(subject, &[]).unwrap();
    let mut step = subject.step.clone();
    let input = super::FrozenStepInput {
        schema_version: 10, step_id: step.step_id, step_no: step.step_no,
        events: prepared.events, provider_binding: subject.broker.as_ref().map(|broker| broker.binding().clone()),
        payload: prepared.payload, context: prepared.context,
        semantic_wait: prepared.semantic_wait,
        identity: subject.identity.clone(),
        identity_provider_revision: subject.identity_service.revision().into(),
    };
    step.input_plan_ref = Some(super::seal_step_object(&subject.store, step.step_no,
        &serde_json::to_vec(&input).unwrap(), "application/json", "input_plans",
        "subject_step_input", 10).unwrap());
    step.state = SubjectStepState::Preparing;
    step.context_revision = Some(step.step_no);
    step.record_revision = RecordRevision(1);
    step
}

#[tokio::test]
async fn staged_input_recovers_before_s0_without_duplication() {
    let (_dir, store, agent) = fixture();
    let subject = state(store.clone(), agent).unwrap();
    let original = crate::inbox::InboxEvent {
        source: "napcat".into(), ..message("recover this exact input")
    };
    store.stage_subject_inputs(&subject.head, &subject.step, &[
        shenko_store::SubjectHistoryInput {
            event_id: original.event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1, "text":"[web_ui] recover this exact input", "inbox_event":&original}),
        },
    ]).unwrap();
    drop(subject);
    drop(store);
    let store = Arc::new(Store::open(_dir.join("store.db"), _dir.join("cas")).unwrap());
    let mut reopened = state(store.clone(), agent).unwrap();
    let replayed = crate::inbox::InboxEvent {
        created_at: chrono::Utc::now() + chrono::Duration::seconds(5),
        ..original.clone()
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    execute_step(1, &[replayed], &mut reopened, &cancel).await.unwrap();
    let sealed = super::frozen_input(&store, &reopened.step).unwrap();
    assert_eq!(sealed.events, vec![original]);
    assert_eq!(sealed.context.source.input_raw_event_ids.len(), 1);
    assert_eq!(sealed.payload.messages.len(), 1, "staged input is neither lost nor duplicated");
    let shenko_broker::request::InferencePart::Text { text } = &sealed.payload.messages[0].parts[0] else {
        panic!("native text projection missing")
    };
    assert!(text.contains("recover this exact input"));
}

#[test]
fn native_result_replay_keeps_first_delivery_metadata() {
    let (_dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    let original = crate::inbox::InboxEvent {
        event_id: EntityId::new(), kind: crate::inbox::InboxEventKind::TaskResult,
        payload: serde_json::json!({"result":"volatile-delivery-fixture"}),
        created_at: chrono::Utc::now(), source: "native-child".into(),
    };
    store.stage_subject_inputs(&subject.head, &subject.step, &[
        shenko_store::SubjectHistoryInput {
            event_id: original.event_id, event_type: "tool_result".into(),
            data: serde_json::json!({"v":1,"text":"delivery fixture, not an execution receipt", "inbox_event":&original}),
        },
    ]).unwrap();
    let replayed = crate::inbox::InboxEvent {
        created_at: chrono::Utc::now() + chrono::Duration::seconds(5), ..original.clone()
    };
    let prepared = crate::context_projection::prepare_context(&mut subject, &[replayed]).unwrap();
    assert_eq!(prepared.events, vec![original]);
    assert_eq!(prepared.context.source.input_raw_event_ids.len(), 1);
    let events = store.session_events_range(EntityId(subject.head.branch_id.0), 0, i64::MAX).unwrap();
    assert_eq!(events.len(), 1);
}

#[test]
fn installed_awareness_reaches_the_sealed_prompt_as_advisory_text() {
    fn install(dir: &std::path::Path) -> Arc<shenko_awareness_core::AwarenessRegistry> {
        let packages = dir.join("awareness");
        std::fs::create_dir_all(packages.join("check-commitments")).unwrap();
        std::fs::write(
            packages.join("check-commitments/AWARENESS.md"),
            "---\nid: test.check-commitments\nversion: 1\n\
             capability_card: 约定 承诺 时间冲突\nprovenance: [SPEC §14]\n---\n\
             承诺一旦给出就落在时间里；不记得自己许过什么，很难说现在还剩下多少余地。\n",
        )
        .unwrap();
        let registry = Arc::new(shenko_awareness_core::AwarenessRegistry::new());
        registry.install_from_dir(&packages).unwrap();
        registry
    }

    let (_dir, store, agent) = fixture();
    let registry = install(&_dir);
    let mut subject = state(store.clone(), agent)
        .unwrap()
        .with_awareness(Arc::clone(&registry));
    // The message mentions a word from the unit's own card, as ordinary
    // conversation would; retrieval must work from real input, not a keyword
    // command the user has to know.
    let event = crate::inbox::InboxEvent {
        source: "napcat".into(),
        ..message("上次的约定还算数吗？周末一起看展览")
    };
    let prepared =
        crate::context_projection::prepare_context(&mut subject, &[event]).unwrap();
    let prompt = &prepared.payload.system_prompt;
    assert!(
        prompt.starts_with(&crate::canonical_system_prompt()),
        "the stable contract stays the prefix so KV/model binding remains reusable"
    );
    assert!(prompt.contains("不记得自己许过什么"), "awareness body missing: {prompt}");
    assert!(prompt.contains("不是命令"), "advisory framing missing: {prompt}");
    assert!(
        prompt.find("不记得自己许过什么") > prompt.find("你是一个持续运行的个人 agent"),
        "per-step material must follow the stable prefix"
    );

    // Disabled units are not injected at all; no code branch executes either way.
    registry.disable("test.check-commitments").unwrap();
    let (_dir, store, agent) = fixture();
    let mut subject = state(store, agent)
        .unwrap()
        .with_awareness(Arc::clone(&registry));
    let event = crate::inbox::InboxEvent {
        source: "napcat".into(),
        ..message("上次的约定还算数吗？周末一起看展览")
    };
    let prepared =
        crate::context_projection::prepare_context(&mut subject, &[event]).unwrap();
    assert!(!prepared.payload.system_prompt.contains("不记得自己许过什么"));
}

#[test]
fn personality_stays_out_of_the_tool_contract() {
    use shenko_types::identity::{IdentityChange, IdentitySource};
    let (_dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    let genesis = shenko_identity::DefaultIdentityPolicy::genesis(agent, Some("岚"));
    subject.identity = Some(
        shenko_identity::DefaultIdentityPolicy::successor(
            &genesis,
            &IdentityChange::Personality("话少、直接，不把猜测说成事实".into()),
            IdentitySource::Operator,
        )
        .unwrap(),
    );
    subject = subject.with_native_capabilities(vec![
        "history.query/v1 只读检索已提交的原始历史".into(),
    ]);
    let event = crate::inbox::InboxEvent {
        source: "napcat".into(),
        ..message("你在吗")
    };
    let prepared =
        crate::context_projection::prepare_context(&mut subject, &[event]).unwrap();
    let prompt = &prepared.payload.system_prompt;
    // The persona is prompt text only: it never adds a tool, never changes the
    // sealed protocol, and it is placed after the stable contract plus the
    // installed native definitions (SPEC §6 ordering, R053/R054).
    assert!(prepared.payload.tools.is_empty());
    let contract = crate::canonical_system_prompt();
    assert!(prompt.starts_with(&contract));
    assert!(!prompt[..contract.len()].contains("话少"));
    assert!(prompt.contains("history.query/v1 只读检索已提交的原始历史"));
    assert!(prompt.contains("话少、直接，不把猜测说成事实"));
    assert!(
        prompt.find("话少、直接，不把猜测说成事实")
            > prompt.find("history.query/v1 只读检索已提交的原始历史"),
        "persona must follow the tool contract: {prompt}"
    );
}

#[tokio::test]
async fn late_history_change_rejects_s0_seal_without_advancing_step() {
    let (_dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    let step = sealed_preparing_step(&mut subject);
    let late_id = EntityId::new();
    store.append_subject_raw_fact(&subject.head, late_id, "user",
        &serde_json::json!({"v":1,"text":"arrived between projection and seal"})).unwrap();
    let error = store.upsert_step(&step, &subject.head, RecordRevision(0)).unwrap_err();
    assert!(matches!(error, ShenkoError::ResnapshotRequired { .. }));
    let (_, current) = store.load_subject(agent).unwrap();
    assert_eq!(current.state, SubjectStepState::Planned);
    assert_eq!(current.input_plan_ref, None);
    assert_eq!(store.session_event(late_id).unwrap().unwrap().event_id, late_id);

    // An execution-only commit can change a call's outcome without changing
    // the raw leaf. It must invalidate the projected request just the same.
    let step = sealed_preparing_step(&mut subject);
    let scope = EntityId(subject.head.branch_id.0);
    let leaf = store.session_leaf_seq(scope).unwrap();
    store.commit(shenko_types::store::WriteBatch::new()).unwrap();
    let error = store.upsert_step(&step, &subject.head, RecordRevision(0)).unwrap_err();
    assert!(matches!(error, ShenkoError::ResnapshotRequired { .. }));
    assert_eq!(store.session_leaf_seq(scope).unwrap(), leaf);
    let (_, current) = store.load_subject(agent).unwrap();
    assert_eq!(current.state, SubjectStepState::Planned);

    // A fresh S0 can now seal the same step with the late fact intact and no
    // model admission. Reprojection is not replay of an already sent request.
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    execute_step(1, &[], &mut subject, &cancel).await.unwrap();
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    assert_eq!(subject.step.active_request_id, None);
    let input = super::frozen_input(&store, &subject.step).unwrap();
    assert!(serde_json::to_string(&input.payload).unwrap()
        .contains("arrived between projection and seal"));
    assert_eq!(store.session_event(late_id).unwrap().unwrap().event_id, late_id);
}

#[test]
fn budget_projection_commits_window_and_keeps_unknown_call_verbatim() {
    let (_dir, store, agent) = fixture();
    let (offline, handle) = shenko_broker::broker::Broker::offline();
    let binding = handle.binding().clone();
    let capabilities = shenko_broker::request::ProviderCapabilities {
        // The wait controls add to the mandatory system prefix. Keep enough
        // room for that prefix and the preserved unknown call while still
        // exercising compaction under the same default threshold.
        binding: binding.clone(), max_context_tokens: Some(1500), vision: None,
    };
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(capabilities.clone()));
    subject.max_tokens = 10;
    subject.previous_input_binding = Some(binding);
    subject.previous_input_tokens = Some(700);
    subject.previous_estimated_input_tokens = Some(1);
    subject.context_registry.install(Arc::new(shenko_compact_budget::BudgetCompact::new(
        // The larger context still needs a bounded tail that actually drops
        // old history; the behavior under test is preserving unresolved calls.
        shenko_compact_budget::BudgetConfig { trigger_ratio: 0.85, recent_ratio: 0.1 },
    )));
    let call_id = EntityId::new();
    store.append_subject_raw_fact(&subject.head, EntityId::new(), "tool_call", &serde_json::json!({
        "v":1,"text":"unresolved fixture call", "call_id":call_id,
        "intent_id":EntityId::new(), "tool":"fixture", "args_ref":"unbound-fixture",
    })).unwrap();
    let mut raw_ids = Vec::new();
    for index in 0..18 {
        let fact = store.append_subject_raw_fact(&subject.head, EntityId::new(),
            if index % 2 == 0 { "user" } else { "assistant" },
            &serde_json::json!({"v":1,"text":format!("history-{index}-{}", "x".repeat(80))})).unwrap();
        raw_ids.push(fact.event_id);
    }
    let prepared = crate::context_projection::prepare_context(&mut subject, &[]).unwrap();
    assert!(prepared.context.trigger_budget_input_tokens.unwrap() > 700);
    assert_eq!(prepared.context.observed_input_tokens, Some(700));
    assert_eq!(prepared.context.reserved_output_tokens, 10);
    assert!(matches!(&prepared.context.selection,
        crate::context_projection::ContextSelection::Compacted { .. }));
    let projected = prepared.payload.messages.iter().flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            shenko_broker::request::InferencePart::Text { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>();
    assert!(projected.iter().any(|text| text.contains("context derivative/pointer")));
    assert!(projected.iter().any(|text| text.contains(&call_id.to_string()) && text.contains("Unknown")));
    assert!(projected.iter().any(|text| text.contains("history-17-")));
    let source = &prepared.context.source;
    assert_eq!(source.leaf_seq, Some(19));
    let head = store.context_window_head(source.scope_id).unwrap().unwrap();
    assert_eq!((head.window_id, head.generation), (source.window_id, 2));
    let persisted = store.session_events_range(source.scope_id, 0, i64::MAX).unwrap();
    assert!(raw_ids.iter().all(|id| persisted.iter().any(|event| event.event_id == *id)));
    assert_eq!(persisted[0].event_type, "tool_call");
    let profile = prepared.context.profile_digest.clone();
    let window_id = source.window_id;
    drop((subject, offline));
    let (offline, handle) = shenko_broker::broker::Broker::offline();
    let mut reopened = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(capabilities));
    reopened.max_tokens = 10;
    let resumed = crate::context_projection::prepare_context(&mut reopened, &[]).unwrap();
    assert_eq!(resumed.context.profile_digest, profile);
    assert_eq!(resumed.context.source.window_id, window_id);
    assert_eq!(resumed.context.source.model_generation, 1);
    assert_eq!(resumed.context.source.window_generation, 2);
    drop((reopened, offline));
}

#[test]
fn retained_calibrated_budget_over_limit_blocks_dispatch() {
    let (_dir, store, agent) = fixture();
    let (offline, handle) = shenko_broker::broker::Broker::offline();
    let binding = handle.binding().clone();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(shenko_broker::request::ProviderCapabilities {
            binding: binding.clone(), max_context_tokens: Some(1000), vision: None,
        }));
    subject.max_tokens = 10;
    subject.previous_input_binding = Some(binding);
    subject.previous_input_tokens = Some(1000);
    subject.previous_estimated_input_tokens = Some(1);
    let error = crate::context_projection::prepare_context(&mut subject, &[])
        .err().expect("calibrated over-budget retention must be rejected");
    assert!(error.to_string().contains("projected input budget"));
    assert_eq!(subject.step.state, SubjectStepState::Planned);
    assert_eq!(subject.step.active_request_id, None);
    drop(offline);
}

#[tokio::test]
async fn budget_fresh_batch_survives_uncommitted_reopen_and_successful_s1() {
    let (dir, store, agent) = fixture();
    let (offline, handle) = shenko_broker::broker::Broker::offline();
    let capabilities = shenko_broker::request::ProviderCapabilities {
        binding: handle.binding().clone(), max_context_tokens: Some(4000), vision: None,
    };
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(capabilities));
    subject.max_tokens = 10;
    subject.context_registry.install(Arc::new(shenko_compact_budget::BudgetCompact::new(
        shenko_compact_budget::BudgetConfig { trigger_ratio: 0.1, recent_ratio: 0.01 },
    )));
    for index in 0..20 {
        store.append_subject_raw_fact(&subject.head, EntityId::new(), "user",
            &serde_json::json!({"v":1,"text":format!("old-{index}-{}", "x".repeat(400))})).unwrap();
    }
    let events: Vec<_> = (0..12).map(|index| crate::inbox::InboxEvent {
        event_id: EntityId::new(), kind: crate::inbox::InboxEventKind::Message,
        payload: serde_json::json!({"content":format!("fresh-{index}-{}", "z".repeat(80))}),
        source: "batch-fixture".into(), created_at: chrono::Utc::now(),
    }).collect();
    // Interrupt after S0. No input may be disposed just because the window
    // has committed a compact derivative.
    let close = tokio_util::sync::CancellationToken::new();
    close.cancel();
    execute_step(1, &events, &mut subject, &close).await.unwrap();
    let frozen = super::frozen_input(&store, &subject.step).unwrap();
    assert!(matches!(frozen.context.selection, crate::context_projection::ContextSelection::Compacted { .. }));
    let text = frozen.payload.messages.iter().flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            shenko_broker::request::InferencePart::Text { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("\n");
    assert!(text.contains("context derivative/pointer"));
    let mut previous = 0;
    for event in &events {
        let body = event.payload["content"].as_str().unwrap();
        let position = text.find(body).expect("every acknowledged input needs its first body presentation");
        assert!(position >= previous);
        previous = position;
        assert!(store.event_disposition_commit(agent, event.event_id).unwrap().is_none());
    }
    let raw_ids = frozen.context.source.input_raw_event_ids.clone();
    assert_eq!(raw_ids.len(), events.len());
    let sealed_payload = serde_json::to_vec(&frozen.payload).unwrap();
    drop((subject, offline, store));
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let mut subject = state(store.clone(), agent).unwrap();
    let resumed = super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(resumed.events, events);
    assert_eq!(resumed.context.source.input_raw_event_ids, raw_ids);
    assert_eq!(serde_json::to_vec(&resumed.payload).unwrap(), sealed_payload);
    // Feed the same durable result boundary used by recovery: this fixture
    // exercises real S1, not a live provider or network claim.
    let mut step = subject.step.clone();
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    let revision = step.record_revision;
    step.record_revision = RecordRevision(revision.0 + 1);
    store.upsert_step(&step, &subject.head, revision).unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(store.cas.seal_object(b"batch understood", "text/plain", "model_results", "text", 1).unwrap().object);
    step.model_outcome_ref = Some(store.cas.seal_object(&serde_json::to_vec(&SubjectModelOutcome {
        schema_version: 2, request_id: step.active_request_id.unwrap(),
        result_ref: step.model_result_ref.clone().unwrap(), stop_reason: "end_turn".into(),
        raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(), usage: None,
    }).unwrap(), "application/json", "model_outcomes", "SubjectModelOutcome", 2).unwrap().object);
    let revision = step.record_revision;
    step.record_revision = RecordRevision(revision.0 + 1);
    store.upsert_step(&step, &subject.head, revision).unwrap();
    subject.step = step;
    execute_step(1, &[], &mut subject, &tokio_util::sync::CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(subject.head.committed_step_no, 1);
    for event in &events {
        assert!(store.event_disposition_commit(agent, event.event_id).unwrap().is_some());
    }
    let raw = store.session_events_range(EntityId(subject.head.branch_id.0), 0, i64::MAX).unwrap();
    let retained: Vec<_> = raw.iter().filter(|record| raw_ids.contains(&record.event_id)).collect();
    assert_eq!(retained.iter().map(|record| record.event_id).collect::<Vec<_>>(), raw_ids);
    for (record, event) in retained.iter().zip(&events) {
        let staged: serde_json::Value = serde_json::from_str(&record.data).unwrap();
        assert_eq!(staged["inbox_event"]["payload"], event.payload);
    }
}

#[test]
fn execution_commit_during_projection_is_rejected() {
    struct MutatingHistory {
        inner: shenko_context_policy::StoreHistory,
        store: Arc<Store>,
        fired: std::sync::atomic::AtomicBool,
    }
    impl shenko_context_policy::HistorySource for MutatingHistory {
        fn read_range(&self, from: u64, to: u64) -> Result<Vec<shenko_context_policy::HistoryItem>, shenko_context_policy::StorageError> {
            if !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.store.commit(shenko_types::store::WriteBatch::new()).unwrap();
            }
            self.inner.read_range(from, to)
        }
        fn read_event(&self, id: EntityId) -> Result<Option<shenko_context_policy::HistoryItem>, shenko_context_policy::StorageError> {
            self.inner.read_event(id)
        }
        fn leaf_seq(&self) -> Result<Option<u64>, shenko_context_policy::StorageError> {
            self.inner.leaf_seq()
        }
    }
    let (_dir, store, agent) = fixture();
    let (head, _) = store.load_subject(agent).unwrap();
    let scope = EntityId(head.branch_id.0);
    store.append_subject_raw_fact(&head, EntityId::new(), "user", &serde_json::json!({"v":1,"text":"source"})).unwrap();
    let before = store.current_commit_seq().unwrap();
    let history = MutatingHistory {
        inner: shenko_context_policy::StoreHistory::new(store.clone(), scope, Arc::new(shenko_context_policy::estimate_tokens)),
        store: store.clone(), fired: std::sync::atomic::AtomicBool::new(false),
    };
    let window = shenko_context_policy::CommittedWindow::genesis();
    let error = crate::context_projection::project_payload_stable(
        &store, &window, &history, "fixture".into(), None, 10, &[],
    ).err().expect("a mutation during execution reads must invalidate the projection");
    assert!(error.to_string().contains("during projection"));
    assert_eq!(store.current_commit_seq().unwrap(), before + 1);
}

#[test]
fn oversized_fresh_batch_stays_pending_across_reopen() {
    let (dir, store, agent) = fixture();
    let (offline, handle) = shenko_broker::broker::Broker::offline();
    let capabilities = shenko_broker::request::ProviderCapabilities {
        binding: handle.binding().clone(), max_context_tokens: Some(1000), vision: None,
    };
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(capabilities.clone()));
    subject.max_tokens = 10;
    subject.context_registry.install(Arc::new(shenko_compact_budget::BudgetCompact::new(
        shenko_compact_budget::BudgetConfig { trigger_ratio: 0.1, recent_ratio: 0.01 },
    )));
    let events: Vec<_> = (0..12).map(|index| crate::inbox::InboxEvent {
        event_id: EntityId::new(), kind: crate::inbox::InboxEventKind::Message,
        payload: serde_json::json!({"content":format!("oversized-{index}-{}", "q".repeat(800))}),
        source: "batch-fixture".into(), created_at: chrono::Utc::now(),
    }).collect();
    let error = crate::context_projection::prepare_context(&mut subject, &events).err().unwrap();
    assert!(error.to_string().contains("projected input budget"));
    let pending = store.stage_subject_inputs(&subject.head, &subject.step, &[]).unwrap();
    let original: Vec<_> = pending.iter().map(|record| (record.event_id, record.data.clone())).collect();
    assert_eq!(original.len(), events.len());
    drop((subject, offline, store));
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (offline, handle) = shenko_broker::broker::Broker::offline();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(capabilities));
    subject.max_tokens = 10;
    let pending = store.stage_subject_inputs(&subject.head, &subject.step, &[]).unwrap();
    assert_eq!(pending.iter().map(|record| (record.event_id, record.data.clone())).collect::<Vec<_>>(), original);
    assert!(crate::context_projection::prepare_context(&mut subject, &[]).is_err());
    assert_eq!(subject.step.state, SubjectStepState::Planned);
    for event in &events {
        assert!(store.event_disposition_commit(agent, event.event_id).unwrap().is_none());
    }
    drop(offline);
}

#[tokio::test]
async fn cas_seal_failure_returns_error_without_head_advance_or_publication() {
    let (dir, store, agent) = fixture();
    let (head, planned) = store.load_subject(agent).unwrap();
    let mut subject = state(store.clone(), agent).unwrap();
    assert_eq!(subject.step.step_id, planned.step_id);
    assert_eq!(subject.head.branch_id, head.branch_id);
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    subject = subject.with_outbox(tx);
    // 用普通文件占据 CAS 根，确定性触发 mkdir 错误，不依赖进程权限。
    std::fs::remove_dir_all(dir.join("cas")).unwrap();
    std::fs::write(dir.join("cas"), b"not a directory").unwrap();
    let result = execute_step(planned.step_no, &[], &mut subject, &tokio_util::sync::CancellationToken::new())
    .await;
    assert!(result.is_err());
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let (after, still_planned) = store.load_subject(agent).unwrap();
    assert_eq!(after.committed_at, head.committed_at);
    assert_eq!(after.next_step_id, planned.step_id);
    assert_eq!(after.record_revision, head.record_revision);
    assert_eq!(still_planned.state, SubjectStepState::Planned);
    assert!(still_planned.input_plan_ref.is_none());
    assert_eq!(still_planned.record_revision, RecordRevision(0));
    drop(subject);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

/// SPEC §6 R053/R054/R117：人格只在 S0 边界生效，合同前缀不随身份变化，改名不换 agent。
#[tokio::test]
async fn identity_revision_applies_at_step_boundary_behind_fixed_contract() {
    use shenko_types::identity::{IdentityChange, IdentitySource, NamingOrigin};
    let (dir, store, agent) = fixture();
    store
        .initialize_identity(&shenko_identity::DefaultIdentityPolicy::genesis(
            agent, None,
        ))
        .unwrap();
    let mut subject = state(store.clone(), agent).unwrap();
    let before = super::build_system_prompt(&subject, &[]);
    assert!(before.starts_with(super::SUBJECT_CONTRACT));
    assert!(!before.contains("你是 Shenko"));
    assert!(before.contains("你还没有名字"));

    let source = IdentitySource::Subject {
        step_id: subject.step.step_id,
        step_no: 1,
    };
    let current = subject.identity.as_ref().unwrap();
    let renamed = shenko_identity::DefaultIdentityPolicy::successor(
        current,
        &IdentityChange::Name("岚".into()),
        source,
    )
    .unwrap();
    store.append_identity(&renamed, current.revision).unwrap();
    assert_eq!(
        (renamed.agent_id, renamed.revision, renamed.naming),
        (agent, 2, NamingOrigin::SelfChosen)
    );
    // 本步尚未到边界：仍是 revision 1。
    assert_eq!(subject.identity.as_ref().unwrap().revision, 1);

    // 无 broker 时 execute_step 在 S0 之后报错，但 S0 已装配新身份并封存输入计划。
    let planned = subject.step.step_no;
    assert!(
        execute_step(
            planned,
            &[],
            &mut subject,
            &tokio_util::sync::CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(subject.identity.as_ref().unwrap().revision, 2);
    assert!(subject.step.input_plan_ref.is_some());
    let after = super::build_system_prompt(&subject, &[]);
    assert!(after.starts_with(super::SUBJECT_CONTRACT));
    assert!(after.contains("「岚」") && after.contains("自己取的"));
    assert_eq!(store.identity_history(agent).unwrap().len(), 2);
    drop(subject);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

/// One interrupted input's recovery journey: close before dispatch, rejected
/// admission, then persisted result recovery and stale ingress replay.
#[tokio::test]
async fn frozen_input_survives_shutdown_rejection_and_recorded_result_reopen() {
    let (dir, store, agent) = fixture();
    let event = crate::inbox::InboxEvent {
        event_id: EntityId::new(),
        kind: crate::inbox::InboxEventKind::Message,
        payload: serde_json::json!({"content":"original frozen input"}),
        source: "fixture".into(),
        created_at: chrono::Utc::now(),
    };
    let (offline, handle) = shenko_broker::broker::Broker::offline();
    drop(offline);
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let close = tokio_util::sync::CancellationToken::new();
    close.cancel();
    assert!(
        execute_step(1, &[event.clone()], &mut subject, &close)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    drop(subject);
    drop(store);

    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (closed_broker, handle) = shenko_broker::broker::Broker::offline();
    drop(closed_broker); // positively rejected admission; no provider ever dispatched
    let subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let retained_inbox = subject.inbox.clone();
    assert!(
        super::run_subject_loop(subject, tokio_util::sync::CancellationToken::new())
            .await
            .is_err()
    );
    assert!(retained_inbox.sender().is_closed());
    let (head, mut step) = store.load_subject(agent).unwrap();
    assert_eq!(step.state, SubjectStepState::RetryPending);
    let request_id = step.active_request_id;
    drop(retained_inbox);
    drop(store);

    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    // Reopen validates the typed certificate; simulate the durable result boundary
    // of that same request, not a fabricated S1 or a second model invocation.
    let subject = state(store.clone(), agent).unwrap();
    assert_eq!(
        super::frozen_input(&store, &subject.step).unwrap().events[0].payload,
        event.payload
    );
    drop(subject);
    let expected = step.record_revision;
    step.state = SubjectStepState::AwaitingModel;
    step.record_revision = RecordRevision(expected.0 + 1);
    store.upsert_step(&step, &head, expected).unwrap();
    let expected = step.record_revision;
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(
        store
            .cas
            .seal_object(b"[delegate]", "text/plain", "model_results", "text", 1)
            .unwrap()
            .object,
    );
    step.record_revision = RecordRevision(expected.0 + 1);
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: step.model_result_ref.clone().unwrap(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );
    store.upsert_step(&step, &head, expected).unwrap();
    drop(store);

    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let mut subject = state(store.clone(), agent).unwrap(); // no broker: resending would fail
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    subject = subject.with_outbox(tx);
    assert_eq!(subject.step.active_request_id, request_id);
    assert!(
        execute_step(
            1,
            &[],
            &mut subject,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .is_none()
    );
    // A complete malformed control is durably rejected, not a permanent
    // ResultRecorded crashloop or a fabricated successful S1/ACK.
    assert_eq!(subject.head.committed_step_no, 0);
    assert_eq!(subject.head.committed_at, head.committed_at);
    assert_ne!(subject.step.step_id, step.step_id);
    assert!(subject.step.context_revision > step.context_revision);
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    assert!(rx.try_recv().is_err());
    assert!(
        store
            .event_disposition_commit(agent, event.event_id)
            .unwrap()
            .is_none()
    );
    let retired = store
        .get_step(&head.branch_id, step.step_id)
        .unwrap()
        .unwrap();
    assert_eq!(retired.state, SubjectStepState::Retired);
    assert_eq!(retired.model_result_ref, step.model_result_ref);
    assert_eq!(retired.input_plan_ref, step.input_plan_ref);
    let rejection: ControlOutputRejection = serde_json::from_slice(
        &super::read_sealed(&store, retired.retry_reason_ref.as_ref().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(rejection.request_id, request_id.unwrap());
    assert_eq!(rejection.error.code, "CONTROL_PROTOCOL_INVALID");
    assert_eq!(rejection.replacement_step_id, subject.step.step_id);
    let correction = super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(correction.events[0].event_id, event.event_id);
    let correction_text = |message: &shenko_broker::request::InferenceMessage| -> String {
        message
            .parts
            .iter()
            .filter_map(|part| match part {
                shenko_broker::request::InferencePart::Text { text } => Some(text.as_str()),
                shenko_broker::request::InferencePart::Image { .. } => None,
            })
            .collect::<String>()
    };
    assert_eq!(
        correction_text(&correction.payload.messages[1]),
        "[delegate]"
    );
    assert!(correction_text(&correction.payload.messages[2]).contains("CONTROL_PROTOCOL_INVALID"));
    drop(subject);
    drop(store);
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let subject = state(store.clone(), agent).unwrap();
    assert_eq!(subject.step.step_id, rejection.replacement_step_id);
    // Close/reopen before dispatch retains the correction. A terminal result
    // of the NEW request can now settle the original input exactly once.
    let mut corrected = subject.step.clone();
    corrected.state = SubjectStepState::AwaitingModel;
    corrected.active_request_id = Some(EntityId::new());
    assert_ne!(corrected.active_request_id, request_id);
    corrected.record_revision = RecordRevision(1);
    store
        .upsert_step(&corrected, &subject.head, RecordRevision(0))
        .unwrap();
    corrected.state = SubjectStepState::ResultRecorded;
    corrected.model_result_ref = Some(
        store
            .cas
            .seal_object(
                b"persisted complete result",
                "text/plain",
                "model_results",
                "text",
                1,
            )
            .unwrap()
            .object,
    );
    corrected.record_revision = RecordRevision(2);
    corrected.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                    schema_version: 2,
                    request_id: corrected.active_request_id.unwrap(),
                    result_ref: corrected.model_result_ref.clone().unwrap(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );

    store
        .upsert_step(&corrected, &subject.head, RecordRevision(1))
        .unwrap();
    drop(subject);
    let mut subject = state(store.clone(), agent).unwrap();
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    subject = subject.with_outbox(tx);
    execute_step(1, &[], &mut subject, &tokio_util::sync::CancellationToken::new())
    .await
    .unwrap()
    .unwrap();
    assert_eq!(subject.head.committed_step_no, 1);
    assert_eq!(rx.try_recv().unwrap().text, "persisted complete result");
    assert!(
        store
            .event_disposition_commit(agent, event.event_id)
            .unwrap()
            .is_some()
    );
    // A previously queued replay must be filtered after the resumed S1, even
    // though main admitted it before that S1's disposition became visible.
    subject.inbox.sender().send(event).await.unwrap();
    assert!(
        super::run_subject_loop(subject, tokio_util::sync::CancellationToken::new())
            .await
            .is_err()
    );
    let (head, next) = store.load_subject(agent).unwrap();
    assert_eq!(head.committed_step_no, 1);
    assert!(
        super::frozen_input(&store, &next)
            .unwrap()
            .events
            .is_empty()
    );
    // Keep the fixture on container disk for independent inspection.
}

/// One real process-crash/CAS recovery journey: all-or-none S1, death before
/// postcommit work, then an operator editing during the successor's inference.
#[tokio::test]
async fn identity_s1_crash_and_concurrent_editor_keep_durable_decision() {
    use shenko_identity::DefaultIdentityPolicy as Policy;
    use shenko_types::{identity::*, output::*};
    if let Some(dir) = std::env::var_os("SHENKO_C22_CRASH_CHILD") {
        let dir = PathBuf::from(dir);
        let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
        let (step, head, next): (SubjectStep, SubjectBranchHead, EntityId) =
            serde_json::from_slice(&std::fs::read(dir.join("candidate.json")).unwrap()).unwrap();
        store
            .commit_step_and_advance_head(&step, &head, next)
            .unwrap();
        // Actual process death immediately after the SQL commit, with no loop
        // identity write, in-memory adoption, broadcaster or worker ever run.
        #[cfg(unix)]
        {
            std::process::Command::new("kill")
                .args(["-KILL", &std::process::id().to_string()])
                .status()
                .unwrap();
            unreachable!("SIGKILL must end the process");
        }
        #[cfg(not(unix))]
        std::process::exit(73); // abrupt exit without destructors on other platforms
    }
    let (dir, store, agent) = fixture();
    let profile = store
        .initialize_identity(&Policy::genesis(agent, None))
        .unwrap();
    let event = crate::inbox::InboxEvent {
        event_id: EntityId::new(),
        kind: crate::inbox::InboxEventKind::Message,
        payload: serde_json::json!({"content":"choose your name"}),
        source: "fixture".into(),
        created_at: chrono::Utc::now(),
    };
    let mut subject = state(store.clone(), agent).unwrap();
    let close = tokio_util::sync::CancellationToken::new();
    close.cancel();
    execute_step(1, &[event.clone()], &mut subject, &close)
        .await
        .unwrap();
    let head = subject.head.clone();
    let mut step = subject.step.clone();
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    step.record_revision = RecordRevision(2);
    store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
    let raw = "[identity:name] 岚\n[identity:soul] 记住关系，不靠 delegate 或委派替代理解";
    let raw_ref = store
        .cas
        .seal_object(raw.as_bytes(), "text/plain", "model_results", "text", 1)
        .unwrap()
        .object;
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(raw_ref.clone());
    step.record_revision = RecordRevision(3);
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: step.model_result_ref.clone().unwrap(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );
    store.upsert_step(&step, &head, RecordRevision(2)).unwrap();
    let output = crate::output::parse_output(step.step_id, raw_ref.clone(), raw, &subject.identity_service, None)
    .unwrap();
    assert_eq!(output.controls.len(), 2);
    let changes: Vec<_> = output
        .controls
        .iter()
        .map(|control| {
            let OutputAction::Identity { change } = &control.action else {
                panic!("no synthetic memory/task")
            };
            change.clone()
        })
        .collect();
    let next = EntityId::new();
    let mut decision = StepDecision {
        schema_version: 6,
        step_id: step.step_id,
        output,
        identity_intent: Some(IdentityIntent {
            intent_id: EntityId::new(),
            action_ids: vec![],
            expected_revision: 1,
            provider_revision: subject.identity_service.revision().into(),
            proposed: Policy::revise(
                &profile,
                &changes,
                IdentitySource::Subject {
                    step_id: step.step_id,
                    step_no: 1,
                },
            )
            .unwrap(),
        }),
        input_plan_ref: step.input_plan_ref.clone().unwrap(),
        model_result_ref: Some(raw_ref.clone()),
        applied_steering_refs: vec![],
        event_dispositions: vec![EventDisposition {
            event_id: event.event_id,
            disposition: Disposition::Acknowledged,
        }],
        delegated_task_intents: vec![],
        child_dialogue_intents: vec![],
        memory_intents: vec![],
        policy_intents: vec![],
        publication_intents: vec![],
        pending_obligations: vec![],
        evidence_refs: vec![],
        successor_step_id: step.step_id,
    };
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(4);
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "decisions",
                "StepDecision",
                6,
            )
            .unwrap()
            .object,
    );
    // A successor collision happens after identity insertion inside S1. It must
    // roll back the identity, receipt and event disposition as well as the head.
    assert!(
        store
            .commit_step_and_advance_head(&step, &head, step.step_id)
            .is_err()
    );
    assert_eq!(store.latest_identity(agent).unwrap().unwrap().revision, 1);
    assert!(store.identity_receipt(step.step_id).unwrap().is_none());
    assert!(
        store
            .event_disposition_commit(agent, event.event_id)
            .unwrap()
            .is_none()
    );
    decision.successor_step_id = next;
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "decisions",
                "StepDecision",
                6,
            )
            .unwrap()
            .object,
    );
    std::fs::write(
        dir.join("candidate.json"),
        serde_json::to_vec(&(&step, &head, next)).unwrap(),
    )
    .unwrap();
    drop(subject);
    drop(store);
    let status=std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact","loop_runner::recovery_tests::identity_s1_crash_and_concurrent_editor_keep_durable_decision","--nocapture"])
        .env("SHENKO_C22_CRASH_CHILD",&dir).status().unwrap();
    assert!(!status.success());
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let receipt = store.identity_receipt(step.step_id).unwrap().unwrap();
    assert!(matches!(
        receipt.outcome,
        IdentityOutcome::Applied {
            expected_revision: 1,
            revision: 2
        }
    ));
    assert_eq!(
        store.load_subject(agent).unwrap().0.committed_at,
        receipt.committed_at
    );
    assert_eq!(
        store
            .latest_identity(agent)
            .unwrap()
            .unwrap()
            .display_name
            .as_deref(),
        Some("岚")
    );
    assert_eq!(
        store.cas.read_object(&raw_ref.digest).unwrap(),
        raw.as_bytes()
    );
    let mut subject = state(store.clone(), agent).unwrap();
    execute_step(2, &[], &mut subject, &close)
        .await
        .unwrap();
    let sealed = super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(sealed.identity.unwrap().revision, 2);
    let current = store.latest_identity(agent).unwrap().unwrap();
    let operator = Policy::revise(
        &current,
        &[IdentityChange::Name("operator choice".into())],
        IdentitySource::Operator,
    )
    .unwrap();
    store.append_identity(&operator, 2).unwrap();
    let head = subject.head.clone();
    let mut step = subject.step.clone();
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    step.record_revision = RecordRevision(2);
    store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.record_revision = RecordRevision(3);
    step.model_result_ref = Some(
        store
            .cas
            .seal_object(
                "[identity:name] stale model choice".as_bytes(),
                "text/plain",
                "model_results",
                "text",
                1,
            )
            .unwrap()
            .object,
    );
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: step.model_result_ref.clone().unwrap(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );
    store.upsert_step(&step, &head, RecordRevision(2)).unwrap();
    drop(subject);
    let mut subject = state(store.clone(), agent).unwrap();
    let (tx, mut rx) = tokio::sync::broadcast::channel(4);
    subject = subject.with_outbox(tx);
    execute_step(2, &[], &mut subject, &tokio_util::sync::CancellationToken::new())
    .await
    .unwrap();
    assert_eq!(rx.try_recv().unwrap().identity_revision, Some(3));
    let conflict = store.identity_receipt(step.step_id).unwrap().unwrap();
    assert!(matches!(
        conflict.outcome,
        IdentityOutcome::Conflict {
            expected_revision: 2,
            actual_revision: Some(3)
        }
    ));
    assert_eq!(
        store
            .latest_identity(agent)
            .unwrap()
            .unwrap()
            .display_name
            .as_deref(),
        Some("operator choice")
    );
    assert_eq!(subject.head.committed_step_no, 2);
    assert!(super::build_system_prompt(&subject, &[]).contains("上一步身份意图未采用"));
    drop(subject);
    // Simulate a pre-raw-history instance without deleting its committed
    // steps/CAS evidence. Admission must request migration, not thin history.
    rusqlite::Connection::open(dir.join("store.db")).unwrap().execute_batch(
        "DELETE FROM subject_raw_inputs; DELETE FROM session_events; DELETE FROM raw_sessions;",
    ).unwrap();
    let error = state(store.clone(), agent).err().expect("legacy raw gap must block");
    assert!(error.to_string().contains("raw history migration required"));
    // Retain this container-disk fixture as crash evidence, not production data.
}

/// Native Cordis replacement reaches the real S0 consumer. The old generation
/// finishes only its sealed request; recovery never reinterprets it with B.
#[tokio::test]
async fn native_identity_replacement_is_bound_to_sealed_request() {
    use cordis_core::{Context, Plugin, PreparedPlugin};
    use shenko_identity::{
        DefaultIdentityPolicy as DefaultPolicy, IdentityPlugin, IdentityPolicy, IdentityService,
    };
    use shenko_types::identity::*;
    struct OtherPolicy;
    impl IdentityPolicy for OtherPolicy {
        fn revision(&self) -> &str {
            "test.identity/other@1"
        }
        fn genesis(&self, agent: EntityId, name: Option<&str>) -> IdentityProfile {
            DefaultPolicy::genesis(agent, name)
        }
        fn persona_section(&self, profile: &IdentityProfile) -> String {
            format!("replacement provider at revision {}", profile.revision)
        }
        fn parse_control(&self, header: &str, value: &str) -> Option<IdentityChange> {
            DefaultPolicy.parse_control(header, value)
        }
        fn revise(
            &self,
            profile: &IdentityProfile,
            changes: &[IdentityChange],
            source: IdentitySource,
        ) -> Option<IdentityProfile> {
            DefaultPolicy::revise(profile, changes, source)
        }
    }
    let (_dir, store, agent) = fixture();
    let ctx = Context::new();
    let (head, _) = store.load_subject(agent).unwrap();
    let registry = Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
        shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(store.clone(), EntityId(head.branch_id.0)),
        )),
    ));
    let context_plugin = shenko_context_policy::ContextPolicyHost;
    let context_input = context_plugin.prepare(registry).unwrap();
    let _context_host = ctx
        .spawn(PreparedPlugin::from_input(context_plugin, context_input)).await.unwrap();
    let first = ctx.spawn(shenko_identity::default_plugin()).await.unwrap();
    let service = ctx.try_service::<IdentityService>().unwrap();
    store
        .initialize_identity(&service.genesis(agent, None))
        .unwrap();
    let mut subject =
        SubjectLoopState::new(agent, Arc::new(Inbox::new(8)), store.clone(), ctx.clone()).unwrap();
    let close = tokio_util::sync::CancellationToken::new();
    close.cancel();
    execute_step(1, &[], &mut subject, &close)
        .await
        .unwrap();
    let frozen = super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(
        frozen.identity_provider_revision,
        "shenko.identity/default@1"
    );
    assert!(frozen.payload.system_prompt.contains("你还没有名字"));
    first.dispose().await.unwrap();
    assert!(ctx.try_service::<IdentityService>().is_err());
    let plugin = IdentityPlugin;
    let prepared = plugin.prepare(Arc::new(OtherPolicy)).unwrap();
    let second = ctx
        .spawn(PreparedPlugin::from_input(plugin, prepared))
        .await
        .unwrap();
    assert_eq!(
        ctx.try_service::<IdentityService>().unwrap().revision(),
        "test.identity/other@1"
    );
    let reopened =
        SubjectLoopState::new(agent, Arc::new(Inbox::new(8)), store.clone(), ctx.clone());
    assert!(
        reopened
            .err()
            .unwrap()
            .to_string()
            .contains("IDENTITY_PROVIDER_REVISION_MISMATCH")
    );
    // The original live request retained its original service until S1, even
    // though Cordis withdrew that service from new consumers.
    let head = subject.head.clone();
    let mut step = subject.step.clone();
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    step.record_revision = RecordRevision(2);
    store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.record_revision = RecordRevision(3);
    step.model_result_ref = Some(
        store
            .cas
            .seal_object(
                "[identity:name] 岚".as_bytes(),
                "text/plain",
                "model_results",
                "text",
                1,
            )
            .unwrap()
            .object,
    );
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: step.model_result_ref.clone().unwrap(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );
    store.upsert_step(&step, &head, RecordRevision(2)).unwrap();
    subject.step = step;
    execute_step(1, &[], &mut subject, &close)
        .await
        .unwrap();
    assert_eq!(store.latest_identity(agent).unwrap().unwrap().revision, 2);
    execute_step(2, &[], &mut subject, &close)
        .await
        .unwrap();
    let next = super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(next.identity_provider_revision, "test.identity/other@1");
    assert!(
        next.payload
            .system_prompt
            .contains("replacement provider at revision 2")
    );
    assert!(!next.payload.system_prompt.contains("你还没有名字"));
    second.dispose().await.unwrap();
    assert!(ctx.try_service::<IdentityService>().is_err());
}

/// Feed one already-recorded model result through the real S1 admission path.
/// This is a deterministic product-mechanics fixture, not live-model proof.
fn record_result(
    store: &Arc<Store>,
    subject: &mut SubjectLoopState,
    raw: &str,
) -> shenko_types::ids::EntityId {
    let head = subject.head.clone();
    let mut step = subject.step.clone();
    let step_id = step.step_id;
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    let revision = step.record_revision;
    step.record_revision = RecordRevision(revision.0 + 1);
    store.upsert_step(&step, &head, revision).unwrap();
    let raw_ref = store
        .cas
        .seal_object(raw.as_bytes(), "text/plain", "model_results", "text", 1)
        .unwrap()
        .object;
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(raw_ref.clone());
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: raw_ref,
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );
    let revision = step.record_revision;
    step.record_revision = RecordRevision(revision.0 + 1);
    store.upsert_step(&step, &head, revision).unwrap();
    subject.step = step;
    step_id
}

/// R143/§41.5: a wait is a durable subject decision, not an in-memory flag.
/// Polling never renews its deadline, an unrelated message does not end it and
/// a late continue cannot resurrect a wait that already ended.
async fn commit_output(
    store: &Arc<Store>,
    subject: &mut SubjectLoopState,
    seq: u64,
    events: &[crate::inbox::InboxEvent],
    raw: &str,
) -> shenko_types::ids::EntityId {
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    execute_step(seq, events, subject, &cancel).await.unwrap();
    let step_id = record_result(store, subject, raw);
    execute_step(
        seq,
        &[],
        subject,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("S1 commits");
    step_id
}

#[tokio::test]
async fn semantic_wait_is_durable_and_never_renewed_by_polling() {
    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    let first = commit_output(
        &store,
        &mut subject,
        1,
        &[],
        "[wait] {\"condition\":\"实验出现足以判断的结果\",\"max_wait_secs\":120}\n继续观察，不作无依据的结论。",
    )
    .await;
    let wait = store
        .semantic_wait(agent)
        .unwrap()
        .expect("the start control creates a durable wait");
    assert!(wait.is_active());
    assert_eq!(wait.condition, "实验出现足以判断的结果");
    assert_eq!(wait.created_step_id, first);
    assert_eq!(
        wait.deadline_at
            .signed_duration_since(wait.started_at)
            .num_seconds(),
        120
    );
    assert!(matches!(
        store.semantic_wait_receipt(first).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Started
    ));
    let original_deadline = wait.deadline_at;
    let original_start = wait.started_at;

    // Recovery must not silently continue this wait with a different model
    // binding. It remains durable rather than pretending the wait ended.
    let (offline, changed_binding) = shenko_broker::broker::Broker::offline();
    let mut changed_subject = state(store.clone(), agent).unwrap().with_broker(changed_binding);
    let error = crate::context_projection::prepare_context(&mut changed_subject, &[])
        .err().expect("a different model binding cannot continue this wait");
    assert!(error.to_string().contains("semantic wait model binding changed"));
    assert!(store.semantic_wait(agent).unwrap().unwrap().is_active());
    drop((changed_subject, offline));

    // The next S0 binds the same wait without asking the model to copy its id.
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    execute_step(2, &[], &mut subject, &cancel).await.unwrap();
    let frozen = super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(frozen.schema_version, 10);
    let sealed_wait = frozen.semantic_wait.expect("S0 freezes the active wait");
    assert_eq!(sealed_wait.wait_id, wait.wait_id);
    assert_eq!(sealed_wait.deadline_at, original_deadline);
    assert!(frozen.payload.system_prompt.contains(&wait.condition));
    assert!(!frozen.payload.system_prompt.contains(&wait.wait_id.to_string()));
    // Discard this sealed input only by completing it: the recorded result is
    // bound to the same S0, exactly as after a process reopen.
    let continued = record_result(
        &store,
        &mut subject,
        "[wait:continue]\n条件尚未满足，继续等待。",
    );
    execute_step(2, &[], &mut subject, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap()
        .expect("continue commits");
    let after_continue = store.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(after_continue.wait_id, wait.wait_id);
    assert_eq!(after_continue.started_at, original_start);
    assert_eq!(after_continue.deadline_at, original_deadline);
    assert!(matches!(
        store.semantic_wait_receipt(continued).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Continued
    ));
    let history = store.session_events_range(EntityId(subject.head.branch_id.0), 0, i64::MAX).unwrap();
    assert!(history.iter().any(|event| event.event_type == "assistant"
        && event.data.contains("[wait:continue]")));
    assert!(!history.iter().any(|event| event.event_type == "tool_call"
        && serde_json::from_str::<serde_json::Value>(&event.data).ok()
            .is_some_and(|data| data["tool"] == "policy")));

    // A new message is context for the model, not an automatic wait exit.
    let event = message("还有别的事要说，但实验结果仍未出来");
    let with_event = commit_output(
        &store,
        &mut subject,
        3,
        &[event.clone()],
        "我记下了新消息，实验仍未完成。",
    )
    .await;
    assert!(store.event_disposition_commit(agent, event.event_id).unwrap().is_some());
    let after_event = store.semantic_wait(agent).unwrap().unwrap();
    assert!(after_event.is_active());
    assert_eq!(after_event.deadline_at, original_deadline);
    assert!(store.semantic_wait_receipt(with_event).unwrap().is_none());

    let ended = commit_output(
        &store,
        &mut subject,
        4,
        &[],
        "[wait:end] 条件已经满足\n回到正常思考。",
    )
    .await;
    let after_end = store.semantic_wait(agent).unwrap().unwrap();
    assert!(matches!(
        &after_end.status,
        SemanticWaitStatus::Ended { reason } if reason == "条件已经满足"
    ));
    assert_eq!(after_end.deadline_at, original_deadline);
    assert!(matches!(
        store.semantic_wait_receipt(ended).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Ended
    ));

    let late = commit_output(
        &store,
        &mut subject,
        5,
        &[],
        "[wait:continue]\n迟到的继续。",
    )
    .await;
    assert!(matches!(
        store.semantic_wait_receipt(late).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Rejected { .. }
    ));
    assert!(matches!(
        store.semantic_wait(agent).unwrap().unwrap().status,
        SemanticWaitStatus::Ended { .. }
    ));
    // A changed judgment may replace an active wait in one S1. The original
    // condition and deadline stay in the old record rather than being renewed.
    commit_output(&store, &mut subject, 6, &[],
        "[wait] {\"condition\":\"收到对方的答复\",\"max_wait_secs\":120}\n等待答复。").await;
    let old = store.semantic_wait(agent).unwrap().unwrap();
    let replaced = commit_output(&store, &mut subject, 7, &[message("现在请关注另一个独立结果")],
        "[wait:replace] {\"condition\":\"收到新的独立结果\",\"max_wait_secs\":180,\"reason\":\"关注转向新的结果\"}\n我改变关注。").await;
    let new = store.semantic_wait(agent).unwrap().unwrap();
    assert_ne!(new.wait_id, old.wait_id);
    assert_eq!(new.condition, "收到新的独立结果");
    assert_eq!(new.deadline_at.signed_duration_since(new.started_at).num_seconds(), 180);
    assert!(matches!(store.semantic_wait_receipt(replaced).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Replaced));
    let previous_json: String = rusqlite::Connection::open(dir.join("store.db")).unwrap()
        .query_row("SELECT data FROM semantic_waits WHERE wait_id=?1", [old.wait_id.to_string()], |r| r.get(0)).unwrap();
    let previous: SemanticWait = serde_json::from_str(&previous_json).unwrap();
    assert_eq!(previous.deadline_at, old.deadline_at);
    assert!(matches!(previous.status, SemanticWaitStatus::Ended { .. }));

    let renewal = commit_output(&store, &mut subject, 8, &[],
        "[wait:replace] {\"condition\":\"收到新的独立结果\",\"max_wait_secs\":600,\"reason\":\"还想等\"}\n继续观察。").await;
    assert!(matches!(store.semantic_wait_receipt(renewal).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Rejected { .. }));
    let after_renewal = store.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(after_renewal.wait_id, new.wait_id);
    assert_eq!(after_renewal.deadline_at, new.deadline_at);
    // Rejecting stale or renewing controls never costs the subject its successor.
    assert_eq!(subject.head.committed_step_no, 8);
    assert_eq!(subject.step.step_no, 9);
    // Keep the fixture on container disk for independent inspection.
}
