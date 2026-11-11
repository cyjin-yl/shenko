//! Memory persistence and search — docs/specs/memory.md#spec-12-2 (intent → committed) and
//! docs/specs/context-policies.md#spec-12-3 (中文检索 / Chinese retrieval).
//!
//! Two representations live in SQLite:
//! * `memory_intents` — the durable intent queue. An intent is accepted
//!   before any memory entry exists and later transitions to `committed`
//!   (with an `entry_id`) or `failed` (with a reason).
//! * `memory_entries` — the searchable memory records.
//!
//! A `forget` intent binds the `entry_id` it retires instead of producing a
//! new entry: committing it flags that entry `deleted` (docs/specs/memory.md#spec-12-4 semantic
//! forget). Search and list keep their `deleted = 0` filter, the original
//! row and text stay durable, and [`Store::memory_entry`] reads them back.
//!
//! Search uses FTS5 when the build provides it and a LIKE scan otherwise.
//! Chinese text is routed to LIKE even when FTS5 exists: `unicode61` treats a
//! run of Han characters as one token, so a substring query such as "深刻"
//! does not match the token for "我的深刻记忆". This limitation is deliberate
//! and covered by tests.
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use shenko_types::commit::{CommitReceipt, CommitRef, DurabilityProfile, UpdatedEntity};
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, RecordRevision};
use shenko_types::object::ObjectRef;

use crate::store::Store;

const ENTRY_COLUMNS: &str = "m.entry_id, m.agent_id, m.kind, m.content, m.source_ref, \
     m.source, m.applicability, m.confidence, m.scope, m.record_revision, m.created_at_ms, \
     m.updated_commit_seq, m.deleted, m.superseded_by, m.supersedes, m.git_commit, m.git_path";

/// A persisted memory record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub entry_id: EntityId,
    pub agent_id: EntityId,
    /// The open knowledge kind of this fact ("note", "commitment",
    /// "insight", ...). It is NOT the operation that produced the entry:
    /// `save`/`correct`/`forget`/`recall`/`reflect` are operation kinds and
    /// live on the intent row (docs/specs/memory.md#spec-12-2).
    pub kind: String,
    pub content: String,
    /// Human-readable provenance label of the observation this entry came
    /// from (a session, a task, a conversation). Never an authority grant.
    #[serde(default)]
    pub source: Option<String>,
    /// Locator of the sealed CAS object holding the original payload.
    pub source_ref: Option<String>,
    /// Where the fact applies ("only at the office", "for this project").
    #[serde(default)]
    pub applicability: Option<String>,
    /// Confidence in the fact as recorded. `None` means unknown, not zero.
    #[serde(default)]
    pub confidence: Option<f64>,
    pub scope: String,
    pub record_revision: u64,
    pub created_at_ms: i64,
    pub updated_commit_seq: Option<u64>,
    /// Semantic-forget flag (docs/specs/memory.md#spec-12-4): a forgotten entry is invisible to
    /// default search/list but the original text stays readable and the row
    /// is never physically removed by this path.
    #[serde(default)]
    pub deleted: bool,
    /// The entry that replaced this one (docs/specs/memory.md#spec-45-12 supersession). A
    /// superseded entry stays readable but is no longer a current fact.
    #[serde(default)]
    pub superseded_by: Option<EntityId>,
    /// The entry this one replaced.
    #[serde(default)]
    pub supersedes: Option<EntityId>,
    /// Commit of the Git knowledge repository that holds the text form of
    /// this entry (docs/specs/memory.md#spec-10-3). `None` means the text is not in Git yet.
    #[serde(default)]
    pub git_commit: Option<String>,
    /// Path of the text form inside the knowledge repository.
    #[serde(default)]
    pub git_path: Option<String>,
}

impl MemoryEntry {
    pub fn new(agent_id: EntityId, content: impl Into<String>) -> Self {
        Self {
            entry_id: EntityId::new(),
            agent_id,
            kind: "note".to_string(),
            content: content.into(),
            source: None,
            source_ref: None,
            applicability: None,
            confidence: None,
            scope: "personal".to_string(),
            record_revision: 0,
            created_at_ms: Utc::now().timestamp_millis(),
            updated_commit_seq: None,
            deleted: false,
            superseded_by: None,
            supersedes: None,
            git_commit: None,
            git_path: None,
        }
    }
}

/// Durable intent row (`state` = accepted | committed | failed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIntentRecord {
    pub intent_id: EntityId,
    pub agent_id: EntityId,
    /// The operation this request performs. This vocabulary is open and is
    /// distinct from the knowledge kind of the produced entry.
    pub kind: String,
    /// The open knowledge kind requested for the produced entry.
    #[serde(default = "default_knowledge_kind")]
    pub knowledge_kind: String,
    pub scope: String,
    pub content: String,
    pub content_ref: Option<String>,
    pub entry_id: Option<EntityId>,
    /// The entry this request replaces (operation `correct`). The replaced
    /// entry keeps its text and gains a supersession link.
    #[serde(default)]
    pub supersedes_entry_id: Option<EntityId>,
    /// Where the fact applies; `None` means "not stated".
    #[serde(default)]
    pub applicability: Option<String>,
    /// Confidence in the fact as stated; `None` means unknown.
    #[serde(default)]
    pub confidence: Option<f64>,
    /// The revision the writer expected to base the change on. A different
    /// current revision is a conflict, never a silent overwrite (docs/specs/memory.md#spec-12-2).
    #[serde(default)]
    pub base_revision: Option<u64>,
    /// Caller-owned idempotency identity. Two requests with the same key for
    /// the same agent are one request, whoever retries.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Every provenance reference this request was derived from.
    #[serde(default)]
    pub sources: Vec<String>,
    pub state: String,
    pub reason: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// The default open knowledge kind of a retained fact.
pub fn default_knowledge_kind() -> String {
    "note".to_string()
}

/// Operation kinds of the durable memory request chain (docs/specs/memory.md#spec-12-2).
/// The vocabulary is open: a caller may use another verb, and the store
/// treats it as an operation, never as a knowledge kind.
pub const MEMORY_OP_SAVE: &str = "save";
pub const MEMORY_OP_CORRECT: &str = "correct";
pub const MEMORY_OP_FORGET: &str = "forget";
pub const MEMORY_OP_RECALL: &str = "recall";
pub const MEMORY_OP_REFLECT: &str = "reflect";

/// A memory request as the caller states it. The store admits it once and
/// keeps its identity, so a replay resolves to the same durable request.
#[derive(Debug, Clone)]
pub struct MemoryIntentRequest {
    pub intent_id: EntityId,
    pub agent_id: EntityId,
    /// The operation: save | correct | forget | recall | reflect | ...
    pub operation: String,
    /// The open knowledge kind of the produced entry.
    pub knowledge_kind: String,
    pub scope: String,
    pub content: String,
    pub content_ref: Option<ObjectRef>,
    /// The entry this request produces (or, for `forget`, retires).
    pub entry_id: Option<EntityId>,
    /// The entry a `correct` replaces.
    pub supersedes_entry_id: Option<EntityId>,
    pub applicability: Option<String>,
    pub confidence: Option<f64>,
    pub base_revision: Option<u64>,
    pub idempotency_key: Option<String>,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct MemoryIntentBinding {
    schema_version: u32,
    content_ref: Option<ObjectRef>,
}

pub(crate) fn same_memory_object(left: Option<&ObjectRef>, right: Option<&ObjectRef>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            // Content identity only. A locator names a place on THIS node and
            // the store namespace names the backend that holds it ("local"
            // here, "replicated" on the peer), so neither may decide whether
            // two nodes describe the same bytes.
            left.digest == right.digest && left.bytes == right.bytes
                && left.media_type == right.media_type && left.codec == right.codec
        }
        _ => false,
    }
}

/// The node-independent tail of a CAS locator: `<ab>/<cd>/<hex>`.
///
/// A locator names a place on one node (`file:///node-a/cas/ab/cd/<hex>`), so
/// comparing whole locator strings across nodes reports a conflict for
/// identical content. The content-addressed tail is the same everywhere.
pub(crate) fn portable_locator(locator: &str) -> &str {
    let mut segments: Vec<&str> = locator.split('/').collect();
    if segments.len() < 3 {
        return locator;
    }
    let digest = segments[segments.len() - 1];
    let shard = segments[segments.len() - 2];
    let parent = segments[segments.len() - 3];
    let is_digest = digest.len() == 64
        && digest.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    let is_shard = shard.len() == 2 && shard.chars().all(|c| c.is_ascii_hexdigit());
    let is_parent = parent.len() == 2 && parent.chars().all(|c| c.is_ascii_hexdigit());
    if !(is_digest && is_shard && is_parent) {
        // An unknown locator shape keeps its literal form: two different
        // unknown shapes are never silently equated.
        return locator;
    }
    let mut tail_start = locator.len();
    for _ in 0..3 {
        match locator[..tail_start].rfind('/') {
            Some(index) => tail_start = index,
            None => return locator,
        }
    }
    &locator[tail_start + 1..]
}

/// Do two locators name the same content, regardless of the node holding it?
pub(crate) fn same_locator(left: Option<&ObjectRef>, right: Option<&str>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => portable_locator(&left.locator) == portable_locator(right),
        _ => false,
    }
}

pub(crate) fn memory_binding_matches_locator(data: &str, locator: Option<&str>) -> bool {
    serde_json::from_str::<MemoryIntentBinding>(data).is_ok_and(|binding| {
        binding.schema_version == 1
            && same_locator(binding.content_ref.as_ref(), locator)
    })
}

pub(crate) fn same_memory_bindings(left: &str, right: &str) -> bool {
    match (serde_json::from_str::<MemoryIntentBinding>(left), serde_json::from_str::<MemoryIntentBinding>(right)) {
        (Ok(left), Ok(right)) if left.schema_version == 1 && right.schema_version == 1 =>
            same_memory_object(left.content_ref.as_ref(), right.content_ref.as_ref()),
        _ => false,
    }
}

/// The inner option is the supplied object reference; the outer option records
/// whether an exact reference binding was retained at admission at all.
#[derive(Debug, Clone)]
pub struct StoredMemoryIntent {
    pub record: MemoryIntentRecord,
    pub content_ref: Option<Option<ObjectRef>>,
}

#[derive(Debug, Clone)]
pub enum MemoryIntentAdmission {
    Inserted(StoredMemoryIntent),
    Existing(StoredMemoryIntent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryIntentFailure {
    Failed,
    AlreadyTerminal(String),
    NotFound,
}

/// The result of committing a memory request (docs/specs/memory.md#spec-12-2).
#[derive(Debug, Clone)]
pub enum MemoryCommitOutcome {
    /// The entry was written and its intent is committed. `superseded` names
    /// the entry this one replaced, if any.
    Committed {
        receipt: CommitReceipt,
        entry: MemoryEntry,
        superseded: Option<EntityId>,
    },
    /// The request conflicts with the current durable state. Nothing was
    /// written: the current entry keeps its text and the request keeps its
    /// own claim, so the subject can adjudicate instead of losing evidence.
    Conflicted { reason: String, current_revision: u64 },
}

impl MemoryIntentRecord {
    pub fn accepted(
        intent_id: EntityId,
        agent_id: EntityId,
        kind: impl Into<String>,
        scope: impl Into<String>,
        content: impl Into<String>,
        content_ref: Option<String>,
        entry_id: Option<EntityId>,
    ) -> Self {
        Self::accepted_with(
            intent_id,
            agent_id,
            kind,
            scope,
            content,
            content_ref,
            entry_id,
            MemoryRequestDetail::default(),
        )
    }

    /// The full request envelope: knowledge kind, applicability, confidence,
    /// base revision, idempotency identity and provenance.
    #[allow(clippy::too_many_arguments)]
    pub fn accepted_with(
        intent_id: EntityId,
        agent_id: EntityId,
        kind: impl Into<String>,
        scope: impl Into<String>,
        content: impl Into<String>,
        content_ref: Option<String>,
        entry_id: Option<EntityId>,
        detail: MemoryRequestDetail,
    ) -> Self {
        let now = Utc::now().timestamp_millis();
        Self {
            intent_id,
            agent_id,
            kind: kind.into(),
            knowledge_kind: detail.knowledge_kind,
            scope: scope.into(),
            content: content.into(),
            content_ref,
            entry_id,
            supersedes_entry_id: detail.supersedes_entry_id,
            applicability: detail.applicability,
            confidence: detail.confidence,
            base_revision: detail.base_revision,
            idempotency_key: detail.idempotency_key,
            sources: detail.sources,
            state: "accepted".to_string(),
            reason: None,
            created_at_ms: now,
            updated_at_ms: now,
        }
    }

    /// Build the durable record of an admitted request.
    pub fn from_request(request: &MemoryIntentRequest) -> Self {
        Self::accepted_with(
            request.intent_id,
            request.agent_id,
            request.operation.clone(),
            request.scope.clone(),
            request.content.clone(),
            request.content_ref.as_ref().map(|object| object.locator.clone()),
            request.entry_id,
            MemoryRequestDetail {
                knowledge_kind: request.knowledge_kind.clone(),
                supersedes_entry_id: request.supersedes_entry_id,
                applicability: request.applicability.clone(),
                confidence: request.confidence,
                base_revision: request.base_revision,
                idempotency_key: request.idempotency_key.clone(),
                sources: request.sources.clone(),
            },
        )
    }
}

/// The request envelope fields that are not the request identity itself.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRequestDetail {
    pub knowledge_kind: String,
    pub supersedes_entry_id: Option<EntityId>,
    pub applicability: Option<String>,
    pub confidence: Option<f64>,
    pub base_revision: Option<u64>,
    pub idempotency_key: Option<String>,
    pub sources: Vec<String>,
}

impl Default for MemoryRequestDetail {
    fn default() -> Self {
        MemoryRequestDetail {
            knowledge_kind: default_knowledge_kind(),
            supersedes_entry_id: None,
            applicability: None,
            confidence: None,
            base_revision: None,
            idempotency_key: None,
            sources: Vec::new(),
        }
    }
}

fn read_memory_intent(
    conn: &rusqlite::Connection,
    intent_id: EntityId,
) -> Result<Option<StoredMemoryIntent>, ShenkoError> {
    let row = conn.query_row(
        "SELECT agent_id,kind,scope,content,content_ref,entry_id,state,reason,created_at_ms,updated_at_ms,
                knowledge_kind,applicability,confidence,base_revision,idempotency_key,sources,supersedes_entry_id
         FROM memory_intents WHERE intent_id=?1",
        [intent_id.to_string()],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
            r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?,
            r.get::<_, String>(6)?, r.get::<_, Option<String>>(7)?, r.get::<_, i64>(8)?, r.get::<_, i64>(9)?,
            r.get::<_, Option<String>>(10)?, r.get::<_, Option<String>>(11)?, r.get::<_, Option<f64>>(12)?,
            r.get::<_, Option<i64>>(13)?, r.get::<_, Option<String>>(14)?, r.get::<_, Option<String>>(15)?,
            r.get::<_, Option<String>>(16)?)),
    ).optional().map_err(|e| ShenkoError::storage(e.to_string()))?;
    let Some((agent, kind, scope, content, locator, entry, state, reason, created, updated,
        knowledge_kind, applicability, confidence, base_revision, idempotency_key, sources,
        supersedes_entry_id)) = row else {
        return Ok(None);
    };
    let binding: Option<String> = conn.query_row(
        "SELECT data FROM memory_intent_bindings WHERE intent_id=?1",
        [intent_id.to_string()], |r| r.get(0),
    ).optional().map_err(|e| ShenkoError::storage(e.to_string()))?;
    let content_ref = binding.map(|data| -> Result<Option<ObjectRef>, ShenkoError> {
        let binding: MemoryIntentBinding = serde_json::from_str(&data)
            .map_err(|e| ShenkoError::storage(format!("invalid memory intent binding: {e}")))?;
        if binding.schema_version != 1 {
            return Err(ShenkoError::conflict("unsupported memory intent binding version"));
        }
        if !same_locator(binding.content_ref.as_ref(), locator.as_deref()) {
            return Err(ShenkoError::conflict("memory intent locator and binding differ"));
        }
        Ok(binding.content_ref)
    }).transpose()?;
    Ok(Some(StoredMemoryIntent {
        record: MemoryIntentRecord {
            intent_id,
            agent_id: agent.parse().map_err(|e| ShenkoError::storage(format!("invalid memory agent id: {e}")))?,
            kind,
            knowledge_kind: knowledge_kind.unwrap_or_else(default_knowledge_kind),
            scope, content, content_ref: locator,
            entry_id: entry.map(|id| id.parse().map_err(|e| ShenkoError::storage(format!("invalid memory entry id: {e}")))).transpose()?,
            supersedes_entry_id: supersedes_entry_id
                .map(|id| id.parse().map_err(|e| ShenkoError::storage(format!("invalid memory supersedes id: {e}"))))
                .transpose()?,
            applicability,
            confidence,
            base_revision: base_revision.map(|value| value.max(0) as u64),
            idempotency_key,
            sources: sources
                .map(|raw| serde_json::from_str::<Vec<String>>(&raw))
                .transpose()
                .map_err(|e| ShenkoError::storage(format!("invalid memory intent sources: {e}")))?
                .unwrap_or_default(),
            state, reason, created_at_ms: created, updated_at_ms: updated,
        },
        content_ref,
    }))
}

fn memory_storage_failure(error: &ShenkoError) -> bool {
    matches!(error, ShenkoError::Storage(_) | ShenkoError::Io(_) | ShenkoError::Serde(_))
}

impl Store {
    /// Insert a memory entry and record the commit in the ledger commit log.
    pub fn insert_memory_entry(&self, entry: &MemoryEntry) -> Result<CommitReceipt, ShenkoError> {
        self.commit_entry(entry, None)
    }

    /// Atomically insert the entry produced by an intent and mark that intent
    /// committed. One ledger commit covers both facts. A request whose base
    /// revision no longer matches the current state ends as `conflicted`
    /// instead of overwriting another writer's fact (docs/specs/memory.md#spec-12-2).
    pub fn commit_memory_intent(
        &self,
        entry: &MemoryEntry,
        intent_id: EntityId,
    ) -> Result<MemoryCommitOutcome, ShenkoError> {
        self.ledger.with_mutation(
            || self.commit_entry_outcome(entry, Some(intent_id)),
            memory_storage_failure,
        )
    }

    /// Atomically mark the entry a `forget` intent is bound to as deleted
    /// and mark that intent committed (docs/specs/memory.md#spec-12-4 semantic forget).
    ///
    /// This is *not* an erase: the row, its original text and its
    /// provenance stay durable and readable through
    /// [`Store::memory_entry`], while `search_memory`/`list_memory` stop
    /// surfacing it. A replay after commit reports the entry already
    /// forgotten instead of redoing the decision. A missing or foreign
    /// target refuses the commit and leaves the intent `accepted` and
    /// retryable: the entry may still arrive through replication, and the
    /// refusal is never mistaken for a settled request.
    pub fn commit_memory_forget(
        &self,
        intent_id: EntityId,
    ) -> Result<CommitReceipt, ShenkoError> {
        self.ledger
            .with_mutation(|| self.try_commit_forget(intent_id), memory_storage_failure)
    }

    fn try_commit_forget(
        &self,
        intent_id: EntityId,
    ) -> Result<CommitReceipt, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| ShenkoError::storage(e.to_string()))?;

        let stored = read_memory_intent(&tx, intent_id)?
            .ok_or_else(|| ShenkoError::not_found("memory intent"))?;
        let request = &stored.record;
        if request.state != "accepted" || request.kind != "forget" {
            return Err(ShenkoError::conflict("memory forget intent is terminal or not a forget"));
        }
        let target = request
            .entry_id
            .ok_or_else(|| ShenkoError::conflict("memory forget intent has no target entry"))?;

        // A semantic forget may only retire an entry owned by the same agent
        // that made the request; it is never a cross-agent delete channel.
        let (owner, revision): (String, i64) = tx
            .query_row(
                "SELECT agent_id, record_revision FROM memory_entries WHERE entry_id=?1",
                [target.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| ShenkoError::storage(e.to_string()))?
            .ok_or_else(|| ShenkoError::not_found("memory forget target entry"))?;
        if owner != request.agent_id.to_string() {
            return Err(ShenkoError::conflict("memory forget target belongs to another agent"));
        }

        let seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "UPDATE memory_entries SET deleted=1, record_revision=?2, \
             updated_commit_seq=?3, updated_at=datetime('now') WHERE entry_id=?1",
            params![target.to_string(), revision.max(0) + 1, seq.0 as i64],
        )
        .map_err(|e| ShenkoError::storage(e.to_string()))?;
        let changed = tx
            .execute(
                "UPDATE memory_intents SET state='committed',reason=NULL,updated_at_ms=?2 \
                 WHERE intent_id=?1 AND state='accepted'",
                params![intent_id.to_string(), Utc::now().timestamp_millis()],
            )
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        if changed != 1 {
            return Err(ShenkoError::conflict("memory intent changed during commit"));
        }

        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
             VALUES (?1,?2,?3,1)",
            params![
                self.ledger.ledger_id().to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string(),
            ],
        )
        .map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.commit()
            .map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(CommitReceipt {
            commit_ref: CommitRef::new(self.ledger.ledger_id(), seq),
            durability_profile: DurabilityProfile::PowerLossLocal,
            store_incarnation: self.capabilities.incarnation,
            updated: vec![UpdatedEntity {
                entity_id: target,
                record_revision: RecordRevision((revision.max(0) + 1) as u64),
            }],
            sealed_objects: Vec::new(),
        })
    }

    fn commit_entry(
        &self,
        entry: &MemoryEntry,
        intent_id: Option<EntityId>,
    ) -> Result<CommitReceipt, ShenkoError> {
        self.ledger
            .with_mutation(|| self.try_commit_entry(entry, intent_id), memory_storage_failure)
    }

    fn try_commit_entry(
        &self,
        entry: &MemoryEntry,
        intent_id: Option<EntityId>,
    ) -> Result<CommitReceipt, ShenkoError> {
        match self.commit_entry_outcome(entry, intent_id)? {
            MemoryCommitOutcome::Committed { receipt, .. } => Ok(receipt),
            MemoryCommitOutcome::Conflicted { reason, .. } => {
                Err(ShenkoError::conflict(format!("memory request conflicts: {reason}")))
            }
        }
    }

    /// Commit the entry an admitted request produced.
    ///
    /// The outcome separates the two results docs/specs/memory.md#spec-12-2 asks for: a commit
    /// and a conflict. A conflict is a durable fact of its own — the request
    /// keeps both sides of the disagreement (the current entry stays exactly
    /// as it was, the request keeps its own text) and the intent ends in the
    /// `conflicted` state instead of pretending it was remembered.
    fn commit_entry_outcome(
        &self,
        entry: &MemoryEntry,
        intent_id: Option<EntityId>,
    ) -> Result<MemoryCommitOutcome, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        let seq = self.ledger.allocate_seq(&tx)?;

        if let Some(intent_id) = intent_id {
            let stored = read_memory_intent(&tx, intent_id)?
                .ok_or_else(|| ShenkoError::not_found("memory intent"))?;
            let request = &stored.record;
            if request.state != "accepted" {
                return Err(ShenkoError::conflict("memory intent is terminal"));
            }
            // The base revision is the revision the writer read before it
            // formed the request. A different current revision means another
            // writer changed the same fact first: the request is conflicting,
            // not stale-but-writable.
            if let Some(expected) = request.base_revision {
                let current = Self::memory_entry_revision(&tx, request.supersedes_entry_id)
                    .or_else(|| Self::memory_entry_revision(&tx, Some(entry.entry_id)))
                    .unwrap_or(0);
                if current != expected {
                    let reason = format!(
                        "base revision {expected} does not match the current revision {current}"
                    );
                    let changed = tx.execute(
                        "UPDATE memory_intents SET state='conflicted',reason=?2,updated_at_ms=?3
                         WHERE intent_id=?1 AND state='accepted'",
                        params![intent_id.to_string(), reason.clone(), Utc::now().timestamp_millis()],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                    if changed != 1 {
                        return Err(ShenkoError::conflict("memory intent changed during commit"));
                    }
                    tx.execute(
                        "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
                         VALUES (?1,?2,?3,1)",
                        params![
                            self.ledger.ledger_id().to_string(),
                            seq.0 as i64,
                            self.capabilities.incarnation.to_string(),
                        ],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                    tx.commit()
                        .map_err(|e| ShenkoError::storage(e.to_string()))?;
                    return Ok(MemoryCommitOutcome::Conflicted { reason, current_revision: current });
                }
            }
        }

        let superseded = Self::write_memory_entry(&tx, entry, intent_id, seq)?;

        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
             VALUES (?1,?2,?3,1)",
            params![
                self.ledger.ledger_id().to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string(),
            ],
        )
        .map_err(|e| ShenkoError::storage(e.to_string()))?;

        tx.commit()
            .map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(MemoryCommitOutcome::Committed {
            receipt: CommitReceipt {
                commit_ref: CommitRef::new(self.ledger.ledger_id(), seq),
                durability_profile: DurabilityProfile::PowerLossLocal,
                store_incarnation: self.capabilities.incarnation,
                updated: vec![UpdatedEntity {
                    entity_id: entry.entry_id,
                    record_revision: RecordRevision(entry.record_revision),
                }],
                sealed_objects: Vec::new(),
            },
            entry: entry.clone(),
            superseded,
        })
    }

    /// The current revision of a stored entry, if it exists.
    fn memory_entry_revision(
        tx: &rusqlite::Transaction<'_>,
        entry_id: Option<EntityId>,
    ) -> Option<u64> {
        let entry_id = entry_id?;
        tx.query_row(
            "SELECT record_revision FROM memory_entries WHERE entry_id=?1",
            [entry_id.to_string()],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .ok()
        .flatten()
        .map(|revision| revision.max(0) as u64)
    }

    /// Write the entry of an admitted request.
    ///
    /// The write is a true upsert, not `INSERT OR REPLACE`: REPLACE deletes
    /// the conflicting row without firing DELETE triggers (unless
    /// `recursive_triggers` is on), which would leave the old text in the
    /// FTS index as a stale orphan. DO UPDATE fires the update trigger and
    /// keeps the rowid stable.
    ///
    /// Two facts are monotonic here: a forgotten entry keeps its tombstone
    /// and a superseded entry keeps its supersession. A later write can
    /// therefore never resurrect a retired fact or silently un-supersede a
    /// replaced one (docs/specs/memory.md#spec-12-4, docs/specs/memory.md#spec-45-12).
    pub(crate) fn write_memory_entry(
        tx: &rusqlite::Transaction<'_>,
        entry: &MemoryEntry,
        intent_id: Option<EntityId>,
        seq: shenko_types::ids::LedgerSeq,
    ) -> Result<Option<EntityId>, ShenkoError> {
        let mut superseded = None;
        if let Some(intent_id) = intent_id {
            let stored = read_memory_intent(tx, intent_id)?
                .ok_or_else(|| ShenkoError::not_found("memory intent"))?;
            let request = &stored.record;
            if request.state != "accepted" || request.agent_id != entry.agent_id
                || request.entry_id != Some(entry.entry_id)
                || request.knowledge_kind != entry.kind
                || request.scope != entry.scope
            {
                return Err(ShenkoError::conflict("memory intent is terminal or entry binding differs"));
            }
            if !request.content.is_empty() || request.content_ref.is_none() {
                if request.content != entry.content {
                    return Err(ShenkoError::conflict("memory entry content differs from accepted intent"));
                }
            } else {
                let object = stored.content_ref.as_ref().and_then(Option::as_ref)
                    .ok_or_else(|| ShenkoError::conflict("legacy memory content identity is unavailable"))?;
                if object.bytes != entry.content.len() as u64
                    || object.digest != shenko_types::object::Digest::blake3(entry.content.as_bytes())
                {
                    return Err(ShenkoError::conflict("memory entry content differs from the accepted object"));
                }
            }
            // The request may replace an earlier statement of the same fact.
            // The replaced entry keeps its text and gains the supersession
            // link; it is never rewritten and never deleted here.
            if let Some(previous) = request.supersedes_entry_id {
                let (owner, deleted, replaced_by): (String, i64, Option<String>) = tx
                    .query_row(
                        "SELECT agent_id, deleted, superseded_by FROM memory_entries WHERE entry_id=?1",
                        [previous.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(|e| ShenkoError::storage(e.to_string()))?
                    .ok_or_else(|| ShenkoError::not_found("memory superseded entry"))?;
                if owner != request.agent_id.to_string() {
                    return Err(ShenkoError::conflict("memory superseded entry belongs to another agent"));
                }
                if deleted != 0 {
                    return Err(ShenkoError::conflict("memory superseded entry is retired"));
                }
                if let Some(existing) = replaced_by {
                    if existing != entry.entry_id.to_string() {
                        return Err(ShenkoError::conflict("memory entry was already superseded by another fact"));
                    }
                } else {
                    tx.execute(
                        "UPDATE memory_entries SET superseded_by=?2, record_revision=record_revision+1,
                         updated_commit_seq=?3, updated_at=datetime('now') WHERE entry_id=?1",
                        params![previous.to_string(), entry.entry_id.to_string(), seq.0 as i64],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                    superseded = Some(previous);
                }
            }
            let changed = tx.execute(
                "UPDATE memory_intents SET state='committed',reason=NULL,updated_at_ms=?2
                 WHERE intent_id=?1 AND state='accepted'",
                params![intent_id.to_string(), Utc::now().timestamp_millis()],
            ).map_err(|e| ShenkoError::storage(e.to_string()))?;
            if changed != 1 {
                return Err(ShenkoError::conflict("memory intent changed during commit"));
            }
        }
        // A true upsert, not INSERT OR REPLACE: REPLACE deletes the conflicting
        // row without firing DELETE triggers (unless recursive_triggers is on),
        // which would leave the old text in the FTS index as a stale orphan.
        // DO UPDATE fires the update trigger and keeps the rowid stable.
        // `deleted` and `superseded_by` are monotonic: a forgotten entry keeps
        // its tombstone, a superseded entry keeps its supersession, and an
        // imported tombstone is never silently resurrected.
        tx.execute(
            "INSERT INTO memory_entries
             (entry_id, agent_id, kind, content, source, source_ref, applicability, confidence,
              scope, record_revision, created_at_ms, updated_commit_seq, updated_at, deleted,
              superseded_by, supersedes, git_commit, git_path)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,datetime('now'),?13,?14,?15,?16,?17)
             ON CONFLICT(entry_id) DO UPDATE SET
                agent_id = excluded.agent_id,
                kind = excluded.kind,
                content = excluded.content,
                source = excluded.source,
                source_ref = excluded.source_ref,
                applicability = excluded.applicability,
                confidence = excluded.confidence,
                scope = excluded.scope,
                record_revision = excluded.record_revision,
                created_at_ms = excluded.created_at_ms,
                updated_commit_seq = excluded.updated_commit_seq,
                updated_at = datetime('now'),
                deleted = MAX(memory_entries.deleted, excluded.deleted),
                superseded_by = COALESCE(memory_entries.superseded_by, excluded.superseded_by),
                supersedes = COALESCE(memory_entries.supersedes, excluded.supersedes),
                git_commit = COALESCE(excluded.git_commit, memory_entries.git_commit),
                git_path = COALESCE(excluded.git_path, memory_entries.git_path)",
            params![
                entry.entry_id.to_string(),
                entry.agent_id.to_string(),
                entry.kind,
                entry.content,
                entry.source,
                entry.source_ref,
                entry.applicability,
                entry.confidence,
                entry.scope,
                entry.record_revision as i64,
                entry.created_at_ms,
                seq.0 as i64,
                entry.deleted as i64,
                entry.superseded_by.map(|id| id.to_string()),
                entry.supersedes.map(|id| id.to_string()),
                entry.git_commit,
                entry.git_path,
            ],
        )
        .map_err(|e| ShenkoError::storage(e.to_string()))?;

        Ok(superseded)
    }

    /// Admit the exact request once. A replay returns the original binding and
    /// state; a different request with the same identity cannot replace it.
    pub fn admit_memory_intent(
        &self,
        rec: &MemoryIntentRecord,
        content_ref: Option<&ObjectRef>,
    ) -> Result<MemoryIntentAdmission, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_admit_memory_intent(rec, content_ref),
            memory_storage_failure,
        )
    }

    fn try_admit_memory_intent(
        &self,
        rec: &MemoryIntentRecord,
        content_ref: Option<&ObjectRef>,
    ) -> Result<MemoryIntentAdmission, ShenkoError> {
        if rec.state != "accepted" || rec.reason.is_some() || rec.entry_id.is_none()
            || !same_locator(content_ref, rec.content_ref.as_deref())
        {
            return Err(ShenkoError::conflict("invalid memory intent admission"));
        }
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        if let Some(existing) = read_memory_intent(&tx, rec.intent_id)? {
            let original = &existing.record;
            if original.content_ref.is_some() && existing.content_ref.is_none() {
                return Err(ShenkoError::conflict("legacy memory reference identity is unavailable"));
            }
            let same_reference = match existing.content_ref.as_ref() {
                Some(stored) => same_memory_object(stored.as_ref(), content_ref),
                None => content_ref.is_none(),
            };
            if original.agent_id != rec.agent_id || original.kind != rec.kind
                || original.knowledge_kind != rec.knowledge_kind
                || original.scope != rec.scope || original.content != rec.content
                || original.applicability != rec.applicability
                || original.confidence != rec.confidence
                || original.base_revision != rec.base_revision
                || original.sources != rec.sources
                || original.supersedes_entry_id != rec.supersedes_entry_id
                || !same_reference
            {
                return Err(ShenkoError::conflict("memory intent identity has different input"));
            }
            return Ok(MemoryIntentAdmission::Existing(existing));
        }
        // An explicit idempotency key names one request for one agent. A
        // retry with the same key resolves to the original request instead
        // of writing a second memory (docs/specs/memory.md#spec-12-2).
        if let Some(key) = rec.idempotency_key.as_ref().filter(|key| !key.is_empty()) {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT intent_id FROM memory_intents
                     WHERE agent_id=?1 AND idempotency_key=?2 AND state!='failed'
                     ORDER BY created_at_ms LIMIT 1",
                    params![rec.agent_id.to_string(), key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            if let Some(intent_id) = existing {
                let id = intent_id
                    .parse()
                    .map_err(|e| ShenkoError::storage(format!("invalid memory intent id: {e}")))?;
                let stored = read_memory_intent(&tx, id)?
                    .ok_or_else(|| ShenkoError::not_found("memory intent"))?;
                let original = &stored.record;
                let same_reference = match stored.content_ref.as_ref() {
                    Some(stored_ref) => same_memory_object(stored_ref.as_ref(), content_ref),
                    None => content_ref.is_none(),
                };
                if original.agent_id != rec.agent_id || original.kind != rec.kind
                    || original.knowledge_kind != rec.knowledge_kind
                    || original.scope != rec.scope || original.content != rec.content
                    || original.applicability != rec.applicability
                    || original.confidence != rec.confidence
                    || original.sources != rec.sources
                    || !same_reference
                {
                    return Err(ShenkoError::conflict(
                        "memory idempotency key is bound to a different request",
                    ));
                }
                return Ok(MemoryIntentAdmission::Existing(stored));
            }
        }
        tx.execute(
            "INSERT INTO memory_intents
             (intent_id,agent_id,kind,knowledge_kind,scope,content,content_ref,entry_id,
              supersedes_entry_id,applicability,confidence,base_revision,idempotency_key,sources,
              state,reason,created_at_ms,updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                rec.intent_id.to_string(), rec.agent_id.to_string(), rec.kind,
                rec.knowledge_kind, rec.scope, rec.content, rec.content_ref,
                rec.entry_id.map(|id| id.to_string()),
                rec.supersedes_entry_id.map(|id| id.to_string()),
                rec.applicability, rec.confidence,
                rec.base_revision.map(|value| value as i64),
                rec.idempotency_key,
                serde_json::to_string(&rec.sources).map_err(|e| ShenkoError::storage(e.to_string()))?,
                rec.state, rec.reason, rec.created_at_ms, rec.updated_at_ms,
            ],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;
        let binding = MemoryIntentBinding { schema_version: 1, content_ref: content_ref.cloned() };
        tx.execute("INSERT INTO memory_intent_bindings(intent_id,data) VALUES(?1,?2)",
            params![rec.intent_id.to_string(), serde_json::to_string(&binding)?],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;
        tx.commit().map_err(|e| ShenkoError::storage(e.to_string()))?;
        Ok(MemoryIntentAdmission::Inserted(StoredMemoryIntent {
            record: rec.clone(), content_ref: Some(content_ref.cloned()),
        }))
    }

    /// Read the durable request and its exact binding, if retained.
    pub fn memory_intent(&self, id: EntityId) -> Result<Option<StoredMemoryIntent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_memory_intent(&conn, id)
    }

    /// Enumerate only this consumer's accepted library intents. Subject-owned
    /// intents use a distinct kind and are settled by their own dispatcher.
    pub fn pending_library_memory_intents(&self, agent_id: EntityId, limit: usize) -> Result<Vec<EntityId>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT intent_id FROM memory_intents m WHERE m.agent_id=?1 AND m.state='accepted'
             AND m.kind IN ('save','correct','forget','recall','reflect')
             AND NOT EXISTS (SELECT 1 FROM subject_intents s WHERE s.intent_id=m.intent_id AND s.kind='memory')
             ORDER BY m.created_at_ms,m.intent_id LIMIT ?2",
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;
        let limit = i64::try_from(limit).map_err(|_| ShenkoError::conflict("memory intent limit exceeds SQLite range"))?;
        let rows = stmt.query_map(params![agent_id.to_string(), limit], |r| r.get::<_, String>(0))
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        rows.map(|row| row.map_err(|e| ShenkoError::storage(e.to_string()))?
            .parse().map_err(|e| ShenkoError::storage(format!("invalid memory intent id: {e}"))))
            .collect()
    }

    /// Current durable state of an intent, if present.
    pub fn memory_intent_state(&self, intent_id: EntityId) -> Result<Option<String>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        match conn.query_row(
            "SELECT state FROM memory_intents WHERE intent_id=?1",
            [intent_id.to_string()],
            |r| r.get::<_, String>(0),
        ) {
            Ok(state) => Ok(Some(state)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(ShenkoError::storage(e.to_string())),
        }
    }

    /// Fail only an accepted intent; a late worker cannot overwrite a terminal fact.
    pub fn fail_memory_intent(&self, intent_id: EntityId, reason: &str) -> Result<MemoryIntentFailure, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_fail_memory_intent(intent_id, reason),
            memory_storage_failure,
        )
    }

    fn try_fail_memory_intent(&self, intent_id: EntityId, reason: &str) -> Result<MemoryIntentFailure, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        let changed = tx.execute(
            "UPDATE memory_intents
                SET state='failed', reason=?2, updated_at_ms=?3
              WHERE intent_id=?1 AND state='accepted'",
            params![intent_id.to_string(), reason, Utc::now().timestamp_millis(),],
        ).map_err(|e| ShenkoError::storage(e.to_string()))?;
        if changed == 1 {
            tx.commit().map_err(|e| ShenkoError::storage(e.to_string()))?;
            return Ok(MemoryIntentFailure::Failed);
        }
        match read_memory_intent(&tx, intent_id)? {
            Some(record) if record.record.state == "committed" || record.record.state == "failed" =>
                Ok(MemoryIntentFailure::AlreadyTerminal(record.record.state)),
            None => Ok(MemoryIntentFailure::NotFound),
            Some(_) => Err(ShenkoError::conflict("memory intent has an unsupported state")),
        }
    }

    /// Search memory content.
    ///
    /// The live set is what default recall surfaces: neither semantically
    /// forgotten nor superseded entries. Use [`Store::search_memory_including_retired`]
    /// for an audit read that keeps tombstones and supersession status.
    ///
    /// * CJK queries use the LIKE substring path (see module docs).
    /// * Other queries use FTS5 `MATCH` when available; any FTS error falls
    ///   back to LIKE so a build without FTS5 still searches correctly.
    pub fn search_memory(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        self.search_memory_with(query, limit, false)
    }

    /// Search every stored entry, including semantically forgotten and
    /// superseded ones. Results keep their `deleted` / `superseded_by`
    /// status so the caller can tell an outdated fact from a current one.
    pub fn search_memory_including_retired(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        self.search_memory_with(query, limit, true)
    }

    fn search_memory_with(
        &self,
        query: &str,
        limit: usize,
        including_retired: bool,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        if contains_cjk(query) {
            return self.search_memory_like(query, limit, including_retired);
        }
        match self.search_memory_fts(query, limit, including_retired) {
            Ok(entries) => Ok(entries),
            Err(e) => {
                tracing::debug!(error = %e, "FTS5 memory search failed; using LIKE fallback");
                self.search_memory_like(query, limit, including_retired)
            }
        }
    }

    fn search_memory_fts(
        &self,
        query: &str,
        limit: usize,
        including_retired: bool,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        // Quoting makes user input a phrase and avoids FTS5 query syntax errors.
        let quoted = format!("\"{}\"", query.replace('"', "\"\""));
        let retired = if including_retired { "" } else { " AND m.superseded_by IS NULL" };
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
               FROM memory_entries_fts f
               JOIN memory_entries m ON m.rowid = f.rowid
              WHERE memory_entries_fts MATCH ?1 AND m.deleted = 0{retired}
              ORDER BY rank
              LIMIT ?2"
        );
        self.query_entries(&sql, params![quoted, limit as i64])
    }

    fn search_memory_like(
        &self,
        query: &str,
        limit: usize,
        including_retired: bool,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        let retired = if including_retired { "" } else { " AND m.superseded_by IS NULL" };
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
               FROM memory_entries m
              WHERE m.deleted = 0{retired} AND m.content LIKE ?1 ESCAPE '\\'
              ORDER BY m.created_at_ms DESC, m.rowid DESC
              LIMIT ?2"
        );
        self.query_entries(&sql, params![like_pattern(query), limit as i64])
    }

    /// Most recent live memory entries, newest first.
    pub fn list_memory(&self, limit: usize) -> Result<Vec<MemoryEntry>, ShenkoError> {
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
               FROM memory_entries m
              WHERE m.deleted = 0 AND m.superseded_by IS NULL
              ORDER BY m.created_at_ms DESC, m.rowid DESC
              LIMIT ?1"
        );
        self.query_entries(&sql, params![limit as i64])
    }

    /// Most recent entries regardless of tombstone or supersession status.
    pub fn list_memory_including_retired(&self, limit: usize) -> Result<Vec<MemoryEntry>, ShenkoError> {
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
               FROM memory_entries m
              ORDER BY m.created_at_ms DESC, m.rowid DESC
              LIMIT ?1"
        );
        self.query_entries(&sql, params![limit as i64])
    }

    /// Read one entry by id, including semantically forgotten rows.
    ///
    /// Semantic forget keeps the original text durable (docs/specs/memory.md#spec-12-4); this
    /// read-back is how an audit or a specified re-check reaches it. Search
    /// and list stay live-only.
    pub fn memory_entry(&self, entry_id: EntityId) -> Result<Option<MemoryEntry>, ShenkoError> {
        let sql = format!(
            "SELECT {ENTRY_COLUMNS}
               FROM memory_entries m
              WHERE m.entry_id = ?1"
        );
        let mut entries = self.query_entries(&sql, params![entry_id.to_string()])?;
        Ok(entries.pop())
    }

    /// The supersession chain an entry belongs to, oldest first (docs/specs/memory.md#spec-45-12).
    ///
    /// Every version of the fact is kept: neither the replaced nor the
    /// replacing entry is rewritten here, the caller can see exactly which
    /// statement superseded which and why the current one rules.
    pub fn memory_supersession_lineage(
        &self,
        entry_id: EntityId,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut lineage = Vec::new();
        let mut cursor = entry_id;
        // Walk to the oldest version of the fact.
        while let Some(entry) = read_entry_by_id(&conn, cursor)? {
            let Some(previous) = entry.supersedes else {
                break;
            };
            cursor = previous;
        }
        // Walk forward to the newest version.
        loop {
            let Some(entry) = read_entry_by_id(&conn, cursor)? else {
                break;
            };
            lineage.push(entry.clone());
            let Some(next) = entry.superseded_by else {
                break;
            };
            cursor = next;
        }
        Ok(lineage)
    }

    /// Record the Git knowledge commit and path of an entry after the text
    /// form was committed to the memory repository (docs/specs/memory.md#spec-10-3). The SQLite
    /// row stays the truth of record; the Git ref is explainable text.
    pub fn mark_memory_git(
        &self,
        entry_id: EntityId,
        git_commit: &str,
        git_path: &str,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                let changed = tx
                    .execute(
                        "UPDATE memory_entries SET git_commit=?2, git_path=?3,
                         updated_at=datetime('now') WHERE entry_id=?1",
                        params![entry_id.to_string(), git_commit, git_path],
                    )
                    .map_err(|e| ShenkoError::storage(e.to_string()))?;
                if changed != 1 {
                    return Err(ShenkoError::not_found("memory entry"));
                }
                tx.commit().map_err(|e| ShenkoError::storage(e.to_string()))?;
                Ok(())
            },
            memory_storage_failure,
        )
    }

    fn query_entries(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<MemoryEntry>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        let rows = stmt
            .query_map(params, |row| Ok(row_to_components(row)))
            .map_err(|e| ShenkoError::storage(e.to_string()))?;

        let mut entries = Vec::new();
        for row in rows {
            let parts = row.map_err(|e| ShenkoError::storage(e.to_string()))?;
            entries.push(parts.into_entry()?);
        }
        Ok(entries)
    }
}

/// Raw column values, converted to domain types outside the rusqlite closure
/// so invalid ids surface as ShenkoError rather than opaque SQL errors.
struct RawEntry {
    entry_id: String,
    agent_id: String,
    kind: String,
    content: String,
    source_ref: Option<String>,
    source: Option<String>,
    applicability: Option<String>,
    confidence: Option<f64>,
    scope: String,
    record_revision: i64,
    created_at_ms: i64,
    updated_commit_seq: Option<i64>,
    deleted: i64,
    superseded_by: Option<String>,
    supersedes: Option<String>,
    git_commit: Option<String>,
    git_path: Option<String>,
}

fn row_to_components(row: &rusqlite::Row<'_>) -> RawEntry {
    RawEntry {
        entry_id: row.get(0).unwrap_or_default(),
        agent_id: row.get(1).unwrap_or_default(),
        kind: row.get(2).unwrap_or_default(),
        content: row.get(3).unwrap_or_default(),
        source_ref: row.get(4).unwrap_or_default(),
        source: row.get(5).unwrap_or_default(),
        applicability: row.get(6).unwrap_or_default(),
        confidence: row.get(7).unwrap_or_default(),
        scope: row.get(8).unwrap_or_default(),
        record_revision: row.get(9).unwrap_or_default(),
        created_at_ms: row.get(10).unwrap_or_default(),
        updated_commit_seq: row.get(11).unwrap_or_default(),
        deleted: row.get(12).unwrap_or_default(),
        superseded_by: row.get(13).unwrap_or_default(),
        supersedes: row.get(14).unwrap_or_default(),
        git_commit: row.get(15).unwrap_or_default(),
        git_path: row.get(16).unwrap_or_default(),
    }
}

impl RawEntry {
    fn into_entry(self) -> Result<MemoryEntry, ShenkoError> {
        let parse_optional = |value: Option<String>, what: &str| {
            value
                .map(|id| id.parse())
                .transpose()
                .map_err(|e| ShenkoError::storage(format!("invalid stored memory {what} id: {e}")))
        };
        Ok(MemoryEntry {
            entry_id: parse_entity(&self.entry_id)?,
            agent_id: parse_entity(&self.agent_id)?,
            kind: self.kind,
            content: self.content,
            source: self.source,
            source_ref: self.source_ref,
            applicability: self.applicability,
            confidence: self.confidence,
            scope: self.scope,
            record_revision: self.record_revision.max(0) as u64,
            created_at_ms: self.created_at_ms,
            updated_commit_seq: self.updated_commit_seq.map(|v| v.max(0) as u64),
            deleted: self.deleted != 0,
            superseded_by: parse_optional(self.superseded_by, "superseded-by")?,
            supersedes: parse_optional(self.supersedes, "supersedes")?,
            git_commit: self.git_commit,
            git_path: self.git_path,
        })
    }
}

fn parse_entity(value: &str) -> Result<EntityId, ShenkoError> {
    value
        .parse()
        .map_err(|_| ShenkoError::storage(format!("invalid stored entity id: {value}")))
}

/// Read one entry inside an open transaction; used by the lineage walk.
fn read_entry_by_id(
    conn: &rusqlite::Connection,
    entry_id: EntityId,
) -> Result<Option<MemoryEntry>, ShenkoError> {
    let sql = format!(
        "SELECT {ENTRY_COLUMNS}
           FROM memory_entries m
          WHERE m.entry_id = ?1"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ShenkoError::storage(e.to_string()))?;
    let rows = stmt
        .query_map(params![entry_id.to_string()], |row| Ok(row_to_components(row)))
        .map_err(|e| ShenkoError::storage(e.to_string()))?;
    let mut entries = Vec::new();
    for row in rows {
        let parts = row.map_err(|e| ShenkoError::storage(e.to_string()))?;
        entries.push(parts.into_entry()?);
    }
    Ok(entries.pop())
}

/// `unicode61` cannot segment Han text, so queries containing CJK code points
/// take the substring-capable LIKE path.
pub(crate) fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c as u32,
            0x3400..=0x4DBF   // CJK Extension A
            | 0x4E00..=0x9FFF // CJK Unified Ideographs
            | 0xF900..=0xFAFF // CJK Compatibility Ideographs
            | 0x20000..=0x2FA1F // CJK Extension B+
        )
    })
}

/// Escape LIKE metacharacters so user text matches literally.
pub(crate) fn like_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("100%_x"), "%100\\%\\_x%");
    }

    #[test]
    fn cjk_detection_covers_han_ranges() {
        assert!(contains_cjk("深刻"));
        assert!(!contains_cjk("shenko memory"));
    }

    /// The FTS MATCH path must actually serve ASCII queries, and reopening a
    /// store must repair an index left stale by a crash (docs/specs/memory.md#spec-12-2 index
    /// repair), rather than silently hiding committed memories forever.
    #[test]
    fn fts_index_serves_ascii_and_repairs_on_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "shenko-fts-repair-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("memory.db");
        let cas = dir.join("cas");
        let agent = EntityId::new();
        let lost = EntityId::new();

        {
            let store = Store::open(db.clone(), cas.clone()).unwrap();
            if !store.capabilities.fts5_available {
                let _ = std::fs::remove_dir_all(&dir);
                return; // index repair only applies to FTS5 builds
            }
            let original = MemoryEntry::new(agent, "quick brown fox");
            store.insert_memory_entry(&original).unwrap();
            assert_eq!(store.search_memory_fts("quick", 5, false).unwrap().len(), 1);

            // Re-saving the same entry_id must reindex: the old text becomes
            // unsearchable and the FTS index stays consistent (upsert fires
            // the update trigger; INSERT OR REPLACE would leave an orphan).
            let mut revised = MemoryEntry::new(agent, "lazy dog sleeps");
            revised.entry_id = original.entry_id;
            revised.record_revision = 1;
            store.insert_memory_entry(&revised).unwrap();
            assert!(store.search_memory_fts("quick", 5, false).unwrap().is_empty());
            assert_eq!(store.search_memory_fts("lazy", 5, false).unwrap().len(), 1);
            {
                let conn = store.ledger.conn.lock();
                conn.execute_batch(
                    "INSERT INTO memory_entries_fts(memory_entries_fts, rank)
                     VALUES('integrity-check', 1)",
                )
                .unwrap();
            }

            // Simulate a crash that left the index stale: drop the insert
            // trigger and append a row behind the index's back.
            {
                let conn = store.ledger.conn.lock();
                conn.execute_batch("DROP TRIGGER memory_entries_ai;")
                    .unwrap();
                conn.execute(
                    "INSERT INTO memory_entries
                        (entry_id, agent_id, kind, content, created_at_ms)
                     VALUES (?1, ?2, 'note', 'lost until reopen', 1)",
                    params![lost.to_string(), agent.to_string()],
                )
                .unwrap();
            }
        }

        let store = Store::open(db, cas).unwrap();
        let hits = store.search_memory("lost", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "reopening should rebuild the stale FTS index"
        );
        assert_eq!(hits[0].entry_id, lost);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
