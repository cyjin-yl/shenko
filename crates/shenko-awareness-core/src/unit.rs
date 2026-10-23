use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use chrono::{DateTime, Utc};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwarenessUnit { pub unit_id: EntityId, pub name: String, pub description: String, pub source: String, pub applicability: String, pub content: String, pub enabled: bool, pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc> }
impl AwarenessUnit {
    pub fn new(name: impl Into<String>, description: impl Into<String>, content: impl Into<String>, source: impl Into<String>, applicability: impl Into<String>) -> Self {
        let now = Utc::now();
        Self { unit_id: EntityId::new(), name: name.into(), description: description.into(), source: source.into(), applicability: applicability.into(), content: content.into(), enabled: true, created_at: now, updated_at: now }
    }
}
