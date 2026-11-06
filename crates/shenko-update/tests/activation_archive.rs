//! One installation's own copies are not peers.
//!
//! A forward activation and a rollback both copy the selected generation into
//! the next slot and then import the facts the selected copy committed since.
//! The old import path treated that copy as a foreign peer: every non-terminal
//! task was parked with a `cognition_imported` memo and every `ready`/`claimed`
//! subject intent became `unknown`, so a candidate holding its own unmodified
//! root task reported a conflict before its smoke could even run.
//!
//! These tests use two real SQLite copies of one genesis installation — the
//! same backup `slots::copy_slot` takes, with the copy's own ledger writer
//! identity — and assert the observable end state of `import_delta` /
//! `import_rollback_delta`: the live copy's work and memory arrive with the
//! state they really have, an exact retry writes nothing, a peer destination
//! still parks the same archive, and another installation's state is refused
//! before anything is published.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
use shenko_store::memory::MemoryEntry;
use shenko_store::{MemoryIntentRecord, Store};
use shenko_types::activation::{ActivationOrigin, ActivationSelector};
use shenko_types::commit::CommitRef;
use shenko_types::ids::{
    BranchId, ControllerEpoch, EntityId, LedgerSeq, RecordRevision, RuntimeGeneration,
};
use shenko_types::object::ObjectRef;
use shenko_types::subject::{
    GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
};
use shenko_types::task::{DispatchEligibility, TaskRecord};

use shenko_update::migration::{self, import_delta, import_rollback_delta};
use shenko_update::slots;

/// One installation under a shared CAS: the generation that currently holds
/// control and the offline copy a hand-over made from it.
struct Fixture {
    root: PathBuf,
    cas: PathBuf,
    /// The generation that currently holds control.
    source: PathBuf,
    /// The offline copy an activation/rollback imports into.
    candidate: PathBuf,
    agent: EntityId,
    root_task: EntityId,
}

impl Fixture {
    fn source_ledger(&self) -> EntityId {
        self.open_source().ledger.ledger_id()
    }

    fn open_source(&self) -> Store {
        Store::open(self.source.clone(), self.cas.clone()).unwrap()
    }

    fn open_candidate(&self) -> Store {
        Store::open(self.candidate.clone(), self.cas.clone()).unwrap()
    }

    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A throwaway installation root plus the shared CAS both copies use.
fn installation_root() -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("shenko-activation-archive-{}", EntityId::new()));
    let cas = root.join("cas");
    fs::create_dir_all(&cas).unwrap();
    (root, cas)
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

/// A real genesis installation: immutable birth, branch head, first planned
/// step, root task and the committed genesis selector.
///
/// The agent identity and the installation name are caller-supplied, so a
/// second, foreign installation can be built over the same CAS with the same
/// subject id and a different birth.
fn installation(cas: &Path, agent: EntityId, name: &str) -> Fixture {
    let root = cas.parent().unwrap().to_path_buf();
    let source = root.join(name).join("selected").join("shenko.db");
    let candidate = root.join(name).join("next").join("shenko.db");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    let store = Store::open(source.clone(), cas.to_path_buf()).unwrap();
    let ledger_id = store.ledger.ledger_id();
    let generation = RuntimeGeneration::default();
    let seed_manifest = seal(
        &store,
        &serde_json::json!({ "kind": "seed_manifest", "complete_product_seed": false }),
    );
    let installation_id = EntityId::new();
    let bootstrap_event = seal(
        &store,
        &serde_json::json!({
            "event": "bootstrap",
            "kind": "genesis",
            "installation_id": installation_id,
            "seed_manifest_ref": seed_manifest,
        }),
    );
    let genesis_id = EntityId::new();
    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: seed_manifest.clone(),
        initial_profile_ref: seed_manifest.clone(),
        home_manifest_ref: seed_manifest.clone(),
        bootstrap_event_ref: bootstrap_event,
        created_commit: CommitRef::new(ledger_id, LedgerSeq(0)),
    };
    let first_step_id = EntityId::new();
    let root_task_id = EntityId::new();
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: root_task_id,
        controller_epoch: ControllerEpoch(1),
        software_generation: generation.clone(),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
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
    let state_ref = seal(
        &store,
        &serde_json::json!({ "schema_version": 1, "generation": generation.0 }),
    );
    let route_ref = seal(
        &store,
        &serde_json::json!({ "schema_version": 1, "generation": generation.0 }),
    );
    let selector = ActivationSelector {
        agent_id: agent,
        selector_revision: 0,
        active_epoch: 1,
        generation: generation.clone(),
        state_ref,
        origin: ActivationOrigin::Genesis {
            genesis_ref: GenesisRef { ledger_id, genesis_id },
        },
        route_authority_ref: route_ref,
    };
    let root_task =
        TaskRecord::subject_root(root_task_id, agent, seed_manifest, genesis.created_commit);
    store
        .initialize_genesis_with(&genesis, &head, &first_step, &root_task, Some(&selector))
        .unwrap();

    Fixture {
        root,
        cas: cas.to_path_buf(),
        source,
        candidate,
        agent,
        root_task: root_task_id,
    }
}

/// Take the same consistent copy `slots::copy_slot` takes, including the
/// copy's own ledger writer identity: independent local commits cannot share
/// `(ledger_id, sequence)`, while every copied source `CommitRef` stays as it
/// was.
fn copy_source(fixture: &Fixture) {
    let candidate = fixture.candidate.clone();
    fs::create_dir_all(candidate.parent().unwrap()).unwrap();
    slots::backup_database(&fixture.source, &candidate).unwrap();
    let copy = Connection::open(&candidate).unwrap();
    copy.execute(
        "UPDATE ledger_identity SET ledger_id=?1 WHERE singleton=1",
        [EntityId::new().to_string()],
    )
    .unwrap();
    copy.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
}

/// Commit one memory fact through the real save path.
fn save(store: &Store, agent: EntityId, content: &str) -> (EntityId, EntityId) {
    let intent = EntityId::new();
    let entry_id = EntityId::new();
    let record = MemoryIntentRecord::accepted(
        intent,
        agent,
        "save",
        "personal",
        content,
        None,
        Some(entry_id),
    );
    store.admit_memory_intent(&record, None).unwrap();
    let mut entry = MemoryEntry::new(agent, content);
    entry.entry_id = entry_id;
    entry.kind = "note".into();
    store.commit_memory_intent(&entry, intent).unwrap();
    (intent, entry_id)
}

/// A forward activation migrates the selected copy's closure into the offline
/// candidate before the candidate is smoked. Its own work must arrive as it is:
/// the root task keeps its identity, state and revision (no park memo, no
/// invented unknown intents), and a memory request the live copy settled after
/// the copy was taken continues in the candidate instead of being refused.
#[test]
fn a_forward_activation_delta_admits_the_live_copys_own_work() {
    let agent = EntityId::new();
    let (_root, cas) = installation_root();
    let fixture = installation(&cas, agent, "owned");
    // The request exists when the copy is taken, but is not settled yet.
    let pending_intent = EntityId::new();
    let pending_entry = EntityId::new();
    {
        let source = fixture.open_source();
        let record = MemoryIntentRecord::accepted(
            pending_intent,
            agent,
            "save",
            "personal",
            "decided before the copy",
            None,
            Some(pending_entry),
        );
        source.admit_memory_intent(&record, None).unwrap();
    }
    copy_source(&fixture);
    // After W0 the live copy keeps committing: it settles that request and
    // records a second fact.
    let (new_intent, new_entry) = {
        let source = fixture.open_source();
        let mut entry = MemoryEntry::new(agent, "decided before the copy");
        entry.entry_id = pending_entry;
        entry.kind = "note".into();
        source.commit_memory_intent(&entry, pending_intent).unwrap();
        save(&source, agent, "recorded after the copy")
    };
    let candidate_ledger = fixture.open_candidate().ledger.ledger_id();

    let report = import_delta(&fixture.source, &fixture.candidate, &fixture.cas).unwrap();
    assert!(
        report.blocked_units.is_empty(),
        "an owned candidate must import its own live copy without blocked units: {:?}",
        report.blocked_units
    );
    assert!(
        report.archive_path.is_some(),
        "the typed snapshot stays beside the retained copy as recovery evidence"
    );
    assert!(
        report.rows_written() > 0,
        "the facts the live copy committed after the copy must actually be written"
    );

    let candidate = fixture.open_candidate();
    // The work is the live copy's own work: same record, no import memo.
    let source_task = fixture
        .open_source()
        .get_task(fixture.root_task)
        .unwrap()
        .expect("the selected copy has its root task");
    let candidate_task = candidate
        .get_task(fixture.root_task)
        .unwrap()
        .expect("the root task is carried into the candidate");
    assert!(
        !candidate_task.memos.contains_key("cognition_imported"),
        "an owned copy must not be parked as foreign work: {:?}",
        candidate_task.memos.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        serde_json::to_value(&candidate_task).unwrap(),
        serde_json::to_value(&source_task).unwrap(),
        "the candidate holds the live copy's task record exactly"
    );
    // The memory request the live copy settled after the copy continues here.
    assert_eq!(
        candidate
            .memory_intent_state(pending_intent)
            .unwrap()
            .as_deref(),
        Some("committed"),
        "a settled request is carried with its terminal state"
    );
    assert_eq!(
        candidate
            .memory_entry(pending_entry)
            .unwrap()
            .expect("the settled fact arrived")
            .content,
        "decided before the copy"
    );
    assert_eq!(
        candidate
            .memory_entry(new_entry)
            .unwrap()
            .expect("the later fact arrived")
            .content,
        "recorded after the copy"
    );
    assert_eq!(
        candidate.memory_intent_state(new_intent).unwrap().as_deref(),
        Some("committed")
    );
    // Importing facts is not a new installation: the copy keeps its own writer
    // identity, and the candidate is still not the selected generation.
    assert_eq!(candidate.ledger.ledger_id(), candidate_ledger);
    let (head, _) = candidate.load_subject(agent).unwrap();
    assert_eq!(head.agent_id, fixture.agent);
    assert_eq!(
        head.mode,
        SubjectMode::Active,
        "the imported boundary is the live copy's own boundary, not a new authority"
    );
    drop(candidate);

    // An exact retry rewrites nothing and duplicates no fact.
    let retry = import_delta(&fixture.source, &fixture.candidate, &fixture.cas).unwrap();
    assert!(retry.blocked_units.is_empty(), "{:?}", retry.blocked_units);
    assert_eq!(retry.rows_written(), 0, "an exact retry writes no row");
    let candidate = fixture.open_candidate();
    assert_eq!(candidate.list_memory(64).unwrap().len(), 2);
    assert_eq!(
        candidate.get_task(fixture.root_task).unwrap().unwrap().record_revision,
        source_task.record_revision
    );
    drop(candidate);
    fixture.cleanup();
}

/// The very archive an owned candidate admits verbatim still arrives parked at
/// a peer: a replica is not a second live writer, and its copy of the root task
/// is explicitly marked and never dispatchable there.
#[test]
fn the_same_archive_still_parks_at_a_peer() {
    let agent = EntityId::new();
    let (_root, cas) = installation_root();
    let fixture = installation(&cas, agent, "owned");
    let source_ledger = fixture.source_ledger();
    let archive = {
        let source = fixture.open_source();
        let export = source
            .export_cognition(&CognitionSnapshotOptions {
                memory_root: None,
                workspace_roots: Vec::new(),
                include_git_history: false,
                local_boundary_preserved: false,
            })
            .unwrap();
        let mut bytes = Vec::new();
        export.write_archive(&mut bytes).unwrap();
        bytes
    };
    let peer_dir = fixture.root.join("peer");
    fs::create_dir_all(&peer_dir).unwrap();
    let peer = Store::open(peer_dir.join("shenko.db"), fixture.cas.clone()).unwrap();
    assert!(
        peer.get_existing_agent_id().unwrap().is_none(),
        "a peer store is not this installation"
    );
    let outcome = peer
        .import_cognition_archive(
            &archive[..],
            &CognitionImportOptions {
                memory_root: None,
                workspace_root: None,
                peer_id: Some(source_ledger),
            },
        )
        .unwrap();
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    let task = peer
        .get_task(fixture.root_task)
        .unwrap()
        .expect("a peer keeps the replicated task as recovery data");
    assert!(
        task.memos.contains_key("cognition_imported"),
        "peer work stays parked and marked"
    );
    let parked = matches!(task.dispatch_eligibility, DispatchEligibility::Parked { .. });
    assert!(parked, "a peer never dispatches replicated work");
    drop(peer);
    fixture.cleanup();
}

/// A rollback is the same import in the other direction: the newer generation
/// is paused at a clean boundary and its closure, including that boundary, is
/// imported into the retained copy. The retained copy carries the paused head,
/// its selector and the newer generation's work unchanged.
#[test]
fn a_rollback_delta_carries_the_paused_boundary_and_the_work() {
    let agent = EntityId::new();
    let (_root, cas) = installation_root();
    let fixture = installation(&cas, agent, "owned");
    copy_source(&fixture);
    let paused = {
        let source = fixture.open_source();
        let (head, _) = source.load_subject(agent).unwrap();
        source
            .request_subject_quiesce(agent, EntityId::new())
            .unwrap();
        source
            .pause_subject_at_boundary(&head)
            .unwrap()
            .expect("the boundary pauses the subject")
    };
    assert_eq!(paused.mode, SubjectMode::Paused);

    let report = import_rollback_delta(&fixture.source, &fixture.candidate, &fixture.cas).unwrap();
    assert!(
        report.blocked_units.is_empty(),
        "a rollback into the retained copy of the same installation must not be blocked: {:?}",
        report.blocked_units
    );
    assert_eq!(report.uncovered_source_commits, 0);

    let source = fixture.open_source();
    let candidate = fixture.open_candidate();
    // The complete paused boundary arrived: same head identity and revision,
    // same selector, and a readable clean successor in THIS ledger.
    let (source_head, source_step) = source.load_subject(agent).unwrap();
    let (candidate_head, candidate_step) = candidate.load_subject(agent).unwrap();
    assert_eq!(candidate_head.mode, SubjectMode::Paused);
    assert_eq!(candidate_head.record_revision, paused.record_revision);
    assert_eq!(candidate_head.next_step_id, paused.next_step_id);
    assert_eq!(candidate_head.controller_epoch, source_head.controller_epoch);
    assert_eq!(candidate_step.step_id, source_step.step_id);
    let source_selector = source
        .activation_selector(agent)
        .unwrap()
        .expect("source selector");
    let candidate_selector = candidate
        .activation_selector(agent)
        .unwrap()
        .expect("candidate selector");
    assert_eq!(
        serde_json::to_value(&candidate_selector).unwrap(),
        serde_json::to_value(&source_selector).unwrap(),
        "the successor copy receives its current source's control state"
    );
    // The work is preserved as it is, not parked and not turned into unknown.
    let source_task = source.get_task(fixture.root_task).unwrap().unwrap();
    let candidate_task = candidate.get_task(fixture.root_task).unwrap().unwrap();
    assert!(!candidate_task.memos.contains_key("cognition_imported"));
    assert_eq!(
        serde_json::to_value(&candidate_task).unwrap(),
        serde_json::to_value(&source_task).unwrap()
    );
    drop(candidate);
    drop(source);
    fixture.cleanup();
}

/// Another installation's archive is refused. The candidate keeps its own
/// facts, its own watermark and no retained archive, and the refusal names the
/// identity that did not match.
#[test]
fn a_foreign_installations_archive_is_refused_before_it_publishes() {
    let agent = EntityId::new();
    let (_root, cas) = installation_root();
    let fixture = installation(&cas, agent, "owned");
    copy_source(&fixture);
    // Same subject id and the same CAS, different birth: a different
    // installation wearing the same subject identity.
    let foreign = installation(&fixture.cas, agent, "foreign");
    let before = migration::watermark_of(&fixture.candidate).unwrap();
    let before_task = fixture
        .open_candidate()
        .get_task(fixture.root_task)
        .unwrap()
        .unwrap();

    let report = import_delta(&foreign.source, &fixture.candidate, &fixture.cas).unwrap();
    assert!(
        report
            .blocked_units
            .iter()
            .any(|unit| unit.contains("activation-archive-genesis-mismatch")),
        "the refusal must name the mismatched identity: {:?}",
        report.blocked_units
    );
    assert_eq!(report.rows_written(), 0, "a refused archive writes no row");
    assert_eq!(report.archive_path, None, "a refused archive is not retained");
    assert_eq!(
        migration::watermark_of(&fixture.candidate).unwrap(),
        before,
        "the candidate's own history is untouched"
    );
    let candidate = fixture.open_candidate();
    let after_task = candidate.get_task(fixture.root_task).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&after_task).unwrap(),
        serde_json::to_value(&before_task).unwrap(),
        "the candidate keeps its own task"
    );
    assert!(
        candidate.list_memory(64).unwrap().is_empty(),
        "nothing of the foreign installation reached the candidate"
    );
    drop(candidate);
    let retained = fs::read_dir(fixture.candidate.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains("activation-import"))
        .count();
    assert_eq!(retained, 0, "a refused archive is never written beside the copy");
    // The foreign source itself is untouched: it was only read.
    assert!(Path::new(&foreign.source).is_file());
    fixture.cleanup();
}
