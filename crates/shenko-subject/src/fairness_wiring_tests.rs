//! Real Store/broker/queue mechanics for the admission, fairness and
//! cancellation wiring (SPEC §13.3/§13.4/§41.9).
//!
//! Every case drives product paths: a real `ModelChildHandler`, the real
//! `NativeChildService` dispatcher, the real broker dispatch loop and a real
//! scripted loopback provider. They are mechanism evidence, not live-model
//! behaviour and not provider-capacity proof.
use super::{commit_output, fixture, state};
use crate::{ModelChildHandler, NativeChildService};
use shenko_broker::{
    broker::{Broker, BrokerHandle},
    local_provider::LocalModelProvider,
    request::*,
};
use shenko_store::{Store, external::MODEL_CHILD_STEP_OPERATION};
use shenko_types::{
    ids::*,
    operation::{AttemptDispatchStage, EffectStatus, OperationOutcome, OperationState},
    task::*,
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn read_wire(socket: &mut TcpStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "loopback request ended before its body");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + len {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + len]).unwrap();
            }
        }
    }
}

async fn answer(socket: &mut TcpStream, content: &str) {
    let raw = serde_json::json!({
        "id": "scripted-loopback-result",
        "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 7},
    })
    .to_string();
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}",
                raw.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    socket.shutdown().await.unwrap();
}

fn delegation(model: &str, prompt: &str, goal: &str, max_tokens: u64) -> String {
    format!(
        "[delegate] {}",
        serde_json::json!({"goal": goal, "system_prompt": prompt, "model": model, "max_tokens": max_tokens})
    )
}

fn service(
    store: &Arc<Store>,
    handle: BrokerHandle,
    generation: RuntimeGeneration,
) -> Arc<NativeChildService> {
    let service = Arc::new(NativeChildService::new(Arc::clone(store), generation));
    service
        .install(Arc::new(
            ModelChildHandler::new(Arc::clone(store), handle, cordis_core::Context::new()).unwrap(),
        ))
        .unwrap();
    service.recover().unwrap();
    service
}

fn child_steps(store: &Store, task_id: EntityId) -> Vec<shenko_types::operation::OperationRecord> {
    store
        .external_task_operations(task_id, MODEL_CHILD_STEP_OPERATION)
        .expect("the child run records its model steps")
}

async fn until(done: impl Fn() -> bool, state: impl Fn() -> String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "durable mechanics did not converge: {}",
            state()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// §13.3 step 1 dependency inheritance: a child created by another child's tool
/// call has a parent that is genuinely parked on it, so its generation is
/// dispatched under the subject tier. The same child declared directly by the
/// subject step is not promoted, because the subject keeps stepping.
#[tokio::test]
async fn only_a_parked_parent_lends_its_class_to_the_child_generation() {
    let (_dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let wire = read_wire(&mut socket).await;
            let model = wire["model"].as_str().unwrap_or_default().to_owned();
            let already_delegated = wire.to_string().contains("[tool_result:model.child:1]");
            if model == "parked-parent" && !already_delegated {
                // The parent's first generation asks for the one nested child
                // it was granted; after that result it answers, so the run
                // never delegates twice.
                let nested = serde_json::json!({
                    "goal": "Return one observed sentence.",
                    "system_prompt": "Report only what you were given.",
                    "model": "parked-nested",
                    "max_tokens": 32,
                });
                let content = format!("[tool:model.child:1] {nested}");
                answer(&mut socket, &content).await;
            } else {
                answer(&mut socket, "scripted answer").await;
            }
        }
    });
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint.clone(), "subject-model".into()),
        2,
    )
    .unwrap();
    let mut subject = state(store.clone(), agent)
        .unwrap()
        .with_broker(handle.clone());
    let parent = serde_json::json!({
        "goal": "Delegate one nested child, then answer.",
        "system_prompt": "Report only the facts you were given.",
        "model": "parked-parent",
        "max_tokens": 64,
        "tools": [{"kind": "model.child", "definition_version": 1}],
    });
    commit_output(&store, &mut subject, 1, &[], &format!("[delegate] {parent}")).await;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    native.run_once().await.unwrap();
    let task = store
        .native_task_candidates_after(None, 8)
        .unwrap()
        .pop()
        .unwrap()
        .1;
    until(
        || store.native_task_result(task.task_id).is_ok_and(|result| result.is_some()),
        || {
            format!(
                "steps={:?}",
                store.external_task_operations(task.task_id, MODEL_CHILD_STEP_OPERATION)
                    .map(|rows| rows.len())
            )
        },
    )
    .await;
    let result = store.native_task_result(task.task_id).unwrap().unwrap();
    assert!(
        matches!(result.outcome, TaskOutcome::Succeeded),
        "the parent run only completes when the nested child returned: {:?}",
        result.outcome
    );
    let steps = child_steps(&store, task.task_id);
    assert_eq!(steps.len(), 2, "the parent child took its two real steps");
    // The second step was sealed with the nested child's tool result, so the
    // nested generation really ran as a parked dependency of this task.
    let sealed = store.cas.read_object(&steps[1].final_input_ref.digest).unwrap();
    let sealed: serde_json::Value = serde_json::from_slice(&sealed).unwrap();
    assert!(
        sealed["payload"]["messages"]
            .to_string()
            .contains("[tool_result:model.child:1]"),
        "the parent continued from the nested child's real result"
    );
    let fairness = handle.fairness();
    assert_eq!(
        fairness.promoted, 1,
        "only the nested child inherited its parked parent's class: {fairness:?}"
    );
    native.close_and_join(0).await;
    run.abort();
    let _ = run.await;
    server.abort();
    let _ = server.await;
}

/// §13.4/§41.9: a request whose measured cost exceeds the installed effective
/// window is refused before it exists as work. The step keeps its armed attempt
/// and its frozen input, and neither this process nor a reopen invents unknown
/// capacity debt for a generation that was never sent.
#[tokio::test]
async fn a_measured_overflow_is_never_enqueued_and_owns_no_unknown_debt() {
    let (dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            sent.send(read_wire(&mut socket).await).unwrap();
            answer(&mut socket, "unreachable under the installed window").await;
        }
    });
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint.clone(), "subject-model".into()),
        1,
    )
    .unwrap();
    let mut subject = state(store.clone(), agent)
        .unwrap()
        .with_broker(handle.clone());
    commit_output(
        &store,
        &mut subject,
        1,
        &[],
        &delegation(
            "windowed-child",
            "Report only the supplied facts.",
            "Return this finite review.",
            64,
        ),
    )
    .await;
    // An observed effective window far below any real child payload: the
    // sealing side's measured cost cannot fit, so nothing may be enqueued.
    handle.install_effective_window(Some(8));
    assert_eq!(handle.effective_window(), Some(8));
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    native.run_once().await.unwrap();
    let task = store
        .native_task_candidates_after(None, 8)
        .unwrap()
        .pop()
        .unwrap()
        .1;
    until(
        || {
            store.external_task(task.task_id).is_ok_and(|task| {
                task.abort_request.is_none() && !matches!(task.state, TaskState::Running { .. })
            })
        },
        || format!("task={:?}", store.external_task(task.task_id).map(|task| task.state)),
    )
    .await;
    let step = child_steps(&store, task.task_id).pop().unwrap();
    assert_eq!(
        step.state,
        OperationState::InFlight,
        "a refused generation arms its attempt but never resolves an operation"
    );
    let attempt = store.external_operation_attempt(step.operation_id).unwrap();
    assert_eq!(attempt.dispatch_stage, AttemptDispatchStage::Armed);
    assert!(
        attempt.final_receipt_id.is_none(),
        "nothing crossed the send boundary"
    );
    assert_eq!(
        handle.active_count(),
        0,
        "a proven pre-enqueue refusal is not unknown capacity debt"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), received.recv())
            .await
            .is_err(),
        "the refused generation reached the provider"
    );
    native.close_and_join(0).await;
    drop(native);
    drop(subject);
    run.abort();
    let _ = run.await;
    drop(handle);
    drop(store);
    // A reopen must not turn the same armed-but-unsent step into debt.
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    reopened.start_subject_dispatcher().unwrap();
    let (restored_broker, restored) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "subject-model".into()),
        1,
    )
    .unwrap();
    let (head, _) = reopened.load_subject(agent).unwrap();
    let restored_service = service(&reopened, restored.clone(), head.software_generation);
    assert_eq!(
        restored.active_count(),
        0,
        "the reopen reconstructed debt for a generation that never existed"
    );
    assert!(reopened.native_task_result(task.task_id).unwrap().is_none());
    assert_eq!(
        reopened
            .external_operation_attempt(step.operation_id)
            .unwrap()
            .dispatch_stage,
        AttemptDispatchStage::Armed
    );
    restored_service.close_and_join(0).await;
    drop(restored_service);
    drop(restored);
    drop(restored_broker);
    server.abort();
    let _ = server.await;
}

/// §41.9 `queued → terminal(cancelled before dispatch)`: a durable abort of a
/// child whose generation is still queued settles it through the broker, and
/// only `Settled` lets the step carry never-dispatched evidence. An occupied
/// slot keeps the generation provably in the queue.
#[tokio::test]
async fn a_durable_abort_settles_a_queued_generation_as_never_dispatched() {
    let (_dir, store, agent) = fixture();
    store.start_subject_dispatcher().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires, mut wire_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut wait = Some(wait);
        while let Ok((mut socket, _)) = listener.accept().await {
            wires.send(read_wire(&mut socket).await).unwrap();
            if let Some(wait) = wait.take() {
                wait.await.unwrap();
            }
            answer(&mut socket, "scripted answer").await;
        }
    });
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint.clone(), "subject-model".into()),
        1,
    )
    .unwrap();
    let mut subject = state(store.clone(), agent)
        .unwrap()
        .with_broker(handle.clone());
    commit_output(
        &store,
        &mut subject,
        1,
        &[],
        &delegation(
            "queued-child",
            "Report only the supplied facts.",
            "Return this finite review.",
            64,
        ),
    )
    .await;
    let native = service(&store, handle.clone(), subject.head.software_generation.clone());
    let run = tokio::spawn(broker.run());
    // Occupy the single slot with a request whose provider answer never comes.
    let (held_tx, _held_rx) = tokio::sync::oneshot::channel();
    handle
        .submit(InferenceRequest {
            request_id: EntityId::new(),
            agent_id: agent,
            conversation_id: agent,
            provider_binding: handle.binding().clone(),
            priority: InferencePriority::Normal,
            epoch: ControllerEpoch(7),
            payload: InferencePayload {
                system_prompt: "hold the only slot".into(),
                messages: vec![InferenceMessage::text("user", "hold")],
                max_tokens: Some(64),
                tools: Vec::new(),
                native_options: Default::default(),
            },
            result_tx: held_tx,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), wire_rx.recv())
        .await
        .unwrap()
        .expect("the slot holder reached the provider");
    native.run_once().await.unwrap();
    let task = store
        .native_task_candidates_after(None, 8)
        .unwrap()
        .pop()
        .unwrap()
        .1;
    let step = child_steps(&store, task.task_id).pop().unwrap();
    until(
        || handle.fairness().queued.iter().copied().sum::<usize>() == 1,
        || format!("fairness={:?}", handle.fairness()),
    )
    .await;
    assert_eq!(handle.active_count(), 2, "one running, one queued");
    native
        .cancel(task.task_id, "owner cancelled", AbortScope::Task)
        .unwrap();
    until(
        || {
            store
                .external_operation(step.operation_id)
                .is_ok_and(|operation| operation.state == OperationState::Resolved)
        },
        || {
            format!(
                "operation={:?}",
                store
                    .external_operation(step.operation_id)
                    .map(|row| row.state)
            )
        },
    )
    .await;
    let settled = store.external_operation(step.operation_id).unwrap();
    assert!(
        matches!(settled.outcome, Some(OperationOutcome::Cancelled)),
        "the queued generation settled as cancelled before dispatch: {:?}",
        settled.outcome
    );
    assert_eq!(settled.effect_status, EffectStatus::None);
    assert_eq!(
        handle.active_count(),
        1,
        "the cancelled generation released its own admission; only the slot holder remains"
    );
    assert!(
        store.native_task_result(task.task_id).unwrap().is_none(),
        "a cancelled-before-dispatch generation never settles the task"
    );
    assert!(store
        .external_task(task.task_id)
        .unwrap()
        .abort_request
        .is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(150), wire_rx.recv())
            .await
            .is_err(),
        "the settled generation was sent after all"
    );
    native.close_and_join(0).await;
    // Release the holder so the surviving request completes on its own path.
    release.send(()).unwrap();
    until(
        || handle.active_count() == 0,
        || format!("active={}", handle.active_count()),
    )
    .await;
    run.abort();
    let _ = run.await;
    server.abort();
    let _ = server.await;
}

/// The closed-admission counterexample from the parent's real run: when the
/// dispatch loop is gone, a submission is refused before it exists as work, and
/// that refusal is a typed proof rather than a lost dispatch.
#[tokio::test]
async fn a_closed_dispatch_loop_refuses_before_enqueue_without_losing_a_dispatch() {
    let (offline, handle) = Broker::offline();
    let (result_tx, _result_rx) = tokio::sync::oneshot::channel();
    let request = InferenceRequest {
        request_id: EntityId::new(),
        agent_id: EntityId::new(),
        conversation_id: EntityId::new(),
        provider_binding: handle.binding().clone(),
        priority: InferencePriority::Normal,
        epoch: ControllerEpoch(1),
        payload: InferencePayload {
            system_prompt: "closed".into(),
            messages: Vec::new(),
            max_tokens: Some(8),
            tools: Vec::new(),
            native_options: Default::default(),
        },
        result_tx,
    };
    drop(offline);
    let error = handle.submit(request).unwrap_err();
    assert!(
        BrokerHandle::never_enqueued(&error),
        "a closed loop is a proven pre-enqueue refusal: {error}"
    );
    assert_eq!(
        error
            .downcast_ref::<shenko_broker::broker::NotEnqueued>()
            .map(|refusal| refusal.code.as_str()),
        Some("BROKER_ADMISSION_CLOSED")
    );
    assert_eq!(handle.active_count(), 0);
}
