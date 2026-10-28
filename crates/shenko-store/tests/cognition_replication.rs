//! Cognition recovery replication — SPEC §5.4.
//!
//! A store hands another node its real facts and the verified closure of the
//! objects they reference. The receiving store publishes nothing before the
//! whole closure arrived, keeps diverging facts on both sides and never
//! dispatches imported work.

use shenko_store::cognition::{
    CognitionAck, CognitionImportOptions, CognitionSnapshotOptions,
};
use shenko_store::memory::MemoryEntry;
use shenko_store::store::Store;
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::{
    BranchId, ControllerEpoch, EntityId, LedgerSeq, RecordRevision, RuntimeGeneration, TaskRevision,
};
use shenko_types::identity::{IdentityProfile, IdentitySource, NamingOrigin};
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionedRef};
use shenko_types::operation::{
    AttemptDispatchStage, AttemptRecord, AuthorityRef, EffectStatus, OperationContractRef,
    OperationRecord, OperationState,
};
use shenko_types::store::{RawInboxEvent, RawSession, RawSessionEvent, WriteBatch};
use shenko_types::subject::{
    GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
};
use shenko_types::task::{
    DispatchEligibility, ImplementationRef, TaskOwner, TaskRecord, TaskState,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-cognition-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn legacy_pending(db: &Path, agent: EntityId, kind: &str, content: &str) -> EntityId {
    let id = EntityId::new();
    rusqlite::Connection::open(db).unwrap().execute(
        "INSERT INTO memory_intents
         (intent_id,agent_id,kind,scope,content,content_ref,entry_id,state,reason,created_at_ms,updated_at_ms)
         VALUES (?1,?2,?3,'personal',?4,'legacy-locator',?5,'accepted',NULL,1,1)",
        rusqlite::params![id.to_string(), agent.to_string(), kind, content, EntityId::new().to_string()],
    ).unwrap();
    id
}

#[test]
fn legacy_pending_library_reference_is_not_a_verified_recovery() {
    let dir = tmp_dir();
    let db = dir.join("old.db");
    let source = Store::open(db.clone(), dir.join("old-cas")).unwrap();
    let intent = legacy_pending(&db, EntityId::new(), "save", "");
    let mut snapshot = source.export_cognition(&CognitionSnapshotOptions::default()).unwrap();
    assert!(snapshot.manifest.gaps.iter().any(|gap| gap.blocking));

    // Old schema 2 senders did not report this missing identity as a gap.
    snapshot.manifest.schema = 2;
    snapshot.manifest.gaps.clear();
    let mut archive = Vec::new();
    snapshot.write_archive(&mut archive).unwrap();
    let receiver = Store::open(dir.join("receiver.db"), dir.join("receiver-cas")).unwrap();
    assert!(receiver.import_cognition_archive(&archive[..], &Default::default()).is_err());
    assert_eq!(receiver.memory_intent_state(intent).unwrap(), None);
    assert_conflicting_import_cannot_graft_a_binding_onto_a_legacy_intent();
    assert_binding_conflict_does_not_advance_a_partial_baseline();
    assert_conflicting_entry_is_not_exposed_or_overwritten();
}

fn assert_conflicting_import_cannot_graft_a_binding_onto_a_legacy_intent() {
    use shenko_store::MemoryIntentRecord;
    let dir = tmp_dir();
    let target_db = dir.join("target.db");
    let target = Store::open(target_db.clone(), dir.join("target-cas")).unwrap();
    let agent = EntityId::new();
    let intent = legacy_pending(&target_db, agent, "save", "");
    let source = Store::open(dir.join("source.db"), dir.join("source-cas")).unwrap();
    let body = "不同来源的正文";
    let object = source.cas.seal_object(body.as_bytes(), "text/plain", "memory", "raw", 1).unwrap().object;
    let entry_id = target.memory_intent(intent).unwrap().unwrap().record.entry_id.unwrap();
    let record = MemoryIntentRecord::accepted(intent, agent, "save", "personal", "", Some(object.locator.clone()), Some(entry_id));
    source.admit_memory_intent(&record, Some(&object)).unwrap();
    let mut source_entry = MemoryEntry::new(agent, body);
    source_entry.entry_id = entry_id;
    source_entry.kind = "save".into();
    source.commit_memory_intent(&source_entry, intent).unwrap();
    let mut archive = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut archive).unwrap();
    let outcome = target.import_cognition_archive(&archive[..], &Default::default()).unwrap();
    assert!(!outcome.ack(EntityId::new()).verified);
    let local = target.memory_intent(intent).unwrap().unwrap();
    assert!(local.content_ref.is_none(), "conflicting source binding must not attach to a legacy local intent");
    let mut wrong = MemoryEntry::new(agent, body);
    wrong.entry_id = local.record.entry_id.unwrap();
    wrong.kind = "save".into();
    assert!(target.commit_memory_intent(&wrong, intent).is_err());
    assert!(target.list_memory(10).unwrap().is_empty());
}

fn assert_binding_conflict_does_not_advance_a_partial_baseline() {
    use shenko_store::MemoryIntentRecord;
    let dir = tmp_dir();
    let db = dir.join("source.db");
    let source = Store::open(db.clone(), dir.join("source-cas")).unwrap();
    let target = Store::open(dir.join("target.db"), dir.join("target-cas")).unwrap();
    let agent = EntityId::new();
    let intent = EntityId::new();
    let entry_id = EntityId::new();
    let original = source.cas.seal_object(b"original body", "text/plain", "memory", "raw", 1).unwrap().object;
    let replacement = source.cas.seal_object(b"different body", "text/plain", "memory", "raw", 1).unwrap().object;
    let record = MemoryIntentRecord::accepted(intent, agent, "save", "personal", "", Some(original.locator.clone()), Some(entry_id));
    source.admit_memory_intent(&record, Some(&original)).unwrap();
    let options = CognitionImportOptions { peer_id: Some(EntityId::new()), ..Default::default() };
    let export = || {
        let mut bytes = Vec::new();
        source.write_cognition_archive(&Default::default(), None, &mut bytes).unwrap();
        bytes
    };
    assert!(target.import_cognition_archive(&export()[..], &options).unwrap().ack(EntityId::new()).verified);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let replace_binding = |object: &ObjectRef| {
        let data = serde_json::json!({"schema_version": 1, "content_ref": object}).to_string();
        conn.execute("UPDATE memory_intents SET content_ref=?1 WHERE intent_id=?2", rusqlite::params![object.locator, intent.to_string()]).unwrap();
        conn.execute("UPDATE memory_intent_bindings SET data=?1 WHERE intent_id=?2", rusqlite::params![data, intent.to_string()]).unwrap();
    };
    replace_binding(&replacement);
    let conflict = target.import_cognition_archive(&export()[..], &options).unwrap();
    assert!(!conflict.ack(EntityId::new()).verified);
    assert_eq!(target.memory_intent(intent).unwrap().unwrap().content_ref.unwrap().unwrap().digest, original.digest);
    assert_eq!(target.memory_intent_state(intent).unwrap().as_deref(), Some("accepted"));

    replace_binding(&original);
    let mut entry = MemoryEntry::new(agent, "original body");
    entry.entry_id = entry_id;
    entry.kind = "save".into();
    source.commit_memory_intent(&entry, intent).unwrap();
    let continued = target.import_cognition_archive(&export()[..], &options).unwrap();
    assert!(continued.ack(EntityId::new()).verified);
    assert_eq!(target.memory_intent_state(intent).unwrap().as_deref(), Some("committed"));
    assert_eq!(target.list_memory(10).unwrap()[0].content, "original body");

    conn.execute("UPDATE memory_intents SET state='accepted',reason=NULL WHERE intent_id=?1", [intent.to_string()]).unwrap();
    let rollback = target.import_cognition_archive(&export()[..], &options).unwrap();
    assert!(!rollback.ack(EntityId::new()).verified);
    assert_eq!(target.memory_intent_state(intent).unwrap().as_deref(), Some("committed"));
}

fn assert_conflicting_entry_is_not_exposed_or_overwritten() {
    use shenko_store::MemoryIntentRecord;
    let dir = tmp_dir();
    let source = Store::open(dir.join("source.db"), dir.join("source-cas")).unwrap();
    let target = Store::open(dir.join("target.db"), dir.join("target-cas")).unwrap();
    let agent = EntityId::new();
    let intent = EntityId::new();
    let entry_id = EntityId::new();
    let request = |content| MemoryIntentRecord::accepted(intent, agent, "save", "personal", content, None, Some(entry_id));
    target.admit_memory_intent(&request("local A"), None).unwrap();
    source.admit_memory_intent(&request("remote B"), None).unwrap();
    let entry = |content| {
        let mut entry = MemoryEntry::new(agent, content);
        entry.entry_id = entry_id;
        entry.kind = "save".into();
        entry
    };
    source.commit_memory_intent(&entry("remote B"), intent).unwrap();
    let mut archive = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut archive).unwrap();
    let outcome = target.import_cognition_archive(&archive[..], &Default::default()).unwrap();
    assert!(!outcome.ack(EntityId::new()).verified);
    assert!(target.list_memory(10).unwrap().is_empty());
    target.commit_memory_intent(&entry("local A"), intent).unwrap();
    assert_eq!(target.list_memory(10).unwrap()[0].content, "local A");
    assert_eq!(source.list_memory(10).unwrap()[0].content, "remote B");
}

fn version_ref(name: &str) -> VersionedRef {
    VersionedRef {
        artifact: ArtifactRef {
            digest: Digest::blake3(name.as_bytes()),
            locator: format!("file://{name}"),
        },
        symbol: name.to_string(),
        schema: SchemaRef {
            name: name.to_string(),
            major: 1,
            digest: Digest::blake3(name.as_bytes()),
        },
    }
}

struct Seeded {
    store: Arc<Store>,
    agent_id: EntityId,
    genesis_id: EntityId,
    session_id: EntityId,
    inbox_event_id: EntityId,
    task_id: EntityId,
    operation_id: EntityId,
    attempt_id: EntityId,
    home: ObjectRef,
    checkpoint: ObjectRef,
}

/// A store that holds the facts a recovering self needs: identity, subject
/// genesis, raw history, a task, an operation with an unknown effect, the
/// attempt that was armed for it and the receipt that observed it.
fn seeded_store() -> Seeded {
    let dir = tmp_dir();
    let store = Arc::new(Store::open(dir.join("a.db"), dir.join("cas")).unwrap());

    let home = store
        .cas
        .seal_object(b"home manifest bytes", "application/json", "seed", "json", 1)
        .unwrap()
        .object;
    let checkpoint = store
        .cas
        .seal_object(b"checkpoint body", "text/plain", "checkpoints", "raw", 1)
        .unwrap()
        .object;
    let input = store
        .cas
        .seal_object(b"task input", "text/plain", "inputs", "raw", 1)
        .unwrap()
        .object;
    let receipt_raw = store
        .cas
        .seal_object(b"provider said maybe", "text/plain", "evidence", "raw", 1)
        .unwrap()
        .object;

    let agent_id = EntityId::new();
    let genesis_id = EntityId::new();
    let branch_id = BranchId::new();
    let first_step_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let genesis_commit = CommitRef::new(ledger_id, LedgerSeq(0));

    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: home.clone(),
        initial_profile_ref: home.clone(),
        home_manifest_ref: home.clone(),
        bootstrap_event_ref: home.clone(),
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
    store
        .initialize_identity(&IdentityProfile {
            agent_id,
            revision: 1,
            display_name: Some("Recovering Self".into()),
            naming: NamingOrigin::UserGiven,
            soul: Some("keep the thread".into()),
            personality: None,
            source: IdentitySource::Genesis,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

    let commit = CommitRef::new(ledger_id, LedgerSeq(1));
    let session_id = EntityId::new();
    let inbox_event_id = EntityId::new();
    let task_id = EntityId::new();
    let operation_id = EntityId::new();
    let attempt_id = EntityId::new();

    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id,
        conversation_id: agent_id,
        owner: TaskOwner::Task { id: agent_id },
        background: false,
        kind: "delegation".into(),
        definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: EntityId::nil(),
            artifact_digest: Digest::blake3(b"delegation implementation"),
            registry_revision: 1,
        },
        input_ref: input.clone(),
        state: TaskState::Pending {
            checkpoint_ref: Some(checkpoint.clone()),
        },
        abort_request: None,
        memos: serde_json::Map::new(),
        residual_case_ids: vec![],
        created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: commit,
        updated_commit: commit,
    };
    let operation = OperationRecord {
        record_schema_version: 1,
        operation_id,
        record_revision: RecordRevision(0),
        task_id,
        logical_key: "send-report".into(),
        operation_name: "test.send".into(),
        input_schema_version: 1,
        output_schema_version: 1,
        final_input_ref: input.clone(),
        input_digest: Digest::blake3(b"task input"),
        contract_ref: OperationContractRef {
            plugin_id: EntityId::nil(),
            artifact_digest: Digest::blake3(b"contract"),
            contract_digest: Digest::blake3(b"contract digest"),
            receipt_reader_ref: version_ref("receipt-reader"),
        },
        resource_bindings_ref: input.clone(),
        authority_ref: AuthorityRef {
            delegation_id: EntityId::new(),
            delegation_revision: RecordRevision(0),
            decision_ref: EntityId::new(),
        },
        acceptance_predicate_ref: input.clone(),
        retry_contract_ref: input,
        checkpoint_scope_ref: None,
        state: OperationState::InFlight,
        outcome: None,
        outcome_conflict: false,
        effect_status: EffectStatus::Unknown,
        retry_of: None,
        compensates: None,
        possible_duplicate_of: vec![],
        prepared_commit: commit,
        evidence_through: None,
    };
    let attempt = AttemptRecord {
        record_schema_version: 1,
        attempt_id,
        operation_id,
        attempt_no: 1,
        invocation_id: EntityId::new(),
        worker_id: EntityId::new(),
        node_id: EntityId::new(),
        epoch: ControllerEpoch(1),
        implementation_ref: version_ref("provider"),
        dispatch_stage: AttemptDispatchStage::Armed,
        armed_commit: commit,
        provider_handle: None,
        provider_idempotency: None,
        transport_ref: None,
        started_at: None,
        last_observed_at: None,
        final_receipt_id: None,
    };
    let receipt = EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(operation_id),
        attempt_id: Some(attempt_id),
        source_id: "test".into(),
        source_instance_id: "test".into(),
        source_event_key: None,
        producer_epoch: None,
        producer_schema_version: None,
        raw_ref: receipt_raw,
        decoder_ref: None,
        decode_status: DecodeStatus::Uninterpreted,
        observation_level: None,
        normalized_ref: None,
        observed_at: None,
        received_commit: commit,
    };

    let mut batch = WriteBatch::new();
    batch.raw_session_upserts.push(RawSession {
        session_id,
        agent_id,
        started_at: "2026-10-26T00:00:00Z".into(),
        data: "{}".into(),
    });
    batch.session_event_upserts.push(RawSessionEvent {
        event_id: EntityId::new(),
        session_id,
        seq: 1,
        event_type: "message".into(),
        role: "user".into(),
        data: "{\"text\":\"记得这条未竟的工作\"}".into(),
    });
    batch.inbox_event_upserts.push(RawInboxEvent {
        event_id: inbox_event_id,
        agent_id,
        source: "user".into(),
        kind: "message".into(),
        payload_json: "{\"text\":\"还没处理的用户输入\"}".into(),
        received_at_ms: 1_800_000_000_000,
    });
    batch.task_upserts.push(task);
    batch.operation_upserts.push(operation);
    batch.attempt_upserts.push(attempt);
    batch.evidence_upserts.push(receipt);
    store.commit(batch).unwrap();

    Seeded {
        store,
        agent_id,
        genesis_id,
        session_id,
        inbox_event_id,
        task_id,
        operation_id,
        attempt_id,
        home,
        checkpoint,
    }
}

#[test]
fn receiving_store_reads_the_real_facts_after_import() {
    let a = seeded_store();
    let memory = tmp_dir();
    std::fs::write(memory.join("note.md"), "memory body").unwrap();

    let options = CognitionSnapshotOptions {
        memory_root: Some(memory.clone()),
        ..Default::default()
    };
    let mut archive: Vec<u8> = Vec::new();
    let receipt = a
        .store
        .write_cognition_archive(&options, None, &mut archive)
        .unwrap();
    assert!(
        receipt.complete,
        "the fixture closure must be complete: {:?}",
        receipt.gaps
    );
    assert!(receipt.objects > 0, "the referenced objects travel with it");
    assert_eq!(receipt.files, 1);

    let b_dir = tmp_dir();
    let b_memory = tmp_dir();
    let b = Store::open(b_dir.join("b.db"), b_dir.join("cas")).unwrap();
    let outcome = b
        .import_cognition_archive(
            &archive[..],
            &CognitionImportOptions {
                memory_root: Some(b_memory.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(outcome.inserted > 0);
    assert!(outcome.conflicts.is_empty());
    assert_eq!(outcome.verified_objects, receipt.objects);
    drop(b);

    // The facts survive a reopen: this is recovery data, not a live session.
    let b = Store::open(b_dir.join("b.db"), b_dir.join("cas")).unwrap();
    let identity = b
        .latest_identity(a.agent_id)
        .unwrap()
        .expect("the identity arrived");
    assert_eq!(identity.display_name.as_deref(), Some("Recovering Self"));
    assert_eq!(identity.soul.as_deref(), Some("keep the thread"));

    let genesis = b
        .get_subject_genesis(a.agent_id)
        .unwrap()
        .expect("the genesis arrived");
    assert_eq!(genesis.genesis_id, a.genesis_id);
    let (head, step) = b.load_subject(a.agent_id).unwrap();
    assert_eq!(head.committed_step_no, 0);
    assert_eq!(step.step_no, 1);

    let events = b
        .session_events_range(a.session_id, 0, i64::MAX)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].data.contains("记得"));

    // A pending input is recovery data: it arrives still pending.
    let inbox: (i64, String) = rusqlite::Connection::open(b_dir.join("b.db"))
        .unwrap()
        .query_row(
            "SELECT consumed,data FROM inbox_events WHERE event_id=?1",
            [a.inbox_event_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(inbox.0, 0, "a pending input must stay pending");
    assert!(inbox.1.contains("还没处理"));

    // The subject keeps its own genesis, and the provenance says where it
    // came from, so the runtime never compares a foreign home manifest.
    let origin = b
        .cognition_subject_origin(a.agent_id)
        .unwrap()
        .expect("the imported subject has a provenance");
    assert_eq!(origin.source_ledger_id, a.store.ledger.ledger_id());
    assert_eq!(origin.genesis_id, a.genesis_id);
    assert_eq!(origin.fingerprint, receipt.fingerprint);

    let operation = b.external_operation(a.operation_id).unwrap();
    assert_eq!(operation.effect_status, EffectStatus::Unknown);
    let attempt = b.external_operation_attempt(a.operation_id).unwrap();
    assert_eq!(attempt.attempt_id, a.attempt_id);
    assert_eq!(attempt.dispatch_stage, AttemptDispatchStage::Armed);

    // The verified closure arrived with the facts.
    assert_eq!(
        b.cas.read_object(&a.home.digest).unwrap(),
        b"home manifest bytes"
    );
    assert_eq!(
        b.cas.read_object(&a.checkpoint.digest).unwrap(),
        b"checkpoint body"
    );
    assert_eq!(
        std::fs::read_to_string(b_memory.join("note.md")).unwrap(),
        "memory body"
    );

    // Imported work is parked here: a replica is not a second live writer.
    let task = b.external_task(a.task_id).unwrap();
    assert!(matches!(task.state, TaskState::Pending { .. }));
    assert!(matches!(
        task.dispatch_eligibility,
        DispatchEligibility::Parked { .. }
    ));
}

#[test]
fn debt_needs_a_verified_closure_and_a_replay_duplicates_nothing() {
    let a = seeded_store();
    let peer = EntityId::new();
    let options = CognitionSnapshotOptions::default();
    let mut archive: Vec<u8> = Vec::new();
    let receipt = a
        .store
        .write_cognition_archive(&options, Some(peer), &mut archive)
        .unwrap();
    assert!(receipt.complete);

    let status = a.store.cognition_peer_status(peer, &options).unwrap();
    assert!(
        !status.verified_recoverable,
        "an unacknowledged export is not recoverable at the peer"
    );
    assert!(!status.unsent_changes, "the export carries the current snapshot");
    assert!(status.unverified_transfer);

    // A partial transfer never advances the acknowledgement.
    a.store
        .record_cognition_ack(&CognitionAck {
            peer_id: peer,
            fingerprint: receipt.fingerprint.clone(),
            high_water: receipt.high_water,
            verified: false,
            missing: vec![Digest::blake3(b"not delivered")],
            note: Some("transfer stopped".into()),
        })
        .unwrap();
    let status = a.store.cognition_peer_status(peer, &options).unwrap();
    assert!(!status.verified_recoverable);
    assert!(status.last_failure.is_some());

    // A new local fact is debt until the peer verified a newer snapshot.
    a.store
        .insert_memory_entry(&MemoryEntry::new(a.agent_id, "written while offline"))
        .unwrap();
    // The source also continues the same task: that is a continuation, not a
    // divergence.
    let mut advanced = a.store.external_task(a.task_id).unwrap();
    advanced.record_revision = advanced.record_revision.next();
    advanced
        .memos
        .insert("source_continued".into(), serde_json::json!("newer"));
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(advanced);
    a.store.commit(batch).unwrap();
    let status = a.store.cognition_peer_status(peer, &options).unwrap();
    assert!(
        status.unsent_changes,
        "the offline fact is not protected at the peer"
    );
    assert!(!status.verified_recoverable);

    let b_dir = tmp_dir();
    let b = Store::open(b_dir.join("b.db"), b_dir.join("cas")).unwrap();

    // A truncated transfer is refused and publishes nothing.
    let mut broken = archive.clone();
    broken.truncate(broken.len() - 12);
    assert!(
        b.import_cognition_archive(&broken[..], &CognitionImportOptions::default())
            .is_err(),
        "a truncated transfer must not be published"
    );
    assert!(
        b.session_events_range(a.session_id, 0, i64::MAX)
            .unwrap()
            .is_empty(),
        "the refused transfer left no fact behind"
    );

    // The complete transfer arrives and is verified.
    let outcome = b
        .import_cognition_archive(&archive[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(outcome.inserted > 0);
    let ack = outcome.ack(peer);
    assert!(ack.verified);
    a.store.record_cognition_ack(&ack).unwrap();
    let status = a.store.cognition_peer_status(peer, &options).unwrap();
    assert!(!status.verified_recoverable, "the peer still holds an older snapshot");
    assert!(status.unsent_changes);

    // The peer receives the newer snapshot and verifies it.
    let mut newer: Vec<u8> = Vec::new();
    let newer_receipt = a
        .store
        .write_cognition_archive(&options, Some(peer), &mut newer)
        .unwrap();
    assert!(newer_receipt.complete);
    let outcome = b
        .import_cognition_archive(&newer[..], &CognitionImportOptions::default())
        .unwrap();
    assert!(outcome.inserted > 0, "the offline fact arrived");
    assert!(
        outcome.updated > 0,
        "the continued task must advance, not look like a divergence"
    );
    assert!(
        outcome.conflicts.is_empty(),
        "a continuing source is not a divergence: {:?}",
        outcome.conflicts
    );
    a.store.record_cognition_ack(&outcome.ack(peer)).unwrap();
    let status = a.store.cognition_peer_status(peer, &options).unwrap();
    assert!(status.verified_recoverable, "the peer holds the same snapshot");
    assert!(!status.unsent_changes);
    assert!(!status.unverified_transfer);

    // A replay of the same snapshot duplicates nothing.
    let replay = b
        .import_cognition_archive(&newer[..], &CognitionImportOptions::default())
        .unwrap();
    assert_eq!(replay.inserted, 0, "a replay inserted a duplicate fact");
    assert!(replay.unchanged > 0);
    assert!(replay.conflicts.is_empty());
    assert_eq!(
        b.session_events_range(a.session_id, 0, i64::MAX)
            .unwrap()
            .len(),
        1,
        "a replay duplicated the raw history"
    );
}

#[test]
fn divergence_is_kept_and_the_workspace_is_not_recovery_data() {
    let a = seeded_store();
    let memory = tmp_dir();
    std::fs::write(memory.join("note.md"), "memory body").unwrap();
    let workspace = tmp_dir();
    std::fs::write(workspace.join("scratch.txt"), "workspace body").unwrap();

    // The working directory is not recovery data unless it is asked for.
    let plain = a
        .store
        .export_cognition(&CognitionSnapshotOptions::default())
        .unwrap();
    assert!(
        plain.manifest.files.is_empty(),
        "the workspace must not travel by default"
    );
    let with_memory = a
        .store
        .export_cognition(&CognitionSnapshotOptions {
            memory_root: Some(memory.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(with_memory.manifest.files.len(), 1);
    assert_eq!(with_memory.manifest.files[0].path, "note.md");
    let with_workspace = a
        .store
        .export_cognition(&CognitionSnapshotOptions {
            workspace_roots: vec![workspace.clone()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(with_workspace.manifest.files.len(), 1);
    assert_eq!(
        with_workspace.manifest.files[0].origin,
        shenko_store::cognition_archive::CognitionFileOrigin::Workspace
    );

    let mut archive: Vec<u8> = Vec::new();
    a.store
        .write_cognition_archive(&CognitionSnapshotOptions::default(), None, &mut archive)
        .unwrap();

    let b_dir = tmp_dir();
    let b = Store::open(b_dir.join("b.db"), b_dir.join("cas")).unwrap();
    b.import_cognition_archive(&archive[..], &CognitionImportOptions::default())
        .unwrap();

    // The receiving store continues the same task in its own way.
    let mut local = b.external_task(a.task_id).unwrap();
    local.record_revision = local.record_revision.next();
    local
        .memos
        .insert("local_edit".into(), serde_json::json!("kept here"));
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(local);
    b.commit(batch).unwrap();

    let outcome = b
        .import_cognition_archive(&archive[..], &CognitionImportOptions::default())
        .unwrap();
    assert_eq!(outcome.conflicts.len(), 1, "the divergence must be explicit");
    assert_eq!(outcome.conflicts[0].table, "tasks");
    let kept = b.external_task(a.task_id).unwrap();
    assert!(
        kept.memos.contains_key("local_edit"),
        "the local value must be kept"
    );
    let conflicts = b.cognition_conflicts(10).unwrap();
    assert_eq!(conflicts.len(), 1);

    // The memory history travels with the content, and the repository's own
    // configuration never leaves the node.
    if git_available() {
        let git_memory = tmp_dir();
        assert!(git(&git_memory, &["init", "--quiet", "--initial-branch=main"]).is_some());
        std::fs::write(git_memory.join("note.md"), "memory body").unwrap();
        assert!(git(&git_memory, &["add", "."]).is_some());
        assert!(
            git(
                &git_memory,
                &[
                    "-c",
                    "user.name=Shenko",
                    "-c",
                    "user.email=shenko@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "memory",
                ]
            )
            .is_some()
        );
        let git_options = CognitionSnapshotOptions {
            memory_root: Some(git_memory.clone()),
            ..Default::default()
        };
        let export = a.store.export_cognition(&git_options).unwrap();
        assert!(
            export.manifest.git_history.is_some(),
            "the memory history must travel with the content"
        );
        let mut git_archive: Vec<u8> = Vec::new();
        a.store
            .write_cognition_archive(&git_options, None, &mut git_archive)
            .unwrap();
        let b_memory = tmp_dir();
        let outcome = b
            .import_cognition_archive(
                &git_archive[..],
                &CognitionImportOptions {
                    memory_root: Some(b_memory.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(outcome.restored_git_history);
        assert_eq!(
            std::fs::read_to_string(b_memory.join("note.md")).unwrap(),
            "memory body"
        );
        assert_eq!(
            git(&b_memory, &["rev-parse", "HEAD"]),
            git(&git_memory, &["rev-parse", "HEAD"]),
            "the restored history must reach the same commit"
        );
    }
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}
