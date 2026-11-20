//! Real subject branches and immutable ancestry.
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

use crate::{Store, StoredSessionEvent, store::{checked_head, read_step, storage_error, validate_step_binding}};

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
    /// Uncommitted selections retained by the first child step. The original
    /// raw rows, source step, inbox identities and physical attempts stay put.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherited_inputs: Vec<SubjectBranchInput>,
    pub root_task_id: EntityId,
    pub first_step_id: EntityId,
    pub created_commit: CommitRef,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectBranchInput {
    pub source_step_id: EntityId,
    pub source_event_id: EntityId,
    pub raw_event_id: EntityId,
    pub session_id: EntityId,
    pub seq: i64,
}

/// Original call/result rows in ascending semantic-history sequence order.
/// `next_from` is an inclusive continuation within the caller's fixed upper
/// bound. None means that bound was exhausted, including empty segments.
#[derive(Debug)]
pub struct BranchHistoryCallPage {
    pub events: Vec<StoredSessionEvent>,
    pub next_from: Option<i64>,
}

fn history_call_page_conn(conn: &Connection, branch: EntityId, from: i64, through: i64,
    limit: usize) -> Result<BranchHistoryCallPage, ShenkoError>
{
    if limit == 0 {
        return Err(ShenkoError::other("call page limit must be positive"));
    }
    let sql_limit = i64::try_from(limit)
        .map_err(|_| ShenkoError::other("call page limit exceeds SQLite INTEGER"))?;
    let mut events = Vec::new();
    let mut query = conn.prepare_cached(&format!(
        "SELECT {} FROM session_events WHERE session_id=?1 AND seq BETWEEN ?2 AND ?3 \
         AND event_type IN ('tool_call', 'tool_result') ORDER BY seq LIMIT ?4",
        crate::session_events::COLUMNS,
    )).map_err(storage_error)?;
    for (scope, lower, upper) in history_segments(conn, branch, from, through)? {
        let rows = query.query_map(params![scope.to_string(), lower, upper,
            sql_limit - events.len() as i64], crate::session_events::row).map_err(storage_error)?;
        for row in rows {
            events.push(crate::session_events::parse_row(row.map_err(storage_error)?)?);
        }
        if events.len() == limit {
            let next_from = events.last()
                .and_then(|event| event.seq.checked_add(1))
                .filter(|next| *next <= through);
            return Ok(BranchHistoryCallPage { events, next_from });
        }
    }
    Ok(BranchHistoryCallPage { events, next_from: None })
}

pub(crate) fn input_links(conn: &Connection, step: EntityId)
    -> Result<Vec<SubjectBranchInput>, ShenkoError>
{
    let mut stmt = conn.prepare_cached("SELECT source_event_id,raw_event_id,session_id,seq FROM subject_raw_inputs WHERE step_id=?1 ORDER BY seq")
        .map_err(storage_error)?;
    let links = stmt.query_map([step.to_string()], |row| Ok((
        row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?,
    ))).map_err(storage_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(storage_error)?;
    links.into_iter().map(|(source, raw, session, seq)| Ok(SubjectBranchInput {
        source_step_id: step, source_event_id: source.parse().map_err(storage_error)?,
        raw_event_id: raw.parse().map_err(storage_error)?, session_id: session.parse().map_err(storage_error)?, seq,
    })).collect()
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
        self.fork_subject_branch_under(expected, branch, reason, None)
    }

    pub fn fork_subject_branch_under(&self, expected: &SubjectBranchHead, branch: BranchId,
        reason: &str, permit: Option<&shenko_types::installation::InstallationPermit>)
        -> Result<SubjectBranchFork, ShenkoError>
    {
        self.fork_subject_branch_admitted(expected, branch, reason, permit, None)
    }

    /// A source-reserved availability transition, never a runtime grant. Only
    /// the first local continuation of an exactly imported bootstrap may
    /// replace its foreign route. Established local writers use the ordinary
    /// checked API, including its handoff and selected-route checks.
    pub fn fork_availability_subject_branch(
        &self, expected: &SubjectBranchHead, branch: BranchId, reason: &str,
        origin: &Digest, authority: &dyn shenko_types::authority::AvailabilityForkAuthority,
        executable: &std::path::Path,
    ) -> Result<SubjectBranchFork, ShenkoError> {
        authority.validate_fork(expected.agent_id, &expected.branch_id, origin, &branch)?;
        let genesis = self.get_subject_genesis(expected.agent_id)?
            .ok_or_else(|| ShenkoError::conflict("availability fork genesis missing"))?;
        if Digest::blake3(&serde_json::to_vec(&genesis)?) != *origin
            || genesis.genesis_id != expected.genesis_ref.genesis_id
            || genesis.created_commit.ledger_id != expected.genesis_ref.ledger_id
        {
            return Err(ShenkoError::conflict("availability fork differs from reserved subject origin"));
        }
        let permit = authority.installation_permit();
        let local = crate::store::read_local_subject_id(&self.ledger.conn.lock(), self.ledger.ledger_id())?;
        // A newly selected software copy can have a valid local route before
        // its first local subject commit. Keep it on ordinary fork admission.
        let needs_imported_route = local.is_none()
            && permit.is_some_and(|permit| self.ledger.check_selected_installation(permit).is_err());
        self.fork_subject_branch_admitted(expected, branch, reason, permit,
            needs_imported_route.then_some((origin, executable)))
    }

    fn fork_subject_branch_admitted(&self, expected: &SubjectBranchHead, branch: BranchId,
        reason: &str, permit: Option<&shenko_types::installation::InstallationPermit>,
        imported: Option<(&Digest, &std::path::Path)>)
        -> Result<SubjectBranchFork, ShenkoError>
    {
        if branch == expected.branch_id || branch.0 == EntityId::nil().0 || reason.trim().is_empty() {
            return Err(ShenkoError::conflict("fork needs a distinct branch and an explicit reason"));
        }
        let write = || {
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
            let local_selector = imported.map(|(origin, executable)|
                self.imported_fork_selector(&tx, &parent, &branch, origin, executable)).transpose()?;
            let pending = read_step(&tx, &parent.branch_id, parent.next_step_id)?
                .ok_or_else(|| ShenkoError::conflict("fork parent successor missing"))?;
            validate_step_binding(&parent, &pending)?;
            if pending.state == SubjectStepState::Committed {
                return Err(ShenkoError::conflict("fork parent successor already committed"));
            }
            let parent_data = serde_json::to_string(&parent)?;
            let parent_snapshot_id = Digest::blake3(parent_data.as_bytes());
            let parent_history = history_range_conn(&tx, EntityId(parent.branch_id.0), 0, i64::MAX)?;
            let seq = self.ledger.allocate_seq(&tx)?;
            let commit = CommitRef::new(self.ledger.ledger_id(), seq);
            let fork = SubjectBranchFork {
                schema_version: 2, agent_id: parent.agent_id, branch_id: branch.clone(),
                parent_head: parent.clone(), parent_snapshot_id: parent_snapshot_id.clone(),
                parent_history_leaf: parent_history.last().map(|event| event.seq),
                parent_history_count: parent_history.len() as u64,
                parent_history_digest: history_digest(&parent_history)?,
                parent_note_revision: tx.query_row("SELECT MAX(revision) FROM context_notes WHERE scope_id=?1",
                    [parent.branch_id.0.to_string()], |row| row.get(0)).map_err(storage_error)?,
                inherited_inputs: input_links(&tx, parent.next_step_id)?,
                root_task_id: EntityId::new(), first_step_id: EntityId::new(),
                created_commit: commit, reason: reason.to_owned(),
            };
            let mut head = parent.clone();
            if let Some(selector) = &local_selector {
                head.controller_epoch = shenko_types::ids::ControllerEpoch(selector.active_epoch);
                crate::activation::cas_selector(&tx, selector.selector_revision - 1, selector)?;
                tx.execute("UPDATE activation_selectors SET updated_commit_seq=?2 WHERE agent_id=?1",
                    params![head.agent_id.to_string(), seq.0 as i64]).map_err(storage_error)?;
            }
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
            // Staging consumed the volatile delivery before S1. Preserve its
            // selection in the child without redelivering or cloning the raw
            // fact. The immutable fork receipt is the provenance for each link.
            let inherited = tx.execute("INSERT INTO subject_raw_inputs(step_id,source_event_id,raw_event_id,session_id,seq,commit_seq) SELECT ?1,source_event_id,raw_event_id,session_id,seq,?2 FROM subject_raw_inputs WHERE step_id=?3",
                params![step.step_id.to_string(), seq.0 as i64, parent.next_step_id.to_string()]).map_err(storage_error)?;
            if inherited != fork.inherited_inputs.len() {
                return Err(ShenkoError::conflict("fork input selection changed"));
            }
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
            self.record_subject_commit(&tx, commit, 7 + inherited as i64 + i64::from(local_selector.is_some()))?;
            tx.commit().map_err(storage_error)?;
            Ok(fork)
        };
        let storage_failure = |error: &ShenkoError| matches!(error, ShenkoError::Storage(_));
        if imported.is_some() {
            let permit = permit.ok_or_else(|| shenko_types::authority::authority_fenced(
                "imported fork requires the source reservation's installation permit"))?;
            self.ledger.with_imported_fork_mutation_under(permit, write, storage_failure)
        } else {
            self.ledger.with_mutation_under(permit, write, storage_failure)
        }
    }

    /// Called only inside the fork's IMMEDIATE transaction and physical
    /// permit. Every foreign fact stays retained; the mutable local selector
    /// advances with the child epoch, before any launch or runtime takeover.
    fn imported_fork_selector(&self, conn: &Connection, parent: &SubjectBranchHead,
        child: &BranchId, origin: &Digest, executable: &std::path::Path)
        -> Result<shenko_types::activation::ActivationSelector, ShenkoError>
    {
        use shenko_types::activation::{ActivationOrigin, RouteAuthority};
        let reject = || shenko_types::authority::authority_fenced(
            "availability fork is not an unowned, exactly imported bootstrap");
        if crate::store::read_local_subject_id(conn, self.ledger.ledger_id())?.is_some()
            || parent.branch_origin.is_some()
        { return Err(reject()); }
        let raw: String = conn.query_row("SELECT data FROM subject_genesis WHERE subject_id=?1",
            [parent.agent_id.to_string()], |row| row.get(0)).map_err(storage_error)?;
        let genesis: shenko_types::subject::SubjectGenesis = serde_json::from_str(&raw)?;
        let imported: Option<(String, String)> = conn.query_row(
            "SELECT source_ledger_id,genesis_id FROM cognition_origins WHERE agent_id=?1",
            [parent.agent_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional().map_err(storage_error)?;
        if genesis.subject_id != parent.agent_id || Digest::blake3(&serde_json::to_vec(&genesis)?) != *origin
            || genesis.genesis_id != parent.genesis_ref.genesis_id
            || genesis.created_commit.ledger_id != parent.genesis_ref.ledger_id
            || genesis.created_commit.ledger_id == self.ledger.ledger_id()
            || imported != Some((genesis.created_commit.ledger_id.to_string(), genesis.genesis_id.to_string()))
        { return Err(reject()); }
        let raw: Option<String> = conn.query_row(
            "SELECT data FROM local_installation_provisioning WHERE singleton=1", [], |row| row.get(0))
            .optional().map_err(storage_error)?;
        let provision: crate::local_installation::LocalInstallationProvisioning =
            serde_json::from_str(&raw.ok_or_else(reject)?)?;
        // This shared reader verifies the bootstrap event/index agreement,
        // imported origin, local home CAS evidence and physical receipt.
        if crate::local_installation::installation_id_for_subject_conn(conn, &self.cas, parent.agent_id)?
            != provision.installation_id
            || provision.installation_id != self.ledger.ledger_id()
        { return Err(reject()); }
        let owned: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM subject_authority_bindings)
                 OR EXISTS(SELECT 1 FROM commit_authority_policy)
                 OR EXISTS(SELECT 1 FROM activation_records)
                 OR (SELECT COUNT(*) FROM activation_selectors)!=1", [], |row| row.get(0))
            .map_err(storage_error)?;
        if owned { return Err(reject()); }
        let (domain, db, cas) = self.ledger.installation.as_ref().ok_or_else(reject)?;
        let root = domain.root();
        let db = db.canonicalize().map_err(storage_error)?;
        let slot = db.parent().ok_or_else(reject)?;
        // A first imported bootstrap cannot select or manufacture a software
        // generation in an existing version slot. Such a slot needs its real
        // activation/recovery contract, not this initial continuation proof.
        if slot != root || cas.canonicalize().map_err(storage_error)? != root.join("cas") {
            return Err(reject());
        }
        match std::fs::symlink_metadata(root.join("slot.json")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(storage_error(error)),
            Ok(_) => return Err(reject()),
        }
        let mut selector = crate::activation::read_selector(conn, parent.agent_id)?.ok_or_else(reject)?;
        let indexed: (u64, u64) = conn.query_row(
            "SELECT selector_revision,active_epoch FROM activation_selectors WHERE agent_id=?1",
            [parent.agent_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?))).map_err(storage_error)?;
        let bytes = self.cas.read_object(&selector.route_authority_ref.digest)?;
        let route: RouteAuthority = serde_json::from_slice(&bytes)?;
        if selector.agent_id != parent.agent_id || selector.generation != parent.software_generation
            || selector.generation != shenko_types::ids::RuntimeGeneration::default()
            || indexed != (selector.selector_revision, selector.active_epoch)
            || selector.active_epoch != parent.controller_epoch.0
            || !matches!(&selector.origin, ActivationOrigin::Genesis { genesis_ref } if *genesis_ref == parent.genesis_ref)
            || bytes.len() as u64 != selector.route_authority_ref.bytes
            || Digest::blake3(&bytes) != selector.route_authority_ref.digest
            || route.schema_version != RouteAuthority::SCHEMA_VERSION
            || route.agent_id != parent.agent_id || route.generation != selector.generation
            || route.active_epoch == 0 || route.active_epoch > selector.active_epoch
            || route.selector_revision > selector.selector_revision || route.activation_id.is_some()
            || route.primary_claim.is_some()
            || route.data_dir == root.to_string_lossy() || route.slot_dir == slot.to_string_lossy()
        { return Err(reject()); }
        let parent_selector = serde_json::to_value(&selector)?;
        selector.selector_revision = selector.selector_revision.checked_add(1).ok_or_else(reject)?;
        selector.active_epoch = selector.active_epoch.checked_add(1).ok_or_else(reject)?;
        let executable = executable.canonicalize().map_err(storage_error)?;
        let program = executable.to_string_lossy().into_owned();
        let data_dir = root.to_string_lossy().into_owned();
        let local_route = RouteAuthority {
            schema_version: RouteAuthority::SCHEMA_VERSION, agent_id: parent.agent_id,
            generation: selector.generation.clone(), active_epoch: selector.active_epoch,
            selector_revision: selector.selector_revision, data_dir: data_dir.clone(),
            slot_dir: slot.to_string_lossy().into_owned(), artifact_path: program.clone(),
            artifact_digest: Digest::blake3(&std::fs::read(&executable).map_err(storage_error)?),
            launch_command: vec![program, "--data-dir".into(), data_dir, "start".into()],
            primary_claim: None, activation_id: None,
        };
        selector.route_authority_ref = self.cas.seal_object(&serde_json::to_vec(&local_route)?,
            "application/json", "subject_branch", "json", 1)?.object;
        selector.state_ref = self.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
            "schema_version":1, "kind":"activation_state_ref", "slot":"data-dir", "db":"shenko.db",
            "controller_epoch":selector.active_epoch, "generation":selector.generation,
            "availability_fork": {"parent_branch_id":parent.branch_id, "branch_id":child,
                "origin_proof":origin, "installation_id":provision.installation_id,
                "parent_selector":parent_selector},
        }))?, "application/json", "subject_branch", "json", 1)?.object;
        Ok(selector)
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

    /// Select only original call/result rows over the frozen ancestral prefix
    /// and this branch's suffix. The limit applies globally across segments;
    /// payloads and original evidence scopes remain uninterpreted and intact.
    /// Each page resolves ancestry and reads rows in one SQLite transaction.
    pub fn branch_history_call_page(&self, branch: EntityId, from: i64, through: i64,
        limit: usize) -> Result<BranchHistoryCallPage, ShenkoError>
    {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(storage_error)?;
        let page = history_call_page_conn(&tx, branch, from, through, limit)?;
        tx.commit().map_err(storage_error)?;
        Ok(page)
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
    let mut links = HashMap::new();
    let mut current_heads = HashSet::new();
    let mut events: HashMap<EntityId, Vec<StoredSessionEvent>> = HashMap::new();
    for row in rows {
        match row.table.as_str() {
            "subject_branch_forks" => {
                let fork: SubjectBranchFork = serde_json::from_str(row.value("data").context("fork data missing")?)?;
                ensure!(matches!(fork.schema_version, 1 | 2)
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
            "subject_raw_inputs" => {
                let input = SubjectBranchInput {
                    source_step_id: row.value("step_id").context("input step missing")?.parse()?,
                    source_event_id: row.value("source_event_id").context("input source missing")?.parse()?,
                    raw_event_id: row.value("raw_event_id").context("input raw identity missing")?.parse()?,
                    session_id: row.value("session_id").context("input session missing")?.parse()?,
                    seq: row.value("seq").context("input sequence missing")?.parse()?,
                };
                ensure!(links.insert((input.source_step_id, input.source_event_id), input).is_none(),
                    "duplicate subject input selection");
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
    let raw_by_id: HashMap<_, _> = events.values().flatten().map(|event| (event.event_id, event)).collect();
    let mut original_selections = HashSet::new();
    for link in links.values() {
        let raw = raw_by_id.get(&link.raw_event_id).context("selected input original missing")?;
        let step = steps.get(&link.source_step_id).context("input selection step missing")?;
        ensure!(raw.session_id == link.session_id && raw.seq == link.seq,
            "input selection changed its raw source coordinates");
        if step.branch_id.0 == link.session_id.0 {
            ensure!(original_selections.insert(link.raw_event_id), "raw input has multiple original selections");
        } else {
            let fork = forks.get(&EntityId(step.branch_id.0)).context("input crossed branches without a fork")?;
            ensure!(step.step_id == fork.first_step_id && fork.inherited_inputs.iter().any(|input|
                input.source_event_id == link.source_event_id && input.raw_event_id == link.raw_event_id
                    && input.session_id == link.session_id && input.seq == link.seq),
                "cross-branch input has no exact first-step fork selection");
        }
    }
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
        let prefix_ids: HashSet<_> = inherited.iter().map(|event| event.event_id).collect();
        let parent_inputs: Vec<_> = links.values().filter(|input|
            input.source_step_id == fork.parent_head.next_step_id && prefix_ids.contains(&input.raw_event_id)).collect();
        ensure!(fork.schema_version != 1 || (parent_inputs.is_empty() && fork.inherited_inputs.is_empty()),
            "legacy fork omitted uncommitted input provenance; explicit continuation repair required");
        ensure!(parent_inputs.len() == fork.inherited_inputs.len(),
            "fork omitted uncommitted input selections from its parent prefix");
        let mut sources = HashSet::new();
        let mut raws = HashSet::new();
        for input in &fork.inherited_inputs {
            ensure!(input.source_step_id == fork.parent_head.next_step_id
                && sources.insert(input.source_event_id) && raws.insert(input.raw_event_id)
                && prefix_ids.contains(&input.raw_event_id)
                && links.get(&(input.source_step_id, input.source_event_id)) == Some(input),
                "fork inherited input has no exact parent selection in its frozen prefix");
            let child = links.get(&(fork.first_step_id, input.source_event_id))
                .context("fork first step omitted an inherited input")?;
            ensure!(child.raw_event_id == input.raw_event_id && child.session_id == input.session_id
                && child.seq == input.seq, "fork input changed its original raw identity");
            let raw = raw_by_id.get(&input.raw_event_id).context("fork input raw original missing")?;
            let data: serde_json::Value = serde_json::from_str(&raw.data)?;
            ensure!(data.pointer("/inbox_event/event_id").and_then(serde_json::Value::as_str)
                == Some(input.source_event_id.to_string().as_str()), "fork input differs from its admitted original");
        }
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

#[cfg(test)]
mod call_page_tests {
    use super::*;
    use shenko_types::ids::{ControllerEpoch, LedgerSeq, RuntimeGeneration};
    use shenko_types::subject::GenesisRef;

    fn event(conn: &Connection, scope: EntityId, seq: i64, kind: &str, data: &str)
        -> StoredSessionEvent
    {
        conn.execute("INSERT OR IGNORE INTO raw_sessions(session_id,agent_id,started_at,data)
            VALUES (?1,?1,'','{}')", [scope.to_string()]).unwrap();
        let event = StoredSessionEvent { event_id: EntityId::new(), session_id: scope,
            seq, event_type: kind.into(), role: "fixture".into(), data: data.into() };
        conn.execute("INSERT INTO session_events(event_id,session_id,seq,event_type,role,data)
            VALUES (?1,?2,?3,?4,?5,?6)", params![event.event_id.to_string(), scope.to_string(),
                seq, kind, event.role, data]).unwrap();
        event
    }

    // A reader fixture, not a claim about fork admission. The stored receipt
    // gives history_segments the same typed frozen bounds as a durable fork.
    fn fork(conn: &Connection, parent: EntityId, child: EntityId, leaf: i64) {
        let commit = CommitRef::new(EntityId::new(), LedgerSeq::ZERO);
        let head = SubjectBranchHead {
            branch_origin: None, agent_id: parent, branch_id: BranchId(parent.0),
            subject_root_task_id: EntityId::new(), controller_epoch: ControllerEpoch(0),
            software_generation: RuntimeGeneration("call-page-fixture".into()),
            genesis_ref: GenesisRef { ledger_id: commit.ledger_id, genesis_id: EntityId::new() },
            committed_step_no: 0, committed_step_id: None, committed_at: commit,
            next_step_id: EntityId::new(), mode: SubjectMode::Active,
            mode_reason_ref: None, record_revision: RecordRevision(0),
        };
        let receipt = SubjectBranchFork {
            schema_version: 2, agent_id: parent, branch_id: BranchId(child.0),
            parent_head: head, parent_snapshot_id: Digest::blake3(b"reader fixture"),
            parent_history_leaf: Some(leaf), parent_history_count: 0,
            parent_history_digest: Digest::blake3(b"reader fixture"), parent_note_revision: None,
            inherited_inputs: vec![], root_task_id: EntityId::new(), first_step_id: EntityId::new(),
            created_commit: commit, reason: "bounded reader fixture".into(),
        };
        conn.execute("INSERT INTO subject_branch_forks
            (branch_id,agent_id,parent_branch_id,data,created_commit_seq) VALUES (?1,?2,?2,?3,0)",
            params![child.to_string(), parent.to_string(), serde_json::to_string(&receipt).unwrap()]).unwrap();
    }

    #[test]
    fn call_pages_preserve_original_ancestry_bounds_order_and_global_limit() {
        let conn = Connection::open_in_memory().unwrap();
        crate::migrations::run_migrations(&conn).unwrap();
        let (root, child, middle, grandchild, sibling) =
            (EntityId::new(), EntityId::new(), EntityId::new(), EntityId::new(), EntityId::new());
        let first = event(&conn, root, 0, "tool_call", "{\"v\":99,\"call_id\":\"same\"}");
        let repeated = event(&conn, root, 2, "tool_result", "uninterpreted malformed payload");
        for seq in (1..=500).filter(|seq| *seq != 2) {
            event(&conn, root, seq, "user", &"ordinary body ".repeat(1024));
        }
        fork(&conn, root, child, 500);
        let child_call = event(&conn, child, 501, "tool_call", "{\"call_id\":\"same\"}");
        fork(&conn, child, middle, 501);
        event(&conn, middle, 502, "assistant", "empty call segment");
        fork(&conn, middle, grandchild, 502);
        let result = event(&conn, grandchild, 503, "tool_result", "{\"call_id\":\"same\"}");
        event(&conn, grandchild, 504, "user", "ordinary tail");
        event(&conn, root, 505, "tool_call", "later parent must not leak");
        event(&conn, child, 506, "tool_result", "later child must not leak");
        event(&conn, middle, 507, "tool_call", "later middle must not leak");
        fork(&conn, root, sibling, 500);
        event(&conn, sibling, 501, "tool_result", "sibling must not leak");

        let page = history_call_page_conn(&conn, grandchild, 0, 504, 3).unwrap();
        assert_eq!(page.events, vec![first.clone(), repeated.clone(), child_call.clone()]);
        assert_eq!(page.next_from, Some(502), "limit is global across ancestral segments");
        let rest = history_call_page_conn(&conn, grandchild, page.next_from.unwrap(), 504, 3).unwrap();
        assert_eq!(rest.events, vec![result.clone()], "an empty middle segment cannot stop enumeration");
        assert_eq!(rest.next_from, None);
        let bounded = history_call_page_conn(&conn, grandchild, 2, 502, 10).unwrap();
        assert_eq!(bounded.events, vec![repeated, child_call]);
        assert_eq!(bounded.next_from, None);
        let mut from = 0;
        let mut paged = Vec::new();
        loop {
            let page = history_call_page_conn(&conn, grandchild, from, 504, 1).unwrap();
            assert!(page.events.len() <= 1);
            paged.extend(page.events);
            match page.next_from { Some(next) => from = next, None => break }
        }
        assert_eq!(paged, vec![first, bounded.events[0].clone(), bounded.events[1].clone(), result]);
        assert!(history_call_page_conn(&conn, grandchild, 504, 504, 1).unwrap().events.is_empty());
        assert!(history_call_page_conn(&conn, grandchild, 5, 4, 1).unwrap().events.is_empty());
        assert!(history_call_page_conn(&conn, grandchild, 0, 504, 0).is_err());
    }

    #[test]
    fn call_page_partial_index_supports_selective_sequence_search() {
        let conn = Connection::open_in_memory().unwrap();
        crate::migrations::run_migrations(&conn).unwrap();
        let scope = EntityId::new();
        event(&conn, scope, 0, "tool_call", "original call");
        for seq in 1..=256 {
            event(&conn, scope, seq, "user", "ordinary body");
        }
        conn.execute_batch("ANALYZE session_events;").unwrap();
        // Only stable plan properties: the named partial index participates in
        // a SEARCH. Do not pin SQLite's wording, node IDs or cost estimates.
        let mut explain = conn.prepare(&format!(
            "EXPLAIN QUERY PLAN SELECT {} FROM session_events WHERE session_id=?1 \
             AND seq BETWEEN ?2 AND ?3 AND event_type IN ('tool_call', 'tool_result') \
             ORDER BY seq LIMIT ?4", crate::session_events::COLUMNS)).unwrap();
        let plan = explain.query_map(params![scope.to_string(), 0, 1000, 512],
            |row| row.get::<_, String>(3)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert!(plan.iter().any(|detail| detail.contains("SEARCH")
            && detail.contains("idx_session_events_calls")), "{plan:?}");
    }
}
