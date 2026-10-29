//! C18 durable quota ↔ Store lifecycle: typed admission persisted on the
//! mutation line, armed reservations surviving reopen, and the AwaitingModel
//! CAS refusing requests with no armed reservation. Synthetic policies and
//! quota values only; no provider/account traffic is claimed.
use shenko_quota::{
    decide, forecast, from_api_headers, AdmissionOutcome, AdmissionState,
    Attribution, BackendState, DispatchEvidence, ForecastOptions, ModelProfile, ObservationContext,
    PolicyDecision, QuotaPolicy, QuotaSnapshot, Range, SourceKind, SourceObservationPolicy,
    StaleUse, UnknownQuota, WindowCost, WindowRef,
};
use shenko_store::store::Store;
use shenko_types::ids::{BranchId, EntityId, RecordRevision};
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

/// Admit and return (sealed request id, admission): the send's admission
/// identity and the immutable request it charges are distinct, and every
/// caller binds both.
fn admitted(
    store: &Store,
    policy: &QuotaPolicy,
    decision: &PolicyDecision,
) -> (EntityId, shenko_quota::QuotaAdmission) {
    let request_id = EntityId::new();
    let send_id = EntityId::new();
    let receipt = store
        .admit_quota_send(request_id, policy, decision, send_id, &availability(), now())
        .unwrap();
    match receipt.outcome {
        AdmissionOutcome::Quota(admission) if admission.state == AdmissionState::Admitted => {
            (request_id, admission)
        }
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

/// Armed reservations keep their exact commit identity and bound attempt
/// rows across a Store reopen.
#[test]
fn armed_reservation_survives_reopen() {
    let dir = tmp_dir();
    let store = Arc::new(Store::open(dir.join("quota.db"), dir.join("cas")).unwrap());
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 10);
    let (sealed, admission) = admitted(&store, &policy, &decision);
    let (armed, arm_commit) = store.arm_quota_send(sealed, &admission).unwrap();
    assert_eq!(armed.state, AdmissionState::Admitted);
    let reservation = pacer_reservation(&store, admission.request_id);
    assert_eq!(reservation.attempts.len(), 1);
    let bound = reservation.attempts[0].ledger_commit;
    assert_eq!(bound, arm_commit);
    assert_eq!(reservation.outstanding.len(), 1);
    assert_eq!(
        store.quota_send_ids(sealed).unwrap(),
        vec![(admission.request_id, 0)]
    );
    drop(store);

    // Reopen: durable reservation and its bound commit survive verbatim.
    let reopened = Arc::new(Store::open(dir.join("quota.db"), dir.join("cas")).unwrap());
    let retained = pacer_reservation(&reopened, admission.request_id);
    assert_eq!(retained.admission, armed);
    assert_eq!(retained.attempts[0].ledger_commit, bound);
    assert_eq!(retained.dispatch, Some(DispatchEvidence::ArmedOrUnknown));
    assert_eq!(
        reopened.quota_send_ids(sealed).unwrap(),
        vec![(admission.request_id, 0)]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A never-dispatched cancellation cannot downgrade armed evidence: it is
/// rejected before any mutation and the armed charge stays.
#[test]
fn armed_cancel_rejects_contradictory_never_dispatched() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 10);
    let (sealed, admission) = admitted(&store, &policy, &decision);
    let (armed, _) = store.arm_quota_send(sealed, &admission).unwrap();
    assert!(store
        .cancel_quota_admission(&armed, DispatchEvidence::NeverDispatched)
        .is_err());
    let after = pacer_reservation(&store, admission.request_id);
    assert_eq!(after.dispatch, Some(DispatchEvidence::ArmedOrUnknown));
    assert_eq!(after.outstanding[0].amount.upper, 1.0);
    std::fs::remove_dir_all(dir).unwrap();
}

/// The honest armed cancellation retains [0, upper] until coverage; the
/// record is terminal and re-admission only reports it.
#[test]
fn armed_cancel_retains_unknown_charge() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 10);
    let (sealed, admission) = admitted(&store, &policy, &decision);
    let (armed, _) = store.arm_quota_send(sealed, &admission).unwrap();
    let (cancelled, retained_costs, _) = store
        .cancel_quota_admission(&armed, DispatchEvidence::ArmedOrUnknown)
        .unwrap();
    assert_eq!(cancelled.state, AdmissionState::Cancelled);
    assert_eq!(retained_costs[0].amount.lower, 0.0);
    assert_eq!(retained_costs[0].amount.upper, 1.0);
    let replay = store
        .admit_quota_send(sealed, &policy, &decision, admission.request_id, &availability(), now())
        .unwrap();
    assert!(matches!(
        &replay.outcome,
        AdmissionOutcome::Existing { admission }
            if admission.state == AdmissionState::Cancelled
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

/// With no recorded policy the unconfigured provider path stays open: the
/// AwaitingModel CAS needs no quota state at all.
#[test]
fn awaiting_model_cas_open_without_quota_policy() {
    let (dir, store, agent_id) = initialized_subject();
    let (head, planned) = store.load_subject(agent_id).unwrap();
    let preparing = preparing_step(&store, &head, planned);
    let mut awaiting = preparing.clone();
    awaiting.state = SubjectStepState::AwaitingModel;
    awaiting.record_revision = RecordRevision(2);
    awaiting.active_request_id = Some(EntityId::new());
    store
        .upsert_step(&awaiting, &head, preparing.record_revision)
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

/// With a recorded policy the same transition is gated: an unarmed request
/// is refused, and the identical CAS succeeds after admit+arm of that exact
/// request id.
#[test]
fn awaiting_model_cas_requires_armed_reservation() {
    let (dir, store, agent_id) = initialized_subject();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 10);
    let (head, planned) = store.load_subject(agent_id).unwrap();
    let preparing = preparing_step(&store, &head, planned);
    let request_id = EntityId::new();
    let mut blocked = preparing.clone();
    blocked.state = SubjectStepState::AwaitingModel;
    blocked.record_revision = RecordRevision(2);
    blocked.active_request_id = Some(request_id);
    assert!(store
        .upsert_step(&blocked, &head, preparing.record_revision)
        .is_err());
    // Admit and arm one send of this exact request id; the identical CAS
    // succeeds only when it claims that exact send.
    let send_id = EntityId::new();
    let receipt = store
        .admit_quota_send(request_id, &policy, &decision, send_id, &availability(), now())
        .unwrap();
    let AdmissionOutcome::Quota(admission) = receipt.outcome else {
        panic!("expected fresh admission");
    };
    store.arm_quota_send(request_id, &admission).unwrap();
    store
        .upsert_step_claiming_send(&blocked, &head, preparing.record_revision, send_id)
        .unwrap();
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
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
    let (_req1, first) = admitted(&store, &policy, &decision);
    let (_req2, _second) = admitted(&store, &policy, &decision);
    let third_id = EntityId::new();
    let req3 = EntityId::new();
    let waiting = store
        .admit_quota_send(req3, &policy, &decision, third_id, &availability(), now())
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
        .admit_quota_send(req3, &policy, &decision, third_id, &availability(), now())
        .unwrap();
    assert!(matches!(
        &retried.outcome,
        AdmissionOutcome::Quota(a) if a.state == AdmissionState::Admitted
    ));
    std::fs::remove_dir_all(dir).unwrap();
}

/// A decision whose cited forecast was computed under options the policy's
/// source admission does not match is rejected: the forecast chain must run
/// under the authorizing policy's own source TTL and stale-use.
#[test]
fn decision_requires_policy_admitted_source_options() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let snap = snapshot(10);
    store.record_quota_snapshot(&snap).unwrap();
    // Same admitted source kind, but a stale-use the policy refuses.
    let forecast = forecast(
        &snap,
        &[],
        &HashMap::new(),
        now(),
        ForecastOptions { ttl: chrono::Duration::minutes(5), stale: StaleUse::LastKnown },
    );
    store.record_quota_forecast(&forecast).unwrap();
    let decision =
        decide(&policy, std::slice::from_ref(&forecast), &availability(), None, now()).unwrap();
    assert!(store.record_quota_decision(&decision).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

/// An already-admitted request re-evaluated under a DIFFERENT recorded
/// decision reconciles: the armed attempt rows and their commit identity
/// survive, the admission refreshes to the new decision, and no second
/// budget is granted.
#[test]
fn admitted_request_reconciles_under_refreshed_decision() {
    let dir = tmp_dir();
    let store = Store::open(dir.join("quota.db"), dir.join("cas")).unwrap();
    let policy = policy();
    store.record_quota_policy(&policy, now()).unwrap();
    let decision = decide_fixture(&store, &policy, 10);
    let (sealed, admission) = admitted(&store, &policy, &decision);
    let (armed, bound) = store.arm_quota_send(sealed, &admission).unwrap();
    // A superseding decision (e.g. recomputed after expiry by another
    // worker) shares the same forecasts but carries a fresh identity.
    let mut refreshed = decision.clone();
    refreshed.decision_id = EntityId::new();
    store.record_quota_decision(&refreshed).unwrap();
    let outcome = store
        .admit_quota_send(sealed, &policy, &refreshed, admission.request_id, &availability(), now())
        .unwrap();
    assert!(matches!(
        &outcome.outcome,
        AdmissionOutcome::Existing { admission }
            if admission.state == AdmissionState::Admitted
                && admission.decision_id == refreshed.decision_id
                && admission.request_id == armed.request_id
    ));
    // The preserved armed binding still verifies its original attempt rows.
    let reservation = pacer_reservation(&store, admission.request_id);
    assert_eq!(reservation.attempts[0].ledger_commit, bound);
    assert_eq!(reservation.dispatch, Some(DispatchEvidence::ArmedOrUnknown));
    let (rearmed, _) = store.arm_quota_send(sealed, &reservation.admission).unwrap();
    assert_eq!(rearmed.state, AdmissionState::Admitted);
    assert_eq!(store.quota_attempts("anthropic", "sub").unwrap().len(), 1);
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
    let (sealed, admission) = admitted(&store, &policy, &decision);
    let (armed, bound) = store.arm_quota_send(sealed, &admission).unwrap();
    // Bucket had room for exactly one: the next admission must wait.
    let waiting = store
        .admit_quota_send(EntityId::new(), &policy, &decision, EntityId::new(), &availability(), now())
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
        .admit_quota_send(EntityId::new(), &policy, &next, EntityId::new(), &availability(), now())
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
        "schema_version":11, "step_id":step.step_id, "step_no":step.step_no, "events":[],
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
