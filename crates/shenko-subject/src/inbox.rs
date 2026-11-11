//! Persistent subject inbox — docs/specs/subject-runtime.md#spec-5-1
use tokio::sync::{mpsc, Mutex};
use shenko_types::ids::EntityId;
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

/// Subject inbox — uses tokio::sync::Mutex so the guard is Send across .await.
pub struct Inbox {
    tx: mpsc::Sender<InboxEvent>,
    rx: Mutex<mpsc::Receiver<InboxEvent>>,
}

impl Inbox {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self { tx, rx: Mutex::new(rx) }
    }

    /// Stop new in-memory admissions even while an owning Arc survives in main.
    pub async fn close(&self) { self.rx.lock().await.close(); }

    pub fn sender(&self) -> mpsc::Sender<InboxEvent> { self.tx.clone() }

    pub async fn recv_batch(&self, max: usize, timeout_ms: u64) -> Vec<InboxEvent> {
        let mut events = vec![];
        let mut rx = self.rx.lock().await;
        // First drain everything that is already buffered. A wake channel
        // that is full must be emptied completely: otherwise the dispatcher
        // blocks on a bounded buffer that only this consumer can empty, and
        // the subject's own next input is stuck behind it.
        while events.len() < max {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(_) => break,
            }
        }
        if events.len() >= max {
            return events;
        }
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_millis(timeout_ms);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => {
                    events.push(ev);
                    if events.len() >= max { break; }
                    // Take everything that is already waiting instead of
                    // sleeping through it.
                    while events.len() < max {
                        match rx.try_recv() {
                            Ok(event) => events.push(event),
                            Err(_) => break,
                        }
                    }
                }
                _ => break,
            }
        }
        events
    }
}
