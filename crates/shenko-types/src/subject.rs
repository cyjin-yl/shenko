//! Subject step types — SPEC §5.1, §41.3
//! S0: fix input plan + context version.
//! S1: commit decision, event disposition, delegation intent, unique successor.
use crate::commit::CommitRef;
use crate::ids::{
    BranchId, ControllerEpoch, EntityId, RecordRevision, RuntimeGeneration, TaskRevision,
};
use crate::object::ObjectRef;
use crate::task::WaitJoinPolicy;
use serde::{Deserialize, Serialize};

/// S0: input plan sealed for one subject step.
/// Fixes the attention context version before the model is called.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectStepInput {
    pub step_id: EntityId,
    pub step_seq: u64,
    pub agent_id: EntityId,
    pub branch_id: crate::ids::BranchId,
    pub controller_epoch: crate::ids::ControllerEpoch,
    pub attention_context_ref: ObjectRef,
    pub inbox_cursor: InboxCursor,
    pub sealed_at: chrono::DateTime<chrono::Utc>,
    pub sealed_commit: CommitRef,
}

/// S1: subject step decision committed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectStepDecision {
    pub step_id: EntityId,
    pub decided_at: chrono::DateTime<chrono::Utc>,
    pub decision_ref: ObjectRef,
    pub event_dispositions: Vec<EventDisposition>,
    pub delegation_intents: Vec<DelegationIntent>,
    pub memory_intents: Vec<MemoryIntent>,
    pub successor_request_id: EntityId,
    pub semantic_wait: Option<SemanticWait>,
    pub decided_commit: CommitRef,
}

/// Inbox cursor — tracks consumed events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxCursor {
    pub through_event_id: Option<EntityId>,
    pub consumed_count: u64,
}

/// How a subject step handled an incoming event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDisposition {
    pub event_id: EntityId,
    pub disposition: Disposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Acknowledged,
    Deferred,
    Delegated,
    Ignored,
}

/// Intent to delegate work to a child agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationIntent {
    pub intent_id: EntityId,
    pub goal: String,
    pub system_prompt: String,
    pub model: Option<String>,
    pub capabilities: Vec<String>,
    pub budget_hint: Option<BudgetHint>,
    pub reply_grant: Option<ReplyGrant>,
}

/// Budget hint for a child agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetHint {
    pub max_tokens: Option<u64>,
    pub max_duration_secs: Option<u64>,
}

/// Reply grant — allows a child to publish directly.
/// Bound to task/generation, target provider/account/conversation/thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyGrant {
    pub grant_id: EntityId,
    pub task_id: EntityId,
    pub target_provider: String,
    pub target_account: String,
    pub target_conversation: String,
    pub reply_to_event_id: EntityId,
    pub allowed_message_types: Vec<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Intent to save/recall/forget memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryIntent {
    pub intent_id: EntityId,
    pub kind: MemoryIntentKind,
    pub scope: String,
    pub content_ref: Option<ObjectRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryIntentKind {
    Save,
    Correct,
    Forget,
    Recall,
}

/// Semantic wait — subject voluntarily waits on a condition.
/// Max 600 seconds per SPEC §5.1/U55.
/// Still polls with same model + prior+new context during wait.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticWait {
    pub condition_description: String,
    pub max_wait_secs: u64, // ≤ 600
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl SemanticWait {
    pub const MAX_WAIT_SECS: u64 = 600;

    pub fn new(condition: impl Into<String>, max_wait_secs: u64) -> Self {
        assert!(
            max_wait_secs <= Self::MAX_WAIT_SECS,
            "semantic wait max 600s"
        );
        Self {
            condition_description: condition.into(),
            max_wait_secs,
            started_at: chrono::Utc::now(),
        }
    }

    pub fn is_expired(&self) -> bool {
        let elapsed = chrono::Utc::now()
            .signed_duration_since(self.started_at)
            .num_seconds() as u64;
        elapsed >= self.max_wait_secs
    }
}

/// Subject branch head — tracks the continuous subject loop state. SPEC §41.5
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectBranchHead {
    pub agent_id: EntityId,
    pub branch_id: BranchId,
    pub subject_root_task_id: EntityId,
    pub controller_epoch: ControllerEpoch,
    pub software_generation: RuntimeGeneration,
    pub genesis_ref: GenesisRef,
    pub committed_step_no: u64,
    pub committed_step_id: Option<EntityId>,
    pub committed_at: CommitRef,
    pub next_step_id: EntityId,
    pub mode: SubjectMode,
    pub mode_reason_ref: Option<ObjectRef>,
    pub record_revision: RecordRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectMode {
    Active,
    Quiescing,
    Handoff,
    Paused,
    RecoveryBlocked,
}

/// Persistent genesis record — the immutable birth of a subject instance. SPEC §44.6
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectGenesis {
    pub genesis_id: EntityId,
    pub subject_id: EntityId,
    pub bootstrap_id: EntityId,
    pub launch_id: EntityId,
    pub seed_manifest_ref: ObjectRef,
    pub initial_profile_ref: ObjectRef,
    pub home_manifest_ref: ObjectRef,
    pub bootstrap_event_ref: ObjectRef,
    pub created_commit: CommitRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisRef {
    pub ledger_id: EntityId,
    pub genesis_id: EntityId,
}

/// SubjectStep state machine per SPEC §41.5
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectStep {
    pub step_id: EntityId,
    pub branch_id: BranchId,
    pub step_no: u64,
    pub expected_parent_commit: CommitRef,
    pub controller_epoch: ControllerEpoch,
    pub software_generation: RuntimeGeneration,
    pub state: SubjectStepState,
    pub input_plan_ref: Option<ObjectRef>,
    pub context_revision: Option<u64>,
    pub active_request_id: Option<EntityId>,
    pub active_attempt_id: Option<EntityId>,
    pub model_result_ref: Option<ObjectRef>,
    pub decision_ref: Option<ObjectRef>,
    pub commit_ref: Option<CommitRef>,
    pub retry_reason_ref: Option<ObjectRef>,
    pub record_revision: RecordRevision,
    pub created_by_step_id: Option<EntityId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectStepState {
    Planned,
    Preparing,
    AwaitingModel,
    ResultRecorded,
    Committed,
    RetryPending,
    Retired,
}

/// Positive evidence that the original request cannot still produce a result.
/// A transport error, cancelled local future or generic provider failure is NOT evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectRetryEvidence {
    pub request_id: EntityId,
    pub fact: InferenceRetryFact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceRetryFact {
    NeverDispatched,
    ProviderRejected { http_status: u16 },
}

impl SubjectRetryEvidence {
    pub fn permits_retry(&self, request_id: Option<EntityId>) -> bool {
        request_id == Some(self.request_id)
            && matches!(
                self.fact,
                InferenceRetryFact::NeverDispatched
                    | InferenceRetryFact::ProviderRejected { http_status: 429 }
            )
    }
}

/// TaskWait — child wait record. SPEC §41.5
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskWait {
    pub waiting_task_id: EntityId,
    pub expected_task_revision: TaskRevision,
    pub on_task_ids: Vec<EntityId>,
    pub policy: WaitJoinPolicy,
    pub resume_checkpoint_ref: Option<ObjectRef>,
    pub created_commit: CommitRef,
}

/// StepDecision — the committed decision for one subject step. SPEC §41.5
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDecision {
    pub schema_version: u32,
    pub step_id: EntityId,
    pub output: crate::output::ParsedOutput,
    pub identity_intent: Option<crate::identity::IdentityIntent>,
    pub input_plan_ref: ObjectRef,
    pub model_result_ref: Option<ObjectRef>,
    pub applied_steering_refs: Vec<ObjectRef>,
    pub event_dispositions: Vec<EventDisposition>,
    pub delegated_task_intents: Vec<DelegationIntent>,
    pub child_dialogue_intents: Vec<DialogueIntent>,
    pub memory_intents: Vec<MemoryIntent>,
    pub policy_intents: Vec<PolicyIntent>,
    pub publication_intents: Vec<PublicationIntent>,
    pub pending_obligations: Vec<PendingObligation>,
    pub evidence_refs: Vec<ObjectRef>,
    pub successor_step_id: EntityId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogueIntent {
    pub intent_id: EntityId,
    pub target_task_id: EntityId,
    pub message: String,
    pub in_reply_to: Option<EntityId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyIntent {
    pub intent_id: EntityId,
    pub kind: PolicyKind,
    pub description: String,
    pub target_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    ResourceShare,
    UpdatePolicy,
    NetworkPolicy,
    RetentionPolicy,
    InterruptPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicationIntent {
    pub intent_id: EntityId,
    pub target: crate::output::PublicationTarget,
    pub content: String,
    pub reply_grant_id: Option<EntityId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingObligation {
    pub obligation_id: EntityId,
    pub description: String,
    pub depends_on_task_id: Option<EntityId>,
    pub resume_condition: String,
}
