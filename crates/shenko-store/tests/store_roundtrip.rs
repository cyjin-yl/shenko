//! S1 验证：SQLite WAL store round-trip + CAS seal/read
//! 验证 §41.2: 单 mutation line, CAS object seal/read, FTS5 detection

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
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
    use shenko_types::subject::{
        GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
    };

    let dir = tmp_dir();
    let db = dir.join("head.db");
    let cas = dir.join("cas");
    let store = Arc::new(Store::open(db, cas).unwrap());

    let agent_id = EntityId::new();
    let branch_id = BranchId::new();
    let genesis_id = EntityId::new();
    let bootstrap_id = EntityId::new();
    let launch_id = EntityId::new();
    let first_step_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();

    let dummy_ref = store
        .cas
        .seal_object(b"seed", "application/json", "test", "raw", 1)
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
    let receipt = store
        .initialize_genesis(&genesis, &head, &first_step)
        .unwrap();
    assert_eq!(receipt.updated.len(), 3);

    (dir, store, agent_id)
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
    mut step: shenko_types::subject::SubjectStep,
    semantic_wait: Option<shenko_types::subject::SemanticWait>,
) -> shenko_types::subject::SubjectStep {
    use shenko_types::{ids::*, subject::*};
    let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
        shenko_context_policy::StoreWindowLog::new(Arc::clone(store), EntityId(head.branch_id.0)),
    ));
    let profile = shenko_types::object::Digest::blake3(b"store-roundtrip-profile");
    ledger.rebind_model(1, &profile.0).unwrap();
    let window = ledger.current().unwrap();
    store.stage_subject_inputs(head, &step, &[]).unwrap();
    let plan = serde_json::json!({
        "schema_version":10, "step_id":step.step_id, "step_no":step.step_no, "events":[],
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
        "subject_step_input", 10).unwrap().object);
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
    mut step: shenko_types::subject::SubjectStep,
    next: shenko_types::ids::EntityId,
) -> shenko_types::subject::SubjectStep {
    use shenko_types::{ids::*, subject::*};
    let decision = StepDecision {
        schema_version: 6,
        output: shenko_types::output::ParsedOutput {
            schema_version: 3,
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
        }],
        delegated_task_intents: vec![],
        child_dialogue_intents: vec![],
        memory_intents: vec![],
        policy_intents: vec![],
        publication_intents: vec![],
        pending_obligations: vec![],
        evidence_refs: vec![],
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
                6,
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
                6,
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
    let target = Store::open(dir.join("replay.db"), dir.join("replay-cas")).unwrap();
    target.import_cognition_archive(&archive[..], &CognitionImportOptions::default()).unwrap();

    let first_page = target.committed_decisions_after(0, None, 1).unwrap();
    assert_eq!(first_page.len(), 1);
    let (local_seq, first, _) = &first_page[0];
    assert_eq!(first.step_id, first_id);
    assert_eq!(first.step_no, 1);
    assert_eq!(first.commit_ref, Some(first_receipt.commit_ref));
    let second_page = target.committed_decisions_after(*local_seq, Some(first.step_id), 1).unwrap();
    assert_eq!(second_page.len(), 1);
    let (next_seq, second, _) = &second_page[0];
    assert_eq!(*next_seq, *local_seq);
    assert_eq!(second.step_id, second_id);
    assert_eq!(second.step_no, 2);
    assert_eq!(second.commit_ref, Some(second_receipt.commit_ref));
    assert!(target.committed_decisions_after(*next_seq, Some(second.step_id), 1).unwrap().is_empty());
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
        "application/json", "decisions", "StepDecision", 6).unwrap().object);
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
