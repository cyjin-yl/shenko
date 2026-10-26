//! Node-local routes and confirmation facts; credentials are resolved by SSH.
use super::{Node, text};
use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use shenko_types::{ids::EntityId, object::Digest};
use std::path::PathBuf;

pub(crate) const SYNC_SCHEMA: &str = "CREATE TABLE node_sync_peers(node_id TEXT PRIMARY KEY,config TEXT NOT NULL,confirmation TEXT,failure TEXT);";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPeer {
    pub node_id: EntityId,
    pub ssh_target: String,
    pub remote_binary: String,
    pub remote_data_dir: PathBuf,
    #[serde(default)]
    pub workspaces: Vec<PathBuf>,
    pub remote_workspace_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfirmation {
    pub cognition_fingerprint: Digest,
    pub units_fingerprint: Digest,
    pub confirmed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncFailure {
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPeerState {
    pub peer: SyncPeer,
    pub confirmation: Option<SyncConfirmation>,
    pub failure: Option<SyncFailure>,
}

impl Node {
    pub fn register_sync_peer(&mut self, peer: &SyncPeer) -> Result<()> {
        self.conn.execute(
            "INSERT INTO node_sync_peers(node_id,config) VALUES(?1,?2) ON CONFLICT(node_id) DO UPDATE SET config=excluded.config,confirmation=NULL",
            params![peer.node_id.to_string(), text(peer)?],
        )?;
        Ok(())
    }

    pub fn sync_peers(&self) -> Result<Vec<SyncPeerState>> {
        let mut statement = self
            .conn
            .prepare("SELECT config,confirmation,failure FROM node_sync_peers ORDER BY node_id")?;
        let rows = statement.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (peer, confirmation, failure) = row?;
            Ok(SyncPeerState {
                peer: serde_json::from_str(&peer)?,
                confirmation: confirmation.map(|s| serde_json::from_str(&s)).transpose()?,
                failure: failure.map(|s| serde_json::from_str(&s)).transpose()?,
            })
        })
        .collect()
    }

    pub fn confirm_sync_peer(
        &mut self,
        peer: &SyncPeer,
        confirmation: &SyncConfirmation,
    ) -> Result<()> {
        // Do not confirm a route whose configuration changed during transfer.
        self.conn.execute(
            "UPDATE node_sync_peers SET confirmation=?1,failure=NULL WHERE node_id=?2 AND config=?3",
            params![text(confirmation)?,peer.node_id.to_string(),text(peer)?],
        )?;
        Ok(())
    }

    pub fn fail_sync_peer(&mut self, peer: &SyncPeer, reason: String) -> Result<()> {
        let failure = SyncFailure {
            observed_at: chrono::Utc::now(),
            reason,
        };
        self.conn.execute(
            "UPDATE node_sync_peers SET failure=?1 WHERE node_id=?2 AND config=?3",
            params![text(&failure)?, peer.node_id.to_string(), text(peer)?],
        )?;
        Ok(())
    }
}
