//! The bounded memory-intervention worker as a native child task.
//! SPEC §14.4 (主动记忆 worker), §14.2 (四种状态不同处理).
//!
//! The worker is one finite task. It reads the structured bank of one
//! research epoch, asks the installed [`MemoryInterventionPolicy`] what to
//! ask, performs exactly one bounded generation through the shared broker
//! with the same durable request sealing every other model call uses, and
//! turns the answer into one decision: stay silent, or one reminder with
//! provenance, relevance and scope.
//!
//! What this handler deliberately does *not* do:
//!
//! * it does not decide *whether* to run. The trigger is a task of the
//!   subject's own making; the policy only decides what the worker says;
//! * it does not own a goal loop. The task ends with one decision;
//! * it does not filter reminders by the user's personal interest and it
//!   never asks a human for approval before a reminder is published;
//! * it does not touch the subject's identity, plan or context window. It
//!   records a decision and, for a reminder, publishes exactly one context
//!   contribution for the request path.
//!
//! The decision is durable before it is visible: the reminder row, its
//! provenance and the usage receipt live in the store, and the contribution
//! body is a sealed object rather than inline text.

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_broker::broker::{BrokerHandle, DispatchFence, InferenceReceiptSink, InferenceRecoverySource};
use shenko_broker::request::*;
use shenko_memory::{
    BankSnapshot, InterventionBudget, InterventionDecision, InterventionModelReply,
    InterventionObservation, MemoryInterventionPolicy, MemoryInterventionRegistry,
    ReminderPublisher, TrajectoryItem, epoch_scope,
};
use shenko_store::external::InferenceSendBinding;
use shenko_store::{
    BankSelection, BankEntryKind, InterventionDecisionKind, InterventionReminder, Store,
};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel};
use shenko_types::ids::*;
use shenko_types::object::*;
use shenko_types::operation::*;
use shenko_types::task::TaskRecord;
use std::sync::Arc;

use crate::quota_gate::{QuotaGate, observe_quota_response, quota_dispatch_gate, settle_quota_terminal};
use crate::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};

/// The native definition kind of the memory-intervention worker.
pub const PMA_EVALUATE_KIND: &str = "pma.evaluate";
/// The durable operation name of one bounded intervention call.
pub const PMA_OPERATION: &str = "pma.evaluate.v1";
/// The role revision of the worker.
pub const PMA_ROLE: &str = "memory-intervention/worker@1";
/// The default lifetime of a reminder that names no expiry.
pub const DEFAULT_REMINDER_TTL_SECS: u64 = 600;

/// The request the subject hands to the worker. It names the target lineage
/// and epoch; the observation itself is read from the store, so the model
/// never has to carry the bank in its output.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InterventionRequest {
    pub lineage_id: EntityId,
    pub epoch_id: EntityId,
    /// `subject` or `epoch:<id>`. Defaults to the epoch of the request.
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_trajectory_items: Option<usize>,
    #[serde(default)]
    pub max_bank_entries: Option<usize>,
    /// The bounded trajectory the worker observes. When it is absent the
    /// worker observes the recorded attempts of the epoch.
    #[serde(default)]
    pub trajectory: Option<Vec<TrajectoryItem>>,
    /// The model that performs the bounded call. `None` keeps the broker's
    /// bound model.
    #[serde(default)]
    pub model: Option<String>,
}

/// The sealed input of one bounded intervention call. It binds the task, the
/// delegation that produced it, the provider, the payload and the policy
/// that asked for it, so a recovered call can be checked against exactly the
/// request that was sealed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenIntervention {
    schema_version: u32,
    task_id: EntityId,
    request_id: EntityId,
    created_by_step_id: EntityId,
    agent_id: EntityId,
    epoch_id: EntityId,
    lineage_id: EntityId,
    /// The delegation input of this task, verbatim.
    delegation: serde_json::Value,
    provider_binding: ProviderBinding,
    payload: InferencePayload,
    payload_fingerprint: Digest,
    software_generation: RuntimeGeneration,
    role_revision: String,
    policy_name: String,
    policy_revision: String,
    /// `true` when the policy decided without an inference.
    decided_without_call: bool,
}

impl FrozenIntervention {
    fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported frozen intervention version");
        ensure!(self.role_revision == PMA_ROLE, "frozen intervention role mismatch");
        ensure!(
            Digest::blake3(&serde_json::to_vec(&self.payload)?) == self.payload_fingerprint,
            "frozen intervention payload fingerprint mismatch"
        );
        ensure!(
            self.payload.tools.is_empty(),
            "the memory intervention worker has no tool schema"
        );
        Ok(())
    }
}

fn plugin_id() -> EntityId {
    "9c2f6d51-4a83-4c2b-9f77-2b6e51d0c913"
        .parse()
        .expect("fixed memory intervention plugin UUID")
}

fn contract_digest() -> Digest {
    // Recovery pins the executing code as well as its role contract.
    Digest::blake3(include_bytes!("intervention.rs"))
}

fn seal<T: Serialize + ?Sized>(store: &Store, value: &T, schema: &str) -> Result<ObjectRef> {
    match store.cas.seal_object(&serde_json::to_vec(value)?, "application/json", "pma-effects", schema, 1) {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => {
            store.close_external_writer();
            Err(error.into())
        }
    }
}

fn read_frozen(store: &Store, object: &ObjectRef) -> Result<FrozenIntervention> {
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
        "memory intervention input closure mismatch"
    );
    let frozen: FrozenIntervention = serde_json::from_slice(&bytes)?;
    frozen.validate()?;
    Ok(frozen)
}

/// The native handler of the memory-intervention worker.
pub struct PmaEvaluateHandler {
    store: Arc<Store>,
    broker: BrokerHandle,
    policy: Arc<MemoryInterventionRegistry>,
    publisher: Option<Arc<dyn ReminderPublisher>>,
}

impl PmaEvaluateHandler {
    pub fn new(
        store: Arc<Store>,
        broker: BrokerHandle,
        policy: Arc<MemoryInterventionRegistry>,
    ) -> Self {
        Self { store, broker, policy, publisher: None }
    }

    /// Install the seam that publishes a prepared reminder into the request
    /// path. Without it the decision is still durable, it is simply not
    /// projected into a later request.
    pub fn with_publisher(mut self, publisher: Arc<dyn ReminderPublisher>) -> Self {
        self.publisher = Some(publisher);
        self
    }

    /// The observation the policy decides on: the epoch's bank and the
    /// recorded attempts, both bounded.
    fn observation(
        &self,
        request: &InterventionRequest,
        epoch_id: EntityId,
        lineage_id: EntityId,
    ) -> Result<InterventionObservation> {
        let entries = self.store.bank_entries(epoch_id, BankSelection::All)?;
        let now = chrono::Utc::now().timestamp_millis();
        let bank: Vec<BankSnapshot> = entries
            .iter()
            .map(|entry| BankSnapshot {
                entry_id: entry.entry_id,
                kind: entry.kind.as_str().to_string(),
                content: entry.content.clone(),
                source_refs: entry.source_refs.clone(),
                scope: entry.scope.clone(),
                confidence: entry.confidence,
                age_ms: now - entry.created_at_ms,
            })
            .collect();
        let trajectory = match request.trajectory.clone() {
            Some(items) => items,
            None => entries
                .iter()
                .filter(|entry| !entry.kind.survives_reset())
                .map(|entry| TrajectoryItem {
                    kind: entry.kind.as_str().to_string(),
                    summary: entry.content.clone(),
                    at_ms: entry.created_at_ms,
                })
                .collect(),
        };
        Ok(InterventionObservation {
            epoch_id,
            lineage_id,
            now_ms: now,
            budget: InterventionBudget {
                max_tokens: request.max_tokens.unwrap_or(1024).clamp(64, 4096),
                max_trajectory_items: request.max_trajectory_items.unwrap_or(16).min(64),
                max_bank_entries: request.max_bank_entries.unwrap_or(32).min(128),
            },
            trajectory,
            bank,
        })
    }

    /// The decision of one bounded call, derived from the recorded answer.
    fn decide(
        &self,
        frozen: &FrozenIntervention,
        observation: &InterventionObservation,
        reply: &InterventionModelReply,
    ) -> Result<InterventionDecision> {
        let policy = self.policy.current();
        ensure!(
            policy.name() == frozen.policy_name && policy.revision() == frozen.policy_revision,
            "the installed intervention policy changed after the request was sealed"
        );
        Ok(policy.interpret(observation, reply)?)
    }

    /// Record the decision and, for a reminder, publish it into the request
    /// path. The decision is durable before it becomes visible.
    fn record_decision(
        &self,
        frozen: &FrozenIntervention,
        request: &InterventionRequest,
        observation: &InterventionObservation,
        decision: &InterventionDecision,
    ) -> Result<InterventionReminder> {
        let now = chrono::Utc::now().timestamp_millis();
        let (kind, content, provenance, relevance, scope, expires_at_ms, reason) = match decision {
            InterventionDecision::Silent { reason } => (
                InterventionDecisionKind::Silent,
                None,
                Vec::new(),
                None,
                request
                    .scope
                    .clone()
                    .unwrap_or_else(|| epoch_scope(frozen.epoch_id)),
                None,
                Some(reason.clone()),
            ),
            InterventionDecision::Remind {
                content,
                provenance,
                relevance,
                scope,
                expires_in_secs,
            } => (
                InterventionDecisionKind::Remind,
                Some(content.clone()),
                provenance.clone(),
                Some(relevance.clone()),
                scope.clone(),
                Some(now + expires_in_secs.unwrap_or(DEFAULT_REMINDER_TTL_SECS) * 1000),
                None,
            ),
        };
        let reminder = InterventionReminder {
            // One bounded call produces exactly one decision, so the request
            // identity is the decision identity: a replay records it once.
            reminder_id: frozen.request_id,
            epoch_id: frozen.epoch_id,
            lineage_id: frozen.lineage_id,
            decision: kind,
            policy: frozen.policy_name.clone(),
            policy_revision: frozen.policy_revision.clone(),
            content,
            provenance,
            relevance,
            scope,
            expires_at_ms,
            transient: true,
            bank_entry_ids: observation
                .bank
                .iter()
                .filter(|entry| {
                    decision_provenance(decision).iter().any(|reference| {
                        reference == &entry.entry_id.to_string()
                            || entry.source_refs.iter().any(|source| source == reference)
                    })
                })
                .map(|entry| entry.entry_id)
                .collect(),
            reason,
            created_at_ms: now,
            used_step_id: None,
            used_at_ms: None,
        };
        self.store.record_intervention(&reminder)?;
        Ok(reminder)
    }

    /// Publish a reminder as one context contribution for the scope it was
    /// addressed to.
    fn publish_reminder(
        &self,
        reminder: &InterventionReminder,
        agent_id: EntityId,
    ) -> Result<()> {
        let Some(publisher) = self.publisher.as_ref() else {
            return Ok(());
        };
        if reminder.decision != InterventionDecisionKind::Remind {
            return Ok(());
        }
        let provider = shenko_memory::PmaContributionProvider::new(Arc::clone(&self.store));
        let scope_id = self.scope_id_for(&reminder.scope, reminder.epoch_id, agent_id)?;
        let scope = shenko_context_policy::contribution::ContributionScope {
            scope_id,
            task_id: None,
        };
        let contribution =
            provider.contribution_for(reminder, &scope, chrono::Utc::now().timestamp_millis())?;
        publisher.publish(&scope, vec![contribution])?;
        Ok(())
    }

    /// The scope a reminder belongs to. A subject-directed reminder is
    /// published under the subject's current branch, a worker-directed one
    /// under its epoch.
    fn scope_id_for(
        &self,
        scope: &str,
        epoch_id: EntityId,
        agent_id: EntityId,
    ) -> Result<EntityId> {
        if scope == shenko_memory::SUBJECT_SCOPE {
            let (head, _step) = self.store.load_subject(agent_id)?;
            return Ok(head.branch_id);
        }
        if let Some(rest) = scope.strip_prefix("epoch:") {
            let parsed: EntityId = rest
                .parse()
                .map_err(|_| anyhow::anyhow!("reminder scope is not a valid epoch identity"))?;
            ensure!(parsed == epoch_id, "reminder scope names another epoch");
            return Ok(parsed);
        }
        Err(anyhow::anyhow!(
            "reminder scope must be `subject` or `epoch:<id>`, not `{scope}`"
        ))
    }
}

fn decision_provenance(decision: &InterventionDecision) -> Vec<String> {
    match decision {
        InterventionDecision::Silent { .. } => Vec::new(),
        InterventionDecision::Remind { provenance, .. } => provenance.clone(),
    }
}

struct InterventionReceiptSink {
    store: Arc<Store>,
    invocation: shenko_store::TaskInvocation,
    frozen: FrozenIntervention,
    input_ref: ObjectRef,
    operation: OperationRecord,
    attempt: AttemptRecord,
    send_binding: InferenceSendBinding,
}

impl InferenceReceiptSink for InterventionReceiptSink {
    fn may_dispatch(&self) -> Result<DispatchFence> {
        Ok(match self.store.commit_native_dispatch(&self.invocation)? {
            shenko_store::child::NativeDispatchTransition::Won => DispatchFence::Send,
            shenko_store::child::NativeDispatchTransition::Fenced => DispatchFence::Fenced,
            shenko_store::child::NativeDispatchTransition::AlreadyCommitted => {
                DispatchFence::Unknown
            }
        })
    }

    fn record(&self, result: &InferenceResult) -> Result<()> {
        ensure!(
            result.request_id == self.frozen.request_id
                && result
                    .effective
                    .matches_seal(&self.frozen.provider_binding, &self.frozen.payload),
            "memory intervention result differs from the frozen request"
        );
        let complete = result.status == InferenceStatus::Success
            && result.termination.kind == TerminationKind::ProviderTerminal
            && result.response.as_ref().is_some_and(|response| {
                matches!(response.stop_reason.as_str(), "stop" | "end_turn" | "stop_sequence")
            });
        let (outcome, effect) = match result.termination.kind {
            TerminationKind::ProviderTerminal => (
                Some(if complete {
                    OperationOutcome::Succeeded
                } else {
                    OperationOutcome::Failed
                }),
                EffectStatus::Complete,
            ),
            TerminationKind::NeverDispatched => (
                Some(if result.status == InferenceStatus::Cancelled {
                    OperationOutcome::Cancelled
                } else {
                    OperationOutcome::Failed
                }),
                EffectStatus::None,
            ),
            TerminationKind::TransportRejected => (Some(OperationOutcome::Failed), EffectStatus::None),
            TerminationKind::Unknown => (None, EffectStatus::Unknown),
        };
        let text = result.response.as_ref().map(|response| {
            let end = response
                .content
                .char_indices()
                .map(|(index, _)| index)
                .find(|index| *index > 64 * 1024)
                .unwrap_or(response.content.len());
            response.content[..end].to_string()
        });
        let raw = seal(&self.store, result, "pma-provider-result")?;
        let normalized = seal(
            &self.store,
            &serde_json::json!({
                "schema_version": 1,
                "task_id": self.frozen.task_id,
                "request_id": result.request_id,
                "provider_binding": self.frozen.provider_binding,
                "selected_send_id": self.send_binding.send_id,
                "role_revision": PMA_ROLE,
                "policy": self.frozen.policy_name,
                "policy_revision": self.frozen.policy_revision,
                "max_tokens": self.frozen.payload.max_tokens,
                "payload_fingerprint": self.frozen.payload_fingerprint,
                "frozen_input_ref": self.input_ref,
                "generation_complete": complete,
                "status": result.status,
                "termination": result.termination,
                "text": text,
                "usage": result.response.as_ref().and_then(|response| response.usage.as_ref()),
            }),
            "pma-result",
        )?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(self.operation.operation_id),
            attempt_id: Some(self.attempt.attempt_id),
            source_id: PMA_OPERATION.into(),
            source_instance_id: self.invocation.node_id.to_string(),
            source_event_key: Some(format!("pma-request:{}", result.request_id)),
            producer_epoch: Some(self.invocation.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(self.operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: (result.termination.kind == TerminationKind::ProviderTerminal)
                .then_some(ObservationLevel::RemoteCompleted),
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: self.operation.prepared_commit,
        };
        // E1 has no task commit authority; cancellation or teardown cannot
        // discard this original fact.
        self.store.record_external_evidence(
            self.operation.operation_id,
            self.attempt.attempt_id,
            evidence,
            outcome,
            effect,
        )?;
        if let Some(send_id) = self.send_binding.send_id {
            observe_quota_response(
                &self.store,
                &self.frozen.provider_binding,
                result,
                self.frozen.request_id,
                send_id,
            );
            settle_quota_terminal(
                &self.store,
                result,
                self.frozen.request_id,
                send_id,
            );
        }
        Ok(())
    }
}

impl InferenceRecoverySource for InterventionReceiptSink {
    fn request_id(&self) -> EntityId {
        self.frozen.request_id
    }
    fn binding(&self) -> &ProviderBinding {
        &self.frozen.provider_binding
    }
    fn payload(&self) -> &InferencePayload {
        &self.frozen.payload
    }
    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        read_original_terminal(&self.store, &self.operation, &self.attempt, &self.frozen)
    }
}

#[async_trait]
impl NativeTaskHandler for PmaEvaluateHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: PMA_EVALUATE_KIND.into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(),
                artifact_digest: contract_digest(),
                registry_revision: 0,
            },
            role_revision: PMA_ROLE.into(),
            authority_refs: vec!["model.inference.finite-text/v1".into()],
            prompt_section: Some(format!(
                "记忆干预 worker 已注册（kind={PMA_EVALUATE_KIND}, version=1）：用 [delegate:{PMA_EVALUATE_KIND}:1] 传入 JSON {{\"lineage_id\":\"...\",\"epoch_id\":\"...\"}} 启动一次有界判断；它只输出保持沉默或一条带出处的提醒，不接管主体计划，也不需要任何人批准。当前 provider 通路为 {}。",
                self.broker.binding().profile_id(),
            )),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == PMA_OPERATION,
            "original memory intervention operation mismatch"
        );
        let frozen = read_frozen(&self.store, &operation.final_input_ref)?;
        ensure!(frozen.task_id == task.task_id, "original memory intervention task mismatch");
        let attempt = self.store.external_operation_attempt(operation_id)?;
        self.broker.register_recovered(Arc::new(RecoveryReceipt {
            store: Arc::clone(&self.store),
            operation: operation.clone(),
            attempt,
            frozen,
        }))?;
        Ok(operation)
    }
}

struct RecoveryReceipt {
    store: Arc<Store>,
    operation: OperationRecord,
    attempt: AttemptRecord,
    frozen: FrozenIntervention,
}

impl InferenceRecoverySource for RecoveryReceipt {
    fn request_id(&self) -> EntityId {
        self.frozen.request_id
    }
    fn binding(&self) -> &ProviderBinding {
        &self.frozen.provider_binding
    }
    fn payload(&self) -> &InferencePayload {
        &self.frozen.payload
    }
    fn read_terminal_result(&self) -> Result<Option<InferenceResult>> {
        read_original_terminal(&self.store, &self.operation, &self.attempt, &self.frozen)
    }
}

fn read_original_terminal(
    store: &Store,
    original: &OperationRecord,
    original_attempt: &AttemptRecord,
    frozen: &FrozenIntervention,
) -> Result<Option<InferenceResult>> {
    let operation = store.external_operation(original.operation_id)?;
    let attempt = store.external_attempt(original_attempt.attempt_id)?;
    ensure!(
        operation.operation_name == PMA_OPERATION
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
        "recovered memory intervention operation/input identity mismatch"
    );
    if attempt.dispatch_stage != AttemptDispatchStage::Ended {
        return Ok(None);
    }
    let id = attempt
        .final_receipt_id
        .context("ended memory intervention attempt lacks receipt")?;
    let evidence = store.external_evidence(id)?;
    ensure!(
        evidence.receipt_id == id
            && evidence.operation_id == Some(operation.operation_id)
            && evidence.attempt_id == Some(attempt.attempt_id)
            && evidence.source_id == PMA_OPERATION,
        "recovered memory intervention final E1 identity mismatch"
    );
    let bytes = store.cas.read_object(&evidence.raw_ref.digest)?;
    ensure!(
        bytes.len() as u64 == evidence.raw_ref.bytes
            && Digest::blake3(&bytes) == evidence.raw_ref.digest,
        "recovered memory intervention receipt CAS mismatch"
    );
    let result: InferenceResult = serde_json::from_slice(&bytes)?;
    ensure!(
        result.request_id == frozen.request_id
            && result
                .effective
                .matches_seal(&frozen.provider_binding, &frozen.payload)
            && result.termination.kind != TerminationKind::Unknown,
        "recovered memory intervention final E1 lacks original terminal evidence"
    );
    if let Some(binding_ref) = attempt.transport_ref.as_ref() {
        let bytes = store.cas.read_object(&binding_ref.digest)?;
        ensure!(
            bytes.len() as u64 == binding_ref.bytes
                && Digest::blake3(&bytes) == binding_ref.digest,
            "original selected-send metadata CAS mismatch"
        );
        let binding: InferenceSendBinding = serde_json::from_slice(&bytes)?;
        ensure!(
            binding.schema_version == 1 && binding.request_id == frozen.request_id,
            "original quota send belongs to another request"
        );
        if let Some(send_id) = binding.send_id {
            settle_quota_terminal(store, &result, frozen.request_id, send_id);
        }
    }
    Ok(Some(result))
}

impl PmaEvaluateHandler {
    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let request: InterventionRequest = serde_json::from_value(invocation.input.clone())
            .context("memory intervention request is not a valid JSON request")?;
        let epoch = self
            .store
            .research_epoch(request.epoch_id)?
            .ok_or_else(|| anyhow::anyhow!("RESEARCH_EPOCH_NOT_FOUND: the target epoch does not exist"))?;
        ensure!(
            epoch.lineage_id == request.lineage_id,
            "RESEARCH_LINEAGE_MISMATCH: the epoch belongs to another lineage"
        );
        let observation =
            self.observation(&request, epoch.epoch_id, epoch.lineage_id)?;
        let policy = self.policy.current();
        let plan = policy.plan(&observation)?;
        ensure!(
            plan.call.is_some() || plan.decision.is_some(),
            "INTERVENTION_PLAN_EMPTY: the policy asked for neither a call nor a decision"
        );

        let request_id = EntityId::new();
        let broker = match plan.call.as_ref().and_then(|call| call.model.clone()) {
            Some(model) => self.broker.for_model(&model)?,
            None => self.broker.clone(),
        };
        let binding = broker.binding().clone();
        let payload = plan.call.as_ref().map(|call| InferencePayload {
            system_prompt: call.system_prompt.clone(),
            messages: vec![InferenceMessage::text("user", &call.user_prompt)],
            max_tokens: Some(call.max_tokens),
            tools: vec![],
            native_options: Default::default(),
        });
        // The quota-bound send is selected before the attempt is armed: the
        // armed attempt must carry the exact send that may go out.
        let quota_send = match quota_dispatch_gate(
            &self.store,
            &binding,
            request_id,
            &tokio_util::sync::CancellationToken::new(),
            Some(&invocation.task_invocation),
        )
        .await?
        {
            QuotaGate::Ready(send) => send,
            QuotaGate::Cancelled => {
                anyhow::bail!("memory intervention cancelled before the quota-bound T1")
            }
        };
        let send_binding = InferenceSendBinding { schema_version: 1, request_id, send_id: quota_send };
        let transport_ref = seal(&self.store, &send_binding, "pma-selected-send")?;
        let frozen = FrozenIntervention {
            schema_version: 1,
            task_id: invocation.task.task_id,
            request_id,
            created_by_step_id: invocation.task.created_by_step_id.unwrap_or(invocation.task_invocation.invocation_id),
            agent_id: invocation.task.agent_id,
            epoch_id: epoch.epoch_id,
            lineage_id: epoch.lineage_id,
            delegation: invocation.input.clone(),
            provider_binding: binding.clone(),
            payload: payload.clone().unwrap_or_else(|| InferencePayload {
                system_prompt: String::new(),
                messages: vec![],
                max_tokens: None,
                tools: vec![],
                native_options: Default::default(),
            }),
            payload_fingerprint: Digest::blake3(
                &serde_json::to_vec(&payload.clone().unwrap_or_else(|| InferencePayload {
                    system_prompt: String::new(),
                    messages: vec![],
                    max_tokens: None,
                    tools: vec![],
                    native_options: Default::default(),
                }))?,
            ),
            software_generation: invocation.context.software_generation.clone(),
            role_revision: PMA_ROLE.into(),
            policy_name: policy.name().to_string(),
            policy_revision: policy.revision().to_string(),
            decided_without_call: plan.call.is_none(),
        };
        frozen.validate()?;
        let input_ref = seal(&self.store, &frozen, "pma-frozen-input")?;
        if let Some(existing) = self.store.get_operation(invocation.operation_id)? {
            ensure!(
                existing.task_id == frozen.task_id
                    && existing.operation_name == PMA_OPERATION
                    && existing.input_digest == input_ref.digest,
                "original memory intervention operation mismatch"
            );
            return Ok(existing);
        }
        let digest = contract_digest();
        let version = VersionRef {
            artifact: ArtifactRef { digest: digest.clone(), locator: "builtin:pma-evaluate-v1".into() },
            symbol: PMA_OPERATION.into(),
            schema: SchemaRef {
                name: "pma-decision-result".into(),
                major: 1,
                digest: digest.clone(),
            },
        };
        let resources = ResourceBindings {
            provider: binding.profile_id().into(),
            account_identity: match &binding {
                ProviderBinding::Anthropic { credential_fingerprint, .. } => credential_fingerprint
                    .as_ref()
                    .map(|fingerprint| format!("credential:{fingerprint}"))
                    .unwrap_or_default(),
                _ => String::new(),
            },
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "finite_text_generation_no_tools".into(),
            extra: serde_json::json!({
                "provider_binding": binding,
                "request_id": request_id,
                "task_id": frozen.task_id,
                "epoch_id": frozen.epoch_id,
                "lineage_id": frozen.lineage_id,
                "selected_send_id": quota_send,
                "role_revision": PMA_ROLE,
                "policy": frozen.policy_name,
                "policy_revision": frozen.policy_revision,
                "payload_fingerprint": frozen.payload_fingerprint,
                "decided_without_call": frozen.decided_without_call,
            }),
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: frozen.task_id,
            logical_key: "native-handler".into(),
            operation_name: PMA_OPERATION.into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: plugin_id(),
                artifact_digest: digest.clone(),
                contract_digest: digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: seal(&self.store, &resources, "pma-resource-binding")?,
            authority_ref: invocation.authority,
            acceptance_predicate_ref: seal(
                &self.store,
                "one terminal bounded decision (silent or one reminder) with provenance; not a semantic goal-success judgement",
                "pma-acceptance",
            )?,
            retry_contract_ref: seal(
                &self.store,
                "one armed attempt; no automatic replay, including uncertain send; reconcile original evidence",
                "pma-retry",
            )?,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let mut attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: operation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: Some(transport_ref),
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        if let Some(existing) =
            self.store
                .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }

        let decision = match (plan.call, payload) {
            (None, _) => {
                // The policy decided without an inference. The decision itself
                // is the durable effect of this attempt.
                let decision = plan
                    .decision
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("INTERVENTION_PLAN_EMPTY"))?;
                let raw = seal(&self.store, &decision, "pma-local-decision")?;
                let normalized = seal(
                    &self.store,
                    &serde_json::json!({
                        "schema_version": 1,
                        "task_id": frozen.task_id,
                        "request_id": frozen.request_id,
                        "policy": frozen.policy_name,
                        "policy_revision": frozen.policy_revision,
                        "decided_without_call": true,
                        "decision": decision,
                    }),
                    "pma-decision",
                )?;
                let evidence = EvidenceReceipt {
                    record_schema_version: 1,
                    receipt_id: EntityId::new(),
                    operation_id: Some(operation.operation_id),
                    attempt_id: Some(attempt.attempt_id),
                    source_id: PMA_OPERATION.into(),
                    source_instance_id: invocation.task_invocation.node_id.to_string(),
                    source_event_key: Some(format!("pma-decision:{}", frozen.request_id)),
                    producer_epoch: Some(invocation.task_invocation.epoch.0),
                    producer_schema_version: Some("1".into()),
                    raw_ref: raw,
                    decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
                    decode_status: DecodeStatus::Decoded,
                    // No provider was contacted: there is no remote
                    // observation to report for a locally decided attempt.
                    observation_level: None,
                    normalized_ref: Some(normalized),
                    observed_at: Some(chrono::Utc::now()),
                    received_commit: operation.prepared_commit,
                };
                self.store.record_external_evidence(
                    operation.operation_id,
                    attempt.attempt_id,
                    evidence,
                    Some(OperationOutcome::Succeeded),
                    EffectStatus::Complete,
                )?;
                decision
            }
            (Some(_call), Some(payload)) => {
                let sink = Arc::new(InterventionReceiptSink {
                    store: Arc::clone(&self.store),
                    invocation: invocation.task_invocation.clone(),
                    frozen: frozen.clone(),
                    input_ref: input_ref.clone(),
                    operation: operation.clone(),
                    attempt: attempt.clone(),
                    send_binding: send_binding.clone(),
                });
                let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                let request = InferenceRequest {
                    request_id,
                    agent_id: invocation.task.agent_id,
                    conversation_id: invocation.task.conversation_id,
                    provider_binding: binding.clone(),
                    priority: InferencePriority::Normal,
                    epoch: invocation.task_invocation.epoch,
                    payload: payload.clone(),
                    result_tx,
                };
                if let Err(error) = broker.submit_recorded(request, sink) {
                    // An admission error is not authority to reinterpret an
                    // armed identity as safe to replay.
                    broker.recover_unknown(request_id);
                    return Err(error);
                }
                let result = result_rx
                    .await
                    .context("memory intervention outcome not durably settled; reconcile the original attempt")?;
                ensure!(
                    result.request_id == request_id,
                    "memory intervention result identity mismatch"
                );
                let text = result
                    .response
                    .as_ref()
                    .map(|response| response.content.clone())
                    .unwrap_or_default();
                let reply = InterventionModelReply {
                    text,
                    tokens: result
                        .response
                        .as_ref()
                        .and_then(|response| response.usage.as_ref())
                        .map(|usage| usage.output_tokens.unwrap_or(0))
                        .unwrap_or(0),
                };
                self.decide(&frozen, &observation, &reply)?
            }
            (Some(_call), None) => {
                anyhow::bail!("INTERVENTION_PLAN_MISMATCH: a planned call has no sealed payload")
            }
        };

        let reminder = self.record_decision(&frozen, &request, &observation, &decision)?;
        self.publish_reminder(&reminder, frozen.agent_id)?;
        self.store.external_operation(operation.operation_id)
    }
}

/// The bank kinds that survive a reset. A worker of a new epoch observes the
/// still-valid facts; the failure narrative stays in the frozen archive.
pub fn carried_bank_kinds() -> [BankEntryKind; 3] {
    [
        BankEntryKind::Requirement,
        BankEntryKind::EnvironmentFact,
        BankEntryKind::OpenGoal,
    ]
}
