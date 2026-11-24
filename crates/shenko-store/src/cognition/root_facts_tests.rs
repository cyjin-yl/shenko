use std::path::PathBuf;

use anyhow::Result;
use rusqlite::params;
use shenko_types::cognition::AttitudeConfidence;
use shenko_types::cognitive_root::{
    CognitiveRootCapture, CognitiveRootContent, CognitiveRootNode, CognitiveRootReinstatement,
    CognitionSelectorRef, EffectDisposition, ExternalEffectDisposition, HomeContentRef,
    RootApplication, RootApplicationScope, RootApplicationState, RootFence, RootRef,
    RuntimeStateGap, RuntimeStateParticipant, SoftwareSlotRef, UnitSelectorEntry, UnitSelectorSnapshot,
    HOME_CONTENT_CODEC, HOME_CONTENT_NAMESPACE, SOFTWARE_SLOT_CODEC, SOFTWARE_SLOT_NAMESPACE,
    UNIT_SELECTOR_CODEC, UNIT_SELECTOR_NAMESPACE,
};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::{BranchId, ControllerEpoch, EntityId, LedgerSeq, RecordRevision, RuntimeGeneration};
use shenko_types::object::Digest;
use shenko_types::store::WriteBatch;
use shenko_types::subject::{GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState};

use crate::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
use crate::cognition::rows::{TABLES, read_table, set_value, spec_for};
use crate::cognition::snapshot::fingerprint_of_manifest;
use crate::cognition_archive::{ARCHIVE_SCHEMA, CognitionRow, ROOT_FACT_CLASSES, validate_manifest_schema};
use crate::memory::MemoryEntry;
use crate::store::Store;

struct Fixture {
    root: PathBuf,
    store: Store,
    agent: EntityId,
    first: CognitiveRootCapture,
    reinstatement: CognitiveRootReinstatement,
    nodes: Vec<CognitiveRootNode>,
    original_operation: EntityId,
    refused_operation: EntityId,
}

fn fence() -> RootFence {
    RootFence { generation: RuntimeGeneration::default(), epoch: ControllerEpoch(1), selector_revision: 0 }
}

fn content(store: &Store, label: &str) -> Result<CognitiveRootContent> {
    let home = store.cas.seal_object(
        &serde_json::to_vec(&serde_json::json!({"home": label}))?, "application/json",
        HOME_CONTENT_NAMESPACE, HOME_CONTENT_CODEC, 1,
    )?.object;
    let software = store.cas.seal_object(
        b"{\"generation\":\"seed\"}", "application/json", SOFTWARE_SLOT_NAMESPACE, SOFTWARE_SLOT_CODEC, 1,
    )?.object;
    let artifact = store.cas.seal_object(b"immutable unit artifact", "text/plain", "test", "raw", 1)?.object;
    let units = UnitSelectorSnapshot::ok(vec![UnitSelectorEntry {
        unit_id: "root-recovery-unit".into(), revision: "1".into(), artifact_digest: artifact.digest, enabled: true,
    }]);
    let units = store.cas.seal_object(
        &units.canonical_bytes()?, "application/json", UNIT_SELECTOR_NAMESPACE, UNIT_SELECTOR_CODEC, 1,
    )?.object;
    Ok(CognitiveRootContent {
        schema_version: CognitiveRootContent::SCHEMA_VERSION,
        home: HomeContentRef { manifest: home.digest },
        software: SoftwareSlotRef { manifest: software.digest, generation: RuntimeGeneration::default(), artifact_digest: None },
        units: CognitionSelectorRef { snapshot: units.digest },
        runtime: RuntimeStateParticipant::NotCaptured(RuntimeStateGap::new("Store fixture has no live guest")),
    })
}

fn capture(store: &Store, label: &str) -> Result<CognitiveRootCapture> {
    Ok(CognitiveRootCapture {
        request_id: EntityId::new(), fence: fence(), content: content(store, label)?,
        reasons: vec![], confidence: AttitudeConfidence::Unknown,
        effects: vec![ExternalEffectDisposition { effect_id: EntityId::new(), disposition: EffectDisposition::Unknown }],
    })
}

fn application(store: &Store, revision: u64, state: RootApplicationState) -> Result<()> {
    store.record_root_application(&RootApplication {
        root_revision: revision, scope: RootApplicationScope::HomeFiles, state,
        updated_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
    })?;
    Ok(())
}

fn fixture() -> Result<Fixture> {
    let root = std::env::temp_dir().join(format!("shenko-root-facts-{}", EntityId::new()));
    std::fs::create_dir(&root)?;
    let source_root = root.join("source");
    std::fs::create_dir(&source_root)?;
    let store = Store::open(source_root.join("store.db"), source_root.join("cas"))?;
    let seed = store.cas.seal_object(b"root recovery seed", "text/plain", "test", "raw", 1)?.object;
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let branch = BranchId::new();
    let first_step = EntityId::new();
    let commit = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let genesis = SubjectGenesis {
        genesis_id, subject_id: agent, bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
        seed_manifest_ref: seed.clone(), initial_profile_ref: seed.clone(), home_manifest_ref: seed.clone(),
        bootstrap_event_ref: seed.clone(), created_commit: commit,
    };
    let head = SubjectBranchHead {
        branch_origin: None, agent_id: agent, branch_id: branch.clone(), subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef { ledger_id: commit.ledger_id, genesis_id }, committed_step_no: 0,
        committed_step_id: None, committed_at: commit, next_step_id: first_step, mode: SubjectMode::Active,
        mode_reason_ref: None, record_revision: RecordRevision(0),
    };
    let step = SubjectStep {
        step_id: first_step, branch_id: branch, step_no: 1, expected_parent_commit: commit,
        controller_epoch: head.controller_epoch, software_generation: head.software_generation.clone(),
        state: SubjectStepState::Planned, input_plan_ref: None, context_revision: None, active_request_id: None,
        active_attempt_id: None, model_result_ref: None, model_outcome_ref: None, decision_ref: None,
        commit_ref: None, retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None,
    };
    let task = shenko_types::task::TaskRecord::subject_root(head.subject_root_task_id, agent, seed, commit);
    store.initialize_genesis(&genesis, &head, &step, &task)?;
    let first = capture(&store, "first")?;
    let one = store.capture_cognitive_root(first.clone())?;
    let two = store.capture_cognitive_root(capture(&store, "second")?)?;
    let reinstatement = CognitiveRootReinstatement {
        request_id: EntityId::new(), fence: fence(), target: RootRef::of(&one), reasons: vec![],
        confidence: AttitudeConfidence::Unknown,
        effects: vec![ExternalEffectDisposition { effect_id: EntityId::new(), disposition: EffectDisposition::Reconciled }],
    };
    let three = store.reinstate_cognitive_root(reinstatement.clone())?;
    for state in [
        RootApplicationState::Deferred { detail: "original guest owns the mount".into() },
        RootApplicationState::Failed { detail: "native file verification failed".into() },
        RootApplicationState::Applied { roots: 1, files: 3 },
        RootApplicationState::Applied { roots: 1, files: 3 },
    ] { application(&store, three.root_revision, state)?; }

    // Genuine X04 ingress may retain foreign operation/attempt associations.
    // The conflicting source event can belong to a different operation from
    // its original, without either operation existing on this node.
    let original_operation = EntityId::new();
    let refused_operation = EntityId::new();
    let original = EvidenceReceipt {
        record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(original_operation),
        attempt_id: Some(EntityId::new()), source_id: "root-recovery-evidence".into(),
        source_instance_id: "foreign-observer".into(), source_event_key: Some("event-1".into()),
        producer_epoch: Some(7), producer_schema_version: Some("1".into()),
        raw_ref: store.cas.seal_object(b"original external observation", "text/plain", "test", "raw", 1)?.object,
        decoder_ref: None, decode_status: DecodeStatus::Uninterpreted, observation_level: None,
        normalized_ref: None, observed_at: None, received_commit: commit,
    };
    let mut batch = WriteBatch::new();
    batch.evidence_upserts.push(original.clone());
    store.commit(batch)?;
    let mut refused = original;
    refused.receipt_id = EntityId::new();
    refused.operation_id = Some(refused_operation);
    refused.attempt_id = Some(EntityId::new());
    refused.raw_ref = store.cas.seal_object(b"divergent external observation", "text/plain", "test", "raw", 1)?.object;
    refused.normalized_ref = Some(store.cas.seal_object(b"refused normalized observation", "text/plain", "test", "raw", 1)?.object);
    let mut batch = WriteBatch::new();
    batch.evidence_upserts.push(refused);
    assert!(matches!(store.commit(batch), Err(shenko_types::error::ShenkoError::Protocol(error)) if error.code == "RECEIPT_KEY_CONFLICT"));
    Ok(Fixture { root, store, agent, first, reinstatement, nodes: vec![one, two, three], original_operation, refused_operation })
}

fn facts(store: &Store) -> Result<Vec<CognitionRow>> {
    let conn = store.ledger.conn.lock();
    let mut rows = Vec::new();
    for spec in TABLES.iter().filter(|spec| ROOT_FACT_CLASSES.contains(&spec.name)
        || matches!(spec.name, "cognition_contents" | "cognition_attitudes" | "cognition_legacy_content")) {
        rows.extend(read_table(&conn, spec)?);
    }
    Ok(rows)
}

fn root_baselines(store: &Store) -> Result<Vec<(String, String, String, String)>> {
    let conn = store.ledger.conn.lock();
    let mut query = conn.prepare("SELECT table_name,entity_key,row_values,updated_at FROM cognition_baselines
        WHERE table_name LIKE 'cognitive_root_%' OR table_name IN ('cognition_contents','cognition_attitudes','cognition_legacy_content')
        ORDER BY table_name,entity_key")?;
    Ok(query.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn archive(store: &Store) -> Result<Vec<u8>> {
    let export = store.export_cognition(&Default::default())?;
    assert!(export.manifest.blocking_gaps().is_empty(), "{:?}", export.manifest.gaps);
    let mut bytes = Vec::new();
    export.write_archive(&mut bytes)?;
    Ok(bytes)
}

#[test]
fn required_root_facts_roundtrip_replay_and_reopen() -> Result<()> {
    let source = fixture()?;
    let expected = facts(&source.store)?;
    for family in ROOT_FACT_CLASSES { assert!(expected.iter().any(|row| row.table == *family)); }
    for row in expected.iter().filter(|row| matches!(row.table.as_str(), "cognitive_root_effects" | "cognitive_root_applications")) {
        assert!(row.value("id").is_none(), "storage autoid leaked into recovery facts");
    }
    let applications = source.store.root_applications(3)?;
    assert_eq!(applications.len(), 4);
    assert_ne!(applications[2].updated_commit, applications[3].updated_commit);
    let conflicts = source.store.external_receipt_conflicts(source.refused_operation)?;
    assert_eq!(conflicts.len(), 1);
    let original = source.store.external_operation_evidence(source.original_operation)?;
    let peer = EntityId::new();
    let options = CognitionImportOptions { peer_id: Some(peer), ..Default::default() };
    let db = source.root.join("receiver.db");
    let cas = source.root.join("receiver-cas");
    let receiver = Store::open(db.clone(), cas.clone())?;
    assert_ne!(receiver.ledger.ledger_id(), source.store.ledger.ledger_id());
    // Different storage autoids must neither collide nor become new identities.
    receiver.ledger.conn.lock().execute("INSERT INTO sqlite_sequence(name,seq) VALUES('cognitive_root_effects',1000)", [])?;
    receiver.ledger.conn.lock().execute("INSERT INTO sqlite_sequence(name,seq) VALUES('cognitive_root_applications',2000)", [])?;
    let bytes = archive(&source.store)?;
    let original_archive = source.root.join("source").join(format!("schema{ARCHIVE_SCHEMA}-populated.cognition"));
    std::fs::write(&original_archive, &bytes)?;
    eprintln!("retained schema-{ARCHIVE_SCHEMA} root facts source={} archive={}",
        source.root.join("source").display(), original_archive.display());
    assert!(receiver.import_cognition_archive(&bytes[..], &options)?.ack(peer).verified);
    assert_eq!(facts(&receiver)?, expected);
    for node in &source.nodes {
        assert_eq!(receiver.cognitive_root_node(node.root_revision)?.as_ref(), Some(node));
        assert_eq!(receiver.cas.read_object(node.content.digest())?, source.store.cas.read_object(node.content.digest())?);
    }
    assert_eq!(receiver.cognitive_root_chain(source.agent, 20)?, source.store.cognitive_root_chain(source.agent, 20)?);
    assert_eq!(receiver.root_applications(3)?, applications);
    let restored = receiver.external_receipt_conflicts(source.refused_operation)?;
    assert_eq!(serde_json::to_value(&restored)?, serde_json::to_value(&conflicts)?);
    assert_eq!(receiver.external_operation_evidence(source.original_operation)?, original);
    assert!(receiver.external_operation_evidence(source.refused_operation)?.is_empty());
    assert_eq!(receiver.cas.read_object(&restored[0].receipt.raw_ref.digest)?, b"divergent external observation");
    assert_eq!(receiver.cas.read_object(&restored[0].receipt.normalized_ref.as_ref().unwrap().digest)?, b"refused normalized observation");
    assert_eq!(receiver.capture_cognitive_root(source.first.clone())?, source.nodes[0]);
    assert_eq!(receiver.reinstate_cognitive_root(source.reinstatement.clone())?, source.nodes[2]);
    let replay = receiver.import_cognition_archive(&bytes[..], &options)?;
    assert_eq!((replay.inserted, replay.updated), (0, 0));
    assert!(replay.ack(peer).verified);
    assert_eq!(facts(&receiver)?, expected);
    drop(receiver);
    let receiver = Store::open(db, cas)?;
    assert_eq!(facts(&receiver)?, expected);
    assert_eq!(receiver.root_applications(3)?, applications);
    application(&receiver, 3, RootApplicationState::Applied { roots: 1, files: 3 })?;
    let relay_expected = facts(&receiver)?;
    let relay_applications = receiver.root_applications(3)?;
    assert_eq!(relay_applications[..4], applications);
    assert_eq!(relay_applications[4].updated_commit.ledger_id, receiver.ledger.ledger_id());
    let relay = archive(&receiver)?;
    let next = Store::open(source.root.join("relay.db"), source.root.join("relay-cas"))?;
    assert!(next.import_cognition_archive(&relay[..], &Default::default())?.ack(peer).verified);
    assert_eq!(facts(&next)?, relay_expected, "relay must retain foreign ledger coordinates and every repeated application");
    assert_eq!(next.root_applications(3)?, relay_applications);
    Ok(())
}

#[test]
fn divergent_root_revision_rejects_complete_chain_and_baselines() -> Result<()> {
    let source = fixture()?;
    let peer = EntityId::new();
    let options = CognitionImportOptions { peer_id: Some(peer), ..Default::default() };
    let db = source.root.join("receiver.db");
    let cas = source.root.join("receiver-cas");
    let receiver = Store::open(db.clone(), cas.clone())?;
    assert!(receiver.import_cognition_archive(&archive(&source.store)?[..], &options)?.ack(peer).verified);
    // A saved baseline establishes ancestry, never permission to replace an
    // immutable root member or assign another UUID to the same root request.
    for case in ["node", "request", "effect", "application"] {
        let kept = facts(&receiver)?;
        let baselines = root_baselines(&receiver)?;
        let mut export = source.store.export_cognition(&Default::default())?;
        let (table, column, value) = match case {
            "node" => ("cognitive_root_nodes", "fence_json", serde_json::to_string(&RootFence {
                epoch: ControllerEpoch(2), ..fence()
            })?),
            "request" => ("cognitive_root_requests", "request_id", EntityId::new().to_string()),
            "effect" => ("cognitive_root_effects", "disposition", "reconciled".into()),
            "application" => ("cognitive_root_applications", "state_json", serde_json::to_string(&RootApplicationState::Applied { roots: 1, files: 999 })?),
            _ => unreachable!(),
        };
        let row = export.manifest.rows.iter_mut().find(|row| row.table == table
            && (case != "effect" || row.value("disposition") == Some("unknown"))).unwrap();
        replace(row, column, value);
        export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
        let mut divergent = Vec::new();
        export.write_archive(&mut divergent)?;
        let outcome = receiver.import_cognition_archive(&divergent[..], &options)?;
        assert!(!outcome.ack(peer).verified, "{case}");
        assert!(outcome.conflicts.iter().any(|conflict| conflict.table == table), "{case}");
        assert_eq!(facts(&receiver)?, kept, "{case}");
        assert_eq!(root_baselines(&receiver)?, baselines, "{case}");
    }
    let local = receiver.capture_cognitive_root(capture(&receiver, "local fourth")?)?;
    application(&receiver, local.root_revision, RootApplicationState::Applied { roots: 1, files: 1 })?;
    let kept = facts(&receiver)?;
    let kept_baselines = root_baselines(&receiver)?;
    let remote = source.store.capture_cognitive_root(capture(&source.store, "remote fourth")?)?;
    assert_eq!(remote.root_revision, local.root_revision);
    let tail = source.store.capture_cognitive_root(capture(&source.store, "remote fifth")?)?;
    application(&source.store, 3, RootApplicationState::Deferred { detail: "new peer-side attempt".into() })?;
    application(&source.store, tail.root_revision, RootApplicationState::Applied { roots: 1, files: 9 })?;
    let independent = MemoryEntry::new(source.agent, "unrelated continuation remains admissible");
    source.store.insert_memory_entry(&independent)?;
    let bytes = archive(&source.store)?;
    let outcome = receiver.import_cognition_archive(&bytes[..], &options)?;
    assert!(!outcome.ack(peer).verified);
    for family in ["cognitive_root_nodes", "cognitive_root_requests", "cognitive_root_effects", "cognitive_root_applications",
        "cognition_contents", "cognition_attitudes"] {
        assert!(outcome.conflicts.iter().any(|conflict| conflict.table == family), "missing {family} conflict");
    }
    assert_eq!(facts(&receiver)?, kept);
    assert_eq!(root_baselines(&receiver)?, kept_baselines);
    assert!(receiver.cognitive_root_node(tail.root_revision)?.is_none());
    assert!(receiver.attitude(tail.attitude_id)?.is_none());
    let tail_request = source.store.export_cognition(&Default::default())?.manifest.rows.into_iter()
        .find(|row| row.table == "cognitive_root_requests" && row.value("root_revision") == Some("5")).unwrap();
    assert!(receiver.cognitive_root_request(tail_request.key.parse()?)?.is_none());
    assert!(receiver.list_memory(100)?.iter().any(|entry| entry.entry_id == independent.entry_id));
    drop(receiver);
    let receiver = Store::open(db, cas)?;
    assert_eq!(facts(&receiver)?, kept);
    assert_eq!(root_baselines(&receiver)?, kept_baselines);
    // A later, valid independent snapshot still advances its own fact unit.
    let later = MemoryEntry::new(source.agent, "later independent observation");
    source.store.insert_memory_entry(&later)?;
    let outcome = receiver.import_cognition_archive(&archive(&source.store)?[..], &options)?;
    assert!(!outcome.conflicts.is_empty());
    assert!(receiver.list_memory(100)?.iter().any(|entry| entry.entry_id == later.entry_id));
    assert_eq!(facts(&receiver)?, kept);
    assert_eq!(root_baselines(&receiver)?, kept_baselines);
    Ok(())
}

fn replace(row: &mut CognitionRow, column: &str, value: String) {
    set_value(row, column, Some(value));
    row.key = spec_for(&row.table).unwrap().key.iter()
        .map(|column| row.value(column).unwrap_or_default()).collect::<Vec<_>>().join("\u{1f}");
}

#[test]
fn malformed_required_root_rows_refuse_before_publication() -> Result<()> {
    let source = fixture()?;
    let memory = source.root.join("source-memory");
    std::fs::create_dir(&memory)?;
    std::fs::write(memory.join("must-not-publish.txt"), b"unpublished remote file")?;
    for case in ["content_record", "digest", "codec", "revision", "fence", "content_parent", "attitude_parent",
        "attitude_binding", "request_parent", "request_kind", "request_commit", "target", "missing_target",
        "effect_disposition", "effect_parent", "application_scope", "application_state", "application_parent", "application_order", "conflict_binding"] {
        let mut export = source.store.export_cognition(&CognitionSnapshotOptions {
            memory_root: Some(memory.clone()), include_git_history: false, ..Default::default()
        })?;
        let rows = &mut export.manifest.rows;
        match case {
            "content_parent" => rows.retain(|row| !(row.table == "cognition_contents" && row.value("digest") == Some(source.nodes[0].content.digest().0.as_str()))),
            "attitude_parent" => rows.retain(|row| !(row.table == "cognition_attitudes" && row.key == source.nodes[0].attitude_id.to_string())),
            "request_parent" => rows.retain(|row| !(row.table == "cognitive_root_requests" && row.key == source.first.request_id.to_string())),
            "missing_target" => rows.retain(|row| !(row.table == "cognitive_root_nodes" && row.key == "1")),
            "application_order" => {
                let indices: Vec<usize> = rows.iter().enumerate().filter(|(_, row)| row.table == "cognitive_root_applications")
                    .map(|(index, _)| index).collect();
                rows.swap(indices[0], indices[1]);
            }
            "target" => {
                let wrong = serde_json::to_string(&RootRef { root_revision: 1, content: Digest::blake3(b"wrong historical target") })?;
                for row in rows.iter_mut().filter(|row| row.value("root_revision") == Some("3")) {
                    if row.table == "cognitive_root_nodes" { replace(row, "reinstated_from_json", wrong.clone()); }
                    if row.table == "cognitive_root_requests" { replace(row, "target_json", wrong.clone()); }
                }
            }
            _ => {
                let (table, column, value) = match case {
                    "content_record" => ("cognitive_root_nodes", "content_record_json", "{}".into()),
                    "digest" => ("cognitive_root_nodes", "content_digest", Digest::blake3(b"wrong node digest").0),
                    "codec" => {
                        let row = rows.iter().find(|row| row.table == "cognitive_root_nodes").unwrap();
                        let mut content: serde_json::Value = serde_json::from_str(row.value("content_record_json").unwrap())?;
                        content["object"]["codec"]["name"] = "json".into();
                        ("cognitive_root_nodes", "content_record_json", serde_json::to_string(&content)?)
                    }
                    "revision" => ("cognitive_root_nodes", "root_revision", "0".into()),
                    "fence" => ("cognitive_root_nodes", "fence_json", "{}".into()),
                    "attitude_binding" => ("cognition_attitudes", "agent_id", EntityId::new().to_string()),
                    "request_kind" => ("cognitive_root_requests", "kind", "reinstate".into()),
                    "request_commit" => ("cognitive_root_requests", "recorded_commit_ledger", EntityId::new().to_string()),
                    "effect_disposition" => ("cognitive_root_effects", "disposition", "retracted".into()),
                    "effect_parent" => ("cognitive_root_effects", "root_revision", "999".into()),
                    "application_scope" => ("cognitive_root_applications", "scope", "unknown_scope".into()),
                    "application_state" => ("cognitive_root_applications", "state_json", "{}".into()),
                    "application_parent" => ("cognitive_root_applications", "root_revision", "999".into()),
                    "conflict_binding" => ("evidence_receipt_conflicts", "original_receipt_id", EntityId::new().to_string()),
                    _ => unreachable!(),
                };
                let row = rows.iter_mut().find(|row| row.table == table
                    && (case != "request_kind" || row.key == source.first.request_id.to_string())).unwrap();
                replace(row, column, value);
            }
        }
        export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
        let mut bytes = Vec::new();
        export.write_archive(&mut bytes)?;
        let receiver = Store::open(source.root.join(format!("bad-{case}.db")), source.root.join(format!("bad-{case}-cas")))?;
        let destination = source.root.join(format!("bad-{case}-memory"));
        std::fs::create_dir(&destination)?;
        let before = receiver.current_commit_seq()?;
        let error = receiver.import_cognition_archive(&bytes[..], &CognitionImportOptions {
            memory_root: Some(destination.clone()), ..Default::default()
        }).unwrap_err();
        assert!(error.to_string().contains("root") || error.to_string().contains("evidence receipt conflict"), "{case}: {error:#}");
        assert_eq!(receiver.current_commit_seq()?, before, "{case}");
        assert!(facts(&receiver)?.is_empty(), "{case}");
        assert!(!destination.join("must-not-publish.txt").exists(), "{case}");
        let conn = receiver.ledger.conn.lock();
        for table in ["cognition_baselines", "cognition_import_order", "subject_genesis", "evidence_receipts"] {
            let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))?;
            assert_eq!(count, 0, "{case}: published {table}");
        }
    }
    source.store.ledger.conn.lock().execute("UPDATE cognitive_root_nodes SET content_record_json='{}' WHERE root_revision=1", [])?;
    let broken = source.store.export_cognition(&Default::default())?;
    assert!(broken.manifest.gaps.iter().any(|gap| gap.blocking && gap.kind == "root_required_fact"));
    Ok(())
}

#[test]
fn required_root_membership_preserves_historical_format_boundaries() -> Result<()> {
    // This is a manifest-gate regression, not old/new producer compatibility
    // evidence. Actual historical readers and archives are verified separately.
    let root = std::env::temp_dir().join(format!("shenko-root-membership-{}", EntityId::new()));
    std::fs::create_dir(&root)?;
    let store = Store::open(root.join("empty.db"), root.join("cas"))?;
    let current = store.export_cognition(&Default::default())?.manifest;
    assert!(current.rows.is_empty());
    assert_eq!(current.schema, ARCHIVE_SCHEMA);
    validate_manifest_schema(&current)?;
    for family in ROOT_FACT_CLASSES {
        let mut missing = current.clone();
        missing.fact_classes.retain(|class| class != family);
        assert!(validate_manifest_schema(&missing).is_err(), "empty required family {family}");
    }
    for schema in (2..=7).chain(10..ARCHIVE_SCHEMA) {
        for dialogue in if schema == 7 { vec![true, false] } else { vec![false] } {
            let mut historical = current.clone();
            historical.schema = schema;
            historical.fact_classes.retain(|class| !ROOT_FACT_CLASSES.contains(&class.as_str())
                && (!matches!(class.as_str(), "dialogue_endpoints" | "dialogue_messages") || dialogue)
                && (!matches!(class.as_str(), "subject_branch_forks" | "subject_head_history") || schema >= 10 || (schema == 7 && !dialogue)));
            if dialogue {
                historical.fact_classes.extend(["dialogue_endpoints".into(), "dialogue_messages".into()]);
            }
            validate_manifest_schema(&historical)?;
            for family in ROOT_FACT_CLASSES {
                let mut claimed = historical.clone();
                claimed.fact_classes.push((*family).into());
                assert!(validate_manifest_schema(&claimed).is_err(), "schema {schema} claimed {family}");
                let mut undeclared = historical.clone();
                undeclared.rows.push(CognitionRow { table: (*family).into(), key: "undeclared".into(), values: vec![] });
                assert!(validate_manifest_schema(&undeclared).is_err(), "schema {schema} carried {family}");
            }
        }
    }
    let mut combined_seven = current.clone();
    combined_seven.schema = 7;
    combined_seven.fact_classes.retain(|class| !ROOT_FACT_CLASSES.contains(&class.as_str()));
    combined_seven.fact_classes.extend(["dialogue_endpoints".into(), "dialogue_messages".into()]);
    assert!(validate_manifest_schema(&combined_seven).is_err());
    Ok(())
}

#[test]
fn foreign_refused_original_does_not_require_a_local_original_receipt() -> Result<()> {
    let source = fixture()?;
    let expected = source.store.external_receipt_conflicts(source.refused_operation)?;
    let mut export = source.store.export_cognition(&Default::default())?;
    export.manifest.rows.retain(|row| row.table != "evidence_receipts");
    export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
    let mut bytes = Vec::new();
    export.write_archive(&mut bytes)?;
    let receiver = Store::open(source.root.join("foreign.db"), source.root.join("foreign-cas"))?;
    assert!(receiver.import_cognition_archive(&bytes[..], &Default::default())?.ack(EntityId::new()).verified);
    assert!(receiver.external_operation_evidence(source.original_operation)?.is_empty());
    let actual = receiver.external_receipt_conflicts(source.refused_operation)?;
    assert_eq!(serde_json::to_value(actual)?, serde_json::to_value(expected)?);
    let row = facts(&receiver)?.into_iter().find(|row| row.table == "evidence_receipt_conflicts").unwrap();
    let count: i64 = receiver.ledger.conn.lock().query_row("SELECT COUNT(*) FROM evidence_receipts WHERE receipt_id=?1",
        params![row.value("original_receipt_id")], |row| row.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}

#[test]
#[ignore = "requires SHENKO_ROOT_OLD9_ARCHIVE from the real schema-9 producer"]
fn genuine_schema_nine_refuses_without_publishing() -> Result<()> {
    let original = std::env::var_os("SHENKO_ROOT_OLD9_ARCHIVE")
        .ok_or_else(|| anyhow::anyhow!("set SHENKO_ROOT_OLD9_ARCHIVE to the retained schema-9 producer output"))?;
    let archive = std::fs::read(original)?;
    let root = std::env::temp_dir().join(format!("shenko-root-old9-{}", EntityId::new()));
    std::fs::create_dir(&root)?;
    let db = root.join("receiver.db");
    let cas = root.join("cas");
    let store = Store::open(db.clone(), cas.clone())?;
    let marks = |store: &Store| -> Result<Vec<i64>> {
        let conn = store.ledger.conn.lock();
        ["cognition_baselines", "cognition_receipts", "cognition_import_order", "cognition_origins"].into_iter()
            .map(|table| Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))?))
            .collect()
    };
    let before = (store.current_commit_seq()?, facts(&store)?, marks(&store)?);
    let refusal = store.import_cognition_archive(&archive[..], &Default::default()).unwrap_err();
    assert!(refusal.to_string().contains("unsupported cognition archive schema 9"), "{refusal}");
    assert_eq!((store.current_commit_seq()?, facts(&store)?, marks(&store)?), before);
    drop(store);
    let reopened = Store::open(db, cas)?;
    assert_eq!((reopened.current_commit_seq()?, facts(&reopened)?, marks(&reopened)?), before);
    eprintln!("retained genuine schema-9 refusal receiver={}", root.display());
    Ok(())
}
