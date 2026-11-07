//! Consumer-visible configuration behaviours — SPEC §43.1, R133, CFG02.
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

use parking_lot::RwLock;
use serde_json::{Value, json};
use uuid::Uuid;

use shenko_kernel::{FiberHandle, Supervisor};
use shenko_store::child::NATIVE_INPUT;
use shenko_subject::{
    InboxEvent, InboxEventKind, NativeChildPlugin, NativeChildService, NativeTaskHandler,
    NativeTaskHandlerPlugin,
};
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
    let head = SubjectBranchHead {
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
    assert_eq!(
        refused.change.receipt.as_ref().map(|receipt| receipt.outcome.as_str()),
        Some("failed")
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
    let implementation = handler.descriptor().implementation_ref.clone();
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
    let implementation = handler.descriptor().implementation_ref.clone();
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
