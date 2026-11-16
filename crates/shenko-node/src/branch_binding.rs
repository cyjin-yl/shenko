//! Recoverable projection of a locally committed subject fork into Node.
//!
//! The trusted Store adapter verifies the receipt bytes and local ledger. This
//! layer permits only same-subject ancestry into a recorded availability branch;
//! it cannot adopt another self or turn a historical head into live authority.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use shenko_types::{commit::CommitRef, ids::EntityId, object::ObjectRef};

use crate::{BranchFact, BranchKind, Identity, NodeAuthorityGuard};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubjectBranchProjection {
    pub parent: Identity,
    pub branch_id: EntityId,
    pub store_commit: CommitRef,
    pub fork_receipt: ObjectRef,
}

impl NodeAuthorityGuard {
    /// Commit the Node half AFTER the Store fork receipt is durable. A crash
    /// before this call is recovered by verifying that same receipt again.
    pub fn project_subject_branch(self, projection: &SubjectBranchProjection) -> Result<Identity> {
        let snapshot = self.snapshot();
        let target = Identity { branch_id: projection.branch_id, ..projection.parent.clone() };
        ensure!(projection.branch_id != projection.parent.branch_id
            && projection.branch_id != EntityId::nil()
            && projection.store_commit.ledger_id != EntityId::nil()
            && projection.store_commit.seq.0 > 0
            && projection.fork_receipt.codec.name == "subject_branch_fork"
            && projection.fork_receipt.codec.major == 1
            && projection.fork_receipt.bytes > 0,
            "invalid subject fork projection");
        let key = format!("subject_branch_projection:{}", projection.branch_id);
        let conn = self.connection();
        let prior: Option<String> = conn.query_row("SELECT v FROM meta WHERE k=?1",
            [&key], |row| row.get(0)).optional()?;
        if let Some(prior) = prior {
            ensure!(serde_json::from_str::<SubjectBranchProjection>(&prior)? == *projection
                && snapshot.identity == target, "fork projection replay differs from durable identity");
            return Ok(target);
        }
        ensure!(snapshot.identity == projection.parent, "Node identity moved before fork projection");
        let data: String = conn.query_row("SELECT data FROM node_branches WHERE branch_id=?1",
            [projection.branch_id.to_string()], |row| row.get(0)).optional()?
            .context("fork has no recorded availability branch")?;
        let branch: BranchFact = serde_json::from_str(&data)?;
        ensure!(branch.branch_id == projection.branch_id && branch.kind == BranchKind::Availability
            && branch.subject_id == Some(projection.parent.agent_id)
            && !branch.global_quorum && branch.merged.is_none(),
            "fork target is not this subject's open availability branch");
        conn.execute("INSERT INTO meta(k,v) VALUES(?1,?2)",
            params![key, serde_json::to_string(projection)?])?;
        conn.execute("UPDATE meta SET v=?1 WHERE k='identity'",
            [serde_json::to_string(&Some(&target))?])?;
        self.commit()?;
        Ok(target)
    }
}

/// Historical source identity permits original E1 retention only. It confers
/// no new dispatch permission and cannot settle a current-branch task.
pub(crate) fn retained_subject_identity(conn: &Connection, current: &Identity, source: &Identity) -> Result<bool> {
    if current.agent_id != source.agent_id || current.origin_proof != source.origin_proof { return Ok(false); }
    let mut query = conn.prepare("SELECT v FROM meta WHERE k LIKE 'subject_branch_projection:%'")?;
    let rows = query.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let projection: SubjectBranchProjection = serde_json::from_str(&row?)?;
        if projection.parent == *source { return Ok(true); }
    }
    Ok(false)
}
