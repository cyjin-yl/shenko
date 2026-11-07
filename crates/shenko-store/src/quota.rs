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
    cancel, mark_armed, AdmissionOutcome, AttemptBilling, BackendState, DispatchEvidence, Choice,
    LocalAttempt, Pacer, PolicyDecision, QuotaAdmission, QuotaEvent, QuotaEventKind, QuotaForecast,
    QuotaPolicy, QuotaSnapshot, Range, SourceKind, WindowCost, WindowRef,
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
    provider_id TEXT NOT NULL,
    account_scope_ref TEXT NOT NULL,
    window_key TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (request_id, provider_id, account_scope_ref, window_key, commit_seq)
);
CREATE TABLE IF NOT EXISTS quota_sends (
    send_id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL,
    send_seq INTEGER NOT NULL,
    claimed INTEGER NOT NULL DEFAULT 0,
    claimed_seq INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS quota_events (
    event_id TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    account_scope_ref TEXT NOT NULL,
    kind TEXT NOT NULL,
    data TEXT NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_quota_events_scope ON quota_events(account_scope_ref, commit_seq);
"#;

/// The pacer write produced by `admit_quota_send`: the typed admission
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

/// Write one durable quota event inside the transaction that wrote the fact
/// it describes. The event and its cause therefore commit together: a
/// recorded observation always has its notification, and a notification
/// never exists without the fact.
fn insert_quota_event(
    tx: &Transaction<'_>,
    event: &QuotaEvent,
    seq: LedgerSeq,
) -> Result<(), ShenkoError> {
    let data = serde_json::to_string(event)?;
    tx.execute(
        "INSERT INTO quota_events(event_id,provider_id,account_scope_ref,kind,data,commit_seq)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            event.event_id.to_string(),
            event.provider_id,
            event.account_scope_ref,
            event.kind.as_str(),
            data,
            seq.0 as i64
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

/// The window keys of a snapshot whose period end moved forward since the
/// previous reading of the same source. A new period is an observed reset,
/// not a silently replaced balance.
fn reset_window_keys(previous: &QuotaSnapshot, current: &QuotaSnapshot) -> Vec<String> {
    let mut keys = Vec::new();
    for window in &current.windows {
        let Some(before) = previous.windows.iter().find(|old| old.window_key == window.window_key)
        else {
            continue;
        };
        let (Some(before_reset), Some(after_reset)) = (before.resets_at, window.resets_at) else {
            continue;
        };
        if after_reset > before_reset {
            keys.push(window.window_key.clone());
        }
    }
    keys
}

/// The profile a decision selects, if it selects one. A wait, an exhausted
/// provider or a blocked observation selects no model.
fn chosen_profile(choice: &Choice) -> Option<String> {
    match choice {
        Choice::Cloud { profile_id, .. } | Choice::Local { profile_id } => Some(profile_id.clone()),
        _ => None,
    }
}

/// The previous forecast of one account scope, ignoring one forecast id.
///
/// The forecast table stores the whole record, so the scope is resolved
/// from the record itself. The scan is bounded: only the newest rows can
/// be the previous forecast of the scope.
fn previous_scope_forecast(
    tx: &Transaction<'_>,
    provider_id: &str,
    account_scope_ref: &str,
    exclude: EntityId,
) -> Result<Option<QuotaForecast>, ShenkoError> {
    let mut stmt = tx
        .prepare(
            "SELECT data FROM quota_forecasts WHERE forecast_id<>?1 ORDER BY commit_seq DESC LIMIT 16",
        )
        .map_err(storage_error)?;
    let rows = stmt
        .query_map([exclude.to_string()], |r| r.get::<_, String>(0))
        .map_err(storage_error)?;
    for row in rows {
        let data = row.map_err(storage_error)?;
        let forecast: QuotaForecast = serde_json::from_str(&data)?;
        if forecast.provider_id == provider_id && forecast.account_scope_ref == account_scope_ref {
            return Ok(Some(forecast));
        }
    }
    Ok(None)
}

/// Whether any quota policy is recorded. With none, inference dispatch has no
/// quota gate at all and admissions are never required.
pub(crate) fn quota_gate_open(conn: &Connection) -> Result<bool, ShenkoError> {
    let count: i64 = conn
        .query_row("SELECT count(*) FROM quota_policies", [], |r| r.get(0))
        .map_err(storage_error)?;
    Ok(count > 0)
}

/// The sends of one sealed request in arming-commit order with each send's
/// claim commit seq (0 = unclaimed); a claimed send is the one an
/// AwaitingModel CAS durably bound, and claimed_seq orders dispatch
/// bindings across retries.
pub(crate) fn quota_request_sends(
    conn: &Connection,
    request_id: EntityId,
) -> Result<Vec<(EntityId, u64)>, ShenkoError> {
    let mut stmt = conn
        .prepare("SELECT send_id, claimed_seq FROM quota_sends WHERE request_id=?1 ORDER BY send_seq")
        .map_err(storage_error)?;
    let rows = stmt
        .query_map(
            [request_id.to_string()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .map_err(storage_error)?;
    let mut sends = Vec::new();
    for row in rows {
        let (send, claimed_seq) = row.map_err(storage_error)?;
        let send_id = send.parse::<EntityId>().map_err(|_| {
            conflict("quota_sends row has a malformed send id")
        })?;
        sends.push((send_id, claimed_seq.max(0) as u64));
    }
    Ok(sends)
}

/// The dispatch claim inside every AwaitingModel step CAS: once quota is
/// configured, a request may be dispatched only through the exact send the
/// admitting gate selected. The claim verifies the send's lineage to this
/// immutable request, its admitted+armed+unclaimed state, and claims it in
/// the same transaction as the step CAS — never re-reading a mutable
/// "latest" — so a send published by another writer can neither hijack the
/// claim nor strand this send's budget. `claim_seq` stamps the durable
/// dispatch order for later evidence binding.
pub(crate) fn quota_claim_send(
    tx: &Transaction<'_>,
    request_id: EntityId,
    send_id: EntityId,
    claim_seq: LedgerSeq,
) -> Result<bool, ShenkoError> {
    let sends = quota_request_sends(tx, request_id)?;
    let Some((_, claimed_seq)) = sends.iter().find(|(known, _)| *known == send_id) else {
        return Ok(false);
    };
    if *claimed_seq != 0 {
        return Ok(false);
    }
    let pacer = read_pacer(tx)?;
    let armed = pacer.reservation(&send_id).is_some_and(|reservation| {
        reservation.admission.state == shenko_quota::AdmissionState::Admitted
            && reservation.dispatch == Some(DispatchEvidence::ArmedOrUnknown)
    });
    if !armed {
        return Ok(false);
    }
    Ok(tx
        .execute(
            "UPDATE quota_sends SET claimed=1, claimed_seq=?2 WHERE send_id=?1 AND claimed=0",
            params![send_id.to_string(), claim_seq.0 as i64],
        )
        .map_err(storage_error)?
        == 1)
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
                // The previous reading of the same source, used only to see
                // whether a window started a new period.
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT data FROM quota_snapshots
                         WHERE provider_id=?1 AND account_scope_ref=?2 AND snapshot_id<>?3
                         ORDER BY commit_seq DESC LIMIT 1",
                        params![snapshot.provider_id, snapshot.account_scope_ref,
                            snapshot.snapshot_id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                let observed = QuotaEvent::new(
                    snapshot.provider_id.clone(),
                    snapshot.account_scope_ref.clone(),
                    QuotaEventKind::Observed,
                    serde_json::json!({
                        "snapshot_id": snapshot.snapshot_id,
                        "source_kind": snapshot.source_kind,
                        "source_contract_ref": snapshot.source_contract_ref,
                        "source_version": snapshot.source_version,
                        "scope": snapshot.scope,
                        "attribution": snapshot.attribution,
                        "observation_status": snapshot.observation_status,
                        "observation_error": snapshot.observation_error,
                        "windows": snapshot.windows.iter().map(|window| serde_json::json!({
                            "window_key": window.window_key,
                            "scope": window.scope,
                            "metric": window.metric,
                            "unit": window.unit,
                            "kind": window.kind,
                            "enforcement": window.enforcement,
                            "remaining": window.remaining,
                            "used": window.used,
                            "capacity": window.capacity,
                            "resets_at": window.resets_at,
                            "replenishes_by": window.replenishes_by,
                            "inherits_from": window.inherits_from,
                        })).collect::<Vec<_>>(),
                        "absent_windows": snapshot.absent_windows,
                        "observed_at": snapshot.observed_at,
                    }),
                    snapshot.observed_at,
                );
                insert_quota_event(&tx, &observed, seq)?;
                if let Some(previous) = previous {
                    if let Ok(previous) = serde_json::from_str::<QuotaSnapshot>(&previous) {
                        let reset = reset_window_keys(&previous, snapshot);
                        if !reset.is_empty() {
                            let event = QuotaEvent::new(
                                snapshot.provider_id.clone(),
                                snapshot.account_scope_ref.clone(),
                                QuotaEventKind::ResetObserved,
                                serde_json::json!({
                                    "snapshot_id": snapshot.snapshot_id,
                                    "previous_snapshot_id": previous.snapshot_id,
                                    "windows": reset,
                                }),
                                snapshot.observed_at,
                            );
                            insert_quota_event(&tx, &event, seq)?;
                        }
                    }
                }
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
                // A forecast whose planning view moved is a fact the subject
                // may act on. The comparison is against the previous
                // forecast of the same scope, so a recomputation of the same
                // numbers stays quiet.
                if let Some(previous) = previous_scope_forecast(
                    &tx,
                    &forecast.provider_id,
                    &forecast.account_scope_ref,
                    forecast.forecast_id,
                )? {
                    if shenko_quota::forecast_material(&previous)
                        != shenko_quota::forecast_material(forecast)
                    {
                        let event = QuotaEvent::new(
                            forecast.provider_id.clone(),
                            forecast.account_scope_ref.clone(),
                            QuotaEventKind::ForecastChanged,
                            serde_json::json!({
                                "forecast_id": forecast.forecast_id,
                                "previous_forecast_id": previous.forecast_id,
                                "based_on_snapshot": forecast.based_on_snapshot,
                                "freshness": forecast.freshness,
                                "source_validity": forecast.source_validity,
                                // Which replaceable forecast strategy produced
                                // the new plan (SPEC §42.1). Absent on legacy
                                // records: unknown, never assumed.
                                "cost_model_version": forecast.cost_model_version.clone(),
                                // How far the plan reaches and how much it can
                                // be trusted (SPEC §42.3).
                                "horizon": forecast.horizon,
                                "confidence": forecast.confidence,
                                "windows": forecast.windows.iter().map(|window| serde_json::json!({
                                    "window_key": window.window_key,
                                    "unit": window.unit,
                                    "basis": window.basis,
                                    "planning": window.planning,
                                    "remaining": window.remaining,
                                    "unreflected_own_cost": window.unreflected_own_cost,
                                })).collect::<Vec<_>>(),
                            }),
                            forecast.computed_at,
                        );
                        insert_quota_event(&tx, &event, seq)?;
                    }
                }
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
                // The policy belongs to the accounts its profiles charge, so
                // the event names the first scope it binds. A policy with
                // no cloud profile binds no account and names none.
                let (provider_id, account_scope_ref) = policy
                    .profiles
                    .iter()
                    .flat_map(|profile| profile.cost_per_request.iter())
                    .map(|cost| (cost.window.provider_id.clone(), cost.window.account_scope_ref.clone()))
                    .next()
                    .unwrap_or_default();
                let event = QuotaEvent::new(
                    provider_id,
                    account_scope_ref,
                    QuotaEventKind::PolicyChanged,
                    serde_json::json!({
                        "policy_id": policy.policy_id,
                        "revision": policy.revision,
                        "profiles": policy.profiles.iter()
                            .map(|profile| profile.profile_id.clone()).collect::<Vec<_>>(),
                    }),
                    now,
                );
                insert_quota_event(&tx, &event, seq)?;
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
                    let forecast_data: Option<String> = tx
                        .query_row(
                            "SELECT data FROM quota_forecasts WHERE forecast_id=?1",
                            [forecast_id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?;
                    let Some(forecast_data) = forecast_data else {
                        return Err(conflict("decision cites an unrecorded forecast"));
                    };
                    let cited: QuotaForecast = serde_json::from_str(&forecast_data)?;
                    // The forecast chain must have been produced under this
                    // policy's own source admission: the base snapshot must
                    // be recorded, come from a source this policy admits,
                    // and be computed with that source's exact TTL and
                    // stale-use options. A forecast produced under another
                    // source's (or a guessed) lifetime can never authorize
                    // admission under this policy.
                    let snapshot_data: Option<String> = tx
                        .query_row(
                            "SELECT data FROM quota_snapshots WHERE snapshot_id=?1",
                            [cited.based_on_snapshot.to_string()],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?;
                    let Some(snapshot_data) = snapshot_data else {
                        return Err(conflict("cited forecast has no recorded snapshot"));
                    };
                    let snapshot: QuotaSnapshot = serde_json::from_str(&snapshot_data)?;
                    // The cited forecast may only rest on a source this
                    // policy admits, under that source's own contract and
                    // with that source's exact lifetime options.
                    let admitted = policy.observation_sources.iter().any(|entry| {
                        entry.source_kind == snapshot.source_kind
                            && entry.admits_contract(&snapshot.source_contract_ref)
                            && entry.options() == cited.options
                    });
                    if !admitted {
                        return Err(conflict(
                            "decision relies on a source the policy does not admit under its own options",
                        ));
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
                // A change of the selected model is a fact the subject may
                // act on, but only when the route really changed: repeating
                // the same decision is not a new routing event.
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT data FROM quota_decisions WHERE decision_id<>?1 ORDER BY commit_seq DESC LIMIT 1",
                        [decision.decision_id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                let previous_profile = previous
                    .as_deref()
                    .map(serde_json::from_str::<PolicyDecision>)
                    .transpose()?
                    .and_then(|previous| chosen_profile(&previous.choice));
                let current_profile = chosen_profile(&decision.choice);
                if previous.is_some() && previous_profile != current_profile {
                    let (provider_id, account_scope_ref) = decision
                        .constraints
                        .first()
                        .map(|constraint| {
                            (
                                constraint.window.provider_id.clone(),
                                constraint.window.account_scope_ref.clone(),
                            )
                        })
                        .or_else(|| {
                            current_profile.as_ref().and_then(|profile_id| {
                                policy.profiles.iter().find(|profile| &profile.profile_id == profile_id)
                            })
                            .and_then(|profile| profile.cost_per_request.first())
                            .map(|cost| {
                                (
                                    cost.window.provider_id.clone(),
                                    cost.window.account_scope_ref.clone(),
                                )
                            })
                        })
                        .unwrap_or_default();
                    let event = QuotaEvent::new(
                        provider_id,
                        account_scope_ref,
                        QuotaEventKind::RoutingChanged,
                        serde_json::json!({
                            "decision_id": decision.decision_id,
                            "policy_id": decision.policy_id,
                            "policy_revision": decision.policy_revision,
                            "profile_id": current_profile,
                            "previous_profile_id": previous_profile,
                            "choice": decision.choice,
                            "decided_at": decision.decided_at,
                        }),
                        decision.decided_at,
                    );
                    insert_quota_event(&tx, &event, seq)?;
                }
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

    /// The most recently recorded snapshot for one provider/account scope
    /// produced by one observation source. Forecasts may only consume a
    /// snapshot through the policy's admission entry for that exact source.
    pub fn latest_quota_snapshot_for_source(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
        source: SourceKind,
    ) -> Result<Option<QuotaSnapshot>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT data FROM quota_snapshots WHERE provider_id=?1 AND account_scope_ref=?2 ORDER BY commit_seq DESC",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![provider_id, account_scope_ref], |r| r.get::<_, String>(0))
            .map_err(storage_error)?;
        for row in rows {
            let data = row.map_err(storage_error)?;
            let snapshot: QuotaSnapshot = serde_json::from_str(&data)?;
            if snapshot.source_kind == source {
                return Ok(Some(snapshot));
            }
        }
        Ok(None)
    }

    /// The most recently recorded snapshot of one exact source contract.
    ///
    /// One source kind can carry several contracts — a configured limit
    /// document and a usage report are both management API facts but not
    /// the same fact. A policy admits a contract, so the forecast chain
    /// must be able to pick exactly the reading that contract produced.
    pub fn latest_quota_snapshot_for_contract(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
        source: SourceKind,
        contract: &str,
    ) -> Result<Option<QuotaSnapshot>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT data FROM quota_snapshots WHERE provider_id=?1 AND account_scope_ref=?2 ORDER BY commit_seq DESC",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![provider_id, account_scope_ref], |r| r.get::<_, String>(0))
            .map_err(storage_error)?;
        for row in rows {
            let data = row.map_err(storage_error)?;
            let snapshot: QuotaSnapshot = serde_json::from_str(&data)?;
            if snapshot.source_kind == source && snapshot.source_contract_ref == contract {
                return Ok(Some(snapshot));
            }
        }
        Ok(None)
    }

    /// The most recently recorded snapshot of one provider/account scope,
    /// whatever source contract produced it.
    ///
    /// The subject-readable view needs the newest reading of an account,
    /// not a reading of one contract: the contracts are different facts
    /// about the same account and the view names the one it shows.
    pub fn latest_quota_snapshot(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
    ) -> Result<Option<QuotaSnapshot>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT data FROM quota_snapshots WHERE provider_id=?1 AND account_scope_ref=?2 ORDER BY commit_seq DESC LIMIT 1",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![provider_id, account_scope_ref], |r| r.get::<_, String>(0))
            .map_err(storage_error)?;
        for row in rows {
            let data = row.map_err(storage_error)?;
            let snapshot: QuotaSnapshot = serde_json::from_str(&data)?;
            return Ok(Some(snapshot));
        }
        Ok(None)
    }

    /// The most recently recorded forecast of one provider/account scope.
    ///
    /// The subject-readable view needs the plan its current decision was
    /// computed from, and that plan belongs to the account, not to one
    /// window. The scope lives inside the forecast body (the table predates
    /// per-scope columns), so the lookup reads it from the recorded JSON
    /// instead of adding a column or rewriting existing rows.
    pub fn latest_quota_forecast(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
    ) -> Result<Option<QuotaForecast>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM quota_forecasts
                 WHERE json_extract(data,'$.provider_id')=?1
                   AND json_extract(data,'$.account_scope_ref')=?2
                 ORDER BY commit_seq DESC LIMIT 1",
                params![provider_id, account_scope_ref],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let Some(data) = data else { return Ok(None) };
        let forecast: QuotaForecast = serde_json::from_str(&data)?;
        if forecast.provider_id != provider_id || forecast.account_scope_ref != account_scope_ref {
            return Err(conflict("recorded forecast scope differs from its stored columns"));
        }
        Ok(Some(forecast))
    }

    /// Measured useful work of one subject's admitted rounds, read from the
    /// ledger's own record (SPEC §42.3 `expected_useful_work`).
    ///
    /// Rounds are the subject's own request attempts that reached terminal
    /// provider evidence; useful outcomes are the steps among them whose S1
    /// action durably committed. A step that was retried, rejected or never
    /// committed counts as a round that did not advance the subject — this is
    /// a measurement, never a score and never "one request equals one useful
    /// outcome". A subject with no terminal round has no evidence: `None`,
    /// not zero.
    pub fn quota_work_evidence(
        &self,
        agent_id: EntityId,
    ) -> Result<Option<shenko_quota::WorkEvidence>, ShenkoError> {
        // The ledger position the counts below are read at. Read first: the
        // connection guard must not be held across another lock.
        let head = self.current_commit_seq()?;
        let conn = self.ledger.conn.lock();
        let rounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subject_request_attempts WHERE agent_id=?1 AND state='terminal'",
                [agent_id.to_string()],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        if rounds <= 0 {
            return Ok(None);
        }
        let useful: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT attempts.step_id) FROM subject_request_attempts AS attempts
                 JOIN subject_steps AS steps ON steps.step_id=attempts.step_id
                 WHERE attempts.agent_id=?1 AND attempts.state='terminal'
                   AND json_extract(steps.data,'$.commit_ref') IS NOT NULL",
                [agent_id.to_string()],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        if useful < 0 || useful > rounds {
            // A committed step per terminal round can never exceed one; a
            // larger count means the durable rows contradict the unit, and
            // the honest answer is that this cannot be measured.
            return Ok(None);
        }
        Ok(Some(shenko_quota::WorkEvidence {
            unit: shenko_quota::WorkUnit::CommittedSubjectStep,
            rounds_observed: rounds as u64,
            useful_observed: useful as u64,
            evidence: CommitRef::new(self.ledger.ledger_id(), LedgerSeq(head)),
        }))
    }

    /// Record one durable quota event (SPEC §42.6). Re-recording the
    /// identical event is idempotent; the same id with different bytes is a
    /// conflict.
    pub fn record_quota_event(&self, event: &QuotaEvent) -> Result<CommitReceipt, ShenkoError> {
        if event.event_id == EntityId::nil() {
            return Err(conflict("quota event has no identity"));
        }
        self.ledger.with_mutation(
            || {
                let data = serde_json::to_string(event)?;
                let event_id = event.event_id.to_string();
                let provider_id = event.provider_id.clone();
                let account_scope_ref = event.account_scope_ref.clone();
                let kind = event.kind.as_str();
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let existing: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT data, commit_seq FROM quota_events WHERE event_id=?1",
                        [&event_id],
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
                    return Err(conflict("quota event id exists with different content"));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                tx.execute(
                    "INSERT INTO quota_events(event_id,provider_id,account_scope_ref,kind,data,commit_seq)
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        event_id,
                        provider_id,
                        account_scope_ref,
                        kind,
                        data,
                        seq.0 as i64
                    ],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, commit, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(self.subject_receipt(
                    commit,
                    vec![UpdatedEntity {
                        entity_id: event.event_id,
                        record_revision: RecordRevision(0),
                    }],
                ))
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Durable quota events after a commit position, oldest first, each with
    /// the commit position it was written at. This is the subject-readable
    /// stream of quota facts (SPEC §42.6).
    pub fn quota_events_since(
        &self,
        after_commit_seq: u64,
        limit: usize,
    ) -> Result<Vec<(u64, QuotaEvent)>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT commit_seq, data FROM quota_events WHERE commit_seq > ?1 ORDER BY commit_seq LIMIT ?2",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![after_commit_seq as i64, limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(storage_error)?;
        let mut events = Vec::new();
        for row in rows {
            let (seq, data) = row.map_err(storage_error)?;
            events.push((seq.max(0) as u64, serde_json::from_str(&data)?));
        }
        Ok(events)
    }

    /// The commit position of the newest quota event, or zero when there is
    /// none. A reader that starts here sees the facts that happen next, not
    /// a replay of the whole history.
    pub fn latest_quota_event_seq(&self) -> Result<u64, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let seq: Option<i64> = conn
            .query_row("SELECT max(commit_seq) FROM quota_events", [], |r| r.get(0))
            .map_err(storage_error)?;
        Ok(seq.unwrap_or(0).max(0) as u64)
    }

    /// The most recent event of one kind for one provider/account scope.
    /// It is the comparison point for merging repeated notifications.
    pub fn latest_quota_event(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
        kind: QuotaEventKind,
    ) -> Result<Option<QuotaEvent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM quota_events WHERE provider_id=?1 AND account_scope_ref=?2 AND kind=?3
                 ORDER BY commit_seq DESC LIMIT 1",
                params![provider_id, account_scope_ref, kind.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        data.map(|data| serde_json::from_str(&data).map_err(ShenkoError::from)).transpose()
    }

    /// Sends recorded for one sealed request in arming-commit order with
    /// each send's claim commit seq (0 = unclaimed). A claimed send has a
    /// committed AwaitingModel binding; the largest claim seq is the most
    /// recently authorized dispatch.
    pub fn quota_send_ids(
        &self,
        request_id: EntityId,
    ) -> Result<Vec<(EntityId, u64)>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        quota_request_sends(&conn, request_id)
    }

    /// Every recorded attempt in one provider/account scope, commit order.
    pub fn quota_attempts(
        &self,
        provider_id: &str,
        account_scope_ref: &str,
    ) -> Result<Vec<LocalAttempt>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT data FROM quota_attempts WHERE provider_id=?1 AND account_scope_ref=?2 ORDER BY commit_seq",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![provider_id, account_scope_ref], |r| r.get::<_, String>(0))
            .map_err(storage_error)?;
        let mut attempts = Vec::new();
        for row in rows {
            let attempt: LocalAttempt = serde_json::from_str(&row.map_err(storage_error)?)?;
            attempts.push(attempt);
        }
        Ok(attempts)
    }

    /// The current durable pacer; an empty ledger has an empty pacer.
    pub fn quota_pacer(&self) -> Result<Pacer, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_pacer(&conn)
    }

    /// Admission of one wire SEND under the built-in cadence baseline.
    ///
    /// A runtime that installed a cadence strategy uses
    /// [`Self::admit_quota_send_with`]; this entry point is exactly the
    /// zero-plugin configuration and therefore keeps the pre-existing
    /// behavior byte for byte.
    pub fn admit_quota_send(
        &self,
        request_id: EntityId,
        policy: &QuotaPolicy,
        decision: &PolicyDecision,
        send_id: EntityId,
        availability: &HashMap<String, BackendState>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<QuotaAdmissionReceipt, ShenkoError> {
        self.admit_quota_send_with(
            &shenko_quota::QuotaStrategies::baseline(),
            request_id,
            policy,
            decision,
            send_id,
            availability,
            now,
        )
    }

    /// Run typed admission for one wire SEND `send_id` of the immutable
    /// sealed `request_id` and persist any pacer change plus the send's
    /// lineage row in the same mutation, so a charged send can never become
    /// unfindable. An existing lineage row must bind the identical request;
    /// the policy and decision must be the recorded rows verbatim.
    ///
    /// `strategies` supplies the replaceable admission cadence (SPEC §42.1,
    /// §42.5). The cadence is consulted inside this same transaction and may
    /// only withhold a fresh admission: the pacer, the preserved armed
    /// history and every refusal stay the library's own transition, so a
    /// strategy cannot grant budget the portfolio does not have. A refusing
    /// strategy aborts the transaction with nothing written.
    pub fn admit_quota_send_with(
        &self,
        strategies: &shenko_quota::QuotaStrategies,
        request_id: EntityId,
        policy: &QuotaPolicy,
        decision: &PolicyDecision,
        send_id: EntityId,
        availability: &HashMap<String, BackendState>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<QuotaAdmissionReceipt, ShenkoError> {
        let policy_data = serde_json::to_string(policy)?;
        let decision_data = serde_json::to_string(decision)?;
        let request_row = request_id.to_string();
        let send_row = send_id.to_string();
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
                let outcome = shenko_quota::admit_with_cadence(
                    policy,
                    decision,
                    send_id,
                    &mut pacer,
                    availability,
                    now,
                    strategies.cadence(),
                )
                .map_err(|error| conflict(&format!("quota cadence strategy refused: {error}")))?;
                let mut commit = None;
                if pacer != before {
                    let seq = self.ledger.allocate_seq(&tx)?;
                    // The send→request lineage is born with the reservation
                    // it charges; a later arming commit verifies, never
                    // invents, this association.
                    let lineage: Option<String> = tx
                        .query_row(
                            "SELECT request_id FROM quota_sends WHERE send_id=?1",
                            [&send_row],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?;
                    match lineage {
                        Some(bound) if bound == request_row => {}
                        Some(_) => {
                            return Err(conflict("send id is bound to another request"))
                        }
                        None => {
                            tx.execute(
                                "INSERT INTO quota_sends(send_id,request_id,send_seq,claimed) VALUES(?1,?2,?3,0)",
                                params![send_row, request_row, seq.0 as i64],
                            )
                            .map_err(storage_error)?;
                        }
                    }
                    write_pacer(&tx, &pacer, seq)?;
                    // A constraint that stops a request is a fact the
                    // subject may act on. The same constraint repeated for
                    // the same scope is not a new event.
                    let constraint_state = match &outcome {
                        AdmissionOutcome::Quota(admission)
                        | AdmissionOutcome::Existing { admission } => match admission.state {
                            shenko_quota::AdmissionState::QuotaPolicyWait => Some("quota_policy_wait"),
                            shenko_quota::AdmissionState::ProviderExhausted => Some("provider_exhausted"),
                            shenko_quota::AdmissionState::ObservationBlocked => Some("observation_blocked"),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(state) = constraint_state {
                        let (provider_id, account_scope_ref) = decision
                            .constraints
                            .first()
                            .map(|constraint| {
                                (
                                    constraint.window.provider_id.clone(),
                                    constraint.window.account_scope_ref.clone(),
                                )
                            })
                            .unwrap_or_default();
                        let previous: Option<String> = tx
                            .query_row(
                                "SELECT data FROM quota_events
                                 WHERE provider_id=?1 AND account_scope_ref=?2 AND kind=?3
                                 ORDER BY commit_seq DESC LIMIT 1",
                                params![provider_id, account_scope_ref,
                                    QuotaEventKind::ConstraintReached.as_str()],
                                |r| r.get(0),
                            )
                            .optional()
                            .map_err(storage_error)?;
                        let repeated = previous
                            .and_then(|data| serde_json::from_str::<QuotaEvent>(&data).ok())
                            .and_then(|event| {
                                event.payload.get("state").and_then(|value| value.as_str()).map(str::to_owned)
                            })
                            .is_some_and(|last| last == state);
                        if !repeated {
                            let reason = match &outcome {
                                AdmissionOutcome::Quota(admission)
                                | AdmissionOutcome::Existing { admission } => admission.reason.clone(),
                                _ => String::new(),
                            };
                            let event = QuotaEvent::new(
                                provider_id,
                                account_scope_ref,
                                QuotaEventKind::ConstraintReached,
                                serde_json::json!({
                                    "state": state,
                                    "request_id": request_id,
                                    "decision_id": decision.decision_id,
                                    "reason": reason,
                                }),
                                now,
                            );
                            insert_quota_event(&tx, &event, seq)?;
                        }
                    }
                    let commit_ref = CommitRef::new(self.ledger.ledger_id(), seq);
                    self.record_subject_commit(&tx, commit_ref, 1)?;
                    commit = Some(commit_ref);
                } else if pacer.reservation(&send_id).is_some() {
                    // Idempotent replays still prove the existing send's
                    // lineage: a send bound to another request must never
                    // report an existing admission for THIS request.
                    let lineage: Option<String> = tx
                        .query_row(
                            "SELECT request_id FROM quota_sends WHERE send_id=?1",
                            [&send_row],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?;
                    match lineage.as_deref() {
                        Some(bound) if bound == request_row => {}
                        Some(_) => {
                            return Err(conflict("send id is bound to another request"))
                        }
                        None => {
                            return Err(conflict(
                                "charged send has no lineage; reconcile before admitting",
                            ))
                        }
                    }
                }
                tx.commit().map_err(storage_error)?;
                Ok(QuotaAdmissionReceipt { outcome, commit })
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Persist the armed binding for one wire SEND of `request_id` BEFORE
    /// its dispatch: one ledger attempt row per quota window plus the
    /// send→request lineage row, all in this same commit. The immutable
    /// sealed request id is preserved; each retry wires a fresh admission
    /// identity through a new `send_id` recorded here. Replaying an
    /// already-armed send verifies its original rows and lineage instead
    /// of creating parallel attempts; the caller must pass the armed
    /// admission record (from `quota_pacer`), since the pre-arm revision
    /// can never prove its own arming.
    pub fn arm_quota_send(
        &self,
        request_id: EntityId,
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
                                "SELECT 1 FROM quota_attempts WHERE request_id=?1 AND provider_id=?2 AND account_scope_ref=?3 AND window_key=?4 AND commit_seq=?5",
                                params![admission.request_id.to_string(),
                                    bound.window.provider_id, bound.window.account_scope_ref,
                                    bound.window.window_key, bound.ledger_commit.seq.0 as i64],
                                |r| r.get(0),
                            )
                            .optional()
                            .map_err(storage_error)?;
                        if present.is_none() {
                            return Err(conflict("armed attempt row is missing"));
                        }
                    }
                    // The send→request lineage must also match verbatim.
                    let lineage: Option<String> = tx
                        .query_row(
                            "SELECT request_id FROM quota_sends WHERE send_id=?1",
                            [admission.request_id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?;
                    if lineage.as_deref() != Some(request_id.to_string().as_str()) {
                        return Err(conflict("armed send lineage is missing or bound elsewhere"));
                    }
                    admission.clone()
                } else {
                    // The lineage was written by the admission commit; an
                    // armed send that its own admit did not bind is ledger
                    // damage, never silently re-anchored.
                    let lineage: Option<String> = tx
                        .query_row(
                            "SELECT request_id FROM quota_sends WHERE send_id=?1",
                            [admission.request_id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage_error)?;
                    if lineage.as_deref() != Some(request_id.to_string().as_str()) {
                        return Err(conflict("armed send lineage is missing or bound elsewhere"));
                    }
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
                            "INSERT INTO quota_attempts(request_id,provider_id,account_scope_ref,window_key,commit_seq,data) VALUES(?1,?2,?3,?4,?5,?6)",
                            params![attempt.request_id.to_string(), attempt.provider_id,
                                attempt.account_scope_ref, attempt.window_key,
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
    /// is matched on its exact request, full scoped window identity and
    /// commit; a stale or rewritten row is a conflict, never overwritten.
    pub fn settle_quota_attempt(
        &self,
        request_id: EntityId,
        window: &WindowRef,
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
                        "SELECT data FROM quota_attempts WHERE request_id=?1 AND provider_id=?2 AND account_scope_ref=?3 AND window_key=?4 AND commit_seq=?5",
                        params![request_id.to_string(), window.provider_id,
                            window.account_scope_ref, window.window_key, seq.0 as i64],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                let Some(old) = old else {
                    return Err(ShenkoError::not_found("quota attempt row"));
                };
                let mut attempt: LocalAttempt = serde_json::from_str(&old)?;
                if attempt.request_id != request_id
                    || attempt.provider_id != window.provider_id
                    || attempt.account_scope_ref != window.account_scope_ref
                    || attempt.window_key != window.window_key
                    || attempt.ledger_commit != CommitRef::new(self.ledger.ledger_id(), seq)
                {
                    return Err(conflict("quota attempt identity mismatch"));
                }
                if attempt.cost == cost && attempt.billing == billing {
                    // Original E1 can be revisited after a crash between
                    // receipt and billing settlement. An identical settlement
                    // neither charges again nor rewrites a historical attempt.
                    let observed_seq: i64 = tx.query_row(
                        "SELECT COALESCE(max(seq),0) FROM commit_log", [], |row| row.get(0),
                    ).map_err(storage_error)?;
                    tx.commit().map_err(storage_error)?;
                    return Ok(self.subject_receipt(
                        CommitRef::new(self.ledger.ledger_id(), LedgerSeq(observed_seq as u64)), vec![],
                    ));
                }
                attempt.cost = cost;
                attempt.billing = billing;
                let commit_seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), commit_seq);
                let changed = tx
                    .execute(
                        "UPDATE quota_attempts SET data=?6 WHERE request_id=?1 AND provider_id=?2 AND account_scope_ref=?3 AND window_key=?4 AND commit_seq=?5 AND data=?7",
                        params![request_id.to_string(), window.provider_id,
                            window.account_scope_ref, window.window_key, seq.0 as i64,
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
