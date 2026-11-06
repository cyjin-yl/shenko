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
use cordis_core::{Context, InjectSpec, Plugin, Service};
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
use shenko_types::task::{ImplementationRef, TaskRecord};
use std::sync::Arc;

use crate::quota_gate::{QuotaGate, observe_quota_response, quota_dispatch_gate, settle_quota_terminal};
use crate::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_memory::intervention_runtime::PmaContributionRuntime;

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
    /// The bounded observation this call decided on. It is frozen so that a
    /// recovered call re-derives exactly the same decision from exactly the
    /// same facts. Sealed inputs of an older revision simply lack it.
    #[serde(default)]
    observation: Option<InterventionObservation>,
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

/// Rebuild the native definition when either memory service is replaced.
/// The handler must not retain a withdrawn contribution registration.
pub struct PmaEvaluatePlugin;

impl Plugin for PmaEvaluatePlugin {
    type Config = (Arc<Store>, BrokerHandle);
    type Input = Self::Config;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("shenko-pma-evaluate")
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none()
            .require(crate::NativeChildService::NAME)
            .require(MemoryInterventionRegistry::NAME)
            .require(PmaContributionRuntime::NAME)
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, (store, broker): &Self::Input) -> Result<(), Self::ApplyError> {
        let policy = ctx.try_service::<MemoryInterventionRegistry>().map_err(std::io::Error::other)?;
        let publisher = ctx.try_service::<PmaContributionRuntime>().map_err(std::io::Error::other)?;
        let handler: Arc<dyn NativeTaskHandler> = Arc::new(
            PmaEvaluateHandler::new(store.clone(), broker.clone(), policy).with_publisher(publisher),
        );
        crate::NativeTaskHandlerPlugin.apply(ctx, &handler).await
    }
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
        let decision = policy.interpret(observation, reply)?;
        if let InterventionDecision::Remind { scope, .. } = &decision {
            shenko_store::validate_intervention_scope(frozen.epoch_id, scope)?;
        }
        Ok(decision)
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
                Some(
                    now + i64::try_from(
                        expires_in_secs.unwrap_or(DEFAULT_REMINDER_TTL_SECS) * 1000,
                    )
                    .map_err(|_| anyhow::anyhow!("reminder expiry overflows the durable clock"))?,
                ),
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
                .take(observation.budget.max_bank_entries)
                .filter(|entry| {
                    decision_provenance(decision).iter().any(|reference| {
                        reference.parse::<EntityId>().ok() == Some(entry.entry_id)
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
        Ok(reminder)
    }

    /// Commit one decision of a bounded attempt and the settlement that
    /// accepts it. The decision row becomes visible in the same transaction
    /// that resolves the operation, so a successful intervention and its
    /// durable decision are one fact.
    fn commit_decision(
        &self,
        frozen: &FrozenIntervention,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        request: &InterventionRequest,
        observation: &InterventionObservation,
        decision: &InterventionDecision,
        raw: ObjectRef,
        recovered: bool,
        source_instance_id: String,
        observation_level: Option<ObservationLevel>,
    ) -> Result<InterventionReminder> {
        let reminder = self.record_decision(frozen, request, observation, decision)?;
        // The complete decision remains evidence, not rolling tool context.
        // Only the prepared contribution may deliver a transient reminder's
        // body; the task result carries its identity and evidence reference.
        let decision_ref = seal(&self.store, &reminder, "pma-recorded-decision")?;
        let body = serde_json::json!({
            "schema_version": 2,
            "task_id": frozen.task_id,
            "request_id": frozen.request_id,
            "policy": frozen.policy_name,
            "policy_revision": frozen.policy_revision,
            "decided_without_call": frozen.decided_without_call,
            "decision": reminder.decision,
            "reminder_id": reminder.reminder_id,
            "epoch_id": reminder.epoch_id,
            "lineage_id": reminder.lineage_id,
            "scope": reminder.scope,
            "decision_ref": decision_ref,
            "recovered": recovered,
        });
        let normalized = seal(&self.store, &body, "pma-decision")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: PMA_OPERATION.into(),
            source_instance_id,
            source_event_key: Some(format!("pma-decision:{}", frozen.request_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_intervention_settlement(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            OperationOutcome::Succeeded,
            EffectStatus::Complete,
            Some(&reminder),
        )?;
        Ok(reminder)
    }

    /// Commit the failure of one bounded attempt whose answer carries no
    /// usable decision. The answer stays the original fact, and no silence
    /// is fabricated for it.
    fn commit_refusal(
        &self,
        frozen: &FrozenIntervention,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        raw: ObjectRef,
        body: serde_json::Value,
        source_instance_id: String,
        observation_level: Option<ObservationLevel>,
    ) -> Result<()> {
        let normalized = seal(&self.store, &body, "pma-refusal")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: PMA_OPERATION.into(),
            source_instance_id,
            source_event_key: Some(format!("pma-decision:{}", frozen.request_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_intervention_settlement(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            OperationOutcome::Failed,
            EffectStatus::None,
            None,
        )?;
        Ok(())
    }

    /// Refresh the addressed scope from all of its committed reminders.
    fn publish_reminder(
        &self,
        reminder: &InterventionReminder,
    ) -> Result<()> {
        let Some(publisher) = self.publisher.as_ref() else {
            return Ok(());
        };
        if reminder.decision != InterventionDecisionKind::Remind {
            return Ok(());
        }
        if let Some(scope) = self.store.intervention_delivery_scope(reminder)? {
            publisher.refresh(&scope)?;
        }
        Ok(())
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
        Ok(match self.store.commit_native_dispatch(
            &self.invocation, self.operation.operation_id, self.attempt.attempt_id,
        )? {
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
        match result.termination.kind {
            // A bounded answer is not yet a decision: the answer becomes
            // durable, and the operation stays unresolved until the policy
            // commits its decision together with the settlement.
            TerminationKind::ProviderTerminal => {
                self.store.record_intervention_provider_evidence(
                    self.operation.operation_id,
                    self.attempt.attempt_id,
                    evidence,
                )?;
            }
            // Nothing was answered, so there is no decision to commit.
            TerminationKind::NeverDispatched => {
                let outcome = if result.status == InferenceStatus::Cancelled {
                    OperationOutcome::Cancelled
                } else {
                    OperationOutcome::Failed
                };
                self.store.record_intervention_settlement(
                    self.operation.operation_id,
                    self.attempt.attempt_id,
                    evidence,
                    outcome,
                    EffectStatus::None,
                    None,
                )?;
            }
            TerminationKind::TransportRejected => {
                self.store.record_intervention_settlement(
                    self.operation.operation_id,
                    self.attempt.attempt_id,
                    evidence,
                    OperationOutcome::Failed,
                    EffectStatus::None,
                    None,
                )?;
            }
            // The send stayed uncertain: the attempt keeps no outcome and is
            // never replayed.
            TerminationKind::Unknown => {
                self.store.record_external_evidence(
                    self.operation.operation_id,
                    self.attempt.attempt_id,
                    evidence,
                    None,
                    EffectStatus::Unknown,
                )?;
            }
        }
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
            attempt: attempt.clone(),
            frozen: frozen.clone(),
        }))?;
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        // The answer is durable but its decision is not: re-derive the same
        // decision from the frozen observation and the original answer. The
        // frozen observation is what makes the re-derived decision the same
        // decision.
        let observation = frozen.observation.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "the original observation was not frozen; the decision cannot be re-derived"
            )
        })?;
        let request: InterventionRequest = serde_json::from_value(frozen.delegation.clone())
            .context("the frozen delegation is not a valid intervention request")?;
        if frozen.decided_without_call {
            // The policy decided without an inference, so the decision is
            // re-derived from the frozen observation alone.
            let policy = self.policy.current();
            ensure!(
                policy.name() == frozen.policy_name && policy.revision() == frozen.policy_revision,
                "the installed intervention policy changed after the request was sealed"
            );
            let plan = policy.plan(observation)?;
            let decision = plan.decision.ok_or_else(|| {
                anyhow::anyhow!("the frozen policy no longer decides without a call")
            })?;
            let raw = seal(&self.store, &decision, "pma-local-decision")?;
            let reminder = self.commit_decision(
                &frozen,
                &operation,
                &attempt,
                &request,
                observation,
                &decision,
                raw,
                true,
                attempt.node_id.to_string(),
                None,
            )?;
            self.publish_reminder(&reminder)?;
            return self.store.external_operation(operation_id);
        }
        let Some(result) = read_original_terminal(&self.store, &operation, &attempt, &frozen)? else {
            // The send stayed uncertain: the attempt keeps no outcome and is
            // never replayed.
            return Ok(operation);
        };
        let text = result
            .response
            .as_ref()
            .map(|response| response.content.clone())
            .unwrap_or_default();
        let reply = InterventionModelReply {
            text: text.clone(),
            tokens: result
                .response
                .as_ref()
                .and_then(|response| response.usage.as_ref())
                .map(|usage| usage.output_tokens.unwrap_or(0))
                .unwrap_or(0),
        };
        match self.decide(&frozen, observation, &reply) {
            Ok(decision) => {
                let raw = seal(&self.store, &result, "pma-provider-result")?;
                let reminder = self.commit_decision(
                    &frozen,
                    &operation,
                    &attempt,
                    &request,
                    observation,
                    &decision,
                    raw,
                    true,
                    attempt.node_id.to_string(),
                    Some(ObservationLevel::RemoteCompleted),
                )?;
                self.publish_reminder(&reminder)?;
            }
            Err(error) => {
                // The original answer carries no usable decision: the bounded
                // call failed, and no silence is fabricated for it.
                let raw = seal(&self.store, &result, "pma-provider-result")?;
                let body = serde_json::json!({
                    "schema_version": 1,
                    "task_id": frozen.task_id,
                    "request_id": frozen.request_id,
                    "decision_error": error.to_string(),
                    "recovered": true,
                });
                self.commit_refusal(
                    &frozen,
                    &operation,
                    &attempt,
                    raw,
                    body,
                    attempt.node_id.to_string(),
                    Some(ObservationLevel::RemoteCompleted),
                )?;
            }
        }
        self.store.external_operation(operation_id)
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
            observation: Some(observation.clone()),
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

        let reminder = match (plan.call, payload) {
            (None, _) => {
                // The policy decided without an inference. The decision itself
                // is the durable effect of this attempt.
                let decision = plan
                    .decision
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("INTERVENTION_PLAN_EMPTY"))?;
                let raw = seal(&self.store, &decision, "pma-local-decision")?;
                let reminder = self.commit_decision(
                    &frozen,
                    &operation,
                    &attempt,
                    &request,
                    &observation,
                    &decision,
                    raw,
                    false,
                    invocation.task_invocation.node_id.to_string(),
                    // No provider was contacted: there is no remote
                    // observation to report for a locally decided attempt.
                    None,
                )?;
                Some(reminder)
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
                let inference_request = InferenceRequest {
                    request_id,
                    agent_id: invocation.task.agent_id,
                    conversation_id: invocation.task.conversation_id,
                    provider_binding: binding.clone(),
                    priority: InferencePriority::Normal,
                    epoch: invocation.task_invocation.epoch,
                    payload: payload.clone(),
                    result_tx,
                };
                if let Err(error) = broker.submit_recorded(inference_request, sink) {
                    // An admission error is not authority to reinterpret an
                    // armed identity as safe to replay. A proven pre-enqueue
                    // refusal created no work, no send and no charge, so it
                    // must not become unknown capacity debt either.
                    if !BrokerHandle::never_enqueued(&error) {
                        broker.recover_unknown(request_id);
                    }
                    return Err(error);
                }
                let result = result_rx
                    .await
                    .context("memory intervention outcome not durably settled; reconcile the original attempt")?;
                ensure!(
                    result.request_id == request_id,
                    "memory intervention result identity mismatch"
                );
                // The sink recorded the answer. The decision commits together
                // with the settlement, so an attempt that ended without an
                // answer to interpret settles as it is.
                let settled = self.store.external_operation(operation.operation_id)?;
                if result.termination.kind == TerminationKind::Unknown {
                    // The send stayed uncertain: the attempt keeps no outcome
                    // and is never replayed.
                    return Ok(settled);
                }
                if settled.state == OperationState::Resolved {
                    None
                } else {
                    let text = result
                        .response
                        .as_ref()
                        .map(|response| response.content.clone())
                        .unwrap_or_default();
                    let reply = InterventionModelReply {
                        text: text.clone(),
                        tokens: result
                            .response
                            .as_ref()
                            .and_then(|response| response.usage.as_ref())
                            .map(|usage| usage.output_tokens.unwrap_or(0))
                            .unwrap_or(0),
                    };
                    match self.decide(&frozen, &observation, &reply) {
                        Ok(decision) => {
                            let raw = seal(&self.store, &result, "pma-provider-result")?;
                            let reminder = self.commit_decision(
                                &frozen,
                                &operation,
                                &attempt,
                                &request,
                                &observation,
                                &decision,
                                raw,
                                false,
                                invocation.task_invocation.node_id.to_string(),
                                Some(ObservationLevel::RemoteCompleted),
                            )?;
                            Some(reminder)
                        }
                        Err(error) => {
                            // An answer that carries no usable decision is a
                            // failed bounded call, never a fabricated silence.
                            let raw = seal(&self.store, &result, "pma-provider-result")?;
                            let body = serde_json::json!({
                                    "schema_version": 1,
                                    "task_id": frozen.task_id,
                                    "request_id": frozen.request_id,
                                    "decision_error": error.to_string(),
                                });
                            self.commit_refusal(
                                &frozen,
                                &operation,
                                &attempt,
                                raw,
                                body,
                                invocation.task_invocation.node_id.to_string(),
                                Some(ObservationLevel::RemoteCompleted),
                            )?;
                            None
                        }
                    }
                }
            }
            (Some(_call), None) => {
                anyhow::bail!("INTERVENTION_PLAN_MISMATCH: a planned call has no sealed payload")
            }
        };

        if let Some(reminder) = reminder.as_ref() {
            self.publish_reminder(reminder)?;
        }
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
