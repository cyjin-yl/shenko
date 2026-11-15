//! Source-reserved authority for Store decisions and effect admissions.
//!
//! No concrete election provider is named here. The provider orders immutable
//! admissions outside SQLite and reserves its actual source before this Store
//! takes its write lock. Late evidence does not use this admission path.
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use parking_lot::RwLock;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::{CommitAdmission, CommitAuthorityGrant, CommitAuthorityGuard,
        CommitAuthorityProvider, CommitAuthorityStamp, authority_fenced},
    commit::CommitRef,
    error::ShenkoError,
    ids::{ControllerEpoch, EntityId, RuntimeGeneration},
    inbox::{InboxEvent, InboxEventKind},
    subject::{SubjectBranchHead, SubjectMode, SubjectStep},
    task::{TaskOwner, TaskRecord},
};

use crate::{Store, store::{checked_head_mode, read_step, storage_error, validate_step_binding}};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commit_authority_policy (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), source_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS subject_authority_bindings (
 agent_id TEXT PRIMARY KEY, data TEXT NOT NULL, updated_commit_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS control_materializations (
 conflict_key TEXT PRIMARY KEY, action_id TEXT NOT NULL UNIQUE, data TEXT NOT NULL,
 updated_commit_seq INTEGER NOT NULL
);
"#;

#[derive(Default)]
pub(crate) struct AuthorityRuntime {
    provider: RwLock<Option<Arc<dyn CommitAuthorityProvider>>>,
    /// A superseded live process cannot call takeover again and steal back
    /// the new writer's slot. A new Store incarnation explicitly resumes it.
    activated: AtomicBool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectAuthorityBinding {
    pub schema_version: u32,
    pub stamp: CommitAuthorityStamp,
    pub writer_incarnation: EntityId,
    pub controller_epoch: ControllerEpoch,
    pub software_generation: RuntimeGeneration,
    pub committed_at: CommitRef,
}

/// Generic decision positions and factual read dependencies. Concrete Store
/// boundaries supply them; the authority source only orders these identities.
#[derive(Default)]
pub(crate) struct AdmissionPosition {
    pub key: String,
    pub aliases: Vec<String>,
    pub reads: Vec<String>,
    pub operation_id: Option<EntityId>,
    pub attempt_id: Option<EntityId>,
    pub bind_parent: bool,
    pub expected_tasks: Vec<TaskRecord>,
    /// Rows whose structural authority fields are checked by the boundary
    /// itself; later factual progress need not invalidate that authority.
    pub snapshot_tasks: Vec<TaskRecord>,
    pub expected_steps: Vec<SubjectStep>,
}

pub(crate) fn task_revision_position(task: &TaskRecord) -> String {
    format!("task/{}/revision/{}", task.task_id, task.record_revision.0)
}

pub(crate) fn task_authority_position(task: &TaskRecord) -> String {
    format!("task/{}/authority/{}", task.task_id, task.task_revision.0)
}

/// Retain this value until AFTER the Store transaction commits or rolls back.
pub(crate) struct AuthorityReservation {
    guard: Option<Box<dyn CommitAuthorityGuard>>,
    provider: Option<Arc<dyn CommitAuthorityProvider>>,
    recorded: AtomicBool,
    admission: Option<CommitAdmission>,
    grant: Option<CommitAuthorityGrant>,
    expected_tasks: Vec<TaskRecord>,
    expected_steps: Vec<SubjectStep>,
    recovery: bool,
}

impl AuthorityRuntime {
    pub(crate) fn reserve(&self) -> Result<AuthorityReservation, ShenkoError> {
        let provider = self.provider.read().clone();
        Ok(AuthorityReservation {
            guard: provider.as_ref().map(|provider| provider.acquire(None)).transpose()?,
            provider, recorded: AtomicBool::new(false), admission: None, grant: None, expected_tasks: Vec::new(), expected_steps: Vec::new(), recovery: false,
        })
    }

    fn prepare(&self, admission: CommitAdmission) -> Result<AuthorityReservation, ShenkoError> {
        let Some(provider) = self.provider.read().clone() else {
            return Ok(AuthorityReservation { guard: None, provider: None, recorded: AtomicBool::new(false), admission: None, grant: None, expected_tasks: Vec::new(), expected_steps: Vec::new(), recovery: false });
        };
        let grant = provider.prepare(&admission)?;
        if grant.as_ref().is_some_and(|grant| grant.admission != admission) {
            return Err(authority_fenced("authority returned a grant for another immutable action"));
        }
        let guard = provider.acquire(grant.as_ref()).map_err(|error| {
            if let Some(grant) = &grant { provider.action_unmaterialized(grant); }
            error
        })?;
        Ok(AuthorityReservation { guard: Some(guard), provider: Some(provider), recorded: AtomicBool::new(false), admission: Some(admission), grant, expected_tasks: Vec::new(), expected_steps: Vec::new(), recovery: false })
    }
}

impl AuthorityReservation {
    pub(crate) fn check(
        &self, conn: &Connection, incarnation: EntityId, subject: Option<EntityId>,
    ) -> Result<(), ShenkoError> {
        let required: Option<String> = conn.query_row(
            "SELECT source_id FROM commit_authority_policy WHERE singleton=1", [], |r| r.get(0),
        ).optional().map_err(storage_error)?;
        if required.is_none() && self.guard.is_none() { return Ok(()) }
        let stamp = self.guard.as_ref().and_then(|guard| guard.stamp())
            .ok_or_else(|| authority_fenced("no reserved current authority for this Store"))?;
        if required.as_deref() != Some(stamp.source_id.as_str())
            || subject.is_some_and(|subject| subject != stamp.subject_id)
        {
            return Err(authority_fenced("authority source or subject binding changed"));
        }
        let binding = read_binding(conn, stamp.subject_id)?;
        if !self.recovery && binding.as_ref().is_none_or(|binding|
            binding.stamp != *stamp || binding.writer_incarnation != incarnation)
        {
            return Err(authority_fenced("the writer or its authority commitment was superseded"));
        }
        let raw: String = conn.query_row(
            "SELECT data FROM subject_branch_heads WHERE agent_id=?1",
            [stamp.subject_id.to_string()], |r| r.get(0),
        ).map_err(storage_error)?;
        let head: SubjectBranchHead = serde_json::from_str(&raw)?;
        if head.branch_id != stamp.branch_id
            || !matches!(head.mode, SubjectMode::Active | SubjectMode::Paused)
            || (!self.recovery && binding.as_ref().is_none_or(|binding|
                head.controller_epoch != binding.controller_epoch
                    || head.software_generation != binding.software_generation))
        {
            return Err(authority_fenced("the active branch, epoch or software generation changed"));
        }
        if let Some(admission) = &self.admission {
            if admission.subject_id != head.agent_id || admission.branch_id != head.branch_id
                || admission.expected_parent.is_some_and(|parent| parent != head.committed_at)
                || admission.controller_epoch != head.controller_epoch
                || admission.software_generation != head.software_generation
                || admission.expected_head_revision.is_some_and(|revision| revision != head.record_revision)
            {
                return Err(authority_fenced("the ordered action no longer names this Store predecessor"));
            }
        }
        for expected in &self.expected_tasks {
            let raw: String = conn.query_row("SELECT data FROM tasks WHERE task_id=?1",
                [expected.task_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
            let current: TaskRecord = serde_json::from_str(&raw)?;
            if serde_json::to_value(&current)? != serde_json::to_value(expected)? {
                return Err(ShenkoError::conflict("task or ancestor changed after its authority plan was frozen"));
            }
        }
        for expected in &self.expected_steps {
            let current = read_step(conn, &expected.branch_id, expected.step_id)?
                .ok_or_else(|| ShenkoError::not_found("admission subject step"))?;
            if serde_json::to_value(&current)? != serde_json::to_value(expected)? {
                return Err(ShenkoError::conflict("subject step changed after its authority plan was frozen"));
            }
        }
        Ok(())
    }

    /// The admission fact and the effect marker share this commit. A replayed
    /// provider grant is retained but cannot create fresh permission to send.
    pub(crate) fn record_admission(
        &self, tx: &Transaction<'_>, commit: CommitRef,
    ) -> Result<bool, ShenkoError> {
        let Some(admission) = &self.admission else { return Ok(false) };
        let event_id = EntityId::derived("commit-authority-admission-v1", admission.conflict_key.as_bytes());
        let existing = admission_applied(tx, admission)?;
        if !existing {
            let event = InboxEvent {
                event_id, kind: InboxEventKind::SystemEvent, source: "commit-authority".into(),
                created_at: chrono::Utc::now(),
                payload: serde_json::json!({"kind": "authority.admission_applied", "admission": admission,
                    "grant": self.grant, "committed_at": commit}),
            };
            tx.execute("INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
                VALUES(?1,?2,'system_event','commit-authority',0,?3)",
                params![event_id.to_string(), admission.subject_id.to_string(), serde_json::to_string(&event)?],
            ).map_err(storage_error)?;
        }
        // This local receipt is deliberately absent from cognition exports.
        // A replicated source event cannot create local materialization.
        if !admission_materialized(tx, admission)? {
            tx.execute("INSERT INTO control_materializations(conflict_key,action_id,data,updated_commit_seq)
                VALUES(?1,?2,?3,?4)", params![admission.conflict_key, admission.action_id.to_string(),
                serde_json::to_string(&serde_json::json!({"admission":admission, "grant":self.grant, "committed_at":commit}))?,
                commit.seq.0 as i64]).map_err(storage_error)?;
        }
        Ok(existing || self.grant.as_ref().is_some_and(|grant| grant.replayed))
    }

    /// Call only after the enclosing SQLite COMMIT succeeded. Merely staging
    /// a receipt does not survive a later SQL error or failed commit.
    pub(crate) fn materialized(&self) { self.recorded.store(true, Ordering::Release); }

}

impl Drop for AuthorityReservation {
    fn drop(&mut self) {
        if !self.recorded.load(Ordering::Acquire)
            && let (Some(provider), Some(grant)) = (&self.provider, &self.grant)
        { provider.action_unmaterialized(grant); }
    }
}

impl Store {
    /// Attach the real source before starting any decision/dispatch consumers.
    /// The durable marker survives reopen; absent adapters subsequently fail
    /// closed for control while independent evidence ingress remains usable.
    pub fn attach_commit_authority(
        &self, provider: Arc<dyn CommitAuthorityProvider>,
    ) -> Result<(), ShenkoError> {
        if provider.source_id().is_empty() {
            return Err(authority_fenced("authority source identity is empty"));
        }
        let guard = provider.acquire(None)?;
        let mut installed = self.ledger.authority.provider.write();
        if installed.as_ref().is_some_and(|old| old.source_id() != provider.source_id()) {
            return Err(authority_fenced("a different authority source is already attached"));
        }
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            let source: Option<String> = tx.query_row(
                "SELECT source_id FROM commit_authority_policy WHERE singleton=1", [], |row| row.get(0),
            ).optional().map_err(storage_error)?;
            if source.as_ref().is_some_and(|source| source != provider.source_id()) {
                return Err(authority_fenced("this ledger is bound to another authority source"));
            }
            if source.is_none() {
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute("INSERT INTO commit_authority_policy VALUES(1,?1)", [provider.source_id()])
                    .map_err(storage_error)?;
                self.ledger.record_commit(&tx, seq, 1)?;
            }
            tx.commit().map_err(storage_error)?;
            Ok(())
        }, |error| matches!(error, ShenkoError::Storage(_)))?;
        *installed = Some(provider);
        drop(guard);
        Ok(())
    }

    pub(crate) fn reserve_authority(&self) -> Result<AuthorityReservation, ShenkoError> {
        self.ledger.authority.reserve()
    }

    /// Seal the proposal before network consensus. The caller must revalidate
    /// all expected rows in its final Store transaction under the reservation.
    pub(crate) fn reserve_admission<T: Serialize>(
        &self, head: &SubjectBranchHead, mut position: AdmissionPosition, payload: &T,
    ) -> Result<AuthorityReservation, ShenkoError> {
        if self.ledger.authority.provider.read().is_none() { return self.reserve_authority(); }
        {
            let current = self.reserve_authority()?;
            current.check(&self.ledger.conn.lock(), self.capabilities.incarnation, Some(head.agent_id))?;
        }
        position.aliases.sort(); position.aliases.dedup();
        position.reads.sort(); position.reads.dedup();
        let snapshot_tasks = position.expected_tasks.iter().chain(&position.snapshot_tasks).cloned().collect::<Vec<_>>();
        let predecessor_ref = self.prepare_control_predecessor_for_admission(
            head, &snapshot_tasks, &position.expected_steps)?;
        let payload_ref = self.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
            "schema_version": 1, "kind": "store.control_admission", "head": head,
            "source_ledger_id": self.ledger.ledger_id(), "predecessor_ref": predecessor_ref,
            "expected_tasks": position.expected_tasks, "snapshot_tasks": position.snapshot_tasks,
            "expected_steps": position.expected_steps,
            "action": payload,
        }))?, "application/json", "commit-authority", "commit-admission", 1)?.object;
        let mut reservation = self.ledger.authority.prepare(CommitAdmission {
            schema_version: 1, subject_id: head.agent_id, branch_id: head.branch_id.clone(),
            action_id: EntityId::derived("commit-admission-action-v1", position.key.as_bytes()),
            conflict_key: position.key, conflict_aliases: position.aliases,
            required_unconsumed: position.reads,
            expected_parent: position.bind_parent.then_some(head.committed_at),
            expected_head_revision: position.bind_parent.then_some(head.record_revision),
            controller_epoch: head.controller_epoch, software_generation: head.software_generation.clone(),
            operation_id: position.operation_id, attempt_id: position.attempt_id, payload_ref,
        })?;
        reservation.expected_tasks = position.expected_tasks;
        reservation.expected_steps = position.expected_steps;
        Ok(reservation)
    }

    /// One task write consumes its prior record revision. Ancestors are reads:
    /// siblings may use them concurrently, but a chosen ancestor cancellation
    /// fences every later descendant send even before its Store projection.
    pub(crate) fn task_admission_position(
        &self, task_id: EntityId, key: String,
    ) -> Result<(SubjectBranchHead, AdmissionPosition), ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(storage_error)?;
        let task = crate::child::read_task(&tx, task_id).map_err(storage_error)?
            .ok_or_else(|| ShenkoError::not_found("admission task"))?;
        let (head, _) = crate::store::read_subject(&tx, task.agent_id)?;
        crate::child::ensure_owner_active(&tx, &task, &head).map_err(|error|
            ShenkoError::conflict(format!("task branch/owner is not current: {error}")))?;
        let mut position = AdmissionPosition { key,
            aliases: vec![task_revision_position(&task)], expected_tasks: vec![task.clone()],
            ..Default::default() };
        let mut owner = task.owner;
        let mut seen = std::collections::BTreeSet::new();
        while let TaskOwner::Task { id } = owner {
            if !seen.insert(id) || seen.len() > 64 { return Err(ShenkoError::conflict("task owner cycle")); }
            let parent = crate::child::read_task(&tx, id).map_err(storage_error)?
                .ok_or_else(|| ShenkoError::not_found("task authority ancestor"))?;
            position.reads.push(task_revision_position(&parent));
            owner = parent.owner.clone();
            position.expected_tasks.push(parent);
        }
        Ok((head, position))
    }

    pub(crate) fn reserve_task_admission<T: Serialize>(
        &self, task_id: EntityId, conflict_key: String, operation_id: EntityId,
        attempt_id: EntityId, payload: &T,
    ) -> Result<AuthorityReservation, ShenkoError> {
        if self.ledger.authority.provider.read().is_none() { return self.reserve_authority(); }
        let (head, mut position) = self.task_admission_position(task_id, conflict_key)?;
        position.operation_id = Some(operation_id);
        position.attempt_id = Some(attempt_id);
        self.reserve_admission(&head, position, payload)
    }

    pub(crate) fn reserve_task_control<T: Serialize>(
        &self, task_id: EntityId, phase: &str, payload: &T,
    ) -> Result<AuthorityReservation, ShenkoError> {
        if self.ledger.authority.provider.read().is_none() { return self.reserve_authority(); }
        let (head, mut position) = self.task_admission_position(task_id, String::new())?;
        let task = position.expected_tasks.first().ok_or_else(|| ShenkoError::not_found("admission task"))?;
        position.key = format!("task/{}/{}/{}", task_id, phase, task.record_revision.0);
        self.reserve_admission(&head, position, payload)
    }

    pub(crate) fn reserve_recovered_admission(
        &self, mut grant: CommitAuthorityGrant,
        expected_tasks: Vec<TaskRecord>, expected_steps: Vec<SubjectStep>,
    ) -> Result<AuthorityReservation, ShenkoError> {
        grant.replayed = true;
        let provider = self.ledger.authority.provider.read().clone()
            .ok_or_else(|| authority_fenced("original admission recovery requires its actual authority source"))?;
        let guard = provider.acquire(Some(&grant)).map_err(|error| {
            provider.action_unmaterialized(&grant);
            error
        })?;
        Ok(AuthorityReservation { guard: Some(guard), provider: Some(provider), recorded: AtomicBool::new(false), admission: Some(grant.admission.clone()),
            grant: Some(grant), expected_tasks, expected_steps, recovery: true })
    }

    /// Reuse the same typed closure verifier as cognition replication. Every
    /// required byte is carried by the quorum's accepted value, including the
    /// original plan needed after a committed-but-unapplied source crash.
    pub fn admission_objects(
        &self, root: &shenko_types::object::ObjectRef,
    ) -> anyhow::Result<Vec<shenko_types::authority::CommitAdmissionObject>> {
        let (closure, missing) = self.collect_closure(std::slice::from_ref(root))?;
        anyhow::ensure!(missing.is_empty(), "admission CAS closure is incomplete");
        closure.into_iter().map(|object| {
            let bytes = self.cas.read_object(&object.digest)?;
            anyhow::ensure!(bytes.len() as u64 == object.bytes
                && shenko_types::object::Digest::blake3(&bytes) == object.digest,
                "admission CAS object changed during export");
            Ok(shenko_types::authority::CommitAdmissionObject {
                reference: shenko_types::object::ObjectRef { digest: object.digest, bytes: object.bytes,
                    media_type: object.media_type, store_namespace: object.store_namespace,
                    locator: String::new(), codec: object.codec }, bytes,
            })
        }).collect()
    }

    pub fn subject_authority_binding(
        &self, agent: EntityId,
    ) -> Result<Option<SubjectAuthorityBinding>, ShenkoError> {
        read_binding(&self.ledger.conn.lock(), agent)
    }

    /// One explicit takeover: head/registered step epoch, activation selector,
    /// dispatcher incarnation and immutable authority fact change together.
    /// Original requests, attempts, E1 and task ownership are not erased.
    pub fn activate_runtime_authority(
        &self, expected: &SubjectBranchHead,
    ) -> Result<SubjectBranchHead, ShenkoError> {
        let reservation = self.reserve_authority()?;
        let stamp = reservation.guard.as_ref().and_then(|guard| guard.stamp()).cloned()
            .ok_or_else(|| authority_fenced("current authority cannot be activated"))?;
        if stamp.subject_id != expected.agent_id || stamp.branch_id != expected.branch_id {
            return Err(authority_fenced("authority does not own this actual subject branch"));
        }
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            let mut head = checked_head_mode(&tx, expected, false)?;
            if !matches!(head.mode, SubjectMode::Active | SubjectMode::Paused) {
                return Err(authority_fenced("handoff or stopped branch requires its own recovery"));
            }
            let source: String = tx.query_row(
                "SELECT source_id FROM commit_authority_policy WHERE singleton=1", [], |r| r.get(0),
            ).map_err(storage_error)?;
            if source != stamp.source_id { return Err(authority_fenced("authority source changed")); }
            let prior = read_binding(&tx, head.agent_id)?;
            if let Some(prior) = &prior {
                if self.ledger.authority.activated.load(Ordering::Acquire)
                    && prior.writer_incarnation != self.capabilities.incarnation
                {
                    return Err(authority_fenced("superseded process cannot reclaim the writer slot"));
                }
                if prior.stamp == stamp && prior.writer_incarnation == self.capabilities.incarnation
                    && prior.controller_epoch == head.controller_epoch
                    && prior.software_generation == head.software_generation
                { return Ok(head); }
            }
            let mut step = read_step(&tx, &head.branch_id, head.next_step_id)?
                .ok_or_else(|| authority_fenced("takeover requires the registered original step"))?;
            validate_step_binding(&head, &step)?;
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            head.controller_epoch = ControllerEpoch(head.controller_epoch.0.checked_add(1)
                .ok_or_else(|| authority_fenced("controller epoch exhausted"))?);
            head.record_revision = head.record_revision.next();
            step.controller_epoch = head.controller_epoch;
            step.record_revision = step.record_revision.next();
            let previous_boundary = if head.mode == SubjectMode::Paused {
                let raw: String = tx.query_row(
                    "SELECT data FROM subject_quiesce_requests WHERE agent_id=?1 AND state='paused'",
                    [head.agent_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
                let previous: crate::runtime_boundary::SubjectQuiesce = serde_json::from_str(&raw)?;
                let mut rebound = previous.clone();
                rebound.epoch = head.controller_epoch;
                rebound.boundary_at = Some(commit);
                tx.execute("UPDATE subject_quiesce_requests SET data=?2,updated_commit_seq=?3 WHERE request_id=?1 AND state='paused'",
                    params![rebound.request_id.to_string(), serde_json::to_string(&rebound)?, seq.0 as i64])
                    .map_err(storage_error)?;
                Some(previous)
            } else { None };
            // Keep the software selector coherent across process takeover.
            // This changes no software closure, slot, origin or route.
            if let Some(mut selector) = crate::activation::read_selector(&tx, head.agent_id)? {
                if selector.generation != head.software_generation {
                    return Err(authority_fenced("software activation must complete before takeover"));
                }
                let revision = selector.selector_revision;
                selector.selector_revision = revision.checked_add(1)
                    .ok_or_else(|| authority_fenced("selector revision exhausted"))?;
                selector.active_epoch = head.controller_epoch.0;
                crate::activation::cas_selector(&tx, revision, &selector)?;
                tx.execute("UPDATE activation_selectors SET updated_commit_seq=?2 WHERE agent_id=?1",
                    params![head.agent_id.to_string(), seq.0 as i64]).map_err(storage_error)?;
            }
            tx.execute("UPDATE subject_branch_heads SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE agent_id=?1",
                params![head.agent_id.to_string(), head.record_revision.0 as i64, serde_json::to_string(&head)?, seq.0 as i64])
                .map_err(storage_error)?;
            tx.execute("UPDATE subject_steps SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE step_id=?1",
                params![step.step_id.to_string(), step.record_revision.0 as i64, serde_json::to_string(&step)?, seq.0 as i64])
                .map_err(storage_error)?;
            let binding = SubjectAuthorityBinding { schema_version: 1, stamp: stamp.clone(),
                writer_incarnation: self.capabilities.incarnation, controller_epoch: head.controller_epoch,
                software_generation: head.software_generation.clone(), committed_at: commit };
            tx.execute("INSERT INTO subject_authority_bindings VALUES(?1,?2,?3)
                ON CONFLICT(agent_id) DO UPDATE SET data=excluded.data,updated_commit_seq=excluded.updated_commit_seq",
                params![head.agent_id.to_string(), serde_json::to_string(&binding)?, seq.0 as i64]).map_err(storage_error)?;
            tx.execute("INSERT INTO subject_dispatch_owner VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET owner=excluded.owner",
                [self.capabilities.incarnation.to_string()]).map_err(storage_error)?;
            let event = InboxEvent { event_id: EntityId::new(), kind: InboxEventKind::SystemEvent,
                source: "commit-authority".into(), created_at: chrono::Utc::now(),
                payload: serde_json::json!({"kind": "authority.bound", "previous": prior,
                    "previous_boundary": previous_boundary, "binding": binding}) };
            tx.execute("INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
                VALUES(?1,?2,'system_event','commit-authority',0,?3)",
                params![event.event_id.to_string(), head.agent_id.to_string(), serde_json::to_string(&event)?])
                .map_err(storage_error)?;
            self.ledger.record_commit(&tx, seq, 5)?;
            tx.commit().map_err(storage_error)?;
            self.ledger.authority.activated.store(true, Ordering::Release);
            Ok(head)
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }
}

fn read_binding(conn: &Connection, agent: EntityId) -> Result<Option<SubjectAuthorityBinding>, ShenkoError> {
    let raw: Option<String> = conn.query_row("SELECT data FROM subject_authority_bindings WHERE agent_id=?1",
        [agent.to_string()], |r| r.get(0)).optional().map_err(storage_error)?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(ShenkoError::from)).transpose()
}

/// An exact applied fact is historical and remains readable without a live
/// source reservation. Both the indexed columns and the original JSON agree.
pub(crate) fn admission_applied(conn: &Connection, admission: &CommitAdmission) -> Result<bool, ShenkoError> {
    let event_id = EntityId::derived("commit-authority-admission-v1", admission.conflict_key.as_bytes());
    let row: Option<(String, String, String, String)> = conn.query_row(
        "SELECT agent_id,kind,source,data FROM inbox_events WHERE event_id=?1",
        [event_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional().map_err(storage_error)?;
    let Some((agent, kind, source, raw)) = row else { return Ok(false); };
    let event: InboxEvent = serde_json::from_str(&raw)?;
    let original: CommitAdmission = serde_json::from_value(event.payload["admission"].clone())?;
    if agent != admission.subject_id.to_string() || kind != "system_event" || source != "commit-authority"
        || event.event_id != event_id || event.kind != InboxEventKind::SystemEvent
        || event.source != source || original != *admission
        || event.payload["kind"] != "authority.admission_applied"
    { return Err(ShenkoError::conflict("applied admission identity or immutable descriptor differs")); }
    Ok(true)
}

pub(crate) fn admission_materialized(conn: &Connection, admission: &CommitAdmission) -> Result<bool, ShenkoError> {
    let row: Option<(String, String, i64)> = conn.query_row(
        "SELECT action_id,data,updated_commit_seq FROM control_materializations WHERE conflict_key=?1",
        [&admission.conflict_key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(storage_error)?;
    let Some((action_id, raw, seq)) = row else { return Ok(false); };
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    let original: CommitAdmission = serde_json::from_value(value["admission"].clone())?;
    let commit: CommitRef = serde_json::from_value(value["committed_at"].clone())?;
    let local: String = conn.query_row("SELECT ledger_id FROM ledger_identity WHERE singleton=1", [], |row| row.get(0))
        .map_err(storage_error)?;
    if action_id != admission.action_id.to_string() || original != *admission
        || commit.ledger_id.to_string() != local || i64::try_from(commit.seq.0).ok() != Some(seq)
    { return Err(ShenkoError::conflict("local control materialization identity differs")); }
    Ok(true)
}
