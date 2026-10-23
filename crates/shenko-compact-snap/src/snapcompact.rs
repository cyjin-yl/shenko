use shenko_types::ids::EntityId;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

/// Manifest describing a compact plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactManifest {
    pub plugin_id: &'static str,
    pub version: &'static str,
}

/// A stored snapshot record: the last assistant message plus metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapRecord {
    pub snap_id: EntityId,
    pub agent_id: EntityId,
    pub created_at: DateTime<Utc>,
    pub summary: String,
}

/// Legacy record kept for existing callers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapCompactRecord {
    pub snap_id: EntityId,
    pub agent_id: EntityId,
    pub created_at: DateTime<Utc>,
    pub snapshot_ref: serde_json::Value,
    pub delta_since_snap: Vec<serde_json::Value>,
}

impl SnapCompactRecord {
    pub fn new(agent_id: EntityId) -> Self {
        Self {
            snap_id: EntityId::new(),
            agent_id,
            created_at: Utc::now(),
            snapshot_ref: serde_json::Value::Null,
            delta_since_snap: vec![],
        }
    }
}

/// A generic message type used by the snapshot plugin when a concrete
/// message type is not provided by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapMessage {
    pub role: String,
    pub content: String,
}

/// Snapshot-driven compaction plugin.
///
/// Triggers every `snapshot_after_n_turns` turns and stores the last
/// assistant message as a named snapshot.  The snapshot strategy differs
/// from budget compaction: instead of dropping old messages it records a
/// reference point that can later be used to summarise the session.
#[derive(Debug, Clone)]
pub struct SnapCompactPlugin {
    pub snapshot_after_n_turns: usize,
}

impl SnapCompactPlugin {
    pub fn new(snapshot_after_n_turns: usize) -> Self {
        Self { snapshot_after_n_turns }
    }

    /// Returns true when `turn_count` is a non-zero multiple of the
    /// configured interval.
    pub fn should_snapshot(&self, turn_count: usize) -> bool {
        turn_count > 0 && turn_count % self.snapshot_after_n_turns == 0
    }

    /// Store the last assistant message from `messages` as a snapshot.
    /// If no assistant message is found, the snapshot summary is empty.
    pub fn take_snapshot(&self, agent_id: EntityId, messages: &[SnapMessage]) -> SnapRecord {
        let summary = messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        SnapRecord {
            snap_id: EntityId::new(),
            agent_id,
            created_at: Utc::now(),
            summary,
        }
    }

    /// Plugin identity manifest.
    pub fn manifest(&self) -> CompactManifest {
        CompactManifest {
            plugin_id: "snap-compact",
            version: "0.1.0",
        }
    }
}
