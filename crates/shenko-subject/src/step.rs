//! Subject step execution — S0 seal + S1 commit. SPEC §41.3.
use shenko_types::ids::EntityId;
use shenko_types::subject::InboxCursor;
use shenko_types::commit::CommitRef;
use shenko_types::object::ObjectRef;
use serde::{Deserialize, Serialize};

/// Minimal attention context for one step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionContext {
    pub step_seq: u64,
    pub recent_events: Vec<serde_json::Value>,
    pub active_goals: Vec<String>,
    pub memory_snippets: Vec<String>,
}

/// Build S0 input plan for a subject step.
pub fn seal_step_input(
    agent_id: EntityId,
    step_seq: u64,
    context: &AttentionContext,
    inbox_cursor: InboxCursor,
    commit_ref: CommitRef,
    context_object_ref: ObjectRef,
) -> shenko_types::subject::SubjectStepInput {
    let _ = context; // full plan sealing reads attention context version
    shenko_types::subject::SubjectStepInput {
        step_id: EntityId::new(),
        step_seq,
        agent_id,
        branch_id: shenko_types::ids::BranchId::new(),
        controller_epoch: shenko_types::ids::ControllerEpoch(1),
        attention_context_ref: context_object_ref,
        inbox_cursor,
        sealed_at: chrono::Utc::now(),
        sealed_commit: commit_ref,
    }
}
