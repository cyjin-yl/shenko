use shenko_types::ids::EntityId;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapCompactRecord { pub snap_id: EntityId, pub agent_id: EntityId, pub created_at: DateTime<Utc>, pub snapshot_ref: serde_json::Value, pub delta_since_snap: Vec<serde_json::Value> }
impl SnapCompactRecord { pub fn new(agent_id: EntityId) -> Self { Self { snap_id: EntityId::new(), agent_id, created_at: Utc::now(), snapshot_ref: serde_json::Value::Null, delta_since_snap: vec![] } } }
