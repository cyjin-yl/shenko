//! Consistent recovery snapshots, archive streaming and content fingerprints.
//!
//! Public snapshot values remain owned by `cognition`; collection freezes one
//! SQLite view and archive writing verifies the originally selected payloads.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior};

use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::Digest;

use crate::cognition_archive::{
    CognitionDerivative, CognitionFileDescriptor, CognitionFileOrigin, CognitionGap,
    CognitionGitDescriptor, CognitionManifest, CognitionObjectDescriptor, CognitionRow,
    write_manifest, write_payload,
};
use crate::store::{Store, StorePublication};

use super::admission::{
    archive_membership_gaps, is_local_control_class, local_fact_classes, memory_intent_binding_gaps,
};
use super::files::{capture_git_history, collect_files, file_origin_name};
use super::rows::{TABLES, read_derivatives, read_table};
use super::{CognitionExport, CognitionReceipt, CognitionSnapshotOptions, db};

/// Everything one snapshot consists of.
struct Collected {
    rows: Vec<CognitionRow>,
    derivatives: Vec<CognitionDerivative>,
    objects: Vec<CognitionObjectDescriptor>,
    files: Vec<(CognitionFileDescriptor, PathBuf)>,
    file_origins: Vec<CognitionFileOrigin>,
    git_history: Option<CognitionGitDescriptor>,
    git_bundle: Option<Vec<u8>>,
    gaps: Vec<CognitionGap>,
    agent_id: Option<EntityId>,
    high_water: LedgerSeq,
    snapshot_seq: Option<u64>,
}

impl CognitionExport {
    /// Stream this snapshot as an archive.
    ///
    /// The snapshot is not rebuilt: the manifest, the fingerprint and the
    /// closure were computed once, and every payload is verified against the
    /// manifest while it is written. A payload that changed in between stops
    /// the transfer instead of shipping bytes that do not match the
    /// fingerprint.
    pub fn write_archive<W: Write>(&self, mut writer: W) -> Result<CognitionReceipt> {
        write_manifest(&mut writer, &self.manifest)?;
        for (index, descriptor) in self.manifest.objects.iter().enumerate() {
            let bytes = self.cas.read_object(&descriptor.digest)?;
            ensure!(
                bytes.len() as u64 == descriptor.bytes
                    && Digest::blake3(&bytes) == descriptor.digest,
                "the exported object {} no longer matches the snapshot",
                descriptor.digest
            );
            write_payload(
                &mut writer,
                "object",
                &[index.to_string(), bytes.len().to_string()],
                &bytes,
            )?;
        }
        for (index, (descriptor, path)) in self.files.iter().enumerate() {
            let bytes = fs::read(path)
                .with_context(|| format!("read the exported file {}", path.display()))?;
            ensure!(
                Digest::blake3(&bytes) == descriptor.digest,
                "the exported file {} changed after the snapshot was taken",
                descriptor.path
            );
            write_payload(
                &mut writer,
                "file",
                &[index.to_string(), bytes.len().to_string()],
                &bytes,
            )?;
        }
        if let (Some(descriptor), Some(bytes)) =
            (self.manifest.git_history.as_ref(), self.git_bundle.as_ref())
        {
            ensure!(
                bytes.len() as u64 == descriptor.bytes
                    && Digest::blake3(&bytes) == descriptor.digest,
                "the exported memory history no longer matches the snapshot"
            );
            write_payload(&mut writer, "git", &[bytes.len().to_string()], bytes)?;
        }
        writer.flush()?;
        Ok(CognitionReceipt {
            fingerprint: self.manifest.fingerprint.clone(),
            high_water: self.manifest.high_water,
            rows: self.manifest.rows.len(),
            objects: self.manifest.objects.len(),
            files: self.manifest.files.len(),
            gaps: self.manifest.gaps.clone(),
            complete: self.manifest.blocking_gaps().is_empty(),
            exported_at: self.manifest.created_at,
        })
    }
}


impl Store {
    /// Export the recovery snapshot of this store.
    ///
    /// The facts come from one consistent read snapshot. The CAS closure is
    /// verified object by object, recursively: every referenced object must
    /// exist and must hash to its digest, otherwise the snapshot is
    /// incomplete and cannot be claimed recoverable.
    pub fn export_cognition(&self, options: &CognitionSnapshotOptions) -> Result<CognitionExport> {
        let publication = Arc::new(crate::timing::measure("snapshot.cas_publication_wait", || self.ledger.acquire_cas_publication())?);
        self.export_cognition_owned(options, publication, None)
    }

    /// The caller acquires publication before its original physical permit.
    /// The result shares that lease until the last archive read and Drop.
    pub fn export_cognition_under(&self, options: &CognitionSnapshotOptions,
        publication: &StorePublication,
        permit: Option<&shenko_types::installation::InstallationPermit>) -> Result<CognitionExport> {
        let publication = publication.retain_cas(&self.cas)?;
        self.export_cognition_owned(options, publication, permit)
    }

    fn export_cognition_owned(&self, options: &CognitionSnapshotOptions,
        publication: Arc<crate::roots::CasWriteLease>,
        permit: Option<&shenko_types::installation::InstallationPermit>) -> Result<CognitionExport> {
        let collected = self.collect_snapshot(options, true, permit)?;
        let fingerprint = fingerprint_of(&collected);
        let source_snapshot_seq = collected
            .snapshot_seq
            .ok_or_else(|| anyhow!("cognition export did not allocate its durable order"))?;
        let files: Vec<CognitionFileDescriptor> = collected
            .files
            .iter()
            .map(|(descriptor, _)| descriptor.clone())
            .collect();
        let git_history = collected.git_history.clone();
        let manifest = CognitionManifest {
            schema: crate::cognition_archive::ARCHIVE_SCHEMA,
            fact_classes: local_fact_classes(options.local_boundary_preserved),
            fingerprint,
            source_ledger_id: self.ledger.ledger_id(),
            source_incarnation: self.capabilities.incarnation,
            source_snapshot_seq,
            agent_id: collected.agent_id,
            created_at: chrono::Utc::now(),
            high_water: collected.high_water,
            rows: collected.rows,
            derivatives: collected.derivatives,
            objects: collected.objects,
            file_origins: collected.file_origins,
            files,
            git_history,
            gaps: collected.gaps,
        };
        Ok(CognitionExport {
            manifest,
            cas: Arc::clone(&self.cas),
            files: collected.files,
            git_bundle: collected.git_bundle,
            _publication: publication,
        })
    }

    /// Write the recovery snapshot as a streamable archive.
    ///
    /// When a peer is given, the export is recorded for that peer: the peer
    /// now holds this snapshot, but it is not yet verified recoverable.
    pub fn write_cognition_archive<W: Write>(
        &self,
        options: &CognitionSnapshotOptions,
        peer_id: Option<EntityId>,
        mut writer: W,
    ) -> Result<CognitionReceipt> {
        let export = self.export_cognition(options)?;
        let receipt = export.write_archive(&mut writer)?;
        if let Some(peer_id) = peer_id {
            self.note_cognition_export(peer_id, &receipt)?;
        }
        Ok(receipt)
    }

    /// Fingerprint of the snapshot this store currently holds.
    ///
    /// The fingerprint covers the replicated facts, the verified object
    /// closure and the memory files, so a memory or identity change is
    /// visible even when the ledger sequence did not move.
    pub fn cognition_fingerprint(
        &self,
        options: &CognitionSnapshotOptions,
    ) -> Result<(Digest, LedgerSeq)> {
        let _publication = self.ledger.acquire_cas_publication()?;
        let collected = self.collect_snapshot(options, false, None)?;
        let fingerprint = fingerprint_of(&collected);
        let high_water = collected.high_water;
        self.store_local_fingerprint(&fingerprint, high_water)?;
        Ok((fingerprint, high_water))
    }

    fn collect_snapshot(
        &self,
        options: &CognitionSnapshotOptions,
        allocate_order: bool,
        permit: Option<&shenko_types::installation::InstallationPermit>,
    ) -> Result<Collected> {
        let (rows, derivatives, agent_id, high_water, snapshot_seq) =
            crate::timing::measure("snapshot.rows", || self.snapshot_rows(allocate_order, options.local_boundary_preserved, permit))?;
        let (roots, external) = self.cognition_object_roots(&rows);
        let (objects, missing) = crate::timing::measure("snapshot.closure", || self.collect_closure(&roots))?;

        let mut files: Vec<(CognitionFileDescriptor, PathBuf)> = Vec::new();
        let retired_roots = {
            let conn = self.ledger.conn.lock();
            let mut stmt = conn.prepare("SELECT path FROM cognition_retirements")?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.into_iter().map(PathBuf::from).collect::<BTreeSet<_>>()
        };
        let mut file_origins = Vec::new();
        let _validation = crate::timing::Phase::new("snapshot.validate_facts");
        let mut gaps = memory_intent_binding_gaps(&rows);
        gaps.extend(crate::media::media_recovery_gaps(&rows));
        // The REQUIRED-fact closure: a member row whose anchor is missing
        // means this store does not hold a complete set of facts, and the
        // export must say so instead of shipping an archive that claims
        // recoverability.
        gaps.extend(archive_membership_gaps(&rows));
        if let Err(error) = self.validate_obligation_rows(&rows) {
            gaps.push(CognitionGap { kind: "obligation_required_fact".into(), detail: error.to_string(), blocking: true });
        }
        drop(_validation);
        if let Some(root) = &options.memory_root {
            file_origins.push(CognitionFileOrigin::Memory);
            collect_files(
                root,
                CognitionFileOrigin::Memory,
                &retired_roots,
                &mut files,
                &mut gaps,
            )?;
        }
        if !options.workspace_roots.is_empty() {
            file_origins.push(CognitionFileOrigin::Workspace);
            for root in &options.workspace_roots {
                collect_files(
                    root,
                    CognitionFileOrigin::Workspace,
                    &retired_roots,
                    &mut files,
                    &mut gaps,
                )?;
            }
        }
        file_origins.sort_unstable();
        let mut git_history: Option<CognitionGitDescriptor> = None;
        let mut git_bundle: Option<Vec<u8>> = None;
        if options.include_git_history {
            if let Some(root) = &options.memory_root {
                match capture_git_history(root) {
                    Ok(Some((descriptor, bytes))) => {
                        git_bundle = Some(bytes);
                        git_history = Some(descriptor);
                    }
                    Ok(None) => {}
                    Err(error) => gaps.push(CognitionGap {
                        kind: "git_history".into(),
                        detail: error.to_string(),
                        blocking: true,
                    }),
                }
            }
        }
        for digest in missing {
            gaps.push(CognitionGap {
                kind: "missing_object".into(),
                detail: digest.to_string(),
                blocking: true,
            });
        }
        for digest in &external {
            if !self.cas.object_exists(digest) {
                gaps.push(CognitionGap {
                    kind: "external_artifact".into(),
                    detail: digest.to_string(),
                    blocking: false,
                });
            }
        }
        Ok(Collected {
            rows,
            derivatives,
            objects,
            files,
            file_origins,
            git_history,
            git_bundle,
            gaps,
            agent_id,
            high_water,
            snapshot_seq,
        })
    }

    /// One consistent read snapshot of the replicated facts.
    fn snapshot_rows(
        &self,
        allocate_order: bool,
        local_boundary_preserved: bool,
        borrowed: Option<&shenko_types::installation::InstallationPermit>,
    ) -> Result<(
        Vec<CognitionRow>,
        Vec<CognitionDerivative>,
        Option<EntityId>,
        LedgerSeq,
        Option<u64>,
    )> {
        let owned = if allocate_order && borrowed.is_none() {
            crate::timing::measure("snapshot.installation_wait", || self.ledger.acquire_installation(false))?
        } else { None };
        if allocate_order && let Some((domain, _, _)) = &self.ledger.installation {
            borrowed.or(owned.as_ref()).context("snapshot order requires an installation permit")?.validate(domain)?;
        }
        let mut conn = crate::timing::measure("sqlite.mutex_wait", || self.ledger.conn.lock());
        let behavior = if allocate_order {
            TransactionBehavior::Immediate
        } else {
            TransactionBehavior::Deferred
        };
        let _begin = crate::timing::Phase::new(if allocate_order { "sqlite.begin_immediate" } else { "sqlite.begin_deferred" });
        let tx = conn.transaction_with_behavior(behavior).map_err(db)?;
        drop(_begin);
        // The subject boundary request is local controller state, not part of
        // this archive schema. A paused head without its request would strand
        // a peer receiver and must never be acknowledged as a complete
        // snapshot. The one exception is the documented same-installation
        // handover: the caller opts in only when the destination installs
        // the complete paused boundary separately before applying.
        let paused: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM subject_branch_heads WHERE mode='paused')",
            [],
            |row| row.get(0),
        ).map_err(db)?;
        ensure!(!paused || local_boundary_preserved, "cognition snapshot unavailable while a local subject boundary is paused; preserve the complete store or resume before exporting");
        let snapshot_seq = if allocate_order {
            let current: Option<i64> = tx
                .query_row(
                    "SELECT sequence FROM cognition_export_order WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db)?;
            let next = current.unwrap_or(0).checked_add(1).ok_or_else(|| {
                anyhow!("cognition export order exhausted its durable integer range")
            })?;
            tx.execute(
                "INSERT INTO cognition_export_order(singleton,sequence) VALUES(1,?1)
                 ON CONFLICT(singleton) DO UPDATE SET sequence=excluded.sequence",
                [next],
            )
            .map_err(db)?;
            Some(u64::try_from(next)?)
        } else {
            None
        };
        let mut rows: Vec<CognitionRow> = Vec::new();
        for spec in TABLES {
            if !local_boundary_preserved && is_local_control_class(spec.name) {
                continue;
            }
            rows.extend(read_table(&tx, spec)?);
        }
        let derivatives = read_derivatives(&tx)?;
        // Replicated genesis rows stay history; the export is labeled by the
        // subject this installation actually owns, never an arbitrary row.
        let agent_id: Option<EntityId> =
            crate::store::read_local_subject_id(&tx, self.ledger.ledger_id())?;
        let high_water: i64 = tx
            .query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| {
                row.get(0)
            })
            .map_err(db)?;
        crate::timing::measure("sqlite.commit", || tx.commit()).map_err(db)?;
        Ok((
            rows,
            derivatives,
            agent_id,
            LedgerSeq(high_water as u64),
            snapshot_seq,
        ))
    }
}

/// Content fingerprint of a snapshot.
///
/// The fingerprint covers the replicated facts, the verified object closure
/// and the memory content. It deliberately does not cover the ledger
/// sequence or the export time: a snapshot that carries the same content is
/// the same snapshot, so recording an acknowledgement can never create new
/// debt and an identity or memory change is visible even when the ledger
/// sequence did not move.
fn fingerprint_of(collected: &Collected) -> Digest {
    fingerprint_parts(
        &collected.rows,
        &collected.objects,
        collected.files.iter().map(|(descriptor, _)| descriptor),
        &collected.file_origins,
        collected.git_history.as_ref(),
    )
}

pub(super) fn fingerprint_of_manifest(manifest: &CognitionManifest) -> Digest {
    fingerprint_parts(
        &manifest.rows,
        &manifest.objects,
        manifest.files.iter(),
        &manifest.file_origins,
        manifest.git_history.as_ref(),
    )
}

fn fingerprint_parts<'a>(
    rows: &[CognitionRow],
    objects: &[CognitionObjectDescriptor],
    files: impl IntoIterator<Item = &'a CognitionFileDescriptor>,
    file_origins: &[CognitionFileOrigin],
    git_history: Option<&CognitionGitDescriptor>,
) -> Digest {
    let mut buffer: Vec<u8> = Vec::new();
    buffer.extend_from_slice(b"shenko-cognition-fingerprint/2\n");
    let mut rows: Vec<&CognitionRow> = rows.iter().collect();
    rows.sort_by(|a, b| a.table.cmp(&b.table).then(a.key.cmp(&b.key)));
    for row in rows {
        buffer.extend_from_slice(b"row\t");
        buffer.extend_from_slice(row.table.as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(row.key.as_bytes());
        for (name, value) in &row.values {
            buffer.push(b'\t');
            buffer.extend_from_slice(name.as_bytes());
            buffer.push(b'=');
            if let Some(value) = value {
                buffer.extend_from_slice(value.as_bytes());
            }
        }
        buffer.push(b'\n');
    }
    let mut objects: Vec<&CognitionObjectDescriptor> = objects.iter().collect();
    objects.sort_by(|a, b| a.digest.0.cmp(&b.digest.0));
    for descriptor in objects {
        buffer.extend_from_slice(b"object\t");
        buffer.extend_from_slice(descriptor.digest.0.as_bytes());
        buffer.push(b'\n');
    }
    let mut file_origins = file_origins.to_vec();
    file_origins.sort_unstable();
    for origin in file_origins {
        buffer.extend_from_slice(b"file-origin\t");
        buffer.extend_from_slice(file_origin_name(origin).as_bytes());
        buffer.push(b'\n');
    }
    let mut files: Vec<&CognitionFileDescriptor> = files.into_iter().collect();
    files.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(file_origin_name(a.origin).cmp(file_origin_name(b.origin)))
            .then(a.digest.0.cmp(&b.digest.0))
    });
    for descriptor in files {
        buffer.extend_from_slice(b"file\t");
        buffer.extend_from_slice(file_origin_name(descriptor.origin).as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(descriptor.path.as_bytes());
        buffer.push(b'\t');
        buffer.extend_from_slice(descriptor.digest.0.as_bytes());
        buffer.push(b'\n');
    }
    if let Some(descriptor) = git_history {
        buffer.extend_from_slice(b"git-head\t");
        buffer.extend_from_slice(descriptor.head.as_bytes());
        buffer.push(b'\n');
        let mut refs: Vec<&(String, String)> = descriptor.refs.iter().collect();
        refs.sort();
        for (name, sha) in refs {
            buffer.extend_from_slice(b"git\t");
            buffer.extend_from_slice(name.as_bytes());
            buffer.push(b'\t');
            buffer.extend_from_slice(sha.as_bytes());
            buffer.push(b'\n');
        }
    }
    Digest::blake3(&buffer)
}
