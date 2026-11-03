//! Discoverable capability registry — SPEC §5.5, §18
//! Main and children share the same capability catalog.
use std::collections::HashMap;
use parking_lot::RwLock;
use shenko_types::ids::EntityId;
use shenko_types::agent::AgentCapability;

/// Shared capability registry.
pub struct CapabilityRegistry {
    entries: RwLock<HashMap<String, CapabilityEntry>>,
}

#[derive(Debug, Clone)]
pub struct CapabilityEntry {
    pub capability_id: String,
    pub version: String,
    pub description: String,
    pub plugin_id: Option<EntityId>,
    pub enabled: bool,
    pub registered_by: EntityId,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, entry: CapabilityEntry) {
        let mut entries = self.entries.write();
        entries.insert(entry.capability_id.clone(), entry);
    }

    pub fn get(&self, capability_id: &str) -> Option<CapabilityEntry> {
        self.entries.read().get(capability_id).cloned()
    }

    pub fn list_enabled(&self) -> Vec<CapabilityEntry> {
        self.entries.read().values().filter(|e| e.enabled).cloned().collect()
    }

    pub fn disable(&self, capability_id: &str) {
        let mut entries = self.entries.write();
        if let Some(e) = entries.get_mut(capability_id) {
            e.enabled = false;
        }
    }

    pub fn to_agent_capabilities(&self) -> Vec<AgentCapability> {
        self.entries.read().values().map(|e| AgentCapability {
            capability_id: e.capability_id.clone(),
            version: e.version.clone(),
            description: e.description.clone(),
            plugin_ref: e.plugin_id,
            enabled: e.enabled,
        }).collect()
    }
}

impl Default for CapabilityRegistry {
    fn default() -> Self { Self::new() }
}
