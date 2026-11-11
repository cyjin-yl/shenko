//! Durable main-request receipts — docs/specs/subject-runtime.md#spec-41-5, docs/specs/durable-state.md#spec-41-4 (X03/X11/X12).
//!
//! The subject's own finite model generation uses the same durable
//! task/operation/attempt discipline as a child: before any wire send, the
//! exact request identity (request_id + attempt_id + selected send), the
//! sealed input plan and the payload/provider digests are committed in L.
//! The broker's receipt sink records the terminal provider evidence
//! independently of whether the caller is still waiting, so a late or lost
//! caller never loses the result and never gains a reason to replay. An
//! unknown outcome stays unknown: the armed quota send is never settled and
//! the step is reconciled, not blindly re-dispatched.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::{Digest, ObjectRef};

use crate::store::{Store, storage_error};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS subject_request_attempts (
    request_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    send_id TEXT,
    state TEXT NOT NULL,
    input_plan_ref TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    provider_binding_digest TEXT NOT NULL,
    data TEXT NOT NULL,
    created_commit_seq INTEGER NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_subject_request_step
    ON subject_request_attempts(agent_id, step_id);
"#;

/// The durable lifecycle of one main-request attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectRequestState {
    /// Committed before submission; the broker has not executed the send
    /// boundary yet. A crash here proves the request never dispatched.
    Armed,
    /// The broker's send boundary passed; the request may be in flight. A
    /// crash here leaves the outcome unknown until terminal evidence lands.
    Dispatched,
    /// Terminal provider evidence is durably recorded; `terminal` is set.
    Terminal,
}

/// The terminal evidence of one main-request attempt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubjectRequestTerminal {
    /// `provider_terminal` keeps the stop reason; `never_dispatched` proves
    /// no wire send happened; `transport_rejected` keeps the failure class;
    /// `unknown` stays recoverable debt and is never renamed to failure.
    pub outcome: serde_json::Value,
    /// Sealed original `InferenceResult`, when a result object exists.
    pub result_ref: Option<ObjectRef>,
    pub recorded_commit: CommitRef,
}

/// One durable main-request attempt (schema 1). The same request_id is the
/// single identity of the sealed step's wire request; a new attempt of the
/// same step uses a new request_id under a new attempt row.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubjectRequestAttempt {
    pub schema_version: u32,
    pub request_id: EntityId,
    pub attempt_id: EntityId,
    pub agent_id: EntityId,
    pub step_id: EntityId,
    pub send_id: Option<EntityId>,
    pub state: SubjectRequestState,
    pub input_plan_ref: ObjectRef,
    pub payload_digest: Digest,
    pub provider_binding_digest: Digest,
    pub terminal: Option<SubjectRequestTerminal>,
    pub created_commit: CommitRef,
}

impl Store {
    /// Commit the armed attempt before any wire send. Idempotent on
    /// `request_id`: an existing identical row replays its commit; the same
    /// request_id with different facts is a conflict that never rewrites the
    /// original record. The arm commits in the SAME transaction as the step's
    /// transition into `AwaitingModel` when called inside one; the standalone
    /// form is the recovery path that re-arms a retried attempt.
    pub fn arm_subject_request(
        &self,
        attempt: &SubjectRequestAttempt,
    ) -> Result<CommitRef, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_arm_subject_request(attempt),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_arm_subject_request(
        &self,
        attempt: &SubjectRequestAttempt,
    ) -> Result<CommitRef, ShenkoError> {
        if attempt.schema_version != 1
            || attempt.request_id == EntityId::nil()
            || attempt.attempt_id == EntityId::nil()
        {
            return Err(ShenkoError::conflict("invalid subject request attempt"));
        }
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        if let Some(existing) = self.read_subject_request_tx(&tx, attempt.request_id)? {
            // Idempotency compares the durable identity of the request: the
            // sealed plan, payload/binding digests and the attempt binding.
            // `created_commit` is assigned by this store and never part of it.
            if existing.attempt_id == attempt.attempt_id
                && existing.agent_id == attempt.agent_id
                && existing.step_id == attempt.step_id
                && existing.send_id == attempt.send_id
                && existing.input_plan_ref == attempt.input_plan_ref
                && existing.payload_digest == attempt.payload_digest
                && existing.provider_binding_digest == attempt.provider_binding_digest
            {
                tx.commit().map_err(storage_error)?;
                return Ok(existing.created_commit);
            }
            return Err(ShenkoError::conflict(
                "subject request identity reused with different facts",
            ));
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        let mut stored = attempt.clone();
        stored.created_commit = commit;
        self.insert_subject_request_tx(&tx, &stored, seq)?;
        self.record_subject_commit(&tx, commit, 1)?;
        tx.commit().map_err(storage_error)?;
        Ok(commit)
    }

    /// Tx-scoped insert of one armed attempt. The step transition that
    /// admits a request uses it, so the `AwaitingModel` transition and the
    /// armed request identity commit in ONE transaction: a request can never
    /// be visible without its receipt identity, and the identity can never
    /// be visible without the step that owns it.
    pub(crate) fn insert_subject_request_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        attempt: &SubjectRequestAttempt,
        seq: LedgerSeq,
    ) -> Result<(), ShenkoError> {
        if let Some(existing) = self.read_subject_request_tx(tx, attempt.request_id)? {
            // Re-arming the SAME request identity is idempotent. Different
            // facts under one identity are a conflict: the original record
            // is never rewritten.
            if existing.attempt_id == attempt.attempt_id
                && existing.agent_id == attempt.agent_id
                && existing.step_id == attempt.step_id
                && existing.input_plan_ref == attempt.input_plan_ref
                && existing.payload_digest == attempt.payload_digest
                && existing.provider_binding_digest == attempt.provider_binding_digest
 {
                return Ok(());
            }
            return Err(ShenkoError::conflict(
                "subject request identity reused with different facts",
            ));
        }
        let stored = attempt;
        tx.execute(
            "INSERT INTO subject_request_attempts
                (request_id, agent_id, step_id, attempt_id, send_id, state,
                 input_plan_ref, payload_digest, provider_binding_digest,
                 data, created_commit_seq, updated_commit_seq)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)",
            params![
                stored.request_id.to_string(),
                stored.agent_id.to_string(),
                stored.step_id.to_string(),
                stored.attempt_id.to_string(),
                stored.send_id.map(|id| id.to_string()),
                serde_json::to_string(&stored.state)?,
                serde_json::to_string(&stored.input_plan_ref)?,
                stored.payload_digest.0.as_str(),
                stored.provider_binding_digest.0.as_str(),
                serde_json::to_string(&stored)?,
                seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    /// The durable send boundary, called by the broker receipt sink exactly
    /// once per armed attempt. A second call for the same request is a
    /// conflict: the request may already be in flight and its identity must
    /// not be re-marked.
    pub fn mark_subject_request_dispatched(
        &self,
        request_id: EntityId,
    ) -> Result<CommitRef, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_transition_subject_request(
                request_id,
                SubjectRequestState::Armed,
                SubjectRequestState::Dispatched,
                None,
            ),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Record terminal provider evidence independently of the caller. This
    /// is the durable receipt: it runs even when the waiting task is gone, so
    /// the result survives a crash between the wire answer and the step's S1.
    pub fn record_subject_request_terminal(
        &self,
        request_id: EntityId,
        terminal: &SubjectRequestTerminal,
    ) -> Result<CommitRef, ShenkoError> {
        self.ledger.with_mutation(
            || {
                self.try_transition_subject_request(
                    request_id,
                    SubjectRequestState::Dispatched,
                    SubjectRequestState::Terminal,
                    Some(terminal),
                )
                .or_else(|error| {
                    // The send boundary and the terminal evidence can race a
                    // crash either way; arming evidence for an Armed row is
                    // still the same original request, never a second send.
                    if matches!(error, ShenkoError::Conflict(_)) {
                        self.try_transition_subject_request(
                            request_id,
                            SubjectRequestState::Armed,
                            SubjectRequestState::Terminal,
                            Some(terminal),
                        )
                    } else {
                        Err(error)
                    }
                })
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_transition_subject_request(
        &self,
        request_id: EntityId,
        from: SubjectRequestState,
        to: SubjectRequestState,
        terminal: Option<&SubjectRequestTerminal>,
    ) -> Result<CommitRef, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let mut attempt = self
            .read_subject_request_tx(&tx, request_id)?
            .ok_or_else(|| ShenkoError::not_found("subject request attempt"))?;
        if attempt.state != from {
            return Err(ShenkoError::conflict(
                "subject request is not in the expected state",
            ));
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        attempt.state = to;
        if let Some(mut terminal) = terminal.cloned() {
            // The commit that records this terminal IS the terminal's commit:
            // the receipt and its commit reference are one fact.
            terminal.recorded_commit = commit;
            attempt.terminal = Some(terminal);
        }
        let changed = tx
            .execute(
                "UPDATE subject_request_attempts SET state=?2, data=?3, updated_commit_seq=?4
                 WHERE request_id=?1",
                params![
                    request_id.to_string(),
                    serde_json::to_string(&attempt.state)?,
                    serde_json::to_string(&attempt)?,
                    seq.0 as i64,
                ],
            )
            .map_err(storage_error)?;
        if changed != 1 {
            return Err(ShenkoError::conflict("stale subject request row"));
        }
        self.record_subject_commit(&tx, commit, 1)?;
        tx.commit().map_err(storage_error)?;
        Ok(commit)
    }

    /// Read one attempt by its exact request identity.
    pub fn subject_request_attempt(
        &self,
        request_id: EntityId,
    ) -> Result<Option<SubjectRequestAttempt>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        self.read_subject_request_tx(&conn, request_id)
    }

    /// All attempts of one step, oldest first. Resume uses this to find an
    /// armed attempt whose terminal receipt may have landed while the caller
    /// was gone — the durable receipt, never a blind replay, decides.
    pub fn subject_request_attempts_for_step(
        &self,
        agent_id: EntityId,
        step_id: EntityId,
    ) -> Result<Vec<SubjectRequestAttempt>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT data FROM subject_request_attempts
                 WHERE agent_id=?1 AND step_id=?2
                 ORDER BY created_commit_seq",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![agent_id.to_string(), step_id.to_string()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(ShenkoError::Serde))
            .collect()
    }

    fn read_subject_request_tx(
        &self,
        conn: &rusqlite::Connection,
        request_id: EntityId,
    ) -> Result<Option<SubjectRequestAttempt>, ShenkoError> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM subject_request_attempts WHERE request_id=?1",
                params![request_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        data.map(|data| serde_json::from_str(&data).map_err(ShenkoError::Serde))
            .transpose()
    }
}
