//! Evidence-based forgetting — SPEC §51.7 and §12.4.
//!
//! §51.7 measures decay in **subject-relative running time**, not wall clock
//! time, and separates the projection stage (default recall prominence) from
//! the bytes. This module is the mechanism half of that contract:
//!
//! * durable evidence rows say that one piece of registered content was used,
//!   referenced, retrieved or revised at one measured active-runtime instant,
//!   or that an explicit expiry deadline was set for it;
//! * a bounded cycle evaluates at most `ForgettingPolicy::max_candidates`
//!   registered publications, accepts an explicit [`ForgettingClock`] sample
//!   from the caller (the main-loop meter owner), and never substitutes wall
//!   clock time for a missing measurement;
//! * a decision is durable as a receipt: forgotten, retained by a reference
//!   exception, or unevaluated. "Kept" is reported but is not written: nothing
//!   happened;
//! * a forgotten publication is unpublished by removing its
//!   `cognition_contents` row and writing the existing monotone
//!   `retention_tombstones` row. Bytes are released only when the policy
//!   explicitly asks for it, the object belongs to the cognition content
//!   namespace, and a complete retention inventory plus the transaction-local
//!   archive-root audit both prove that no durable artifact references it.
//!
//! **A remembered fact is never silently dropped.** Any attitude, legacy
//! mapping, cognitive-root fact, archive/receipt carrier or other retained
//! artifact that names the digest is a retention exception: the publication
//! and its bytes stay, and the receipt records the exception instead of a
//! deletion. Content with no evidence at all is *unevaluated*; the cycle does
//! not guess that a fact has decayed just because no signal was recorded.
//!
//! The tables are local mechanism state, like a runtime segment meter: they
//! are not semantic attitudes and are not replicated archive facts. The
//! durable deletion tombstone lives in the replicated `retention_tombstones`
//! table, so a peer generation can still see that the content was forgotten.

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

use crate::atoms::COGNITION_CONTENT_NAMESPACE;
use crate::retained_objects::{
    RetainedObjectSnapshot, RetentionLimits, RetentionOrigin, RetentionReference,
};
use crate::roots::CasWriteFence;
use crate::store::Store;

/// Additive local tables for evidence and receipts.
///
/// `forgetting_evidence` is an append-only mechanism ledger: a wrong evidence
/// record is corrected by recording a new observation, never by rewriting the
/// old one. `forgetting_receipts` is append-only as well; `receipt_key` only
/// deduplicates an exact replay of the same decision over the same evidence
/// and the same reference exception set.
pub const FORGETTING_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS forgetting_evidence (
    evidence_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    digest TEXT NOT NULL,
    kind TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    active_ms INTEGER NOT NULL,
    expires_at_active_ms INTEGER,
    step_id TEXT,
    created_seq INTEGER NOT NULL,
    CHECK (active_ms >= 0),
    CHECK (expires_at_active_ms IS NULL OR kind = 'expiry'),
    CHECK (expires_at_active_ms IS NULL OR expires_at_active_ms >= active_ms)
);
CREATE INDEX IF NOT EXISTS idx_forgetting_evidence_digest
    ON forgetting_evidence(agent_id, digest, active_ms, observed_at_ms);

CREATE TABLE IF NOT EXISTS forgetting_receipts (
    receipt_id TEXT PRIMARY KEY,
    receipt_key TEXT NOT NULL UNIQUE,
    agent_id TEXT NOT NULL,
    digest TEXT NOT NULL,
    decision TEXT NOT NULL,
    reason TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    references_json TEXT NOT NULL,
    active_ms INTEGER NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    created_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_forgetting_receipts_digest
    ON forgetting_receipts(agent_id, digest, observed_at_ms);
"#;

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

fn decode_id(value: &str, column: usize) -> rusqlite::Result<EntityId> {
    value.parse().map_err(|error: uuid::Error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn decode_digest(value: &str, column: usize) -> rusqlite::Result<Digest> {
    let canonical = value.strip_prefix("blake3:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if canonical {
        Ok(Digest(value.to_string()))
    } else {
        Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(ShenkoError::conflict(
                "forgetting row contains a non-canonical digest",
            )),
        ))
    }
}

/// What was observed about one registered piece of content.
///
/// The kind is a mechanism classification, never a semantic judgement about
/// the content. It carries no weight, threshold or keyword: the policy decides
/// how a class contributes, and the policy is the caller's revisable content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgettingEvidenceKind {
    /// The content was used by the subject (tool use, deliberate consultation).
    Use,
    /// The subject explicitly referenced or quoted this content.
    Reference,
    /// A retrieval/recall query returned this content.
    Retrieval,
    /// A new version of this content line was written. The new digest is a
    /// different publication; the observation here is only that the older one
    /// was still in use at this instant.
    Revision,
    /// An explicit deadline in subject-relative active milliseconds.
    Expiry,
}

impl ForgettingEvidenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Reference => "reference",
            Self::Retrieval => "retrieval",
            Self::Revision => "revision",
            Self::Expiry => "expiry",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ShenkoError> {
        Ok(match value {
            "use" => Self::Use,
            "reference" => Self::Reference,
            "retrieval" => Self::Retrieval,
            "revision" => Self::Revision,
            "expiry" => Self::Expiry,
            other => return Err(ShenkoError::storage(format!(
                "unknown forgetting evidence kind {other}"
            ))),
        })
    }

    fn refreshes(self) -> bool {
        matches!(self, Self::Use | Self::Reference | Self::Retrieval | Self::Revision)
    }
}

/// One durable observation about one (agent, content) pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgettingEvidence {
    pub evidence_id: EntityId,
    pub agent_id: EntityId,
    pub digest: Digest,
    pub kind: ForgettingEvidenceKind,
    /// Wall-clock mechanism timestamp; informational for diagnostics only.
    pub observed_at_ms: i64,
    /// Cumulative subject-relative main-loop active milliseconds when the
    /// observation was made. This is the only clock decay reads.
    pub active_ms: i64,
    /// For [`ForgettingEvidenceKind::Expiry`] only: the deadline on the same
    /// cumulative active-runtime clock.
    #[serde(default)]
    pub expires_at_active_ms: Option<i64>,
    #[serde(default)]
    pub step_id: Option<EntityId>,
}

impl ForgettingEvidence {
    /// Build a refresh observation (`use`/`reference`/`retrieval`/`revision`).
    pub fn refresh(
        agent_id: EntityId,
        digest: Digest,
        kind: ForgettingEvidenceKind,
        active_ms: i64,
        observed_at_ms: i64,
    ) -> Result<Self, ShenkoError> {
        if !kind.refreshes() {
            return Err(ShenkoError::conflict(
                "a refresh evidence record cannot carry the expiry kind",
            ));
        }
        Ok(Self {
            evidence_id: EntityId::new(),
            agent_id,
            digest,
            kind,
            observed_at_ms,
            active_ms,
            expires_at_active_ms: None,
            step_id: None,
        })
    }

    /// Build an explicit expiry observation.
    pub fn expiry(
        agent_id: EntityId,
        digest: Digest,
        active_ms: i64,
        expires_at_active_ms: i64,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            evidence_id: EntityId::new(),
            agent_id,
            digest,
            kind: ForgettingEvidenceKind::Expiry,
            observed_at_ms,
            active_ms,
            expires_at_active_ms: Some(expires_at_active_ms),
            step_id: None,
        }
    }

    pub fn with_step(mut self, step_id: EntityId) -> Self {
        self.step_id = Some(step_id);
        self
    }

    pub fn validate(&self) -> Result<(), ShenkoError> {
        if self.active_ms < 0 || self.observed_at_ms < 0 {
            return Err(ShenkoError::conflict(
                "forgetting evidence times must be nonnegative",
            ));
        }
        match (self.kind, self.expires_at_active_ms) {
            (ForgettingEvidenceKind::Expiry, None) => {
                return Err(ShenkoError::conflict(
                    "expiry evidence requires an active-runtime deadline",
                ));
            }
            (ForgettingEvidenceKind::Expiry, Some(deadline)) if deadline < self.active_ms => {
                return Err(ShenkoError::conflict(
                    "an expiry deadline cannot precede its observation",
                ));
            }
            (kind, Some(_)) if kind != ForgettingEvidenceKind::Expiry => {
                return Err(ShenkoError::conflict(
                    "only expiry evidence may carry a deadline",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

/// One measured sample of the subject's main-loop active runtime.
///
/// The owner of the runtime meter supplies this; this module never reads a
/// wall clock and never fabricates activity. When no measurement exists the
/// caller cannot run an evaluation, and registered content stays unevaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgettingClock {
    /// Cumulative subject-relative active milliseconds, including only time in
    /// which the main inference loop was actually executing.
    pub active_ms: i64,
    /// Wall-clock mechanism timestamp of this sample.
    pub observed_at_ms: i64,
}

impl ForgettingClock {
    pub fn new(active_ms: i64, observed_at_ms: i64) -> Result<Self, ShenkoError> {
        let clock = Self { active_ms, observed_at_ms };
        clock.validate()?;
        Ok(clock)
    }

    pub fn validate(self) -> Result<(), ShenkoError> {
        if self.active_ms < 0 || self.observed_at_ms < 0 {
            return Err(ShenkoError::conflict(
                "a forgetting clock sample must be nonnegative",
            ));
        }
        Ok(())
    }
}

/// Caller-owned, revisable forgetting policy.
///
/// The engine contains no formula of its own: every threshold below is an
/// explicit input. A caller that wants the defaults in
/// `forgetting` definition content can pass them; a subject that revises the
/// policy passes another value. There is no keyword or regular-expression gate
/// anywhere in the decision.
#[derive(Debug, Clone)]
pub struct ForgettingPolicy {
    /// Active milliseconds without a refresh observation after which a
    /// publication is eligible to be forgotten.
    pub decay_after_active_ms: i64,
    /// Maximum registered publications evaluated in one bounded cycle.
    pub max_candidates: usize,
    /// Maximum evidence rows read for one publication.
    pub max_evidence_per_candidate: usize,
    /// How long the cycle may wait for the exclusive CAS publication fence.
    /// `0` refuses immediately when a writer holds the store.
    pub fence_wait_ms: u64,
    /// Finite budgets for the retention-reference inventory.
    pub reference_limits: RetentionLimits,
    /// Whether an eligible, unreferenced cognition publication may have its
    /// CAS bytes released after its registration row is removed. When false,
    /// forgetting unpublishes the registration and keeps the bytes reachable
    /// by digest.
    pub release_unreferenced_bytes: bool,
}

impl Default for ForgettingPolicy {
    fn default() -> Self {
        Self {
            // Seven active days; this is a starting policy, not a kernel rule.
            decay_after_active_ms: 7 * 24 * 60 * 60 * 1_000,
            max_candidates: 256,
            max_evidence_per_candidate: 256,
            fence_wait_ms: 2_000,
            reference_limits: RetentionLimits::default(),
            release_unreferenced_bytes: false,
        }
    }
}

impl ForgettingPolicy {
    pub fn validate(&self) -> Result<(), ShenkoError> {
        let maxima = RetentionLimits::default();
        if self.decay_after_active_ms <= 0
            || self.max_candidates == 0
            || self.max_candidates > 10_000
            || self.max_evidence_per_candidate == 0
            || self.max_evidence_per_candidate > 4_096
            || self.fence_wait_ms > 60_000
            || self.reference_limits.rows == 0
            || self.reference_limits.rows > maxima.rows
            || self.reference_limits.row_bytes == 0
            || self.reference_limits.row_bytes > maxima.row_bytes
            || self.reference_limits.json_bytes == 0
            || self.reference_limits.json_bytes > maxima.json_bytes
            || self.reference_limits.read_bytes == 0
            || self.reference_limits.read_bytes > maxima.read_bytes
            || self.reference_limits.references == 0
            || self.reference_limits.references > maxima.references
            || self.reference_limits.depth == 0
            || self.reference_limits.depth > maxima.depth
        {
            return Err(ShenkoError::conflict(
                "forgetting policy must be positive and within the reader's finite maxima",
            ));
        }
        Ok(())
    }
}

/// What one cycle decided about one publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgettingDecisionKind {
    /// The registration was removed; the tombstone is durable.
    Forgotten,
    /// A durable artifact still references the content, so both the
    /// registration and its bytes stay (retention exception).
    Retained,
    /// There was not enough honest evidence to decide. Nothing changed.
    Unevaluated,
}

impl ForgettingDecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forgotten => "forgotten",
            Self::Retained => "retained",
            Self::Unevaluated => "unevaluated",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ShenkoError> {
        Ok(match value {
            "forgotten" => Self::Forgotten,
            "retained" => Self::Retained,
            "unevaluated" => Self::Unevaluated,
            other => {
                return Err(ShenkoError::storage(format!(
                    "unknown forgetting decision {other}"
                )));
            }
        })
    }
}

/// One durable decision receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgettingReceipt {
    pub receipt_id: EntityId,
    /// Stable deduplication key: agent, digest, decision, exact evidence ids
    /// and the exact reference-origin set.
    pub receipt_key: String,
    pub agent_id: EntityId,
    pub digest: Digest,
    pub decision: ForgettingDecisionKind,
    pub reason: String,
    /// JSON array of the evidence rows this decision read.
    pub evidence_json: String,
    /// JSON array of retention-reference origins for a retained decision.
    pub references_json: String,
    pub active_ms: i64,
    pub observed_at_ms: i64,
    pub created_seq: i64,
}

/// One publication's evaluated outcome.
#[derive(Debug, Clone)]
pub struct ForgettingOutcome {
    pub digest: Digest,
    pub decision: ForgettingDecisionKind,
    pub reason: String,
    pub last_refresh_active_ms: Option<i64>,
    pub expiry_active_ms: Option<i64>,
    pub references: Vec<RetentionReference>,
    pub tombstone_id: Option<EntityId>,
    pub receipt_id: Option<EntityId>,
    pub object_released: bool,
}

/// One bounded cycle's report. No field claims a wall-clock decay: the
/// `active_ms` and `observed_at_ms` are exactly the caller's sample.
#[derive(Debug, Clone, Default)]
pub struct ForgettingCycle {
    pub inventory_complete: bool,
    /// Completeness of the second, fence-held inventory used for byte
    /// release. It stays false when the release pass was skipped.
    pub release_inventory_complete: bool,
    pub fence_busy: bool,
    pub candidates: usize,
    pub skipped_budget: usize,
    pub kept: usize,
    pub forgotten: usize,
    pub retained: usize,
    pub unevaluated: usize,
    /// Objects whose registration had already been forgotten in an earlier
    /// cycle (for example a crash between commit and byte release) and whose
    /// bytes were released by this cycle.
    pub pending_released: usize,
    pub outcomes: Vec<ForgettingOutcome>,
}

impl Store {
    /// Record one durable evidence observation for a registered publication.
    ///
    /// A replay with the same evidence identity and the same fields is an
    /// audited no-op; a different payload for the same identity is a persistent
    /// conflict. Evidence never rewrites an older observation.
    pub fn record_forgetting_evidence(
        &self,
        evidence: &ForgettingEvidence,
    ) -> Result<(), ShenkoError> {
        evidence.validate()?;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let registered: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM cognition_contents WHERE digest=?1)",
                        params![evidence.digest.0],
                        |row| row.get(0),
                    )
                    .map_err(storage)?;
                if !registered {
                    return Err(ShenkoError::not_found(format!(
                        "forgetting evidence names unregistered content {}",
                        evidence.digest
                    )));
                }
                let existing = tx
                    .query_row(
                        "SELECT agent_id,digest,kind,observed_at_ms,active_ms,\
                                expires_at_active_ms,step_id
                         FROM forgetting_evidence WHERE evidence_id=?1",
                        params![evidence.evidence_id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, i64>(4)?,
                                row.get::<_, Option<i64>>(5)?,
                                row.get::<_, Option<String>>(6)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(storage)?;
                if let Some((agent_id, digest, kind, observed_at_ms, active_ms, expires, step)) =
                    existing
                {
                    let same = agent_id == evidence.agent_id.to_string()
                        && digest == evidence.digest.0
                        && kind == evidence.kind.as_str()
                        && observed_at_ms == evidence.observed_at_ms
                        && active_ms == evidence.active_ms
                        && expires == evidence.expires_at_active_ms
                        && step == evidence.step_id.map(|id| id.to_string());
                    return if same {
                        Ok(())
                    } else {
                        Err(ShenkoError::conflict(
                            "forgetting evidence identity exists with a different observation",
                        ))
                    };
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO forgetting_evidence
                     (evidence_id,agent_id,digest,kind,observed_at_ms,active_ms,
                      expires_at_active_ms,step_id,created_seq)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        evidence.evidence_id.to_string(),
                        evidence.agent_id.to_string(),
                        evidence.digest.0,
                        evidence.kind.as_str(),
                        evidence.observed_at_ms,
                        evidence.active_ms,
                        evidence.expires_at_active_ms,
                        evidence.step_id.map(|id| id.to_string()),
                        seq.0 as i64,
                    ],
                )
                .map_err(storage)?;
                self.ledger.record_commit(&tx, seq, 1)?;
                tx.commit().map_err(storage)?;
                Ok(())
            },
            |error: &ShenkoError| {
                matches!(
                    error,
                    ShenkoError::Io(_) | ShenkoError::Serde(_) | ShenkoError::Storage(_)
                )
            },
        )
    }

    /// Evidence rows for one (agent, content) pair, newest active observation
    /// first. The read is bounded by the caller's limit.
    pub fn forgetting_evidence(
        &self,
        agent_id: EntityId,
        digest: &Digest,
        limit: usize,
    ) -> Result<Vec<ForgettingEvidence>, ShenkoError> {
        if limit == 0 || limit > 4_096 {
            return Err(ShenkoError::conflict(
                "forgetting evidence read limit must be positive and bounded",
            ));
        }
        let conn = self.ledger.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT evidence_id,agent_id,digest,kind,observed_at_ms,active_ms,\
                        expires_at_active_ms,step_id
                 FROM forgetting_evidence
                 WHERE agent_id=?1 AND digest=?2
                 ORDER BY active_ms DESC, observed_at_ms DESC, evidence_id DESC
                 LIMIT ?3",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(
                params![agent_id.to_string(), digest.0, limit as i64],
                |row| {
                    Ok(ForgettingEvidence {
                        evidence_id: decode_id(&row.get::<_, String>(0)?, 0)?,
                        agent_id: decode_id(&row.get::<_, String>(1)?, 1)?,
                        digest: decode_digest(&row.get::<_, String>(2)?, 2)?,
                        kind: ForgettingEvidenceKind::parse(&row.get::<_, String>(3)?)
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    3,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?,
                        observed_at_ms: row.get(4)?,
                        active_ms: row.get(5)?,
                        expires_at_active_ms: row.get(6)?,
                        step_id: row
                            .get::<_, Option<String>>(7)?
                            .map(|value| decode_id(&value, 7))
                            .transpose()?,
                    })
                },
            )
            .map_err(storage)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)
    }

    /// The bounded periodic entry point: evaluate at most
    /// `policy.max_candidates` registered publications and return.
    ///
    /// It never sleeps, retries, spawns or reads a wall clock. Callers schedule
    /// the next cycle. Registration/tombstone decisions use the ordinary
    /// mutation line; the exclusive CAS publication fence is taken only for
    /// the final byte-release pass, so a removal cannot race a writer that is
    /// sealing and publishing a reference. When the fence is busy the
    /// registration decisions stay durable, byte release is skipped, and the
    /// cycle reports `fence_busy`; the next cycle retries the release from the
    /// tombstones.
    pub fn run_forgetting_cycle(
        &self,
        agent_id: EntityId,
        policy: &ForgettingPolicy,
        clock: ForgettingClock,
    ) -> Result<ForgettingCycle, ShenkoError> {
        policy.validate()?;
        clock.validate()?;
        let mut report = ForgettingCycle::default();

        let snapshot = self
            .retained_object_snapshot(policy.reference_limits.clone())
            .map_err(|error| {
                ShenkoError::storage(format!("forgetting retention inventory: {error:#}"))
            })?;
        report.inventory_complete = snapshot.complete;
        let references = reference_map(&snapshot);

        let candidates = self.forgetting_candidates(policy.max_candidates.saturating_add(1))?;
        report.candidates = candidates.len().min(policy.max_candidates);
        report.skipped_budget = candidates.len().saturating_sub(policy.max_candidates);
        for digest in candidates.iter().take(policy.max_candidates) {
            let evidence =
                self.forgetting_evidence(agent_id, digest, policy.max_evidence_per_candidate)?;
            let evaluation = evaluate_candidate(&evidence, policy, clock);
            let refs: Vec<RetentionReference> = references
                .get(digest)
                .map(|refs| {
                    refs.iter()
                        .filter(|reference| !forgetting_bookkeeping(reference))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            if !evaluation.eligible {
                if evaluation.decided {
                    report.kept += 1;
                } else {
                    report.unevaluated += 1;
                    let (receipt_id, _) = self.record_decision_receipt(
                        agent_id,
                        digest,
                        ForgettingDecisionKind::Unevaluated,
                        &evaluation.reason,
                        &evidence,
                        &refs,
                        clock,
                    )?;
                    report.outcomes.push(ForgettingOutcome {
                        digest: digest.clone(),
                        decision: ForgettingDecisionKind::Unevaluated,
                        reason: evaluation.reason,
                        last_refresh_active_ms: evaluation.last_refresh_active_ms,
                        expiry_active_ms: evaluation.expiry_active_ms,
                        references: refs,
                        tombstone_id: None,
                        receipt_id: Some(receipt_id),
                        object_released: false,
                    });
                }
                continue;
            }

            if !snapshot.complete {
                report.unevaluated += 1;
                let reason = "retention inventory is incomplete; bytes and publication are kept";
                let (receipt_id, _) = self.record_decision_receipt(
                    agent_id,
                    digest,
                    ForgettingDecisionKind::Unevaluated,
                    reason,
                    &evidence,
                    &refs,
                    clock,
                )?;
                report.outcomes.push(ForgettingOutcome {
                    digest: digest.clone(),
                    decision: ForgettingDecisionKind::Unevaluated,
                    reason: reason.to_string(),
                    last_refresh_active_ms: evaluation.last_refresh_active_ms,
                    expiry_active_ms: evaluation.expiry_active_ms,
                    references: refs,
                    tombstone_id: None,
                    receipt_id: Some(receipt_id),
                    object_released: false,
                });
                continue;
            }

            if !refs.is_empty() {
                report.retained += 1;
                let reason = "a retained artifact still references this content";
                let (receipt_id, _) = self.record_decision_receipt(
                    agent_id,
                    digest,
                    ForgettingDecisionKind::Retained,
                    reason,
                    &evidence,
                    &refs,
                    clock,
                )?;
                report.outcomes.push(ForgettingOutcome {
                    digest: digest.clone(),
                    decision: ForgettingDecisionKind::Retained,
                    reason: reason.to_string(),
                    last_refresh_active_ms: evaluation.last_refresh_active_ms,
                    expiry_active_ms: evaluation.expiry_active_ms,
                    references: refs,
                    tombstone_id: None,
                    receipt_id: Some(receipt_id),
                    object_released: false,
                });
                continue;
            }

            match self.unpublish_forgotten(
                agent_id,
                digest,
                policy,
                clock,
                &evaluation,
                &evidence,
            )? {
                UnpublishOutcome::Forgotten {
                    tombstone_id,
                    receipt_id,
                    object_released,
                } => {
                    report.forgotten += 1;
                    report.outcomes.push(ForgettingOutcome {
                        digest: digest.clone(),
                        decision: ForgettingDecisionKind::Forgotten,
                        reason: evaluation.reason.clone(),
                        last_refresh_active_ms: evaluation.last_refresh_active_ms,
                        expiry_active_ms: evaluation.expiry_active_ms,
                        references: Vec::new(),
                        tombstone_id: Some(tombstone_id),
                        receipt_id: Some(receipt_id),
                        object_released,
                    });
                }
                UnpublishOutcome::Retained { references, reason } => {
                    report.retained += 1;
                    let (receipt_id, _) = self.record_decision_receipt(
                        agent_id,
                        digest,
                        ForgettingDecisionKind::Retained,
                        &reason,
                        &evidence,
                        &references,
                        clock,
                    )?;
                    report.outcomes.push(ForgettingOutcome {
                        digest: digest.clone(),
                        decision: ForgettingDecisionKind::Retained,
                        reason,
                        last_refresh_active_ms: evaluation.last_refresh_active_ms,
                        expiry_active_ms: evaluation.expiry_active_ms,
                        references,
                        tombstone_id: None,
                        receipt_id: Some(receipt_id),
                        object_released: false,
                    });
                }
                UnpublishOutcome::AlreadyGone { receipt_id } => {
                    report.forgotten += 1;
                    report.outcomes.push(ForgettingOutcome {
                        digest: digest.clone(),
                        decision: ForgettingDecisionKind::Forgotten,
                        reason: "the publication was already removed by a concurrent cycle"
                            .to_string(),
                        last_refresh_active_ms: evaluation.last_refresh_active_ms,
                        expiry_active_ms: evaluation.expiry_active_ms,
                        references: Vec::new(),
                        tombstone_id: None,
                        receipt_id,
                        object_released: false,
                    });
                }
            }
        }

        // Byte release is the only step that can lose an unremembered object,
        // so it is isolated behind the exclusive publication fence and a
        // *fresh* inventory. A writer that published a new reference after
        // the evaluation is therefore seen, and the object is retained.
        if policy.release_unreferenced_bytes {
            match CasWriteFence::acquire(self.cas.base(), policy.fence_wait_ms) {
                Ok(fence) => {
                    let fresh = self
                        .retained_object_snapshot(policy.reference_limits.clone())
                        .map_err(|error| {
                            ShenkoError::storage(format!(
                                "forgetting release inventory: {error:#}"
                            ))
                        })?;
                    let fresh_references = reference_map(&fresh);
                    let released =
                        self.release_pending_forgotten(clock, &fresh_references, fresh.complete)?;
                    report.release_inventory_complete = fresh.complete;
                    let released_set: std::collections::BTreeSet<Digest> =
                        released.iter().cloned().collect();
                    for outcome in &mut report.outcomes {
                        if released_set.contains(&outcome.digest) {
                            outcome.object_released = true;
                        }
                    }
                    report.pending_released = released.len();
                    drop(fence);
                }
                Err(error) if error.to_string().contains("CAS_ROOT_OWNER_BUSY") => {
                    report.fence_busy = true;
                }
                Err(error) => {
                    return Err(ShenkoError::storage(format!(
                        "forgetting cycle could not take the CAS fence: {error:#}"
                    )));
                }
            }
        }
        Ok(report)
    }

    /// Decision receipts for the subject, newest first, optionally narrowed to
    /// one content digest.
    pub fn forgetting_receipts(
        &self,
        agent_id: EntityId,
        digest: Option<&Digest>,
        limit: usize,
    ) -> Result<Vec<ForgettingReceipt>, ShenkoError> {
        if limit == 0 || limit > 10_000 {
            return Err(ShenkoError::conflict(
                "forgetting receipt read limit must be positive and bounded",
            ));
        }
        let conn = self.ledger.conn.lock();
        let (sql, digest_param) = match digest {
            Some(_) => (
                "SELECT receipt_id,receipt_key,agent_id,digest,decision,reason,evidence_json,\
                        references_json,active_ms,observed_at_ms,created_seq
                 FROM forgetting_receipts WHERE agent_id=?1 AND digest=?2
                 ORDER BY observed_at_ms DESC, receipt_id DESC LIMIT ?3",
                Some(digest.unwrap().0.clone()),
            ),
            None => (
                "SELECT receipt_id,receipt_key,agent_id,digest,decision,reason,evidence_json,\
                        references_json,active_ms,observed_at_ms,created_seq
                 FROM forgetting_receipts WHERE agent_id=?1
                 ORDER BY observed_at_ms DESC, receipt_id DESC LIMIT ?2",
                None,
            ),
        };
        let mut statement = conn.prepare(sql).map_err(storage)?;
        let mut receipts = Vec::new();
        match digest_param {
            Some(digest) => {
                let rows = statement
                    .query_map(
                        params![agent_id.to_string(), digest, limit as i64],
                        decode_receipt,
                    )
                    .map_err(storage)?;
                for row in rows {
                    receipts.push(row.map_err(storage)?);
                }
            }
            None => {
                let rows = statement
                    .query_map(params![agent_id.to_string(), limit as i64], decode_receipt)
                    .map_err(storage)?;
                for row in rows {
                    receipts.push(row.map_err(storage)?);
                }
            }
        }
        Ok(receipts)
    }

    fn forgetting_candidates(&self, limit: usize) -> Result<Vec<Digest>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT c.digest FROM cognition_contents c
                 WHERE NOT EXISTS (
                    SELECT 1 FROM retention_tombstones t
                    WHERE t.target_kind='object'
                      AND (t.target_id=c.digest
                           OR instr(','||t.target_id||',', ','||c.digest||',') > 0)
                 )
                 ORDER BY c.registered_at_ms, c.digest
                 LIMIT ?1",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![limit as i64], |row| {
                decode_digest(&row.get::<_, String>(0)?, 0)
            })
            .map_err(storage)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)
    }

    fn record_decision_receipt(
        &self,
        agent_id: EntityId,
        digest: &Digest,
        decision: ForgettingDecisionKind,
        reason: &str,
        evidence: &[ForgettingEvidence],
        references: &[RetentionReference],
        clock: ForgettingClock,
    ) -> Result<(EntityId, bool), ShenkoError> {
        let receipt = make_receipt(
            agent_id,
            digest,
            decision,
            reason,
            evidence,
            references,
            clock,
        )?;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let (receipt_id, inserted) =
                    Self::insert_receipt_tx(&tx, &receipt, seq.0 as i64)?;
                if inserted {
                    self.ledger.record_commit(&tx, seq, 1)?;
                    tx.commit().map_err(storage)?;
                } else {
                    drop(tx);
                }
                Ok((receipt_id, inserted))
            },
            |error: &ShenkoError| {
                matches!(
                    error,
                    ShenkoError::Io(_) | ShenkoError::Serde(_) | ShenkoError::Storage(_)
                )
            },
        )
    }

    /// Insert a receipt unless its exact deduplication key already exists.
    /// Returns the durable receipt identity and whether this call inserted it.
    fn insert_receipt_tx(
        tx: &rusqlite::Transaction<'_>,
        receipt: &ForgettingReceipt,
        created_seq: i64,
    ) -> Result<(EntityId, bool), ShenkoError> {
        let existing: Option<String> = tx
            .query_row(
                "SELECT receipt_id FROM forgetting_receipts WHERE receipt_key=?1",
                params![receipt.receipt_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        if let Some(existing) = existing {
            let receipt_id: EntityId = existing.parse().map_err(storage)?;
            return Ok((receipt_id, false));
        }
        tx.execute(
            "INSERT INTO forgetting_receipts
             (receipt_id,receipt_key,agent_id,digest,decision,reason,evidence_json,
              references_json,active_ms,observed_at_ms,created_seq)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                receipt.receipt_id.to_string(),
                receipt.receipt_key,
                receipt.agent_id.to_string(),
                receipt.digest.0,
                receipt.decision.as_str(),
                receipt.reason,
                receipt.evidence_json,
                receipt.references_json,
                receipt.active_ms,
                receipt.observed_at_ms,
                created_seq,
            ],
        )
        .map_err(storage)?;
        Ok((receipt.receipt_id, true))
    }

    fn unpublish_forgotten(
        &self,
        agent_id: EntityId,
        digest: &Digest,
        policy: &ForgettingPolicy,
        clock: ForgettingClock,
        evaluation: &Evaluation,
        evidence: &[ForgettingEvidence],
    ) -> Result<UnpublishOutcome, ShenkoError> {
        let receipt = make_receipt(
            agent_id,
            digest,
            ForgettingDecisionKind::Forgotten,
            &evaluation.reason,
            evidence,
            &[],
            clock,
        )?;
        let object = self.content(digest)?.map(|content| content.object);
        let Some(object) = object else {
            let (receipt_id, _) = self.record_decision_receipt(
                agent_id,
                digest,
                ForgettingDecisionKind::Forgotten,
                "the publication was already removed",
                evidence,
                &[],
                clock,
            )?;
            return Ok(UnpublishOutcome::AlreadyGone {
                receipt_id: Some(receipt_id),
            });
        };

        let effect_record = serde_json::to_string(&serde_json::json!({
            "mechanism": "evidence_forgetting",
            "agent_id": agent_id.to_string(),
            "digest": digest.to_string(),
            "decision": "forgotten",
            "reason": evaluation.reason.clone(),
            "last_refresh_active_ms": evaluation.last_refresh_active_ms,
            "expiry_active_ms": evaluation.expiry_active_ms,
            "active_ms": clock.active_ms,
            "observed_at_ms": clock.observed_at_ms,
            "registration_removed": true,
            "byte_release_requested": policy.release_unreferenced_bytes
                && object.store_namespace == COGNITION_CONTENT_NAMESPACE,
            "references": serde_json::Value::Array(Vec::new()),
        }))
        .map_err(storage)?;
        let tombstone_id = EntityId::new();

        let outcome = self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage)?;

                if Self::direct_references(&tx, digest)? {
                    drop(tx);
                    return Ok(UnpublishOutcome::Retained {
                        references: Vec::new(),
                        reason: "a durable cognition row directly references this content"
                            .to_string(),
                    });
                }
                let deleted = tx
                    .execute(
                        "DELETE FROM cognition_contents WHERE digest=?1",
                        params![digest.0],
                    )
                    .map_err(storage)?;
                if deleted == 0 {
                    drop(tx);
                    return Ok(UnpublishOutcome::AlreadyGone { receipt_id: None });
                }

                // A publication is only unpublishable when no replicated
                // archive row still reaches it. This is evaluated *after* the
                // registration row is gone, so the row that is being removed
                // cannot name itself as a reason to keep the object.
                match crate::roots::referenced_objects(&tx, self.cas.as_ref()) {
                    Ok(roots)
                        if roots.digests.contains(digest) || roots.missing.contains(digest) =>
                    {
                        drop(tx);
                        return Ok(UnpublishOutcome::Retained {
                            references: Vec::new(),
                            reason: "an archive/carrier root still references this content"
                                .to_string(),
                        });
                    }
                    Ok(_) => {}
                    Err(error) => {
                        drop(tx);
                        return Ok(UnpublishOutcome::Retained {
                            references: Vec::new(),
                            reason: format!(
                                "the archive-root audit failed; content is retained: {error:#}"
                            ),
                        });
                    }
                }

                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO retention_tombstones
                     (tombstone_id,target_kind,target_id,scope,reason,effect_record,deleted_at_ms)
                     VALUES (?1,'object',?2,'cognition_content',?3,?4,?5)",
                    params![
                        tombstone_id.to_string(),
                        digest.0,
                        evaluation.reason.as_str(),
                        effect_record,
                        clock.observed_at_ms,
                    ],
                )
                .map_err(storage)?;
                let (receipt_id, inserted) =
                    Self::insert_receipt_tx(&tx, &receipt, seq.0 as i64)?;
                self.ledger
                    .record_commit(&tx, seq, 1 + if inserted { 1 } else { 0 })?;
                tx.commit().map_err(storage)?;
                Ok(UnpublishOutcome::Forgotten {
                    tombstone_id,
                    receipt_id,
                    object_released: false,
                })
            },
            |error: &ShenkoError| {
                matches!(
                    error,
                    ShenkoError::Io(_) | ShenkoError::Serde(_) | ShenkoError::Storage(_)
                )
            },
        )?;

        Ok(outcome)
    }

    fn direct_references(
        tx: &rusqlite::Transaction<'_>,
        digest: &Digest,
    ) -> Result<bool, ShenkoError> {
        let referenced: bool = tx
            .query_row(
                "SELECT
                    EXISTS(SELECT 1 FROM cognition_attitudes
                           WHERE digest=?1 OR supersedes_content=?1)
                 OR EXISTS(SELECT 1 FROM cognition_legacy_content
                           WHERE content_digest=?1)
                 OR EXISTS(SELECT 1 FROM cognitive_root_nodes
                           WHERE content_digest=?1)
                 OR EXISTS(SELECT 1 FROM cognitive_root_requests
                           WHERE content_digest=?1)",
                params![digest.0],
                |row| row.get(0),
            )
            .map_err(storage)?;
        Ok(referenced)
    }

    fn release_pending_forgotten(
        &self,
        clock: ForgettingClock,
        references: &std::collections::BTreeMap<Digest, Vec<RetentionReference>>,
        inventory_complete: bool,
    ) -> Result<Vec<Digest>, ShenkoError> {
        if !inventory_complete {
            return Ok(Vec::new());
        }
        let tombstones = self.forgotten_cognition_tombstones(4096)?;
        let mut released = Vec::new();
        for (digest, release_requested) in tombstones {
            if !release_requested {
                continue;
            }
            if self.content(&digest)?.is_some() || !self.cas.object_exists(&digest) {
                continue;
            }
            if references
                .get(&digest)
                .is_some_and(|refs| refs.iter().any(|reference| !forgetting_bookkeeping(reference)))
            {
                continue;
            }
            match self.cas.remove_object(&digest) {
                Ok(true) => released.push(digest),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        %digest,
                        %error,
                        %clock.observed_at_ms,
                        "pending forgotten object could not be released; it stays reachable"
                    );
                }
            }
        }
        Ok(released)
    }

    fn forgotten_cognition_tombstones(
        &self,
        limit: usize,
    ) -> Result<Vec<(Digest, bool)>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT target_id,effect_record FROM retention_tombstones
                 WHERE target_kind='object' AND scope='cognition_content'
                   AND instr(target_id, ',') = 0
                 ORDER BY deleted_at_ms DESC, tombstone_id DESC
                 LIMIT ?1",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![limit as i64], |row| {
                let digest = decode_digest(&row.get::<_, String>(0)?, 0)?;
                let effect = row.get::<_, String>(1)?;
                let requested = serde_json::from_str::<serde_json::Value>(&effect)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("byte_release_requested")
                            .and_then(|requested| requested.as_bool())
                    })
                    .unwrap_or(false);
                Ok((digest, requested))
            })
            .map_err(storage)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(storage)
    }
}

fn decode_receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<ForgettingReceipt> {
    Ok(ForgettingReceipt {
        receipt_id: decode_id(&row.get::<_, String>(0)?, 0)?,
        receipt_key: row.get(1)?,
        agent_id: decode_id(&row.get::<_, String>(2)?, 2)?,
        digest: decode_digest(&row.get::<_, String>(3)?, 3)?,
        decision: ForgettingDecisionKind::parse(&row.get::<_, String>(4)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        reason: row.get(5)?,
        evidence_json: row.get(6)?,
        references_json: row.get(7)?,
        active_ms: row.get(8)?,
        observed_at_ms: row.get(9)?,
        created_seq: row.get(10)?,
    })
}

/// The evaluated state of one candidate. `decided` is true when evidence
/// exists and the clock can be compared; `eligible` says whether decay or an
/// explicit deadline has been reached.
struct Evaluation {
    eligible: bool,
    decided: bool,
    last_refresh_active_ms: Option<i64>,
    expiry_active_ms: Option<i64>,
    reason: String,
}

fn reference_map(
    snapshot: &RetainedObjectSnapshot,
) -> std::collections::BTreeMap<Digest, Vec<RetentionReference>> {
    let mut references: std::collections::BTreeMap<Digest, Vec<RetentionReference>> =
        std::collections::BTreeMap::new();
    for reference in &snapshot.references {
        references
            .entry(reference.digest.clone())
            .or_default()
            .push(reference.clone());
    }
    references
}

fn evaluate_candidate(
    evidence: &[ForgettingEvidence],
    policy: &ForgettingPolicy,
    clock: ForgettingClock,
) -> Evaluation {
    let last_refresh = evidence
        .iter()
        .filter(|record| record.kind.refreshes())
        .map(|record| record.active_ms)
        .max();
    let expiry = evidence
        .iter()
        .filter(|record| record.kind == ForgettingEvidenceKind::Expiry)
        .filter_map(|record| record.expires_at_active_ms)
        .max();

    if last_refresh.is_none() && expiry.is_none() {
        return Evaluation {
            eligible: false,
            decided: false,
            last_refresh_active_ms: None,
            expiry_active_ms: None,
            reason: "no use, reference, retrieval, revision or expiry evidence".to_string(),
        };
    }
    if last_refresh.is_some_and(|refresh| refresh > clock.active_ms) {
        return Evaluation {
            eligible: false,
            decided: false,
            last_refresh_active_ms: last_refresh,
            expiry_active_ms: expiry,
            reason: format!(
                "the clock sample ({}) precedes the newest evidence ({}); no decay is claimed",
                clock.active_ms,
                last_refresh.unwrap_or_default()
            ),
        };
    }

    let decayed = last_refresh.is_some_and(|refresh| {
        clock.active_ms.saturating_sub(refresh) >= policy.decay_after_active_ms
    });
    // A refresh at or after the deadline supersedes that deadline: the subject
    // used the content again, so the old expiry no longer describes it.
    let deadline_passed = expiry.is_some_and(|deadline| {
        clock.active_ms >= deadline && last_refresh.is_none_or(|refresh| refresh < deadline)
    });
    if !decayed && !deadline_passed {
        return Evaluation {
            eligible: false,
            decided: true,
            last_refresh_active_ms: last_refresh,
            expiry_active_ms: expiry,
            reason: "the content was refreshed or its deadline has not been reached".to_string(),
        };
    }

    let reason = match (decayed, deadline_passed) {
        (true, true) => format!(
            "no refresh within {} active ms and the explicit deadline was reached",
            policy.decay_after_active_ms
        ),
        (true, false) => format!(
            "no refresh within {} active ms of subject-relative running time",
            policy.decay_after_active_ms
        ),
        (false, true) => "the explicit active-runtime expiry deadline was reached".to_string(),
        (false, false) => unreachable!("eligibility requires one expiry cause"),
    };
    Evaluation {
        eligible: true,
        decided: true,
        last_refresh_active_ms: last_refresh,
        expiry_active_ms: expiry,
        reason,
    }
}

/// Bookkeeping rows that name a digest because this module is evaluating it,
/// not because a retained artifact needs it. The publication row itself is
/// excluded because unpublishing is exactly the removal of that row; every
/// other reference carrier is a retention exception.
fn forgetting_bookkeeping(reference: &RetentionReference) -> bool {
    match &reference.origin {
        RetentionOrigin::Row { table, .. } => {
            table.as_str() == "cognition_contents"
                || table.starts_with("forgetting_")
                // The tombstone row is the durable authorisation for this
                // release; a pre-existing tombstone also removes the content
                // from the candidate query, so it cannot manufacture a
                // retention exception for the object it retires.
                || table.as_str() == "retention_tombstones"
        }
        RetentionOrigin::Object { .. } | RetentionOrigin::Schema { .. } => false,
    }
}

fn make_receipt(
    agent_id: EntityId,
    digest: &Digest,
    decision: ForgettingDecisionKind,
    reason: &str,
    evidence: &[ForgettingEvidence],
    references: &[RetentionReference],
    clock: ForgettingClock,
) -> Result<ForgettingReceipt, ShenkoError> {
    let evidence_json = serde_json::to_string(evidence).map_err(storage)?;
    let references_json = serde_json::to_string(references).map_err(storage)?;
    let mut evidence_ids = evidence
        .iter()
        .map(|record| record.evidence_id.to_string())
        .collect::<Vec<_>>();
    evidence_ids.sort();
    let mut origin_keys = references
        .iter()
        .map(|reference| serde_json::to_string(&reference.origin).unwrap_or_default())
        .collect::<Vec<_>>();
    origin_keys.sort();
    let key_bytes = format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        agent_id.to_string(),
        digest.to_string(),
        decision.as_str(),
        evidence_ids.join(","),
        origin_keys.join(",")
    );
    Ok(ForgettingReceipt {
        receipt_id: EntityId::new(),
        receipt_key: Digest::blake3(key_bytes.as_bytes()).0,
        agent_id,
        digest: digest.clone(),
        decision,
        reason: reason.to_string(),
        evidence_json,
        references_json,
        active_ms: clock.active_ms,
        observed_at_ms: clock.observed_at_ms,
        created_seq: 0,
    })
}

enum UnpublishOutcome {
    Forgotten {
        tombstone_id: EntityId,
        receipt_id: EntityId,
        object_released: bool,
    },
    Retained {
        references: Vec<RetentionReference>,
        reason: String,
    },
    AlreadyGone {
        receipt_id: Option<EntityId>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_rejects_a_deadline_on_a_refresh_kind() {
        let mut evidence = ForgettingEvidence::refresh(
            EntityId::new(),
            Digest::blake3(b"content"),
            ForgettingEvidenceKind::Use,
            10,
            1,
        )
        .unwrap();
        evidence.expires_at_active_ms = Some(20);
        assert!(evidence.validate().is_err());
    }

    #[test]
    fn evidence_rejects_a_deadline_before_its_observation() {
        let evidence =
            ForgettingEvidence::expiry(EntityId::new(), Digest::blake3(b"content"), 10, 5, 1);
        assert!(evidence.validate().is_err());
    }

    #[test]
    fn decay_needs_a_refresh_and_an_active_clock() {
        let agent = EntityId::new();
        let digest = Digest::blake3(b"content");
        let policy = ForgettingPolicy {
            decay_after_active_ms: 100,
            ..ForgettingPolicy::default()
        };
        let clock = ForgettingClock::new(150, 1).unwrap();
        assert!(!evaluate_candidate(&[], &policy, clock).decided);
        let used = vec![
            ForgettingEvidence::refresh(agent, digest.clone(), ForgettingEvidenceKind::Use, 0, 1)
                .unwrap(),
        ];
        let fresh = evaluate_candidate(&used, &policy, ForgettingClock::new(99, 2).unwrap());
        assert!(fresh.decided && !fresh.eligible);
        let stale = evaluate_candidate(&used, &policy, clock);
        assert!(stale.decided && stale.eligible);
    }
}
