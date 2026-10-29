//! Durable seat records and single-writer lease admission — SPEC §17.1, §35.5.
//!
//! A `TaskSeat` is a stable product identity for one remote-agent work
//! position; a tmux pane id is only its current *location*, never its durable
//! identity. When the same work moves to a different pane/host the old record
//! keeps its history and the new record names it via `supersedes`; when the
//! position is gone the record is retired, not rewritten in place.
//! At most one active record may claim a `(host, pane_id)` location; the
//! invariant is enforced inside the admitting transaction and backed by a
//! partial unique index.
//!
//! `check_seat_write` exposes durable lease/seat facts to trusted executors;
//! it neither arms an operation nor sends input. The current runtime does not
//! yet have a seat-child handler consuming this interface. A successful read
//! is not an atomic check-and-send fence, and native tmux/human input remains
//! outside this database's control (SPEC §35.5).

use crate::Store;
use anyhow::{ensure, Result};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use shenko_types::{
    commit::CommitRef,
    error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError},
    ids::*,
};

pub(crate) const SEAT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS seat_records(
    seat_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    status TEXT NOT NULL,
    supersedes TEXT,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seat_task ON seat_records(task_id, updated_commit_seq);
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_successor ON seat_records(supersedes) WHERE supersedes IS NOT NULL;
-- A pane id is a physical location, and one location hosts at most one live
-- work position (SPEC §17.1): SQLite enforces it on the stored location facts,
-- so no trusted caller can admit a second active seat for the same pane.
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_active_location ON seat_records(
    json_extract(data,'$.location.host'), json_extract(data,'$.location.pane_id')
) WHERE status='active';
CREATE TABLE IF NOT EXISTS seat_leases(
    lease_id TEXT PRIMARY KEY,
    seat_id TEXT NOT NULL,
    holder_task_id TEXT NOT NULL,
    state TEXT NOT NULL,
    end_reason TEXT,
    superseded_by TEXT,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
-- The durable uniqueness rule behind §17.1: at most one lease may be current
-- for one seat at a time, enforced by SQLite rather than by callers.
CREATE UNIQUE INDEX IF NOT EXISTS idx_seat_current_lease
    ON seat_leases(seat_id) WHERE state='current';
CREATE INDEX IF NOT EXISTS idx_lease_holder ON seat_leases(seat_id, holder_task_id);
"#;

/// Current physical reach of a seat — SPEC §17.1 locator fields.
/// Kept as one sealed shape so future adapters add fields without changing
/// the records table. A pane id or host string here is evidence, not identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeatLocation {
    /// Existing tmux foreground TUI (SPEC §35.5 first path).
    Tmux {
        /// Host reach: "local" or an SSH target label the adapter resolved.
        host: String,
        session: String,
        window: u32,
        pane: u32,
        /// tmux's own pane id (e.g. `%42`); stable across reorder on one server.
        pane_id: String,
        cwd: String,
        current_command: String,
    },
}

/// Lifecycle of one seat record. `retired` records keep full history and are
/// still addressable through the supersedes chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatStatus {
    Active,
    Retired,
}

impl SeatStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

/// Durable seat record — SPEC §17.1 TaskSeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatRecord {
    pub schema_version: u32,
    pub seat_id: EntityId,
    /// Stable incarnation; bumps whenever the same record's location was
    /// confirmed to have changed in place (kept for future adapters; this
    /// slice always moves via supersedes so it stays 0).
    pub incarnation: SeatIncarnation,
    /// The task this seat serves. Facts only; a seat does not own the task.
    pub task_id: EntityId,
    /// Adapter family and the version/level it actually negotiated.
    pub adapter: String,
    pub adapter_version: String,
    pub location: SeatLocation,
    /// Seat this record succeeds; the predecessor keeps its own history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<EntityId>,
    pub status: SeatStatus,
    pub record_revision: RecordRevision,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

/// Durable writer lease — SPEC §17.1 单写者.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatLease {
    pub schema_version: u32,
    pub lease_id: EntityId,
    pub seat_id: EntityId,
    /// The task holding write authority; bound at admission, never inferred.
    pub holder_task_id: EntityId,
    /// Location incarnation the holder verified when it acquired the lease.
    pub acquired_at_incarnation: SeatIncarnation,
    pub state: SeatLeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<LeaseEndReason>,
    /// Lease that took over, when this one was superseded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<EntityId>,
    pub record_revision: RecordRevision,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatLeaseState {
    Current,
    Ended,
}

/// Why a lease stopped being writable. `human_takeover` is recorded by the
/// observer that proved it; nothing here fabricates the observation itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseEndReason {
    /// A different writer acquired a newer lease on the same seat.
    Superseded,
    /// The seat moved; the record was superseded by a new location record.
    SeatSuperseded,
    /// The seat record was retired without a successor.
    SeatRetired,
    /// A human (or unmanaged actor) took the input path over.
    HumanTakeover,
    /// The holder released the seat normally.
    Released,
}

/// End reasons a caller may record explicitly through `end_seat_lease`.
/// The remaining `LeaseEndReason` values describe transitions this operation
/// does not perform — supersede, relocation and retirement are recorded only
/// by `acquire_seat_lease`, `relocate_seat` and `retire_seat`, which make the
/// corresponding seat/lease change in the same commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatLeaseEnd {
    /// The holder released the seat normally.
    Released,
    /// A human (or unmanaged actor) took the input path over, proven by a
    /// real observation.
    HumanTakeover,
}

impl SeatLeaseEnd {
    fn reason(self) -> LeaseEndReason {
        match self {
            Self::Released => LeaseEndReason::Released,
            Self::HumanTakeover => LeaseEndReason::HumanTakeover,
        }
    }
}

impl SeatLease {
    pub fn is_current(&self) -> bool {
        self.state == SeatLeaseState::Current
    }
}

fn seat_error(code: &str, retry: RetryStrategy, details: serde_json::Value) -> ShenkoError {
    ShenkoError::Protocol(
        ProtocolError::new(code, ErrorScope::Seat, retry).with_details(details),
    )
}

fn read_seat(conn: &rusqlite::Connection, id: EntityId) -> Result<Option<SeatRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_records WHERE seat_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn read_lease(conn: &rusqlite::Connection, id: EntityId) -> Result<Option<SeatLease>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_leases WHERE lease_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

fn write_seat(tx: &Transaction<'_>, seat: &SeatRecord, seq: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO seat_records(seat_id,task_id,status,supersedes,data,updated_commit_seq)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(seat_id) DO UPDATE SET
             task_id=excluded.task_id, status=excluded.status, supersedes=excluded.supersedes,
             data=excluded.data, updated_commit_seq=excluded.updated_commit_seq",
        params![
            seat.seat_id.to_string(),
            seat.task_id.to_string(),
            seat.status.as_str(),
            seat.supersedes.map(|s| s.to_string()),
            serde_json::to_string(seat)?,
            seq as i64,
        ],
    )?;
    Ok(())
}

fn write_lease(tx: &Transaction<'_>, lease: &SeatLease, seq: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO seat_leases(lease_id,seat_id,holder_task_id,state,end_reason,superseded_by,data,updated_commit_seq)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(lease_id) DO UPDATE SET
             state=excluded.state, end_reason=excluded.end_reason,
             superseded_by=excluded.superseded_by, data=excluded.data,
             updated_commit_seq=excluded.updated_commit_seq",
        params![
            lease.lease_id.to_string(),
            lease.seat_id.to_string(),
            lease.holder_task_id.to_string(),
            match lease.state {
                SeatLeaseState::Current => "current",
                SeatLeaseState::Ended => "ended",
            },
            lease
                .ended_reason
                .map(|r| serde_json::to_string(&r)).transpose()?,
            lease.superseded_by.map(|s| s.to_string()),
            serde_json::to_string(lease)?,
            seq as i64,
        ],
    )?;
    Ok(())
}

fn current_lease(tx: &Transaction<'_>, seat_id: EntityId) -> Result<Option<SeatLease>> {
    let data: Option<String> = tx
        .query_row(
            "SELECT data FROM seat_leases WHERE seat_id=?1 AND state='current'",
            [seat_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

/// The single active seat bound to `(host, pane_id)` — the invariant behind
/// `idx_seat_active_location`, evaluated inside the caller's transaction so a
/// competing registration can never slip between lookup and insert.
fn active_seat_at_pane(
    conn: &rusqlite::Connection,
    host: &str,
    pane_id: &str,
) -> Result<Option<SeatRecord>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM seat_records WHERE status='active'
             AND json_extract(data,'$.location.host')=?1
             AND json_extract(data,'$.location.pane_id')=?2",
            [host, pane_id],
            |r| r.get(0),
        )
        .optional()?;
    data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
}

/// Refuse to admit a second active seat at `location`. The typed error names
/// the incumbent so the caller can retire/relocate it or bind to it after
/// reconciliation — never silently stacked on the same pane. A relocation may
/// replace its own predecessor at the same physical pane: the predecessor is
/// retired before the successor is inserted in the same transaction.
fn claim_seat_location(
    conn: &rusqlite::Connection,
    location: &SeatLocation,
    replacing: Option<EntityId>,
) -> Result<()> {
    let (host, pane_id) = match location {
        SeatLocation::Tmux { host, pane_id, .. } => (host.as_str(), pane_id.as_str()),
    };
    if let Some(incumbent) = active_seat_at_pane(conn, host, pane_id)? {
        if Some(incumbent.seat_id) == replacing {
            return Ok(());
        }
        return Err(seat_error(
            "seat_location_occupied",
            RetryStrategy::AfterReconcile,
            serde_json::json!({
                "claimant_seat_id": incumbent.seat_id.to_string(),
                "claimant_task_id": incumbent.task_id.to_string(),
                "host": host,
                "pane_id": pane_id,
            }),
        )
        .into());
    }
    Ok(())
}

/// End `lease` inside `tx` at `commit`. No-op if already ended (idempotent —
/// a crashed commit must not become a phantom second ending).
fn end_lease_tx(
    tx: &Transaction<'_>,
    lease: &mut SeatLease,
    reason: LeaseEndReason,
    superseded_by: Option<EntityId>,
    commit: CommitRef,
) -> Result<()> {
    if !lease.is_current() {
        return Ok(());
    }
    lease.state = SeatLeaseState::Ended;
    lease.ended_reason = Some(reason);
    lease.superseded_by = superseded_by;
    lease.record_revision = lease.record_revision.next();
    lease.updated_commit = commit;
    write_lease(tx, lease, commit.seq.0)
}

fn seat_log(tx: &Transaction<'_>, store: &Store, commit: CommitRef, count: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,?4)",
        params![
            commit.ledger_id.to_string(),
            commit.seq.0 as i64,
            store.capabilities.incarnation.to_string(),
            count
        ],
    )?;
    Ok(())
}

fn storage_failure(e: &anyhow::Error) -> bool {
    e.is::<rusqlite::Error>() || e.is::<serde_json::Error>()
        || e.downcast_ref::<ShenkoError>()
            .is_some_and(|e| matches!(e, ShenkoError::Storage(_)))
}

impl Store {
    /// Register a newly discovered seat. Returns the existing record when the
    /// id is already known (idempotent re-registration after a restart).
    /// Refuses a second active seat at an already-claimed pane inside the same
    /// transaction — `seat_at_pane` is a lookup hint, not the admission check.
    pub fn register_seat(
        &self,
        seat_id: EntityId,
        task_id: EntityId,
        adapter: impl Into<String>,
        adapter_version: impl Into<String>,
        location: SeatLocation,
    ) -> Result<SeatRecord> {
        let adapter = adapter.into();
        let adapter_version = adapter_version.into();
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(existing) = read_seat(&tx, seat_id)? {
                    ensure!(existing.task_id == task_id && existing.adapter == adapter
                        && existing.adapter_version == adapter_version && existing.location == location,
                        "seat identity already bound to different facts");
                    tx.commit()?;
                    return Ok(existing);
                }
                claim_seat_location(&tx, &location, None)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let seat = SeatRecord {
                    schema_version: 1,
                    seat_id,
                    incarnation: SeatIncarnation(0),
                    task_id,
                    adapter: adapter.into(),
                    adapter_version: adapter_version.into(),
                    location,
                    supersedes: None,
                    status: SeatStatus::Active,
                    record_revision: RecordRevision(0),
                    created_commit: commit,
                    updated_commit: commit,
                };
                write_seat(&tx, &seat, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(seat)
            },
            storage_failure,
        )
    }

    /// Register the confirmed new location of work that used to live at
    /// `previous`. The predecessor record keeps its history, is retired, and
    /// its current lease (if any) ends — a stale pane id never stays writable.
    /// `task_id` must name the predecessor's own task: relocating a seat can
    /// never rebind the position to a different task. The target pane must be
    /// free or still occupied only by this predecessor.
    pub fn relocate_seat(
        &self,
        previous: EntityId,
        task_id: EntityId,
        adapter: impl Into<String>,
        adapter_version: impl Into<String>,
        location: SeatLocation,
    ) -> Result<SeatRecord> {
        let adapter = adapter.into();
        let adapter_version = adapter_version.into();
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut old = read_seat(&tx, previous)?.ok_or_else(|| {
                    ShenkoError::not_found("relocate_seat: unknown predecessor seat")
                })?;
                if old.task_id != task_id {
                    return Err(seat_error(
                        "seat_task_mismatch",
                        RetryStrategy::NeverAutomatically,
                        serde_json::json!({
                            "seat_id": previous.to_string(),
                            "seat_task_id": old.task_id.to_string(),
                            "claimed_task_id": task_id.to_string(),
                        }),
                    )
                    .into());
                }
                // Idempotent replay of an already-committed relocation.
                if let Some(next) = old.supersedes_or_successor(&tx)? {
                    ensure!(next.task_id == task_id && next.adapter == adapter
                        && next.adapter_version == adapter_version && next.location == location,
                        "seat relocation already bound to different facts");
                    tx.commit()?;
                    return Ok(next);
                }
                claim_seat_location(&tx, &location, Some(previous))?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let seat = SeatRecord {
                    schema_version: 1,
                    seat_id: EntityId::new(),
                    incarnation: SeatIncarnation(0),
                    task_id,
                    adapter: adapter.into(),
                    adapter_version: adapter_version.into(),
                    location,
                    supersedes: Some(previous),
                    status: SeatStatus::Active,
                    record_revision: RecordRevision(0),
                    created_commit: commit,
                    updated_commit: commit,
                };
                if let Some(mut lease) = current_lease(&tx, previous)? {
                    end_lease_tx(&tx, &mut lease, LeaseEndReason::SeatSuperseded, None, commit)?;
                }
                old.status = SeatStatus::Retired;
                old.record_revision = old.record_revision.next();
                old.updated_commit = commit;
                write_seat(&tx, &old, seq.0)?;
                write_seat(&tx, &seat, seq.0)?;
                seat_log(&tx, self, commit, 2)?;
                tx.commit()?;
                Ok(seat)
            },
            storage_failure,
        )
    }

    /// Mark a seat record unusable without a successor (pane dead, remote
    /// agent exited). Its current lease ends; the record stays for history.
    pub fn retire_seat(&self, seat_id: EntityId) -> Result<SeatRecord> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut seat = read_seat(&tx, seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("retire_seat: unknown seat"))?;
                if seat.status == SeatStatus::Retired {
                    tx.commit()?;
                    return Ok(seat); // idempotent
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                if let Some(mut lease) = current_lease(&tx, seat_id)? {
                    end_lease_tx(&tx, &mut lease, LeaseEndReason::SeatRetired, None, commit)?;
                }
                seat.status = SeatStatus::Retired;
                seat.record_revision = seat.record_revision.next();
                seat.updated_commit = commit;
                write_seat(&tx, &seat, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(seat)
            },
            storage_failure,
        )
    }

    /// Acquire the single writer lease for a seat. If another lease is
    /// current it is ended with `Superseded` — by §17.1 a new valid writer
    /// always displaces the old one, and the displaced holder's writes are
    /// refused by lease id afterwards.
    /// `lease_id` is the caller's acquisition identity, chosen and durably
    /// saved before the attempt so the outcome is always re-identifiable:
    /// replaying the same
    /// acquisition after a crash or an uncertain commit returns the stored
    /// lease — even one already ended — instead of minting a new lease that
    /// could displace a newer legitimate writer.
    /// Trusted runtime API, not a model-facing authority constructor: callers
    /// must validate the task/controller delegation before acquiring a lease.
    /// These local records are not replicated authority or remote fencing.
    pub fn acquire_seat_lease(
        &self,
        seat_id: EntityId,
        holder_task_id: EntityId,
        lease_id: EntityId,
    ) -> Result<SeatLease> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(existing) = read_lease(&tx, lease_id)? {
                    // This exact acquisition already committed; replay returns
                    // it verbatim, never a phantom second displacement.
                    ensure!(existing.seat_id == seat_id && existing.holder_task_id == holder_task_id,
                        "seat lease id already bound to different acquisition facts");
                    tx.commit()?;
                    return Ok(existing);
                }
                let seat = read_seat(&tx, seat_id)?
                    .ok_or_else(|| ShenkoError::not_found("acquire_seat_lease: unknown seat"))?;
                ensure!(
                    seat.status == SeatStatus::Active,
                    seat_error(
                        "seat_retired",
                        RetryStrategy::AfterReconcile,
                        serde_json::json!({"seat_id": seat_id.to_string()}),
                    )
                );
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let lease = SeatLease {
                    schema_version: 1,
                    lease_id,
                    seat_id,
                    holder_task_id,
                    acquired_at_incarnation: seat.incarnation,
                    state: SeatLeaseState::Current,
                    ended_reason: None,
                    superseded_by: None,
                    record_revision: RecordRevision(0),
                    created_commit: commit,
                    updated_commit: commit,
                };
                if let Some(mut old_lease) = current_lease(&tx, seat_id)? {
                    end_lease_tx(
                        &tx,
                        &mut old_lease,
                        LeaseEndReason::Superseded,
                        Some(lease.lease_id),
                        commit,
                    )?;
                }
                write_lease(&tx, &lease, seq.0)?;
                seat_log(&tx, self, commit, 1)?;
                tx.commit()?;
                Ok(lease)
            },
            storage_failure,
        )
    }

    /// End a lease explicitly: a normal release or a proven human takeover.
    /// `SeatLeaseEnd` cannot express `Superseded`/`SeatSuperseded`/`SeatRetired`
    /// — those reasons only become true alongside the transition that produces
    /// them, so the public path can never record a lifecycle claim without
    /// making it. Idempotent: replaying an end on an already-ended lease
    /// returns the stored record unchanged.
    pub fn end_seat_lease(&self, lease_id: EntityId, end: SeatLeaseEnd) -> Result<SeatLease> {
        self.ledger.with_mutation(
            || {
                let mut c = self.ledger.conn.lock();
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut lease = read_lease(&tx, lease_id)?
                    .ok_or_else(|| ShenkoError::not_found("end_seat_lease: unknown lease"))?;
                if lease.is_current() {
                    let seq = self.ledger.allocate_seq(&tx)?;
                    let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                    end_lease_tx(&tx, &mut lease, end.reason(), None, commit)?;
                    seat_log(&tx, self, commit, 1)?;
                }
                tx.commit()?;
                Ok(lease)
            },
            storage_failure,
        )
    }

    /// Write admission — SPEC §17.1, §35.5. Returns the *committed* lease when
    /// `lease_id` still carries write authority on a live seat for `task_id`.
    /// Every refusal is typed: the caller must re-observe/reconcile rather
    /// than blind-send. Lease id binds the holder epoch; a lease superseded
    /// after admission can never become current again.
    pub fn check_seat_write(&self, lease_id: EntityId, task_id: EntityId) -> Result<SeatLease> {
        let mut connection = self.ledger.conn.lock();
        let conn = connection.transaction()?;
        let lease = read_lease(&conn, lease_id)?.ok_or_else(|| {
            seat_error(
                "seat_lease_unknown",
                RetryStrategy::AfterReconcile,
                serde_json::json!({"lease_id": lease_id.to_string()}),
            )
        })?;
        if lease.holder_task_id != task_id {
            return Err(seat_error(
                "seat_lease_holder_mismatch",
                RetryStrategy::NeverAutomatically,
                serde_json::json!({
                    "lease_id": lease_id.to_string(),
                    "holder": lease.holder_task_id.to_string(),
                    "claiming_task": task_id.to_string(),
                }),
            )
            .into());
        }
        if !lease.is_current() {
            // The end reason goes into details as its real JSON value so
            // consumers can deserialize `reason` back into `LeaseEndReason`.
            let reason = lease
                .ended_reason
                .map(serde_json::to_value).transpose()?
                .unwrap_or_else(|| serde_json::Value::String("ended".to_string()));
            return Err(seat_error(
                "seat_lease_ended",
                match lease.ended_reason {
                    Some(LeaseEndReason::SeatSuperseded | LeaseEndReason::SeatRetired) =>
                        RetryStrategy::AfterReconcile,
                    _ => RetryStrategy::NeverAutomatically,
                },
                serde_json::json!({"lease_id": lease_id.to_string(), "reason": reason}),
            )
            .into());
        }
        let seat = read_seat(&conn, lease.seat_id)?.ok_or_else(|| {
            seat_error(
                "seat_record_missing",
                RetryStrategy::AfterReconcile,
                serde_json::json!({"seat_id": lease.seat_id.to_string()}),
            )
        })?;
        if seat.status != SeatStatus::Active {
            return Err(seat_error(
                "seat_retired",
                RetryStrategy::AfterReconcile,
                serde_json::json!({"seat_id": seat.seat_id.to_string()}),
            )
            .into());
        }
        if seat.incarnation != lease.acquired_at_incarnation {
            return Err(seat_error(
                "seat_location_changed",
                RetryStrategy::AfterReconcile,
                serde_json::json!({
                    "seat_id": seat.seat_id.to_string(),
                    "acquired_at": lease.acquired_at_incarnation.0,
                    "current": seat.incarnation.0,
                }),
            )
            .into());
        }
        Ok(lease)
    }

    /// Read one seat record.
    pub fn seat(&self, seat_id: EntityId) -> Result<Option<SeatRecord>> {
        read_seat(&self.ledger.conn.lock(), seat_id)
    }

    /// Read one lease.
    pub fn seat_lease(&self, lease_id: EntityId) -> Result<Option<SeatLease>> {
        read_lease(&self.ledger.conn.lock(), lease_id)
    }

    /// All seats registered for a task, newest first — the location history
    /// §17.1 requires keeping when a seat moves.
    pub fn seat_task_seats(&self, task_id: EntityId) -> Result<Vec<SeatRecord>> {
        let conn = self.ledger.conn.lock();
        let mut q = conn.prepare(
            "SELECT data FROM seat_records WHERE task_id=?1 ORDER BY updated_commit_seq DESC",
        )?;
        let rows = q
            .query_map([task_id.to_string()], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter()
            .map(|d| serde_json::from_str(d).map_err(Into::into))
            .collect()
    }

    /// Follow the supersedes chain to the latest record for the
    /// work that used to sit at `seat_id` (or the record itself if it is
    /// still the latest one). The latest record may itself be retired.
    pub fn seat_successor(&self, seat_id: EntityId) -> Result<Option<SeatRecord>> {
        let conn = self.ledger.conn.lock();
        let mut current = match read_seat(&conn, seat_id)? {
            Some(s) => s,
            None => return Ok(None),
        };
        // Every link visits one more record, so a walk that revisits an id can
        // only be a malformed cycle: surface it, never loop or cap valid moves.
        let mut visited = std::collections::HashSet::from([current.seat_id]);
        loop {
            let next: Option<String> = conn
                .query_row(
                    "SELECT data FROM seat_records WHERE supersedes=?1",
                    [current.seat_id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            match next {
                None => return Ok(Some(current)),
                Some(data) => {
                    current = serde_json::from_str(&data)?;
                    if !visited.insert(current.seat_id) {
                        return Err(ShenkoError::conflict(format!(
                            "seat supersedes chain cycles at {}",
                            current.seat_id
                        ))
                        .into());
                    }
                }
            }
        }
    }

    /// Find a live seat at a given tmux pane id — the "find the existing
    /// work position again" lookup. Returns at most one *active* record; a
    /// retired record at the same locator is not resurrected.
    pub fn seat_at_pane(&self, host: &str, pane_id: &str) -> Result<Option<SeatRecord>> {
        active_seat_at_pane(&self.ledger.conn.lock(), host, pane_id)
    }
}

impl SeatRecord {
    /// Successor record that superseded this one, if relocation was recorded.
    fn supersedes_or_successor(&self, conn: &rusqlite::Connection) -> Result<Option<SeatRecord>> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM seat_records WHERE supersedes=?1",
                [self.seat_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        data.map(|d| serde_json::from_str(&d).map_err(Into::into)).transpose()
    }
}
