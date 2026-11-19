include!("native_ssh_installation_tests.rs");
include!("ssh_public_identity_tests.rs");
include!("native_ssh_memory_helper_tests.rs");
include!("node_cognition_report_tests.rs");
use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use shenko_identity::IdentityService;

use crate::cli::Cli;
use crate::cli_dispatch::configured_ssh_public_identity_output;
use crate::node_sync;
use crate::runtime_bootstrap::bootstrap_genesis;
use crate::runtime_napcat::napcat_inbox_event;
use crate::runtime_plugins::{install_native_ssh, install_node_capability_reader, install_node_selection_recorder};
use shenko_channel_napcat::{NapCatClient, NapCatConfig, spool::IngressSpool};
use shenko_types::{
    ids::*,
    operation::{EffectStatus, OperationOutcome, OperationState},
    subject::*,
    task::{AbortScope, TaskOutcome, TaskState},
};

#[tokio::test]
async fn node_capability_reader_is_discoverable_only_while_installed() {
    let dir = std::env::temp_dir().join(format!("shenko-node-reader-install-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(), ..Default::default()
    }).prepare(store.ledger.ledger_id()).unwrap();
    let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
    drop(home);
    store.start_subject_dispatcher().unwrap();
    let node = node_sync::open_node(&dir, &store).unwrap();
    let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
    supervisor.spawn(shenko_subject::NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
    let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
    assert!(service.installed_prompt_sections().is_empty());
    install_node_capability_reader(&supervisor, store.clone(), node).await.unwrap();
    let sections = service.installed_prompt_sections();
    assert_eq!(sections.len(), 1);
    assert!(sections[0].contains("[delegate:node.capabilities.query:1]"));
    // Real parser/S1/registry/handler/T1/E1/T2, with deterministic output
    // supplied as a fixture. This is not model-behavior validation.
    let task_id = EntityId::new();
    commit_native_fixture_step(&store, "[delegate:node.capabilities.query:1] {\"limit\":1}", &[task_id], vec![]);
    drive_native_until(&service, || Ok(store.native_task_result(task_id)?.is_some())).await.unwrap();
    let result = store.native_task_result(task_id).unwrap().unwrap();
    assert_eq!(result.outcome, TaskOutcome::Succeeded);
    assert!(store.ready_native_results(agent, 8).unwrap().iter().any(|ready| ready.result_id == result.result_id));
    let evidence = store.external_operation_evidence(result.operation_id).unwrap();
    assert_eq!(evidence.len(), 1);
    let observation: serde_json::Value = serde_json::from_slice(&store.cas.read_object(
        &evidence[0].normalized_ref.as_ref().unwrap().digest,
    ).unwrap()).unwrap();
    assert_eq!(observation["snapshot"]["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(observation["dispatch"], "not_dispatched");
    assert_eq!(observation["execution_authority"], "not_granted");
    supervisor.stop().await.unwrap();
    assert!(service.installed_prompt_sections().is_empty());
    drop(service); drop(supervisor); drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn node_selection_recorder_delivers_explicit_choice_only_while_installed() {
    let dir = std::env::temp_dir().join(format!("shenko-node-selection-install-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(), ..Default::default()
    }).prepare(store.ledger.ledger_id()).unwrap();
    let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
    drop(home);
    store.start_subject_dispatcher().unwrap();
    let mut observer = node_sync::open_node(&dir, &store).unwrap();
    // Explicit fixture publication; the installed read tool never refreshes.
    observer.capability_profile().unwrap();
    let recorder = node_sync::open_node(&dir, &store).unwrap();
    let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
    supervisor.spawn(shenko_subject::NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
    let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
    assert!(service.installed_prompt_sections().is_empty());
    install_node_capability_reader(&supervisor, store.clone(), observer).await.unwrap();
    assert!(!service.installed_prompt_sections().iter().any(|section| section.contains("[delegate:node.selection.record:1]")));
    install_node_selection_recorder(&supervisor, store.clone(), recorder).await.unwrap();
    assert_eq!(service.installed_prompt_sections().len(), 2);
    assert!(service.installed_prompt_sections().iter().any(|section| section.contains("[delegate:node.selection.record:1]")));
    // Real parser/S1/registry/T1/Node command/E1/T2, deterministic fixture
    // output only. This does not sample a model or start SSH.
    let query = EntityId::new();
    commit_native_fixture_step(&store, "[delegate:node.capabilities.query:1] {\"limit\":1}", &[query], vec![]);
    drive_native_until(&service, || Ok(store.native_task_result(query)?.is_some())).await.unwrap();
    let query_result = store.native_task_result(query).unwrap().unwrap();
    let candidate = &query_result.result.as_ref().unwrap()["snapshot"]["candidates"][0];
    let task = EntityId::new();
    let input = serde_json::json!({
        "node_id":candidate["node_id"],
        "observed_profile_digest":candidate["knowledge"]["observation"]["profile_digest"],
        "reason":"I explicitly choose the observed fixture node; no dispatch requested",
    });
    commit_native_fixture_step(&store, &format!("[delegate:node.selection.record:1] {input}"), &[task], vec![]);
    drive_native_until(&service, || Ok(store.native_task_result(task)?.is_some())).await.unwrap();
    let result = store.native_task_result(task).unwrap().unwrap();
    assert_eq!(result.outcome, TaskOutcome::Succeeded);
    let output = result.result.as_ref().unwrap();
    assert_eq!(output["receipt"]["selection"]["decision_id"], serde_json::json!(result.operation_id));
    assert_eq!(output["receipt"]["selection"]["node_id"], input["node_id"]);
    assert_eq!(output["receipt"]["selection"]["reason"], input["reason"]);
    assert_eq!(output["receipt"]["outcome"]["status"], "recorded");
    assert_eq!(output["effect_scope"], "local_selection_journal_only");
    assert_eq!(output["dispatch"], "not_dispatched");
    assert_eq!(output["execution_authority"], "not_granted");
    assert_eq!(store.external_operation_attempts(result.operation_id).unwrap().len(), 1);
    assert_eq!(store.external_operation_evidence(result.operation_id).unwrap().len(), 1);
    assert!(store.ready_native_results(agent, 8).unwrap().iter().any(|ready| ready.result_id == result.result_id));
    supervisor.stop().await.unwrap();
    assert!(service.installed_prompt_sections().is_empty());
    drop(service); drop(supervisor); drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

// Literal pre-attribution canonical bytes survive reopen/replay while
// Planned and after sealing, then S1 is processed once before I mirrors
// its ACK. This is native I/L/CAS mechanics, not model-behavior proof.
#[tokio::test]
async fn reopen_reconciles_spool_from_committed_subject_disposition() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("shenko-continuity-{}", EntityId::new()));
    std::fs::create_dir(&dir).unwrap();
    let spool_path = dir.join("ingress.sqlite");
    let spool = IngressSpool::open(&spool_path, 9).unwrap();
    let received = spool
        .ingest(serde_json::json!({
            "post_type":"message_sent", "message_type":"group", "group_id":7,
            "self_id":9, "user_id":9, "message_id":42, "raw_message":"continue",
            "sender":{"user_id":9,"nickname":"深刻"}
        }))?
        .event()
        .context("fixture ingress did not produce an event")?;
    drop(spool);

    let config = NapCatConfig {
        bot_qq: 9,
        ws_url: "ws://127.0.0.1:0".into(),
        ..Default::default()
    };
    let (client, mut rx, actions) =
        NapCatClient::open_persistent(config.clone(), &spool_path).unwrap();
    let client = Arc::new(client);
    let plugin: Arc<dyn shenko_types::channel::ChannelPlugin> = Arc::new(
        shenko_channel_napcat::NapCatChannel::new(config.clone(), client.clone()),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let connector = tokio::spawn(NapCatClient::run_connection_loop(
        config.clone(),
        client.event_sender(),
        actions,
        client.pending_actions.clone(),
        client.spool.clone(),
        cancel.clone(),
    ));
    // Delivery now runs independently of connection establishment.
    let replayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    connector.await.unwrap().unwrap();

    // This original deliberately comes from the old wire shape, not a
    // round trip through the candidate's serializer. Ownership and shape
    // were absent; its already-staged text is the surviving attribution.
    let old_payload = serde_json::json!({
        "schema_version":1,"provider":"qq","account":"qq:9",
        "transport_identity_domain":"onebot_v11","conversation":"7",
        "message_id":"42","received_at":"2026-11-12T00:00:00Z",
        "actor":{"platform_id":"9","display_name":"深刻"},
        "content":{"text":"continue"},
        "reply_target":{"kind":"group","id":"7","label":"群 7",
            "account":"qq:9","conversation":"7"}
    });
    let inbox_event = shenko_types::inbox::InboxEvent {
        event_id: received.spool_id,
        kind: shenko_types::inbox::InboxEventKind::Message,
        created_at: serde_json::from_value(old_payload["received_at"].clone())?,
        payload: old_payload,
        source: shenko_channel_napcat::QQ_SOURCE.into(),
    };
    let old_event_bytes = serde_json::to_vec(&inbox_event)?;
    let historical = plugin.classify(&inbox_event.source, &inbox_event.payload)
        .context("historical inbox event was not classified")?;
    assert_eq!(historical.actor.own_account, None);
    assert_eq!(serde_json::to_value(&historical)?, inbox_event.payload);
    assert!(plugin.staged_text(&historical).contains("是否本账号发出未记录"));
    let newly_classified = plugin.classify(&inbox_event.source, &received.raw)
        .context("original wire event was not classified")?;
    assert_eq!(newly_classified.actor.own_account, Some(true));
    assert!(newly_classified.reply_target.as_ref()
        .context("newly classified event has no reply target")?.shape.is_some());

    let store = Arc::new(shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
    let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(),
        ..Default::default()
    })
    .prepare(store.ledger.ledger_id())
    .unwrap();
    let model_profile = shenko_seed::ModelProfile::from_env();
    let agent = bootstrap_genesis(
        &store,
        None,
        &prepared_home.manifest,
        &model_profile,
    )
    .unwrap();
    let (head, step) = store.load_subject(agent)?;
    assert!(
        store
            .event_disposition_commit(agent, inbox_event.event_id)
            .unwrap()
            .is_none()
    );
    assert!(!client.spool.event(received.spool_id).unwrap().consumed);
    let object = store
        .cas
        .seal_object(b"continue", "text/plain", "test", "raw", 1)
        .unwrap()
        .object;
    let old_text = format!("[event {}] [深刻（本账号发出） · 群 7] continue", inbox_event.event_id);
    let history_input = shenko_store::SubjectHistoryInput {
        event_id: inbox_event.event_id, event_type: "user".into(),
        data: serde_json::json!({"v":1,"text":old_text,"inbox_event":&inbox_event}),
    };
    let staged = store.stage_subject_inputs(&head, &step, std::slice::from_ref(&history_input))?;
    let first_raw_id = staged[0].event_id;
    let first_raw_bytes = staged[0].data.clone();
    assert!(store.first_subject_input(EntityId::new(), inbox_event.event_id)?.is_none());
    drop(store);

    let store = Arc::new(open_native_fixture(&dir));
    let (head, mut step) = store.load_subject(agent).unwrap();
    let admitted = napcat_inbox_event(&store, agent, &client.spool, &plugin, replayed.clone())
        .await?.context("planned replay did not retain its inbox event")?;
    assert_eq!(serde_json::to_vec(&admitted)?, old_event_bytes);
    assert_eq!(store.first_subject_input(agent, admitted.event_id)?
        .context("planned replay lost its first subject input")?.data, first_raw_bytes);
    assert_eq!(store.session_event(first_raw_id)?
        .context("planned replay lost its original session event")?.data, first_raw_bytes);
    assert!(!client.spool.event(received.spool_id).unwrap().consumed);
    // The original conflict fence still rejects new facts under this id.
    let mut changed = history_input;
    changed.data["inbox_event"]["payload"] = serde_json::to_value(&newly_classified)?;
    let error = store.stage_subject_inputs(&head, &step, &[changed]).err()
        .context("different inbox facts were unexpectedly accepted under the original event id")?;
    assert!(error.to_string().contains("already staged with different facts"));
    step.input_plan_ref = Some(prepare_fixture_s0(&store, &head, &step));
    let sealed_ref = step.input_plan_ref.clone().context("fixture S0 input plan was not retained")?;
    let sealed_bytes = store.cas.read_object(&sealed_ref.digest)?;
    step.state = SubjectStepState::Preparing;
    step.record_revision = RecordRevision(1);
    store.upsert_step(&step, &head, RecordRevision(0)).unwrap();
    drop(store);

    let store = Arc::new(open_native_fixture(&dir));
    let (head, mut step) = store.load_subject(agent)?;
    let sealed_replay = napcat_inbox_event(&store, agent, &client.spool, &plugin, replayed.clone())
        .await?.context("sealed replay did not retain its inbox event")?;
    assert_eq!(serde_json::to_vec(&sealed_replay)?, old_event_bytes);
    assert_eq!(step.input_plan_ref.as_ref(), Some(&sealed_ref));
    assert_eq!(store.cas.read_object(&sealed_ref.digest)?, sealed_bytes);
    let frozen: serde_json::Value = serde_json::from_slice(&sealed_bytes)?;
    assert_eq!(frozen["events"], serde_json::json!([&inbox_event]));
    assert_eq!(frozen["payload"]["messages"][0]["parts"][0]["text"], old_text);
    assert_eq!(store.session_event(first_raw_id)?
        .context("sealed replay lost its original session event")?.data, first_raw_bytes);
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    step.record_revision = RecordRevision(2);
    store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(object.clone());
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
    let next = EntityId::new();
    let identity_context = cordis_core::Context::new();
    let _publication = identity_context
        .provide(Arc::new(IdentityService::new(Arc::new(
            shenko_identity::DefaultIdentityPolicy,
        ))))
        .unwrap();
    let identity_service = identity_context.try_service::<IdentityService>().unwrap();
    let _context = identity_context.provide(Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
        shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(store.clone(), EntityId(head.branch_id.0)),
        )),
    ))).unwrap();
    let decision = StepDecision {
        schema_version: 8,
        output: shenko_subject::output::parse_output(step.step_id, object.clone(), "continue", &identity_service, &shenko_channel_napcat::qq_test_registry(), None)
        .unwrap(),
        identity_intent: None,
        step_id: step.step_id,
        input_plan_ref: step.input_plan_ref.clone().unwrap(),
        model_result_ref: Some(object),
        applied_steering_refs: vec![],
        event_dispositions: vec![EventDisposition {
            event_id: inbox_event.event_id,
            disposition: Disposition::Acknowledged,
            reason: None,
            review_after: None,
            priority: None,
        }],
        delegated_task_intents: vec![],
        child_dialogue_intents: vec![],
        memory_intents: vec![],
        policy_intents: vec![],
        publication_intents: vec![],
        pending_obligations: vec![],
        evidence_refs: vec![],
        attention_policy: None,
        unknown_acknowledgements: vec![],
        successor_step_id: next,
    };
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "test",
                "StepDecision",
                8,
            )
            .unwrap()
            .object,
    );
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(4);
    let receipt = store
        .commit_step_and_advance_head(&step, &head, next)
        .unwrap();
    // Intentionally omit ACK: model-side processing committed, I mirror is stale.
    assert!(!client.spool.event(received.spool_id)?.consumed);
    drop(store);
    drop(plugin);
    drop(client);

    let store = Arc::new(
        shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap(),
    );
    assert_eq!(
        bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap(),
        agent
    );
    let restored = shenko_subject::loop_runner::SubjectLoopState::new(
        agent,
        Arc::new(shenko_subject::inbox::Inbox::new(8)),
        store.clone(),
        identity_context,
        shenko_channel_napcat::qq_test_registry(),
    )
    .unwrap();
    assert_eq!(restored.head.branch_id, head.branch_id);
    assert_eq!(restored.head.committed_at, receipt.commit_ref);
    assert_eq!(restored.step.step_id, next);
    assert_eq!(restored.step.state, SubjectStepState::Planned);
    let (client, _events, _actions) = NapCatClient::open_persistent(config.clone(), &spool_path)?;
    let client = Arc::new(client);
    let spool = client.spool.clone();
    let plugin: Arc<dyn shenko_types::channel::ChannelPlugin> = Arc::new(
        shenko_channel_napcat::NapCatChannel::new(config, client),
    );
    let pending = spool.consume(8).unwrap();
    assert_eq!(pending.len(), 1);
    let commit = store
        .event_disposition_commit(agent, pending[0].spool_id)
        .unwrap()
        .unwrap();
    assert_eq!(commit, receipt.commit_ref);
    assert!(napcat_inbox_event(&store, agent, &spool, &plugin, replayed).await?.is_none());
    assert_eq!(store.load_subject(agent)?.0.committed_step_no, 1);
    assert_eq!(spool.pending_count().unwrap(), 0);
    assert_eq!(spool.event(received.spool_id).unwrap().raw, received.raw);
    assert!(spool.ingest(received.raw).unwrap().event().is_none());
    // Temporary evidence files are deliberately retained for inspection.
    Ok(())
}

fn open_native_fixture(dir: &std::path::Path) -> shenko_store::store::Store {
    shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap()
}

pub(super) fn prepare_fixture_s0(
    store: &Arc<shenko_store::store::Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    step: &shenko_types::subject::SubjectStep,
) -> shenko_types::object::ObjectRef {
    let scope = EntityId(head.branch_id.0);
    let staged = store.stage_subject_inputs(head, step, &[]).unwrap();
    let input_raw_event_ids = staged.iter().map(|row| row.event_id).collect::<Vec<_>>();
    let (events, messages): (Vec<_>, Vec<_>) = staged.iter().map(|row| {
        let data: serde_json::Value = serde_json::from_str(&row.data).unwrap();
        let text = data.get("text").and_then(serde_json::Value::as_str).unwrap_or(&row.data);
        (data.get("inbox_event").cloned().unwrap(),
            shenko_broker::request::InferenceMessage::text("user", text))
    }).unzip();
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(store.clone(), scope),
    ));
    let profile = shenko_types::object::Digest::blake3(b"native-recovery-fixture");
    ledger.rebind_model(1, &profile.0).unwrap();
    let window = ledger.current().unwrap();
    let leaf_seq = store.branch_history_leaf_seq(scope).unwrap()
        .map(|seq| u64::try_from(seq).unwrap());
    let input = serde_json::json!({
        "schema_version":12, "step_id":step.step_id, "step_no":step.step_no, "provider_selection":{"schema_version":1,"binding":null,"model":null,"profile_id":null,"profile_revision":null,"credential_fingerprint":null,"resource_domain_id":null,"serializer_revision":null}, "request_id":"00000000-0000-0000-0000-000000000000",
        "events":events, "provider_binding":null,
        "payload":shenko_broker::request::InferencePayload {
            system_prompt:"native recovery fixture".into(), messages,
            max_tokens:Some(4096), tools:vec![], native_options:Default::default(),
        },
        "semantic_wait":store.semantic_wait(head.agent_id).unwrap(), "identity":store.latest_identity(head.agent_id).unwrap(),
        "identity_provider_revision":"shenko.identity/default@1",
        "context":{
            "source":shenko_store::SubjectContextRef {
                scope_id:scope, source_commit_seq:store.current_commit_seq().unwrap(), leaf_seq,
                input_raw_event_ids, window_id:window.id,
                window_generation:window.generation, model_generation:1 },
            "profile_digest":profile,
            "estimated_input_tokens":null, "budget_input_tokens":null,
            "trigger_budget_input_tokens":null,
            "budget_provenance":"native fixture without measured current input",
            "reserved_output_tokens":4096, "unfinished_call_ids":[],
            "capabilities":null, "observed_input_tokens":null,
            "token_counter":"heuristic:cjk-1-other-4@1; not a provider tokenizer",
            "policy_contract":shenko_context_policy::CONTRACT_VERSION,
            "selection":{"outcome":"unavailable","reason":"fixture_without_model_capabilities"},
        },
    });
    store.cas.seal_object(&serde_json::to_vec(&input).unwrap(), "application/json",
        "input_plans", "subject_step_input", 12).unwrap().object
}

/// Deterministic model-output fixture admitted through the real parser and S1
/// transaction. No model provider is invoked by this test helper.
fn commit_native_fixture_step(
    store: &Arc<shenko_store::store::Store>,
    model_text: &str,
    task_ids: &[EntityId],
    event_dispositions: Vec<EventDisposition>,
) -> shenko_types::commit::CommitRef {
    let (head, mut step) = store
        .load_subject(store.get_existing_agent_id().unwrap().unwrap())
        .unwrap();
    let object = store
        .cas
        .seal_object(model_text.as_bytes(), "text/plain", "test", "raw", 1)
        .unwrap()
        .object;
    step.input_plan_ref = Some(prepare_fixture_s0(store, &head, &step));
    step.state = SubjectStepState::Preparing;
    step.record_revision = RecordRevision(1);
    store.upsert_step(&step, &head, RecordRevision(0)).unwrap();
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    step.record_revision = RecordRevision(2);
    store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(object.clone());
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: object.clone(),
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
    step.record_revision = RecordRevision(3);
    store.upsert_step(&step, &head, RecordRevision(2)).unwrap();

    let identity = IdentityService::new(Arc::new(shenko_identity::DefaultIdentityPolicy));
    let output = shenko_subject::output::parse_output(step.step_id, object.clone(), model_text, &identity, &shenko_channel_napcat::qq_test_registry(), None)
    .unwrap();
    let mut delegated_task_intents = vec![];
    for control in &output.controls {
        if let shenko_types::output::OutputAction::Delegate { text, definition } =
            &control.action
        {
            delegated_task_intents.push(DelegationIntent {
                intent_id: task_ids[delegated_task_intents.len()],
                goal: text.clone(),
                system_prompt: text.clone(),
                model: None,
                capabilities: vec![],
                budget_hint: None,
                reply_grant: None,
                definition: definition.clone(),
            });
        }
    }
    assert_eq!(delegated_task_intents.len(), task_ids.len());
    let next = EntityId::new();
    let decision = StepDecision {
        schema_version: 8,
        output,
        identity_intent: None,
        step_id: step.step_id,
        input_plan_ref: step.input_plan_ref.clone().unwrap(),
        model_result_ref: Some(object),
        applied_steering_refs: vec![],
        event_dispositions,
        delegated_task_intents,
        child_dialogue_intents: vec![],
        memory_intents: vec![],
        policy_intents: vec![],
        publication_intents: vec![],
        pending_obligations: vec![],
        evidence_refs: vec![],
        attention_policy: None,
        unknown_acknowledgements: vec![],
        successor_step_id: next,
    };
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "test",
                "StepDecision",
                8,
            )
            .unwrap()
            .object,
    );
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(4);
    store
        .commit_step_and_advance_head(&step, &head, next)
        .unwrap()
        .commit_ref
}

async fn native_fixture_helper(dir: &std::path::Path) -> Result<()> {
    let store = Arc::new(open_native_fixture(dir));
    store.start_subject_dispatcher()?;
    let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
    let _child = supervisor
        .spawn(
            shenko_subject::NativeChildPlugin,
            (store.clone(), RuntimeGeneration::default()),
        )
        .await?;
    let handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HostPrintfHandler::new(store.clone()));
    let _handler = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
        .await?;
    let native = supervisor
        .context()
        .try_service::<shenko_subject::NativeChildService>()?;
    native.recover()?;
    let agent = store.get_existing_agent_id()?.expect("fixture identity exists");
    let cancel = tokio_util::sync::CancellationToken::new();
    let dispatcher = tokio::spawn(shenko_subject::dispatch::run_committed_dispatcher(
        store.clone(), native, agent, None, cancel.clone(), cancel.clone(),
    ));
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.ready_native_results(agent, 1)?.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        anyhow::Ok(())
    }).await;
    cancel.cancel();
    let dispatched = dispatcher.await;
    supervisor.stop().await?;
    observed??;
    dispatched??;
    Ok(())
}

/// Drive bounded dispatcher passes until a durable condition is met. The
/// dispatcher manages executions asynchronously; asserting "two passes
/// finish a task" would re-serialize them in the test's favour.
async fn drive_native_until(
    native: &Arc<shenko_subject::NativeChildService>,
    mut met: impl FnMut() -> Result<bool>,
) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            native.run_once().await?;
            if met()? {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?
}

// Real local process boundary: one process executes the committed Pending
// task and exits; its parent reopens the ledger and must not execute it again.
// The model output is a deterministic fixture, not production-model evidence.
#[tokio::test]
async fn native_printf_result_survives_real_process_reopen() {
    const HELPER: &str = "SHENKO_NATIVE_PRINTF_REOPEN_HELPER";
    if let Some(dir) = std::env::var_os(HELPER) {
        native_fixture_helper(std::path::Path::new(&dir))
            .await
            .unwrap();
        return;
    }
    let dir = std::env::temp_dir().join(format!("shenko-native-reopen-{}", EntityId::new()));
    std::fs::create_dir(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(),
        ..Default::default()
    })
    .prepare(store.ledger.ledger_id())
    .unwrap();
    let model_profile = shenko_seed::ModelProfile::from_env();
    let agent = bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap();
    store.start_subject_dispatcher().unwrap();
    let printf_task = EntityId::new();
    let missing_task = EntityId::new();
    let expected = "native child 真实结果\nwith spaces";
    let model_text = format!(
        "[delegate:host.printf:1] {}\n[delegate:example.missing:1] {{\"text\":\"missing\"}}",
        serde_json::json!({"text":expected,"timeout_secs":5})
    );
    commit_native_fixture_step(
        &store,
        &model_text,
        &[printf_task, missing_task],
        vec![],
    );
    let pending = store.external_task(printf_task).unwrap();
    assert!(matches!(pending.state, TaskState::Pending { .. }));
    assert_eq!(pending.kind, "host.printf");
    assert!(store.native_task_result(printf_task).unwrap().is_none());
    drop(store);

    let status = tokio::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("recovery_integration_tests::native_printf_result_survives_real_process_reopen")
        .arg("--nocapture")
        .env(HELPER, &dir)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "native helper process failed: {status}");

    let store = Arc::new(open_native_fixture(&dir));
    assert_eq!(
        bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap(),
        agent
    );
    store.start_subject_dispatcher().unwrap();
    let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
    let _child = supervisor
        .spawn(
            shenko_subject::NativeChildPlugin,
            (store.clone(), RuntimeGeneration::default()),
        )
        .await
        .unwrap();
    let handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HostPrintfHandler::new(store.clone()));
    let _handler = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
        .await
        .unwrap();
    let native = supervisor
        .context()
        .try_service::<shenko_subject::NativeChildService>()
        .unwrap();
    native.recover().unwrap();
    drive_native_until(&native, || {
        Ok(store.native_task_result(printf_task)?.is_some())
    })
    .await
    .unwrap();

    let result = store.native_task_result(printf_task).unwrap().unwrap();
    assert!(matches!(&result.outcome, TaskOutcome::Succeeded));
    assert_eq!(result.task_id, printf_task);
    assert_eq!(
        result
            .result
            .as_ref()
            .and_then(|value| value.get("stdout_utf8"))
            .and_then(serde_json::Value::as_str),
        Some(expected)
    );
    let task = store.external_task(printf_task).unwrap();
    assert!(task.is_terminal());
    assert_eq!(task.updated_commit, result.commit_ref);
    let attempts = store
        .external_operation_attempts(result.operation_id)
        .unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].invocation_id, result.invocation_id);
    let evidence = store
        .external_operation_evidence(result.operation_id)
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].receipt_id, result.receipt_id);
    assert_eq!(
        shenko_store::child::park_reason(&store.external_task(missing_task).unwrap()),
        Some("handler_unavailable")
    );
    assert!(store.native_task_result(missing_task).unwrap().is_none());
    assert_eq!(store.ready_native_results(agent, 8).unwrap().len(), 1);

    // A receiver can close while its owning Arc still lives. Durable
    // results do not make that unexpected delivery failure a success.
    let closed_inbox = Arc::new(shenko_subject::inbox::Inbox::new(8));
    closed_inbox.close().await;
    let closed_cancel = tokio_util::sync::CancellationToken::new();
    let mut closed_dispatcher = tokio::spawn(
        shenko_subject::dispatch::run_committed_dispatcher(
            store.clone(), native.clone(), agent,
            Some(closed_inbox.sender()),
            tokio_util::sync::CancellationToken::new(), closed_cancel.clone(),
        ),
    );
    let failed = tokio::time::timeout(
        std::time::Duration::from_secs(5), &mut closed_dispatcher,
    ).await;
    closed_cancel.cancel();
    if failed.is_err() {
        closed_dispatcher.await.unwrap().unwrap();
        supervisor.stop().await.unwrap();
        panic!("unexpected inbox closure was silently suppressed by the native dispatcher");
    }
    let delivery_error = failed.unwrap().unwrap().expect_err(
        "unexpected inbox closure must propagate, not return success");
    assert!(delivery_error.is::<tokio::sync::mpsc::error::SendError<shenko_types::inbox::InboxEvent>>(),
        "the propagated cause must be receiver loss, not an unrelated store failure");
    let (head, _) = store.load_subject(agent).unwrap();
    let error_filter = shenko_store::SessionEventFilter {
        from_seq: 0,
        to_seq: i64::MAX,
        limit: 20,
        event_types: vec![shenko_store::RUNTIME_ERROR_EVENT_TYPE.to_owned()],
        roles: vec![],
    };
    let errors = store.list_session_events(
        EntityId(head.branch_id.0), &error_filter,
    ).unwrap();
    assert_eq!(errors.len(), 1);
    let error: serde_json::Value = serde_json::from_str(&errors[0].data).unwrap();
    assert_eq!(error["kind"], "native_dispatcher");
    assert_eq!(error["journal_dropped_total"], 0);
    assert_eq!(store.ready_native_results(agent, 8).unwrap()[0].result_id, result.result_id);
    assert_eq!(
        serde_json::to_value(store.external_operation_evidence(result.operation_id).unwrap()).unwrap(),
        serde_json::to_value(&evidence).unwrap(),
        "delivery failure must not replace the original native E1",
    );

    // Stopping subject delivery must not stop durable consumers before
    // the final S1 work can settle. Execute another real printf while
    // the same inbox is closed, with only the subject token cancelled.
    let subject_stopping = tokio_util::sync::CancellationToken::new();
    subject_stopping.cancel();
    let settling_task = EntityId::new();
    let settling_text = "native work settles after subject delivery stops";
    commit_native_fixture_step(
        &store,
        &format!("[delegate:host.printf:1] {}", serde_json::json!({
            "text": settling_text, "timeout_secs": 5,
        })),
        &[settling_task], vec![],
    );
    let settling_cancel = tokio_util::sync::CancellationToken::new();
    let settling_dispatcher = tokio::spawn(
        shenko_subject::dispatch::run_committed_dispatcher(
            store.clone(), native.clone(), agent, Some(closed_inbox.sender()),
            subject_stopping, settling_cancel.clone(),
        ),
    );
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(result) = store.native_task_result(settling_task)? {
                break anyhow::Ok(result);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await;
    settling_cancel.cancel();
    settling_dispatcher.await.unwrap().unwrap();
    let settled = settled.unwrap().unwrap();
    assert!(matches!(&settled.outcome, TaskOutcome::Succeeded));
    assert_eq!(settled.result.as_ref().unwrap()["stdout_utf8"], settling_text);
    assert_eq!(store.external_operation_attempts(settled.operation_id).unwrap().len(), 1);
    assert_eq!(store.ready_native_results(agent, 8).unwrap().len(), 2);
    assert_eq!(store.list_session_events(EntityId(head.branch_id.0), &error_filter).unwrap().len(), 1,
        "expected shutdown must not add a runtime error");

    let inbox = Arc::new(shenko_subject::inbox::Inbox::new(8));
    let cancel = tokio_util::sync::CancellationToken::new();
    let dispatcher = tokio::spawn(shenko_subject::dispatch::run_committed_dispatcher(
        store.clone(),
        native.clone(),
        agent,
        Some(inbox.sender()),
        cancel.clone(),
        cancel.clone(),
    ));
    let events = inbox.recv_batch(2, 2_000).await;
    cancel.cancel();
    dispatcher.await.unwrap().unwrap();
    assert_eq!(events.len(), 2);
    let original_event = events.iter().find(|event| event.event_id == result.result_id).unwrap();
    let settled_event = events.iter().find(|event| event.event_id == settled.result_id).unwrap();
    assert_eq!(settled_event.payload["result"]["stdout_utf8"], settling_text);
    assert_eq!(
        original_event.kind,
        shenko_types::inbox::InboxEventKind::TaskResult
    );
    assert_eq!(
        original_event
            .payload
            .get("result")
            .and_then(|value| value.get("stdout_utf8"))
            .and_then(serde_json::Value::as_str),
        Some(expected)
    );
    assert_eq!(
        store
            .external_operation_attempts(result.operation_id)
            .unwrap()
            .len(),
        1
    );
    supervisor.stop().await.unwrap();

    commit_native_fixture_step(
        &store,
        "已接收并核对 child 结果",
        &[],
        events.iter().map(|event| EventDisposition {
            event_id: event.event_id,
            disposition: Disposition::Acknowledged,
            reason: None,
            review_after: None,
            priority: None,
        }).collect(),
    );
    assert!(store.ready_native_results(agent, 8).unwrap().is_empty());
    assert_eq!(
        store
            .external_operation_attempts(result.operation_id)
            .unwrap()
            .len(),
        1
    );
    drop(native);
    drop(_handler);
    drop(_child);
    drop(supervisor);
    drop(store);
    let reopened = open_native_fixture(&dir);
    let reopened_errors = reopened.list_session_events(
        EntityId(head.branch_id.0), &error_filter,
    ).unwrap();
    assert_eq!(reopened_errors.len(), 1);
    assert_eq!(reopened_errors[0].data, errors[0].data);
    assert!(reopened.ready_native_results(agent, 8).unwrap().is_empty());
    assert_eq!(
        serde_json::to_value(reopened.external_operation_evidence(result.operation_id).unwrap()).unwrap(),
        serde_json::to_value(&evidence).unwrap(),
    );
    // The same real receipt delivered again after the reopen is a replay
    // of one fact: no second receipt, no new operation revision, and the
    // single writer still accepts the next fact.
    let before_replay = reopened.external_operation(result.operation_id).unwrap();
    let replayed = reopened
        .record_external_evidence(
            result.operation_id,
            attempts[0].attempt_id,
            evidence[0].clone(),
            Some(OperationOutcome::Succeeded),
            EffectStatus::None,
        )
        .unwrap();
    assert_eq!(replayed.record_revision, before_replay.record_revision);
    assert_eq!(replayed.evidence_through, before_replay.evidence_through);
    assert_eq!(
        reopened
            .external_operation_evidence(result.operation_id)
            .unwrap()
            .len(),
        1
    );
    assert!(reopened
        .external_receipt_conflicts(result.operation_id)
        .unwrap()
        .is_empty());
    assert_eq!(
        reopened.external_operation_attempts(result.operation_id).unwrap().len(),
        1
    );
    // The process/ledger/evidence directory is retained for readback.
}

/// A receipt is one fact. Replaying it must not consume a second commit,
/// duplicate its evidence or close the single writer; a different original
/// under the same stable source event scope is an explicit
/// `RECEIPT_KEY_CONFLICT` that preserves the refused original, never a
/// silent replacement and never a second folding of the operation.
///
/// A separate process reopens the retained ledger and performs the replay,
/// the conflict and the task settlement; this process then reads the
/// durable result back. Nothing here executes an external effect: the
/// operation is armed by `arm_simulated_native_operation`.
#[test]
fn receipt_replay_and_source_key_conflict_survive_real_process_reopen() {
    const HELPER: &str = "SHENKO_RECEIPT_CONFLICT_REOPEN_HELPER";
    if let Some(payload) = std::env::var_os(HELPER) {
        run_receipt_conflict_helper(std::path::Path::new(&payload)).unwrap();
        return;
    }
    let dir = std::env::temp_dir().join(format!("shenko-receipt-conflict-{}", EntityId::new()));
    std::fs::create_dir(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(),
        ..Default::default()
    })
    .prepare(store.ledger.ledger_id())
    .unwrap();
    let agent = bootstrap_genesis(
        &store,
        None,
        &home.manifest,
        &shenko_seed::ModelProfile::from_env(),
    )
    .unwrap();
    store.start_subject_dispatcher().unwrap();
    let task_id = EntityId::new();
    commit_native_fixture_step(
        &store,
        "[delegate:host.printf:1] {\"text\":\"receipt admission\"}",
        &[task_id],
        vec![],
    );
    let descriptor = shenko_kernel::task::NativeTaskHandler::descriptor(
        &shenko_exec_host::HostPrintfHandler::new(store.clone()),
    ).unwrap();
    let claim = store
        .claim_native_task(task_id, descriptor.implementation_ref.clone(), false)
        .unwrap()
        .unwrap();
    let (operation, attempt) = arm_simulated_native_operation(
        &store,
        &claim,
        descriptor.implementation_ref.clone(),
        "host.printf.v1",
    )
    .unwrap();
    let source_instance = attempt.node_id.to_string();
    let original = native_observation_receipt(
        &operation,
        &attempt,
        &source_instance,
        "printf-observation",
        seal_native_observation(&store, br#"{"stdout":"original"}"#),
    );
    let resolved = store
        .record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            original.clone(),
            Some(OperationOutcome::Succeeded),
            EffectStatus::Complete,
        )
        .unwrap();
    assert_eq!(resolved.state, OperationState::Resolved);
    assert_eq!(
        store
            .external_operation_evidence(operation.operation_id)
            .unwrap()
            .len(),
        1
    );
    let conflict = native_observation_receipt(
        &operation,
        &attempt,
        &source_instance,
        "printf-observation",
        seal_native_observation(&store, br#"{"stdout":"divergent"}"#),
    );
    // The same source id and stable event key seen by one observer is one
    // source scope; a second observer instance is a different scope.
    let foreign_instance = native_observation_receipt(
        &operation,
        &attempt,
        &format!("{source_instance}.observer"),
        "printf-observation",
        seal_native_observation(&store, br#"{"stdout":"second observer"}"#),
    );
    let after_conflict = native_observation_receipt(
        &operation,
        &attempt,
        &source_instance,
        "printf-observation-followup",
        seal_native_observation(&store, br#"{"stdout":"later original"}"#),
    );
    let payload = ReceiptConflictPayload {
        dir: dir.clone(),
        operation_id: operation.operation_id,
        attempt_id: attempt.attempt_id,
        original: original.clone(),
        conflict: conflict.clone(),
        foreign_instance: foreign_instance.clone(),
        after_conflict: after_conflict.clone(),
        claim: ReceiptConflictClaim {
            task: claim.task.clone(),
            invocation: claim.invocation.clone(),
            operation_id: claim.operation_id,
            result_id: claim.result_id,
            software_generation: claim.software_generation.clone(),
            settlement_only: claim.settlement_only,
        },
        report: dir.join("receipt-conflict-report.json"),
    };
    let payload_path = dir.join("receipt-conflict-payload.json");
    std::fs::write(&payload_path, serde_json::to_vec(&payload).unwrap()).unwrap();
    drop(store);

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(
            "recovery_integration_tests::receipt_replay_and_source_key_conflict_survive_real_process_reopen",
        )
        .arg("--nocapture")
        .env(HELPER, &payload_path)
        .status()
        .unwrap();
    assert!(status.success(), "receipt admission helper process failed: {status}");

    let report: ReceiptConflictReport =
        serde_json::from_slice(&std::fs::read(&payload.report).unwrap()).unwrap();
    assert_eq!(
        report.replay_record_revision, resolved.record_revision.0,
        "the replayed receipt must bind to the durable operation fact",
    );
    assert_eq!(
        report.evidence_receipt_ids,
        vec![
            original.receipt_id,
            foreign_instance.receipt_id,
            after_conflict.receipt_id
        ],
        "the refused receipt must never become this operation's evidence",
    );
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].receipt_id, conflict.receipt_id);
    assert_eq!(report.conflicts[0].original_receipt_id, original.receipt_id);
    assert_eq!(
        report.conflicts[0].matched,
        shenko_store::ConflictIdentity::SourceEventKey
    );
    assert!(
        report.conflicts[0]
            .differing_fields
            .iter()
            .any(|field| field == "raw_ref"),
        "the conflict must name the differing original, not only its identity",
    );
    assert_eq!(report.conflicts[0].receipt.raw_ref, conflict.raw_ref);
    assert_eq!(
        report.conflicts[0].receipt.operation_id,
        Some(operation.operation_id)
    );

    let reopened = open_native_fixture(&dir);
    let conflicts = reopened
        .external_receipt_conflicts(operation.operation_id)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&conflicts).unwrap(),
        serde_json::to_value(&report.conflicts).unwrap(),
        "conflict evidence must survive the process that refused it",
    );
    let evidence = reopened
        .external_operation_evidence(operation.operation_id)
        .unwrap();
    assert_eq!(evidence.len(), 3);
    assert_eq!(evidence[0].receipt_id, original.receipt_id);
    assert_eq!(evidence[0].raw_ref, original.raw_ref);
    // The first durable receipt is not rewritten by the replay or the
    // conflict: same identity, same original, same observed facts.
    assert_eq!(evidence[0].observed_at, original.observed_at);
    assert_eq!(evidence[0].source_event_key, original.source_event_key);
    let operation_after = reopened.external_operation(operation.operation_id).unwrap();
    assert!(matches!(operation_after.state, OperationState::Resolved));
    assert_eq!(operation_after.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(
        serde_json::to_value(&operation_after).unwrap(),
        serde_json::to_value(&report.operation).unwrap()
    );
    let task = reopened.external_task(task_id).unwrap();
    assert!(task.is_terminal());
    assert_eq!(
        reopened
            .native_task_result(task_id)
            .unwrap()
            .unwrap()
            .result_id,
        report.result_id,
        "the task effect must settle exactly once from the replayed evidence",
    );
    // A late observation in the reopened process is still admitted, and it
    // does not rewrite the already-settled durable result.
    let late = native_observation_receipt(
        &operation,
        &attempt,
        &source_instance,
        "printf-observation-late",
        seal_native_observation(&reopened, br#"{"stdout":"late"}"#),
    );
    reopened
        .record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            late,
            Some(OperationOutcome::Succeeded),
            EffectStatus::Complete,
        )
        .unwrap();
    assert_eq!(
        reopened
            .external_operation_evidence(operation.operation_id)
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        reopened
            .native_task_result(task_id)
            .unwrap()
            .unwrap()
            .result_id,
        report.result_id
    );
    // The process/ledger/conflict directory is retained for readback.
}

/// One stable source event of a simulated printf observation, sealed by the
/// calling ledger's CAS. Only the source scope and the original bytes
/// decide admission; no external process runs.
fn native_observation_receipt(
    operation: &shenko_types::operation::OperationRecord,
    attempt: &shenko_types::operation::AttemptRecord,
    source_instance: &str,
    source_event_key: &str,
    raw: shenko_types::object::ObjectRef,
) -> shenko_types::evidence::EvidenceReceipt {
    shenko_types::evidence::EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(operation.operation_id),
        attempt_id: Some(attempt.attempt_id),
        source_id: "host.printf.v1".into(),
        source_instance_id: source_instance.into(),
        source_event_key: Some(source_event_key.into()),
        producer_epoch: Some(attempt.epoch.0),
        producer_schema_version: Some("1".into()),
        raw_ref: raw,
        decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: shenko_types::evidence::DecodeStatus::Decoded,
        observation_level: Some(shenko_types::evidence::ObservationLevel::RemoteCompleted),
        normalized_ref: None,
        observed_at: Some(chrono::Utc::now()),
        received_commit: operation.prepared_commit,
    }
}

fn seal_native_observation(
    store: &shenko_store::store::Store,
    bytes: &[u8],
) -> shenko_types::object::ObjectRef {
    store
        .cas
        .seal_object(
            bytes,
            "application/json",
            "receipt-admission-fixture",
            "printf-observation",
            1,
        )
        .unwrap()
        .object
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReceiptConflictClaim {
    task: shenko_types::task::TaskRecord,
    invocation: shenko_types::task::TaskInvocation,
    operation_id: EntityId,
    result_id: EntityId,
    software_generation: RuntimeGeneration,
    settlement_only: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReceiptConflictPayload {
    dir: std::path::PathBuf,
    operation_id: EntityId,
    attempt_id: EntityId,
    original: shenko_types::evidence::EvidenceReceipt,
    conflict: shenko_types::evidence::EvidenceReceipt,
    foreign_instance: shenko_types::evidence::EvidenceReceipt,
    after_conflict: shenko_types::evidence::EvidenceReceipt,
    claim: ReceiptConflictClaim,
    report: std::path::PathBuf,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReceiptConflictReport {
    replay_record_revision: u64,
    evidence_receipt_ids: Vec<EntityId>,
    conflicts: Vec<shenko_store::EvidenceReceiptConflict>,
    operation: shenko_types::operation::OperationRecord,
    result_id: EntityId,
}

fn run_receipt_conflict_helper(path: &std::path::Path) -> Result<()> {
    use shenko_types::error::{RetryStrategy, ShenkoError};
    let payload: ReceiptConflictPayload = serde_json::from_slice(&std::fs::read(path)?)?;
    let store = Arc::new(open_native_fixture(&payload.dir));
    store.start_subject_dispatcher()?;
    let operation = store.external_operation(payload.operation_id)?;
    assert_eq!(operation.state, OperationState::Resolved);
    let evidence_before = store.external_operation_evidence(payload.operation_id)?;
    assert_eq!(evidence_before.len(), 1);

    // 1. The identical fact again is a replay of the durable projection.
    let replayed = store.record_external_evidence(
        payload.operation_id,
        payload.attempt_id,
        payload.original.clone(),
        Some(OperationOutcome::Succeeded),
        EffectStatus::Complete,
    )?;
    assert_eq!(
        replayed.record_revision, operation.record_revision,
        "a replay must not consume an operation revision",
    );
    assert_eq!(replayed.outcome, operation.outcome);
    assert_eq!(replayed.evidence_through, operation.evidence_through);
    assert_eq!(
        store
            .external_operation_evidence(payload.operation_id)?
            .len(),
        1,
        "a replay must not duplicate the durable receipt",
    );
    assert!(store
        .external_receipt_conflicts(payload.operation_id)?
        .is_empty());

    // 2. A different original under the same scope and stable event key is
    // refused explicitly, and the refusal itself becomes durable evidence.
    let refusal = store
        .record_external_evidence(
            payload.operation_id,
            payload.attempt_id,
            payload.conflict.clone(),
            Some(OperationOutcome::Succeeded),
            EffectStatus::Complete,
        )
        .expect_err("a divergent original under one source event key must be refused");
    let typed = refusal
        .downcast_ref::<ShenkoError>()
        .expect("the refusal must be a typed protocol error");
    let ShenkoError::Protocol(protocol) = typed else {
        panic!("unexpected refusal: {typed}");
    };
    assert_eq!(protocol.code, "RECEIPT_KEY_CONFLICT");
    assert_eq!(protocol.retry, RetryStrategy::NeverAutomatically);
    assert_eq!(protocol.operation_id, Some(payload.operation_id));
    assert!(
        protocol.details["differing_fields"]
            .as_array()
            .is_some_and(|fields| fields.iter().any(|field| field == "raw_ref")),
        "the refusal must report the differing original bytes",
    );
    let conflicts = store.external_receipt_conflicts(payload.operation_id)?;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].receipt_id, payload.conflict.receipt_id);
    assert_eq!(conflicts[0].original_receipt_id, payload.original.receipt_id);
    assert_eq!(
        conflicts[0].matched,
        shenko_store::ConflictIdentity::SourceEventKey
    );
    assert_eq!(
        serde_json::to_value(&store.external_operation(payload.operation_id)?)?,
        serde_json::to_value(&operation)?,
        "a refused receipt must not move the operation projection",
    );
    assert_eq!(
        serde_json::to_value(&store.external_operation_evidence(payload.operation_id)?)?,
        serde_json::to_value(&evidence_before)?,
        "a refused receipt must not rewrite the admitted evidence",
    );

    // 3. Refusing the same receipt again preserves one fact, not two.
    store
        .record_external_evidence(
            payload.operation_id,
            payload.attempt_id,
            payload.conflict.clone(),
            Some(OperationOutcome::Succeeded),
            EffectStatus::Complete,
        )
        .expect_err("the repeated divergent original stays refused");
    assert_eq!(
        store.external_receipt_conflicts(payload.operation_id)?.len(),
        1,
        "a repeated conflict must not duplicate the preserved fact",
    );

    // 4. The writer is still usable, and a different observer instance of
    // the same source and key is a different source scope, not a conflict.
    let foreign = store.record_external_evidence(
        payload.operation_id,
        payload.attempt_id,
        payload.foreign_instance.clone(),
        Some(OperationOutcome::Succeeded),
        EffectStatus::Complete,
    )?;
    assert_eq!(foreign.state, OperationState::Resolved);
    // 5. A later original under a new stable key is admitted normally.
    store.record_external_evidence(
        payload.operation_id,
        payload.attempt_id,
        payload.after_conflict.clone(),
        Some(OperationOutcome::Succeeded),
        EffectStatus::Complete,
    )?;

    // 6. The durable task effect settles once from the replayed evidence,
    // and settles the same result again when it is replayed.
    let claim = shenko_store::NativeTaskClaim {
        task: payload.claim.task.clone(),
        invocation: payload.claim.invocation.clone(),
        operation_id: payload.claim.operation_id,
        result_id: payload.claim.result_id,
        software_generation: payload.claim.software_generation.clone(),
        settlement_only: payload.claim.settlement_only,
    };
    let current = store.external_operation(payload.operation_id)?;
    let result = store
        .complete_native_task(&claim, &current)?
        .expect("the resolved operation must settle exactly one result");
    assert_eq!(result.operation_id, payload.operation_id);
    let resettled = store
        .complete_native_task(&claim, &current)?
        .expect("a settled task replays its durable result");
    assert_eq!(resettled.result_id, result.result_id);

    let report = ReceiptConflictReport {
        replay_record_revision: replayed.record_revision.0,
        evidence_receipt_ids: store
            .external_operation_evidence(payload.operation_id)?
            .iter()
            .map(|receipt| receipt.receipt_id)
            .collect(),
        conflicts: store.external_receipt_conflicts(payload.operation_id)?,
        operation: store.external_operation(payload.operation_id)?,
        result_id: result.result_id,
    };
    assert_eq!(
        report.replay_record_revision, operation.record_revision.0,
        "the helper reports the projection the replay bound to",
    );
    std::fs::write(&payload.report, serde_json::to_vec(&report)?)?;
    Ok(())
}

/// Arm the existing T1 record without spawning a process. This is explicitly
/// a fault/interleaving simulation; only the preceding test is real process
/// execution evidence. It verifies fencing and recovery, not SSH or a model.
fn arm_simulated_native_operation(
    store: &shenko_store::store::Store,
    claim: &shenko_store::NativeTaskClaim,
    implementation: shenko_types::task::ImplementationRef,
    operation_name: &str,
) -> Result<(
    shenko_types::operation::OperationRecord,
    shenko_types::operation::AttemptRecord,
)> {
    use shenko_types::{commit::CommitRef, object::*, operation::*};
    let seal =
        |value: &serde_json::Value, schema: &str| -> Result<shenko_types::object::ObjectRef> {
            Ok(store
                .cas
                .seal_object(
                    &serde_json::to_vec(value)?,
                    "application/json",
                    "native-fault-simulation",
                    schema,
                    1,
                )?
                .object)
        };
    let input_ref = seal(&serde_json::json!({"text":"simulated"}), "input")?;
    let bindings = ResourceBindings {
        provider: "simulation.only".into(),
        account_identity: String::new(),
        runtime_node: claim.invocation.node_id.to_string(),
        mount_generation: 0,
        capability_level: "no_process_was_started".into(),
        extra: serde_json::json!({"simulation":true}),
    };
    let resource = seal(&serde_json::to_value(bindings)?, "bindings")?;
    let acceptance = seal(
        &serde_json::json!("original simulated result"),
        "acceptance",
    )?;
    let retry = seal(&serde_json::json!("no automatic replay"), "retry")?;
    let version = VersionRef {
        artifact: ArtifactRef {
            digest: implementation.artifact_digest.clone(),
            locator: "simulation:host-printf-v1".into(),
        },
        symbol: format!("{operation_name}.execute"),
        schema: SchemaRef {
            name: "host-printf-observation-v1".into(),
            major: 1,
            digest: implementation.artifact_digest.clone(),
        },
    };
    let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
    let mut operation = OperationRecord {
        record_schema_version: 1,
        operation_id: claim.operation_id,
        record_revision: RecordRevision(0),
        task_id: claim.task.task_id,
        logical_key: "native-handler".into(),
        operation_name: operation_name.into(),
        input_schema_version: 1,
        output_schema_version: 1,
        final_input_ref: input_ref.clone(),
        input_digest: input_ref.digest.clone(),
        contract_ref: OperationContractRef {
            plugin_id: implementation.plugin_id,
            artifact_digest: implementation.artifact_digest.clone(),
            contract_digest: implementation.artifact_digest.clone(),
            receipt_reader_ref: version.clone(),
        },
        resource_bindings_ref: resource,
        authority_ref: AuthorityRef {
            delegation_id: claim.task.task_id,
            delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: claim
                .task
                .created_by_step_id
                .unwrap_or(claim.invocation.invocation_id),
        },
        acceptance_predicate_ref: acceptance,
        retry_contract_ref: retry,
        checkpoint_scope_ref: None,
        state: OperationState::InFlight,
        outcome: None,
        outcome_conflict: false,
        effect_status: EffectStatus::Unknown,
        retry_of: None,
        compensates: None,
        possible_duplicate_of: vec![],
        prepared_commit: zero,
        evidence_through: None,
    };
    let mut attempt = AttemptRecord {
        record_schema_version: 1,
        attempt_id: EntityId::new(),
        operation_id: claim.operation_id,
        attempt_no: 1,
        invocation_id: claim.invocation.invocation_id,
        worker_id: claim.invocation.worker_id,
        node_id: claim.invocation.node_id,
        epoch: claim.invocation.epoch,
        implementation_ref: version,
        dispatch_stage: AttemptDispatchStage::Armed,
        armed_commit: zero,
        provider_handle: None,
        provider_idempotency: None,
        transport_ref: None,
        started_at: None,
        last_observed_at: None,
        final_receipt_id: None,
    };
    assert!(
        store
            .arm_external_operation(&claim.invocation, &mut operation, &mut attempt)?
            .is_none()
    );
    Ok((operation, attempt))
}

#[test]
fn subject_boundary_keeps_admitted_operation_and_late_evidence() {
    let dir = std::env::temp_dir().join(format!("shenko-native-boundary-{}", EntityId::new()));
    std::fs::create_dir(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(), ..Default::default()
    }).prepare(store.ledger.ledger_id()).unwrap();
    let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
    store.start_subject_dispatcher().unwrap();
    let admitted = EntityId::new();
    let pending = EntityId::new();
    commit_native_fixture_step(
        &store,
        "[delegate:host.printf:1] {\"text\":\"admitted\"}\n[delegate:host.printf:1] {\"text\":\"pending\"}",
        &[admitted, pending], vec![],
    );
    let descriptor = shenko_kernel::task::NativeTaskHandler::descriptor(
        &shenko_exec_host::HostPrintfHandler::new(store.clone()),
    ).unwrap();
    let claim = store.claim_native_task(admitted, descriptor.implementation_ref.clone(), false).unwrap().unwrap();
    // This exercises the real T1/E1 ledger, not a completed handoff or a
    // claim that an external process ran.
    let (operation, attempt) = arm_simulated_native_operation(&store, &claim, descriptor.implementation_ref.clone(), "host.printf.v1").unwrap();
    let request_id = EntityId::new();
    store.request_subject_quiesce(agent, request_id).unwrap();
    let (head, _) = store.load_subject(agent).unwrap();
    assert!(store.pause_subject_at_boundary(&head).unwrap().is_some());
    assert!(store.claim_native_task(pending, descriptor.implementation_ref.clone(), false).unwrap().is_none());
    assert_eq!(store.external_operation(operation.operation_id).unwrap().state, OperationState::InFlight);

    let raw = store.cas.seal_object(b"late evidence", "text/plain", "test", "evidence", 1).unwrap().object;
    let evidence = shenko_types::evidence::EvidenceReceipt {
        record_schema_version: 1, receipt_id: EntityId::new(),
        operation_id: Some(operation.operation_id), attempt_id: Some(attempt.attempt_id),
        source_id: "boundary.fixture".into(), source_instance_id: attempt.node_id.to_string(),
        source_event_key: Some("late".into()), producer_epoch: Some(attempt.epoch.0),
        producer_schema_version: Some("1".into()), raw_ref: raw,
        decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: shenko_types::evidence::DecodeStatus::Decoded,
        observation_level: Some(shenko_types::evidence::ObservationLevel::PostconditionVerified),
        normalized_ref: None, observed_at: Some(chrono::Utc::now()),
        received_commit: operation.prepared_commit,
    };
    let recorded = store.record_external_evidence(operation.operation_id, attempt.attempt_id, evidence,
        Some(OperationOutcome::Succeeded), EffectStatus::None).unwrap();
    assert_eq!(recorded.state, OperationState::Resolved);
    assert_eq!(store.external_operation_attempts(operation.operation_id).unwrap().len(), 1);
    store.resume_subject_quiesce(agent, request_id).unwrap();
    assert!(store.claim_native_task(pending, descriptor.implementation_ref, false).unwrap().is_some());
}

#[tokio::test]
async fn native_cancel_late_evidence_and_unknown_recovery_are_fenced() {
    let dir = std::env::temp_dir().join(format!("shenko-native-fencing-{}", EntityId::new()));
    std::fs::create_dir(&dir).unwrap();
    let old_store = Arc::new(open_native_fixture(&dir));
    let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(),
        ..Default::default()
    })
    .prepare(old_store.ledger.ledger_id())
    .unwrap();
    bootstrap_genesis(
        &old_store,
        None,
        &prepared_home.manifest,
        &shenko_seed::ModelProfile::from_env(),
    ).unwrap();
    old_store.start_subject_dispatcher().unwrap();
    let cancelled_task = EntityId::new();
    let unknown_task = EntityId::new();
    commit_native_fixture_step(
        &old_store,
        "[delegate:host.printf:1] {\"text\":\"cancel\"}\n[delegate:host.printf:1] {\"text\":\"unknown\"}",
        &[cancelled_task, unknown_task],
        vec![],
    );
    let descriptor = shenko_kernel::task::NativeTaskHandler::descriptor(
        &shenko_exec_host::HostPrintfHandler::new(old_store.clone()),
    ).unwrap();
    let cancelled_claim = old_store
        .claim_native_task(cancelled_task, descriptor.implementation_ref.clone(), false)
        .unwrap()
        .unwrap();
    let unknown_claim = old_store
        .claim_native_task(unknown_task, descriptor.implementation_ref.clone(), false)
        .unwrap()
        .unwrap();
    let (cancelled_operation, cancelled_attempt) = arm_simulated_native_operation(
        &old_store,
        &cancelled_claim,
        descriptor.implementation_ref.clone(),
        "host.printf.v1",
    )
    .unwrap();
    let (_unknown_operation, _unknown_attempt) = arm_simulated_native_operation(
        &old_store,
        &unknown_claim,
        descriptor.implementation_ref.clone(),
        "host.printf.v1",
    )
    .unwrap();
    let old_native = shenko_subject::NativeChildService::new(
        old_store.clone(),
        RuntimeGeneration::default(),
    );
    assert_eq!(
        old_native
            .cancel(
                cancelled_task,
                "test cancellation",
                AbortScope::SubtreeOrdinary
            )
            .unwrap()
            .cancelled,
        vec![cancelled_task]
    );

    // A late original result is durable E1, not authority for the cancelled
    // invocation to create T2 or a subject result.
    let raw = old_store
        .cas
        .seal_object(
            br#"{"simulated_late_result":true}"#,
            "application/json",
            "native-fault-simulation",
            "late-result",
            1,
        )
        .unwrap()
        .object;
    let late = shenko_types::evidence::EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(cancelled_operation.operation_id),
        attempt_id: Some(cancelled_attempt.attempt_id),
        source_id: "simulation.original".into(),
        source_instance_id: cancelled_attempt.node_id.to_string(),
        source_event_key: Some("late-original-result".into()),
        producer_epoch: Some(cancelled_attempt.epoch.0),
        producer_schema_version: Some("1".into()),
        raw_ref: raw,
        decoder_ref: Some(cancelled_operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: shenko_types::evidence::DecodeStatus::Decoded,
        observation_level: Some(
            shenko_types::evidence::ObservationLevel::PostconditionVerified,
        ),
        normalized_ref: None,
        observed_at: Some(chrono::Utc::now()),
        received_commit: cancelled_operation.prepared_commit,
    };
    let resolved = old_store
        .record_external_evidence(
            cancelled_operation.operation_id,
            cancelled_attempt.attempt_id,
            late,
            Some(OperationOutcome::Succeeded),
            EffectStatus::None,
        )
        .unwrap();
    assert_eq!(resolved.state, OperationState::Resolved);
    assert!(
        old_store
            .complete_native_task(&cancelled_claim, &resolved)
            .unwrap()
            .is_none()
    );
    assert!(
        old_store
            .native_task_result(cancelled_task)
            .unwrap()
            .is_none()
    );

    let reopened = Arc::new(open_native_fixture(&dir));
    reopened.start_subject_dispatcher().unwrap();
    assert!(
        old_store
            .complete_native_task(&cancelled_claim, &resolved)
            .is_err()
    );
    drop(old_native);
    drop(old_store);

    let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
    let _child = supervisor
        .spawn(
            shenko_subject::NativeChildPlugin,
            (reopened.clone(), RuntimeGeneration::default()),
        )
        .await
        .unwrap();
    let handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HostPrintfHandler::new(reopened.clone()));
    let _handler = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
        .await
        .unwrap();
    let native = supervisor
        .context()
        .try_service::<shenko_subject::NativeChildService>()
        .unwrap();
    native.recover().unwrap();
    drive_native_until(&native, || {
        let task = reopened.external_task(unknown_task)?;
        Ok(matches!(task.state, TaskState::Pending { .. })
            && shenko_store::child::park_reason(&task) == Some("reconciling"))
    })
    .await
    .unwrap();

    let unknown = reopened
        .external_operation(unknown_claim.operation_id)
        .unwrap();
    assert_eq!(unknown.state, OperationState::Reconciling);
    assert_eq!(unknown.effect_status, EffectStatus::Unknown);
    assert_eq!(unknown.outcome, None);
    let unknown_task_record = reopened.external_task(unknown_task).unwrap();
    assert!(matches!(
        unknown_task_record.state,
        TaskState::Pending { .. }
    ));
    assert_eq!(
        shenko_store::child::park_reason(&unknown_task_record),
        Some("reconciling")
    );
    assert_eq!(
        reopened
            .external_operation_attempts(unknown_claim.operation_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .external_operation_evidence(unknown_claim.operation_id)
            .unwrap()
            .len(),
        1
    );
    assert!(reopened.native_task_result(unknown_task).unwrap().is_none());
    assert!(
        reopened
            .native_task_result(cancelled_task)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened
            .external_operation_attempts(cancelled_operation.operation_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .external_operation_evidence(cancelled_operation.operation_id)
            .unwrap()
            .len(),
        1
    );
    assert!(
        reopened
            .external_task(cancelled_task)
            .unwrap()
            .abort_request
            .is_some()
    );
    supervisor.stop().await.unwrap();
    // Simulated fault artifacts and all original receipts remain on disk.
}

// ---------------------------------------------------------------
// C15 Computer Use: real X11 (x11rb wire) observation/input/lease.
// Xvfb is spawned only by these tests; DISPLAY is never assumed and
// no human desktop is touched. x11rb is a dev-dependency of this bin.
// ---------------------------------------------------------------

/// Owning teardown for a spawned Xvfb: `Child::drop` does NOT kill, so a
/// raw Child would leak the server on success or on assert-panic.
struct XvfbGuard(std::process::Child);

impl XvfbGuard {
    fn kill_and_wait(mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for XvfbGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The display + the process that owns it. `owned: None` means the
/// display came from SHENKO_TEST_X11_DISPLAY and must never be killed.
struct X11Display {
    display: String,
    owned: Option<XvfbGuard>,
}

/// A usable X11 display: a live server (owned by us or injected) whose
/// connect actually succeeds — never a dead child sharing the number.
fn x11_display_or_skip() -> Option<X11Display> {
    use x11rb::rust_connection::RustConnection;
    if let Ok(display) = std::env::var("SHENKO_TEST_X11_DISPLAY") {
        // The port bound only applies when the resolver constructs a
        // TCP address; Unix-only `unix/:60000` is valid.
        let safe = x11rb::reexports::x11rb_protocol::parse_display::parse_display(
            Some(display.as_str()),
        )
        .is_ok_and(|p| {
            let wants_tcp = ((p.protocol.is_none()
                || p.protocol.as_deref() != Some("unix"))
                && !p.host.is_empty()
                && p.host != "unix")
                || (p.protocol.is_none() && p.host.is_empty());
            !wants_tcp || p.display <= 59535
        });
        if safe && RustConnection::connect(Some(display.as_str())).is_ok() {
            return Some(X11Display { display, owned: None });
        }
    }
    // Parallel test processes must not reserve the same display: spread
    // candidates seeded by this pid, skip numbers with a live server
    // lock or answering socket BEFORE spawn, then admit only when the
    // answering server is provably THIS child — the X lock file is
    // created atomically by Xvfb and carries its PID. The upper bound
    // is 59535, not 65535: the x11rb resolver computes `6000 + display`
    // in u16 for its TCP fallback, so larger numbers panic under
    // overflow checks even when only the unix socket is usable.
    // Residual hazard: two children may still race into spawn; the
    // loser exits or fails to claim the lock and is killed below, so an
    // external server is never killed nor silently adopted.
    let pid = std::process::id() as usize;
    let range = 59535usize - 90;
    for attempt in 0..48usize {
        let n = 90usize + (pid.wrapping_mul(2654435761).wrapping_add(attempt * 7919)) % range;
        let display = format!(":{n}");
        let lock_path = format!("/tmp/.X{n}-lock");
        // Never spawn on a number already reserved or serving.
        if std::path::Path::new(&lock_path).exists()
            || RustConnection::connect(Some(display.as_str())).is_ok()
        {
            continue;
        }
        let mut child = match std::process::Command::new("Xvfb")
            .args([&display, "-screen", "0", "800x600x24"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => continue,
        };
        // Admit only if THIS child is alive, the endpoint answers, AND
        // the server's lock names our child's PID — socket-connect alone
        // cannot prove ownership under parallel startup.
        for _ in 0..100 {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break, // child exited: not ours
                Ok(None) => {
                    if RustConnection::connect(Some(display.as_str())).is_ok() {
                        let owned_lock = std::fs::read_to_string(&lock_path)
                            .ok()
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            == Some(child.id());
                        if owned_lock {
                            return Some(X11Display {
                                display,
                                owned: Some(XvfbGuard(child)),
                            });
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    None
}

struct ComputerUseFixture {
    store: Arc<shenko_store::store::Store>,
    backend: Arc<shenko_exec_host::ComputerUseBackend>,
    handlers: ComputerUseHandlers,
    dir: std::path::PathBuf,
    /// The one admitted controller instance all fixture tasks delegate
    /// from (session owner / lease holder identity).
    instance_id: EntityId,
}

struct ComputerUseHandlers {
    session: shenko_exec_host::ComputerUseSessionHandler,
    observe: shenko_exec_host::ComputerUseObserveHandler,
    lease: shenko_exec_host::ComputerUseLeaseHandler,
    input: shenko_exec_host::ComputerUseInputHandler,
}

/// Real Store + committed Pending native tasks through the actual S1
/// transaction; claims and invocations mirror NativeChildService::process.
fn computer_use_fixture(
    task_count: usize,
    model_text: &str,
) -> (ComputerUseFixture, Vec<EntityId>) {
    let dir = std::env::temp_dir().join(format!("shenko-cu-{}", EntityId::new()));
    std::fs::create_dir(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(),
        ..Default::default()
    })
    .prepare(store.ledger.ledger_id())
    .unwrap();
    let model_profile = shenko_seed::ModelProfile::from_env();
    bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap();
    store.start_subject_dispatcher().unwrap();
    let task_ids: Vec<EntityId> = (0..task_count).map(|_| EntityId::new()).collect();
    commit_native_fixture_step(&store, model_text, &task_ids, vec![]);
    let backend = Arc::new(shenko_exec_host::ComputerUseBackend::new(store.clone()));
    (
        ComputerUseFixture {
            store,
            backend: backend.clone(),
            handlers: ComputerUseHandlers {
                session: shenko_exec_host::ComputerUseSessionHandler::new(backend.clone()),
                observe: shenko_exec_host::ComputerUseObserveHandler::new(backend.clone()),
                lease: shenko_exec_host::ComputerUseLeaseHandler::new(backend.clone()),
                input: shenko_exec_host::ComputerUseInputHandler::new(backend.clone()),
            },
            dir,
            instance_id: EntityId::new(),
        },
        task_ids,
    )
}

/// Claim a Pending task and run the real handler boundary. `input` is the
/// exact frozen op input (the committed delegate JSON is only the
/// admission marker; real dispatch resolves it from the frozen decision).
async fn drive_native_as(
    fixture: &ComputerUseFixture,
    caller_instance_id: EntityId,
    handler: &dyn shenko_kernel::task::NativeTaskHandler,
    task_id: EntityId,
    input: serde_json::Value,
) -> shenko_types::operation::OperationRecord {
    let descriptor = handler.descriptor().unwrap();
    let claim = fixture
        .store
        .claim_native_task(task_id, descriptor.implementation_ref, false)
        .unwrap()
        .expect("pending task claimable");
    let invocation = shenko_kernel::task::NativeTaskInvocation {
        cancellation: tokio_util::sync::CancellationToken::new(),
        child_tools: None,
        context: shenko_types::agent::CallContext {
            caller_instance_id,
            role_revision: descriptor.role_revision,
            task_id: Some(task_id),
            expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id),
            controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: descriptor.authority_refs,
            trace_id: EntityId::new(),
        },
        task_invocation: claim.invocation.clone(),
        task: claim.task.clone(),
        operation_id: claim.operation_id,
        authority: shenko_types::operation::AuthorityRef {
            delegation_id: task_id,
            delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: claim
                .task
                .created_by_step_id
                .unwrap_or(claim.invocation.invocation_id),
        },
        input,
    };
    handler.execute(invocation).await.unwrap()
}

/// Drive as the fixture's single admitted controller instance.
async fn drive_native(
    fixture: &ComputerUseFixture,
    handler: &dyn shenko_kernel::task::NativeTaskHandler,
    task_id: EntityId,
    input: serde_json::Value,
) -> shenko_types::operation::OperationRecord {
    drive_native_as(fixture, fixture.instance_id, handler, task_id, input).await
}

/// The sealed normalized result of an operation's evidence (T2 output).
fn op_normalized(
    fixture: &ComputerUseFixture,
    operation_id: EntityId,
) -> serde_json::Value {
    let evidence = fixture
        .store
        .external_operation_evidence(operation_id)
        .unwrap();
    let last = evidence.last().expect("at least one receipt");
    let normalized = last
        .normalized_ref
        .clone()
        .or_else(|| Some(last.raw_ref.clone()))
        .unwrap();
    let bytes = fixture.store.cas.read_object(&normalized.digest).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn computer_use_delegates(specs: &[(u32, &str)]) -> (String, usize) {
    let mut lines = String::new();
    for (version, kind) in specs {
        lines.push_str(&format!(
            "[delegate:{kind}:{version}] {{\"admission\":true}}\n"
        ));
    }
    (lines, specs.len())
}

// Permanent suite: 4 test fns, 9 independent behavior cases.
// Deferred to the ephemeral native probes (C15X11NativePrep fixture):
// typed-unavailable backend/display, unreachable display, unknown-session
// inject, cross-display independent leases, bogus observation id,
// sibling-session non-revoke, session close fencing, store-reopen
// persistence, dead-display follow-ups, lost-transport partial
// accounting, dispatch run_once wiring.
//
// Cases 1-3: real Xvfb — framebuffer/CAS observation; pointer-move
// postcondition; raw XTEST button delivery to a real client window with
// honest ACK-only Partial accounting.
#[tokio::test]
async fn computer_use_xvfb_observe_and_real_input() {
    let Some(x11) = x11_display_or_skip() else {
        eprintln!("SKIP computer_use_xvfb_observe_and_real_input: no usable X11 display");
        return;
    };
    use x11rb::{connection::Connection, protocol::xproto, protocol::xproto::ConnectionExt as _};
    let (delegate, _) = computer_use_delegates(&[
        (1, "computer.use.session"),
        (1, "computer.use.observe"),
        (1, "computer.use.lease"),
        (1, "computer.use.input"),
        (1, "computer.use.input"),
    ]);
    let (fixture, ids) = computer_use_fixture(5, &delegate);

    let opened = drive_native(
        &fixture,
        &fixture.handlers.session,
        ids[0],
        serde_json::json!({"backend":"x11","display":x11.display.as_str()}),
    )
    .await;
    assert_eq!(opened.state, OperationState::Resolved);
    let opened_n = op_normalized(&fixture, opened.operation_id);
    assert_eq!(opened_n["result"], "opened");
    let session_id: EntityId = opened_n["session"]["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(opened_n["session"]["xtest_version"].is_string());

    // The delivery target window must exist BEFORE the observation is
    // bound — creating it after observe would stale the fingerprint.
    let (client, screen_num) =
        x11rb::rust_connection::RustConnection::connect(Some(x11.display.as_str())).unwrap();
    let screen = &client.setup().roots[screen_num];
    let window = client.generate_id().unwrap();
    client
        .create_window(
            screen.root_depth,
            window,
            screen.root,
            10,
            10,
            120,
            80,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &xproto::CreateWindowAux::new()
                .event_mask(xproto::EventMask::BUTTON_PRESS | xproto::EventMask::EXPOSURE),
        )
        .unwrap()
        .check()
        .unwrap();
    client.map_window(window).unwrap().check().unwrap();
    client.flush().unwrap();

    let observed = drive_native(
        &fixture,
        &fixture.handlers.observe,
        ids[1],
        serde_json::json!({"session_id":session_id}),
    )
    .await;
    assert_eq!(observed.state, OperationState::Resolved);
    let observed_n = op_normalized(&fixture, observed.operation_id);
    assert_eq!(observed_n["result"], "observed");
    let observation_id = observed_n["observation_id"].as_str().unwrap().to_string();
    // The raw pixels are real CAS evidence: exact ZPixmap size bound.
    let raw_ref = &observed_n["observation"]["raw_pixels_ref"];
    let raw_digest = shenko_types::object::Digest(
        raw_ref["digest"].as_str().unwrap().to_string(),
    );
    let raw = fixture.store.cas.read_object(&raw_digest).unwrap();
    assert_eq!(
        raw.len() as u64,
        raw_ref["bytes"].as_u64().unwrap()
    );
    assert!(raw.len() > 800 * 600);

    let lease = drive_native(
        &fixture,
        &fixture.handlers.lease,
        ids[2],
        serde_json::json!({"session_id":session_id,"op":"acquire"}),
    )
    .await;
    let lease_n = op_normalized(&fixture, lease.operation_id);
    assert_eq!(lease_n["result"], "acquired");
    let token: EntityId = lease_n["lease"]["lease_token"].as_str().unwrap().parse().unwrap();

    let moved = drive_native(
        &fixture,
        &fixture.handlers.input,
        ids[3],
        serde_json::json!({
            "session_id":session_id,"lease_token":token,
            "observation_id":observation_id,
            "actions":[{"kind":"pointer_move","x":40,"y":40}],
        }),
    )
    .await;
    assert_eq!(moved.state, OperationState::Resolved);
    assert_eq!(moved.effect_status, EffectStatus::Complete);
    let moved_n = op_normalized(&fixture, moved.operation_id);
    assert_eq!(moved_n["pointer_post"], serde_json::json!([40, 40]));

    let clicked = drive_native(
        &fixture,
        &fixture.handlers.input,
        ids[4],
        serde_json::json!({
            "session_id":session_id,"lease_token":token,
            "observation_id":observation_id,
            "actions":[{"kind":"button","button":1,"press":"tap"}],
        }),
    )
    .await;
    assert_eq!(clicked.state, OperationState::Resolved);
    // Button ACK alone cannot claim an app effect: Partial, not Complete.
    assert_eq!(clicked.effect_status, EffectStatus::Partial);
    // The real client window actually received the press at 40x40.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let press = loop {
        match client.poll_for_event().unwrap() {
            Some(x11rb::protocol::Event::ButtonPress(e)) => break e,
            _ => {
                assert!(std::time::Instant::now() < deadline, "no real ButtonPress reached client");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    assert_eq!(press.detail, 1);
    assert_eq!((press.event_x, press.event_y), (30, 30));
}

// Cases 4-6: one display domain = one writer across ALL sessions; two
// sessions on the same Xvfb share the lease (a DIFFERENT controller
// instance cannot acquire), and explicit takeover bumps the epoch so a
// pre-takeover observation is fenced; the new holder must re-observe
// before injecting. The old-token rejection is the same revocation code
// path and is covered by the stale-observation case plus external prep.
#[tokio::test]
async fn computer_use_lease_exclusion_and_takeover() {
    let Some(x11) = x11_display_or_skip() else {
        eprintln!("SKIP computer_use_lease_exclusion_and_takeover: no usable X11 display");
        return;
    };
    let (delegate, _) = computer_use_delegates(&[
        (1, "computer.use.session"),
        (1, "computer.use.session"),
        (1, "computer.use.lease"),
        (1, "computer.use.lease"),
        (1, "computer.use.lease"),
        (1, "computer.use.observe"),
        (1, "computer.use.input"),
        (1, "computer.use.observe"),
        (1, "computer.use.input"),
    ]);
    let (fixture, ids) = computer_use_fixture(9, &delegate);
    let opened = drive_native(&fixture, &fixture.handlers.session, ids[0],
        serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
    let opened_n = op_normalized(&fixture, opened.operation_id);
    let session_id: EntityId = opened_n["session"]["session_id"].as_str().unwrap().parse().unwrap();
    let domain_id = opened_n["session"]["domain_id"].as_str().unwrap().to_string();

    // A second session on the SAME display binds the same domain.
    let opened_b = drive_native(&fixture, &fixture.handlers.session, ids[1],
        serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
    let opened_b_n = op_normalized(&fixture, opened_b.operation_id);
    let session_b: EntityId = opened_b_n["session"]["session_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(opened_b_n["session"]["domain_id"].as_str().unwrap(), domain_id.as_str(),
        "sessions on one display share the lease domain");

    let a = drive_native(&fixture, &fixture.handlers.lease, ids[2],
        serde_json::json!({"session_id":session_id,"op":"acquire"})).await;
    let a_n = op_normalized(&fixture, a.operation_id);

    // A DIFFERENT controller instance cannot become a writer on the same
    // domain, even through a separate admitted session.
    let conflict = drive_native_as(&fixture, EntityId::new(), &fixture.handlers.lease, ids[3],
        serde_json::json!({"session_id":session_b,"op":"acquire"})).await;
    let conflict_n = op_normalized(&fixture, conflict.operation_id);
    assert_eq!(conflict_n["result"], "rejected");
    assert_eq!(conflict_n["reason"], "lease_conflict");

    let observed = drive_native(&fixture, &fixture.handlers.observe, ids[5],
        serde_json::json!({"session_id":session_id})).await;
    let observation_id = op_normalized(&fixture, observed.operation_id)
        ["observation_id"].as_str().unwrap().to_string();

    // Human takeover: A's token becomes stale; late input is fenced out.
    let human = drive_native(&fixture, &fixture.handlers.lease, ids[4],
        serde_json::json!({"session_id":session_id,"op":"takeover"})).await;
    let human_n = op_normalized(&fixture, human.operation_id);
    assert_eq!(human_n["result"], "taken_over");
    let token_h: EntityId = human_n["lease"]["lease_token"].as_str().unwrap().parse().unwrap();
    assert!(human_n["lease"]["epoch"].as_u64().unwrap() > a_n["lease"]["epoch"].as_u64().unwrap());

    // An observation bound to the PRE-takeover epoch is stale too: the
    // holder must re-observe under the new epoch before injecting.
    let pre_epoch = drive_native(&fixture, &fixture.handlers.input, ids[6],
        serde_json::json!({"session_id":session_id,"lease_token":token_h,
            "observation_id":observation_id,
            "actions":[{"kind":"pointer_move","x":7,"y":7}]})).await;
    assert_eq!(
        op_normalized(&fixture, pre_epoch.operation_id)["reason"],
        "observation_stale_epoch"
    );
    let observed2 = drive_native(&fixture, &fixture.handlers.observe, ids[7],
        serde_json::json!({"session_id":session_id})).await;
    let observation_id2 = op_normalized(&fixture, observed2.operation_id)
        ["observation_id"].as_str().unwrap().to_string();
    let fresh = drive_native(&fixture, &fixture.handlers.input, ids[8],
        serde_json::json!({"session_id":session_id,"lease_token":token_h,
            "observation_id":observation_id2,
            "actions":[{"kind":"pointer_move","x":7,"y":7}]})).await;
    assert_eq!(fresh.state, OperationState::Resolved);
}

// Case 7: stale observation is rejected before any event; a changed
// window layout invalidates the bound coordinates.
#[tokio::test]
async fn computer_use_stale_observation_rejected() {
    let Some(x11) = x11_display_or_skip() else {
        eprintln!("SKIP computer_use_stale_observation_rejected: no usable X11 display");
        return;
    };
    use x11rb::{connection::Connection, protocol::xproto, protocol::xproto::ConnectionExt as _};
    let (delegate, _) = computer_use_delegates(&[
        (1, "computer.use.session"),
        (1, "computer.use.observe"),
        (1, "computer.use.lease"),
        (1, "computer.use.input"),
    ]);
    let (fixture, ids) = computer_use_fixture(4, &delegate);
    let opened = drive_native(&fixture, &fixture.handlers.session, ids[0],
        serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
    let session_id: EntityId = op_normalized(&fixture, opened.operation_id)
        ["session"]["session_id"].as_str().unwrap().parse().unwrap();
    let observed = drive_native(&fixture, &fixture.handlers.observe, ids[1],
        serde_json::json!({"session_id":session_id})).await;
    let observation_id = op_normalized(&fixture, observed.operation_id)
        ["observation_id"].as_str().unwrap().to_string();
    let lease = drive_native(&fixture, &fixture.handlers.lease, ids[2],
        serde_json::json!({"session_id":session_id,"op":"acquire"})).await;
    let token: EntityId = op_normalized(&fixture, lease.operation_id)
        ["lease"]["lease_token"].as_str().unwrap().parse().unwrap();

    // Change the real window layout after the observation was bound.
    let (client, screen_num) =
        x11rb::rust_connection::RustConnection::connect(Some(x11.display.as_str())).unwrap();
    let screen = &client.setup().roots[screen_num];
    let window = client.generate_id().unwrap();
    client
        .create_window(
            screen.root_depth, window, screen.root, 0, 0, 100, 100, 0,
            xproto::WindowClass::INPUT_OUTPUT, screen.root_visual,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap().check().unwrap();
    client.map_window(window).unwrap().check().unwrap();
    client.flush().unwrap();

    let stale = drive_native(&fixture, &fixture.handlers.input, ids[3],
        serde_json::json!({"session_id":session_id,"lease_token":token,
            "observation_id":observation_id,
            "actions":[{"kind":"pointer_move","x":5,"y":5}]})).await;
    let stale_n = op_normalized(&fixture, stale.operation_id);
    assert_eq!(stale_n["result"], "rejected");
    assert_eq!(stale_n["reason"], "observation_stale_layout");
    assert_eq!(stale.effect_status, EffectStatus::None);
}

// Cases 8-9: (a) an armed input op whose controller produced no
// conclusive evidence reconciles to honest Unknown with no retry;
// (b) after REAL display loss (our own spawned server killed and reaped)
// follow-up input is rejected by re-verification, never sent blind.
// Skipped when the display is borrowed — we only kill servers we own.
#[tokio::test]
async fn computer_use_armed_reconcile_and_dead_display_fencing() {
    let Some(mut x11) = x11_display_or_skip() else {
        eprintln!("SKIP computer_use_armed_reconcile_and_dead_display_fencing: no usable X11");
        return;
    };
    let (delegate, _) = computer_use_delegates(&[
        (1, "computer.use.session"),
        (1, "computer.use.observe"),
        (1, "computer.use.lease"),
        (1, "computer.use.input"),
        (1, "computer.use.input"),
    ]);
    let (fixture, ids) = computer_use_fixture(5, &delegate);
    let opened = drive_native(&fixture, &fixture.handlers.session, ids[0],
        serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
    let session_id: EntityId = op_normalized(&fixture, opened.operation_id)
        ["session"]["session_id"].as_str().unwrap().parse().unwrap();
    let observed = drive_native(&fixture, &fixture.handlers.observe, ids[1],
        serde_json::json!({"session_id":session_id})).await;
    let observation_id = op_normalized(&fixture, observed.operation_id)
        ["observation_id"].as_str().unwrap().to_string();
    let lease = drive_native(&fixture, &fixture.handlers.lease, ids[2],
        serde_json::json!({"session_id":session_id,"op":"acquire"})).await;
    let token: EntityId = op_normalized(&fixture, lease.operation_id)
        ["lease"]["lease_token"].as_str().unwrap().parse().unwrap();

    // An armed input attempt whose controller never produced a result:
    // reconcile reports Unknown with honest progress, arms no retry.
    let input_desc = shenko_kernel::task::NativeTaskHandler::descriptor(
        &fixture.handlers.input,
    ).unwrap();
    let claim = fixture
        .store
        .claim_native_task(ids[3], input_desc.implementation_ref.clone(), false)
        .unwrap()
        .unwrap();
    let (armed_op, _armed_attempt) = arm_simulated_native_operation(
        &fixture.store,
        &claim,
        input_desc.implementation_ref.clone(),
        "computer.use.input",
    )
    .unwrap();
    let task = fixture.store.external_task(ids[3]).unwrap();
    let reconciled = shenko_kernel::task::NativeTaskHandler::reconcile(
        &fixture.handlers.input,
        &task,
        armed_op.operation_id,
    )
    .await
    .unwrap();
    assert_eq!(reconciled.effect_status, EffectStatus::Unknown);
    assert_eq!(reconciled.state, OperationState::Reconciling);
    assert_eq!(
        fixture
            .store
            .external_operation_attempts(armed_op.operation_id)
            .unwrap()
            .len(),
        1
    );
    // Display lost mid-work: only a server WE spawned is terminated —
    // a borrowed SHENKO_TEST_X11_DISPLAY is never killed, and the
    // dead-display case is then reported as skipped rather than faked.
    let Some(xvfb) = x11.owned.take() else {
        eprintln!("SKIP dead-display fencing: display not owned by this test");
        return;
    };
    xvfb.kill_and_wait();
    let late = drive_native(&fixture, &fixture.handlers.input, ids[4],
        serde_json::json!({"session_id":session_id,"lease_token":token,
            "observation_id":observation_id,
            "actions":[{"kind":"pointer_move","x":9,"y":9},
                       {"kind":"button","button":1,"press":"tap"}]})).await;
    let late_n = op_normalized(&fixture, late.operation_id);
    assert_eq!(late_n["result"], "rejected");
    assert_eq!(late_n["sent_events"].as_u64().unwrap_or(0), 0);
    assert_eq!(late.effect_status, EffectStatus::None);
}

/// adoption retires the node's independent self while its
/// retained experience survives as a recorded branch — but the node
/// identity and the store's derived subject binding are one installation
/// fact. An adoption that cannot move them together must refuse before
/// the node is touched, because succeeding would leave every later
/// `open_node` failing on `Node/subject branch differs`. Adopting the
/// bound identity back is the path that re-converges an already diverged
/// installation.
#[tokio::test]
async fn sync_adopt_keeps_subject_binding_coherent() {
    let dir = std::env::temp_dir().join(format!("shenko-adopt-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: dir.clone(),
        ..Default::default()
    })
    .prepare(store.ledger.ledger_id())
    .unwrap();
    let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
    drop(home);
    let bound = node_sync::open_node(&dir, &store)
        .unwrap()
        .local_identity()
        .unwrap()
        .expect("initialized node carries the bound subject identity");
    assert_eq!(bound.agent_id, agent);

    // An adoption the binding cannot follow is refused BEFORE the node
    // records anything: the command reports no success.
    let foreign = shenko_node::Identity {
        agent_id: EntityId::new(),
        branch_id: EntityId::new(),
        origin_proof: shenko_types::object::Digest::blake3(b"trusted peer genesis"),
    };
    let foreign_file = dir.join("foreign-identity.json");
    std::fs::write(&foreign_file, serde_json::to_vec(&foreign).unwrap()).unwrap();
    let refusal = node_sync::run(
        node_sync::SyncCommand::Adopt {
            identity_file: foreign_file,
            trusted_node: EntityId::new(),
            provenance: "test-fixture: operator-provided file".into(),
            reason: "join the peer's existing self".into(),
        },
        dir.clone(),
    )
    .await
    .expect_err("adoption must refuse when the bound subject cannot transition");
    assert!(
        refusal.to_string().contains("sync adopt refused"),
        "unexpected refusal message: {refusal:#}"
    );
    // The refused adoption left the installation exactly as it was:
    // startup still opens, the bound identity is intact, no adoption
    // was recorded.
    let node = node_sync::open_node(&dir, &store).unwrap();
    assert_eq!(node.local_identity().unwrap(), Some(bound.clone()));
    assert!(node
        .branches()
        .unwrap()
        .iter()
        .all(|branch| branch.branch_id != foreign.branch_id));
    drop(node);

    // The historical defect state — node identity adopted while the
    // binding stayed behind — is reproduced at the node level so the
    // command path can prove recovery. The next open sees the real
    // divergence and refuses, as before.
    {
        let mut node = node_sync::open_node(&dir, &store).unwrap();
        node.adopt_identity(shenko_node::AdoptRequest {
            operation_id: EntityId::new(),
            trusted_identity: foreign.clone(),
            trusted_node_id: EntityId::new(),
            provenance: "test-fixture: operator-provided file".into(),
            reason: "reproduce the diverged binding defect".into(),
        })
        .unwrap();
    }
    let divergence = node_sync::open_node(&dir, &store)
        .err()
        .expect("diverged node/subject pair must still be refused");
    assert!(divergence.to_string().contains("Node/subject branch differs"));
    // The startup observation path is equally blocked before recovery.
    assert!(node_sync::observe(&dir, &store, EntityId::new(), None).is_err());

    // Adopting the bound identity back through the real command
    // re-converges the pair and records the retired self's experience.
    let bound_file = dir.join("bound-identity.json");
    std::fs::write(&bound_file, serde_json::to_vec(&bound).unwrap()).unwrap();
    node_sync::run(
        node_sync::SyncCommand::Adopt {
            identity_file: bound_file,
            trusted_node: EntityId::new(),
            provenance: "test-fixture: installation binding projection".into(),
            reason: "re-converge the diverged node/subject pair".into(),
        },
        dir.clone(),
    )
    .await
    .unwrap();
    let node = node_sync::open_node(&dir, &store).unwrap();
    assert_eq!(node.local_identity().unwrap(), Some(bound.clone()));
    // Both retired selves stay auditable: nothing was deleted.
    let retired: Vec<EntityId> = node
        .branches()
        .unwrap()
        .into_iter()
        .filter_map(|branch| {
            (branch.kind == shenko_node::BranchKind::AdoptedExperience)
                .then_some(branch.branch_id)
        })
        .collect();
    assert!(retired.contains(&bound.branch_id) && retired.contains(&foreign.branch_id));
    // Startup and role observation work again on the recovered pair.
    drop(node);
    node_sync::observe(&dir, &store, EntityId::new(), None).unwrap();
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

/// With no local subject bound, node-level adoption is the only
/// consistent state `open_node` can check: it proceeds and the node
/// carries the adopted self until the subject's own facts arrive.
#[tokio::test]
async fn sync_adopt_without_bound_subject_stays_coherent() {
    let dir = std::env::temp_dir().join(format!("shenko-adopt-unbound-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(open_native_fixture(&dir));
    assert!(store.installation_binding().unwrap().subject.is_none());
    let trusted = shenko_node::Identity {
        agent_id: EntityId::new(),
        branch_id: EntityId::new(),
        origin_proof: shenko_types::object::Digest::blake3(b"trusted peer genesis"),
    };
    let identity_file = dir.join("trusted-identity.json");
    std::fs::write(&identity_file, serde_json::to_vec(&trusted).unwrap()).unwrap();
    node_sync::run(
        node_sync::SyncCommand::Adopt {
            identity_file,
            trusted_node: EntityId::new(),
            provenance: "test-fixture: operator-provided file".into(),
            reason: "carry the peer's self until its facts arrive".into(),
        },
        dir.clone(),
    )
    .await
    .unwrap();
    // Every later open stays consistent: still no bound subject, so the
    // binding gate never compares identities.
    let node = node_sync::open_node(&dir, &store).unwrap();
    assert_eq!(node.local_identity().unwrap(), Some(trusted));
    drop(node);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
