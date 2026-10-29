//! Real Store/parser/broker/HTTP mechanics with scripted loopback responses.
//! These are not live-model behavior or provider-capacity evidence.
use super::{commit_output, execute_step, fixture, record_result, state};
use crate::{ModelChildHandler, NativeChildService};
use shenko_broker::{broker::{Broker, BrokerHandle}, local_provider::LocalModelProvider};
use shenko_store::{Store, child::{NATIVE_OPERATION_ID, memo_uuid}};
use shenko_types::{ids::*, operation::OperationState, task::*};
use std::{sync::Arc, time::Duration};
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
    service.install(Arc::new(ModelChildHandler::new(Arc::clone(store), handle).unwrap())).unwrap();
    service.recover().unwrap();
    service
}

async fn drive_until(service: &Arc<NativeChildService>, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            service.run_once().await.unwrap();
            if done() { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("bounded model child mechanics did not converge");
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
    drive_until(&native, || store.ready_native_results(agent, 8).unwrap().len() == 2).await;
    let wires = [wires_rx.recv().await.unwrap(), wires_rx.recv().await.unwrap()];
    server.await.unwrap();
    let results = store.ready_native_results(agent, 8).unwrap();
    for wire in &wires {
        let (semantic, goal, cap) = match wire["model"].as_str().unwrap() {
            "child-a" => ("Independent analyst. Keep literal evidence.", "Explain the supplied alpha evidence.", 96),
            "child-b" => ("Independent critic. Question assumptions.", "Review the supplied beta assumptions.", 128),
            other => panic!("unexpected provider model: {other}"),
        };
        assert!(wire["messages"][0]["content"].as_str().unwrap().ends_with(semantic));
        assert!(wire["messages"][0]["content"].as_str().unwrap().contains("本轮没有任何工具 schema"));
        assert_eq!(wire["messages"][1]["content"], goal);
        assert_eq!(wire["max_tokens"], cap);
        assert!(wire.get("tools").is_none());
        let result = results.iter().find(|result| result.result.as_ref().unwrap()["provider_binding"]["model"] == wire["model"]).unwrap();
        assert!(matches!(result.outcome, TaskOutcome::Succeeded));
        let task = store.external_task(result.task_id).unwrap();
        assert_eq!(task.owner, TaskOwner::Task { id: subject.head.subject_root_task_id });
        let operation = store.external_operation(result.operation_id).unwrap();
        let frozen: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&operation.input_digest).unwrap()).unwrap();
        assert_eq!(frozen["spec"]["system_prompt"], semantic);
        assert_eq!(frozen["payload"]["system_prompt"], wire["messages"][0]["content"]);
        assert_eq!(frozen["provider_binding"]["model"], wire["model"]);
        assert_eq!(store.external_operation_attempts(result.operation_id).unwrap().len(), 1);
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
    assert_eq!(next.active_count(), 0);
    let ready = reopened.ready_native_results(agent, 8).unwrap();
    assert_eq!(ready.iter().map(|r| r.result_id).collect::<Vec<_>>(), results.iter().map(|r| r.result_id).collect::<Vec<_>>());
    for result in ready { assert_eq!(reopened.external_operation_attempts(result.operation_id).unwrap().len(), 1); }
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
    let operation_id = memo_uuid(&task, NATIVE_OPERATION_ID).unwrap();
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
    let operation_id = memo_uuid(&task, NATIVE_OPERATION_ID).unwrap();
    drive_until(&native, || store.external_operation(operation_id).unwrap().state == OperationState::Reconciling).await;
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
    let _reader = ModelChildHandler::new(reopened.clone(), recovered.clone()).unwrap();
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

#[tokio::test]
async fn child_budget_cannot_exceed_its_sealed_parent_cap() {
    let (_dir, store, agent) = fixture();
    let (_, handle) = Broker::with_local_provider(LocalModelProvider::new("http://127.0.0.1:1".into(), "subject-model".into()), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
    subject.max_tokens = 32;
    let stop = tokio_util::sync::CancellationToken::new(); stop.cancel();
    execute_step(1, &[], &mut subject, &stop).await.unwrap();
    let old_step = record_result(&store, &mut subject, &delegation("child", "Independent semantic prompt.", "Finite task.", 33));
    assert!(execute_step(1, &[], &mut subject, &tokio_util::sync::CancellationToken::new()).await.unwrap().is_none());
    assert_eq!(subject.head.committed_step_no, 0);
    assert_ne!(subject.step.step_id, old_step);
    assert!(store.native_task_candidates_after(None, 8).unwrap().is_empty());
}
