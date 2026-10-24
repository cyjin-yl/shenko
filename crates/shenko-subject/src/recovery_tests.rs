//! 本地 SQLite/CAS 恢复验证，不代表真实 provider 或部署验收。
use super::{SubjectLoopState, execute_step};
use crate::{dispatch::ChildDispatcher, inbox::Inbox};
use shenko_store::store::Store;
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
    let (dispatcher, _rx) = ChildDispatcher::new(agent);
    SubjectLoopState::new(agent, Arc::new(Inbox::new(8)), Arc::new(dispatcher), store)
}

#[test]
fn reopening_awaiting_model_keeps_original_request_blocked() {
    let (dir, store, agent) = fixture();
    let (head, mut step) = store.load_subject(agent).unwrap();
    step.state = SubjectStepState::Preparing;
    step.input_plan_ref = Some(
        store
            .cas
            .seal_object(b"plan", "text/plain", "test", "raw", 1)
            .unwrap()
            .object,
    );
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
    let result = execute_step(planned.step_no, &[], &mut subject, false).await;
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
