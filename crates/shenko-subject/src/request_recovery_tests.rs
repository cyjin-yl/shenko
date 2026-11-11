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
/// attempt. docs/specs/subject-runtime.md#spec-5-1, docs/specs/subject-runtime.md#spec-41-5, R143, RT16, A01.
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

    // The degradation is a durable fact the subject can read (docs/specs/cognition-units.md#spec-45-14), not a
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
/// docs/specs/subject-runtime.md#spec-5-1, docs/specs/subject-runtime.md#spec-41-5, R143, RT16.
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
