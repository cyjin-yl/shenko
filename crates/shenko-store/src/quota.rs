//! Durable provider-quota state on the single mutation line (SPEC §42.5).
//!
//! Every quota write — observation snapshot, forecast, policy, sealed
//! decision, admission/pacer state and attempt ledger rows — commits inside
//! the same IMMEDIATE ledger transaction it belongs to. A reservation and
//! its armed attempt rows therefore survive reopen, another worker cannot
//! replay a recorded admission into a second dispatch, and no second JSON
//! ledger exists next to the Store truth.
//!
//! `LocalAttempt.ledger_commit` is assigned by the ledger itself inside the
//! arming transaction, so a bound attempt is always identified by the exact
//! account/window/unit/request plus its real CommitRef. Armed or unknown
//! reservations stay charged until same-ledger provider coverage proves the
//! cost; only `NeverDispatched` evidence releases a reservation, and the
//! pacer rejects such evidence once an armed binding exists.
use crate::store::{storage_error, Store};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use shenko_quota::{
    admit, cancel, mark_armed, AdmissionOutcome, AttemptBilling, BackendState, DispatchEvidence,
    LocalAttempt, Pacer, PolicyDecision, QuotaAdmission, QuotaForecast, QuotaPolicy, QuotaSnapshot,
    Range, WindowCost,
};
use shenko_types::commit::{CommitRef, CommitReceipt, UpdatedEntity};
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq, RecordRevision};
use std::collections::HashMap;

/// Additive quota tables; history is never rewritten by a migration.
pub(crate) const QUOTA_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS quota_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    account_scope_ref TEXT NOT NULL,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS quota_forecasts (
    forecast_id TEXT PRIMARY KEY,
    based_on_snapshot TEXT NOT NULL,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS quota_policies (
    policy_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (policy_id, revision)
);
CREATE TABLE IF NOT EXISTS quota_decisions (
    decision_id TEXT PRIMARY KEY,
    policy_id TEXT NOT NULL,
    policy_revision INTEGER NOT NULL,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS quota_pacer (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS quota_attempts (
    request_id TEXT NOT NULL,
    window_key TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (request_id, window_key, commit_seq)
);
"#;

/// The pacer write produced by `admit_quota_request`: the typed admission
/// outcome plus the ledger position of the durable record, when the call
/// actually changed state.
#[derive(Debug, Clone)]
pub struct QuotaAdmissionReceipt {
    pub outcome: AdmissionOutcome,
    pub commit: Option<CommitRef>,
}

fn conflict(reason: &str) -> ShenkoError {
    ShenkoError::conflict(reason)
}

fn read_pacer(conn: &Connection) -> Result<Pacer, ShenkoError> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM quota_pacer WHERE singleton=1", [], |r| r.get(0))
        .optional()
        .map_err(storage_error)?;
    match data {
        Some(data) => Ok(serde_json::from_str(&data)?),
        None => Ok(Pacer::default()),
    }
}

fn write_pacer(
    tx: &Transaction<'_>,
    pacer: &Pacer,
    seq: LedgerSeq,
) -> Result<(), ShenkoError> {
    tx.execute(
        "INSERT INTO quota_pacer(singleton, data, commit_seq) VALUES(1,?1,?2)
         ON CONFLICT(singleton) DO UPDATE SET data=excluded.data, commit_seq=excluded.commit_seq",
        params![serde_json::to_string(pacer)?, seq.0 as i64],
    )
    .map_err(storage_error)?;
    Ok(())
}

/// Whether any quota policy is recorded. With none, inference dispatch has no
/// quota gate at all and admissions are never required.
pub(crate) fn quota_gate_open(conn: &Connection) -> Result<bool, ShenkoError> {
    let count: i64 = conn
        .query_row("SELECT count(*) FROM quota_policies", [], |r| r.get(0))
        .map_err(storage_error)?;
    Ok(count > 0)
}

/// The dispatch gate checked inside every AwaitingModel step CAS: once quota
/// is configured, a request may be dispatched only after its reservation is
/// admitted AND armed on this ledger.
pub(crate) fn quota_request_armed(
    conn: &Connection,
    request_id: EntityId,
) -> Result<bool, ShenkoError> {
    let pacer = read_pacer(conn)?;
    Ok(pacer.reservation(&request_id).is_some_and(|reservation| {
        reservation.admission.state == shenko_quota::AdmissionState::Admitted
            && reservation.dispatch == Some(DispatchEvidence::ArmedOrUnknown)
    }))
}

impl Store {
    /// Record an observation snapshot. Re-recording the identical snapshot is
    /// idempotent; the same id with different bytes is a conflict.
    pub fn record_quota_snapshot(
        &self,
        snapshot: &QuotaSnapshot,
    ) -> Result<CommitReceipt, ShenkoError> {
        if snapshot.snapshot_id == EntityId::nil() {
            return Err(conflict("quota snapshot has no identity"));
        }
        self.ledger.with_mutation(
            || {
                let data = serde_json::to_string(snapshot)?;
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let existing: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT data, commit_seq FROM quota_snapshots WHERE snapshot_id=?1",
                        [snapshot.snapshot_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if let Some((old, seq)) = existing {
                    if old == data {
                        tx.commit().map_err(storage_error)?;
                        return Ok(self.subject_receipt(
                            CommitRef::new(self.ledger.ledger_id(), LedgerSeq(seq as u64)),
                            vec![],
                        ));
                    }
                    return Err(conflict("quota snapshot id exists with different content"));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                tx.execute(
                    "INSERT INTO quota_snapshots(snapshot_id,provider_id,account_scope_ref,data,commit_seq) VALUES(?1,?2,?3,?4,?5)",
                    params![snapshot.snapshot_id.to_string(), snapshot.provider_id,
                        snapshot.account_scope_ref, data, seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(
                    commit,
                    vec![UpdatedEntity {
                        entity_id: snapshot.snapshot_id,
                        record_revision: RecordRevision(0),
                    }],
                ))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Record a forecast; its source snapshot must already be recorded for
    /// the same provider/account so the provenance chain stays intact.
    pub fn record_quota_forecast(
        &self,
        forecast: &QuotaForecast,
    ) -> Result<CommitReceipt, ShenkoError> {
        if forecast.forecast_id == EntityId::nil() {
            return Err(conflict("quota forecast has no identity"));
        }
        self.ledger.with_mutation(
            || {
                let data = serde_json::to_string(forecast)?;
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let snapshot: Option<(String, String, String)> = tx
                    .query_row(
                        "SELECT provider_id, account_scope_ref, data FROM quota_snapshots WHERE snapshot_id=?1",
                        [forecast.based_on_snapshot.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(storage_error)?;
                let Some((provider, scope, snapshot_data)) = snapshot else {
                    return Err(conflict("forecast source snapshot is not recorded"));
                };
                if provider != forecast.provider_id || scope != forecast.account_scope_ref {
                    return Err(conflict("forecast scope differs from its source snapshot"));
                }
                let stored: QuotaSnapshot = serde_json::from_str(&snapshot_data)?;
                if stored.snapshot_id != forecast.based_on_snapshot {
                    return Err(conflict("stored snapshot identity mismatch"));
                }
                let existing: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT data, commit_seq FROM quota_forecasts WHERE forecast_id=?1",
                        [forecast.forecast_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if let Some((old, seq)) = existing {
                    if old == data {
                        tx.commit().map_err(storage_error)?;
                        return Ok(self.subject_receipt(
                            CommitRef::new(self.ledger.ledger_id(), LedgerSeq(seq as u64)),
                            vec![],
                        ));
                    }
                    return Err(conflict("quota forecast id exists with different content"));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                tx.execute(
                    "INSERT INTO quota_forecasts(forecast_id,based_on_snapshot,data,commit_seq) VALUES(?1,?2,?3,?4)",
                    params![forecast.forecast_id.to_string(), forecast.based_on_snapshot.to_string(),
                        data, seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(
                    commit,
                    vec![UpdatedEntity {
                        entity_id: forecast.forecast_id,
                        record_revision: RecordRevision(0),
                    }],
                ))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Record one policy revision. The same revision with different content
    /// is rejected; `now` bounds the validation just like decision time.
    pub fn record_quota_policy(
        &self,
        policy: &QuotaPolicy,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<CommitReceipt, ShenkoError> {
        if policy.policy_id == EntityId::nil() || shenko_quota::policy_digest(policy).is_err()
            || shenko_quota::decide(policy, &[], &HashMap::<String, BackendState>::new(), None, now).is_err()
        {
            return Err(conflict("quota policy is invalid or unsupported"));
        }
        self.ledger.with_mutation(
            || {
                let data = serde_json::to_string(policy)?;
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let existing: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT data, commit_seq FROM quota_policies WHERE policy_id=?1 AND revision=?2",
                        params![policy.policy_id.to_string(), policy.revision as i64],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if let Some((old, seq)) = existing {
                    if old == data {
                        tx.commit().map_err(storage_error)?;
                        return Ok(self.subject_receipt(
                            CommitRef::new(self.ledger.ledger_id(), LedgerSeq(seq as u64)),
                            vec![],
                        ));
                    }
                    return Err(conflict("quota policy revision exists with different content"));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                tx.execute(
                    "INSERT INTO quota_policies(policy_id,revision,data,commit_seq) VALUES(?1,?2,?3,?4)",
                    params![policy.policy_id.to_string(), policy.revision as i64, data, seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(
                    commit,
                    vec![UpdatedEntity {
                        entity_id: policy.policy_id,
                        record_revision: RecordRevision(policy.revision),
                    }],
                ))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Record a sealed decision. Its policy revision must be the recorded
    /// policy row and every referenced forecast must be recorded.
    pub fn record_quota_decision(
        &self,
        decision: &PolicyDecision,
    ) -> Result<CommitReceipt, ShenkoError> {
        if decision.decision_id == EntityId::nil() {
            return Err(conflict("quota decision has no identity"));
        }
        self.ledger.with_mutation(
            || {
                let data = serde_json::to_string(decision)?;
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let policy_data: Option<String> = tx
                    .query_row(
                        "SELECT data FROM quota_policies WHERE policy_id=?1 AND revision=?2",
                        params![decision.policy_id.to_string(), decision.policy_revision as i64],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                let Some(policy_data) = policy_data else {
                    return Err(conflict("decision policy revision is not recorded"));
                };
                let policy: QuotaPolicy = serde_json::from_str(&policy_data)?;
                let digest = shenko_quota::policy_digest(&policy)
                    .map_err(|_| conflict("stored policy cannot be digested"))?;
                if digest != decision.policy_digest {
                    return Err(conflict("decision is not bound to the recorded policy"));
                }
                for forecast_id in &decision.based_on_forecasts {
                    let present: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM quota_forecasts WHERE forecast_id=?1",
                            [forecast_id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?;
                    if present.is_none() {
                        return Err(conflict("decision cites an unrecorded forecast"));
                    }
                }
                let existing: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT data, commit_seq FROM quota_decisions WHERE decision_id=?1",
                        [decision.decision_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if let Some((old, seq)) = existing {
                    if old == data {
                        tx.commit().map_err(storage_error)?;
                        return Ok(self.subject_receipt(
                            CommitRef::new(self.ledger.ledger_id(), LedgerSeq(seq as u64)),
                            vec![],
                        ));
                    }
                    return Err(conflict("quota decision id exists with different content"));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                tx.execute(
                    "INSERT INTO quota_decisions(decision_id,policy_id,policy_revision,data,commit_seq) VALUES(?1,?2,?3,?4,?5)",
                    params![decision.decision_id.to_string(), decision.policy_id.to_string(),
                        decision.policy_revision as i64, data, seq.0 as i64],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(
                    commit,
                    vec![UpdatedEntity {
                        entity_id: decision.decision_id,
                        record_revision: RecordRevision(0),
                    }],
                ))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// The most recently recorded policy, if quota is configured at all.
    pub fn latest_quota_policy(&self) -> Result<Option<QuotaPolicy>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM quota_policies ORDER BY commit_seq DESC, revision DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        data.map(|data| serde_json::from_str(&data).map_err(ShenkoError::from)).transpose()
    }

    /// The most recently recorded decision together with the exact policy
    /// revision it was sealed against.
    pub fn latest_quota_decision(
        &self,
    ) -> Result<Option<(QuotaPolicy, PolicyDecision)>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let row: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT data, policy_id, policy_revision FROM quota_decisions ORDER BY commit_seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(storage_error)?;
        let Some((data, policy_id, revision)) = row else { return Ok(None) };
        let policy_data: Option<String> = conn
            .query_row(
                "SELECT data FROM quota_policies WHERE policy_id=?1 AND revision=?2",
                params![policy_id, revision],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let Some(policy_data) = policy_data else {
            return Err(conflict("recorded decision has no policy row"));
        };
        Ok(Some((serde_json::from_str(&policy_data)?, serde_json::from_str(&data)?)))
    }

    /// The most recently recorded snapshot for one provider/account scope.
    pub fn latest_quota_snapshot(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
    ) -> Result<Option<QuotaSnapshot>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM quota_snapshots WHERE provider_id=?1 AND account_scope_ref=?2 ORDER BY commit_seq DESC LIMIT 1",
                params![provider_id, account_scope_ref],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        data.map(|data| serde_json::from_str(&data).map_err(ShenkoError::from)).transpose()
    }

    /// Every recorded attempt in one provider/account scope, commit order.
    pub fn quota_attempts(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
    ) -> Result<Vec<LocalAttempt>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare("SELECT data FROM quota_attempts ORDER BY commit_seq")
            .map_err(storage_error)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(storage_error)?;
        let mut attempts = Vec::new();
        for row in rows {
            let attempt: LocalAttempt = serde_json::from_str(&row.map_err(storage_error)?)?;
            if attempt.provider_id == provider_id && attempt.account_scope_ref == account_scope_ref
            {
                attempts.push(attempt);
            }
        }
        Ok(attempts)
    }

    /// The current durable pacer; an empty ledger has an empty pacer.
    pub fn quota_pacer(&self) -> Result<Pacer, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_pacer(&conn)
    }

    /// Run typed admission for `request_id` and persist any pacer change in
    /// the same mutation. The policy and decision must be the recorded rows
    /// verbatim; an unrecorded or rewritten source is a conflict.
    pub fn admit_quota_request(
        &self,
        policy: &QuotaPolicy,
        decision: &PolicyDecision,
        request_id: EntityId,
        availability: &HashMap<String, BackendState>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<QuotaAdmissionReceipt, ShenkoError> {
        let policy_data = serde_json::to_string(policy)?;
        let decision_data = serde_json::to_string(decision)?;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let stored_policy: Option<String> = tx
                    .query_row(
                        "SELECT data FROM quota_policies WHERE policy_id=?1 AND revision=?2",
                        params![policy.policy_id.to_string(), policy.revision as i64],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if stored_policy.as_deref() != Some(policy_data.as_str()) {
                    return Err(conflict("admission policy is not the recorded policy"));
                }
                let stored_decision: Option<String> = tx
                    .query_row(
                        "SELECT data FROM quota_decisions WHERE decision_id=?1",
                        [decision.decision_id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if stored_decision.as_deref() != Some(decision_data.as_str()) {
                    return Err(conflict("admission decision is not the recorded decision"));
                }
                if decision.policy_id != policy.policy_id
                    || decision.policy_revision != policy.revision
                {
                    return Err(conflict("decision does not belong to this policy"));
                }
                let mut pacer = read_pacer(&tx)?;
                let before = pacer.clone();
                let outcome =
                    admit(policy, decision, request_id, &mut pacer, availability, now);
                let mut commit = None;
                if pacer != before {
                    let seq = self.ledger.allocate_seq(&tx)?;
                    write_pacer(&tx, &pacer, seq)?;
                    let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
                    self.record_subject_commit(&tx, commit_ref, 1)?;
                    commit = Some(commit_ref);
                }
                tx.commit().map_err(storage_error)?;
                Ok(QuotaAdmissionReceipt { outcome, commit })
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Persist the armed binding for an admitted request BEFORE its dispatch:
    /// one ledger attempt row per quota window, each bound to this same
    /// commit. Replaying an already-armed admission verifies the original
    /// rows instead of creating parallel attempts; the caller must pass the
    /// armed admission record (from `quota_pacer`), since the pre-arm
    /// revision can never prove its own arming.
    pub fn arm_quota_admission(
        &self,
        admission: &QuotaAdmission,
    ) -> Result<(QuotaAdmission, CommitRef), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let mut pacer = read_pacer(&tx)?;
                let bound_attempts = match pacer.reservation(&admission.request_id) {
                    Some(reservation)
                        if reservation.dispatch == Some(DispatchEvidence::ArmedOrUnknown)
                            && reservation.admission == *admission =>
                    {
                        Some(reservation.attempts.clone())
                    }
                    _ => None,
                };
                let armed = if let Some(bound_attempts) = bound_attempts {
                    // Armed already: the first commits are the truth. They
                    // must still exist; a missing row is ledger damage,
                    // never silently re-armed under a different commit.
                    for bound in &bound_attempts {
                        if bound.ledger_commit.ledger_id != self.ledger.ledger_id() {
                            return Err(conflict(
                                "armed attempt is bound to another ledger",
                            ));
                        }
                        let present: Option<i64> = tx
                            .query_row(
                                "SELECT 1 FROM quota_attempts WHERE request_id=?1 AND window_key=?2 AND commit_seq=?3",
                                params![admission.request_id.to_string(),
                                    bound.window.window_key, bound.ledger_commit.seq.0 as i64],
                                |r| r.get(0),
                            )
                            .optional()
                            .map_err(storage_error)?;
                        if present.is_none() {
                            return Err(conflict("armed attempt row is missing"));
                        }
                    }
                    admission.clone()
                } else {
                    let attempts: Vec<LocalAttempt> = admission
                        .estimated_cost
                        .iter()
                        .map(|cost| LocalAttempt {
                            request_id: admission.request_id,
                            provider_id: cost.window.provider_id.clone(),
                            account_scope_ref: cost.window.account_scope_ref.clone(),
                            window_key: cost.window.window_key.clone(),
                            unit: cost.unit.clone(),
                            ledger_commit: commit,
                            cost: cost.amount,
                            billing: AttemptBilling::Unknown,
                        })
                        .collect();
                    let armed = mark_armed(&mut pacer, admission, &attempts)
                        .map_err(|error| {
                            conflict(&format!("quota arm rejected: {error}"))
                        })?;
                    for attempt in &attempts {
                        tx.execute(
                            "INSERT INTO quota_attempts(request_id,window_key,commit_seq,data) VALUES(?1,?2,?3,?4)",
                            params![attempt.request_id.to_string(), attempt.window_key,
                                commit.seq.0 as i64, serde_json::to_string(attempt)?],
                        )
                        .map_err(storage_error)?;
                    }
                    write_pacer(&tx, &pacer, seq)?;
                    armed
                };
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok((armed, commit))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// A conclusive retry of the same sealed request dispatches a second wire
    /// attempt. Record that attempt so the ledger reflects every real send;
    /// the reservation keeps its bound cost until coverage proves it.
    pub fn record_quota_redispatch(
        &self,
        admission: &QuotaAdmission,
    ) -> Result<CommitRef, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let pacer = read_pacer(&tx)?;
                let armed = pacer.reservation(&admission.request_id).is_some_and(|reservation| {
                    reservation.dispatch == Some(DispatchEvidence::ArmedOrUnknown)
                        && reservation.admission == *admission
                });
                if !armed {
                    return Err(conflict(
                        "quota redispatch without an armed reservation",
                    ));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                for cost in &admission.estimated_cost {
                    let attempt = LocalAttempt {
                        request_id: admission.request_id,
                        provider_id: cost.window.provider_id.clone(),
                        account_scope_ref: cost.window.account_scope_ref.clone(),
                        window_key: cost.window.window_key.clone(),
                        unit: cost.unit.clone(),
                        ledger_commit: commit,
                        cost: cost.amount,
                        billing: AttemptBilling::Unknown,
                    };
                    tx.execute(
                        "INSERT INTO quota_attempts(request_id,window_key,commit_seq,data) VALUES(?1,?2,?3,?4)",
                        params![attempt.request_id.to_string(), attempt.window_key,
                            commit.seq.0 as i64, serde_json::to_string(&attempt)?],
                    )
                    .map_err(storage_error)?;
                }
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(commit)
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Cancel a reservation. A never-dispatched request releases its window
    /// budgets; an armed or unknown request keeps the conservative charge
    /// until same-ledger coverage proves the cost.
    pub fn cancel_quota_admission(
        &self,
        admission: &QuotaAdmission,
        evidence: DispatchEvidence,
    ) -> Result<(QuotaAdmission, Vec<WindowCost>, CommitRef), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let mut pacer = read_pacer(&tx)?;
                let (cancelled, retained) = cancel(&mut pacer, admission, evidence)
                    .map_err(|error| conflict(&format!("quota cancel rejected: {error}")))?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                if evidence == DispatchEvidence::NeverDispatched {
                    tx.execute(
                        "DELETE FROM quota_attempts WHERE request_id=?1",
                        [admission.request_id.to_string()],
                    )
                    .map_err(storage_error)?;
                }
                write_pacer(&tx, &pacer, seq)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok((cancelled, retained, commit))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Settle one recorded attempt's cost interval and billing state. The row
    /// is matched on its exact request/window/commit identity; a stale or
    /// rewritten row is a conflict, never overwritten.
    pub fn settle_quota_attempt(
        &self,
        request_id: EntityId,
        window_key: &str,
        seq: LedgerSeq,
        cost: Range,
        billing: AttemptBilling,
    ) -> Result<CommitReceipt, ShenkoError> {
        if Range::new(cost.lower, cost.upper).is_none() {
            return Err(conflict("attempt cost is not a valid range"));
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let old: Option<String> = tx
                    .query_row(
                        "SELECT data FROM quota_attempts WHERE request_id=?1 AND window_key=?2 AND commit_seq=?3",
                        params![request_id.to_string(), window_key, seq.0 as i64],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                let Some(old) = old else {
                    return Err(ShenkoError::not_found("quota attempt row"));
                };
                let mut attempt: LocalAttempt = serde_json::from_str(&old)?;
                if attempt.request_id != request_id
                    || attempt.window_key != window_key
                    || attempt.ledger_commit != CommitRef::new(self.ledger.ledger_id(), seq)
                {
                    return Err(conflict("quota attempt identity mismatch"));
                }
                attempt.cost = cost;
                attempt.billing = billing;
                let commit_seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), commit_seq);
                let changed = tx
                    .execute(
                        "UPDATE quota_attempts SET data=?4 WHERE request_id=?1 AND window_key=?2 AND commit_seq=?3 AND data=?5",
                        params![request_id.to_string(), window_key, seq.0 as i64,
                            serde_json::to_string(&attempt)?, old],
                    )
                    .map_err(storage_error)?;
                if changed != 1 {
                    return Err(conflict("stale quota attempt row"));
                }
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(
                    commit,
                    vec![UpdatedEntity {
                        entity_id: request_id,
                        record_revision: RecordRevision(0),
                    }],
                ))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }
}
