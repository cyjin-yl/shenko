//! Durable plugin-root lifecycle: prepared → published → attached → retired.
//!
//! The live [`shenko_kernel::runtime::PluginRuntime`] is a projection of what
//! one process currently offers. It is not an installation database and it is
//! gone after a crash. This module owns the durable facts a projection cannot
//! carry:
//!
//! * which exact runtime implementation (`plugin_id` + implementation
//!   artifact) was installed for which exact Node unit version;
//! * that it was published for new calls, with the registry state observed at
//!   that moment;
//! * that a concrete process generation attached it, so new calls may use it;
//! * that it was retired, after which new calls stop but the installation,
//!   frozen operation contracts and evidence receipts stay readable.
//!
//! Each transition commits exactly one append-only admission receipt with its
//! own `CommitRef`. A retry of the same phase in the same process generation
//! returns the original receipt rather than appending a second one. Attach is
//! refused unless the durable publication committed first
//! ([`PluginLifecycleRefusalCode::AttachBeforePublish`]).
//!
//! This is not a second operation ledger. Operation rows, operation contracts
//! and evidence receipts stay in their existing tables; the lifecycle record
//! only names the exact plugin/artifact those facts refer to, and retirement
//! never deletes them.

use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, RuntimeGeneration};
use shenko_types::object::Digest;
use shenko_types::plugin_runtime::{
    PluginArtifactBinding, PluginAttachRequest, PluginCallResolution, PluginImplementationAvailability,
    PluginLifecycleRefusal, PluginLifecycleRefusalCode, PluginPhaseAdmission, PluginPrepareRequest,
    PluginPublishRequest, PluginRecoveryAction, PluginRecoveryPlan, PluginRetireRequest,
    PluginRootIdentity, PluginRootPhase, PluginRootRecord,
};

use crate::Store;
use crate::store::storage_error;

/// Tables owned by this module. Created on first use inside the writing
/// transaction, like the activation tables; the ledger migration file is not
/// rewritten for an additive lifecycle record.
pub const PLUGIN_ROOT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS plugin_root_installations (
    installation_id TEXT PRIMARY KEY,
    plugin_id TEXT NOT NULL,
    artifact_digest TEXT NOT NULL,
    generation INTEGER NOT NULL,
    phase TEXT NOT NULL,
    record_revision INTEGER NOT NULL,
    artifact_retained INTEGER NOT NULL,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_plugin_root_plugin_artifact
    ON plugin_root_installations(plugin_id, artifact_digest, updated_commit_seq);
CREATE INDEX IF NOT EXISTS idx_plugin_root_phase
    ON plugin_root_installations(phase, updated_commit_seq);

CREATE TABLE IF NOT EXISTS plugin_root_admissions (
    admission_id TEXT PRIMARY KEY,
    installation_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    phase TEXT NOT NULL,
    runtime_generation TEXT NOT NULL,
    record_revision INTEGER NOT NULL,
    data TEXT NOT NULL,
    committed_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_root_admission_phase
    ON plugin_root_admissions(installation_id, generation, phase, runtime_generation);
CREATE INDEX IF NOT EXISTS idx_plugin_root_admission_installation
    ON plugin_root_admissions(installation_id, generation, committed_seq);
"#;

/// Create the plugin-root lifecycle tables if this Store has none yet.
pub fn ensure_plugin_root_schema(conn: &Connection) -> Result<(), ShenkoError> {
    conn.execute_batch(PLUGIN_ROOT_SCHEMA).map_err(storage_error)
}

fn refusal(
    code: PluginLifecycleRefusalCode,
    installation_id: Option<EntityId>,
    phase: Option<PluginRootPhase>,
    detail: impl Into<String>,
) -> anyhow::Error {
    let mut refusal = PluginLifecycleRefusal::new(code, detail);
    refusal.installation_id = installation_id;
    refusal.phase = phase;
    refusal.into()
}

fn storage_failure(error: &anyhow::Error) -> bool {
    error.is::<rusqlite::Error>()
        || error.is::<serde_json::Error>()
        || error
            .downcast_ref::<ShenkoError>()
            .is_some_and(|error| matches!(error, ShenkoError::Storage(_)))
}

fn admission_id(
    installation_id: EntityId,
    generation: u64,
    phase: PluginRootPhase,
    runtime_generation: &RuntimeGeneration,
) -> EntityId {
    EntityId::derived(
        "shenko.plugin-root.admission/v1",
        format!(
            "{installation_id}/{generation}/{}/{}",
            phase.name(),
            runtime_generation.0
        )
        .as_bytes(),
    )
}

/// The facts one phase admission is built from. Every phase shares one
/// constructor, so the inputs travel as one record instead of a long
/// parameter list.
struct AdmissionInput<'a> {
    installation_id: EntityId,
    generation: u64,
    phase: PluginRootPhase,
    record_revision: u64,
    runtime_generation: &'a RuntimeGeneration,
    observed_registry_revision: u64,
    observed_capability_revision: u64,
    observed_hook_graph_digest: Option<Digest>,
    artifact: PluginArtifactBinding,
    reason: &'a str,
    evidence: &'a [shenko_types::object::ObjectRef],
    unresolved_operations: Option<u64>,
    committed_at: CommitRef,
}

fn make_admission(input: AdmissionInput<'_>) -> PluginPhaseAdmission {
    PluginPhaseAdmission {
        schema_version: 1,
        admission_id: admission_id(
            input.installation_id,
            input.generation,
            input.phase,
            input.runtime_generation,
        ),
        installation_id: input.installation_id,
        generation: input.generation,
        phase: input.phase,
        record_revision: input.record_revision,
        runtime_generation: input.runtime_generation.clone(),
        observed_registry_revision: input.observed_registry_revision,
        observed_capability_revision: input.observed_capability_revision,
        observed_hook_graph_digest: input.observed_hook_graph_digest,
        artifact: input.artifact,
        reason: input.reason.to_owned(),
        evidence: input.evidence.to_vec(),
        unresolved_operations: input.unresolved_operations,
        committed_at: input.committed_at,
    }
}

fn insert_admission(tx: &Transaction<'_>, admission: &PluginPhaseAdmission) -> Result<(), ShenkoError> {
    tx.execute(
        "INSERT INTO plugin_root_admissions
            (admission_id, installation_id, generation, phase, runtime_generation, record_revision, data, committed_seq)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            admission.admission_id.to_string(),
            admission.installation_id.to_string(),
            admission.generation as i64,
            admission.phase.name(),
            admission.runtime_generation.0,
            admission.record_revision as i64,
            serde_json::to_string(admission).map_err(storage_error)?,
            admission.committed_at.seq.0 as i64,
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

fn write_record(tx: &Transaction<'_>, record: &PluginRootRecord) -> Result<(), ShenkoError> {
    tx.execute(
        "INSERT INTO plugin_root_installations
            (installation_id, plugin_id, artifact_digest, generation, phase, record_revision, artifact_retained, data, updated_commit_seq)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(installation_id) DO UPDATE SET
            plugin_id=excluded.plugin_id,
            artifact_digest=excluded.artifact_digest,
            generation=excluded.generation,
            phase=excluded.phase,
            record_revision=excluded.record_revision,
            artifact_retained=excluded.artifact_retained,
            data=excluded.data,
            updated_commit_seq=excluded.updated_commit_seq",
        params![
            record.installation_id.to_string(),
            record.identity.plugin_id.to_string(),
            record.identity.implementation.artifact.digest.0,
            record.generation as i64,
            record.phase.name(),
            record.record_revision as i64,
            record.artifact_retained as i64,
            serde_json::to_string(record).map_err(storage_error)?,
            record.updated_commit.seq.0 as i64,
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

fn read_record(
    conn: &Connection,
    installation_id: EntityId,
) -> Result<Option<PluginRootRecord>, ShenkoError> {
    let row: Option<(String, String, i64, String, String, i64, i64)> = conn
        .query_row(
            "SELECT plugin_id, artifact_digest, generation, phase, data, record_revision, artifact_retained
             FROM plugin_root_installations WHERE installation_id=?1",
            [installation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()
        .map_err(storage_error)?;
    let Some((plugin_id, artifact_digest, generation, phase, data, record_revision, artifact_retained)) = row
    else {
        return Ok(None);
    };
    let record: PluginRootRecord = serde_json::from_str(&data).map_err(storage_error)?;
    // Columns exist so SQL can select without decoding every record; they are
    // never a second truth. A disagreement means one of the two was rewritten
    // outside this module, which is a storage corruption, not a phase.
    let consistent = record.identity.installation_id() == installation_id
        && record.identity.plugin_id.to_string() == plugin_id
        && record.identity.implementation.artifact.digest.0 == artifact_digest
        && record.generation == u64::try_from(generation).unwrap_or(u64::MAX)
        && record.phase.name() == phase
        && record.record_revision == u64::try_from(record_revision).unwrap_or(u64::MAX)
        && record.artifact_retained == (artifact_retained != 0);
    if !consistent {
        return Err(ShenkoError::storage(
            "plugin-root lifecycle columns disagree with their committed record",
        ));
    }
    Ok(Some(record))
}

fn read_admission(
    conn: &Connection,
    installation_id: EntityId,
    generation: u64,
    phase: PluginRootPhase,
    runtime_generation: &RuntimeGeneration,
) -> Result<Option<PluginPhaseAdmission>, ShenkoError> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM plugin_root_admissions
             WHERE installation_id=?1 AND generation=?2 AND phase=?3 AND runtime_generation=?4",
            params![
                installation_id.to_string(),
                generation as i64,
                phase.name(),
                runtime_generation.0
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    data.map(|data| serde_json::from_str(&data).map_err(storage_error))
        .transpose()
}

fn read_admissions(
    conn: &Connection,
    installation_id: EntityId,
) -> Result<Vec<PluginPhaseAdmission>, ShenkoError> {
    let rows: Vec<String> = conn
        .prepare(
            "SELECT data FROM plugin_root_admissions
             WHERE installation_id=?1 ORDER BY committed_seq, admission_id",
        )
        .map_err(storage_error)?
        .query_map([installation_id.to_string()], |row| row.get(0))
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage_error)?;
    rows.into_iter()
        .map(|data| serde_json::from_str(&data).map_err(storage_error))
        .collect()
}

/// True when the artifact binding is still addressable by its owner.
fn artifact_available(store: &Store, binding: &PluginArtifactBinding) -> bool {
    match binding {
        PluginArtifactBinding::StoreCas { object } => store.cas.object_exists(&object.digest),
        PluginArtifactBinding::InstallationUnit { retained } => *retained,
    }
}

/// The published phase's artifact must still be addressable before a new
/// attachment is admitted; a vanished implementation cannot serve new calls.
fn artifact_still_available(store: &Store, record: &PluginRootRecord) -> bool {
    record
        .published
        .as_ref()
        .is_some_and(|published| artifact_available(store, &published.artifact))
        && record.artifact_retained
}

fn same_prepare(existing: &PluginRootRecord, request: &PluginPrepareRequest) -> Result<(), anyhow::Error> {
    let recorded = &existing.prepared.artifact;
    match (recorded, &request.artifact) {
        (PluginArtifactBinding::StoreCas { object: a }, PluginArtifactBinding::StoreCas { object: b }) => {
            ensure!(
                a.digest == b.digest,
                refusal(
                    PluginLifecycleRefusalCode::IdentityConflict,
                    Some(existing.installation_id),
                    Some(existing.phase),
                    "prepare replay names a different CAS implementation digest"
                )
            );
        }
        (
            PluginArtifactBinding::InstallationUnit { retained: recorded },
            PluginArtifactBinding::InstallationUnit { retained: requested },
        ) => {
            ensure!(
                !(*recorded && !*requested),
                refusal(
                    PluginLifecycleRefusalCode::ArtifactUnavailable,
                    Some(existing.installation_id),
                    Some(existing.phase),
                    "prepare replay reports the installation unit no longer retains its artifact"
                )
            );
        }
        _ => {
            return Err(refusal(
                PluginLifecycleRefusalCode::AdmissionConflict,
                Some(existing.installation_id),
                Some(existing.phase),
                "prepare replay changed the artifact binding kind",
            ));
        }
    }
    Ok(())
}

/// Count the unresolved operations recorded for this exact plugin/artifact.
///
/// This is the Store's own operation rows, not a caller assertion. It is the
/// honest fact retirement leaves behind: those calls may still be running, so
/// retirement stops *new* calls without claiming the old ones stopped.
fn unresolved_operations(
    conn: &Connection,
    plugin_id: EntityId,
    artifact_digest: &Digest,
) -> Result<u64, ShenkoError> {
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM operations
             WHERE json_extract(data,'$.contract_ref.plugin_id')=?1
               AND json_extract(data,'$.contract_ref.artifact_digest')=?2
               AND json_extract(data,'$.state') IN ('prepared','in_flight','reconciling')",
            params![plugin_id.to_string(), artifact_digest.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?
        .unwrap_or(0);
    Ok(u64::try_from(count).unwrap_or(0))
}

impl Store {
    /// Admit the prepare phase, or replay an already prepared generation.
    ///
    /// A retired generation opens the next generation when the artifact is
    /// still retained. That is an explicit new lifecycle for the same exact
    /// installation (re-enable), not a revival of the retired generation: the
    /// retirement admission and its `effect_status=none` fact stay immutable.
    pub fn prepare_plugin_root(
        &self,
        identity: &PluginRootIdentity,
        request: &PluginPrepareRequest,
    ) -> Result<PluginRootRecord> {
        let installation_id = identity.installation_id();
        let available = match &request.artifact {
            PluginArtifactBinding::StoreCas { object } => self.cas.object_exists(&object.digest),
            PluginArtifactBinding::InstallationUnit { retained } => *retained,
        };
        self.ledger
            .with_mutation(
                || {
                    let mut conn = self.ledger.conn.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    ensure_plugin_root_schema(&tx)?;
                    if let Some(existing) = read_record(&tx, installation_id)? {
                        if existing.identity != *identity {
                            return Err(refusal(
                                PluginLifecycleRefusalCode::IdentityConflict,
                                Some(installation_id),
                                Some(existing.phase),
                                "the installation identity was redefined",
                            ));
                        }
                        match existing.phase {
                            PluginRootPhase::Prepared
                            | PluginRootPhase::Published
                            | PluginRootPhase::Attached => {
                                same_prepare(&existing, request)?;
                                tx.commit()?;
                                return Ok(existing);
                            }
                            PluginRootPhase::Retired => {
                                ensure!(
                                    existing.artifact_retained && available,
                                    refusal(
                                        PluginLifecycleRefusalCode::ArtifactUnavailable,
                                        Some(installation_id),
                                        Some(existing.phase),
                                        "a retired generation cannot be reopened after its artifact left the installation"
                                    )
                                );
                                let generation = existing.generation + 1;
                                let committed_at = CommitRef::new(
                                    self.ledger.ledger_id(),
                                    self.ledger.allocate_seq(&tx)?,
                                );
                                let revision = existing.record_revision + 1;
                                let prepared = make_admission(
                                    AdmissionInput {
                                        installation_id,
                                        generation,
                                        phase: PluginRootPhase::Prepared,
                                        record_revision: revision,
                                        runtime_generation: &request.runtime_generation,
                                        observed_registry_revision: 0,
                                        observed_capability_revision: 0,
                                        observed_hook_graph_digest: None,
                                        artifact: request.artifact.clone(),
                                        reason: &request.reason,
                                        evidence: &request.evidence,
                                        unresolved_operations: None,
                                        committed_at,
                                    },
                                );
                                let record = PluginRootRecord {
                                    schema_version: 1,
                                    installation_id,
                                    identity: identity.clone(),
                                    generation,
                                    phase: PluginRootPhase::Prepared,
                                    record_revision: revision,
                                    prepared: prepared.clone(),
                                    published: None,
                                    attached: None,
                                    retired: None,
                                    attached_runtime_generation: None,
                                    artifact_retained: true,
                                    evidence_retained: existing.evidence_retained,
                                    in_flight_operations_at_retirement: None,
                                    prepared_commit: committed_at,
                                    updated_commit: committed_at,
                                };
                                insert_admission(&tx, &prepared)?;
                                write_record(&tx, &record)?;
                                self.record_subject_commit(&tx, committed_at, 2)?;
                                tx.commit()?;
                                return Ok(record);
                            }
                        }
                    }
                    ensure!(
                        available,
                        refusal(
                            PluginLifecycleRefusalCode::ArtifactUnavailable,
                            Some(installation_id),
                            None,
                            "the implementation artifact is not addressable for a fresh installation"
                        )
                    );
                    let committed_at =
                        CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                    let prepared = make_admission(
                        AdmissionInput {
                            installation_id,
                            generation: 0,
                            phase: PluginRootPhase::Prepared,
                            record_revision: 0,
                            runtime_generation: &request.runtime_generation,
                            observed_registry_revision: 0,
                            observed_capability_revision: 0,
                            observed_hook_graph_digest: None,
                            artifact: request.artifact.clone(),
                            reason: &request.reason,
                            evidence: &request.evidence,
                            unresolved_operations: None,
                            committed_at,
                        },
                    );
                    let record = PluginRootRecord {
                        schema_version: 1,
                        installation_id,
                        identity: identity.clone(),
                        generation: 0,
                        phase: PluginRootPhase::Prepared,
                        record_revision: 0,
                        prepared: prepared.clone(),
                        published: None,
                        attached: None,
                        retired: None,
                        attached_runtime_generation: None,
                        artifact_retained: true,
                        evidence_retained: true,
                        in_flight_operations_at_retirement: None,
                        prepared_commit: committed_at,
                        updated_commit: committed_at,
                    };
                    insert_admission(&tx, &prepared)?;
                    write_record(&tx, &record)?;
                    self.record_subject_commit(&tx, committed_at, 2)?;
                    tx.commit()?;
                    Ok(record)
                },
                storage_failure,
            )
            .with_context(|| format!("prepare plugin root {installation_id}"))
    }

    /// Admit the publish phase of the current generation.
    ///
    /// Publication is what authorizes a later attachment. It is idempotent:
    /// the same process generation observing the same registry state gets the
    /// original receipt back instead of a second publication.
    pub fn publish_plugin_root(
        &self,
        installation_id: EntityId,
        request: &PluginPublishRequest,
    ) -> Result<PluginRootRecord> {
        self.ledger
            .with_mutation(
                || {
                    let mut conn = self.ledger.conn.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    ensure_plugin_root_schema(&tx)?;
                    let mut record = read_record(&tx, installation_id)?.ok_or_else(|| {
                        refusal(
                            PluginLifecycleRefusalCode::UnknownInstallation,
                            Some(installation_id),
                            None,
                            "publish needs an existing prepared installation",
                        )
                    })?;
                    if record.phase == PluginRootPhase::Retired {
                        return Err(refusal(
                            PluginLifecycleRefusalCode::Retired,
                            Some(installation_id),
                            Some(record.phase),
                            "a retired generation cannot be published; prepare the next generation explicitly",
                        ));
                    }
                    if record.phase != PluginRootPhase::Prepared {
                        // Publication already committed for this generation.
                        // A retry in the same process generation must name the
                        // same observation; a later process generation simply
                        // observes that the publication is already durable.
                        if let Some(published) = record.published.as_ref()
                            && published.runtime_generation == request.runtime_generation
                            && (published.observed_registry_revision
                                != request.observed_registry_revision
                                || published.observed_capability_revision
                                    != request.observed_capability_revision)
                        {
                            return Err(refusal(
                                PluginLifecycleRefusalCode::AdmissionConflict,
                                Some(installation_id),
                                Some(record.phase),
                                "the same process generation republished a different registry observation",
                            ));
                        }
                        tx.commit()?;
                        return Ok(record);
                    }
                    ensure!(
                        artifact_available(self, &record.prepared.artifact)
                            && record.artifact_retained,
                        refusal(
                            PluginLifecycleRefusalCode::ArtifactUnavailable,
                            Some(installation_id),
                            Some(record.phase),
                            "the prepared implementation artifact is no longer addressable",
                        )
                    );
                    let committed_at =
                        CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                    let revision = record.record_revision + 1;
                    let admission = make_admission(
                        AdmissionInput {
                            installation_id,
                            generation: record.generation,
                            phase: PluginRootPhase::Published,
                            record_revision: revision,
                            runtime_generation: &request.runtime_generation,
                            observed_registry_revision: request.observed_registry_revision,
                            observed_capability_revision: request.observed_capability_revision,
                            observed_hook_graph_digest: request.observed_hook_graph_digest.clone(),
                            artifact: record.prepared.artifact.clone(),
                            reason: &request.reason,
                            evidence: &request.evidence,
                            unresolved_operations: None,
                            committed_at,
                        },
                    );
                    insert_admission(&tx, &admission)?;
                    record.phase = PluginRootPhase::Published;
                    record.record_revision = revision;
                    record.published = Some(admission);
                    record.updated_commit = committed_at;
                    write_record(&tx, &record)?;
                    self.record_subject_commit(&tx, committed_at, 2)?;
                    tx.commit()?;
                    Ok(record)
                },
                storage_failure,
            )
            .with_context(|| format!("publish plugin root {installation_id}"))
    }

    /// Admit the attach phase of the current generation.
    ///
    /// Attach is refused before publication. A new process generation may
    /// attach the durable publication again; that appends a new attachment
    /// admission and never rewrites the dead generation's receipt.
    pub fn attach_plugin_root(
        &self,
        installation_id: EntityId,
        request: &PluginAttachRequest,
    ) -> Result<PluginRootRecord> {
        self.ledger
            .with_mutation(
                || {
                    let mut conn = self.ledger.conn.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    ensure_plugin_root_schema(&tx)?;
                    let mut record = read_record(&tx, installation_id)?.ok_or_else(|| {
                        refusal(
                            PluginLifecycleRefusalCode::UnknownInstallation,
                            Some(installation_id),
                            None,
                            "attach needs an existing published installation",
                        )
                    })?;
                    match record.phase {
                        PluginRootPhase::Prepared => {
                            return Err(refusal(
                                PluginLifecycleRefusalCode::AttachBeforePublish,
                                Some(installation_id),
                                Some(record.phase),
                                "attach is refused until the durable publication commits",
                            ));
                        }
                        PluginRootPhase::Retired => {
                            return Err(refusal(
                                PluginLifecycleRefusalCode::Retired,
                                Some(installation_id),
                                Some(record.phase),
                                "a retired generation does not attach; prepare the next generation explicitly",
                            ));
                        }
                        PluginRootPhase::Published | PluginRootPhase::Attached => {}
                    }
                    if let Some(existing) = read_admission(
                        &tx,
                        installation_id,
                        record.generation,
                        PluginRootPhase::Attached,
                        &request.runtime_generation,
                    )? {
                        // This exact process generation already attached. Only
                        // an identical observation is a replay.
                        ensure!(
                            existing.observed_registry_revision
                                == request.observed_registry_revision
                                && existing.observed_capability_revision
                                    == request.observed_capability_revision,
                            refusal(
                                PluginLifecycleRefusalCode::AdmissionConflict,
                                Some(installation_id),
                                Some(record.phase),
                                "the same process generation attached a different registry observation",
                            )
                        );
                        tx.commit()?;
                        return Ok(record);
                    }
                    ensure!(
                        artifact_still_available(self, &record),
                        refusal(
                            PluginLifecycleRefusalCode::ArtifactUnavailable,
                            Some(installation_id),
                            Some(record.phase),
                            "the published implementation artifact is no longer addressable",
                        )
                    );
                    let committed_at =
                        CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                    let revision = record.record_revision + 1;
                    let published_artifact = record
                        .published
                        .as_ref()
                        .map(|published| published.artifact.clone())
                        .unwrap_or_else(|| record.prepared.artifact.clone());
                    let admission = make_admission(
                        AdmissionInput {
                            installation_id,
                            generation: record.generation,
                            phase: PluginRootPhase::Attached,
                            record_revision: revision,
                            runtime_generation: &request.runtime_generation,
                            observed_registry_revision: request.observed_registry_revision,
                            observed_capability_revision: request.observed_capability_revision,
                            observed_hook_graph_digest: None,
                            artifact: published_artifact,
                            reason: &request.reason,
                            evidence: &request.evidence,
                            unresolved_operations: None,
                            committed_at,
                        },
                    );
                    insert_admission(&tx, &admission)?;
                    record.phase = PluginRootPhase::Attached;
                    record.record_revision = revision;
                    record.attached = Some(admission);
                    record.attached_runtime_generation = Some(request.runtime_generation.clone());
                    record.updated_commit = committed_at;
                    write_record(&tx, &record)?;
                    self.record_subject_commit(&tx, committed_at, 2)?;
                    tx.commit()?;
                    Ok(record)
                },
                storage_failure,
            )
            .with_context(|| format!("attach plugin root {installation_id}"))
    }

    /// Retire the current generation.
    ///
    /// New calls stop. Unresolved operations and their evidence receipts stay
    /// readable; the record states how many operations were still unresolved
    /// at that commit instead of claiming they stopped.
    pub fn retire_plugin_root(
        &self,
        installation_id: EntityId,
        request: &PluginRetireRequest,
    ) -> Result<PluginRootRecord> {
        self.ledger
            .with_mutation(
                || {
                    let mut conn = self.ledger.conn.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    ensure_plugin_root_schema(&tx)?;
                    let mut record = read_record(&tx, installation_id)?.ok_or_else(|| {
                        refusal(
                            PluginLifecycleRefusalCode::UnknownInstallation,
                            Some(installation_id),
                            None,
                            "retire needs an existing installation",
                        )
                    })?;
                    if record.phase == PluginRootPhase::Retired {
                        if let Some(existing) = read_admission(
                            &tx,
                            installation_id,
                            record.generation,
                            PluginRootPhase::Retired,
                            &request.runtime_generation,
                        )? {
                            ensure!(
                                existing.reason == request.reason,
                                refusal(
                                    PluginLifecycleRefusalCode::AdmissionConflict,
                                    Some(installation_id),
                                    Some(record.phase),
                                    "the same process generation retired with a different reason",
                                )
                            );
                            tx.commit()?;
                            return Ok(record);
                        }
                        return Err(refusal(
                            PluginLifecycleRefusalCode::Retired,
                            Some(installation_id),
                            Some(record.phase),
                            "this generation is already retired by another process generation",
                        ));
                    }
                    let unresolved = unresolved_operations(
                        &tx,
                        record.identity.plugin_id,
                        &record.identity.implementation.artifact.digest,
                    )?;
                    let artifact_retained = match &record.prepared.artifact {
                        PluginArtifactBinding::StoreCas { object } => {
                            self.cas.object_exists(&object.digest)
                        }
                        PluginArtifactBinding::InstallationUnit { retained } => {
                            request.artifact_retained && *retained
                        }
                    };
                    let committed_at =
                        CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                    let revision = record.record_revision + 1;
                    let artifact = record
                        .published
                        .as_ref()
                        .map(|published| published.artifact.clone())
                        .unwrap_or_else(|| record.prepared.artifact.clone());
                    let admission = make_admission(
                        AdmissionInput {
                            installation_id,
                            generation: record.generation,
                            phase: PluginRootPhase::Retired,
                            record_revision: revision,
                            runtime_generation: &request.runtime_generation,
                            observed_registry_revision: 0,
                            observed_capability_revision: 0,
                            observed_hook_graph_digest: None,
                            artifact,
                            reason: &request.reason,
                            evidence: &request.evidence,
                            unresolved_operations: Some(unresolved),
                            committed_at,
                        },
                    );
                    insert_admission(&tx, &admission)?;
                    record.phase = PluginRootPhase::Retired;
                    record.record_revision = revision;
                    record.retired = Some(admission);
                    record.artifact_retained = artifact_retained;
                    record.evidence_retained = true;
                    record.in_flight_operations_at_retirement = Some(unresolved);
                    record.updated_commit = committed_at;
                    write_record(&tx, &record)?;
                    self.record_subject_commit(&tx, committed_at, 2)?;
                    tx.commit()?;
                    Ok(record)
                },
                storage_failure,
            )
            .with_context(|| format!("retire plugin root {installation_id}"))
    }

    /// The durable record of one installation, when it exists.
    pub fn plugin_root_record(
        &self,
        installation_id: EntityId,
    ) -> Result<Option<PluginRootRecord>> {
        let conn = self.ledger.conn.lock();
        ensure_plugin_root_schema(&conn)?;
        Ok(read_record(&conn, installation_id)?)
    }

    /// Every installation record, optionally filtered by phase, newest first.
    pub fn plugin_root_records(
        &self,
        phase: Option<PluginRootPhase>,
        limit: usize,
    ) -> Result<Vec<PluginRootRecord>> {
        let conn = self.ledger.conn.lock();
        ensure_plugin_root_schema(&conn)?;
        let rows: Vec<String> = conn
            .prepare(
                "SELECT data FROM plugin_root_installations
                 WHERE (?1 IS NULL OR phase=?1)
                 ORDER BY updated_commit_seq DESC, installation_id LIMIT ?2",
            )?
            .query_map(params![phase.map(PluginRootPhase::name), limit as i64], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }

    /// The append-only phase admissions of one installation.
    pub fn plugin_root_admissions(
        &self,
        installation_id: EntityId,
    ) -> Result<Vec<PluginPhaseAdmission>> {
        let conn = self.ledger.conn.lock();
        ensure_plugin_root_schema(&conn)?;
        Ok(read_admissions(&conn, installation_id)?)
    }

    /// Whether this installation may serve new calls in the given process
    /// generation. Publication alone is not enough; it must be attached by the
    /// current generation and not retired.
    pub fn plugin_root_admits_new_calls(
        &self,
        installation_id: EntityId,
        current: &RuntimeGeneration,
    ) -> Result<bool> {
        Ok(self
            .plugin_root_record(installation_id)?
            .is_some_and(|record| record.admits_new_calls(current)))
    }

    /// Crash/restart recovery derived only from the committed phase.
    ///
    /// The plan names the operation that is still allowed. It never republishes
    /// a committed publication, never assumes an in-flight call finished, and
    /// never treats a retained artifact as a loaded executable.
    pub fn plugin_root_recovery_plan(
        &self,
        installation_id: EntityId,
        current: &RuntimeGeneration,
    ) -> Result<Option<PluginRecoveryPlan>> {
        let conn = self.ledger.conn.lock();
        ensure_plugin_root_schema(&conn)?;
        let Some(record) = read_record(&conn, installation_id)? else {
            return Ok(None);
        };
        let unresolved = unresolved_operations(
            &conn,
            record.identity.plugin_id,
            &record.identity.implementation.artifact.digest,
        )?;
        let (action, admits_new_calls) = match record.phase {
            PluginRootPhase::Prepared => (PluginRecoveryAction::Publish, false),
            PluginRootPhase::Published => (PluginRecoveryAction::Attach, false),
            PluginRootPhase::Attached => match record.attached_runtime_generation.clone() {
                Some(previous) if previous == *current => (PluginRecoveryAction::Ready, true),
                Some(previous) => (
                    PluginRecoveryAction::Reattach {
                        previous_runtime_generation: previous,
                    },
                    false,
                ),
                // An attached record always has its attachment generation; a
                // missing one is a corrupt record, not a state to guess about.
                None => {
                    return Err(ShenkoError::storage(
                        "attached plugin-root record has no attachment generation",
                    )
                    .into());
                }
            },
            PluginRootPhase::Retired => (PluginRecoveryAction::Retained, false),
        };
        Ok(Some(PluginRecoveryPlan {
            schema_version: 1,
            installation_id,
            generation: record.generation,
            phase: record.phase,
            current_runtime_generation: current.clone(),
            action,
            admits_new_calls,
            unresolved_operations: unresolved,
            artifact_retained: record.artifact_retained,
            evidence_retained: record.evidence_retained,
        }))
    }

    /// Resolve one frozen operation against the durable lifecycle.
    ///
    /// The operation's own contract reference is the identity: the call keeps
    /// naming the plugin and artifact it was frozen against. Retirement does
    /// not delete that call or its receipts; it only removes the live
    /// attachment.
    pub fn resolve_operation_plugin(
        &self,
        operation_id: EntityId,
        current: &RuntimeGeneration,
    ) -> Result<Option<PluginCallResolution>> {
        let conn = self.ledger.conn.lock();
        ensure_plugin_root_schema(&conn)?;
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM operations WHERE operation_id=?1",
                [operation_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(data) = data else { return Ok(None) };
        let operation: serde_json::Value = serde_json::from_str(&data)?;
        let plugin_id: EntityId = operation
            .get("contract_ref")
            .and_then(|contract| contract.get("plugin_id"))
            .and_then(serde_json::Value::as_str)
            .context("operation has no frozen contract plugin identity")?
            .parse()?;
        let artifact_digest = operation
            .get("contract_ref")
            .and_then(|contract| contract.get("artifact_digest"))
            .and_then(serde_json::Value::as_str)
            .context("operation has no frozen contract artifact identity")?
            .to_owned();
        let receipt_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM evidence_receipts WHERE operation_id=?1",
                [operation_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let rows: Vec<String> = conn
            .prepare(
                "SELECT data FROM plugin_root_installations
                 WHERE plugin_id=?1 AND artifact_digest=?2
                 ORDER BY updated_commit_seq DESC, installation_id",
            )?
            .query_map(params![plugin_id.to_string(), artifact_digest.as_str()], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let record: PluginRootRecord = serde_json::from_str(&row)?;
            if record.identity.installation_id() != record.installation_id {
                return Err(ShenkoError::storage(
                    "plugin-root record identity does not match its committed installation",
                )
                .into());
            }
            records.push(record);
        }
        // A new generation replaces the installation row's current view, so a
        // previous generation's retirement is read back from its append-only
        // admission. Without this, reopening an installation would make an old
        // frozen call look as if the artifact had never been retained.
        let retirement: Option<PluginPhaseAdmission> = conn
            .query_row(
                "SELECT a.data FROM plugin_root_admissions a
                 JOIN plugin_root_installations i ON i.installation_id=a.installation_id
                 WHERE i.plugin_id=?1 AND i.artifact_digest=?2 AND a.phase='retired'
                 ORDER BY a.committed_seq DESC LIMIT 1",
                params![plugin_id.to_string(), artifact_digest.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|data| serde_json::from_str(&data))
            .transpose()?;
        // Prefer the attachment the current process generation owns, then any
        // attachment, then a bare publication, then retirement, then a
        // prepared-only installation. A retained artifact is never reported
        // as live, and a prepared installation is never reported as attached.
        let availability = records
            .iter()
            .find_map(|record| {
                (record.phase == PluginRootPhase::Attached
                    && record.attached_runtime_generation.as_ref() == Some(current))
                .then(|| PluginImplementationAvailability::Live {
                    runtime_generation: current.clone(),
                })
            })
            .or_else(|| {
                records.iter().find_map(|record| match record.phase {
                    PluginRootPhase::Attached => record.attached_runtime_generation.clone().map(
                        |previous_runtime_generation| {
                            PluginImplementationAvailability::AwaitingReattach {
                                previous_runtime_generation,
                            }
                        },
                    ),
                    _ => None,
                })
            })
            .or_else(|| {
                records
                    .iter()
                    .find(|record| record.phase == PluginRootPhase::Published)
                    .map(|_| PluginImplementationAvailability::AwaitingAttach)
            })
            .or_else(|| {
                records.iter().find_map(|record| {
                    (record.phase == PluginRootPhase::Retired).then(|| {
                        PluginImplementationAvailability::RetainedOnly {
                            retired_commit: record
                                .retired
                                .as_ref()
                                .map(|retired| retired.committed_at)
                                .unwrap_or(record.updated_commit),
                        }
                    })
                })
            })
            .or_else(|| {
                retirement
                    .as_ref()
                    .map(|retired| PluginImplementationAvailability::RetainedOnly {
                        retired_commit: retired.committed_at,
                    })
            })
            .or_else(|| {
                records
                    .iter()
                    .any(|record| record.phase == PluginRootPhase::Prepared)
                    .then_some(PluginImplementationAvailability::NotPublished)
            })
            .unwrap_or(PluginImplementationAvailability::Missing);
        Ok(Some(PluginCallResolution {
            operation_id,
            plugin_id,
            artifact_digest: Digest(artifact_digest),
            availability,
            receipts_readable: true,
            receipt_count: u64::try_from(receipt_count).unwrap_or(0),
        }))
    }
}

// The tests below are UNRUN. They are the packet's acceptance fixtures for the
// phase machine, not evidence that this candidate compiled or behaved.
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use shenko_types::object::{ArtifactRef, ObjectRef, SchemaRef, VersionedRef};

    fn fixture() -> Result<(tempfile::TempDir, Store, PluginRootIdentity, ObjectRef)> {
        let dir = tempfile::tempdir()?;
        let store = Store::open(dir.path().join("shenko.db"), dir.path().join("cas"))?;
        let object = store
            .cas
            .seal_object(
                b"fixture implementation bytes",
                "application/octet-stream",
                "test",
                "raw",
                1,
            )?
            .object;
        let identity = PluginRootIdentity {
            unit_id: "test.unit".into(),
            unit_revision: "1".into(),
            unit_artifact_digest: Digest::blake3(b"test unit package"),
            plugin_id: EntityId::new(),
            implementation: VersionedRef {
                artifact: ArtifactRef {
                    digest: object.digest.clone(),
                    locator: object.locator.clone(),
                },
                symbol: "test.handler".into(),
                schema: SchemaRef {
                    name: "test-schema".into(),
                    major: 1,
                    digest: Digest::blake3(b"test-schema"),
                },
            },
            provider_ids: vec!["test.provider.a".into(), "test.provider.b".into()],
            capability_ids: vec!["test.capability".into()],
        };
        Ok((dir, store, identity, object))
    }

    fn prepare(
        store: &Store,
        identity: &PluginRootIdentity,
        object: &ObjectRef,
        runtime: &RuntimeGeneration,
    ) -> Result<PluginRootRecord> {
        store.prepare_plugin_root(
            identity,
            &PluginPrepareRequest {
                runtime_generation: runtime.clone(),
                artifact: PluginArtifactBinding::StoreCas {
                    object: object.clone(),
                },
                reason: "fixture prepare".into(),
                evidence: vec![],
            },
        )
    }

    fn publish(
        store: &Store,
        installation_id: EntityId,
        runtime: &RuntimeGeneration,
    ) -> Result<PluginRootRecord> {
        store.publish_plugin_root(
            installation_id,
            &PluginPublishRequest {
                runtime_generation: runtime.clone(),
                observed_registry_revision: 7,
                observed_capability_revision: 9,
                observed_hook_graph_digest: Some(Digest::blake3(b"fixture hook graph")),
                reason: "fixture publish".into(),
                evidence: vec![],
            },
        )
    }

    fn attach(
        store: &Store,
        installation_id: EntityId,
        runtime: &RuntimeGeneration,
        registry_revision: u64,
    ) -> Result<PluginRootRecord> {
        store.attach_plugin_root(
            installation_id,
            &PluginAttachRequest {
                runtime_generation: runtime.clone(),
                observed_registry_revision: registry_revision,
                observed_capability_revision: registry_revision,
                reason: "fixture attach".into(),
                evidence: vec![],
            },
        )
    }

    fn refusal(error: &anyhow::Error) -> &PluginLifecycleRefusal {
        error
            .downcast_ref::<PluginLifecycleRefusal>()
            .expect("lifecycle refusal is typed")
    }

    #[test]
    fn attach_before_publish_is_refused_and_leaves_prepared_intact() -> Result<()> {
        let (_dir, store, identity, object) = fixture()?;
        let installation_id = identity.installation_id();
        let first = RuntimeGeneration::new("process-1");
        let prepared = prepare(&store, &identity, &object, &first)?;
        assert_eq!(prepared.phase, PluginRootPhase::Prepared);
        assert_eq!(prepared.record_revision, 0);

        let error = attach(&store, installation_id, &first, 1).unwrap_err();
        assert_eq!(
            refusal(&error).code,
            PluginLifecycleRefusalCode::AttachBeforePublish
        );

        let current = store
            .plugin_root_record(installation_id)?
            .expect("prepared record");
        assert_eq!(current.phase, PluginRootPhase::Prepared);
        assert_eq!(current.record_revision, prepared.record_revision);
        assert_eq!(store.plugin_root_admissions(installation_id)?.len(), 1);
        assert!(!store.plugin_root_admits_new_calls(installation_id, &first)?);
        assert_eq!(
            store
                .plugin_root_recovery_plan(installation_id, &first)?
                .expect("plan")
                .action,
            PluginRecoveryAction::Publish
        );
        Ok(())
    }

    #[test]
    fn publish_attach_reattach_keep_one_admission_per_process_generation() -> Result<()> {
        let (_dir, store, identity, object) = fixture()?;
        let installation_id = identity.installation_id();
        let first = RuntimeGeneration::new("process-1");
        prepare(&store, &identity, &object, &first)?;

        let published = publish(&store, installation_id, &first)?;
        assert_eq!(published.phase, PluginRootPhase::Published);
        let republished = publish(&store, installation_id, &first)?;
        assert_eq!(
            republished.record_revision, published.record_revision,
            "publication replay must not append a second publication"
        );

        let attached = attach(&store, installation_id, &first, 10)?;
        assert_eq!(attached.phase, PluginRootPhase::Attached);
        assert_eq!(
            attached.attached_runtime_generation.as_ref(),
            Some(&first)
        );
        assert!(store.plugin_root_admits_new_calls(installation_id, &first)?);

        let replay = attach(&store, installation_id, &first, 10)?;
        assert_eq!(replay.record_revision, attached.record_revision);
        let conflicting = attach(&store, installation_id, &first, 11).unwrap_err();
        assert_eq!(
            refusal(&conflicting).code,
            PluginLifecycleRefusalCode::AdmissionConflict
        );

        let second = RuntimeGeneration::new("process-2");
        let restarted = attach(&store, installation_id, &second, 12)?;
        assert!(restarted.record_revision > attached.record_revision);
        assert_eq!(
            restarted.attached_runtime_generation.as_ref(),
            Some(&second)
        );
        assert!(!store.plugin_root_admits_new_calls(installation_id, &first)?);
        assert!(store.plugin_root_admits_new_calls(installation_id, &second)?);

        let plan = store
            .plugin_root_recovery_plan(installation_id, &second)?
            .expect("plan");
        assert_eq!(plan.action, PluginRecoveryAction::Ready);
        assert!(plan.admits_new_calls);
        let dead = RuntimeGeneration::new("process-3");
        let plan = store
            .plugin_root_recovery_plan(installation_id, &dead)?
            .expect("plan");
        assert_eq!(
            plan.action,
            PluginRecoveryAction::Reattach {
                previous_runtime_generation: second.clone()
            }
        );
        assert!(!plan.admits_new_calls);

        let admissions = store.plugin_root_admissions(installation_id)?;
        assert_eq!(
            admissions
                .iter()
                .filter(|admission| admission.phase == PluginRootPhase::Attached)
                .count(),
            2,
            "each live process generation keeps its own attachment receipt"
        );
        Ok(())
    }

    #[test]
    fn retirement_stops_new_calls_and_keeps_old_operation_receipts() -> Result<()> {
        let (_dir, store, identity, object) = fixture()?;
        let installation_id = identity.installation_id();
        let first = RuntimeGeneration::new("process-1");
        prepare(&store, &identity, &object, &first)?;
        publish(&store, installation_id, &first)?;
        attach(&store, installation_id, &first, 10)?;

        let operation_id = EntityId::new();
        {
            let conn = store.ledger.conn.lock();
            conn.execute(
                "INSERT INTO operations(operation_id, record_revision, data, updated_commit_seq)
                 VALUES(?1,0,?2,0)",
                params![
                    operation_id.to_string(),
                    serde_json::json!({
                        "contract_ref": {
                            "plugin_id": identity.plugin_id.to_string(),
                            "artifact_digest": identity.implementation.artifact.digest.0.clone(),
                        },
                        "state": "in_flight",
                    })
                    .to_string()
                ],
            )?;
            conn.execute(
                "INSERT INTO evidence_receipts(receipt_id, operation_id, data, updated_commit_seq)
                 VALUES(?1,?2,?3,0)",
                params![
                    EntityId::new().to_string(),
                    operation_id.to_string(),
                    "{\"fixture\":\"retained receipt\"}"
                ],
            )?;
        }

        let retired = store.retire_plugin_root(
            installation_id,
            &PluginRetireRequest {
                runtime_generation: first.clone(),
                reason: "fixture retirement".into(),
                artifact_retained: true,
                evidence: vec![],
            },
        )?;
        assert_eq!(retired.phase, PluginRootPhase::Retired);
        assert_eq!(retired.in_flight_operations_at_retirement, Some(1));
        assert!(!store.plugin_root_admits_new_calls(installation_id, &first)?);

        let replay = store.retire_plugin_root(
            installation_id,
            &PluginRetireRequest {
                runtime_generation: first.clone(),
                reason: "fixture retirement".into(),
                artifact_retained: true,
                evidence: vec![],
            },
        )?;
        assert_eq!(replay.record_revision, retired.record_revision);
        let attach_refused = attach(&store, installation_id, &first, 10).unwrap_err();
        assert_eq!(
            refusal(&attach_refused).code,
            PluginLifecycleRefusalCode::Retired
        );
        let publish_refused = publish(&store, installation_id, &first).unwrap_err();
        assert_eq!(
            refusal(&publish_refused).code,
            PluginLifecycleRefusalCode::Retired
        );

        let retired_admission = store
            .plugin_root_admissions(installation_id)?
            .into_iter()
            .find(|admission| admission.phase == PluginRootPhase::Retired)
            .expect("retirement admission");
        assert_eq!(retired_admission.unresolved_operations, Some(1));

        let resolution = store
            .resolve_operation_plugin(operation_id, &first)?
            .expect("frozen operation");
        assert!(matches!(
            resolution.availability,
            PluginImplementationAvailability::RetainedOnly { .. }
        ));
        assert!(resolution.receipts_readable);
        assert_eq!(resolution.receipt_count, 1);

        let plan = store
            .plugin_root_recovery_plan(installation_id, &first)?
            .expect("plan");
        assert_eq!(plan.action, PluginRecoveryAction::Retained);
        assert_eq!(plan.unresolved_operations, 1);
        assert!(!plan.admits_new_calls);

        // Re-enabling the same exact installation opens the next generation.
        // The old retirement admission stays immutable and the old call still
        // resolves to the retained artifact instead of disappearing.
        let reopened = prepare(&store, &identity, &object, &first)?;
        assert_eq!(reopened.generation, retired.generation + 1);
        assert_eq!(reopened.phase, PluginRootPhase::Prepared);
        let after_reopen = store
            .resolve_operation_plugin(operation_id, &first)?
            .expect("frozen operation");
        assert!(matches!(
            after_reopen.availability,
            PluginImplementationAvailability::RetainedOnly { .. }
        ));
        assert_eq!(after_reopen.receipt_count, 1);
        Ok(())
    }

    #[test]
    fn installation_identity_is_order_stable_and_strictly_immutable() -> Result<()> {
        let (_dir, store, identity, object) = fixture()?;
        let first = RuntimeGeneration::new("process-1");
        let mut reordered = identity.clone();
        reordered.provider_ids.reverse();
        assert_eq!(
            reordered.installation_id(),
            identity.installation_id(),
            "observation order is not installation identity"
        );
        prepare(&store, &identity, &object, &first)?;
        let error = prepare(&store, &reordered, &object, &first).unwrap_err();
        assert_eq!(
            refusal(&error).code,
            PluginLifecycleRefusalCode::IdentityConflict
        );
        Ok(())
    }
}
