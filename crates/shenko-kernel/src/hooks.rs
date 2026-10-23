//! Hook API -- SPEC §37
//! Deterministic injection order, version/field compatibility, inter-generation switching.
use std::sync::Arc;
use parking_lot::RwLock;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use anyhow::Result;
use shenko_types::ids::EntityId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    BeforeInference = 10,
    AfterInference = 20,
    BeforeOperation = 30,
    AfterOperation = 40,
    CapabilityChanged = 50,
    GenerationHandoff = 60,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum HookPayload {
    BeforeInference { request_id: EntityId, agent_id: EntityId, model: String },
    AfterInference { request_id: EntityId, stop_reason: String, output_tokens: u64 },
    BeforeOperation { operation_id: EntityId, operation_name: String },
    AfterOperation { operation_id: EntityId, outcome: String, effect_status: String },
    CapabilityChanged { capability_id: String, enabled: bool },
    GenerationHandoff { old_generation: String, new_generation: String },
}

#[derive(Clone)]
pub struct HookEntry {
    pub hook_id: EntityId,
    pub plugin_id: EntityId,
    pub phase: HookPhase,
    pub priority: i32,
    pub name: String,
    pub handler: Arc<dyn HookHandler>,
}

#[async_trait]
pub trait HookHandler: Send + Sync + 'static {
    async fn handle(&self, payload: &HookPayload) -> Result<HookResult>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    pub hook_id: EntityId,
    pub allowed: bool,
    pub modified: bool,
    pub reason: Option<String>,
}

impl HookResult {
    pub fn allow() -> Self { Self { hook_id: EntityId::new(), allowed: true, modified: false, reason: None } }
    pub fn block(reason: impl Into<String>) -> Self { Self { hook_id: EntityId::new(), allowed: false, modified: false, reason: Some(reason.into()) } }
}

pub struct HookRegistry {
    hooks: RwLock<Vec<HookEntry>>,
}

impl HookRegistry {
    pub fn new() -> Self { Self { hooks: RwLock::new(vec![]) } }

    pub fn register(&self, entry: HookEntry) {
        let mut hooks = self.hooks.write();
        hooks.push(entry);
        hooks.sort_by_key(|h| (h.phase as i32, h.priority));
    }

    pub fn unregister(&self, hook_id: EntityId) {
        self.hooks.write().retain(|h| h.hook_id != hook_id);
    }

    pub fn list(&self) -> Vec<(String, HookPhase)> {
        self.hooks.read().iter().map(|h| (h.name.clone(), h.phase)).collect()
    }

    pub async fn run_phase(&self, payload: &HookPayload) -> Vec<HookResult> {
        let phase = match payload {
            HookPayload::BeforeInference { .. } => HookPhase::BeforeInference,
            HookPayload::AfterInference { .. } => HookPhase::AfterInference,
            HookPayload::BeforeOperation { .. } => HookPhase::BeforeOperation,
            HookPayload::AfterOperation { .. } => HookPhase::AfterOperation,
            HookPayload::CapabilityChanged { .. } => HookPhase::CapabilityChanged,
            HookPayload::GenerationHandoff { .. } => HookPhase::GenerationHandoff,
        };
        let handlers: Vec<Arc<dyn HookHandler>> = {
            let hooks = self.hooks.read();
            hooks.iter().filter(|h| h.phase == phase).map(|h| Arc::clone(&h.handler)).collect()
        };
        let mut results = vec![];
        for handler in handlers {
            match handler.handle(payload).await {
                Ok(r) => results.push(r),
                Err(e) => { tracing::warn!(err=%e, "hook handler error"); results.push(HookResult::allow()); }
            }
        }
        results
    }

    pub async fn check_allowed(&self, payload: &HookPayload) -> bool {
        self.run_phase(payload).await.iter().all(|r| r.allowed)
    }
}

impl Default for HookRegistry { fn default() -> Self { Self::new() } }
