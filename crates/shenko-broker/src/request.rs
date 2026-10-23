use serde::{Deserialize, Serialize};
use shenko_types::ids::{EntityId, ControllerEpoch};
use tokio::sync::oneshot;

pub struct InferenceRequest {
    pub request_id: EntityId,
    pub agent_id: EntityId,
    pub conversation_id: EntityId,
    pub model: String,
    pub priority: InferencePriority,
    pub epoch: ControllerEpoch,
    pub payload: InferencePayload,
    pub result_tx: oneshot::Sender<InferenceResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferencePriority { Low = 0, Normal = 1, Subject = 2, Urgent = 3 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferencePayload {
    pub system_prompt: String,
    pub messages: Vec<InferenceMessage>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f32>,
    pub tools: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceMessage { pub role: String, pub content: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResult {
    pub request_id: EntityId,
    pub status: InferenceStatus,
    pub response: Option<InferenceResponse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceStatus { Success, Failed, Cancelled, QueuedCapacityWait }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse { pub content: String, pub stop_reason: String, pub usage: Option<TokenUsage> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage { pub input_tokens: u64, pub output_tokens: u64, pub cache_read_tokens: Option<u64>, pub cache_write_tokens: Option<u64> }
