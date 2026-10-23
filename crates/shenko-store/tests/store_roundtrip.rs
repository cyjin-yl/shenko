//! S1 验证：SQLite WAL store round-trip + CAS seal/read
//! 验证 §41.2: 单 mutation line, CAS object seal/read, FTS5 detection

use shenko_store::store::Store;
use shenko_types::store::WriteBatch;
use std::path::PathBuf;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-test-{}", std::process::id()));
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
    let sealed = store.cas.seal_object(data, "text/plain", "test", "raw", 1).unwrap();

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

    assert_eq!(receipt.durability_profile, shenko_types::commit::DurabilityProfile::PowerLossLocal);
    assert!(!receipt.commit_ref.ledger_id.as_uuid().is_nil());
}

#[test]
fn s1_commit_advances_branch_head() {
    use shenko_types::subject::{
        SubjectGenesis, SubjectBranchHead, SubjectStep, SubjectStepState,
        SubjectMode, GenesisRef,
    };
    use shenko_types::ids::*;
    use shenko_types::object::{ObjectRef, SchemaRef, Digest};
    use shenko_types::commit::CommitRef;

    let dir = tmp_dir();
    let db = dir.join("head.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let agent_id = EntityId::new();
    let branch_id = BranchId::new();
    let genesis_id = EntityId::new();
    let bootstrap_id = EntityId::new();
    let launch_id = EntityId::new();
    let first_step_id = EntityId::new();
    let second_step_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();

    let codec = SchemaRef { name: "raw".into(), major: 1, digest: Digest::blake3(b"raw") };
    let dummy_ref = ObjectRef {
        digest: Digest::blake3(b"seed"), bytes: 4,
        media_type: "application/json".into(),
        store_namespace: "test".into(), locator: "".into(), codec: codec.clone(),
    };
    let genesis_commit = CommitRef::new(ledger_id, LedgerSeq(0));

    let genesis = SubjectGenesis {
        genesis_id, subject_id: agent_id, bootstrap_id, launch_id,
        seed_manifest_ref: dummy_ref.clone(),
        initial_profile_ref: dummy_ref.clone(),
        home_manifest_ref: dummy_ref.clone(),
        bootstrap_event_ref: dummy_ref.clone(),
        created_commit: genesis_commit,
    };

    let head = SubjectBranchHead {
        agent_id, branch_id: branch_id.clone(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef { ledger_id, genesis_id },
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
        decision_ref: None,
        commit_ref: None,
        retry_reason_ref: None,
        record_revision: RecordRevision(0),
        created_by_step_id: None,
    };

    // Initialize genesis atomically
    let receipt = store.initialize_genesis(&genesis, &head, &first_step).unwrap();
    assert_eq!(receipt.updated.len(), 3);

    // Now simulate S1: commit step 1 and advance head to point at step 2
    let mut committed_step = first_step.clone();
    committed_step.state = SubjectStepState::Committed;
    committed_step.record_revision = RecordRevision(1);

    let receipt2 = store.commit_step_and_advance_head(
        &committed_step,
        agent_id,
        branch_id.clone(),
        second_step_id,
        RecordRevision(0),
    ).unwrap();

    // Receipt should have 2 updated entities (step + head)
    assert_eq!(receipt2.updated.len(), 2);
    // head entity_id == agent_id
    let head_update = receipt2.updated.iter().find(|u| u.entity_id == agent_id).unwrap();
    assert_eq!(head_update.record_revision, RecordRevision(1));
}

#[test]
fn task_roundtrip() {
    use shenko_types::task::{TaskRecord, TaskOwner, TaskState, ImplementationRef, DispatchEligibility};
    use shenko_types::ids::*;
    use shenko_types::commit::CommitRef;
    use shenko_types::object::{ObjectRef, SchemaRef, Digest};

    let dir = tmp_dir();
    let db = dir.join("task.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let agent_id = EntityId::new();
    let conv_id = EntityId::new();
    let task_id = EntityId::new();
    let commit_ref = CommitRef::new(store.ledger.ledger_id(), shenko_types::ids::LedgerSeq(1));

    let codec = SchemaRef { name: "raw".into(), major: 1, digest: Digest::blake3(b"raw") };
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
        state: TaskState::Pending { checkpoint_ref: None },
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
