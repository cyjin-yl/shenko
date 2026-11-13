//! Real Store/parser/broker/HTTP mechanics with scripted loopback responses.
//! These are not live-model behavior or provider-capacity evidence.
use super::{commit_output, execute_step, fixture, record_result, state};
use crate::{ModelChildHandler, NativeChildService};
use shenko_broker::{broker::{Broker, BrokerHandle}, local_provider::LocalModelProvider};
use shenko_store::{Store, external::MODEL_CHILD_STEP_OPERATION};
use shenko_types::{ids::*, operation::OperationState, task::*};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}};

async fn read_wire(socket: &mut TcpStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "loopback request ended before its body");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let len: usize = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
            }).unwrap();
            if bytes.len() >= end + 4 + len {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + len]).unwrap();
            }
        }
    }
}

async fn reply(socket: &mut TcpStream, model: &str) {
    let raw = serde_json::json!({
        "id":"scripted-loopback-result", "choices":[{"message":{"role":"assistant",
            "content":format!("scripted answer for {model}")},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":12,"completion_tokens":7},
    }).to_string();
    socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes()).await.unwrap();
    socket.shutdown().await.unwrap();
}

fn delegation(model: &str, prompt: &str, goal: &str, max_tokens: u64) -> String {
    format!("[delegate] {}", serde_json::json!({"goal":goal,"system_prompt":prompt,"model":model,"max_tokens":max_tokens}))
}

fn service(store: &Arc<Store>, handle: BrokerHandle, generation: RuntimeGeneration) -> Arc<NativeChildService> {
    let service = Arc::new(NativeChildService::new(Arc::clone(store), generation));
    service.install(Arc::new(ModelChildHandler::new(Arc::clone(store), handle, cordis_core::Context::new()).unwrap())).unwrap();
    service.recover().unwrap();
    service
}

/// The one durable model step of a child run. Every model step of a finite
/// child is its own operation on the shared inference ledger.
fn child_step(store: &Store, task_id: EntityId) -> shenko_types::operation::OperationRecord {
    store
        .external_task_operations(task_id, MODEL_CHILD_STEP_OPERATION)
        .expect("the child run records its model steps")
        .pop()
        .expect("the child run has at least one model step")
}

/// A terminal provider refusal is a Failed generation, never an empty
/// successful run. The step keeps its original E1 and the run-result
/// operation settles Failed with the refusal as its durable record.
#[tokio::test]
async fn a_rejected_generation_settles_the_run_as_failed_with_its_original_e1() {
    for truncated in [false, true] {
        assert_failed_generation(truncated).await;
    }
}

async fn assert_failed_generation(truncated: bool) {
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let wire = read_wire(&mut socket).await;
            wires_tx.send(wire).unwrap();
            let (status, body) = if truncated {
                ("200 OK", serde_json::json!({"choices":[{"message":{"role":"assistant",
                    "content":"[reply] incomplete answer must not publish"},"finish_reason":"length"}]}).to_string())
            } else {
                ("404 Not Found", serde_json::json!({"error":"no such model"}).to_string())
            };
            socket.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let spec = serde_json::json!({"model":"rejected-child", "system_prompt":"Report only observed facts.",
        "goal":"Return this finite review.", "max_tokens":64,
        "reply_grant":{"target":{"schema_version":2,"channel":"qq","kind":"group","id":"12345","account":"qq:9"},
            "allowed_message_types":["text"],"expires_at":chrono::Utc::now()+chrono::Duration::hours(1)}});
    commit_output(&store, &mut subject, 1, &[], &format!("[delegate] {spec}")).await;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let run = tokio::spawn(broker.run());
    drive_until(&native, || store.native_task_result(task.task_id).is_ok_and(|result| result.is_some()),
        || format!("operations={:?}", store.external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION).map(|rows| rows.len()))).await;
    let step = child_step(&store, task.task_id);
    assert!(matches!(step.outcome, Some(shenko_types::operation::OperationOutcome::Failed)),
        "a rejected generation is a failed step, not a successful one: {:?}", step.outcome);
    let attempt = store.external_operation_attempt(step.operation_id).unwrap();
    let receipt = store.external_evidence(attempt.final_receipt_id.expect("the refusal is durable evidence")).unwrap();
    let raw: serde_json::Value = serde_json::from_slice(
        &store.cas.read_object(&receipt.raw_ref.digest).unwrap()).unwrap();
    if truncated {
        assert_eq!(raw["response"]["content"], "[reply] incomplete answer must not publish");
        assert_eq!(raw["response"]["stop_reason"], "length");
    } else {
        assert_eq!(raw["status"]["rejected"]["http_status"], 404);
        assert_eq!(raw["termination"]["kind"], "transport_rejected");
        assert!(raw["response"].is_null(), "a refused request has no provider text to adopt");
    }
    let result = store.native_task_result(task.task_id).unwrap().unwrap();
    assert!(matches!(result.outcome, TaskOutcome::Failed { .. }), "a failed generation cannot settle a successful run: {:?}", result.outcome);
    assert_eq!(result.result.as_ref().unwrap()["termination"], "generation_failed");
    assert_eq!(result.result.as_ref().unwrap()["publications"], serde_json::json!([]));
    assert!(store.committed_intents(Some("publication"), None, 1).unwrap().is_empty(),
        "a partial reply must not enter the publication ledger");
    assert_eq!(wires_rx.len(), 1, "a refused generation is terminal: no second request");
    native.close_and_join(0).await;
    drop(native); drop(subject);
    run.abort(); let _ = run.await;
    drop(handle);
    server.abort();
    let _ = server.await;
    drop(store);
    // This is a reload of an already settled result, not run reconciliation.
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    let (_, next) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let (head, _) = reopened.load_subject(agent).unwrap();
    let restored = service(&reopened, next.clone(), head.software_generation);
    restored.recover().unwrap();
    assert!(matches!(reopened.native_task_result(task.task_id).unwrap().unwrap().outcome, TaskOutcome::Failed { .. }),
        "a durable failed run must not become successful after a reopen");
    restored.close_and_join(0).await;
    drop(restored); drop(next);
}

#[tokio::test]
async fn failed_generation_reopens_after_e1_without_a_second_send() {
    use shenko_kernel::task::{NativeTaskHandler, NativeTaskInvocation};
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut wait = Some(wait);
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            sent.send(read_wire(&mut socket).await).unwrap();
            if let Some(wait) = wait.take() { wait.await.unwrap(); }
            let body = b"{\"error\":\"no such model\"}";
            socket.write_all(format!("HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(&store, &mut subject, 1, &[], &delegation("failed-child", "Keep the original evidence.", "Report the provider outcome.", 64)).await;
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let handler = Arc::new(ModelChildHandler::new(store.clone(), handle.clone(), cordis_core::Context::new()).unwrap());
    let descriptor = handler.descriptor();
    let native = Arc::new(NativeChildService::new(store.clone(), subject.head.software_generation.clone()));
    native.install(handler.clone()).unwrap();
    let claim = store.claim_native_task(task.task_id, descriptor.implementation_ref, false).unwrap().unwrap();
    let decision: shenko_types::subject::StepDecision = serde_json::from_slice(&store.cas.read_object(&task.input_ref.digest).unwrap()).unwrap();
    let input = decision.delegated_task_intents.iter().find(|intent| intent.intent_id == task.task_id).unwrap().definition.as_ref().unwrap().input.clone();
    let invocation = NativeTaskInvocation {
        context: shenko_types::agent::CallContext {
            caller_instance_id: store.capabilities.incarnation, role_revision: descriptor.role_revision,
            task_id: Some(task.task_id), expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id), controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(), authority_refs: descriptor.authority_refs, trace_id: EntityId::new(),
        },
        task_invocation: claim.invocation.clone(), task: claim.task.clone(), operation_id: claim.operation_id,
        authority: shenko_types::operation::AuthorityRef {
            delegation_id: task.task_id, delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: task.created_by_step_id.unwrap(),
        },
        input, child_tools: Some(native.tool_runtime()),
    };
    let broker_run = tokio::spawn(broker.run());
    let mut execution = Box::pin(handler.execute(invocation));
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            result = &mut execution => panic!("child ended before provider release: {result:?}"),
            wire = received.recv() => assert_eq!(wire.unwrap()["model"], "failed-child"),
        }
    }).await.unwrap();
    release.send(()).unwrap();
    // Do not poll the worker again: the real broker records E1 independently.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if child_step(&store, task.task_id).state == OperationState::Resolved { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    let step = child_step(&store, task.task_id);
    let attempt = store.external_operation_attempt(step.operation_id).unwrap();
    let evidence = store.external_evidence(attempt.final_receipt_id.unwrap()).unwrap();
    let raw = store.cas.read_object(&evidence.raw_ref.digest).unwrap();
    assert_eq!(step.outcome, Some(shenko_types::operation::OperationOutcome::Failed));
    assert!(store.get_operation(claim.operation_id).unwrap().is_none());
    assert!(store.native_task_result(task.task_id).unwrap().is_none());
    drop(execution);
    native.close_and_join(0).await;
    drop(native); drop(handler); drop(subject);
    broker_run.abort(); let _ = broker_run.await;
    drop(handle); drop(store);

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    let (broker, next) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let (head, _) = reopened.load_subject(agent).unwrap();
    let restored = service(&reopened, next.clone(), head.software_generation);
    let broker_run = tokio::spawn(broker.run());
    drive_until(&restored, || reopened.native_task_result(task.task_id).unwrap().is_some(),
        || format!("task={:?}", reopened.external_task(task.task_id).map(|task| task.state))).await;
    let result = reopened.native_task_result(task.task_id).unwrap().unwrap();
    assert!(matches!(result.outcome, TaskOutcome::Failed { .. }));
    assert_eq!(result.result.unwrap()["termination"], "generation_failed");
    assert_eq!(reopened.external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION).unwrap().len(), 1);
    assert_eq!(reopened.external_operation_attempt(step.operation_id).unwrap().final_receipt_id, attempt.final_receipt_id);
    assert_eq!(reopened.cas.read_object(&evidence.raw_ref.digest).unwrap(), raw);
    assert!(received.try_recv().is_err(), "recovery sent a second generation");
    restored.close_and_join(0).await;
    broker_run.abort(); let _ = broker_run.await;
    server.abort(); let _ = server.await;
}

async fn converge_with_bounds<F, Fut, D, S>(
    mut step: F,
    mut done: D,
    mut state: S,
    pass_bound: Duration,
    convergence_bound: Duration,
) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
    D: FnMut() -> bool,
    S: FnMut() -> String,
{
    let started = std::time::Instant::now();
    let mut passes = 0u64;
    loop {
        if started.elapsed() > convergence_bound {
            return Err(format!(
                "no convergence within {convergence_bound:?} after {passes} dispatch passes (durable state: {})",
                state()
            ));
        }
        match tokio::time::timeout(pass_bound, step()).await {
            Ok(()) => {}
            Err(_) => {
                return Err(format!(
                    "a dispatch pass did not finish within {pass_bound:?} (durable state: {})",
                    state()
                ));
            }
        }
        passes += 1;
        if done() { return Ok(()); }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// One bounded dispatch pass of the native service.
const PASS_BOUND: Duration = Duration::from_secs(30);
/// The overall bound of one convergence.
const CONVERGENCE_BOUND: Duration = Duration::from_secs(120);

/// The convergence bound must interrupt a dispatch pass that never finishes
/// and must bound a dispatch that never converges. A counter-only control
/// would prove neither.
#[tokio::test]
async fn convergence_bound_interrupts_a_stuck_pass_and_a_diverging_dispatch() {
    let stuck = converge_with_bounds(
        || std::future::pending::<()>(),
        || false,
        || "durable state: probe".to_string(),
        Duration::from_millis(50),
        Duration::from_secs(5),
    )
    .await
    .expect_err("a dispatch pass that never finishes must be interrupted");
    assert!(stuck.contains("did not finish"), "unexpected refusal: {stuck}");

    let diverging = converge_with_bounds(
        || async {},
        || false,
        || "durable state: probe".to_string(),
        Duration::from_millis(50),
        Duration::from_millis(120),
    )
    .await
    .expect_err("a dispatch that never converges must be bounded");
    assert!(diverging.contains("no convergence"), "unexpected refusal: {diverging}");
}

async fn drive_until(
    service: &Arc<NativeChildService>,
    done: impl Fn() -> bool,
    state: impl Fn() -> String,
) {
    converge_with_bounds(
        || async { service.run_once().await.unwrap(); },
        done,
        state,
        PASS_BOUND,
        CONVERGENCE_BOUND,
    )
    .await
    .unwrap_or_else(|error| panic!("bounded model child mechanics did not converge: {error}"));
}

#[tokio::test]
async fn distinct_authored_children_use_selected_wire_and_reopen_owned_results() {
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = read_wire(&mut socket).await;
            reply(&mut socket, wire["model"].as_str().unwrap()).await;
            wires_tx.send(wire).unwrap();
        }
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let first = delegation("child-a", "Independent analyst. Keep literal evidence.", "Explain the supplied alpha evidence.", 96);
    let second = delegation("child-b", "Independent critic. Question assumptions.", "Review the supplied beta assumptions.", 128);
    commit_output(&store, &mut subject, 1, &[], &format!("{first}\n{second}\n两项有限研究已委派。")).await;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    drive_until(
        &native,
        || store.ready_native_results(agent, 8).unwrap().len() == 2,
        || {
            let candidates = store
                .native_task_candidates_after(None, 8)
                .map(|rows| {
                    rows.iter()
                        .map(|(_, record)| format!("{:?}", record.state))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|error| format!("unreadable:{error}"));
            format!(
                "candidates=[{candidates}] ready={} broker_active={}",
                store.ready_native_results(agent, 8).map(|rows| rows.len()).unwrap_or(0),
                handle.active_count(),
            )
        },
    ).await;
    let wires = [wires_rx.recv().await.unwrap(), wires_rx.recv().await.unwrap()];
    server.await.unwrap();
    let results = store.ready_native_results(agent, 8).unwrap();
    for wire in &wires {
        let (semantic, goal, cap) = match wire["model"].as_str().unwrap() {
            "child-a" => ("Independent analyst. Keep literal evidence.", "Explain the supplied alpha evidence.", 96),
            "child-b" => ("Independent critic. Question assumptions.", "Review the supplied beta assumptions.", 128),
            other => panic!("unexpected provider model: {other}"),
        };
        // The child's own system prompt is the role contract, the authored
        // semantic prompt and the installed tool catalog.
        assert!(wire["messages"][0]["content"].as_str().unwrap().contains(semantic));
        assert_eq!(wire["messages"][1]["content"], goal);
        assert_eq!(wire["max_tokens"], cap);
        let result = results.iter().find(|result| result.result.as_ref().unwrap()["provider_binding"]["model"] == wire["model"]).unwrap();
        assert!(matches!(result.outcome, TaskOutcome::Succeeded));
        assert_eq!(result.result.as_ref().unwrap()["termination"], "completed");
        assert!(result.result.as_ref().unwrap()["answer"].as_str().unwrap().contains(&format!(
            "scripted answer for {}", wire["model"].as_str().unwrap())));
        let task = store.external_task(result.task_id).unwrap();
        assert_eq!(task.owner, TaskOwner::Task { id: subject.head.subject_root_task_id });
        // The sealed delegation contract binds the authored choice.
        let operation = store.external_operation(result.operation_id).unwrap();
        let frozen: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&operation.input_digest).unwrap()).unwrap();
        assert_eq!(frozen["spec"]["system_prompt"], semantic);
        assert_eq!(frozen["provider_binding"]["model"], wire["model"]);
        // The model step is its own durable operation with its own frozen payload.
        let step = child_step(&store, result.task_id);
        let step_input: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&step.final_input_ref.digest).unwrap()).unwrap();
        assert_eq!(step_input["payload"]["system_prompt"], wire["messages"][0]["content"]);
        assert_eq!(step_input["max_tokens"], cap);
        assert_eq!(step_input["step_index"], 0);
        assert_eq!(store.external_operation_attempts(result.operation_id).unwrap().len(), 1);
        assert_eq!(store.external_operation_attempts(step.operation_id).unwrap().len(), 1);
    }
    assert_eq!(handle.binding().model(), "subject-model", "child choices cannot mutate the subject binding");
    assert_eq!(handle.active_count(), 0);
    native.close_and_join(0).await;
    drop(native);
    run.abort(); let _ = run.await;
    drop(subject); drop(store);
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    let (_, next) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let (head, _) = reopened.load_subject(agent).unwrap();
    let native = service(&reopened, next.clone(), head.software_generation);
    native.run_once().await.unwrap();
    assert_eq!(handle.active_count(), 0);
    let ready = reopened.ready_native_results(agent, 8).unwrap();
    assert_eq!(ready.iter().map(|r| r.result_id).collect::<Vec<_>>(), results.iter().map(|r| r.result_id).collect::<Vec<_>>());
    for result in ready {
        assert_eq!(reopened.external_operation_attempts(result.operation_id).unwrap().len(), 1);
        // The reopened child did not send a second generation.
        assert_eq!(reopened.external_operation_attempts(child_step(&reopened, result.task_id).operation_id).unwrap().len(), 1);
    }
}

#[tokio::test]
async fn cancelled_stopped_waiter_retains_late_provider_e1_without_t2() {
    let (_dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wire_tx, wire_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let wire = read_wire(&mut socket).await;
        wire_tx.send(wire).unwrap();
        release_rx.await.unwrap();
        reply(&mut socket, "cancel-child").await;
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(&store, &mut subject, 1, &[], &delegation("cancel-child", "Report only the supplied facts.", "Return this finite review.", 64)).await;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    native.run_once().await.unwrap();
    let wire = tokio::time::timeout(Duration::from_secs(5), wire_rx).await.unwrap().unwrap();
    assert_eq!(wire["model"], "cancel-child");
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let operation_id = child_step(&store, task.task_id).operation_id;
    assert_eq!(handle.active_count(), 1);
    native.cancel(task.task_id, "owner cancelled", AbortScope::Task).unwrap();
    native.close_and_join(0).await;
    assert_eq!(handle.active_count(), 1, "stopping a managed waiter is not provider termination");
    release_tx.send(()).unwrap();
    server.await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.external_operation(operation_id).unwrap().state != OperationState::Resolved {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    assert_eq!(handle.active_count(), 0);
    assert!(store.external_task(task.task_id).unwrap().abort_request.is_some());
    assert!(store.native_task_result(task.task_id).unwrap().is_none());
    assert!(store.ready_native_results(agent, 8).unwrap().is_empty());
    assert_eq!(store.external_operation_evidence(operation_id).unwrap().len(), 1);
    assert_eq!(store.external_operation_attempts(operation_id).unwrap().len(), 1);
    run.abort(); let _ = run.await;
}

#[tokio::test]
async fn uncertain_child_restores_original_debt_and_never_redispatches() {
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let wire = read_wire(&mut socket).await;
        drop(socket); // No provider-terminal evidence, not a rejection.
        (wire, listener)
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(&store, &mut subject, 1, &[], &delegation("uncertain-child", "Keep uncertainty explicit.", "Review the supplied finite evidence.", 64)).await;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    native.run_once().await.unwrap();
    let (wire, listener) = tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
    assert_eq!(wire["model"], "uncertain-child");
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let operation_id = child_step(&store, task.task_id).operation_id;
    drive_until(
        &native,
        || store.external_operation(operation_id).unwrap().state == OperationState::Reconciling,
        || format!(
            "operation={:?} attempts={:?}",
            store.external_operation(operation_id).map(|row| row.state),
            store.external_operation_attempts(operation_id).map(|rows| rows.len()),
        ),
    ).await;
    let original = store.external_operation(operation_id).unwrap();
    let attempt = store.external_operation_attempt(operation_id).unwrap();
    assert_eq!(handle.active_count(), 1);
    native.close_and_join(0).await;
    run.abort(); let _ = run.await;
    drop(native); drop(subject); drop(store);
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    let (broker, recovered) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let (head, _) = reopened.load_subject(agent).unwrap();
    let native = service(&reopened, recovered.clone(), head.software_generation);
    assert_eq!(recovered.active_count(), 1, "debt is rebuilt before the broker starts");
    // Repeated reader construction/reconciliation must not double-charge it.
    let _reader = ModelChildHandler::new(reopened.clone(), recovered.clone(), cordis_core::Context::new()).unwrap();
    assert_eq!(recovered.active_count(), 1);
    let run = tokio::spawn(broker.run());
    for _ in 0..4 { native.run_once().await.unwrap(); tokio::time::sleep(Duration::from_millis(5)).await; }
    assert!(tokio::time::timeout(Duration::from_millis(100), listener.accept()).await.is_err(), "uncertain model send was replayed");
    assert_eq!(reopened.external_operation(operation_id).unwrap().input_digest, original.input_digest);
    assert_eq!(reopened.external_operation_attempt(operation_id).unwrap().attempt_id, attempt.attempt_id);
    assert_eq!(reopened.external_operation_attempts(operation_id).unwrap().len(), 1);
    assert!(reopened.native_task_result(task.task_id).unwrap().is_none());
    assert_eq!(recovered.active_count(), 1);
    run.abort(); let _ = run.await;
}

/// Accepted child requests consume their own transient material before the
/// provider answers. A replacement registry and a reopened Store must not
/// revive an older step's material, nor hide newly prepared material.
#[tokio::test]
async fn child_contributions_survive_registry_replacement_and_reopen_without_reuse() {
    use shenko_context_policy::contribution::*;
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut release = Some(release_rx);
        for text in ["[tool:fixture.missing:1] {}", "finished the finite observation"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            wires_tx.send(read_wire(&mut socket).await).unwrap();
            if let Some(release) = release.take() { release.await.unwrap(); }
            let raw = serde_json::json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":12,"completion_tokens":7}}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(&store, &mut subject, 1, &[], &format!("[delegate] {}", serde_json::json!({
        "goal":"Observe prepared context.","system_prompt":"Use only supplied facts.","model":"child-contributions","max_tokens":64,
        "budget":{"max_steps":3,"max_total_tokens":192,"max_tool_calls":3},
        "tools":[{"kind":"fixture.missing","definition_version":1}]
    }))).await;
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let scope = ContributionScope { scope_id: task.task_id, task_id: Some(task.task_id) };
    let text = "PRIVATE_CHILD_MATERIAL_ONCE";
    let item = ContextContribution {
        key: "once".into(), slot: ContributionSlot::TransientReminder,
        source: ContributionSource { plugin_id: EntityId::new(), plugin_version: "fixture/1".into(), artifact_digest: None, kind: ContributionKind::Reminder },
        scope: scope.clone(), input_revision: "original".into(), lifetime: ContributionLifetime::ThisRequest,
        required: true, budget_tokens: 32, tokens: 8,
        data: store.cas.seal_object(&ContributionPayload::seal(text).unwrap(), "application/json", "fixture", "ContributionPayload", 1).unwrap().object,
        prepared_at_ms: 1,
    };
    let publish = |registry: &ContributionRegistry, material: Vec<ContextContribution>| {
        let registration = registry.register_publisher(item.source.clone());
        registry.publish(registry.begin_publication(registration, &scope).unwrap(), material).unwrap();
    };
    let supervisor = shenko_kernel::Supervisor::new(subject.head.software_generation.clone());
    let registry = Arc::new(ContributionRegistry::new());
    let host = supervisor.spawn(ContributionHost, registry.clone()).await.unwrap();
    publish(&registry, vec![item.clone()]);
    let native = Arc::new(NativeChildService::new(store.clone(), subject.head.software_generation.clone()));
    native.install(Arc::new(ModelChildHandler::new(store.clone(), handle.clone(), supervisor.context().clone()).unwrap())).unwrap();
    native.recover().unwrap();
    let run = tokio::spawn(broker.run());
    native.run_once().await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), wires_rx.recv()).await.unwrap().unwrap();
    assert_eq!(first["messages"].as_array().unwrap().iter().filter(|message| message["content"].as_str()
        == Some("[prepared contributions]\nPRIVATE_CHILD_MATERIAL_ONCE")).count(), 1);
    let operation = child_step(&store, task.task_id);
    let input: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&operation.final_input_ref.digest).unwrap()).unwrap();
    let request_id: EntityId = serde_json::from_value(input["request_id"].clone()).unwrap();
    let used = registry.usage(&request_id).expect("T1, not the provider answer, consumes material");
    assert_eq!(used.sent, vec![item.clone()]);
    assert_eq!(used.seal_digest, operation.final_input_ref.digest);
    assert!(store.native_task_result(task.task_id).unwrap().is_none());

    supervisor.uninstall(&host).await.unwrap();
    let replacement = Arc::new(ContributionRegistry::new());
    supervisor.spawn(ContributionHost, replacement.clone()).await.unwrap();
    let mut fresh = item.clone();
    fresh.key = "fresh".into();
    fresh.input_revision = "new observation".into();
    fresh.data = store.cas.seal_object(&ContributionPayload::seal("FRESH_AFTER_REPLACEMENT").unwrap(), "application/json", "fixture", "ContributionPayload", 1).unwrap().object;
    publish(&replacement, vec![item.clone(), fresh.clone()]);
    release_tx.send(()).unwrap();
    drive_until(&native, || store.native_task_result(task.task_id).unwrap().is_some(),
        || format!("attempts={:?}", store.external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION).map(|rows| rows.len()))).await;
    let second = wires_rx.recv().await.unwrap();
    assert!(!second.to_string().contains(text), "an earlier request's material resurfaced");
    assert!(second["messages"].as_array().unwrap().iter().any(|message| message["content"] == "[prepared contributions]\nFRESH_AFTER_REPLACEMENT"));
    assert_eq!(replacement.usage(&request_id), Some(used));
    assert!(replacement.collect(&scope, 2).contributions.is_empty());
    assert!(matches!(store.native_task_result(task.task_id).unwrap().unwrap().outcome, TaskOutcome::Succeeded));
    // The inspection trajectory reads the same run back from ledger facts:
    // both model steps in order with their sealed records, the tool call the
    // first step made, and the final answer (docs/specs/web-interface.md#5-2).
    let trajectory = store.task_trajectory(task.agent_id, task.task_id).unwrap().expect("the run has a trajectory");
    assert_eq!(trajectory.task.kind, crate::model_child::MODEL_CHILD_KIND);
    assert!(trajectory.input.as_ref().is_some_and(|input| input.to_string().contains("Observe prepared context.")),
        "the delegation brief is readable: {:?}", trajectory.input);
    let steps: Vec<_> = trajectory.operations.iter()
        .filter(|operation| operation.operation_name == MODEL_CHILD_STEP_OPERATION).collect();
    assert_eq!(steps.len(), 2, "{:?}", trajectory.operations);
    let first = steps[0].record.as_ref().expect("first step record is sealed");
    assert_eq!(first["tool_calls"].as_array().map(Vec::len), Some(1), "{first}");
    assert_eq!(steps[1].record.as_ref().unwrap()["text"], "finished the finite observation");
    server.await.unwrap();
    native.close_and_join(0).await;
    drop(native); drop(subject);
    run.abort(); let _ = run.await;
    drop(handle);
    supervisor.stop().await.unwrap();
    drop(store);
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (offline, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let context = cordis_core::Context::new();
    let restored = Arc::new(ContributionRegistry::new());
    context.provide(restored.clone()).unwrap();
    publish(&restored, vec![item.clone(), fresh]);
    let _reader = ModelChildHandler::new(reopened, handle, context).unwrap();
    assert!(restored.collect(&scope, 3).contributions.is_empty());
    assert_eq!(restored.usages().iter().map(|usage| usage.sent.len()).sum::<usize>(), 2);
    assert_eq!(restored.usage(&request_id).unwrap().sent, vec![item.clone()]);
    assert_eq!(restored.usage(&request_id).unwrap().seal_digest, operation.final_input_ref.digest);
    drop(offline);
}

/// A finite authored budget may exceed both the delegating step's budget and
/// transport defaults. Resource admission still happens before actual sends.
#[tokio::test]
async fn child_authored_budget_is_not_capped_by_defaults() {
    let (_dir, store, agent) = fixture();
    let (_, handle) = Broker::with_local_provider(LocalModelProvider::new("http://127.0.0.1:1".into(), "subject-model".into()), 1).unwrap();
    let output_budget = handle.binding().default_max_tokens().checked_add(1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
    subject.max_tokens = 32;
    let stop = tokio_util::sync::CancellationToken::new(); stop.cancel();
    execute_step(1, &[], &mut subject, &stop).await.unwrap();
    let authored = format!("[delegate] {}", serde_json::json!({
        "goal":"Finite task.", "system_prompt":"Independent semantic prompt.",
        "model":"child", "max_tokens":output_budget,
        "budget":{"max_steps":64,"max_tool_calls":64,"max_total_tokens":output_budget.checked_mul(64).unwrap()}
    }));
    record_result(&store, &mut subject, &authored);
    let committed = execute_step(1, &[], &mut subject, &tokio_util::sync::CancellationToken::new()).await.unwrap();
    assert!(committed.is_some(), "the child's budget is not the delegating step's cap");
    assert_eq!(subject.head.committed_step_no, 1);
    assert_eq!(store.native_task_candidates_after(None, 8).unwrap().len(), 1);
}

#[tokio::test]
async fn cloud_child_t1_and_result_keep_the_selected_send_not_a_later_budget() {
    use shenko_broker::provider::AnthropicProvider;
    use shenko_quota::*;
    use shenko_store::external::InferenceSendBinding;
    use std::collections::HashMap;

    let (_dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wire_tx, wire_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        wire_tx.send(read_wire(&mut socket).await).unwrap();
        release_rx.await.unwrap();
        let raw = r#"{"content":[{"type":"text","text":"scripted quota-bound cloud result"}],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":7}}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let (broker, handle) = Broker::with_anthropic_provider(
        AnthropicProvider::with_endpoint(
            "scripted-loopback-not-a-credential".into(),
            endpoint,
            "subject-cloud",
        ),
        1,
    )
    .unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(&store, &mut subject, 1, &[], &delegation("cloud-child", "Independent finite reviewer.", "Review the supplied evidence.", 64)).await;
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    // Install the synthetic policy after the scripted parent result; that
    // fixture does not claim to perform a cloud subject inference under quota.
    let window = |key: &str| WindowRef { provider_id: "anthropic".into(), account_scope_ref: "synthetic-child-quota".into(), window_key: key.into() };
    let policy = QuotaPolicy {
        policy_id: EntityId::new(), revision: 1,
        profiles: vec![ModelProfile { profile_id: "cloud-child-profile".into(), model_ref: "cloud-child".into(), local: false,
            cost_per_request: vec![
                WindowCost { window: window("api:requests"), unit: "requests".into(), amount: Range::point(1.0).unwrap() },
                WindowCost { window: window("api:tokens"), unit: "tokens".into(), amount: Range::point(64.0).unwrap() },
            ],
            effort_levels: Vec::new(),
        }], assumptions: vec![], min_progress_per_hour: 0.0, switching_hysteresis: 0.0,
        on_unknown: UnknownQuota::Block, allow_overage: false, spending_authority_ref: None,
        decision_ttl: chrono::Duration::minutes(5), observation_sources: vec![SourceObservationPolicy {
            source_kind: SourceKind::ApiHeaders, contract: None,
            ttl: chrono::Duration::minutes(5), stale: StaleUse::Refuse,
        }],
        objective: PolicyObjective::Balanced, target_utilization_range: None, planning_horizon: None,
        emergency_reserve_by_window: Vec::new(), uncertainty_margin: None, admission_pacing: AdmissionPacing::default(),
    };
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    let snapshot = from_api_headers(&ObservationContext {
        provider_id: "anthropic".into(), account_scope_ref: "synthetic-child-quota".into(), source_version: None,
        attribution: Attribution::Exclusive, observed_at: chrono::Utc::now(),
    }, [
        ("anthropic-ratelimit-requests-limit", "100"), ("anthropic-ratelimit-requests-remaining", "100"),
        ("anthropic-ratelimit-requests-reset", "2099-01-01T00:00:00Z"),
        ("anthropic-ratelimit-tokens-limit", "1000"), ("anthropic-ratelimit-tokens-remaining", "1000"),
        ("anthropic-ratelimit-tokens-reset", "2099-01-01T00:00:00Z"),
    ]);
    store.record_quota_snapshot(&snapshot).unwrap();
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    native.run_once().await.unwrap();
    let wire = tokio::time::timeout(Duration::from_secs(5), wire_rx).await.unwrap().unwrap();
    assert_eq!(wire["model"], "cloud-child");
    assert!(wire["system"].as_str().unwrap().contains("Independent finite reviewer."));
    let step = child_step(&store, task.task_id);
    let operation_id = step.operation_id;
    let operation = store.external_operation(operation_id).unwrap();
    let attempt = store.external_operation_attempt(operation_id).unwrap();
    let selected: InferenceSendBinding = serde_json::from_slice(&store.cas.read_object(&attempt.transport_ref.unwrap().digest).unwrap()).unwrap();
    let send = selected.send_id.expect("configured cloud quota must bind its exact send");
    assert_eq!(store.quota_send_ids(selected.request_id).unwrap(), vec![(send, operation.prepared_commit.seq.0)]);

    // A second budget is real durable state, but is not this wire's send.
    // Its later creation must not redirect E1 or settle somebody else's rows.
    let (_, decision) = store.latest_quota_decision().unwrap().unwrap();
    let later = EntityId::new();
    let availability = HashMap::from([("cloud-child-profile".into(), BackendState::Available)]);
    let admission = match store.admit_quota_send(selected.request_id, &policy, &decision, later, &availability, chrono::Utc::now()).unwrap().outcome {
        AdmissionOutcome::Quota(admission) | AdmissionOutcome::Existing { admission } if admission.state == AdmissionState::Admitted => admission,
        other => panic!("later synthetic budget was not admitted: {other:?}"),
    };
    store.arm_quota_send(selected.request_id, &admission).unwrap();
    release_tx.send(()).unwrap(); server.await.unwrap();
    drive_until(
        &native,
        || store.native_task_result(task.task_id).unwrap().is_some(),
        || format!(
            "result_present={:?} attempts={:?}",
            store.native_task_result(task.task_id).map(|row| row.is_some()),
            store.external_operation_attempts(child_step(&store, task.task_id).operation_id).map(|rows| rows.len()),
        ),
    ).await;
    let result = store.native_task_result(task.task_id).unwrap().unwrap();
    assert_eq!(result.result.as_ref().unwrap()["termination"], "completed");
    assert!(result.result.as_ref().unwrap()["answer"].as_str().unwrap().contains("scripted quota-bound cloud result"));
    let rows = store.quota_attempts("anthropic", "synthetic-child-quota").unwrap();
    assert_eq!(rows.len(), 4);
    for row in rows {
        if row.request_id == send {
            assert_eq!(row.billing, AttemptBilling::Settled);
            assert_eq!(row.cost, Range::point(if row.window_key == "api:tokens" { 19.0 } else { 1.0 }).unwrap());
        } else {
            assert_eq!(row.request_id, later);
            assert_eq!(row.billing, AttemptBilling::Unknown);
        }
    }
    assert!(store.quota_send_ids(selected.request_id).unwrap().contains(&(later, 0)));
    assert_eq!(store.external_operation_attempts(operation_id).unwrap().len(), 1);
    assert_eq!(handle.active_count(), 0);
    native.close_and_join(0).await;
    run.abort(); let _ = run.await;
}

/// An owner message delivered while a child is running reaches that same
/// discussion, is settled exactly once after the step that read it, and is
/// never read again by a restart.
#[tokio::test]
async fn owner_message_reaches_a_running_child_and_is_settled_once() {
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut release = Some(release_rx);
        for text in ["first observation recorded", "owner message answered"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            wires_tx.send(read_wire(&mut socket).await).unwrap();
            if let Some(release) = release.take() {
                release.await.unwrap();
            }
            let raw = serde_json::json!({
                "choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":12,"completion_tokens":7},
            }).to_string();
            socket
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let (broker, handle) =
        Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1)
            .unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(
        &store,
        &mut subject,
        1,
        &[],
        &delegation(
            "owner-message-child",
            "同一讨论里继续观察。",
            "观察一次实验并交回结论。",
            64,
        ),
    )
    .await;
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    native.run_once().await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), wires_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["model"], "owner-message-child");
    assert!(
        !first.to_string().contains("OWNER_MESSAGE"),
        "a message authored after this step was sent cannot be inside it"
    );

    // The owner writes to the live child while its first generation is still
    // in flight. The delivery is a durable S1 fact bound to this endpoint.
    let message = "OWNER_MESSAGE: 用 alpha 数据集重看一遍。";
    let step_id = commit_output(
        &store,
        &mut subject,
        2,
        &[],
        &format!(
            "[dialogue] {}",
            serde_json::json!({"target_task_id": task.task_id, "message": message})
        ),
    )
    .await;
    let delivered = store.committed_intents(Some("dialogue"), None, 8).unwrap();
    assert_eq!(delivered.len(), 1, "one authored dialogue intent");
    let intent_id = delivered[0].intent_id;
    assert_eq!(delivered[0].step_id, step_id);
    assert_eq!(delivered[0].state, "claimed", "delivered to a live endpoint");
    assert_eq!(store.pending_child_dialogue(task.task_id).unwrap().len(), 1);
    assert!(
        store.native_task_result(task.task_id).unwrap().is_none(),
        "the child had not finished when the owner wrote to it"
    );
    release_tx.send(()).unwrap();

    drive_until(
        &native,
        || store.native_task_result(task.task_id).unwrap().is_some(),
        || {
            format!(
                "steps={:?} pending={:?}",
                store
                    .external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION)
                    .map(|rows| rows.len()),
                store.pending_child_dialogue(task.task_id).map(|rows| rows.len()),
            )
        },
    )
    .await;
    let second = wires_rx.recv().await.unwrap();
    let rendered: Vec<String> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    let turn = format!("[owner 消息 intent:{intent_id}]\n{message}");
    assert!(
        rendered.iter().any(|content| content == &turn),
        "the running child never read the owner message: {rendered:?}"
    );
    assert_eq!(
        rendered.last().map(String::as_str),
        Some(turn.as_str()),
        "a delivered message is the newest user turn of the same discussion"
    );

    let result = store.native_task_result(task.task_id).unwrap().unwrap();
    assert!(matches!(result.outcome, TaskOutcome::Succeeded));
    let payload = result.result.as_ref().unwrap();
    assert_eq!(payload["termination"], "completed");
    assert_eq!(payload["question"], serde_json::Value::Null);
    assert_eq!(payload["dialogue"]["read"], 1);
    assert_eq!(payload["dialogue"]["undelivered"], serde_json::json!([]));
    assert_eq!(
        store.committed_intents(Some("dialogue"), None, 8).unwrap()[0].state,
        "settled",
        "the step that read the message settles the authored intent"
    );
    assert!(store.pending_child_dialogue(task.task_id).unwrap().is_empty());
    // The step that read the message sealed it together with its payload.
    let steps = store
        .external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION)
        .unwrap();
    assert_eq!(steps.len(), 2);
    let sealed: serde_json::Value =
        serde_json::from_slice(&store.cas.read_object(&steps[1].final_input_ref.digest).unwrap()).unwrap();
    assert_eq!(sealed["dialogue"][0]["message"], message);
    assert_eq!(sealed["dialogue"][0]["intent_id"], serde_json::json!(intent_id));
    assert_eq!(sealed["dialogue"][0]["target_task_id"], serde_json::json!(task.task_id));
    let sealed_turn = &sealed["payload"]["messages"].as_array().unwrap().last().unwrap()["parts"][0];
    assert_eq!(sealed_turn["type"], "text");
    assert_eq!(sealed_turn["text"], serde_json::json!(turn));
    assert_eq!(store.external_operation_attempts(steps[1].operation_id).unwrap().len(), 1);

    native.close_and_join(0).await;
    drop(native);
    drop(subject);
    drop(handle);
    run.abort();
    let _ = run.await;
    drop(store);

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    assert_eq!(
        reopened.committed_intents(Some("dialogue"), None, 8).unwrap()[0].state,
        "settled",
        "a restart must not re-admit an already settled delivery"
    );
    let (next, handle) =
        Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let (head, _) = reopened.load_subject(agent).unwrap();
    let restored = service(&reopened, handle.clone(), head.software_generation);
    let broker_run = tokio::spawn(next.run());
    for _ in 0..3 {
        restored.run_once().await.unwrap();
    }
    assert!(
        wires_rx.try_recv().is_err(),
        "a settled terminal child must not be dispatched again"
    );
    assert_eq!(
        reopened
            .external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION)
            .unwrap()
            .len(),
        2,
        "a restart never adds a step to a finished run"
    );
    assert_eq!(handle.active_count(), 0);
    restored.close_and_join(0).await;
    broker_run.abort();
    let _ = broker_run.await;
    server.abort();
    let _ = server.await;
}

/// A child that asks its owner a question ends as a durable decision, and the
/// owner's answer continues the very same discussion after a restart instead
/// of starting a second child or re-running the first run's steps.
#[tokio::test]
async fn a_child_question_is_answered_by_the_same_discussion_after_reopen() {
    // The suite has to be able to FINISH: this case hangs on main (issue
    // #266), and one hanging case hides every other result — the harness
    // waits forever and nobody can tell green from red. A bounded wait turns
    // the hang into the ordinary red case it actually is, with the same
    // evidence: the child's question must be answered after a reopen.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        a_child_question_is_answered_by_the_same_discussion_after_reopen_body(),
    )
    .await;
    assert!(
        outcome.is_ok(),
        "the child's question must be answered by the owner after a reopen (issue #266)"
    );
}

async fn a_child_question_is_answered_by_the_same_discussion_after_reopen_body() {
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for text in ["[ask] 要观察哪个数据集？", "数据集 alpha 的观察完成"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            wires_tx.send(read_wire(&mut socket).await).unwrap();
            let raw = serde_json::json!({
                "choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":12,"completion_tokens":7},
            }).to_string();
            socket
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let (broker, handle) =
        Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1)
            .unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let task = {
        commit_output(
            &store,
            &mut subject,
            1,
            &[],
            &delegation(
                "question-child",
                "只报告观察事实。",
                "观察一次实验并交回结论。",
                64,
            ),
        )
        .await;
        store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1
    };
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    drive_until(
        &native,
        || store.native_task_result(task.task_id).unwrap().is_some(),
        || format!("candidates={:?}", store.native_task_candidates_after(None, 8).map(|rows| rows.len())),
    )
    .await;
    let first = wires_rx.recv().await.unwrap();
    assert_eq!(first["model"], "question-child");
    let result = store.native_task_result(task.task_id).unwrap().unwrap();
    let payload = result.result.as_ref().unwrap();
    assert!(matches!(result.outcome, TaskOutcome::Succeeded));
    assert_eq!(payload["termination"], "needs_decision");
    assert_eq!(payload["question"], "要观察哪个数据集？");
    assert_eq!(
        payload["continuation"]["continue_task_id"],
        serde_json::json!(task.task_id)
    );
    assert_eq!(payload["continuation"]["owner_answer_required"], true);
    assert_eq!(
        payload["continuation"]["remaining_total_tokens"],
        serde_json::json!(512_u64 - 64_u64),
        "the answer may only spend what the question left"
    );
    assert_eq!(payload["steps"], 1, "the question ended the run");
    assert!(wires_rx.try_recv().is_err(), "asking does not send a second generation");

    native.close_and_join(0).await;
    drop(native);
    drop(subject);
    drop(handle);
    run.abort();
    let _ = run.await;
    drop(store);

    // A restart keeps the question as the durable decision of that child.
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    let kept = reopened.native_task_result(task.task_id).unwrap().unwrap();
    assert!(matches!(kept.outcome, TaskOutcome::Succeeded));
    assert_eq!(
        kept.result.as_ref().unwrap()["termination"],
        "needs_decision",
        "a restart must not turn a question into a completed answer"
    );

    // The owner answers the question it read: the continuation is the same
    // discussion, on the bounded remainder of the original run.
    let (broker, handle) =
        Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1)
            .unwrap();
    let mut subject = state(reopened.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(
        &reopened,
        &mut subject,
        2,
        &[],
        &format!(
            "[delegate] {}",
            serde_json::json!({
                "goal": "观察一次实验并交回结论。",
                "system_prompt": "只报告观察事实。",
                "model": "question-child",
                "max_tokens": 64,
                "budget": {"max_total_tokens": 448, "max_steps": 4, "max_tool_calls": 4},
                "continue_task_id": task.task_id,
                "owner_answer": "数据集是 alpha。",
            })
        ),
    )
    .await;
    let continued = reopened
        .native_task_candidates_after(None, 8)
        .unwrap()
        .pop()
        .unwrap()
        .1;
    assert_ne!(continued.task_id, task.task_id, "a continuation is its own finite run");
    let native = service(&reopened, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    drive_until(
        &native,
        || reopened.native_task_result(continued.task_id).unwrap().is_some(),
        || format!("candidates={:?}", reopened.native_task_candidates_after(None, 8).map(|rows| rows.len())),
    )
    .await;
    let second = wires_rx.recv().await.unwrap();
    let rendered: Vec<String> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        rendered
            .iter()
            .any(|content| content.contains("[ask] 要观察哪个数据集？")),
        "the continued discussion reads the question it asked: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|content| content == "[owner 回答]\n数据集是 alpha。"),
        "the owner's own words are the next turn: {rendered:?}"
    );
    let answer_turn = rendered
        .iter()
        .position(|content| content == "[owner 回答]\n数据集是 alpha。")
        .unwrap();
    let question_turn = rendered
        .iter()
        .position(|content| content.contains("[ask] 要观察哪个数据集？"))
        .unwrap();
    assert!(question_turn < answer_turn, "the answer follows the question it answers");

    let settled = reopened.native_task_result(continued.task_id).unwrap().unwrap();
    assert!(matches!(settled.outcome, TaskOutcome::Succeeded));
    let payload = settled.result.as_ref().unwrap();
    assert_eq!(payload["termination"], "completed");
    assert_eq!(payload["continued_from"], serde_json::json!(task.task_id));
    assert_eq!(payload["question"], serde_json::Value::Null);
    let step = child_step(&reopened, continued.task_id);
    let sealed: serde_json::Value =
        serde_json::from_slice(&reopened.cas.read_object(&step.final_input_ref.digest).unwrap()).unwrap();
    assert_eq!(sealed["step_index"], 1, "the continuation continues the transcript");
    // The first run's step is never re-sent by the continuation.
    assert_eq!(
        reopened
            .external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .external_operation_attempts(child_step(&reopened, task.task_id).operation_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .external_task_operations(continued.task_id, MODEL_CHILD_STEP_OPERATION)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(handle.binding().model(), "subject-model", "child choices cannot mutate the subject binding");

    native.close_and_join(0).await;
    run.abort();
    let _ = run.await;
    server.abort();
    let _ = server.await;
}

/// A worker that disappears after the last step's E1 and before the run
/// result settles the same decision on recovery. It must not send a second
/// generation for a run that already answered.
#[tokio::test]
async fn a_decided_run_reopens_without_a_second_generation() {
    use shenko_kernel::task::{NativeTaskHandler, NativeTaskInvocation};
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        sent.send(read_wire(&mut socket).await).unwrap();
        release_rx.await.unwrap();
        reply(&mut socket, "decided-child").await;
        // A second request would surface here: the listener stays open.
        let (_socket, _) = listener.accept().await.unwrap();
        sent.send(serde_json::json!({"unexpected":"second send"})).unwrap();
    });
    let (broker, handle) =
        Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1)
            .unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(
        &store,
        &mut subject,
        1,
        &[],
        &delegation("decided-child", "只报告观察事实。", "观察一次实验并交回结论。", 64),
    )
    .await;
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let handler = Arc::new(
        ModelChildHandler::new(store.clone(), handle.clone(), cordis_core::Context::new()).unwrap(),
    );
    let native = Arc::new(NativeChildService::new(
        store.clone(),
        subject.head.software_generation.clone(),
    ));
    native.install(handler.clone()).unwrap();
    let descriptor = handler.descriptor();
    let claim = store
        .claim_native_task(task.task_id, descriptor.implementation_ref, false)
        .unwrap()
        .unwrap();
    let decision: shenko_types::subject::StepDecision =
        serde_json::from_slice(&store.cas.read_object(&task.input_ref.digest).unwrap()).unwrap();
    let input = decision
        .delegated_task_intents
        .iter()
        .find(|intent| intent.intent_id == task.task_id)
        .unwrap()
        .definition
        .as_ref()
        .unwrap()
        .input
        .clone();
    let invocation = NativeTaskInvocation {
        context: shenko_types::agent::CallContext {
            caller_instance_id: store.capabilities.incarnation,
            role_revision: descriptor.role_revision,
            task_id: Some(task.task_id),
            expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id),
            controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: descriptor.authority_refs,
            trace_id: EntityId::new(),
        },
        task_invocation: claim.invocation.clone(),
        task: claim.task.clone(),
        operation_id: claim.operation_id,
        authority: shenko_types::operation::AuthorityRef {
            delegation_id: task.task_id,
            delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: task.created_by_step_id.unwrap(),
        },
        input,
        child_tools: Some(native.tool_runtime()),
    };
    let broker_run = tokio::spawn(broker.run());
    let mut execution = Box::pin(handler.execute(invocation));
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            result = &mut execution => panic!("the child ended before the provider answered: {result:?}"),
            wire = received.recv() => assert_eq!(wire.unwrap()["model"], "decided-child"),
        }
    })
    .await
    .unwrap();
    // Release the provider answer but stop polling the worker: the real
    // broker records E1 independently, exactly as it does after a crash.
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if child_step(&store, task.task_id).state == OperationState::Resolved {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        store.native_task_result(task.task_id).unwrap().is_none(),
        "the run result must not exist yet"
    );
    drop(execution);
    native.close_and_join(0).await;
    drop(native);
    drop(handler);
    drop(subject);
    broker_run.abort();
    let _ = broker_run.await;
    drop(handle);
    drop(store);

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    let (broker, next) =
        Broker::with_local_provider(LocalModelProvider::new(endpoint, "subject-model".into()), 1).unwrap();
    let (head, _) = reopened.load_subject(agent).unwrap();
    let restored = service(&reopened, next.clone(), head.software_generation);
    let broker_run = tokio::spawn(broker.run());
    drive_until(
        &restored,
        || reopened.native_task_result(task.task_id).unwrap().is_some(),
        || {
            format!(
                "steps={:?} pending={:?}",
                reopened
                    .external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION)
                    .map(|rows| rows.len()),
                reopened.external_task(task.task_id).map(|task| format!("{:?}", task.state)),
            )
        },
    )
    .await;
    let result = reopened.native_task_result(task.task_id).unwrap().unwrap();
    assert!(matches!(result.outcome, TaskOutcome::Succeeded));
    assert_eq!(result.result.as_ref().unwrap()["termination"], "completed");
    assert_eq!(
        result.result.as_ref().unwrap()["answer"],
        "scripted answer for decided-child"
    );
    assert_eq!(
        reopened
            .external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION)
            .unwrap()
            .len(),
        1,
        "recovery settles the decided run instead of adding a step"
    );
    assert_eq!(
        reopened
            .external_operation_attempts(child_step(&reopened, task.task_id).operation_id)
            .unwrap()
            .len(),
        1
    );
    assert!(
        received.try_recv().is_err(),
        "a decided run must not send a second generation after recovery"
    );
    restored.close_and_join(0).await;
    broker_run.abort();
    let _ = broker_run.await;
    server.abort();
    let _ = server.await;
}

/// A child cannot widen its own capability by spawning a nested child: the
/// nested tool choice stays inside what its parent was granted, and the child
/// reads the real refusal instead of an invented success or a reprieve.
#[tokio::test]
async fn a_nested_child_cannot_widen_its_parents_granted_tools() {
    let (_dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for text in [
            format!(
                "[tool:model.child:1] {}",
                serde_json::json!({
                    "goal": "嵌套观察。",
                    "system_prompt": "不要扩大能力。",
                    "model": "nested-child",
                    "max_tokens": 32,
                    "tools": [{"kind": "fixture.not_granted", "definition_version": 1}],
                })
            ),
            "没有可执行的嵌套委派，如实交回。".to_string(),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            wires_tx.send(read_wire(&mut socket).await).unwrap();
            let raw = serde_json::json!({
                "choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":12,"completion_tokens":7},
            }).to_string();
            socket
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let (broker, handle) =
        Broker::with_local_provider(LocalModelProvider::new(endpoint.clone(), "subject-model".into()), 1)
            .unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    commit_output(
        &store,
        &mut subject,
        1,
        &[],
        &format!(
            "[delegate] {}",
            serde_json::json!({
                "goal": "观察一次实验。",
                "system_prompt": "只报告观察事实。",
                "model": "narrowing-child",
                "max_tokens": 64,
                "budget": {"max_total_tokens": 256, "max_steps": 3, "max_tool_calls": 3},
                // Only the nested-child definition is granted. No other
                // capability is delegated to this child.
                "tools": [{"kind": "model.child", "definition_version": 1}],
            })
        ),
    )
    .await;
    let task = store.native_task_candidates_after(None, 8).unwrap().pop().unwrap().1;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    drive_until(
        &native,
        || store.native_task_result(task.task_id).unwrap().is_some(),
        || format!("steps={:?}", store.external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION).map(|rows| rows.len())),
    )
    .await;
    let first = wires_rx.recv().await.unwrap();
    assert_eq!(first["model"], "narrowing-child");
    let second = wires_rx.recv().await.unwrap();
    let rendered: Vec<String> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    let refusal = rendered
        .iter()
        .find(|content| content.starts_with("[tool_result:model.child:1]"))
        .unwrap_or_else(|| panic!("the child never read its refusal: {rendered:?}"));
    assert!(
        refusal.contains("may not request a tool its own parent was not granted"),
        "the child must read the real reason: {refusal}"
    );
    assert!(refusal.contains("fixture.not_granted:1"));
    // No nested run exists at all: the widening was refused before execution.
    let result = store.native_task_result(task.task_id).unwrap().unwrap();
    assert_eq!(result.result.as_ref().unwrap()["termination"], "completed");
    assert_eq!(
        store.external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION).unwrap().len(),
        2
    );
    // The parent subject-root task and this child exist; a refused widening
    // adds no third task at all.
    let parent = subject.head.subject_root_task_id.to_string();
    let children = store
        .list_task_summaries(agent, 64)
        .unwrap()
        .into_iter()
        .filter(|recent| recent.task_id != task.task_id.to_string() && recent.task_id != parent)
        .count();
    assert_eq!(children, 0, "a refused nested delegation creates no child task at all");
    native.close_and_join(0).await;
    run.abort();
    let _ = run.await;
    server.abort();
    let _ = server.await;
}
