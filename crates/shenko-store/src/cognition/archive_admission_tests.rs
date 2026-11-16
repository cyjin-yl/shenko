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
