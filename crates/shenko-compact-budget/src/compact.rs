use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetCompactConfig { pub trigger_at_fraction: f32, pub keep_recent_events: usize }
impl Default for BudgetCompactConfig { fn default() -> Self { Self { trigger_at_fraction: 0.8, keep_recent_events: 50 } } }
pub struct BudgetCompactor { config: BudgetCompactConfig }
impl BudgetCompactor {
    pub fn new(config: BudgetCompactConfig) -> Self { Self { config } }
    pub fn should_compact(&self, used: u64, max: u64) -> bool { used as f32 / max as f32 >= self.config.trigger_at_fraction }
}
