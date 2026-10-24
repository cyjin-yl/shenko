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
    let result = execute_step(planned.step_no, &[], &mut subject, false, &tokio_util::sync::CancellationToken::new()).await;
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
    store.ensure_identity(agent, None).unwrap();
    let mut subject = state(store.clone(), agent).unwrap();
    let before = super::build_system_prompt(&subject, false);
    assert!(before.starts_with(super::SUBJECT_CONTRACT));
    assert!(!before.contains("你是 Shenko"));
    assert!(before.contains("你还没有名字"));

    let source = IdentitySource::Subject { step_id: subject.step.step_id, step_no: 1 };
    let renamed = store
        .change_identity(agent, &IdentityChange::Name("岚".into()), source)
        .unwrap()
        .unwrap();
    assert_eq!((renamed.agent_id, renamed.revision, renamed.naming), (agent, 2, NamingOrigin::SelfChosen));
    // 本步尚未到边界：仍是 revision 1。
    assert_eq!(subject.identity.as_ref().unwrap().revision, 1);

    // 无 broker 时 execute_step 在 S0 之后报错，但 S0 已装配新身份并封存输入计划。
    let planned = subject.step.step_no;
    assert!(execute_step(planned, &[], &mut subject, false, &tokio_util::sync::CancellationToken::new()).await.is_err());
    assert_eq!(subject.identity.as_ref().unwrap().revision, 2);
    assert!(subject.step.input_plan_ref.is_some());
    let after = super::build_system_prompt(&subject, false);
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
        event_id: EntityId::new(), kind: crate::inbox::InboxEventKind::Message,
        payload: serde_json::json!({"content":"original frozen input"}),
        source: "fixture".into(), created_at: chrono::Utc::now(),
    };
    let mut subject = state(store.clone(), agent).unwrap();
    let close = tokio_util::sync::CancellationToken::new();
    close.cancel();
    assert!(execute_step(1, &[event.clone()], &mut subject, false, &close).await.unwrap().is_none());
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    drop(subject);
    drop(store);

    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (closed_broker, handle) = shenko_broker::broker::Broker::offline();
    drop(closed_broker); // positively rejected admission; no provider ever dispatched
    let subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let retained_inbox = subject.inbox.clone();
    assert!(super::run_subject_loop(subject, tokio_util::sync::CancellationToken::new()).await.is_err());
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
    assert_eq!(super::frozen_input(&store, &subject.step).unwrap().events[0].payload, event.payload);
    drop(subject);
    let expected = step.record_revision;
    step.state = SubjectStepState::AwaitingModel;
    step.record_revision = RecordRevision(expected.0 + 1);
    store.upsert_step(&step, &head, expected).unwrap();
    let expected = step.record_revision;
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(store.cas.seal_object(b"persisted complete result", "text/plain", "model_results", "text", 1).unwrap().object);
    step.record_revision = RecordRevision(expected.0 + 1);
    store.upsert_step(&step, &head, expected).unwrap();
    drop(store);

    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let mut subject = state(store.clone(), agent).unwrap(); // no broker: resending would fail
    let (tx, mut rx) = tokio::sync::broadcast::channel(8);
    subject = subject.with_outbox(tx);
    assert_eq!(subject.step.active_request_id, request_id);
    execute_step(1, &[], &mut subject, false, &tokio_util::sync::CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(subject.head.committed_step_no, 1);
    assert_eq!(rx.try_recv().unwrap().content, "persisted complete result");
    assert!(store.event_disposition_commit(agent, event.event_id).unwrap().is_some());
    // A previously queued replay must be filtered after the resumed S1, even
    // though main admitted it before that S1's disposition became visible.
    subject.inbox.sender().send(event).await.unwrap();
    assert!(super::run_subject_loop(subject, tokio_util::sync::CancellationToken::new()).await.is_err());
    let (head, next) = store.load_subject(agent).unwrap();
    assert_eq!(head.committed_step_no, 1);
    assert!(super::frozen_input(&store, &next).unwrap().events.is_empty());
    // Keep the fixture on container disk for independent inspection.
}
