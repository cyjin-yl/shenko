//! Real native admission/T1/local recovery; no provider runner or wire calls.
use super::*;
use anyhow::Context as _;
use serde_json::json;
use shenko_broker::{broker::Broker, local_provider::LocalModelProvider};
use shenko_store::{ResearchBudget, ResearchClosure, ResearchEnvironment};
use shenko_types::{inbox::{InboxEvent, InboxEventKind}, subject::*, task::*};
use std::time::Duration;

fn initialize(store: &Store) -> Result<EntityId> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(
            b"synthetic PMA decision test",
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
        created_commit: precommit,
    };
    let head = SubjectBranchHead { branch_origin: None,
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration("native-pma-test".into()),
        genesis_ref: GenesisRef { ledger_id, genesis_id },
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
    let root = TaskRecord::subject_root(head.subject_root_task_id, agent, object, precommit);
    store.initialize_genesis(&genesis, &head, &first, &root)?;
    Ok(agent)
}

fn admit(
    store: &Store,
    agent: EntityId,
    implementation_ref: ImplementationRef,
    lineage: EntityId,
    epoch: EntityId,
) -> Result<EntityId> {
    let (head, _) = store.load_subject(agent)?;
    let task_id = EntityId::new();
    let input_ref = store
        .cas
        .seal_object(
            &serde_json::to_vec(&json!({
                "lineage_id": lineage,
                "epoch_id": epoch,
                "max_tokens": 96,
                "trajectory": [
                    {"kind": "message", "summary": "synthetic bounded decision", "at_ms": 1}
                ]
            }))?,
            "application/json",
            "fixture",
            "pma-request",
            1,
        )?
        .object;
    let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id: agent,
        conversation_id: agent,
        owner: TaskOwner::Task {
            id: head.subject_root_task_id,
        },
        background: false,
        kind: "pma.evaluate".into(),
        definition_version: 1,
        implementation_ref,
        input_ref,
        state: TaskState::Pending {
            checkpoint_ref: None,
        },
        abort_request: None,
        memos: serde_json::Map::from_iter([
            ("intent_id".into(), json!(task_id)),
            (shenko_store::child::NATIVE_INPUT.into(), json!("object")),
        ]),
        residual_case_ids: vec![],
        created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: zero,
        updated_commit: zero,
    };
    let event = InboxEvent {
        event_id: EntityId::new(),
        kind: InboxEventKind::ArtifactChanged,
        created_at: chrono::Utc::now(),
        source: "native-pma-test".into(),
        payload: json!({"task_id": task_id}),
    };
    let raw = shenko_types::store::RawInboxEvent {
        event_id: event.event_id,
        agent_id: agent,
        source: event.source.clone(),
        kind: "artifact_changed".into(),
        payload_json: serde_json::to_string(&event)?,
        received_at_ms: event.created_at.timestamp_millis(),
    };
    ensure!(
        store.admit_native_task(&task, &raw)?.is_none(),
        "fresh native admission"
    );
    Ok(task_id)
}

#[derive(Clone, Copy)]
enum RefusalControl { ChangedAuthority, InvalidNoSend }

async fn recover_interrupted_local(
    retain_historical_charge: bool,
    refusal: Option<RefusalControl>,
) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("shenko-pma-local-recovery-{}", EntityId::new()));
    std::fs::create_dir_all(&dir)?;
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas"))?);
    let agent = initialize(&store)?;
    store.start_subject_dispatcher()?;
    let closure = ResearchClosure {
        constraints: vec![], effects: vec![],
        budget: ResearchBudget { max_tokens: Some(1000), max_duration_secs: None, max_cost_units: None, notes: None },
        environment: ResearchEnvironment {
            environment_id: "interrupted-local-decision".into(), facts: vec![], toolchain: vec![],
            dataset_versions: vec![], reproducible: true,
        },
    };
    let epoch = store.open_research_lineage(EntityId::new(), EntityId::new(), None, &closure)?;
    let (_runner, handle) = Broker::with_local_provider(LocalModelProvider::new(
        "http://127.0.0.1:1".into(), "local-recovery-model".into()), 1)?;
    let policies = Arc::new(MemoryInterventionRegistry::new(Arc::new(
        shenko_memory::SilentMemoryInterventionPolicy,
    )));
    let armed = Arc::new(tokio::sync::Notify::new());
    let mut initial = PmaEvaluateHandler::new(store.clone(), handle.clone(), policies.clone());
    initial.pause_after_arm = Some(armed.clone());
    let initial = Arc::new(initial);
    let descriptor = initial.descriptor()?;
    let (head, _) = store.load_subject(agent)?;
    let native = Arc::new(crate::NativeChildService::new(store.clone(), head.software_generation));
    native.install(initial.clone())?;
    native.recover()?;
    let task_id = admit(&store, agent, descriptor.implementation_ref.clone(), epoch.lineage_id, epoch.epoch_id)?;
    let claim = store.claim_native_task(task_id, descriptor.implementation_ref, false)?
        .context("the real native authority claim")?;
    let input = serde_json::from_slice(&store.cas.read_object(&claim.task.input_ref.digest)?)?;
    let invocation = NativeTaskInvocation {
        context: shenko_types::agent::CallContext {
            caller_instance_id: store.capabilities.incarnation,
            role_revision: descriptor.role_revision,
            task_id: Some(task_id), expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id), controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: descriptor.authority_refs, trace_id: EntityId::new(),
        },
        task_invocation: claim.invocation.clone(), task: claim.task.clone(), operation_id: claim.operation_id,
        authority: AuthorityRef {
            delegation_id: task_id, delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: claim.task.created_by_step_id.unwrap_or(claim.invocation.invocation_id),
        },
        input, child_tools: None, cancellation: Default::default(),
    };
    let mut execution = Box::pin(initial.execute(invocation));
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = &mut execution => anyhow::bail!("execution passed the armed interruption: {result:?}"),
            _ = armed.notified() => Ok::<_, anyhow::Error>(()),
        }
    }).await??;
    // Drop the actual execution future at the real post-arm/pre-decision
    // boundary. The hook has no release-build field, code or allocation.
    drop(execution);
    let operation = store.external_operation(claim.operation_id)?;
    let attempt = store.external_operation_attempt(operation.operation_id)?;
    ensure!(operation.state == OperationState::InFlight && operation.outcome.is_none());
    ensure!(attempt.dispatch_stage == AttemptDispatchStage::Armed && attempt.final_receipt_id.is_none());
    ensure!(store.latest_intervention(epoch.epoch_id)?.is_none());
    ensure!(store.native_task_result(task_id)?.is_none());
    let frozen = read_frozen(&store, &operation.final_input_ref)?;
    ensure!(frozen.decided_without_call);
    let send_ref = attempt.transport_ref.as_ref().context("sealed local no-send identity")?;
    let send_bytes = store.cas.read_object(&send_ref.digest)?;
    ensure!(send_bytes.len() as u64 == send_ref.bytes && Digest::blake3(&send_bytes) == send_ref.digest);
    let send: InferenceSendBinding = serde_json::from_slice(&send_bytes)?;
    ensure!(send.request_id == frozen.request_id && send.send_id.is_none());
    ensure!(handle.active_count() == 0 && handle.domain().quarantined_debt().is_empty());

    if retain_historical_charge {
        // Reproduce the old recovery API on the authentic armed local row.
        // This creates the historical false charge without changing its
        // ledger facts or inventing a terminal provider result.
        handle.register_recovered(Arc::new(RecoveryReceipt {
            store: store.clone(), operation: operation.clone(), attempt: attempt.clone(), frozen: frozen.clone(),
        }))?;
        ensure!(handle.active_count() == 1);
        let debt = handle.domain().quarantined_debt();
        ensure!(debt.len() == 1);
        ensure!(debt[0].request_id == frozen.request_id);
    }
    let debt_before = serde_json::to_value(handle.domain().quarantined_debt())?;
    let active_before = handle.active_count();
    let recovery = Arc::new(PmaEvaluateHandler::new(store.clone(), handle.clone(), policies));
    if let Some(control) = refusal {
        // Deliberately alter persisted original evidence after the handler's
        // authenticated snapshot. Neither control invents terminal evidence.
        match control {
            RefusalControl::ChangedAuthority => {
                let mut changed = operation.clone();
                changed.authority_ref.decision_ref = EntityId::new();
                // Keep the revision identical so this specifically exercises
                // the authority triplet comparison, independently of CAS.
                let conn = rusqlite::Connection::open(dir.join("store.db"))?;
                conn.execute(
                    "UPDATE operations SET record_revision=?2,data=?3 WHERE operation_id=?1",
                    rusqlite::params![operation.operation_id.to_string(), changed.record_revision.0 as i64,
                        serde_json::to_string(&changed)?],
                )?;
            }
            RefusalControl::InvalidNoSend => {
                let invalid = InferenceSendBinding {
                    schema_version: 1, request_id: frozen.request_id, send_id: Some(EntityId::new()),
                };
                let mut changed = attempt.clone();
                changed.transport_ref = Some(seal(&store, &invalid, "pma-invalid-no-send-control")?);
                let conn = rusqlite::Connection::open(dir.join("store.db"))?;
                conn.execute("UPDATE attempts SET data=?2 WHERE attempt_id=?1",
                    rusqlite::params![attempt.attempt_id.to_string(), serde_json::to_string(&changed)?])?;
            }
        }
        let before_operation = serde_json::to_value(store.external_operation(operation.operation_id)?)?;
        let before_attempt = serde_json::to_value(store.external_attempt(attempt.attempt_id)?)?;
        let error = match control {
            RefusalControl::ChangedAuthority => {
                let observation = frozen.observation.as_ref().context("frozen local observation")?;
                let request = serde_json::from_value(frozen.delegation.clone())?;
                let decision = recovery.policy.current().plan(observation)?.decision.context("local decision")?;
                let raw = seal(&store, &decision, "pma-local-decision")?;
                recovery.commit_decision(&frozen, &operation, &attempt, &request, observation,
                    &decision, raw, true, attempt.node_id.to_string(), None).unwrap_err()
            }
            RefusalControl::InvalidNoSend => {
                let error = recovery.reconcile(&claim.task, operation.operation_id).await.unwrap_err();
                // Also drive the real settlement boundary with its CURRENT
                // preimage: no-send authentication cannot rely on the handler
                // pre-read alone or merely fail the stale-attempt comparison.
                let changed = store.external_attempt(attempt.attempt_id)?;
                let observation = frozen.observation.as_ref().context("frozen local observation")?;
                let request = serde_json::from_value(frozen.delegation.clone())?;
                let decision = recovery.policy.current().plan(observation)?.decision.context("local decision")?;
                let raw = seal(&store, &decision, "pma-local-decision")?;
                let settlement_error = recovery.commit_decision(&frozen, &operation, &changed,
                    &request, observation, &decision, raw, true, attempt.node_id.to_string(), None).unwrap_err();
                ensure!(settlement_error.to_string().contains(
                    "local intervention lacks authenticated original no-send evidence"),
                    "unexpected transactional marker refusal: {settlement_error:#}");
                error
            }
        };
        let expected = match control {
            RefusalControl::ChangedAuthority => "local intervention original settlement preimage changed",
            RefusalControl::InvalidNoSend => "original local intervention no-send marker mismatch",
        };
        ensure!(error.to_string().contains(expected), "unexpected refusal: {error:#}");
        ensure!(serde_json::to_value(store.external_operation(operation.operation_id)?)? == before_operation);
        ensure!(serde_json::to_value(store.external_attempt(attempt.attempt_id)?)? == before_attempt);
        ensure!(store.external_operation_evidence(operation.operation_id)?.is_empty());
        ensure!(store.latest_intervention(epoch.epoch_id)?.is_none());
        ensure!(store.native_task_result(task_id)?.is_none());
        ensure!(handle.active_count() == active_before);
        ensure!(serde_json::to_value(handle.domain().quarantined_debt())? == debt_before);
        native.close_and_join(0).await;
        drop(native);
        drop(initial);
        drop(recovery);
        drop(handle);
        drop(_runner);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        return Ok(());
    }
    // Retire the interrupted service, then use the shipped recovery scan and
    // managed settlement worker. No original running claim completes this task.
    native.close_and_join(0).await;
    let (head, _) = store.load_subject(agent)?;
    let restarted = Arc::new(crate::NativeChildService::new(store.clone(), head.software_generation));
    restarted.install(recovery.clone())?;
    ensure!(restarted.recover()? == 1);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            restarted.run_once().await?;
            if store.native_task_result(task_id)?.is_some() { break; }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    }).await??;
    let resolved = store.external_operation(operation.operation_id)?;
    ensure!(resolved.state == OperationState::Resolved && resolved.outcome == Some(OperationOutcome::Succeeded));
    let settled_attempt = store.external_operation_attempt(operation.operation_id)?;
    ensure!(settled_attempt.attempt_id == attempt.attempt_id);
    ensure!(settled_attempt.invocation_id == attempt.invocation_id && settled_attempt.armed_commit == attempt.armed_commit);
    ensure!(settled_attempt.transport_ref == attempt.transport_ref && settled_attempt.final_receipt_id.is_some());
    ensure!(serde_json::to_value(&resolved.authority_ref)? == serde_json::to_value(&operation.authority_ref)?);
    let decision = store.latest_intervention(epoch.epoch_id)?.context("recovered local decision")?;
    ensure!(decision.decision == InterventionDecisionKind::Silent);
    ensure!(decision.reason.as_deref() == Some("the silent baseline policy is installed"));
    let result = store.native_task_result(task_id)?.context("native authority settlement")?;
    ensure!(result.outcome == TaskOutcome::Succeeded);
    let completed = store.get_task(task_id)?.context("recovered native task")?;
    let assignment: ExecutionAssignment = serde_json::from_value(completed.memos
        .get(shenko_store::child::NATIVE_ASSIGNMENT).context("restart settlement assignment")?.clone())?;
    ensure!(assignment.assigned_commit.ledger_id == attempt.armed_commit.ledger_id
        && assignment.assigned_commit.seq > attempt.armed_commit.seq);
    ensure!(result.invocation_id == attempt.invocation_id
        && result.operation_id == operation.operation_id
        && Some(result.receipt_id) == settled_attempt.final_receipt_id);
    let replay = recovery.reconcile(&claim.task, operation.operation_id).await?;
    ensure!(replay.state == OperationState::Resolved);
    ensure!(store.external_operation_attempts(operation.operation_id)?.len() == 1);
    ensure!(store.external_operation_attempt(operation.operation_id)?.final_receipt_id == settled_attempt.final_receipt_id);
    ensure!(handle.active_count() == active_before);
    ensure!(serde_json::to_value(handle.domain().quarantined_debt())? == debt_before);
    ensure!(store.quota_send_ids(frozen.request_id)?.is_empty());
    let reopened = Store::open(dir.join("store.db"), dir.join("cas"))?;
    ensure!(reopened.external_operation(operation.operation_id)?.state == OperationState::Resolved);
    ensure!(reopened.external_operation_attempt(operation.operation_id)?.final_receipt_id == settled_attempt.final_receipt_id);
    ensure!(reopened.native_task_result(task_id)?.context("reopened native result")?.outcome == TaskOutcome::Succeeded);
    drop(reopened);
    restarted.close_and_join(0).await;
    drop(restarted);
    drop(native);
    drop(initial);
    drop(recovery);
    drop(handle);
    drop(_runner);
    drop(store);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_local_attempt_recovers_without_inference_charge() -> Result<()> {
    recover_interrupted_local(false, None).await
}

#[tokio::test(flavor = "current_thread")]
async fn historical_local_false_charge_is_identified_and_preserved() -> Result<()> {
    recover_interrupted_local(true, None).await
}

#[tokio::test(flavor = "current_thread")]
async fn changed_original_authority_refuses_local_settlement() -> Result<()> {
    recover_interrupted_local(false, Some(RefusalControl::ChangedAuthority)).await
}

#[tokio::test(flavor = "current_thread")]
async fn sealed_send_refuses_local_no_send_recovery() -> Result<()> {
    recover_interrupted_local(false, Some(RefusalControl::InvalidNoSend)).await
}
