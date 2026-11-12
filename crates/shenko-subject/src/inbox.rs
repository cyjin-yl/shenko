//! Persistent subject inbox — docs/specs/subject-runtime.md#spec-5-1
use tokio::sync::{mpsc, Mutex};

// The event types are protocol data defined in `shenko-types`; they are
// re-exported here because the queue below carries them.
pub use shenko_types::inbox::{InboxEvent, InboxEventKind, SELF_WAKE_KIND, SUBJECT_WAKE_SOURCE};

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
