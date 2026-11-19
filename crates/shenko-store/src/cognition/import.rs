//! Recovery-import orchestration and its single ledger publication transaction.
//!
//! Archive admission precedes reconciliation; required conflict closures are
//! settled before file/Git publication, and source order advances with COMMIT.

use std::collections::HashSet;
use std::io::Read;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use shenko_types::subject::{SubjectBranchHead, SubjectMode, SubjectStepState};

use crate::cognition_archive::{
    CognitionFileDescriptor, CognitionFileOrigin, CognitionGap, CognitionGitDescriptor, CognitionManifest,
    CognitionRow, read_archive,
};
use crate::store::Store;

use super::admission::{
    LOCAL_CONTROL_CLASSES, archive_membership_gaps, check_archive_membership,
    memory_intent_binding_gaps, migrate_legacy_rows, validate_cognition_rows,
};
use super::memory::apply_memory_pairs;
use super::peers::{record_conflict, record_origin};
use super::reconcile::{
    apply_context_notebook, apply_media_streams, apply_semantic_units, is_unit_table,
};
use super::rows::{
    RowKind, TABLES, describe_difference, effective_row, insert_row,
    is_immutable_terminal_settlement, read_baseline, read_existing, read_table,
    rows_equivalent, spec_for, update_row, write_baseline,
};
use super::snapshot::fingerprint_of_manifest;
use super::tasks::{apply_task_closures, verify_no_rejected_task_consumers};
use super::{
    CognitionApplyOutcome, CognitionConflict, CognitionImportOptions, CognitionImportOutcome,
    db, is_constraint_violation, storage_failure,
};

/// Verified, uninstalled archive bytes bound to the original receiving Store
/// and options. This value carries no selection or mutation authority. Its
/// private fields and consuming publication API prevent an external caller
/// from substituting a manifest or moving a prepared import to another Store.
pub struct PreparedCognitionImport {
    target_incarnation: EntityId,
    target_ledger: EntityId,
    options: CognitionImportOptions,
    manifest: CognitionManifest,
    files: Vec<(CognitionFileDescriptor, Vec<u8>)>,
    git_history: Option<(CognitionGitDescriptor, Vec<u8>)>,
    gaps: Vec<CognitionGap>,
}

impl Store {
    /// Import a recovery snapshot.
    ///
    /// Nothing is published before the whole closure arrived and verified: a
    /// missing object, a corrupt payload or an incomplete closure stops the
    /// import and leaves the receiving store untouched.
    pub fn import_cognition_archive<R: Read>(
        &self,
        reader: R,
        options: &CognitionImportOptions,
    ) -> Result<CognitionImportOutcome> {
        self.import_cognition_archive_under(reader, options, None)
    }

    pub fn import_cognition_archive_under<R: Read>(&self, reader: R,
        options: &CognitionImportOptions,
        borrowed: Option<&shenko_types::installation::InstallationPermit>) -> Result<CognitionImportOutcome> {
        let prepared = self.prepare_cognition_import(reader, options)?;
        self.apply_prepared_cognition_import_under(prepared, borrowed)
    }

    /// Read, seal and verify the whole archive without taking a live mutation
    /// permit. Publication still rechecks import order and all live conflicts.
    pub fn prepare_cognition_import<R: Read>(&self, reader: R,
        options: &CognitionImportOptions) -> Result<PreparedCognitionImport> {
        let mut files: Vec<(CognitionFileDescriptor, Vec<u8>)> = Vec::new();
        let mut git_history: Option<(CognitionGitDescriptor, Vec<u8>)> = None;
        let mut manifest = {
            let mut reader = reader;
            read_archive(
                &mut reader,
                |descriptor, bytes| {
                    self.cas.seal_object(
                        bytes,
                        &descriptor.media_type,
                        &descriptor.store_namespace,
                        &descriptor.codec.name,
                        descriptor.codec.major,
                    )?;
                    Ok(())
                },
                &mut files,
                &mut git_history,
            )?
        };
        let fingerprint = fingerprint_of_manifest(&manifest);
        ensure!(
            fingerprint == manifest.fingerprint,
            "cognition manifest fingerprint does not match its received rows and verified payload descriptors"
        );
        // The archive's membership is checked against this reader's own
        // classes before a single row is applied.
        check_archive_membership(&manifest)?;
        // Local control facts are installed only by a declared handover. The
        // claim comes from the importer, never from the archive.
        if !options.local_control_handover
            && let Some(class) = LOCAL_CONTROL_CLASSES.iter().find(|class| {
                manifest.fact_classes.iter().any(|declared| declared == *class)
                    || manifest.rows.iter().any(|row| row.table == **class)
            })
        {
            bail!(
                "cognition archive carries the local control fact class {class}; \
                 only a declared same-installation handover may install it"
            );
        }
        // A legacy archive is migrated to the current column layout first:
        // the layout only grew, so the migration is by name and keeps every
        // fact's identity.
        let mut legacy_gaps = Vec::new();
        // Schema 6 changes required semantics, not the schema-5 columns.
        if manifest.schema < 5 {
            legacy_gaps = migrate_legacy_rows(&mut manifest);
        }
        validate_cognition_rows(&manifest)?;
        self.validate_obligation_rows(&manifest.rows)?;

        let mut gaps = manifest.gaps.clone();
        gaps.extend(legacy_gaps);
        gaps.extend(memory_intent_binding_gaps(&manifest.rows));
        gaps.extend(crate::media::media_recovery_gaps(&manifest.rows));
        gaps.extend(archive_membership_gaps(&manifest.rows));
        gaps.extend(self.verify_manifest_closure(&manifest));
        let blocking: Vec<String> = gaps
            .iter()
            .filter(|gap| gap.blocking)
            .map(|gap| format!("{}: {}", gap.kind, gap.detail))
            .collect();
        if !blocking.is_empty() {
            bail!(
                "cognition snapshot is incomplete, nothing published: {}",
                blocking.join(", ")
            );
        }
        Ok(PreparedCognitionImport {
            target_incarnation: self.capabilities.incarnation,
            target_ledger: self.ledger.ledger_id(), options: options.clone(),
            manifest, files, git_history, gaps,
        })
    }

    /// Consume verified bytes at their original receiver. An enclosing A/B
    /// cutover can borrow its reservation without doing archive I/O inside it.
    pub fn apply_prepared_cognition_import_under(&self, prepared: PreparedCognitionImport,
        borrowed: Option<&shenko_types::installation::InstallationPermit>) -> Result<CognitionImportOutcome> {
        ensure!(prepared.target_incarnation == self.capabilities.incarnation
            && prepared.target_ledger == self.ledger.ledger_id(),
            "prepared cognition import belongs to another Store receiver");
        let PreparedCognitionImport { options, manifest, files, git_history, gaps, .. } = prepared;
        let peer_id = options.peer_id.unwrap_or(EntityId::nil());
        let verified_objects = manifest.objects.len();
        let fingerprint = manifest.fingerprint.clone();
        // Download/verification above holds no installed mutation lock. Only
        // the final live file/Git/row publication enters the shared domain,
        // before taking the existing import gate or either SQLite lock.
        let owned = if borrowed.is_none() {
            self.ledger.acquire_installation(!options.local_control_handover)?
        } else { None };
        let permit = borrowed.or(owned.as_ref());
        if !options.local_control_handover && let Some(permit) = permit {
            self.validate_installation_permit(permit)?;
        }
        let mut import_gate = self.cognition_import_gate.lock();
        let _gate_tx = import_gate.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.check_import_order(&manifest)?;
        // Reject unresumable imported heads or a conflicting paused successor
        // before publishing files or rows. Recheck under the mutation
        // transaction as the local subject may reach a boundary meanwhile.
        {
            let mut conn = self.ledger.conn.lock();
            let tx = conn.transaction().map_err(db)?;
            validate_subject_boundaries(&tx, &manifest)?;
            tx.commit().map_err(db)?;
        }

        for descriptor in &manifest.files {
            ensure!(
                manifest.file_origins.contains(&descriptor.origin),
                "cognition file {} has an origin outside the declared snapshot coverage",
                descriptor.path
            );
        }
        ensure!(
            !manifest.file_origins.contains(&CognitionFileOrigin::Memory)
                || options.memory_root.is_some(),
            "cognition snapshot covers memory files but no memory root was given"
        );
        ensure!(
            !manifest
                .file_origins
                .contains(&CognitionFileOrigin::Workspace)
                || options.workspace_root.is_some(),
            "cognition snapshot covers workspace files but no workspace root was given"
        );
        ensure!(
            manifest.git_history.is_none() || options.memory_root.is_some(),
            "cognition snapshot carries memory history but no memory root was given"
        );

        // All required row conflicts are decided while the IMMEDIATE ledger
        // transaction is still held, before any file or Git publication.
        let (applied, verified_files, restored_git_history) =
            self.apply_rows(&manifest, peer_id, &files, &git_history, &options, permit)?;
        let conflicts = applied.conflicts;

        self.record_receipt(
            peer_id,
            &manifest,
            verified_objects,
            verified_files,
            applied.inserted,
            applied.updated,
            applied.unchanged,
            conflicts.len(),
            permit,
        )?;

        Ok(CognitionImportOutcome {
            fingerprint,
            high_water: manifest.high_water,
            inserted: applied.inserted,
            updated: applied.updated,
            unchanged: applied.unchanged,
            conflicts,
            verified_objects,
            verified_files,
            restored_git_history,
            gaps,
        })
    }

    fn check_import_order(&self, manifest: &CognitionManifest) -> Result<()> {
        let conn = self.ledger.conn.lock();
        check_import_order_conn(&conn, manifest)
    }

    /// Apply the replicated facts to this store.
    ///
    /// A row that already exists with the same content is left alone, so a
    /// replay never duplicates a fact. A row that this store still holds
    /// exactly as the peer last delivered it takes the newer value, so a
    /// continuing source advances instead of looking like a divergence. A row
    /// that this store changed on its own keeps the local value and is
    /// reported as a conflict.
    fn apply_rows(
        &self,
        manifest: &CognitionManifest,
        peer_id: EntityId,
        files: &[(CognitionFileDescriptor, Vec<u8>)],
        git_history: &Option<(CognitionGitDescriptor, Vec<u8>)>,
        options: &CognitionImportOptions,
        permit: Option<&shenko_types::installation::InstallationPermit>,
    ) -> Result<(CognitionApplyOutcome, usize, bool)> {
        let mut ordered: Vec<&CognitionRow> = manifest.rows.iter().collect();
        ordered.sort_by_key(|row| {
            TABLES
                .iter()
                .position(|spec| spec.name == row.table)
                .unwrap_or(usize::MAX)
        });
        // Local retirement roots must survive rollback of the imported facts.
        // Reserve and register them before the transaction can move any bytes.
        let retirements = self.prepare_file_retirements(manifest, peer_id, options, permit)?;
        self.ledger.with_maintenance_mutation_under(permit,
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(db)?;
                validate_cognition_rows(manifest)?;
                self.validate_obligation_rows(&manifest.rows)?;
                check_import_order_conn(&tx, manifest)?;
                validate_subject_boundaries(&tx, manifest)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let mut inserted = 0usize;
                let mut updated = 0usize;
                let mut unchanged = 0usize;
                let mut conflicts: Vec<CognitionConflict> = Vec::new();
                let (media, media_events) = apply_media_streams(&tx, &ordered, peer_id, seq, manifest)?;
                inserted += media.inserted;
                updated += media.updated;
                unchanged += media.unchanged;
                conflicts.extend(media.conflicts);
                let referenced_memory_entries: HashSet<&str> = ordered.iter()
                    .filter(|row| row.table == "memory_intents")
                    .filter_map(|row| row.value("entry_id"))
                    .collect();
                let mut memory_applied = false;
                let mut task_closure_applied = false;
                let mut task_owned_rows = HashSet::new();
                let mut semantic_units_applied = false;
                let mut notebook_applied = false;
                let mut unit_owned_rows: HashSet<(&str, &str)> = HashSet::new();
                for row in &ordered {
                    // Another branch is a parallel historical fact. It cannot
                    // replace this installation's active controller projection.
                    // Retain every exact head by content identity instead of
                    // treating distinct branches as competing updates to it.
                    if row.table == "subject_branch_heads" && archives_parallel_head(&tx, row)? {
                        let data = row.value("data").context("parallel head data missing")?;
                        let snapshot = Digest::blake3(data.as_bytes());
                        let historical = CognitionRow {
                            table: "subject_head_history".into(), key: snapshot.0.clone(),
                            values: vec![("snapshot_id".into(), Some(snapshot.0)),
                                ("agent_id".into(), row.value("agent_id").map(str::to_owned)),
                                ("branch_id".into(), row.value("branch_id").map(str::to_owned)),
                                ("data".into(), Some(data.to_owned()))],
                        };
                        let history_spec = spec_for("subject_head_history").context("branch history table is not registered")?;
                        match read_existing(&tx, history_spec, &historical)? {
                            Some(existing) => {
                                ensure!(rows_equivalent(history_spec, &existing, &historical.values), "historical head content collision");
                                unchanged += 1;
                            }
                            None => {
                                insert_row(&tx, history_spec, &historical, seq)?;
                                record_origin(&tx, history_spec, &historical, peer_id, &manifest.fingerprint, manifest.source_ledger_id)?;
                                inserted += 1;
                            }
                        }
                        continue;
                    }
                    if matches!(row.table.as_str(), "media_streams" | "media_stream_ends" | "media_segment_revisions"
                        | "media_recordings" | "media_recording_controls" | "media_priority_inputs")
                        || (row.table == "inbox_events" && media_events.contains(row.key.as_str()))
                    {
                        continue;
                    }
                    if !memory_applied && matches!(row.table.as_str(), "memory_entries" | "memory_intents" | "memory_intent_bindings") {
                        let outcome = apply_memory_pairs(&tx, &ordered, peer_id, seq)?;
                        inserted += outcome.inserted;
                        updated += outcome.updated;
                        unchanged += outcome.unchanged;
                        conflicts.extend(outcome.conflicts);
                        memory_applied = true;
                    }
                    // A task and every dependent execution/result fact is ONE
                    // conflict unit. Importing remote operations, evidence or
                    // results under a task_id whose local input differs would
                    // stitch unrelated execution history onto the local task.
                    if !task_closure_applied
                        && matches!(row.table.as_str(), "session_events" | "subject_raw_inputs" | "inbox_events"
                            | "tasks" | "operations" | "attempts" | "evidence_receipts" | "task_results")
                    {
                        // raw_sessions and subject_steps have already applied,
                        // but NO pending/raw/staged delivery has been published.
                        let (outcome, owned, rejected, rejected_ids) =
                            apply_task_closures(&tx, &ordered, peer_id, seq, manifest)?;
                        inserted += outcome.inserted;
                        updated += outcome.updated;
                        unchanged += outcome.unchanged;
                        conflicts.extend(outcome.conflicts);
                        task_owned_rows = owned;
                        // Whole-publication boundary: if any consumer
                        // committed earlier in this transaction (subject
                        // step's frozen S0, context window chain, manifest
                        // derivatives) depends on a rejected delivery and the
                        // dependency cannot be proven removable without
                        // mutating sealed identities, reject the whole
                        // import instead of publishing a partial falsehood.
                        verify_no_rejected_task_consumers(
                            self, &tx, &ordered, &manifest.derivatives,
                            &rejected, &rejected_ids,
                        )?;
                        task_closure_applied = true;
                    }
                    // Semantic conflict units (seats, research epochs, quota
                    // requests, persons, activation selectors, output parts,
                    // stream journal): every member of one bound effect is
                    // applied or refused together.
                    if !semantic_units_applied && is_unit_table(&row.table) {
                        let (outcome, owned) =
                            apply_semantic_units(&tx, &ordered, peer_id, seq, manifest)?;
                        inserted += outcome.inserted;
                        updated += outcome.updated;
                        unchanged += outcome.unchanged;
                        conflicts.extend(outcome.conflicts);
                        unit_owned_rows = owned;
                        semantic_units_applied = true;
                    }
                    if matches!(row.table.as_str(), "context_notes" | "context_rollover_requests") {
                        if !notebook_applied {
                            let outcome = apply_context_notebook(&tx, &ordered)?;
                            inserted += outcome.inserted;
                            updated += outcome.updated;
                            unchanged += outcome.unchanged;
                            conflicts.extend(outcome.conflicts);
                            notebook_applied = true;
                        }
                        continue;
                    }
                    if row.table == "memory_intents" || row.table == "memory_intent_bindings"
                        || (row.table == "memory_entries" && referenced_memory_entries.contains(row.key.as_str()))
                    {
                        continue;
                    }
                    if task_owned_rows.contains(&(row.table.as_str(), row.key.as_str())) {
                        continue;
                    }
                    if unit_owned_rows.contains(&(row.table.as_str(), row.key.as_str())) {
                        continue;
                    }
                    let Some(spec) = spec_for(&row.table) else {
                        continue;
                    };
                    let effective = effective_row(row, spec)?;
                    let local = read_existing(&tx, spec, &effective)?;
                    let baseline = read_baseline(&tx, peer_id, spec.name, &effective.key)?;
                    match local {
                        None => {
                            match insert_row(&tx, spec, &effective, seq) {
                                Ok(()) => {
                                    inserted += 1;
                                    write_baseline(
                                        &tx,
                                        peer_id,
                                        spec.name,
                                        &effective.key,
                                        &effective.values,
                                    )?;
                                    record_origin(
                                        &tx,
                                        spec,
                                        &effective,
                                        peer_id,
                                        &manifest.fingerprint,
                                        manifest.source_ledger_id,
                                    )?;
                                }
                                Err(error) if is_constraint_violation(&error) => {
                                    let detail =
                                        "the imported fact breaks a local uniqueness rule; the local value is kept"
                                            .to_string();
                                    conflicts.push(CognitionConflict {
                                        table: spec.name.to_string(),
                                        key: effective.key.clone(),
                                        detail: detail.clone(),
                                    });
                                    record_conflict(
                                        &tx,
                                        peer_id,
                                        spec.name,
                                        &effective.key,
                                        &detail,
                                    )?;
                                }
                                Err(error) => return Err(error),
                            }
                        }
                        Some(local_values) => {
                            ensure!(!matches!(spec.name, "subject_branch_forks" | "subject_head_history")
                                || rows_equivalent(spec, &local_values, &effective.values),
                                "immutable subject branch provenance cannot be replaced");
                            ensure!(spec.kind != RowKind::SubjectIntent
                                || !is_immutable_terminal_settlement(&local_values)
                                || rows_equivalent(spec, &local_values, &effective.values),
                                "an existing obligation terminal settlement cannot be replaced");
                            if rows_equivalent(spec, &local_values, &effective.values) {
                                unchanged += 1;
                                write_baseline(
                                    &tx,
                                    peer_id,
                                    spec.name,
                                    &effective.key,
                                    &effective.values,
                                )?;
                            } else if let Some(baseline_values) = baseline.as_ref() {
                                if rows_equivalent(spec, &local_values, baseline_values) {
                                    // This store still holds the value the
                                    // peer last delivered, so the newer
                                    // value is the peer's continuation, not
                                    // a divergence.
                                    update_row(&tx, spec, &effective, seq)?;
                                    updated += 1;
                                    write_baseline(
                                        &tx,
                                        peer_id,
                                        spec.name,
                                        &effective.key,
                                        &effective.values,
                                    )?;
                                    record_origin(
                                        &tx,
                                        spec,
                                        &effective,
                                        peer_id,
                                        &manifest.fingerprint,
                                        manifest.source_ledger_id,
                                    )?;
                                } else {
                                    let detail = describe_difference(
                                        &local_values,
                                        &effective.values,
                                    );
                                    conflicts.push(CognitionConflict {
                                        table: spec.name.to_string(),
                                        key: effective.key.clone(),
                                        detail: detail.clone(),
                                    });
                                    record_conflict(
                                        &tx,
                                        peer_id,
                                        spec.name,
                                        &effective.key,
                                        &detail,
                                    )?;
                                }
                            } else {
                                // The fact was not delivered by this peer, so
                                // the local value is not a copy of it.
                                let detail = describe_difference(
                                    &local_values,
                                    &effective.values,
                                );
                                conflicts.push(CognitionConflict {
                                    table: spec.name.to_string(),
                                    key: effective.key.clone(),
                                    detail: detail.clone(),
                                });
                                record_conflict(
                                    &tx,
                                    peer_id,
                                    spec.name,
                                    &effective.key,
                                    &detail,
                                )?;
                            }
                        }
                    }
                }
                for derivative in &manifest.derivatives {
                    ensure!(
                        derivative.blake3.as_str()
                            == blake3::hash(&derivative.bytes).to_hex().as_str(),
                        "context window derivative fails its blake3"
                    );
                    let exists: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM context_window_derivatives WHERE window_id=?1 AND idx=?2",
                            params![derivative.window_id.to_string(), derivative.idx],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(db)?;
                    if exists.is_none() {
                        tx.execute(
                            "INSERT INTO context_window_derivatives(window_id,idx,media_type,blake3,bytes)
                             VALUES(?1,?2,?3,?4,?5)",
                            params![
                                derivative.window_id.to_string(),
                                derivative.idx,
                                derivative.media_type,
                                derivative.blake3,
                                derivative.bytes,
                            ],
                        )
                        .map_err(db)?;
                        inserted += 1;
                    } else {
                        unchanged += 1;
                    }
                }
                // Staged inputs can reach prepare_context without traversing
                // inbox_events again. Recheck the ACTUAL reconciled store,
                // not only the incoming manifest: if a media conflict rolled
                // back admission but ordinary history imported its staged
                // delivery, reject before commit and roll back every SQL row.
                if ordered.iter().any(|row| matches!(row.table.as_str(),
                    "media_streams" | "media_stream_ends" | "media_segment_revisions"
                        | "media_recordings" | "media_recording_controls" | "media_priority_inputs"))
                {
                    let mut media_rows = Vec::new();
                    for spec in TABLES.iter().filter(|spec| matches!(spec.name,
                        "media_streams" | "media_stream_ends" | "media_segment_revisions"
                            | "media_recordings" | "media_recording_controls" | "media_priority_inputs"
                            | "inbox_events" | "session_events" | "subject_raw_inputs"))
                    {
                        media_rows.extend(read_table(&tx, spec)?);
                    }
                    if let Some(gap) = crate::media::media_recovery_gaps(&media_rows).first() {
                        bail!("media import would publish a partial recovery binding; SQLite publication rolled back: {}", gap.detail);
                    }
                }
                if ordered.iter().any(|row| row.table == "subject_obligations"
                    || (row.table == "subject_intents" && row.value("contract") == Some(crate::subject_state::OBLIGATION_TERMINAL_CONTRACT)))
                {
                    // A required member cannot be declined while a sibling
                    // decision, projection or baseline is published. Refuse
                    // the complete import before the filesystem boundary.
                    ensure!(!conflicts.iter().any(|conflict| matches!(conflict.table.as_str(),
                        "subject_obligations" | "subject_steps" | "subject_branch_heads"
                        | "subject_intents" | "subject_intent_steps" | "context_windows"
                        | "subject_raw_inputs" | "session_events" | "inbox_events" | "channel_record_migrations")),
                        "required obligation conflict unit diverges; nothing published");
                    self.validate_obligation_recovery(&tx)?;
                }
                if tx.query_row("SELECT EXISTS(SELECT 1 FROM subject_branch_forks)", [], |row| row.get::<_, bool>(0)).map_err(db)? {
                    let mut branch_rows = Vec::new();
                    for spec in TABLES.iter().filter(|spec| matches!(spec.name,
                        "subject_branch_forks" | "subject_head_history" | "subject_branch_heads"
                        | "subject_steps" | "tasks" | "session_events" | "subject_raw_inputs" | "context_notes"))
                    {
                        branch_rows.extend(read_table(&tx, spec)?);
                    }
                    // Re-check the ACTUAL reconciled closure: a rejected raw
                    // event cannot leave an admitted child with partial memory.
                    crate::subject_branches::validate_branch_rows(&branch_rows)?;
                }
                let (verified_files, file_conflicts) = self.restore_files(
                    &tx, files, &options.memory_root, &options.workspace_root, &manifest.file_origins, peer_id,
                    &retirements,
                )?;
                conflicts.extend(file_conflicts);
                let mut restored_git_history = false;
                if let Some(root) = options.memory_root.as_ref() {
                    let (restored, conflict) = self.restore_git_history(&tx, git_history, root, peer_id)?;
                    restored_git_history = restored;
                    conflicts.extend(conflict);
                }
                if inserted > 0 || updated > 0 {
                    tx.execute(
                        "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count)
                         VALUES(?1,?2,?3,?4)",
                        params![
                            self.ledger.ledger_id().to_string(),
                            seq.0 as i64,
                            self.capabilities.incarnation.to_string(),
                            (inserted + updated) as i64,
                        ],
                    )
                    .map_err(db)?;
                }
                record_import_order(&tx, peer_id, manifest)?;
                tx.commit().map_err(db)?;
                Ok((CognitionApplyOutcome {
                    inserted,
                    updated,
                    unchanged,
                    conflicts,
                }, verified_files, restored_git_history))
            },
            storage_failure,
        )
    }
}

fn check_import_order_conn(
    conn: &rusqlite::Connection,
    manifest: &CognitionManifest,
) -> Result<()> {
    let previous: Option<(i64, i64, String)> = conn
        .query_row(
            "SELECT snapshot_seq,high_water,fingerprint FROM cognition_import_order
             WHERE source_ledger_id=?1",
            [manifest.source_ledger_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let snapshot_seq = i64::try_from(manifest.source_snapshot_seq)?;
    let high_water = i64::try_from(manifest.high_water.0)?;
    if let Some((previous_seq, previous_high_water, previous_fingerprint)) = previous {
        ensure!(
            snapshot_seq >= previous_seq,
            "stale cognition snapshot from {}: durable export order {snapshot_seq} precedes {previous_seq}",
            manifest.source_ledger_id
        );
        ensure!(
            high_water >= previous_high_water,
            "stale cognition snapshot from {}: source high-water {high_water} precedes {previous_high_water}",
            manifest.source_ledger_id
        );
        if snapshot_seq == previous_seq {
            ensure!(
                high_water == previous_high_water
                    && manifest.fingerprint.0.as_str() == previous_fingerprint.as_str(),
                "conflicting cognition snapshots share source ledger {} and durable export order {snapshot_seq}",
                manifest.source_ledger_id
            );
        }
    }
    Ok(())
}

/// The pause record is local controller state and is not in archive schema
/// 2/3. A peer cannot manufacture that authority merely by importing a head.
/// Likewise a divergent peer's step must not be applied independently of a
/// retained paused head, even if the step still matches the peer baseline.
fn validate_subject_boundaries(
    conn: &rusqlite::Connection,
    manifest: &CognitionManifest,
) -> Result<()> {
    let head_spec = spec_for("subject_branch_heads").expect("subject head table is registered");
    let step_spec = spec_for("subject_steps").expect("subject step table is registered");

    for incoming in manifest.rows.iter().filter(|row| row.table == head_spec.name) {
        let data = incoming.value("data").context("imported subject head has no data")?;
        let value: serde_json::Value = serde_json::from_str(data).context("invalid imported subject head")?;
        let data_mode = value.get("mode").and_then(serde_json::Value::as_str)
            .context("imported subject head has no serialized mode")?;
        let column_mode = incoming.value("mode").context("imported subject head has no mode column")?;
        // Legacy migration maps columns by name. It must not turn contradictory
        // index columns and serialized head data into an admitted archive.
        let head: SubjectBranchHead = serde::Deserialize::deserialize(&value)?;
        let entity = |name: &str| incoming.value(name).and_then(|value| value.parse::<EntityId>().ok());
        let number = |name: &str| incoming.value(name).and_then(|value| value.parse::<u64>().ok());
        ensure!(entity("agent_id") == Some(head.agent_id)
            && entity("branch_id") == Some(EntityId(head.branch_id.0))
            && entity("next_step_id") == Some(head.next_step_id)
            && number("committed_step_no") == Some(head.committed_step_no)
            && number("record_revision") == Some(head.record_revision.0)
            && incoming.value("committed_step_id").is_some() == head.committed_step_id.is_some()
            && entity("committed_step_id") == head.committed_step_id
            && column_mode == data_mode,
            "imported subject head column/value mismatch");
        if (column_mode == "paused" || data_mode == "paused") && !archives_parallel_head(conn, incoming)? {
            ensure!(column_mode == "paused" && data_mode == "paused",
                "imported paused subject head has inconsistent mode fields");
            let local = read_existing(conn, head_spec, incoming)?
                .context("imported paused subject head has no local boundary")?;
            ensure!(rows_equivalent(head_spec, &local, &incoming.values),
                "imported paused subject head differs from the locally retained boundary");
            validate_local_paused_successor(conn, &head)?;
        }
    }

    // Check the actual local head, not merely the peer's head row. The peer
    // can omit its head or submit an active one that conflicts with ours.
    let mut query = conn.prepare(
        "SELECT data,mode FROM subject_branch_heads WHERE mode='paused' OR json_extract(data,'$.mode')='paused'",
    ).map_err(db)?;
    let local_heads = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(db)?.collect::<rusqlite::Result<Vec<_>>>().map_err(db)?;
    for (data, mode) in local_heads {
        let head: SubjectBranchHead = serde_json::from_str(&data)?;
        ensure!(mode == "paused" && head.mode == SubjectMode::Paused,
            "local paused subject head has inconsistent mode fields");
        let agent_id = head.agent_id.to_string();
        for incoming in manifest.rows.iter().filter(|row|
            row.table == head_spec.name && row.value("agent_id") == Some(agent_id.as_str())
                && row.value("branch_id") == Some(head.branch_id.0.to_string().as_str()))
        {
            validate_local_paused_successor(conn, &head)?;
            let local = read_existing(conn, head_spec, incoming)?
                .context("local paused subject head is missing")?;
            ensure!(rows_equivalent(head_spec, &local, &incoming.values),
                "imported head conflicts with the locally retained paused subject boundary");
        }
        let next_id = head.next_step_id.to_string();
        for incoming in manifest.rows.iter().filter(|row|
            row.table == step_spec.name && row.value("step_id") == Some(next_id.as_str()))
        {
            validate_local_paused_successor(conn, &head)?;
            let local = read_existing(conn, step_spec, incoming)?
                .context("local paused subject successor is missing")?;
            ensure!(rows_equivalent(step_spec, &local, &incoming.values),
                "imported step conflicts with the locally retained paused subject successor");
        }
    }
    Ok(())
}

fn archives_parallel_head(conn: &rusqlite::Connection, incoming: &CognitionRow) -> Result<bool> {
    let local: Option<String> = conn.query_row(
        "SELECT branch_id FROM subject_branch_heads WHERE agent_id=?1",
        [incoming.value("agent_id").context("subject head agent missing")?], |row| row.get(0),
    ).optional().map_err(db)?;
    Ok(local.as_deref().is_some_and(|branch| Some(branch) != incoming.value("branch_id")))
}

fn validate_local_paused_successor(conn: &rusqlite::Connection, head: &SubjectBranchHead) -> Result<()> {
    crate::runtime_boundary::validate_paused_head(conn, head)?;
    let step = crate::store::read_step(conn, &head.branch_id, head.next_step_id)?
        .context("local paused subject successor is missing")?;
    crate::store::validate_step_binding(head, &step)?;
    ensure!(step.state == SubjectStepState::Planned
        && step.active_request_id.is_none() && step.active_attempt_id.is_none(),
        "local paused subject successor is not a clean boundary");
    Ok(())
}

fn record_import_order(
    tx: &Transaction<'_>,
    peer_id: EntityId,
    manifest: &CognitionManifest,
) -> Result<()> {
    tx.execute(
        "INSERT INTO cognition_import_order(source_ledger_id,peer_id,snapshot_seq,high_water,fingerprint)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(source_ledger_id) DO UPDATE SET
            peer_id=excluded.peer_id,
            snapshot_seq=excluded.snapshot_seq,
            high_water=excluded.high_water,
            fingerprint=excluded.fingerprint",
        params![
            manifest.source_ledger_id.to_string(),
            peer_id.to_string(),
            i64::try_from(manifest.source_snapshot_seq)?,
            i64::try_from(manifest.high_water.0)?,
            manifest.fingerprint.to_string(),
        ],
    )?;
    Ok(())
}
