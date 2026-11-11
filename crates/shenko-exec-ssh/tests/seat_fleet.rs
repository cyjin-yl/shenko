//! Read-only fleet through the real native claim, operation and CAS evidence path.
use anyhow::{Context, Result};
use shenko_exec_ssh::{SeatFleetHandler, seat::runtime::SeatRuntime};
use shenko_store::{SeatLocation, Store, child::NATIVE_INPUT};
use shenko_subject::{NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    agent::CallContext, commit::CommitRef, ids::*, operation::*,
    store::WriteBatch, subject::*, task::*,
};
use std::sync::Arc;

#[tokio::test]
async fn fleet_native_claim_is_bounded_read_only_and_replays_original_evidence() -> Result<()> {
    let root = std::env::temp_dir().join(format!("shenko-native-fleet-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let store = Arc::new(Store::open(root.join("ledger.db"), root.join("cas"))?);
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let seed = store.cas.seal_object(b"native fleet fixture", "text/plain", "fixture", "raw", 1)?.object;
    let head = SubjectBranchHead {
        agent_id: agent, branch_id: BranchId::new(), subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration::new("fleet-fixture"),
        genesis_ref: GenesisRef { ledger_id: store.ledger.ledger_id(), genesis_id },
        committed_step_no: 0, committed_step_id: None, committed_at: zero,
        next_step_id: EntityId::new(), mode: SubjectMode::Active, mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let genesis = SubjectGenesis {
        genesis_id, subject_id: agent, bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
        seed_manifest_ref: seed.clone(), initial_profile_ref: seed.clone(), home_manifest_ref: seed.clone(),
        bootstrap_event_ref: seed.clone(), created_commit: zero,
    };
    let step = SubjectStep {
        step_id: head.next_step_id, branch_id: head.branch_id.clone(), step_no: 1,
        expected_parent_commit: zero, controller_epoch: head.controller_epoch,
        software_generation: head.software_generation.clone(), state: SubjectStepState::Planned,
        input_plan_ref: None, context_revision: None, active_request_id: None, active_attempt_id: None,
        model_result_ref: None, model_outcome_ref: None, decision_ref: None, commit_ref: None,
        retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None,
    };
    let root_task = TaskRecord::subject_root(head.subject_root_task_id, agent, seed.clone(), zero);
    store.initialize_genesis(&genesis, &head, &step, &root_task)?;
    store.start_subject_dispatcher()?;

    let owned = head.subject_root_task_id;
    let mut newest = None;
    for n in 0..34 {
        let seat = store.register_seat(EntityId::new(), owned, "omp_rpc", "1", SeatLocation::OmpRpc {
            host: "local".into(), remote_session_id: format!("private-command-{n} --secret=never-disclose"),
        })?;
        newest = Some(seat);
    }
    let first = newest.unwrap();
    // The question is admitted by the real seat writer before enumeration.
    let lease = store.acquire_seat_lease(shenko_store::SeatLeaseRequest {
        lease_id: EntityId::new(), seat_id: first.seat_id, holder_task_id: owned,
        lease_epoch: 1, fence_capability: "omp.rpc:fixture".into(),
    })?;
    let question = store.open_seat_question(shenko_store::SeatQuestionRecord {
        schema_version: 2, question_id: EntityId::new(), seat_id: first.seat_id, task_id: owned,
        seat_incarnation: SeatIncarnation(0), task_revision: TaskRevision(0), lease_id: lease.lease_id,
        lease_epoch: 0, question_revision: 0, record_revision: RecordRevision(0),
        created_commit: zero, updated_commit: zero, state: shenko_store::SeatQuestionState::Open,
        question: shenko_store::SeatQuestionFact {
            source_kind: "native_rpc".into(), question_kind: "information".into(),
            native_request_id: Some("q1".into()), native_tool_use_id: None, native_payload: None,
            title: None, text: "Which dataset?".into(), options: vec![], selected_option: None,
            confidence: shenko_store::SeatConfidenceFact { level: "high".into(), basis: "native request".into() },
            unknown: vec![], observed_screen_revision: 1, expires_at_ms: None, fingerprint: "q1".into(),
        },
    })?;
    let summary = store.seat_fleet_summary(Some(owned))?;
    assert_eq!(summary.len(), shenko_store::SEAT_FLEET_LIMIT + 1);
    assert_eq!(summary.iter().find(|entry| entry.seat_id == first.seat_id).unwrap().open_question_count, 1);
    store.mark_seat_question_state(question.question_id, question.record_revision,
        &[shenko_store::SeatQuestionState::Open], shenko_store::SeatQuestionState::Expired)?;
    assert_eq!(store.seat_fleet_summary(Some(owned))?.iter()
        .find(|entry| entry.seat_id == first.seat_id).unwrap().open_question_count, 0);

    let other_task = EntityId::new();
    store.register_seat(EntityId::new(), other_task, "omp_rpc", "1", SeatLocation::OmpRpc {
        host: "local".into(), remote_session_id: "another-owned-task".into(),
    })?;
    let handler = SeatFleetHandler::new(Arc::clone(&store), Arc::new(SeatRuntime::new(Arc::clone(&store))));
    let descriptor = handler.descriptor();
    let task_id = EntityId::new();
    let mut memos = serde_json::Map::new();
    memos.insert(NATIVE_INPUT.into(), serde_json::json!({}));
    let task = TaskRecord {
        record_schema_version: 1, task_id, task_revision: TaskRevision(1), record_revision: RecordRevision(1),
        agent_id: agent, conversation_id: agent, owner: TaskOwner::Task { id: owned }, background: false,
        kind: descriptor.kind.clone(), definition_version: 1, implementation_ref: descriptor.implementation_ref.clone(),
        input_ref: seed, state: TaskState::Pending { checkpoint_ref: None }, abort_request: None,
        memos, residual_case_ids: vec![], created_by_step_id: Some(step.step_id),
        dispatch_eligibility: DispatchEligibility::Ready, no_progress_crashes: 0,
        first_run_at: None, terminal_at: None, created_commit: zero, updated_commit: zero,
    };
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task);
    store.commit(batch)?;
    let claim = store.claim_native_task(task_id, descriptor.implementation_ref.clone(), false)?
        .context("fleet task must be claimable")?;
    let operation = handler.execute(NativeTaskInvocation {
        child_tools: None,
        context: CallContext {
            caller_instance_id: store.capabilities.incarnation, role_revision: descriptor.role_revision,
            task_id: Some(task_id), expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id), controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(), authority_refs: descriptor.authority_refs,
            trace_id: EntityId::new(),
        },
        task_invocation: claim.invocation.clone(), task: claim.task.clone(), operation_id: claim.operation_id,
        authority: AuthorityRef {
            delegation_id: task_id, delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: step.step_id,
        },
        input: serde_json::json!({}),
    }).await?;
    assert_eq!(operation.state, OperationState::Resolved);
    assert_eq!(operation.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(operation.effect_status, EffectStatus::None);
    let attempt = store.external_operation_attempt(operation.operation_id)?;
    let receipt_id = attempt.final_receipt_id.context("fleet has durable evidence")?;
    let receipt = store.external_evidence(receipt_id)?;
    let bytes = store.cas.read_object(&receipt.raw_ref.digest)?;
    let payload: serde_json::Value = serde_json::from_slice(&bytes)?;
    let seats = payload["seats"].as_array().context("bounded fleet array")?;
    assert_eq!(seats.len(), 32);
    assert_eq!(payload["truncated"], true);
    assert_eq!(payload["task_id"], serde_json::Value::Null);
    assert!(seats.iter().any(|seat| seat["task_id"] == other_task.to_string()));
    assert!(seats.iter().any(|seat| seat["task_id"] == owned.to_string()));
    assert!(seats.iter().all(|seat| seat["durable_only"] == true && seat["transport_live"] == false));
    assert!(!String::from_utf8_lossy(&bytes).contains("never-disclose"));
    assert_eq!(store.seat(first.seat_id)?.unwrap().record_revision, first.record_revision);
    assert_eq!(store.seat_current_lease(first.seat_id)?.unwrap().lease_id, lease.lease_id);

    store.register_seat(EntityId::new(), owned, "omp_rpc", "1", SeatLocation::OmpRpc {
        host: "local".into(), remote_session_id: "a-newer-seat-after-snapshot".into(),
    })?;
    let replay = handler.reconcile(&claim.task, operation.operation_id).await?;
    assert_eq!(replay.operation_id, operation.operation_id);
    assert_eq!(store.external_operation_attempt(replay.operation_id)?.final_receipt_id, Some(receipt_id));
    assert_eq!(store.cas.read_object(&receipt.raw_ref.digest)?, bytes, "settled recovery reads original evidence");
    Ok(())
}
