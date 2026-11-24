//! Durable crash-window fixtures, not provider/model or physical-crash proof.
use super::{execute_step, fixture, state, SubjectLoopState};
use shenko_broker::{
    broker::{Broker, InferenceReceiptSink},
    local_provider::LocalModelProvider,
    request::*,
};
use shenko_store::{Store, subject_request::{SubjectRequestAttempt, SubjectRequestState}};
use shenko_types::{commit::CommitRef, ids::*, object::Digest, subject::*};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

fn broker() -> (Broker, shenko_broker::broker::BrokerHandle) {
    Broker::with_local_provider(
        LocalModelProvider::new("http://127.0.0.1:1".into(), "recovery-fixture".into()), 1,
    ).unwrap()
}

async fn arm(subject: &mut SubjectLoopState) -> SubjectRequestAttempt {
    let cancel = CancellationToken::new();
    cancel.cancel();
    execute_step(1, &[], subject, &cancel).await.unwrap();
    let input = super::super::frozen_input(&subject.store, &subject.step).unwrap();
    let attempt = SubjectRequestAttempt {
        schema_version: 1,
        request_id: input.request_id,
        attempt_id: EntityId::new(),
        agent_id: subject.agent_id,
        step_id: subject.step.step_id,
        send_id: None,
        state: SubjectRequestState::Armed,
        input_plan_ref: subject.step.input_plan_ref.clone().unwrap(),
        payload_digest: Digest::blake3(&serde_json::to_vec(&input.payload).unwrap()),
        provider_binding_digest: Digest::blake3(&serde_json::to_vec(&input.provider_binding).unwrap()),
        terminal: None,
        created_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
    };
    let old = subject.step.record_revision;
    subject.step.state = SubjectStepState::AwaitingModel;
    subject.step.active_request_id = Some(attempt.request_id);
    subject.step.record_revision = RecordRevision(old.0 + 1);
    subject.store.upsert_step_arming_request(&subject.step, &subject.head, old, None, &attempt).unwrap();
    attempt
}

fn receipt(subject: &SubjectLoopState, attempt: &SubjectRequestAttempt, status: InferenceStatus, kind: TerminationKind) {
    let input = super::super::frozen_input(&subject.store, &subject.step).unwrap();
    let binding = input.provider_binding.unwrap();
    let result = InferenceResult {
        request_id: attempt.request_id, status, response: None,
        effective: EffectiveInference {
            profile_id: binding.profile_id().into(),
            profile_revision: binding.profile_revision(),
            api_version: binding.api_version().map(str::to_owned),
            max_tokens: input.payload.max_tokens.unwrap_or_else(|| binding.default_max_tokens()),
            native_options: input.payload.native_options.clone(),
            provider_binding: binding.clone(),
        },
        termination: TerminationEvidence { kind, stop_reason: None, max_tokens: None, output_tokens: None, length: false },
        purpose: RequestPurpose::Subject, stream: None, quota_headers: vec![],
    };
    let sink = crate::subject_receipt::SubjectReceiptSink {
        store: subject.store.clone(), agent_id: subject.agent_id, step_id: subject.step.step_id,
        request_id: attempt.request_id, input_plan_ref: attempt.input_plan_ref.clone(),
        payload_digest: attempt.payload_digest.clone(), provider_binding_digest: attempt.provider_binding_digest.clone(),
        binding, payload: input.payload,
    };
    sink.record(&result).unwrap();
}

#[tokio::test]
async fn armed_unsent_reopen_records_positive_retry_without_capacity_debt() {
    let (dir, store, agent) = fixture();
    let (_old_broker, handle) = broker();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let attempt = arm(&mut subject).await;
    let original_seal = subject.step.input_plan_ref.clone();
    drop(subject);
    drop(store);
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (_new_broker, handle) = broker();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    execute_step(1, &[], &mut subject, &CancellationToken::new()).await.unwrap();
    assert_eq!(handle.active_count(), 0, "a request that never sent cannot consume a broker slot");
    assert_eq!(subject.step.state, SubjectStepState::RetryPending);
    assert_eq!(subject.step.input_plan_ref, original_seal);
    let proof: SubjectRetryEvidence = serde_json::from_slice(
        &store.cas.read_object(&subject.step.retry_reason_ref.as_ref().unwrap().digest).unwrap(),
    ).unwrap();
    assert_eq!(proof.request_id, attempt.request_id);
    assert!(matches!(proof.fact, InferenceRetryFact::NeverDispatched));
    assert_eq!(store.subject_request_attempt(attempt.request_id).unwrap().unwrap().state, SubjectRequestState::Armed);
    assert_eq!(store.subject_request_attempts_for_step(agent, attempt.step_id).unwrap().len(), 1);
}

#[tokio::test]
async fn terminal_refusals_reopen_with_live_policy_instead_of_model_text_adoption() {
    let cases = [
        (InferenceStatus::Rejected { http_status: 429 }, TerminationKind::TransportRejected, true, "provider rejection persisted"),
        (InferenceStatus::Rejected { http_status: 401 }, TerminationKind::TransportRejected, false, "PROVIDER_REJECTED"),
        (InferenceStatus::Unavailable, TerminationKind::NeverDispatched, false, "MODEL_PROVIDER_UNAVAILABLE"),
        (InferenceStatus::InvalidRequest { code: WireErrorCode::MessageRuleViolation }, TerminationKind::NeverDispatched, false, "PROVIDER_MESSAGE_RULE_VIOLATION"),
    ];
    for (status, kind, retry, expected_error) in cases {
        let (dir, store, agent) = fixture();
        let (_old_broker, handle) = broker();
        let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
        let attempt = arm(&mut subject).await;
        receipt(&subject, &attempt, status, kind);
        let terminal = store.subject_request_attempt(attempt.request_id).unwrap().unwrap().terminal.unwrap();
        drop(subject);
        drop(store);
        let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let (_new_broker, handle) = broker();
        let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
        let error = execute_step(1, &[], &mut subject, &CancellationToken::new()).await.unwrap_err();
        assert!(error.to_string().contains(expected_error), "wrong typed refusal: {error}");
        assert_eq!(handle.active_count(), 0);
        assert!(subject.step.model_result_ref.is_none());
        assert_eq!(subject.head.committed_step_no, 0);
        assert_eq!(subject.step.state, if retry { SubjectStepState::RetryPending } else { SubjectStepState::AwaitingModel });
        if retry {
            let proof: SubjectRetryEvidence = serde_json::from_slice(&store.cas.read_object(
                &subject.step.retry_reason_ref.as_ref().unwrap().digest,
            ).unwrap()).unwrap();
            assert_eq!(proof.request_id, attempt.request_id);
            assert!(matches!(proof.fact, InferenceRetryFact::ProviderRejected { http_status: 429 }));
        }
        let retained = store.subject_request_attempt(attempt.request_id).unwrap().unwrap();
        assert_eq!(retained.state, SubjectRequestState::Terminal);
        assert_eq!(retained.terminal.unwrap().result_ref, terminal.result_ref);
    }
}

#[tokio::test]
async fn unknown_dispatch_recovery_keeps_capacity_and_original_evidence() {
    for terminal_unknown in [false, true] {
        let (dir, store, agent) = fixture();
        let (_old_broker, handle) = broker();
        let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
        let attempt = arm(&mut subject).await;
        store.mark_subject_request_dispatched(attempt.request_id).unwrap();
        if terminal_unknown { receipt(&subject, &attempt, InferenceStatus::Failed, TerminationKind::Unknown); }
        let prior = store.subject_request_attempt(attempt.request_id).unwrap().unwrap();
        drop(subject);
        drop(store);
        let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let (_new_broker, handle) = broker();
        let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
        execute_step(1, &[], &mut subject, &CancellationToken::new()).await.unwrap();
        assert_eq!(handle.active_count(), 1, "unknown dispatch is not proof of release");
        assert_eq!(subject.step.state, SubjectStepState::RetryPending);
        let proof: SubjectRetryEvidence = serde_json::from_slice(&store.cas.read_object(
            &subject.step.retry_reason_ref.as_ref().unwrap().digest,
        ).unwrap()).unwrap();
        assert!(matches!(proof.fact, InferenceRetryFact::DispatchLost { .. }));
        assert_eq!(proof.request_id, attempt.request_id);
        let retained = store.subject_request_attempt(attempt.request_id).unwrap().unwrap();
        assert_eq!(retained.state, prior.state);
        assert_eq!(retained.attempt_id, prior.attempt_id);
        assert_eq!(retained.terminal.as_ref().and_then(|t|t.result_ref.as_ref()), prior.terminal.as_ref().and_then(|t|t.result_ref.as_ref()));
        assert_eq!(store.subject_request_attempts_for_step(agent, attempt.step_id).unwrap().len(), 1);
    }
}

/// The live loop must STOP on unreconciled dispatch debt instead of arming a
/// second request for the same sealed step. The single-step recovery above
/// proves the durable conversion; this case proves the next live iteration
/// cannot turn that conversion into a second send.
#[tokio::test]
async fn the_live_loop_stops_on_unknown_dispatch_debt_without_a_second_request() {
    for terminal_unknown in [false, true] {
        let (dir, store, agent) = fixture();
        let (_old_broker, handle) = broker();
        let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
        let attempt = arm(&mut subject).await;
        store.mark_subject_request_dispatched(attempt.request_id).unwrap();
        if terminal_unknown {
            receipt(&subject, &attempt, InferenceStatus::Failed, TerminationKind::Unknown);
        }
        let prior = store.subject_request_attempt(attempt.request_id).unwrap().unwrap();
        drop(subject);
        drop(store);

        let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let (_new_broker, handle) = broker();
        let subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            super::super::run_subject_loop(subject, CancellationToken::new()),
        )
        .await
        .expect("the live loop stops instead of pacing against unknown debt")
        .expect_err("unknown dispatch debt must stop the live loop");
        assert!(
            error.to_string().contains("unreconciled dispatch debt"),
            "wrong stop cause: {error}"
        );

        let attempts = store.subject_request_attempts_for_step(agent, attempt.step_id).unwrap();
        assert_eq!(attempts.len(), 1, "the live loop armed a second request");
        assert_eq!(attempts[0].request_id, attempt.request_id);
        let retained = store.subject_request_attempt(attempt.request_id).unwrap().unwrap();
        assert_eq!(retained.state, prior.state);
        assert_eq!(retained.attempt_id, prior.attempt_id);
        assert_eq!(
            retained.terminal.as_ref().and_then(|t| t.result_ref.as_ref()),
            prior.terminal.as_ref().and_then(|t| t.result_ref.as_ref())
        );
        assert_eq!(handle.active_count(), 1, "the original unknown charge was released");
        let (head, step) = store.load_subject(agent).unwrap();
        assert_eq!(head.committed_step_no, 0, "the head advanced past unknown debt");
        assert_eq!(step.state, SubjectStepState::RetryPending);
    }
}

#[tokio::test]
async fn broker_admission_rejection_stops_with_durable_primary_cause() {
    use shenko_types::error::{ErrorScope, RetryStrategy, ShenkoError};

    let (dir, store, agent) = fixture();
    let (broker, handle) = broker();
    let subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let step_id = subject.step.step_id;
    let session_id = EntityId(subject.head.branch_id.0);
    // Closing the actual broker queue makes admission fail without a provider
    // call. The subject must still keep the armed request and its unknown debt.
    drop(broker);
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        super::super::run_subject_loop(subject, CancellationToken::new()),
    ).await.expect("a rejected admission stops instead of replaying").unwrap_err();
    let ShenkoError::Protocol(protocol) = &error else {
        panic!("admission must retain its typed request error: {error}");
    };
    assert_eq!(protocol.code, "BROKER_ADMISSION_REJECTED");
    assert_eq!(protocol.scope, ErrorScope::Request);
    assert_eq!(protocol.retry, RetryStrategy::AfterReconcile);
    let primary = protocol.details["admission_error"].as_str().unwrap();
    assert!(primary.contains("BROKER_ADMISSION_CLOSED"), "wrong admission cause: {primary}");
    let request_id: EntityId = serde_json::from_value(protocol.details["request_id"].clone()).unwrap();
    assert_eq!(protocol.details["outcome"], "unknown");
    assert_eq!(protocol.details["unknown_charge_restored"], true);
    assert!(protocol.details["recovery_error"].is_null());
    assert!(protocol.details["retry_record_error"].is_null());
    assert_eq!(handle.active_count(), 1);
    assert_eq!(store.error_journal_dropped(), 0);
    drop(handle);
    drop(store);

    let reopened = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let (head, step) = reopened.load_subject(agent).unwrap();
    assert_eq!(head.committed_step_no, 0);
    assert_eq!(head.next_step_id, step_id);
    assert_eq!(step.state, SubjectStepState::RetryPending);
    assert_eq!(step.active_request_id, Some(request_id));
    let attempts = reopened.subject_request_attempts_for_step(agent, step_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].request_id, request_id);
    assert_eq!(attempts[0].state, SubjectRequestState::Armed);
    let events = reopened.list_session_events(session_id, &shenko_store::SessionEventFilter {
        from_seq: 0,
        to_seq: i64::MAX,
        limit: 20,
        event_types: vec![shenko_store::RUNTIME_ERROR_EVENT_TYPE.to_owned()],
        roles: vec![],
    }).unwrap();
    assert_eq!(events.len(), 1, "a rejected admission journals one loop stop");
    let data: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(data["kind"], "subject_loop");
    assert!(data["message"].as_str().unwrap().contains(primary));
    assert_eq!(data["extra"]["error"]["code"], protocol.code);
    assert_eq!(data["extra"]["error"]["scope"], "request");
    assert_eq!(data["extra"]["error"]["retry"], "after_reconcile");
    assert_eq!(data["extra"]["error"]["details"], protocol.details);
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn broker_admission_retry_state_read_failure_preserves_primary_and_journal() {
    use shenko_types::error::{ErrorScope, RetryStrategy, ShenkoError};

    let (dir, store, agent) = fixture();
    let (broker, handle) = broker();
    let subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let step_id = subject.step.step_id;
    let session_id = EntityId(subject.head.branch_id.0);
    // Corrupt only the armed step's JSON projection in its real arming
    // update. Both retry recording and the following load must then
    // fail to decode it, while the head and error journal remain writable.
    let connection = rusqlite::Connection::open(dir.join("store.db")).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER corrupt_armed_step_projection AFTER UPDATE ON subject_steps
         WHEN OLD.state='preparing' AND NEW.state='awaiting_model'
         BEGIN
             UPDATE subject_steps
             SET data=json_set(NEW.data, '$.record_revision', 'unreadable retry-state fixture')
             WHERE step_id=NEW.step_id;
         END;",
    ).unwrap();
    drop(connection);
    drop(broker);

    // Exercise execute_step, the compound retry_continuation error branch,
    // and the production journal projection through the actual subject loop.
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        super::super::run_subject_loop(subject, CancellationToken::new()),
    ).await.expect("an unreadable retry state must stop instead of replaying").unwrap_err();
    let ShenkoError::Protocol(protocol) = &error else {
        panic!("the failed retry-state load replaced the primary error: {error}");
    };
    assert_eq!(protocol.code, "BROKER_ADMISSION_REJECTED");
    assert_eq!(protocol.scope, ErrorScope::Request);
    assert_eq!(protocol.retry, RetryStrategy::AfterReconcile);
    let primary = protocol.details["admission_error"].as_str().unwrap();
    assert!(primary.contains("BROKER_ADMISSION_CLOSED"), "wrong admission cause: {primary}");
    let request_id: EntityId = serde_json::from_value(protocol.details["request_id"].clone()).unwrap();
    assert_eq!(protocol.details["outcome"], "unknown");
    assert_eq!(protocol.details["unknown_charge_restored"], true);
    assert!(protocol.details["recovery_error"].is_null());
    assert!(protocol.details["retry_record_error"].as_str().unwrap()
        .contains("unreadable retry-state fixture"));
    assert_eq!(handle.active_count(), 1);
    assert!(store.ledger.writer_is_open(), "a projection decode failure leaves journaling available");
    assert_eq!(store.error_journal_dropped(), 0);
    drop(handle);
    drop(store);

    let reopened = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    assert!(matches!(reopened.load_subject(agent), Err(ShenkoError::Serde(_))),
        "the malformed durable projection was not silently repaired");
    let attempts = reopened.subject_request_attempts_for_step(agent, step_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].request_id, request_id);
    assert_eq!(attempts[0].state, SubjectRequestState::Armed);
    let events = reopened.list_session_events(session_id, &shenko_store::SessionEventFilter {
        from_seq: 0,
        to_seq: i64::MAX,
        limit: 20,
        event_types: vec![shenko_store::RUNTIME_ERROR_EVENT_TYPE.to_owned()],
        roles: vec![],
    }).unwrap();
    assert_eq!(events.len(), 1, "the compound failure journals one loop stop");
    let data: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(data["kind"], "subject_loop");
    assert_eq!(data["journal_dropped_total"], 0);
    assert_eq!(data["extra"]["error"]["code"], "BROKER_ADMISSION_REJECTED");
    assert_eq!(data["extra"]["error"]["scope"], "request");
    assert_eq!(data["extra"]["error"]["retry"], "after_reconcile");
    assert_eq!(data["extra"]["error"]["details"], protocol.details);
    let secondary = data["extra"]["retry_state_error"]["message"].as_str().unwrap();
    assert!(secondary.starts_with("serde error:"), "wrong retry-state failure: {secondary}");
    assert!(secondary.contains("unreadable retry-state fixture"));
    assert_eq!(data["message"], format!(
        "subject loop stopped on error: {error}: {primary}; durable retry state unreadable: {secondary}"
    ));
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Read one JSON request body from a loopback provider connection.
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

/// A scripted provider: each accepted connection answers with the next
/// scripted response; the last response is reused after the script runs out.
async fn serve(
    listener: TcpListener,
    script: Vec<(&'static str, String)>,
    wires: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
) {
    let mut index = 0usize;
    while let Ok((mut socket, _)) = listener.accept().await {
        let wire = read_wire(&mut socket).await;
        wires.send(wire).unwrap();
        let (status, body) = script
            .get(index)
            .cloned()
            .unwrap_or_else(|| script.last().cloned().expect("a scripted answer"));
        index += 1;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
    }
}

/// The product output the scripted provider returns: a subject-authored
/// semantic wait, so the retried attempt must also commit through the real
/// parser and the real S1 wait settlement.
const OBSERVING: &str = "[wait] {\"condition\":\"出现足以判断的新证据\",\"max_wait_secs\":120}\n继续观察，不作无依据的结论。";

fn observing_body() -> String {
    serde_json::json!({
        "id": "subject-continuation-fixture",
        "choices": [{"message": {"role": "assistant", "content": OBSERVING}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 23, "completion_tokens": 11},
    })
    .to_string()
}

/// A recorded provider rejection (429 / capacity) is reconciled inside the
/// running subject instead of ending it: the same sealed step continues under
/// a NEW request identity, the committed S1 still carries the subject's
/// semantic wait, and the original refusal stays durable evidence of the first
/// attempt. R143, RT16, A01.
#[tokio::test]
async fn a_recorded_provider_rejection_continues_the_same_sealed_subject_step() {
    let (_dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(serve(
        listener,
        vec![
            (
                "429 Too Many Requests",
                serde_json::json!({"error": "rate limited"}).to_string(),
            ),
            ("200 OK", observing_body()),
        ],
        wires_tx,
    ));
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "continuation-fixture".into()),
        1,
    )
    .unwrap();
    let broker_task = tokio::spawn(broker.run());
    let subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let cancel = CancellationToken::new();
    let loop_task = tokio::spawn(super::super::run_subject_loop(subject, cancel.clone()));

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (head, _) = store.load_subject(agent).unwrap();
            if head.committed_step_no >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a recorded provider rejection must not stop the subject loop");
    cancel.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), loop_task)
        .await
        .expect("the subject loop settles at the requested shutdown");
    assert!(
        matches!(&stopped, Ok(Ok(()))),
        "unexpected subject loop end: {stopped:?}"
    );
    server.abort();
    broker_task.abort();

    // The retried attempt is the SAME sealed request projection, not a fresh
    // S0 or a new successor step.
    let first = wires_rx.try_recv().expect("the first attempt reached the provider");
    let second = wires_rx.try_recv().expect("the retried attempt reached the provider");
    assert_eq!(
        first["messages"], second["messages"],
        "a retry must reuse the sealed request projection"
    );
    assert_eq!(first["model"], second["model"]);

    let (head, successor) = store.load_subject(agent).unwrap();
    let wait = store
        .semantic_wait(agent)
        .unwrap()
        .expect("the committed S1 created the durable wait");
    assert!(wait.is_active());
    let step = store
        .get_step(&head.branch_id, wait.created_step_id)
        .unwrap()
        .expect("the wait names the step that decided it");
    assert_eq!(step.step_no, 1);
    assert_eq!(step.state, SubjectStepState::Committed);
    assert_eq!(
        successor.step_no,
        head.committed_step_no + 1,
        "the head advanced to the one registered successor"
    );

    let evidence: SubjectRetryEvidence = serde_json::from_slice(
        &store
            .cas
            .read_object(
                &step
                    .retry_reason_ref
                    .as_ref()
                    .expect("the committed step keeps its refusal evidence")
                    .digest,
            )
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        evidence.fact,
        InferenceRetryFact::ProviderRejected { http_status: 429 }
    ));
    assert_ne!(
        Some(evidence.request_id),
        step.active_request_id,
        "the retry is a new request identity, never a replay of the spent one"
    );
    assert_eq!(
        store.subject_request_attempts_for_step(agent, step.step_id).unwrap().len(),
        2,
        "one armed attempt per request identity"
    );
    assert!(wait.deadline_at - wait.started_at <= chrono::TimeDelta::seconds(600));
    assert!(matches!(
        store
            .semantic_wait_receipt(step.step_id)
            .unwrap()
            .expect("the committed step has its exact wait receipt")
            .outcome,
        SemanticWaitOutcome::Started
    ));

    // The degradation is a durable fact the subject can read, not a
    // stderr warning only.
    let history = store
        .session_events_range(EntityId(head.branch_id.0), 0, i64::MAX)
        .unwrap();
    assert!(
        history.iter().any(|event| {
            let data: serde_json::Value = serde_json::from_str(&event.data).unwrap_or_default();
            data.get("kind").and_then(|value| value.as_str()) == Some("subject_retry")
        }),
        "the retry is visible as a runtime fact"
    );
}

/// A refusal without retry evidence stops the subject for reconciliation: it is
/// never silently re-dispatched and it does not advance the head. The retry
/// continuation must not turn an unknown or refused outcome into a loop.
#[tokio::test]
async fn a_refusal_without_retry_evidence_stops_the_subject_instead_of_replaying() {
    let (_dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(serve(
        listener,
        vec![(
            "401 Unauthorized",
            serde_json::json!({"error": "invalid api key"}).to_string(),
        )],
        wires_tx,
    ));
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "continuation-fixture".into()),
        1,
    )
    .unwrap();
    let broker_task = tokio::spawn(broker.run());
    let subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        super::super::run_subject_loop(subject, CancellationToken::new()),
    )
    .await
    .expect("a non-retryable refusal stops the loop");
    assert!(outcome
        .expect_err("a refusal is not a continuation")
        .to_string()
        .contains("PROVIDER_REJECTED"));
    server.abort();
    broker_task.abort();

    let (head, step) = store.load_subject(agent).unwrap();
    assert_eq!(head.committed_step_no, 0);
    assert_eq!(step.state, SubjectStepState::AwaitingModel);
    assert!(step.retry_reason_ref.is_none(), "a refusal records no retry evidence");
    wires_rx.try_recv().expect("the refusal reached the provider");
    assert!(
        wires_rx.try_recv().is_err(),
        "a non-retryable refusal is not replayed"
    );
}

/// The subject's own semantic wait is a continuation, not a wake-up: with NO
/// input at all the same serial loop keeps asking the same model, every
/// continuance preserves the one fixed deadline, and a process restart takes
/// over the same wait record instead of restarting its 600s budget.
/// R143, RT16.
#[tokio::test]
async fn the_loop_keeps_polling_an_active_semantic_wait_and_a_restart_keeps_its_deadline() {
    const CONTINUING: &str =
        "[wait:continue]\n继续观察，不作无依据的结论。";
    let (dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, _wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let continuing = serde_json::json!({
        "id": "subject-wait-fixture",
        "choices": [{"message": {"role": "assistant", "content": CONTINUING}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 23, "completion_tokens": 11},
    })
    .to_string();
    let server = tokio::spawn(serve(
        listener,
        vec![("200 OK", observing_body()), ("200 OK", continuing)],
        wires_tx,
    ));
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "continuation-fixture".into()),
        1,
    )
    .unwrap();
    let broker_task = tokio::spawn(broker.run());
    // The inbox stays empty for the whole run: every step below is produced by
    // the subject continuing itself, not by an incoming event.
    let subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let cancel = CancellationToken::new();
    let loop_task = tokio::spawn(super::super::run_subject_loop(subject, cancel.clone()));

    let mut first_observation = None;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let (head, _) = store.load_subject(agent).unwrap();
            if head.committed_step_no >= 1 && first_observation.is_none() {
                first_observation = store.semantic_wait(agent).unwrap();
            }
            if head.committed_step_no >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the subject keeps polling an active semantic wait without new input");
    let (head, _) = store.load_subject(agent).unwrap();
    let wait = store.semantic_wait(agent).unwrap().expect("the wait is durable");
    assert!(wait.is_active());
    let first = first_observation.expect("the wait was observable after the first committed step");
    assert_eq!(first.wait_id, wait.wait_id, "polling never mints a second wait");
    assert_eq!(first.started_at, wait.started_at, "polling never restarts the wait");
    assert_eq!(first.deadline_at, wait.deadline_at, "polling never renews the deadline");
    assert_eq!(
        wait.deadline_at - wait.started_at,
        chrono::TimeDelta::seconds(120),
        "the deadline is the duration the subject chose, fixed once",
    );
    let steps = store.sealed_subject_steps(&head.branch_id).unwrap();
    let receipts: Vec<SemanticWaitReceipt> = steps
        .iter()
        .filter_map(|step| store.semantic_wait_receipt(step.step_id).unwrap())
        .collect();
    assert!(
        receipts.iter().any(|receipt| matches!(receipt.outcome, SemanticWaitOutcome::Continued)),
        "a later step must continue the same wait: {receipts:?}"
    );
    assert_eq!(
        receipts.iter().filter(|receipt| matches!(receipt.outcome, SemanticWaitOutcome::Started)).count(),
        1,
        "no step may start a second wait over an active one: {receipts:?}"
    );
    assert!(receipts.iter().all(|receipt| receipt
        .wait
        .as_ref()
        .is_none_or(|record| record.deadline_at == wait.deadline_at)));

    // Stop at a planned boundary, then take over the same installation.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (_, step) = store.load_subject(agent).unwrap();
            if step.state == SubjectStepState::Planned {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the loop settles at a planned boundary");
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), loop_task).await;
    server.abort();
    broker_task.abort();
    drop(store);

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let recovered = state(reopened, agent).unwrap();
    let taken_over = recovered
        .semantic_wait
        .clone()
        .expect("the active wait survives the restart");
    assert_eq!(taken_over.wait_id, wait.wait_id);
    assert_eq!(taken_over.started_at, wait.started_at);
    assert_eq!(taken_over.deadline_at, wait.deadline_at);
    assert!(taken_over.is_active());
}

/// A human-requested pause must actually stop inference. A retry boundary has
/// no in-flight model request, so the loop stops there with the retry evidence
/// intact instead of continuing to re-attempt under the requested pause.
#[tokio::test]
async fn a_requested_pause_is_honoured_at_a_retry_boundary() {
    let (_dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(serve(
        listener,
        vec![(
            "429 Too Many Requests",
            serde_json::json!({"error": "rate limited"}).to_string(),
        )],
        wires_tx,
    ));
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "continuation-fixture".into()),
        1,
    )
    .unwrap();
    let broker_task = tokio::spawn(broker.run());
    let subject = state(store.clone(), agent).unwrap().with_broker(handle);
    let loop_task = tokio::spawn(super::super::run_subject_loop(
        subject,
        CancellationToken::new(),
    ));
    tokio::time::timeout(Duration::from_secs(20), wires_rx.recv())
        .await
        .expect("the first attempt reaches the provider")
        .expect("the scripted provider observes a request");
    store
        .request_subject_quiesce(agent, EntityId::new())
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(20), loop_task)
        .await
        .expect("a requested pause must stop the retrying subject");
    assert!(outcome.expect("the loop task did not panic").is_err());
    server.abort();
    broker_task.abort();

    let (_, step) = store.load_subject(agent).unwrap();
    assert_eq!(step.state, SubjectStepState::RetryPending);
    assert!(step.retry_reason_ref.is_some(), "the refusal stays durable");
    let open = store
        .open_subject_quiesce(agent)
        .unwrap()
        .expect("the requested pause is still open, never silently cleared");
    assert_eq!(open.state, shenko_store::runtime_boundary::SubjectQuiesceState::Requested);
    // The pause is honoured at a boundary: no unbounded retry streak happened.
    let mut extra = 0;
    while wires_rx.try_recv().is_ok() {
        extra += 1;
    }
    assert!(extra <= 2, "the pause must bound the retry streak, saw {extra} extra attempts");
}

fn preseal_policy() -> shenko_quota::QuotaPolicy {
    use shenko_quota::*;
    QuotaPolicy {
        policy_id: EntityId::new(), revision: 1,
        profiles: vec![
            ModelProfile {
                profile_id: "alternate".into(), model_ref: "quota-alternate".into(), local: false,
                cost_per_request: vec![WindowCost {
                    window: WindowRef { provider_id: "wire-fixture".into(),
                        account_scope_ref: "preseal-quota".into(), window_key: "requests".into() },
                    unit: "requests".into(), amount: Range::point(1.0).unwrap(),
                }], effort_levels: Vec::new(),
            },
            ModelProfile { profile_id: "configured".into(), model_ref: "configured-fixed".into(),
                local: true, cost_per_request: Vec::new(), effort_levels: Vec::new() },
        ],
        assumptions: Vec::new(), min_progress_per_hour: 0.0, switching_hysteresis: 0.0,
        // This fixture explicitly authorizes unknown quota. No synthetic
        // balance is recorded, and the real durable send is still charged.
        on_unknown: UnknownQuota::Admit, allow_overage: false, spending_authority_ref: None,
        decision_ttl: chrono::Duration::minutes(5), observation_sources: Vec::new(),
        objective: PolicyObjective::Balanced, target_utilization_range: None, planning_horizon: None,
        emergency_reserve_by_window: Vec::new(), uncertainty_margin: None,
        admission_pacing: AdmissionPacing::default(),
    }
}

/// Real provider metadata and generation HTTP, with the terminal response
/// held until the original subject waiter has gone away.
async fn serve_preseal_selection(
    listener: TcpListener,
    wire: tokio::sync::oneshot::Sender<serde_json::Value>,
    release: tokio::sync::oneshot::Receiver<()>,
) {
    let (mut metadata, _) = listener.accept().await.unwrap();
    let mut headers = Vec::new();
    while !headers.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut buffer = [0_u8; 4096];
        let count = metadata.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "metadata read ended before its headers");
        headers.extend_from_slice(&buffer[..count]);
    }
    assert!(std::str::from_utf8(&headers).unwrap().starts_with("GET /v1/models HTTP/1.1\r\n"));
    let body = serde_json::json!({"data": [
        {"id": "configured-fixed", "max_model_len": 1},
        {"id": "quota-alternate", "max_model_len": 32768}
    ]}).to_string();
    metadata.write_all(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len(),
    ).as_bytes()).await.unwrap();
    metadata.shutdown().await.unwrap();
    let (mut generation, _) = listener.accept().await.unwrap();
    wire.send(read_wire(&mut generation).await).unwrap();
    release.await.unwrap();
    let body = observing_body();
    generation.write_all(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len(),
    ).as_bytes()).await.unwrap();
    generation.shutdown().await.unwrap();
}

/// The configured model cannot fit even its output reservation. Only selecting
/// the alternate installed provider before context preparation can reach this
/// wire. A cancelled waiter, changed policy and withdrawn alternate runtime
/// then force reopen to use the original terminal receipt and exact S0.
#[tokio::test]
async fn quota_selection_prepares_alternate_model_and_reopens_original_seal() {
    use shenko_broker::{admission::CapacityProvenance, native_provider::NativeBackendProvider,
        profile::native_backends, registry::ProviderRegistry,
        resource::{ResourceDomain, ResourceDomainIdentity}};
    let (dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wire_tx, wire_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_preseal_selection(listener, wire_tx, release_rx));
    let configured = LocalModelProvider::new("http://127.0.0.1:1".into(), "configured-fixed".into())
        .with_resource_domain("quota-selection", None);
    let configured_binding = configured.binding();
    // Strata's actual HTTP provider has one metadata read and no assumed
    // metrics endpoint. This fixture claims no physical KV capability.
    let alternate = NativeBackendProvider::new(native_backends::strata(
        &endpoint, "quota-alternate", "quota-selection",
    ), None).unwrap();
    let selected_binding = shenko_broker::registry::ProviderRuntime::binding(&alternate);
    let registry = Arc::new(ProviderRegistry::new());
    registry.install(Arc::new(configured.clone()));
    let alternate_epoch = registry.install(Arc::new(alternate));
    let domain = ResourceDomain::new(ResourceDomainIdentity {
        domain_id: "quota-selection".into(), engine_identity: "quota-selection-fixture".into(),
        engine_incarnation: None, epoch: 1,
    }, 1, CapacityProvenance::Unknown, None, CapacityProvenance::Unknown);
    let (runner, handle) = Broker::with_registry(registry.clone(), domain).unwrap();
    let broker_task = tokio::spawn(runner.run());
    let mut policy = preseal_policy();
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone())
        .with_model_capabilities(Some(ProviderCapabilities {
            binding: configured_binding.clone(), max_context_tokens: Some(1), vision: None,
        }));
    subject.max_tokens = 73;
    let step_id = subject.step.step_id;
    let cancel = CancellationToken::new();
    let mut execution = Box::pin(execute_step(1, &[], &mut subject, &cancel));
    let wire = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::select! {
            _result = &mut execution => panic!("subject ended before the alternate wire: {_result:?}"),
            _wire = wire_rx => _wire.expect("the real provider observed the request"),
        }
    }).await.expect("selection and context preparation must reach the alternate provider");
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(5), &mut execution).await.unwrap().unwrap().is_none());
    drop(execution);
    assert_eq!(subject.step.state, SubjectStepState::AwaitingModel);
    assert_eq!(subject.broker.as_ref().unwrap().binding(), &configured_binding,
        "routing does not replace the configured broker or its recovery registry");
    let seal = subject.step.input_plan_ref.clone().unwrap();
    let sealed_bytes = store.cas.read_object(&seal.digest).unwrap();
    let input = super::super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(Digest::blake3(&sealed_bytes), seal.digest);
    assert_eq!(input.provider_binding.as_ref(), Some(&selected_binding));
    assert_eq!(input.provider_selection.binding.as_ref(), Some(&selected_binding));
    let selection = input.provider_selection.quota.as_ref().unwrap();
    assert_eq!(selection.profile_id, "alternate");
    assert_eq!(selection.policy_id, policy.policy_id);
    assert_eq!(selection.policy_revision, policy.revision);
    assert_eq!(selection.policy_digest, shenko_quota::policy_digest(&policy).unwrap());
    assert_ne!(selection.decision_id, EntityId::nil());
    let caps = input.context.capabilities.as_ref().unwrap();
    assert_eq!(caps.binding, selected_binding);
    assert_eq!(caps.max_context_tokens, Some(32768), "metadata must select the exact model card");
    assert_eq!(input.payload.max_tokens, Some(73));
    assert_eq!(input.context.reserved_output_tokens, 73);
    assert!(input.context.budget_input_tokens.unwrap() + input.context.reserved_output_tokens <= 32768);
    let (generation, profile) = subject.context_registry.ledger().model_profile().unwrap().unwrap();
    assert_eq!(generation, input.context.source.model_generation);
    assert_eq!(profile, input.context.profile_digest.0);
    assert_eq!(wire["model"], "quota-alternate");
    assert_eq!(wire["max_tokens"], 73);
    let mut expected_messages = Vec::new();
    if !input.payload.system_prompt.is_empty() {
        expected_messages.push(serde_json::json!({"role":"system", "content":input.payload.system_prompt}));
    }
    for message in &input.payload.messages {
        let text = message.parts.iter().map(|part| match part {
            InferencePart::Text { text } => text.as_str(),
            _ => panic!("the fixture has no image parts"),
        }).collect::<String>();
        expected_messages.push(serde_json::json!({"role":message.role, "content":text}));
    }
    assert_eq!(wire["messages"], serde_json::json!(expected_messages));
    let original = store.subject_request_attempt(input.request_id).unwrap().unwrap();
    assert_eq!(original.input_plan_ref, seal);
    assert_eq!(original.state, SubjectRequestState::Dispatched);
    let send = original.send_id.unwrap();
    let admission = store.quota_pacer().unwrap().reservation(&send).unwrap().admission.clone();
    assert_eq!(admission.profile_id.as_deref(), Some("alternate"));
    assert_eq!(handle.active_count(), 1, "cancelling the waiter keeps the original physical owner");

    policy.revision += 1;
    policy.profiles[0].effort_levels = vec![shenko_quota::EffortLevel {
        effort: "unmapped-effort".into(), cost_per_request: policy.profiles[0].cost_per_request.clone(),
    }];
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store.subject_request_attempt(input.request_id).unwrap().unwrap().state == SubjectRequestState::Terminal {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("the original sink persists the late terminal without its waiter");
    let terminal = store.subject_request_attempt(input.request_id).unwrap().unwrap().terminal.unwrap();
    let result: InferenceResult = serde_json::from_slice(&store.cas.read_object(
        &terminal.result_ref.as_ref().unwrap().digest,
    ).unwrap()).unwrap();
    assert_eq!(result.effective.provider_binding, selected_binding);
    assert_eq!(result.effective.max_tokens, 73);
    handle.request_stop();
    tokio::time::timeout(Duration::from_secs(5), broker_task).await.unwrap().unwrap();
    assert!(registry.uninstall(selected_binding.profile_id(), alternate_epoch));
    drop((subject, store, handle, registry));

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (_runner, handle) = Broker::with_local_provider(configured, 1).unwrap();
    assert!(!handle.serves_model("quota-alternate"));
    super::super::restore_inference_accounting(&reopened, agent, &handle).unwrap();
    let mut resumed = state(reopened.clone(), agent).unwrap().with_broker(handle.clone())
        .with_model_capabilities(Some(ProviderCapabilities {
            binding: configured_binding, max_context_tokens: Some(1), vision: None,
        }));
    resumed.max_tokens = 999;
    assert!(tokio::time::timeout(Duration::from_secs(5),
        execute_step(1, &[], &mut resumed, &CancellationToken::new()),
    ).await.unwrap().unwrap().is_some());
    let committed = reopened.get_step(&resumed.head.branch_id, step_id).unwrap().unwrap();
    assert_eq!(committed.state, SubjectStepState::Committed);
    assert_eq!(committed.active_request_id, Some(input.request_id));
    assert_eq!(committed.input_plan_ref.as_ref(), Some(&seal));
    assert_eq!(reopened.cas.read_object(&seal.digest).unwrap(), sealed_bytes);
    assert_eq!(reopened.subject_request_attempts_for_step(agent, step_id).unwrap().len(), 1);
    assert_eq!(reopened.quota_send_ids(input.request_id).unwrap().len(), 1);
    assert_eq!(reopened.quota_pacer().unwrap().reservation(&send).unwrap().admission, admission);
    assert_eq!(handle.active_count(), 0);
    assert_eq!(resumed.head.committed_step_no, 1);
    assert_eq!(resumed.step.step_no, 2);
}

#[tokio::test]
async fn quota_named_effort_refuses_fresh_subject_before_s0_or_send() {
    let (_dir, store, agent) = fixture();
    let (_runner, handle) = Broker::with_anthropic_provider(
        shenko_broker::provider::AnthropicProvider::with_endpoint(
            "quota-effort-fixture".into(), "http://127.0.0.1:1".into(), "configured-fixed",
        ), 1,
    ).unwrap();
    let mut policy = preseal_policy();
    policy.profiles.truncate(1);
    policy.profiles[0].effort_levels = vec![shenko_quota::EffortLevel {
        effort: "unmapped-effort".into(), cost_per_request: policy.profiles[0].cost_per_request.clone(),
    }];
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let error = execute_step(1, &[], &mut subject, &CancellationToken::new()).await.unwrap_err();
    let shenko_types::error::ShenkoError::Protocol(error) = error else {
        panic!("named effort must return a typed semantic-binding refusal");
    };
    assert_eq!(error.code, "QUOTA_EFFORT_BINDING_UNSUPPORTED");
    let (_, step) = store.load_subject(agent).unwrap();
    assert_eq!(step.state, SubjectStepState::Planned);
    assert!(step.input_plan_ref.is_none());
    assert!(store.subject_request_attempts_for_step(agent, step.step_id).unwrap().is_empty());
    assert_eq!(store.quota_pacer().unwrap().reservations().count(), 0);
    assert!(store.quota_attempts("wire-fixture", "preseal-quota").unwrap().is_empty());
    assert_eq!(handle.active_count(), 0);
}

/// Script only provider replies; exercise the production metadata/HTTP,
/// receipt, S0/S1 and parser paths. Metadata reads are not counted as sends.
async fn serve_wait_routes(
    listener: TcpListener,
    replies: Vec<String>,
    wires: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
) {
    let mut index = 0;
    while let Ok((mut socket, _)) = listener.accept().await {
        let mut bytes = Vec::new();
        let end = loop {
            let mut chunk = [0_u8; 4096];
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "provider request ended before its headers");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break end;
            }
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap().to_owned();
        let (status, body) = if headers.starts_with("GET /v1/models ") {
            ("200 OK", serde_json::json!({"data": [
                {"id":"configured-fixed", "max_model_len":32768},
                {"id":"quota-alternate", "max_model_len":32768},
            ]}).to_string())
        } else if headers.starts_with("GET /metrics ") {
            ("404 Not Found", "{}".into())
        } else {
            assert!(headers.starts_with("POST /v1/chat/completions "));
            let len: usize = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
            }).unwrap();
            while bytes.len() < end + 4 + len {
                let mut chunk = [0_u8; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0, "provider request ended before its body");
                bytes.extend_from_slice(&chunk[..count]);
            }
            wires.send(serde_json::from_slice(&bytes[end + 4..end + 4 + len]).unwrap()).unwrap();
            let text = replies.get(index).unwrap_or_else(|| replies.last().unwrap());
            index += 1;
            ("200 OK", serde_json::json!({
                "id":"semantic-wait-routing-fixture",
                "choices":[{"message":{"role":"assistant", "content":text}, "finish_reason":"stop"}],
                "usage":{"prompt_tokens":23, "completion_tokens":11},
            }).to_string())
        };
        socket.write_all(format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len(),
        ).as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn quota_policy_change_and_reopen_preserve_active_wait_until_subject_ends_it() {
    use shenko_broker::{admission::CapacityProvenance, native_provider::NativeBackendProvider,
        profile::native_backends, registry::ProviderRegistry,
        resource::{ResourceDomain, ResourceDomainIdentity}};
    let (dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let event = super::message("等待的结果到了，请继续处理这条新消息。");
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(serve_wait_routes(listener, vec![
        "[wait] {\"condition\":\"出现新证据\",\"max_wait_secs\":600}".into(),
        "[wait:continue]".into(),
        format!("[wait:end] 新证据已经到达\n[dispose] {}", serde_json::json!({
            "event_id":event.event_id, "disposition":"acknowledged",
        })),
        "继续处理后续工作。".into(),
    ], wires_tx));
    let configured = LocalModelProvider::new(endpoint.clone(), "configured-fixed".into())
        .with_resource_domain("wait-routing", None);
    let original_binding = configured.binding();
    let alternate = NativeBackendProvider::new(native_backends::strata(
        &endpoint, "quota-alternate", "wait-routing",
    ), None).unwrap();
    let alternate_binding = shenko_broker::registry::ProviderRuntime::binding(&alternate);
    let registry = Arc::new(ProviderRegistry::new());
    let original_epoch = registry.install(Arc::new(configured.clone()));
    registry.install(Arc::new(alternate));
    let domain = ResourceDomain::new(ResourceDomainIdentity {
        domain_id:"wait-routing".into(), engine_identity:"wait-routing-fixture".into(),
        engine_incarnation:None, epoch:1,
    }, 1, CapacityProvenance::Unknown, None, CapacityProvenance::Unknown);
    let (runner, handle) = Broker::with_registry(registry.clone(), domain.clone()).unwrap();
    let broker_task = tokio::spawn(runner.run());
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let cancel = CancellationToken::new();
    execute_step(1, &[], &mut subject, &cancel).await.unwrap().unwrap();
    assert_eq!(wires_rx.recv().await.unwrap()["model"], "configured-fixed");
    let original_wait = store.semantic_wait(agent).unwrap().unwrap();
    assert!(original_wait.is_active());

    // The current policy would normally choose the other installed provider.
    let policy = preseal_policy();
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    let candidate = crate::quota_gate::select_quota_route(
        &store, Some(&handle), &cancel, &subject.quota_strategies,
        crate::quota_gate::QuotaRouteOwner::Subject(agent),
    ).await.unwrap().unwrap();
    assert_eq!(candidate.broker.unwrap().binding(), &alternate_binding);
    let continued_step = subject.step.step_id;
    execute_step(2, &[], &mut subject, &cancel).await.unwrap().unwrap();
    assert_eq!(wires_rx.recv().await.unwrap()["model"], "configured-fixed");
    let continued_wait = store.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(continued_wait.wait_id, original_wait.wait_id);
    assert_eq!(continued_wait.started_at, original_wait.started_at);
    assert_eq!(continued_wait.deadline_at, original_wait.deadline_at);
    let attempt = store.subject_request_attempts_for_step(agent, continued_step).unwrap().pop().unwrap();
    let admission = store.quota_pacer().unwrap().reservation(&attempt.send_id.unwrap()).unwrap().admission.clone();
    assert_eq!(admission.profile_id.as_deref(), Some("configured"), "the pinned wait still passes quota admission");
    let input = super::super::frozen_input(&store,
        &store.get_step(&subject.head.branch_id, continued_step).unwrap().unwrap()).unwrap();
    assert_eq!(input.provider_binding.as_ref(), Some(&original_binding));
    assert_eq!(input.semantic_wait.as_ref().unwrap().deadline_at, original_wait.deadline_at);
    handle.request_stop();
    broker_task.await.unwrap();
    drop((subject, store, handle));

    // Reopen with the alternate default. A withdrawn original must not turn
    // into silent fallback, a new wait, a send or a forged spending receipt.
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    assert!(registry.uninstall(original_binding.profile_id(), original_epoch));
    let (runner, handle) = Broker::with_registry(registry.clone(), domain).unwrap();
    let handle = handle.for_binding(&alternate_binding).unwrap();
    let broker_task = tokio::spawn(runner.run());
    let mut resumed = state(reopened.clone(), agent).unwrap().with_broker(handle.clone())
        .with_model_capabilities(Some(ProviderCapabilities {
            binding:alternate_binding.clone(), max_context_tokens:Some(32768), vision:None,
        }));
    let stopped_step = resumed.step.step_id;
    let error = execute_step(3, std::slice::from_ref(&event), &mut resumed, &cancel).await.unwrap_err();
    assert!(error.to_string().contains("semantic wait original provider is unavailable"));
    assert_eq!(resumed.step.state, SubjectStepState::Planned);
    assert!(resumed.step.input_plan_ref.is_none());
    assert!(reopened.subject_request_attempts_for_step(agent, stopped_step).unwrap().is_empty());
    assert!(wires_rx.try_recv().is_err(), "withdrawal cannot dispatch through the replacement");
    let waiting = reopened.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(waiting.wait_id, original_wait.wait_id);
    assert_eq!(waiting.deadline_at, original_wait.deadline_at);

    registry.install(Arc::new(configured));
    execute_step(3, std::slice::from_ref(&event), &mut resumed, &cancel).await.unwrap().unwrap();
    let wire = wires_rx.recv().await.unwrap();
    assert_eq!(wire["model"], "configured-fixed");
    assert!(wire["messages"].to_string().contains("等待的结果到了"));
    assert!(reopened.event_disposition_commit(agent, event.event_id).unwrap().is_some());
    let ended = reopened.semantic_wait(agent).unwrap().unwrap();
    assert!(matches!(ended.status, SemanticWaitStatus::Ended { .. }));
    assert_eq!(ended.deadline_at, original_wait.deadline_at);
    execute_step(4, &[], &mut resumed, &cancel).await.unwrap().unwrap();
    assert_eq!(wires_rx.recv().await.unwrap()["model"], "quota-alternate");
    assert_eq!(resumed.head.committed_step_no, 4);
    handle.request_stop();
    broker_task.await.unwrap();
    server.abort();
}

#[tokio::test]
async fn expired_wait_does_not_pin_a_reopened_subject_to_its_missing_original_model() {
    let (dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wires_tx, mut wires_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(serve_wait_routes(listener, vec![
        "[wait] {\"condition\":\"等待短时观察\",\"max_wait_secs\":1}".into(),
        "等待已经失效，继续正常工作。".into(),
    ], wires_tx));
    let (runner, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint.clone(), "configured-fixed".into()), 1,
    ).unwrap();
    let broker_task = tokio::spawn(runner.run());
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    execute_step(1, &[], &mut subject, &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(wires_rx.recv().await.unwrap()["model"], "configured-fixed");
    let original = store.semantic_wait(agent).unwrap().unwrap();
    handle.request_stop();
    broker_task.await.unwrap();
    drop((subject, store, handle));
    let until_expiry = (original.deadline_at - chrono::Utc::now()).to_std().unwrap_or_default();
    tokio::time::sleep(until_expiry + Duration::from_millis(20)).await;

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (runner, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "replacement".into()), 1,
    ).unwrap();
    let broker_task = tokio::spawn(runner.run());
    let mut resumed = state(reopened.clone(), agent).unwrap().with_broker(handle.clone());
    execute_step(2, &[], &mut resumed, &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(wires_rx.recv().await.unwrap()["model"], "replacement");
    let expired = reopened.semantic_wait(agent).unwrap().unwrap();
    assert!(matches!(expired.status, SemanticWaitStatus::Expired { .. }));
    assert_eq!(expired.wait_id, original.wait_id);
    assert_eq!(expired.started_at, original.started_at);
    assert_eq!(expired.deadline_at, original.deadline_at);
    assert_eq!(resumed.head.committed_step_no, 2);
    handle.request_stop();
    broker_task.await.unwrap();
    server.abort();
}

async fn serve_effort_original(
    listener: TcpListener,
    wire: tokio::sync::oneshot::Sender<serde_json::Value>,
    release: tokio::sync::oneshot::Receiver<()>,
) {
    let (mut socket, _) = listener.accept().await.unwrap();
    wire.send(read_wire(&mut socket).await).unwrap();
    release.await.unwrap();
    let body = serde_json::json!({
        "id":"effort-original", "type":"message", "role":"assistant", "model":"quota-alternate",
        "content":[{"type":"text", "text":OBSERVING}], "stop_reason":"end_turn",
        "usage":{"input_tokens":31, "output_tokens":17}
    }).to_string();
    socket.write_all(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len(),
    ).as_bytes()).await.unwrap();
    socket.shutdown().await.unwrap();
}

/// Actual AnthropicProvider serialization, Store/S0, hook stop, durable quota
/// admission and receipt recovery. A changed priced effort must refuse the
/// original seal before admission; a restored policy can send that same seal.
/// Its late terminal then survives a policy change and absent original runtime.
#[tokio::test]
async fn quota_anthropic_effort_matches_wire_and_preserves_original_resume() {
    use shenko_broker::effort::{AnthropicEffort, ProviderEffort};
    use shenko_kernel::hooks::HookPointId;
    let (dir, store, agent) = fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (wire_tx, wire_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_effort_original(listener, wire_tx, release_rx));
    let (runner, handle) = Broker::with_anthropic_provider(
        shenko_broker::provider::AnthropicProvider::with_endpoint(
            "quota-effort-fixture".into(), endpoint, "quota-alternate",
        ), 1,
    ).unwrap();
    let broker_task = tokio::spawn(runner.run());
    let binding = handle.binding().clone();
    let mut policy = preseal_policy();
    policy.profiles.truncate(1);
    let costs = policy.profiles[0].cost_per_request.clone();
    policy.profiles[0].effort_levels = vec![shenko_quota::EffortLevel {
        effort: "high".into(), cost_per_request: costs,
    }];
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    // Reuse the real StopUnit fixture. The same handler/graph later clears;
    // no test-only production callback or replacement graph admits the seal.
    let (hooks, _registration, failing) = super::stop_registry(HookPointId::SUBJECT_BEFORE_INFERENCE);
    let hooks = Arc::new(hooks);
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone())
        .with_model_capabilities(Some(ProviderCapabilities {
            binding: binding.clone(), max_context_tokens: Some(32768), vision: None,
        })).with_hooks(hooks.clone());
    subject.max_tokens = 73;
    let implicit_profile = subject.bind_context_profile().unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(5),
        execute_step(1, &[], &mut subject, &CancellationToken::new()),
    ).await.unwrap().unwrap_err();
    assert!(stopped.to_string().contains("stopped the unit"));
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    let step_id = subject.step.step_id;
    let seal = subject.step.input_plan_ref.clone().unwrap();
    let sealed_bytes = store.cas.read_object(&seal.digest).unwrap();
    let input = super::super::frozen_input(&store, &subject.step).unwrap();
    let effort = input.provider_selection.quota.as_ref().unwrap().effort.as_ref().unwrap();
    assert_eq!(effort.requested, "high");
    assert_eq!(effort.native, ProviderEffort::AnthropicMessages { level: AnthropicEffort::High });
    assert_eq!(input.payload.native_options["output_config"]["effort"], "high");
    assert_eq!(input.payload.max_tokens, Some(73));
    assert_eq!(input.context.reserved_output_tokens, 73);
    assert_ne!(input.context.profile_digest, implicit_profile,
        "explicit effort participates in context identity before preparation");
    assert_eq!(input.provider_binding.as_ref(), Some(&binding));
    assert_eq!(store.quota_pacer().unwrap().reservations().count(), 0);

    failing.store(false, std::sync::atomic::Ordering::SeqCst);
    policy.revision += 1;
    policy.profiles[0].effort_levels[0].effort = "medium".into();
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    let changed = tokio::time::timeout(Duration::from_secs(5),
        execute_step(1, &[], &mut subject, &CancellationToken::new()),
    ).await.unwrap().unwrap_err();
    let shenko_types::error::ShenkoError::Protocol(changed) = changed else {
        panic!("a new effort price cannot reinterpret the old native seal");
    };
    assert_eq!(changed.code, "QUOTA_EFFORT_BINDING_MISMATCH");
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    assert_eq!(subject.step.input_plan_ref.as_ref(), Some(&seal));
    assert_eq!(store.quota_pacer().unwrap().reservations().count(), 0);
    assert!(store.subject_request_attempts_for_step(agent, step_id).unwrap().is_empty());

    policy.revision += 1;
    policy.profiles[0].effort_levels[0].effort = "high".into();
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    subject.max_tokens = 901;
    let cancel = CancellationToken::new();
    let mut execution = Box::pin(execute_step(1, &[], &mut subject, &cancel));
    let wire = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            _response = wire_rx => _response.expect("the Messages wire delivered no body"),
            _ = &mut execution => panic!("the original high-effort seal ended before its Messages wire"),
        }
    }).await.expect("the exact original high-effort seal must reach Messages HTTP");
    assert_eq!(wire["model"], "quota-alternate");
    assert_eq!(wire["max_tokens"], 73);
    assert_eq!(wire["output_config"], input.payload.native_options["output_config"]);
    assert_eq!(wire["output_config"]["effort"], "high");
    cancel.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(5), &mut execution).await.unwrap().unwrap().is_none());
    drop(execution);
    let attempt = store.subject_request_attempt(input.request_id).unwrap().unwrap();
    assert_eq!(attempt.input_plan_ref, seal);
    assert_eq!(attempt.state, SubjectRequestState::Dispatched);
    let send = attempt.send_id.unwrap();
    let admission = store.quota_pacer().unwrap().reservation(&send).unwrap().admission.clone();
    assert_eq!(handle.active_count(), 1, "the cancelled waiter does not release the original send");

    policy.revision += 1;
    policy.profiles[0].effort_levels[0].effort = "medium".into();
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store.subject_request_attempt(input.request_id).unwrap().unwrap().state == SubjectRequestState::Terminal { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("the original receipt owner retains the late terminal");
    let terminal = store.subject_request_attempt(input.request_id).unwrap().unwrap().terminal.unwrap();
    let result: InferenceResult = serde_json::from_slice(&store.cas.read_object(
        &terminal.result_ref.as_ref().unwrap().digest,
    ).unwrap()).unwrap();
    assert_eq!(result.effective.native_options, input.payload.native_options);
    assert_eq!(result.effective.max_tokens, 73);
    handle.request_stop();
    tokio::time::timeout(Duration::from_secs(5), broker_task).await.unwrap().unwrap();
    drop((subject, handle, store));

    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (_offline, handle) = Broker::offline().unwrap();
    super::super::restore_inference_accounting(&reopened, agent, &handle).unwrap();
    let mut resumed = state(reopened.clone(), agent).unwrap().with_broker(handle.clone()).with_hooks(hooks);
    resumed.max_tokens = 999;
    assert!(tokio::time::timeout(Duration::from_secs(5),
        execute_step(1, &[], &mut resumed, &CancellationToken::new()),
    ).await.unwrap().unwrap().is_some());
    let committed = reopened.get_step(&resumed.head.branch_id, step_id).unwrap().unwrap();
    assert_eq!(committed.input_plan_ref.as_ref(), Some(&seal));
    assert_eq!(committed.active_request_id, Some(input.request_id));
    assert_eq!(reopened.cas.read_object(&seal.digest).unwrap(), sealed_bytes);
    assert_eq!(reopened.subject_request_attempts_for_step(agent, step_id).unwrap().len(), 1);
    assert_eq!(reopened.quota_pacer().unwrap().reservation(&send).unwrap().admission, admission);
    assert_eq!(handle.active_count(), 0);
    assert_eq!(resumed.head.committed_step_no, 1);
    assert_eq!(resumed.step.step_no, 2);
}

#[tokio::test]
async fn quota_known_effort_on_unmapped_runtime_refuses_before_s0() {
    use shenko_broker::{admission::CapacityProvenance, native_provider::NativeBackendProvider,
        profile::native_backends, resource::{ResourceDomain, ResourceDomainIdentity}};
    let (_dir, store, agent) = fixture();
    let runtime = NativeBackendProvider::new(native_backends::strata(
        "http://127.0.0.1:1", "quota-alternate", "effort-unmapped",
    ), None).unwrap();
    let domain = ResourceDomain::new(ResourceDomainIdentity {
        domain_id:"effort-unmapped".into(), engine_identity:"effort-unmapped".into(),
        engine_incarnation:None, epoch:1,
    }, 1, CapacityProvenance::Unknown, None, CapacityProvenance::Unknown);
    let (_runner, handle) = Broker::with_runtime(Arc::new(runtime), domain).unwrap();
    let mut policy = preseal_policy();
    policy.profiles.truncate(1);
    policy.profiles[0].effort_levels = vec![shenko_quota::EffortLevel {
        effort:"high".into(), cost_per_request:policy.profiles[0].cost_per_request.clone(),
    }];
    store.record_quota_policy(&policy, chrono::Utc::now()).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle.clone());
    let error = execute_step(1, &[], &mut subject, &CancellationToken::new()).await.unwrap_err();
    let shenko_types::error::ShenkoError::Protocol(error) = error else {
        panic!("a known name on an unmapped runtime must fail as unsupported");
    };
    assert_eq!(error.code, "QUOTA_EFFORT_BINDING_UNSUPPORTED");
    assert_eq!(subject.step.state, SubjectStepState::Planned);
    assert!(subject.step.input_plan_ref.is_none());
    assert!(store.subject_request_attempts_for_step(agent, subject.step.step_id).unwrap().is_empty());
    assert_eq!(store.quota_pacer().unwrap().reservations().count(), 0);
    assert!(store.quota_attempts("wire-fixture", "preseal-quota").unwrap().is_empty());
    assert_eq!(handle.active_count(), 0);
}
