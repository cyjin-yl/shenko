//! Child agent dispatcher — SPEC §5.2, §5.3
//!
//! For now the dispatcher only forwards each DelegationIntent to an mpsc
//! channel so an external seat adapter can pick it up. No real model call
//! happens here yet — children will get their own inference path later.
use shenko_types::ids::EntityId;
use shenko_types::subject::DelegationIntent;
use anyhow::Result;
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver, unbounded_channel};

pub struct ChildDispatcher {
    agent_id: EntityId,
    tx: UnboundedSender<(EntityId, DelegationIntent)>,
}

impl ChildDispatcher {
    /// Create a dispatcher and the receiver that external code drains.
    /// SPEC §5.2: the main subject delegates; a seat adapter consumes.
    pub fn new(agent_id: EntityId) -> (Self, UnboundedReceiver<(EntityId, DelegationIntent)>) {
        let (tx, rx) = unbounded_channel();
        (Self { agent_id, tx }, rx)
    }

    /// Dispatch a child task. Spawns a tokio task that logs the delegation
    /// and forwards the intent to the channel. Returns the assigned task id.
    pub async fn dispatch(&self, intent: DelegationIntent) -> Result<EntityId> {
        let task_id = EntityId::new();
        let tx = self.tx.clone();
        let agent_id = self.agent_id;
        let goal = intent.goal.clone();
        tokio::spawn(async move {
            tracing::info!(
                agent_id = %agent_id,
                task_id = %task_id,
                goal = %goal,
                "child dispatched"
            );
            let _ = tx.send((task_id, intent));
        });
        Ok(task_id)
    }
}
