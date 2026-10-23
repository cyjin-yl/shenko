use std::collections::HashMap;
use parking_lot::RwLock;
use shenko_types::ids::EntityId;
use crate::unit::AwarenessUnit;
pub struct AwarenessRegistry { units: RwLock<HashMap<EntityId, AwarenessUnit>> }
impl AwarenessRegistry {
    pub fn new() -> Self { Self { units: RwLock::new(HashMap::new()) } }
    pub fn install(&self, unit: AwarenessUnit) { self.units.write().insert(unit.unit_id, unit); }
    pub fn enabled_units(&self) -> Vec<AwarenessUnit> { self.units.read().values().filter(|u| u.enabled).cloned().collect() }
    pub fn disable(&self, unit_id: EntityId) { if let Some(u) = self.units.write().get_mut(&unit_id) { u.enabled = false; } }
}
impl Default for AwarenessRegistry { fn default() -> Self { Self::new() } }
