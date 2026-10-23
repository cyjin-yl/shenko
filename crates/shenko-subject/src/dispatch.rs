//! Child agent dispatcher — SPEC §5.2, §5.3
use std::sync::Arc;
use shenko_types::ids::EntityId;
use shenko_types::subject::DelegationIntent;
use anyhow::Result;

pub struct ChildDispatcher {
    agent_id: EntityId,
}

impl ChildDispatcher {
    pub fn new(agent_id: EntityId) -> Self {
        Self { agent_id }
    }

    pub async fn dispatch(&self, intent: DelegationIntent) -> Result<EntityId> {
        let task_id = EntityId::new();
        tracing::info!(
            agent_id = %self.agent_id,
            task_id = %task_id,
            goal = %intent.goal,
            "child dispatched"
        );
        Ok(task_id)
    }
}
