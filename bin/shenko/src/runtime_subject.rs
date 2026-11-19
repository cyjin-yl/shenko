//! A model incarnation may stop without taking ingress and recovery with it.
use std::sync::Arc;

use shenko_store::Store;
use shenko_subject::loop_runner::{SubjectLoopState, run_subject_loop_retaining_inbox};
use shenko_types::{error::ShenkoError, ids::EntityId};
use tokio_util::sync::CancellationToken;

fn recovery_fingerprint(store: &Store, agent: EntityId) -> Result<Vec<u8>, ShenkoError> {
    let (head, step) = store.load_subject(agent)?;
    let attempt = step.active_request_id.map(|id| store.subject_request_attempt(id)).transpose()?;
    Ok(serde_json::to_vec(&(head, step, attempt))?)
}

fn original_ready(store: &Store, agent: EntityId) -> Result<bool, ShenkoError> {
    use shenko_types::subject::{InferenceRetryFact, SubjectRetryEvidence, SubjectStepState};
    let (_, step) = store.load_subject(agent)?;
    if step.state == SubjectStepState::ResultRecorded {
        return Ok(true);
    }
    if step.state == SubjectStepState::RetryPending {
        let object = step.retry_reason_ref.as_ref()
            .ok_or_else(|| ShenkoError::conflict("recovery_blocked: missing retry evidence"))?;
        let evidence: SubjectRetryEvidence = serde_json::from_slice(
            &store.cas.read_object(&object.digest)?,
        )?;
        // Match the live loop's resource-wait classification. DispatchLost
        // retains uncertain debt; reopening it is not original-result recovery.
        return Ok(matches!(
            evidence.fact,
            InferenceRetryFact::NeverDispatched
                | InferenceRetryFact::ProviderRejected { http_status: 429 }
        ));
    }
    let Some(request) = step.active_request_id else { return Ok(false); };
    let Some(attempt) = store.subject_request_attempt(request)? else { return Ok(false); };
    let Some(terminal) = attempt.terminal else { return Ok(false); };
    let Some(result_ref) = terminal.result_ref else { return Ok(false); };
    let result: shenko_broker::request::InferenceResult = serde_json::from_slice(
        &store.cas.read_object(&result_ref.digest)?,
    )?;
    Ok(matches!(result.status, shenko_broker::request::InferenceStatus::Success))
}

/// Reconstruct only after a durable subject/request fact changed. A timer is
/// only an observation cadence, never permission to resend an Unknown request.
/// The fresh state constructor and the ordinary S0/S1 recovery path still check
/// every original seal, authority epoch and request receipt.
pub(crate) async fn supervise(
    mut make: impl FnMut() -> Result<SubjectLoopState, ShenkoError> + Send,
    store: Arc<Store>,
    agent: EntityId,
    live: Arc<shenko_ui::ws::LiveStatus>,
    cancel: CancellationToken,
) -> Result<(), ShenkoError> {
    let mut last_original_recovery = None;
    loop {
        if cancel.is_cancelled() { return Ok(()); }
        let result = match make() {
            Ok(state) => {
                live.set_subject_blocked(None);
                let stop = cancel.clone();
                match tokio::spawn(run_subject_loop_retaining_inbox(state, stop)).await {
                    Ok(result) => result,
                    Err(error) => Err(ShenkoError::other(format!("subject incarnation terminated: {error}"))),
                }
            }
            Err(error) => Err(error),
        };
        if cancel.is_cancelled() { return result; }
        let error = match result {
            Ok(()) => ShenkoError::other("subject incarnation stopped unexpectedly"),
            Err(error) => error,
        };
        // Loss of persistence or authority belongs to installation recovery.
        // A model/request refusal belongs to this supervised incarnation.
        if matches!(&error, ShenkoError::Storage(_) | ShenkoError::Io(_))
            || matches!(&error, ShenkoError::Protocol(p) if p.scope == shenko_types::error::ErrorScope::Writer)
        {
            return Err(error);
        }
        // A late original may have arrived between the failed reader and this
        // supervisor. Give that exact new fact one recovery pass; never loop
        // on the same result or mistake a timer for replay permission.
        let before = recovery_fingerprint(&store, agent)?;
        if original_ready(&store, agent)? && last_original_recovery.as_ref() != Some(&before) {
            last_original_recovery = Some(before);
            continue;
        }
        store.journal_runtime_error(agent, shenko_store::RuntimeErrorContext {
            kind: "subject_incarnation_blocked".into(),
            source_module: module_path!().into(),
            message: error.to_string(),
            extra: serde_json::json!({"ingress_available":true,"automatic_replay":false}),
            ..Default::default()
        });
        live.set_subject_blocked(Some(
            "主体推理等待原请求对账或配置修复；消息已持久接收，查询与恢复服务继续运行".into()
        ));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
            }
            if recovery_fingerprint(&store, agent)? != before { break; }
        }
    }
}
