//! Nine independent behaviors across four consumer tests:
//! (1) interrupted version publication, (2) exact code/content rollback,
//! (3) new business data retained, (4) real HTTP→native→WASI→durable result,
//! (5) lost-response/reopen replay, (6) conflicting identity, (7) stale page,
//! (8) unknown original-attempt recovery, (9) unavailable runtime admission.
use super::*;
use crate::playground_backend::{ArtifactActor, BackendActionRequest, PlaygroundBackendHandler};
use anyhow::Context;
use axum::{body::to_bytes, http::Request};
use shenko_kernel::{FiberHandle, Supervisor};
use shenko_subject::{NativeChildPlugin, NativeTaskHandlerPlugin};
use shenko_kernel::task::NativeTaskHandler;
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
    let head = SubjectBranchHead { branch_origin: None,
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
fn interrupted_versions_and_exact_rollback_keep_new_business_data() -> anyhow::Result<()> {
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
    assert_eq!(pg.get(&id)?.context("artifact exists")?.revision, newer.revision);
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
    Ok(())
}

/// A reader's action on a work is bound to the revision they saw and to the
/// request that named it (acceptance PG07).
///
/// A work that moved on refuses the stale action instead of applying it to
/// whatever is there now; the request id rides the durable transition, so a
/// repeated submit is recognisable as the same request rather than a new one.
#[test]
fn a_readers_action_is_bound_to_a_revision_and_a_request() -> anyhow::Result<()> {
    let (root, store, _) = fixture("reader-action");
    let pg_root = root.join("playground");
    let pg = PlaygroundState::open(pg_root.clone(), store.clone()).unwrap();
    let id = EntityId::new();
    pg.put(id, "first".into(), "text/plain".into(), b"first body").unwrap();
    let first = pg.get(&id)?.context("artifact exists")?;

    // The complete mutation, not a separate preflight, binds the revision.
    pg.stage_artifact(id, Some(first.revision), Some("reader-first")).unwrap();
    pg.put(id, "second".into(), "text/plain".into(), b"second body").unwrap();
    let stale = first.revision;
    let before_stale = pg.durable_record(id).unwrap().unwrap();
    pg.stage_artifact(id, Some(stale), Some("reader-stale")).expect_err("the work moved on");
    let after_stale = pg.durable_record(id).unwrap().unwrap();
    assert_eq!(after_stale.updated_commit, before_stale.updated_commit);
    assert_eq!(after_stale.candidate_manifest_refs, before_stale.candidate_manifest_refs);

    // A stage with the current revision succeeds, and the durable transition
    // names the request that asked for it.
    let request_id = "reader-request-1";
    let revision = pg.get(&id)?.context("artifact exists")?.revision;
    pg.stage_artifact(id, Some(revision), Some(request_id)).unwrap();
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
    let before = record.updated_commit;
    pg.stage_artifact(id, Some(revision), Some(request_id)).unwrap();
    let after = pg.durable_record(id).unwrap().unwrap().updated_commit;
    assert_eq!(
        before, after,
        "a repeated submit produces no second effect: the durable row is not written again"
    );
    drop(pg);
    drop(store);
    fs::remove_dir_all(root).unwrap();
    Ok(())
}

#[test]
fn concurrent_content_stage_and_publish_preserve_committed_facts() {
    let (root, store, _) = fixture("concurrent-selection");
    let pg = PlaygroundState::open(root.join("playground"), store.clone()).unwrap();
    let id = EntityId::new();
    pg.put(id, "first".into(), "text/plain".into(), b"first").unwrap();

    let start = std::sync::Barrier::new(3);
    let (staged, second) = std::thread::scope(|scope| {
        let stage = scope.spawn(|| {
            start.wait();
            pg.stage_artifact(id, None, Some("concurrent-stage")).unwrap()
        });
        let put = scope.spawn(|| {
            start.wait();
            pg.put(id, "second".into(), "text/plain".into(), b"second").unwrap()
        });
        start.wait();
        (stage.join().unwrap(), put.join().unwrap())
    });
    let candidate = staged.candidate_manifest_refs.last().unwrap().clone();
    let record = pg.durable_record(id).unwrap().unwrap();
    let version: ArtifactVersion = serde_json::from_slice(&store.cas.read_object(&record.state_ref.digest).unwrap()).unwrap();
    assert_eq!(version.revision, second.revision);
    assert_eq!(version.content_ref, second.content_ref);
    assert_eq!(record.title, second.title);
    assert!(record.candidate_manifest_refs.contains(&candidate));

    let start = std::sync::Barrier::new(3);
    let third = std::thread::scope(|scope| {
        let publish = scope.spawn(|| {
            start.wait();
            pg.publish_artifact(id, &candidate.digest, None, Some("concurrent-publish")).unwrap()
        });
        let put = scope.spawn(|| {
            start.wait();
            pg.put(id, "third".into(), "text/plain".into(), b"third").unwrap()
        });
        start.wait();
        publish.join().unwrap();
        put.join().unwrap()
    });
    let record = pg.durable_record(id).unwrap().unwrap();
    let version: ArtifactVersion = serde_json::from_slice(&store.cas.read_object(&record.state_ref.digest).unwrap()).unwrap();
    assert_eq!(version.revision, third.revision);
    assert_eq!(version.content_ref, third.content_ref);
    assert_eq!(record.title, third.title);
    assert_eq!(record.active_manifest_ref.as_ref(), Some(&candidate));
    assert!(record.candidate_manifest_refs.contains(&candidate));
}

#[test]
fn public_action_executes_wasi_and_reopen_replays_original_receipt() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {

    let (root, store, agent) = fixture("http");
    let pg_root = root.join("playground");
    let host = runtime(store.clone(), pg_root.clone()).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1); // later wakes fill it; facts stay durable
    let shared = Arc::new(PlaygroundState::open(pg_root.clone(), store.clone())?);
    let pg = shared.as_ref().clone(); // a handler already holds this clone
    let assembled = crate::server::UiServer::new(0)
        .with_store(store.clone()).with_native(host.native.clone()).with_inbox(tx)
        .with_playground_state(shared.clone()).configured_playground()?.context("shared playground is assembled")?;
    assert!(Arc::ptr_eq(&shared, &assembled));
    let id = EntityId::new();
    let app = router(pg.clone());
    assert_eq!(http(&app, "PUT", &format!("/playground/{id}"), b"binary counter application".to_vec()).await.0, StatusCode::OK);
    let wake = rx.try_recv().context("server assembly attaches the shared subject inbox")?;
    assert_eq!(wake.payload["artifact_id"], json!(id));
    drop(assembled); drop(shared);
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
    // Reader responses can be lost independently of the backend response.
    // Publish A, adopt B later, roll back once, then author a newer draft.
    // Replaying either earlier request after reopen must report its exact
    // original result without selecting an older artifact state again.
    let stage_uri = format!("/playground/{id}/stage");
    let publish_uri = format!("/playground/{id}/publish");
    let rollback_uri = format!("/playground/{id}/rollback");
    let (status, staged_a) = http(&app, "POST", &stage_uri, serde_json::to_vec(&json!({
        "revision": record.revision, "request_id": "reader-stage-a"}))?).await;
    assert_eq!(status, StatusCode::OK, "{staged_a}");
    let candidate_a = staged_a["record"]["candidate_manifest_refs"][0]["digest"].clone();
    let newer = pg.put(id, "candidate B".into(), "text/plain".into(), b"candidate B")?;
    let (status, staged_b) = http(&app, "POST", &stage_uri, serde_json::to_vec(&json!({
        "revision": newer.revision, "request_id": "reader-stage-b"}))?).await;
    assert_eq!(status, StatusCode::OK, "{staged_b}");
    let candidate_b = staged_b["record"]["candidate_manifest_refs"][1]["digest"].clone();
    let publish_a = json!({"revision": newer.revision, "request_id": "reader-publish-a", "manifest_digest": candidate_a});
    let (status, published_a) = http(&app, "POST", &publish_uri, serde_json::to_vec(&publish_a)?).await;
    assert_eq!(status, StatusCode::OK, "{published_a}");
    let publish_b = json!({"revision": newer.revision, "request_id": "reader-publish-b", "manifest_digest": candidate_b});
    let (status, published_b) = http(&app, "POST", &publish_uri, serde_json::to_vec(&publish_b)?).await;
    assert_eq!(status, StatusCode::OK, "{published_b}");
    let rollback_request = json!({"revision": record.revision, "request_id": "reader-rollback"});
    let (status, rolled_back) = http(&app, "POST", &rollback_uri, serde_json::to_vec(&rollback_request)?).await;
    assert_eq!(status, StatusCode::OK, "{rolled_back}");
    pg.put(id, "newer after rollback".into(), "text/plain".into(), b"keep this draft")?;
    let reader_head = pg.durable_record(id)?.context("current reader head")?;
    let reader_history = pg.revisions(id)?;
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
    let app = router(pg.clone());
    let (status, published_replay) = http(&app, "POST", &publish_uri, serde_json::to_vec(&publish_a)?).await;
    assert_eq!(status, StatusCode::OK, "{published_replay}");
    assert_eq!(published_replay["replayed"], true);
    assert_eq!(published_replay["record"], published_a["record"]);
    assert_eq!(published_replay["receipt_ref"], published_a["receipt_ref"]);
    let (status, rollback_replay) = http(&app, "POST", &rollback_uri, serde_json::to_vec(&rollback_request)?).await;
    assert_eq!(status, StatusCode::OK, "{rollback_replay}");
    assert_eq!(rollback_replay["replayed"], true);
    assert_eq!(rollback_replay["version"], rolled_back["version"]);
    assert_eq!(rollback_replay["receipt_ref"], rolled_back["receipt_ref"]);
    let mut changed_publish = publish_a;
    changed_publish["manifest_digest"] = candidate_b;
    assert_eq!(http(&app, "POST", &publish_uri, serde_json::to_vec(&changed_publish)?).await.0, StatusCode::CONFLICT);
    let mut changed_rollback = rollback_request;
    changed_rollback["revision"] = json!(newer.revision);
    assert_eq!(http(&app, "POST", &rollback_uri, serde_json::to_vec(&changed_rollback)?).await.0, StatusCode::CONFLICT);
    assert_eq!(pg.durable_record(id)?.context("retained reader head")?.updated_commit, reader_head.updated_commit);
    assert_eq!(pg.revisions(id)?, reader_history);
    assert_eq!(pg.read_content(&pg.get(&id)?.context("current artifact")?)?, b"keep this draft");
    assert_eq!(fs::read(pg.artifact_dir(id).join("state/counter.bin"))?, vec![1]);
    let mut conflict = action.clone();
    conflict.input = json!({"amount":2});
    assert!(matches!(pg.invoke_action(id, conflict, &actor), Err(ActionError::Conflict)));
    pg.bind_backend(id, b"(module (func (export \"_start\")))").unwrap();
    let mut old_page = action;
    old_page.request_id = "new-click-from-old-page".into();
    assert!(matches!(pg.invoke_action(id, old_page, &actor), Err(ActionError::Stale)));
    // A real unavailable CAS head is an availability error, never absence
    // or an empty successful list. Restore the owned fixture after observing it.
    let head = pg.durable_record(id)?.context("head before closure fault")?;
    let head_path = store.cas.verified_object_path(&head.state_ref)?;
    let head_bytes = fs::read(&head_path)?;
    fs::write(&head_path, b"corrupt fixture head")?;
    assert_eq!(http(&app, "GET", &format!("/playground/{id}"), Vec::new()).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(http(&app, "GET", &format!("/playground/{id}/versions"), Vec::new()).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    let surface = crate::api::router(crate::api::ApiState::new().with_store(store.clone()).with_playground(pg.clone()));
    let (_, unavailable) = http(&surface, "GET", "/api/playground", Vec::new()).await;
    assert_eq!(unavailable["available"], false);
    assert!(unavailable["error"].is_string());
    fs::write(head_path, head_bytes)?;
    drop(surface); drop(app);
    host.supervisor.stop().await.unwrap();
    drop(pg); drop(host); drop(store);
    fs::remove_dir_all(root).unwrap();
    Ok(())
    })
}

#[tokio::test]
async fn unknown_guest_attempt_survives_reopen_without_rearm() {
    let (root, store, _) = fixture("unknown");
    let pg_root = root.join("playground");
    let host = runtime(store.clone(), pg_root.clone()).await;
    let pg = PlaygroundState::open(pg_root.clone(), store.clone()).unwrap().with_native(host.native.clone());
    let id = EntityId::new();
    pg.put(id, "interrupted".into(), "text/plain".into(), b"original").unwrap();
    // The real guest changes business data before waiting. Teardown must
    // cancel and join this guest, preserving the original Unknown evidence
    // and its writes. A ten-second guest clock fallback bounds a regression.
    let trapped = std::str::from_utf8(COUNTER).unwrap()
        .replacen("(module", "(module (import \"wasi_snapshot_preview1\" \"clock_time_get\" (func $clock (param i32 i64 i32) (result i32)))", 1)
        .replace("(then (call $exit (i32.const 45))))))", r#"(then (call $exit (i32.const 45))))
        (drop (call $clock (i32.const 1) (i64.const 0) (i32.const 320)))
        (i64.store (i32.const 328) (i64.add (i64.load (i32.const 320)) (i64.const 10000000000)))
        (loop $waiting
          (drop (call $clock (i32.const 1) (i64.const 0) (i32.const 320)))
          (br_if $waiting (i64.lt_u (i64.load (i32.const 320)) (i64.load (i32.const 328)))))
        unreachable))"#);
    let binding = pg.bind_backend(id, trapped.as_bytes()).unwrap();
    let action = request(&binding, "unknown-original");
    let actor = ArtifactActor::Participant;
    pg.invoke_action(id, action.clone(), &actor).unwrap();
    drive_until(&host.native, || pg.artifact_dir(id).join("state/counter.bin").is_file()).await;
    tokio::time::timeout(std::time::Duration::from_secs(3), host.supervisor.stop()).await
        .expect("teardown must signal and join the actual guest before its fallback clock").unwrap();
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

/// A work's page is a trusted extension: served same-origin without a
/// sandbox, its declared type pinned and its revision named, so the safety
/// rope is rollback to a named revision.
#[test]
fn a_work_page_is_served_as_a_trusted_revision() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let (root, store, _) = fixture("trusted");
            let pg = PlaygroundState::open(root.join("playground"), store)?;
            let id = EntityId::new();
            pg.put(id, "page".into(), "text/html".into(), b"<script>fetch('/api/self')</script>")?;
            let response = router(pg).oneshot(Request::builder().uri(format!("/playground/{id}")).body(Body::empty())?)
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().get(header::CONTENT_SECURITY_POLICY).is_none(), "trusted by default, not sandboxed");
            assert_eq!(response.headers().get(header::CONTENT_TYPE).context("work page has no content type")?.to_str()?, "text/html");
            assert_eq!(response.headers().get(header::X_CONTENT_TYPE_OPTIONS).context("work page has no nosniff header")?.to_str()?, "nosniff");
            assert_eq!(response.headers().get("x-artifact-revision").context("work page has no revision header")?.to_str()?, "1");
            Ok(())
        })
}

/// A published work lens shows the adopted page, not a later draft head; with
/// nothing adopted there is no adopted page.
#[tokio::test]
async fn the_adopted_page_is_served_until_a_new_publication() -> anyhow::Result<()> {
    let (root, store, _) = fixture("adopted");
    let pg = PlaygroundState::open(root.join("playground"), store)?;
    let id = EntityId::new();
    pg.put(id, "page".into(), "text/html".into(), b"<p>r1</p>")?;
    let app = router(pg.clone());
    let get = |uri: String| -> anyhow::Result<_> {
        Ok(app.clone().oneshot(Request::builder().uri(uri).body(Body::empty())?))
    };
    assert_eq!(get(format!("/playground/{id}/adopted"))?.await?.status(), StatusCode::NOT_FOUND);
    let staged = pg.stage_artifact(id, Some(1), Some("stage-r1"))?;
    pg.publish_artifact(id, &staged.candidate_manifest_refs.last().context("stage produced no candidate manifest")?.digest, Some(1), Some("publish-r1"))?;
    pg.put(id, "page".into(), "text/html".into(), b"<p>r2 draft</p>")?;
    let adopted = get(format!("/playground/{id}/adopted"))?.await?;
    assert_eq!(adopted.headers().get("x-artifact-revision").context("adopted page has no revision header")?.to_str()?, "1");
    assert_eq!(to_bytes(adopted.into_body(), 1024).await?.as_ref(), b"<p>r1</p>");
    let head = get(format!("/playground/{id}"))?.await?;
    assert_eq!(head.headers().get("x-artifact-revision").context("draft page has no revision header")?.to_str()?, "2", "the draft head is still readable as such");
    Ok(())
}
