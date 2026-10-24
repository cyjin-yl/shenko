//! Versioned instance identity storage — SPEC §6, R117.
//!
//! Every revision is kept; the latest one is current. A revision can only be
//! appended as exactly `current + 1`, so two concurrent editors cannot both
//! claim the same successor and no change silently overwrites another.
use rusqlite::{Connection, OptionalExtension, params};

use shenko_types::error::ShenkoError;
use shenko_types::identity::{IdentityChange, IdentityProfile, IdentitySource};
use shenko_types::ids::EntityId;

use crate::store::Store;

pub(crate) const IDENTITY_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS identity_profiles (
    agent_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    profile TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, revision)
);
";

impl Store {
    /// Current identity revision of `agent_id`, if any was recorded.
    pub fn latest_identity(&self, agent_id: EntityId) -> Result<Option<IdentityProfile>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_latest(&conn, agent_id)
    }

    /// All revisions, oldest first — the traceable history of name and soul.
    pub fn identity_history(&self, agent_id: EntityId) -> Result<Vec<IdentityProfile>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare("SELECT profile FROM identity_profiles WHERE agent_id = ?1 ORDER BY revision")
            .map_err(db)?;
        let rows = stmt
            .query_map(params![agent_id.to_string()], |r| r.get::<_, String>(0))
            .map_err(db)?;
        rows.map(|row| decode(&row.map_err(db)?)).collect()
    }

    /// Record revision 1 unless the agent already has an identity. Returns the
    /// current revision either way; older installations adopt one on start.
    pub fn ensure_identity(&self, agent_id: EntityId, name: Option<&str>) -> Result<IdentityProfile, ShenkoError> {
        let conn = self.ledger.conn.lock();
        if let Some(current) = read_latest(&conn, agent_id)? {
            return Ok(current);
        }
        let genesis = IdentityProfile::genesis(agent_id, name);
        insert(&conn, &genesis)?;
        Ok(genesis)
    }

    /// Apply one change on top of the current revision. `Ok(None)` means the
    /// change was a no-op. Fails with a conflict when no identity exists yet.
    pub fn change_identity(
        &self,
        agent_id: EntityId,
        change: &IdentityChange,
        source: IdentitySource,
    ) -> Result<Option<IdentityProfile>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let current = read_latest(&conn, agent_id)?
            .ok_or_else(|| ShenkoError::conflict("identity not initialised for agent"))?;
        let Some(next) = current.successor(change, source) else { return Ok(None) };
        insert(&conn, &next)?;
        Ok(Some(next))
    }

    /// Append a revision prepared elsewhere (e.g. by an identity plugin). It
    /// must be exactly the successor of the stored current revision; a stale
    /// editor gets a conflict instead of overwriting a newer change.
    pub fn append_identity(&self, next: &IdentityProfile) -> Result<(), ShenkoError> {
        let conn = self.ledger.conn.lock();
        let expected = read_latest(&conn, next.agent_id)?.map_or(1, |p| p.revision + 1);
        if next.revision != expected {
            return Err(ShenkoError::conflict(format!(
                "identity revision {} is not the successor (expected {expected})",
                next.revision
            )));
        }
        insert(&conn, next)
    }
}

fn read_latest(conn: &Connection, agent_id: EntityId) -> Result<Option<IdentityProfile>, ShenkoError> {
    conn.query_row(
        "SELECT profile FROM identity_profiles WHERE agent_id = ?1 ORDER BY revision DESC LIMIT 1",
        params![agent_id.to_string()],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map_err(db)?
    .map(|json| decode(&json))
    .transpose()
}

/// The primary key rejects a second writer of the same revision.
fn insert(conn: &Connection, profile: &IdentityProfile) -> Result<(), ShenkoError> {
    let json = serde_json::to_string(profile).map_err(|e| ShenkoError::storage(e.to_string()))?;
    conn.execute(
        "INSERT INTO identity_profiles (agent_id, revision, profile, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            profile.agent_id.to_string(),
            profile.revision as i64,
            json,
            profile.created_at.to_rfc3339(),
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
            ShenkoError::conflict(format!("identity revision {} already exists", profile.revision))
        }
        other => db(other),
    })?;
    Ok(())
}

fn decode(json: &str) -> Result<IdentityProfile, ShenkoError> {
    serde_json::from_str(json).map_err(|e| ShenkoError::storage(format!("invalid identity profile: {e}")))
}

fn db(error: rusqlite::Error) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}
