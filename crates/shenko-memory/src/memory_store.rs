use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::subject::MemoryIntent;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::{self, UnboundedSender, UnboundedReceiver};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub entry_id: EntityId,
    pub agent_id: EntityId,
    pub kind: MemoryKind,
    pub content: String,
    pub source: Option<String>,
    pub applicability: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind { Awareness, Experience, Commitment, Relationship, Note, Skill }

impl MemoryEntry {
    pub fn new(agent_id: EntityId, kind: MemoryKind, content: String) -> Self {
        let now = Utc::now();
        Self {
            entry_id: EntityId::new(),
            agent_id,
            kind,
            content,
            source: None,
            applicability: None,
            created_at: now,
            updated_at: now,
            deleted: false,
        }
    }
}

/// Stable reference returned to callers after an intent is enqueued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedRef {
    pub intent_id: EntityId,
    pub status: &'static str,
}

/// Acceptance channel for memory save intents (SPEC §12.2).
pub struct MemoryStore {
    memory_intent_tx: UnboundedSender<MemoryIntent>,
}

impl MemoryStore {
    /// Create a new store and return it together with the receiver end.
    /// Call `spawn_worker` on the receiver to start processing.
    pub fn new() -> (Self, UnboundedReceiver<MemoryIntent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { memory_intent_tx: tx }, rx)
    }

    /// Enqueue a memory intent; returns a stable `AcceptedRef` with status "pending".
    pub fn accept_intent(&self, intent: MemoryIntent) -> Result<AcceptedRef, anyhow::Error> {
        let intent_id = intent.intent_id.clone();
        self.memory_intent_tx
            .send(intent)
            .map_err(|e| anyhow::anyhow!("memory intent channel closed: {}", e))?;
        Ok(AcceptedRef { intent_id, status: "pending" })
    }

    /// Spawn a Tokio task that drains the intent receiver and logs each arrival.
    /// Real persistence is wired here in a later phase.
    pub fn spawn_worker(mut rx: UnboundedReceiver<MemoryIntent>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(intent) = rx.recv().await {
                info!("memory intent accepted: {}", intent.intent_id);
            }
        })
    }
}
