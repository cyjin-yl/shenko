//! One finite, text-only model generation on the shared inference broker.
//! Authored semantics are independent; role, empty toolset, transport, output
//! budget and task authority are runtime facts frozen before any model send.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_broker::{broker::{BrokerHandle, InferenceReceiptSink, InferenceRecoverySource}, request::*};
use shenko_broker::broker::DispatchFence;
use shenko_store::{Store, TaskInvocation};
use shenko_store::external::InferenceSendBinding;
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*, object::*, operation::*, output::ModelChildSpec,
    subject::{BudgetHint, DelegationIntent}, task::*,
};
use std::sync::Arc;
use crate::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use crate::quota_gate::{QuotaGate, quota_dispatch_gate, observe_quota_response, settle_quota_terminal};

pub const MODEL_CHILD_KIND: &str = "model.child";
const OPERATION: &str = "model.child.generate.v1";
const ROLE: &str = "model-child/text-generation@1";
const ROLE_CONTRACT: &str = "运行角色：有明确任务边界的有限模型 child，不是持续主体。\n\
本次只执行一次文本推理，受已冻结模型与输出预算限制；本轮没有任何工具 schema、文件、shell、网络或发信入口。\n\
你的输出是交回 owner 的结果正文，不是主体控制协议；不得声称未实际执行的工具或外部动作已经完成。\n\
以下是 owner 为你独立编写的完整语义 system prompt：";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenModelChild {
    schema_version: u32,
    task_id: EntityId,
    request_id: EntityId,
    created_by_step_id: EntityId,
    spec: ModelChildSpec,
    provider_binding: ProviderBinding,
    parent_max_tokens: u64,
    software_generation: RuntimeGeneration,
    role_revision: String,
    payload: InferencePayload,
    prompt_fingerprint: Digest,
    toolset_fingerprint: Digest,
    payload_fingerprint: Digest,
}

impl FrozenModelChild {
    fn new(
        task_id: EntityId, request_id: EntityId, step_id: EntityId, spec: ModelChildSpec,
        transport: &ProviderBinding, parent_max_tokens: u64, generation: RuntimeGeneration,
    ) -> Result<Self> {
        ensure!(!spec.goal.trim().is_empty() && spec.goal.len() <= 64 * 1024, "invalid model child goal");
        ensure!(!spec.system_prompt.trim().is_empty() && spec.system_prompt.len() <= 64 * 1024, "invalid child semantic prompt");
        ensure!(spec.max_tokens > 0 && spec.max_tokens <= parent_max_tokens, "child output budget exceeds sealed parent cap");
        let provider_binding = transport.with_model(&spec.model)?;
        let system_prompt = format!("{ROLE_CONTRACT}\n\n{}", spec.system_prompt);
        let prompt_fingerprint = Digest::blake3(system_prompt.as_bytes());
        let payload = InferencePayload {
            system_prompt,
            messages: vec![InferenceMessage::text("user", &spec.goal)],
            max_tokens: Some(spec.max_tokens),
            tools: vec![],
            native_options: Default::default(),
        };
        Ok(Self {
            schema_version: 1, task_id, request_id, created_by_step_id: step_id,
            provider_binding, parent_max_tokens, software_generation: generation,
            role_revision: ROLE.into(), prompt_fingerprint,
            toolset_fingerprint: Digest::blake3(&serde_json::to_vec(&payload.tools)?),
            payload_fingerprint: Digest::blake3(&serde_json::to_vec(&payload)?),
            spec, payload,
        })
    }

    fn validate(&self) -> Result<()> {
        let expected = Self::new(
            self.task_id, self.request_id, self.created_by_step_id, self.spec.clone(),
            &self.provider_binding, self.parent_max_tokens, self.software_generation.clone(),
        )?;
        ensure!(self == &expected, "frozen model child contract/fingerprint mismatch");
        Ok(())
    }
}

/// Only the subject's admitted structured control reaches this constructor.
/// The binding is its sealed S0 transport, never mutable runtime environment.
pub(crate) fn prepare_model_delegation(
    task_id: EntityId, step_id: EntityId, spec: &ModelChildSpec,
    transport: &ProviderBinding, parent_max_tokens: u64, generation: &RuntimeGeneration,
) -> Result<DelegationIntent> {
    let frozen = FrozenModelChild::new(
        task_id, EntityId::new(), step_id, spec.clone(), transport, parent_max_tokens, generation.clone(),
    )?;
    Ok(DelegationIntent {
        intent_id: task_id, goal: spec.goal.clone(), system_prompt: spec.system_prompt.clone(),
        model: Some(spec.model.clone()), capabilities: vec![],
        budget_hint: Some(BudgetHint { max_tokens: Some(spec.max_tokens), max_duration_secs: None }),
        reply_grant: None,
        definition: Some(NativeTaskDefinition {
            kind: MODEL_CHILD_KIND.into(), definition_version: 1, input: serde_json::to_value(frozen)?,
        }),
    })
}

fn plugin_id() -> EntityId {
    "de5bdc7a-ae54-45d5-948c-125f515f866d".parse().expect("fixed model child plugin UUID")
}

fn contract_digest() -> Digest {
    // Recovery pins the executing code as well as its role contract.
    Digest::blake3(include_bytes!("model_child.rs"))
}

fn seal<T: Serialize + ?Sized>(store: &Store, value: &T, schema: &str) -> Result<ObjectRef> {
    match store.cas.seal_object(&serde_json::to_vec(value)?, "application/json", "model-child-effects", schema, 1) {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => { store.close_external_writer(); Err(error.into()) }
    }
}

fn read_frozen(store: &Store, object: &ObjectRef) -> Result<FrozenModelChild> {
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest, "model child input closure mismatch");
    let frozen: FrozenModelChild = serde_json::from_slice(&bytes)?;
    frozen.validate()?;
    Ok(frozen)
}

pub struct ModelChildHandler {
    store: Arc<Store>,
    broker: BrokerHandle,
}

impl ModelChildHandler {
    /// Restore debt BEFORE starting the broker or subject. Include cancelled
    /// tasks: cancellation is not provider termination, and no callback is
    /// needed to reconstruct an original armed/unknown request after restart.
    pub fn new(store: Arc<Store>, broker: BrokerHandle) -> Result<Self> {
        let mut after = None;
        loop {
            let page = store.external_operations_after(OPERATION, after, 128)?;
            if page.is_empty() { break; }
            for operation in &page {
                let attempt = store.external_operation_attempt(operation.operation_id)?;
                let frozen = read_frozen(&store, &operation.final_input_ref)?;
                broker.register_recovered(Arc::new(RecoveryReceipt {
                    store: Arc::clone(&store), operation: operation.clone(), attempt, frozen,
                }))?;
                after = Some(operation.operation_id);
            }
        }
        Ok(Self { store, broker })
    }

    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let frozen: FrozenModelChild = serde_json::from_value(invocation.input)?;
        frozen.validate()?;
        ensure!(frozen.task_id == invocation.task.task_id
            && Some(frozen.created_by_step_id) == invocation.task.created_by_step_id
            && frozen.software_generation == invocation.context.software_generation
            && invocation.context.role_revision == ROLE, "model child task/role/generation mismatch");
        let broker = self.broker.for_model(&frozen.spec.model)?;
        ensure!(broker.binding() == &frozen.provider_binding, "MODEL_PROVIDER_BINDING_MISMATCH: reconcile frozen child before changing transport");
        let input_ref = seal(&self.store, &frozen, "model-child-frozen-input")?;
        if let Some(existing) = self.store.get_operation(invocation.operation_id)? {
            ensure!(existing.task_id == frozen.task_id && existing.operation_name == OPERATION
                && existing.input_digest == input_ref.digest, "original model child operation mismatch");
            return Ok(existing); // A repeated T1 identity is never another send.
        }
        let request_id = frozen.request_id;
        let quota_send = match quota_dispatch_gate(
            &self.store, &frozen.provider_binding, request_id,
            &tokio_util::sync::CancellationToken::new(), Some(&invocation.task_invocation),
        ).await? {
            QuotaGate::Ready(send) => send,
            QuotaGate::Cancelled => anyhow::bail!("model child cancelled before quota-bound T1"),
        };
        let send_binding = InferenceSendBinding { schema_version: 1, request_id, send_id: quota_send };
        let transport_ref = seal(&self.store, &send_binding, "model-child-selected-send")?;
        let resources = ResourceBindings {
            provider: frozen.provider_binding.profile_id().into(),
            // The sealed binding's non-secret credential fingerprint: two
            // accounts on the same endpoint/model are distinct identities.
            account_identity: match &frozen.provider_binding {
                ProviderBinding::Anthropic { credential_fingerprint, .. } => credential_fingerprint
                    .as_ref().map(|fp| format!("credential:{fp}")).unwrap_or_default(),
                _ => String::new(),
            },
            runtime_node: invocation.task_invocation.node_id.to_string(), mount_generation: 0,
            capability_level: "finite_text_generation_no_tools".into(),
            extra: serde_json::json!({
                "provider_binding": frozen.provider_binding,
                "request_id": request_id, "task_id": frozen.task_id,
                "selected_send_id": quota_send,
                "role_revision": ROLE, "max_tokens": frozen.spec.max_tokens,
                "prompt_fingerprint": frozen.prompt_fingerprint,
                "toolset_fingerprint": frozen.toolset_fingerprint,
                "payload_fingerprint": frozen.payload_fingerprint,
                "checkpoint_identity_verified": false,
            }),
        };
        let digest = contract_digest();
        let version = VersionRef {
            artifact: ArtifactRef { digest: digest.clone(), locator: "builtin:model-child-v1".into() },
            symbol: OPERATION.into(),
            schema: SchemaRef { name: "model-child-provider-result".into(), major: 1, digest: digest.clone() },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1, operation_id: invocation.operation_id,
            record_revision: RecordRevision(0), task_id: frozen.task_id, logical_key: "native-handler".into(),
            operation_name: OPERATION.into(), input_schema_version: 1, output_schema_version: 1,
            final_input_ref: input_ref.clone(), input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: plugin_id(), artifact_digest: digest.clone(), contract_digest: digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: seal(&self.store, &resources, "model-child-resource-binding")?,
            authority_ref: invocation.authority,
            acceptance_predicate_ref: seal(&self.store, "matching terminal generation with stop/end_turn/stop_sequence; not a semantic goal-success judgement", "model-child-acceptance")?,
            retry_contract_ref: seal(&self.store, "one armed attempt; no automatic replay, including uncertain send; reconcile original evidence", "model-child-retry")?,
            checkpoint_scope_ref: None, state: OperationState::InFlight, outcome: None,
            outcome_conflict: false, effect_status: EffectStatus::Unknown,
            retry_of: None, compensates: None, possible_duplicate_of: vec![],
            prepared_commit: zero, evidence_through: None,
        };
        let mut attempt = AttemptRecord {
            record_schema_version: 1, attempt_id: EntityId::new(), operation_id: operation.operation_id,
            attempt_no: 1, invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id, node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch, implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed, armed_commit: zero,
            provider_handle: None, provider_idempotency: None, transport_ref: Some(transport_ref),
            started_at: None, last_observed_at: None, final_receipt_id: None,
        };
        if let Some(existing) = self.store.arm_external_inference_operation(&invocation.task_invocation, &mut operation, &mut attempt, &send_binding)? {
            return Ok(existing);
        }
        // T1 is now durable; it does not prove the queued request was sent.
        // The broker owns receipt recording even if this managed waiter dies.
        let sink = Arc::new(ChildReceiptSink {
            store: Arc::clone(&self.store), invocation: invocation.task_invocation,
            frozen, input_ref, operation: operation.clone(), attempt,
            send_binding,
        });
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let req = InferenceRequest {
            request_id, agent_id: invocation.task.agent_id,
            conversation_id: invocation.task.conversation_id,
            provider_binding: sink.frozen.provider_binding.clone(), priority: InferencePriority::Normal,
            epoch: sink.invocation.epoch, payload: sink.frozen.payload.clone(), result_tx,
        };
        if let Err(error) = broker.submit_recorded(req, sink) {
            // A generic admission error is not authority to reinterpret an
            // already armed identity as safe to replay (it may be a duplicate).
            broker.recover_unknown(request_id);
            return Err(error);
        }
        let result = result_rx.await.context("model child outcome not durably settled; reconcile original attempt")?;
        ensure!(result.request_id == request_id, "model child result identity mismatch");
        self.store.external_operation(operation.operation_id)
    }
}

struct ChildReceiptSink {
    store: Arc<Store>,
    invocation: TaskInvocation,
    frozen: FrozenModelChild,
    input_ref: ObjectRef,
    operation: OperationRecord,
    attempt: AttemptRecord,
    send_binding: InferenceSendBinding,
}

impl InferenceReceiptSink for ChildReceiptSink {
    fn may_dispatch(&self) -> Result<DispatchFence> {
        // The durable send-boundary transition: serialized with cancellation
        // and authority changes inside one immediate transaction. A cancelled
        // task is fenced before any send; a committed dispatch is reported as
        // unknown so the original attempt stays recoverable and is never
        // re-sent or marked never-dispatched.
        Ok(match self.store.commit_native_dispatch(&self.invocation)? {
            shenko_store::child::NativeDispatchTransition::Won => DispatchFence::Send,
            shenko_store::child::NativeDispatchTransition::Fenced => DispatchFence::Fenced,
            shenko_store::child::NativeDispatchTransition::AlreadyCommitted => DispatchFence::Unknown,
        })
    }

    fn record(&self, result: &InferenceResult) -> Result<()> {
        ensure!(result.request_id == self.frozen.request_id
            && result.effective.matches_seal(&self.frozen.provider_binding, &self.frozen.payload),
            "model child result differs from frozen request");
        let complete = result.status == InferenceStatus::Success
            && result.termination.kind == TerminationKind::ProviderTerminal
            && result.response.as_ref().is_some_and(|response|
                matches!(response.stop_reason.as_str(), "stop" | "end_turn" | "stop_sequence"));
        let (outcome, effect) = match result.termination.kind {
            TerminationKind::ProviderTerminal => (Some(if complete { OperationOutcome::Succeeded } else { OperationOutcome::Failed }), EffectStatus::Complete),
            TerminationKind::NeverDispatched => (Some(if result.status == InferenceStatus::Cancelled { OperationOutcome::Cancelled } else { OperationOutcome::Failed }), EffectStatus::None),
            TerminationKind::TransportRejected => (Some(OperationOutcome::Failed), EffectStatus::None),
            TerminationKind::Unknown => (None, EffectStatus::Unknown),
        };
        let output_ref = result.response.as_ref().map(|response| {
            self.store.cas.seal_object(response.content.as_bytes(), "text/plain", "model-child-effects", "model-child-text", 1)
                .map(|sealed| sealed.object)
        }).transpose().map_err(|error| { self.store.close_external_writer(); error })?;
        let text = result.response.as_ref().map(|response| {
            let end = response.content.char_indices().map(|(index, _)| index)
                .find(|index| *index > 64 * 1024).unwrap_or(response.content.len());
            &response.content[..end]
        });
        let raw = seal(&self.store, result, "model-child-provider-result")?;
        let normalized = seal(&self.store, &serde_json::json!({
            "schema_version": 1, "task_id": self.frozen.task_id,
            "request_id": result.request_id, "provider_binding": self.frozen.provider_binding,
            "selected_send_id": self.send_binding.send_id,
            "role_revision": ROLE, "max_tokens": self.frozen.spec.max_tokens,
            "prompt_fingerprint": self.frozen.prompt_fingerprint,
            "toolset_fingerprint": self.frozen.toolset_fingerprint,
            "payload_fingerprint": self.frozen.payload_fingerprint,
            "frozen_input_ref": self.input_ref,
            "generation_complete": complete, "status": result.status,
            "termination": result.termination,
            "text": text, "text_truncated": result.response.as_ref().is_some_and(|response| text.is_some_and(|text| text.len() != response.content.len())),
            "output_ref": output_ref,
            "stop_reason": result.response.as_ref().map(|response| &response.stop_reason),
            "usage": result.response.as_ref().and_then(|response| response.usage.as_ref()),
        }), "model-child-result")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1, receipt_id: EntityId::new(),
            operation_id: Some(self.operation.operation_id), attempt_id: Some(self.attempt.attempt_id),
            source_id: OPERATION.into(), source_instance_id: self.invocation.node_id.to_string(),
            source_event_key: Some(format!("model-request:{}", result.request_id)),
            producer_epoch: Some(self.invocation.epoch.0), producer_schema_version: Some("1".into()),
            raw_ref: raw, decoder_ref: Some(self.operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: (result.termination.kind == TerminationKind::ProviderTerminal)
                .then_some(ObservationLevel::RemoteCompleted),
            normalized_ref: Some(normalized), observed_at: Some(chrono::Utc::now()),
            received_commit: self.operation.prepared_commit,
        };
        // E1 intentionally has no task commit authority. Cancellation, epoch
        // changes and worker teardown cannot discard this original fact.
        self.store.record_external_evidence(self.operation.operation_id, self.attempt.attempt_id, evidence, outcome, effect)?;
        if let Some(send_id) = self.send_binding.send_id {
            observe_quota_response(&self.store, &self.frozen.provider_binding, result, self.frozen.request_id, send_id);
            settle_quota_terminal(&self.store, result, self.frozen.request_id, send_id);
        }
        Ok(())
    }
}

impl InferenceRecoverySource for ChildReceiptSink {
    fn request_id(&self) -> EntityId { self.frozen.request_id }
    fn binding(&self) -> &ProviderBinding { &self.frozen.provider_binding }
    fn payload(&self) -> &InferencePayload { &self.frozen.payload }
    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        read_original_terminal(&self.store, &self.operation, &self.attempt, &self.frozen)
    }
}

#[async_trait]
impl NativeTaskHandler for ModelChildHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: MODEL_CHILD_KIND.into(), definition_version: 1,
            implementation_ref: ImplementationRef { plugin_id: plugin_id(), artifact_digest: contract_digest(), registry_revision: 0 },
            role_revision: ROLE.into(), authority_refs: vec!["model.inference.finite-text/v1".into()],
            prompt_section: Some(format!(
                "有限模型 child 已注册：使用 [delegate] 的 goal/system_prompt/model/max_tokens JSON。当前 provider 通路为 {}；model 名称不代表已验证可用。每次仅一次文本 generation，tools=[]，T1/E1/T2 与主体共用 broker/资源域。",
                self.broker.binding().profile_id(),
            )),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(operation.task_id == task.task_id && operation.operation_name == OPERATION, "original model child operation mismatch");
        let frozen = read_frozen(&self.store, &operation.final_input_ref)?;
        ensure!(frozen.task_id == task.task_id, "original model child task mismatch");
        let attempt = self.store.external_operation_attempt(operation_id)?;
        self.broker.register_recovered(Arc::new(RecoveryReceipt {
            store: Arc::clone(&self.store), operation: operation.clone(), attempt, frozen,
        }))?;
        // These transports expose no original-request query/idempotency API.
        // Reading E1 is the only recovery action; absence remains unknown.
        Ok(operation)
    }
}

struct RecoveryReceipt {
    store: Arc<Store>,
    operation: OperationRecord,
    attempt: AttemptRecord,
    frozen: FrozenModelChild,
}

impl InferenceRecoverySource for RecoveryReceipt {
    fn request_id(&self) -> EntityId { self.frozen.request_id }
    fn binding(&self) -> &ProviderBinding { &self.frozen.provider_binding }
    fn payload(&self) -> &InferencePayload { &self.frozen.payload }

    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        read_original_terminal(&self.store, &self.operation, &self.attempt, &self.frozen)
    }
}

fn read_original_terminal(
    store: &Store, original: &OperationRecord, original_attempt: &AttemptRecord, frozen: &FrozenModelChild,
) -> Result<Option<InferenceResult>> {
        let operation = store.external_operation(original.operation_id)?;
        let attempt = store.external_attempt(original_attempt.attempt_id)?;
        ensure!(operation.operation_name == OPERATION
            && operation.task_id == frozen.task_id
            && attempt.operation_id == operation.operation_id
            && attempt.transport_ref == original_attempt.transport_ref
            && attempt.armed_commit == original_attempt.armed_commit
            && attempt.invocation_id == original_attempt.invocation_id
            && attempt.epoch == original_attempt.epoch
            && operation.final_input_ref == original.final_input_ref
            && operation.resource_bindings_ref == original.resource_bindings_ref
            && operation.contract_ref.contract_digest == original.contract_ref.contract_digest
            && operation.input_digest == original.input_digest
            && operation.input_digest == operation.final_input_ref.digest,
            "recovered inference operation/input identity mismatch");
        if attempt.dispatch_stage != AttemptDispatchStage::Ended { return Ok(None); }
        let id = attempt.final_receipt_id.context("ended model attempt lacks receipt")?;
        let evidence = store.external_evidence(id)?;
        ensure!(evidence.receipt_id == id && evidence.operation_id == Some(operation.operation_id)
            && evidence.attempt_id == Some(attempt.attempt_id) && evidence.source_id == OPERATION,
            "recovered inference final E1 identity mismatch");
        let bytes = store.cas.read_object(&evidence.raw_ref.digest)?;
        ensure!(bytes.len() as u64 == evidence.raw_ref.bytes
            && Digest::blake3(&bytes) == evidence.raw_ref.digest, "recovered inference receipt CAS mismatch");
        let result: InferenceResult = serde_json::from_slice(&bytes)?;
        ensure!(result.request_id == frozen.request_id
            && result.effective.matches_seal(&frozen.provider_binding, &frozen.payload)
            && result.termination.kind != TerminationKind::Unknown,
            "recovered inference final E1 lacks original terminal evidence");
        let binding_ref = attempt.transport_ref.as_ref().context("original inference attempt lacks selected-send metadata")?;
        let bytes = store.cas.read_object(&binding_ref.digest)?;
        ensure!(bytes.len() as u64 == binding_ref.bytes && Digest::blake3(&bytes) == binding_ref.digest,
            "original selected-send metadata CAS mismatch");
        let binding: InferenceSendBinding = serde_json::from_slice(&bytes)?;
        ensure!(binding.schema_version == 1 && binding.request_id == frozen.request_id,
            "original quota send belongs to another request");
        if let Some(send_id) = binding.send_id {
            // Retry a missing settlement from the exact original E1. Do not
            // replay header observations with a fresh clock/TTL on recovery.
            settle_quota_terminal(store, &result, frozen.request_id, send_id);
        }
        Ok(Some(result))
}
