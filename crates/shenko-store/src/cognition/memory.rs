//! Memory intent, immutable binding and entry reconciliation as one conflict unit.
//!
//! Source-proven semantic retirement is the only baseline-free forward move;
//! refused requests cannot publish through their shared entry or sidecar.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction};

use shenko_types::ids::{EntityId, LedgerSeq};

use crate::cognition_archive::CognitionRow;

use super::peers::record_conflict;
use super::rows::{
    TableSpec, effective_row, insert_row, memory_row_value, read_baseline, read_existing,
    rows_equivalent, set_value, spec_for, update_row, write_baseline,
};
use super::{CognitionApplyOutcome, CognitionConflict, db, is_constraint_violation};

#[derive(Clone, Copy)]
enum MemoryRowOutcome {
    Inserted,
    Updated,
    Unchanged,
    Conflict,
}

fn apply_memory_row(
    tx: &Transaction<'_>, spec: &TableSpec, row: &CognitionRow, peer_id: EntityId, seq: LedgerSeq,
) -> Result<MemoryRowOutcome> {
    let local = read_existing(tx, spec, row)?;
    let baseline = read_baseline(tx, peer_id, spec.name, &row.key)?;
    let result = match local {
        None => match insert_row(tx, spec, row, seq) {
            Ok(()) => MemoryRowOutcome::Inserted,
            Err(error) if is_constraint_violation(&error) => return Ok(MemoryRowOutcome::Conflict),
            Err(error) => return Err(error),
        },
        Some(local) if rows_equivalent(spec, &local, &row.values) => MemoryRowOutcome::Unchanged,
        Some(local) if baseline.as_ref().is_some_and(|baseline| rows_equivalent(spec, &local, baseline)) => {
            update_row(tx, spec, row, seq)?;
            MemoryRowOutcome::Updated
        }
        Some(_) => return Ok(MemoryRowOutcome::Conflict),
    };
    write_baseline(tx, peer_id, spec.name, &row.key, &row.values)?;
    Ok(result)
}

/// One memory intent inside an archive conflict unit: the request, its
/// durable binding, the local rows it would replace and the per-row apply
/// results a committed unit reports.
struct MemoryIntentMember {
    key: String,
    parent: CognitionRow,
    binding: Option<CognitionRow>,
    local: Option<Vec<(String, Option<String>)>>,
    local_binding: Option<String>,
    compatible: bool,
    results: [Option<MemoryRowOutcome>; 2],
}

/// Did the unit refuse a peer fact that differs from what this store holds?
/// An identical row kept by the rejection is not a conflict; a refused
/// insertion, update or identity-mismatched request is.
fn memory_member_differs(member: &MemoryIntentMember) -> bool {
    let parent_differs = match member.results[0] {
        Some(MemoryRowOutcome::Conflict | MemoryRowOutcome::Inserted | MemoryRowOutcome::Updated) => true,
        Some(MemoryRowOutcome::Unchanged) => false,
        None => match &member.local {
            None => true,
            Some(local) => local != &member.parent.values,
        },
    };
    let binding_differs = match member.results[1] {
        Some(MemoryRowOutcome::Conflict | MemoryRowOutcome::Inserted | MemoryRowOutcome::Updated) => true,
        Some(MemoryRowOutcome::Unchanged) => false,
        None => match (&member.local_binding, &member.binding) {
            (None, None) => false,
            (Some(_), None) => true,
            (None, Some(incoming)) => incoming.value("data").is_some(),
            (Some(local), Some(incoming)) => incoming.value("data") != Some(local.as_str()),
        },
    };
    parent_differs || binding_differs
}

const MEMORY_RETIREMENT_COLUMNS: &[&str] = &["deleted", "record_revision", "updated_at"];

/// `memory_entries` records a semantic forget as `deleted`, not as a
/// tombstone timestamp, so `row_is_retired` does not describe it.
fn memory_entry_retired(values: &[(String, Option<String>)]) -> bool {
    memory_row_value(values, "deleted").is_some_and(|value| value != "0" && !value.is_empty())
}

/// Is the peer's retirement of this entry source-proven?
///
/// A retirement is not a generic row replacement: the retained copy owns the
/// entry's content, provenance and identity, so only the request that retired
/// the fact may move it forward, and only when every other column already
/// agrees. The archive must therefore carry the peer's own committed `forget`
/// request for this entry — applied in this unit without conflict — the
/// peer's entry must be retired while the local one is live, nothing outside
/// the retirement columns may differ, and the revision must advance by exactly
/// the one that forget commits. An unrelated or concurrent edit still
/// conflicts, and a revival is never admitted.
fn memory_retirement_admitted(
    entry_spec: &TableSpec,
    local: &[(String, Option<String>)],
    entry: &CognitionRow,
    members: &[MemoryIntentMember],
) -> bool {
    if memory_entry_retired(local) || !memory_entry_retired(&entry.values) {
        return false;
    }
    let untouched = entry_spec
        .columns
        .iter()
        .copied()
        .filter(|column| !MEMORY_RETIREMENT_COLUMNS.contains(column))
        .all(|column| memory_row_value(local, column) == entry.value(column));
    if !untouched {
        return false;
    }
    let revision_advanced = match (
        memory_row_value(local, "record_revision").and_then(|value| value.parse::<u64>().ok()),
        entry.value("record_revision").and_then(|value| value.parse::<u64>().ok()),
    ) {
        (Some(local), Some(incoming)) => incoming == local + 1,
        _ => false,
    };
    if !revision_advanced {
        return false;
    }
    members.iter().any(|member| {
        let parent = &member.parent;
        !matches!(member.results[0], Some(MemoryRowOutcome::Conflict))
            && parent.value("kind") == Some("forget")
            && parent.value("state") == Some("committed")
            && parent.value("entry_id") == Some(entry.key.as_str())
            && parent.value("agent_id") == entry.value("agent_id")
    })
}

/// A request, its immutable reference, the produced or targeted entry and
/// every other intent bound to that same entry are reconciled as one atomic
/// conflict unit. A locator-only legacy request cannot acquire another
/// source's digest merely because its intent ID or locator happens to match,
/// and a refused intent can never publish its effect through a sibling: a
/// divergent `forget` cannot hide the entry its `save` parent still
/// protects, and a divergent entry refuses the intents that reference it.
///
/// A retirement the peer provably committed is admitted on top of that rule
/// by `memory_retirement_admitted`.
pub(super) fn apply_memory_pairs(
    tx: &Transaction<'_>, rows: &[&CognitionRow], peer_id: EntityId, seq: LedgerSeq,
) -> Result<CognitionApplyOutcome> {
    let parents = spec_for("memory_intents").expect("memory intent table is registered");
    let sidecars = spec_for("memory_intent_bindings").expect("memory binding table is registered");
    let entry_spec = spec_for("memory_entries").expect("memory entry table is registered");
    let bindings: HashMap<&str, &CognitionRow> = rows.iter()
        .filter(|row| row.table == sidecars.name)
        .map(|row| (row.key.as_str(), *row)).collect();
    let entries: HashMap<&str, &CognitionRow> = rows.iter()
        .filter(|row| row.table == entry_spec.name)
        .map(|row| (row.key.as_str(), *row)).collect();

    // Group intents by the entry they reference: every intent bound to the
    // same `entry_id` shares that entry's fate, so the whole group must
    // reconcile together or not at all. Intents without an entry form their
    // own unit. Grouping follows manifest order so application and conflict
    // reporting stay deterministic.
    let mut units: Vec<(String, Vec<MemoryIntentMember>)> = Vec::new();
    let mut unit_index: HashMap<String, usize> = HashMap::new();
    for row in rows.iter().filter(|row| row.table == parents.name) {
        let mut parent = effective_row(row, parents)?;
        let mut binding = bindings.get(row.key.as_str()).map(|row| effective_row(row, sidecars)).transpose()?;
        let local = read_existing(tx, parents, &parent)?;
        let local_binding: Option<String> = tx.query_row(
            "SELECT data FROM memory_intent_bindings WHERE intent_id=?1",
            [&row.key], |record| record.get(0),
        ).optional().map_err(db)?;
        let mut compatible = true;
        if let Some(local) = &local {
            for column in ["agent_id", "kind", "scope", "content", "entry_id", "created_at_ms"] {
                compatible &= memory_row_value(local, column) == parent.value(column);
            }
            let local_state = memory_row_value(local, "state");
            let incoming_state = parent.value("state");
            compatible &= match local_state {
                Some("accepted") => matches!(incoming_state, Some("accepted" | "committed" | "failed")),
                Some("committed" | "failed") => local_state == incoming_state
                    && memory_row_value(local, "reason") == parent.value("reason")
                    && memory_row_value(local, "updated_at_ms") == parent.value("updated_at_ms"),
                _ => false,
            };
            let local_locator = memory_row_value(local, "content_ref");
            match (local_binding.as_deref(), binding.as_mut()) {
                (None, None) => {
                    // The dedicated subject representation keeps no binding
                    // sidecar; compare the content-addressed locator tail,
                    // never the node-local path (docs/specs/memory.md#spec-12-2: a locator may
                    // change while the content identity stays).
                    compatible &= match (local_locator, parent.value("content_ref")) {
                        (None, None) => true,
                        (Some(left), Some(right)) => {
                            crate::memory::portable_locator(left)
                                == crate::memory::portable_locator(right)
                        }
                        _ => false,
                    };
                }
                (Some(local_data), Some(incoming)) => {
                    let incoming_data = incoming.value("data");
                    compatible &= crate::memory::memory_binding_matches_locator(local_data, local_locator)
                        && incoming_data.is_some_and(|data| crate::memory::same_memory_bindings(local_data, data));
                    if let Some(baseline) = read_baseline(tx, peer_id, sidecars.name, &incoming.key)? {
                        compatible &= memory_row_value(&baseline, "data")
                            .is_some_and(|data| crate::memory::same_memory_bindings(local_data, data));
                    }
                    if compatible {
                        set_value(&mut parent, "content_ref", local_locator.map(str::to_owned));
                        set_value(incoming, "data", Some(local_data.to_owned()));
                    }
                }
                _ => compatible = false,
            }
        } else if local_binding.is_some() {
            compatible = false;
        }

        let unit_key = parent.value("entry_id").unwrap_or(row.key.as_str()).to_string();
        let index = *unit_index.entry(unit_key.clone()).or_insert_with(|| {
            units.push((unit_key, Vec::new()));
            units.len() - 1
        });
        units[index].1.push(MemoryIntentMember {
            key: row.key.clone(),
            parent,
            binding,
            local,
            local_binding,
            compatible,
            results: [None; 2],
        });
    }

    let mut outcome = CognitionApplyOutcome::default();
    for (entry_key, members) in &mut units {
        let entry = entries
            .get(entry_key.as_str())
            .map(|row| effective_row(row, entry_spec))
            .transpose()?;
        // One savepoint covers the entry and every referencing intent: any
        // refused member, or an entry the peer changed past its delivered
        // baseline, rolls the whole unit back so no partial row or baseline
        // mirror survives.
        let mut rejected = members.iter().any(|member| !member.compatible);
        let mut entry_result = None;
        if !rejected {
            tx.execute("SAVEPOINT memory_intent_unit", []).map_err(db)?;
            for member in members.iter_mut() {
                member.results[0] = Some(apply_memory_row(tx, parents, &member.parent, peer_id, seq)?);
                if !matches!(member.results[0], Some(MemoryRowOutcome::Conflict)) {
                    if let Some(binding) = &member.binding {
                        member.results[1] = Some(apply_memory_row(tx, sidecars, binding, peer_id, seq)?);
                    }
                }
            }
            if members.iter().all(|member| {
                member.results.iter().all(|result| !matches!(result, Some(MemoryRowOutcome::Conflict)))
            }) {
                if let Some(entry) = &entry {
                    entry_result = Some(apply_memory_row(tx, entry_spec, entry, peer_id, seq)?);
                    // The generic rule refuses any differing row it has no
                    // delivered baseline for. A retirement the peer committed
                    // is the one forward move a retained copy may accept
                    // without one: it changes nothing but the retirement and
                    // travels with the request that performed it.
                    if matches!(entry_result, Some(MemoryRowOutcome::Conflict)) {
                        if let Some(local) = read_existing(tx, entry_spec, entry)? {
                            if memory_retirement_admitted(entry_spec, &local, entry, members) {
                                match update_row(tx, entry_spec, entry, seq) {
                                    Ok(()) => {
                                        write_baseline(
                                            tx,
                                            peer_id,
                                            entry_spec.name,
                                            &entry.key,
                                            &entry.values,
                                        )?;
                                        entry_result = Some(MemoryRowOutcome::Updated);
                                    }
                                    Err(error) if is_constraint_violation(&error) => {}
                                    Err(error) => return Err(error),
                                }
                            }
                        }
                    }
                }
            }
            rejected = entry_result.is_some_and(|result| matches!(result, MemoryRowOutcome::Conflict))
                || members.iter().any(|member| {
                    member.results.iter().any(|result| matches!(result, Some(MemoryRowOutcome::Conflict)))
                });
            if rejected {
                tx.execute("ROLLBACK TO memory_intent_unit", []).map_err(db)?;
            }
            tx.execute("RELEASE memory_intent_unit", []).map_err(db)?;
        }
        if !rejected {
            for member in members.iter() {
                for result in member.results.into_iter().flatten() {
                    match result {
                        MemoryRowOutcome::Inserted => outcome.inserted += 1,
                        MemoryRowOutcome::Updated => outcome.updated += 1,
                        MemoryRowOutcome::Unchanged => outcome.unchanged += 1,
                        MemoryRowOutcome::Conflict => unreachable!(),
                    }
                }
            }
            match entry_result {
                Some(MemoryRowOutcome::Inserted) => outcome.inserted += 1,
                Some(MemoryRowOutcome::Updated) => outcome.updated += 1,
                Some(MemoryRowOutcome::Unchanged) => outcome.unchanged += 1,
                Some(MemoryRowOutcome::Conflict) | None => {}
            }
        } else {
            for member in members.iter() {
                if !memory_member_differs(member) {
                    continue;
                }
                let detail = "memory intent and its reference conflict; the local identity is kept".to_string();
                record_conflict(tx, peer_id, parents.name, &member.key, &detail)?;
                outcome.conflicts.push(CognitionConflict {
                    table: parents.name.into(),
                    key: member.key.clone(),
                    detail,
                });
            }
            if let Some(entry) = &entry {
                let entry_differs = match entry_result {
                    Some(MemoryRowOutcome::Conflict | MemoryRowOutcome::Inserted | MemoryRowOutcome::Updated) => true,
                    Some(MemoryRowOutcome::Unchanged) => false,
                    None => match read_existing(tx, entry_spec, entry)? {
                        None => true,
                        Some(local) => local != entry.values,
                    },
                };
                if entry_differs {
                    let detail = "the memory entry reconciles with its intents; the local value is kept".to_string();
                    record_conflict(tx, peer_id, entry_spec.name, &entry.key, &detail)?;
                    outcome.conflicts.push(CognitionConflict {
                        table: entry_spec.name.into(),
                        key: entry.key.clone(),
                        detail,
                    });
                }
            }
        }
    }
    Ok(outcome)
}
