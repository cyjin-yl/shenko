//! Durable runtime-error journal — SPEC §45.14. Local SQLite/CAS only; no
//! provider or deployment evidence is claimed here.
use shenko_store::{RUNTIME_ERROR_EVENT_TYPE, RuntimeErrorContext, Store};
use shenko_types::commit::CommitRef;
use shenko_types::ids::*;
use shenko_types::subject::*;
use std::path::PathBuf;

fn fixture() -> (PathBuf, Store, EntityId) {
    let dir = std::env::temp_dir().join(format!("shenko-journal-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let agent = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(b"seed", "text/plain", "test", "raw", 1)
        .unwrap()
        .object;
    let first_id = EntityId::new();
    let genesis = SubjectGenesis {
        genesis_id: EntityId::new(),
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(),
        initial_profile_ref: object.clone(),
        home_manifest_ref: object.clone(),
        bootstrap_event_ref: object.clone(),
        created_commit: precommit,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(7),
        software_generation: RuntimeGeneration("fixture-generation".into()),
        genesis_ref: GenesisRef { ledger_id, genesis_id: genesis.genesis_id },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: precommit,
        next_step_id: first_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first = SubjectStep {
        step_id: first_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: precommit,
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
        precommit,
    );
    store
        .initialize_genesis(&genesis, &head, &first, &root_task)
        .unwrap();
    (dir, store, agent)
}

#[test]
fn runtime_error_facts_commit_redacted_and_readable() {
    let (dir, store, agent) = fixture();
    let stored = store
        .record_runtime_error(
            agent,
            RuntimeErrorContext {
                kind: "subject_loop".into(),
                source_module: "shenko_subject::loop_runner".into(),
                message: "request failed with api_key=sk-9f8s7d and authorization: Bearer abc123 tail".into(),
                step_no: Some(41),
                task_id: None,
                extra: serde_json::json!({"step_id": "00000000-0000-0000-0000-000000000001"}),
            },
        )
        .unwrap();
    assert_eq!(stored.event_type, RUNTIME_ERROR_EVENT_TYPE);
    let second = store
        .record_runtime_error(
            agent,
            RuntimeErrorContext {
                kind: "native_execute".into(),
                source_module: "shenko_subject::child".into(),
                message: format!("token=def456 长消息{}尾部", "很".repeat(6000)),
                step_no: None,
                task_id: Some(EntityId::new()),
                extra: serde_json::json!({}),
            },
        )
        .unwrap();
    assert!(second.seq > stored.seq);

    let session_id = stored.session_id;
    let events = store.session_events_range(session_id, 1, i64::MAX).unwrap();
    assert_eq!(events.len(), 2);
    let first: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(first["v"], 1);
    assert_eq!(first["kind"], "subject_loop");
    assert_eq!(first["source_module"], "shenko_subject::loop_runner");
    assert_eq!(first["step_no"], 41);
    let message = first["message"].as_str().unwrap();
    assert!(!message.contains("sk-9f8s7d"), "api key must be redacted: {message}");
    assert!(!message.contains("abc123"), "bearer token must be redacted: {message}");
    assert!(message.contains("<redacted>"));
    assert_eq!(first["message_truncated"], false);
    assert_eq!(first["journal_dropped_total"], 0);
    assert!(first["error_digest"].as_str().unwrap().starts_with("blake3:"));
    assert!(first["occurred_at"].as_str().unwrap().contains("T"));
    assert_eq!(first["extra"]["step_id"], "00000000-0000-0000-0000-000000000001");

    let second_entry: serde_json::Value = serde_json::from_str(&events[1].data).unwrap();
    let second_message = second_entry["message"].as_str().unwrap();
    assert_eq!(second_entry["message_truncated"], true);
    assert!(!second_message.contains("def456"), "token must be redacted: {second_message}");
    assert!(second_message.chars().count() <= 4096 + 1);

    // Searchable as ordinary raw facts through the same read surface.
    let hits = store
        .search_session_events(
            session_id,
            "redacted",
            &shenko_store::SessionEventFilter::default(),
        )
        .unwrap();
    assert_eq!(hits.len(), 2);
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn runtime_error_journal_fail_soft_counts_drops_and_surfaces_them() {
    let (dir, store, agent) = fixture();
    // An agent with no subject branch cannot be journaled; the call is
    // fail-soft and only counts.
    store.journal_runtime_error(
        EntityId::new(),
        RuntimeErrorContext {
            kind: "subject_loop".into(),
            source_module: "test".into(),
            message: "loop died before genesis".into(),
            ..Default::default()
        },
    );
    assert_eq!(store.error_journal_dropped(), 1);
    store.journal_runtime_error(
        EntityId::new(),
        RuntimeErrorContext {
            kind: "subject_loop".into(),
            source_module: "test".into(),
            message: "loop died again".into(),
            ..Default::default()
        },
    );
    assert_eq!(store.error_journal_dropped(), 2);

    // The next committed entry surfaces the cumulative drop count to readers.
    let stored = store
        .record_runtime_error(
            agent,
            RuntimeErrorContext {
                kind: "native_dispatcher".into(),
                source_module: "shenko_subject::dispatch".into(),
                message: "dispatcher pass failed".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let data: serde_json::Value = serde_json::from_str(&stored.data).unwrap();
    assert_eq!(data["journal_dropped_total"], 2);
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}
