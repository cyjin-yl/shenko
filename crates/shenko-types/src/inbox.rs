//! Inbox event protocol data — docs/specs/subject-runtime.md#spec-5-1
//!
//! What arrives in the subject's inbox is plain data. It is defined here so
//! that a plugin that produces or inspects inbox events does not have to
//! depend on the subject crate (docs/specs/kernel.md#kernel-7). The queue
//! that carries these events stays with the subject.
use crate::ids::EntityId;
use serde::{Deserialize, Serialize};

/// The inbox event kind the subject's own due-obligation wakes arrive under.
///
/// The value is the serialized [`InboxEventKind::SelfWake`] name: the durable
/// admission contract binds the kind argument to the complete original, so
/// the two spellings can never drift apart.
pub const SELF_WAKE_KIND: &str = "self_wake";

/// The inbox source the subject's own wakes arrive from (docs/specs/subject-runtime.md#spec-13-8; the
/// subject produces this fact about itself, no external channel does).
pub const SUBJECT_WAKE_SOURCE: &str = "subject";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEvent {
    pub event_id: EntityId,
    pub kind: InboxEventKind,
    pub payload: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxEventKind {
    Message,
    TaskResult,
    MemoryResult,
    SemanticWaitCheck,
    SystemEvent,
    /// A Playground artifact changed (PUT or backend action). docs/specs/configuration-playground.md#spec-43-2.
    ArtifactChanged,
    /// A durable transcript revision of one continuous-media stream segment
    /// (partial, final, superseded or final-correction). docs/specs/contacts.md#spec-15-2, docs/specs/media.md#spec-20.
    MediaRevision,
    /// A media stream opened or reached a terminal state (cancelled,
    /// disconnected, ended). docs/specs/media.md#spec-20.
    MediaStream,
    /// An admitted recording control (start/stop) with the recording's
    /// projection state. docs/specs/media.md#spec-20.
    MediaRecording,
    /// A media session priority input (barge-in, interjection, stop
    /// playback, urgent message). docs/specs/media.md#spec-20.
    MediaPriority,
    /// A durable subject obligation reached its `due_at` time (docs/specs/subject-runtime.md#spec-13-8;
    /// R071/R072 proactive steps). The subject itself produces this fact;
    /// the obligation row, not this wake, remains the authority.
    SelfWake,
}
