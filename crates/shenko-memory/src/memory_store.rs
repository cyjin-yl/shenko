//! Memory acceptance and persistence — docs/specs/memory.md#spec-12-2 (intent → committed).
//!
//! An intent is accepted immediately and made durable as an `accepted` row in
//! `memory_intents`. Processing an intent turns it into a searchable memory
//! entry and marks the intent `committed`, or marks it `failed`/`conflicted`
//! with a reason. A `forget` intent is bound to the entry it retires:
//! processing marks that entry deleted (semantic forget, docs/specs/memory.md#spec-12-4) in one
//! ledger transaction. The original text stays durable and readable through
//! [`MemoryStore::read`]; search and list simply stop surfacing it.
//!
//! The operation kind of a request (save/correct/forget/recall/reflect) and
//! the open knowledge kind of the produced entry ("note", "commitment",
//! "insight", ...) are separate facts everywhere, including the schema.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::info;

use shenko_store::{
    MemoryCommitOutcome, MemoryEntry, MemoryIntentAdmission, MemoryIntentFailure,
    MemoryIntentRecord, Store, MEMORY_OP_FORGET, MEMORY_OP_RECALL,
};
use shenko_types::ids::EntityId;
use shenko_types::object::ObjectRef;
use shenko_types::subject::{MemoryIntent, MemoryIntentKind};

use crate::backend::{MemoryHit, MemoryRequest, MemorySupersessionStatus, RecallQuery};
use crate::excerpt::excerpt_for;

/// Stable reference returned to callers after an intent is accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedRef {
    pub intent_id: EntityId,
    pub status: &'static str,
}

/// The full request envelope held until the intent is processed.
#[derive(Debug, Clone)]
pub struct QueuedIntent {
    pub intent: MemoryIntent,
    pub entry_id: EntityId,
    pub content: String,
    pub scope: String,
    pub knowledge_kind: String,
    pub supersedes_entry_id: Option<EntityId>,
    pub applicability: Option<String>,
    pub confidence: Option<f64>,
}

/// Result of processing one intent.
#[derive(Debug, Clone)]
pub enum ProcessOutcome {
    /// The intent became a persisted memory entry.
    Committed(MemoryEntry),
    /// The request conflicts with the current fact; both sides are kept and
    /// the durable intent ends `conflicted` (docs/specs/memory.md#spec-12-2).
    Conflicted { reason: String, current_revision: u64 },
    /// Persistence was refused; the durable intent row records the reason.
    Failed(String),
    /// The intent had already reached a terminal state.
    AlreadyProcessed(String),
    /// No queued or durable intent with that id exists.
    NotFound,
}

/// Acceptance and processing surface for memory intents (docs/specs/memory.md#spec-12-2).
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

    /// Queue a subject-style intent together with its payload text. The
    /// knowledge kind of the produced entry is the open "note" vocabulary.
    pub fn accept_intent_with_content(
        &self,
        intent: MemoryIntent,
        content: Option<String>,
    ) -> Result<AcceptedRef, anyhow::Error> {
        if intent.kind == MemoryIntentKind::Forget {
            return Err(anyhow::anyhow!(
                "forget intents bind the retired entry id; use accept_forget()"
            ));
        }
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
                "committed" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "committed" });
                }
                "failed" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "failed" });
                }
                "conflicted" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "conflicted" });
                }
                "accepted" => {},
                _ => return Err(anyhow::anyhow!("unsupported durable memory intent state")),
            }
            entry_id = stored.record.entry_id.ok_or_else(|| anyhow::anyhow!("accepted memory has no entry binding"))?;
        }

        self.enqueue(QueuedIntent {
            intent: intent.clone(),
            entry_id,
            content,
            scope,
            knowledge_kind: shenko_store::default_knowledge_kind(),
            supersedes_entry_id: None,
            applicability: None,
            confidence: None,
        });

        if let Some(tx) = &self.legacy_tx {
            tx.send(intent)
                .map_err(|e| anyhow::anyhow!("memory intent channel closed: {e}"))?;
        }

        Ok(AcceptedRef { intent_id, status: "accepted" })
    }

    /// Admit a full memory request: the operation, the open knowledge kind,
    /// applicability, confidence, base revision, idempotency identity and the
    /// provenance sources. The content is sealed into the CAS so the request
    /// keeps its exact object identity across nodes.
    pub fn accept_request(
        &self,
        request: &MemoryRequest,
    ) -> Result<AcceptedRef, anyhow::Error> {
        if request.operation == MEMORY_OP_FORGET {
            return Err(anyhow::anyhow!(
                "forget intents bind the retired entry id; use accept_forget()"
            ));
        }
        let intent_id = request.intent_id;
        let mut store_request = request.to_store_request(self.agent_id);
        if !request.content.is_empty() {
            store_request.content_ref = Some(self.seal_text(&request.content)?);
        }
        let mut entry_id = store_request.entry_id.unwrap_or_else(EntityId::new);
        store_request.entry_id = Some(entry_id);

        if let Some(store) = &self.store {
            let record = MemoryIntentRecord::from_request(&store_request);
            let admitted = store
                .admit_memory_intent(&record, store_request.content_ref.as_ref())
                .map_err(|e| anyhow::anyhow!("persist accepted memory intent: {e}"))?;
            let stored = match admitted {
                MemoryIntentAdmission::Inserted(stored) | MemoryIntentAdmission::Existing(stored) => stored,
            };
            match stored.record.state.as_str() {
                "committed" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "committed" });
                }
                "failed" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "failed" });
                }
                "conflicted" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "conflicted" });
                }
                "accepted" => {},
                _ => return Err(anyhow::anyhow!("unsupported durable memory intent state")),
            }
            entry_id = stored.record.entry_id.ok_or_else(|| anyhow::anyhow!("accepted memory has no entry binding"))?;
        }

        self.enqueue(QueuedIntent {
            intent: MemoryIntent {
                intent_id,
                kind: match request.operation.as_str() {
                    shenko_store::MEMORY_OP_CORRECT => MemoryIntentKind::Correct,
                    _ => MemoryIntentKind::Save,
                },
                scope: request.scope.clone(),
                content_ref: store_request.content_ref.clone(),
            },
            entry_id,
            content: request.content.clone(),
            scope: request.scope.clone(),
            knowledge_kind: request.knowledge_kind.clone(),
            supersedes_entry_id: request.supersedes_entry_id,
            applicability: request.applicability.clone(),
            confidence: request.confidence,
        });

        Ok(AcceptedRef { intent_id, status: "accepted" })
    }

    /// Admit a semantic-forget intent bound to the entry it retires.
    ///
    /// The caller keeps the intent id, so a replay after a restart re-reads
    /// the same durable request instead of fabricating a second one; the
    /// intent's `entry_id` binding is the target being forgotten. A `Forget`
    /// intent carries no object reference.
    pub fn accept_forget(
        &self,
        intent: MemoryIntent,
        entry_id: EntityId,
    ) -> Result<AcceptedRef, anyhow::Error> {
        if intent.kind != MemoryIntentKind::Forget {
            return Err(anyhow::anyhow!("accept_forget requires a Forget intent"));
        }
        if intent.content_ref.is_some() {
            return Err(anyhow::anyhow!("forget intents do not carry an object reference"));
        }
        let intent_id = intent.intent_id;
        let scope = if intent.scope.is_empty() {
            "personal".to_string()
        } else {
            intent.scope.clone()
        };

        if let Some(store) = &self.store {
            let record = MemoryIntentRecord::accepted(
                intent_id,
                self.agent_id,
                kind_str(intent.kind),
                scope.clone(),
                String::new(),
                None,
                Some(entry_id),
            );
            let admitted = store
                .admit_memory_intent(&record, None)
                .map_err(|e| anyhow::anyhow!("persist accepted memory intent: {e}"))?;
            let stored = match admitted {
                MemoryIntentAdmission::Inserted(stored) | MemoryIntentAdmission::Existing(stored) => stored,
            };
            // The durable binding wins over the terminal status: a replay
            // bound to a different target must refuse even when the original
            // request already committed or failed.
            if stored.record.entry_id != Some(entry_id) {
                return Err(anyhow::anyhow!(
                    "forget intent is already bound to a different target entry"
                ));
            }
            match stored.record.state.as_str() {
                "committed" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "committed" });
                }
                "failed" => {
                    self.remove_queued(intent_id);
                    return Ok(AcceptedRef { intent_id, status: "failed" });
                }
                "accepted" => {}
                _ => return Err(anyhow::anyhow!("unsupported durable memory intent state")),
            }
        }

        self.enqueue(QueuedIntent {
            intent: intent.clone(),
            entry_id,
            content: String::new(),
            scope,
            knowledge_kind: shenko_store::default_knowledge_kind(),
            supersedes_entry_id: None,
            applicability: None,
            confidence: None,
        });

        if let Some(tx) = &self.legacy_tx {
            tx.send(intent)
                .map_err(|e| anyhow::anyhow!("memory intent channel closed: {e}"))?;
        }

        Ok(AcceptedRef { intent_id, status: "accepted" })
    }

    fn enqueue(&self, queued: QueuedIntent) {
        let mut queue = self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !queue.iter().any(|item| item.intent.intent_id == queued.intent.intent_id) {
            queue.push_back(queued);
        }
        drop(queue);
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
    /// committed, or mark it failed/conflicted with a reason.
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
        if stored.record.state == "committed" || stored.record.state == "failed"
            || stored.record.state == "conflicted"
        {
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
                    shenko_store::MEMORY_OP_SAVE => MemoryIntentKind::Save,
                    shenko_store::MEMORY_OP_CORRECT => MemoryIntentKind::Correct,
                    shenko_store::MEMORY_OP_FORGET => MemoryIntentKind::Forget,
                    shenko_store::MEMORY_OP_RECALL => MemoryIntentKind::Recall,
                    _ => MemoryIntentKind::Save,
                };
                let content_ref = match stored.content_ref {
                    Some(reference) => reference,
                    None if stored.record.content_ref.is_none() => None,
                    None => return Err(anyhow::anyhow!("legacy memory content reference cannot be recovered without its digest")),
                };
                let entry_id = stored.record.entry_id.ok_or_else(|| anyhow::anyhow!("accepted memory has no entry binding"))?;
                QueuedIntent {
                    intent: MemoryIntent { intent_id, kind, scope: stored.record.scope.clone(), content_ref },
                    entry_id,
                    content: stored.record.content.clone(),
                    scope: stored.record.scope.clone(),
                    knowledge_kind: stored.record.knowledge_kind.clone(),
                    supersedes_entry_id: stored.record.supersedes_entry_id,
                    applicability: stored.record.applicability.clone(),
                    confidence: stored.record.confidence,
                }
            }
        };

        match self.commit_queued(&store, &queued) {
            Ok(entry) => {
                self.remove_queued(intent_id);
                Ok(ProcessOutcome::Committed(entry))
            }
            Err(e) => {
                if let Some(conflict) = e.downcast_ref::<MemoryConflict>() {
                    // The store already recorded the durable conflict; both
                    // sides of the disagreement stay readable.
                    self.remove_queued(intent_id);
                    return Ok(ProcessOutcome::Conflicted {
                        reason: conflict.reason.clone(),
                        current_revision: conflict.current_revision,
                    });
                }
                if let Some(store_error) = e.downcast_ref::<shenko_types::error::ShenkoError>() {
                    if matches!(store_error, shenko_types::error::ShenkoError::Conflict(_)) {
                        if let Some(state) = store.memory_intent_state(intent_id)? {
                            if state == "committed" || state == "failed" || state == "conflicted" {
                                self.remove_queued(intent_id);
                                if state == "conflicted" {
                                    let stored = store.memory_intent(intent_id)?;
                                    let reason = stored
                                        .and_then(|stored| stored.record.reason)
                                        .unwrap_or_else(|| "memory request conflicted".to_string());
                                    return Ok(ProcessOutcome::Conflicted { reason, current_revision: 0 });
                                }
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
        // Another handle or an import may already have settled a queued ID,
        // so the durable pending query alone cannot reconcile this cache.
        let queued_ids: Vec<EntityId> = self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|queued| queued.intent.intent_id)
            .collect();
        for id in queued_ids {
            if matches!(self.process_intent(id)?, ProcessOutcome::Committed(_)) {
                committed += 1;
            }
        }
        let Some(store) = &self.store else { return Ok(committed) };
        loop {
            let ids = store.pending_library_memory_intents(self.agent_id, 256)?;
            if ids.is_empty() { break; }
            for id in ids {
                if matches!(self.process_intent(id)?, ProcessOutcome::Committed(_)) {
                    committed += 1;
                }
            }
        }
        Ok(committed)
    }

    /// Search persisted memory (docs/specs/context-policies.md#spec-12-3). Delegates to the store, which
    /// selects FTS5 or a LIKE substring scan. Live facts only.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, anyhow::Error> {
        let store = self.store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("memory store has no persistence backend; use with_store()")
        })?;
        let mut entries = store
            .search_memory(query, limit)
            .map_err(|e| anyhow::anyhow!("search memory: {e}"))?;
        entries.retain(|entry| entry.agent_id == self.agent_id);
        Ok(entries)
    }

    /// Recent persisted memory entries, newest first.
    pub fn list(&self, limit: usize) -> Result<Vec<MemoryEntry>, anyhow::Error> {
        let store = self.store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("memory store has no persistence backend; use with_store()")
        })?;
        let mut entries = store
            .list_memory(limit)
            .map_err(|e| anyhow::anyhow!("list memory: {e}"))?;
        entries.retain(|entry| entry.agent_id == self.agent_id);
        Ok(entries)
    }

    /// Read one entry by id, including semantically forgotten rows. This is
    /// the read-back channel docs/specs/memory.md#spec-12-4 requires: forget hides the entry
    /// from `search`/`list` but never erases the original text.
    pub fn read(&self, entry_id: EntityId) -> Result<Option<MemoryEntry>, anyhow::Error> {
        let store = self.store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("memory store has no persistence backend; use with_store()")
        })?;
        store
            .memory_entry(entry_id)
            .map_err(|e| anyhow::anyhow!("read memory: {e}"))
    }

    /// Recall with match-centred excerpts. There is no audience ACL: the
    /// subject's own knowledge is never hidden from the subject by the
    /// current chat audience (docs/specs/subject-runtime.md#spec-6).
    pub fn recall(&self, query: &RecallQuery) -> Result<Vec<MemoryHit>, anyhow::Error> {
        let store = self.store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("memory store has no persistence backend; use with_store()")
        })?;
        let limit = query.limit.max(1);
        let mut entries = if query.text.trim().is_empty() {
            if query.include_retired {
                store.list_memory_including_retired(limit)
            } else {
                store.list_memory(limit)
            }
        } else if query.include_retired {
            store.search_memory_including_retired(&query.text, limit)
        } else {
            store.search_memory(&query.text, limit)
        }
        .map_err(|e| anyhow::anyhow!("recall memory: {e}"))?;
        if !query.knowledge_kinds.is_empty() {
            entries.retain(|entry| query.knowledge_kinds.iter().any(|kind| kind == &entry.kind));
        }
        if !query.scopes.is_empty() {
            entries.retain(|entry| query.scopes.iter().any(|scope| scope == &entry.scope));
        }
        // Recall is over this backend's own memory: another agent's entries
        // are never surfaced through this surface (docs/specs/memory.md#spec-12-1 scope).
        entries.retain(|entry| entry.agent_id == self.agent_id);
        let hits = entries
            .into_iter()
            .map(|entry| {
                let excerpt = if query.text.trim().is_empty() {
                    None
                } else {
                    Some(excerpt_for(&entry.content, &query.text))
                };
                let supersession = if entry.superseded_by.is_some() {
                    MemorySupersessionStatus::Superseded
                } else if entry.supersedes.is_some() {
                    MemorySupersessionStatus::Superseding
                } else {
                    MemorySupersessionStatus::Current
                };
                MemoryHit { entry, excerpt, supersession }
            })
            .collect();
        Ok(hits)
    }

    fn seal_text(&self, content: &str) -> Result<ObjectRef, anyhow::Error> {
        let store = self.store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("memory store has no persistence backend; use with_store()")
        })?;
        let sealed = store
            .cas
            .seal_object(content.as_bytes(), "text/plain", "memory-intents", "text", 1)
            .map_err(|e| anyhow::anyhow!("seal memory content: {e}"))?;
        Ok(sealed.object)
    }

    fn commit_queued(
        &self,
        store: &Store,
        queued: &QueuedIntent,
    ) -> Result<MemoryEntry, anyhow::Error> {
        match queued.intent.kind {
            MemoryIntentKind::Save | MemoryIntentKind::Correct => {}
            MemoryIntentKind::Forget => {
                store
                    .commit_memory_forget(queued.intent.intent_id)
                    .map_err(anyhow::Error::new)?;
                // The intent row already committed above; the read-back only
                // reports what was retired and can never silently succeed.
                return store
                    .memory_entry(queued.entry_id)
                    .map_err(anyhow::Error::new)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("forgotten memory entry is missing after commit")
                    });
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

        let mut entry = MemoryEntry::new(self.agent_id, content);
        entry.entry_id = queued.entry_id;
        entry.scope = queued.scope.clone();
        entry.kind = queued.knowledge_kind.clone();
        entry.applicability = queued.applicability.clone();
        entry.confidence = queued.confidence;
        entry.supersedes = queued.supersedes_entry_id;
        entry.source = queued
            .intent
            .content_ref
            .as_ref()
            .map(|_| format!("memory-intent:{}", queued.intent.intent_id));
        entry.source_ref = queued
            .intent
            .content_ref
            .as_ref()
            .map(|object| object.locator.clone());

        match store
            .commit_memory_intent(&entry, queued.intent.intent_id)
            .map_err(anyhow::Error::new)?
        {
            MemoryCommitOutcome::Committed { .. } => Ok(entry),
            MemoryCommitOutcome::Conflicted { reason, current_revision } => {
                Err(anyhow::anyhow!(MemoryConflict { reason, current_revision }))
            }
        }
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

/// Internal carrier so a store conflict can be told apart from a plain
/// processing failure without string matching.
struct MemoryConflict {
    reason: String,
    current_revision: u64,
}

impl std::fmt::Display for MemoryConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "memory conflict: {} (current revision {})", self.reason, self.current_revision)
    }
}

impl std::fmt::Debug for MemoryConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for MemoryConflict {}

fn kind_str(kind: MemoryIntentKind) -> &'static str {
    match kind {
        MemoryIntentKind::Save => shenko_store::MEMORY_OP_SAVE,
        MemoryIntentKind::Correct => shenko_store::MEMORY_OP_CORRECT,
        MemoryIntentKind::Forget => shenko_store::MEMORY_OP_FORGET,
        MemoryIntentKind::Recall => MEMORY_OP_RECALL,
    }
}
