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
    provider::AnthropicProvider,
};
use shenko_kernel::Supervisor;
use shenko_memory::{MemoryInterventionRegistry, ModelMemoryInterventionPolicy};
use shenko_store::external::{InferenceSendBinding, PMA_QUOTA_INPUT_SCHEMA};
use shenko_store::{
    InterventionDecisionKind, ResearchBudget, ResearchClosure, ResearchEnvironment, Store,
};
use shenko_subject::{
    NativeChildPlugin, NativeChildService, NativeTaskHandlerPlugin, PmaEvaluateHandler,
};
use shenko_kernel::task::NativeTaskHandler;
use shenko_types::inbox::{InboxEvent, InboxEventKind};
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
    let implementation = handler.descriptor()?.implementation_ref;
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

fn record_cloud_quota(store: &Store) -> Result<()> {
    use shenko_quota::*;

    let now = chrono::Utc::now();
    let window = |key: &str| WindowRef {
        provider_id: "anthropic".into(),
        account_scope_ref: "intervention-quota-fixture".into(),
        window_key: key.into(),
    };
    let costs = vec![
        WindowCost { window: window("api:requests"), unit: "requests".into(), amount: Range::point(1.0).context("request cost")? },
        WindowCost { window: window("api:tokens"), unit: "tokens".into(), amount: Range::point(64.0).context("token cost")? },
    ];
    store.record_quota_policy(&QuotaPolicy {
        policy_id: EntityId::new(), revision: 1,
        profiles: vec![ModelProfile {
            profile_id: "pma-cloud".into(), model_ref: "synthetic-pma-model".into(), local: false,
            effort_levels: vec![EffortLevel { effort: "high".into(), cost_per_request: costs.clone() }],
            cost_per_request: costs, max_output_tokens: std::num::NonZeroU32::new(32),
        }],
        assumptions: vec![], min_progress_per_hour: 0.0, switching_hysteresis: 0.0,
        on_unknown: UnknownQuota::Block, allow_overage: false, spending_authority_ref: None,
        decision_ttl: chrono::Duration::minutes(5),
        observation_sources: vec![SourceObservationPolicy {
            source_kind: SourceKind::ApiHeaders, contract: None,
            ttl: chrono::Duration::minutes(5), stale: StaleUse::Refuse,
        }],
        objective: PolicyObjective::Balanced, target_utilization_range: None, planning_horizon: None,
        emergency_reserve_by_window: vec![], uncertainty_margin: None,
        admission_pacing: AdmissionPacing::default(),
    }, now)?;
    store.record_quota_snapshot(&from_api_headers(&ObservationContext {
        provider_id: "anthropic".into(), account_scope_ref: "intervention-quota-fixture".into(),
        source_version: None, attribution: Attribution::Exclusive, observed_at: now,
    }, [
        ("anthropic-ratelimit-requests-limit", "100"),
        ("anthropic-ratelimit-requests-remaining", "100"),
        ("anthropic-ratelimit-requests-reset", "2099-01-01T00:00:00Z"),
        ("anthropic-ratelimit-tokens-limit", "1000"),
        ("anthropic-ratelimit-tokens-remaining", "1000"),
        ("anthropic-ratelimit-tokens-reset", "2099-01-01T00:00:00Z"),
    ]))?;
    Ok(())
}

async fn attempt(answer: &'static str, quota: bool) -> Result<Attempt> {
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
    let (wire_tx, mut wire_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await?;
            let wire = read_wire(&mut socket).await?;
            if quota { wire_tx.send(wire)?; }
            count_server.fetch_add(1, Ordering::SeqCst);
            let raw = if quota {
                json!({"content": [{"type": "text", "text": answer}], "stop_reason": "end_turn",
                    "usage": {"input_tokens": 12, "output_tokens": 7}})
            } else {
                json!({"id": "scripted-pma-response",
                    "choices": [{"message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 12, "completion_tokens": 7}})
            }.to_string();
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
    let (broker, handle) = if quota {
        record_cloud_quota(&store)?;
        Broker::with_anthropic_provider(AnthropicProvider::with_endpoint(
            "scripted-loopback-not-a-credential".into(), endpoint, "synthetic-pma-model",
        ), 1)?
    } else {
        Broker::with_local_provider(LocalModelProvider::new(endpoint, "synthetic-pma-model".into()), 1)?
    };
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
    if quota {
        let wire = wire_rx.try_recv().context("original PMA provider request")?;
        let attempt = store.external_operation_attempt(operation.operation_id)?;
        let marker = attempt.transport_ref.as_ref().context("original PMA send marker")?;
        let binding: InferenceSendBinding = serde_json::from_slice(&store.cas.read_object(&marker.digest)?)?;
        let send = binding.send_id.context("original PMA quota send")?;
        let input: Value = serde_json::from_slice(&store.cas.read_object(&operation.final_input_ref.digest)?)?;
        assert_eq!(operation.input_schema_version, PMA_QUOTA_INPUT_SCHEMA);
        assert_eq!(input["requested_max_tokens"], 96);
        assert_eq!(input["quota_selection"]["max_output_tokens"], 32);
        assert_eq!(wire["model"], "synthetic-pma-model");
        assert_eq!(wire["max_tokens"], 32);
        assert_eq!(wire["max_tokens"], input["payload"]["max_tokens"]);
        assert_eq!(wire["output_config"]["effort"], "high");
        assert_eq!(wire["output_config"], input["payload"]["native_options"]["output_config"]);
        assert_eq!(store.quota_send_ids(binding.request_id)?, vec![(send, attempt.armed_commit.seq.0)]);
        let receipt = store.external_evidence(attempt.final_receipt_id.context("original PMA E1")?)?;
        assert_eq!(receipt.attempt_id, Some(attempt.attempt_id));
    }
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
    if !quota {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(Attempt {
        operation,
        decision,
        outcome,
        task_outcome,
        requests,
    })
}

/// The shipped local baseline commits its native decision without a provider
/// send, even when quota is configured and the selected runtime is withdrawn.
#[tokio::test(flavor = "current_thread")]
async fn shipped_silent_policy_settles_with_quota_and_withdrawn_runtime() -> Result<()> {
    use shenko_quota::*;
    let dir = std::env::temp_dir().join(format!("shenko-pma-local-quota-{}", EntityId::new()));
    std::fs::create_dir_all(&dir)?;
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas"))?);
    let agent = initialize(&store)?;
    store.start_subject_dispatcher()?;
    let closure = ResearchClosure {
        constraints: vec![], effects: vec![],
        budget: ResearchBudget { max_tokens: Some(1000), max_duration_secs: None, max_cost_units: None, notes: None },
        environment: ResearchEnvironment {
            environment_id: "silent-quota-withdrawal".into(), facts: vec![], toolchain: vec![],
            dataset_versions: vec![], reproducible: true,
        },
    };
    let epoch = store.open_research_lineage(EntityId::new(), EntityId::new(), None, &closure)?;
    let provider = Arc::new(LocalModelProvider::new("http://127.0.0.1:1".into(), "silent-quota-model".into()));
    let registry = Arc::new(shenko_broker::registry::ProviderRegistry::new());
    let installation = registry.install(provider.clone());
    let domain = shenko_broker::resource::ResourceDomain::new(
        shenko_broker::resource::ResourceDomainIdentity {
            domain_id: provider.profile().resource_domain_id.clone(),
            engine_identity: "silent-quota-control".into(), engine_incarnation: None, epoch: 1,
        }, 1, shenko_broker::admission::CapacityProvenance::Unknown,
        None, shenko_broker::admission::CapacityProvenance::Unknown,
    );
    let (_broker, handle) = Broker::with_registry(registry.clone(), domain)?;
    let policy = QuotaPolicy {
        policy_id: EntityId::new(), revision: 1,
        profiles: vec![ModelProfile {
            profile_id: "silent-quota-local".into(), model_ref: "silent-quota-model".into(),
            local: true, cost_per_request: vec![], effort_levels: vec![], max_output_tokens: None,
        }],
        assumptions: vec![], min_progress_per_hour: 0.0, switching_hysteresis: 0.0,
        on_unknown: UnknownQuota::Block, allow_overage: false, spending_authority_ref: None,
        decision_ttl: chrono::Duration::minutes(5), observation_sources: vec![],
        objective: PolicyObjective::Balanced, target_utilization_range: None, planning_horizon: None,
        emergency_reserve_by_window: vec![], uncertainty_margin: None,
        admission_pacing: AdmissionPacing::default(),
    };
    store.record_quota_policy(&policy, chrono::Utc::now())?;
    ensure!(registry.uninstall(&provider.profile().profile_id, installation));
    ensure!(!handle.serves_model("silent-quota-model"));
    let (head, _) = store.load_subject(agent)?;
    let supervisor = Supervisor::new(head.software_generation.clone());
    supervisor.spawn(NativeChildPlugin, (store.clone(), head.software_generation)).await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    let policies = Arc::new(MemoryInterventionRegistry::new(Arc::new(
        shenko_memory::SilentMemoryInterventionPolicy,
    )));
    let handler: Arc<dyn NativeTaskHandler> = Arc::new(PmaEvaluateHandler::new(store.clone(), handle, policies));
    let implementation = handler.descriptor()?.implementation_ref;
    supervisor.spawn(NativeTaskHandlerPlugin, handler).await?;
    native.recover()?;
    let task = self::admit(&store, agent, implementation, epoch.lineage_id, epoch.epoch_id)?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            native.run_once().await?;
            if let Some(result) = store.native_task_result(task)? {
                ensure!(result.outcome == TaskOutcome::Succeeded, "silent policy did not settle: {result:?}");
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await??;
    let decision = store.latest_intervention(epoch.epoch_id)?.context("durable shipped silent decision")?;
    assert_eq!(decision.decision, InterventionDecisionKind::Silent);
    assert_eq!(decision.reason.as_deref(), Some("the silent baseline policy is installed"));
    let operations = store.external_task_operations(task, "pma.evaluate.v1")?;
    ensure!(operations.len() == 1);
    let operation = &operations[0];
    ensure!(operation.state == OperationState::Resolved && operation.outcome == Some(OperationOutcome::Succeeded));
    let frozen: Value = serde_json::from_slice(&store.cas.read_object(&operation.final_input_ref.digest)?)?;
    ensure!(frozen["decided_without_call"] == true);
    let request_id: EntityId = serde_json::from_value(frozen["request_id"].clone())?;
    ensure!(store.quota_send_ids(request_id)?.is_empty());
    ensure!(store.quota_pacer()?.reservations().next().is_none());
    ensure!(store.latest_quota_decision()?.is_none());
    let attempts = store.external_operation_attempts(operation.operation_id)?;
    ensure!(attempts.len() == 1, "the local durable effect retains its native authority attempt");
    let transport = attempts[0].transport_ref.as_ref().context("local no-send binding")?;
    let send: Value = serde_json::from_slice(&store.cas.read_object(&transport.digest)?)?;
    ensure!(send["send_id"].is_null());
    tokio::time::timeout(Duration::from_secs(30), supervisor.stop()).await??;
    drop(native);
    drop(supervisor);
    drop(store);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

/// An answer the policy cannot read is a failed bounded call: the task does
/// not report success and no silence is fabricated for it.
#[tokio::test(flavor = "current_thread")]
async fn an_unreadable_answer_settles_as_a_failure_without_a_decision() -> Result<()> {
    let attempt = attempt("this is not a decision JSON", false).await?;
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
    let attempt = attempt(r#"{"decision":"silent","reason":"nothing worth saying"}"#, false).await?;
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
async fn cloud_intervention_sends_the_sealed_effort_and_output_under_exact_t1() -> Result<()> {
    let attempt = attempt(r#"{"decision":"silent","reason":"nothing worth saying"}"#, true).await?;
    assert_eq!(attempt.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(attempt.decision.context("durable intervention decision")?.decision, InterventionDecisionKind::Silent);
    assert_eq!(attempt.requests, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn an_unroutable_reminder_fails_without_committing_a_poisoned_decision() -> Result<()> {
    let attempt = attempt(r#"{"decision":"remind","content":"Keep external IDs lossless.","provenance":["fixture:source"],"relevance":"the current migration","scope":"迁移导出的数值处理与提交实现","expires_in_secs":600}"#, false).await?;
    assert_eq!(attempt.operation.state, OperationState::Resolved);
    assert_eq!(attempt.outcome, Some(OperationOutcome::Failed));
    assert!(matches!(attempt.task_outcome, Some(TaskOutcome::Failed { .. })));
    assert!(attempt.decision.is_none(), "invalid routing must not poison pending reminder preparation");
    assert_eq!(attempt.requests, 1, "refusal does not resend the original inference");
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
        for name in ["A-first", "A-second", "B-first", "B-after-restart", "A-owned-epoch"] {
            let (mut socket, _) = listener.accept().await?;
            let _wire = read_wire(&mut socket).await?;
            let address = if name == "A-owned-epoch" { format!("epoch:{}", ea.epoch_id) } else { "subject".into() };
            let answer = json!({"decision":"remind", "content":name, "provenance":["scope-fixture"],
                "relevance":"the current research", "scope":address, "expires_in_secs":600}).to_string();
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
    let implementation = PmaEvaluateHandler::new(store.clone(), handle.clone(), policy).descriptor()?.implementation_ref;
    supervisor.spawn(shenko_subject::intervention::PmaEvaluatePlugin, (store.clone(), handle.clone())).await?;
    let keys = |scope| {
        let mut keys: Vec<_> = registry.collect(&scope_wide(scope), chrono::Utc::now().timestamp_millis())
            .contributions.into_iter().map(|item| item.key).collect();
        keys.sort(); keys
    };
    let mut a_keys = Vec::new();
    let mut b_keys = Vec::new();
    for (index, (agent, epoch)) in [(a, &ea), (a, &ea), (b, &eb), (b, &eb), (a, &ea)].into_iter().enumerate() {
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
        let tool_result = store.native_task_result(task)?.context("settled PMA task result")?;
        let projected = tool_result.result.context("decision identity projection")?;
        assert!(!projected.to_string().contains(reminder.content.as_deref().unwrap()),
            "transient reminder content must not become permanent rolling tool history");
        let decision_ref: shenko_types::object::ObjectRef = serde_json::from_value(projected["decision_ref"].clone())?;
        let bytes = store.cas.read_object(&decision_ref.digest)?;
        assert_eq!(shenko_types::object::Digest::blake3(&bytes), decision_ref.digest);
        assert_eq!(serde_json::from_slice::<shenko_store::InterventionReminder>(&bytes)?, reminder);
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
