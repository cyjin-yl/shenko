//! Real subject branches and immutable ancestry — docs/specs/nodes.md#spec-5-4.
//!
//! The one local controller projection changes branches atomically. Originals
//! on the parent (including unresolved requests and effects) keep their IDs and
//! ownership. A child reads a frozen parent prefix; it never rewrites those
//! originals as if they happened on the child. Neither ancestry nor imported
//! head snapshots confer controller or dispatcher authority.
use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    commit::CommitRef,
    error::ShenkoError,
    ids::{BranchId, EntityId, RecordRevision},
    object::Digest,
    subject::{SubjectBranchHead, SubjectMode, SubjectStep, SubjectStepState},
    task::TaskRecord,
};

use crate::{Store, StoredSessionEvent, store::{checked_head, storage_error}};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS subject_head_history (
    snapshot_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    data TEXT NOT NULL,
    received_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS subject_head_history_branch
    ON subject_head_history(agent_id, branch_id);
CREATE TABLE IF NOT EXISTS subject_branch_forks (
    branch_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    parent_branch_id TEXT NOT NULL,
    data TEXT NOT NULL,
    created_commit_seq INTEGER NOT NULL
);
"#;

/// A durable fork is a new continuation of the same genesis, not a new self.
/// The exact parent head and history digest make a partial recovery detectable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectBranchFork {
    pub schema_version: u32,
    pub agent_id: EntityId,
    pub branch_id: BranchId,
    pub parent_head: SubjectBranchHead,
    pub parent_snapshot_id: Digest,
    pub parent_history_leaf: Option<i64>,
    pub parent_history_count: u64,
    pub parent_history_digest: Digest,
    pub parent_note_revision: Option<i64>,
    pub root_task_id: EntityId,
    pub first_step_id: EntityId,
    pub created_commit: CommitRef,
    pub reason: String,
}

pub(crate) fn read_fork(conn: &Connection, branch: EntityId)
    -> Result<Option<SubjectBranchFork>, ShenkoError>
{
    let data: Option<String> = conn.query_row(
        "SELECT data FROM subject_branch_forks WHERE branch_id=?1",
        [branch.to_string()], |row| row.get(0),
    ).optional().map_err(storage_error)?;
    data.map(|data| serde_json::from_str(&data).map_err(Into::into)).transpose()
}

pub(crate) fn history_digest(events: &[StoredSessionEvent]) -> Result<Digest, ShenkoError> {
    Ok(Digest::blake3(&serde_json::to_vec(events)?))
}

/// Segments are disjoint: a child's own sequence starts after its inherited
/// prefix. Bounds are frozen in the fork receipt, so later parent events never
/// enter a child merely because they arrived during a reconciliation.
pub(crate) fn history_segments(conn: &Connection, branch: EntityId, from: i64, to: i64)
    -> Result<Vec<(EntityId, i64, i64)>, ShenkoError>
{
    let mut segments = Vec::new();
    let mut visited = HashSet::new();
    let mut current = branch;
    let mut upper = to;
    while upper >= from {
        if !visited.insert(current) {
            return Err(ShenkoError::conflict("cyclic subject branch ancestry"));
        }
        let fork = read_fork(conn, current)?;
        let lower = fork.as_ref().and_then(|fork| fork.parent_history_leaf)
            .map(|leaf| leaf.checked_add(1).ok_or_else(||
                ShenkoError::conflict("subject history sequence exhausted")))
            .transpose()?.unwrap_or(0).max(from);
        if lower <= upper { segments.push((current, lower, upper)); }
        match fork {
            Some(fork) => match fork.parent_history_leaf {
                Some(leaf) => {
                    upper = upper.min(leaf);
                    current = EntityId(fork.parent_head.branch_id.0);
                }
                None => break,
            },
            None => break,
        }
    }
    segments.reverse();
    Ok(segments)
}

pub(crate) fn history_range_conn(conn: &Connection, branch: EntityId, from: i64, to: i64)
    -> Result<Vec<StoredSessionEvent>, ShenkoError>
{
    let mut events = Vec::new();
    for (scope, lower, upper) in history_segments(conn, branch, from, to)? {
        let mut query = conn.prepare_cached(&format!(
            "SELECT {} FROM session_events WHERE session_id=?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq",
            crate::session_events::COLUMNS,
        )).map_err(storage_error)?;
        let rows = query.query_map(params![scope.to_string(), lower, upper],
            crate::session_events::row).map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(storage_error)?;
        events.extend(rows.into_iter().map(crate::session_events::parse_row)
            .collect::<Result<Vec<_>, _>>()?);
    }
    Ok(events)
}

pub(crate) fn history_leaf_conn(conn: &Connection, branch: EntityId)
    -> Result<Option<i64>, ShenkoError>
{
    let own: Option<i64> = conn.query_row(
        "SELECT MAX(seq) FROM session_events WHERE session_id=?1",
        [branch.to_string()], |row| row.get(0),
    ).map_err(storage_error)?;
    let inherited = read_fork(conn, branch)?.and_then(|fork| fork.parent_history_leaf);
    Ok(own.into_iter().chain(inherited).max())
}

impl Store {
    pub fn subject_branch_fork(&self, branch: BranchId)
        -> Result<Option<SubjectBranchFork>, ShenkoError>
    {
        read_fork(&self.ledger.conn.lock(), EntityId(branch.0))
    }

    /// Fork the current local projection. The caller holds its authority
    /// provider's transition reservation; the durable authority binding remains
    /// attached to the old branch until an explicit takeover binds the child.
    /// A crash in between leaves the child fenced. Repeating this exact fork
    /// returns its original receipt and never creates another task or step.
    pub fn fork_subject_branch(&self, expected: &SubjectBranchHead, branch: BranchId, reason: &str)
        -> Result<SubjectBranchFork, ShenkoError>
    {
        if branch == expected.branch_id || branch.0 == EntityId::nil().0 || reason.trim().is_empty() {
            return Err(ShenkoError::conflict("fork needs a distinct branch and an explicit reason"));
        }
        self.ledger.with_mutation(|| {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_error)?;
            if let Some(fork) = read_fork(&tx, EntityId(branch.0))? {
                if fork.agent_id != expected.agent_id
                    || serde_json::to_value(&fork.parent_head)? != serde_json::to_value(expected)?
                    || fork.reason != reason
                {
                    return Err(ShenkoError::conflict("fork identity already binds different facts"));
                }
                return Ok(fork);
            }
            let parent = checked_head(&tx, expected)?;
            let parent_data = serde_json::to_string(&parent)?;
            let parent_snapshot_id = Digest::blake3(parent_data.as_bytes());
            let parent_history = history_range_conn(&tx, EntityId(parent.branch_id.0), 0, i64::MAX)?;
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            let fork = SubjectBranchFork {
                schema_version: 1, agent_id: parent.agent_id, branch_id: branch.clone(),
                parent_head: parent.clone(), parent_snapshot_id: parent_snapshot_id.clone(),
                parent_history_leaf: parent_history.last().map(|event| event.seq),
                parent_history_count: parent_history.len() as u64,
                parent_history_digest: history_digest(&parent_history)?,
                parent_note_revision: tx.query_row("SELECT MAX(revision) FROM context_notes WHERE scope_id=?1",
                    [parent.branch_id.0.to_string()], |row| row.get(0)).map_err(storage_error)?,
                root_task_id: EntityId::new(), first_step_id: EntityId::new(),
                created_commit: commit, reason: reason.to_owned(),
            };
            let mut head = parent.clone();
            head.branch_origin = Some(self.cas.seal_object(
                &serde_json::to_vec(&fork)?, "application/json", "subject_branch", "subject_branch_fork", 1,
            )?.object);
            head.branch_id = branch.clone();
            head.subject_root_task_id = fork.root_task_id;
            head.committed_step_no = 0;
            head.committed_step_id = None;
            head.committed_at = commit;
            head.next_step_id = fork.first_step_id;
            head.mode = SubjectMode::Active;
            head.mode_reason_ref = None;
            head.record_revision = RecordRevision(parent.record_revision.0.checked_add(1)
                .ok_or_else(|| ShenkoError::conflict("head revision exhausted"))?);
            let step = SubjectStep {
                step_id: head.next_step_id, branch_id: branch.clone(), step_no: 1,
                expected_parent_commit: commit, controller_epoch: head.controller_epoch,
                software_generation: head.software_generation.clone(), state: SubjectStepState::Planned,
                input_plan_ref: None, context_revision: None, active_request_id: None,
                active_attempt_id: None, model_result_ref: None, model_outcome_ref: None,
                decision_ref: None, commit_ref: None, retry_reason_ref: None,
                record_revision: RecordRevision(0), created_by_step_id: None,
            };
            let root_data: String = tx.query_row("SELECT data FROM tasks WHERE task_id=?1",
                [parent.subject_root_task_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
            let parent_root: TaskRecord = serde_json::from_str(&root_data)?;
            let root = TaskRecord::subject_root(fork.root_task_id, head.agent_id, parent_root.input_ref, commit);
            tx.execute("INSERT OR IGNORE INTO subject_head_history(snapshot_id,agent_id,branch_id,data,received_commit_seq) VALUES(?1,?2,?3,?4,?5)",
                params![parent_snapshot_id.0, parent.agent_id.to_string(), parent.branch_id.0.to_string(), parent_data, seq.0 as i64])
                .map_err(storage_error)?;
            tx.execute("INSERT INTO subject_branch_forks(branch_id,agent_id,parent_branch_id,data,created_commit_seq) VALUES(?1,?2,?3,?4,?5)",
                params![branch.0.to_string(), head.agent_id.to_string(), parent.branch_id.0.to_string(), serde_json::to_string(&fork)?, seq.0 as i64])
                .map_err(storage_error)?;
            tx.execute("INSERT INTO tasks(task_id,record_revision,data,updated_commit_seq) VALUES(?1,0,?2,?3)",
                params![root.task_id.to_string(), serde_json::to_string(&root)?, seq.0 as i64]).map_err(storage_error)?;
            crate::store::insert_step(&tx, &step, seq.0)?;
            let changed = tx.execute("UPDATE subject_branch_heads SET branch_id=?1,committed_step_no=0,committed_step_id=NULL,next_step_id=?2,mode='active',record_revision=?3,data=?4,updated_commit_seq=?5 WHERE agent_id=?6 AND branch_id=?7 AND record_revision=?8",
                params![branch.0.to_string(), head.next_step_id.to_string(), head.record_revision.0 as i64,
                    serde_json::to_string(&head)?, seq.0 as i64, head.agent_id.to_string(),
                    parent.branch_id.0.to_string(), parent.record_revision.0 as i64]).map_err(storage_error)?;
            if changed != 1 { return Err(ShenkoError::conflict("fork lost its parent head")); }
            crate::raw_session::ensure_raw_session(&tx, EntityId(branch.0), head.agent_id,
                &crate::raw_session::raw_session_data(head.agent_id, &branch))?;
            // Retain an explicit fork revision, including a clearing note;
            // never resurrect an earlier visible note after a reset.
            tx.execute("INSERT INTO context_notes(scope_id,revision,body,source,written_at_ms,visible) SELECT ?1,1,body,'fork',written_at_ms,visible FROM context_notes WHERE scope_id=?2 ORDER BY revision DESC LIMIT 1",
                params![branch.0.to_string(), parent.branch_id.0.to_string()]).map_err(storage_error)?;
            self.record_subject_commit(&tx, commit, 7)?;
            tx.commit().map_err(storage_error)?;
            Ok(fork)
        }, |error| matches!(error, ShenkoError::Storage(_)))
    }

    pub fn branch_history_range(&self, branch: EntityId, from: i64, to: i64)
        -> Result<Vec<StoredSessionEvent>, ShenkoError>
    {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(storage_error)?;
        let events = history_range_conn(&tx, branch, from, to)?;
        tx.commit().map_err(storage_error)?;
        Ok(events)
    }

    pub fn branch_history_event(&self, branch: EntityId, event_id: EntityId)
        -> Result<Option<StoredSessionEvent>, ShenkoError>
    {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(storage_error)?;
        let event = tx.query_row(&format!("SELECT {} FROM session_events WHERE event_id=?1",
            crate::session_events::COLUMNS), [event_id.to_string()], crate::session_events::row)
            .optional().map_err(storage_error)?.map(crate::session_events::parse_row).transpose()?;
        let event = match event {
            Some(event) => {
                let visible = history_segments(&tx, branch, event.seq, event.seq)?.iter()
                    .any(|(scope, _, _)| *scope == event.session_id);
                visible.then_some(event)
            }
            None => None,
        };
        tx.commit().map_err(storage_error)?;
        Ok(event)
    }

    pub fn branch_history_leaf_seq(&self, branch: EntityId) -> Result<Option<i64>, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(storage_error)?;
        let leaf = history_leaf_conn(&tx, branch)?;
        tx.commit().map_err(storage_error)?;
        Ok(leaf)
    }
}

/// Verify a recovery closure before any imported projection is published.
/// A fork's source prefix and note are required facts, not best-effort hints.
pub(crate) fn validate_branch_rows(rows: &[crate::cognition_archive::CognitionRow]) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use std::collections::HashMap;
    let mut forks = HashMap::new();
    let mut heads = Vec::new();
    let mut snapshots = HashMap::new();
    let mut steps = HashMap::new();
    let mut current_heads = HashSet::new();
    let mut events: HashMap<EntityId, Vec<StoredSessionEvent>> = HashMap::new();
    for row in rows {
        match row.table.as_str() {
            "subject_branch_forks" => {
                let fork: SubjectBranchFork = serde_json::from_str(row.value("data").context("fork data missing")?)?;
                ensure!(fork.schema_version == 1
                    && row.value("branch_id") == Some(fork.branch_id.0.to_string().as_str())
                    && row.value("agent_id") == Some(fork.agent_id.to_string().as_str())
                    && row.value("parent_branch_id") == Some(fork.parent_head.branch_id.0.to_string().as_str())
                    && fork.agent_id == fork.parent_head.agent_id
                    && fork.branch_id != fork.parent_head.branch_id
                    && !fork.reason.trim().is_empty()
                    && fork.parent_history_leaf.is_none_or(|leaf| leaf >= 0),
                    "invalid subject branch ancestry binding");
                ensure!(forks.insert(EntityId(fork.branch_id.0), fork).is_none(), "duplicate subject fork");
            }
            "subject_head_history" | "subject_branch_heads" => {
                let data = row.value("data").context("subject head data missing")?;
                let head: SubjectBranchHead = serde_json::from_str(data)?;
                ensure!(row.value("agent_id") == Some(head.agent_id.to_string().as_str())
                    && row.value("branch_id") == Some(head.branch_id.0.to_string().as_str()),
                    "historical subject head identity mismatch");
                if row.table == "subject_head_history" {
                    let digest = Digest::blake3(data.as_bytes());
                    ensure!(row.value("snapshot_id") == Some(digest.0.as_str()), "historical head digest mismatch");
                    snapshots.insert(digest.0, data);
                } else {
                    current_heads.insert((head.agent_id, head.next_step_id, head.record_revision.0));
                }
                heads.push(head);
            }
            "subject_steps" => {
                let step: SubjectStep = serde_json::from_str(row.value("data").context("subject step data missing")?)?;
                ensure!(row.value("step_id") == Some(step.step_id.to_string().as_str())
                    && row.value("branch_id") == Some(step.branch_id.0.to_string().as_str()), "subject step identity mismatch");
                ensure!(steps.insert(step.step_id, step).is_none(), "duplicate subject step identity");
            }
            "session_events" => {
                let event = StoredSessionEvent {
                    event_id: row.value("event_id").context("raw event id missing")?.parse()?,
                    session_id: row.value("session_id").context("raw event session missing")?.parse()?,
                    seq: row.value("seq").context("raw event seq missing")?.parse()?,
                    event_type: row.value("event_type").context("raw event type missing")?.to_owned(),
                    role: row.value("role").context("raw event role missing")?.to_owned(),
                    data: row.value("data").context("raw event data missing")?.to_owned(),
                };
                events.entry(event.session_id).or_default().push(event);
            }
            _ => {}
        }
    }
    for raw in events.values_mut() { raw.sort_by_key(|event| event.seq); }
    for head in &heads {
        let successor = steps.get(&head.next_step_id).context("subject head's registered successor is missing")?;
        ensure!(successor.branch_id == head.branch_id
            && successor.step_no == head.committed_step_no.checked_add(1).context("subject step number exhausted")?
            && successor.expected_parent_commit == head.committed_at,
            "subject head successor has another branch, position or parent");
        if current_heads.contains(&(head.agent_id, head.next_step_id, head.record_revision.0)) {
            ensure!(successor.controller_epoch == head.controller_epoch
                && successor.software_generation == head.software_generation,
                "current subject successor has another epoch or generation");
        }
        if let Some(committed_id) = head.committed_step_id {
            let committed = steps.get(&committed_id).context("subject head's committed step is missing")?;
            ensure!(committed.branch_id == head.branch_id && committed.step_no == head.committed_step_no
                && committed.state == SubjectStepState::Committed && committed.commit_ref == Some(head.committed_at),
                "subject head committed step has another decision binding");
        } else {
            ensure!(head.committed_step_no == 0, "subject head omitted its committed step identity");
        }
        if let Some(origin) = &head.branch_origin {
            let fork = forks.get(&EntityId(head.branch_id.0)).context("subject head omitted its fork receipt")?;
            let bytes = serde_json::to_vec(fork)?;
            ensure!(origin.digest == Digest::blake3(&bytes) && origin.bytes == bytes.len() as u64
                && fork.agent_id == head.agent_id && fork.root_task_id == head.subject_root_task_id
                && fork.parent_head.genesis_ref == head.genesis_ref,
                "subject head origin differs from its fork receipt");
        } else {
            ensure!(!forks.contains_key(&EntityId(head.branch_id.0)), "fork head omitted its origin reference");
        }
    }
    for (branch, fork) in &forks {
        let parent = serde_json::to_string(&fork.parent_head)?;
        ensure!(fork.parent_snapshot_id == Digest::blake3(parent.as_bytes())
            && snapshots.get(&fork.parent_snapshot_id.0).copied() == Some(parent.as_str()),
            "fork omitted its exact parent head snapshot");
        ensure!(heads.iter().any(|head| EntityId(head.branch_id.0) == *branch), "fork child head missing");
        ensure!(rows.iter().any(|row| row.table == "tasks" && row.value("task_id") == Some(fork.root_task_id.to_string().as_str())),
            "fork root task missing");
        let first = rows.iter().find(|row| row.table == "subject_steps" && row.value("step_id") == Some(fork.first_step_id.to_string().as_str()))
            .context("fork first step missing")?;
        let first: SubjectStep = serde_json::from_str(first.value("data").context("fork first step data missing")?)?;
        ensure!(first.branch_id == fork.branch_id && first.step_no == 1 && first.expected_parent_commit == fork.created_commit,
            "fork first step has another parent");
        if let Some(leaf) = fork.parent_history_leaf {
            ensure!(events.get(branch).is_none_or(|raw| raw.iter().all(|event| event.seq > leaf)),
                "child raw event overlaps inherited history");
        }
        let mut visited = HashSet::from([*branch]);
        let mut ancestor = EntityId(fork.parent_head.branch_id.0);
        while let Some(parent) = forks.get(&ancestor) {
            ensure!(visited.insert(ancestor), "cyclic subject fork ancestry");
            ensure!(parent.agent_id == fork.agent_id, "fork crossed subject identity");
            ancestor = EntityId(parent.parent_head.branch_id.0);
        }
        ensure!(visited.insert(ancestor), "cyclic subject fork ancestry");
        let mut inherited = Vec::new();
        let mut scope = EntityId(fork.parent_head.branch_id.0);
        let mut upper = fork.parent_history_leaf;
        while let Some(bound) = upper {
            let parent = forks.get(&scope);
            let lower = parent.and_then(|parent| parent.parent_history_leaf)
                .map(|leaf| leaf.checked_add(1).context("raw sequence exhausted")).transpose()?.unwrap_or(0);
            if let Some(raw) = events.get(&scope) {
                inherited.extend(raw.iter().filter(|event| event.seq >= lower && event.seq <= bound).cloned());
            }
            match parent {
                Some(parent) => {
                    upper = parent.parent_history_leaf.map(|leaf| leaf.min(bound));
                    scope = EntityId(parent.parent_head.branch_id.0);
                }
                None => break,
            }
        }
        inherited.sort_by_key(|event| event.seq);
        ensure!(inherited.last().map(|event| event.seq) == fork.parent_history_leaf
            && inherited.len() as u64 == fork.parent_history_count
            && history_digest(&inherited)? == fork.parent_history_digest,
            "fork inherited raw prefix is incomplete or changed");
        if let Some(revision) = fork.parent_note_revision {
            let parent_scope = fork.parent_head.branch_id.0.to_string();
            let child_scope = branch.to_string();
            let revision = revision.to_string();
            let parent = rows.iter().find(|row| row.table == "context_notes"
                && row.value("scope_id") == Some(parent_scope.as_str()) && row.value("revision") == Some(revision.as_str()))
                .context("fork parent notebook revision missing")?;
            let child = rows.iter().find(|row| row.table == "context_notes"
                && row.value("scope_id") == Some(child_scope.as_str()) && row.value("revision") == Some("1"))
                .context("fork inherited notebook revision missing")?;
            ensure!(["body", "visible", "written_at_ms"].into_iter().all(|column| parent.value(column) == child.value(column))
                && child.value("source") == Some("fork"), "fork notebook provenance changed");
        }
    }
    Ok(())
}
