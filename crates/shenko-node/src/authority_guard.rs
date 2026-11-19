//! Local authority reservation shared by every SQLite control-plane writer.
//!
//! This lock serializes locally learned authority changes. It does not prove
//! that a remote quorum remains reachable or provide a distributed lease.
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension};

use crate::{BranchFact, BranchKind, FencingToken, Identity, MemberConfiguration, Node, election};
use shenko_types::ids::EntityId;

#[derive(Debug, Clone)]
pub struct NodeAuthoritySnapshot {
    pub node_id: EntityId,
    pub identity: Identity,
    pub term: u64,
    pub configuration: Option<MemberConfiguration>,
    pub fence: Option<FencingToken>,
    pub branch: Option<BranchFact>,
    pub component_revision: u64,
    pub component: Option<crate::ComponentCertificate>,
}

impl NodeAuthoritySnapshot {
    /// A local branch can admit fresh work only while its opening authority
    /// still matches the exact control facts in this snapshot. Keeping the
    /// historical branch open does not override a newly learned primary.
    pub fn is_current_availability(&self) -> bool {
        self.branch.as_ref().is_some_and(|branch| {
            branch.branch_id == self.identity.branch_id
                && self.has_current_availability_authority(branch)
        })
    }

    fn has_current_availability_authority(&self, branch: &BranchFact) -> bool {
        branch.kind == BranchKind::Availability
            && branch.subject_id == Some(self.identity.agent_id)
            && !branch.global_quorum
            && branch.merged.is_none()
            && branch
                .availability_authority
                .as_ref()
                .is_some_and(|authority| {
                    authority.holder_node_id == self.node_id
                        && authority.component_revision == self.component_revision
                        && match (&authority.component, &self.component) {
                            (Some(expected), Some(component)) => component.digest().as_ref().is_ok_and(|digest| digest == expected)
                                && component.proposal.coordinator == self.node_id
                                && component.proposal.branch_id == branch.branch_id,
                            (None, None) => true,
                            _ => false,
                        }
                        && authority.opening_term == self.term
                        && authority.configuration_revision
                            == self.configuration.as_ref().map(|config| config.revision)
                        && authority.configuration_digest.as_ref()
                            == self.configuration.as_ref().map(|config| &config.digest)
                        && authority.fence == self.fence
                })
    }
}

/// Owns a connection with a SQLite RESERVED write lock. Normal Node writers
/// cannot advance term, configuration, identity or branch state until Drop.
pub struct NodeAuthorityGuard {
    node: Node,
    snapshot: NodeAuthoritySnapshot,
    permit: Option<shenko_types::installation::InstallationPermit>,
}

impl NodeAuthorityGuard {
    pub fn installation_permit(&self) -> Option<&shenko_types::installation::InstallationPermit> {
        self.permit.as_ref()
    }
    pub fn snapshot(&self) -> &NodeAuthoritySnapshot {
        &self.snapshot
    }

    /// Validate a target before creating its Store fork, while this reservation
    /// excludes concurrent Node authority changes. The target need not be the
    /// current identity yet. Repairing an already durable fork is a separate
    /// historical projection and must not use this fresh-work check.
    pub fn validate_availability_branch(&self, branch_id: EntityId) -> Result<()> {
        let raw: String = self
            .node
            .conn
            .query_row(
                "SELECT data FROM node_branches WHERE branch_id=?1",
                [branch_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .context("target has no recorded availability branch")?;
        let branch: BranchFact = serde_json::from_str(&raw)?;
        ensure!(
            branch.branch_id == branch_id
                && self.snapshot.has_current_availability_authority(&branch),
            "availability branch opening authority is no longer current"
        );
        Ok(())
    }

    /// Verify the original chosen action from this reserved connection. A
    /// recovered certificate may contain another valid acknowledgement set;
    /// the immutable value identity and descriptor must remain identical.
    pub fn validate_admission_grant(
        &self,
        grant: &shenko_types::authority::CommitAuthorityGrant,
    ) -> Result<()> {
        let stored = Node::committed_admission_conn(&self.node.conn, &grant.admission)?
            .context("admission has no committed local control value")?;
        anyhow::ensure!(
            stored.grant_id == grant.grant_id && stored.admission == grant.admission,
            "admission grant does not match its actual committed value"
        );
        anyhow::ensure!(
            grant.admission.subject_id == self.snapshot.identity.agent_id
                && grant.admission.branch_id.0 == self.snapshot.identity.branch_id.0,
            "admission grant belongs to another actual subject branch"
        );
        if !grant.replayed {
            Node::validate_admission_current_conn(&self.node.conn, &grant.admission)?;
        }
        Ok(())
    }

    /// Internal projection repair may write while preserving this reservation.
    /// It must not begin another transaction or release the connection early.
    pub(crate) fn connection(&self) -> &Connection {
        &self.node.conn
    }

    /// Publish a validated projection repair after its source Store receipt is
    /// durable. Drop still rolls back if COMMIT fails or this method is unused.
    pub(crate) fn commit(self) -> Result<()> {
        self.node.conn.execute_batch("COMMIT")?;
        Ok(())
    }
}

impl Drop for NodeAuthorityGuard {
    fn drop(&mut self) {
        // Closing the owned connection also rolls back on an error. A completed
        // explicit commit has no transaction left to roll back.
        let _ = self.node.conn.execute_batch("ROLLBACK");
    }
}

impl shenko_types::authority::AvailabilityForkAuthority for NodeAuthorityGuard {
    fn validate_fork(&self, subject: EntityId, parent: &shenko_types::ids::BranchId,
        origin: &shenko_types::object::Digest, child: &shenko_types::ids::BranchId)
        -> Result<(), shenko_types::error::ShenkoError>
    {
        let identity = &self.snapshot.identity;
        if identity.agent_id != subject || identity.branch_id.0 != parent.0
            || &identity.origin_proof != origin || parent == child
        {
            return Err(shenko_types::authority::authority_fenced(
                "availability fork differs from the reserved parent identity"));
        }
        self.validate_availability_branch(EntityId(child.0)).map_err(|error|
            shenko_types::authority::authority_fenced(format!("availability fork is no longer current: {error:#}")))
    }

    fn installation_permit(&self) -> Option<&shenko_types::installation::InstallationPermit> {
        self.permit.as_ref()
    }
}

impl Node {
    /// Consume this connection so a caller cannot open a nested transaction or
    /// accidentally use its stale snapshot after releasing the reservation.
    pub fn reserve_authority(self) -> Result<NodeAuthorityGuard> {
        let permit = self.installation_permit()?;
        self.reserve_authority_under(permit)
    }

    pub fn reserve_authority_under(self,
        permit: Option<shenko_types::installation::InstallationPermit>) -> Result<NodeAuthorityGuard> {
        if let Some(domain) = &self.installation {
            permit.as_ref().context("installed Node reservation requires a physical permit")?.validate(domain)?;
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let snapshot = Self::authority_snapshot_conn(&self.conn)?;
        Ok(NodeAuthorityGuard {
            node: self,
            snapshot,
            permit,
        })
    }

    pub(crate) fn authority_snapshot_conn(conn: &Connection) -> Result<NodeAuthoritySnapshot> {
        let node_id = Self::stored_node_id(conn)?.context("node is not initialized")?;
        let identity = Self::stored_identity(conn)?.context("node has no subject identity")?;
        let branch: Option<String> = conn
            .query_row(
                "SELECT data FROM node_branches WHERE branch_id=?1",
                [identity.branch_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let (component_revision, component) = Self::component_state_conn(conn)?;
        Ok(NodeAuthoritySnapshot {
            node_id,
            identity,
            term: election::state_number(conn, "term")?,
            configuration: Self::committed_config(conn)?,
            fence: Self::fencing_token_conn(conn)?,
            branch: branch.map(|raw| serde_json::from_str(&raw)).transpose()?,
            component_revision,
            component,
        })
    }
}
