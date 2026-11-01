//! Durable receipts for the subject's own finite generation — SPEC §41.5/§41.4.
//!
//! The subject's own turn uses the same discipline as a finite child: the
//! request identity, the sealed input plan and the payload/provider digests
//! are committed BEFORE any wire send, the broker's send boundary is recorded
//! exactly once, and the terminal provider evidence is committed
//! independently of whether the waiting step is still alive. A late or lost
//! caller never loses the result and never gains a reason to replay.
//!
//! An unknown outcome stays unknown: the armed send is never settled and the
//! step is reconciled, never blindly re-dispatched.

use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use shenko_broker::broker::{DispatchFence, InferenceReceiptSink, InferenceRecoverySource};
use shenko_broker::request::{InferencePayload, InferenceResult, ProviderBinding};
use shenko_store::subject_request::{SubjectRequestAttempt, SubjectRequestState};
use shenko_types::commit::CommitRef;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, ObjectRef};

/// The subject's own durable receipt sink. It owns the send boundary and the
/// terminal evidence of exactly one main request.
pub(crate) struct SubjectReceiptSink {
    pub store: Arc<shenko_store::store::Store>,
    pub agent_id: EntityId,
    pub step_id: EntityId,
    pub request_id: EntityId,
    pub input_plan_ref: ObjectRef,
    pub payload_digest: Digest,
    pub provider_binding_digest: Digest,
    pub binding: ProviderBinding,
    pub payload: InferencePayload,
}

impl InferenceReceiptSink for SubjectReceiptSink {
    /// The durable send boundary. `Armed` means this submission owns the one
    /// send of this request; `Dispatched` means a send may already have
    /// committed, so the outcome stays recoverable debt and is never marked
    /// never-sent; `Terminal` means the request already finished and no
    /// second send is allowed.
    fn may_dispatch(&self) -> Result<DispatchFence> {
        let attempt = self
            .store
            .subject_request_attempt(self.request_id)?
            .ok_or_else(|| anyhow::anyhow!("subject request attempt is not armed"))?;
        match attempt.state {
            SubjectRequestState::Armed => match self.store.mark_subject_request_dispatched(self.request_id)
            {
                Ok(_) => Ok(DispatchFence::Send),
                // Another writer already crossed the boundary. The request may
                // be in flight: the outcome stays unknown, never never-sent.
                Err(_) => Ok(DispatchFence::Unknown),
            },
            SubjectRequestState::Dispatched => Ok(DispatchFence::Unknown),
            SubjectRequestState::Terminal => Ok(DispatchFence::Fenced),
        }
    }

    /// The terminal provider evidence of this request. It commits even when
    /// the waiting step is gone, so the result survives a crash between the
    /// wire answer and the step's S1.
    fn record(&self, result: &InferenceResult) -> Result<()> {
        ensure!(
            result.request_id == self.request_id,
            "subject request result identity mismatch"
        );
        let raw = self
            .store
            .cas
            .seal_object(
                &serde_json::to_vec(result)?,
                "application/json",
                "subject_requests",
                "subject-request-result",
                1,
            )
            .map_err(|error| {
                self.store.close_external_writer();
                error
            })?;
        let terminal = shenko_store::subject_request::SubjectRequestTerminal {
            outcome: serde_json::json!({
                "status": result.status,
                "termination": result.termination,
                "stop_reason": result.response.as_ref().map(|response| response.stop_reason.clone()),
                "usage": result.response.as_ref().and_then(|response| response.usage.as_ref()),
            }),
            result_ref: Some(raw.object),
            // The store assigns the commit that records this terminal.
            recorded_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
        };
        self.store
            .record_subject_request_terminal(self.request_id, &terminal)?;
        Ok(())
    }
}

impl InferenceRecoverySource for SubjectReceiptSink {
    fn request_id(&self) -> EntityId {
        self.request_id
    }

    fn binding(&self) -> &ProviderBinding {
        &self.binding
    }

    fn payload(&self) -> &InferencePayload {
        &self.payload
    }

    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        let Some(attempt) = self.store.subject_request_attempt(self.request_id)? else {
            return Ok(None);
        };
        read_terminal_result(&self.store, &attempt)
    }
}

/// Read the sealed terminal result of one durable attempt. The receipt is
/// bound to its own request identity, its own sealed plan and its own
/// payload digest: a row that points at another request is a conflict, never
/// a result.
pub(crate) fn read_terminal_result(
    store: &shenko_store::store::Store,
    attempt: &SubjectRequestAttempt,
) -> Result<Option<InferenceResult>> {
    let Some(terminal) = attempt.terminal.as_ref() else {
        return Ok(None);
    };
    let Some(result_ref) = terminal.result_ref.as_ref() else {
        return Ok(None);
    };
    let bytes = store
        .cas
        .read_object(&result_ref.digest)
        .context("subject request receipt is not readable")?;
    ensure!(
        bytes.len() as u64 == result_ref.bytes && Digest::blake3(&bytes) == result_ref.digest,
        "subject request receipt CAS mismatch"
    );
    let result: InferenceResult = serde_json::from_slice(&bytes)?;
    ensure!(
        result.request_id == attempt.request_id,
        "subject request receipt identity mismatch"
    );
    Ok(Some(result))
}