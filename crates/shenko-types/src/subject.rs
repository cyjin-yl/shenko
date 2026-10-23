//! Subject step types — SPEC §5.1, §41.3
//! S0: fix input plan + context version.
//! S1: commit decision, event disposition, delegation intent, unique successor.
use serde::{Deserialize, Serialize};
use crate::ids::{EntityId, BranchId, ControllerEpoch};
use crate::commit::CommitRef;
use crate::object::ObjectRef;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubjectStepInput {
    pub step_id: EntityId,
    pub step_seq: u64,
    pub agent_id: EntityId,
    pub branch_id: BranchId,
    pub controller_epoch: ControllerEpoch,
    pub attention_context_ref: ObjectRef,
    pub inbox_cursor: InboxCursor,
    pub sealed_at: chrono::DateTime<chrono::Utc>,
    pub sealed_commit: CommitRef,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxCursor {
    pub through_event_id: Option<EntityId>,
    pub consumed_count: u64,
}

impl Default for InboxCursor {
    fn default() -> Self { Self { through_event_id: None, consumed_count: 0 } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDisposition {
    pub event_id: EntityId,
    pub disposition: Disposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition { Acknowledged, Deferred, Delegated, Ignored }

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetHint {
    pub max_tokens: Option<u64>,
    pub max_duration_secs: Option<u64>,
}

/// Bound to task/generation + target provider/account/conversation/thread.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryIntent {
    pub intent_id: EntityId,
    pub kind: MemoryIntentKind,
    pub scope: String,
    pub content_ref: Option<ObjectRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryIntentKind { Save, Correct, Forget, Recall }

/// Subject voluntarily waits on a semantic condition. Hard max 600s. SPEC §5.1/U55.
/// Still polls with same model + prior+new context during wait.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticWait {
    pub condition_description: String,
    pub max_wait_secs: u64,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

impl SemanticWait {
    pub const MAX_WAIT_SECS: u64 = 600;

    pub fn new(condition: impl Into<String>, max_wait_secs: u64) -> Self {
        let capped = max_wait_secs.min(Self::MAX_WAIT_SECS);
        Self {
            condition_description: condition.into(),
            max_wait_secs: capped,
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
