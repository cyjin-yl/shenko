//! Public query inputs over a real installation, Store and durable attempt.

use super::*;
use crate::CognitionWriteHandler;
use parking_lot::Mutex;
use shenko_node::Node;
use shenko_store::TaskInvocation;
use shenko_types::agent::CallContext;
use shenko_types::ids::RuntimeGeneration;
use shenko_types::store::WriteBatch;
use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskState};
use std::collections::BTreeSet;
use std::path::PathBuf;

struct Fixture {
    dir: PathBuf,
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("shenko-cognition-query-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).expect("store"));
        let node = Arc::new(Mutex::new(Node::open(&dir.join("node")).expect("node")));
        let catalog = Arc::new(CognitionCatalog::new(node));
        Self { dir, store, catalog }
    }

    fn task(
        &self,
        descriptor: &NativeTaskDescriptor,
        input_ref: &ObjectRef,
    ) -> (TaskRecord, TaskInvocation) {
        let task_id = EntityId::new();
        let invocation_id = EntityId::new();
        let epoch = ControllerEpoch(1);
        let agent_id = EntityId::new();
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task { id: EntityId::new() },
            background: false,
            kind: descriptor.kind.clone(),
            definition_version: descriptor.definition_version,
            implementation_ref: descriptor.implementation_ref.clone(),
            input_ref: input_ref.clone(),
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos: serde_json::Map::new(),
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        self.store.commit(batch).expect("task commit");
        let invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        (task, invocation)
    }

    fn native(
        &self,
        task: TaskRecord,
        invocation: TaskInvocation,
        input: serde_json::Value,
    ) -> NativeTaskInvocation {
        let task_id = task.task_id;
        let mut context = CallContext::new_system(RuntimeGeneration::default());
        context.task_id = Some(task_id);
        context.invocation_id = Some(invocation.invocation_id);
        context.controller_epoch = invocation.epoch;
        NativeTaskInvocation {
            context,
            task_invocation: invocation,
            task,
            operation_id: EntityId::new(),
            authority: AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(0),
                decision_ref: EntityId::new(),
            },
            input,
            child_tools: None,
        }
    }

    fn input_ref(&self, input: &serde_json::Value) -> ObjectRef {
        self.store.cas.seal_object(
            &serde_json::to_vec(input).expect("input JSON"),
            "application/json", "native-cognition", "fixture-input", 1,
        ).expect("sealed input").object
    }

    fn result(store: &Store, operation_id: EntityId) -> serde_json::Value {
        let receipts = store.external_operation_evidence(operation_id).expect("evidence");
        let object = receipts.last().expect("query receipt")
            .normalized_ref.as_ref().expect("normalized result");
        serde_json::from_slice(&store.cas.read_object(&object.digest).expect("result bytes"))
            .expect("result JSON")
    }

    async fn execute(
        &self,
        handler: &dyn NativeTaskHandler,
        input: serde_json::Value,
    ) -> OperationRecord {
        let input_ref = self.input_ref(&input);
        let (task, invocation) = self.task(&handler.descriptor(), &input_ref);
        handler.execute(self.native(task, invocation, input)).await.expect("native execution")
    }

    async fn write_unit(&self, unit_id: &str) -> String {
        let observed = Digest::blake3(b"durable query fixture provenance");
        self.store.cas.seal_object(
            b"durable query fixture provenance", "text/plain", "fixture", "observation", 1,
        ).expect("provenance bytes");
        let write = CognitionWriteHandler::new(
            Arc::clone(&self.store), Arc::clone(&self.catalog), self.dir.join("cognition"), None,
        );
        let operation = self.execute(&write, serde_json::json!({
            "unit_id": unit_id,
            "revision": "1",
            "kind": "awareness",
            "title": "Query fixture",
            "capability_card": "observations retain their source",
            "body": "A retained observation describes its own source and scope.",
            "provenance_refs": ["fixture:native-query"],
            "derived_from": {
                "kind": "cas_object",
                "reference": "query fixture provenance",
                "digest": observed.0,
            },
        })).await;
        let result = Self::result(&self.store, operation.operation_id);
        assert_eq!(operation.outcome, Some(OperationOutcome::Succeeded), "{result}");
        result["artifact_digest"].as_str().expect("written artifact").to_string()
    }

    async fn observation(&self, unit_id: &str, bytes: &[u8]) {
        let artifact = self.write_unit(unit_id).await;
        self.catalog.register_observation(unit_id, "1", &Digest(artifact), "fixture-report", bytes)
            .expect("registered observation");
    }
}

#[tokio::test]
async fn evidence_public_input_selects_the_units_retained_observation() {
    let fixture = Fixture::new();
    let expected = b"the target installation retained these exact bytes";
    fixture.observation("awareness.query.target", expected).await;
    fixture.observation("awareness.query.other", b"another unit's unrelated observation").await;
    let handler = CognitionQueryHandler::evidence(
        Arc::clone(&fixture.store), Arc::clone(&fixture.catalog),
    );
    let operation = fixture.execute(&handler, serde_json::json!({
        "unit_id": "awareness.query.target",
    })).await;
    let result = Fixture::result(&fixture.store, operation.operation_id);
    assert_eq!(operation.outcome, Some(OperationOutcome::Succeeded), "{result}");
    assert_eq!(operation.effect_status, EffectStatus::None);
    assert_eq!(result["op"], "evidence");
    let observations = result["observations"].as_array().expect("observations");
    let facts: Vec<_> = observations.iter().map(|entry| (
        entry["digest"].as_str().expect("digest").to_string(),
        entry["kind"].as_str().expect("kind").to_string(),
        entry["bytes"].as_u64().expect("bytes"),
        entry["bytes_held"].as_bool().expect("held bytes"),
    )).collect();
    assert_eq!(facts, vec![(Digest::blake3(expected).0, "fixture-report".into(), expected.len() as u64, true)]);
}

#[tokio::test]
async fn list_public_pages_cover_the_installation_and_refuse_a_stale_revision() {
    let fixture = Fixture::new();
    let expected = ["awareness.query.alpha", "awareness.query.beta", "awareness.query.gamma"];
    for unit_id in expected {
        fixture.write_unit(unit_id).await;
    }
    let handler = CognitionQueryHandler::list(
        Arc::clone(&fixture.store), Arc::clone(&fixture.catalog),
    );
    let first = fixture.execute(&handler, serde_json::json!({"start": 0, "count": 1})).await;
    let mut page = Fixture::result(&fixture.store, first.operation_id);
    assert_eq!(first.outcome, Some(OperationOutcome::Succeeded), "{page}");
    let revision = page["catalog_revision"].as_u64().expect("installation revision");
    let mut starts = BTreeSet::new();
    let mut observed = Vec::new();
    loop {
        let start = page["start"].as_u64().expect("page start");
        assert!(starts.insert(start), "pagination revisited start {start}");
        for card in page["cards"].as_array().expect("capability cards") {
            let unit_id = card["unit_id"].as_str().expect("unit ID");
            if unit_id.starts_with("awareness.query.") {
                observed.push(unit_id.to_string());
            }
        }
        let Some(next_start) = page["next_start"].as_u64() else { break };
        assert!(next_start > start, "pagination did not advance");
        let next = fixture.execute(&handler, serde_json::json!({
            "start": next_start, "count": 1, "catalog_revision": revision,
        })).await;
        page = Fixture::result(&fixture.store, next.operation_id);
        assert_eq!(next.outcome, Some(OperationOutcome::Succeeded), "{page}");
    }
    observed.sort();
    assert_eq!(observed, expected);

    fixture.write_unit("awareness.query.new-version").await;
    let stale = fixture.execute(&handler, serde_json::json!({
        "start": 1, "count": 1, "catalog_revision": revision,
    })).await;
    let result = Fixture::result(&fixture.store, stale.operation_id);
    assert_eq!(stale.outcome, Some(OperationOutcome::Failed));
    assert_eq!(stale.effect_status, EffectStatus::None);
    assert_eq!(result["error"]["code"], "catalog_query_failed");
    assert!(result["error"]["message"].as_str().expect("failure reason")
        .contains("installation revision moved"), "{result}");
}

#[tokio::test]
async fn reopened_legacy_frozen_query_keeps_its_input_and_single_attempt() {
    let fixture = Fixture::new();
    let expected = b"a retained observation survives the Store reopen";
    fixture.observation("awareness.query.recovery", expected).await;
    let handler = CognitionQueryHandler::evidence(
        Arc::clone(&fixture.store), Arc::clone(&fixture.catalog),
    );
    // Historical v1 bytes: public decoding must not change the durable codec.
    let legacy_bytes = br#"{"op":"evidence","unit_id":"awareness.query.recovery"}"#;
    let input_ref = handler.seal_bytes(legacy_bytes, "cognition-query-frozen-input")
        .expect("old frozen input");
    let public = serde_json::json!({"unit_id": "awareness.query.recovery"});
    let frozen = handler.freeze(public).expect("public query");
    assert_eq!(handler.seal(&frozen, "cognition-query-frozen-input").expect("sealed query").digest,
        input_ref.digest);
    let (task, invocation) = fixture.task(&handler.descriptor(), &input_ref);
    let native = fixture.native(task.clone(), invocation, serde_json::Value::Null);
    let (mut operation, mut attempt) = handler.operation_parts(&native, input_ref.clone())
        .expect("durable query");
    assert!(fixture.store.arm_external_operation(
        &native.task_invocation, &mut operation, &mut attempt,
    ).expect("arm").is_none());
    assert_eq!(operation.state, OperationState::InFlight);
    assert!(fixture.store.external_operation_evidence(operation.operation_id)
        .expect("pre-crash evidence").is_empty());
    let operation_id = operation.operation_id;
    let attempt_id = attempt.attempt_id;
    let dir = fixture.dir.clone();
    drop(handler);
    drop(fixture);

    let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).expect("reopened Store"));
    let node = Arc::new(Mutex::new(Node::open(&dir.join("node")).expect("reopened Node")));
    let catalog = Arc::new(CognitionCatalog::new(node));
    let recovered = CognitionQueryHandler::evidence(Arc::clone(&store), catalog);
    let task = store.external_task(task.task_id).expect("reopened task");
    let resolved = recovered.reconcile(&task, operation_id).await.expect("reconcile");
    assert_eq!(resolved.state, OperationState::Resolved);
    assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(resolved.final_input_ref.digest, input_ref.digest);
    assert_eq!(store.cas.read_object(&resolved.final_input_ref.digest).expect("frozen bytes"), legacy_bytes);
    let result = Fixture::result(&store, operation_id);
    let observations = result["observations"].as_array().expect("retained observations");
    let digests: Vec<_> = observations.iter().map(|entry| entry["digest"].as_str().expect("digest"))
        .collect();
    assert_eq!(digests, vec![Digest::blake3(expected).0]);
    let receipt = store.external_operation_evidence(operation_id).expect("recovered evidence");
    assert_eq!(receipt.len(), 1);
    let receipt_id = receipt[0].receipt_id;
    assert_eq!(receipt[0].attempt_id, Some(attempt_id));

    recovered.reconcile(&task, operation_id).await.expect("repeated reconciliation");
    assert_eq!(store.external_operation_attempt(operation_id).expect("original attempt").attempt_id,
        attempt_id);
    assert_eq!(store.external_operation_attempts(operation_id).expect("attempts").len(), 1);
    let repeated = store.external_operation_evidence(operation_id).expect("repeated evidence");
    assert_eq!(repeated.len(), 1);
    assert_eq!(repeated[0].receipt_id, receipt_id);
}

#[tokio::test]
async fn invalid_public_shapes_do_not_arm_or_mutate_the_task() {
    let fixture = Fixture::new();
    for (kind, input) in [
        ("evidence", serde_json::json!({"op": "list", "unit_id": "awareness.query.target"})),
        ("evidence", serde_json::json!({"unit_id": "awareness.query.target", "start": 0})),
        ("list", serde_json::json!({"unit_id": "awareness.query.target"})),
        ("evidence", serde_json::json!(["awareness.query.target"])),
    ] {
        let handler = if kind == "list" {
            CognitionQueryHandler::list(Arc::clone(&fixture.store), Arc::clone(&fixture.catalog))
        } else {
            CognitionQueryHandler::evidence(Arc::clone(&fixture.store), Arc::clone(&fixture.catalog))
        };
        let input_ref = fixture.input_ref(&input);
        let (task, invocation) = fixture.task(&handler.descriptor(), &input_ref);
        let task_id = task.task_id;
        let before = serde_json::to_value(fixture.store.external_task(task_id).expect("before task"))
            .expect("task JSON");
        let native = fixture.native(task, invocation, input);
        let operation_id = native.operation_id;
        assert!(handler.execute(native).await.is_err(), "accepted invalid {kind} input");
        assert!(fixture.store.external_operation(operation_id).is_err(), "armed invalid input");
        assert!(fixture.store.external_operation_attempts(operation_id).expect("attempts").is_empty());
        assert!(fixture.store.external_operation_evidence(operation_id).expect("evidence").is_empty());
        let after = serde_json::to_value(fixture.store.external_task(task_id).expect("after task"))
            .expect("task JSON");
        assert_eq!(after, before, "invalid input changed the running task");
    }
}
