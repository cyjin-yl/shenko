//! Durable crash-window fixtures, not provider/model or physical-crash proof.
use super::{execute_step, fixture, state, SubjectLoopState};
use shenko_broker::{
    broker::{Broker, InferenceReceiptSink},
    local_provider::LocalModelProvider,
    request::*,
};
use shenko_store::{Store, subject_request::{SubjectRequestAttempt, SubjectRequestState}};
use shenko_types::{commit::CommitRef, ids::*, object::Digest, subject::*};
use std::sync::Arc;
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
