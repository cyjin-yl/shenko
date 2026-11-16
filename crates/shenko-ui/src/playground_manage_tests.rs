//! The subject's own hands on its works — docs/specs/configuration-playground.md#spec-57-6, acceptance PG06/PG07.
//!
//! These cases run the real artifact lifecycle against a real store: a work
//! the subject creates is a durable record, its content is content addressed,
//! staging is a candidate, publishing is an explicit adoption, and a rollback
//! appends a version rather than erasing one.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use shenko_store::store::Store;
use shenko_types::commit::CommitRef;
use shenko_types::ids::*;
use shenko_types::subject::{
    GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
};

use crate::playground::PlaygroundState;
use crate::playground_manage::PlaygroundManageHandler;

/// A store with one subject in it, plus a root for the artifacts.
fn fixture(label: &str) -> (PathBuf, Arc<Store>, EntityId) {
    let root = std::env::temp_dir().join(format!("shenko-pg-manage-{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas")).unwrap());
    let agent = EntityId::new();
    let branch = BranchId::new();
    let first = EntityId::new();
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
        bootstrap_event_ref: object.clone(),
        created_commit: zero,
    };
    let head = SubjectBranchHead { branch_origin: None,
        agent_id: agent,
        branch_id: branch.clone(),
        subject_root_task_id: EntityId::new(),
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
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent,
        object,
        zero,
    );
    store.initialize_genesis(&genesis, &head, &step, &root_task).unwrap();
    (root, store, agent)
}

/// The lifecycle the subject drives: a work it creates is durable, its
/// content is versioned, staging proposes, publishing adopts, and a rollback
/// appends a version instead of erasing one.
#[test]
fn the_subject_can_create_iterate_and_adopt_a_work() -> anyhow::Result<()> {
    let (root, store, agent) = fixture("lifecycle");
    let playground = Arc::new(
        PlaygroundState::open(root.join("playground"), Arc::clone(&store)).expect("playground opens"),
    );
    let handler = PlaygroundManageHandler::new(Arc::clone(&store), Arc::clone(&playground));

    // The frozen request the subject would write.
    let create = serde_json::json!({
        "schema_version": 1,
        "op": "create",
        "title": "我的第一件作品",
        "mime": "text/html",
        "content": "<h1>hello</h1>"
    });
    let frozen = handler.freeze_for_test(create).expect("a create request freezes");
    let (result, outcome, _effect) = handler.observe_for_test(&frozen)?;
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(result["status"], "ok");
    let artifact_id = result["artifact_id"].as_str().expect("the work has an id").to_string();
    assert_eq!(result["revision"], 1);

    // The work is a durable record the subject can find again.
    let record = store
        .artifact_record(artifact_id.parse().unwrap())
        .unwrap()
        .expect("the work has a durable record");
    assert_eq!(record.title, "我的第一件作品");
    assert!(record.active_manifest_ref.is_none(), "authoring does not adopt the new work");

    // New content is a new revision, and the old one is still there.
    let put = serde_json::json!({
        "schema_version": 1,
        "op": "put",
        "artifact_id": artifact_id,
        "content": "<h1>hello again</h1>"
    });
    let frozen = handler.freeze_for_test(put).expect("a put request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen)?;
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(result["revision"], 2);
    let revisions = playground
        .revisions(artifact_id.parse().unwrap())
        .expect("the revisions are readable");
    assert_eq!(revisions, vec![1, 2], "a rollback can return to either");

    // Staging proposes the authored head without adopting it.
    let stage = serde_json::json!({"schema_version": 1, "op": "stage", "artifact_id": artifact_id});
    let frozen = handler.freeze_for_test(stage).expect("a stage request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen)?;
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(result["candidate_manifests"], 1, "the authored head is a candidate: {result}");
    let staged = store.artifact_record(artifact_id.parse().unwrap()).unwrap().unwrap();
    assert!(staged.active_manifest_ref.is_none(), "staging does not adopt");
    let candidate = staged.candidate_manifest_refs[0].clone();

    // Publishing explicitly adopts the genuinely staged candidate.
    let publish = serde_json::json!({"schema_version": 1, "op": "publish", "artifact_id": artifact_id});
    let frozen = handler.freeze_for_test(publish).expect("a publish request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen)?;
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded, "{result}");
    let record = store
        .artifact_record(artifact_id.parse().unwrap())
        .unwrap()
        .expect("the work has a durable record");
    assert_eq!(record.active_manifest_ref.as_ref(), Some(&candidate));
    assert_eq!(record.candidate_manifest_refs, vec![candidate.clone()]);

    // A rollback appends a version rather than erasing one.
    let rollback = serde_json::json!({
        "schema_version": 1,
        "op": "rollback",
        "artifact_id": artifact_id,
        "source_revision": 1
    });
    let frozen = handler
        .freeze_for_test(rollback)
        .expect("a rollback request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen)?;
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(result["revision"], 3, "the rollback is a new revision");
    let revisions = playground
        .revisions(artifact_id.parse().unwrap())
        .expect("the revisions are readable");
    assert_eq!(revisions, vec![1, 2, 3], "nothing was erased");
    let record = store.artifact_record(artifact_id.parse().unwrap()).unwrap().unwrap();
    assert_eq!(record.active_manifest_ref.as_ref(), Some(&candidate), "rollback authors content without changing adoption");
    assert_eq!(record.candidate_manifest_refs, vec![candidate.clone()]);

    playground.stage_artifact(artifact_id.parse().unwrap(), None, Some("stage-rollback")).unwrap();
    let staged = store.artifact_record(artifact_id.parse().unwrap()).unwrap().unwrap();
    assert!(staged.candidate_manifest_refs.contains(&candidate), "staging retains the adopted proposal");
    assert!(staged.candidate_manifest_refs.iter().any(|reference| reference != &candidate), "the rollback has a distinct proposal");
    playground.put(artifact_id.parse().unwrap(), "new draft".into(), "text/html".into(), b"<h1>not adopted</h1>").unwrap();
    let edited = store.artifact_record(artifact_id.parse().unwrap()).unwrap().unwrap();
    assert_eq!(edited.active_manifest_ref, staged.active_manifest_ref, "editing preserves the adopted version");
    assert_eq!(edited.candidate_manifest_refs, staged.candidate_manifest_refs, "editing preserves staged proposals");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

/// A request the subject may not make is refused before anything happens.
#[test]
fn a_request_the_contract_does_not_allow_is_refused() {
    let (root, store, _agent) = fixture("refused");
    let playground = Arc::new(
        PlaygroundState::open(root.join("playground"), Arc::clone(&store)).expect("playground opens"),
    );
    let handler = PlaygroundManageHandler::new(Arc::clone(&store), Arc::clone(&playground));

    // An operation the contract does not have.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "delete_everything"}))
        .is_err());
    // A create without a title is not a work.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "create", "mime": "text/plain", "content": "x"}))
        .is_err());
    // A rollback that names no revision cannot return to one.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "rollback", "artifact_id": "00000000-0000-0000-0000-000000000000"}))
        .is_err());
    // An unknown field is refused rather than ignored.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "list", "surprise": true}))
        .is_err());

    let _ = std::fs::remove_dir_all(&root);
}

/// Arm the real native operation/attempt against the fixture's current
/// subject, then let the regression stop at the effect/E1 crash boundary.
fn armed(
    handler: &PlaygroundManageHandler,
    store: &Store,
    agent: EntityId,
    input: serde_json::Value,
) -> anyhow::Result<(
    shenko_kernel::task::NativeTaskInvocation,
    shenko_types::operation::OperationRecord,
    shenko_types::operation::AttemptRecord,
    super::ManageInput,
)> {
    use shenko_kernel::task::NativeTaskHandler;
    use shenko_types::task::{RunMode, TaskInvocation, TaskOwner, TaskRecord, TaskState};
    let frozen = handler
        .freeze(input.clone())
        .context("test fixture must be available")?;
    let input_ref = handler
        .seal(&frozen, "playground-frozen-input")
        .context("test fixture must be available")?;
    let (head, _) = store
        .load_subject(agent)
        .context("test fixture must be available")?;
    let task_id = EntityId::new();
    let invocation_id = EntityId::new();
    let descriptor = handler.descriptor();
    let mut task = TaskRecord::subject_root(task_id, agent, input_ref.clone(), head.committed_at);
    task.owner = TaskOwner::Task {
        id: head.subject_root_task_id,
    };
    task.kind = descriptor.kind;
    task.implementation_ref = descriptor.implementation_ref;
    task.created_by_step_id = Some(head.next_step_id);
    task.state = TaskState::Running {
        checkpoint_ref: None,
        invocation_id,
        mode: RunMode::Run,
        epoch: head.controller_epoch,
    };
    task.memos
        .insert(shenko_store::child::NATIVE_CHILD.into(), true.into());
    let mut batch = shenko_types::store::WriteBatch::new();
    batch.task_upserts.push(task.clone());
    store
        .commit(batch)
        .context("test fixture must be available")?;
    let invocation = shenko_kernel::task::NativeTaskInvocation {
        context: shenko_types::agent::CallContext::new_system(head.software_generation.clone()),
        task_invocation: TaskInvocation {
            task_id,
            task_revision: task.task_revision,
            record_revision: task.record_revision,
            invocation_id,
            epoch: head.controller_epoch,
            software_generation: head.software_generation,
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        },
        task,
        operation_id: EntityId::new(),
        authority: shenko_types::operation::AuthorityRef {
            delegation_id: task_id,
            delegation_revision: RecordRevision(0),
            decision_ref: head.next_step_id,
        },
        input,
        child_tools: None,
    };
    let (mut operation, mut attempt) = handler
        .operation_parts(&invocation, input_ref)
        .context("test fixture must be available")?;
    assert!(
        store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)
            .context("test fixture must be available")?
            .is_none()
    );
    Ok((invocation, operation, attempt, frozen))
}

/// One interrupted authoring session: competing original create deliveries,
/// lost E1 and failed file projection, later edits and a rollback, followed by
/// another dispatcher owning recovery. All original results must remain
/// exact while the artifact's newer history and business state remain intact.
#[test]
fn committed_manage_effect_recovers_original_attempt_without_repeating_mutations()
-> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {

    use shenko_kernel::task::NativeTaskHandler;
    use shenko_store::artifacts::ArtifactOperation;
    use shenko_types::operation::{EffectStatus, OperationOutcome, OperationState};
    let (root, store, agent) = fixture("effect-before-receipt");
    store
        .start_subject_dispatcher()
        .context("test fixture must be available")?;
    let pg_root = root.join("playground");
    let playground = Arc::new(
        PlaygroundState::open(pg_root.clone(), store.clone())
            .context("test fixture must be available")?,
    );
    let handler = PlaygroundManageHandler::new(store.clone(), playground.clone());
    let (create_inv, create_op, create_att, frozen) = armed(
        &handler,
        &store,
        agent,
        serde_json::json!({"schema_version": 1, "op": "create", "title": "retained work", "mime": "text/plain", "content": "first"}),
    )?;
    let (result, outcome, effect, first) =
        handler.prepare_observation(&frozen, create_op.operation_id);
    assert_eq!(outcome, OperationOutcome::Succeeded);
    let first = first.context("test fixture must be available")?;
    let artifact_id = first.record.artifact_id;
    let first_checkpoint = handler
        .checkpoint(
            &create_op,
            &create_att,
            result.clone(),
            outcome,
            effect,
            Some(&first),
        )
        .context("test fixture must be available")?;
    let (retry_result, retry_outcome, retry_effect, retry) =
        handler.prepare_observation(&frozen, create_op.operation_id);
    let retry = retry.context("test fixture must be available")?;
    let retry_checkpoint = handler
        .checkpoint(
            &create_op,
            &create_att,
            retry_result,
            retry_outcome,
            retry_effect,
            Some(&retry),
        )
        .context("test fixture must be available")?;
    // The CAS/SQLite commit is available while the local projection is not.
    std::fs::write(
        playground.artifact_dir(artifact_id).join("versions"),
        b"blocked projection",
    )
    .context("test fixture must be available")?;
    let start = std::sync::Barrier::new(3);
    let commit = |prepared: &crate::playground::PreparedArtifact, checkpoint| {
        start.wait();
        playground
            .commit_prepared(
                Some(prepared),
                Some(ArtifactOperation {
                    invocation: &create_inv.task_invocation,
                    operation_id: create_op.operation_id,
                    operation_name: &create_op.operation_name,
                    contract_digest: &create_op.contract_ref.contract_digest,
                    attempt_id: create_att.attempt_id,
                    input_digest: &create_op.input_digest,
                    checkpoint_ref: checkpoint,
                }),
            )
            .context("test fixture must be available")
    };
    let (a, b) = std::thread::scope(|scope| -> anyhow::Result<_> {
        let a = scope.spawn(|| commit(&first, &first_checkpoint));
        let b = scope.spawn(|| commit(&retry, &retry_checkpoint));
        start.wait();
        Ok((
            a.join()
                .map_err(|_| anyhow::anyhow!("first commit worker panicked"))??,
            b.join()
                .map_err(|_| anyhow::anyhow!("second commit worker panicked"))??,
        ))
    })?;
    assert_ne!(
        a.replayed, b.replayed,
        "only one original delivery may commit"
    );
    assert_eq!(
        a.checkpoint_ref, b.checkpoint_ref,
        "the winning observation is immutable"
    );
    assert_eq!(
        store
            .list_artifact_records()
            .context("test fixture must be available")?
            .len(),
        1
    );
    assert!(
        store
            .external_operation_evidence(create_op.operation_id)
            .context("test fixture must be available")?
            .is_empty(),
        "crash before E1"
    );
    assert_eq!(
        playground
            .get(&artifact_id)
            .context("test fixture must be available")?
            .context("test fixture must be available")?
            .revision,
        1
    );
    assert_eq!(
        playground
            .read_content(
                &playground
                    .get(&artifact_id)
                    .context("test fixture must be available")?
                    .context("test fixture must be available")?
            )
            .context("test fixture must be available")?,
        b"first"
    );

    // Reopen the real database and select a new dispatcher; the old store is
    // retained only to model a delayed old-process callback after takeover.
    let reopened = Arc::new(
        Store::open(root.join("shenko.db"), root.join("cas"))
            .context("test fixture must be available")?,
    );
    reopened
        .start_subject_dispatcher()
        .context("test fixture must be available")?;
    let recovered = Arc::new(
        PlaygroundState::open(pg_root.clone(), reopened.clone())
            .context("test fixture must be available")?,
    );
    let recovery = PlaygroundManageHandler::new(reopened.clone(), recovered.clone());
    let resolved = recovery
        .reconcile(&create_inv.task, create_op.operation_id)
        .await
        .context("test fixture must be available")?;
    assert_eq!(resolved.state, OperationState::Resolved);
    assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(resolved.effect_status, EffectStatus::Complete);
    let receipts = reopened
        .external_operation_evidence(create_op.operation_id)
        .context("test fixture must be available")?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].attempt_id, Some(create_att.attempt_id));
    let exact: serde_json::Value = serde_json::from_slice(
        &reopened
            .cas
            .read_object(
                &receipts[0]
                    .normalized_ref
                    .as_ref()
                    .context("test fixture must be available")?
                    .digest,
            )
            .context("test fixture must be available")?,
    )
    .context("test fixture must be available")?;
    assert_eq!(exact, result);
    // A stale dispatcher can read its original checkpoint without writing.
    let replay = playground
        .commit_prepared(
            Some(&retry),
            Some(ArtifactOperation {
                invocation: &create_inv.task_invocation,
                operation_id: create_op.operation_id,
                operation_name: &create_op.operation_name,
                contract_digest: &create_op.contract_ref.contract_digest,
                attempt_id: create_att.attempt_id,
                input_digest: &create_op.input_digest,
                checkpoint_ref: &retry_checkpoint,
            }),
        )
        .context("test fixture must be available")?;
    assert!(replay.replayed);
    std::fs::remove_file(recovered.artifact_dir(artifact_id).join("versions"))
        .context("test fixture must be available")?;

    let (put_inv, put_op, put_att, put_input) = armed(
        &recovery,
        &reopened,
        agent,
        serde_json::json!({"schema_version": 1, "op": "put", "artifact_id": artifact_id, "content": "second"}),
    )?;
    let (put_result, outcome, effect, put) =
        recovery.prepare_observation(&put_input, put_op.operation_id);
    let put = put.context("test fixture must be available")?;
    let put_checkpoint = recovery
        .checkpoint(
            &put_op,
            &put_att,
            put_result.clone(),
            outcome,
            effect,
            Some(&put),
        )
        .context("test fixture must be available")?;
    // The previous dispatcher cannot make a new mutation, even with the
    // otherwise current operation/attempt identity from the same database.
    assert!(
        playground
            .commit_prepared(
                Some(&put),
                Some(ArtifactOperation {
                    invocation: &put_inv.task_invocation,
                    operation_id: put_op.operation_id,
                    operation_name: &put_op.operation_name,
                    contract_digest: &put_op.contract_ref.contract_digest,
                    attempt_id: put_att.attempt_id,
                    input_digest: &put_op.input_digest,
                    checkpoint_ref: &put_checkpoint,
                })
            )
            .is_err()
    );
    recovered
        .commit_prepared(
            Some(&put),
            Some(ArtifactOperation {
                invocation: &put_inv.task_invocation,
                operation_id: put_op.operation_id,
                operation_name: &put_op.operation_name,
                contract_digest: &put_op.contract_ref.contract_digest,
                attempt_id: put_att.attempt_id,
                input_digest: &put_op.input_digest,
                checkpoint_ref: &put_checkpoint,
            }),
        )
        .context("test fixture must be available")?;
    let (rollback_inv, rollback_op, rollback_att, rollback_input) = armed(
        &recovery,
        &reopened,
        agent,
        serde_json::json!({"schema_version": 1, "op": "rollback", "artifact_id": artifact_id, "source_revision": 1}),
    )?;
    let (rollback_result, outcome, effect, rollback) =
        recovery.prepare_observation(&rollback_input, rollback_op.operation_id);
    let rollback = rollback.context("test fixture must be available")?;
    let rollback_checkpoint = recovery
        .checkpoint(
            &rollback_op,
            &rollback_att,
            rollback_result.clone(),
            outcome,
            effect,
            Some(&rollback),
        )
        .context("test fixture must be available")?;
    let mut stale_epoch = rollback_inv.task_invocation.clone();
    stale_epoch.epoch = ControllerEpoch(stale_epoch.epoch.0 + 1);
    assert!(
        recovered
            .commit_prepared(
                Some(&rollback),
                Some(ArtifactOperation {
                    invocation: &stale_epoch,
                    operation_id: rollback_op.operation_id,
                    operation_name: &rollback_op.operation_name,
                    contract_digest: &rollback_op.contract_ref.contract_digest,
                    attempt_id: rollback_att.attempt_id,
                    input_digest: &rollback_op.input_digest,
                    checkpoint_ref: &rollback_checkpoint,
                })
            )
            .is_err()
    );
    recovered
        .commit_prepared(
            Some(&rollback),
            Some(ArtifactOperation {
                invocation: &rollback_inv.task_invocation,
                operation_id: rollback_op.operation_id,
                operation_name: &rollback_op.operation_name,
                contract_digest: &rollback_op.contract_ref.contract_digest,
                attempt_id: rollback_att.attempt_id,
                input_digest: &rollback_op.input_digest,
                checkpoint_ref: &rollback_checkpoint,
            }),
        )
        .context("test fixture must be available")?;
    let state = recovered.artifact_dir(artifact_id).join("state");
    std::fs::create_dir_all(&state).context("test fixture must be available")?;
    std::fs::write(state.join("new-fact"), b"must survive replay")
        .context("test fixture must be available")?;
    recovered
        .put(
            artifact_id,
            "newer draft".into(),
            "text/plain".into(),
            b"newer content",
        )
        .context("test fixture must be available")?;
    let before = reopened
        .artifact_record(artifact_id)
        .context("test fixture must be available")?
        .context("test fixture must be available")?;
    let pending_before = reopened
        .pending_inbox_events(agent, 100)
        .context("test fixture must be available")?
        .len();
    // A projected head from another history may have a larger counter. It
    // remains a retained file fact; reopen must not adopt it over the ledger.
    let mut foreign = recovered
        .get(&artifact_id)
        .context("test fixture must be available")?
        .context("test fixture must be available")?;
    foreign.revision = 99;
    foreign.parent_revision = Some(4);
    foreign.title = "foreign projected head".into();
    foreign.content_ref = reopened
        .cas
        .seal_object(b"foreign", "text/plain", "fixture", "artifact-content", 1)
        .context("test fixture must be available")?
        .object;
    foreign.bytes_path = recovered.artifact_dir(artifact_id).join("versions/99");
    let foreign_head = serde_json::to_vec(&foreign).context("test fixture must be available")?;
    std::fs::write(&foreign.bytes_path, b"foreign").context("test fixture must be available")?;
    std::fs::write(
        recovered.artifact_dir(artifact_id).join("versions/99.json"),
        &foreign_head,
    )
    .context("test fixture must be available")?;
    std::fs::write(
        recovered.artifact_dir(artifact_id).join("meta.json"),
        &foreign_head,
    )
    .context("test fixture must be available")?;
    drop(recovery);
    drop(recovered);
    drop(reopened);

    let reopened = Arc::new(
        Store::open(root.join("shenko.db"), root.join("cas"))
            .context("test fixture must be available")?,
    );
    reopened
        .start_subject_dispatcher()
        .context("test fixture must be available")?;
    let recovered = Arc::new(
        PlaygroundState::open(pg_root, reopened.clone())
            .context("test fixture must be available")?,
    );
    let recovery = PlaygroundManageHandler::new(reopened.clone(), recovered.clone());
    recovery
        .reconcile(&put_inv.task, put_op.operation_id)
        .await
        .context("test fixture must be available")?;
    recovery
        .reconcile(&rollback_inv.task, rollback_op.operation_id)
        .await
        .context("test fixture must be available")?;
    recovery
        .reconcile(&rollback_inv.task, rollback_op.operation_id)
        .await
        .context("test fixture must be available")?;
    assert_eq!(
        reopened
            .artifact_record(artifact_id)
            .context("test fixture must be available")?
            .context("test fixture must be available")?
            .updated_commit,
        before.updated_commit
    );
    assert_eq!(
        recovered
            .revisions(artifact_id)
            .context("test fixture must be available")?,
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        recovered
            .read_content(
                &recovered
                    .get(&artifact_id)
                    .context("test fixture must be available")?
                    .context("test fixture must be available")?
            )
            .context("test fixture must be available")?,
        b"newer content"
    );
    assert_eq!(
        std::fs::read(state.join("new-fact")).context("test fixture must be available")?,
        b"must survive replay"
    );
    let retained = recovered
        .artifact_dir(artifact_id)
        .join("retained-heads")
        .join(format!("{}.raw", blake3::hash(&foreign_head).to_hex()));
    assert_eq!(
        std::fs::read(retained).context("test fixture must be available")?,
        foreign_head
    );
    assert_eq!(
        std::fs::read(recovered.artifact_dir(artifact_id).join("versions/99"))
            .context("test fixture must be available")?,
        b"foreign"
    );
    assert_eq!(
        reopened
            .pending_inbox_events(agent, 100)
            .context("test fixture must be available")?
            .len(),
        pending_before
    );
    assert_eq!(
        reopened
            .external_operation_evidence(put_op.operation_id)
            .context("test fixture must be available")?
            .len(),
        1
    );
    assert_eq!(
        reopened
            .external_operation_evidence(rollback_op.operation_id)
            .context("test fixture must be available")?
            .len(),
        1
    );
    let rollback_receipt = reopened
        .external_operation_evidence(rollback_op.operation_id)
        .context("test fixture must be available")?
        .pop()
        .context("test fixture must be available")?;
    let exact: serde_json::Value = serde_json::from_slice(
        &reopened
            .cas
            .read_object(
                &rollback_receipt
                    .normalized_ref
                    .context("test fixture must be available")?
                    .digest,
            )
            .context("test fixture must be available")?,
    )
    .context("test fixture must be available")?;
    assert_eq!(
        exact, rollback_result,
        "rollback recovery reports its original revision, not today's head"
    );
    drop(recovery);
    drop(recovered);
    drop(reopened);
    drop(handler);
    drop(playground);
    drop(store);
    std::fs::remove_dir_all(root).context("test fixture must be available")?;
    Ok(())
    })
}
