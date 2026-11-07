//! Same-reader state migration through Store's declared recovery archive.
//!
//! The Store owns mandatory-fact classes, reference decoding, tombstones and
//! atomic task/effect conflict units. SQLite table names are not a migration
//! contract; unknown formats require an actual versioned migrator/reader.
//!
//! An archive is only a *snapshot*. A hand-over whose destination becomes the
//! selected generation must also prove the snapshot covers the source it
//! copied: a fact the source committed after the snapshot would otherwise be
//! dropped from the new authority without any conflict being reported. The
//! source watermark is therefore observed on both sides of the capture and
//! reported in every [`ImportReport`]; `import_rollback_delta` turns an
//! uncovered source into a refusal, while the forward delta keeps the same
//! observation visible without changing its retry behaviour.
use std::path::Path;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use shenko_store::{Store, cognition::{CognitionImportOptions, CognitionSnapshotOptions}};
use shenko_types::{commit::CommitRef, ids::{EntityId, LedgerSeq}};

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("typed state migration unavailable: {0:#}")]
    Contract(#[from] anyhow::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableImport {
    pub table: String,
    pub rows_compared: u64,
    pub rows_written: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub tables: Vec<TableImport>,
    pub commits_imported: u64,
    pub watermark: Option<CommitRef>,
    /// The source's own committed high water, observed before the snapshot
    /// was captured: the watermark this hand-over actually starts from.
    #[serde(default)]
    pub source_watermark_before: Option<CommitRef>,
    /// The source's committed high water observed after the snapshot was
    /// applied. A value above `watermark` names source facts the archive did
    /// not carry.
    #[serde(default)]
    pub source_watermark_after: Option<CommitRef>,
    /// Source commits observed beyond the archive's high water at the moment
    /// this call finished.
    #[serde(default)]
    pub uncovered_source_commits: u64,
    /// The archive this call retained beside the destination. It names a
    /// complete snapshot; an export that could not be completed keeps its
    /// staging name instead, so nothing claims a whole archive it is not.
    #[serde(default)]
    pub archive_path: Option<String>,
    pub blocked_units: Vec<String>,
}
impl ImportReport {
    pub fn rows_written(&self) -> u64 { self.tables.iter().map(|table| table.rows_written).sum() }
}

/// What an uncovered source means for this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceCoverage {
    /// The destination does not become the selected generation yet (the
    /// forward delta runs while the live subject is still committing): an
    /// uncovered source is reported, never invented into a success, and the
    /// existing retry/catch-up path keeps working.
    Observed,
    /// The destination is about to be selected (rollback): the archive must
    /// already cover every committed source fact, otherwise those facts would
    /// disappear along with the software pointer.
    SelectedDestination,
}

/// Import a verified Store archive, preserving its typed source identities.
/// The archive remains beside the destination as retained recovery evidence.
/// Local-only lease/owner rows stay local; the initial consistent A/B copy
/// retains them, and current owners explicitly hand resources over later.
/// Conflicts are retained by Store, never overwritten as a generic row merge.
pub fn import_delta(source_db: &Path, target_db: &Path, cas_dir: &Path) -> Result<ImportReport, MigrationError> {
    import_delta_inner(source_db, target_db, cas_dir, SourceCoverage::Observed)
}

/// Import a verified Store archive into the generation a rollback is about to
/// select.
///
/// This is the same typed delta as [`import_delta`], plus one proof the
/// hand-over needs: the snapshot must cover the source's committed high water
/// both at capture time and when the import finishes. A source that commits
/// after its snapshot (a claimed invocation, an independent ingress, a late
/// E1) is a blocking unit, so the caller refuses the selection and keeps the
/// working generation instead of switching to a copy that silently lost those
/// facts. A retry takes a new snapshot; nothing is replayed or overwritten.
pub fn import_rollback_delta(source_db: &Path, target_db: &Path, cas_dir: &Path) -> Result<ImportReport, MigrationError> {
    import_delta_inner(source_db, target_db, cas_dir, SourceCoverage::SelectedDestination)
}

fn import_delta_inner(
    source_db: &Path,
    target_db: &Path,
    cas_dir: &Path,
    coverage: SourceCoverage,
) -> Result<ImportReport, MigrationError> {
    let source = Store::open(source_db.to_owned(), cas_dir.to_owned()).map_err(anyhow::Error::from)?;
    let target = Store::open(target_db.to_owned(), cas_dir.to_owned()).map_err(anyhow::Error::from)?;
    // A generation hand-over migrates one subject between two copies of
    // itself. Two provably different local subjects are two installations;
    // importing one into the other would graft a foreign closure onto the
    // retained copy. An absent subject (a store without genesis) is not a
    // mismatch — it is simply not a claim either way.
    let source_subject = source.get_existing_agent_id().map_err(anyhow::Error::from)?;
    let target_subject = target.get_existing_agent_id().map_err(anyhow::Error::from)?;
    if let Some(detail) = subject_binding_refusal(source_subject, target_subject) {
        return Ok(ImportReport {
            tables: Vec::new(), commits_imported: 0, watermark: None,
            source_watermark_before: None, source_watermark_after: None,
            uncovered_source_commits: 0, archive_path: None,
            blocked_units: vec![detail],
        });
    }
    let source_root = source_db.parent().ok_or_else(|| anyhow::anyhow!("source directory missing"))?;
    let target_root = target_db.parent().ok_or_else(|| anyhow::anyhow!("target directory missing"))?;
    let source_memory = source_root.join("memory");
    let target_memory = target_root.join("memory");
    // The source watermark is read from committed state on its own read-only
    // connection: it must describe the source, not this process's view. Only a
    // destination that is about to be selected depends on it as a proof, so a
    // forward delta keeps working exactly as before when the source cannot be
    // observed while it is still committing.
    let observe = |db: &Path| match coverage {
        SourceCoverage::SelectedDestination => watermark_of(db),
        SourceCoverage::Observed => Ok(watermark_of(db).ok().flatten()),
    };
    let source_before = observe(source_db)?;
    let export = source.export_cognition(&CognitionSnapshotOptions {
        memory_root: source_memory.is_dir().then_some(source_memory),
        workspace_roots: Vec::new(), include_git_history: true,
        // This is the same-installation handover, not a peer replication:
        // the final catch-up legitimately runs while the subject is
        // quiesced, and `copy_paused_activation_boundary` below installs
        // the complete paused boundary at the destination before the
        // archive is applied.
        local_boundary_preserved: true,
    })?;
    let archive_name = format!("activation-import-{}.shka", EntityId::new());
    let complete_path = target_root.join(&archive_name);
    // The bytes are written under a staging name and only promoted once the
    // export reported a complete snapshot, so `activation-import-*.shka`
    // beside a generation always means a whole typed snapshot. A partial
    // write keeps its staging name and is never mistaken for one.
    let stage_path = target_root.join(format!(".{archive_name}.stage"));
    let receipt = {
        let mut stage = std::fs::OpenOptions::new().create_new(true).write(true).open(&stage_path)?;
        let receipt = export.write_archive(&mut stage)?;
        stage.sync_all()?;
        receipt
    };
    let archive_path = if receipt.complete {
        std::fs::rename(&stage_path, &complete_path)?;
        std::fs::File::open(target_root)?.sync_all()?;
        complete_path
    } else {
        stage_path
    };
    let retained = Some(archive_path.to_string_lossy().into_owned());
    let mut blocked_units: Vec<String> = Vec::new();
    let uncovered_before = uncovered(receipt.high_water, source_before);
    blocked_units.extend(snapshot_coverage_refusal(coverage, source_before, receipt.high_water));
    if !receipt.complete {
        blocked_units.extend(receipt.gaps.iter().filter(|gap| gap.blocking)
            .map(|gap| format!("{}: {}", gap.kind, gap.detail)));
        return Ok(ImportReport { tables: Vec::new(), commits_imported: 0,
            watermark: Some(CommitRef::new(source.ledger.ledger_id(), receipt.high_water)),
            source_watermark_before: source_before, source_watermark_after: source_before,
            uncovered_source_commits: uncovered_before, archive_path: retained, blocked_units });
    }
    if let Some(agent) = source_subject {
        target.copy_paused_activation_boundary(&source, agent).map_err(anyhow::Error::from)?;
    }
    let outcome = target.import_cognition_archive(std::fs::File::open(&archive_path)?, &CognitionImportOptions {
        memory_root: target_memory.is_dir().then_some(target_memory), workspace_root: None,
        // Stable source identity binds Store's baselines and exact repeated
        // imports; retries never create a fresh replication relationship.
        peer_id: Some(source.ledger.ledger_id()),
        // The declared same-installation handover is the only import that may
        // adopt local control facts (the scope's pending/answered rollover
        // request). A peer import cannot.
        local_control_handover: true,
    })?;
    blocked_units.extend(outcome.gaps.iter().filter(|gap| gap.blocking)
        .map(|gap| format!("{}: {}", gap.kind, gap.detail)));
    blocked_units.extend(outcome.conflicts.iter().map(|conflict| format!("typed recovery conflict: {conflict:?}")));
    // Re-read the source after the snapshot was applied: the facts that must
    // not disappear are the ones committed up to this point, not the ones the
    // snapshot happened to see.
    let source_after = observe(source_db)?;
    let uncovered_after = uncovered(outcome.high_water, source_after);
    blocked_units.extend(source_advance_refusal(
        coverage,
        source_before,
        source_after,
        outcome.high_water,
    ));
    Ok(ImportReport {
        tables: vec![TableImport { table: "store-declared-recovery-closure".into(),
            rows_compared: (outcome.inserted + outcome.updated + outcome.unchanged) as u64,
            rows_written: (outcome.inserted + outcome.updated) as u64 }],
        // Source commits remain source references; importing them as local
        // sequence numbers would alias independent A/B ledger commits.
        commits_imported: 0,
        watermark: Some(CommitRef::new(source.ledger.ledger_id(), outcome.high_water)),
        source_watermark_before: source_before,
        source_watermark_after: source_after,
        uncovered_source_commits: uncovered_after,
        archive_path: retained,
        blocked_units,
    })
}

pub fn read_watermark(conn: &Connection) -> Result<Option<CommitRef>, MigrationError> {
    let ledger: Option<String> = conn.query_row("SELECT ledger_id FROM ledger_identity WHERE singleton=1", [], |row| row.get(0)).optional()?;
    let Some(ledger) = ledger else { return Ok(None); };
    let seq: i64 = conn.query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log WHERE ledger_id=?1", [&ledger], |row| row.get(0))?;
    let ledger = ledger.parse::<EntityId>().map_err(|error| anyhow::anyhow!("invalid source ledger identity: {error}"))?;
    Ok(Some(CommitRef::new(ledger, LedgerSeq(seq as u64))))
}
pub fn watermark_of(db_path: &Path) -> Result<Option<CommitRef>, MigrationError> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    read_watermark(&conn)
}

/// Source commits the snapshot does not cover.
fn uncovered(snapshot: LedgerSeq, observed: Option<CommitRef>) -> u64 {
    observed
        .map(|observed| observed.seq.0.saturating_sub(snapshot.0))
        .unwrap_or(0)
}

/// Two different local subjects are two installations. A hand-over migrates
/// one subject's facts into another copy of that same subject, never into
/// somebody else's state.
fn subject_binding_refusal(source: Option<EntityId>, target: Option<EntityId>) -> Option<String> {
    match (source, target) {
        (Some(source), Some(target)) if source != target => Some(format!(
            "subject-binding-mismatch: the source owns subject {source} and the retained copy owns {target}; a generation hand-over never migrates another installation's state"
        )),
        _ => None,
    }
}

/// The refusal of a snapshot that already stops short of the source it was
/// taken from. Only a destination that is about to become the selected
/// generation refuses: the forward delta legitimately runs against a live
/// source and keeps observing instead.
fn snapshot_coverage_refusal(
    coverage: SourceCoverage,
    source_before: Option<CommitRef>,
    snapshot: LedgerSeq,
) -> Option<String> {
    if coverage != SourceCoverage::SelectedDestination {
        return None;
    }
    let before = source_before?;
    if uncovered(snapshot, source_before) == 0 {
        return None;
    }
    Some(format!(
        "source-not-covered: the snapshot stops at {snapshot} but the source had already committed {before}; the selected generation would not carry those facts"
    ))
}

/// The refusal of a source that committed after its snapshot was taken. The
/// destination must carry the closure committed up to the moment it takes
/// over, so this compares the *post-import* observation, not the capture.
fn source_advance_refusal(
    coverage: SourceCoverage,
    source_before: Option<CommitRef>,
    source_after: Option<CommitRef>,
    imported: LedgerSeq,
) -> Option<String> {
    if coverage != SourceCoverage::SelectedDestination {
        return None;
    }
    let Some(after) = source_after else {
        return Some("source-coverage-unobservable: the source reports no committed high water, so the snapshot cannot be shown to cover it".to_string());
    };
    if let Some(before) = source_before {
        if before.ledger_id != after.ledger_id {
            return Some(format!(
                "source-identity-changed: the source ledger was {} before the snapshot and {} after it, so the snapshot describes neither",
                before.ledger_id, after.ledger_id
            ));
        }
    }
    if uncovered(imported, source_after) == 0 {
        return None;
    }
    Some(format!(
        "source-advanced: the source committed {after} after the snapshot at {imported}, so the selected generation would not carry that closure; keep the working generation and take a new snapshot"
    ))
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    fn at(seq: u64) -> CommitRef {
        CommitRef::new(EntityId::nil(), LedgerSeq(seq))
    }

    /// The retained copy is about to be selected: a snapshot that already
    /// stops short of its source must be refused before anything is applied,
    /// and a covering snapshot must not be.
    #[test]
    fn a_selected_destination_refuses_a_snapshot_that_stops_short() {
        let coverage = SourceCoverage::SelectedDestination;
        assert!(snapshot_coverage_refusal(coverage, Some(at(9)), LedgerSeq(4)).is_some());
        assert!(snapshot_coverage_refusal(coverage, Some(at(4)), LedgerSeq(4)).is_none());
        assert!(snapshot_coverage_refusal(coverage, Some(at(4)), LedgerSeq(9)).is_none());
        assert!(snapshot_coverage_refusal(coverage, None, LedgerSeq(4)).is_none());
        // The forward delta runs against a live source and never refuses here.
        assert!(snapshot_coverage_refusal(SourceCoverage::Observed, Some(at(9)), LedgerSeq(4)).is_none());
    }

    /// Facts committed after the capture are the difference between a
    /// rollback that preserves them and one that silently drops them.
    #[test]
    fn a_selected_destination_refuses_facts_committed_after_the_snapshot() {
        let coverage = SourceCoverage::SelectedDestination;
        let refusal = source_advance_refusal(coverage, Some(at(4)), Some(at(7)), LedgerSeq(4))
            .expect("a source that advanced must be refused");
        assert!(refusal.contains("source-advanced"), "{refusal}");
        assert!(source_advance_refusal(coverage, Some(at(7)), Some(at(7)), LedgerSeq(7)).is_none());
        assert!(source_advance_refusal(coverage, Some(at(4)), Some(at(4)), LedgerSeq(4)).is_none());
        assert!(source_advance_refusal(coverage, Some(at(4)), None, LedgerSeq(4)).is_some());
        assert!(
            source_advance_refusal(SourceCoverage::Observed, Some(at(4)), Some(at(7)), LedgerSeq(4))
                .is_none()
        );
        // A source that changed ledger identity mid-call describes neither
        // snapshot.
        let other_ledger = CommitRef::new(EntityId::new(), LedgerSeq(7));
        let changed = source_advance_refusal(coverage, Some(at(4)), Some(other_ledger), LedgerSeq(7))
            .expect("a replaced source must be refused");
        assert!(changed.contains("source-identity-changed"), "{changed}");
    }

    /// Two provably different subjects are two installations. The hand-over
    /// refuses rather than grafting a foreign closure onto the retained copy,
    /// and a store without a local subject is not a claim either way.
    #[test]
    fn a_handover_refuses_two_different_subjects() {
        let subject = EntityId::new();
        assert!(subject_binding_refusal(Some(subject), Some(subject)).is_none());
        let refusal = subject_binding_refusal(Some(subject), Some(EntityId::new()))
            .expect("two different subjects must be refused");
        assert!(refusal.contains("subject-binding-mismatch"), "{refusal}");
        assert!(subject_binding_refusal(Some(subject), None).is_none());
        assert!(subject_binding_refusal(None, Some(subject)).is_none());
        assert!(subject_binding_refusal(None, None).is_none());
    }
}
