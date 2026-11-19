//! Per-class merge rules and complete semantic conflict units.
//!
//! Each group borrows the import transaction and owns one savepoint. A rejected
//! member keeps the whole local unit, including its baselines and delivery facts.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, anyhow, ensure};
use rusqlite::{OptionalExtension, Transaction, params};

use shenko_types::ids::{EntityId, LedgerSeq};

use crate::cognition_archive::{CognitionManifest, CognitionRow};

use super::peers::{record_conflict, record_origin};
use super::rows::{
    RowKind, TABLES, TableSpec, effective_row, insert_row, memory_row_value, read_baseline,
    read_existing, rows_equivalent, spec_for, update_row, write_baseline,
};
use super::{CognitionApplyOutcome, CognitionConflict, db, is_constraint_violation};

#[derive(Clone, Copy)]
pub(super) enum RecoveryFactOutcome {
    Inserted,
    Updated,
    Unchanged,
    Conflict,
}

/// The columns that carry a retirement marker on a monotone fact class.
const TOMBSTONE_COLUMNS: &[&str] = &["tombstoned_at_ms", "deleted_at_ms", "superseded_by"];

/// The merge of a peer fact that diverges from the local one, for the fact
/// classes whose OWN rule decides the outcome. `None` means the class has
/// no merge rule: the local fact stays and the divergence is a conflict.
///
/// Two classes differ from a verbatim copy:
///   * a retirement is monotone — a peer may retire a live local fact and
///     may never revive a retired one;
///   * a record with its own revision is ordered by that revision, so the
///     peer advances the record only with a strictly greater one. Which
///     peer answered first never decides.
fn merge_diverged_fact(
    tx: &Transaction<'_>,
    spec: &TableSpec,
    row: &CognitionRow,
    local: &[(String, Option<String>)],
    seq: LedgerSeq,
) -> Result<Option<RecoveryFactOutcome>> {
    match spec.kind {
        RowKind::Monotonic => {
            let local_retired = row_is_retired(local);
            let peer_retired = row_is_retired(&row.values);
            if !peer_retired || local_retired {
                // A retired local fact keeps its retirement, and two
                // different retirements of the same fact keep the first.
                return Ok(None);
            }
            match update_row(tx, spec, row, seq) {
                Ok(()) => Ok(Some(RecoveryFactOutcome::Updated)),
                Err(error) if is_constraint_violation(&error) => Ok(None),
                Err(error) => Err(error),
            }
        }
        RowKind::Revisioned => {
            let column = revision_column(spec);
            let local_revision = column.and_then(|column| {
                local
                    .iter()
                    .find(|(name, _)| name == column)
                    .and_then(|(_, value)| value.as_deref())
                    .and_then(|value| value.parse::<u64>().ok())
            });
            let peer_revision = column
                .and_then(|column| row.value(column))
                .and_then(|value| value.parse::<u64>().ok());
            let ahead = match (local_revision, peer_revision) {
                (Some(local_revision), Some(peer_revision)) => peer_revision > local_revision,
                _ => false,
            };
            if !ahead {
                // The peer is behind, or both records claim the same
                // revision with different content: the local record stays
                // and the divergence is reported.
                return Ok(None);
            }
            match update_row(tx, spec, row, seq) {
                Ok(()) => Ok(Some(RecoveryFactOutcome::Updated)),
                Err(error) if is_constraint_violation(&error) => Ok(None),
                Err(error) => Err(error),
            }
        }
        RowKind::Ladder => {
            let Some(ladder) = state_ladder(spec) else {
                return Ok(None);
            };
            if !ladder_advances(ladder, local, row) {
                return Ok(None);
            }
            match update_row(tx, spec, row, seq) {
                Ok(()) => Ok(Some(RecoveryFactOutcome::Updated)),
                Err(error) if is_constraint_violation(&error) => Ok(None),
                Err(error) => Err(error),
            }
        }
        _ => Ok(None),
    }
}

/// The monotone state ladder of a fact class: the states in the order they
/// may be reached. A peer may move a row forward along it and never
/// backward, and a terminal state is never rewritten.
fn state_ladder(spec: &TableSpec) -> Option<&'static [&'static str]> {
    match spec.name {
        // The subject arms a request before any wire send, the broker's
        // send boundary flips it to dispatched exactly once, and the
        // terminal provider evidence ends it.
        "subject_request_attempts" => Some(&["armed", "dispatched", "terminal"]),
        // Obligations use the branching rule in apply_recovery_fact.
        _ => None,
    }
}

/// Does the peer move this row forward along its ladder? A state the class
/// does not know is never admissible, and a terminal state is never left.
fn ladder_advances(
    ladder: &[&str],
    local: &[(String, Option<String>)],
    row: &CognitionRow,
) -> bool {
    let peer_state = row.value("state").unwrap_or_default();
    let Some(peer_index) = ladder
        .iter()
        .position(|state| *state == ladder_state(peer_state))
    else {
        return false;
    };
    let local_state = local
        .iter()
        .find(|(name, _)| name == "state")
        .and_then(|(_, value)| value.as_deref())
        .unwrap_or_default();
    match ladder
        .iter()
        .position(|state| *state == ladder_state(local_state))
    {
        Some(local_index) => peer_index > local_index,
        None => true,
    }
}

/// The stored state text of a ladder class. The state is stored as its
/// serialized form, so the same ladder must read both the bare name and
/// the quoted one.
fn ladder_state(value: &str) -> &str {
    value.trim().trim_matches('"')
}

/// Is this row a retirement of its fact?
fn row_is_retired(values: &[(String, Option<String>)]) -> bool {
    values.iter().any(|(name, value)| {
        TOMBSTONE_COLUMNS
            .iter()
            .any(|column| *column == name.as_str())
            && value.as_deref().is_some_and(|value| !value.is_empty())
    })
}

/// The explicit revision column of a revisioned fact class.
fn revision_column(spec: &TableSpec) -> Option<&'static str> {
    ["record_revision", "revision", "selector_revision", "created_seq"]
        .into_iter()
        .find(|name| spec.columns.contains(name))
}

fn media_fact_compatible(local: &[(String, Option<String>)], incoming: &CognitionRow) -> bool {
    let value = |name: &str| local.iter().find(|(column, _)| column == name)
        .and_then(|(_, value)| value.as_deref());
    local.iter().all(|(column, local)| {
        let immutable = match incoming.table.as_str() {
            "inbox_events" => column != "consumed",
            "media_stream_ends" => true,
            "media_recording_controls" | "media_priority_inputs" => true,
            "media_recordings" => matches!(column.as_str(),
                "stream_key" | "agent_id" | "recording_id" | "started_at_ms"
                    | "start_control_id" | "scope_json")
                || (matches!(value("state"), Some("stopped" | "failed"))
                    && matches!(column.as_str(),
                        "state" | "stopped_at_ms" | "stop_control_id" | "reason")),
            "media_segment_revisions" => column != "superseded_by_revision" || local.is_some(),
            "media_streams" => matches!(column.as_str(),
                "stream_key" | "agent_id" | "provider" | "account" | "stream_id" | "opened_at_ms")
                || (value("open_id").is_some() && matches!(column.as_str(),
                    "open_id" | "open_event_id" | "open_commit_ref" | "descriptor"))
                || (matches!(value("state"), Some("ended" | "cancelled"))
                    && matches!(column.as_str(), "state" | "end_id" | "end_event_id" | "ended_at_ms" | "end_reason")),
            _ => false,
        };
        !immutable || local.as_deref() == incoming.value(column)
    }) && !(incoming.table == "media_streams" && value("state") == Some("disconnected")
        && incoming.value("state") == Some("open"))
}

pub(super) fn apply_recovery_fact(
    tx: &Transaction<'_>, spec: &TableSpec, row: &CognitionRow, peer_id: EntityId, seq: LedgerSeq,
) -> Result<RecoveryFactOutcome> {
    let local = read_existing(tx, spec, row)?;
    if matches!(spec.name, "media_streams" | "media_stream_ends" | "media_segment_revisions"
        | "media_recordings" | "media_recording_controls" | "media_priority_inputs" | "inbox_events")
        && local.as_ref().is_some_and(|local| !media_fact_compatible(local, row))
    {
        return Ok(RecoveryFactOutcome::Conflict);
    }
    if spec.name == "subject_obligations" {
        ensure!(matches!(row.value("state"), Some("open" | "completed" | "cancelled")), "invalid obligation state");
        if let Some(local) = &local {
            let immutable = local.iter().all(|(name, value)| name == "state" || value.as_deref() == row.value(name));
            let state = memory_row_value(local, "state");
            if !immutable || !(state == row.value("state")
                || (state == Some("open") && matches!(row.value("state"), Some("completed" | "cancelled"))))
            {
                return Ok(RecoveryFactOutcome::Conflict);
            }
            let result = if state == row.value("state") { RecoveryFactOutcome::Unchanged } else {
                tx.execute("UPDATE subject_obligations SET state=?2,updated_seq=?3 WHERE obligation_id=?1",
                    params![row.key, row.value("state"), seq.0 as i64]).map_err(db)?;
                RecoveryFactOutcome::Updated
            };
            write_baseline(tx, peer_id, spec.name, &row.key, &row.values)?;
            return Ok(result);
        }
    }
    let baseline = read_baseline(tx, peer_id, spec.name, &row.key)?;
    let result = match local {
        None => match insert_row(tx, spec, row, seq) {
            Ok(()) => RecoveryFactOutcome::Inserted,
            Err(error) if is_constraint_violation(&error) => return Ok(RecoveryFactOutcome::Conflict),
            Err(error) => return Err(error),
        },
        Some(local) if rows_equivalent(spec, &local, &row.values) => RecoveryFactOutcome::Unchanged,
        Some(local) => {
            // A class with a monotone state ladder moves forward only: the
            // peer may advance the row, never rewind it, and a terminal
            // state is never rewritten.
            if let Some(ladder) = state_ladder(spec) {
                if !ladder_advances(ladder, &local, row) {
                    return Ok(RecoveryFactOutcome::Conflict);
                }
            }
            if baseline
                .as_ref()
                .is_some_and(|baseline| rows_equivalent(spec, &local, baseline))
            {
                match update_row(tx, spec, row, seq) {
                    Ok(()) => {}
                    Err(error) if is_constraint_violation(&error) => {
                        return Ok(RecoveryFactOutcome::Conflict)
                    }
                    Err(error) => return Err(error),
                }
                RecoveryFactOutcome::Updated
            } else {
                match merge_diverged_fact(tx, spec, row, &local, seq)? {
                    Some(outcome) => outcome,
                    None => return Ok(RecoveryFactOutcome::Conflict),
                }
            }
        }
    };
    write_baseline(tx, peer_id, spec.name, &row.key, &row.values)?;
    Ok(result)
}

/// Merge the authored note journals and (in a handover) the rollover
/// requests.
///
/// A scope's note journal is one conflict unit: every revision both sides
/// hold must be byte-identical, and only then are the incoming revisions
/// beyond the local head appended. A divergent revision refuses the whole
/// incoming journal of that scope, so a peer tail never lands on a different
/// local history. Revision numbers are kept exactly, so compare-and-append
/// continues on the recovered head.
///
/// A rollover request may only move forward: the same request gains the
/// window that answered it. A different request, or an answer that would be
/// rewound or replaced, is a conflict and the local row is kept — an archive
/// cannot discard a boundary this installation still owes the subject.
pub(super) fn apply_context_notebook(
    tx: &Transaction<'_>,
    rows: &[&CognitionRow],
) -> Result<CognitionApplyOutcome> {
    let mut outcome = CognitionApplyOutcome::default();
    let mut journals: BTreeMap<&str, Vec<&CognitionRow>> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.table == "context_notes") {
        journals.entry(row.value("scope_id").unwrap_or_default()).or_default().push(row);
    }
    for (scope, incoming) in journals {
        let mut local: HashMap<i64, (String, String, i64, i64)> = HashMap::new();
        {
            let mut stmt = tx
                .prepare(
                    "SELECT revision, body, source, written_at_ms, visible
                     FROM context_notes WHERE scope_id = ?1",
                )
                .map_err(db)?;
            let mut cursor = stmt.query([scope]).map_err(db)?;
            while let Some(row) = cursor.next().map_err(db)? {
                local.insert(
                    row.get(0).map_err(db)?,
                    (row.get(1).map_err(db)?, row.get(2).map_err(db)?, row.get(3).map_err(db)?, row.get(4).map_err(db)?),
                );
            }
        }
        let mut parsed = Vec::with_capacity(incoming.len());
        for row in &incoming {
            let int = |column: &str| -> Result<i64> {
                row.value(column)
                    .unwrap_or_default()
                    .parse()
                    .map_err(|_| anyhow!("invalid context_notes {column} for scope {scope}"))
            };
            parsed.push((
                int("revision")?,
                row.value("body").unwrap_or_default().to_string(),
                row.value("source").unwrap_or_default().to_string(),
                int("written_at_ms")?,
                int("visible")?,
            ));
        }
        let divergent = parsed.iter().find(|(revision, body, source, at, visible)| {
            local
                .get(revision)
                .is_some_and(|existing| existing != &(body.clone(), source.clone(), *at, *visible))
        });
        if let Some((revision, ..)) = divergent {
            outcome.conflicts.push(CognitionConflict {
                table: "context_notes".into(),
                key: scope.to_string(),
                detail: format!(
                    "authored revision {revision} differs from the local journal; \
                     the incoming journal of this scope was not applied"
                ),
            });
            continue;
        }
        for (revision, body, source, at, visible) in parsed {
            if local.contains_key(&revision) {
                outcome.unchanged += 1;
                continue;
            }
            tx.execute(
                "INSERT INTO context_notes(scope_id, revision, body, source, written_at_ms, visible)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![scope, revision, body, source, at, visible],
            )
            .map_err(db)?;
            outcome.inserted += 1;
        }
    }

    for row in rows.iter().filter(|row| row.table == "context_rollover_requests") {
        let scope = row.value("scope_id").unwrap_or_default();
        let request_id = row.value("request_id").unwrap_or_default();
        let requested_at: i64 = row
            .value("requested_at_ms")
            .unwrap_or_default()
            .parse()
            .map_err(|_| anyhow!("invalid rollover request time for scope {scope}"))?;
        let consumed = row.value("consumed_window").filter(|value| !value.is_empty());
        let local: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT request_id, consumed_window FROM context_rollover_requests WHERE scope_id = ?1",
                [scope],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db)?;
        let conflict = |detail: &str| CognitionConflict {
            table: "context_rollover_requests".into(),
            key: scope.to_string(),
            detail: detail.to_string(),
        };
        match local {
            None => {
                tx.execute(
                    "INSERT INTO context_rollover_requests(scope_id, request_id, requested_at_ms, consumed_window)
                     VALUES(?1, ?2, ?3, ?4)",
                    rusqlite::params![scope, request_id, requested_at, consumed],
                )
                .map_err(db)?;
                outcome.inserted += 1;
            }
            Some((local_request, _)) if local_request != request_id => {
                outcome.conflicts.push(conflict(
                    "a different rollover request is recorded locally; it is kept",
                ));
            }
            Some((_, local_consumed)) => match (local_consumed.as_deref(), consumed) {
                (a, b) if a == b => outcome.unchanged += 1,
                (None, Some(window)) => {
                    tx.execute(
                        "UPDATE context_rollover_requests SET consumed_window = ?2 WHERE scope_id = ?1",
                        rusqlite::params![scope, window],
                    )
                    .map_err(db)?;
                    outcome.updated += 1;
                }
                _ => outcome.conflicts.push(conflict(
                    "the local answer to this rollover request is final; it is kept",
                )),
            },
        }
    }
    Ok(outcome)
}

/// The tables that belong to a multi-row semantic conflict unit. Rows of a
/// unit are applied inside one savepoint: if any member conflicts, the
/// WHOLE unit stays local, so a peer can never graft half a bound effect
/// (a lease without its seat, a send without its request, a question's
/// decision without its question).
fn unit_key_for<'a>(
    row: &'a CognitionRow,
    question_seats: &HashMap<&'a str, String>,
) -> Option<String> {
    match row.table.as_str() {
        "research_epochs" => Some(format!("epoch/{}", row.key)),
        "research_candidates" | "intervention_bank_entries" | "intervention_reminders" => {
            row.value("epoch_id").map(|epoch| format!("epoch/{epoch}"))
        }
        "seat_records" => Some(format!("seat/{}", row.key)),
        "seat_leases" | "seat_questions" => {
            row.value("seat_id").map(|seat| format!("seat/{seat}"))
        }
        "seat_answer_decisions" => row
            .value("question_id")
            .and_then(|question| question_seats.get(question))
            .map(|seat| format!("seat/{seat}")),
        "quota_attempts" | "quota_sends" => row
            .value("request_id")
            .map(|request| format!("quota_request/{request}")),
        "quota_events" => Some(format!("quota_event/{}", row.key)),
        "retention_tombstones" => Some(format!("tombstone/{}", row.key)),
        "web_input_submissions" => Some(format!(
            "web_input/{}/{}",
            row.value("agent_id").unwrap_or_default(),
            row.value("submission_id").unwrap_or_default()
        )),
        "web_input_withdrawals" => Some(format!(
            "web_input/{}/{}",
            row.value("agent_id").unwrap_or_default(),
            row.value("submission_id").unwrap_or_default()
        )),
        "channel_runtime_facts" => Some(format!("channel_fact/{}", row.key)),
        "channel_record_migrations" => Some(format!(
            "channel_migration/{}/{}",
            row.value("record_kind").unwrap_or_default(),
            row.value("record_id").unwrap_or_default()
        )),
        "subject_request_attempts" => Some(format!("request/{}", row.key)),
        "subject_obligations" => Some(format!("obligation/{}", row.key)),
        "subject_attention_policies" => Some(format!("attention/{}", row.key)),
        "person_claims" => Some(format!(
            "person/{}/{}",
            row.value("agent_id").unwrap_or_default(),
            row.value("person_id").unwrap_or_default()
        )),
        "person_identity_events" => Some(format!(
            "person/{}/{}",
            row.value("agent_id").unwrap_or_default(),
            row.value("source_person_id").unwrap_or_default()
        )),
        "admin_facts" => (row.value("subject_kind") == Some("person")).then(|| {
            format!(
                "person/{}/{}",
                row.value("agent_id").unwrap_or_default(),
                row.value("subject_key").unwrap_or_default()
            )
        }),
        "activation_records" | "activation_selectors" | "activation_launch_receipts" => row
            .value("agent_id")
            .map(|agent| format!("activation/{agent}")),
        "subject_output_parts" => Some(format!(
            "output/{}/{}/{}",
            row.value("agent_id").unwrap_or_default(),
            row.value("stream_id").unwrap_or_default(),
            row.value("message_id").unwrap_or_default()
        )),
        "content_streams" => Some(format!("stream/{}", row.key)),
        "content_stream_units" => row
            .value("stream_id")
            .map(|stream| format!("stream/{stream}")),
        "media_recordings" | "media_recording_controls" | "media_priority_inputs" => row
            .value("stream_key")
            .map(|stream| format!("media/{stream}")),
        "artifact_records" => Some(format!("artifact/{}", row.key)),
        "artifact_manifests" | "artifact_runtime_instances" => row
            .value("artifact_id")
            .map(|artifact| format!("artifact/{artifact}")),
        "owner_grants" => Some(format!("grant/{}", row.key)),
        "config_entries" => Some(format!(
            "config/{}/{}",
            row.value("namespace").unwrap_or_default(),
            row.value("logical_key").unwrap_or_default()
        )),
        "config_changes" | "config_adoptions" => Some(format!(
            "config/{}/{}",
            row.value("namespace").unwrap_or_default(),
            row.value("logical_key").unwrap_or_default()
        )),
        _ => None,
    }
}

/// True when the row's table participates in a semantic conflict unit.
pub(super) fn is_unit_table(table: &str) -> bool {
    matches!(
        table,
        "research_epochs"
            | "research_candidates"
            | "intervention_bank_entries"
            | "intervention_reminders"
            | "retention_tombstones"
            | "seat_records"
            | "seat_leases"
            | "seat_questions"
            | "seat_answer_decisions"
            | "quota_attempts"
            | "quota_sends"
            | "quota_events"
            | "channel_runtime_facts"
            | "channel_record_migrations"
            | "person_claims"
            | "person_identity_events"
            | "admin_facts"
            | "activation_records"
            | "activation_selectors"
            | "activation_launch_receipts"
            | "subject_output_parts"
            | "content_streams"
            | "content_stream_units"
            | "media_recordings"
            | "media_recording_controls"
            | "media_priority_inputs"
            | "artifact_records"
            | "artifact_manifests"
            | "artifact_runtime_instances"
            | "owner_grants"
            | "config_entries"
            | "config_changes"
            | "config_adoptions"
            | "subject_request_attempts"
            | "subject_obligations"
            | "subject_attention_policies"
    )
}

/// Apply every semantic unit of this archive atomically. Each unit is one
/// savepoint; a member that diverges from the local fact rejects the whole
/// unit and every member is reported as a conflict, mirroring the task
/// closure semantics. Returns the outcome plus every row the pass owned.
pub(super) fn apply_semantic_units<'a>(
    tx: &Transaction<'_>,
    rows: &[&'a CognitionRow],
    peer_id: EntityId,
    seq: LedgerSeq,
    manifest: &CognitionManifest,
) -> Result<(CognitionApplyOutcome, HashSet<(&'a str, &'a str)>)> {
    // Answer decisions bind to their question's seat; questions that are not
    // in this archive resolve through the local store.
    let mut question_seats: HashMap<&'a str, String> = HashMap::new();
    for row in rows
        .iter()
        .copied()
        .filter(|row| row.table == "seat_questions")
    {
        if let Some(seat) = row.value("seat_id") {
            question_seats.insert(row.key.as_str(), seat.to_string());
        }
    }
    for row in rows
        .iter()
        .copied()
        .filter(|row| row.table == "seat_answer_decisions")
    {
        let Some(question) = row.value("question_id") else { continue };
        if question_seats.contains_key(question) {
            continue;
        }
        let seat: Option<String> = tx
            .query_row(
                "SELECT seat_id FROM seat_questions WHERE question_id=?1",
                [question],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)?;
        if let Some(seat) = seat {
            question_seats.insert(question, seat);
        }
    }

    let mut groups: HashMap<String, Vec<&'a CognitionRow>> = HashMap::new();
    let mut owned: HashSet<(&'a str, &'a str)> = HashSet::new();
    for row in rows.iter().copied().filter(|row| is_unit_table(&row.table)) {
        let key = unit_key_for(row, &question_seats)
            .unwrap_or_else(|| format!("unbound/{}/{}", row.table, row.key));
        groups.entry(key).or_default().push(row);
        owned.insert((row.table.as_str(), row.key.as_str()));
    }

    let mut outcome = CognitionApplyOutcome::default();
    for (unit, mut group) in groups {
        // The same fixed SQL identifier is reused only after RELEASE.
        tx.execute("SAVEPOINT semantic_unit", []).map_err(db)?;
        let mut conflict = false;
        let mut part = CognitionApplyOutcome::default();
        group.sort_by_key(|row| TABLES.iter().position(|spec| spec.name == row.table));
        for row in &group {
            let Some(spec) = spec_for(&row.table) else {
                conflict = true;
                break;
            };
            let effective = effective_row(row, spec)?;
            match apply_recovery_fact(tx, spec, &effective, peer_id, seq)? {
                RecoveryFactOutcome::Conflict => {
                    conflict = true;
                    break;
                }
                RecoveryFactOutcome::Inserted => part.inserted += 1,
                RecoveryFactOutcome::Updated => part.updated += 1,
                RecoveryFactOutcome::Unchanged => {
                    part.unchanged += 1;
                    continue;
                }
            }
            record_origin(
                tx,
                spec,
                &effective,
                peer_id,
                &manifest.fingerprint,
                manifest.source_ledger_id,
            )?;
        }
        if conflict {
            tx.execute("ROLLBACK TO semantic_unit", []).map_err(db)?;
        }
        tx.execute("RELEASE semantic_unit", []).map_err(db)?;
        if conflict {
            for row in group {
                let detail = format!(
                    "conflict unit {unit} diverges from the local fact; the complete local unit is kept"
                );
                record_conflict(tx, peer_id, &row.table, &row.key, &detail)?;
                outcome.conflicts.push(CognitionConflict {
                    table: row.table.clone(),
                    key: row.key.clone(),
                    detail,
                });
            }
        } else {
            outcome.inserted += part.inserted;
            outcome.updated += part.updated;
            outcome.unchanged += part.unchanged;
        }
    }
    Ok((outcome, owned))
}

pub(super) fn apply_media_streams<'a>(
    tx: &Transaction<'_>, rows: &[&'a CognitionRow], peer_id: EntityId,
    seq: LedgerSeq, manifest: &CognitionManifest,
) -> Result<(CognitionApplyOutcome, HashSet<&'a str>)> {
    let inbox: HashMap<&'a str, &'a CognitionRow> = rows.iter().copied()
        .filter(|row| row.table == "inbox_events")
        .map(|row| (row.key.as_str(), row)).collect();
    let mut groups: HashMap<&'a str, Vec<&'a CognitionRow>> = HashMap::new();
    let mut media_events: HashSet<&'a str> = HashSet::new();
    for row in rows.iter().copied().filter(|row| matches!(row.table.as_str(),
        "media_streams" | "media_stream_ends" | "media_segment_revisions"
        | "media_recordings" | "media_recording_controls" | "media_priority_inputs"))
    {
        let key = row.value("stream_key").context("media stream identity missing")?;
        let group = groups.entry(key).or_default();
        group.push(row);
        if row.table == "media_recordings" {
            // The latest-state projection re-binds the event of its latest
            // control notice; the notice owns the delivery identity and its
            // durable original.
            continue;
        }
        if let Some(id) = row.value(if row.table == "media_streams" { "open_event_id" } else { "event_id" }) {
            ensure!(media_events.insert(id), "media facts share a delivery identity");
            group.push(*inbox.get(id).context("media delivery original missing")?);
        }
    }
    let mut outcome = CognitionApplyOutcome::default();
    for stream in rows.iter().copied().filter(|row| row.table == "media_streams") {
        let mut group = groups.remove(stream.key.as_str()).context("media closure group missing")?;
        group.sort_by_key(|row| TABLES.iter().position(|spec| spec.name == row.table));
        tx.execute("SAVEPOINT media_stream_closure", []).map_err(db)?;
        let mut part = CognitionApplyOutcome::default();
        let mut conflict = false;
        for row in group {
            let spec = spec_for(&row.table).context("unregistered media recovery table")?;
            match apply_recovery_fact(tx, spec, row, peer_id, seq)? {
                RecoveryFactOutcome::Conflict => { conflict = true; break; }
                RecoveryFactOutcome::Inserted => part.inserted += 1,
                RecoveryFactOutcome::Updated => part.updated += 1,
                RecoveryFactOutcome::Unchanged => { part.unchanged += 1; continue; }
            }
            record_origin(tx, spec, row, peer_id, &manifest.fingerprint, manifest.source_ledger_id)?;
        }
        if conflict {
            tx.execute("ROLLBACK TO media_stream_closure", []).map_err(db)?;
        }
        tx.execute("RELEASE media_stream_closure", []).map_err(db)?;
        if conflict {
            let detail = "media stream facts and pending deliveries conflict; the complete local binding is kept".to_owned();
            record_conflict(tx, peer_id, "media_streams", &stream.key, &detail)?;
            outcome.conflicts.push(CognitionConflict {
                table: "media_streams".into(), key: stream.key.clone(), detail,
            });
        } else {
            outcome.inserted += part.inserted;
            outcome.updated += part.updated;
            outcome.unchanged += part.unchanged;
        }
    }
    Ok((outcome, media_events))
}
