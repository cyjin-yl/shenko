//! The memory-intervention worker settles one decision with its operation.
//!
//! The decision *is* the effect of the bounded call, so a successful
//! intervention and its durable decision are one fact: neither exists without
//! the other. An answer the policy cannot read is a failed bounded call — it
//! never settles as a success with no decision, and it never becomes a
//! fabricated silence.
//!
//! The proof runs the real native task runtime, the real broker and a real
//! loopback provider against a real Store.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use shenko_broker::{
    broker::{Broker, BrokerHandle},
    local_provider::LocalModelProvider,
};
use shenko_kernel::Supervisor;
use shenko_memory::{MemoryInterventionRegistry, ModelMemoryInterventionPolicy};
use shenko_store::{
    InterventionDecisionKind, ResearchBudget, ResearchClosure, ResearchEnvironment, Store,
};
use shenko_subject::{
    InboxEvent, InboxEventKind, NativeChildPlugin, NativeChildService, NativeTaskHandler,
    NativeTaskHandlerPlugin, PmaEvaluateHandler,
};
use shenko_types::commit::CommitRef;
use shenko_types::ids::*;
use shenko_types::operation::{OperationOutcome, OperationState};
use shenko_types::subject::*;
use shenko_types::task::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

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
    let head = SubjectBranchHead {
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

async fn read_wire(socket: &mut tokio::net::TcpStream) -> Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let count = socket.read(&mut chunk).await?;
        ensure!(count != 0, "request ended before body");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then_some(value.trim())
                })
                .context("content length")?
                .parse()?;
            if bytes.len() >= end + 4 + len {
                return Ok(serde_json::from_slice(&bytes[end + 4..end + 4 + len])?);
            }
        }
    }
}

async fn runtime(
    store: &Arc<Store>,
    handle: BrokerHandle,
    agent: EntityId,
) -> Result<(Supervisor, Arc<NativeChildService>, ImplementationRef)> {
    let (head, _) = store.load_subject(agent)?;
    let supervisor = Supervisor::new(head.software_generation.clone());
    supervisor
        .spawn(NativeChildPlugin, (store.clone(), head.software_generation))
        .await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    let policy = Arc::new(MemoryInterventionRegistry::new(Arc::new(
        ModelMemoryInterventionPolicy,
    )));
    let handler: Arc<dyn NativeTaskHandler> = Arc::new(PmaEvaluateHandler::new(
        store.clone(),
        handle,
        policy,
    ));
    let implementation = handler.descriptor().implementation_ref;
    supervisor.spawn(NativeTaskHandlerPlugin, handler).await?;
    native.recover()?;
    Ok((supervisor, native, implementation))
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

/// One bounded intervention attempt against a scripted provider.
struct Attempt {
    operation: shenko_types::operation::OperationRecord,
    decision: Option<shenko_store::InterventionReminder>,
    outcome: Option<OperationOutcome>,
    task_outcome: Option<TaskOutcome>,
    requests: usize,
}

async fn attempt(answer: &'static str) -> Result<Attempt> {
    let dir = std::env::temp_dir().join(format!("shenko-pma-decision-{}", EntityId::new()));
    std::fs::create_dir_all(&dir)?;
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas"))?);
    let agent = initialize(&store)?;
    store.start_subject_dispatcher()?;
    let closure = ResearchClosure {
        constraints: vec![],
        budget: ResearchBudget {
            max_tokens: Some(1000),
            max_duration_secs: None,
            max_cost_units: None,
            notes: None,
        },
        effects: vec![],
        environment: ResearchEnvironment {
            environment_id: "pma-test".into(),
            facts: vec!["synthetic observation".into()],
            toolchain: vec![],
            dataset_versions: vec![],
            reproducible: true,
        },
    };
    let epoch = store.open_research_lineage(EntityId::new(), EntityId::new(), None, &closure)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let count = Arc::new(AtomicUsize::new(0));
    let count_server = count.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await?;
            let _wire = read_wire(&mut socket).await?;
            count_server.fetch_add(1, Ordering::SeqCst);
            let raw = json!({
                "id": "scripted-pma-response",
                "choices": [{"message": {"role": "assistant", "content": answer},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 7}
            })
            .to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}",
                        raw.len()
                    )
                    .as_bytes(),
                )
                .await?;
            socket.shutdown().await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "synthetic-pma-model".into()),
        1,
    )?;
    let broker_run = tokio::spawn(broker.run());
    let (supervisor, native, implementation) = runtime(&store, handle.clone(), agent).await?;
    let task = admit(
        &store,
        agent,
        implementation,
        epoch.lineage_id,
        epoch.epoch_id,
    )?;
    tokio::time::timeout(Duration::from_secs(30), native.run_once()).await??;
    let start = Instant::now();
    loop {
        if store.native_task_result(task)?.is_some() {
            break;
        }
        if start.elapsed() >= Duration::from_secs(30) {
            let current = store.external_task(task)?;
            let operation = store
                .external_task_operations(task, "pma.evaluate.v1")?
                .pop();
            anyhow::bail!(
                "the intervention task did not settle: state={:?} park={:?} operation={:?}",
                current.state,
                shenko_store::child::park_reason(&current),
                operation.map(|operation| (operation.state, operation.outcome, operation.evidence_through.is_some()))
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let operation = store
        .external_task_operations(task, "pma.evaluate.v1")?
        .pop()
        .context("the original PMA operation")?;
    let outcome = operation.outcome;
    let decision = store.latest_intervention(epoch.epoch_id)?;
    let task_outcome = store.native_task_result(task)?.map(|result| result.outcome);
    let requests = count.load(Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(30), supervisor.stop()).await??;
    drop(native);
    drop(supervisor);
    drop(handle);
    broker_run.abort();
    let _ = broker_run.await;
    server.abort();
    match server.await {
        Err(error) if error.is_cancelled() => {}
        Ok(result) => result?,
        Err(error) => return Err(error.into()),
    }
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(Attempt {
        operation,
        decision,
        outcome,
        task_outcome,
        requests,
    })
}

/// An answer the policy cannot read is a failed bounded call: the task does
/// not report success and no silence is fabricated for it.
#[tokio::test(flavor = "current_thread")]
async fn an_unreadable_answer_settles_as_a_failure_without_a_decision() -> Result<()> {
    let attempt = attempt("this is not a decision JSON").await?;
    assert_eq!(attempt.outcome, Some(OperationOutcome::Failed));
    assert_eq!(attempt.operation.state, OperationState::Resolved);
    assert!(
        matches!(attempt.task_outcome, Some(TaskOutcome::Failed { .. })),
        "a bounded call without a decision must not report success: {:?}",
        attempt.task_outcome
    );
    assert!(
        attempt.decision.is_none(),
        "a failed bounded call must not record a decision: {:?}",
        attempt.decision
    );
    assert_eq!(attempt.requests, 1, "the bounded call is dispatched once");
    Ok(())
}

/// A decision and the settlement that accepts it are one fact: the task
/// succeeds exactly when its decision is durable.
#[tokio::test(flavor = "current_thread")]
async fn a_silent_answer_settles_with_exactly_one_durable_decision() -> Result<()> {
    let attempt = attempt(r#"{"decision":"silent","reason":"nothing worth saying"}"#).await?;
    assert_eq!(attempt.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(attempt.operation.state, OperationState::Resolved);
    assert!(
        matches!(attempt.task_outcome, Some(TaskOutcome::Succeeded)),
        "a bounded call with a durable decision reports success: {:?}",
        attempt.task_outcome
    );
    let decision = attempt
        .decision
        .clone()
        .context("a settled silent decision must be durable")?;
    assert_eq!(decision.decision, InterventionDecisionKind::Silent);
    assert_eq!(decision.reason.as_deref(), Some("nothing worth saying"));
    assert_eq!(attempt.requests, 1, "the bounded call is dispatched once");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn reminder_batches_are_scoped_and_rebind_after_plugin_restart() -> Result<()> {
    use shenko_context_policy::{ContributionHost, ContributionRegistry, scope_wide};
    use shenko_memory::{ReminderPublisher, intervention_runtime::{
        MemoryInterventionConfig, MemoryInterventionPlugin, PmaContributionRuntime,
    }};
    let dir = std::env::temp_dir().join(format!("shenko-pma-scope-{}", EntityId::new()));
    std::fs::create_dir_all(&dir)?;
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas"))?);
    let a = initialize(&store)?;
    let b = initialize(&store)?;
    store.start_subject_dispatcher()?;
    let (ha, _) = store.load_subject(a)?;
    let (hb, _) = store.load_subject(b)?;
    let closure = ResearchClosure {
        constraints: vec![], effects: vec![],
        budget: ResearchBudget { max_tokens: Some(1000), max_duration_secs: None, max_cost_units: None, notes: None },
        environment: ResearchEnvironment {
            environment_id: "pma-scope".into(), facts: vec![], toolchain: vec![],
            dataset_versions: vec![], reproducible: true,
        },
    };
    let ea = store.open_research_lineage(EntityId::new(), EntityId::new(), Some(ha.subject_root_task_id), &closure)?;
    let eb = store.open_research_lineage(EntityId::new(), EntityId::new(), Some(hb.subject_root_task_id), &closure)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        for name in ["A-first", "A-second", "B-first", "B-after-restart"] {
            let (mut socket, _) = listener.accept().await?;
            let _wire = read_wire(&mut socket).await?;
            let answer = json!({"decision":"remind", "content":name, "provenance":["scope-fixture"],
                "relevance":"the current research", "scope":"subject", "expires_in_secs":600}).to_string();
            let raw = json!({"id":name, "choices":[{"message":{"role":"assistant","content":answer},
                "finish_reason":"stop"}], "usage":{"prompt_tokens":12,"completion_tokens":7}}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes()).await?;
            socket.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "pma-scope".into()), 1)?;
    let broker_run = tokio::spawn(broker.run());
    let supervisor = Supervisor::new(ha.software_generation.clone());
    supervisor.spawn(NativeChildPlugin, (store.clone(), ha.software_generation)).await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    let registry = Arc::new(ContributionRegistry::new());
    supervisor.spawn(ContributionHost, registry.clone()).await?;
    let failure_cancel = tokio_util::sync::CancellationToken::new();
    let policy = Arc::new(MemoryInterventionRegistry::new(Arc::new(ModelMemoryInterventionPolicy)));
    let projection = supervisor.spawn(MemoryInterventionPlugin, MemoryInterventionConfig {
        store: store.clone(), agent_id: a, policy: policy.clone(), failure_cancel: failure_cancel.clone(),
    }).await?;
    let implementation = PmaEvaluateHandler::new(store.clone(), handle.clone(), policy).descriptor().implementation_ref;
    supervisor.spawn(shenko_subject::intervention::PmaEvaluatePlugin, (store.clone(), handle.clone())).await?;
    let keys = |scope| {
        let mut keys: Vec<_> = registry.collect(&scope_wide(scope), chrono::Utc::now().timestamp_millis())
            .contributions.into_iter().map(|item| item.key).collect();
        keys.sort(); keys
    };
    let mut a_keys = Vec::new();
    let mut b_keys = Vec::new();
    for (index, (agent, epoch)) in [(a, &ea), (a, &ea), (b, &eb), (b, &eb)].into_iter().enumerate() {
        if index == 3 {
            let old = supervisor.context().try_service::<PmaContributionRuntime>()?;
            supervisor.restart(&projection).await?;
            assert!(old.refresh(&scope_wide(EntityId(ha.branch_id.0))).is_err());
            assert_eq!(keys(EntityId(ha.branch_id.0)), a_keys, "startup restores the full pending batch");
            assert!(!failure_cancel.is_cancelled(), "plugin replacement must not stop its owner");
        }
        let task = admit(&store, agent, implementation.clone(), epoch.lineage_id, epoch.epoch_id)?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                native.run_once().await?;
                if let Some(result) = store.native_task_result(task)? {
                    ensure!(result.outcome == TaskOutcome::Succeeded, "PMA task failed: {result:?}");
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await??;
        let reminder = store.latest_intervention(epoch.epoch_id)?.context("committed reminder")?;
        let expected = if agent == a { &mut a_keys } else { &mut b_keys };
        expected.push(format!("pma.reminder.{}", reminder.reminder_id));
        expected.sort();
        assert_eq!(keys(EntityId(ha.branch_id.0)), a_keys);
        assert_eq!(keys(EntityId(hb.branch_id.0)), b_keys);
        assert!(keys(ea.epoch_id).is_empty(), "subject reminders are not epoch reminders");
        assert!(keys(EntityId::new()).is_empty(), "unrelated subjects receive nothing");
    }
    server.await??;
    let retained = supervisor.context().try_service::<PmaContributionRuntime>()?;
    supervisor.stop().await?;
    assert!(retained.refresh(&scope_wide(EntityId(ha.branch_id.0))).is_err());
    assert!(keys(EntityId(ha.branch_id.0)).is_empty());
    assert!(!failure_cancel.is_cancelled(), "normal teardown must not signal runtime failure");
    broker_run.abort();
    let _ = broker_run.await;
    Ok(())
}
