//! Local authority reservation shared by every SQLite control-plane writer.
//!
//! This lock serializes locally learned authority changes. It does not prove
//! that a remote quorum remains reachable or provide a distributed lease.
use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::{BranchFact, FencingToken, Identity, MemberConfiguration, Node, election};
use shenko_types::ids::EntityId;

#[derive(Debug, Clone)]
pub struct NodeAuthoritySnapshot {
    pub node_id: EntityId,
    pub identity: Identity,
    pub term: u64,
    pub configuration: Option<MemberConfiguration>,
    pub fence: Option<FencingToken>,
    pub branch: Option<BranchFact>,
}

/// Owns a connection with a SQLite RESERVED write lock. Normal Node writers
/// cannot advance term, configuration, identity or branch state until Drop.
pub struct NodeAuthorityGuard {
    node: Node,
    snapshot: NodeAuthoritySnapshot,
}

impl NodeAuthorityGuard {
    pub fn snapshot(&self) -> &NodeAuthoritySnapshot {
        &self.snapshot
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

impl Node {
    /// Consume this connection so a caller cannot open a nested transaction or
    /// accidentally use its stale snapshot after releasing the reservation.
    pub fn reserve_authority(self) -> Result<NodeAuthorityGuard> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let snapshot = Self::authority_snapshot_conn(&self.conn)?;
        Ok(NodeAuthorityGuard { node: self, snapshot })
    }

    pub(crate) fn authority_snapshot_conn(conn: &Connection) -> Result<NodeAuthoritySnapshot> {
        let node_id = Self::stored_node_id(conn)?.context("node is not initialized")?;
        let identity = Self::stored_identity(conn)?.context("node has no subject identity")?;
        let branch: Option<String> = conn.query_row(
            "SELECT data FROM node_branches WHERE branch_id=?1",
            [identity.branch_id.to_string()], |row| row.get(0),
        ).optional()?;
        Ok(NodeAuthoritySnapshot {
            node_id, identity,
            term: election::state_number(conn, "term")?,
            configuration: Self::committed_config(conn)?,
            fence: Self::fencing_token_conn(conn)?,
            branch: branch.map(|raw| serde_json::from_str(&raw)).transpose()?,
        })
    }
}
