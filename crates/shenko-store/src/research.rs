//! Research epochs, the structured intervention bank, bounded reminder
//! decisions and retention tombstones — SPEC §14.2–§14.5 (research reset and
//! the active memory worker) with §12.4 (semantic forget / archive_move /
//! erase / restore) and §10.5 (complete object closure, honest reclaim
//! reporting).
//!
//! This is not a second memory store. The searchable personal memory and its
//! open knowledge-kind `String` stay in [`crate::memory`]; what lives here is
//! the *research* state (lineage, epochs, candidates, the structured bank),
//! the decision ledger of the bounded intervention worker, and the monotonic
//! deletion tombstones a forget / archive / erase produces.
//!
//! Three separations are deliberate:
//!
//! * a research **reset** changes what a new worker sees; it never deletes
//!   the frozen epoch, its candidates or its bank entries — the old epoch
//!   stays addressable as an archive for audit and explicit look-back;
//! * a **semantic forget** marks a fact as no longer part of default recall
//!   while its text stays durable; only an **erase** removes content, and it
//!   leaves a tombstone that keeps identity, scope and effect but not the
//!   deleted body;
//! * the **best artifact** of an epoch is validated before a reset may carry
//!   it: the artifact bytes must still be in the content store under the
//!   recorded digest, a score must exist together with the identity of the
//!   verifier that produced it, and the reproducible environment must be
//!   recorded. An unvalidated best is reported as such instead of being
//!   presented as a starting point.
//!
//! Every write here uses the same ledger line as the rest of the store
//! (`with_mutation` → immediate transaction → `commit_log`), so an epoch
//! reset, a bank write and a tombstone are ordinary durable commits.

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::plugin_runtime::ContributionScope;
use shenko_types::subject::SubjectBranchHead;
use shenko_types::task::{TaskOwner, TaskRecord};

use crate::store::Store;

/// Schema of the durable research facts. It is created idempotently by
/// [`ensure_research_schema`]; the aggregate migration stream owns the same
/// statements for a fresh installation.
pub const RESEARCH_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS research_epochs (
    epoch_id TEXT PRIMARY KEY,
    reset_request_id TEXT,
    lineage_id TEXT NOT NULL,
    epoch_no INTEGER NOT NULL,
    task_id TEXT,
    status TEXT NOT NULL,
    constraints_ref TEXT,
    budget_ref TEXT,
    effects_ref TEXT,
    environment_ref TEXT,
    best_candidate_id TEXT,
    brief_ref TEXT,
    reset_from_epoch TEXT,
    reset_reason TEXT,
    reset_receipt TEXT,
    created_at_ms INTEGER NOT NULL,
    frozen_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS research_epochs_lineage ON research_epochs(lineage_id, epoch_no);
CREATE UNIQUE INDEX IF NOT EXISTS research_epochs_reset_request ON research_epochs(reset_request_id)
    WHERE reset_request_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS research_candidates (
    candidate_id TEXT PRIMARY KEY,
    epoch_id TEXT NOT NULL,
    artifact_ref TEXT NOT NULL,
    score REAL,
    score_source TEXT,
    environment_ref TEXT,
    notes TEXT,
    frozen INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS research_candidates_epoch ON research_candidates(epoch_id, created_at_ms);
CREATE TABLE IF NOT EXISTS intervention_bank_entries (
    entry_id TEXT PRIMARY KEY,
    epoch_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    content TEXT NOT NULL,
    source_refs TEXT NOT NULL,
    scope TEXT NOT NULL,
    confidence REAL,
    base_revision INTEGER NOT NULL DEFAULT 0,
    supersedes TEXT,
    superseded_by TEXT,
    carried_from TEXT,
    tombstoned_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS intervention_bank_epoch ON intervention_bank_entries(epoch_id, kind, created_at_ms);
CREATE TABLE IF NOT EXISTS intervention_reminders (
    reminder_id TEXT PRIMARY KEY,
    epoch_id TEXT NOT NULL,
    lineage_id TEXT NOT NULL,
    decision TEXT NOT NULL,
    policy TEXT NOT NULL,
    policy_revision TEXT NOT NULL,
    content TEXT,
    provenance TEXT NOT NULL,
    relevance TEXT,
    scope TEXT NOT NULL,
    expires_at_ms INTEGER,
    transient INTEGER NOT NULL DEFAULT 1,
    bank_entry_ids TEXT NOT NULL,
    reason TEXT,
    created_at_ms INTEGER NOT NULL,
    used_step_id TEXT,
    used_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS intervention_reminders_epoch ON intervention_reminders(epoch_id, created_at_ms);
CREATE TABLE IF NOT EXISTS retention_tombstones (
    tombstone_id TEXT PRIMARY KEY,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    reason TEXT NOT NULL,
    effect_record TEXT NOT NULL,
    deleted_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS retention_tombstones_target ON retention_tombstones(target_kind, target_id);
CREATE TABLE IF NOT EXISTS archive_moves (
    archive_id TEXT PRIMARY KEY,
    source_ref TEXT NOT NULL,
    target_provider TEXT NOT NULL,
    target_namespace TEXT NOT NULL,
    target_locator TEXT NOT NULL,
    manifest_ref TEXT NOT NULL,
    object_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,
    hash_verified INTEGER NOT NULL,
    readback_verified INTEGER NOT NULL,
    local_released INTEGER NOT NULL DEFAULT 0,
    local_release_note TEXT,
    created_at_ms INTEGER NOT NULL
);
"#;

/// Make sure the research tables exist on this database. The DDL is
/// idempotent, so calling it on every entry point is safe and keeps the
/// module usable against a database that was created before these facts
/// existed.
pub fn ensure_research_schema(conn: &Connection) -> Result<(), ShenkoError> {
    conn.execute_batch(RESEARCH_SCHEMA_SQL)
        .map_err(|error| ShenkoError::storage(format!("research schema: {error}")))
}

pub(crate) fn research_storage_failure(error: &ShenkoError) -> bool {
    matches!(
        error,
        ShenkoError::Storage(_) | ShenkoError::Io(_) | ShenkoError::Serde(_)
    )
}

fn encode_object(object: &ObjectRef) -> Result<String, ShenkoError> {
    Ok(serde_json::to_string(object)?)
}

fn decode_object(text: Option<&str>) -> Result<Option<ObjectRef>, ShenkoError> {
    match text {
        None | Some("") => Ok(None),
        Some(value) => Ok(Some(serde_json::from_str(value)?)),
    }
}

fn encode_list<T: Serialize>(values: &[T]) -> Result<String, ShenkoError> {
    Ok(serde_json::to_string(values)?)
}

fn decode_list<T: for<'de> Deserialize<'de>>(text: Option<&str>) -> Result<Vec<T>, ShenkoError> {
    match text {
        None | Some("") => Ok(Vec::new()),
        Some(value) => Ok(serde_json::from_str(value)?),
    }
}

fn parse_id(value: &str) -> Result<EntityId, ShenkoError> {
    value
        .parse()
        .map_err(|error| ShenkoError::storage(format!("invalid research id: {error}")))
}

/// Whether two recorded decisions describe the same decision.
///
/// The creation clock and the expiry are the first writer's, and the
/// consumption receipt is a later projection of the same row, so a replay
/// under one identity keeps the first row instead of conflicting with it.
fn same_decision(existing: &InterventionReminder, incoming: &InterventionReminder) -> bool {
    existing.epoch_id == incoming.epoch_id
        && existing.lineage_id == incoming.lineage_id
        && existing.decision == incoming.decision
        && existing.policy == incoming.policy
        && existing.policy_revision == incoming.policy_revision
        && existing.content == incoming.content
        && existing.provenance == incoming.provenance
        && existing.relevance == incoming.relevance
        && existing.scope == incoming.scope
        && existing.reason == incoming.reason
        && existing.transient == incoming.transient
        && existing.bank_entry_ids == incoming.bank_entry_ids
}

fn optional_id(value: Option<String>) -> Result<Option<EntityId>, ShenkoError> {
    value.as_deref().map(parse_id).transpose()
}

/// One research lineage's epoch. An epoch is the identity of a worker's
/// session and candidate workspace; a reset moves the lineage to the next
/// epoch without touching the identity of the subject or the owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchEpoch {
    pub epoch_id: EntityId,
    /// The idempotency identity of the reset that created this epoch. A
    /// replayed reset returns the same epoch instead of creating a second
    /// one.
    pub reset_request_id: Option<EntityId>,
    pub lineage_id: EntityId,
    pub epoch_no: u64,
    pub task_id: Option<EntityId>,
    pub status: ResearchEpochStatus,
    /// Sealed closure of the still-valid constraints (SPEC §14.2).
    pub constraints_ref: Option<ObjectRef>,
    /// Sealed closure of the budget the worker may spend (SPEC §14.3).
    pub budget_ref: Option<ObjectRef>,
    /// Sealed closure of the external effects that already happened. It is
    /// carried into the next epoch so a reset cannot repeat them.
    pub effects_ref: Option<ObjectRef>,
    /// Sealed record of the reproducible environment (SPEC §14.3).
    pub environment_ref: Option<ObjectRef>,
    pub best_candidate_id: Option<EntityId>,
    /// Sealed worker brief: what a new worker of this epoch may see.
    pub brief_ref: Option<ObjectRef>,
    pub reset_from_epoch: Option<EntityId>,
    pub reset_reason: Option<String>,
    /// JSON validation facts of the reset that created this epoch.
    pub reset_receipt: Option<String>,
    pub created_at_ms: i64,
    pub frozen_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResearchEpochStatus {
    /// The epoch a worker currently runs in.
    Active,
    /// The epoch was closed by a reset. Its candidates and bank stay
    /// addressable as an archive.
    Frozen,
}

/// One candidate artifact of an epoch, with the score that a named verifier
/// produced for it. The score is not an objective optimum: the source is
/// recorded next to it so a later reset can tell a fragile judge from a
/// reproducible check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchCandidate {
    pub candidate_id: EntityId,
    pub epoch_id: EntityId,
    pub artifact_ref: ObjectRef,
    pub score: Option<f64>,
    pub score_source: Option<String>,
    pub environment_ref: Option<ObjectRef>,
    pub notes: Option<String>,
    pub frozen: bool,
    pub created_at_ms: i64,
}

/// The kind of fact a bank entry carries. The kind is an open string in the
/// durable row; the enumeration here is the vocabulary the shipped worker
/// understands, not a closed schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BankEntryKind {
    /// A requirement that still holds for this lineage.
    Requirement,
    /// A fact about the real environment the work runs in.
    EnvironmentFact,
    /// One attempt. Attempts are not carried into a new epoch: they are the
    /// failure narrative a reset removes from the worker's view.
    Attempt,
    /// One diagnosis. Same rule as attempts.
    Diagnosis,
    /// An unfinished goal the lineage still pursues.
    OpenGoal,
}

impl BankEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requirement => "requirement",
            Self::EnvironmentFact => "environment_fact",
            Self::Attempt => "attempt",
            Self::Diagnosis => "diagnosis",
            Self::OpenGoal => "open_goal",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ShenkoError> {
        match value {
            "requirement" => Ok(Self::Requirement),
            "environment_fact" => Ok(Self::EnvironmentFact),
            "attempt" => Ok(Self::Attempt),
            "diagnosis" => Ok(Self::Diagnosis),
            "open_goal" => Ok(Self::OpenGoal),
            other => Err(ShenkoError::conflict(format!(
                "unknown bank entry kind: {other}"
            ))),
        }
    }

    /// Whether a reset carries this kind into the next epoch. Requirements,
    /// environment facts and open goals survive; attempts and diagnoses are
    /// the failure narrative that stays in the frozen archive.
    pub fn survives_reset(self) -> bool {
        matches!(self, Self::Requirement | Self::EnvironmentFact | Self::OpenGoal)
    }
}

/// One entry of the structured bank the memory worker observes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionBankEntry {
    pub entry_id: EntityId,
    pub epoch_id: EntityId,
    pub kind: BankEntryKind,
    pub content: String,
    /// Where the fact came from. A reminder without provenance is not
    /// accepted by the intervention worker.
    pub source_refs: Vec<String>,
    pub scope: String,
    pub confidence: Option<f64>,
    pub base_revision: u64,
    /// The entry this one corrects.
    pub supersedes: Option<EntityId>,
    /// The entry that corrected this one.
    pub superseded_by: Option<EntityId>,
    /// The entry this one was carried from by a reset.
    pub carried_from: Option<EntityId>,
    pub tombstoned_at_ms: Option<i64>,
    pub created_at_ms: i64,
}

/// The bounded worker's decision about one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionDecisionKind {
    /// Nothing worth saying right now.
    Silent,
    /// One reminder with provenance, relevance and scope.
    Remind,
}

/// The durable record of one intervention decision. A silent decision is
/// recorded too: a missed reminder is a fact the policy can be evaluated
/// against (SPEC §14.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionReminder {
    pub reminder_id: EntityId,
    pub epoch_id: EntityId,
    pub lineage_id: EntityId,
    pub decision: InterventionDecisionKind,
    pub policy: String,
    pub policy_revision: String,
    pub content: Option<String>,
    pub provenance: Vec<String>,
    pub relevance: Option<String>,
    pub scope: String,
    pub expires_at_ms: Option<i64>,
    /// A transient reminder leaves the visible context after the request it
    /// was prepared for; the ledger keeps the injection and its provenance.
    pub transient: bool,
    pub bank_entry_ids: Vec<EntityId>,
    pub reason: Option<String>,
    pub created_at_ms: i64,
    pub used_step_id: Option<EntityId>,
    pub used_at_ms: Option<i64>,
}

/// The monotonic tombstone of a deleted fact. It keeps identity, scope and
/// the effect record; it never keeps the body that was deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionTombstone {
    pub tombstone_id: EntityId,
    pub target_kind: String,
    pub target_id: String,
    pub scope: String,
    pub reason: String,
    /// JSON: what was actually deleted and what copies remain reachable.
    pub effect_record: String,
    pub deleted_at_ms: i64,
}

/// One completed archive move: a complete object closure left the local
/// store for an authenticated target, was verified by hash and read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveMoveRecord {
    pub archive_id: EntityId,
    pub source_ref: String,
    pub target_provider: String,
    pub target_namespace: String,
    pub target_locator: String,
    pub manifest_ref: String,
    pub object_count: u64,
    pub total_bytes: u64,
    pub hash_verified: bool,
    pub readback_verified: bool,
    /// Whether the local copy was released. The content store has no object
    /// delete, so a move normally keeps the local copy and says so.
    pub local_released: bool,
    pub local_release_note: Option<String>,
    pub created_at_ms: i64,
}

/// A constraint that still holds for a lineage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchConstraint {
    pub id: String,
    pub statement: String,
    pub source: Option<String>,
}

/// The budget a worker of an epoch may spend. The reset carries it into the
/// next epoch; it never grows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchBudget {
    pub max_tokens: Option<u64>,
    pub max_duration_secs: Option<u64>,
    pub max_cost_units: Option<u64>,
    pub notes: Option<String>,
}

/// One external effect that already happened. A new worker must be able to
/// see it, so it does not repeat a side effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchEffect {
    pub effect_id: String,
    pub description: String,
    pub external: bool,
    pub recorded_at_ms: i64,
}

/// The reproducible environment of an epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchEnvironment {
    pub environment_id: String,
    pub facts: Vec<String>,
    pub toolchain: Vec<String>,
    pub dataset_versions: Vec<String>,
    /// Whether this environment can be reconstructed from the record alone.
    pub reproducible: bool,
}

/// The closure a lineage keeps across epochs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchClosure {
    pub constraints: Vec<ResearchConstraint>,
    pub budget: ResearchBudget,
    pub effects: Vec<ResearchEffect>,
    pub environment: ResearchEnvironment,
}

/// Why a reset happened. The thresholds behind these signals are evaluated
/// by the caller; the enum only records which signal fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResearchResetTrigger {
    Stagnation,
    IsomorphicProposal,
    EvidenceDirectionChanged,
    PlannedRound,
    SubjectRequest,
}

impl ResearchResetTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stagnation => "stagnation",
            Self::IsomorphicProposal => "isomorphic_proposal",
            Self::EvidenceDirectionChanged => "evidence_direction_changed",
            Self::PlannedRound => "planned_round",
            Self::SubjectRequest => "subject_request",
        }
    }
}

/// The validation of the candidate a reset wants to carry forward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BestCandidateValidation {
    pub candidate_id: Option<EntityId>,
    pub artifact_present: bool,
    pub artifact_digest_matches: bool,
    pub score_present: bool,
    pub score_source: Option<String>,
    pub environment_recorded: bool,
    pub valid: bool,
    pub problems: Vec<String>,
}

/// One candidate carried into a new epoch. The whole frozen set is carried,
/// not only the best one: a reset must not collapse a diverse candidate set
/// into one fragile judge result (SPEC §14.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarriedCandidate {
    pub candidate_id: EntityId,
    pub artifact_ref: ObjectRef,
    pub score: Option<f64>,
    pub score_source: Option<String>,
    pub environment_ref: Option<ObjectRef>,
}

/// One bank entry carried into a new epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarriedBankEntry {
    pub entry_id: EntityId,
    pub carried_from: EntityId,
    pub kind: BankEntryKind,
    pub content: String,
    pub source_refs: Vec<String>,
    pub scope: String,
    pub confidence: Option<f64>,
}

/// What a worker of an epoch is allowed to see. It holds the still-valid
/// constraints, the environment facts, the reproducible best artifact, the
/// budget, the effects that already happened and the open goals. It does not
/// hold the previous epoch's failure narrative: those entries stay in the
/// frozen archive and are only reachable by an explicit look-back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchWorkerBrief {
    pub schema_version: u32,
    pub epoch_id: EntityId,
    pub lineage_id: EntityId,
    pub epoch_no: u64,
    pub reset_trigger: ResearchResetTrigger,
    pub reset_reason: String,
    pub constraints: Vec<ResearchConstraint>,
    pub budget: ResearchBudget,
    pub effects: Vec<ResearchEffect>,
    pub environment: ResearchEnvironment,
    pub best: Option<CarriedCandidate>,
    pub carried_candidates: Vec<CarriedCandidate>,
    pub carried_bank_entries: Vec<CarriedBankEntry>,
    pub open_goals: Vec<String>,
    /// How many attempt/diagnosis entries of the previous epoch stayed in the
    /// archive instead of being projected into this worker.
    pub excluded_failure_entries: usize,
    /// The frozen epoch id an explicit audit or look-back can address.
    pub archive_epoch_id: Option<EntityId>,
}

/// The result of one reset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchResetOutcome {
    pub previous_epoch: ResearchEpoch,
    pub epoch: ResearchEpoch,
    pub brief: ResearchWorkerBrief,
    pub validation: BestCandidateValidation,
    pub carried_candidates: Vec<CarriedCandidate>,
    pub carried_bank_entry_ids: Vec<EntityId>,
    pub excluded_failure_entries: usize,
}

/// Which bank entries a reader may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BankSelection {
    /// Everything of the epoch that is not tombstoned.
    All,
    /// Only non-tombstoned entries of the kinds a fresh worker inherits.
    Carried,
    /// Only the failure narrative, including tombstoned rows for an explicit audit.
    Failures,
}

impl Store {
    /// Open a research lineage with its first epoch. The closure is sealed
    /// into the content store first, so the epoch row never points at an
    /// object that does not exist yet.
    pub fn open_research_lineage(
        &self,
        request_id: EntityId,
        lineage_id: EntityId,
        task_id: Option<EntityId>,
        closure: &ResearchClosure,
    ) -> Result<ResearchEpoch, ShenkoError> {
        // Idempotent on the opening request: a replayed open returns the
        // epoch the original call created instead of a second lineage.
        if let Some(existing) = self.epoch_by_reset_request(request_id)? {
            return Ok(existing);
        }
        let sealed = self.seal_closure(closure)?;
        let epoch = ResearchEpoch {
            epoch_id: EntityId::new(),
            reset_request_id: Some(request_id),
            lineage_id,
            epoch_no: 1,
            task_id,
            status: ResearchEpochStatus::Active,
            constraints_ref: Some(sealed.constraints),
            budget_ref: Some(sealed.budget),
            effects_ref: Some(sealed.effects),
            environment_ref: Some(sealed.environment),
            best_candidate_id: None,
            brief_ref: None,
            reset_from_epoch: None,
            reset_reason: None,
            reset_receipt: None,
            created_at_ms: Utc::now().timestamp_millis(),
            frozen_at_ms: None,
        };
        let brief = self.seal_worker_brief(&ResearchWorkerBrief {
            schema_version: 1,
            epoch_id: epoch.epoch_id,
            lineage_id,
            epoch_no: 1,
            reset_trigger: ResearchResetTrigger::PlannedRound,
            reset_reason: "research lineage opened".to_string(),
            constraints: closure.constraints.clone(),
            budget: closure.budget.clone(),
            effects: closure.effects.clone(),
            environment: closure.environment.clone(),
            best: None,
            carried_candidates: Vec::new(),
            carried_bank_entries: Vec::new(),
            open_goals: Vec::new(),
            excluded_failure_entries: 0,
            archive_epoch_id: None,
        })?;
        let mut epoch = epoch;
        epoch.brief_ref = Some(brief);
        let receipt = self.ledger.with_mutation(
            || {
                self.try_insert_epoch(&epoch, &[], &[])?;
                Ok(())
            },
            research_storage_failure,
        )?;
        let _ = receipt;
        Ok(epoch)
    }

    fn seal_closure(&self, closure: &ResearchClosure) -> Result<SealedClosure, ShenkoError> {
        let constraints = self.seal_json(
            &closure.constraints,
            "research-closures",
            "research-constraints",
        )?;
        let budget = self.seal_json(&closure.budget, "research-closures", "research-budget")?;
        let effects = self.seal_json(&closure.effects, "research-closures", "research-effects")?;
        let environment = self.seal_json(
            &closure.environment,
            "research-closures",
            "research-environment",
        )?;
        Ok(SealedClosure { constraints, budget, effects, environment })
    }

    fn seal_json<T: Serialize + ?Sized>(
        &self,
        value: &T,
        namespace: &str,
        codec: &str,
    ) -> Result<ObjectRef, ShenkoError> {
        let bytes = serde_json::to_vec(value)?;
        match self.cas.seal_object(&bytes, "application/json", namespace, codec, 1) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.close_external_writer();
                Err(error)
            }
        }
    }

    fn seal_worker_brief(&self, brief: &ResearchWorkerBrief) -> Result<ObjectRef, ShenkoError> {
        self.seal_json(brief, "research-closures", "research-worker-brief")
    }

    /// The active epoch of a lineage, if the lineage has one.
    pub fn active_research_epoch(
        &self,
        lineage_id: EntityId,
    ) -> Result<Option<ResearchEpoch>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        read_epoch_by(
            &conn,
            "WHERE lineage_id=?1 AND status='active' \
             ORDER BY epoch_no DESC LIMIT 1",
            params![lineage_id.to_string()],
        )
    }

    /// One epoch by identity.
    pub fn research_epoch(&self, epoch_id: EntityId) -> Result<Option<ResearchEpoch>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        read_epoch_by(
            &conn,
            "WHERE epoch_id=?1",
            params![epoch_id.to_string()],
        )
    }

    /// The epoch a reset with this request id created, if one exists.
    fn epoch_by_reset_request(
        &self,
        request_id: EntityId,
    ) -> Result<Option<ResearchEpoch>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        read_epoch_by(
            &conn,
            "WHERE reset_request_id=?1",
            params![request_id.to_string()],
        )
    }

    /// Every epoch of a lineage, oldest first. A frozen epoch is the archive
    /// a reset leaves behind.
    pub fn research_lineage(
        &self,
        lineage_id: EntityId,
    ) -> Result<Vec<ResearchEpoch>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT epoch_id FROM research_epochs WHERE lineage_id=?1 ORDER BY epoch_no ASC",
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let ids = statement
            .query_map(params![lineage_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(|error| ShenkoError::storage(error.to_string()))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        drop(statement);
        let mut epochs = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(epoch) = read_epoch_by(
                &conn,
                "WHERE epoch_id=?1",
                params![id],
            )? {
                epochs.push(epoch);
            }
        }
        Ok(epochs)
    }

    /// Record a candidate artifact of an epoch.
    pub fn add_research_candidate(
        &self,
        candidate: &ResearchCandidate,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || self.try_add_candidate(candidate),
            research_storage_failure,
        )
    }

    fn try_add_candidate(&self, candidate: &ResearchCandidate) -> Result<(), ShenkoError> {
        let candidate_id = candidate.candidate_id.to_string();
        let epoch_id = candidate.epoch_id.to_string();
        let artifact_ref = encode_object(&candidate.artifact_ref)?;
        let environment_ref = candidate.environment_ref.as_ref().map(encode_object).transpose()?;
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        ensure_research_schema(&tx)?;
        // Identity covers the evidence, not the first-observed timestamp or
        // the frozen state that a later reset projects onto this row.
        let existing: Option<(bool, Option<f64>)> = tx
            .query_row(
                "SELECT epoch_id=?2 AND artifact_ref=?3 AND score_source IS ?4
                        AND environment_ref IS ?5 AND notes IS ?6, score
                 FROM research_candidates WHERE candidate_id=?1",
                params![candidate_id, epoch_id, artifact_ref, candidate.score_source,
                    environment_ref, candidate.notes],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        if let Some((same_evidence, score)) = existing {
            return if same_evidence && score == candidate.score {
                Ok(())
            } else {
                Err(ShenkoError::conflict(
                    "research candidate identity exists with different content",
                ))
            };
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT INTO research_candidates
             (candidate_id,epoch_id,artifact_ref,score,score_source,environment_ref,notes,frozen,created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                candidate_id,
                epoch_id,
                artifact_ref,
                candidate.score,
                candidate.score_source,
                environment_ref,
                candidate.notes,
                candidate.frozen as i64,
                candidate.created_at_ms,
            ],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        self.write_commit_log(&tx, seq)?;
        tx.commit()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        Ok(())
    }

    /// The candidates of an epoch, oldest first.
    pub fn research_candidates(
        &self,
        epoch_id: EntityId,
    ) -> Result<Vec<ResearchCandidate>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT candidate_id,epoch_id,artifact_ref,score,score_source,environment_ref,notes,frozen,created_at_ms
                 FROM research_candidates WHERE epoch_id=?1 ORDER BY created_at_ms ASC, candidate_id ASC",
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let rows = statement
            .query_map(params![epoch_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<f64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let mut candidates = Vec::new();
        for row in rows {
            let (
                candidate_id,
                epoch_id,
                artifact_ref,
                score,
                score_source,
                environment_ref,
                notes,
                frozen,
                created_at_ms,
            ) = row.map_err(|error| ShenkoError::storage(error.to_string()))?;
            candidates.push(ResearchCandidate {
                candidate_id: parse_id(&candidate_id)?,
                epoch_id: parse_id(&epoch_id)?,
                artifact_ref: serde_json::from_str(&artifact_ref)?,
                score,
                score_source,
                environment_ref: decode_object(environment_ref.as_deref())?,
                notes,
                frozen: frozen != 0,
                created_at_ms,
            });
        }
        Ok(candidates)
    }

    fn epoch_candidate_pool(
        &self,
        epoch: &ResearchEpoch,
    ) -> Result<Vec<CarriedCandidate>, ShenkoError> {
        let read_brief = |source: &ResearchEpoch| -> Result<ResearchWorkerBrief, ShenkoError> {
            let reference = source.brief_ref.as_ref()
                .ok_or_else(|| ShenkoError::storage("research epoch lacks its sealed brief"))?;
            let bytes = self.cas.read_object(&reference.digest)?;
            if bytes.len() as u64 != reference.bytes || Digest::blake3(&bytes) != reference.digest {
                return Err(ShenkoError::storage("research worker brief does not match its sealed digest"));
            }
            let brief: ResearchWorkerBrief = serde_json::from_slice(&bytes)?;
            if brief.schema_version != 1 || brief.epoch_id != source.epoch_id
                || brief.lineage_id != source.lineage_id || brief.epoch_no != source.epoch_no
                || brief.archive_epoch_id != source.reset_from_epoch
            {
                return Err(ShenkoError::storage("research worker brief does not match its epoch"));
            }
            Ok(brief)
        };
        let brief = read_brief(epoch)?;
        let own = self.research_candidates(epoch.epoch_id)?;
        let mut pool = brief.carried_candidates;
        let mut positions = std::collections::HashMap::with_capacity(pool.len() + own.len() + 1);
        for (index, candidate) in pool.iter().enumerate() {
            if positions.insert(candidate.candidate_id, (index, epoch.epoch_id)).is_some() {
                return Err(ShenkoError::storage("research worker brief repeats a candidate identity"));
            }
        }
        // The last snapshot identity detects repetition within one carried set
        // without allocating a separate identity set for every ancestor.
        let mut merge = |candidate: CarriedCandidate, source: EntityId, reject_repeat: bool| {
            if let Some((index, last_source)) = positions.get_mut(&candidate.candidate_id) {
                if reject_repeat && *last_source == source {
                    return Err(ShenkoError::storage("research worker brief repeats a candidate identity"));
                }
                if pool[*index] != candidate {
                    return Err(ShenkoError::storage("research candidate identity has conflicting inherited evidence"));
                }
                *last_source = source;
            } else {
                positions.insert(candidate.candidate_id, (pool.len(), source));
                pool.push(candidate);
            }
            Ok(())
        };
        // Inheritance is a sealed projection, not a second candidate row.
        for candidate in brief.best.into_iter().chain(own.into_iter().map(|candidate| CarriedCandidate {
            candidate_id: candidate.candidate_id,
            artifact_ref: candidate.artifact_ref,
            score: candidate.score,
            score_source: candidate.score_source,
            environment_ref: candidate.environment_ref,
        })) {
            merge(candidate, epoch.epoch_id, false)?;
        }
        // Older resets could omit inherited candidates. Recover only their
        // sealed ancestry, never mutable historical candidate or bank rows.
        let mut ancestor: Option<ResearchEpoch> = None;
        loop {
            let current = ancestor.as_ref().unwrap_or(epoch);
            let Some(previous_id) = current.reset_from_epoch else {
                if current.epoch_no != 1 {
                    return Err(ShenkoError::storage("research epoch ancestry ends before its root"));
                }
                break;
            };
            let previous = self.research_epoch(previous_id)?
                .ok_or_else(|| ShenkoError::storage("research epoch ancestor is not retained"))?;
            if previous.lineage_id != epoch.lineage_id
                || previous.epoch_no.checked_add(1) != Some(current.epoch_no)
            {
                return Err(ShenkoError::storage("research epoch ancestry does not match its lineage and sequence"));
            }
            let brief = read_brief(&previous)?;
            for candidate in brief.carried_candidates {
                merge(candidate, previous.epoch_id, true)?;
            }
            if let Some(best) = brief.best {
                merge(best, previous.epoch_id, false)?;
            }
            ancestor = Some(previous);
        }
        Ok(pool)
    }

    /// Reset a lineage to its next epoch.
    ///
    /// The candidates of the running epoch are frozen, the best candidate is
    /// validated against the content store, the still-valid bank entries are
    /// carried over and a new epoch with its own worker brief is created.
    /// The previous epoch stays readable: the reset changes what a new worker
    /// sees, it does not delete the evidence of the old attempts.
    ///
    /// The reset is idempotent on `request_id`: a replayed reset returns the
    /// epoch the original call created, instead of creating a second one.
    pub fn reset_research_epoch(
        &self,
        request_id: EntityId,
        lineage_id: EntityId,
        trigger: ResearchResetTrigger,
        reason: &str,
        closure: &ResearchClosure,
        best_candidate_id: Option<EntityId>,
    ) -> Result<ResearchResetOutcome, ShenkoError> {
        if let Some(existing) = self.epoch_by_reset_request(request_id)? {
            let brief = self
                .research_worker_brief(existing.epoch_id)?
                .ok_or_else(|| ShenkoError::storage("reset epoch lacks its sealed brief"))?;
            let validation: BestCandidateValidation = existing
                .reset_receipt
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or(BestCandidateValidation {
                    candidate_id: None,
                    artifact_present: false,
                    artifact_digest_matches: false,
                    score_present: false,
                    score_source: None,
                    environment_recorded: false,
                    valid: false,
                    problems: vec!["the original reset carried no validation receipt".to_string()],
                });
            let carried_candidates = brief.carried_candidates.clone();
            let carried_bank_entry_ids: Vec<EntityId> = brief.carried_bank_entries
                .iter()
                .map(|entry| entry.entry_id)
                .collect();
            let previous_id = existing.reset_from_epoch
                .ok_or_else(|| ShenkoError::storage("reset epoch lacks its origin"))?;
            let previous_epoch = self.research_epoch(previous_id)?
                .ok_or_else(|| ShenkoError::storage("reset epoch origin is not retained"))?;
            let excluded_failure_entries = brief.excluded_failure_entries;
            return Ok(ResearchResetOutcome {
                previous_epoch,
                epoch: existing.clone(),
                brief,
                validation,
                carried_candidates,
                carried_bank_entry_ids,
                excluded_failure_entries,
            });
        }
        let current = self
            .active_research_epoch(lineage_id)?
            .ok_or_else(|| ShenkoError::not_found("active research epoch"))?;
        let carried_candidates = self.epoch_candidate_pool(&current)?;
        let validation = self.validate_best_candidate(&current, &carried_candidates, best_candidate_id)?;
        if best_candidate_id.is_some() && !validation.valid {
            return Err(ShenkoError::conflict(format!(
                "research best candidate is not validated: {}",
                validation.problems.join("; ")
            )));
        }
        let best = if validation.valid {
            validation.candidate_id
                .and_then(|id| carried_candidates.iter().find(|item| item.candidate_id == id))
                .cloned()
        } else {
            None
        };
        let validated_best_id = best.as_ref().map(|candidate| candidate.candidate_id);

        // The bank of the running epoch: still-valid facts travel, the
        // failure narrative stays in the frozen archive.
        let excluded_failure_entries = {
            let conn = self.ledger.conn.lock();
            ensure_research_schema(&conn)?;
            conn.query_row(
                "SELECT COUNT(*) FROM intervention_bank_entries WHERE epoch_id=?1 AND kind IN (?2,?3)",
                params![current.epoch_id.to_string(), BankEntryKind::Attempt.as_str(), BankEntryKind::Diagnosis.as_str()],
                |row| row.get::<_, usize>(0),
            ).map_err(|error| ShenkoError::storage(error.to_string()))?
        };
        let carried_entries = self.bank_entries(current.epoch_id, BankSelection::Carried)?;
        let open_goals: Vec<String> = carried_entries
            .iter()
            .filter(|entry| entry.kind == BankEntryKind::OpenGoal)
            .map(|entry| entry.content.clone())
            .collect();
        let carried_bank_entries: Vec<CarriedBankEntry> = carried_entries
            .into_iter()
            .map(|entry| CarriedBankEntry {
                entry_id: EntityId::new(),
                carried_from: entry.entry_id,
                kind: entry.kind,
                content: entry.content,
                source_refs: entry.source_refs,
                scope: entry.scope,
                confidence: entry.confidence,
            })
            .collect();

        let sealed = self.seal_closure(closure)?;
        let epoch_id = EntityId::new();
        let epoch_no = current.epoch_no + 1;
        let brief = ResearchWorkerBrief {
            schema_version: 1,
            epoch_id,
            lineage_id,
            epoch_no,
            reset_trigger: trigger,
            reset_reason: reason.to_string(),
            constraints: closure.constraints.clone(),
            budget: closure.budget.clone(),
            effects: closure.effects.clone(),
            environment: closure.environment.clone(),
            best,
            carried_candidates: carried_candidates.clone(),
            carried_bank_entries: carried_bank_entries.clone(),
            open_goals,
            excluded_failure_entries,
            archive_epoch_id: Some(current.epoch_id),
        };
        let brief_ref = self.seal_worker_brief(&brief)?;
        let receipt = serde_json::to_string(&validation)?;
        let epoch = ResearchEpoch {
            epoch_id,
            reset_request_id: Some(request_id),
            lineage_id,
            epoch_no,
            task_id: current.task_id,
            status: ResearchEpochStatus::Active,
            constraints_ref: Some(sealed.constraints),
            budget_ref: Some(sealed.budget),
            effects_ref: Some(sealed.effects),
            environment_ref: Some(sealed.environment),
            best_candidate_id: validated_best_id,
            brief_ref: Some(brief_ref),
            reset_from_epoch: Some(current.epoch_id),
            reset_reason: Some(reason.to_string()),
            reset_receipt: Some(receipt),
            created_at_ms: Utc::now().timestamp_millis(),
            frozen_at_ms: None,
        };
        self.ledger.with_mutation(
            || {
                self.try_insert_epoch(&epoch, &carried_bank_entries, &[current.epoch_id])?;
                Ok(())
            },
            research_storage_failure,
        )?;
        Ok(ResearchResetOutcome {
            previous_epoch: current,
            epoch,
            brief,
            validation,
            carried_candidates,
            carried_bank_entry_ids: carried_bank_entries
                .iter()
                .map(|entry| entry.entry_id)
                .collect(),
            excluded_failure_entries,
        })
    }

    fn try_insert_epoch(
        &self,
        epoch: &ResearchEpoch,
        carried: &[CarriedBankEntry],
        frozen_epochs: &[EntityId],
    ) -> Result<(), ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        ensure_research_schema(&tx)?;
        let seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT INTO research_epochs
             (epoch_id,reset_request_id,lineage_id,epoch_no,task_id,status,constraints_ref,budget_ref,effects_ref,
              environment_ref,best_candidate_id,brief_ref,reset_from_epoch,reset_reason,reset_receipt,
              created_at_ms,frozen_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                epoch.epoch_id.to_string(),
                epoch.reset_request_id.map(|id| id.to_string()),
                epoch.lineage_id.to_string(),
                epoch.epoch_no as i64,
                epoch.task_id.map(|id| id.to_string()),
                match epoch.status {
                    ResearchEpochStatus::Active => "active",
                    ResearchEpochStatus::Frozen => "frozen",
                },
                epoch.constraints_ref.as_ref().map(encode_object).transpose()?,
                epoch.budget_ref.as_ref().map(encode_object).transpose()?,
                epoch.effects_ref.as_ref().map(encode_object).transpose()?,
                epoch.environment_ref.as_ref().map(encode_object).transpose()?,
                epoch.best_candidate_id.map(|id| id.to_string()),
                epoch.brief_ref.as_ref().map(encode_object).transpose()?,
                epoch.reset_from_epoch.map(|id| id.to_string()),
                epoch.reset_reason,
                epoch.reset_receipt,
                epoch.created_at_ms,
                epoch.frozen_at_ms,
            ],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        for entry in carried {
            tx.execute(
                "INSERT INTO intervention_bank_entries
                 (entry_id,epoch_id,kind,content,source_refs,scope,confidence,base_revision,
                  supersedes,superseded_by,carried_from,tombstoned_at_ms,created_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,NULL,NULL,?9,NULL,?10)",
                params![
                    entry.entry_id.to_string(),
                    epoch.epoch_id.to_string(),
                    entry.kind.as_str(),
                    entry.content,
                    encode_list(&entry.source_refs)?,
                    entry.scope,
                    entry.confidence,
                    0i64,
                    entry.carried_from.to_string(),
                    Utc::now().timestamp_millis(),
                ],
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        }
        // Freeze the candidates of the epoch that just ended. The rows stay
        // addressable: a reset never deletes the evidence of old attempts.
        tx.execute(
            "UPDATE research_candidates SET frozen=1 WHERE epoch_id IN (SELECT value FROM json_each(?1))",
            params![serde_json::to_string(
                &frozen_epochs.iter().map(|id| id.to_string()).collect::<Vec<String>>()
            )?],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        for previous in frozen_epochs {
            tx.execute(
                "UPDATE research_epochs SET status='frozen', frozen_at_ms=?2 WHERE epoch_id=?1 AND status='active'",
                params![previous.to_string(), Utc::now().timestamp_millis()],
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        }
        self.write_commit_log(&tx, seq)?;
        tx.commit()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        Ok(())
    }

    /// Validate the candidate a reset wants to carry forward. The artifact
    /// must still be in the content store under the recorded digest, a score
    /// must exist together with the verifier that produced it, and the
    /// reproducible environment must be recorded.
    fn validate_best_candidate(
        &self,
        epoch: &ResearchEpoch,
        candidates: &[CarriedCandidate],
        requested: Option<EntityId>,
    ) -> Result<BestCandidateValidation, ShenkoError> {
        let selected = requested.or(epoch.best_candidate_id);
        let Some(candidate_id) = selected else {
            return Ok(BestCandidateValidation {
                candidate_id: None,
                artifact_present: false,
                artifact_digest_matches: false,
                score_present: false,
                score_source: None,
                environment_recorded: epoch.environment_ref.is_some(),
                valid: false,
                problems: vec!["no best candidate is recorded for this epoch".to_string()],
            });
        };
        let Some(candidate) = candidates.iter().find(|item| item.candidate_id == candidate_id)
        else {
            return Ok(BestCandidateValidation {
                candidate_id: Some(candidate_id),
                artifact_present: false,
                artifact_digest_matches: false,
                score_present: false,
                score_source: None,
                environment_recorded: epoch.environment_ref.is_some(),
                valid: false,
                problems: vec!["the named best candidate is not part of this epoch".to_string()],
            });
        };
        let mut problems = Vec::new();
        let artifact_present = self.cas.object_exists(&candidate.artifact_ref.digest);
        if !artifact_present {
            problems.push("the best artifact is no longer in the content store".to_string());
        }
        let artifact_digest_matches = if artifact_present {
            match self.cas.read_object(&candidate.artifact_ref.digest) {
                Ok(bytes) => {
                    let matches = Digest::blake3(&bytes) == candidate.artifact_ref.digest
                        && bytes.len() as u64 == candidate.artifact_ref.bytes;
                    if !matches {
                        problems.push(
                            "the best artifact bytes do not match the recorded digest".to_string(),
                        );
                    }
                    matches
                }
                Err(error) => {
                    problems.push(format!("the best artifact cannot be read: {error}"));
                    false
                }
            }
        } else {
            false
        };
        let score_present = candidate.score.is_some();
        if !score_present {
            problems.push("the best candidate has no recorded score".to_string());
        }
        let score_source = candidate.score_source.clone();
        if score_source.as_deref().map(str::trim).unwrap_or("").is_empty() {
            problems.push("the best candidate has no recorded score source".to_string());
        }
        let environment_recorded = candidate.environment_ref.is_some() || epoch.environment_ref.is_some();
        if !environment_recorded {
            problems.push("the reproducible environment of the best candidate is not recorded".to_string());
        }
        let valid = problems.is_empty();
        Ok(BestCandidateValidation {
            candidate_id: Some(candidate_id),
            artifact_present,
            artifact_digest_matches,
            score_present,
            score_source,
            environment_recorded,
            valid,
            problems,
        })
    }

    /// Read the sealed worker brief of an epoch.
    pub fn research_worker_brief(
        &self,
        epoch_id: EntityId,
    ) -> Result<Option<ResearchWorkerBrief>, ShenkoError> {
        let Some(epoch) = self.research_epoch(epoch_id)? else {
            return Ok(None);
        };
        let Some(brief_ref) = epoch.brief_ref else {
            return Ok(None);
        };
        let bytes = self.cas.read_object(&brief_ref.digest)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Write one entry of the structured bank.
    pub fn put_bank_entry(
        &self,
        entry: &InterventionBankEntry,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || self.try_put_bank_entry(entry),
            research_storage_failure,
        )
    }

    fn try_put_bank_entry(&self, entry: &InterventionBankEntry) -> Result<(), ShenkoError> {
        let entry_id = entry.entry_id.to_string();
        let epoch_id = entry.epoch_id.to_string();
        let source_refs = encode_list(&entry.source_refs)?;
        let carried_from = entry.carried_from.map(|id| id.to_string());
        let supersedes = entry.supersedes.map(|id| id.to_string());
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        ensure_research_schema(&tx)?;
        // Compare the original evidence while preserving later lifecycle
        // projections. An explicit correction target must already match;
        // an original write without one may replay after a later link.
        let existing: Option<(bool, Option<f64>)> = tx
            .query_row(
                "SELECT epoch_id=?2 AND kind=?3 AND content=?4 AND source_refs=?5
                        AND scope=?6 AND base_revision=?7 AND carried_from IS ?8
                        AND (?9 IS NULL OR supersedes IS ?9), confidence
                 FROM intervention_bank_entries WHERE entry_id=?1",
                params![entry_id, epoch_id, entry.kind.as_str(), entry.content, source_refs,
                    entry.scope, entry.base_revision as i64, carried_from, supersedes],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        if let Some((same_evidence, confidence)) = existing {
            return if same_evidence && confidence == entry.confidence {
                Ok(())
            } else {
                Err(ShenkoError::conflict(
                    "bank entry identity exists with different content",
                ))
            };
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT INTO intervention_bank_entries
             (entry_id,epoch_id,kind,content,source_refs,scope,confidence,base_revision,
              supersedes,superseded_by,carried_from,tombstoned_at_ms,created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                entry_id,
                epoch_id,
                entry.kind.as_str(),
                entry.content,
                source_refs,
                entry.scope,
                entry.confidence,
                entry.base_revision as i64,
                supersedes,
                entry.superseded_by.map(|id| id.to_string()),
                carried_from,
                entry.tombstoned_at_ms,
                entry.created_at_ms,
            ],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        if let Some(superseded) = entry.supersedes {
            tx.execute(
                "UPDATE intervention_bank_entries SET superseded_by=?2 WHERE entry_id=?1",
                params![superseded.to_string(), entry.entry_id.to_string()],
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        }
        self.write_commit_log(&tx, seq)?;
        tx.commit()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        Ok(())
    }

    /// The bank entries of an epoch.
    pub fn bank_entries(
        &self,
        epoch_id: EntityId,
        selection: BankSelection,
    ) -> Result<Vec<InterventionBankEntry>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT entry_id,epoch_id,kind,content,source_refs,scope,confidence,base_revision,
                        supersedes,superseded_by,carried_from,tombstoned_at_ms,created_at_ms
                 FROM intervention_bank_entries WHERE epoch_id=?1 AND (?2 OR tombstoned_at_ms IS NULL)
                 ORDER BY created_at_ms ASC, entry_id ASC",
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let mut rows = statement
            .query(params![epoch_id.to_string(), selection == BankSelection::Failures])
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next().map_err(|error| ShenkoError::storage(error.to_string()))? {
            let kind = Self::bank_entry_kind(row)?;
            let visible = match selection {
                BankSelection::All => true,
                BankSelection::Carried => kind.survives_reset(),
                BankSelection::Failures => !kind.survives_reset(),
            };
            if visible {
                entries.push(Self::bank_entry_from_row(row, kind)?);
            }
        }
        Ok(entries)
    }

    /// Read one retained bank entry by identity, including tombstoned rows.
    /// This explicit audit path does not make the entry eligible for injection.
    pub fn bank_entry(&self, entry_id: EntityId) -> Result<Option<InterventionBankEntry>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn.prepare(
            "SELECT entry_id,epoch_id,kind,content,source_refs,scope,confidence,base_revision,
                    supersedes,superseded_by,carried_from,tombstoned_at_ms,created_at_ms
             FROM intervention_bank_entries WHERE entry_id=?1",
        ).map_err(|error| ShenkoError::storage(error.to_string()))?;
        let mut rows = statement.query(params![entry_id.to_string()])
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        rows.next().map_err(|error| ShenkoError::storage(error.to_string()))?
            .map(|row| Self::bank_entry_from_row(row, Self::bank_entry_kind(row)?))
            .transpose()
    }

    fn bank_entry_kind(row: &rusqlite::Row<'_>) -> Result<BankEntryKind, ShenkoError> {
        BankEntryKind::parse(row.get_ref(2)
            .map_err(|error| ShenkoError::storage(error.to_string()))?
            .as_str().map_err(|error| ShenkoError::storage(error.to_string()))?)
    }

    fn bank_entry_from_row(
        row: &rusqlite::Row<'_>,
        kind: BankEntryKind,
    ) -> Result<InterventionBankEntry, ShenkoError> {
        let fields = (|| -> rusqlite::Result<_> {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<f64>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, i64>(12)?,
            ))
        })().map_err(|error| ShenkoError::storage(error.to_string()))?;
        let (entry_id, epoch_id, content, source_refs, scope, confidence, base_revision,
            supersedes, superseded_by, carried_from, tombstoned_at_ms, created_at_ms) = fields;
        Ok(InterventionBankEntry {
            entry_id: parse_id(&entry_id)?, epoch_id: parse_id(&epoch_id)?, kind, content,
            source_refs: decode_list::<String>(Some(&source_refs))?, scope, confidence,
            base_revision: base_revision.max(0) as u64,
            supersedes: optional_id(supersedes)?, superseded_by: optional_id(superseded_by)?,
            carried_from: optional_id(carried_from)?, tombstoned_at_ms, created_at_ms,
        })
    }

    /// Link two retained bank entries as a correction. Both bodies remain
    /// readable, and the relation is committed atomically (SPEC §12.2).
    pub fn supersede_bank_entry(
        &self,
        superseded: EntityId,
        replacement: EntityId,
    ) -> Result<(), ShenkoError> {
        if superseded == replacement {
            return Err(ShenkoError::conflict("a bank entry cannot supersede itself"));
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(crate::store::storage_error)?;
                ensure_research_schema(&tx)?;
                let old_id = superseded.to_string();
                let new_id = replacement.to_string();
                let old_link: Option<Option<String>> = tx.query_row(
                    "SELECT superseded_by FROM intervention_bank_entries WHERE entry_id=?1",
                    [&old_id], |row| row.get(0),
                ).optional().map_err(crate::store::storage_error)?;
                let new_link: Option<Option<String>> = tx.query_row(
                    "SELECT supersedes FROM intervention_bank_entries WHERE entry_id=?1",
                    [&new_id], |row| row.get(0),
                ).optional().map_err(crate::store::storage_error)?;
                let old_link = old_link.ok_or_else(|| ShenkoError::not_found("superseded bank entry"))?;
                let new_link = new_link.ok_or_else(|| ShenkoError::not_found("replacement bank entry"))?;
                if old_link.as_deref() == Some(new_id.as_str()) && new_link.as_deref() == Some(old_id.as_str()) {
                    return Ok(());
                }
                if old_link.as_deref().is_some_and(|id| id != new_id.as_str())
                    || new_link.as_deref().is_some_and(|id| id != old_id.as_str())
                {
                    return Err(ShenkoError::conflict("bank supersession relation already names another entry"));
                }
                let closes_cycle: bool = tx.query_row(
                    "WITH RECURSIVE successors(entry_id) AS (
                        SELECT ?1 UNION
                        SELECT bank.superseded_by FROM intervention_bank_entries bank
                        JOIN successors ON bank.entry_id=successors.entry_id
                        WHERE bank.superseded_by IS NOT NULL
                     ) SELECT EXISTS(SELECT 1 FROM successors WHERE entry_id=?2)",
                    params![new_id, old_id], |row| row.get(0),
                ).map_err(crate::store::storage_error)?;
                if closes_cycle {
                    return Err(ShenkoError::conflict("bank supersession would create a cycle"));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute("UPDATE intervention_bank_entries SET superseded_by=?2 WHERE entry_id=?1",
                    params![old_id, new_id]).map_err(crate::store::storage_error)?;
                tx.execute("UPDATE intervention_bank_entries SET supersedes=?2 WHERE entry_id=?1",
                    params![new_id, old_id]).map_err(crate::store::storage_error)?;
                self.write_commit_log(&tx, seq)?;
                tx.commit().map_err(crate::store::storage_error)?;
                Ok(())
            },
            research_storage_failure,
        )
    }

    /// Tombstone one bank entry. The row stays readable; it is no longer part
    /// of the default view of the bank.
    pub fn tombstone_bank_entry(
        &self,
        entry_id: EntityId,
        reason: &str,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || self.try_tombstone_bank_entry(entry_id, reason),
            research_storage_failure,
        )
    }

    fn try_tombstone_bank_entry(
        &self,
        entry_id: EntityId,
        reason: &str,
    ) -> Result<(), ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        // A replayed tombstone is the same terminal fact, not a second one:
        // recovery may re-drive an Armed attempt whose original write already
        // landed. Read the current state first.
        {
            let existing: Option<Option<i64>> = conn
                .query_row(
                    "SELECT tombstoned_at_ms FROM intervention_bank_entries WHERE entry_id=?1",
                    params![entry_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| ShenkoError::storage(error.to_string()))?;
            if existing.flatten().is_some() {
                return Ok(());
            }
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        ensure_research_schema(&tx)?;
        let seq = self.ledger.allocate_seq(&tx)?;
        let changed = tx.execute(
            "UPDATE intervention_bank_entries SET tombstoned_at_ms=?2
             WHERE entry_id=?1 AND tombstoned_at_ms IS NULL",
            params![entry_id.to_string(), Utc::now().timestamp_millis()],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        if changed != 1 {
            return Err(ShenkoError::conflict(format!(
                "bank entry {entry_id} is missing or already tombstoned ({reason})"
            )));
        }
        self.write_commit_log(&tx, seq)?;
        tx.commit()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        Ok(())
    }

    /// The accepting E1 transaction owns this write. A decision must never
    /// become visible independently of its operation's successful settlement.
    pub(crate) fn insert_intervention(
        tx: &rusqlite::Transaction<'_>,
        reminder: &InterventionReminder,
    ) -> Result<bool, ShenkoError> {
        validate_intervention_scope(reminder.epoch_id, &reminder.scope)?;
        ensure_research_schema(tx)?;
        if let Some(existing) = read_reminder_by(
            tx,
            "SELECT reminder_id FROM intervention_reminders WHERE reminder_id=?1",
            params![reminder.reminder_id.to_string()],
        )? {
            // A replay of the same decision keeps the first stored row: the
            // creation clock and the expiry are the first writer's, and the
            // consumption receipt is a later projection of the same row.
            // A different decision under one identity is a conflict.
            return if same_decision(&existing, reminder) {
                Ok(false)
            } else {
                Err(ShenkoError::conflict(
                    "intervention decision identity exists with different evidence",
                ))
            };
        }
        tx.execute(
            "INSERT INTO intervention_reminders
             (reminder_id,epoch_id,lineage_id,decision,policy,policy_revision,content,provenance,
              relevance,scope,expires_at_ms,transient,bank_entry_ids,reason,created_at_ms,
              used_step_id,used_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,NULL,NULL)",
            params![
                reminder.reminder_id.to_string(),
                reminder.epoch_id.to_string(),
                reminder.lineage_id.to_string(),
                match reminder.decision {
                    InterventionDecisionKind::Remind => "remind",
                    InterventionDecisionKind::Silent => "silent",
                },
                reminder.policy,
                reminder.policy_revision,
                reminder.content,
                encode_list(&reminder.provenance)?,
                reminder.relevance,
                reminder.scope,
                reminder.expires_at_ms,
                reminder.transient as i64,
                encode_list(
                    &reminder
                        .bank_entry_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<String>>(),
                )?,
                reminder.reason,
                reminder.created_at_ms,
            ],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        Ok(true)
    }

    /// One recorded decision.
    pub fn intervention_reminder(
        &self,
        reminder_id: EntityId,
    ) -> Result<Option<InterventionReminder>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        read_reminder_by(
            &conn,
            "SELECT reminder_id FROM intervention_reminders WHERE reminder_id=?1",
            params![reminder_id.to_string()],
        )
    }

    /// The most recent decision of an epoch.
    pub fn latest_intervention(
        &self,
        epoch_id: EntityId,
    ) -> Result<Option<InterventionReminder>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        read_reminder_by(
            &conn,
            "SELECT reminder_id FROM intervention_reminders WHERE epoch_id=?1 \
             ORDER BY created_at_ms DESC, reminder_id DESC LIMIT 1",
            params![epoch_id.to_string()],
        )
    }

    /// Resolve the addressed consumer from committed epoch/task ownership.
    /// An epoch belongs to its nearest model child, or to the subject root;
    /// it is never broadcast to siblings or to later delegated workers.
    pub fn intervention_delivery_scope(
        &self,
        reminder: &InterventionReminder,
    ) -> Result<Option<ContributionScope>, ShenkoError> {
        let mut locked = self.ledger.conn.lock();
        let conn = locked.transaction().map_err(|error| ShenkoError::storage(error.to_string()))?;
        reminder_delivery_scope(&conn, reminder.epoch_id, &reminder.scope)
    }

    /// Prepared batches lost on restart are derived from the pending durable
    /// decisions. Their consumers replay accepted request usage separately.
    pub fn pending_intervention_scopes(
        &self,
        agent_id: EntityId,
        now_ms: i64,
    ) -> Result<Vec<ContributionScope>, ShenkoError> {
        let mut locked = self.ledger.conn.lock();
        let conn = locked.transaction().map_err(|error| ShenkoError::storage(error.to_string()))?;
        let reminders = pending_scope_reminders(&conn, Some(&agent_id.to_string()), now_ms)?;
        let mut scopes = Vec::new();
        let mut resolved = std::collections::BTreeMap::new();
        for reminder in reminders {
            let scope = match resolved.entry((reminder.epoch_id, reminder.scope)) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let scope = reminder_delivery_scope(&conn, entry.key().0, &entry.key().1)?;
                    entry.insert(scope)
                }
            };
            if let Some(scope) = scope { scopes.push(scope.clone()); }
        }
        scopes.sort_by_key(|scope| (scope.scope_id, scope.task_id));
        scopes.dedup();
        Ok(scopes)
    }

    /// Usable material for one exact request scope. Branch-wide reminders
    /// never enter a child's scope; malformed ownership is not an empty plan.
    pub fn pending_interventions_for_scope(
        &self,
        scope: &ContributionScope,
        now_ms: i64,
    ) -> Result<Vec<InterventionReminder>, ShenkoError> {
        let mut locked = self.ledger.conn.lock();
        let conn = locked.transaction().map_err(|error| ShenkoError::storage(error.to_string()))?;
        let agent: Option<String> = match scope.task_id {
            Some(task) if task == scope.scope_id => conn.query_row(
                "SELECT json_extract(data,'$.agent_id') FROM tasks
                 WHERE task_id=?1 AND json_extract(data,'$.kind')=?2",
                params![task.to_string(), crate::external::MODEL_CHILD_KIND], |row| row.get(0),
            ).optional(),
            Some(_) => return Err(ShenkoError::conflict("noncanonical child contribution scope")),
            None => conn.query_row(
                "SELECT agent_id FROM subject_branch_heads WHERE branch_id=?1",
                [scope.scope_id.to_string()], |row| row.get(0),
            ).optional(),
        }.map_err(|error| ShenkoError::storage(error.to_string()))?;
        let Some(agent) = agent else { return Ok(Vec::new()); };
        let mut resolved = std::collections::BTreeMap::new();
        let mut addressed = Vec::new();
        for reminder in pending_scope_reminders(&conn, Some(&agent), now_ms)? {
            let delivery = match resolved.entry((reminder.epoch_id, reminder.scope.clone())) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let delivery = reminder_delivery_scope(&conn, entry.key().0, &entry.key().1)?;
                    entry.insert(delivery)
                }
            };
            if delivery.as_ref() == Some(scope) {
                addressed.push(reminder);
            }
        }
        Ok(addressed)
    }

    /// The reminders of a lineage that are still usable: not expired, not
    /// already spent (a transient reminder leaves the visible context after
    /// the request it was prepared for).
    pub fn pending_interventions(
        &self,
        lineage_id: EntityId,
        now_ms: i64,
    ) -> Result<Vec<InterventionReminder>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT reminder_id FROM intervention_reminders
                 WHERE lineage_id=?1 AND decision='remind'
                   AND (expires_at_ms IS NULL OR expires_at_ms > ?2)
                   AND (transient=0 OR used_at_ms IS NULL)
                 ORDER BY created_at_ms ASC, reminder_id ASC",
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let ids = statement
            .query_map(params![lineage_id.to_string(), now_ms], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| ShenkoError::storage(error.to_string()))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        drop(statement);
        let mut reminders = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(reminder) = read_reminder_by(
                &conn,
                "SELECT reminder_id FROM intervention_reminders WHERE reminder_id=?1",
                params![id],
            )? {
                reminders.push(reminder);
            }
        }
        Ok(reminders)
    }

    /// The still-usable intervention reminders of every active research epoch
    /// whose owning task belongs to this agent, oldest first, bounded to
    /// `limit`. This is the agent-scoped drain the PMA intervention worker
    /// uses; it is a different durable set from the library memory intents
    /// (`store/src/memory.rs::pending_library_memory_intents`) and the two
    /// never share a method.
    pub fn pending_intervention_reminders(
        &self,
        agent_id: EntityId,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<InterventionReminder>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT r.reminder_id FROM intervention_reminders r
                 JOIN research_epochs e ON e.epoch_id = r.epoch_id
                 JOIN tasks t ON t.task_id = e.task_id
                 WHERE e.status='active'
                   AND json_extract(t.data, '$.agent_id') = ?1
                   AND r.decision='remind'
                   AND (r.expires_at_ms IS NULL OR r.expires_at_ms > ?2)
                   AND (r.transient=0 OR r.used_at_ms IS NULL)
                 ORDER BY r.created_at_ms ASC, r.reminder_id ASC
                 LIMIT ?3",
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let ids = statement
            .query_map(
                params![agent_id.to_string(), now_ms, limit as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        drop(statement);
        let mut reminders = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(reminder) = read_reminder_by(
                &conn,
                "SELECT reminder_id FROM intervention_reminders WHERE reminder_id=?1",
                params![id],
            )? {
                // An agent-scoped drain never prepares a decision whose
                // accepted evidence names another author. The decision and its
                // receipt stay readable; the omission is reported by
                // [`Store::pending_intervention_omissions`].
                if reminder_omission(&conn, &reminder)?.is_none() {
                    reminders.push(reminder);
                }
            }
        }
        Ok(reminders)
    }

    /// Committed reminders that this agent may NOT prepare, each with the
    /// durable link that refused it.
    ///
    /// A decision that is already committed stays readable and unchanged; this
    /// is the explicit omission the preparation boundary reports instead of
    /// publishing foreign material or silently dropping it. Owner and author
    /// are read from accepted evidence (`pma-decision:<reminder_id>` receipt →
    /// operation → task) and from the epoch's committed owning task — never
    /// from model text, the reminder body or the declared address.
    pub fn pending_intervention_omissions(
        &self,
        agent_id: EntityId,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<InterventionOmission>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT r.reminder_id FROM intervention_reminders r
                 JOIN research_epochs e ON e.epoch_id = r.epoch_id
                 JOIN tasks t ON t.task_id = e.task_id
                 WHERE e.status='active'
                   AND json_extract(t.data, '$.agent_id') = ?1
                   AND r.decision='remind'
                   AND (r.expires_at_ms IS NULL OR r.expires_at_ms > ?2)
                   AND (r.transient=0 OR r.used_at_ms IS NULL)
                 ORDER BY r.created_at_ms ASC, r.reminder_id ASC
                 LIMIT ?3",
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let ids = statement
            .query_map(
                params![agent_id.to_string(), now_ms, limit as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        drop(statement);
        let mut omissions = Vec::new();
        for id in ids {
            if let Some(reminder) = read_reminder_by(
                &conn,
                "SELECT reminder_id FROM intervention_reminders WHERE reminder_id=?1",
                params![id],
            )? {
                if let Some(omission) = reminder_omission(&conn, &reminder)? {
                    omissions.push(omission);
                }
            }
        }
        Ok(omissions)
    }

    /// Record that a reminder was actually sent in a call. This is the usage
    /// receipt of the intervention worker: the ledger keeps what was
    /// injected, its provenance and the call it was injected into.
    pub fn record_intervention_use(
        &self,
        reminder_id: EntityId,
        step_id: Option<EntityId>,
        now_ms: i64,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                ensure_research_schema(&tx)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let changed = tx.execute(
                    "UPDATE intervention_reminders SET used_step_id=?2, used_at_ms=?3
                     WHERE reminder_id=?1 AND used_at_ms IS NULL",
                    params![
                        reminder_id.to_string(),
                        step_id.map(|id| id.to_string()),
                        now_ms
                    ],
                )
                .map_err(|error| ShenkoError::storage(error.to_string()))?;
                if changed != 1 {
                    return Err(ShenkoError::conflict(
                        "intervention reminder was already used or is unknown",
                    ));
                }
                self.write_commit_log(&tx, seq)?;
                tx.commit()
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                Ok(())
            },
            research_storage_failure,
        )
    }

    /// Record a deletion tombstone. The tombstone keeps identity, scope and
    /// the effect record; it never keeps the deleted body.
    pub fn record_tombstone(
        &self,
        tombstone: &RetentionTombstone,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                ensure_research_schema(&tx)?;
                // A replayed mechanism is the same terminal fact, not a
                // second one: recovery may re-drive an Armed attempt whose
                // original write already landed.
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT effect_record FROM retention_tombstones WHERE tombstone_id=?1",
                        params![tombstone.tombstone_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                if let Some(effect_record) = existing {
                    return if effect_record == tombstone.effect_record {
                        Ok(())
                    } else {
                        Err(ShenkoError::conflict(
                            "retention tombstone identity exists with a different closure",
                        ))
                    };
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO retention_tombstones
                     (tombstone_id,target_kind,target_id,scope,reason,effect_record,deleted_at_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        tombstone.tombstone_id.to_string(),
                        tombstone.target_kind,
                        tombstone.target_id,
                        tombstone.scope,
                        tombstone.reason,
                        tombstone.effect_record,
                        tombstone.deleted_at_ms,
                    ],
                )
                .map_err(|error| ShenkoError::storage(error.to_string()))?;
                self.write_commit_log(&tx, seq)?;
                tx.commit()
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                Ok(())
            },
            research_storage_failure,
        )
    }

    /// The tombstones of a target kind, newest first.
    pub fn retention_tombstones(
        &self,
        target_kind: Option<&str>,
    ) -> Result<Vec<RetentionTombstone>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let (sql, parameter): (String, Option<String>) = match target_kind {
            Some(kind) => (
                "SELECT tombstone_id,target_kind,target_id,scope,reason,effect_record,deleted_at_ms
                 FROM retention_tombstones WHERE target_kind=?1 ORDER BY deleted_at_ms DESC, tombstone_id DESC"
                    .to_string(),
                Some(kind.to_string()),
            ),
            None => (
                "SELECT tombstone_id,target_kind,target_id,scope,reason,effect_record,deleted_at_ms
                 FROM retention_tombstones ORDER BY deleted_at_ms DESC, tombstone_id DESC"
                    .to_string(),
                None,
            ),
        };
        let mut statement = conn.prepare(&sql).map_err(|error| ShenkoError::storage(error.to_string()))?;
        let decode_row = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                row.get::<_, String>(2)?, row.get::<_, String>(3)?,
                row.get::<_, String>(4)?, row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        };
        let rows = match &parameter {
            Some(value) => statement
                .query_map(params![value], decode_row)
                .map_err(|error| ShenkoError::storage(error.to_string()))?,
            None => statement
                .query_map([], decode_row)
                .map_err(|error| ShenkoError::storage(error.to_string()))?,
        };
        let mut tombstones = Vec::new();
        for row in rows {
            let (
                tombstone_id,
                target_kind,
                target_id,
                scope,
                reason,
                effect_record,
                deleted_at_ms,
            ) = row.map_err(|error| ShenkoError::storage(error.to_string()))?;
            tombstones.push(RetentionTombstone {
                tombstone_id: parse_id(&tombstone_id)?,
                target_kind,
                target_id,
                scope,
                reason,
                effect_record,
                deleted_at_ms,
            });
        }
        Ok(tombstones)
    }

    /// Record one completed archive move.
    pub fn record_archive_move(
        &self,
        record: &ArchiveMoveRecord,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                ensure_research_schema(&tx)?;
                // A replayed archive move is the same terminal fact.
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT manifest_ref FROM archive_moves WHERE archive_id=?1",
                        params![record.archive_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                if let Some(manifest_ref) = existing {
                    return if manifest_ref == record.manifest_ref {
                        Ok(())
                    } else {
                        Err(ShenkoError::conflict(
                            "archive move identity exists with a different manifest",
                        ))
                    };
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO archive_moves
                     (archive_id,source_ref,target_provider,target_namespace,target_locator,manifest_ref,
                      object_count,total_bytes,hash_verified,readback_verified,local_released,
                      local_release_note,created_at_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![
                        record.archive_id.to_string(),
                        record.source_ref,
                        record.target_provider,
                        record.target_namespace,
                        record.target_locator,
                        record.manifest_ref,
                        record.object_count as i64,
                        record.total_bytes as i64,
                        record.hash_verified as i64,
                        record.readback_verified as i64,
                        record.local_released as i64,
                        record.local_release_note,
                        record.created_at_ms,
                    ],
                )
                .map_err(|error| ShenkoError::storage(error.to_string()))?;
                self.write_commit_log(&tx, seq)?;
                tx.commit()
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                Ok(())
            },
            research_storage_failure,
        )
    }

    /// Every recorded archive move, newest first.
    pub fn archive_moves(&self) -> Result<Vec<ArchiveMoveRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        ensure_research_schema(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT archive_id,source_ref,target_provider,target_namespace,target_locator,manifest_ref,
                        object_count,total_bytes,hash_verified,readback_verified,local_released,
                        local_release_note,created_at_ms
                 FROM archive_moves ORDER BY created_at_ms DESC, archive_id DESC",
            )
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            })
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let mut moves = Vec::new();
        for row in rows {
            let (
                archive_id,
                source_ref,
                target_provider,
                target_namespace,
                target_locator,
                manifest_ref,
                object_count,
                total_bytes,
                hash_verified,
                readback_verified,
                local_released,
                local_release_note,
                created_at_ms,
            ) = row.map_err(|error| ShenkoError::storage(error.to_string()))?;
            moves.push(ArchiveMoveRecord {
                archive_id: parse_id(&archive_id)?,
                source_ref,
                target_provider,
                target_namespace,
                target_locator,
                manifest_ref,
                object_count: object_count.max(0) as u64,
                total_bytes: total_bytes.max(0) as u64,
                hash_verified: hash_verified != 0,
                readback_verified: readback_verified != 0,
                local_released: local_released != 0,
                local_release_note,
                created_at_ms,
            });
        }
        Ok(moves)
    }

    fn write_commit_log(
        &self,
        tx: &rusqlite::Transaction<'_>,
        seq: LedgerSeq,
    ) -> Result<(), ShenkoError> {
        tx.execute(
            "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
             VALUES (?1,?2,?3,1)",
            params![
                self.ledger.ledger_id().to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string(),
            ],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        Ok(())
    }
}

struct SealedClosure {
    constraints: ObjectRef,
    budget: ObjectRef,
    effects: ObjectRef,
    environment: ObjectRef,
}

const EPOCH_COLUMNS: &str = "epoch_id,reset_request_id,lineage_id,epoch_no,task_id,status,\
     constraints_ref,budget_ref,effects_ref,environment_ref,best_candidate_id,brief_ref,\
     reset_from_epoch,reset_reason,reset_receipt,created_at_ms,frozen_at_ms";

/// The full projection of one epoch row.
const EPOCH_SELECT: &str = "SELECT epoch_id,reset_request_id,lineage_id,epoch_no,task_id,status,\
     constraints_ref,budget_ref,effects_ref,environment_ref,best_candidate_id,brief_ref,\
     reset_from_epoch,reset_reason,reset_receipt,created_at_ms,frozen_at_ms FROM research_epochs";

fn read_epoch_by(
    conn: &Connection,
    predicate: &str,
    parameter: impl rusqlite::Params,
) -> Result<Option<ResearchEpoch>, ShenkoError> {
    let row = conn
        .query_row(&format!("{EPOCH_SELECT} {predicate}"), parameter, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, i64>(15)?,
                row.get::<_, Option<i64>>(16)?,
            ))
        })
        .optional()
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
    let Some((
        epoch_id,
        reset_request_id,
        lineage_id,
        epoch_no,
        task_id,
        status,
        constraints_ref,
        budget_ref,
        effects_ref,
        environment_ref,
        best_candidate_id,
        brief_ref,
        reset_from_epoch,
        reset_reason,
        reset_receipt,
        created_at_ms,
        frozen_at_ms,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(ResearchEpoch {
        epoch_id: parse_id(&epoch_id)?,
        reset_request_id: optional_id(reset_request_id)?,
        lineage_id: parse_id(&lineage_id)?,
        epoch_no: epoch_no.max(0) as u64,
        task_id: optional_id(task_id)?,
        status: match status.as_str() {
            "active" => ResearchEpochStatus::Active,
            "frozen" => ResearchEpochStatus::Frozen,
            other => {
                return Err(ShenkoError::conflict(format!(
                    "unknown research epoch status: {other}"
                )));
            }
        },
        constraints_ref: decode_object(constraints_ref.as_deref())?,
        budget_ref: decode_object(budget_ref.as_deref())?,
        effects_ref: decode_object(effects_ref.as_deref())?,
        environment_ref: decode_object(environment_ref.as_deref())?,
        best_candidate_id: optional_id(best_candidate_id)?,
        brief_ref: decode_object(brief_ref.as_deref())?,
        reset_from_epoch: optional_id(reset_from_epoch)?,
        reset_reason,
        reset_receipt,
        created_at_ms,
        frozen_at_ms,
    }))
}

const REMINDER_COLUMNS: &str = "reminder_id,epoch_id,lineage_id,decision,policy,policy_revision,\
     content,provenance,relevance,scope,expires_at_ms,transient,bank_entry_ids,reason,created_at_ms,\
     used_step_id,used_at_ms";

/// Validate a reminder address before accepting it as a durable decision.
/// Ownership routing is separate: neither prose nor another epoch is a
/// destination, even when the reminder text itself is useful.
pub fn validate_intervention_scope(epoch: EntityId, address: &str) -> Result<(), ShenkoError> {
    if address == "subject"
        || address.strip_prefix("epoch:").and_then(|id| id.parse::<EntityId>().ok()) == Some(epoch)
    {
        Ok(())
    } else {
        Err(ShenkoError::conflict("reminder names a different or invalid scope"))
    }
}

/// One SQL snapshot; the optional agent binding avoids scanning other agents
/// for each prepared scope. Callers keep the connection lock through routing.
///
/// A reminder that survives the SQL scope filter is still checked against the
/// author of its accepted decision evidence; a foreign-authored decision is
/// left out of the prepared set instead of being published.
fn pending_scope_reminders(
    conn: &Connection,
    agent: Option<&str>,
    now_ms: i64,
) -> Result<Vec<InterventionReminder>, ShenkoError> {
    ensure_research_schema(conn)?;
    let mut statement = conn.prepare(
        "SELECT r.reminder_id FROM intervention_reminders r
         JOIN research_epochs e ON e.epoch_id=r.epoch_id
         LEFT JOIN tasks t ON t.task_id=e.task_id
         WHERE e.status='active' AND r.decision='remind'
           AND (?1 IS NULL OR json_extract(t.data,'$.agent_id')=?1)
           AND (r.expires_at_ms IS NULL OR r.expires_at_ms>?2)
           AND (r.transient=0 OR r.used_at_ms IS NULL)
         ORDER BY r.created_at_ms,r.reminder_id",
    ).map_err(|error| ShenkoError::storage(error.to_string()))?;
    let ids = statement.query_map(params![agent, now_ms], |row| row.get::<_, String>(0))
        .map_err(|error| ShenkoError::storage(error.to_string()))?
        .collect::<Result<Vec<_>, _>>().map_err(|error| ShenkoError::storage(error.to_string()))?;
    drop(statement);
    let mut admitted = Vec::with_capacity(ids.len());
    for id in ids {
        let reminder = read_reminder_by(conn,
            "SELECT reminder_id FROM intervention_reminders WHERE reminder_id=?1", params![id])?
            .ok_or_else(|| ShenkoError::conflict("pending reminder disappeared within its snapshot"))?;
        // A committed decision whose accepted evidence names another author is
        // never prepared for this epoch's agent. It stays readable, and
        // [`Store::pending_intervention_omissions`] reports it.
        if reminder_omission(conn, &reminder)?.is_none() {
            admitted.push(reminder);
        }
    }
    Ok(admitted)
}

fn reminder_delivery_scope(
    conn: &Connection,
    epoch: EntityId,
    address: &str,
) -> Result<Option<ContributionScope>, ShenkoError> {
    validate_intervention_scope(epoch, address)?;
    let binding: Option<(String, String)> = conn.query_row(
        "SELECT t.data,h.data FROM research_epochs e
         JOIN tasks t ON t.task_id=e.task_id
         JOIN subject_branch_heads h ON h.agent_id=json_extract(t.data,'$.agent_id')
         WHERE e.epoch_id=?1 AND e.status='active'",
        [epoch.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(|error| ShenkoError::storage(error.to_string()))?;
    // Legacy ownerless epochs establish no recipient, never a broadcast.
    let Some((task, head)) = binding else { return Ok(None); };
    let mut task: TaskRecord = serde_json::from_str(&task)?;
    let head: SubjectBranchHead = serde_json::from_str(&head)?;
    let subject = ContributionScope { scope_id: EntityId(head.branch_id.0), task_id: None };
    if address == "subject" { return Ok(Some(subject)); }
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..64 {
        if task.agent_id != head.agent_id || !seen.insert(task.task_id) {
            return Err(ShenkoError::conflict("invalid epoch task ownership chain"));
        }
        if task.task_id == head.subject_root_task_id { return Ok(Some(subject)); }
        if task.kind == crate::external::MODEL_CHILD_KIND {
            return Ok(Some(ContributionScope { scope_id: task.task_id, task_id: Some(task.task_id) }));
        }
        let TaskOwner::Task { id } = task.owner else { return Ok(None); };
        task = crate::child::read_task(conn, id)
            .map_err(|error| ShenkoError::storage(error.to_string()))?
            .ok_or_else(|| ShenkoError::conflict("epoch task owner is missing"))?;
    }
    Err(ShenkoError::conflict("epoch task ownership depth exceeded"))
}

/// One committed reminder that a preparation boundary refused.
///
/// The decision, its receipt and its reminder row are unchanged and stay
/// readable; this record only says why the reminder is not prepared material
/// for this epoch's agent. It carries identities and the refusing link, never
/// the reminder body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterventionOmission {
    pub reminder_id: EntityId,
    pub epoch_id: EntityId,
    /// The address the committed decision declared.
    pub address: String,
    pub kind: InterventionOmissionKind,
    /// The agent the accepted decision evidence names, when it is readable.
    pub author_agent: Option<EntityId>,
    /// The agent of the epoch's committed owning task, when it is readable.
    pub epoch_agent: Option<EntityId>,
    /// The concrete missing or contradictory durable link.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionOmissionKind {
    /// No accepted `pma-decision:` receipt names this reminder.
    AuthorUnproven,
    /// The evidence → operation → task chain is missing or unreadable.
    AuthorDangling,
    /// Accepted decision evidence names more than one author.
    AuthorAmbiguous,
    /// The decision's author is a different agent than the epoch's owner.
    ForeignAuthor,
    /// The epoch's committed owning task does not resolve to an agent.
    /// Both prepared readers bind an agent, so this classification is only
    /// reachable for a snapshot that is not agent-scoped.
    EpochOwnerUnproven,
}

/// The agent that authored one committed decision, read from accepted
/// evidence — never from model text.
///
/// `intervention` records exactly one settlement receipt per bounded call under
/// `source_event_key = "pma-decision:<request_id>"`, and the reminder id IS the
/// frozen request id. That receipt's operation names the task that authored the
/// decision, and that task names its agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReminderAuthor {
    /// Accepted decision evidence names exactly this agent.
    Proven(EntityId),
    /// No accepted `pma-decision:` receipt is linked to the reminder.
    Unproven,
    /// The evidence → operation → task chain is missing or unreadable.
    Dangling(String),
    /// Accepted decision evidence names more than one author.
    Ambiguous(Vec<EntityId>),
}

fn reminder_author(conn: &Connection, reminder_id: EntityId) -> Result<ReminderAuthor, ShenkoError> {
    let key = format!("pma-decision:{reminder_id}");
    let mut statement = conn
        .prepare(
            "SELECT operation_id FROM evidence_receipts
             WHERE json_extract(data,'$.source_event_key')=?1 ORDER BY rowid",
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
    let operations = statement
        .query_map([key], |row| row.get::<_, Option<String>>(0))
        .map_err(|error| ShenkoError::storage(error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
    drop(statement);
    if operations.is_empty() {
        return Ok(ReminderAuthor::Unproven);
    }
    let mut authors = std::collections::BTreeSet::new();
    for operation in operations {
        let Some(operation) = operation else {
            return Ok(ReminderAuthor::Dangling(
                "accepted decision receipt names no operation".into(),
            ));
        };
        let task: Option<Option<String>> = conn
            .query_row(
                "SELECT json_extract(data,'$.task_id') FROM operations WHERE operation_id=?1",
                params![operation],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let Some(task) = task else {
            return Ok(ReminderAuthor::Dangling(format!(
                "authoring operation {operation} is missing"
            )));
        };
        let Some(task) = task else {
            return Ok(ReminderAuthor::Dangling(format!(
                "authoring operation {operation} names no task"
            )));
        };
        let agent: Option<Option<String>> = conn
            .query_row(
                "SELECT json_extract(data,'$.agent_id') FROM tasks WHERE task_id=?1",
                params![task],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let Some(agent) = agent else {
            return Ok(ReminderAuthor::Dangling(format!(
                "authoring task {task} is missing"
            )));
        };
        let Some(agent) = agent.and_then(|agent| agent.parse::<EntityId>().ok()) else {
            return Ok(ReminderAuthor::Dangling(format!(
                "authoring task {task} names no readable agent"
            )));
        };
        authors.insert(agent);
    }
    Ok(match authors.len() {
        1 => ReminderAuthor::Proven(*authors.iter().next().expect("one author")),
        _ => ReminderAuthor::Ambiguous(authors.into_iter().collect()),
    })
}

/// The agent of the epoch's committed owning task — the same ownership
/// `reminder_delivery_scope` resolves. `None` means the committed chain does
/// not name an agent (a legacy ownerless epoch).
fn reminder_epoch_agent(
    conn: &Connection,
    epoch: EntityId,
) -> Result<Option<EntityId>, ShenkoError> {
    let agent: Option<Option<String>> = conn
        .query_row(
            "SELECT json_extract(t.data,'$.agent_id') FROM research_epochs e
             LEFT JOIN tasks t ON t.task_id=e.task_id WHERE e.epoch_id=?1",
            [epoch.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
    Ok(agent.flatten().and_then(|agent| agent.parse::<EntityId>().ok()))
}

/// Whether one committed reminder may enter a prepared batch.
///
/// The reminder body, the model answer and the declared address are not
/// authority: only accepted decision evidence links a reminder to the agent
/// that authored it. A reminder whose author differs from the epoch's committed
/// owner — or whose author cannot be read — is reported as an omission, never
/// prepared. Nothing is deleted, rewritten or re-keyed.
fn reminder_omission(
    conn: &Connection,
    reminder: &InterventionReminder,
) -> Result<Option<InterventionOmission>, ShenkoError> {
    fn omit(
        reminder: &InterventionReminder,
        epoch_agent: Option<EntityId>,
        kind: InterventionOmissionKind,
        author_agent: Option<EntityId>,
        reason: String,
    ) -> InterventionOmission {
        InterventionOmission {
            reminder_id: reminder.reminder_id,
            epoch_id: reminder.epoch_id,
            address: reminder.scope.clone(),
            kind,
            author_agent,
            epoch_agent,
            reason,
        }
    }
    let epoch_agent = reminder_epoch_agent(conn, reminder.epoch_id)?;
    let author = reminder_author(conn, reminder.reminder_id)?;
    Ok(Some(match (&author, epoch_agent) {
        (ReminderAuthor::Proven(author), Some(owner)) if *author == owner => return Ok(None),
        (ReminderAuthor::Proven(author), Some(owner)) => omit(
            reminder,
            epoch_agent,
            InterventionOmissionKind::ForeignAuthor,
            Some(*author),
            format!("decision authored by agent {author}; epoch owned by agent {owner}"),
        ),
        (ReminderAuthor::Proven(author), None) => omit(
            reminder,
            epoch_agent,
            InterventionOmissionKind::EpochOwnerUnproven,
            Some(*author),
            "the epoch's committed owning task does not resolve to an agent".into(),
        ),
        (ReminderAuthor::Unproven, _) => omit(
            reminder,
            epoch_agent,
            InterventionOmissionKind::AuthorUnproven,
            None,
            format!(
                "no accepted pma-decision receipt names reminder {}",
                reminder.reminder_id
            ),
        ),
        (ReminderAuthor::Dangling(reason), _) => omit(
            reminder,
            epoch_agent,
            InterventionOmissionKind::AuthorDangling,
            None,
            reason.clone(),
        ),
        (ReminderAuthor::Ambiguous(authors), _) => omit(
            reminder,
            epoch_agent,
            InterventionOmissionKind::AuthorAmbiguous,
            None,
            format!("accepted decision evidence names several authors: {authors:?}"),
        ),
    }))
}

fn read_reminder_by(
    conn: &Connection,
    sql: &str,
    parameter: impl rusqlite::Params,
) -> Result<Option<InterventionReminder>, ShenkoError> {
    // Callers pass a bare id-lookup SELECT; normalize it to the full
    // projection this reader decodes. A full projection is left untouched.
    let full = if let Some(predicate) =
        sql.strip_prefix("SELECT reminder_id FROM intervention_reminders")
    {
        format!("SELECT {REMINDER_COLUMNS} FROM intervention_reminders{predicate}")
    } else {
        sql.to_string()
    };
    let row = conn
        .query_row(&full, parameter, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<i64>>(16)?,
            ))
        })
        .optional()
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
    let Some((
        reminder_id,
        epoch_id,
        lineage_id,
        decision,
        policy,
        policy_revision,
        content,
        provenance,
        relevance,
        scope,
        expires_at_ms,
        transient,
        bank_entry_ids,
        reason,
        created_at_ms,
        used_step_id,
        used_at_ms,
    )) = row
    else {
        return Ok(None);
    };
    Ok(Some(InterventionReminder {
        reminder_id: parse_id(&reminder_id)?,
        epoch_id: parse_id(&epoch_id)?,
        lineage_id: parse_id(&lineage_id)?,
        decision: match decision.as_str() {
            "remind" => InterventionDecisionKind::Remind,
            "silent" => InterventionDecisionKind::Silent,
            other => {
                return Err(ShenkoError::conflict(format!(
                    "unknown intervention decision: {other}"
                )));
            }
        },
        policy,
        policy_revision,
        content,
        provenance: decode_list::<String>(Some(&provenance))?,
        relevance,
        scope,
        expires_at_ms,
        transient: transient != 0,
        bank_entry_ids: decode_list::<String>(Some(&bank_entry_ids))?
            .iter()
            .map(|id| parse_id(id))
            .collect::<Result<Vec<EntityId>, ShenkoError>>()?,
        reason,
        created_at_ms,
        used_step_id: optional_id(used_step_id)?,
        used_at_ms,
    }))
}

/// The column list of the epoch table, kept next to the reader so a schema
/// change cannot silently shift the tuple order.
#[allow(dead_code)]
pub const EPOCH_COLUMN_LIST: &str = EPOCH_COLUMNS;

/// The column list of the reminder table.
#[allow(dead_code)]
pub const REMINDER_COLUMN_LIST: &str = REMINDER_COLUMNS;

#[cfg(test)]
mod reminder_author_admission_tests {
    use super::*;

    /// The committed reminders one preparation boundary would return.
    fn prepared(store: &Store, agent: EntityId, now_ms: i64) -> Vec<EntityId> {
        let conn = store.ledger.conn.lock();
        let mut ids: Vec<EntityId> =
            pending_scope_reminders(&conn, Some(&agent.to_string()), now_ms)
                .unwrap()
                .into_iter()
                .map(|reminder| reminder.reminder_id)
                .collect();
        ids.sort();
        ids
    }

    fn omissions(store: &Store, agent: EntityId, now_ms: i64) -> Vec<InterventionOmission> {
        store
            .pending_intervention_omissions(agent, now_ms, 8)
            .unwrap()
    }

    /// A reminder id is prepared only while its accepted decision evidence and
    /// its epoch's committed owning task name the same agent. Every other case
    /// is an explicit omission, and the committed decision never changes.
    #[test]
    fn only_the_evidenced_author_of_a_decision_is_prepared() {
        let dir = std::env::temp_dir().join(format!("shenko-reminder-author-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap();
        let owner = EntityId::new();
        let foreign = EntityId::new();
        let owner_task = EntityId::new();
        let foreign_task = EntityId::new();
        let other_task = EntityId::new();
        let epoch = EntityId::new();
        let lineage = EntityId::new();
        let reminder_id = EntityId::new();
        let operation = EntityId::new();
        let other_operation = EntityId::new();
        let now_ms = Utc::now().timestamp_millis();
        let task_data = |agent: EntityId| serde_json::json!({"agent_id": agent.to_string()}).to_string();
        let operation_data =
            |task: EntityId| serde_json::json!({"task_id": task.to_string()}).to_string();
        let receipt_data =
            |key: &str| serde_json::json!({"source_event_key": key}).to_string();
        {
            let conn = store.ledger.conn.lock();
            ensure_research_schema(&conn).unwrap();
            for (task, agent) in [
                (owner_task, owner),
                (foreign_task, foreign),
                (other_task, owner),
            ] {
                conn.execute(
                    "INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,1)",
                    params![task.to_string(), task_data(agent)],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO research_epochs(epoch_id,lineage_id,epoch_no,task_id,status,created_at_ms)
                 VALUES(?1,?2,1,?3,'active',?4)",
                params![epoch.to_string(), lineage.to_string(), owner_task.to_string(), now_ms],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO intervention_reminders
                 (reminder_id,epoch_id,lineage_id,decision,policy,policy_revision,content,provenance,
                  relevance,scope,expires_at_ms,transient,bank_entry_ids,reason,created_at_ms,
                  used_step_id,used_at_ms)
                 VALUES(?1,?2,?3,'remind','fixture-policy','1','prepared body','[]','fixture','subject',
                        NULL,1,'[]',NULL,?4,NULL,NULL)",
                params![reminder_id.to_string(), epoch.to_string(), lineage.to_string(), now_ms],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO operations(operation_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,1)",
                params![operation.to_string(), operation_data(owner_task)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO evidence_receipts(receipt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,1)",
                params![
                    EntityId::new().to_string(),
                    operation.to_string(),
                    receipt_data(&format!("pma-decision:{reminder_id}"))
                ],
            )
            .unwrap();
        }

        // The evidenced author and the epoch owner agree: prepared.
        assert_eq!(prepared(&store, owner, now_ms), vec![reminder_id]);
        assert!(omissions(&store, owner, now_ms).is_empty());

        // The very same committed decision, authored by another agent's
        // operation: readable, never prepared, and reported explicitly.
        {
            let conn = store.ledger.conn.lock();
            conn.execute(
                "UPDATE operations SET data=?2 WHERE operation_id=?1",
                params![operation.to_string(), operation_data(foreign_task)],
            )
            .unwrap();
        }
        assert!(prepared(&store, owner, now_ms).is_empty());
        let reported = omissions(&store, owner, now_ms);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].reminder_id, reminder_id);
        assert_eq!(reported[0].kind, InterventionOmissionKind::ForeignAuthor);
        assert_eq!(reported[0].author_agent, Some(foreign));
        assert_eq!(reported[0].epoch_agent, Some(owner));
        // The committed decision itself is unchanged and still readable.
        let committed = store.intervention_reminder(reminder_id).unwrap().unwrap();
        assert_eq!(committed.content.as_deref(), Some("prepared body"));
        assert_eq!(
            store.latest_intervention(epoch).unwrap().unwrap().reminder_id,
            reminder_id
        );

        // A second accepted decision receipt naming a different author makes
        // the identity ambiguous instead of silently picking one.
        {
            let conn = store.ledger.conn.lock();
            conn.execute(
                "INSERT INTO operations(operation_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,1)",
                params![other_operation.to_string(), operation_data(other_task)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO evidence_receipts(receipt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,1)",
                params![
                    EntityId::new().to_string(),
                    other_operation.to_string(),
                    receipt_data(&format!("pma-decision:{reminder_id}"))
                ],
            )
            .unwrap();
        }
        assert!(prepared(&store, owner, now_ms).is_empty());
        let reported = omissions(&store, owner, now_ms);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].kind, InterventionOmissionKind::AuthorAmbiguous);
        assert!(reported[0].reason.contains("several authors"));

        // A dangling authoring chain is reported as dangling, never guessed.
        {
            let conn = store.ledger.conn.lock();
            conn.execute("DELETE FROM operations", params![]).unwrap();
        }
        assert!(prepared(&store, owner, now_ms).is_empty());
        let reported = omissions(&store, owner, now_ms);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].kind, InterventionOmissionKind::AuthorDangling);

        // Without any accepted decision receipt the reminder is unproven.
        {
            let conn = store.ledger.conn.lock();
            conn.execute("DELETE FROM evidence_receipts", params![]).unwrap();
        }
        assert!(prepared(&store, owner, now_ms).is_empty());
        let reported = omissions(&store, owner, now_ms);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].kind, InterventionOmissionKind::AuthorUnproven);

        // An epoch with no committed owning task makes no agent claim.
        {
            let conn = store.ledger.conn.lock();
            conn.execute(
                "UPDATE research_epochs SET task_id=NULL WHERE epoch_id=?1",
                params![epoch.to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO operations(operation_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,1)",
                params![operation.to_string(), operation_data(owner_task)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO evidence_receipts(receipt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,1)",
                params![
                    EntityId::new().to_string(),
                    operation.to_string(),
                    receipt_data(&format!("pma-decision:{reminder_id}"))
                ],
            )
            .unwrap();
        }
        // An ownerless epoch makes no agent claim, so the reminder is not even
        // addressable to this agent: it is neither prepared nor reported as
        // this agent's omission.
        assert!(prepared(&store, owner, now_ms).is_empty());
        assert!(omissions(&store, owner, now_ms).is_empty());
        // Nothing above deleted, rewrote or re-keyed the decision.
        let committed = store.intervention_reminder(reminder_id).unwrap().unwrap();
        assert_eq!(committed.content.as_deref(), Some("prepared body"));
        assert_eq!(committed.scope, "subject");
    }
}
