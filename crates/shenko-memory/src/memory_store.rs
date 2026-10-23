use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry { pub entry_id: EntityId, pub agent_id: EntityId, pub kind: MemoryKind, pub content: String, pub source: Option<String>, pub applicability: Option<String>, pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc>, pub deleted: bool }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind { Awareness, Experience, Commitment, Relationship, Note, Skill }

impl MemoryEntry {
    pub fn new(agent_id: EntityId, kind: MemoryKind, content: String) -> Self {
        let now = Utc::now();
        Self { entry_id: EntityId::new(), agent_id, kind, content, source: None, applicability: None, created_at: now, updated_at: now, deleted: false }
    }
}
