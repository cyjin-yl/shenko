use shenko_store::{
    BankEntryKind, BankSelection, CarriedCandidate, InterventionBankEntry, ResearchBudget,
    ResearchCandidate, ResearchClosure, ResearchEnvironment, ResearchEpoch, ResearchResetOutcome,
    ResearchResetTrigger, ResearchWorkerBrief, Store,
};
use shenko_types::ids::EntityId;

fn fixture() -> (tempfile::TempDir, Store, ResearchClosure, ResearchEpoch) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("research.db"), root.path().join("cas")).unwrap();
    let closure = ResearchClosure {
        constraints: vec![],
        budget: ResearchBudget {
            max_tokens: Some(1000), max_duration_secs: None, max_cost_units: None, notes: None,
        },
        effects: vec![],
        environment: ResearchEnvironment {
            environment_id: "byte-count-fixture".into(), facts: vec!["synthetic artifact".into()],
            toolchain: vec![], dataset_versions: vec![], reproducible: true,
        },
    };
    let epoch = store.open_research_lineage(EntityId::new(), EntityId::new(), None, &closure).unwrap();
    (root, store, closure, epoch)
}

fn bank_entry(epoch: EntityId, kind: BankEntryKind, content: &str) -> InterventionBankEntry {
    InterventionBankEntry {
        entry_id: EntityId::new(), epoch_id: epoch, kind, content: content.into(),
        source_refs: vec!["fixture:observed-fact".into()], scope: "research".into(),
        confidence: Some(0.75), base_revision: 4, supersedes: None, superseded_by: None,
        carried_from: None, tombstoned_at_ms: None, created_at_ms: 1,
    }
}

fn candidate(store: &Store, epoch: &ResearchEpoch, bytes: &[u8]) -> ResearchCandidate {
    let artifact = store.cas.seal_object(bytes, "text/plain", "test", "raw", 1).unwrap().object;
    let value = ResearchCandidate {
        candidate_id: EntityId::new(), epoch_id: epoch.epoch_id,
        score: Some(store.cas.read_object(&artifact.digest).unwrap().len() as f64),
        score_source: Some("artifact byte count; not a quality estimate".into()),
        artifact_ref: artifact, environment_ref: epoch.environment_ref.clone(),
        notes: None, frozen: false, created_at_ms: 1,
    };
    store.add_research_candidate(&value).unwrap();
    value
}

fn assert_candidates(actual: &[CarriedCandidate], expected: &[&ResearchCandidate]) {
    assert_eq!(actual.len(), expected.len());
    for original in expected {
        let carried = actual.iter().find(|item| item.candidate_id == original.candidate_id).unwrap();
        assert_eq!(carried.artifact_ref, original.artifact_ref);
        assert_eq!(carried.score, original.score);
        assert_eq!(carried.score_source, original.score_source);
        assert_eq!(carried.environment_ref, original.environment_ref);
    }
}

fn assert_same_projection(actual: &ResearchResetOutcome, original: &ResearchResetOutcome) {
    assert_eq!(actual.epoch.epoch_id, original.epoch.epoch_id);
    assert_eq!(actual.brief, original.brief);
    assert_eq!(actual.validation, original.validation);
    assert_eq!(actual.carried_candidates, original.carried_candidates);
    assert_eq!(actual.carried_bank_entry_ids, original.carried_bank_entry_ids);
    assert_eq!(actual.excluded_failure_entries, original.excluded_failure_entries);
}

#[test]
fn candidate_identity_rejects_changed_evidence_and_preserves_first_observation() {
    let (_root, store, closure, epoch) = fixture();
    let foreign = store.open_research_lineage(EntityId::new(), EntityId::new(), None, &closure).unwrap();
    let artifact = store.cas.seal_object(b"original", "text/plain", "test", "raw", 1).unwrap().object;
    let other = store.cas.seal_object(b"different", "text/plain", "test", "raw", 1).unwrap().object;
    for present in [false, true] {
        let original = ResearchCandidate {
            candidate_id: EntityId::new(), epoch_id: epoch.epoch_id,
            artifact_ref: artifact.clone(), score: present.then_some(8.0),
            score_source: present.then(|| "byte count".into()),
            environment_ref: if present { epoch.environment_ref.clone() } else { None },
            notes: present.then(|| "first observation".into()), frozen: false, created_at_ms: 1,
        };
        store.add_research_candidate(&original).unwrap();
        let retained = store.research_candidates(epoch.epoch_id).unwrap();
        let seq = store.current_commit_seq().unwrap();
        let mut replay = original.clone();
        replay.created_at_ms = 99;
        store.add_research_candidate(&replay).unwrap();
        for field in ["epoch", "artifact", "score", "verifier", "environment", "notes"]
            .into_iter().chain(present.then_some("score_absence")) {
            let mut changed = original.clone();
            match field {
                "epoch" => changed.epoch_id = foreign.epoch_id,
                "artifact" => changed.artifact_ref = other.clone(),
                "score" => changed.score = Some(999.0),
                "score_absence" => changed.score = None,
                "verifier" => changed.score_source = (!present).then(|| "other verifier".into()),
                "environment" => changed.environment_ref = if present { None } else { epoch.environment_ref.clone() },
                "notes" => changed.notes = (!present).then(|| "other observation".into()),
                _ => unreachable!(),
            }
            assert!(matches!(store.add_research_candidate(&changed),
                Err(shenko_types::error::ShenkoError::Conflict(_))), "{field}, present={present}");
        }
        assert_eq!(store.research_candidates(epoch.epoch_id).unwrap(), retained);
        assert_eq!(store.current_commit_seq().unwrap(), seq);
    }
}

#[test]
fn bank_identity_rejects_changed_evidence_and_correction_targets() {
    let (_root, store, closure, epoch) = fixture();
    let foreign = store.open_research_lineage(EntityId::new(), EntityId::new(), None, &closure).unwrap();
    for present in [false, true] {
        let mut original = bank_entry(epoch.epoch_id, BankEntryKind::Requirement, "retained fact");
        original.confidence = present.then_some(0.75);
        store.put_bank_entry(&original).unwrap();
        let seq = store.current_commit_seq().unwrap();
        let mut replay = original.clone();
        replay.created_at_ms = 99;
        store.put_bank_entry(&replay).unwrap();
        for field in ["epoch", "kind", "content", "provenance", "scope", "confidence",
            "revision", "ancestry", "supersedes"]
            .into_iter().chain(present.then_some("confidence_absence")) {
            let mut changed = original.clone();
            match field {
                "epoch" => changed.epoch_id = foreign.epoch_id,
                "kind" => changed.kind = BankEntryKind::Attempt,
                "content" => changed.content = "different fact".into(),
                "provenance" => changed.source_refs = vec!["fixture:other-fact".into()],
                "scope" => changed.scope = "other scope".into(),
                "confidence" => changed.confidence = Some(0.25),
                "confidence_absence" => changed.confidence = None,
                "revision" => changed.base_revision += 1,
                "ancestry" => changed.carried_from = Some(EntityId::new()),
                "supersedes" => changed.supersedes = Some(EntityId::new()),
                _ => unreachable!(),
            }
            assert!(matches!(store.put_bank_entry(&changed),
                Err(shenko_types::error::ShenkoError::Conflict(_))), "{field}, present={present}");
        }
        assert_eq!(store.bank_entry(original.entry_id).unwrap(), Some(original));
        assert_eq!(store.current_commit_seq().unwrap(), seq);
    }
}

#[test]
fn original_writes_replay_after_freeze_correction_and_tombstone_without_reverting_them() {
    let (root, store, closure, epoch) = fixture();
    let original = candidate(&store, &epoch, b"retained candidate");
    let old = bank_entry(epoch.epoch_id, BankEntryKind::Requirement, "old fact");
    let replacement = bank_entry(epoch.epoch_id, BankEntryKind::Requirement, "corrected fact");
    let unrelated = bank_entry(epoch.epoch_id, BankEntryKind::Requirement, "unrelated fact");
    for entry in [&old, &replacement, &unrelated] { store.put_bank_entry(entry).unwrap(); }
    store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "freeze evidence", &closure, Some(original.candidate_id)).unwrap();
    store.supersede_bank_entry(old.entry_id, replacement.entry_id).unwrap();
    store.tombstone_bank_entry(old.entry_id, "withdrawn").unwrap();
    let retired = store.bank_entry(old.entry_id).unwrap();
    let corrected = store.bank_entry(replacement.entry_id).unwrap().unwrap();
    assert_eq!(corrected.supersedes, Some(old.entry_id));
    drop(store);
    let store = Store::open(root.path().join("research.db"), root.path().join("cas")).unwrap();
    let seq = store.current_commit_seq().unwrap();
    store.add_research_candidate(&original).unwrap();
    store.put_bank_entry(&old).unwrap();
    store.put_bank_entry(&replacement).unwrap();
    store.put_bank_entry(&corrected).unwrap();
    let mut conflicting = corrected.clone();
    conflicting.supersedes = Some(unrelated.entry_id);
    assert!(matches!(store.put_bank_entry(&conflicting),
        Err(shenko_types::error::ShenkoError::Conflict(_))));
    let mut frozen = original;
    frozen.frozen = true;
    assert_eq!(store.research_candidates(epoch.epoch_id).unwrap(), vec![frozen]);
    assert_eq!(store.bank_entry(old.entry_id).unwrap(), retired);
    assert_eq!(store.bank_entry(replacement.entry_id).unwrap(), Some(corrected));
    assert_eq!(store.bank_entry(unrelated.entry_id).unwrap(), Some(unrelated));
    assert_eq!(store.current_commit_seq().unwrap(), seq);
}

#[test]
fn tombstones_leave_active_views_but_remain_readable_after_reopen() {
    let (root, store, _, epoch) = fixture();
    let mut retired = bank_entry(epoch.epoch_id, BankEntryKind::Requirement, "retired requirement");
    let attempt = bank_entry(epoch.epoch_id, BankEntryKind::Attempt, "retired failure narrative");
    let live = bank_entry(epoch.epoch_id, BankEntryKind::OpenGoal, "unfinished goal");
    for entry in [&retired, &attempt, &live] { store.put_bank_entry(entry).unwrap(); }
    store.tombstone_bank_entry(retired.entry_id, "retired").unwrap();
    store.tombstone_bank_entry(attempt.entry_id, "retired").unwrap();
    drop(store);
    let store = Store::open(root.path().join("research.db"), root.path().join("cas")).unwrap();
    assert_eq!(store.bank_entries(epoch.epoch_id, BankSelection::All).unwrap(), vec![live.clone()]);
    assert_eq!(store.bank_entries(epoch.epoch_id, BankSelection::Carried).unwrap(), vec![live]);
    let retained = store.bank_entry(retired.entry_id).unwrap().unwrap();
    assert!(retained.tombstoned_at_ms.is_some());
    retired.tombstoned_at_ms = retained.tombstoned_at_ms;
    assert_eq!(retained, retired);
    let failures = store.bank_entries(epoch.epoch_id, BankSelection::Failures).unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].entry_id, attempt.entry_id);
    assert_eq!(failures[0].content, attempt.content);
    assert!(failures[0].tombstoned_at_ms.is_some());
    assert_eq!(store.bank_entry(EntityId::new()).unwrap(), None);
}

#[test]
fn successive_resets_keep_inherited_diversity_and_replay_the_sealed_projection() {
    let (root, store, closure, epoch) = fixture();
    let a = candidate(&store, &epoch, b"best A");
    let b = candidate(&store, &epoch, b"alternative B");
    let goal = bank_entry(epoch.epoch_id, BankEntryKind::OpenGoal, "continue comparing");
    let retired = bank_entry(epoch.epoch_id, BankEntryKind::Attempt, "retired attempt");
    let live_failure = bank_entry(epoch.epoch_id, BankEntryKind::Diagnosis, "retained diagnosis");
    for entry in [&goal, &retired, &live_failure] { store.put_bank_entry(entry).unwrap(); }
    store.tombstone_bank_entry(retired.entry_id, "retired").unwrap();
    let request1 = EntityId::new();
    let first = store.reset_research_epoch(request1, epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "first", &closure, Some(a.candidate_id)).unwrap();
    assert_eq!(first.excluded_failure_entries, 2);
    assert_candidates(&first.carried_candidates, &[&a, &b]);
    assert_eq!(first.brief.carried_bank_entries[0].carried_from, goal.entry_id);
    store.tombstone_bank_entry(first.carried_bank_entry_ids[0], "goal changed").unwrap();
    let new_goal = bank_entry(first.epoch.epoch_id, BankEntryKind::OpenGoal, "next comparison");
    store.put_bank_entry(&new_goal).unwrap();
    let c = candidate(&store, &first.epoch, b"new candidate C");
    let replay = store.reset_research_epoch(request1, epoch.lineage_id,
        ResearchResetTrigger::SubjectRequest, "different replay arguments", &closure, None).unwrap();
    assert_same_projection(&replay, &first);
    let request2 = EntityId::new();
    let second = store.reset_research_epoch(request2, epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "explicit inherited best", &closure, Some(a.candidate_id)).unwrap();
    assert!(second.validation.valid);
    assert_eq!(second.epoch.best_candidate_id, Some(a.candidate_id));
    assert_eq!(second.brief.best.as_ref().unwrap().candidate_id, a.candidate_id);
    assert_candidates(&second.carried_candidates, &[&a, &b, &c]);
    assert!(store.research_candidates(second.epoch.epoch_id).unwrap().is_empty());
    let third = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "implicit inherited best", &closure, None).unwrap();
    assert!(third.validation.valid);
    assert_eq!(third.epoch.best_candidate_id, Some(a.candidate_id));
    assert_eq!(third.brief.best.as_ref().unwrap().candidate_id, a.candidate_id);
    assert_candidates(&third.carried_candidates, &[&a, &b, &c]);
    assert!(store.research_candidates(third.epoch.epoch_id).unwrap().is_empty());
    drop(store);
    let store = Store::open(root.path().join("research.db"), root.path().join("cas")).unwrap();
    let replay = store.reset_research_epoch(request2, epoch.lineage_id,
        ResearchResetTrigger::SubjectRequest, "reopen replay", &closure, None).unwrap();
    assert_same_projection(&replay, &second);
    let original_rows = store.research_candidates(epoch.epoch_id).unwrap();
    assert_eq!(original_rows.len(), 2);
    assert!(original_rows.iter().all(|row| row.frozen));
    for original in [&a, &b, &c] {
        assert_eq!(store.cas.read_object(&original.artifact_ref.digest).unwrap().len() as f64,
            original.score.unwrap());
    }
}

#[test]
fn invalid_inherited_best_is_not_presented_as_a_valid_starting_point() {
    let (_root, store, closure, epoch) = fixture();
    let best = candidate(&store, &epoch, b"verified artifact");
    let first = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "first", &closure, Some(best.candidate_id)).unwrap();
    let path = store.cas.verified_object_path(&best.artifact_ref).unwrap();
    // Corrupt only this test's owned CAS object, after the first reset accepted it.
    std::fs::write(path, b"corrupt artifact").unwrap();
    assert!(matches!(store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "explicit invalid best", &closure, Some(best.candidate_id)),
        Err(shenko_types::error::ShenkoError::Conflict(_))));
    assert_eq!(store.active_research_epoch(epoch.lineage_id).unwrap().unwrap().epoch_id, first.epoch.epoch_id);
    let implicit = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "implicit invalid best", &closure, None).unwrap();
    assert!(!implicit.validation.valid);
    assert_eq!(implicit.validation.candidate_id, Some(best.candidate_id));
    assert_eq!(implicit.epoch.best_candidate_id, None);
    assert_eq!(implicit.brief.best, None);
    assert_candidates(&implicit.carried_candidates, &[&best]);
}

#[test]
fn a_missing_sealed_brief_cannot_silently_erase_inheritance() {
    let (root, store, closure, epoch) = fixture();
    let best = candidate(&store, &epoch, b"retained artifact");
    let first = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "first", &closure, Some(best.candidate_id)).unwrap();
    // Simulate a damaged durable row, not an alternative production write path.
    let connection = rusqlite::Connection::open(root.path().join("research.db")).unwrap();
    connection.execute("UPDATE research_epochs SET brief_ref=NULL WHERE epoch_id=?1",
        [first.epoch.epoch_id.to_string()]).unwrap();
    assert!(store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "cannot lose inherited candidates", &closure, None).is_err());
    assert_eq!(store.active_research_epoch(epoch.lineage_id).unwrap().unwrap().epoch_id, first.epoch.epoch_id);
    assert_eq!(store.research_candidates(epoch.epoch_id).unwrap()[0].candidate_id, best.candidate_id);
    assert_eq!(store.cas.read_object(&best.artifact_ref.digest).unwrap(), b"retained artifact");
}

// Fault injection into an owned database: keep the CAS envelope valid while
// reproducing the incomplete projections produced by the older reset writer.
fn replace_brief(store: &Store, database: &std::path::Path, brief: &ResearchWorkerBrief) {
    let reference = store.cas.seal_object(&serde_json::to_vec(brief).unwrap(),
        "application/json", "research-closures", "research-worker-brief", 1).unwrap().object;
    rusqlite::Connection::open(database).unwrap().execute(
        "UPDATE research_epochs SET brief_ref=?1,reset_from_epoch=?2 WHERE epoch_id=?3",
        rusqlite::params![serde_json::to_string(&reference).unwrap(),
            brief.archive_epoch_id.map(|id| id.to_string()), brief.epoch_id.to_string()],
    ).unwrap();
}

#[test]
fn legacy_omissions_recover_from_sealed_ancestry_without_rewriting_old_briefs() {
    let (root, store, closure, epoch) = fixture();
    let database = root.path().join("research.db");
    let a = candidate(&store, &epoch, b"best A");
    let b = candidate(&store, &epoch, b"alternative B");
    let first = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "first", &closure, Some(a.candidate_id)).unwrap();
    let c = candidate(&store, &first.epoch, b"new candidate C");
    let mut old_briefs = Vec::new();
    for _ in 0..2 {
        let mut brief = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
            ResearchResetTrigger::PlannedRound, "legacy reset", &closure, None).unwrap().brief;
        brief.carried_candidates.retain(|item| item.candidate_id == c.candidate_id);
        brief.best = None;
        replace_brief(&store, &database, &brief);
        old_briefs.push(brief);
    }
    // Recovery must not depend on a dangling current best, nor reread a mutable
    // historical candidate row in place of its original sealed evidence.
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.execute("UPDATE research_epochs SET best_candidate_id=NULL WHERE epoch_id=?1",
        [old_briefs.last().unwrap().epoch_id.to_string()]).unwrap();
    connection.execute("UPDATE research_candidates SET score=999,score_source='later row change' WHERE candidate_id=?1",
        [a.candidate_id.to_string()]).unwrap();
    drop(connection);
    drop(store);
    let store = Store::open(database, root.path().join("cas")).unwrap();
    let request = EntityId::new();
    let recovered = store.reset_research_epoch(request, epoch.lineage_id,
        ResearchResetTrigger::SubjectRequest, "recover inherited candidates", &closure, Some(a.candidate_id)).unwrap();
    assert!(recovered.validation.valid);
    assert_eq!(recovered.brief.best.as_ref().unwrap().candidate_id, a.candidate_id);
    assert_candidates(&recovered.carried_candidates, &[&a, &b, &c]);
    assert!(store.research_candidates(recovered.epoch.epoch_id).unwrap().is_empty());
    for brief in old_briefs {
        assert_eq!(store.research_worker_brief(brief.epoch_id).unwrap().unwrap(), brief);
    }
    let replay = store.reset_research_epoch(request, epoch.lineage_id,
        ResearchResetTrigger::PlannedRound, "replay", &closure, None).unwrap();
    assert_same_projection(&replay, &recovered);
}

#[test]
fn broken_ancestry_cannot_publish_a_partial_candidate_pool() {
    for fault in ["missing", "foreign", "cycle", "missing_root", "digest"] {
        let (root, store, closure, epoch) = fixture();
        let best = candidate(&store, &epoch, b"verified A");
        let first = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
            ResearchResetTrigger::PlannedRound, "first", &closure, Some(best.candidate_id)).unwrap();
        let mut brief = first.brief;
        match fault {
            "missing" => brief.archive_epoch_id = Some(EntityId::new()),
            "foreign" => brief.archive_epoch_id = Some(store.open_research_lineage(
                EntityId::new(), EntityId::new(), None, &closure).unwrap().epoch_id),
            "cycle" => brief.archive_epoch_id = Some(brief.epoch_id),
            "missing_root" => brief.archive_epoch_id = None,
            "digest" => {
                let path = store.cas.verified_object_path(epoch.brief_ref.as_ref().unwrap()).unwrap();
                let mut bytes = std::fs::read(&path).unwrap();
                bytes.push(b' '); // Still valid JSON; no longer the sealed bytes.
                std::fs::write(path, bytes).unwrap();
            }
            _ => unreachable!(),
        }
        replace_brief(&store, &root.path().join("research.db"), &brief);
        assert!(store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
            ResearchResetTrigger::PlannedRound, "reject partial inheritance", &closure,
            Some(best.candidate_id)).is_err(), "{fault}");
        assert_eq!(store.active_research_epoch(epoch.lineage_id).unwrap().unwrap().epoch_id,
            first.epoch.epoch_id, "{fault}");
        assert_eq!(store.cas.read_object(&best.artifact_ref.digest).unwrap(), b"verified A");
    }
}

#[test]
fn inconsistent_sealed_candidate_evidence_refuses_a_new_epoch() {
    for repeated in [false, true] {
        let (root, store, closure, epoch) = fixture();
        let best = candidate(&store, &epoch, b"verified A");
        let mut first = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
            ResearchResetTrigger::PlannedRound, "first", &closure, Some(best.candidate_id)).unwrap();
        let second = store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
            ResearchResetTrigger::PlannedRound, "second", &closure, None).unwrap();
        if repeated {
            first.brief.carried_candidates.push(first.brief.carried_candidates[0].clone());
        } else {
            first.brief.carried_candidates[0].score = Some(999.0);
        }
        replace_brief(&store, &root.path().join("research.db"), &first.brief);
        assert!(store.reset_research_epoch(EntityId::new(), epoch.lineage_id,
            ResearchResetTrigger::PlannedRound, "reject inconsistent evidence", &closure,
            Some(best.candidate_id)).is_err());
        assert_eq!(store.active_research_epoch(epoch.lineage_id).unwrap().unwrap().epoch_id,
            second.epoch.epoch_id);
    }
}
