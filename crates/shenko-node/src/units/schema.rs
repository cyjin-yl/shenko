//! Exact installation storage fingerprints and migration of attributable legacy edges.
//!
//! These statements retain the table identities admitted by `Node::open`; no
//! new connection or schema authority is introduced here.

use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use super::codec::validate_descriptor;
use super::installation::StoredVersion;
use super::resolution::{ResolvedDependency, edge_matches_spec, legacy_ambiguous};

/// Exact v3 node-storage fingerprint, retained for admission before migration.
pub(crate) const UNIT_TABLES_V3: &str = "
CREATE TABLE unit_versions(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_current(unit_id TEXT PRIMARY KEY,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,enabled INTEGER NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE unit_edges(unit_id TEXT NOT NULL,revision TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,edge_id));
CREATE TABLE unit_materializations(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,root TEXT NOT NULL,path TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_attempts(attempt_id TEXT PRIMARY KEY,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,state TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_trials(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT NOT NULL,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,outcome TEXT NOT NULL,artifact_bound INTEGER NOT NULL,artifact_verified INTEGER NOT NULL,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);";

const UNIT_EDGE_TABLE: &str = "CREATE TABLE unit_edges(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest,edge_id));";
const UNIT_VERSION_HISTORY_TABLE: &str = "CREATE TABLE unit_version_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL);";
const UNIT_EDGE_HISTORY_TABLE: &str = "CREATE TABLE unit_edge_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL);";

/// Schema-6 node-storage fingerprint. Storage schema 7 adds
/// `UNIT_REMOVALS_TABLE` on top of exactly these tables, so admission of an
/// older installation keeps comparing it against what that version really
/// created instead of accepting a different table set under the same number.
/// The legacy edge table retains every old row; it is evidence, not an
/// authority for ambiguous version ownership.
pub const UNIT_TABLES: &str = "
CREATE TABLE unit_versions(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_version_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_current(unit_id TEXT PRIMARY KEY,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,enabled INTEGER NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE \"unit_edges_legacy\"(unit_id TEXT NOT NULL,revision TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,edge_id));
CREATE TABLE unit_edges(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest,edge_id));
CREATE TABLE unit_edge_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_materializations(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,root TEXT NOT NULL,path TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_attempts(attempt_id TEXT PRIMARY KEY,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,state TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_trials(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT NOT NULL,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,outcome TEXT NOT NULL,artifact_bound INTEGER NOT NULL,artifact_verified INTEGER NOT NULL,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);";

/// Append-only record of one withdrawn version. A removed version leaves the
/// live tables; this row is what explains the gap later, and its evidence
/// (trials, observations, adoption) stays readable.
pub(crate) const UNIT_REMOVALS_TABLE: &str = "
CREATE TABLE unit_removals(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,removed_at TEXT NOT NULL,reason TEXT NOT NULL,data TEXT NOT NULL);";

/// Migrate only attributable legacy choices. Multiple retained digests may
/// have overwritten each other's old rows, even if their declarations match.
/// The untouched table remains available for investigation and recovery.
pub(crate) fn migrate_v3_edges(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch("ALTER TABLE unit_edges RENAME TO unit_edges_legacy;")?;
    tx.execute_batch(UNIT_EDGE_TABLE)?;
    tx.execute_batch(UNIT_VERSION_HISTORY_TABLE)?;
    tx.execute_batch(UNIT_EDGE_HISTORY_TABLE)?;
    let versions: Vec<(String, String, String, String, i64)> = tx
        .prepare(
            "SELECT v.unit_id,v.revision,v.artifact_digest,v.data, \
             (SELECT count(*) FROM unit_versions AS n WHERE n.unit_id=v.unit_id AND n.revision=v.revision) \
             FROM unit_versions AS v ORDER BY v.unit_id,v.revision,v.artifact_digest",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (unit_id, revision, artifact_digest, data, version_count) in versions {
        let stored: StoredVersion = serde_json::from_str(&data)?;
        ensure!(
            stored.unit.unit_id == unit_id
                && stored.unit.revision == revision
                && stored.unit.artifact_digest.0 == artifact_digest,
            "stored unit version identity does not match its key"
        );
        validate_descriptor(&stored.unit)?;
        for spec in &stored.unit.dependencies {
            let old: Option<String> = tx
                .query_row(
                    "SELECT data FROM unit_edges_legacy WHERE unit_id=?1 AND revision=?2 AND edge_id=?3",
                    params![unit_id, revision, spec.edge_id],
                    |r| r.get(0),
                )
                .optional()?;
            let edge = if version_count == 1 {
                old.and_then(|value| serde_json::from_str::<ResolvedDependency>(&value).ok())
                    .filter(|edge| edge_matches_spec(edge, spec))
                    .unwrap_or_else(|| legacy_ambiguous(spec, "legacy edge is missing or differs from the retained declaration; use resolve_version for this exact version"))
            } else {
                legacy_ambiguous(spec, "legacy edge ownership is ambiguous across retained artifact digests; use resolve_version for this exact version")
            };
            tx.execute(
                "INSERT INTO unit_edges(unit_id,revision,artifact_digest,edge_id,data) VALUES(?1,?2,?3,?4,?5)",
                params![unit_id, revision, artifact_digest, spec.edge_id, serde_json::to_string(&edge)?],
            )?;
        }
    }
    Ok(())
}

pub(super) fn ensure_unit_schema(conn: &rusqlite::Connection) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN \
         ('unit_versions','unit_version_history','unit_current','unit_edges','unit_edge_history','unit_edges_legacy','unit_materializations','unit_attempts','unit_trials','unit_removals')",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        count == 10,
        "node storage schema lacks the installation-unit tables; migrate the node storage schema first"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Node;
    use crate::units::codec::b64_decode;
    use crate::units::installation::StoredVersion;
    use crate::units::resolution::{EdgeStatus, ResolvedDependency, resolve_edge};
    use crate::units::test_support::{exact, import, package, scratch};
    use rusqlite::params;
    use serde_json::json;
    use shenko_types::ids::EntityId;
    use super::UNIT_TABLES_V3;

    #[test]
    fn v3_migration_preserves_identity_history_and_ambiguous_legacy_rows() {
        let dir = scratch("legacy-v3");
        let package_root = scratch("legacy-v3-packages");
        let target = package("target", "1.0.0", b"old-target", vec![]);
        let unique = package("unique", "1.0.0", b"old-unique", vec![exact("edge", "target", None)]);
        let first = package("multi", "1.0.0", b"old-one", vec![exact("edge", "target", None)]);
        let second = package("multi", "1.0.0", b"old-two", vec![exact("edge", "target", None)]);
        let node_id = EntityId::new();
        let ledger = EntityId::new();
        let installation = EntityId::new();
        let old_choice = resolve_edge(&first.unit.dependencies[0], &[(
            target.unit.unit_id.clone(), target.unit.revision.clone(), target.unit.artifact_digest.clone(), vec![],
        )]);
        let old_json = serde_json::to_string(&old_choice).unwrap();
        {
            let mut db = rusqlite::Connection::open(dir.join("node.sqlite3")).unwrap();
            let tx = db.transaction().unwrap();
            tx.execute_batch(crate::NODE_TABLES).unwrap();
            tx.execute_batch(crate::RECEIPT_TABLE).unwrap();
            tx.execute_batch(crate::roles::ROLE_SCHEMA).unwrap();
            tx.execute_batch(crate::peers::SYNC_SCHEMA).unwrap();
            tx.execute_batch(UNIT_TABLES_V3).unwrap();
            crate::set(&tx, "installation", &installation).unwrap();
            crate::set(&tx, "node", &node_id).unwrap();
            crate::set(&tx, "ledger", &ledger).unwrap();
            crate::set(&tx, "identity", &Option::<crate::Identity>::None).unwrap();
            crate::set(&tx, "retired_identities", &Vec::<crate::Identity>::new()).unwrap();
            for package in [&target, &unique, &first, &second] {
                let bytes = b64_decode(&package.artifact_base64).unwrap();
                crate::seal(&tx, &bytes, "application/octet-stream", "unit-package").unwrap();
                let stored = StoredVersion { unit: package.unit.clone(), remote_evidence: vec![json!({"historical": true})], imported_at: "old".into(), origin_node: Some("origin".into()) };
                tx.execute("INSERT INTO unit_versions VALUES(?1,?2,?3,'old',?4)",
                    params![package.unit.unit_id, package.unit.revision, package.unit.artifact_digest.0, serde_json::to_string(&stored).unwrap()]).unwrap();
                tx.execute("INSERT OR IGNORE INTO unit_current VALUES(?1,?2,?3,1,'old')",
                    params![package.unit.unit_id, package.unit.revision, package.unit.artifact_digest.0]).unwrap();
            }
            for id in ["unique", "multi"] {
                tx.execute("INSERT INTO unit_edges VALUES(?1,'1.0.0','edge',?2)", params![id, old_json]).unwrap();
            }
            tx.execute("INSERT INTO unit_edges VALUES('multi','1.0.0','stale','unparsed historical row')", []).unwrap();
            tx.execute("INSERT INTO events(id,source,digest,data) VALUES('historic','source','digest','event bytes')", []).unwrap();
            tx.pragma_update(None, "application_id", crate::APPLICATION_ID).unwrap();
            tx.pragma_update(None, "user_version", 3).unwrap();
            tx.commit().unwrap();
        }
        let mut node = Node::open(&dir).unwrap();
        assert_eq!(node.local_node_id().unwrap(), Some(node_id));
        let retained_installation: String = node.conn.query_row("SELECT v FROM meta WHERE k='installation'", [], |r| r.get(0)).unwrap();
        let retained_ledger: String = node.conn.query_row("SELECT v FROM meta WHERE k='ledger'", [], |r| r.get(0)).unwrap();
        assert_eq!(retained_installation, serde_json::to_string(&installation).unwrap());
        assert_eq!(retained_ledger, serde_json::to_string(&ledger).unwrap());
        let event: (i64, String) = node.conn.query_row("SELECT seq,data FROM events WHERE id='historic'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(event, (1, "event bytes".into()));
        let legacy: Vec<(String, String, String)> = node.conn.prepare("SELECT unit_id,edge_id,data FROM unit_edges_legacy ORDER BY unit_id,edge_id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(legacy.len(), 3);
        assert!(legacy.iter().any(|row| row.1 == "stale" && row.2 == "unparsed historical row"));
        let unique_version = node.installed_unit_version("unique", "1.0.0", &unique.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(unique_version.edges[0], old_choice);
        let ambiguous = node.installed_unit_version("multi", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(ambiguous.edges[0].status, EdgeStatus::LegacyAmbiguous);
        assert_eq!(ambiguous.edges[0].selected_artifact_digest, None);
        import(&mut node, &package_root, vec![target.clone()]);
        let still_unknown = node.installed_unit_version("multi", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(still_unknown.edges[0].status, EdgeStatus::LegacyAmbiguous);
        assert_eq!(node.unit_status("multi").unwrap()["readiness"]["state"], json!("unresolved"));
        // An authored schema-3 reimport is an explicit new decision for this
        // digest, not an attribution of the old shared row.
        import(&mut node, &package_root, vec![first.clone()]);
        let recovered = node.installed_unit_version("multi", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(recovered.edges[0].status, EdgeStatus::Satisfied);
        assert_eq!(recovered.edges[0].selected_artifact_digest, Some(target.unit.artifact_digest));
        let saved: String = node.conn.query_row(
            "SELECT data FROM unit_version_history WHERE unit_id='multi' AND revision='1.0.0' AND artifact_digest=?1 ORDER BY seq LIMIT 1",
            [&first.unit.artifact_digest.0], |r| r.get(0),
        ).unwrap();
        let saved: StoredVersion = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved.imported_at, "old");
        assert_eq!(saved.origin_node.as_deref(), Some("origin"));
        assert_eq!(saved.remote_evidence, vec![json!({"historical": true})]);
        let saved_edge: String = node.conn.query_row(
            "SELECT data FROM unit_edge_history WHERE unit_id='multi' AND revision='1.0.0' AND artifact_digest=?1 AND edge_id='edge' ORDER BY seq LIMIT 1",
            [&first.unit.artifact_digest.0], |r| r.get(0),
        ).unwrap();
        let saved_edge: ResolvedDependency = serde_json::from_str(&saved_edge).unwrap();
        assert_eq!(saved_edge.status, EdgeStatus::LegacyAmbiguous);
        assert_eq!(saved_edge.selected_artifact_digest, None);
        drop(node);
        let reopened = Node::open(&dir).unwrap();
        assert_eq!(reopened.local_node_id().unwrap(), Some(node_id));
        assert_eq!(reopened.conn.query_row("SELECT count(*) FROM unit_edges_legacy", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
    }
}
