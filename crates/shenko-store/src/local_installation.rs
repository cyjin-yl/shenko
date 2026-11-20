//! Physical home provisioning belongs to one installation, not to the subject's
//! replicated birth record or the current ledger writer. Peer archives and
//! canonical row transitions must never copy this local fact.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    commit::CommitRef,
    error::ShenkoError,
    ids::EntityId,
    object::{Digest, ObjectRef},
    subject::SubjectGenesis,
};

use crate::{
    ContentStore, InstallationBinding, Store,
    store::{read_installation_binding, read_local_subject_id, storage_error},
};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_installation_provisioning (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    data TEXT NOT NULL
);
"#;

pub const LOCAL_HOME_NAMESPACE: &str = "shenko.installation";
pub const LOCAL_HOME_CODEC: &str = "home-provisioning";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalInstallationProvisioning {
    pub schema_version: u32,
    pub installation_id: EntityId,
    /// Complete, verified manifest of the first successful local preparation.
    /// Later compatible capability upgrades do not rewrite this evidence.
    pub home_manifest_ref: ObjectRef,
    pub committed_at: CommitRef,
    pub store_incarnation: EntityId,
}

/// Both projections come from one staged SQLite read transaction. This is
/// installation metadata; neither projection grants selection or execution.
#[derive(Debug, PartialEq, Eq)]
pub struct HomeInstallationBinding {
    pub binding: InstallationBinding,
    pub installation_id: EntityId,
}

impl Store {
    pub fn local_installation_provisioning(
        &self,
    ) -> Result<Option<LocalInstallationProvisioning>, ShenkoError> {
        read_provisioning(&self.ledger.conn.lock(), &self.cas)
    }

    /// Resolve an existing physical installation first. A genuinely empty,
    /// unprovisioned store uses its initial writer ID once, before Bootstrap
    /// verifies the actual home and persists the local provisioning record.
    pub fn installation_id_for_home(&self) -> Result<EntityId, ShenkoError> {
        installation_id_for_home_conn(&self.ledger.conn.lock(), &self.cas, self.ledger.ledger_id())
    }

    /// Called only after the seed has prepared and independently read back
    /// the actual home. CAS evidence alone is not filesystem admission; the
    /// seed owns that admission and this transaction records its result.
    pub fn record_local_installation_provisioning(
        &self,
        installation_id: EntityId,
        home_manifest_ref: ObjectRef,
    ) -> Result<LocalInstallationProvisioning, ShenkoError> {
        let home = read_home(&self.cas, &home_manifest_ref, installation_id)?;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                if installation_id_for_home_conn(&tx, &self.cas, self.ledger.ledger_id())?
                    != installation_id
                {
                    return Err(ShenkoError::conflict(
                        "prepared home differs from the persisted installation identity",
                    ));
                }
                if let Some(existing) = read_provisioning(&tx, &self.cas)? {
                    let initial = read_home(
                        &self.cas,
                        &existing.home_manifest_ref,
                        existing.installation_id,
                    )?;
                    for field in ["installation_id", "mounts", "created_at", "home_guest_path"] {
                        if initial.get(field) != home.get(field) {
                            return Err(ShenkoError::conflict(format!(
                                "prepared home changed its provisioned {field} binding"
                            )));
                        }
                    }
                    return Ok(existing);
                }
                let committed_at =
                    CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                let record = LocalInstallationProvisioning {
                    schema_version: 1,
                    installation_id,
                    home_manifest_ref: home_manifest_ref.clone(),
                    committed_at,
                    store_incarnation: self.capabilities.incarnation,
                };
                tx.execute(
                    "INSERT INTO local_installation_provisioning(singleton,data) VALUES(1,?1)",
                    [serde_json::to_string(&record).map_err(storage_error)?],
                )
                .map_err(storage_error)?;
                self.record_subject_commit(&tx, committed_at, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(record)
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    /// Zero-touch inspection: existing CAS only, one stable database tuple,
    /// and no schema initialization, home preparation or recovery writes.
    pub fn inspect_home_installation_binding(
        db_path: &Path,
        cas_dir: &Path,
    ) -> anyhow::Result<HomeInstallationBinding> {
        let cas = ContentStore::open_existing(cas_dir.to_owned())?;
        Self::inspect_database(db_path, |conn| {
            let binding = read_installation_binding(conn)?;
            let installation_id = installation_id_for_home_conn(conn, &cas, binding.ledger_id)?;
            Ok(HomeInstallationBinding {
                binding,
                installation_id,
            })
        })
    }
}

fn installation_id_for_home_conn(
    conn: &Connection,
    cas: &ContentStore,
    ledger_id: EntityId,
) -> Result<EntityId, ShenkoError> {
    if let Some(agent) = read_local_subject_id(conn, ledger_id)? {
        // Validate immutable source identity even when local provisioning
        // exists; a local override cannot hide corrupt source bootstrap facts.
        installation_id_for_subject_conn(conn, cas, agent)
    } else {
        Ok(read_provisioning(conn, cas)?.map_or(ledger_id, |record| record.installation_id))
    }
}

pub(crate) fn installation_id_for_subject_conn(
    conn: &Connection,
    cas: &ContentStore,
    agent: EntityId,
) -> Result<EntityId, ShenkoError> {
    let (genesis, source_installation) = source_installation(conn, cas, agent)?;
    let Some(local) = read_provisioning(conn, cas)? else {
        return Ok(source_installation);
    };
    if local.installation_id != source_installation {
        let origin: Option<(String, String)> = conn
            .query_row(
                "SELECT source_ledger_id,genesis_id FROM cognition_origins WHERE agent_id=?1",
                [agent.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(storage_error)?;
        let expected = (
            genesis.created_commit.ledger_id.to_string(),
            genesis.genesis_id.to_string(),
        );
        if origin.as_ref() != Some(&expected) {
            return Err(ShenkoError::conflict(
                "local home differs from subject bootstrap without exact imported origin",
            ));
        }
    }
    Ok(local.installation_id)
}

fn source_installation(
    conn: &Connection,
    cas: &ContentStore,
    agent: EntityId,
) -> Result<(SubjectGenesis, EntityId), ShenkoError> {
    let raw: String = conn
        .query_row(
            "SELECT data FROM subject_genesis WHERE subject_id=?1",
            [agent.to_string()],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    let genesis: SubjectGenesis = serde_json::from_str(&raw).map_err(storage_error)?;
    if genesis.subject_id != agent {
        return Err(ShenkoError::conflict(
            "bootstrap genesis subject binding mismatch",
        ));
    }
    let indexed: Option<(String, String, String)> = conn
        .query_row(
            "SELECT installation_id,subject_id,data FROM bootstrap_records WHERE bootstrap_id=?1",
            [genesis.bootstrap_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(storage_error)?;
    let parse_identity = |value: &serde_json::Value| -> Result<EntityId, ShenkoError> {
        value
            .as_str()
            .ok_or_else(|| ShenkoError::storage("invalid bootstrap installation_id"))?
            .parse::<EntityId>()
            .map_err(storage_error)
    };
    let path = cas
        .verified_object_path(&genesis.bootstrap_event_ref)
        .map_err(storage_error)?;
    let event: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).map_err(storage_error)?)
            .map_err(storage_error)?;
    let event_installation = event
        .get("installation_id")
        .map(parse_identity)
        .transpose()?;
    let indexed_installation = if let Some((installation, subject, data)) = indexed {
        let installation = installation.parse::<EntityId>().map_err(storage_error)?;
        let value: serde_json::Value = serde_json::from_str(&data).map_err(storage_error)?;
        let recorded = value
            .get("installation_id")
            .ok_or_else(|| ShenkoError::storage("bootstrap record has no installation_id"))?;
        if subject.parse::<EntityId>().map_err(storage_error)? != agent
            || parse_identity(recorded)? != installation
        {
            return Err(ShenkoError::conflict(
                "bootstrap installation identity differs from indexed binding",
            ));
        }
        Some(installation)
    } else {
        None
    };
    let installation = match (event_installation, indexed_installation) {
        (Some(event), Some(indexed)) if event != indexed => {
            return Err(ShenkoError::conflict(
                "bootstrap event and provisioning installation identities differ",
            ));
        }
        (Some(installation), _) | (_, Some(installation)) => installation,
        (None, None) => {
            return Err(ShenkoError::storage(
                "bootstrap has no persisted installation identity",
            ));
        }
    };
    Ok((genesis, installation))
}

pub(crate) fn read_provisioning(
    conn: &Connection,
    cas: &ContentStore,
) -> Result<Option<LocalInstallationProvisioning>, ShenkoError> {
    // Read-only inspection also supports installations predating this table.
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_installation_provisioning')",
        [], |row| row.get(0),
    ).map_err(storage_error)?;
    if !exists {
        return Ok(None);
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM local_installation_provisioning WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let record: LocalInstallationProvisioning =
        serde_json::from_str(&raw).map_err(storage_error)?;
    if record.schema_version != 1 || record.committed_at.seq.0 == 0 {
        return Err(ShenkoError::storage(
            "invalid local installation provisioning schema/receipt",
        ));
    }
    // A same-installation SQLite backup retains this original receipt while
    // acquiring a new writer ID. Comparing to the current writer would turn
    // an ordinary software update into a different physical installation.
    let receipt: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM commit_log WHERE ledger_id=?1 AND seq=?2 AND incarnation=?3 AND entity_count=1)",
        params![record.committed_at.ledger_id.to_string(), record.committed_at.seq.0 as i64,
            record.store_incarnation.to_string()],
        |row| row.get(0),
    ).map_err(storage_error)?;
    if !receipt {
        return Err(ShenkoError::conflict(
            "local installation provisioning receipt is missing",
        ));
    }
    read_home(cas, &record.home_manifest_ref, record.installation_id)?;
    Ok(Some(record))
}

fn read_home(
    cas: &ContentStore,
    reference: &ObjectRef,
    installation_id: EntityId,
) -> Result<serde_json::Value, ShenkoError> {
    if reference.store_namespace != LOCAL_HOME_NAMESPACE
        || reference.media_type != "application/json"
        || reference.codec.name != LOCAL_HOME_CODEC
        || reference.codec.major != 1
        || reference.codec.digest != Digest::blake3(LOCAL_HOME_CODEC.as_bytes())
        || reference.bytes > 64 * 1024
    {
        return Err(ShenkoError::storage(
            "invalid local home evidence reference",
        ));
    }
    let path = cas.verified_object_path(reference)?;
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).map_err(storage_error)?)
            .map_err(storage_error)?;
    if value["schema_version"] != 1
        || value["state"] != "ready"
        || value["installation_id"] != serde_json::json!(installation_id)
        || !value["mounts"]
            .as_array()
            .is_some_and(|mounts| !mounts.is_empty())
        || value["created_at"].as_str().is_none()
        || value["home_guest_path"].as_str().is_none()
    {
        return Err(ShenkoError::storage("invalid ready local home evidence"));
    }
    Ok(value)
}
