//! Shared Store-native canonical fixtures. The retained-value authority is a
//! test fixture, not real consensus; production SSH delivery is tested separately.
use anyhow::Result;
use shenko_store::Store;
use std::sync::Arc;

pub fn initialize_subject_in_with_route(
    store: &Store,
    bootstrap_event: &[u8],
    route_dir: Option<&std::path::Path>,
) -> Result<shenko_types::ids::EntityId> {
    use shenko_types::activation::{ActivationOrigin, ActivationSelector, RouteAuthority};
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
    use shenko_types::object::Digest;
    use shenko_types::subject::{
        GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
    };

    let agent_id = EntityId::new();
    let branch_id = BranchId::new();
    let genesis_id = EntityId::new();
    let bootstrap_id = EntityId::new();
    let launch_id = EntityId::new();
    let first_step_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();

    let dummy_ref = store
        .cas
        .seal_object(bootstrap_event, "application/json", "test", "raw", 1)?
        .object;
    let genesis_commit = CommitRef::new(ledger_id, LedgerSeq(0));

    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id,
        launch_id,
        seed_manifest_ref: dummy_ref.clone(),
        initial_profile_ref: dummy_ref.clone(),
        home_manifest_ref: dummy_ref.clone(),
        bootstrap_event_ref: dummy_ref.clone(),
        created_commit: genesis_commit,
    };

    let head = SubjectBranchHead {
        branch_origin: None,
        agent_id,
        branch_id: branch_id.clone(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: genesis_commit,
        next_step_id: first_step_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };

    let first_step = SubjectStep {
        step_id: first_step_id,
        branch_id: branch_id.clone(),
        step_no: 1,
        expected_parent_commit: genesis_commit,
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

    let selector = if let Some(dir) = route_dir {
        let artifact = dir.join("route-fixture-artifact");
        let artifact_bytes = b"context metadata route fixture; never executed";
        std::fs::write(&artifact, artifact_bytes)?;
        let route = RouteAuthority {
            schema_version: RouteAuthority::SCHEMA_VERSION,
            agent_id,
            generation: head.software_generation.clone(),
            active_epoch: 1,
            selector_revision: 0,
            data_dir: dir.to_string_lossy().into_owned(),
            slot_dir: dir.to_string_lossy().into_owned(),
            artifact_path: artifact.to_string_lossy().into_owned(),
            artifact_digest: Digest::blake3(artifact_bytes),
            launch_command: Vec::new(),
            primary_claim: None,
            activation_id: None,
        };
        let route_ref = store
            .cas
            .seal_object(
                &serde_json::to_vec(&route)?,
                "application/json",
                "activation",
                "route-authority",
                1,
            )?
            .object;
        Some(ActivationSelector {
            agent_id,
            selector_revision: 0,
            active_epoch: 1,
            generation: head.software_generation.clone(),
            state_ref: dummy_ref.clone(),
            origin: ActivationOrigin::Genesis {
                genesis_ref: head.genesis_ref.clone(),
            },
            route_authority_ref: route_ref,
        })
    } else {
        None
    };
    // Initialize genesis and its optional real selector in one transaction.
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent_id,
        dummy_ref,
        genesis_commit,
    );
    store.initialize_genesis_with(
        &genesis,
        &head,
        &first_step,
        &root_task,
        selector
            .as_ref()
            .map(|selector| selector as &dyn shenko_store::GenesisExtension),
    )?;

    Ok(agent_id)
}

pub fn recorded_result_with_docket(
    store: &Arc<Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    mut step: shenko_types::subject::SubjectStep,
    semantic_wait: Option<shenko_types::subject::SemanticWait>,
    reconciliation_docket: &[serde_json::Value],
) -> Result<shenko_types::subject::SubjectStep> {
    use shenko_types::{ids::*, subject::*};
    let initial_revision = step.record_revision.0;
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(Arc::clone(store), EntityId(head.branch_id.0)),
    ));
    let profile = shenko_types::object::Digest::blake3(b"store-roundtrip-profile");
    ledger.rebind_model(1, &profile.0).unwrap();
    let window = ledger.current().unwrap();
    let staged = store.stage_subject_inputs(head, &step, &[])?;
    let input_raw_event_ids: Vec<_> = staged.iter().map(|record| record.event_id).collect();
    let events = staged
        .iter()
        .map(|record| {
            serde_json::from_str::<serde_json::Value>(&record.data)
                .map(|data| data["inbox_event"].clone())
        })
        .collect::<serde_json::Result<Vec<_>>>()?;
    let plan_schema = if reconciliation_docket.is_empty() {
        12
    } else {
        13
    };
    let plan = serde_json::json!({
        "schema_version":plan_schema, "reconciliation_docket":reconciliation_docket,
        "step_id":step.step_id, "step_no":step.step_no, "events":events, "provider_selection":{"schema_version":1,"binding":null,"model":null,"profile_id":null,"profile_revision":null,"credential_fingerprint":null,"resource_domain_id":null,"serializer_revision":null}, "request_id":"00000000-0000-0000-0000-000000000000",
        "provider_binding":null, "semantic_wait":semantic_wait, "identity":null,
        "identity_provider_revision":"shenko.identity/default@1",
        "context":{"source":{"scope_id":EntityId(head.branch_id.0),
            "source_commit_seq":store.current_commit_seq().unwrap(),
            "leaf_seq":store.session_leaf_seq(EntityId(head.branch_id.0)).unwrap(),
            "input_raw_event_ids":input_raw_event_ids, "window_id":window.id,
            "window_generation":window.generation, "model_generation":1},
        "profile_digest":profile,
        "estimated_input_tokens":null, "budget_input_tokens":null,
        "trigger_budget_input_tokens":null,
        "budget_provenance":"store fixture without measured current input",
        "reserved_output_tokens":4096, "unfinished_call_ids":[],
        "capabilities":null, "observed_input_tokens":null,
        "token_counter":"heuristic:cjk-1-other-4@1; not a provider tokenizer",
        "policy_contract":shenko_context_policy::CONTRACT_VERSION,
        "selection":{"outcome":"unavailable","reason":"store_fixture_without_model_capabilities"},
        },
        "payload":{"system_prompt":"store recovery fixture", "messages":[],
            "max_tokens":4096, "tools":[]},
    });
    step.state = SubjectStepState::Preparing;
    step.record_revision = RecordRevision(initial_revision + 1);
    step.input_plan_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&plan).unwrap(),
                "application/json",
                "input_plans",
                "subject_step_input",
                plan_schema,
            )
            .unwrap()
            .object,
    );
    store
        .upsert_step(&step, head, RecordRevision(initial_revision))
        .unwrap();
    step.state = SubjectStepState::AwaitingModel;
    step.record_revision = RecordRevision(initial_revision + 2);
    step.active_request_id = Some(EntityId::new());
    store
        .upsert_step(&step, head, RecordRevision(initial_revision + 1))
        .unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.record_revision = RecordRevision(initial_revision + 3);
    step.model_result_ref = Some(
        store
            .cas
            .seal_object("继续先前关注".as_bytes(), "text/plain", "test", "raw", 1)
            .unwrap()
            .object,
    );
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: step.model_result_ref.clone().unwrap(),
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .unwrap(),
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )
            .unwrap()
            .object,
    );
    store
        .upsert_step(&step, head, RecordRevision(initial_revision + 2))
        .unwrap();
    Ok(step)
}

pub fn committed_candidate_with(
    store: &Store,
    mut step: shenko_types::subject::SubjectStep,
    next: shenko_types::ids::EntityId,
    publications: Vec<shenko_types::subject::PublicationIntent>,
) -> shenko_types::subject::SubjectStep {
    use shenko_types::{ids::*, subject::*};
    let decision = StepDecision {
        schema_version: 8,
        output: shenko_types::output::ParsedOutput {
            schema_version: 5,
            step_id: step.step_id,
            raw_ref: step.model_result_ref.clone().unwrap(),
            text: "继续先前关注".into(),
            controls: vec![],
        },
        identity_intent: None,
        step_id: step.step_id,
        input_plan_ref: step.input_plan_ref.clone().unwrap(),
        model_result_ref: step.model_result_ref.clone(),
        applied_steering_refs: vec![],
        event_dispositions: vec![EventDisposition {
            event_id: step.step_id,
            disposition: Disposition::Acknowledged,
            reason: None,
            priority: None,
            review_after: None,
        }],
        delegated_task_intents: vec![],
        child_dialogue_intents: vec![],
        memory_intents: vec![],
        policy_intents: vec![],
        publication_intents: publications,
        pending_obligations: vec![],
        attention_policy: None,
        evidence_refs: vec![],
        unknown_acknowledgements: vec![],
        successor_step_id: next,
    };
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "test",
                "StepDecision",
                8,
            )
            .unwrap()
            .object,
    );
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(step.record_revision.0 + 1);
    step
}

pub fn draft_dispatch_operation(
    store: &Store,
    invocation: &shenko_types::task::TaskInvocation,
    key: &str,
    operation_id: shenko_types::ids::EntityId,
) -> Result<(
    shenko_types::task::TaskInvocation,
    shenko_types::operation::OperationRecord,
    shenko_types::operation::AttemptRecord,
)> {
    use shenko_types::{ids::*, object::*, operation::*};
    let task = store.external_task(invocation.task_id)?;
    let mut current = invocation.clone();
    current.record_revision = task.record_revision;
    let object = task.input_ref;
    let reader = VersionRef {
        artifact: ArtifactRef {
            digest: object.digest.clone(),
            locator: object.locator.clone(),
        },
        symbol: "dispatch-probe".into(),
        schema: object.codec.clone(),
    };
    let operation = OperationRecord {
        record_schema_version: 1,
        operation_id,
        record_revision: RecordRevision(0),
        task_id: task.task_id,
        logical_key: key.into(),
        operation_name: "dispatch.probe".into(),
        input_schema_version: 1,
        output_schema_version: 1,
        final_input_ref: object.clone(),
        input_digest: object.digest.clone(),
        contract_ref: OperationContractRef {
            plugin_id: task.implementation_ref.plugin_id,
            artifact_digest: object.digest.clone(),
            contract_digest: object.digest.clone(),
            receipt_reader_ref: reader.clone(),
        },
        resource_bindings_ref: object.clone(),
        authority_ref: AuthorityRef {
            delegation_id: task.task_id,
            delegation_revision: task.record_revision,
            decision_ref: task.task_id,
        },
        acceptance_predicate_ref: object.clone(),
        retry_contract_ref: object,
        checkpoint_scope_ref: None,
        state: OperationState::InFlight,
        outcome: None,
        outcome_conflict: false,
        effect_status: EffectStatus::Unknown,
        retry_of: None,
        compensates: None,
        possible_duplicate_of: vec![],
        prepared_commit: task.updated_commit,
        evidence_through: None,
    };
    let attempt = AttemptRecord {
        record_schema_version: 1,
        attempt_id: EntityId::new(),
        operation_id: operation.operation_id,
        attempt_no: 1,
        invocation_id: current.invocation_id,
        worker_id: current.worker_id,
        node_id: current.node_id,
        epoch: current.epoch,
        implementation_ref: reader,
        dispatch_stage: AttemptDispatchStage::Armed,
        armed_commit: task.updated_commit,
        provider_handle: None,
        provider_idempotency: None,
        transport_ref: None,
        started_at: None,
        last_observed_at: None,
        final_receipt_id: None,
    };
    Ok((current, operation, attempt))
}

pub struct CanonicalGuard(pub Option<shenko_types::authority::CommitAuthorityStamp>);
impl shenko_types::authority::CommitAuthorityGuard for CanonicalGuard {
    fn stamp(&self) -> Option<&shenko_types::authority::CommitAuthorityStamp> {
        self.0.as_ref()
    }
}

#[derive(Default)]
pub struct NativeT2AuthorityState {
    pub grants: std::collections::BTreeMap<String, shenko_types::authority::CommitAuthorityGrant>,
    pub last: String,
    pub lose_reply: bool,
    pub fail_before_choose: bool,
}

pub struct NativeT2Authority {
    pub source: &'static str,
    pub stamp: Option<shenko_types::authority::CommitAuthorityStamp>,
    pub state: Arc<parking_lot::Mutex<NativeT2AuthorityState>>,
}

impl shenko_types::authority::CommitAuthorityProvider for NativeT2Authority {
    fn source_id(&self) -> &str {
        self.source
    }

    fn prepare(
        &self,
        admission: &shenko_types::authority::CommitAdmission,
    ) -> std::result::Result<
        Option<shenko_types::authority::CommitAuthorityGrant>,
        shenko_types::error::ShenkoError,
    > {
        let mut state = self.state.lock();
        if std::mem::take(&mut state.fail_before_choose) {
            return Err(shenko_types::error::ShenkoError::conflict(
                "fixture failed before choosing dispatch",
            ));
        }
        let grant = if let Some(original) = state.grants.get(&admission.conflict_key) {
            if original.admission != *admission {
                return Err(shenko_types::error::ShenkoError::conflict(
                    "fixture cannot replace a chosen action",
                ));
            }
            let mut replay = original.clone();
            replay.replayed = true;
            replay
        } else {
            let grant = shenko_types::authority::CommitAuthorityGrant {
                admission: admission.clone(),
                grant_id: format!("native-chosen:{}", admission.action_id),
                proof: serde_json::json!({"test":"native T2 retained chosen value"}),
                replayed: false,
            };
            state
                .grants
                .insert(admission.conflict_key.clone(), grant.clone());
            grant
        };
        state.last = admission.conflict_key.clone();
        if std::mem::take(&mut state.lose_reply) {
            return Err(shenko_types::error::ShenkoError::conflict(
                "fixture lost chosen T2 reply",
            ));
        }
        Ok(Some(grant))
    }

    fn acquire(
        &self,
        grant: Option<&shenko_types::authority::CommitAuthorityGrant>,
    ) -> std::result::Result<
        Box<dyn shenko_types::authority::CommitAuthorityGuard>,
        shenko_types::error::ShenkoError,
    > {
        if let Some(grant) = grant
            && self
                .state
                .lock()
                .grants
                .get(&grant.admission.conflict_key)
                .is_none_or(|original| {
                    original.grant_id != grant.grant_id || original.admission != grant.admission
                })
        {
            return Err(shenko_types::error::ShenkoError::conflict(
                "fixture has no matching chosen T2",
            ));
        }
        Ok(Box::new(CanonicalGuard(self.stamp.clone())))
    }
}
