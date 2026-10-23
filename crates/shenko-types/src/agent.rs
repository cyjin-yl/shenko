//! Agent identity and call context — SPEC §41.3
use serde::{Deserialize, Serialize};
use crate::ids::{EntityId, ControllerEpoch, RuntimeGeneration, BranchId};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub EntityId);

impl AgentId {
    pub fn new() -> Self { Self(EntityId::new()) }
    pub fn inner(&self) -> EntityId { self.0 }
}

impl Default for AgentId {
    fn default() -> Self { Self::new() }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent:{}", self.0)
    }
}

/// Must accompany every service invocation. Model params cannot self-declare higher privilege.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallContext {
    pub caller_instance_id: EntityId,
    pub role_revision: String,
    pub task_id: Option<EntityId>,
    pub expected_task_revision: Option<u64>,
    pub invocation_id: Option<EntityId>,
    pub controller_epoch: ControllerEpoch,
    pub software_generation: RuntimeGeneration,
    pub authority_refs: Vec<String>,
    pub trace_id: EntityId,
}

impl CallContext {
    pub fn new_system(generation: RuntimeGeneration) -> Self {
        Self {
            caller_instance_id: EntityId::new(),
            role_revision: "system".to_string(),
            task_id: None,
            expected_task_revision: None,
            invocation_id: Some(EntityId::new()),
            controller_epoch: ControllerEpoch(0),
            software_generation: generation,
            authority_refs: vec!["system".to_string()],
            trace_id: EntityId::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapability {
    pub capability_id: String,
    pub version: String,
    pub description: String,
    pub plugin_ref: Option<EntityId>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub agent_id: AgentId,
    pub branch_id: BranchId,
    pub name: Option<String>,
    pub soul: Option<String>,
    pub personality: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub software_generation: RuntimeGeneration,
    pub capabilities: Vec<AgentCapability>,
}

impl AgentProfile {
    pub fn new_subject(name: Option<String>, soul: Option<String>) -> Self {
        Self {
            agent_id: AgentId::new(),
            branch_id: BranchId::new(),
            name,
            soul,
            personality: None,
            created_at: chrono::Utc::now(),
            software_generation: RuntimeGeneration::default(),
            capabilities: vec![],
        }
    }
}
