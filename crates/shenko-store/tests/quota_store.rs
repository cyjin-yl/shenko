//! C18 durable quota ↔ Store lifecycle: typed admission persisted on the
//! mutation line, armed reservations surviving reopen, and the AwaitingModel
//! CAS refusing requests with no armed reservation. Synthetic policies and
//! quota values only; no provider/account traffic is claimed.
use shenko_quota::{
    decide, forecast, from_api_headers, AdmissionOutcome, AdmissionState, AttemptBilling,
    Attribution, BackendState, DispatchEvidence, ForecastOptions, ModelProfile, ObservationContext,
    PolicyDecision, QuotaPolicy, QuotaSnapshot, Range, SourceKind, SourceObservationPolicy,
    StaleUse, UnknownQuota, WindowCost, WindowRef,
};
use shenko_store::store::Store;
use shenko_types::ids::{BranchId, EntityId, LedgerSeq, RecordRevision};
use shenko_types::subject::{
    GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-quota-test-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// One cloud profile priced at one request against the api:requests bucket.
fn policy() -> QuotaPolicy {
    QuotaPolicy {
        policy_id: EntityId::new(),
        revision: 1,
        profiles: vec![ModelProfile {
            profile_id: "cloud".into(),
            model_ref: "model/cloud".into(),
            local: false,
            cost_per_request: vec![WindowCost {
                window: WindowRef {
                    provider_id: "anthropic".into(),
                    account_scope_ref: "sub".into(),
                    window_key: "api:requests".into(),
                },
                unit: "requests".into(),
                amount: Range::point(1.0).unwrap(),
            }],
        }],
        assumptions: Vec::new(),
        min_progress_per_hour: 0.0,
        switching_hysteresis: 0.0,
        on_unknown: UnknownQuota::Block,
        allow_overage: false,
        spending_authority_ref: None,
        decision_ttl: chrono::Duration::minutes(20),
        observation_sources: vec![SourceObservationPolicy {
            source_kind: SourceKind::ApiHeaders,
            ttl: chrono::Duration::minutes(5),
            stale: StaleUse::Refuse,
        }],
    }
}

fn availability() -> HashMap<String, BackendState> {
    HashMap::from([("cloud".to_string(), BackendState::Available)])
}

fn snapshot(remaining: u64) -> QuotaSnapshot {
    let ctx = ObservationContext {
        provider_id: "anthropic".into(),
        account_scope_ref: "sub".into(),
        source_version: None,
        attribution: Attribution::Exclusive,
        observed_at: now(),
    };
    let remaining = remaining.to_string();
    from_api_headers(
        &ctx,
        [
            ("anthropic-ratelimit-requests-limit", "10"),
            ("anthropic-ratelimit-requests-remaining", remaining.as_str()),
            ("anthropic-ratelimit-requests-reset", "2099-01-01T00:00:00Z"),
        ],
    )
}

/// Record a fresh snapshot → forecast → decision chain for `remaining`.
fn decide_fixture(store: &Store, policy: &QuotaPolicy, remaining: u64) -> PolicyDecision {
    let snap = snapshot(remaining);
    store.record_quota_snapshot(&snap).unwrap();
    let forecast = forecast(
        &snap,
        &store.quota_attempts("anthropic", "sub").unwrap(),
        &HashMap::new(),
        now(),
        ForecastOptions { ttl: chrono::Duration::minutes(5), stale: StaleUse::Refuse },
    );
    store.record_quota_forecast(&forecast).unwrap();
    let decision = decide(policy, &[forecast], &availability(), None, now()).unwrap();
    store.record_quota_decision(&decision).unwrap();
    decision
}

fn admitted(store: &Store, policy: &QuotaPolicy, decision: &PolicyDecision) -> shenko_quota::QuotaAdmission {
    let receipt = store
        .admit_quota_request(policy, decision, EntityId::new(), &availability(), now())
        .unwrap();
    match receipt.outcome {
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted => admission,
        other => panic!("expected a fresh admitted outcome, got {other:?}"),
    }
}

fn pacer_reservation(store: &Store, request_id: EntityId) -> shenko_quota::Reservation {
    store
        .quota_pacer()
        .unwrap()
        .reservation(&request_id)
        .cloned()
        .expect("reservation recorded")
}

/// Armed reservations keep their exact commit identity across reopen, admit
/// replays idempotently, contradictory never-dispatched cancellation is
/// rejected without touching the pacer, and armed cancellation retains the
/// conservative cost interval until coverage.
#[test]
fn armed_reservation_survives_reopen_and_keeps_charge() {
    let dir = tmp_dir();
    let store = Arc::new(Store::open(dir.join("quota.db"), dir.join("cas")).unwrap());
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 10);
    let admission = admitted(&store, &policy, &decision);
    let request_id = admission.request_id;
    let (armed, arm_commit) = store.arm_quota_admission(&admission).unwrap();
    assert_eq!(armed.state, AdmissionState::Admitted);
    let reservation = pacer_reservation(&store, request_id);
    assert_eq!(reservation.attempts.len(), 1);
    let bound = reservation.attempts[0].ledger_commit;
    assert_eq!(bound, arm_commit);
    assert_eq!(reservation.outstanding.len(), 1);
    drop(store);

    // Reopen: durable reservation and its bound commit survive verbatim.
    let reopened = Arc::new(Store::open(dir.join("quota.db"), dir.join("cas")).unwrap());
    let retained = pacer_reservation(&reopened, request_id);
    assert_eq!(retained.admission, armed);
    assert_eq!(retained.attempts[0].ledger_commit, bound);
    assert_eq!(retained.dispatch, Some(DispatchEvidence::ArmedOrUnknown));
    // Re-admission of the same request under the same decision is the
    // existing record, never a second reservation or dispatch authority.
    let replay = reopened
        .admit_quota_request(&policy, &decision, request_id, &availability(), now())
        .unwrap();
    assert!(matches!(
        replay.outcome,
        AdmissionOutcome::Existing { admission } if admission == armed
    ));
    // Armed replay verifies the bound rows instead of arming new commits.
    let (rearmed, _) = reopened.arm_quota_admission(&armed).unwrap();
    assert_eq!(rearmed, armed);
    assert_eq!(reopened.quota_attempts("anthropic", "sub").unwrap().len(), 1);
    // A never-dispatched cancellation cannot downgrade armed evidence; the
    // rejection happens before any pacer mutation and the charge stays.
    assert!(reopened
        .cancel_quota_admission(&armed, DispatchEvidence::NeverDispatched)
        .is_err());
    let after = pacer_reservation(&reopened, request_id);
    assert_eq!(after.dispatch, Some(DispatchEvidence::ArmedOrUnknown));
    assert_eq!(after.outstanding[0].amount.upper, 1.0);
    // The honest armed cancellation retains [0, upper] until coverage.
    let (cancelled, retained_costs, _) = reopened
        .cancel_quota_admission(&armed, DispatchEvidence::ArmedOrUnknown)
        .unwrap();
    assert_eq!(cancelled.state, AdmissionState::Cancelled);
    assert_eq!(retained_costs[0].amount.lower, 0.0);
    assert_eq!(retained_costs[0].amount.upper, 1.0);
    let replay = reopened
        .admit_quota_request(&policy, &decision, request_id, &availability(), now())
        .unwrap();
    assert!(matches!(
        replay.outcome,
        AdmissionOutcome::Existing { admission } if admission.state == AdmissionState::Cancelled
    ));
}

/// The AwaitingModel step CAS refuses a request without an armed reservation
/// once any quota policy is recorded, and accepts it after admit+arm. With no
/// recorded policy the same transition needs no quota state at all.
#[test]
fn awaiting_model_cas_requires_armed_reservation() {
    // No policy recorded: the unconfigured provider path stays open.
    let (dir_a, store_a, agent_a) = initialized_subject();
    let (head_a, planned_a) = store_a.load_subject(agent_a).unwrap();
    let preparing_a = preparing_step(&store_a, &head_a, planned_a);
    let mut awaiting = preparing_a.clone();
    awaiting.state = SubjectStepState::AwaitingModel;
    awaiting.record_revision = RecordRevision(2);
    awaiting.active_request_id = Some(EntityId::new());
    store_a
        .upsert_step(&awaiting, &head_a, preparing_a.record_revision)
        .unwrap();
    drop(store_a);
    std::fs::remove_dir_all(dir_a).unwrap();

    // With a recorded policy the same transition is gated.
    let (dir_b, store_b, agent_b) = initialized_subject();
    let policy = policy();
    store_b.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store_b, &policy, 10);
    let (head_b, planned_b) = store_b.load_subject(agent_b).unwrap();
    let preparing_b = preparing_step(&store_b, &head_b, planned_b);
    let request_id = EntityId::new();
    let mut blocked = preparing_b.clone();
    blocked.state = SubjectStepState::AwaitingModel;
    blocked.record_revision = RecordRevision(2);
    blocked.active_request_id = Some(request_id);
    assert!(store_b
        .upsert_step(&blocked, &head_b, preparing_b.record_revision)
        .is_err());
    // Admit and arm this exact request id, then the identical CAS succeeds.
    let receipt = store_b
        .admit_quota_request(&policy, &decision, request_id, &availability(), now())
        .unwrap();
    let AdmissionOutcome::Quota(admission) = receipt.outcome else {
        panic!("expected fresh admission");
    };
    store_b.arm_quota_admission(&admission).unwrap();
    store_b
        .upsert_step(&blocked, &head_b, preparing_b.record_revision)
        .unwrap();
    std::fs::remove_dir_all(dir_b).unwrap();
}

/// An admitted-but-never-armed request releases its reservation on
/// never-dispatched evidence; the freed budget admits the next request.
#[test]
fn never_dispatched_releases_reservation() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 2);
    // Two reservations exhaust the bucket; a third must wait.
    let first = admitted(&store, &policy, &decision);
    let second = admitted(&store, &policy, &decision);
    let third_id = EntityId::new();
    let waiting = store
        .admit_quota_request(&policy, &decision, third_id, &availability(), now())
        .unwrap();
    assert!(matches!(
        &waiting.outcome,
        AdmissionOutcome::Quota(a) if a.state == AdmissionState::QuotaPolicyWait
    ));
    let (_, retained, _) = store
        .cancel_quota_admission(&first, DispatchEvidence::NeverDispatched)
        .unwrap();
    assert!(retained.is_empty());
    assert!(store
        .quota_attempts("anthropic", "sub")
        .unwrap()
        .iter()
        .all(|attempt| attempt.request_id != first.request_id));
    let retried = store
        .admit_quota_request(&policy, &decision, third_id, &availability(), now())
        .unwrap();
    assert!(matches!(
        &retried.outcome,
        AdmissionOutcome::Quota(a) if a.state == AdmissionState::Admitted
    ));
    let _ = second;
    std::fs::remove_dir_all(dir).unwrap();
}

/// Provenance is by recorded bytes: a forecast without its snapshot, a
/// decision without its policy revision or its cited forecast, and an
/// admission whose policy/decision are not the recorded rows are all
/// rejected.
#[test]
fn unrecorded_provenance_is_rejected() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    let snap = snapshot(10);
    let forecast = forecast(
        &snap,
        &[],
        &HashMap::new(),
        now(),
        ForecastOptions { ttl: chrono::Duration::minutes(5), stale: StaleUse::Refuse },
    );
    // Forecast without recorded source snapshot.
    assert!(store.record_quota_forecast(&forecast).is_err());
    store.record_quota_snapshot(&snap).unwrap();
    store.record_quota_forecast(&forecast).unwrap();
    // Decision without its policy revision.
    let decision = decide(&policy, std::slice::from_ref(&forecast), &availability(), None, now()).unwrap();
    assert!(store.record_quota_decision(&decision).is_err());
    store.record_quota_policy(&policy, now()).unwrap();
    // Decision citing a forecast that is not recorded.
    let mut orphan = decision.clone();
    orphan.based_on_forecasts = vec![EntityId::new()];
    assert!(store.record_quota_decision(&orphan).is_err());
    store.record_quota_decision(&decision).unwrap();
    // Admission must present the recorded policy bytes, not a lookalike.
    let mut drifted = policy.clone();
    drifted.revision = 2;
    assert!(store
        .admit_quota_request(&drifted, &decision, EntityId::new(), &availability(), now())
        .is_err());
    let mut drifted_decision = decision.clone();
    drifted_decision.decision_id = EntityId::new();
    assert!(store
        .admit_quota_request(&policy, &drifted_decision, EntityId::new(), &availability(), now())
        .is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

/// A conclusive retry of the same sealed request records a further attempt
/// row; settling binds measured cost to the exact attempt commit.
#[test]
fn redispatch_and_settle_track_each_wire_attempt() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 10);
    let admission = admitted(&store, &policy, &decision);
    let (armed, first_commit) = store.arm_quota_admission(&admission).unwrap();
    // Redispatch without an armed reservation is rejected.
    let stray = {
        let mut other = admission.clone();
        other.request_id = EntityId::new();
        other
    };
    assert!(store.record_quota_redispatch(&stray).is_err());
    // The real retry adds a distinct attempt commit, still unknown billing.
    let second_commit = store.record_quota_redispatch(&armed).unwrap();
    let attempts = store.quota_attempts("anthropic", "sub").unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].ledger_commit, first_commit);
    assert_eq!(attempts[1].ledger_commit, second_commit);
    assert!(attempts.iter().all(|a| a.billing == AttemptBilling::Unknown));
    // Settle the retry row at its own commit; a stale seq is not found.
    assert!(store
        .settle_quota_attempt(
            admission.request_id,
            "api:requests",
            LedgerSeq(first_commit.seq.0 + 99),
            Range::point(1.0).unwrap(),
            AttemptBilling::Settled,
        )
        .is_err());
    store
        .settle_quota_attempt(
            admission.request_id,
            "api:requests",
            second_commit.seq,
            Range::point(1.0).unwrap(),
            AttemptBilling::Settled,
        )
        .unwrap();
    let settled = store.quota_attempts("anthropic", "sub").unwrap();
    assert_eq!(settled[1].billing, AttemptBilling::Settled);
    assert_eq!(settled[0].billing, AttemptBilling::Unknown);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Pacer history is append-only discounting: a newer snapshot covering the
/// armed attempt lets the next decision stop double counting, while the
/// reservation record itself remains intact.
#[test]
fn coverage_proof_discounts_but_never_erases_reservation() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 1);
    let admission = admitted(&store, &policy, &decision);
    let (armed, bound) = store.arm_quota_admission(&admission).unwrap();
    // Bucket had room for exactly one: the next admission must wait.
    let waiting = store
        .admit_quota_request(&policy, &decision, EntityId::new(), &availability(), now())
        .unwrap();
    assert!(matches!(
        &waiting.outcome,
        AdmissionOutcome::Quota(a) if a.state == AdmissionState::QuotaPolicyWait
    ));
    // A newer snapshot whose provider view already covers our armed commit
    // frees the budget for a fresh decision without touching the record.
    let mut covered = snapshot(10);
    covered.covered_through = Some(shenko_quota::CoveredThrough {
        local_ledger_commit: bound,
    });
    store.record_quota_snapshot(&covered).unwrap();
    let covered_forecast = forecast(
        &covered,
        &store.quota_attempts("anthropic", "sub").unwrap(),
        &HashMap::new(),
        now(),
        ForecastOptions { ttl: chrono::Duration::minutes(5), stale: StaleUse::Refuse },
    );
    store.record_quota_forecast(&covered_forecast).unwrap();
    let next = decide(&policy, &[covered_forecast], &availability(), Some(&decision), now()).unwrap();
    store.record_quota_decision(&next).unwrap();
    let admitted_next = store
        .admit_quota_request(&policy, &next, EntityId::new(), &availability(), now())
        .unwrap();
    assert!(matches!(
        &admitted_next.outcome,
        AdmissionOutcome::Quota(a) if a.state == AdmissionState::Admitted
    ));
    let retained = pacer_reservation(&store, admission.request_id);
    assert_eq!(retained.admission, armed);
    assert_eq!(retained.attempts[0].ledger_commit, bound);
    std::fs::remove_dir_all(dir).unwrap();
}

fn initialized_subject() -> (PathBuf, Arc<Store>, EntityId) {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;

    let dir = tmp_dir();
    let store = Arc::new(Store::open(dir.join("head.db"), dir.join("cas")).unwrap());
    let agent_id = EntityId::new();
    let branch_id = BranchId::new();
    let ledger_id = store.ledger.ledger_id();
    let dummy_ref = store
        .cas
        .seal_object(b"seed", "application/json", "test", "raw", 1)
        .unwrap()
        .object;
    let genesis_commit = CommitRef::new(ledger_id, LedgerSeq(0));
    let genesis = SubjectGenesis {
        genesis_id: EntityId::new(),
        subject_id: agent_id,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: dummy_ref.clone(),
        initial_profile_ref: dummy_ref.clone(),
        home_manifest_ref: dummy_ref.clone(),
        bootstrap_event_ref: dummy_ref.clone(),
        created_commit: genesis_commit,
    };
    let first_step_id = EntityId::new();
    let head = SubjectBranchHead {
        agent_id,
        branch_id: branch_id.clone(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef { ledger_id, genesis_id: genesis.genesis_id },
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
        branch_id,
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
    store.initialize_genesis(&genesis, &head, &first_step).unwrap();
    (dir, store, agent_id)
}

/// Drive a planned step to Preparing with a valid sealed S0, matching the
/// store_roundtrip fixture's context binding.
fn preparing_step(
    store: &Arc<Store>,
    head: &SubjectBranchHead,
    mut step: SubjectStep,
) -> SubjectStep {
    use shenko_types::ids::EntityId;
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(Arc::clone(store), EntityId(head.branch_id.0)),
    ));
    let profile = shenko_types::object::Digest::blake3(b"quota-store-profile");
    ledger.rebind_model(1, &profile.0).unwrap();
    let window = ledger.current().unwrap();
    store.stage_subject_inputs(head, &step, &[]).unwrap();
    let plan = serde_json::json!({
        "schema_version":10, "step_id":step.step_id, "step_no":step.step_no, "events":[],
        "provider_binding":null, "semantic_wait":null, "identity":null,
        "identity_provider_revision":"shenko.identity/default@1",
        "context":{"source":{"scope_id":EntityId(head.branch_id.0),
            "source_commit_seq":store.current_commit_seq().unwrap(),
            "leaf_seq":store.session_leaf_seq(EntityId(head.branch_id.0)).unwrap(),
            "input_raw_event_ids":[], "window_id":window.id,
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
        "payload":{"system_prompt":"quota store fixture", "messages":[],
            "max_tokens":4096, "tools":[]},
    });
    step.state = SubjectStepState::Preparing;
    step.record_revision = RecordRevision(1);
    step.input_plan_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&plan).unwrap(),
                "application/json",
                "input_plans",
                "subject_step_input",
                10,
            )
            .unwrap()
            .object,
    );
    step.context_revision = Some(1);
    store
        .upsert_step(&step, head, RecordRevision(0))
        .unwrap();
    step
}
