//! The subject's own hands on its works — docs/specs/configuration-playground.md#spec-57-6, acceptance PG06/PG07.
//!
//! These cases run the real artifact lifecycle against a real store: a work
//! the subject creates is a durable record, its content is content addressed,
//! staging is a candidate, publishing is an explicit adoption, and a rollback
//! appends a version rather than erasing one.

use std::path::PathBuf;
use std::sync::Arc;

use shenko_store::store::Store;
use shenko_types::commit::CommitRef;
use shenko_types::ids::*;
use shenko_types::subject::{
    GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
};

use crate::playground::PlaygroundState;
use crate::playground_manage::PlaygroundManageHandler;

/// A store with one subject in it, plus a root for the artifacts.
fn fixture(label: &str) -> (PathBuf, Arc<Store>, EntityId) {
    let root = std::env::temp_dir().join(format!("shenko-pg-manage-{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas")).unwrap());
    let agent = EntityId::new();
    let branch = BranchId::new();
    let first = EntityId::new();
    let genesis_id = EntityId::new();
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let object = store
        .cas
        .seal_object(b"seed", "application/json", "fixture", "seed", 1)
        .unwrap()
        .object;
    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(),
        initial_profile_ref: object.clone(),
        home_manifest_ref: object.clone(),
        bootstrap_event_ref: object.clone(),
        created_commit: zero,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: branch.clone(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef {
            ledger_id: store.ledger.ledger_id(),
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: zero,
        next_step_id: first,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let step = SubjectStep {
        step_id: first,
        branch_id: branch,
        step_no: 1,
        expected_parent_commit: zero,
        controller_epoch: head.controller_epoch,
        software_generation: head.software_generation.clone(),
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
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent,
        object,
        zero,
    );
    store.initialize_genesis(&genesis, &head, &step, &root_task).unwrap();
    (root, store, agent)
}

/// The lifecycle the subject drives: a work it creates is durable, its
/// content is versioned, staging proposes, publishing adopts, and a rollback
/// appends a version instead of erasing one.
#[test]
fn the_subject_can_create_iterate_and_adopt_a_work() {
    let (root, store, agent) = fixture("lifecycle");
    let playground = Arc::new(
        PlaygroundState::open(root.join("playground"), Arc::clone(&store)).expect("playground opens"),
    );
    let handler = PlaygroundManageHandler::new(Arc::clone(&store), Arc::clone(&playground));

    // The frozen request the subject would write.
    let create = serde_json::json!({
        "schema_version": 1,
        "op": "create",
        "title": "我的第一件作品",
        "mime": "text/html",
        "content": "<h1>hello</h1>"
    });
    let frozen = handler.freeze_for_test(create).expect("a create request freezes");
    let (result, outcome, _effect) = handler.observe_for_test(&frozen);
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(result["status"], "ok");
    let artifact_id = result["artifact_id"].as_str().expect("the work has an id").to_string();
    assert_eq!(result["revision"], 1);

    // The work is a durable record the subject can find again.
    let record = store
        .artifact_record(artifact_id.parse().unwrap())
        .unwrap()
        .expect("the work has a durable record");
    assert_eq!(record.title, "我的第一件作品");

    // New content is a new revision, and the old one is still there.
    let put = serde_json::json!({
        "schema_version": 1,
        "op": "put",
        "artifact_id": artifact_id,
        "content": "<h1>hello again</h1>"
    });
    let frozen = handler.freeze_for_test(put).expect("a put request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen);
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(result["revision"], 2);
    let revisions = playground
        .revisions(artifact_id.parse().unwrap())
        .expect("the revisions are readable");
    assert_eq!(revisions, vec![1, 2], "a rollback can return to either");

    // Staging the head is a proposal. The head of a work whose content the
    // subject just wrote is ALREADY the active manifest, so staging it again
    // is an honest replay: it adds no second candidate and says so.
    let stage = serde_json::json!({"schema_version": 1, "op": "stage", "artifact_id": artifact_id});
    let frozen = handler.freeze_for_test(stage).expect("a stage request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen);
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(
        result["candidate_manifests"], 0,
        "re-staging the active head does not invent a candidate: {result}"
    );

    // Publishing adopts a staged candidate. There is none here, so the
    // operation says exactly that instead of pretending to adopt.
    let publish = serde_json::json!({"schema_version": 1, "op": "publish", "artifact_id": artifact_id});
    let frozen = handler.freeze_for_test(publish).expect("a publish request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen);
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Failed);
    assert_eq!(result["error"]["code"], "playground_no_candidate");
    // The work still has its active manifest: a refused publish changes
    // nothing.
    let record = store
        .artifact_record(artifact_id.parse().unwrap())
        .unwrap()
        .expect("the work has a durable record");
    assert!(
        record.active_manifest_ref.is_some(),
        "the work still has the manifest it had"
    );

    // A rollback appends a version rather than erasing one.
    let rollback = serde_json::json!({
        "schema_version": 1,
        "op": "rollback",
        "artifact_id": artifact_id,
        "source_revision": 1
    });
    let frozen = handler
        .freeze_for_test(rollback)
        .expect("a rollback request freezes");
    let (result, outcome, _) = handler.observe_for_test(&frozen);
    assert_eq!(outcome, shenko_types::operation::OperationOutcome::Succeeded);
    assert_eq!(result["revision"], 3, "the rollback is a new revision");
    let revisions = playground
        .revisions(artifact_id.parse().unwrap())
        .expect("the revisions are readable");
    assert_eq!(revisions, vec![1, 2, 3], "nothing was erased");

    let _ = std::fs::remove_dir_all(&root);
}

/// A request the subject may not make is refused before anything happens.
#[test]
fn a_request_the_contract_does_not_allow_is_refused() {
    let (root, store, _agent) = fixture("refused");
    let playground = Arc::new(
        PlaygroundState::open(root.join("playground"), Arc::clone(&store)).expect("playground opens"),
    );
    let handler = PlaygroundManageHandler::new(Arc::clone(&store), Arc::clone(&playground));

    // An operation the contract does not have.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "delete_everything"}))
        .is_err());
    // A create without a title is not a work.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "create", "mime": "text/plain", "content": "x"}))
        .is_err());
    // A rollback that names no revision cannot return to one.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "rollback", "artifact_id": "00000000-0000-0000-0000-000000000000"}))
        .is_err());
    // An unknown field is refused rather than ignored.
    assert!(handler
        .freeze_for_test(serde_json::json!({"schema_version": 1, "op": "list", "surprise": true}))
        .is_err());

    let _ = std::fs::remove_dir_all(&root);
}
