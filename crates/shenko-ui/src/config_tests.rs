//! Consumer-visible configuration behaviours, R133, CFG02.
//!
//! Each case is about a different fact rather than a different function:
//! (1) a repeated intent resolves the original change and applies once;
//! (2) a different payload under the same origin+key is refused;
//! (3) a stale base is refused without overwriting the newer value;
//! (4) the built-in namespace's value is live for its consumers and survives a
//!     reopen, and a rejected apply leaves the running value untouched;
//! (5) `config.manage` executes through the real native task/operation/
//!     evidence ledger and its change is the same durable row the service
//!     reads.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fs, sync::Arc};

use anyhow::Context;
use parking_lot::RwLock;
use serde_json::{Value, json};
use uuid::Uuid;

use shenko_kernel::{FiberHandle, Supervisor};
use shenko_store::child::NATIVE_INPUT;
use shenko_subject::{
    NativeChildPlugin, NativeChildService, NativeTaskHandlerPlugin,
};
use shenko_kernel::task::NativeTaskHandler;
use shenko_types::inbox::{InboxEvent, InboxEventKind};
use shenko_types::{
    commit::CommitRef,
    ids::*,
    object::*,
    operation::OperationState,
    store::RawInboxEvent,
    subject::*,
    task::{DispatchEligibility, TaskOwner, TaskRecord, TaskState},
};

use super::*;
use crate::config_namespaces::{DEFAULT_PAGE_LIMIT, PAGE_LIMIT_KEY, UI_NAMESPACE};
use crate::config_task::ConfigHandler;

/// One namespace whose apply is counted, so "applied once" is observable
/// instead of inferred from a row revision.
struct CountingNamespace {
    applies: AtomicUsize,
    value: RwLock<Value>,
}

impl CountingNamespace {
    fn new(initial: Value) -> Self {
        Self {
            applies: AtomicUsize::new(0),
            value: RwLock::new(initial),
        }
    }

    fn applies(&self) -> usize {
        self.applies.load(Ordering::SeqCst)
    }
}

impl ConfigNamespaceHandler for CountingNamespace {
    fn descriptor(&self) -> ConfigNamespace {
        ConfigNamespace {
            namespace: "test.counting".into(),
            schema_ref: SchemaRef {
                name: "test.counting/v1".into(),
                major: 1,
                digest: Digest::blake3(b"test.counting/v1"),
            },
            owner_plugin: builtin_contract("test.counting", b"test.counting/v1"),
            read_contract_ref: builtin_contract("test.counting.read", b"test.counting/v1"),
            apply_contract_ref: builtin_contract("test.counting.apply", b"test.counting/v1"),
            capability_refs: Vec::new(),
            application: ConfigApplication::HotReload,
            effect_contract_ref: builtin_contract("test.counting.effect", b"test.counting/v1"),
            logical_keys: vec!["value".into()],
            description: "counting fixture namespace".into(),
        }
    }

    fn read(&self) -> Result<Value> {
        Ok(self.value.read().clone())
    }

    fn consumers(&self) -> Vec<String> {
        vec!["test.consumer".into()]
    }

    fn apply(&self, change: &shenko_store::config::ConfigChange, desired: &Value) -> Result<ConfigApplyOutcome> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        *self.value.write() = desired.clone();
        Ok(ConfigApplyOutcome {
            adopted: vec![shenko_store::config::ConfigAdoption {
                consumer_id: "test.consumer".into(),
                adopted_revision: change.desired_revision,
                adopted_at_commit: change.updated_commit,
                detail: "test consumer reads the live value".into(),
            }],
            observation_refs: Vec::new(),
            detail: "applied".into(),
        })
    }
}

fn request(
    logical_key: &str,
    origin: &str,
    desired: Value,
    expected_base_revision: Option<u64>,
) -> ConfigApplyRequest {
    ConfigApplyRequest {
        namespace: "test.counting".into(),
        logical_key: logical_key.into(),
        origin_identity: origin.into(),
        actor: ConfigActor::Subject {
            task_id: EntityId::new(),
            delegation_ref: None,
        },
        desired,
        expected_base_revision,
        coordinating_task_id: None,
    }
}

fn store_fixture(label: &str) -> (PathBuf, Arc<Store>) {
    let root = std::env::temp_dir().join(format!(
        "shenko-config-{label}-{}",
        Uuid::new_v4()
    ));
    fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas")).unwrap());
    (root, store)
}

/// A store with a genesis subject, one planned step and the subject dispatcher
/// running, so a native task has a real origin and a real claim path.
fn subject_fixture(label: &str) -> (PathBuf, Arc<Store>, EntityId, EntityId, EntityId) {
    let (root, store) = store_fixture(label);
    let agent = EntityId::new();
    let branch = BranchId::new();
    let first = EntityId::new();
    let root_task_id = EntityId::new();
    let genesis_id = EntityId::new();
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let object = store
        .cas
        .seal_object(b"seed", "application/json", "fixture", "seed", 1)
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
        created_commit: zero,
    };
    let head = SubjectBranchHead { branch_origin: None,
        agent_id: agent,
        branch_id: branch.clone(),
        subject_root_task_id: root_task_id,
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef {
            ledger_id: store.ledger.ledger_id(),
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: zero,
        next_step_id: first,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let step = SubjectStep {
        step_id: first,
        branch_id: branch,
        step_no: 1,
        expected_parent_commit: zero,
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
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
    let root_task =
        TaskRecord::subject_root(root_task_id, agent, genesis.seed_manifest_ref.clone(), zero);
    store
        .initialize_genesis(&genesis, &head, &step, &root_task)
        .unwrap();
    store.start_subject_dispatcher().unwrap();
    (root, store, agent, first, root_task_id)
}

/// (1) A repeated intent resolves the original change, applies once, and does
/// so before the current-revision check — an already-superseded expected base
/// does not turn a replay into BASE_CONFIG_STALE.
#[test]
fn repeated_intent_applies_once_and_precedes_revision_check() {
    let (_root, store) = store_fixture("replay");
    let service = ConfigService::new(Arc::clone(&store));
    let handler = Arc::new(CountingNamespace::new(json!(0)));
    service.register(handler.clone());

    let first = service
        .apply(request("value", "intent-1", json!(11), Some(0)))
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(first.change.state, "applied");
    assert_eq!(handler.applies(), 1);
    assert_eq!(first.change.adopted.len(), 1);
    assert_eq!(first.change.adopted[0].adopted_revision, 1);

    // Same origin, same bytes, now-stale expected base: the durable intent
    // answers, and no second apply runs.
    let replay = service
        .apply(request("value", "intent-1", json!(11), Some(0)))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.change.change_id, first.change.change_id);
    assert_eq!(
        replay.change.receipt.as_ref().map(|receipt| receipt.receipt_id),
        first.change.receipt.as_ref().map(|receipt| receipt.receipt_id)
    );
    assert_eq!(handler.applies(), 1, "a replay must not apply a second effect");
    assert_eq!(service.adoption_state("test.counting", "value").unwrap().len(), 1);
    assert_eq!(service.read("test.counting", "value").unwrap().entry.unwrap().desired_revision, 1);
}

/// (2) The same origin+key with different desired bytes is a conflict, never a
/// silent overwrite through a reused identifier.
#[test]
fn different_payload_under_same_intent_is_refused() {
    let (_root, store) = store_fixture("conflict");
    let service = ConfigService::new(Arc::clone(&store));
    let handler = Arc::new(CountingNamespace::new(json!(0)));
    service.register(handler.clone());

    service
        .apply(request("value", "intent-1", json!(11), None))
        .unwrap();
    let conflict = service
        .apply(request("value", "intent-1", json!(12), None))
        .unwrap_err();
    assert_eq!(conflict.code(), "CONFIG_LOGICAL_KEY_REUSED");
    assert_eq!(conflict.apply_effect_status(), EffectStatus::None);
    assert_eq!(handler.applies(), 1);
    let entry = service.read("test.counting", "value").unwrap().entry.unwrap();
    assert_eq!(entry.desired_revision, 1, "the saved value is not overwritten");
}

/// (3) A caller whose expected base revision is stale is refused, and the
/// newer value stays exactly as it was.
#[test]
fn stale_base_is_refused_without_overwrite() {
    let (_root, store) = store_fixture("stale");
    let service = ConfigService::new(Arc::clone(&store));
    let handler = Arc::new(CountingNamespace::new(json!(0)));
    service.register(handler.clone());

    service
        .apply(request("value", "intent-1", json!(11), Some(0)))
        .unwrap();
    let stale = service
        .apply(request("value", "intent-2", json!(12), Some(0)))
        .unwrap_err();
    assert_eq!(stale.code(), "BASE_CONFIG_STALE");
    assert_eq!(stale.apply_effect_status(), EffectStatus::None);
    assert_eq!(handler.applies(), 1);
    let view = service.read("test.counting", "value").unwrap();
    assert_eq!(view.current_value, Some(json!(11)));
    assert_eq!(view.entry.unwrap().desired_revision, 1);
}

/// (4) The built-in `shenko.ui/page_limit` key is a live consumer setting: it
/// is discoverable, applied without any approval step, observable by the code
/// that uses it, recorded as two consumer adoptions, and restored from the
/// durable entry on reopen. A refused value never overwrites the running one,
/// and a stored value that cannot be trusted is reported as unusable instead
/// of being presented as the compiled default the consumer now runs.
#[test]
fn builtin_page_limit_is_live_adopted_and_survives_reopen() {
    let (_root, store) = store_fixture("page-limit");
    let service = Arc::new(ConfigService::new(Arc::clone(&store)));
    let discovered = service.discover();
    let ui = discovered
        .iter()
        .find(|namespace| namespace.namespace == UI_NAMESPACE)
        .expect("the web slice registers its own namespace");
    assert_eq!(ui.logical_keys, vec![PAGE_LIMIT_KEY.to_string()]);
    assert_eq!(ui.application, ConfigApplication::HotReload);

    assert_eq!(
        crate::config_namespaces::page_limit(Some(&service)),
        DEFAULT_PAGE_LIMIT
    );

    let applied = service
        .apply(ConfigApplyRequest {
            namespace: UI_NAMESPACE.into(),
            logical_key: PAGE_LIMIT_KEY.into(),
            origin_identity: "panel-intent-1".into(),
            actor: ConfigActor::Subject {
                task_id: EntityId::new(),
                delegation_ref: None,
            },
            desired: json!(17),
            expected_base_revision: None,
            coordinating_task_id: None,
        })
        .unwrap();
    assert_eq!(applied.change.state, "applied");
    assert_eq!(crate::config_namespaces::page_limit(Some(&service)), 17);
    let adoptions = service.adoption_state(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap();
    assert_eq!(adoptions.len(), 2, "both listing consumers adopted revision 1");
    assert!(adoptions.iter().all(|adoption| adoption.adopted_revision == 1));

    // Reopen against the same store: the saved revision is what the consumer
    // runs, so a restart does not silently fall back to the compiled default.
    let reopened = Arc::new(ConfigService::new(Arc::clone(&store)));
    assert_eq!(crate::config_namespaces::page_limit(Some(&reopened)), 17);
    assert_eq!(
        reopened.read(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap().current_value,
        Some(json!(17))
    );
    assert_eq!(
        reopened.read(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap().desired_value,
        Some(json!(17))
    );

    // A value outside the published range is not coerced: the intent is saved
    // (that is the durable fact) and its receipt says the apply failed, while
    // the consumer keeps running the value it actually adopted.
    let refused = reopened
        .apply(ConfigApplyRequest {
            namespace: UI_NAMESPACE.into(),
            logical_key: PAGE_LIMIT_KEY.into(),
            origin_identity: "panel-intent-2".into(),
            actor: ConfigActor::Subject {
                task_id: EntityId::new(),
                delegation_ref: None,
            },
            desired: json!(0),
            expected_base_revision: Some(1),
            coordinating_task_id: None,
        })
        .unwrap();
    assert_eq!(refused.change.state, "failed");
    assert_eq!(refused.effect_status(), EffectStatus::None);
    assert_eq!(
        refused.change.receipt.as_ref().map(|receipt| receipt.outcome.as_str()),
        Some("rejected")
    );
    assert_eq!(crate::config_namespaces::page_limit(Some(&reopened)), 17);

    // A fresh process sees a saved revision it cannot trust. It uses the
    // compiled default and says so; it never reports that default as the
    // value the installation asked for.
    let restarted = Arc::new(ConfigService::new(Arc::clone(&store)));
    assert_eq!(
        crate::config_namespaces::page_limit(Some(&restarted)),
        DEFAULT_PAGE_LIMIT
    );
    let unusable = restarted.read(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap();
    assert_eq!(unusable.desired_value, Some(json!(0)));
    assert!(unusable.current_value.is_none());
    assert!(
        unusable
            .current_value_error
            .unwrap_or_default()
            .contains("not usable"),
        "the consumer must state why it cannot report its value"
    );
}

/// (5) The conversational entry point is a real native task: it arms T1,
/// observes the apply, records E1 and leaves a T2 result, and the change it
/// produced is the same durable row the panel reads.
#[tokio::test]
async fn config_manage_applies_through_the_native_ledger() {
    let (root, store, agent, step_id, root_task_id) = subject_fixture("tool");
    let service = Arc::new(ConfigService::new(Arc::clone(&store)));
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let _child: FiberHandle = supervisor
        .spawn(
            NativeChildPlugin,
            (Arc::clone(&store), RuntimeGeneration::default()),
        )
        .await
        .unwrap();
    let handler: Arc<dyn NativeTaskHandler> = Arc::new(ConfigHandler::new(
        Arc::clone(&store),
        Arc::clone(&service),
    ));
    let implementation = handler.descriptor().unwrap().implementation_ref.clone();
    let _handler: FiberHandle = supervisor
        .spawn(NativeTaskHandlerPlugin, handler)
        .await
        .unwrap();
    let native = supervisor
        .context()
        .try_service::<NativeChildService>()
        .unwrap();
    native.recover().unwrap();

    let input = json!({
        "op": "apply",
        "namespace": UI_NAMESPACE,
        "logical_key": PAGE_LIMIT_KEY,
        "desired": 23,
    });
    let input_ref = store
        .cas
        .seal_object(
            &serde_json::to_vec(&input).unwrap(),
            "application/json",
            "fixture",
            "config-task-input",
            1,
        )
        .unwrap()
        .object;
    let task_id = EntityId::new();
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id: agent,
        conversation_id: agent,
        owner: TaskOwner::Task { id: root_task_id },
        background: false,
        kind: "config.manage".into(),
        definition_version: 1,
        implementation_ref: implementation,
        input_ref,
        state: TaskState::Pending {
            checkpoint_ref: None,
        },
        abort_request: None,
        memos: serde_json::Map::from_iter([
            ("intent_id".into(), json!(task_id)),
            (NATIVE_INPUT.into(), json!("object")),
        ]),
        residual_case_ids: vec![],
        created_by_step_id: Some(step_id),
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: zero,
        updated_commit: zero,
    };
    let event = InboxEvent {
        event_id: crate::playground::stable_id(&format!("config-accepted/{task_id}")),
        kind: InboxEventKind::SystemEvent,
        created_at: chrono::Utc::now(),
        source: "config".into(),
        payload: json!({"task_id": task_id, "kind": "config.manage"}),
    };
    let raw = RawInboxEvent {
        event_id: event.event_id,
        agent_id: agent,
        source: event.source.clone(),
        kind: "system_event".into(),
        payload_json: serde_json::to_string(&event).unwrap(),
        received_at_ms: event.created_at.timestamp_millis(),
    };
    store.admit_native_task(&task, &raw).unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            native.run_once().await.unwrap();
            if store.native_task_result(task_id).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("config.manage settles its native result");

    let result = store.native_task_result(task_id).unwrap().unwrap();
    assert!(
        matches!(result.outcome, shenko_types::task::TaskOutcome::Succeeded),
        "unexpected outcome: {:?}",
        result.outcome
    );
    // The change the task made is the one the service and the panel read.
    let view = service.read(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap();
    assert_eq!(view.entry.unwrap().desired_revision, 1);
    assert_eq!(view.current_value, Some(json!(23)));
    assert_eq!(crate::config_namespaces::page_limit(Some(&service)), 23);
    assert_eq!(service.adoption_state(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap().len(), 2);
    // A real operation/attempt/evidence chain exists for the same task.
    let settled_task = store.get_task(task_id).unwrap().unwrap();
    let operation = store
        .external_operation(
            shenko_store::child::memo_uuid(
                &settled_task,
                shenko_store::child::NATIVE_OPERATION_ID,
            )
            .expect("the runtime binds the operation id to the task"),
        )
        .unwrap();
    assert_eq!(operation.operation_name, "config.manage.v1");
    assert_eq!(operation.state, OperationState::Resolved);
    drop(root);
}

/// The input and configuration surfaces share the installation's Store. Use
/// the existing genuine genesis fixture, then admit, seal and commit through
/// its normal mutation API; a reopened reader must recover the same pairing.
#[test]
fn input_history_recovers_committed_selection_from_sealed_s0_and_s1() -> anyhow::Result<()> {
    use crate::input::{SubmissionMode, SubmissionStage, UserSubmission, WebInputService};
    use crate::presentation::{PresentationService, SurfaceCapabilityProvider};
    let (root, store, agent, _, _) = subject_fixture("owner-history");
    let presentation = || PresentationService::new(8,
        SurfaceCapabilityProvider::new(false, "native-receipt-fixture".into()));
    let input = WebInputService::new(Arc::clone(&store), agent, None, presentation());
    let message = UserSubmission {
        schema_version: 2, kind: "submit".into(), submission_id: "committed-history".into(),
        message_id: "original-owner-message".into(), subject_id: agent.to_string(),
        mode: SubmissionMode::Append, content: "原始持久输入".into(), attachment_refs: vec![],
        rich_content_ref: None, observed_step_id: None, observed_request_id: None,
    };
    assert!(matches!(input.submit(&message).stage, SubmissionStage::Accepted));
    let record = store.web_submission(agent, &message.submission_id)?
        .context("accepted submission has no durable record")?;
    let original: Value = serde_json::from_str(&store.pending_inbox_events(agent, 8)?
        .into_iter().find(|(id, _)| *id == record.event_id)
        .context("accepted submission has no original inbox event")?.1)?;
    let (head, mut step) = store.load_subject(agent)?;
    let scope = EntityId(head.branch_id.0);
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(Arc::clone(&store), scope)));
    let profile = Digest::blake3(b"native-input-history-fixture");
    ledger.rebind_model(1, &profile.0)?;
    let window = ledger.current()?;
    let staged = store.stage_subject_inputs(&head, &step, &[
        shenko_store::raw_session::SubjectHistoryInput {
            event_id: record.event_id, event_type: "user".into(),
            data: json!({"v":1, "inbox_event":original, "text":message.content}),
        },
    ])?;
    let request = EntityId::new();
    let plan = json!({
        "schema_version":12, "reconciliation_docket":[], "step_id":step.step_id,
        "step_no":step.step_no, "events":[original], "request_id":request,
        "provider_selection":{"schema_version":1,"binding":null,"model":null,"profile_id":null,
            "profile_revision":null,"credential_fingerprint":null,"resource_domain_id":null,"serializer_revision":null},
        "provider_binding":null, "semantic_wait":null, "identity":null,
        "identity_provider_revision":"shenko.identity/default@1",
        "context":{"source":{"scope_id":scope, "source_commit_seq":store.current_commit_seq()?,
            "leaf_seq":store.session_leaf_seq(scope)?,
            "input_raw_event_ids":[staged[0].event_id], "window_id":window.id,
            "window_generation":window.generation, "model_generation":1},
            "profile_digest":profile, "estimated_input_tokens":null, "budget_input_tokens":null,
            "trigger_budget_input_tokens":null, "budget_provenance":"native receipt fixture without provider",
            "reserved_output_tokens":64, "unfinished_call_ids":[], "capabilities":null,
            "observed_input_tokens":null, "token_counter":"heuristic:cjk-1-other-4@1; not a provider tokenizer",
            "policy_contract":shenko_context_policy::CONTRACT_VERSION,
            "selection":{"outcome":"unavailable","reason":"native_fixture_without_provider"}},
        "payload":{"system_prompt":"native receipt fixture", "messages":[{"role":"user","content":message.content}],
            "max_tokens":64,"tools":[]},
    });
    step.input_plan_ref = Some(store.cas.seal_object(&serde_json::to_vec(&plan)?,
        "application/json", "input_plans", "subject_step_input", 12)?.object);
    step.state = SubjectStepState::Preparing;
    step.record_revision = RecordRevision(1);
    store.upsert_step(&step, &head, RecordRevision(0))?;
    let selected = input.get(&message.submission_id, &message.subject_id, None);
    assert_eq!(selected.stage, Some("selected"));
    assert_eq!(selected.selected_step_id, Some(step.step_id.to_string()));
    step.state = SubjectStepState::AwaitingModel;
    step.record_revision = RecordRevision(2);
    step.active_request_id = Some(request);
    store.upsert_step(&step, &head, RecordRevision(1))?;
    step.model_result_ref = Some(store.cas.seal_object("持久的处理结果".as_bytes(),
        "text/plain", "native-fixture", "model-result", 1)?.object);
    let outcome = SubjectModelOutcome {
        schema_version: 2, request_id: request, result_ref: step.model_result_ref.clone().context("fixture model result is not sealed")?,
        stop_reason: "end_turn".into(), raw_response: r#"{"fixture":"native receipt transition"}"#.into(), usage: None,
    };
    step.model_outcome_ref = Some(store.cas.seal_object(&serde_json::to_vec(&outcome)?,
        "application/json", "model_outcomes", "SubjectModelOutcome", 2)?.object);
    step.state = SubjectStepState::ResultRecorded;
    step.record_revision = RecordRevision(3);
    store.upsert_step(&step, &head, RecordRevision(2))?;
    let next = EntityId::new();
    let decision = StepDecision {
        schema_version: 8,
        output: shenko_types::output::ParsedOutput { schema_version: 5, step_id: step.step_id,
            raw_ref: step.model_result_ref.clone().context("fixture model result is not sealed")?, text: "持久的处理结果".into(), controls: vec![] },
        identity_intent: None, step_id: step.step_id, input_plan_ref: step.input_plan_ref.clone().context("fixture S0 input plan is not sealed")?,
        model_result_ref: step.model_result_ref.clone(), applied_steering_refs: vec![],
        event_dispositions: vec![EventDisposition { event_id: record.event_id,
            disposition: Disposition::Acknowledged, reason: None, priority: None, review_after: None }],
        delegated_task_intents: vec![], child_dialogue_intents: vec![], memory_intents: vec![],
        policy_intents: vec![], publication_intents: vec![], pending_obligations: vec![],
        attention_policy: None, evidence_refs: vec![], unknown_acknowledgements: vec![], successor_step_id: next,
    };
    step.decision_ref = Some(store.cas.seal_object(&serde_json::to_vec(&decision)?,
        "application/json", "native-fixture", "StepDecision", 8)?.object);
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(4);
    store.commit_step_and_advance_head(&step, &head, next)?;
    let committed = store.load_subject(agent)?.0.committed_at;
    store.close_external_writer();
    drop(input);
    drop(ledger);
    drop(store);
    let reopened = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let reader = WebInputService::new(Arc::clone(&reopened), agent, None, presentation());
    let view = reader.get(&message.submission_id, &message.subject_id, None);
    assert_eq!(view.result, "found");
    assert_eq!(view.stage, Some("committed"));
    assert_eq!(view.selected_step_id.as_deref(), Some(step.step_id.to_string().as_str()));
    assert_eq!(view.committed_at.as_deref(), Some(committed.to_string().as_str()));
    let history = WebInputService::history(&reopened, agent, None, 8)?;
    assert_eq!(history.total, 1);
    assert_eq!(history.items[0].payload["content"], message.content);
    assert_eq!(history.items[0].view.selected_step_id, view.selected_step_id);
    assert_eq!(history.items[0].view.committed_at, view.committed_at);
    Ok(())
}

/// The handler refuses an input that names no key before any durable effect.
#[tokio::test]
async fn config_manage_rejects_invalid_input_without_a_change() {
    let (root, store, agent, step_id, root_task_id) = subject_fixture("tool-invalid");
    let service = Arc::new(ConfigService::new(Arc::clone(&store)));
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let _child: FiberHandle = supervisor
        .spawn(
            NativeChildPlugin,
            (Arc::clone(&store), RuntimeGeneration::default()),
        )
        .await
        .unwrap();
    let handler: Arc<dyn NativeTaskHandler> = Arc::new(ConfigHandler::new(
        Arc::clone(&store),
        Arc::clone(&service),
    ));
    let implementation = handler.descriptor().unwrap().implementation_ref.clone();
    let _handler: FiberHandle = supervisor
        .spawn(NativeTaskHandlerPlugin, handler)
        .await
        .unwrap();
    let native = supervisor
        .context()
        .try_service::<NativeChildService>()
        .unwrap();
    native.recover().unwrap();

    let input = json!({"op": "read", "namespace": "", "logical_key": "page_limit"});
    let input_ref = store
        .cas
        .seal_object(
            &serde_json::to_vec(&input).unwrap(),
            "application/json",
            "fixture",
            "config-task-input",
            1,
        )
        .unwrap()
        .object;
    let task_id = EntityId::new();
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id: agent,
        conversation_id: agent,
        owner: TaskOwner::Task { id: root_task_id },
        background: false,
        kind: "config.manage".into(),
        definition_version: 1,
        implementation_ref: implementation,
        input_ref,
        state: TaskState::Pending {
            checkpoint_ref: None,
        },
        abort_request: None,
        memos: serde_json::Map::from_iter([
            ("intent_id".into(), json!(task_id)),
            (NATIVE_INPUT.into(), json!("object")),
        ]),
        residual_case_ids: vec![],
        created_by_step_id: Some(step_id),
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: zero,
        updated_commit: zero,
    };
    let event = InboxEvent {
        event_id: crate::playground::stable_id(&format!("config-accepted/{task_id}")),
        kind: InboxEventKind::SystemEvent,
        created_at: chrono::Utc::now(),
        source: "config".into(),
        payload: json!({"task_id": task_id, "kind": "config.manage"}),
    };
    let raw = RawInboxEvent {
        event_id: event.event_id,
        agent_id: agent,
        source: event.source.clone(),
        kind: "system_event".into(),
        payload_json: serde_json::to_string(&event).unwrap(),
        received_at_ms: event.created_at.timestamp_millis(),
    };
    store.admit_native_task(&task, &raw).unwrap();

    // Drive a bounded number of passes. Whether the runtime reports a failed
    // result or parks the task is a different question; the behaviour under
    // test is that an invalid input produces NO durable configuration effect.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        native.run_once().await.unwrap();
        if store.native_task_result(task_id).unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        store.config_entry(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap().is_none(),
        "an invalid input must not save a desired revision"
    );
    assert!(
        store.config_adoptions(UI_NAMESPACE, PAGE_LIMIT_KEY).unwrap().is_empty(),
        "an invalid input must not record an adoption"
    );
    // The task really reached the handler: a claim records a new task
    // revision, so "no effect" is not the same as "never ran".
    let claimed = store.get_task(task_id).unwrap().unwrap();
    assert!(
        claimed.record_revision.0 > 0,
        "the invalid input must have been claimed and rejected, not ignored"
    );
    if let Some(result) = store.native_task_result(task_id).unwrap() {
        assert!(
            !matches!(result.outcome, shenko_types::task::TaskOutcome::Succeeded),
            "an invalid configuration input must never report success"
        );
    }
    drop(root);
}

/// Attachments are sealed before a submission names them: a sealed reference
/// is admitted and the subject reads its text, while a reference to bytes this
/// store does not hold rejects the whole input.
#[test]
fn submissions_admit_only_sealed_attachments_and_the_subject_reads_their_text() -> anyhow::Result<()> {
    use crate::input::{SubmissionMode, SubmissionStage, UserSubmission, WebInputService};
    use crate::presentation::{PresentationService, SurfaceCapabilityProvider};
    let (root, store, agent, _, _) = subject_fixture("owner-attachments");
    let input = WebInputService::new(Arc::clone(&store), agent, None,
        PresentationService::new(8, SurfaceCapabilityProvider::new(false, "attachment-fixture".into())));
    let notes = store.seal_web_attachment("第一行笔记\n第二行".as_bytes(), "notes.md", "text/markdown")?;
    let photo = store.seal_web_attachment(&[0x89, b'P', b'N', b'G'], "photo.png", "image/png")?;
    let submission = |id: &str, refs: Vec<Value>| UserSubmission {
        schema_version: 2, kind: "submit".into(), submission_id: id.into(), message_id: format!("{id}-message"),
        subject_id: agent.to_string(), mode: SubmissionMode::Append, content: String::new(), attachment_refs: refs,
        rich_content_ref: None, observed_step_id: None, observed_request_id: None,
    };
    let missing = json!({"digest": Digest::blake3(b"never uploaded"), "name": "ghost.txt", "media_type": "text/plain", "bytes": 14});
    let rejected = input.submit(&submission("with-missing", vec![serde_json::to_value(&notes)?, missing]));
    assert!(matches!(rejected.stage, SubmissionStage::Rejected), "{rejected:?}");
    assert!(store.web_submission(agent, "with-missing")?.is_none(), "a rejected input leaves no durable submission");

    let accepted = input.submit(&submission("with-sealed", vec![serde_json::to_value(&notes)?, serde_json::to_value(&photo)?]));
    assert!(matches!(accepted.stage, SubmissionStage::Accepted), "{accepted:?}");
    let record = store.web_submission(agent, "with-sealed")?.context("accepted submission has no record")?;
    let event: Value = serde_json::from_str(&store.pending_inbox_events(agent, 8)?
        .into_iter().find(|(id, _)| *id == record.event_id).context("no inbox event")?.1)?;
    let seen = shenko_subject::web_attachments_text(&store, &event["payload"])?;
    assert!(seen.contains("[附件 1] notes.md · text/markdown") && seen.contains("第一行笔记\n第二行"), "{seen}");
    assert!(seen.contains("[附件 2] photo.png · image/png") && seen.contains("看不到画面"), "{seen}");
    drop(root);
    Ok(())
}

/// Cut the real settlement transaction after the built-in consumer changed.
/// The error deliberately resembles a pre-effect stale-base refusal.
fn cut_config_settlement(root: &std::path::Path) -> anyhow::Result<()> {
    rusqlite::Connection::open(root.join("shenko.db"))?.execute_batch(
        "CREATE TRIGGER config_settlement_cut BEFORE UPDATE ON config_changes
         WHEN json_extract(NEW.data, '$.state') = 'applied'
         BEGIN SELECT RAISE(ABORT, 'BASE_CONFIG_STALE: fixture settlement cut'); END;",
    )?;
    Ok(())
}

fn remove_config_settlement_cut(root: &std::path::Path) -> anyhow::Result<()> {
    rusqlite::Connection::open(root.join("shenko.db"))?
        .execute_batch("DROP TRIGGER config_settlement_cut;")?;
    Ok(())
}

#[test]
fn config_settlement_failure_preserves_effect_and_original_origin() -> anyhow::Result<()> {
    let (root, store) = store_fixture("settlement-unknown");
    let service = ConfigService::new(Arc::clone(&store));
    let mut original = request(PAGE_LIMIT_KEY, "settlement-origin", json!(23), Some(0));
    original.namespace = UI_NAMESPACE.into();
    cut_config_settlement(&root)?;
    let error = service.apply(original.clone()).unwrap_err();
    assert_eq!(error.code(), "CONFIG_APPLY_UNSETTLED");
    assert_eq!(error.apply_effect_status(), EffectStatus::Unknown);
    assert_eq!(service.current_value(UI_NAMESPACE, PAGE_LIMIT_KEY), Some(json!(23)));
    let before = store.config_change_by_origin(&original.origin_identity, PAGE_LIMIT_KEY)?
        .context("the original desired change must remain durable")?;
    assert!(matches!(error, ConfigError::ApplyUnsettled { change_id, .. } if change_id == before.change_id));
    assert!(before.receipt.is_none() && before.adopted.is_empty() && before.attempt.is_some());
    assert!(store.config_adoptions(UI_NAMESPACE, PAGE_LIMIT_KEY)?.is_empty());
    remove_config_settlement_cut(&root)?;

    let reopened = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let replacement = ConfigService::new(Arc::clone(&reopened));
    let mut newer = original.clone();
    newer.origin_identity = "new-consumer-decision".into();
    newer.desired = json!(41);
    newer.expected_base_revision = Some(before.desired_revision);
    let accepted = replacement.apply(newer)?;
    assert_eq!(accepted.effect_status(), EffectStatus::Complete);
    let replay = replacement.apply(original)?;
    assert!(replay.replayed);
    assert_eq!(replay.effect_status(), EffectStatus::Unknown);
    assert_eq!(replay.change.change_id, before.change_id);
    assert_eq!(replay.change.desired_patch_ref, before.desired_patch_ref);
    assert_eq!(replay.change.attempt, before.attempt);
    assert!(replay.change.receipt.is_none() && replay.change.adopted.is_empty());
    assert_eq!(replacement.current_value(UI_NAMESPACE, PAGE_LIMIT_KEY), Some(json!(41)));
    assert!(reopened.config_adoptions(UI_NAMESPACE, PAGE_LIMIT_KEY)?.iter()
        .all(|adoption| adoption.adopted_revision == accepted.change.desired_revision));
    Ok(())
}

/// Build the same invocation as the native dispatcher from its actual Store
/// claim, preserving the subject step, task, authority and operation identities.
fn claimed_config_invocation(
    store: &Store,
    handler: &ConfigHandler,
    agent: EntityId,
    step_id: EntityId,
    root_task_id: EntityId,
    input: Value,
) -> anyhow::Result<(shenko_kernel::task::NativeTaskInvocation, shenko_store::child::NativeTaskClaim)> {
    let descriptor = handler.descriptor()?;
    let task_id = EntityId::new();
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let input_ref = store.seal_config_object(&input, "config-task-input")?;
    let task = TaskRecord {
        record_schema_version: 1, task_id, task_revision: TaskRevision(0),
        record_revision: RecordRevision(0), agent_id: agent, conversation_id: agent,
        owner: TaskOwner::Task { id: root_task_id }, background: false,
        kind: "config.manage".into(), definition_version: 1,
        implementation_ref: descriptor.implementation_ref.clone(), input_ref,
        state: TaskState::Pending { checkpoint_ref: None }, abort_request: None,
        memos: serde_json::Map::from_iter([
            ("intent_id".into(), json!(task_id)), (NATIVE_INPUT.into(), json!("object")),
        ]),
        residual_case_ids: vec![], created_by_step_id: Some(step_id),
        dispatch_eligibility: DispatchEligibility::Ready, no_progress_crashes: 0,
        first_run_at: None, terminal_at: None, created_commit: zero, updated_commit: zero,
    };
    let event = InboxEvent {
        event_id: EntityId::new(), kind: InboxEventKind::SystemEvent,
        created_at: chrono::Utc::now(), source: "config".into(),
        payload: json!({"task_id": task_id, "kind": "config.manage"}),
    };
    store.admit_native_task(&task, &RawInboxEvent {
        event_id: event.event_id, agent_id: agent, source: event.source.clone(),
        kind: "system_event".into(), payload_json: serde_json::to_string(&event)?,
        received_at_ms: event.created_at.timestamp_millis(),
    })?;
    let claim = store.claim_native_task(task_id, descriptor.implementation_ref.clone(), false)?
        .context("the production native task must be claimed")?;
    let invocation = shenko_kernel::task::NativeTaskInvocation {
        context: shenko_types::agent::CallContext {
            caller_instance_id: EntityId::new(), role_revision: descriptor.role_revision,
            task_id: Some(task_id), expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id),
            controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: descriptor.authority_refs, trace_id: EntityId::new(),
        },
        task_invocation: claim.invocation.clone(), task: claim.task.clone(),
        operation_id: claim.operation_id,
        authority: shenko_types::operation::AuthorityRef {
            delegation_id: task_id, delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: step_id,
        },
        input, child_tools: None, cancellation: Default::default(),
    };
    Ok((invocation, claim))
}

#[tokio::test]
async fn config_manage_unsettled_apply_keeps_unknown_evidence_and_refusals() -> anyhow::Result<()> {
    let (root, store, agent, step_id, root_task_id) = subject_fixture("native-effect-unknown");
    let service = Arc::new(ConfigService::new(Arc::clone(&store)));
    let handler = ConfigHandler::new(Arc::clone(&store), Arc::clone(&service));
    // An explicit pre-effect validation refusal still completes with no effect.
    let (refused, refused_claim) = claimed_config_invocation(
        &store, &handler, agent, step_id, root_task_id,
        json!({"op": "apply", "namespace": UI_NAMESPACE, "logical_key": PAGE_LIMIT_KEY, "desired": 0}),
    )?;
    let refused = handler.execute(refused).await?;
    assert_eq!(refused.state, OperationState::Resolved);
    assert_eq!(refused.effect_status, EffectStatus::None);
    assert_eq!(refused.outcome, Some(shenko_types::operation::OperationOutcome::Failed));
    assert!(store.complete_native_task(&refused_claim, &refused)?.is_some());

    let input = json!({"op": "apply", "namespace": UI_NAMESPACE, "logical_key": PAGE_LIMIT_KEY, "desired": 23});
    let (invocation, claim) = claimed_config_invocation(
        &store, &handler, agent, step_id, root_task_id, input,
    )?;
    cut_config_settlement(&root)?;
    // The Store correctly closes its writer on this real SQLite failure, so
    // even the handler's first E1 cannot be committed through the failed handle.
    assert!(handler.execute(invocation).await.is_err());
    assert_eq!(service.current_value(UI_NAMESPACE, PAGE_LIMIT_KEY), Some(json!(23)));
    let original_operation = store.external_operation(claim.operation_id)?;
    let original_attempt = store.external_operation_attempt(claim.operation_id)?;
    let origin = format!("config-operation/{}", claim.operation_id);
    let original_change = store.config_change_by_origin(&origin, PAGE_LIMIT_KEY)?
        .context("the native operation must retain its original desired change")?;
    assert!(original_change.receipt.is_none() && original_change.attempt.is_some());
    remove_config_settlement_cut(&root)?;

    let reopened = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let replacement = Arc::new(ConfigService::new(Arc::clone(&reopened)));
    let recovered_handler = ConfigHandler::new(Arc::clone(&reopened), Arc::clone(&replacement));
    let task = reopened.external_task(claim.task.task_id)?;
    let recovered = recovered_handler.reconcile(&task, claim.operation_id).await?;
    assert_eq!(recovered.state, OperationState::Reconciling);
    assert_eq!(recovered.effect_status, EffectStatus::Unknown);
    assert_eq!(recovered.final_input_ref, original_operation.final_input_ref);
    assert_eq!(recovered.authority_ref.delegation_id, original_operation.authority_ref.delegation_id);
    let attempt = reopened.external_operation_attempt(claim.operation_id)?;
    assert_eq!(attempt.attempt_id, original_attempt.attempt_id);
    let evidence = reopened.external_operation_evidence(claim.operation_id)?;
    assert_eq!(evidence.len(), 1);
    let observation: Value = serde_json::from_slice(&reopened.cas.read_object(&evidence[0].raw_ref.digest)?)?;
    assert_eq!(observation["effect_status"], "unknown");
    assert_eq!(observation["change"]["change_id"], json!(original_change.change_id));
    assert!(reopened.native_task_result(task.task_id)?.is_none());

    let mut newer = request(PAGE_LIMIT_KEY, "native-new-consumer-decision", json!(41), None);
    newer.namespace = UI_NAMESPACE.into();
    replacement.apply(newer)?;
    let repeated = recovered_handler.reconcile(&task, claim.operation_id).await?;
    assert_eq!(repeated.record_revision, recovered.record_revision);
    assert_eq!(repeated.effect_status, EffectStatus::Unknown);
    assert_eq!(reopened.external_operation_evidence(claim.operation_id)?.len(), 1);
    assert_eq!(reopened.external_operation_attempts(claim.operation_id)?.len(), 1);
    assert_eq!(replacement.current_value(UI_NAMESPACE, PAGE_LIMIT_KEY), Some(json!(41)));
    assert_eq!(reopened.config_change(original_change.change_id)?.context("original change disappeared")?.attempt,
        original_change.attempt);
    Ok(())
}
