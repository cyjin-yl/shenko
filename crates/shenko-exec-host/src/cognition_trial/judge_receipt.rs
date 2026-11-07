use super::*;
use parking_lot::Mutex;
use shenko_broker::broker::{DispatchFence, InferenceReceiptSink, InferenceRecoverySource};
use shenko_broker::request::{InferenceResult, ProviderBinding, TerminationKind};

/// Durable coordinates of a judge call, including calls whose waiter failed.
/// A late result remains readable through `journal_id` even when this snapshot
/// was taken before its result arrived.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeReceiptIdentity {
    pub request_id: EntityId,
    pub journal_id: String,
    pub request_ref: Option<ObjectRef>,
    pub request_commit: Option<CommitRef>,
    pub result_ref: Option<ObjectRef>,
    pub result_commit: Option<CommitRef>,
}

#[derive(Serialize)]
pub struct SandboxJudgeContext<'a> {
    pub configuration_ref: &'a ObjectRef,
    pub installation_ref: &'a ObjectRef,
    pub sample_digest: &'a str,
    pub scenario: &'a str,
    pub condition: Condition,
    pub trial: usize,
}

#[derive(Serialize)]
struct RequestEnvelope<'a, C: ?Sized> {
    schema_version: u32,
    request_id: EntityId,
    agent_id: EntityId,
    conversation_id: EntityId,
    provider_binding: &'a ProviderBinding,
    priority: &'a InferencePriority,
    epoch: ControllerEpoch,
    payload: &'a InferencePayload,
    trial: &'a C,
}

#[derive(Serialize, Deserialize)]
struct TerminalReceipt {
    request_id: EntityId,
    request_ref: ObjectRef,
    result_ref: ObjectRef,
}

/// Created only for a fresh judge request. Recovery reads this original sink;
/// it never reconstructs a send-capable sink from an old request identity.
pub(super) struct JudgeReceiptSink {
    store: Arc<Store>,
    identity: JudgeReceiptIdentity,
    binding: ProviderBinding,
    payload: InferencePayload,
    dispatch_claimed: Mutex<bool>,
    result: Mutex<Option<(ObjectRef, CommitRef)>>,
    recording_error: Mutex<Option<String>>,
}

impl JudgeReceiptSink {
    pub fn open<C: Serialize + ?Sized>(
        store: Arc<Store>,
        request: &InferenceRequest,
        context: &C,
        receipt: &mut Option<JudgeReceiptIdentity>,
    ) -> Result<Arc<Self>> {
        let identity = receipt.insert(JudgeReceiptIdentity {
            request_id: request.request_id,
            journal_id: format!("cognition-trial-judge/{}", request.request_id),
            request_ref: None, request_commit: None, result_ref: None, result_commit: None,
        });
        identity.request_ref = Some(seal_trial_object(&store, &RequestEnvelope {
            schema_version: 1, request_id: request.request_id,
            agent_id: request.agent_id, conversation_id: request.conversation_id,
            provider_binding: &request.provider_binding, priority: &request.priority,
            epoch: request.epoch, payload: &request.payload, trial: context,
        }, "cognition-trial-judge-request")?);
        let bound = serde_json::to_string(identity)?;
        store.open_content_stream(&identity.journal_id, request.agent_id,
            "cognition-trial-judge", &bound)?;
        identity.request_commit = Some(store.journal_content_unit(&identity.journal_id, 0,
            "whole", "cognition-trial-judge-request", &bound, false)?.commit_ref);
        Ok(Arc::new(Self {
            store, identity: identity.clone(), binding: request.provider_binding.clone(),
            payload: request.payload.clone(), dispatch_claimed: Mutex::new(false),
            result: Mutex::new(None), recording_error: Mutex::new(None),
        }))
    }

    pub fn snapshot(&self) -> JudgeReceiptIdentity {
        let mut identity = self.identity.clone();
        if let Some((reference, commit)) = self.result.lock().as_ref() {
            identity.result_ref = Some(reference.clone());
            identity.result_commit = Some(*commit);
        }
        identity
    }

    pub fn recording_error(&self) -> Option<String> {
        self.recording_error.lock().clone()
    }

    fn record_result(&self, result: &InferenceResult) -> Result<()> {
        ensure!(result.request_id == self.identity.request_id
            && result.effective.matches_seal(&self.binding, &self.payload),
            "judge result differs from its frozen request");
        let result_ref = seal_trial_object(&self.store, result, "cognition-trial-judge-result")?;
        let terminal = TerminalReceipt {
            request_id: self.identity.request_id,
            request_ref: self.identity.request_ref.clone().context("judge request is not sealed")?,
            result_ref,
        };
        let committed = self.store.journal_content_terminal(&self.identity.journal_id, 2,
            "cognition-trial-judge-result", &serde_json::to_string(&terminal)?)?;
        *self.result.lock() = Some((terminal.result_ref, committed.commit_ref));
        Ok(())
    }
}

impl InferenceReceiptSink for JudgeReceiptSink {
    fn may_dispatch(&self) -> Result<DispatchFence> {
        let mut claimed = self.dispatch_claimed.lock();
        if *claimed { return Ok(DispatchFence::Unknown); }
        // Consume the single local claim before the durable write too: an
        // uncertain journal failure cannot become permission for a retry.
        *claimed = true;
        self.store.journal_content_unit(&self.identity.journal_id, 1, "whole",
            "cognition-trial-judge-dispatch", &serde_json::to_string(&self.identity)?, false)?;
        Ok(DispatchFence::Send)
    }

    fn record(&self, result: &InferenceResult) -> Result<()> {
        let recorded = self.record_result(result);
        if let Err(error) = &recorded {
            *self.recording_error.lock() = Some(format!("{error:#}"));
        }
        recorded
    }
}

impl InferenceRecoverySource for JudgeReceiptSink {
    fn request_id(&self) -> EntityId { self.identity.request_id }
    fn binding(&self) -> &ProviderBinding { &self.binding }
    fn payload(&self) -> &InferencePayload { &self.payload }

    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        let units = self.store.content_stream_units(&self.identity.journal_id, 2)?;
        let Some(unit) = units.into_iter().find(|unit| unit.seq == 2 && unit.terminal) else {
            return Ok(None);
        };
        ensure!(unit.shape == "whole" && unit.content_kind == "cognition-trial-judge-result",
            "judge terminal journal has the wrong kind");
        let terminal: TerminalReceipt = serde_json::from_str(&unit.data)?;
        ensure!(terminal.request_id == self.identity.request_id
            && Some(&terminal.request_ref) == self.identity.request_ref.as_ref(),
            "judge terminal journal belongs to a different request");
        let request_bytes = self.store.cas.read_object(&terminal.request_ref.digest)?;
        ensure!(request_bytes.len() as u64 == terminal.request_ref.bytes
            && Digest::blake3(&request_bytes) == terminal.request_ref.digest,
            "judge request receipt CAS mismatch");
        let bytes = self.store.cas.read_object(&terminal.result_ref.digest)?;
        ensure!(bytes.len() as u64 == terminal.result_ref.bytes
            && Digest::blake3(&bytes) == terminal.result_ref.digest,
            "judge result receipt CAS mismatch");
        let result: InferenceResult = serde_json::from_slice(&bytes)?;
        ensure!(result.request_id == self.identity.request_id
            && result.effective.matches_seal(&self.binding, &self.payload),
            "judge recovered result differs from its frozen request");
        Ok((result.termination.kind != TerminationKind::Unknown).then_some(result))
    }
}
