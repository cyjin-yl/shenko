use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSession { pub session_id: EntityId, pub agent_id: EntityId, pub started_at: DateTime<Utc>, pub ended_at: Option<DateTime<Utc>>, pub metadata: serde_json::Value }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent { pub event_id: EntityId, pub session_id: EntityId, pub seq: u64, pub event_type: SessionEventType, pub role: String, pub content: serde_json::Value, pub created_at: DateTime<Utc> }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventType { Message, ToolCall, ToolResult, Decision, DelegationIntent, SystemEvent }

impl RawSession { pub fn new(agent_id: EntityId) -> Self { Self { session_id: EntityId::new(), agent_id, started_at: Utc::now(), ended_at: None, metadata: serde_json::Value::Object(Default::default()) } } }
