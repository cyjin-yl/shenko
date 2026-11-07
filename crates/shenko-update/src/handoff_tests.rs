//! Regressions for the smoke-admission defect: a candidate whose smoke
//! report did not pass must never become activatable. `prepare` refuses it
//! before any activation record exists, `activate` re-reads the sealed
//! evidence before quiescing, and `rollback` smokes the retained executable
//! under the same rule. In every refusal the working runtime's selector,
//! generation and subject head are exactly what they were before.
//!
//! Contract basis — docs/specs/self-update.md: "`launch_smoke` actually
//! executes the selected candidate artifact against its copy. A
//! missing/malformed manifest, failed command or failed report is an
//! error", and "the previous executable is smoke-launched again against
//! that copy".

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use shenko_types::ids::{BranchId, LedgerSeq};
use shenko_types::subject::{GenesisRef, SubjectGenesis, SubjectStep, SubjectStepState};
use shenko_types::task::{DispatchEligibility, TaskRecord};

use super::*;

/// An explicit local-installation authority: no committed node claim, a
/// present local-branch binding, stable across every re-read so authority
/// comparisons never fire inside these tests.
struct FixtureAuthority;

impl PrimaryClaimSource for FixtureAuthority {
    fn committed_primary_claim(&self) -> Result<Option<PrimaryClaim>> {
        Ok(None)
    }

    fn local_branch_authority(&self) -> Result<Option<serde_json::Value>> {
        Ok(Some(serde_json::json!({ "local_installation": true })))
    }
}

fn authority_json() -> serde_json::Value {
    serde_json::json!({
        "primary_claim": null,
        "local_branch": { "local_installation": true },
    })
}

/// One throwaway installation: a live slot with a real genesis store, the
/// shared CAS, the committed genesis selector at revision 0 and the sealed
/// route authority naming a real retained executable.
struct Installation {
    data_dir: PathBuf,
    live_slot: PathBuf,
    store: Arc<Store>,
    service: UpdateService,
    agent_id: EntityId,
    generation: RuntimeGeneration,
}

impl Installation {
    fn cleanup(self) {
        let dir = self.data_dir.clone();
        drop(self);
        let _ = fs::remove_dir_all(dir);
    }
}

fn seal(store: &Store, value: &serde_json::Value) -> ObjectRef {
    store
        .cas
        .seal_object(
            &serde_json::to_vec(value).unwrap(),
            "application/json",
            "test",
            "json",
            1,
        )
        .unwrap()
        .object
}

fn state_copy_json(slot_dir: &Path, generation: &RuntimeGeneration) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "generation": generation.0,
        "slot_dir": slot_dir.to_string_lossy(),
        "db_path": slot_dir.join(SLOT_DB_FILENAME).to_string_lossy(),
        "watermark": null,
    })
}

fn installation() -> Installation {
    let data_dir = std::env::temp_dir().join(format!("shenko-update-smoke-{}", EntityId::new()));
    let live_slot = data_dir.join("live");
    fs::create_dir_all(&live_slot).unwrap();
    let cas_dir = data_dir.join("cas");
    let store = Arc::new(Store::open(live_slot.join(SLOT_DB_FILENAME), cas_dir).unwrap());
    let ledger_id = store.ledger.ledger_id();
    let generation = RuntimeGeneration::default();

    // The retained executable of the genesis generation: a real program that
    // answers the bounded smoke command for this generation, so a rollback
    // can smoke-launch the retained closure exactly like a forward candidate.
    let executable = write_executable(&data_dir, "shenko-program", &smoke_script(&generation.0));

    let agent_id = EntityId::new();
    let genesis_id = EntityId::new();
    let installation_id = EntityId::new();
    let seed_manifest = seal(
        &store,
        &serde_json::json!({ "kind": "seed_manifest", "complete_product_seed": false }),
    );
    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: seed_manifest.clone(),
        initial_profile_ref: seal(&store, &serde_json::json!({ "kind": "model_profile" })),
        home_manifest_ref: seal(&store, &serde_json::json!({ "kind": "home" })),
        bootstrap_event_ref: seal(
            &store,
            &serde_json::json!({
                "event": "bootstrap",
                "kind": "genesis",
                "installation_id": installation_id,
                "seed_manifest_ref": seed_manifest,
            }),
        ),
        created_commit: CommitRef::new(ledger_id, LedgerSeq(0)),
    };
    let first_step_id = EntityId::new();
    let head = SubjectBranchHead {
        agent_id,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: generation.clone(),
        genesis_ref: GenesisRef { ledger_id, genesis_id },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: CommitRef::new(ledger_id, LedgerSeq(0)),
        next_step_id: first_step_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first_step = SubjectStep {
        step_id: first_step_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: genesis.created_commit,
        controller_epoch: ControllerEpoch(1),
        software_generation: generation.clone(),
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
    let state_ref = seal(&store, &state_copy_json(&live_slot, &generation));
    let route_ref = seal(
        &store,
        &serde_json::to_value(RouteAuthority {
            schema_version: RouteAuthority::SCHEMA_VERSION,
            agent_id,
            generation: generation.clone(),
            active_epoch: 1,
            selector_revision: 0,
            data_dir: data_dir.to_string_lossy().into_owned(),
            slot_dir: live_slot.to_string_lossy().into_owned(),
            artifact_path: executable.to_string_lossy().into_owned(),
            artifact_digest: Digest::blake3(&fs::read(&executable).unwrap()),
            launch_command: vec![
                executable.to_string_lossy().into_owned(),
                "--data-dir".to_string(),
                data_dir.to_string_lossy().into_owned(),
                "start".to_string(),
            ],
            primary_claim: None,
            activation_id: None,
        })
        .unwrap(),
    );
    let selector = ActivationSelector {
        agent_id,
        selector_revision: 0,
        active_epoch: 1,
        generation: generation.clone(),
        state_ref,
        origin: ActivationOrigin::Genesis {
            genesis_ref: GenesisRef { ledger_id, genesis_id },
        },
        route_authority_ref: route_ref,
    };
    let mut root_task = TaskRecord::subject_root(
        head.subject_root_task_id,
        agent_id,
        seed_manifest,
        genesis.created_commit,
    );
    // The delta importer keeps imported work parked and marks it; a copied
    // store whose root task differs from that shape trips the task-closure
    // conflict rule even when the rows are byte-identical. Model the state
    // an installation that already passed one handover carries — the same
    // task identity, parked and marked, so the catch-up reads it as its own
    // earlier delivery rather than a conflicting peer graft.
    root_task.dispatch_eligibility = DispatchEligibility::Parked {
        reason_ref: root_task.task_id,
        until_utc: None,
    };
    root_task.memos.insert(
        "cognition_imported".to_string(),
        serde_json::json!({ "note": "imported work is parked on the receiving store" }),
    );
    store
        .initialize_genesis_with(&genesis, &head, &first_step, &root_task, Some(&selector))
        .unwrap();

    let service = UpdateService::new(
        data_dir.clone(),
        live_slot.clone(),
        store.clone(),
        Arc::new(HandoffParticipantRegistry::for_exclusively_owned_stopped_installation()),
        Arc::new(crate::build::CargoBuildProvider::default()),
    )
    .with_claim_source(Arc::new(FixtureAuthority));
    Installation {
        data_dir,
        live_slot,
        store,
        service,
        agent_id,
        generation,
    }
}

/// The working runtime is still the committed genesis generation: the
/// selector never moved and the subject head is active and unpaused.
fn assert_working_runtime_intact(fixture: &Installation) {
    let selector = fixture
        .store
        .activation_selector(fixture.agent_id)
        .unwrap()
        .expect("the committed selector must still exist");
    assert_eq!(selector.selector_revision, 0);
    assert_eq!(selector.generation, fixture.generation);
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    assert_eq!(head.mode, SubjectMode::Active);
    assert_eq!(head.software_generation, fixture.generation);
}

/// The canonical candidate slot directory for a generation, exactly where
/// `copy_slot` publishes and `activate_inner` opens the candidate store.
fn candidate_slot(fixture: &Installation, generation: &RuntimeGeneration) -> PathBuf {
    slots::SlotLayout::new(fixture.data_dir.clone()).slot_dir(generation)
}

/// An activation record shaped like the ones `prepare` creates, over real
/// sealed references, with a caller-chosen phase and smoke evidence.
fn activation_record(
    fixture: &Installation,
    from_generation: &RuntimeGeneration,
    to_generation: &RuntimeGeneration,
    from_slot: &Path,
    to_slot: &Path,
    phase: ActivationPhase,
    smoke_evidence_ref: Option<ObjectRef>,
) -> ActivationRecord {
    ActivationRecord {
        activation_id: EntityId::new(),
        agent_id: fixture.agent_id,
        from_generation: from_generation.clone(),
        to_generation: to_generation.clone(),
        from_state_ref: seal(&fixture.store, &state_copy_json(from_slot, from_generation)),
        to_state_ref: seal(&fixture.store, &state_copy_json(to_slot, to_generation)),
        migrator_ref: delta_migrator_ref(),
        recovery_closure_ref: seal(
            &fixture.store,
            &serde_json::json!({ "schema_version": 1, "authority": authority_json() }),
        ),
        contract_revision: "shenko.update.activation/v1".to_string(),
        phase,
        w0: None,
        w1: None,
        imported_through: None,
        ingress_cursors_ref: None,
        evidence_cursors_ref: None,
        pending_operations_ref: None,
        inference_allocations_ref: None,
        expected_selector_revision: 0,
        target_epoch: 2,
        smoke_evidence_ref,
        rollback_plan_ref: seal(&fixture.store, &serde_json::json!({ "schema_version": 1, "steps": [] })),
        last_error_ref: None,
        record_revision: 0,
        expected_primary_claim: None,
    }
}

fn smoke_report(
    generation: &RuntimeGeneration,
    passed: bool,
    exit_code: i32,
    checks: Vec<smoke::SmokeCheckOutcome>,
) -> SmokeReport {
    SmokeReport {
        schema_version: smoke::SMOKE_SCHEMA_VERSION,
        generation: generation.0.clone(),
        checks,
        passed,
        exit_code,
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        launched_at: "2026-11-02T00:00:00Z".to_string(),
    }
}

fn write_executable(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

/// A real program that answers the bounded candidate smoke command for one
/// generation: it prints the report the candidate itself would print and
/// exits zero. It never drives a subject or sends an outbox message.
fn smoke_script(generation: &str) -> String {
    let report = serde_json::json!({
        "schema_version": smoke::SMOKE_SCHEMA_VERSION,
        "generation": generation,
        "checks": [{ "name": "state_copy", "passed": true, "detail": null }],
        "passed": true,
        "exit_code": 0,
        "stdout_tail": "",
        "stderr_tail": "",
        "launched_at": "2026-11-02T00:00:00Z",
    });
    format!("#!/bin/sh\ncat <<'SHENKO_SMOKE_EOF'\n{report}\nSHENKO_SMOKE_EOF\nexit 0\n")
}

/// A failed smoke outcome — non-zero exit, no usable report, or an
/// explicit `passed: false` — refuses the prepare before an activation
/// record exists. A missing report is never treated as a pass and a failed
/// report is never fabricated into one.
#[tokio::test]
#[cfg(unix)]
async fn prepare_refuses_a_candidate_whose_smoke_did_not_pass() {
    let fixture = installation();
    let failed_report = serde_json::to_string(&smoke_report(
        &RuntimeGeneration::new("gen-failed-report"),
        false,
        0,
        vec![smoke::SmokeCheckOutcome {
            name: "state_copy".to_string(),
            passed: false,
            detail: Some("the candidate state copy did not open".to_string()),
        }],
    ))
    .unwrap();
    let report_path = fixture.data_dir.join("failed-report.json");
    fs::write(&report_path, &failed_report).unwrap();
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "nonzero-exit-no-report",
            "#!/bin/sh\nexit 3\n".to_string(),
            "exit code 3",
        ),
        (
            "unreadable-report",
            "#!/bin/sh\necho not-a-report\nexit 0\n".to_string(),
            "did not print a readable smoke report",
        ),
        (
            "failed-report",
            format!("#!/bin/sh\ncat '{}'\nexit 0\n", report_path.display()),
            "state_copy",
        ),
    ];
    for (name, script, detail) in cases {
        let candidate = write_executable(&fixture.data_dir, name, &script);
        let error = fixture
            .service
            .prepare(PrepareRequest {
                agent_id: fixture.agent_id,
                generation: RuntimeGeneration::new(format!("gen-{name}")),
                source: CandidateSource::Prebuilt { path: candidate },
                primary_claim: None,
            })
            .await
            .unwrap_err();
        let UpdateError::SmokeFailed(message) = &error else {
            panic!("expected SmokeFailed for {name}, got {error:?}");
        };
        assert!(
            message.contains(detail),
            "the refusal for {name} must name the failed smoke outcome, got: {message}"
        );
        assert!(
            fixture
                .store
                .activation_records(fixture.agent_id)
                .unwrap()
                .is_empty(),
            "a refused candidate must never create an activation record"
        );
        assert_working_runtime_intact(&fixture);
    }
    fixture.cleanup();
}

/// An activation record that reaches `activate` with a saved smoke report
/// that failed — e.g. written by an older build or by direct durable
/// manipulation — is refused before the subject is quiesced: the phase is
/// not enough, the sealed evidence itself must say the candidate passed.
#[tokio::test]
async fn activate_refuses_a_saved_smoke_report_that_failed() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let evidence = seal(
        &fixture.store,
        &serde_json::to_value(smoke_report(
            &target,
            false,
            2,
            vec![smoke::SmokeCheckOutcome {
                name: "state_copy".to_string(),
                passed: false,
                detail: Some("the candidate state copy did not open".to_string()),
            }],
        ))
        .unwrap(),
    );
    let record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &candidate_slot(&fixture, &target),
        ActivationPhase::Smoke,
        Some(evidence),
    );
    fixture.store.create_activation_record(&record).unwrap();

    let error = fixture
        .service
        .activate(record.activation_id, 0)
        .await
        .unwrap_err();
    let UpdateError::SmokeFailed(message) = &error else {
        panic!("expected SmokeFailed, got {error:?}");
    };
    assert!(
        message.contains("did not pass") && message.contains("state_copy"),
        "the refusal must name the saved smoke outcome, got: {message}"
    );
    let record = fixture
        .store
        .activation_record(record.activation_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        record.phase,
        ActivationPhase::Abandoned,
        "the refused activation is abandoned with its error retained"
    );
    assert!(record.last_error_ref.is_some());
    assert_working_runtime_intact(&fixture);
    fixture.cleanup();
}

/// A Smoke-phase record without any saved report is refused the same way:
/// a missing report is never a pass.
#[tokio::test]
async fn activate_refuses_a_record_without_smoke_evidence() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &candidate_slot(&fixture, &target),
        ActivationPhase::Smoke,
        None,
    );
    fixture.store.create_activation_record(&record).unwrap();

    let error = fixture
        .service
        .activate(record.activation_id, 0)
        .await
        .unwrap_err();
    let UpdateError::SmokeFailed(message) = &error else {
        panic!("expected SmokeFailed, got {error:?}");
    };
    assert!(
        message.contains("no saved smoke report"),
        "the refusal must name the missing report, got: {message}"
    );
    assert_working_runtime_intact(&fixture);
    fixture.cleanup();
}

/// Saved smoke evidence that actually passed is not refused by the gate:
/// the activation proceeds through the real hand-off machinery. The
/// fixture gives the candidate a real slot copy and manifest so the full
/// quiesce → catch-up → selector CAS can commit; if a later phase still
/// fails in this reduced fixture, the refusal is never about smoke.
#[tokio::test]
async fn activate_admits_a_saved_smoke_report_that_passed() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let slot = candidate_slot(&fixture, &target);
    fs::create_dir_all(&slot).unwrap();
    slots::backup_database(
        &fixture.live_slot.join(SLOT_DB_FILENAME),
        &slot.join(SLOT_DB_FILENAME),
    )
    .unwrap();
    let program = fixture.data_dir.join("candidate-b-program");
    fs::write(&program, b"candidate executable fixture\n").unwrap();
    slots::write_slot_manifest(
        &slot,
        &slots::SlotManifest {
            schema_version: slots::SlotManifest::SCHEMA_VERSION,
            generation: target.0.clone(),
            cas_dir: fixture.data_dir.join("cas").to_string_lossy().into_owned(),
            artifact_path: Some(program.to_string_lossy().into_owned()),
            artifact_digest: Some(Digest::blake3(&fs::read(&program).unwrap())),
            copied_through_seq: None,
            created_at: "2026-11-02T00:00:00Z".to_string(),
        },
    )
    .unwrap();
    let evidence = seal(
        &fixture.store,
        &serde_json::to_value(smoke_report(&target, true, 0, vec![])).unwrap(),
    );
    let record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &candidate_slot(&fixture, &target),
        ActivationPhase::Smoke,
        Some(evidence),
    );
    fixture.store.create_activation_record(&record).unwrap();

    let outcome = fixture
        .service
        .activate(record.activation_id, 0)
        .await;
    match outcome {
        Ok(outcome) => {
            assert_eq!(outcome.selector.generation, target);
            assert_eq!(outcome.selector.selector_revision, 1);
            let selector = fixture
                .store
                .activation_selector(fixture.agent_id)
                .unwrap()
                .unwrap();
            assert_eq!(selector, outcome.selector);
        }
        Err(error) => {
            assert!(
                !matches!(error, UpdateError::SmokeFailed(_)),
                "a passed, generation-bound smoke report must not be refused, got {error:?}"
            );
            let record = fixture
                .store
                .activation_record(record.activation_id)
                .unwrap()
                .unwrap();
            assert_ne!(
                record.phase,
                ActivationPhase::Smoke,
                "the activation advanced past the smoke gate"
            );
        }
    }
    fixture.cleanup();
}

/// The retained previous executable is smoke-launched again before any
/// rollback record exists (docs/specs/self-update.md). A rollback candidate
/// that fails its smoke is refused; the selected generation, the selector
/// revision and the resumed subject stay exactly as the working runtime
/// left them.
#[tokio::test]
#[cfg(unix)]
async fn rollback_refuses_a_retained_executable_whose_smoke_did_not_pass() {
    let fixture = installation();
    let previous_generation = RuntimeGeneration::new("gen-a");
    let previous_slot = fixture
        .data_dir
        .join("versions")
        .join(slots::sanitize_generation(&previous_generation));
    fs::create_dir_all(&previous_slot).unwrap();
    slots::backup_database(
        &fixture.live_slot.join(SLOT_DB_FILENAME),
        &previous_slot.join(SLOT_DB_FILENAME),
    )
    .unwrap();
    let retained = write_executable(&fixture.data_dir, "retained-a", "#!/bin/sh\nexit 5\n");
    slots::write_slot_manifest(
        &previous_slot,
        &slots::SlotManifest {
            schema_version: slots::SlotManifest::SCHEMA_VERSION,
            generation: previous_generation.0.clone(),
            cas_dir: fixture.data_dir.join("cas").to_string_lossy().into_owned(),
            artifact_path: Some(retained.to_string_lossy().into_owned()),
            artifact_digest: Some(Digest::blake3(&fs::read(&retained).unwrap())),
            copied_through_seq: None,
            created_at: "2026-11-02T00:00:00Z".to_string(),
        },
    )
    .unwrap();

    // The completed forward activation this rollback walks back. Its own
    // smoke evidence passed; the refusal must come from the rollback smoke
    // of the retained executable.
    let passed_evidence = seal(
        &fixture.store,
        &serde_json::to_value(smoke_report(&fixture.generation, true, 0, vec![])).unwrap(),
    );
    let record = activation_record(
        &fixture,
        &previous_generation,
        &fixture.generation,
        &previous_slot,
        &fixture.live_slot,
        ActivationPhase::Activated,
        Some(passed_evidence),
    );
    fixture.store.create_activation_record(&record).unwrap();

    let error = fixture
        .service
        .rollback(record.activation_id, 0)
        .await
        .unwrap_err();
    let UpdateError::SmokeFailed(message) = &error else {
        panic!("expected SmokeFailed, got {error:?}");
    };
    assert!(
        message.contains("exit code 5") && message.contains("did not print a readable smoke report"),
        "the refusal must name the rollback smoke outcome, got: {message}"
    );
    let records = fixture.store.activation_records(fixture.agent_id).unwrap();
    assert_eq!(
        records.len(),
        1,
        "a refused rollback must never create its activation record"
    );
    assert_eq!(records[0].activation_id, record.activation_id);
    assert_eq!(records[0].phase, ActivationPhase::Activated);
    assert_working_runtime_intact(&fixture);
    fixture.cleanup();
}

/// The live activate/rollback path freezes its request, pauses the subject and
/// hands the final catch-up and the selector CAS to a finite helper. If that
/// helper dies before the CAS, nobody on the old side knows the private request
/// id any more. The next owner of the installation takes the boundary back as
/// the *same* controller generation — no new controller epoch, no second
/// primary, no replayed operation — and keeps the abandoned activation and its
/// stopped-resource receipts as evidence.
#[tokio::test]
async fn reconcile_resumes_the_same_controller_after_an_interrupted_handoff() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let request_id = EntityId::new();
    fixture
        .store
        .request_subject_quiesce(fixture.agent_id, request_id)
        .unwrap();
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    let paused = fixture
        .store
        .pause_subject_at_boundary(&head)
        .unwrap()
        .expect("the clean boundary must pause");
    assert_eq!(paused.mode, SubjectMode::Paused);

    // The frozen live-control request exactly as the control transport seals
    // it, and the activation record that names it.
    let installation_id = fixture
        .store
        .installation_id_for_subject(fixture.agent_id)
        .unwrap();
    let selected = fixture
        .store
        .activation_selector(fixture.agent_id)
        .unwrap()
        .unwrap();
    let frozen = seal(
        &fixture.store,
        &serde_json::json!({
            "schema_version": 1,
            "request_id": request_id,
            "agent_id": fixture.agent_id,
            "installation_id": installation_id,
            "command": { "Activate": { "activation": EntityId::new(), "expected_selector_revision": 0 } },
            "source_selector": selected,
            "source_slot": fixture.live_slot,
            "helper_digest": Digest::blake3(b"helper executable"),
        }),
    );
    let mut record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &candidate_slot(&fixture, &target),
        ActivationPhase::Handoff,
        None,
    );
    record.recovery_closure_ref = seal(
        &fixture.store,
        &serde_json::json!({
            "schema_version": 1,
            "authority": authority_json(),
            "process_handoff_request_ref": frozen,
        }),
    );
    // What the old process actually stopped before it died: debt evidence that
    // must survive the abandonment unchanged.
    record.pending_operations_ref = Some(seal(
        &fixture.store,
        &serde_json::json!({
            "receipts": [
                { "participant": "native-children", "stopped": [] },
                { "participant": "playground-services", "stopped": ["4242:1:digest"] },
            ],
        }),
    ));
    let record = fixture.store.create_activation_record(&record).unwrap();

    let answer = fixture
        .service
        .reconcile_interrupted_handoff(fixture.agent_id)
        .unwrap();
    assert_eq!(
        answer.state,
        HandoffReconciliationState::ResumedSameController,
        "an uncommitted hand-over must be taken back, not abandoned: {answer:?}"
    );
    assert!(answer.resumed);
    assert_eq!(answer.request_id, Some(request_id));
    assert_eq!(answer.activation_id, Some(record.activation_id));
    assert_eq!(answer.activation_phase, Some(ActivationPhase::Abandoned));
    assert!(answer.receipt_ref.is_some(), "the answer is a sealed fact");

    let (head, step) = fixture.store.load_subject(fixture.agent_id).unwrap();
    assert_eq!(head.mode, SubjectMode::Active);
    assert_eq!(
        head.controller_epoch,
        ControllerEpoch(1),
        "resuming the same controller never advances the epoch"
    );
    assert_eq!(head.software_generation, fixture.generation);
    assert_eq!(
        step.state,
        SubjectStepState::Planned,
        "the planned successor is preserved for the real loop"
    );

    let boundary = fixture.store.subject_quiesce(request_id).unwrap().unwrap();
    assert_eq!(boundary.state, SubjectQuiesceState::Resumed);
    assert_eq!(boundary.generation, fixture.generation);
    assert_eq!(boundary.epoch, ControllerEpoch(1));

    let abandoned = fixture
        .store
        .activation_record(record.activation_id)
        .unwrap()
        .unwrap();
    assert_eq!(abandoned.phase, ActivationPhase::Abandoned);
    assert_eq!(
        abandoned.pending_operations_ref, record.pending_operations_ref,
        "the stopped-resource receipts must survive the abandonment"
    );
    assert!(abandoned.last_error_ref.is_some());
    assert_working_runtime_intact(&fixture);

    // A second cold start finds nothing left to do and changes nothing.
    let again = fixture
        .service
        .reconcile_interrupted_handoff(fixture.agent_id)
        .unwrap();
    assert_eq!(again.state, HandoffReconciliationState::NothingToDo);
    assert!(!again.resumed);
    fixture.cleanup();
}

/// An operator's `runtime pause` is a control state, not an interrupted
/// hand-over: reconciliation leaves it exactly as it is.
#[tokio::test]
async fn reconcile_leaves_a_deliberate_pause_alone() {
    let fixture = installation();
    let request_id = EntityId::new();
    fixture
        .store
        .request_subject_quiesce(fixture.agent_id, request_id)
        .unwrap();
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    fixture
        .store
        .pause_subject_at_boundary(&head)
        .unwrap()
        .expect("the clean boundary must pause");

    let answer = fixture
        .service
        .reconcile_interrupted_handoff(fixture.agent_id)
        .unwrap();
    assert_eq!(answer.state, HandoffReconciliationState::NothingToDo);
    assert!(!answer.resumed);
    assert_eq!(answer.request_id, Some(request_id));
    assert!(
        answer.reason.contains("not an open hand-over"),
        "the reason must say why the pause was left alone, got {}",
        answer.reason
    );
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    assert_eq!(head.mode, SubjectMode::Paused);
    assert_eq!(
        fixture
            .store
            .subject_quiesce(request_id)
            .unwrap()
            .unwrap()
            .state,
        SubjectQuiesceState::Paused
    );
    fixture.cleanup();
}

/// A selection that committed before its mirror/fence/publication completed is
/// never undone: the retired writer is fenced, the mirrored selector is
/// published, and the paused old boundary is left closed. Reviving it would
/// create a second primary for a generation that no longer owns the subject.
#[tokio::test]
async fn reconcile_fences_the_retired_writer_of_a_committed_selection() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let request_id = EntityId::new();
    fixture
        .store
        .request_subject_quiesce(fixture.agent_id, request_id)
        .unwrap();
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    fixture
        .store
        .pause_subject_at_boundary(&head)
        .unwrap()
        .expect("the clean boundary must pause");
    let record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &candidate_slot(&fixture, &target),
        ActivationPhase::Handoff,
        None,
    );
    let record = fixture.store.create_activation_record(&record).unwrap();

    // The candidate copy the paused subject was migrated into.
    let slot = candidate_slot(&fixture, &target);
    fs::create_dir_all(&slot).unwrap();
    slots::backup_database(
        &fixture.live_slot.join(SLOT_DB_FILENAME),
        &slot.join(SLOT_DB_FILENAME),
    )
    .unwrap();
    let candidate = Store::open(slot.join(SLOT_DB_FILENAME), fixture.data_dir.join("cas")).unwrap();
    let route = seal(
        &candidate,
        &serde_json::to_value(RouteAuthority {
            schema_version: RouteAuthority::SCHEMA_VERSION,
            agent_id: fixture.agent_id,
            generation: target.clone(),
            active_epoch: 2,
            selector_revision: 1,
            data_dir: fixture.data_dir.to_string_lossy().into_owned(),
            slot_dir: slot.to_string_lossy().into_owned(),
            artifact_path: fixture.data_dir.join("candidate-b").to_string_lossy().into_owned(),
            artifact_digest: Digest::blake3(b"candidate-b"),
            launch_command: Vec::new(),
            primary_claim: None,
            activation_id: Some(record.activation_id),
        })
        .unwrap(),
    );
    let mut copied = candidate
        .activation_record(record.activation_id)
        .unwrap()
        .unwrap();
    let watermark = CommitRef::new(fixture.store.ledger.ledger_id(), LedgerSeq(7));
    copied.w1 = Some(watermark);
    copied.imported_through = Some(watermark);
    let next = ActivationSelector {
        agent_id: fixture.agent_id,
        selector_revision: 1,
        active_epoch: 2,
        generation: target.clone(),
        state_ref: record.to_state_ref.clone(),
        origin: ActivationOrigin::Activation {
            activation_id: record.activation_id,
        },
        route_authority_ref: route,
    };
    // The linearization point commits, then the old process dies before it
    // mirrors the selection or fences its own writer.
    candidate.activate_successor(0, &next, &copied).unwrap();
    drop(candidate);

    let answer = fixture
        .service
        .reconcile_interrupted_handoff(fixture.agent_id)
        .unwrap();
    assert_eq!(
        answer.state,
        HandoffReconciliationState::SelectionCommitted,
        "a committed selection is completed, never undone: {answer:?}"
    );
    assert!(!answer.resumed);

    let selector = fixture
        .store
        .activation_selector(fixture.agent_id)
        .unwrap()
        .unwrap();
    assert_eq!(selector.selector_revision, 1);
    assert_eq!(selector.generation, target);
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    assert_eq!(
        head.mode,
        SubjectMode::Handoff,
        "the retired writer is fenced, never resumed"
    );
    assert_eq!(head.controller_epoch, ControllerEpoch(2));
    assert_eq!(
        head.software_generation, fixture.generation,
        "the retired head keeps its own generation"
    );
    assert_eq!(
        fixture
            .store
            .subject_quiesce(request_id)
            .unwrap()
            .unwrap()
            .state,
        SubjectQuiesceState::Paused,
        "a committed selection must not resume the retired boundary"
    );
    fixture.cleanup();
}

/// The program that would become the selection must still be the bytes its
/// smoke ran against. A candidate that changed on disk after its smoke is
/// refused before any resource is quiesced, so the working runtime is never
/// stopped for a generation nobody qualified.
#[tokio::test]
#[cfg(unix)]
async fn activate_refuses_a_candidate_program_that_changed_after_its_smoke() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let slot = candidate_slot(&fixture, &target);
    fs::create_dir_all(&slot).unwrap();
    slots::backup_database(
        &fixture.live_slot.join(SLOT_DB_FILENAME),
        &slot.join(SLOT_DB_FILENAME),
    )
    .unwrap();
    let program = write_executable(&fixture.data_dir, "candidate-b", &smoke_script(&target.0));
    let digested = Digest::blake3(&fs::read(&program).unwrap());
    slots::write_slot_manifest(
        &slot,
        &slots::SlotManifest {
            schema_version: slots::SlotManifest::SCHEMA_VERSION,
            generation: target.0.clone(),
            cas_dir: fixture.data_dir.join("cas").to_string_lossy().into_owned(),
            artifact_path: Some(program.to_string_lossy().into_owned()),
            artifact_digest: Some(digested),
            copied_through_seq: None,
            created_at: "2026-11-02T00:00:00Z".to_string(),
        },
    )
    .unwrap();
    let evidence = seal(
        &fixture.store,
        &serde_json::to_value(smoke_report(&target, true, 0, vec![])).unwrap(),
    );
    let record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &slot,
        ActivationPhase::Smoke,
        Some(evidence),
    );
    let record = fixture.store.create_activation_record(&record).unwrap();

    // Same path, different bytes: the smoke report no longer describes it.
    fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();

    let error = fixture
        .service
        .activate(record.activation_id, 0)
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("does not match its prepared artifact digest"),
        "the refusal must name the changed artifact, got: {message}"
    );
    assert_working_runtime_intact(&fixture);
    assert!(
        fixture
            .store
            .open_subject_quiesce(fixture.agent_id)
            .unwrap()
            .is_none(),
        "the refusal happened before the subject was quiesced"
    );
    let stored = fixture
        .store
        .activation_record(record.activation_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.phase,
        ActivationPhase::Smoke,
        "no phase advanced for a candidate whose bytes nobody smoked"
    );
    fixture.cleanup();
}

/// A paused boundary is only resumed when it is provably this installation's
/// own interrupted hand-over. A frozen live request that names another request
/// id is not ours: the boundary stays paused and every record stays as it was.
#[tokio::test]
async fn reconcile_refuses_a_boundary_bound_to_another_frozen_request() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let request_id = EntityId::new();
    fixture
        .store
        .request_subject_quiesce(fixture.agent_id, request_id)
        .unwrap();
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    fixture
        .store
        .pause_subject_at_boundary(&head)
        .unwrap()
        .expect("the clean boundary must pause");
    let selected = fixture
        .store
        .activation_selector(fixture.agent_id)
        .unwrap()
        .unwrap();
    let foreign = seal(
        &fixture.store,
        &serde_json::json!({
            "schema_version": 1,
            "request_id": EntityId::new(),
            "agent_id": fixture.agent_id,
            "installation_id": fixture.store.installation_id_for_subject(fixture.agent_id).unwrap(),
            "command": { "Activate": { "activation": EntityId::new(), "expected_selector_revision": 0 } },
            "source_selector": selected,
            "source_slot": fixture.live_slot,
            "helper_digest": Digest::blake3(b"other helper"),
        }),
    );
    let mut record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &candidate_slot(&fixture, &target),
        ActivationPhase::Handoff,
        None,
    );
    record.recovery_closure_ref = seal(
        &fixture.store,
        &serde_json::json!({
            "schema_version": 1,
            "authority": authority_json(),
            "process_handoff_request_ref": foreign,
        }),
    );
    let record = fixture.store.create_activation_record(&record).unwrap();

    let error = fixture
        .service
        .reconcile_interrupted_handoff(fixture.agent_id)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("is not this boundary"),
        "the refusal must name the mismatched binding, got: {error:#}"
    );
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    assert_eq!(head.mode, SubjectMode::Paused, "a foreign boundary is never resumed");
    assert_eq!(
        fixture
            .store
            .subject_quiesce(request_id)
            .unwrap()
            .unwrap()
            .state,
        SubjectQuiesceState::Paused
    );
    let stored = fixture
        .store
        .activation_record(record.activation_id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.phase, ActivationPhase::Handoff);
    let selector = fixture
        .store
        .activation_selector(fixture.agent_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        selector.selector_revision, 0,
        "a refused boundary never moves the selector"
    );
    assert_eq!(selector.generation, fixture.generation);
    fixture.cleanup();
}

/// A head that already carries the retired generation's fence is never
/// revived: that copy has lost the right to admit anything.
#[tokio::test]
async fn reconcile_refuses_to_revive_a_fenced_retired_writer() {
    let fixture = installation();
    let request_id = EntityId::new();
    fixture
        .store
        .request_subject_quiesce(fixture.agent_id, request_id)
        .unwrap();
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    let paused = fixture
        .store
        .pause_subject_at_boundary(&head)
        .unwrap()
        .expect("the clean boundary must pause");
    let reason = seal(&fixture.store, &serde_json::json!({ "activation_id": EntityId::new() }));
    let fenced = fixture
        .store
        .fence_head_for_handoff(&paused, paused.record_revision, &reason)
        .unwrap();
    assert_eq!(fenced.mode, SubjectMode::Handoff);

    let error = fixture
        .service
        .reconcile_interrupted_handoff(fixture.agent_id)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("HANDOFF_NOT_RESUMABLE"),
        "a fenced writer must be refused explicitly, got: {error:#}"
    );
    let (head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    assert_eq!(head.mode, SubjectMode::Handoff);
    assert_eq!(head.controller_epoch, fenced.controller_epoch);
    fixture.cleanup();
}

/// The forward hand-over really commits: real slot copy with its own ledger
/// writer identity, real typed catch-up of the paused boundary, the selector
/// CAS with a strictly larger epoch, a clean successor for the new controller,
/// the retired writer's fence and the launcher pointer. The retired copy can no
/// longer admit anything, and the selected generation owns the subject.
#[tokio::test]
#[cfg(unix)]
async fn activate_commits_a_new_generation_and_fences_the_retired_writer() {
    let fixture = installation();
    let target = RuntimeGeneration::new("gen-b");
    let live = AbSlot {
        generation: fixture.generation.clone(),
        slot_dir: fixture.live_slot.clone(),
        db_path: fixture.live_slot.join(SLOT_DB_FILENAME),
        cas_dir: fixture.data_dir.join("cas"),
        artifact_path: None,
        artifact_digest: None,
        is_active: true,
    };
    // Exactly what `prepare` does: an independent slot whose copied state has
    // its own ledger writer identity, then the candidate program.
    let slot = slots::copy_slot(&live, &SlotLayout::new(fixture.data_dir.clone()), &target).unwrap();
    let program = write_executable(&fixture.data_dir, "candidate-b", &smoke_script(&target.0));
    let artifact_digest = Digest::blake3(&fs::read(&program).unwrap());
    slots::write_slot_manifest(
        &slot.slot_dir,
        &slots::SlotManifest {
            schema_version: slots::SlotManifest::SCHEMA_VERSION,
            generation: target.0.clone(),
            cas_dir: fixture.data_dir.join("cas").to_string_lossy().into_owned(),
            artifact_path: Some(program.to_string_lossy().into_owned()),
            artifact_digest: Some(artifact_digest),
            copied_through_seq: None,
            created_at: "2026-11-02T00:00:00Z".to_string(),
        },
    )
    .unwrap();
    let evidence = seal(
        &fixture.store,
        &serde_json::to_value(smoke_report(&target, true, 0, vec![])).unwrap(),
    );
    let record = activation_record(
        &fixture,
        &fixture.generation,
        &target,
        &fixture.live_slot,
        &slot.slot_dir,
        ActivationPhase::Smoke,
        Some(evidence),
    );
    let record = fixture.store.create_activation_record(&record).unwrap();

    let outcome = fixture
        .service
        .activate(record.activation_id, 0)
        .await
        .expect("the hand-over must commit");
    assert_eq!(outcome.selector.selector_revision, 1);
    assert_eq!(outcome.selector.generation, target);
    assert_eq!(
        outcome.selector.active_epoch,
        ControllerEpoch(2),
        "the controller epoch advances exactly once per hand-over"
    );
    assert_eq!(outcome.fenced_head.mode, SubjectMode::Handoff);
    assert_eq!(outcome.fenced_head.controller_epoch, ControllerEpoch(2));

    let retired_selector = fixture
        .store
        .activation_selector(fixture.agent_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        retired_selector, outcome.selector,
        "the retired copy mirrors the committed selection"
    );
    let (retired_head, _) = fixture.store.load_subject(fixture.agent_id).unwrap();
    assert_eq!(retired_head.mode, SubjectMode::Handoff);
    assert_eq!(retired_head.software_generation, fixture.generation);
    assert_eq!(retired_head.controller_epoch, ControllerEpoch(2));

    let selected = Store::open(
        slot.slot_dir.join(SLOT_DB_FILENAME),
        fixture.data_dir.join("cas"),
    )
    .unwrap();
    let (new_head, new_step) = selected.load_subject(fixture.agent_id).unwrap();
    assert_eq!(new_head.mode, SubjectMode::Active);
    assert_eq!(new_head.software_generation, target);
    assert_eq!(new_head.controller_epoch, ControllerEpoch(2));
    assert_eq!(new_head.next_step_id, new_step.step_id);
    assert_eq!(new_step.state, SubjectStepState::Planned);
    assert!(
        selected.open_subject_quiesce(fixture.agent_id).unwrap().is_none(),
        "the successor starts from a closed boundary, not a paused one"
    );
    let pointer: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.data_dir.join(slots::CURRENT_SLOT_FILENAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        pointer["slot_dir"],
        serde_json::json!(slot.slot_dir),
        "the launcher pointer follows the selection"
    );
    // The selected generation produced a launch-bound receipt of its own: the
    // successor, not the helper, records readiness for this selector.
    assert!(
        selected
            .activation_launch_for_selector(fixture.agent_id, 1)
            .unwrap()
            .is_none(),
        "no launch receipt exists before a process actually starts the selection"
    );
    fixture.cleanup();
}
