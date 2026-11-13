//! S1 验证：SQLite WAL store round-trip + CAS seal/read
//! 验证 docs/specs/durable-state.md#spec-41-2: 单 mutation line, CAS object seal/read, FTS5 detection

use shenko_store::store::Store;
use shenko_types::store::WriteBatch;
use std::path::PathBuf;
use std::sync::Arc;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shenko-test-{}",
        shenko_types::ids::EntityId::new()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn store_opens_with_wal_and_fts5_detection() {
    let dir = tmp_dir();
    let db = dir.join("test.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    assert!(store.capabilities.wal_mode, "WAL mode should be enabled");
    assert_eq!(store.capabilities.synchronous, "2", "synchronous=FULL");
    // FTS5 may or may not be available depending on build; just check it's detected
    let _ = store.capabilities.fts5_available;
}

#[test]
fn cas_seal_and_read_roundtrip() {
    let dir = tmp_dir();
    let db = dir.join("cas_test.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let data = b"hello shenko cas";
    let sealed = store
        .cas
        .seal_object(data, "text/plain", "test", "raw", 1)
        .unwrap();

    assert_eq!(sealed.object.bytes, 16);
    assert_eq!(sealed.object.media_type, "text/plain");

    // Read back
    let read = store.cas.read_object(&sealed.object.digest).unwrap();
    assert_eq!(read, data);

    // Exists
    assert!(store.cas.object_exists(&sealed.object.digest));
}

#[test]
fn empty_batch_commit_returns_receipt() {
    let dir = tmp_dir();
    let db = dir.join("empty.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let batch = WriteBatch::new();
    let receipt = store.commit(batch).unwrap();

    assert_eq!(
        receipt.durability_profile,
        shenko_types::commit::DurabilityProfile::PowerLossLocal
    );
    assert!(!receipt.commit_ref.ledger_id.as_uuid().is_nil());
}

fn initialized_subject() -> (PathBuf, Arc<Store>, shenko_types::ids::EntityId) {
    initialized_subject_with_bootstrap_event(b"seed")
}

fn initialized_subject_with_bootstrap_event(
    bootstrap_event: &[u8],
) -> (PathBuf, Arc<Store>, shenko_types::ids::EntityId) {
    let dir = tmp_dir();
    let db = dir.join("head.db");
    let cas = dir.join("cas");
    let store = Arc::new(Store::open(db, cas).unwrap());
    let agent_id = initialize_subject_in(&store, bootstrap_event);
    (dir, store, agent_id)
}

fn initialize_subject_in(store: &Store, bootstrap_event: &[u8]) -> shenko_types::ids::EntityId {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
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
        .seal_object(bootstrap_event, "application/json", "test", "raw", 1)
        .unwrap()
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

    // Initialize genesis atomically
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent_id,
        dummy_ref,
        genesis_commit,
    );
    store
        .initialize_genesis(&genesis, &head, &first_step, &root_task)
        .unwrap();

    agent_id
}

#[test]
fn bootstrap_identity_survives_reopen_without_an_index_and_rejects_divergence() {
    use shenko_types::{error::ShenkoError, ids::EntityId};
    let installation = EntityId::new();
    let event = serde_json::to_vec(&serde_json::json!({"installation_id": installation})).unwrap();
    let (dir, store, agent) = initialized_subject_with_bootstrap_event(&event);
    assert_ne!(installation, store.ledger.ledger_id());
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    assert_eq!(reopened.installation_id_for_subject(agent).unwrap(), installation);

    let genesis = reopened.get_subject_genesis(agent).unwrap().unwrap();
    let foreign = EntityId::new();
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    conn.execute(
        "INSERT INTO bootstrap_records(bootstrap_id,installation_id,subject_id,data,updated_commit_seq) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![genesis.bootstrap_id.to_string(), foreign.to_string(), agent.to_string(),
            serde_json::json!({"installation_id": foreign}).to_string(), genesis.created_commit.seq.0 as i64],
    ).unwrap();
    assert!(matches!(reopened.installation_id_for_subject(agent), Err(ShenkoError::Conflict(_))));
    assert_eq!(reopened.cas.read_object(&genesis.bootstrap_event_ref.digest).unwrap(), event);
}

#[test]
fn bootstrap_owner_projection_resolves_the_committed_grant_after_reopen() {
    use shenko_store::config::OwnerGrant;
    use shenko_types::{ids::EntityId, object::Digest};
    let dir = tmp_dir();
    let store = Store::open(dir.join("owner.db"), dir.join("cas")).unwrap();
    let installation = EntityId::new();
    let principal = store.seal_config_object(&serde_json::json!({"kind": "principal"}), "principal").unwrap();
    let authority = store.seal_config_object(&serde_json::json!({"kind": "authority"}), "authority").unwrap();
    let (_, granted) = store.claim_owner(installation, EntityId::new(), principal, authority,
        Digest::blake3(b"owner-projection-test-token")).unwrap();
    drop(store);
    let reopened = Store::open(dir.join("owner.db"), dir.join("cas")).unwrap();
    let reference = reopened.get_bootstrap_owner_grant(installation).unwrap().unwrap();
    let bytes = std::fs::read(reopened.cas.verified_object_path(&reference).unwrap()).unwrap();
    let projected: OwnerGrant = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(serde_json::to_value(projected).unwrap(), serde_json::to_value(granted.unwrap()).unwrap());
}

#[test]
fn boundary_waits_for_recorded_step_and_survives_reopen() {
    use shenko_store::runtime_boundary::SubjectQuiesceState;
    use shenko_types::{ids::*, subject::*};

    let (dir, store, agent) = initialized_subject();
    let (head, step) = store.load_subject(agent).unwrap();
    let recorded = recorded_result(&store, &head, step);
    let request_id = EntityId::new();
    let request = store.request_subject_quiesce(agent, request_id).unwrap();
    assert_eq!(request.state, SubjectQuiesceState::Requested);
    assert!(store.pause_subject_at_boundary(&head).unwrap().is_none());
    assert_eq!(store.load_subject(agent).unwrap().1.state, SubjectStepState::ResultRecorded);

    let successor = EntityId::new();
    let candidate = committed_candidate(&store, recorded, successor);
    store.commit_step_and_advance_head(&candidate, &head, successor).unwrap();
    let (current, planned) = store.load_subject(agent).unwrap();
    let paused = store.pause_subject_at_boundary(&current).unwrap().unwrap();
    assert_eq!(paused.mode, SubjectMode::Paused);
    assert_eq!(paused.next_step_id, planned.step_id);
    assert!(store.export_cognition(&Default::default()).is_err(),
        "an archive must not omit the local pause record and claim a complete paused head");
    let mut stale_step = planned.clone();
    stale_step.state = SubjectStepState::Preparing;
    stale_step.record_revision = stale_step.record_revision.next();
    assert!(store.upsert_step(&stale_step, &current, planned.record_revision).is_err());
    let boundary = store.subject_quiesce(request_id).unwrap().unwrap();
    assert_eq!(boundary.state, SubjectQuiesceState::Paused);
    assert!(boundary.boundary_at.unwrap().seq > request.requested_at.seq);
    drop(store);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    let (retained, step) = reopened.load_subject(agent).unwrap();
    assert_eq!(retained.mode, SubjectMode::Paused);
    assert_eq!(step.step_id, successor);
    let closed = reopened.resume_subject_quiesce(agent, request_id).unwrap();
    assert_eq!(closed.state, SubjectQuiesceState::Resumed);
    assert!(closed.closed_at.unwrap().seq > boundary.boundary_at.unwrap().seq);
    let (active, step) = reopened.load_subject(agent).unwrap();
    assert_eq!(active.mode, SubjectMode::Active);
    assert_eq!(active.controller_epoch, current.controller_epoch);
    assert_eq!(active.software_generation, current.software_generation);
    assert_eq!(step.step_id, successor);
    assert!(reopened.export_cognition(&Default::default()).is_ok());
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn peer_cannot_advance_successor_of_a_locally_paused_head() {
    use shenko_store::cognition::CognitionImportOptions;
    use shenko_types::ids::EntityId;
    use shenko_types::subject::{SubjectMode, SubjectStepState};

    let (_source_dir, source, agent) = initialized_subject();
    let receiver_dir = tmp_dir();
    let receiver = Store::open(receiver_dir.join("receiver.db"), receiver_dir.join("cas")).unwrap();
    let peer = EntityId::new();
    let options = CognitionImportOptions { peer_id: Some(peer), ..Default::default() };
    let mut initial = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut initial).unwrap();
    assert!(receiver.import_cognition_archive(&initial[..], &options).unwrap().ack(peer).verified);
    let (local, successor) = receiver.load_subject(agent).unwrap();
    let request = EntityId::new();
    receiver.request_subject_quiesce(agent, request).unwrap();
    let paused = receiver.pause_subject_at_boundary(&local).unwrap().unwrap();
    assert_eq!(paused.mode, SubjectMode::Paused);
    let before = receiver.current_commit_seq().unwrap();

    // A real source Store advances its existing S0/result while the receiver
    // retains a different local head. The old row-by-row merge could apply the
    // source step despite keeping the conflicting paused head.
    let (source_head, source_step) = source.load_subject(agent).unwrap();
    let recorded = recorded_result(&source, &source_head, source_step);
    assert_eq!(recorded.state, SubjectStepState::ResultRecorded);
    let mut newer = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut newer).unwrap();
    assert!(receiver.import_cognition_archive(&newer[..], &options).is_err());
    assert_eq!(receiver.current_commit_seq().unwrap(), before);
    let (retained, step) = receiver.load_subject(agent).unwrap();
    assert_eq!(serde_json::to_value(&retained).unwrap(), serde_json::to_value(&paused).unwrap());
    assert_eq!(serde_json::to_value(&step).unwrap(), serde_json::to_value(&successor).unwrap());
    receiver.resume_subject_quiesce(agent, request).unwrap();
    assert_eq!(receiver.load_subject(agent).unwrap().1.state, SubjectStepState::Planned);
}


fn recorded_result(
    store: &Arc<Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    step: shenko_types::subject::SubjectStep,
) -> shenko_types::subject::SubjectStep {
    recorded_result_with_wait(store, head, step, None)
}

/// The frozen S0 input of one step, with the semantic wait snapshot the model
/// was shown.
fn recorded_result_with_wait(
    store: &Arc<Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    step: shenko_types::subject::SubjectStep,
    semantic_wait: Option<shenko_types::subject::SemanticWait>,
) -> shenko_types::subject::SubjectStep {
    recorded_result_with_docket(store, head, step, semantic_wait, &[])
}

/// The frozen S0 input of one step, including the reconciliation docket the
/// model was actually shown.
///
/// A docket makes the plan schema 13; without one the plan stays schema 12,
/// so existing fixtures keep their exact sealed bytes. S1 only accepts an
/// acknowledgement for a case carried in THIS docket.
fn recorded_result_with_docket(
    store: &Arc<Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    mut step: shenko_types::subject::SubjectStep,
    semantic_wait: Option<shenko_types::subject::SemanticWait>,
    reconciliation_docket: &[serde_json::Value],
) -> shenko_types::subject::SubjectStep {
    use shenko_types::{ids::*, subject::*};
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(Arc::clone(store), EntityId(head.branch_id.0)),
    ));
    let profile = shenko_types::object::Digest::blake3(b"store-roundtrip-profile");
    ledger.rebind_model(1, &profile.0).unwrap();
    let window = ledger.current().unwrap();
    store.stage_subject_inputs(head, &step, &[]).unwrap();
    let plan_schema = if reconciliation_docket.is_empty() { 12 } else { 13 };
    let plan = serde_json::json!({
        "schema_version":plan_schema, "reconciliation_docket":reconciliation_docket,
        "step_id":step.step_id, "step_no":step.step_no, "events":[], "provider_selection":{"schema_version":1,"binding":null,"model":null,"profile_id":null,"profile_revision":null,"credential_fingerprint":null,"resource_domain_id":null,"serializer_revision":null}, "request_id":"00000000-0000-0000-0000-000000000000",
        "provider_binding":null, "semantic_wait":semantic_wait, "identity":null,
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
        "payload":{"system_prompt":"store recovery fixture", "messages":[],
            "max_tokens":4096, "tools":[]},
    });
    step.state = SubjectStepState::Preparing;
    step.record_revision = RecordRevision(1);
    step.input_plan_ref = Some(store.cas.seal_object(
        &serde_json::to_vec(&plan).unwrap(), "application/json", "input_plans",
        "subject_step_input", plan_schema).unwrap().object);
    store.upsert_step(&step, head, RecordRevision(0)).unwrap();
    step.state = SubjectStepState::AwaitingModel;
    step.record_revision = RecordRevision(2);
    step.active_request_id = Some(EntityId::new());
    store.upsert_step(&step, head, RecordRevision(1)).unwrap();
    step.state = SubjectStepState::ResultRecorded;
    step.record_revision = RecordRevision(3);
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
    store.upsert_step(&step, head, RecordRevision(2)).unwrap();
    step
}

fn committed_candidate(
    store: &Store,
    step: shenko_types::subject::SubjectStep,
    next: shenko_types::ids::EntityId,
) -> shenko_types::subject::SubjectStep {
    committed_candidate_with(store, step, next, vec![])
}

/// A committed S1 candidate that also authored the given outbox intents, the
/// way a real subject step does.
fn committed_candidate_with(
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

/// Attach one semantic wait change to a committed candidate, exactly as the
/// subject does: the parsed control and the policy intent carry the same
/// action id and the same change.
fn attach_wait(
    step: &mut shenko_types::subject::SubjectStep,
    store: &Store,
    change: &shenko_types::subject::SemanticWaitChange,
    action_id: shenko_types::ids::EntityId,
) {
    use shenko_types::output::{OutputAction, OutputControl};
    use shenko_types::subject::{PolicyIntent, PolicyKind, StepDecision};
    let mut decision: StepDecision = serde_json::from_slice(
        &store
            .cas
            .read_object(&step.decision_ref.as_ref().unwrap().digest)
            .unwrap(),
    )
    .unwrap();
    decision.output.controls.push(OutputControl {
        action_id,
        spans: vec![],
        action: OutputAction::SemanticWait {
            change: change.clone(),
        },
    });
    decision.policy_intents.push(PolicyIntent {
        intent_id: action_id,
        kind: PolicyKind::SemanticWait {
            change: change.clone(),
        },
        description: "subject semantic wait".into(),
        target_revision: None,
    });
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "decisions",
                "StepDecision",
                8,
            )
            .unwrap()
            .object,
    );
}

#[test]
fn s1_commit_advances_branch_head() {
    use shenko_types::{ids::*, subject::*};
    let (dir, store, agent_id) = initialized_subject();
    let (head, first_step) = store.load_subject(agent_id).unwrap();
    let first_id = first_step.step_id;
    let ledger_id = store.ledger.ledger_id();
    let genesis_json: String = rusqlite::Connection::open(dir.join("head.db"))
        .unwrap()
        .query_row(
            "SELECT data FROM subject_genesis WHERE subject_id=?1",
            [agent_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    let genesis: SubjectGenesis = serde_json::from_str(&genesis_json).unwrap();
    assert_eq!(head.committed_at, genesis.created_commit);
    assert_eq!(first_step.expected_parent_commit, genesis.created_commit);
    assert!(genesis.created_commit.seq.0 > 0);
    let second_step_id = EntityId::new();
    let result = recorded_result(&store, &head, first_step);
    let step = committed_candidate(&store, result, second_step_id);
    assert!(
        store
            .event_disposition_commit(agent_id, first_id)
            .unwrap()
            .is_none()
    );
    let receipt = store
        .commit_step_and_advance_head(&step, &head, second_step_id)
        .unwrap();
    assert_eq!(receipt.updated.len(), 3);
    assert_eq!(
        receipt
            .updated
            .iter()
            .find(|u| u.entity_id == agent_id)
            .unwrap()
            .record_revision,
        RecordRevision(1)
    );
    drop(store);

    // 真实重开并读回，而不是只检查回执声称写了什么。
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    assert_eq!(reopened.ledger.ledger_id(), ledger_id);
    assert_eq!(
        reopened
            .event_disposition_commit(agent_id, first_id)
            .unwrap(),
        Some(receipt.commit_ref)
    );
    let (restored, successor) = reopened.load_subject(agent_id).unwrap();
    assert_eq!(restored.branch_id, head.branch_id);
    assert_eq!(restored.subject_root_task_id, head.subject_root_task_id);
    assert_eq!(restored.genesis_ref.genesis_id, head.genesis_ref.genesis_id);
    assert_eq!(restored.genesis_ref.ledger_id, head.genesis_ref.ledger_id);
    assert_eq!(restored.controller_epoch, head.controller_epoch);
    assert_eq!(restored.software_generation, head.software_generation);
    assert_eq!(restored.committed_step_no, 1);
    assert_eq!(restored.committed_step_id, Some(first_id));
    assert_eq!(restored.committed_at, receipt.commit_ref);
    assert_eq!(restored.next_step_id, second_step_id);
    assert_eq!(successor.step_id, second_step_id);
    assert_eq!(successor.step_no, 2);
    assert_eq!(successor.state, SubjectStepState::Planned);
    assert_eq!(successor.expected_parent_commit, receipt.commit_ref);
    assert_eq!(successor.created_by_step_id, Some(first_id));
    let committed = reopened
        .get_step(&head.branch_id, first_id)
        .unwrap()
        .unwrap();
    assert_eq!(committed.state, SubjectStepState::Committed);
    assert_eq!(committed.commit_ref, Some(receipt.commit_ref));
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM subject_steps WHERE branch_id=?1 AND step_no=2",
            [head.branch_id.0.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let next = reopened.commit(WriteBatch::new()).unwrap();
    assert!(next.commit_ref.seq.0 > receipt.commit_ref.seq.0);
}

#[test]
fn imported_committed_steps_replay_in_step_order_with_a_one_item_page() {
    use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
    use shenko_types::ids::EntityId;

    let (dir, source, agent) = initialized_subject();
    let (head, first) = source.load_subject(agent).unwrap();
    // The second UUID sorts before every generated v4 UUID. Ordering only by
    // local import sequence + UUID would deterministically return step 2 first.
    let second_id: EntityId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
    let first_id = first.step_id;
    let result = recorded_result(&source, &head, first);
    let first = committed_candidate(&source, result, second_id);
    let first_receipt = source.commit_step_and_advance_head(&first, &head, second_id).unwrap();
    let (head, second) = source.load_subject(agent).unwrap();
    let third_id = EntityId::new();
    let result = recorded_result(&source, &head, second);
    let second = committed_candidate(&source, result, third_id);
    let second_receipt = source.commit_step_and_advance_head(&second, &head, third_id).unwrap();
    let mut archive = Vec::new();
    source.write_cognition_archive(&CognitionSnapshotOptions::default(), None, &mut archive).unwrap();
    let target = Arc::new(Store::open(dir.join("replay.db"), dir.join("replay-cas")).unwrap());
    let local_agent = initialize_subject_in(&target, b"local replay owner");
    let (local_head, local_step) = target.load_subject(local_agent).unwrap();
    let local_step_id = local_step.step_id;
    let local_next = EntityId::new();
    let local_result = recorded_result(&target, &local_head, local_step);
    let local_committed = committed_candidate(&target, local_result, local_next);
    target.commit_step_and_advance_head(&local_committed, &local_head, local_next).unwrap();
    target.import_cognition_archive(&archive[..], &CognitionImportOptions::default()).unwrap();

    assert_eq!(target.get_existing_agent_id().unwrap(), Some(local_agent));
    let local = target.committed_decisions_after(local_agent, 0, None, 64).unwrap();
    assert_eq!(local.iter().map(|(_, step, _)| step.step_id).collect::<Vec<_>>(), [local_step_id]);
    assert!(target.committed_decisions_after(EntityId::new(), 0, None, 64).unwrap().is_empty());
    let first_page = target.committed_decisions_after(agent, 0, None, 1).unwrap();
    assert_eq!(first_page.len(), 1);
    let (local_seq, first, _) = &first_page[0];
    assert_eq!(first.step_id, first_id);
    assert_eq!(first.step_no, 1);
    assert_eq!(first.commit_ref, Some(first_receipt.commit_ref));
    let second_page = target.committed_decisions_after(agent, *local_seq, Some(first.step_id), 1).unwrap();
    assert_eq!(second_page.len(), 1);
    let (next_seq, second, _) = &second_page[0];
    assert_eq!(*next_seq, *local_seq);
    assert_eq!(second.step_id, second_id);
    assert_eq!(second.step_no, 2);
    assert_eq!(second.commit_ref, Some(second_receipt.commit_ref));
    assert!(target.committed_decisions_after(agent, *next_seq, Some(second.step_id), 1).unwrap().is_empty());
}

#[test]
fn imported_owner_inputs_page_through_one_shared_commit_without_loss() {
    use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
    use shenko_store::web_input::{WEB_INPUT_SOURCE, WebAdmission};
    use shenko_types::inbox::{InboxEvent, InboxEventKind};
    use shenko_types::ids::EntityId;
    use shenko_types::store::RawInboxEvent;

    let (dir, source, agent) = initialized_subject();
    let ids = ["cursor-a", "cursor-\u{e000}", "cursor-\u{10000}"];
    for id in ids {
        let payload = serde_json::json!({
            "schema_version": 2, "type": "submit", "subject_id": agent.to_string(),
            "submission_id": id, "message_id": format!("message-{id}"),
            "mode": "append", "content": format!("原始输入 {id}"), "attachment_refs": [],
        });
        let event = InboxEvent {
            event_id: EntityId::new(), kind: InboxEventKind::Message,
            payload: serde_json::json!({"role": "user", "content": payload["content"], "submission_id": id}),
            created_at: chrono::Utc::now(), source: WEB_INPUT_SOURCE.into(),
        };
        let raw = RawInboxEvent {
            event_id: event.event_id, agent_id: agent, source: event.source.clone(),
            kind: "message".into(), payload_json: serde_json::to_string(&event).unwrap(),
            received_at_ms: event.created_at.timestamp_millis(),
        };
        assert!(matches!(source.admit_web_submission(agent, id, &payload.to_string(), &raw).unwrap(),
            WebAdmission::Admitted(_)));
    }
    let mut archive = Vec::new();
    source.write_cognition_archive(&CognitionSnapshotOptions::default(), None, &mut archive).unwrap();
    let target = Store::open(dir.join("owner-history.db"), dir.join("owner-history-cas")).unwrap();
    target.import_cognition_archive(&archive[..], &CognitionImportOptions::default()).unwrap();
    let imported_seq = target.web_submissions_page(agent, None, 10).unwrap();
    assert_eq!(imported_seq.items.len(), 3);
    let seq = imported_seq.items[0].record.accepted_commit.seq.0;
    assert!(imported_seq.items.iter().all(|row| row.record.accepted_commit.seq.0 == seq));
    let mut before = None;
    let mut recovered = Vec::new();
    loop {
        let page = target.web_submissions_page(agent, before.as_ref(), 1).unwrap();
        assert_eq!(page.total, 3);
        recovered.extend(page.items.into_iter().map(|row| row.record.submission_id));
        if !page.has_older { assert_eq!(page.next_before, None); break; }
        before = page.next_before;
    }
    assert_eq!(recovered, ids.into_iter().rev().collect::<Vec<_>>());
    assert_eq!(target.web_submissions_page(EntityId::new(), None, 1).unwrap().total, 0);
    drop(target);
    let reopened = Store::open(dir.join("owner-history.db"), dir.join("owner-history-cas")).unwrap();
    assert_eq!(reopened.web_submissions_page(agent, before.as_ref(), 1).unwrap()
        .items[0].record.submission_id, ids[0]);
}

#[test]
fn memory_receipt_resolves_the_same_raw_call_after_reopen() {
    use shenko_context_policy::{CallStatus, HistorySource, ItemKind, StoreHistory, estimate_tokens};
    use shenko_types::{ids::*, subject::*};
    let (_dir, store, agent) = initialized_subject();
    let (head, planned) = store.load_subject(agent).unwrap();
    let result = recorded_result(&store, &head, planned);
    let next = EntityId::new();
    let mut step = committed_candidate(&store, result, next);
    let mut decision: StepDecision = serde_json::from_slice(
        &store.cas.read_object(&step.decision_ref.as_ref().unwrap().digest).unwrap(),
    ).unwrap();
    let intent_id = EntityId::new();
    let content = store.cas.seal_object(b"remember the actual receipt", "text/plain",
        "memory", "text", 1).unwrap().object;
    decision.memory_intents = vec![MemoryIntent {
        intent_id, kind: MemoryIntentKind::Save, scope: "subject".into(), content_ref: Some(content),
    }];
    step.decision_ref = Some(store.cas.seal_object(&serde_json::to_vec(&decision).unwrap(),
        "application/json", "decisions", "StepDecision", 8).unwrap().object);
    store.commit_step_and_advance_head(&step, &head, next).unwrap();
    let history = StoreHistory::new(store.clone(), EntityId(head.branch_id.0), Arc::new(estimate_tokens));
    let pending = history.read_range(0, u64::MAX).unwrap();
    assert!(pending.iter().any(|item| matches!(&item.kind,
        ItemKind::ToolCall { call_id, status: CallStatus::Pending, .. } if *call_id == intent_id.to_string())));
    store.start_subject_dispatcher().unwrap();
    assert_eq!(store.settle_subject_memory().unwrap(), 1);
    drop(history);
    drop(store);
    let reopened = Arc::new(Store::open(_dir.join("head.db"), _dir.join("cas")).unwrap());
    let history = StoreHistory::new(reopened, EntityId(head.branch_id.0), Arc::new(estimate_tokens));
    let settled = history.read_range(0, u64::MAX).unwrap();
    for item in &settled {
        match &item.kind {
            ItemKind::ToolCall { call_id, status, .. } | ItemKind::ToolResult { call_id, status }
                if *call_id == intent_id.to_string() =>
            {
                assert_eq!(*status, CallStatus::Executed { ok: true });
            }
            _ => {}
        }
    }
    assert!(settled.iter().any(|item| matches!(&item.kind,
        ItemKind::ToolResult { call_id, .. } if *call_id == intent_id.to_string())));
}

#[test]
fn competing_s1_rejects_stale_writer_without_partial_commit() {
    use shenko_types::{error::ShenkoError, ids::*};
    use std::sync::{Arc, Barrier};
    let (dir, store, agent) = initialized_subject();
    let (head, planned) = store.load_subject(agent).unwrap();
    let result = recorded_result(&store, &head, planned);
    let next_a = EntityId::new();
    let next_b = EntityId::new();
    let step_a = committed_candidate(&store, result.clone(), next_a);
    let step_b = committed_candidate(&store, result, next_b);
    // 同一竞争前提下，错 parent 和陈旧 epoch 都不能占用提交槽。
    let mut wrong_parent = step_a.clone();
    wrong_parent.expected_parent_commit.seq = LedgerSeq(0);
    assert!(matches!(
        store.commit_step_and_advance_head(&wrong_parent, &head, next_a),
        Err(ShenkoError::Conflict(_))
    ));
    let mut stale_head = head.clone();
    stale_head.controller_epoch = ControllerEpoch(head.controller_epoch.0 + 1);
    assert!(matches!(
        store.commit_step_and_advance_head(&step_a, &stale_head, next_a),
        Err(ShenkoError::Conflict(_))
    ));
    let other = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let barrier_a = barrier.clone();
    let head_a = head.clone();
    let a = std::thread::spawn(move || {
        barrier_a.wait();
        store.commit_step_and_advance_head(&step_a, &head_a, next_a)
    });
    let b = std::thread::spawn(move || {
        barrier.wait();
        other.commit_step_and_advance_head(&step_b, &head, next_b)
    });
    let a = a.join().unwrap();
    let b = b.join().unwrap();
    assert_ne!(a.is_ok(), b.is_ok());
    let (winner, loser) = if a.is_ok() {
        (next_a, next_b)
    } else {
        (next_b, next_a)
    };
    assert!(matches!(
        a.as_ref().err().or(b.as_ref().err()).unwrap(),
        ShenkoError::Conflict(_)
    ));
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    let (head, successor) = reopened.load_subject(agent).unwrap();
    assert_eq!(head.next_step_id, winner);
    assert_eq!(successor.step_id, winner);
    assert!(reopened.get_step(&head.branch_id, loser).unwrap().is_none());
    let after: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before + 1);
    let steps: i64 = conn
        .query_row("SELECT count(*) FROM subject_steps", [], |r| r.get(0))
        .unwrap();
    assert_eq!(steps, 2);
    let committed = reopened
        .get_step(&head.branch_id, head.committed_step_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(committed.commit_ref, Some(head.committed_at));
}

#[test]
fn semantic_wait_is_durable_and_a_stale_change_settles_without_failing_s1() {
    use shenko_types::{ids::*, subject::*};
    let (dir, store, agent) = initialized_subject();
    let (head, planned) = store.load_subject(agent).unwrap();
    let wait_id = EntityId::new();
    let next = EntityId::new();
    let condition = "实验出现足以判断的结果".to_string();

    // S1 #1 — the subject starts a semantic wait.
    let result = recorded_result_with_wait(&store, &head, planned, None);
    let mut step = committed_candidate(&store, result, next);
    attach_wait(
        &mut step,
        &store,
        &SemanticWaitChange::Start {
            condition: condition.clone(),
            max_wait_secs: 3,
        },
        wait_id,
    );
    store
        .commit_step_and_advance_head(&step, &head, next)
        .unwrap();

    let wait = store.semantic_wait(agent).unwrap().expect("durable wait");
    assert_eq!(wait.wait_id, wait_id);
    assert_eq!(wait.condition, condition);
    assert!(matches!(wait.status, SemanticWaitStatus::Active));
    // The store owns the deadline: it is fixed once, when the wait starts.
    assert_eq!(
        wait.deadline_at,
        wait.started_at + chrono::TimeDelta::seconds(3)
    );
    let receipt = store
        .semantic_wait_receipt(step.step_id)
        .unwrap()
        .expect("exact receipt");
    assert!(matches!(receipt.outcome, SemanticWaitOutcome::Started));
    // The handled wait intent is settled with that receipt, never blocked.
    let intents = store.committed_intents(Some("policy"), None, 16).unwrap();
    let settled = intents
        .iter()
        .find(|intent| intent.intent_id == wait_id)
        .expect("wait policy intent");
    assert_eq!(settled.state, "settled");
    assert_eq!(settled.contract, "shenko.subject/semantic-wait/1");

    // S1 #2 — a new wait id with the same condition cannot renew the wait.
    let (head2, second) = store.load_subject(agent).unwrap();
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    // The observed wall-clock high water is durable before the next S0.
    let observed = store.observe_semantic_wait(&head2).unwrap().unwrap();
    assert!(matches!(observed.status, SemanticWaitStatus::Active));
    assert!(observed.last_observed_at >= wait.last_observed_at);
    let after_observe: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    assert!(after_observe > before);
    assert_eq!(store.load_subject(agent).unwrap().0.record_revision, head2.record_revision);

    let result2 = recorded_result_with_wait(&store, &head2, second, Some(observed));
    let next2 = EntityId::new();
    let mut step2 = committed_candidate(&store, result2, next2);
    attach_wait(
        &mut step2,
        &store,
        &SemanticWaitChange::Start {
            condition: condition.clone(),
            max_wait_secs: 3,
        },
        EntityId::new(),
    );
    // The stale wait change does not fail the whole S1: the step and its
    // unique successor are still committed.
    store
        .commit_step_and_advance_head(&step2, &head2, next2)
        .unwrap();
    let receipt2 = store
        .semantic_wait_receipt(step2.step_id)
        .unwrap()
        .expect("exact receipt");
    assert!(matches!(
        receipt2.outcome,
        SemanticWaitOutcome::Rejected { .. }
    ));
    let wait2 = store.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(wait2.wait_id, wait_id);
    // The capped deadline of the running wait was not renewed.
    assert_eq!(wait2.deadline_at, wait.deadline_at);
    assert_eq!(wait2.started_at, wait.started_at);
    let (head3, _) = store.load_subject(agent).unwrap();
    assert_eq!(head3.next_step_id, next2);

    // S1 #3 — after the deadline a continue expires the wait instead of
    // reviving it.
    std::thread::sleep(std::time::Duration::from_millis(3100));
    let (head4, third) = store.load_subject(agent).unwrap();
    let result3 = recorded_result_with_wait(&store, &head4, third, Some(wait2.clone()));
    let next3 = EntityId::new();
    let mut step3 = committed_candidate(&store, result3, next3);
    attach_wait(
        &mut step3,
        &store,
        &SemanticWaitChange::Continue { wait_id },
        EntityId::new(),
    );
    store
        .commit_step_and_advance_head(&step3, &head4, next3)
        .unwrap();
    let receipt3 = store
        .semantic_wait_receipt(step3.step_id)
        .unwrap()
        .expect("exact receipt");
    assert!(matches!(
        receipt3.outcome,
        SemanticWaitOutcome::Rejected { .. }
    ));
    let expired = store.semantic_wait(agent).unwrap().unwrap();
    assert!(matches!(expired.status, SemanticWaitStatus::Expired { .. }));
    assert_eq!(expired.deadline_at, wait.deadline_at);
    drop(store);

    // Reopening keeps the original deadline and the terminal fact: a finished
    // wait is never resurrected and its deadline is never reset.
    let reopened = Arc::new(Store::open(dir.join("head.db"), dir.join("cas")).unwrap());
    let restored = reopened.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(restored.wait_id, wait_id);
    assert_eq!(restored.started_at, wait.started_at);
    assert_eq!(restored.deadline_at, wait.deadline_at);
    assert!(matches!(restored.status, SemanticWaitStatus::Expired { .. }));
    // Observing a finished wait changes nothing, commits nothing and leaves
    // the branch head exactly as the subject left it.
    let (head5, _) = reopened.load_subject(agent).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    let observed = reopened.observe_semantic_wait(&head5).unwrap().unwrap();
    assert!(matches!(observed.status, SemanticWaitStatus::Expired { .. }));
    let after: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before);
    let (head6, _) = reopened.load_subject(agent).unwrap();
    assert_eq!(head6.record_revision, head5.record_revision);

    // A fresh id must not turn the just-expired identical condition into an
    // automatic renewal, even when it is proposed in a later committed step.
    let (head, planned) = reopened.load_subject(agent).unwrap();
    let result = recorded_result_with_wait(&reopened, &head, planned, Some(restored.clone()));
    let next = EntityId::new();
    let mut repeated = committed_candidate(&reopened, result, next);
    attach_wait(&mut repeated, &reopened, &SemanticWaitChange::Start {
        condition: condition.clone(), max_wait_secs: 120,
    }, EntityId::new());
    reopened.commit_step_and_advance_head(&repeated, &head, next).unwrap();
    assert!(matches!(reopened.semantic_wait_receipt(repeated.step_id).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Rejected { .. }));
    assert_eq!(reopened.semantic_wait(agent).unwrap().unwrap().wait_id, wait_id);

    // A different new decision may start a distinct wait. A cognition archive
    // must carry both wait records and choose the later one on the receiver,
    // even though all imported rows receive the same local import sequence.
    let (head, planned) = reopened.load_subject(agent).unwrap();
    let result = recorded_result_with_wait(&reopened, &head, planned, Some(restored));
    let next = EntityId::new();
    let mut distinct = committed_candidate(&reopened, result, next);
    let next_wait_id = EntityId::new();
    attach_wait(&mut distinct, &reopened, &SemanticWaitChange::Start {
        condition: "收到新的独立观察".into(), max_wait_secs: 120,
    }, next_wait_id);
    reopened.commit_step_and_advance_head(&distinct, &head, next).unwrap();
    let before_migration = reopened.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(before_migration.wait_id, next_wait_id);
    let mut archive = Vec::new();
    reopened.write_cognition_archive(
        &shenko_store::cognition::CognitionSnapshotOptions::default(), None, &mut archive,
    ).unwrap();
    let receiver = Store::open(dir.join("wait-receiver.db"), dir.join("wait-receiver-cas")).unwrap();
    receiver.import_cognition_archive(
        &archive[..], &shenko_store::cognition::CognitionImportOptions::default(),
    ).unwrap();
    let migrated = receiver.semantic_wait(agent).unwrap().expect("wait imported");
    assert_eq!(migrated.wait_id, next_wait_id);
    assert_eq!(migrated.started_at, before_migration.started_at);
    assert_eq!(migrated.deadline_at, before_migration.deadline_at);
    assert!(matches!(receiver.semantic_wait_receipt(distinct.step_id).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Started));
}

#[test]
fn task_roundtrip() {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
    use shenko_types::object::{Digest, ObjectRef, SchemaRef};
    use shenko_types::task::{
        DispatchEligibility, ImplementationRef, TaskOwner, TaskRecord, TaskState,
    };

    let dir = tmp_dir();
    let db = dir.join("task.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let agent_id = EntityId::new();
    let conv_id = EntityId::new();
    let task_id = EntityId::new();
    let commit_ref = CommitRef::new(store.ledger.ledger_id(), shenko_types::ids::LedgerSeq(1));

    let codec = SchemaRef {
        name: "raw".into(),
        major: 1,
        digest: Digest::blake3(b"raw"),
    };
    let input_ref = ObjectRef {
        digest: Digest::blake3(b"task input"),
        bytes: 10,
        media_type: "text/plain".into(),
        store_namespace: "test".into(),
        locator: "file:///test".into(),
        codec,
    };

    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id,
        conversation_id: conv_id,
        owner: TaskOwner::Conversation { id: conv_id },
        background: false,
        kind: "test".into(),
        definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: EntityId::new(),
            artifact_digest: Digest::blake3(b"plugin"),
            registry_revision: 1,
        },
        input_ref,
        state: TaskState::Pending {
            checkpoint_ref: None,
        },
        abort_request: None,
        memos: Default::default(),
        residual_case_ids: vec![],
        created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: commit_ref,
        updated_commit: commit_ref,
    };

    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task);
    let receipt = store.commit(batch).unwrap();

    assert!(!receipt.updated.is_empty());
    assert_eq!(receipt.updated[0].entity_id, task_id);
}

fn recovery_task(
    store: &Store,
    agent: shenko_types::ids::EntityId,
    number: u64,
    running: bool,
) -> shenko_types::task::TaskRecord {
    use shenko_types::{commit::CommitRef, ids::*, task::*};
    let input = store.cas.seal_object(b"recovery checkpoint", "text/plain", "test", "raw", 1)
        .unwrap().object;
    let task_id = format!("00000000-0000-0000-0000-{number:012x}").parse().unwrap();
    let commit = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    TaskRecord {
        record_schema_version: 1, task_id, task_revision: TaskRevision(3),
        record_revision: RecordRevision(5), agent_id: agent, conversation_id: agent,
        owner: TaskOwner::Conversation { id: agent }, background: false,
        kind: "host.printf".into(), definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: agent, artifact_digest: input.digest.clone(), registry_revision: 1,
        },
        input_ref: input.clone(),
        state: if running {
            TaskState::Running { checkpoint_ref: Some(input), invocation_id: EntityId::new(),
                mode: RunMode::Run, epoch: ControllerEpoch(7) }
        } else { TaskState::Pending { checkpoint_ref: Some(input) } },
        abort_request: None,
        memos: serde_json::Map::from_iter([("native_child".into(), serde_json::json!(true))]),
        residual_case_ids: vec![], created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready, no_progress_crashes: 0,
        first_run_at: None, terminal_at: None, created_commit: commit, updated_commit: commit,
    }
}

#[test]
fn child_tool_creation_claim_and_reopen_keep_one_identity() {
    use shenko_types::{ids::*, task::*};
    let (dir, store, agent) = initialized_subject();
    let parent = claimed_native_task(&store, agent).task;
    let id = EntityId::new();
    let implementation = parent.implementation_ref.clone();
    let claim = store.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), implementation.clone(),
    ).unwrap().unwrap();
    assert_eq!(claim.task.owner, TaskOwner::Task { id: parent.task_id });
    assert_eq!(claim.task.record_revision, RecordRevision(1));
    assert!(matches!(claim.task.state, TaskState::Running { invocation_id, mode: RunMode::Run, .. }
        if invocation_id == claim.invocation.invocation_id));
    assert_eq!(claim.task.created_commit, claim.task.updated_commit);
    let commit = store.current_commit_seq().unwrap();
    assert!(store.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), implementation.clone(),
    ).unwrap().is_none());
    assert_eq!(store.current_commit_seq().unwrap(), commit);
    drop(store);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    let before = reopened.current_commit_seq().unwrap();
    assert!(reopened.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), implementation,
    ).unwrap().is_none());
    let retained = reopened.external_task(id).unwrap();
    assert_eq!(retained.state, claim.task.state);
    assert_eq!(retained.memos, claim.task.memos);
    assert_eq!(retained.updated_commit, claim.task.updated_commit);
    assert_eq!(reopened.current_commit_seq().unwrap(), before);
}

#[test]
fn failed_child_tool_claim_rolls_back_its_creation() {
    use shenko_types::ids::*;
    let (dir, store, agent) = initialized_subject();
    let parent = claimed_native_task(&store, agent).task;
    let id = EntityId::new();
    let connection = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    connection.execute_batch(&format!(
        "CREATE TRIGGER fail_tool_claim BEFORE UPDATE ON tasks WHEN NEW.task_id='{id}'
         BEGIN SELECT RAISE(ABORT,'injected claim failure'); END;"
    )).unwrap();
    let before = store.current_commit_seq().unwrap();
    let error = store.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), parent.implementation_ref.clone(),
    ).unwrap_err();
    assert!(format!("{error:#}").contains("injected claim failure"));
    assert!(store.get_task(id).unwrap().is_none());
    assert_eq!(store.current_commit_seq().unwrap(), before);
    assert_eq!(store.external_task(parent.task_id).unwrap().state, parent.state);
}

fn claimed_native_task(
    store: &Store, agent: shenko_types::ids::EntityId,
) -> shenko_store::child::NativeTaskClaim {
    let mut task = recovery_task(store, agent, 1, false);
    task.memos.insert(shenko_store::child::NATIVE_INPUT.into(), serde_json::json!("object"));
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task.clone());
    store.commit(batch).unwrap();
    store.start_subject_dispatcher().unwrap();
    store.claim_native_task(task.task_id, task.implementation_ref, false)
        .unwrap().expect("the pending native task has an admitted input")
}

fn arm_dispatch_probe(
    store: &Store, invocation: &shenko_types::task::TaskInvocation, key: &str,
) -> (shenko_types::ids::EntityId, shenko_types::ids::EntityId) {
    use shenko_types::{ids::*, object::*, operation::*};
    let task = store.external_task(invocation.task_id).unwrap();
    let mut current = invocation.clone();
    current.record_revision = task.record_revision;
    let object = task.input_ref;
    let reader = VersionRef {
        artifact: ArtifactRef { digest: object.digest.clone(), locator: object.locator.clone() },
        symbol: "dispatch-probe".into(), schema: object.codec.clone(),
    };
    let mut operation = OperationRecord {
        record_schema_version: 1, operation_id: EntityId::new(), record_revision: RecordRevision(0),
        task_id: task.task_id, logical_key: key.into(), operation_name: "dispatch.probe".into(),
        input_schema_version: 1, output_schema_version: 1,
        final_input_ref: object.clone(), input_digest: object.digest.clone(),
        contract_ref: OperationContractRef {
            plugin_id: task.implementation_ref.plugin_id,
            artifact_digest: object.digest.clone(), contract_digest: object.digest.clone(),
            receipt_reader_ref: reader.clone(),
        },
        resource_bindings_ref: object.clone(),
        authority_ref: AuthorityRef {
            delegation_id: task.task_id, delegation_revision: task.record_revision,
            decision_ref: task.task_id,
        },
        acceptance_predicate_ref: object.clone(), retry_contract_ref: object,
        checkpoint_scope_ref: None, state: OperationState::InFlight, outcome: None,
        outcome_conflict: false, effect_status: EffectStatus::Unknown, retry_of: None,
        compensates: None, possible_duplicate_of: vec![], prepared_commit: task.updated_commit,
        evidence_through: None,
    };
    let mut attempt = AttemptRecord {
        record_schema_version: 1, attempt_id: EntityId::new(), operation_id: operation.operation_id,
        attempt_no: 1, invocation_id: current.invocation_id, worker_id: current.worker_id,
        node_id: current.node_id, epoch: current.epoch, implementation_ref: reader,
        dispatch_stage: AttemptDispatchStage::Armed, armed_commit: task.updated_commit,
        provider_handle: None, provider_idempotency: None, transport_ref: None,
        started_at: None, last_observed_at: None, final_receipt_id: None,
    };
    assert!(store.arm_external_operation(&current, &mut operation, &mut attempt).unwrap().is_none());
    (operation.operation_id, attempt.attempt_id)
}

#[test]
fn native_dispatch_is_once_per_attempt_across_steps_cancellation_and_reopen() {
    use shenko_store::child::NativeDispatchTransition::{AlreadyCommitted, Fenced, Won};
    use shenko_types::{ids::*, operation::AttemptDispatchStage, task::*};
    let (dir, store, agent) = initialized_subject();
    let ctx = claimed_native_task(&store, agent).invocation;
    let first = arm_dispatch_probe(&store, &ctx, "step/0");
    let second = arm_dispatch_probe(&store, &ctx, "step/1");
    let cancelled = arm_dispatch_probe(&store, &ctx, "step/2");
    let before = store.current_commit_seq().unwrap();
    assert!(store.commit_native_dispatch(&ctx, first.0, second.1).is_err());
    let mut foreign = ctx.clone();
    foreign.invocation_id = EntityId::new();
    assert!(store.commit_native_dispatch(&foreign, first.0, first.1).is_err());
    assert_eq!(store.current_commit_seq().unwrap(), before);

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let racers: Vec<_> = (0..2).map(|_| {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            barrier.wait();
            store.commit_native_dispatch(&ctx, first.0, first.1).unwrap()
        })
    }).collect();
    let results: Vec<_> = racers.into_iter().map(|racer| racer.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| **result == Won).count(), 1);
    assert_eq!(results.iter().filter(|result| **result == AlreadyCommitted).count(), 1);
    assert_eq!(store.commit_native_dispatch(&ctx, second.0, second.1).unwrap(), Won);
    // A send-boundary commit is not a fabricated provider acknowledgement.
    assert_eq!(store.external_attempt(first.1).unwrap().dispatch_stage, AttemptDispatchStage::Armed);
    store.request_native_cancellation(ctx.task_id, EntityId::new(), EntityId::new(),
        "cancel remaining step", AbortScope::Task).unwrap();
    let before = store.current_commit_seq().unwrap();
    assert_eq!(store.commit_native_dispatch(&ctx, first.0, first.1).unwrap(), AlreadyCommitted);
    assert_eq!(store.commit_native_dispatch(&ctx, cancelled.0, cancelled.1).unwrap(), Fenced);
    assert_eq!(store.current_commit_seq().unwrap(), before);
    drop(store);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    let before = reopened.current_commit_seq().unwrap();
    assert_eq!(reopened.commit_native_dispatch(&ctx, first.0, first.1).unwrap(), AlreadyCommitted);
    assert_eq!(reopened.commit_native_dispatch(&ctx, second.0, second.1).unwrap(), AlreadyCommitted);
    assert_eq!(reopened.commit_native_dispatch(&ctx, cancelled.0, cancelled.1).unwrap(), Fenced);
    assert_eq!(reopened.current_commit_seq().unwrap(), before);
}

#[test]
fn legacy_unattributed_dispatch_never_authorizes_another_attempt() {
    use shenko_store::child::{NATIVE_DISPATCH, NativeDispatchTransition::AlreadyCommitted};
    let (dir, store, agent) = initialized_subject();
    let ctx = claimed_native_task(&store, agent).invocation;
    let first = arm_dispatch_probe(&store, &ctx, "step/0");
    let second = arm_dispatch_probe(&store, &ctx, "step/1");
    let mut task = store.external_task(ctx.task_id).unwrap();
    let marker = serde_json::json!({"task_revision": ctx.task_revision,
        "invocation_id": ctx.invocation_id, "commit": task.updated_commit});
    task.memos.insert(NATIVE_DISPATCH.into(), marker.clone());
    rusqlite::Connection::open(dir.join("head.db")).unwrap().execute(
        "UPDATE tasks SET data=?1 WHERE task_id=?2",
        rusqlite::params![serde_json::to_string(&task).unwrap(), task.task_id.to_string()],
    ).unwrap();
    let before = store.current_commit_seq().unwrap();
    for (operation, attempt) in [first, second] {
        assert_eq!(store.commit_native_dispatch(&ctx, operation, attempt).unwrap(), AlreadyCommitted);
    }
    assert_eq!(store.current_commit_seq().unwrap(), before);
    assert_eq!(store.external_task(ctx.task_id).unwrap().memos.get(NATIVE_DISPATCH), Some(&marker));
}

#[test]
fn native_recovery_scans_past_pending_tasks_and_preserves_checkpoint() {
    use shenko_types::{ids::*, task::*};
    let (dir, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=256 { batch.task_upserts.push(recovery_task(&store, agent, number, false)); }
    let mut running = recovery_task(&store, agent, 257, true);
    running.abort_request = Some(AbortRequest {
        cancel_id: EntityId::new(), decision_ref: EntityId::new(),
        requested_commit: running.created_commit, reason: "cancel before crash".into(), scope: AbortScope::Task,
    });
    let checkpoint = match &running.state {
        TaskState::Running { checkpoint_ref, .. } => checkpoint_ref.clone(), _ => unreachable!(),
    };
    batch.task_upserts.push(running.clone());
    store.commit(batch).unwrap();
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    assert_eq!(reopened.recover_native_tasks().unwrap(), 1);
    let recovered = reopened.external_task(running.task_id).unwrap();
    assert_eq!(recovered.state, TaskState::Pending { checkpoint_ref: checkpoint });
    assert_eq!(recovered.abort_request, running.abort_request);
    assert_eq!(recovered.task_revision, running.task_revision);
    assert_eq!(recovered.record_revision, RecordRevision(6));
    assert_eq!(recovered.memos.get("native_recovery"), Some(&serde_json::json!(true)));
    assert!(matches!(recovered.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    assert_eq!(reopened.recover_native_tasks().unwrap(), 0);
    assert_eq!(reopened.external_task("00000000-0000-0000-0000-000000000001".parse().unwrap())
        .unwrap().record_revision, RecordRevision(5));
}

#[test]
fn native_recovery_recovers_more_than_one_page_of_running_work() {
    use shenko_types::task::TaskState;
    let (_, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=257 { batch.task_upserts.push(recovery_task(&store, agent, number, true)); }
    store.commit(batch).unwrap();
    store.start_subject_dispatcher().unwrap();
    assert_eq!(store.recover_native_tasks().unwrap(), 257);
    for number in 1..=257 {
        let id = format!("00000000-0000-0000-0000-{number:012x}").parse().unwrap();
        let task = store.external_task(id).unwrap();
        assert!(matches!(task.state, TaskState::Pending { checkpoint_ref: Some(_) }));
        assert_eq!(task.memos.get("native_recovery"), Some(&serde_json::json!(true)));
    }
    assert_eq!(store.recover_native_tasks().unwrap(), 0);
}

#[test]
fn dispatch_cursor_reaches_later_work_and_wraps_without_starving() {
    let (_, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=40 { batch.task_upserts.push(recovery_task(&store, agent, number, false)); }
    store.commit(batch).unwrap();
    let mut seen = Vec::new();
    let mut after: Option<(String, shenko_types::ids::EntityId)> = None;
    let mut pages = 0;
    let last_key = loop {
        let page = store.native_task_candidates_after(
            after.as_ref().map(|(created, id)| (created.as_str(), *id)), 32,
        ).unwrap();
        pages += 1;
        assert!(pages <= 3, "scan did not terminate");
        seen.extend(page.iter().map(|(_, task)| task.task_id.to_string()));
        let page_end = page.last().map(|(created, task)| (created.clone(), task.task_id));
        if page.len() < 32 {
            // Short page: the scan reached the end and the next pass restarts.
            break page_end;
        }
        after = page_end;
    };
    assert_eq!(seen.len(), 40, "a later page hid work: {seen:?}");
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "pagination duplicated or reordered work");
    let upper = store.native_task_scan_upper().unwrap().unwrap();
    assert_eq!(Some(upper), last_key, "the round boundary is the last scanned key");
    // Wraparound returns the same complete set rather than staying at the end.
    let restarted = store.native_task_candidates_after(None, 32).unwrap();
    assert_eq!(restarted.len(), 32);
    assert_eq!(restarted[0].1.task_id.to_string(), seen[0]);
}

#[test]
fn native_recovery_rolls_back_all_pages_when_later_identity_is_corrupt() {
    use shenko_types::{ids::*, task::TaskState};
    let (dir, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=257 { batch.task_upserts.push(recovery_task(&store, agent, number, true)); }
    store.commit(batch).unwrap();
    drop(store);

    let corrupt = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    corrupt.execute(
        "UPDATE tasks SET data=json_set(data,'$.task_id',?1) WHERE task_id=?2",
        rusqlite::params![EntityId::nil().to_string(), "00000000-0000-0000-0000-000000000101"],
    ).unwrap();
    drop(corrupt);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    let before = reopened.current_commit_seq().unwrap();
    assert!(reopened.recover_native_tasks().is_err());
    assert_eq!(reopened.current_commit_seq().unwrap(), before);
    let first = reopened.external_task("00000000-0000-0000-0000-000000000001".parse().unwrap()).unwrap();
    assert!(matches!(first.state, TaskState::Running { .. }));
    assert_eq!(first.record_revision, RecordRevision(5));
    assert!(!first.memos.contains_key("native_recovery"));
}

// ---------------------------------------------------------------------------
// Subject outbox: one atomic T1, and a dispatch loop that committed facts can
// never poison. docs/specs/durable-state.md#spec-41-4 X07/X11, docs/specs/channels.md#spec-41-6.
// ---------------------------------------------------------------------------

/// The outbox intent one committed subject step authors.
fn publication_intent() -> shenko_types::subject::PublicationIntent {
    shenko_types::subject::PublicationIntent {
        intent_id: shenko_types::ids::EntityId::new(),
        target: shenko_types::output::PublicationTarget::new(
            "qq",
            "group",
            "123456789",
            "qq:10001",
        ),
        content: "已经提交的回复正文".into(),
        reply_grant_id: None,
    }
}

/// What an installed channel plugin describes for that target. The store owns
/// the ledger; the plugin owns the platform action.
fn publication_dispatch() -> shenko_types::channel::ChannelDispatch {
    shenko_types::channel::ChannelDispatch {
        channel: "qq".into(),
        action: shenko_types::channel::ChannelAction::Send,
        platform_action: "send_group_msg".into(),
        params: serde_json::json!({"group_id":"123456789","message":"已经提交的回复正文"}),
        account: "qq:10001".into(),
        tenant: None,
        conversation: Some("group:123456789".into()),
        attachments: vec![],
        contract_digest: shenko_types::object::Digest::blake3(b"shenko.channel/qq/test/1"),
        acceptance_predicate: "onebot_retcode_zero".into(),
        echo: None,
    }
}

/// Commit one real subject step whose S1 authored `intent`.
fn commit_publication_step(
    store: &Arc<Store>,
    agent: shenko_types::ids::EntityId,
    intent: &shenko_types::subject::PublicationIntent,
) -> shenko_types::subject::SubjectStep {
    let (head, first) = store.load_subject(agent).unwrap();
    let next = shenko_types::ids::EntityId::new();
    let result = recorded_result(store, &head, first);
    let step = committed_candidate_with(store, result, next, vec![intent.clone()]);
    store
        .commit_step_and_advance_head(&step, &head, next)
        .unwrap();
    step
}

#[test]
fn channel_conversations_interleave_publications_with_recorded_message_times() {
    use chrono::{DateTime, Duration};
    use shenko_types::{channel::ChannelEnvelope, ids::EntityId, inbox::{InboxEvent, InboxEventKind}};

    let (dir, store, agent) = initialized_subject();
    let publication = publication_intent();
    commit_publication_step(&store, agent, &publication);
    let task = store.list_task_summaries(agent, 100).unwrap().into_iter()
        .find(|task| task.task_id == publication.intent_id.to_string()).unwrap();
    let published = DateTime::parse_from_rfc3339(&task.created_at).unwrap().to_utc();
    let admit = |text: &str, at: chrono::DateTime<chrono::Utc>| {
        let event_id = EntityId::new();
        let envelope: ChannelEnvelope = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "provider": "qq", "account": "qq:10001",
            "transport_identity_domain": "test:qq", "conversation": "group:123456789",
            "message_id": event_id.to_string(), "received_at": at, "occurred_at": at,
            "actor": {"platform_id": "member", "display_name": "成员"},
            "content": {"text": text},
            "reply_target": {"kind": "group", "id": "123456789", "label": "群聊", "account": "qq:10001"}
        })).unwrap();
        let event = InboxEvent {
            event_id, kind: InboxEventKind::Message, payload: serde_json::to_value(envelope).unwrap(),
            created_at: at, source: "channel/qq".into(),
        };
        let (head, planned) = store.load_subject(agent).unwrap();
        store.stage_subject_inputs(&head, &planned, &[shenko_store::raw_session::SubjectHistoryInput {
            event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1, "inbox_event": event, "text": text}),
        }]).unwrap();
        event_id
    };
    // A delayed delivery can precede an already committed publication in
    // event time. The later message must follow that same publication.
    let before = admit("较早的来信", published - Duration::seconds(1));
    let after = admit("较后的来信", published + Duration::seconds(1));
    let expected = vec![before.to_string(), publication.intent_id.to_string(), after.to_string()];
    let messages = store.channel_conversation_messages(agent, 10).unwrap();
    assert_eq!(messages.iter().map(|message| message.message_id.clone()).collect::<Vec<_>>(), expected);
    assert_eq!(messages.iter().map(|message| message.direction).collect::<Vec<_>>(),
        vec!["unknown", "outbound", "unknown"],
        "historical envelopes without ownership facts do not assert a counterpart");
    assert!(messages.iter().all(|message| message.thread_key == "qq:10001|group|123456789"));
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    assert_eq!(reopened.channel_conversation_messages(agent, 10).unwrap().into_iter()
        .map(|message| message.message_id).collect::<Vec<_>>(), expected);
}

#[test]
fn channel_conversations_limit_selects_latest_publications() {
    let (_dir, store, agent) = initialized_subject();
    let first = publication_intent();
    let second = publication_intent();
    let third = publication_intent();
    for intent in [&first, &second, &third] {
        commit_publication_step(&store, agent, intent);
    }
    let actual = store.channel_conversation_messages(agent, 2).unwrap().into_iter()
        .map(|message| message.message_id).collect::<std::collections::BTreeSet<_>>();
    let expected = [second.intent_id.to_string(), third.intent_id.to_string()]
        .into_iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn inspection_projections_keep_each_subjects_tasks_and_messages_private() {
    use shenko_types::{channel::ChannelEnvelope, ids::EntityId, inbox::{InboxEvent, InboxEventKind}};
    let (_dir, store, first) = initialized_subject();
    let second = initialize_subject_in(&store, b"second subject");
    let first_publication = publication_intent();
    let second_publication = publication_intent();
    commit_publication_step(&store, first, &first_publication);
    commit_publication_step(&store, second, &second_publication);
    let mut events = Vec::new();
    for (agent, text) in [(first, "first private input"), (second, "second private input")] {
        let event_id = EntityId::new();
        let at = chrono::Utc::now();
        let envelope: ChannelEnvelope = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "provider": "qq", "account": "qq:10001",
            "transport_identity_domain": "test:qq", "conversation": "group:123456789",
            "message_id": event_id, "received_at": at, "actor": {"platform_id": "member"},
            "content": {"text": text},
            "reply_target": {"kind": "group", "id": "123456789", "label": "", "account": "qq:10001"}
        })).unwrap();
        let event = InboxEvent {
            event_id, kind: InboxEventKind::Message, payload: serde_json::to_value(envelope).unwrap(),
            created_at: at, source: "channel/qq".into(),
        };
        let (head, planned) = store.load_subject(agent).unwrap();
        store.stage_subject_inputs(&head, &planned, &[shenko_store::raw_session::SubjectHistoryInput {
            event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1, "inbox_event": event, "text": text}),
        }]).unwrap();
        events.push(event_id);
    }
    for (agent, publication, foreign, event) in [
        (first, &first_publication, &second_publication, events[0]),
        (second, &second_publication, &first_publication, events[1]),
    ] {
        let (head, _) = store.load_subject(agent).unwrap();
        let ids = store.list_task_summaries(agent, 100).unwrap().into_iter()
            .map(|task| task.task_id).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids, [head.subject_root_task_id.to_string(), publication.intent_id.to_string()]
            .into_iter().collect());
        assert!(store.task_trajectory(agent, foreign.intent_id).unwrap().is_none());
        let own = store.task_trajectory(agent, publication.intent_id).unwrap().unwrap();
        assert_eq!(own.task.task_id, publication.intent_id.to_string());
        assert!(own.children.is_empty());
        let messages = store.channel_conversation_messages(agent, 100).unwrap().into_iter()
            .map(|message| message.message_id).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(messages, [event.to_string(), publication.intent_id.to_string()].into_iter().collect());
    }
    // The installation cannot select an arbitrary local subject for an API
    // request: ambiguous identity is an error, never an unscoped fallback.
    assert!(store.get_existing_agent_id().is_err());
}

#[test]
fn structural_task_roles_survive_reopen_and_archive_without_kind_inference() {
    use shenko_types::{ids::EntityId, subject::*, task::*};

    let (dir, store, agent) = initialized_subject();
    let (head, first) = store.load_subject(agent).unwrap();
    let next = EntityId::new();
    let result = recorded_result(&store, &head, first);
    let mut step = committed_candidate(&store, result, next);
    let mut decision: StepDecision = serde_json::from_slice(
        &store.cas.read_object(&step.decision_ref.as_ref().unwrap().digest).unwrap(),
    ).unwrap();
    let delegation_id = EntityId::new();
    decision.delegated_task_intents.push(DelegationIntent {
        intent_id: delegation_id,
        goal: "Observe through a future plugin".into(),
        system_prompt: String::new(), model: None, capabilities: vec![],
        budget_hint: None, reply_grant: None,
        definition: Some(NativeTaskDefinition {
            kind: "future.plugin.observe".into(), definition_version: 7,
            input: serde_json::json!({"source":"retained evidence"}),
        }),
    });
    step.decision_ref = Some(store.cas.seal_object(
        &serde_json::to_vec(&decision).unwrap(), "application/json", "test", "StepDecision", 8,
    ).unwrap().object);
    store.commit_step_and_advance_head(&step, &head, next).unwrap();
    store.start_subject_dispatcher().unwrap();
    let delegated = store.external_task(delegation_id).unwrap();
    let implementation = delegated.implementation_ref.clone();
    let parent = store.claim_native_task(delegation_id, implementation.clone(), false).unwrap().unwrap();
    let tool_id = EntityId::new();
    let input = store.cas.seal_object(b"{}", "application/json", "test", "tool", 1).unwrap().object;
    store.claim_child_tool_task(&parent.task, tool_id, "future.plugin.tool", 3, input, implementation)
        .unwrap().unwrap();

    // A familiar definition name, or ownership by the root alone, is no
    // evidence that an older task was delegated or invoked as a tool.
    let mut unclassified = recovery_task(&store, agent, 0x296, false);
    unclassified.kind = "model.child".into();
    unclassified.owner = TaskOwner::Task { id: head.subject_root_task_id };
    let unknown_id = unclassified.task_id;
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(unclassified);
    store.commit(batch).unwrap();

    let roles = |store: &Store| store.list_task_summaries(agent, 100).unwrap().into_iter()
        .map(|task| (task.task_id, task.role)).collect::<std::collections::BTreeMap<_, _>>();
    let expected = [
        (head.subject_root_task_id.to_string(), TaskRole::SubjectRoot),
        (delegation_id.to_string(), TaskRole::Delegation),
        (tool_id.to_string(), TaskRole::Tool),
        (unknown_id.to_string(), TaskRole::Unknown),
    ].into_iter().collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(roles(&store), expected);
    let trajectory = store.task_trajectory(agent, delegation_id).unwrap().unwrap();
    assert_eq!(trajectory.task.role, TaskRole::Delegation);
    assert_eq!(trajectory.children.len(), 1);
    assert_eq!(trajectory.children[0].role, TaskRole::Tool);
    assert_eq!(trajectory.children[0].parent_task_id.as_deref(), Some(delegation_id.to_string().as_str()));
    assert_eq!(serde_json::to_value(&trajectory).unwrap()["task"]["role"], "delegation");
    let root = store.task_trajectory(agent, head.subject_root_task_id).unwrap().unwrap();
    assert!(root.task.parent_task_id.is_none());
    assert!(!root.children.iter().any(|task| task.task_id == root.task.task_id));

    let mut archive = Vec::new();
    store.write_cognition_archive(&Default::default(), None, &mut archive).unwrap();
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    assert_eq!(roles(&reopened), expected);
    let receiver = Store::open(dir.join("role-receiver.db"), dir.join("role-receiver-cas")).unwrap();
    receiver.import_cognition_archive(&archive[..], &Default::default()).unwrap();
    assert_eq!(roles(&receiver), expected);
    let imported = receiver.task_trajectory(agent, delegation_id).unwrap().unwrap();
    assert_eq!(imported.task.role, TaskRole::Delegation);
    assert_eq!(imported.children[0].role, TaskRole::Tool);
    assert_eq!(receiver.external_task(delegation_id).unwrap().input_ref, delegated.input_ref);
}

fn publication_state(store: &Store, intent_id: shenko_types::ids::EntityId) -> String {
    store
        .committed_intents(Some("publication"), None, 16)
        .unwrap()
        .into_iter()
        .find(|intent| intent.intent_id == intent_id)
        .expect("the admitted publication intent is readable")
        .state
}

#[test]
fn publication_t1_claims_and_arms_in_one_commit() {
    use shenko_types::ids::*;
    use shenko_types::operation::{AttemptDispatchStage, OperationState};
    use shenko_types::task::TaskState;

    let (_dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    let step = commit_publication_step(&store, agent, &intent);

    // S1 already exposed the intent and its task together: never a half outbox.
    assert_eq!(store.ready_publications(8).unwrap().len(), 1);
    assert!(matches!(
        store.external_task(intent_id).unwrap().state,
        TaskState::Pending { .. }
    ));

    let before = store.current_commit_seq().unwrap();
    let armed = store
        .arm_publication(intent_id, &publication_dispatch())
        .unwrap()
        .expect("a ready publication arms");
    let after = store.current_commit_seq().unwrap();
    assert_eq!(after, before + 1, "T1 must advance the ledger exactly once");
    assert_eq!(armed.operation_id, intent_id);
    assert_eq!(armed.task_id, intent_id);
    assert_eq!(armed.account, "qq:10001");
    assert_eq!(
        armed.armed_commit,
        shenko_types::commit::CommitRef::new(store.ledger.ledger_id(), LedgerSeq(after))
    );

    // The same commit carries the claim, the operation and the armed attempt.
    assert_eq!(publication_state(&store, intent_id), "claimed");
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::InFlight);
    assert_eq!(operation.prepared_commit, armed.armed_commit);
    assert_eq!(operation.authority_ref.decision_ref, step.step_id);
    let attempts = store.external_operation_attempts(intent_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].dispatch_stage, AttemptDispatchStage::Armed);
    assert_eq!(attempts[0].attempt_id, armed.attempt_id);
    assert_eq!(attempts[0].armed_commit, armed.armed_commit);
    match store.external_task(intent_id).unwrap().state {
        TaskState::Running {
            invocation_id,
            epoch,
            ..
        } => {
            assert_eq!(invocation_id, attempts[0].invocation_id);
            assert_eq!(epoch, attempts[0].epoch);
        }
        other => panic!("the publication task must be running at T1, got {other:?}"),
    }

    // A second claim never authorizes a second send.
    assert!(
        store
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .is_none(),
        "an already claimed publication must not arm again"
    );
    assert!(store.ready_publications(8).unwrap().is_empty());
    assert_eq!(store.external_operation_attempts(intent_id).unwrap().len(), 1);
    assert_eq!(store.current_commit_seq().unwrap(), after);
}

#[test]
fn cancelled_publication_is_settled_never_dispatched_without_stopping_dispatch() {
    use shenko_types::ids::*;
    use shenko_types::task::{AbortScope, TaskState};

    let (dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    commit_publication_step(&store, agent, &intent);

    // The owner withdraws the reply before any T1: a real, committed fact.
    let cancellation = store
        .request_native_cancellation(
            intent_id,
            EntityId::new(),
            EntityId::new(),
            "the subject withdrew this reply",
            AbortScope::Task,
        )
        .unwrap();
    assert!(cancellation.cancelled.contains(&intent_id));

    // Before this contract the call answered `Err(publication_cancelled)`,
    // which the publication consumer propagated as a fatal pass: one cancelled
    // reply took the whole runtime down on every restart, forever.
    let armed = store
        .arm_publication(intent_id, &publication_dispatch())
        .unwrap();
    assert!(armed.is_none(), "a cancelled reply is never armed");
    assert!(
        store.external_operation(intent_id).is_err(),
        "nothing was ever armed, so no external effect is possible"
    );
    assert!(store.ready_publications(8).unwrap().is_empty());
    let settled = store
        .committed_intent(intent_id)
        .unwrap()
        .expect("the settled intent is readable by identity");
    assert_eq!(settled.state, "blocked");
    assert!(
        settled.result_ref.is_some(),
        "the never-dispatched intent names its sealed outcome"
    );
    assert!(
        store.committed_intent(EntityId::new()).unwrap().is_none(),
        "an unknown identity is absent, not an error"
    );

    let outcome_ref = match store.external_task(intent_id).unwrap().state {
        TaskState::Terminal { outcome_ref } => outcome_ref,
        other => panic!("the never-dispatched reply must retire its task, got {other:?}"),
    };
    let detail: serde_json::Value =
        serde_json::from_slice(&store.cas.read_object(&outcome_ref.digest).unwrap()).unwrap();
    assert_eq!(detail["kind"], "publication_never_dispatched");
    assert_eq!(detail["reason_kind"], "cancelled");
    assert_eq!(detail["dispatched"], false);
    assert_eq!(detail["effect_status"], "none");

    // Durable: a reopen reads the same settlement and still refuses to send.
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    assert_eq!(publication_state(&reopened, intent_id), "blocked");
    assert!(
        reopened
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .is_none()
    );
}

#[test]
fn superseded_authority_is_settled_never_dispatched_instead_of_failing() {
    use shenko_types::ids::*;
    use shenko_types::task::TaskState;

    let (dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    commit_publication_step(&store, agent, &intent);

    // A real controller handoff advances the branch epoch; a reply authored by
    // the previous epoch can never be sent by the new controller.
    let (mut head, _) = store.load_subject(agent).unwrap();
    head.controller_epoch = ControllerEpoch(head.controller_epoch.0 + 1);
    head.record_revision = head.record_revision.next();
    rusqlite::Connection::open(dir.join("head.db"))
        .unwrap()
        .execute(
            "UPDATE subject_branch_heads SET record_revision=?2,data=?3 WHERE agent_id=?1",
            rusqlite::params![
                agent.to_string(),
                head.record_revision.0 as i64,
                serde_json::to_string(&head).unwrap()
            ],
        )
        .unwrap();

    let armed = store
        .arm_publication(intent_id, &publication_dispatch())
        .unwrap();
    assert!(armed.is_none());
    assert!(store.external_operation(intent_id).is_err());
    assert_eq!(publication_state(&store, intent_id), "blocked");
    let outcome_ref = match store.external_task(intent_id).unwrap().state {
        TaskState::Terminal { outcome_ref } => outcome_ref,
        other => panic!("the superseded reply must retire its task, got {other:?}"),
    };
    let detail: serde_json::Value =
        serde_json::from_slice(&store.cas.read_object(&outcome_ref.digest).unwrap()).unwrap();
    assert_eq!(detail["reason_kind"], "authority_superseded");
    assert_eq!(detail["effect_status"], "none");
}

/// Real process boundary: one process arms the send and dies; the next open
/// must see the same single attempt, record the loss of control as UNKNOWN,
/// never arm a second send, and accept the platform's late answer for that
/// original attempt.
#[test]
fn armed_publication_survives_a_real_process_death_without_a_second_send() {
    use shenko_types::ids::*;
    use shenko_types::operation::{AttemptDispatchStage, EffectStatus, OperationState};
    use shenko_types::task::TaskState;

    if let Some(dir) = std::env::var_os("SHENKO_C01_PUBLICATION_ARM_CHILD") {
        let dir = PathBuf::from(dir);
        let store = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
        store.start_subject_dispatcher().unwrap();
        let intent_id: EntityId = std::fs::read_to_string(dir.join("intent"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let armed = store
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .expect("the child process arms the send");
        let invocation_id = store
            .external_operation_attempts(intent_id)
            .unwrap()
            .first()
            .map(|attempt| attempt.invocation_id);
        std::fs::write(
            dir.join("armed.json"),
            serde_json::to_vec(&serde_json::json!({
                "operation_id": armed.operation_id,
                "attempt_id": armed.attempt_id,
                "armed_commit": armed.armed_commit,
                "invocation_id": invocation_id,
            }))
            .unwrap(),
        )
        .unwrap();
        // Actual process death immediately after T1 committed: no send was
        // observed by anyone and no destructor ever runs.
        #[cfg(unix)]
        {
            std::process::Command::new("kill")
                .args(["-KILL", &std::process::id().to_string()])
                .status()
                .unwrap();
            unreachable!("SIGKILL must end the process");
        }
        #[cfg(not(unix))]
        std::process::exit(73);
    }

    let (dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    commit_publication_step(&store, agent, &intent);
    std::fs::write(dir.join("intent"), intent_id.to_string()).unwrap();
    drop(store);

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "armed_publication_survives_a_real_process_death_without_a_second_send",
            "--nocapture",
        ])
        .env("SHENKO_C01_PUBLICATION_ARM_CHILD", &dir)
        .status()
        .unwrap();
    assert!(!status.success(), "the child must die, not return");
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );
    let child: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("armed.json")).unwrap()).unwrap();
    let attempt_id: EntityId = child["attempt_id"].as_str().unwrap().parse().unwrap();
    let armed_commit: shenko_types::commit::CommitRef =
        serde_json::from_value(child["armed_commit"].clone()).unwrap();

    // A new incarnation takes the dispatch owner. The armed send lost its
    // process: that is local loss of control, never proof the platform did not
    // receive it.
    let store = Arc::new(Store::open(dir.join("head.db"), dir.join("cas")).unwrap());
    store.start_subject_dispatcher().unwrap();
    assert_eq!(publication_state(&store, intent_id), "unknown");
    assert!(
        store
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .is_none(),
        "the dead process's armed attempt must never be re-armed"
    );
    assert_eq!(store.external_operation_attempts(intent_id).unwrap().len(), 1);
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::InFlight);
    assert_eq!(operation.prepared_commit, armed_commit);

    // Reconciliation records the restart as UNKNOWN evidence and stops there.
    store.reconcile_subject_publications().unwrap();
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::Reconciling);
    assert_eq!(operation.effect_status, EffectStatus::Unknown);
    assert_eq!(operation.outcome, None);
    assert_eq!(publication_state(&store, intent_id), "unknown");
    let attempt = store
        .external_operation_attempts(intent_id)
        .unwrap()
        .remove(0);
    assert_eq!(attempt.attempt_id, attempt_id);
    assert_eq!(attempt.dispatch_stage, AttemptDispatchStage::Unknown);

    // The platform's own answer for that same attempt is the late E1 the
    // recovery contract admits; it resolves the operation without a resend.
    store
        .record_publication_receipt(
            &shenko_store::channel::ArmedPublication {
                operation_id: intent_id,
                attempt_id,
                task_id: intent_id,
                account: "qq:10001".into(),
                armed_commit,
            },
            &shenko_types::channel::ChannelReceipt {
                status: "ok".into(),
                retcode: 0,
                accepted: true,
                provider_message_id: Some("message-1".into()),
                raw: serde_json::json!({"status":"ok","retcode":0,"message_id":"message-1"}),
                observed_at: chrono::Utc::now(),
            },
        )
        .unwrap();
    store.reconcile_subject_publications().unwrap();
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::Resolved);
    assert_eq!(operation.effect_status, EffectStatus::Complete);
    assert_eq!(publication_state(&store, intent_id), "settled");
    assert_eq!(
        store
            .committed_intent(intent_id)
            .unwrap()
            .expect("the settled publication is readable by identity")
            .state,
        "settled"
    );
    match store.external_task(intent_id).unwrap().state {
        TaskState::Terminal { .. } => {}
        other => panic!("the settled publication must retire its task, got {other:?}"),
    }
    // Both originals are preserved; there is still exactly one attempt.
    assert_eq!(
        store.external_operation_evidence(intent_id).unwrap().len(),
        2
    );
    assert_eq!(store.external_operation_attempts(intent_id).unwrap().len(), 1);
}

/// The durable ready lane keeps row identity next to the original bytes, so a
/// consumer can name a result it cannot read instead of failing the whole
/// dispatch pass — while the strict reader still refuses that corruption.
#[test]
fn unreadable_ready_native_result_keeps_its_identity_and_the_row() {
    use shenko_types::ids::*;
    use shenko_types::task::{TaskOutcome, TaskResult};

    let (dir, store, agent) = initialized_subject();
    let evidence_ref = store
        .cas
        .seal_object(b"evidence", "application/json", "test", "raw", 1)
        .unwrap()
        .object;
    let result_id = EntityId::new();
    let healthy = TaskResult {
        schema_version: 1,
        result_id,
        task_id: EntityId::new(),
        task_revision: TaskRevision(0),
        task_record_revision: RecordRevision(0),
        invocation_id: EntityId::new(),
        operation_id: EntityId::new(),
        receipt_id: EntityId::new(),
        outcome: TaskOutcome::Succeeded,
        result: Some(serde_json::json!({"stdout_utf8":"ok"})),
        evidence_ref,
        commit_ref: shenko_types::commit::CommitRef::new(store.ledger.ledger_id(), LedgerSeq(1)),
    };
    // The exact durable shape `finalize_task` writes (kind/source/consumed),
    // with one row from a build whose result shape this one cannot read.
    let unreadable_id = EntityId::new();
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    for (event_id, data) in [
        (
            unreadable_id,
            r#"{"schema_version":99,"future_shape":true}"#.to_string(),
        ),
        (result_id, serde_json::to_string(&healthy).unwrap()),
    ] {
        conn.execute(
            "INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
             VALUES(?1,?2,'task_result','native-child',0,?3)",
            rusqlite::params![event_id.to_string(), agent.to_string(), data],
        )
        .unwrap();
    }
    drop(conn);

    // The row lane preserves identity and bytes for both rows: a consumer can
    // journal the one it cannot read and still deliver the other.
    let rows = store.ready_native_result_rows(agent, 8).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, unreadable_id);
    assert_eq!(rows[0].1, r#"{"schema_version":99,"future_shape":true}"#);
    assert_eq!(rows[1].0, result_id);

    // The strict reader still refuses to invent a result from bad bytes.
    assert!(store.ready_native_results(agent, 8).is_err());

    // Nothing was deleted or consumed by the refusal.
    let pending = store.ready_native_result_rows(agent, 8).unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|(event_id, _)| *event_id == unreadable_id));
}


/// The built-in IM lists the subject's external conversations from its own
/// records. A channel message is read where the runtime keeps it (the staged
/// input of the step that took it); the plugin's audience shape travels with
/// the thread; a publication carries the time the provider answered; and the
/// platform's echo of that publication is the publication, not a second line.
#[test]
fn channel_conversation_reads_staged_messages_and_hides_publication_echoes() {
    use shenko_types::channel::{
        AudienceShape, ChannelActor, ChannelContent, ChannelEnvelope, ChannelReceipt,
        ChannelReplyTarget,
    };
    use shenko_types::ids::{BranchId, EntityId};
    let (_dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();

    // A second real subject shares this Store, including its raw sessions
    // and publication ledger. Channel accounts do not identify the subject.
    let (mut foreign_head, mut foreign_first) = store.load_subject(agent).unwrap();
    let mut foreign_genesis = store.get_subject_genesis(agent).unwrap().unwrap();
    let foreign_agent = EntityId::new();
    foreign_genesis.genesis_id = EntityId::new();
    foreign_genesis.subject_id = foreign_agent;
    foreign_genesis.bootstrap_id = EntityId::new();
    foreign_genesis.launch_id = EntityId::new();
    foreign_head.agent_id = foreign_agent;
    foreign_head.branch_id = BranchId::new();
    foreign_head.subject_root_task_id = EntityId::new();
    foreign_head.genesis_ref.genesis_id = foreign_genesis.genesis_id;
    foreign_head.next_step_id = EntityId::new();
    foreign_first.step_id = foreign_head.next_step_id;
    foreign_first.branch_id = foreign_head.branch_id.clone();
    let foreign_root = shenko_types::task::TaskRecord::subject_root(
        foreign_head.subject_root_task_id,
        foreign_agent,
        foreign_genesis.bootstrap_event_ref.clone(),
        foreign_genesis.created_commit,
    );
    store
        .initialize_genesis(&foreign_genesis, &foreign_head, &foreign_first, &foreign_root)
        .unwrap();

    let intent = publication_intent();
    commit_publication_step(&store, agent, &intent);
    let armed = store
        .arm_publication(intent.intent_id, &publication_dispatch())
        .unwrap()
        .expect("the committed publication arms once");
    let sent_at = chrono::Utc::now();
    let sent_at_text = sent_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .record_publication_receipt(
            &armed,
            &ChannelReceipt {
                status: "ok".into(),
                retcode: 0,
                accepted: true,
                provider_message_id: Some("900".into()),
                raw: serde_json::json!({"status":"ok","retcode":0,"data":{"message_id":900}}),
                observed_at: sent_at,
            },
        )
        .unwrap();

    let audience = ChannelReplyTarget {
        kind: "group".into(),
        id: "123456789".into(),
        label: "群 123456789".into(),
        account: "qq:10001".into(),
        tenant: None,
        conversation: Some("123456789".into()),
        shape: Some(AudienceShape::Group),
    };
    let line = |message_id: &str,
                actor: &str,
                own_account: bool,
                text: &str,
                provider: &str,
                target: &ChannelReplyTarget| {
        let envelope = ChannelEnvelope {
            schema_version: 1,
            provider: provider.into(),
            account: target.account.clone(),
            tenant: target.tenant.clone(),
            transport_identity_domain: "onebot".into(),
            conversation: target.conversation.clone(),
            thread: None,
            message_id: message_id.into(),
            source_event_revision: None,
            occurred_at: Some(sent_at),
            received_at: sent_at,
            actor: ChannelActor {
                platform_id: actor.into(),
                display_name: Some(actor.into()),
                alias: None,
                person_ref: None,
                role_grant_ref: None,
                group_role: None,
                own_account: Some(own_account),
            },
            content: ChannelContent {
                text: text.into(),
                segments: vec![],
                mentions: vec![],
                reply_to: None,
            },
            reply_target: Some(target.clone()),
            attachments: vec![],
            raw_ref: None,
        };
        let event = shenko_types::inbox::InboxEvent {
            event_id: shenko_types::ids::EntityId::new(),
            kind: shenko_types::inbox::InboxEventKind::Message,
            payload: serde_json::to_value(&envelope).unwrap(),
            created_at: sent_at,
            source: provider.into(),
        };
        shenko_store::raw_session::SubjectHistoryInput {
            event_id: event.event_id,
            event_type: "user".into(),
            data: serde_json::json!({"v":1, "inbox_event": event, "text": text}),
        }
    };
    let counterpart = line("901", "20002", false, "有人在吗", "qq", &audience);
    let same_id_counterpart = line("900", "20002", false, "对方的同号消息", "qq", &audience);
    let echo = line("900", "10001", true, "已经提交的回复正文", "qq", &audience);
    let other_account = line(
        "900",
        "30003",
        true,
        "另一账号的消息",
        "qq",
        &ChannelReplyTarget {
            account: "qq:30003".into(),
            ..audience.clone()
        },
    );
    let other_audience = line(
        "900",
        "10001",
        true,
        "另一群的消息",
        "qq",
        &ChannelReplyTarget {
            id: "987654321".into(),
            label: "群 987654321".into(),
            conversation: Some("987654321".into()),
            ..audience.clone()
        },
    );
    let other_kind = line(
        "900",
        "10001",
        true,
        "私聊的同号消息",
        "qq",
        &ChannelReplyTarget {
            kind: "private".into(),
            label: "私聊 123456789".into(),
            shape: Some(AudienceShape::OneToOne),
            ..audience.clone()
        },
    );
    let other_provider = line("900", "10001", true, "另一通道的消息", "relay", &audience);
    let mut own = line("902", "10001", true, "手机上补一句", "qq", &audience);
    own.data["inbox_event"]["payload"]["occurred_at"] = serde_json::Value::Null;
    let (head, planned) = store.load_subject(agent).unwrap();
    store
        .stage_subject_inputs(
            &head,
            &planned,
            &[
                counterpart.clone(),
                same_id_counterpart.clone(),
                echo,
                other_account.clone(),
                other_audience.clone(),
                other_kind.clone(),
                other_provider.clone(),
                own.clone(),
            ],
        )
        .unwrap();

    // Two later steps each re-stage the same real message, followed by
    // unclassified and audience-less message inputs. The six newest raw
    // rows contain only one admissible identity: a three-message window
    // must continue into the older step, past both noise and duplicates.
    for _ in 0..2 {
        let (head, planned) = store.load_subject(agent).unwrap();
        let result = recorded_result(&store, &head, planned);
        let next = shenko_types::ids::EntityId::new();
        let committed = committed_candidate(&store, result, next);
        store
            .commit_step_and_advance_head(&committed, &head, next)
            .unwrap();
        let (head, planned) = store.load_subject(agent).unwrap();
        let mut unclassified = line("903", "20002", false, "原始通知", "qq", &audience);
        unclassified.data["inbox_event"]["payload"] = serde_json::json!({"notice":true});
        let mut without_audience = line("904", "20002", false, "没有回复受众", "qq", &audience);
        without_audience.data["inbox_event"]["payload"]["reply_target"] = serde_json::Value::Null;
        store
            .stage_subject_inputs(
                &head,
                &planned,
                &[own.clone(), unclassified, without_audience],
            )
            .unwrap();
    }

    // The foreign publication deliberately shares the first subject's
    // canonical channel target and its phone message's provider id. Its
    // receipt must not suppress that phone line in the first subject's view.
    let mut foreign_intent = publication_intent();
    foreign_intent.content = "另一个主体的回复".into();
    commit_publication_step(&store, foreign_agent, &foreign_intent);
    let mut foreign_dispatch = publication_dispatch();
    foreign_dispatch.params["message"] = serde_json::json!(foreign_intent.content);
    let foreign_armed = store
        .arm_publication(foreign_intent.intent_id, &foreign_dispatch)
        .unwrap()
        .expect("the foreign subject's own publication arms once");
    store
        .record_publication_receipt(
            &foreign_armed,
            &ChannelReceipt {
                status: "ok".into(),
                retcode: 0,
                accepted: true,
                provider_message_id: Some("902".into()),
                raw: serde_json::json!({"status":"ok","retcode":0,"data":{"message_id":902}}),
                observed_at: sent_at,
            },
        )
        .unwrap();
    let foreign_input = line("905", "40004", false, "另一个主体的来信", "qq", &audience);
    let foreign_echo = line("902", "10001", true, &foreign_intent.content, "qq", &audience);
    let (foreign_head, foreign_planned) = store.load_subject(foreign_agent).unwrap();
    store
        .stage_subject_inputs(
            &foreign_head,
            &foreign_planned,
            &[foreign_input.clone(), foreign_echo],
        )
        .unwrap();

    let before_read = store.current_commit_seq().unwrap();
    let all_rows = store.channel_conversation_messages(agent, 50).unwrap();
    assert_eq!(all_rows.len(), 8, "seven unique messages plus one publication");
    for input in [
        &counterpart,
        &same_id_counterpart,
        &other_account,
        &other_audience,
        &other_kind,
        &other_provider,
        &own,
    ] {
        let event_id = input.event_id.to_string();
        let matches: Vec<_> = all_rows
            .iter()
            .filter(|row| row.message_id == event_id)
            .collect();
        assert_eq!(matches.len(), 1, "the original inbox identity appears once");
        assert_eq!(
            matches[0].text,
            input.data["inbox_event"]["payload"]["content"]["text"].as_str().unwrap(),
        );
        assert_eq!(matches[0].at.as_deref(), Some(sent_at_text.as_str()));
        assert_eq!(
            matches[0].direction,
            if input.data["inbox_event"]["payload"]["actor"]["own_account"].as_bool() == Some(true) {
                "own"
            } else {
                "inbound"
            },
        );
    }

    let recent = store.channel_conversation_messages(agent, 3).unwrap();
    let mut recent_ids: Vec<_> = recent
        .iter()
        .filter(|row| row.direction != "outbound")
        .map(|row| row.message_id.clone())
        .collect();
    recent_ids.sort();
    let mut expected_ids = vec![
        own.event_id.to_string(),
        other_provider.event_id.to_string(),
        other_kind.event_id.to_string(),
    ];
    expected_ids.sort();
    assert_eq!(
        recent_ids,
        expected_ids,
        "fill the window with the most recent unique admitted messages",
    );
    let foreign_rows = store.channel_conversation_messages(foreign_agent, 50).unwrap();
    assert_eq!(foreign_rows.len(), 2, "the foreign subject sees only its own messages");
    let foreign_input_id = foreign_input.event_id.to_string();
    assert!(foreign_rows.iter().any(|row| {
        row.message_id == foreign_input_id && row.direction == "inbound"
    }));
    let foreign_intent_id = foreign_intent.intent_id.to_string();
    assert!(foreign_rows.iter().any(|row| {
        row.message_id == foreign_intent_id && row.direction == "outbound"
    }));
    assert_eq!(
        store.current_commit_seq().unwrap(),
        before_read,
        "conversation reads do not commit facts",
    );

    let rows: Vec<_> = all_rows
        .iter()
        .filter(|row| row.thread_key == "qq:10001|group|123456789")
        .collect();
    let lines: Vec<_> = rows.iter().map(|row| (row.direction, row.text.as_str())).collect();
    assert!(lines.contains(&("inbound", "有人在吗")), "{lines:?}");
    assert!(lines.contains(&("own", "手机上补一句")), "{lines:?}");
    assert_eq!(
        rows.iter()
            .filter(|row| row.text == "已经提交的回复正文")
            .map(|row| row.direction)
            .collect::<Vec<_>>(),
        vec!["outbound"],
        "the platform's echo of a publication is the publication itself"
    );
    let outbound = rows.iter().find(|row| row.direction == "outbound").unwrap();
    assert_eq!(outbound.message_id, intent.intent_id.to_string());
    assert_eq!(outbound.at.as_deref(), Some(sent_at_text.as_str()));
    assert!(rows
        .iter()
        .filter(|row| row.direction != "outbound")
        .all(|row| row.thread_shape == Some(AudienceShape::Group)));
}
