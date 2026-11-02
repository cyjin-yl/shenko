//! Replaceable memory backend — SPEC §12.3.
//!
//! Every memory implementation speaks one contract: retain/recall/reflect/
//! correct/forget/read/flush/health. The storage mechanism never decides
//! what is worth remembering; the caller's request and the subject's judgment
//! do. The store-backed implementation in this crate is the truth of what
//! happened; Git knowledge, external QMD/OpenViking/Hindsight backends and
//! research workers are alternative faces of the same contract, not parallel
//! memory systems.
//!
//! Operation kinds (what to do with a fact) and open knowledge-kind strings
//! (what kind of fact it is) are separate here and in the durable schema.
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use shenko_store::{MemoryCommitOutcome, MemoryEntry, Store};
use shenko_types::ids::EntityId;
use shenko_types::object::ObjectRef;

use crate::excerpt::MatchExcerpt;
use crate::memory_store::{AcceptedRef, MemoryStore, ProcessOutcome};

/// A memory request, as a caller states it. This is the full request
/// envelope: knowledge kind, applicability, confidence, base revision,
/// idempotency identity and provenance ride with the content itself.
#[derive(Debug, Clone)]
pub struct MemoryRequest {
    pub intent_id: EntityId,
    /// The operation: "save" | "reflect" | "correct" | "forget" | "recall" or
    /// another verb the caller owns. Never the knowledge kind.
    pub operation: String,
    /// The open knowledge kind of the produced entry ("note", "commitment",
    /// "insight", "reflection", ...).
    pub knowledge_kind: String,
    pub scope: String,
    pub content: String,
    /// The entry a `correct` replaces; the replaced entry keeps its text and
    /// gains a supersession link (SPEC §45.12).
    pub supersedes_entry_id: Option<EntityId>,
    /// The entry a `forget` retires. The operation target and the supersession
    /// target are separate facts on the envelope.
    pub target_entry_id: Option<EntityId>,
    /// Where the fact applies; `None` means "not stated".
    pub applicability: Option<String>,
    /// Confidence in the fact as stated; `None` means unknown, not zero.
    pub confidence: Option<f64>,
    /// The revision the writer based the request on. A different current
    /// revision is a durable conflict, never a silent overwrite.
    pub base_revision: Option<u64>,
    /// Caller-owned idempotency identity; one key names one request per agent.
    pub idempotency_key: Option<String>,
    /// Every provenance reference this request was derived from.
    pub sources: Vec<String>,
}

impl MemoryRequest {
    /// A save request with the envelope filled in and the knowledge kind left
    /// to the caller.
    pub fn save(agent_id: EntityId, content: impl Into<String>) -> Self {
        Self {
            intent_id: EntityId::new(),
            operation: shenko_store::MEMORY_OP_SAVE.to_string(),
            knowledge_kind: shenko_store::default_knowledge_kind(),
            scope: "personal".to_string(),
            content: content.into(),
            supersedes_entry_id: None,
            target_entry_id: None,
            applicability: None,
            confidence: None,
            base_revision: None,
            idempotency_key: None,
            sources: Vec::new(),
        }
    }

    /// A reflection is a retain whose knowledge kind defaults to
    /// "reflection"; it always carries the experience it came from.
    pub fn reflection(agent_id: EntityId, content: impl Into<String>, sources: Vec<String>) -> Self {
        let _ = agent_id;
        Self {
            intent_id: EntityId::new(),
            operation: shenko_store::MEMORY_OP_REFLECT.to_string(),
            knowledge_kind: "reflection".to_string(),
            scope: "personal".to_string(),
            content: content.into(),
            supersedes_entry_id: None,
            target_entry_id: None,
            applicability: None,
            confidence: None,
            base_revision: None,
            idempotency_key: None,
            sources,
        }
    }

    /// The entry this request operates on: `forget` retires `target_entry_id`.
    pub fn entry_target(&self) -> Option<EntityId> {
        self.target_entry_id
    }

    pub(crate) fn to_store_request(&self, agent_id: EntityId) -> shenko_store::MemoryIntentRequest {
        shenko_store::MemoryIntentRequest {
            intent_id: self.intent_id,
            agent_id,
            operation: self.operation.clone(),
            knowledge_kind: self.knowledge_kind.clone(),
            scope: self.scope.clone(),
            content: self.content.clone(),
            content_ref: None,
            entry_id: self.target_entry_id.or(Some(self.intent_id)),
            supersedes_entry_id: self.supersedes_entry_id,
            applicability: self.applicability.clone(),
            confidence: self.confidence,
            base_revision: self.base_revision,
            idempotency_key: self.idempotency_key.clone(),
            sources: self.sources.clone(),
        }
    }
}

/// A recall query as the subject or a worker states it.
#[derive(Debug, Clone, Default)]
pub struct RecallQuery {
    pub text: String,
    /// Empty means every knowledge kind.
    pub knowledge_kinds: Vec<String>,
    /// Empty means every scope. There is deliberately no audience ACL: the
    /// subject's own personal knowledge is never hidden from the subject by
    /// the current chat audience (SPEC §6 unified knowledge).
    pub scopes: Vec<String>,
    /// Include semantically forgotten and superseded entries, with their
    /// status visible. Default recall surfaces live facts only.
    pub include_retired: bool,
    pub limit: usize,
}

impl RecallQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            limit: 20,
            ..Self::default()
        }
    }
}

/// One recall hit: the entry plus the match-centred excerpt that proves where
/// the match actually is in the original text (SPEC §12.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHit {
    pub entry: MemoryEntry,
    pub excerpt: Option<MatchExcerpt>,
    pub supersession: MemorySupersessionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySupersessionStatus {
    Current,
    Superseded,
    Superseding,
}

/// The retain/reflect/correct result.
#[derive(Debug, Clone)]
pub enum RetainOutcome {
    /// accepted ≠ remembered: the request is durable and will be processed.
    Accepted(AcceptedRef),
    Committed { entry: MemoryEntry, superseded: Option<EntityId> },
    /// The request conflicts with the current fact; both sides are kept.
    Conflicted { reason: String, current_revision: u64 },
    Failed(String),
}

/// The forget result. Semantic forget is monotonic: the entry keeps its text
/// and gains a tombstone; nothing physically disappears.
#[derive(Debug, Clone)]
pub enum ForgetOutcome {
    Committed { entry: MemoryEntry },
    AlreadyRetired(MemoryEntry),
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryHealth {
    pub agent_id: EntityId,
    pub pending_intents: usize,
    pub live_entries: usize,
    pub retired_entries: usize,
    pub fts5_available: bool,
    pub git: GitHealth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHealth {
    pub repository: Option<String>,
    pub head: Option<String>,
    pub commits: u64,
}

#[derive(Debug, Clone)]
pub struct FlushOutcome {
    pub committed: usize,
    pub failed: usize,
    pub conflicts: usize,
}

/// The one memory backend contract (SPEC §12.3). Implementations are
/// replaceable; the store-backed one is the truth of record.
pub trait MemoryBackend: Send + Sync {
    /// Admit and process a retain request. The request is admitted before any
    /// entry exists; processing turns it into a searchable entry.
    fn retain(&self, request: &MemoryRequest) -> Result<RetainOutcome, anyhow::Error>;

    /// Record a reflection derived from experience.
    fn reflect(&self, request: &MemoryRequest) -> Result<RetainOutcome, anyhow::Error>;

    /// Correct a fact: the new entry supersedes the named one. A base
    /// revision mismatch is a conflict, not a silent overwrite.
    fn correct(&self, request: &MemoryRequest) -> Result<RetainOutcome, anyhow::Error>;

    /// Retire a fact. Semantic forget is monotonic: the entry keeps its text
    /// and gains a tombstone; nothing physically disappears.
    fn forget(&self, request: &MemoryRequest) -> Result<ForgetOutcome, anyhow::Error>;

    /// Search memory with match-centred excerpts.
    fn recall(&self, query: &RecallQuery) -> Result<Vec<MemoryHit>, anyhow::Error>;

    /// Read one entry by id, including retired rows.
    fn read(&self, entry_id: EntityId) -> Result<Option<MemoryEntry>, anyhow::Error>;

    /// The supersession lineage of an entry, oldest first.
    fn lineage(&self, entry_id: EntityId) -> Result<Vec<MemoryEntry>, anyhow::Error>;

    /// Process every pending request now.
    fn flush(&self) -> Result<FlushOutcome, anyhow::Error>;

    /// Current backend health.
    fn health(&self) -> Result<MemoryHealth, anyhow::Error>;
}

/// The store-backed memory backend: SQLite ledger + CAS objects are the
/// durable truth (SPEC §12.1/§12.2).
pub struct StoreMemoryBackend {
    worker: MemoryStore,
    store: Arc<Store>,
    git: Option<Arc<crate::knowledge::GitKnowledgeStore>>,
}

impl StoreMemoryBackend {
    pub fn new(agent_id: EntityId, store: Arc<Store>) -> Self {
        Self {
            worker: MemoryStore::with_store(agent_id, Arc::clone(&store)),
            store,
            git: None,
        }
    }

    /// Attach the Git knowledge repository so every committed fact is also
    /// written as an explainable text commit (SPEC §10.3).
    pub fn with_knowledge(mut self, git: Arc<crate::knowledge::GitKnowledgeStore>) -> Self {
        self.git = Some(git);
        self
    }

    fn knowledge_sync(&self, entry: &MemoryEntry) {
        let Some(git) = &self.git else { return };
        if let Err(error) = git.sync_entry(entry) {
            // The Git knowledge repo is explainable text, not the truth of
            // record: a sync failure is logged, never fails the durable
            // commit and never fabricates a Git ref.
            tracing::warn!(error = %error, entry_id = %entry.entry_id, "memory knowledge sync failed");
        }
    }

    /// The durable reason of a conflicted admission replay, if any.
    fn conflict_reason(&self, intent_id: EntityId) -> Option<String> {
        self.store
            .memory_intent(intent_id)
            .ok()
            .flatten()
            .and_then(|stored| stored.record.reason)
    }

    /// A replay that found its request already settled. Conflicts surface
    /// with their durable reason; other terminal states stay as admitted.
    fn replay_outcome(&self, accepted: &AcceptedRef, intent_id: EntityId) -> RetainOutcome {
        if accepted.status == "conflicted" {
            return RetainOutcome::Conflicted {
                reason: self
                    .conflict_reason(intent_id)
                    .unwrap_or_else(|| "memory request conflicts with the current fact".into()),
                current_revision: 0,
            };
        }
        RetainOutcome::Accepted(accepted.clone())
    }
}

impl MemoryBackend for StoreMemoryBackend {
    fn retain(&self, request: &MemoryRequest) -> Result<RetainOutcome, anyhow::Error> {
        let accepted = self.worker.accept_request(request)?;
        if accepted.status != "accepted" {
            return Ok(self.replay_outcome(&accepted, request.intent_id));
        }
        match self.worker.process_intent(request.intent_id)? {
            ProcessOutcome::Committed(entry) => {
                self.knowledge_sync(&entry);
                Ok(RetainOutcome::Committed { entry, superseded: None })
            }
            ProcessOutcome::Conflicted { reason, current_revision } => {
                Ok(RetainOutcome::Conflicted { reason, current_revision })
            }
            ProcessOutcome::Failed(reason) => Ok(RetainOutcome::Failed(reason)),
            ProcessOutcome::AlreadyProcessed(state) if state == "committed" => {
                let entry = self.worker.read(request.intent_id).ok().flatten();
                match entry {
                    Some(entry) => Ok(RetainOutcome::Committed { entry, superseded: None }),
                    None => Ok(RetainOutcome::Failed("committed entry is missing".into())),
                }
            }
            ProcessOutcome::AlreadyProcessed(state) => Ok(RetainOutcome::Failed(format!("intent is {state}"))),
            ProcessOutcome::NotFound => Ok(RetainOutcome::Failed("intent vanished".into())),
        }
    }

    fn reflect(&self, request: &MemoryRequest) -> Result<RetainOutcome, anyhow::Error> {
        let mut request = request.clone();
        request.operation = shenko_store::MEMORY_OP_REFLECT.to_string();
        if request.knowledge_kind.is_empty() {
            request.knowledge_kind = "reflection".to_string();
        }
        self.retain(&request)
    }

    fn correct(&self, request: &MemoryRequest) -> Result<RetainOutcome, anyhow::Error> {
        if request.supersedes_entry_id.is_none() {
            anyhow::bail!("correct requires the entry it supersedes");
        }
        let accepted = self.worker.accept_request(request)?;
        if accepted.status != "accepted" {
            return Ok(self.replay_outcome(&accepted, request.intent_id));
        }
        match self.worker.process_intent(request.intent_id)? {
            ProcessOutcome::Committed(entry) => {
                self.knowledge_sync(&entry);
                let superseded = request.supersedes_entry_id;
                Ok(RetainOutcome::Committed { entry, superseded })
            }
            ProcessOutcome::Conflicted { reason, current_revision } => {
                Ok(RetainOutcome::Conflicted { reason, current_revision })
            }
            ProcessOutcome::Failed(reason) => Ok(RetainOutcome::Failed(reason)),
            ProcessOutcome::AlreadyProcessed(state) => Ok(RetainOutcome::Failed(format!("intent is {state}"))),
            ProcessOutcome::NotFound => Ok(RetainOutcome::Failed("intent vanished".into())),
        }
    }

    fn recall(&self, query: &RecallQuery) -> Result<Vec<MemoryHit>, anyhow::Error> {
        Ok(self.worker.recall(query)?)
    }

    fn forget(&self, request: &MemoryRequest) -> Result<ForgetOutcome, anyhow::Error> {
        let Some(target) = request.entry_target() else {
            anyhow::bail!("forget requires the entry it retires");
        };
        let intent_id = request.intent_id;
        let accepted = self
            .worker
            .accept_forget(
                shenko_types::subject::MemoryIntent {
                    intent_id,
                    kind: shenko_types::subject::MemoryIntentKind::Forget,
                    scope: request.scope.clone(),
                    content_ref: None,
                },
                target,
            )
            ?;
        if accepted.status != "accepted" {
            // Replay of a settled request: read the durable outcome.
            return match self.worker.read(target)? {
                Some(entry) if entry.deleted => Ok(ForgetOutcome::AlreadyRetired(entry)),
                Some(entry) => Ok(ForgetOutcome::Committed { entry }),
                None => Ok(ForgetOutcome::Failed("forgotten entry is missing".into())),
            };
        }
        match self.worker.process_intent(intent_id)? {
            ProcessOutcome::Committed(entry) => Ok(ForgetOutcome::Committed { entry }),
            ProcessOutcome::Failed(reason) => Ok(ForgetOutcome::Failed(reason)),
            ProcessOutcome::AlreadyProcessed(_) => match self.worker.read(target)? {
                Some(entry) if entry.deleted => Ok(ForgetOutcome::AlreadyRetired(entry)),
                Some(entry) => Ok(ForgetOutcome::Committed { entry }),
                None => Ok(ForgetOutcome::Failed("forgotten entry is missing".into())),
            },
            ProcessOutcome::Conflicted { reason, .. } => Ok(ForgetOutcome::Failed(reason)),
            ProcessOutcome::NotFound => Ok(ForgetOutcome::Failed("intent vanished".into())),
        }
    }

    fn read(&self, entry_id: EntityId) -> Result<Option<MemoryEntry>, anyhow::Error> {
        Ok(self.worker.read(entry_id)?)
    }

    fn lineage(&self, entry_id: EntityId) -> Result<Vec<MemoryEntry>, anyhow::Error> {
        self.store
            .memory_supersession_lineage(entry_id)
            .map_err(anyhow::Error::new)
    }

    fn flush(&self) -> Result<FlushOutcome, anyhow::Error> {
        // Reconcile the in-process queue and the durable pending set; each
        // committed fact syncs to the Git knowledge repository.
        let agent_id = self.worker.agent_id();
        let pending = self
            .store
            .pending_library_memory_intents(agent_id, 1024)
            .map_err(anyhow::Error::new)?;
        let mut outcome = FlushOutcome { committed: 0, failed: 0, conflicts: 0 };
        for id in pending {
            match self.worker.process_intent(id) {
                Ok(ProcessOutcome::Committed(entry)) => {
                    self.knowledge_sync(&entry);
                    outcome.committed += 1;
                }
                Ok(ProcessOutcome::Conflicted { .. }) => outcome.conflicts += 1,
                Ok(ProcessOutcome::Failed(_)) => outcome.failed += 1,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, intent_id = %id, "memory flush failed");
                    outcome.failed += 1;
                }
            }
        }
        if let Some(git) = &self.git {
            if let Err(error) = git.sync_pending(&self.store, agent_id) {
                tracing::warn!(error = %error, "memory knowledge sync failed");
                outcome.failed += 1;
            }
        }
        Ok(outcome)
    }

    fn health(&self) -> Result<MemoryHealth, anyhow::Error> {
        let agent_id = self.worker.agent_id();
        let pending = self
            .store
            .pending_library_memory_intents(agent_id, 4096)
            .map_err(anyhow::Error::new)?
            .len();
        let live = self.store.list_memory(10_000).map_err(anyhow::Error::new)?.len();
        let retired = self
            .store
            .list_memory_including_retired(10_000)
            .map_err(anyhow::Error::new)?
            .len()
            .saturating_sub(live);
        let git = match &self.git {
            Some(git) => git.health().unwrap_or(GitHealth {
                repository: None,
                head: None,
                commits: 0,
            }),
            None => GitHealth { repository: None, head: None, commits: 0 },
        };
        Ok(MemoryHealth {
            agent_id,
            pending_intents: pending,
            live_entries: live,
            retired_entries: retired,
            fts5_available: self.store.capabilities.fts5_available,
            git,
        })
    }
}

/// Keep the outcome types used by the store visible to callers of this crate.
pub use shenko_store::MemoryCommitOutcome as StoreCommitOutcome;
