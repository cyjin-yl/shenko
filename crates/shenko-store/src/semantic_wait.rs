//! Durable semantic wait records, receipts and clock bookkeeping — SPEC §5.1, R143.
//!
//! A semantic wait is a durable fact about the subject's own decision to keep
//! thinking until a condition it named is met. The Store owns that lifecycle:
//! the deadline is fixed when the wait starts, no observation can extend it and
//! no later step can resurrect a finished wait. Observation and settlement both
//! happen on the existing ledger mutation line, so a wait never becomes visible
//! before the commit that decided it.
//!
//! Two invariants shape the code here:
//!
//! * A rejected wait change never fails the whole S1. It is settled as a typed
//!   receipt, exactly like an identity conflict, so the other valid actions and
//!   the unique successor of that step are still committed.
//! * The branch head row is never touched by an observation. S1 admits against
//!   the head revision the subject read; a clock observation is not a subject
//!   decision and must not invalidate it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::output::OutputAction;
use shenko_types::subject::{
    PolicyKind, SemanticWait, SemanticWaitChange, SemanticWaitOutcome, SemanticWaitReceipt,
    SemanticWaitStatus, StepDecision, SubjectBranchHead, SubjectStep,
};

use crate::store::{Store, storage_error};

/// The frozen contract of the handled semantic wait policy intent.
pub const SEMANTIC_WAIT_CONTRACT: &str = "shenko.subject/semantic-wait/1";

/// Additive storage for semantic waits and their per step receipts.
///
/// The wait rows are the durable history: a finished wait keeps its original
/// condition, times and reason, and is never rewritten by a later wait.
pub(crate) const SEMANTIC_WAIT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS semantic_waits (
    wait_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    status TEXT NOT NULL,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_semantic_waits_agent ON semantic_waits(agent_id, commit_seq);
CREATE TABLE IF NOT EXISTS semantic_wait_receipts (
    step_id TEXT PRIMARY KEY,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
"#;

/// Conservative clock bookkeeping for semantic waits.
///
/// The deadline of a wait is a wall clock instant fixed when the wait starts.
/// A wall clock that moves backwards must never extend that deadline, so every
/// observation is compared against the highest instant this handle has seen
/// and against a monotonic anchor that can only make the wait end earlier.
#[derive(Debug, Default)]
pub(crate) struct WaitClock {
    agents: HashMap<EntityId, AgentClock>,
}

#[derive(Debug)]
struct AgentClock {
    /// Highest wall clock instant observed for this agent.
    high_water: DateTime<Utc>,
    /// Monotonic anchor of the wait currently under observation.
    anchor: Option<WaitAnchor>,
}

#[derive(Debug)]
struct WaitAnchor {
    wait_id: EntityId,
    instant: Instant,
    /// Budget that was left when the anchor was taken.
    remaining: Duration,
}

/// The clock verdict for one wait observation.
#[derive(Debug, Clone)]
pub(crate) enum WaitExpiry {
    Alive,
    Expired { reason: String },
}

/// One observation: the verdict plus the high water mark it was judged against.
pub(crate) struct WaitObservation {
    pub expiry: WaitExpiry,
    pub high_water: DateTime<Utc>,
}

impl WaitClock {
    pub(crate) fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    /// Judge one observation of `wait` at the wall clock reading `now`.
    ///
    /// The persisted `last_observed_at` is the observation high water mark of
    /// the record, so a reopened store keeps the mark it already committed.
    pub(crate) fn observe(
        &mut self,
        agent_id: EntityId,
        wait: &SemanticWait,
        now: DateTime<Utc>,
    ) -> WaitObservation {
        let persisted = wait.last_observed_at;
        let entry = self.agents.entry(agent_id).or_insert_with(|| AgentClock {
            high_water: persisted,
            anchor: None,
        });
        if persisted > entry.high_water {
            entry.high_water = persisted;
        }
        // A wall clock earlier than the high water mark is a rollback. The
        // deadline of a running wait is never extended by a clock that moved
        // backwards, so the wait ends instead.
        if now < entry.high_water {
            return WaitObservation {
                expiry: WaitExpiry::Expired {
                    reason: "wall clock rolled back before the observed high-water mark".into(),
                },
                high_water: entry.high_water,
            };
        }
        if now >= wait.deadline_at {
            return WaitObservation {
                expiry: WaitExpiry::Expired {
                    reason: "semantic wait deadline reached".into(),
                },
                high_water: entry.high_water,
            };
        }
        // The monotonic anchor can only end the wait earlier. It is taken when
        // this handle first sees the wait, with the budget that was left then.
        let anchored = matches!(&entry.anchor, Some(anchor) if anchor.wait_id == wait.wait_id);
        if !anchored {
            let remaining = wait
                .deadline_at
                .signed_duration_since(entry.high_water.max(now))
                .to_std()
                .unwrap_or(Duration::ZERO);
            entry.anchor = Some(WaitAnchor {
                wait_id: wait.wait_id,
                instant: Instant::now(),
                remaining,
            });
        }
        let anchor = entry.anchor.as_ref().expect("wait anchor");
        if anchor.instant.elapsed() >= anchor.remaining {
            return WaitObservation {
                expiry: WaitExpiry::Expired {
                    reason: "monotonic elapsed time reached the semantic wait deadline".into(),
                },
                high_water: entry.high_water,
            };
        }
        entry.high_water = now;
        WaitObservation {
            expiry: WaitExpiry::Alive,
            high_water: entry.high_water,
        }
    }
}

/// One wait change requested by a committed result.
struct WaitRequest {
    /// The action id of the parsed control. A new wait uses it as its id.
    action_id: EntityId,
    /// The policy intent that carried the change, when there is one.
    intent_id: Option<EntityId>,
    change: SemanticWaitChange,
}

impl Store {
    /// The latest semantic wait record of one agent, terminal records included.
    ///
    /// Reading never restarts a wait and never moves a deadline. A finished
    /// wait stays readable so the subject can be shown why it ended.
    pub fn semantic_wait(&self, agent_id: EntityId) -> Result<Option<SemanticWait>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_latest(&conn, agent_id)
    }

    /// The exact semantic wait receipt committed by one step.
    pub fn semantic_wait_receipt(
        &self,
        step_id: EntityId,
    ) -> Result<Option<SemanticWaitReceipt>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM semantic_wait_receipts WHERE step_id=?1",
                [step_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        data.map(|data| serde_json::from_str(&data).map_err(ShenkoError::from))
            .transpose()
    }

    /// Observe the durable wait before a fresh S0 snapshot.
    ///
    /// The observed clock high-water is durable before the next S0 can be
    /// sealed. Otherwise a crash followed by clock rollback could silently
    /// return to an earlier, still-valid part of the same deadline. This is
    /// a ledger fact, not a subject decision, and never changes the head row.
    pub fn observe_semantic_wait(
        &self,
        expected_head: &SubjectBranchHead,
    ) -> Result<Option<SemanticWait>, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_observe_semantic_wait(expected_head),
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    fn try_observe_semantic_wait(
        &self,
        expected_head: &SubjectBranchHead,
    ) -> Result<Option<SemanticWait>, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let head = crate::store::checked_head(&tx, expected_head)?;
        let current = read_latest(&tx, head.agent_id)?;
        let Some(wait) = current else {
            return Ok(None);
        };
        if !matches!(wait.status, SemanticWaitStatus::Active) {
            return Ok(Some(wait));
        }
        let now = Utc::now();
        let observation = {
            let mut clock = self.semantic_wait_clock.lock();
            clock.observe(head.agent_id, &wait, now)
        };
        let mut observed = wait;
        let prior = observed.last_observed_at;
        observed.last_observed_at = observation.high_water.max(now);
        if let WaitExpiry::Expired { reason } = observation.expiry {
            observed.status = SemanticWaitStatus::Expired { reason };
        } else if observed.last_observed_at == prior {
            return Ok(Some(observed));
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
        observed.updated_commit = commit_ref;
        write_wait(&tx, head.agent_id, &observed, seq.0)?;
        self.record_subject_commit(&tx, commit_ref, 1)?;
        tx.commit().map_err(storage_error)?;
        Ok(Some(observed))
    }

    /// Apply the semantic wait change of one S1 inside the caller's transaction.
    ///
    /// The current wait is read inside the same transaction that writes the
    /// new one, so the S1 settles against exactly the state it observed. A
    /// stale or late change is settled as a typed receipt; it never fails the
    /// whole S1.
    pub(crate) fn apply_semantic_wait(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        step: &SubjectStep,
        decision: &StepDecision,
        commit_ref: CommitRef,
        commit_seq: u64,
    ) -> Result<SemanticWaitReceipt, ShenkoError> {
        let now = Utc::now();
        // The current wait is read INSIDE the S1 transaction.
        let mut current = read_latest(tx, head.agent_id)?;
        let mut expired_in_this_step = false;
        // The clock is checked again at S1: a result that arrives after the
        // deadline can never revive the wait it was produced under.
        if let Some(wait) = current.clone() {
            if matches!(wait.status, SemanticWaitStatus::Active) {
                let observation = {
                    let mut clock = self.semantic_wait_clock.lock();
                    clock.observe(head.agent_id, &wait, now)
                };
                let mut observed = wait;
                observed.last_observed_at = observation.high_water.max(now);
                if let WaitExpiry::Expired { reason } = observation.expiry {
                    observed.status = SemanticWaitStatus::Expired { reason };
                    expired_in_this_step = true;
                }
                observed.updated_commit = commit_ref;
                write_wait(tx, head.agent_id, &observed, commit_seq)?;
                current = Some(observed);
            }
        }

        let request = self.resolve_wait_change(decision)?;
        // The sealed S0 input binds the change to the state the model saw.
        let (sealed, binding_digest) = match &request {
            Some(_) => {
                let (wait, digest) = self.sealed_wait_snapshot(decision)?;
                (wait, Some(digest))
            }
            None => (None, None),
        };

        let mut settled = current.clone();
        let outcome = match &request {
            None if expired_in_this_step => SemanticWaitOutcome::Expired,
            None => SemanticWaitOutcome::Unchanged,
            Some(request) => {
                let outcome = self.settle_wait_change(
                    tx,
                    head,
                    step,
                    decision,
                    request,
                    sealed.as_ref(),
                    binding_digest.as_ref().expect("wait action has a frozen binding"),
                    &settled,
                    now,
                    commit_ref,
                    commit_seq,
                )?;
                if let Some(wait) = outcome.1 {
                    settled = Some(wait);
                }
                outcome.0
            }
        };

        let receipt = SemanticWaitReceipt {
            step_id: step.step_id,
            intent_id: request.as_ref().and_then(|request| request.intent_id),
            outcome,
            wait: settled,
            committed_at: commit_ref,
        };
        // Ordinary thought steps do not create an ever-growing stream of
        // empty wait receipts. A requested change or newly observed expiry is
        // still recorded atomically with this exact S1.
        if receipt.intent_id.is_some()
            || !matches!(receipt.outcome, SemanticWaitOutcome::Unchanged)
        {
            write_receipt(tx, &receipt, commit_seq)?;
        }
        Ok(receipt)
    }

    /// Settle the handled wait policy intent with its exact receipt.
    ///
    /// The row is never left `ready` and never `blocked`: the wait was decided
    /// in this step, and the receipt is the result of that decision.
    pub(crate) fn settle_semantic_wait_intent(
        &self,
        tx: &Transaction<'_>,
        receipt: &SemanticWaitReceipt,
        commit_seq: u64,
    ) -> Result<(), ShenkoError> {
        let Some(intent_id) = receipt.intent_id else {
            return Ok(());
        };
        let wait_id = receipt
            .wait
            .as_ref()
            .map(|wait| wait.wait_id.to_string())
            .unwrap_or_default();
        let result_ref = format!(
            "semantic-wait:{}:{}",
            outcome_tag(&receipt.outcome),
            wait_id
        );
        tx.execute(
            "UPDATE subject_intents SET state='settled', result_ref=?2, revision=revision+1, updated_seq=?3
             WHERE intent_id=?1 AND kind='policy' AND contract=?4",
            params![
                intent_id.to_string(),
                result_ref,
                commit_seq as i64,
                SEMANTIC_WAIT_CONTRACT
            ],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    /// Find the single wait change of a committed result.
    ///
    /// The change must be bound to the parsed control and to the policy intent
    /// that carries the same action id. A result with two wait changes is a
    /// protocol violation, not a stale wait: it is refused as a whole.
    fn resolve_wait_change(
        &self,
        decision: &StepDecision,
    ) -> Result<Option<WaitRequest>, ShenkoError> {
        let mut control: Option<(EntityId, SemanticWaitChange)> = None;
        for parsed in &decision.output.controls {
            if let OutputAction::SemanticWait { change } = &parsed.action {
                if control.is_some() {
                    return Err(ShenkoError::conflict(
                        "more than one semantic wait control in one result",
                    ));
                }
                control = Some((parsed.action_id, change.clone()));
            }
        }
        let mut policy: Option<(EntityId, SemanticWaitChange)> = None;
        for intent in &decision.policy_intents {
            if let PolicyKind::SemanticWait { change } = &intent.kind {
                if policy.is_some() {
                    return Err(ShenkoError::conflict(
                        "more than one semantic wait policy intent in one decision",
                    ));
                }
                policy = Some((intent.intent_id, change.clone()));
            }
        }
        match (control, policy) {
            (None, None) => Ok(None),
            (Some(_), None) | (None, Some(_)) => Err(ShenkoError::conflict(
                "semantic wait requires both its parsed control and matching policy intent",
            )),
            (Some((action_id, control_change)), Some((intent_id, policy_change))) => {
                if action_id != intent_id || !same_change(&control_change, &policy_change) {
                    return Err(ShenkoError::conflict(
                        "semantic wait change is not bound to its parsed control and policy intent",
                    ));
                }
                Ok(Some(WaitRequest {
                    action_id,
                    intent_id: Some(intent_id),
                    change: control_change,
                }))
            }
        }
    }

    /// The wait snapshot the model was shown in the sealed S0 input.
    fn sealed_wait_snapshot(
        &self,
        decision: &StepDecision,
    ) -> Result<(Option<SemanticWait>, shenko_types::object::Digest), ShenkoError> {
        let input: serde_json::Value =
            serde_json::from_slice(&self.cas.read_object(&decision.input_plan_ref.digest)?)?;
        if input.get("schema_version").and_then(serde_json::Value::as_u64) != Some(9) {
            return Err(ShenkoError::conflict(
                "semantic wait change is not bound to its S0 snapshot",
            ));
        }
        let binding = input.get("provider_binding").ok_or_else(||
            ShenkoError::conflict("semantic wait requires its frozen provider binding"))?;
        let digest = shenko_types::object::Digest::blake3(&serde_json::to_vec(binding)?);
        let wait = match input.get("semantic_wait") {
            Some(serde_json::Value::Null) => None,
            Some(value) => Some(serde_json::from_value(value.clone())?),
            None => return Err(ShenkoError::conflict("semantic wait snapshot missing from S0")),
        };
        Ok((wait, digest))
    }

    /// Apply one wait change against the current wait.
    ///
    /// Returns the typed outcome and the wait record after the change, if the
    /// change wrote one.
    #[allow(clippy::too_many_arguments)]
    fn settle_wait_change(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        step: &SubjectStep,
        decision: &StepDecision,
        request: &WaitRequest,
        sealed: Option<&SemanticWait>,
        binding_digest: &shenko_types::object::Digest,
        current: &Option<SemanticWait>,
        now: DateTime<Utc>,
        commit_ref: CommitRef,
        commit_seq: u64,
    ) -> Result<(SemanticWaitOutcome, Option<SemanticWait>), ShenkoError> {
        if sealed.is_some_and(|wait| wait.is_active() && &wait.model_binding_digest != binding_digest) {
            return Ok((SemanticWaitOutcome::Rejected {
                reason: "the frozen model binding differs from the semantic wait's original binding".into(),
            }, None));
        }
        match &request.change {
            SemanticWaitChange::Start {
                condition,
                max_wait_secs,
            } => {
                if condition.trim().is_empty() {
                    return Ok((
                        SemanticWaitOutcome::Rejected {
                            reason: "the semantic wait condition is empty".into(),
                        },
                        None,
                    ));
                }
                if !(1..=SemanticWait::MAX_WAIT_SECS).contains(max_wait_secs) {
                    return Ok((
                        SemanticWaitOutcome::Rejected {
                            reason: "semantic wait duration must be between 1 and 600 seconds".into(),
                        },
                        None,
                    ));
                }
                // A wait that is active now is never replaced, and a wait that
                // was active in the sealed S0 context is never restarted: it
                // expired or ended while this result was in flight.
                if let Some(active) = current
                    .as_ref()
                    .filter(|wait| matches!(wait.status, SemanticWaitStatus::Active))
                {
                    let reason = if active.condition == *condition {
                        "the active semantic wait has the same condition; a new wait id does not renew it"
                    } else {
                        "another semantic wait is already active"
                    };
                    return Ok((
                        SemanticWaitOutcome::Rejected {
                            reason: reason.into(),
                        },
                        None,
                    ));
                }
                if sealed
                    .filter(|wait| matches!(wait.status, SemanticWaitStatus::Active))
                    .is_some()
                {
                    return Ok((
                        SemanticWaitOutcome::Rejected {
                            reason: "the semantic wait observed in the sealed S0 context ended or expired during this request and is not restarted".into(),
                        },
                        None,
                    ));
                }
                if current.as_ref().is_some_and(|wait|
                    !wait.is_active() && wait.condition == condition.trim())
                {
                    return Ok((
                        SemanticWaitOutcome::Rejected {
                            reason: "the same completed semantic wait cannot be restarted as a renewal".into(),
                        },
                        None,
                    ));
                }
                let wait = self.start_semantic_wait(
                    head,
                    step,
                    decision,
                    request,
                    condition.trim(),
                    *max_wait_secs,
                    now,
                    commit_ref,
                    binding_digest,
                )?;
                write_wait(tx, head.agent_id, &wait, commit_seq)?;
                Ok((SemanticWaitOutcome::Started, Some(wait)))
            }
            SemanticWaitChange::Continue { wait_id } => {
                match self.check_wait_target(sealed, current, *wait_id) {
                    Err(outcome) => Ok((outcome, None)),
                    Ok(()) => {
                        let mut continued = current.as_ref().expect("active wait").clone();
                        // A continue never renews: the deadline stays exactly as
                        // it was fixed when the wait started.
                        continued.last_observed_at = now;
                        continued.updated_commit = commit_ref;
                        write_wait(tx, head.agent_id, &continued, commit_seq)?;
                        Ok((SemanticWaitOutcome::Continued, Some(continued)))
                    }
                }
            }
            SemanticWaitChange::End { wait_id, reason } => {
                if reason.trim().is_empty() {
                    return Ok((
                        SemanticWaitOutcome::Rejected {
                            reason: "the semantic wait end reason is empty".into(),
                        },
                        None,
                    ));
                }
                match self.check_wait_target(sealed, current, *wait_id) {
                    Err(outcome) => Ok((outcome, None)),
                    Ok(()) => {
                        let mut ended = current.as_ref().expect("active wait").clone();
                        ended.status = SemanticWaitStatus::Ended {
                            reason: reason.trim().to_owned(),
                        };
                        ended.last_observed_at = now;
                        ended.updated_commit = commit_ref;
                        write_wait(tx, head.agent_id, &ended, commit_seq)?;
                        Ok((SemanticWaitOutcome::Ended, Some(ended)))
                    }
                }
            }
        }
    }

    /// Check that a continue or end targets the wait the model was shown and
    /// the wait that is still running.
    fn check_wait_target(
        &self,
        sealed: Option<&SemanticWait>,
        current: &Option<SemanticWait>,
        wait_id: EntityId,
    ) -> Result<(), SemanticWaitOutcome> {
        let Some(sealed) = sealed else {
            return Err(SemanticWaitOutcome::Rejected {
                reason: "no semantic wait was active in the sealed S0 context".into(),
            });
        };
        if sealed.wait_id != wait_id {
            return Err(SemanticWaitOutcome::Rejected {
                reason: "the wait change targets a different wait than the sealed S0 context".into(),
            });
        }
        let Some(current) = current else {
            return Err(SemanticWaitOutcome::Rejected {
                reason: "no such semantic wait".into(),
            });
        };
        if current.wait_id != wait_id {
            return Err(SemanticWaitOutcome::Rejected {
                reason: "no such semantic wait".into(),
            });
        }
        match &current.status {
            SemanticWaitStatus::Active => Ok(()),
            SemanticWaitStatus::Ended { .. } => Err(SemanticWaitOutcome::Rejected {
                reason: "the semantic wait already ended".into(),
            }),
            SemanticWaitStatus::Expired { .. } => Err(SemanticWaitOutcome::Rejected {
                reason: "the semantic wait deadline was reached before this result was committed"
                    .into(),
            }),
        }
    }

    /// Build the new wait of a start change.
    ///
    /// The Store owns the start and the deadline: the requested duration is
    /// capped, the deadline is fixed here and never moves again.
    #[allow(clippy::too_many_arguments)]
    fn start_semantic_wait(
        &self,
        _head: &SubjectBranchHead,
        step: &SubjectStep,
        decision: &StepDecision,
        request: &WaitRequest,
        condition: &str,
        max_wait_secs: u64,
        now: DateTime<Utc>,
        commit_ref: CommitRef,
        binding_digest: &shenko_types::object::Digest,
    ) -> Result<SemanticWait, ShenkoError> {
        let seconds = i64::try_from(max_wait_secs)
            .map_err(|_| ShenkoError::conflict("semantic wait duration out of range"))?;
        Ok(SemanticWait {
            wait_id: request.action_id,
            condition: condition.to_owned(),
            started_at: now,
            deadline_at: now
                + chrono::TimeDelta::try_seconds(seconds)
                    .ok_or_else(|| ShenkoError::conflict("semantic wait duration out of range"))?,
            context_ref: decision.input_plan_ref.clone(),
            model_binding_digest: binding_digest.clone(),
            status: SemanticWaitStatus::Active,
            last_observed_at: now,
            created_step_id: step.step_id,
            updated_commit: commit_ref,
        })
    }
}

/// The latest wait record of one agent, terminal records included.
pub(crate) fn read_latest(
    conn: &Connection,
    agent_id: EntityId,
) -> Result<Option<SemanticWait>, ShenkoError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT w.data FROM semantic_waits w
             JOIN subject_steps s ON s.step_id=json_extract(w.data,'$.created_step_id')
             JOIN subject_branch_heads h ON h.agent_id=w.agent_id AND h.branch_id=s.branch_id
             WHERE w.agent_id=?1 ORDER BY s.step_no DESC LIMIT 1",
            [agent_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    data.map(|data| serde_json::from_str(&data).map_err(ShenkoError::from))
        .transpose()
}

/// Write one wait record. The history of finished waits is never rewritten.
pub(crate) fn write_wait(
    tx: &Transaction<'_>,
    agent_id: EntityId,
    wait: &SemanticWait,
    commit_seq: u64,
) -> Result<(), ShenkoError> {
    let status = match &wait.status {
        SemanticWaitStatus::Active => "active",
        SemanticWaitStatus::Ended { .. } => "ended",
        SemanticWaitStatus::Expired { .. } => "expired",
    };
    tx.execute(
        "INSERT INTO semantic_waits(wait_id,agent_id,status,data,commit_seq) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(wait_id) DO UPDATE SET agent_id=excluded.agent_id, status=excluded.status,
             data=excluded.data, commit_seq=excluded.commit_seq",
        params![
            wait.wait_id.to_string(),
            agent_id.to_string(),
            status,
            serde_json::to_string(wait)?,
            commit_seq as i64
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

/// Write the receipt of one step.
pub(crate) fn write_receipt(
    tx: &Transaction<'_>,
    receipt: &SemanticWaitReceipt,
    commit_seq: u64,
) -> Result<(), ShenkoError> {
    tx.execute(
        "INSERT INTO semantic_wait_receipts(step_id,data,commit_seq) VALUES(?1,?2,?3)",
        params![
            receipt.step_id.to_string(),
            serde_json::to_string(receipt)?,
            commit_seq as i64
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

/// The short tag of an outcome, used in the settled intent reference.
fn outcome_tag(outcome: &SemanticWaitOutcome) -> &'static str {
    match outcome {
        SemanticWaitOutcome::Unchanged => "unchanged",
        SemanticWaitOutcome::Started => "started",
        SemanticWaitOutcome::Continued => "continued",
        SemanticWaitOutcome::Ended => "ended",
        SemanticWaitOutcome::Expired => "expired",
        SemanticWaitOutcome::Rejected { .. } => "rejected",
    }
}

/// Two changes are the same change when they serialize the same.
fn same_change(left: &SemanticWaitChange, right: &SemanticWaitChange) -> bool {
    match (serde_json::to_value(left), serde_json::to_value(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::LedgerSeq;
    use shenko_types::object::{Digest, ObjectRef, SchemaRef};

    fn active_wait(started_at: DateTime<Utc>, deadline_at: DateTime<Utc>) -> SemanticWait {
        SemanticWait {
            wait_id: EntityId::new(),
            condition: "实验出现足以判断的结果".into(),
            started_at,
            deadline_at,
            context_ref: ObjectRef {
                digest: Digest::blake3(b"input-plan"),
                bytes: 10,
                media_type: "application/json".into(),
                store_namespace: "test".into(),
                locator: "file:///input-plan".into(),
                codec: SchemaRef {
                    name: "input_plans".into(),
                    major: 9,
                    digest: Digest::blake3(b"input-plan"),
                },
            },
            model_binding_digest: Digest::blake3(b"null"),
            status: SemanticWaitStatus::Active,
            last_observed_at: started_at,
            created_step_id: EntityId::new(),
            updated_commit: CommitRef::new(EntityId::new(), LedgerSeq(1)),
        }
    }

    #[test]
    fn clock_rollback_and_stalled_wall_clock_end_the_wait() {
        let agent = EntityId::new();
        let now = Utc::now();
        let wait = active_wait(now, now + chrono::TimeDelta::seconds(600));
        let mut clock = WaitClock::new();
        // A normal observation keeps the wait alive.
        assert!(matches!(
            clock.observe(agent, &wait, now).expiry,
            WaitExpiry::Alive
        ));
        // A wall clock that moved backwards ends the wait instead of
        // extending the deadline it already fixed.
        let rolled_back = clock.observe(agent, &wait, now - chrono::TimeDelta::seconds(5));
        assert!(matches!(
            rolled_back.expiry,
            WaitExpiry::Expired { .. }
        ));

        // A wall clock that stalls cannot keep the wait alive past its
        // deadline: the monotonic anchor ends it anyway.
        let now = Utc::now();
        let wait = active_wait(now, now + chrono::TimeDelta::seconds(1));
        let mut clock = WaitClock::new();
        assert!(matches!(
            clock.observe(agent, &wait, now).expiry,
            WaitExpiry::Alive
        ));
        std::thread::sleep(Duration::from_millis(1100));
        let stalled = clock.observe(agent, &wait, now);
        assert!(matches!(stalled.expiry, WaitExpiry::Expired { .. }));
    }
}
