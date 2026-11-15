//! Shared real Store/Node/native-runtime fixture for history verification.
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use shenko::obelisk::{self, ObeliskPlugin, TASK_KIND};
use shenko_compat::runtime::AdapterHost;
use shenko_kernel::{Supervisor, task::ChildToolCall};
use shenko_node::Node;
use shenko_store::Store;
use shenko_subject::{NativeChildPlugin, NativeChildService};
use shenko_types::{commit::CommitRef, ids::*, operation::AuthorityRef, subject::*, task::*};
use std::sync::Arc;

pub fn initialize(store: &Store) -> Result<(TaskRecord, AuthorityRef)> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let zero = CommitRef::new(ledger_id, LedgerSeq::ZERO);
    let object = store
        .cas
        .seal_object(
            b"external history lifecycle fixture",
            "text/plain",
            "fixture",
            "raw",
            1,
        )?
        .object;
    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(),
        initial_profile_ref: object.clone(),
        home_manifest_ref: object.clone(),
        bootstrap_event_ref: object.clone(),
        created_commit: zero,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration("external-history-fixture".into()),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: zero,
        next_step_id: first_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first = SubjectStep {
        step_id: first_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: zero,
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
    let root = TaskRecord::subject_root(head.subject_root_task_id, agent, object, zero);
    store.initialize_genesis(&genesis, &head, &first, &root)?;
    store.start_subject_dispatcher()?;
    Ok((
        root.clone(),
        AuthorityRef {
            delegation_id: root.task_id,
            delegation_revision: RecordRevision(0),
            decision_ref: genesis_id,
        },
    ))
}

pub async fn runtime(
    store: Arc<Store>,
    node: Arc<Mutex<Node>>,
    root: &std::path::Path,
) -> Result<(Supervisor, Arc<NativeChildService>)> {
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    supervisor.publish_services().await?;
    supervisor
        .spawn(
            NativeChildPlugin,
            (store.clone(), RuntimeGeneration::default()),
        )
        .await?;
    supervisor
        .spawn(
            ObeliskPlugin {
                config_path: obelisk::config_path(root),
            },
            AdapterHost::new(&supervisor, store, node),
        )
        .await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    Ok((supervisor, native))
}

pub fn tool(input: serde_json::Value) -> ChildToolCall {
    let task_id = EntityId::new();
    ChildToolCall {
        call_id: task_id.to_string(),
        task_id,
        kind: TASK_KIND.into(),
        definition_version: 1,
        input,
    }
}
