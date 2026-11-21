use std::fs;

use anyhow::{Context, Result};
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::subject::{SubjectBranchHead, SubjectMode, SubjectStepState};

use crate::cognition_archive::CognitionRow;
use crate::store::Store;

use super::admission::MEMBERSHIP_REQUIRED_SCHEMA;
use super::closure::raw_codec;
use super::snapshot::fingerprint_of_manifest;
use super::{CognitionImportOptions, CognitionSnapshotOptions};
use crate::memory::MemoryEntry;

#[test]
fn generic_json_recovery_preserves_verified_bytes_and_archive_facts() -> Result<()> {
    use shenko_types::store::{RawSession, RawSessionEvent, WriteBatch};

    let root = std::env::temp_dir().join(format!("shenko-generic-json-{}", EntityId::new()));
    fs::create_dir(&root)?;
    let source = Store::open(root.join("source.db"), root.join("source-cas"))?;
    let cases: &[(&[u8], &str, &str)] = &[
        (b"seed", "application/json", "raw"),
        (b"{\"answer\":", "application/problem+json", "json"),
    ];
    let mut roots = Vec::new();
    for &(bytes, media_type, codec) in cases {
        roots.push(source.cas.seal_object(bytes, media_type, "test", codec, 1)?.object);
    }
    let session = EntityId::new();
    let data = serde_json::to_string(&serde_json::json!({"objects": &roots}))?;
    let mut batch = WriteBatch::new();
    batch.raw_session_upserts.push(RawSession {
        session_id: session, agent_id: EntityId::new(),
        started_at: "2026-11-21T00:00:00Z".into(), data: "{}".into(),
    });
    batch.session_event_upserts.push(RawSessionEvent {
        event_id: EntityId::new(), session_id: session, seq: 1,
        event_type: "message".into(), role: "user".into(), data: data.clone(),
    });
    source.commit(batch)?;

    // The shared collector must retain the actual published bytes as GC roots.
    let closure = crate::roots::object_closure(&source.cas, roots.clone());
    assert!(closure.missing.is_empty(), "{:?}", closure.missing);
    assert_eq!(closure.digests, roots.iter().map(|object| object.digest.clone()).collect::<std::collections::BTreeSet<_>>());
    let bytes: u64 = roots.iter().map(|object| object.bytes).sum();
    let (bounded, missing) = source.collect_closure_bounded(&roots, roots.len(), bytes)?;
    assert!(missing.is_empty());
    assert_eq!(bounded.len(), roots.len());

    let export = source.export_cognition(&Default::default())?;
    assert!(export.manifest.blocking_gaps().is_empty(), "{:?}", export.manifest.gaps);
    for object in &roots {
        let declared = export.manifest.objects.iter()
            .find(|declared| declared.digest == object.digest).context("generic object omitted")?;
        assert_eq!(declared.bytes, object.bytes);
        assert_eq!(declared.media_type, object.media_type);
        assert_eq!(declared.codec, object.codec);
    }
    let mut archive = Vec::new();
    assert!(export.write_archive(&mut archive)?.complete);
    drop(export);
    drop(source);
    let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas"))?;
    let imported = receiver.import_cognition_archive(&archive[..], &Default::default())?;
    assert!(imported.ack(EntityId::new()).verified);
    drop(receiver);
    let reopened = Store::open(root.join("receiver.db"), root.join("receiver-cas"))?;
    for (object, &(bytes, _, _)) in roots.iter().zip(cases) {
        assert_eq!(reopened.cas.read_object(&object.digest)?.as_slice(), bytes);
    }
    let events = reopened.session_events_range(session, 0, i64::MAX)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, data);
    assert!(reopened.export_cognition(&Default::default())?.manifest.blocking_gaps().is_empty());
    Ok(())
}

#[test]
fn generic_json_recovery_keeps_integrity_and_declared_schemas_required() -> Result<()> {
    use shenko_types::cognitive_root::{
        COGNITIVE_ROOT_CODEC, COGNITIVE_ROOT_CODEC_MAJOR,
        UNIT_SELECTOR_CODEC, UNIT_SELECTOR_CODEC_MAJOR,
    };

    let root = std::env::temp_dir().join(format!("shenko-generic-json-required-{}", EntityId::new()));
    fs::create_dir(&root)?;
    let source = Store::open(root.join("source.db"), root.join("source-cas"))?;
    let generic = source.cas.seal_object(b"seed", "application/json", "test", "raw", 1)?.object;
    for (codec, major) in [
        (COGNITIVE_ROOT_CODEC, COGNITIVE_ROOT_CODEC_MAJOR),
        (UNIT_SELECTOR_CODEC, UNIT_SELECTOR_CODEC_MAJOR),
        ("shenko-wasm-state", 1),
    ] {
        let mut declared = generic.clone();
        declared.codec.name = codec.into();
        declared.codec.major = major;
        declared.codec.digest = Digest::blake3(codec.as_bytes());
        // Neither generic nor opaque aliases may satisfy a later schema.
        let mut opaque = generic.clone();
        opaque.media_type = "application/octet-stream".into();
        let (_, missing) = source.collect_closure(&[opaque, generic.clone(), declared])?;
        assert_eq!(missing, vec![generic.digest.clone()], "{codec}");
    }

    let absent = Digest::blake3(b"required artifact not sealed here");
    let runtime = serde_json::to_vec(&serde_json::json!({
        "format": "shenko-wasm-state/2", "module": &absent, "module_bytes": 4,
    }))?;
    let runtime = source.cas.seal_object(&runtime, "application/json",
        "seed-wasm-state", "shenko-wasm-state", 1)?.object;
    assert_eq!(source.collect_closure(&[runtime])?.1, vec![absent]);

    let mut wrong_length = generic.clone();
    wrong_length.bytes += 1;
    assert_eq!(source.collect_closure(&[wrong_length])?.1, vec![generic.digest.clone()]);
    fs::write(source.cas.verified_object_path(&generic)?, b"reed")?;
    assert_eq!(source.collect_closure(std::slice::from_ref(&generic))?.1, vec![generic.digest]);
    Ok(())
}

#[test]
fn captured_json_stream_does_not_extend_the_recovery_graph() -> Result<()> {
    let root = std::env::temp_dir().join(format!("shenko-opaque-stream-{}", EntityId::new()));
    fs::create_dir(&root)?;
    let source = Store::open(root.join("source.db"), root.join("source-cas"))?;
    let missing = ObjectRef {
        digest: Digest::blake3(b"guest claims an object that does not exist"),
        bytes: 48,
        media_type: "application/json".into(),
        store_namespace: "guest-claim".into(),
        locator: String::new(),
        codec: raw_codec(),
    };
    let stream_bytes = serde_json::to_vec(&missing)?;
    let stream = source.cas.seal_object(
        &stream_bytes, "application/octet-stream", "wasi-exec-streams", "wasi-stream", 1,
    )?.object;
    let (objects, missing) = source.collect_closure(&[stream.clone()])?;
    assert!(missing.is_empty());
    assert_eq!(objects.len(), 1);
    let mut export = source.export_cognition(&Default::default())?;
    export.manifest.objects = objects;
    export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
    let mut archive = Vec::new();
    export.write_archive(&mut archive)?;
    let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas"))?;
    let imported = receiver.import_cognition_archive(&archive[..], &Default::default())?;
    assert!(imported.ack(EntityId::new()).verified);
    assert_eq!(receiver.cas.read_object(&stream.digest)?, stream_bytes);
    Ok(())
}

#[test]
fn later_typed_alias_expands_its_real_child_on_both_sides() -> Result<()> {
    let root = std::env::temp_dir().join(format!("shenko-typed-alias-{}", EntityId::new()));
    fs::create_dir(&root)?;
    let source = Store::open(root.join("source.db"), root.join("source-cas"))?;
    let child = source.cas.seal_object(b"retained child", "text/plain", "test", "raw", 1)?.object;
    let json = serde_json::to_vec(&child)?;
    let opaque = source.cas.seal_object(&json, "application/octet-stream", "test", "raw", 1)?.object;
    let typed = source.cas.seal_object(&json, "application/json", "test", "json", 1)?.object;
    let parent = source.cas.seal_object(
        &serde_json::to_vec(&typed)?, "application/json", "test", "json", 1,
    )?.object;
    // J is collected opaque before the parent's typed reference to J is
    // expanded. Its child C must still enter the actual export closure.
    let (mut objects, missing) = source.collect_closure(&[opaque.clone(), parent])?;
    assert!(missing.is_empty());
    assert_eq!(objects.len(), 3);
    assert!(objects.iter().any(|object| object.digest == child.digest));
    let alias = objects.iter_mut().find(|object| object.digest == opaque.digest).unwrap();
    assert_eq!(alias.media_type, "application/json");
    // A valid archive may describe J with its opaque alias while the
    // retained parent still references its JSON type. Import must follow
    // that later typed reference, independently of descriptor order.
    alias.media_type = "application/octet-stream".into();
    let mut export = source.export_cognition(&Default::default())?;
    export.manifest.objects = objects;
    export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
    let mut archive = Vec::new();
    export.write_archive(&mut archive)?;
    let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas"))?;
    let imported = receiver.import_cognition_archive(&archive[..], &Default::default())?;
    assert!(imported.ack(EntityId::new()).verified);
    assert_eq!(receiver.cas.read_object(&child.digest)?.as_slice(), b"retained child");
    Ok(())
}

#[test]
fn reordered_labels_cannot_publish_a_paused_head_or_files() {
    for schema in [2, crate::cognition_archive::ARCHIVE_SCHEMA] {
        reject_reordered_head(schema);
    }
}

fn reject_reordered_head(schema: u32) {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::{BranchId, ControllerEpoch, RecordRevision, RuntimeGeneration};
    use shenko_types::subject::{GenesisRef, SubjectGenesis, SubjectStep};

    let root = std::env::temp_dir().join(format!("shenko-row-order-{}", EntityId::new()));
    fs::create_dir(&root).unwrap();
    let source = Store::open(root.join("source.db"), root.join("source-cas")).unwrap();
    let object = source.cas.seal_object(b"seed", "text/plain", "test", "raw", 1).unwrap().object;
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let branch = BranchId::new();
    let first = EntityId::new();
    let commit = CommitRef::new(source.ledger.ledger_id(), LedgerSeq::ZERO);
    let genesis = SubjectGenesis {
        genesis_id, subject_id: agent, bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(), initial_profile_ref: object.clone(),
        home_manifest_ref: object.clone(), bootstrap_event_ref: object.clone(), created_commit: commit,
    };
    let head = SubjectBranchHead { branch_origin: None,
        agent_id: agent, branch_id: branch.clone(), subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef { ledger_id: commit.ledger_id, genesis_id },
        committed_step_no: 0, committed_step_id: None, committed_at: commit,
        next_step_id: first, mode: SubjectMode::Active, mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let step = SubjectStep {
        step_id: first, branch_id: branch, step_no: 1, expected_parent_commit: commit,
        controller_epoch: head.controller_epoch, software_generation: head.software_generation.clone(),
        state: SubjectStepState::Planned, input_plan_ref: None, context_revision: None,
        active_request_id: None, active_attempt_id: None, model_result_ref: None,
        model_outcome_ref: None, decision_ref: None, commit_ref: None,
        retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None,
    };
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent,
        object,
        commit,
    );
    source
        .initialize_genesis(&genesis, &head, &step, &root_task)
        .unwrap();
    let memory = root.join("source-memory");
    fs::create_dir(&memory).unwrap();
    fs::write(memory.join("remote.txt"), b"must not publish").unwrap();
    let mut export = source.export_cognition(&CognitionSnapshotOptions {
        memory_root: Some(memory), include_git_history: false, ..Default::default()
    }).unwrap();
    // Current schemas reject the noncanonical layout; legacy migration
    // must still reject the contradictory named columns and head data.
    export.manifest.schema = schema;
    if schema < 7 {
        // This negative layout fixture predates the fork/history family. Keep
        // every row and the original corruption; omit only empty later classes.
        assert!(!export.manifest.rows.iter().any(|row|
            matches!(row.table.as_str(), "subject_branch_forks" | "subject_head_history")));
        export.manifest.fact_classes.retain(|class|
            !matches!(class.as_str(), "subject_branch_forks" | "subject_head_history"));
    }
    let row = export.manifest.rows.iter_mut().find(|row| row.table == "subject_branch_heads").unwrap();
    let active = row.value("data").unwrap().to_string();
    let mut paused: serde_json::Value = serde_json::from_str(&active).unwrap();
    paused["mode"] = "paused".into();
    row.values[3] = ("mode".into(), Some("active".into()));
    row.values[5] = ("committed_step_id".into(), Some("paused".into()));
    row.values[7] = ("created_at".into(), Some(serde_json::to_string(&paused).unwrap()));
    row.values[8] = ("data".into(), Some(active));
    assert_eq!(row.value("mode"), Some("active"));
    assert_eq!(serde_json::from_str::<serde_json::Value>(row.value("data").unwrap()).unwrap()["mode"], "active");
    assert_eq!(row.values[5].1.as_deref(), Some("paused"));
    assert_eq!(serde_json::from_str::<serde_json::Value>(row.values[7].1.as_deref().unwrap()).unwrap()["mode"], "paused");
    export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
    let mut archive = Vec::new();
    export.write_archive(&mut archive).unwrap();

    let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas")).unwrap();
    let receiver_memory = root.join("receiver-memory");
    fs::create_dir(&receiver_memory).unwrap();
    let before = receiver.current_commit_seq().unwrap();
    let error = receiver.import_cognition_archive(&archive[..], &CognitionImportOptions {
        memory_root: Some(receiver_memory.clone()), ..Default::default()
    }).unwrap_err();
    assert!(error.to_string().contains(if schema == crate::cognition_archive::ARCHIVE_SCHEMA {
        "invalid cognition column layout"
    } else {
        "imported subject head column/value mismatch"
    }));
    assert_eq!(receiver.current_commit_seq().unwrap(), before);
    assert!(receiver.get_existing_agent_id().unwrap().is_none());
    assert!(!receiver_memory.join("remote.txt").exists());
}

#[test]
fn unknown_fact_does_not_produce_a_verified_partial_import() -> Result<()> {
    let root = std::env::temp_dir().join(format!("shenko-cognition-{}", EntityId::new()));
    fs::create_dir(&root)?;
    let source = Store::open(root.join("source.db"), root.join("source-cas"))?;
    source.insert_memory_entry(&MemoryEntry::new(EntityId::new(), "known fact"))?;
    let mut export = source.export_cognition(&Default::default())?;
    export.manifest.rows.push(CognitionRow {
        table: "future_required_fact".into(),
        key: "1".into(),
        values: vec![("id".into(), Some("1".into()))],
    });
    export.manifest.fingerprint = fingerprint_of_manifest(&export.manifest);
    let mut archive = Vec::new();
    export.write_archive(&mut archive)?;
    let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas"))?;
    assert!(receiver.import_cognition_archive(&archive[..], &Default::default()).is_err());
    assert!(receiver.list_memory(10)?.is_empty());
    let before = receiver.current_commit_seq()?;
    // Both the introduction format and the current writer format must
    // reject an intentionally malformed declaration before publishing.
    for schema in [MEMBERSHIP_REQUIRED_SCHEMA, crate::cognition_archive::ARCHIVE_SCHEMA] {
        let mut missing = source.export_cognition(&Default::default())?;
        missing.manifest.schema = schema;
        missing.manifest.fact_classes.clear();
        missing.manifest.fingerprint = fingerprint_of_manifest(&missing.manifest);
        let mut archive = Vec::new();
        missing.write_archive(&mut archive)?;
        let error = receiver.import_cognition_archive(&archive[..], &Default::default())
            .err().context("archive without required membership was admitted")?;
        assert!(error.to_string().contains("must declare its fact classes"));
        assert_eq!(receiver.current_commit_seq()?, before);
        assert!(receiver.list_memory(10)?.is_empty());
    }
    Ok(())
}

/// Cognitive atoms are REQUIRED archive facts. They travel
/// with the content they name, a receiver restores the exact attitude
/// identity/commit/confidence, an archive that names an attitude without
/// its content version is refused whole, and a reader that declares a
/// lower schema refuses the archive at the version gate instead of
/// importing an incomplete closure.
#[test]
fn cognition_atoms_travel_and_are_required_archive_facts() {
    use crate::atoms::{AttitudeDraft, ContentWrite};
    use shenko_types::cognition::{
        AdoptionTarget, AttitudeConfidence, ReasonPointer,
    };

    let root = std::env::temp_dir().join(format!("shenko-atoms-archive-{}", EntityId::new()));
    fs::create_dir(&root).unwrap();
    let source = Store::open(root.join("source.db"), root.join("source-cas")).unwrap();
    let agent = EntityId::new();
    let base = source
        .record_content_attitude(
            agent,
            ContentWrite::text("第一版内容"),
            AttitudeDraft::assertion(vec![], AttitudeConfidence::unknown()),
        )
        .unwrap();
    let revised = source
        .record_content_attitude(
            agent,
            ContentWrite::text("第二版内容"),
            AttitudeDraft::revision(
                base.content.digest().clone(),
                vec![ReasonPointer::content(base.content.digest().clone())],
                AttitudeConfidence::unknown(),
            )
            .unwrap(),
        )
        .unwrap();
    let adopted = source
        .record_attitude_for_digest(
            agent,
            revised.content.object.clone(),
            AttitudeDraft::adoption(
                AdoptionTarget::unit("humanlike-mind"),
                vec![],
                AttitudeConfidence::unknown(),
            ),
        )
        .unwrap();

    let mut export = source.export_cognition(&Default::default()).unwrap();
    assert!(export
        .manifest
        .fact_classes
        .iter()
        .any(|class| class == "cognition_contents"));
    assert!(export
        .manifest
        .fact_classes
        .iter()
        .any(|class| class == "cognition_attitudes"));
    assert!(export.manifest.rows.iter().any(|row| {
        row.table == "cognition_attitudes"
            && row.key == revised.attitude.attitude_id.to_string()
    }));
    assert!(export
        .manifest
        .objects
        .iter()
        .any(|object| object.digest == *revised.content.digest()));
    let mut archive = Vec::new();
    export.write_archive(&mut archive).unwrap();

    let receiver = Store::open(root.join("receiver.db"), root.join("receiver-cas")).unwrap();
    let imported = receiver
        .import_cognition_archive(&archive[..], &Default::default())
        .unwrap();
    assert!(imported.ack(EntityId::new()).verified);
    let restored = receiver
        .attitude(revised.attitude.attitude_id)
        .unwrap()
        .unwrap();
    assert_eq!(restored.created_commit, revised.attitude.created_commit);
    assert_eq!(restored.confidence, Some(AttitudeConfidence::Unknown));
    assert_eq!(
        receiver.content_bytes(revised.content.digest()).unwrap(),
        "第二版内容".as_bytes().to_vec()
    );
    assert_eq!(
        receiver
            .revision_chain(agent, revised.content.digest())
            .unwrap()
            .len(),
        1
    );
    // The peer's adoption is preserved as that peer's record; it is not a
    // local adoption and it never activates anything here.
    assert!(receiver
        .attitudes_for(agent, revised.content.digest())
        .unwrap()
        .iter()
        .any(|attitude| attitude.attitude_id == adopted.attitude.attitude_id));
    assert!(receiver
        .adoption_for(agent, "humanlike-mind", revised.content.digest())
        .unwrap()
        .is_none());

    // An attitude without the content version it names is incomplete.
    let mut broken = source.export_cognition(&Default::default()).unwrap();
    broken
        .manifest
        .rows
        .retain(|row| row.table != "cognition_contents");
    broken.manifest.fingerprint = fingerprint_of_manifest(&broken.manifest);
    let mut broken_archive = Vec::new();
    broken.write_archive(&mut broken_archive).unwrap();
    let second = Store::open(root.join("second.db"), root.join("second-cas")).unwrap();
    assert!(second
        .import_cognition_archive(&broken_archive[..], &Default::default())
        .is_err());
    assert!(second
        .attitude(revised.attitude.attitude_id)
        .unwrap()
        .is_none());

    // A reader that declares a lower schema refuses this closure whole.
    let mut future = source.export_cognition(&Default::default()).unwrap();
    future.manifest.schema = crate::cognition_archive::ARCHIVE_SCHEMA + 1;
    future.manifest.fingerprint = fingerprint_of_manifest(&future.manifest);
    let mut future_archive = Vec::new();
    future.write_archive(&mut future_archive).unwrap();
    assert!(second
        .import_cognition_archive(&future_archive[..], &Default::default())
        .is_err());
}

#[test]
fn native_seven_format_refusal_precedes_payload_and_store_publication() -> Result<()> {
    use shenko_types::store::{RawSession, RawSessionEvent, WriteBatch};

    fn import_marks(store: &Store) -> Result<Vec<i64>> {
        let conn = store.ledger.conn.lock();
        ["cognition_baselines", "cognition_file_baselines", "cognition_receipts",
            "cognition_import_order", "cognition_origins"].into_iter().map(|table| {
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                    .map_err(Into::into)
            }).collect()
    }

    let root = std::env::temp_dir().join(format!("shenko-native-seven-format-{}", EntityId::new()));
    fs::create_dir(&root)?;
    let source = Store::open(root.join("source.db"), root.join("source-cas"))?;
    let bytes = b"original payload must not be published after format refusal";
    let object = source.cas.seal_object(bytes, "text/plain", "test", "raw", 1)?.object;
    let session = EntityId::new();
    let data = serde_json::to_string(&serde_json::json!({"object": &object}))?;
    let mut batch = WriteBatch::new();
    batch.raw_session_upserts.push(RawSession {
        session_id: session, agent_id: EntityId::new(),
        started_at: "2026-11-21T00:00:00Z".into(), data: "{}".into(),
    });
    batch.session_event_upserts.push(RawSessionEvent {
        event_id: EntityId::new(), session_id: session, seq: 1,
        event_type: "message".into(), role: "user".into(), data: data.clone(),
    });
    source.commit(batch)?;
    let memory = root.join("source-memory");
    fs::create_dir(&memory)?;
    fs::write(memory.join("remote.txt"), b"original remote file")?;
    let options = CognitionSnapshotOptions {
        memory_root: Some(memory), include_git_history: false, ..Default::default()
    };
    let native = source.export_cognition(&options)?;
    assert_eq!(native.manifest.schema, 7, "this producer is the authentic fork/history native7");
    for class in ["subject_branch_forks", "subject_head_history"] {
        assert!(native.manifest.fact_classes.iter().any(|declared| declared == class));
    }
    assert!(!native.manifest.fact_classes.iter().any(|class|
        matches!(class.as_str(), "dialogue_endpoints" | "dialogue_messages")));
    assert!(native.manifest.objects.iter().any(|declared| declared.digest == object.digest));
    let mut native_archive = Vec::new();
    assert!(native.write_archive(&mut native_archive)?.complete);

    for case in ["partial-family", "absent-family", "mixed-native-seven", "older-format"] {
        // Only the negative inputs are mutated. The positive archive above is
        // the original Store producer output, with its original version intact.
        let mut malformed = source.export_cognition(&options)?;
        let expected = if case == "older-format" {
            malformed.manifest.schema = 6;
            "historical archive cannot claim later dialogue or fork/history membership"
        } else {
            match case {
                "partial-family" => malformed.manifest.fact_classes.retain(|class|
                    class != "subject_head_history"),
                "absent-family" => malformed.manifest.fact_classes.retain(|class|
                    !matches!(class.as_str(), "subject_branch_forks" | "subject_head_history")),
                "mixed-native-seven" => malformed.manifest.fact_classes.extend([
                    "dialogue_endpoints".into(), "dialogue_messages".into(),
                ]),
                _ => unreachable!(),
            }
            "schema-7 archive must identify exactly one native producer"
        };
        malformed.manifest.fingerprint = fingerprint_of_manifest(&malformed.manifest);
        let mut archive = Vec::new();
        malformed.write_archive(&mut archive)?;

        // The control path owns no file handover and must use the same gate.
        let mut control = source.export_cognition(&Default::default())?.manifest;
        control.schema = malformed.manifest.schema;
        control.fact_classes = malformed.manifest.fact_classes.clone();
        control.fingerprint = fingerprint_of_manifest(&control);
        let error = source.validate_control_manifest(&control).unwrap_err();
        assert!(error.to_string().contains(expected), "{case}: {error}");

        let receiver_dir = root.join(case);
        fs::create_dir(&receiver_dir)?;
        let receiver_memory = receiver_dir.join("memory");
        fs::create_dir(&receiver_memory)?;
        fs::write(receiver_memory.join("local.txt"), b"receiver-owned original")?;
        let receiver = Store::open(receiver_dir.join("receiver.db"), receiver_dir.join("cas"))?;
        let seq_before = receiver.current_commit_seq()?;
        let marks_before = import_marks(&receiver)?;
        assert!(receiver.cas.object_size(&object.digest).is_none());
        let error = receiver.import_cognition_archive(&archive[..], &CognitionImportOptions {
            memory_root: Some(receiver_memory.clone()), ..Default::default()
        }).unwrap_err();
        assert!(error.to_string().contains(expected), "{case}: {error}");
        assert_eq!(receiver.current_commit_seq()?, seq_before);
        assert_eq!(import_marks(&receiver)?, marks_before);
        assert!(receiver.session_events_range(session, 0, i64::MAX)?.is_empty());
        assert!(receiver.cas.object_size(&object.digest).is_none(),
            "{case}: the format gate must run before the CAS payload callback");
        assert_eq!(fs::read(receiver_memory.join("local.txt"))?, b"receiver-owned original");
        assert_eq!(fs::read_dir(&receiver_memory)?.map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?, vec![std::ffi::OsString::from("local.txt")]);
        drop(receiver);
        let reopened = Store::open(receiver_dir.join("receiver.db"), receiver_dir.join("cas"))?;
        assert_eq!(reopened.current_commit_seq()?, seq_before);
        assert_eq!(import_marks(&reopened)?, marks_before);
        assert!(reopened.session_events_range(session, 0, i64::MAX)?.is_empty());
        assert!(reopened.cas.object_size(&object.digest).is_none());
        assert_eq!(fs::read(receiver_memory.join("local.txt"))?, b"receiver-owned original");
        assert_eq!(fs::read_dir(&receiver_memory)?.map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?, vec![std::ffi::OsString::from("local.txt")]);
    }

    // The unmodified native7 producer still imports, reopens and re-exports
    // through the real Store owner with the original bytes and event intact.
    let receiver_dir = root.join("native-positive");
    fs::create_dir(&receiver_dir)?;
    let receiver_memory = receiver_dir.join("memory");
    fs::create_dir(&receiver_memory)?;
    let receiver = Store::open(receiver_dir.join("receiver.db"), receiver_dir.join("cas"))?;
    let imported = receiver.import_cognition_archive(&native_archive[..], &CognitionImportOptions {
        memory_root: Some(receiver_memory.clone()), ..Default::default()
    })?;
    assert!(imported.ack(EntityId::new()).verified);
    assert_eq!(receiver.cas.read_object(&object.digest)?.as_slice(), bytes);
    assert_eq!(fs::read(receiver_memory.join("remote.txt"))?, b"original remote file");
    let events = receiver.session_events_range(session, 0, i64::MAX)?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, data);
    drop(receiver);
    let reopened = Store::open(receiver_dir.join("receiver.db"), receiver_dir.join("cas"))?;
    assert_eq!(reopened.cas.read_object(&object.digest)?.as_slice(), bytes);
    assert_eq!(reopened.session_events_range(session, 0, i64::MAX)?[0].data, data);
    let relay = reopened.export_cognition(&CognitionSnapshotOptions {
        memory_root: Some(receiver_memory), include_git_history: false, ..Default::default()
    })?;
    assert_eq!(relay.manifest.schema, 7);
    assert!(relay.manifest.blocking_gaps().is_empty());
    assert!(relay.manifest.objects.iter().any(|declared| declared.digest == object.digest));
    Ok(())
}
