//! Memory acceptance and persistence — SPEC §12.2 (intent → committed).
//!
//! An intent is accepted immediately and made durable as an `accepted` row in
//! `memory_intents`. Processing an intent turns it into a searchable memory
//! entry and marks the intent `committed`, or marks it `failed` with a reason.
//! The legacy channel-based constructor is kept for callers that only need
//! acceptance notifications; persistence requires [`MemoryStore::with_store`].
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::info;

use shenko_store::{MemoryEntry as StoredMemoryEntry, MemoryIntentAdmission, MemoryIntentFailure, MemoryIntentRecord, Store};
use shenko_types::ids::EntityId;
use shenko_types::subject::{MemoryIntent, MemoryIntentKind};

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

/// Stable reference returned to callers after an intent is accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedRef {
    pub intent_id: EntityId,
    pub status: &'static str,
}

/// An accepted intent held in process memory until processed.
#[derive(Debug, Clone)]
pub struct QueuedIntent {
    pub intent: MemoryIntent,
    pub entry_id: EntityId,
    pub content: String,
    pub scope: String,
}

/// Result of processing one intent.
#[derive(Debug, Clone)]
pub enum ProcessOutcome {
    /// The intent became a persisted memory entry.
    Committed(StoredMemoryEntry),
    /// Persistence was refused; the durable intent row records the reason.
    Failed(String),
    /// The intent had already reached a terminal state.
    AlreadyProcessed(String),
    /// No queued or durable intent with that id exists.
    NotFound,
}

/// Acceptance and processing surface for memory intents (SPEC §12.2).
pub struct MemoryStore {
    agent_id: EntityId,
    store: Option<Arc<Store>>,
    queue: Mutex<VecDeque<QueuedIntent>>,
    legacy_tx: Option<UnboundedSender<MemoryIntent>>,
}

impl MemoryStore {
    /// Legacy constructor: acceptance notifications only, no persistence.
    /// Prefer [`MemoryStore::with_store`] when a store is available.
    pub fn new() -> (Self, UnboundedReceiver<MemoryIntent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                agent_id: EntityId::new(),
                store: None,
                queue: Mutex::new(VecDeque::new()),
                legacy_tx: Some(tx),
            },
            rx,
        )
    }

    /// Store-backed constructor. Accepted intents are persisted immediately
    /// and processed against this handle.
    pub fn with_store(agent_id: EntityId, store: Arc<Store>) -> Self {
        Self {
            agent_id,
            store: Some(store),
            queue: Mutex::new(VecDeque::new()),
            legacy_tx: None,
        }
    }

    pub fn agent_id(&self) -> EntityId {
        self.agent_id
    }

    /// Queue an intent with optional explicit content. When content is absent,
    /// processing reads it from the CAS object named by `content_ref`.
    pub fn accept_intent(&self, intent: MemoryIntent) -> Result<AcceptedRef, anyhow::Error> {
        self.accept_intent_with_content(intent, None)
    }

    /// Queue an intent together with its payload text.
    pub fn accept_intent_with_content(
        &self,
        intent: MemoryIntent,
        content: Option<String>,
    ) -> Result<AcceptedRef, anyhow::Error> {
        let intent_id = intent.intent_id;
        let mut entry_id = EntityId::new();
        let content = content.unwrap_or_default();
        let scope = if intent.scope.is_empty() {
            "personal".to_string()
        } else {
            intent.scope.clone()
        };
        let content_ref = intent
            .content_ref
            .as_ref()
            .map(|object| object.locator.clone());

        if let Some(store) = &self.store {
            let record = MemoryIntentRecord::accepted(
                intent_id,
                self.agent_id,
                kind_str(intent.kind),
                scope.clone(),
                content.clone(),
                content_ref,
                Some(entry_id),
            );
            let admitted = store
                .admit_memory_intent(&record, intent.content_ref.as_ref())
                .map_err(|e| anyhow::anyhow!("persist accepted memory intent: {e}"))?;
            let stored = match admitted {
                MemoryIntentAdmission::Inserted(stored) | MemoryIntentAdmission::Existing(stored) => stored,
            };
            match stored.record.state.as_str() {
                "committed" => return Ok(AcceptedRef { intent_id, status: "committed" }),
                "failed" => return Ok(AcceptedRef { intent_id, status: "failed" }),
                "accepted" => {},
                _ => return Err(anyhow::anyhow!("unsupported durable memory intent state")),
            }
            entry_id = stored.record.entry_id.ok_or_else(|| anyhow::anyhow!("accepted memory has no entry binding"))?;
        }

        let mut queue = self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !queue.iter().any(|item| item.intent.intent_id == intent_id) {
            queue.push_back(QueuedIntent {
                intent: intent.clone(),
                entry_id,
                content,
                scope,
            });
        }
        drop(queue);

        if let Some(tx) = &self.legacy_tx {
            tx.send(intent)
                .map_err(|e| anyhow::anyhow!("memory intent channel closed: {e}"))?;
        }

        Ok(AcceptedRef { intent_id, status: "accepted" })
    }

    /// Snapshot of intents accepted but not yet processed.
    pub fn pending_intents(&self) -> Vec<QueuedIntent> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Process one intent: insert its memory entry and mark the intent
    /// committed, or mark it failed with a reason.
    pub fn process_intent(
        &self,
        intent_id: EntityId,
    ) -> Result<ProcessOutcome, anyhow::Error> {
        let Some(store) = self.store.clone() else {
            return Err(anyhow::anyhow!(
                "memory store has no persistence backend; use with_store()"
            ));
        };

        let Some(stored) = store.memory_intent(intent_id)? else { return Ok(ProcessOutcome::NotFound) };
        if stored.record.agent_id != self.agent_id { return Ok(ProcessOutcome::NotFound); }
        if stored.record.state == "committed" || stored.record.state == "failed" {
            self.remove_queued(intent_id);
            return Ok(ProcessOutcome::AlreadyProcessed(stored.record.state));
        }
        if stored.record.state != "accepted" { return Err(anyhow::anyhow!("unsupported durable memory intent state")); }

        let queued = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|q| q.intent.intent_id == intent_id)
            .cloned();
        let queued = match queued {
            Some(queued) => queued,
            None => {
                let kind = match stored.record.kind.as_str() {
                    "save" => MemoryIntentKind::Save,
                    "correct" => MemoryIntentKind::Correct,
                    "forget" => MemoryIntentKind::Forget,
                    "recall" => MemoryIntentKind::Recall,
                    _ => return Ok(ProcessOutcome::NotFound),
                };
                let content_ref = match stored.content_ref {
                    Some(reference) => reference,
                    None if stored.record.content_ref.is_none() => None,
                    None => return Err(anyhow::anyhow!("legacy memory content reference cannot be recovered without its digest")),
                };
                let entry_id = stored.record.entry_id.ok_or_else(|| anyhow::anyhow!("accepted memory has no entry binding"))?;
                QueuedIntent {
                    intent: MemoryIntent { intent_id, kind, scope: stored.record.scope.clone(), content_ref },
                    entry_id, content: stored.record.content, scope: stored.record.scope,
                }
            }
        };

        match self.commit_queued(&store, &queued) {
            Ok(entry) => {
                self.remove_queued(intent_id);
                Ok(ProcessOutcome::Committed(entry))
            }
            Err(e) => {
                if let Some(store_error) = e.downcast_ref::<shenko_types::error::ShenkoError>() {
                    if matches!(store_error, shenko_types::error::ShenkoError::Conflict(_)) {
                        if let Some(state) = store.memory_intent_state(intent_id)? {
                            if state == "committed" || state == "failed" {
                                self.remove_queued(intent_id);
                                return Ok(ProcessOutcome::AlreadyProcessed(state));
                            }
                        }
                    }
                    return Err(e);
                }
                let reason = e.to_string();
                match store.fail_memory_intent(intent_id, &reason)? {
                    MemoryIntentFailure::Failed => {
                        self.remove_queued(intent_id);
                        Ok(ProcessOutcome::Failed(reason))
                    }
                    MemoryIntentFailure::AlreadyTerminal(state) => {
                        self.remove_queued(intent_id);
                        Ok(ProcessOutcome::AlreadyProcessed(state))
                    }
                    MemoryIntentFailure::NotFound => Ok(ProcessOutcome::NotFound),
                }
            }
        }
    }

    /// Process every queued intent; returns how many were committed.
    pub fn process_all(&self) -> Result<usize, anyhow::Error> {
        let mut committed = 0;
        loop {
            let ids: Vec<EntityId> = match &self.store {
                Some(store) => store.pending_library_memory_intents(self.agent_id, 256)?,
                None => self.pending_intents().iter().map(|q| q.intent.intent_id).collect(),
            };
            if ids.is_empty() { break; }
            for id in ids {
                if matches!(self.process_intent(id)?, ProcessOutcome::Committed(_)) {
                    committed += 1;
                }
            }
        }
        Ok(committed)
    }

    /// Search persisted memory (SPEC §12.3). Delegates to the store, which
    /// selects FTS5 or a LIKE substring scan.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<StoredMemoryEntry>, anyhow::Error> {
        let store = self.store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("memory store has no persistence backend; use with_store()")
        })?;
        store
            .search_memory(query, limit)
            .map_err(|e| anyhow::anyhow!("search memory: {e}"))
    }

    /// Recent persisted memory entries.
    pub fn list(&self, limit: usize) -> Result<Vec<StoredMemoryEntry>, anyhow::Error> {
        let store = self.store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("memory store has no persistence backend; use with_store()")
        })?;
        store
            .list_memory(limit)
            .map_err(|e| anyhow::anyhow!("list memory: {e}"))
    }

    fn commit_queued(
        &self,
        store: &Store,
        queued: &QueuedIntent,
    ) -> Result<StoredMemoryEntry, anyhow::Error> {
        match queued.intent.kind {
            MemoryIntentKind::Save | MemoryIntentKind::Correct => {}
            MemoryIntentKind::Forget => {
                return Err(anyhow::anyhow!(
                    "forget requires a resolved target entry; not supported yet"
                ));
            }
            MemoryIntentKind::Recall => {
                return Err(anyhow::anyhow!(
                    "recall is a query; use search() instead of process_intent()"
                ));
            }
        }

        let content = if !queued.content.is_empty() {
            queued.content.clone()
        } else if let Some(content_ref) = &queued.intent.content_ref {
            let bytes = store
                .cas
                .read_object(&content_ref.digest)
                .map_err(anyhow::Error::new)?;
            String::from_utf8(bytes)?
        } else {
            String::new()
        };

        if content.is_empty() {
            return Err(anyhow::anyhow!("memory intent has no content"));
        }

        let mut entry = StoredMemoryEntry::new(self.agent_id, content);
        entry.entry_id = queued.entry_id;
        entry.scope = queued.scope.clone();
        entry.kind = kind_str(queued.intent.kind).to_string();
        entry.source_ref = queued
            .intent
            .content_ref
            .as_ref()
            .map(|object| object.locator.clone());

        store
            .commit_memory_intent(&entry, queued.intent.intent_id)
            .map_err(anyhow::Error::new)?;
        Ok(entry)
    }

    fn remove_queued(&self, intent_id: EntityId) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|q| q.intent.intent_id != intent_id);
    }

    /// Legacy worker: drain acceptance notifications and log them.
    /// Persistence happens through [`MemoryStore::process_intent`].
    pub fn spawn_worker(mut rx: UnboundedReceiver<MemoryIntent>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(intent) = rx.recv().await {
                info!("memory intent accepted: {}", intent.intent_id);
            }
        })
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new().0
    }
}

fn kind_str(kind: MemoryIntentKind) -> &'static str {
    match kind {
        MemoryIntentKind::Save => "save",
        MemoryIntentKind::Correct => "correct",
        MemoryIntentKind::Forget => "forget",
        MemoryIntentKind::Recall => "recall",
    }
}
