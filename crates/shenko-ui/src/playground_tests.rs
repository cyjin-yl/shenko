//! Nine independent behaviors across four consumer tests:
//! (1) interrupted version publication, (2) exact code/content rollback,
//! (3) new business data retained, (4) real HTTP→native→WASI→durable result,
//! (5) lost-response/reopen replay, (6) conflicting identity, (7) stale page,
//! (8) unknown original-attempt recovery, (9) unavailable runtime admission.
use super::*;
use crate::playground_backend::{ArtifactActor, BackendActionRequest, PlaygroundBackendHandler};
use axum::{body::to_bytes, http::Request};
use shenko_kernel::{FiberHandle, Supervisor};
use shenko_subject::{NativeChildPlugin, NativeTaskHandler, NativeTaskHandlerPlugin};
use shenko_types::{commit::CommitRef, ids::*, operation::OperationState, subject::*};
use tower::ServiceExt;

const COUNTER: &[u8] = include_bytes!("../tests/fixtures/playground-counter.wat");

fn fixture(label: &str) -> (PathBuf, Arc<Store>, EntityId) {
    let root = std::env::temp_dir().join(format!("shenko-playground-{label}-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas")).unwrap());
    let agent = EntityId::new();
    let branch = BranchId::new();
    let first = EntityId::new();
    let genesis_id = EntityId::new();
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let object = store.cas.seal_object(b"seed", "application/json", "fixture", "seed", 1).unwrap().object;
    let genesis = SubjectGenesis {
        genesis_id, subject_id: agent, bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(), initial_profile_ref: object.clone(), home_manifest_ref: object.clone(),
        bootstrap_event_ref: object, created_commit: zero,
    };
    let head = SubjectBranchHead {
        agent_id: agent, branch_id: branch.clone(), subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef { ledger_id: store.ledger.ledger_id(), genesis_id }, committed_step_no: 0,
        committed_step_id: None, committed_at: zero, next_step_id: first, mode: SubjectMode::Active,
        mode_reason_ref: None, record_revision: RecordRevision(0),
    };
    let step = SubjectStep {
        step_id: first, branch_id: branch, step_no: 1, expected_parent_commit: zero,
        controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration::default(), state: SubjectStepState::Planned,
        input_plan_ref: None, context_revision: None, active_request_id: None, active_attempt_id: None,
        model_result_ref: None, model_outcome_ref: None, decision_ref: None, commit_ref: None,
        retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None,
    };
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id, agent, genesis.seed_manifest_ref.clone(), zero,
    );
    store.initialize_genesis(&genesis, &head, &step, &root_task).unwrap();
    store.start_subject_dispatcher().unwrap();
    (root, store, agent)
}

struct Runtime {
    supervisor: Supervisor,
    _child: FiberHandle,
    _handler: FiberHandle,
    native: Arc<NativeChildService>,
}
async fn runtime(store: Arc<Store>, root: PathBuf) -> Runtime {
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let child = supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
    let handler: Arc<dyn NativeTaskHandler> = Arc::new(PlaygroundBackendHandler::new(store, root));
    let handler = supervisor.spawn(NativeTaskHandlerPlugin, handler).await.unwrap();
    let native = supervisor.context().try_service::<NativeChildService>().unwrap();
    native.recover().unwrap();
    Runtime { supervisor, _child: child, _handler: handler, native }
}

/// `run_once` claims candidates and returns after spawning their managed
/// workers; it does not await the guest. Drive bounded passes until the
/// durable receipt reaches the state this test is about.
async fn drive_until(native: &Arc<NativeChildService>, done: impl Fn() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            native.run_once().await.unwrap();
            if done() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("playground action did not converge");
}

fn request(record: &ArtifactVersion, key: &str) -> BackendActionRequest {
    BackendActionRequest { request_id: key.into(), backend_digest: record.backend.as_ref().unwrap().module_digest.clone(),
        action: "increment".into(), input: json!({"amount":1}) }
}
async fn http(app: &Router, method: &str, uri: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(Request::builder().method(method).uri(uri)
        .header("content-type", "application/json").body(Body::from(body)).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[test]
fn interrupted_versions_and_exact_rollback_keep_new_business_data() {
    let (root, store, _) = fixture("versions");
    let pg_root = root.join("playground");
    let pg = PlaygroundState::open(pg_root.clone(), store.clone()).unwrap();
    let id = EntityId::new();
    pg.put(id, "binary original".into(), "application/octet-stream".into(), b"\0\x01original").unwrap();
    let original = pg.bind_backend(id, COUNTER).unwrap();
    pg.put(id, "new UI".into(), "text/html".into(), b"<p>new</p>").unwrap();
    let newer = pg.bind_backend(id, b"(module (func (export \"_start\")))").unwrap();
    fs::create_dir_all(pg.artifact_dir(id).join("state")).unwrap();
    fs::write(pg.artifact_dir(id).join("state/new-user-data.bin"), b"\xffnew fact").unwrap();
    let orphan = newer.revision + 1;
    write_immutable(&pg.version_path(id, orphan), b"not published").unwrap();
    drop(pg);
    let pg = PlaygroundState::open(pg_root, store.clone()).unwrap();
    assert_eq!(pg.get(&id).unwrap().revision, newer.revision);
    assert!(!pg.revisions(id).unwrap().contains(&orphan));
    let restored = pg.rollback(id, original.revision).unwrap();
    assert!(restored.revision > orphan);
    assert_eq!(restored.title, original.title);
    assert_eq!(restored.mime, original.mime);
    assert_eq!(restored.backend, original.backend);
    assert_eq!(pg.read_content(&restored).unwrap(), b"\0\x01original");
    assert_eq!(fs::read(pg.version_path(id, orphan)).unwrap(), b"not published");
    assert_eq!(fs::read(pg.artifact_dir(id).join("state/new-user-data.bin")).unwrap(), b"\xffnew fact");
    // The notification is the exact stable full InboxEvent, already committed;
    // reopening and retrying import cannot manufacture a new stimulus.
    let event = pg.change_event(&restored);
    assert_eq!(store.admit_inbox_event(event.event_id, pg.agent_id, "artifact_changed", "playground",
        &serde_json::to_value(&event).unwrap()).unwrap(), shenko_store::InboxAdmission::Replay);
    drop(pg); drop(store);
    fs::remove_dir_all(root).unwrap();
}

/// A reader's action on a work is bound to the revision they saw and to the
/// request that named it (SPEC §57.6, acceptance PG07).
///
/// A work that moved on refuses the stale action instead of applying it to
/// whatever is there now; the request id rides the durable transition, so a
/// repeated submit is recognisable as the same request rather than a new one.
#[test]
fn a_readers_action_is_bound_to_a_revision_and_a_request() {
    let (root, store, _) = fixture("reader-action");
    let pg_root = root.join("playground");
    let pg = PlaygroundState::open(pg_root.clone(), store.clone()).unwrap();
    let id = EntityId::new();
    pg.put(id, "first".into(), "text/plain".into(), b"first body").unwrap();
    let first = pg.get(&id).unwrap();

    // The action names the revision the reader saw.
    assert!(
        pg.check_revision(id, first.revision).is_ok(),
        "the action acts on the revision that is current"
    );
    pg.put(id, "second".into(), "text/plain".into(), b"second body").unwrap();
    let stale = first.revision;
    let refusal = pg.check_revision(id, stale).expect_err("the work moved on");
    assert!(
        refusal.to_string().contains("revision"),
        "the refusal names the revision the action named: {refusal:#}"
    );

    // A stage with the current revision succeeds, and the durable transition
    // names the request that asked for it.
    let request_id = "reader-request-1";
    pg.stage_artifact(id, Some(request_id)).unwrap();
    let record = pg.durable_record(id).unwrap().expect("the work has a durable record");
    let staged = pg.state_event(&record, "stage", Some(request_id));
    assert_eq!(
        staged.payload.get("request_id").and_then(|value| value.as_str()),
        Some(request_id),
        "the durable transition names the request that asked for it"
    );
    // The transition's IDENTITY does not depend on the request: the same
    // resulting state reached by the same cause is the same fact, so a
    // repeated submit is the same transition rather than a second one.
    let repeated = pg.state_event(&record, "stage", Some(request_id));
    assert_eq!(staged.event_id, repeated.event_id);

    // A repeated submit of the same proposal produces no second transition:
    // the stage is the same fact, so the durable row is not written again.
    let before = record.state_revision.clone();
    pg.stage_artifact(id, Some(request_id)).unwrap();
    let after = pg.durable_record(id).unwrap().unwrap().state_revision;
    assert_eq!(
        before, after,
        "a repeated submit produces no second effect: the durable row is not written again"
    );
    drop(pg);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn public_action_executes_wasi_and_reopen_replays_original_receipt() {
    let (root, store, agent) = fixture("http");
    let pg_root = root.join("playground");
    let host = runtime(store.clone(), pg_root.clone()).await;
    let (tx, _rx) = tokio::sync::mpsc::channel(1); // later wakes fill it; facts stay durable
    let pg = PlaygroundState::open(pg_root.clone(), store.clone()).unwrap().with_native(host.native.clone()).with_inbox(tx);
    let id = EntityId::new();
    let app = router(pg.clone());
    assert_eq!(http(&app, "PUT", &format!("/playground/{id}"), b"binary counter application".to_vec()).await.0, StatusCode::OK);
    let (status, bound) = http(&app, "PUT", &format!("/playground/{id}/backend"), COUNTER.to_vec()).await;
    assert_eq!(status, StatusCode::OK);
    let record: ArtifactVersion = serde_json::from_value(bound["artifact"].clone()).unwrap();
    let action = request(&record, "lost-response");
    let uri = format!("/playground/{id}/backend/action");
    let (status, accepted) = http(&app, "POST", &uri, serde_json::to_vec(&action).unwrap()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let task_id: EntityId = serde_json::from_value(accepted["receipt"]["task_id"].clone()).unwrap();
    drive_until(&host.native, || {
        matches!(pg.action_status(id, "lost-response"), Ok(receipt) if receipt.state == "executed")
    })
    .await;
    let (_, result) = http(&app, "GET", &format!("{uri}?request_id=lost-response"), Vec::new()).await;
    assert_eq!(result["receipt"]["state"], "executed");
    assert_eq!(result["receipt"]["result"]["result"]["output"], json!({"ok":true}));
    assert_eq!(result["receipt"]["operation"]["outcome"], "succeeded");
    assert_eq!(result["receipt"]["result"]["result"]["exit_code"], 0);
    assert_eq!(result["receipt"]["result"]["result"]["change_count"], 1);
    assert_eq!(fs::read(pg.artifact_dir(id).join("state/counter.bin")).unwrap(), vec![1]);
    let ready = store.ready_native_results(agent, 10).unwrap();
    assert!(ready.iter().any(|result| result.task_id == task_id && result.result.as_ref().unwrap()["state_after"].is_object()));
    let operation_id: EntityId = serde_json::from_value(result["receipt"]["operation"]["operation_id"].clone()).unwrap();
    host.supervisor.stop().await.unwrap();
    drop(app); drop(pg); drop(host); drop(store);

    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas")).unwrap());
    store.start_subject_dispatcher().unwrap();
    let host = runtime(store.clone(), pg_root.clone()).await;
    let pg = PlaygroundState::open(pg_root, store.clone()).unwrap().with_native(host.native.clone());
    let actor = ArtifactActor::Participant;
    let replay = pg.invoke_action(id, action.clone(), &actor).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.operation.unwrap().operation_id, operation_id);
    host.native.run_once().await.unwrap();
    assert_eq!(store.external_operation_attempts(operation_id).unwrap().len(), 1);
    assert_eq!(fs::read(pg.artifact_dir(id).join("state/counter.bin")).unwrap(), vec![1]);
    let mut conflict = action.clone();
    conflict.input = json!({"amount":2});
    assert!(matches!(pg.invoke_action(id, conflict, &actor), Err(ActionError::Conflict)));
    pg.bind_backend(id, b"(module (func (export \"_start\")))").unwrap();
    let mut old_page = action;
    old_page.request_id = "new-click-from-old-page".into();
    assert!(matches!(pg.invoke_action(id, old_page, &actor), Err(ActionError::Stale)));
    host.supervisor.stop().await.unwrap();
    drop(pg); drop(host); drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unknown_guest_attempt_survives_reopen_without_rearm() {
    let (root, store, _) = fixture("unknown");
    let pg_root = root.join("playground");
    let host = runtime(store.clone(), pg_root.clone()).await;
    let pg = PlaygroundState::open(pg_root.clone(), store.clone()).unwrap().with_native(host.native.clone());
    let id = EntityId::new();
    pg.put(id, "trap".into(), "text/plain".into(), b"original").unwrap();
    // The real counter mutates binary business data and writes stdout before
    // trapping; unknown must not imply that its already-observed writes vanished.
    let trapped = std::str::from_utf8(COUNTER).unwrap().replace("(then (call $exit (i32.const 45))))))",
        "(then (call $exit (i32.const 45)))) unreachable))");
    let binding = pg.bind_backend(id, trapped.as_bytes()).unwrap();
    let action = request(&binding, "unknown-original");
    let actor = ArtifactActor::Participant;
    pg.invoke_action(id, action.clone(), &actor).unwrap();
    drive_until(&host.native, || {
        matches!(pg.action_status(id, "unknown-original"), Ok(receipt) if receipt.state == "unknown")
    })
    .await;
    let receipt = pg.action_status(id, "unknown-original").unwrap();
    assert_eq!(receipt.state, "unknown");
    assert_eq!(receipt.operation.as_ref().unwrap().state, OperationState::Reconciling);
    let attempt = receipt.attempt.unwrap();
    assert!(receipt.evidence.is_some());
    assert_eq!(fs::read(pg.artifact_dir(id).join("state/counter.bin")).unwrap(), vec![1]);
    host.supervisor.stop().await.unwrap();
    drop(pg); drop(host); drop(store);
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas")).unwrap());
    store.start_subject_dispatcher().unwrap();
    let host = runtime(store.clone(), pg_root.clone()).await;
    let pg = PlaygroundState::open(pg_root, store.clone()).unwrap().with_native(host.native.clone());
    pg.invoke_action(id, action, &actor).unwrap();
    host.native.run_once().await.unwrap();
    let receipt = pg.action_status(id, "unknown-original").unwrap();
    assert_eq!(receipt.attempt.unwrap().attempt_id, attempt.attempt_id);
    assert_eq!(store.external_operation_attempts(attempt.operation_id).unwrap().len(), 1);
    assert_eq!(fs::read(pg.artifact_dir(id).join("state/counter.bin")).unwrap(), vec![1]);
    host.supervisor.stop().await.unwrap();
    drop(pg); drop(host); drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unavailable_runtime_is_rejected_before_durable_task_or_guest_effect() {
    let (root, store, _) = fixture("unavailable");
    let pg = PlaygroundState::open(root.join("playground"), store.clone()).unwrap();
    let id = EntityId::new();
    pg.put(id, "application".into(), "text/plain".into(), b"original").unwrap();
    let binding = pg.bind_backend(id, COUNTER).unwrap();
    let actor = ArtifactActor::Participant;
    assert!(matches!(pg.invoke_action(id, request(&binding, "no-runtime"), &actor), Err(ActionError::Unavailable(_))));
    assert!(store.native_task_candidates_after(None, 10).unwrap().is_empty());
    assert!(!pg.artifact_dir(id).join("state/counter.bin").exists());
    drop(pg); drop(store);
    fs::remove_dir_all(root).unwrap();
}
