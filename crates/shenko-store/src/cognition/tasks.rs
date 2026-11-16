//! Task execution/delivery conflict closures and dependent-consumer refusal.
//!
//! A rejected task cannot survive through staged input, a sealed S0 or a context
//! window. These checks run inside the import transaction without rewriting it.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Transaction};

use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::Digest;

use crate::cognition_archive::{CognitionDerivative, CognitionManifest, CognitionRow};
use crate::store::Store;

use super::peers::{record_conflict, record_origin};
use super::reconcile::{RecoveryFactOutcome, apply_recovery_fact};
use super::rows::{TABLES, effective_row, memory_row_value, read_existing, spec_for};
use super::{CognitionApplyOutcome, CognitionConflict, db};

fn task_row_json(row: &CognitionRow) -> Option<serde_json::Value> {
    row.value("data").and_then(|data| serde_json::from_str(data).ok())
}

/// The exact task-owned event shapes produced by native T2 / Playground.
/// No recursive keyword search: an arbitrary message mentioning a task id
/// must remain an independent observation.
fn task_delivery_owner(event: &serde_json::Value) -> Option<String> {
    let source = event.get("source")?.as_str()?;
    let payload = event.get("payload")?;
    if (source == "native-child" && event.get("kind")?.as_str()? == "task_result")
        || source == "playground"
    {
        payload.get("task_id")?.as_str().map(str::to_owned)
    } else { None }
}

/// Complete task execution AND delivery facts form one conflict unit.
/// Parentless executions are rejected, never grafted onto a local task merely
/// because its deterministic id happens to match. Pending and already-staged
/// T2 text is applied inside the same savepoint as the original sealed input.
pub(super) fn apply_task_closures<'a>(
    tx: &Transaction<'_>,
    rows: &[&'a CognitionRow],
    peer_id: EntityId,
    seq: LedgerSeq,
    manifest: &CognitionManifest,
) -> Result<(
    CognitionApplyOutcome,
    HashSet<(&'a str, &'a str)>,
    HashSet<(&'a str, &'a str)>,
    HashSet<String>,
)> {
    fn add<'a>(
        groups: &mut HashMap<String, Vec<&'a CognitionRow>>,
        owned: &mut HashSet<(&'a str, &'a str)>, row: &'a CognitionRow, task: Option<String>,
    ) -> String {
        let task = task.unwrap_or_else(|| format!("unbound/{}/{}", row.table, row.key));
        groups.entry(task.clone()).or_default().push(row);
        owned.insert((row.table.as_str(), row.key.as_str()));
        task
    }
    let task_spec = spec_for("tasks").expect("task table is registered");
    let tasks: HashMap<&str, &CognitionRow> = rows.iter().copied().filter(|r| r.table == "tasks")
        .map(|row| (row.key.as_str(), row)).collect();
    let mut groups = HashMap::new();
    let mut owned = HashSet::new();
    for row in tasks.values().copied() {
        add(&mut groups, &mut owned, row, Some(row.key.clone()));
    }
    let mut operation_owners = HashMap::new();
    for row in rows.iter().copied().filter(|row| row.table == "operations") {
        let owner = task_row_json(row).and_then(|data| data.get("task_id")?.as_str().map(str::to_owned));
        let task = add(&mut groups, &mut owned, row, owner);
        operation_owners.insert(row.key.as_str(), task);
    }
    for row in rows.iter().copied().filter(|row| matches!(row.table.as_str(), "attempts" | "evidence_receipts" | "task_results")) {
        let task = if row.table == "task_results" {
            row.value("task_id").map(str::to_owned)
        } else {
            row.value("operation_id").and_then(|id| operation_owners.get(id)).cloned()
        };
        add(&mut groups, &mut owned, row, task);
    }
    let mut event_owners = HashMap::new();
    for row in rows.iter().copied().filter(|row| row.table == "inbox_events") {
        let data = task_row_json(row);
        let native = row.value("kind") == Some("task_result") && row.value("source") == Some("native-child");
        let task = if native {
            data.as_ref().and_then(|value| value.get("task_id")?.as_str().map(str::to_owned))
        } else { data.as_ref().and_then(task_delivery_owner) };
        if native || task.is_some() {
            let task = add(&mut groups, &mut owned, row, task);
            event_owners.insert(row.key.as_str(), task);
        }
    }
    let mut raw_owners = HashMap::new();
    let mut invalid_groups = HashSet::new();
    for row in rows.iter().copied().filter(|row| row.table == "session_events") {
        let data = task_row_json(row);
        let original = data.as_ref().and_then(|data| data.get("inbox_event"));
        let task = original.and_then(task_delivery_owner);
        let native = original.is_some_and(|event| event.get("source").and_then(serde_json::Value::as_str) == Some("native-child")
            && event.get("kind").and_then(serde_json::Value::as_str) == Some("task_result"));
        if native || task.is_some() {
            let task = add(&mut groups, &mut owned, row, task);
            let source = original.and_then(|event| event.get("event_id")).and_then(serde_json::Value::as_str).map(str::to_owned);
            raw_owners.insert(row.key.as_str(), (task, source));
        }
    }
    for row in rows.iter().copied().filter(|row| row.table == "subject_raw_inputs") {
        let raw = row.value("raw_event_id").and_then(|id| raw_owners.get(id));
        let event = row.value("source_event_id").and_then(|id| event_owners.get(id));
        if raw.is_some() || event.is_some() {
            let task = raw.map(|(task, _)| task.clone()).or_else(|| event.cloned());
            let task = add(&mut groups, &mut owned, row, task);
            if raw.is_none_or(|(_, source)| source.as_deref() != row.value("source_event_id"))
                || event.is_some_and(|owner| owner != &task)
            {
                invalid_groups.insert(task);
                if let Some(owner) = event { invalid_groups.insert(owner.clone()); }
            }
        }
    }
    let mut outcome = CognitionApplyOutcome::default();
    let mut rejected = HashSet::new();
    let mut rejected_ids = HashSet::new();
    for (task_id, mut group) in groups {
        let parent = tasks.get(task_id.as_str()).copied();
        let mut conflict = parent.is_none() || invalid_groups.contains(&task_id);
        if let Some(parent) = parent {
            // An immutable action input cannot change, even if the nil-peer
            // baseline happens to match the local record from another source.
            if let Some(local) = read_existing(tx, task_spec, parent)? {
                let local = memory_row_value(&local, "data").and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok());
                let incoming = task_row_json(parent);
                if local.as_ref().is_some_and(|data| data["kind"] == "playground.backend")
                    || incoming.as_ref().is_some_and(|data| data["kind"] == "playground.backend")
                {
                    conflict |= match (&local, &incoming) {
                        (Some(local), Some(incoming)) => ["task_id", "agent_id", "kind", "definition_version"].iter()
                            .any(|key| local.get(*key) != incoming.get(*key))
                            || ["digest", "bytes"].iter().any(|key|
                                local.get("input_ref").and_then(|input| input.get(*key))
                                    != incoming.get("input_ref").and_then(|input| input.get(*key))),
                        _ => true,
                    };
                }
            }
        }
        // The same fixed SQL identifier is reused only after RELEASE. UUIDs
        // are data, never interpolated into SQLite identifiers.
        tx.execute("SAVEPOINT task_closure", []).map_err(db)?;
        let mut part = CognitionApplyOutcome::default();
        group.sort_by_key(|row| TABLES.iter().position(|spec| spec.name == row.table));
        if !conflict {
            for row in &group {
                let spec = spec_for(&row.table).context("unregistered task recovery table")?;
                let effective = effective_row(row, spec)?;
                match apply_recovery_fact(tx, spec, &effective, peer_id, seq)? {
                    RecoveryFactOutcome::Conflict => { conflict = true; break; }
                    RecoveryFactOutcome::Inserted => part.inserted += 1,
                    RecoveryFactOutcome::Updated => part.updated += 1,
                    RecoveryFactOutcome::Unchanged => { part.unchanged += 1; continue; }
                }
                record_origin(tx, spec, &effective, peer_id, &manifest.fingerprint, manifest.source_ledger_id)?;
            }
        }
        if conflict { tx.execute("ROLLBACK TO task_closure", []).map_err(db)?; }
        tx.execute("RELEASE task_closure", []).map_err(db)?;
        if conflict {
            rejected_ids.insert(task_id.clone());
            for row in group {
                rejected.insert((row.table.as_str(), row.key.as_str()));
                rejected_ids.insert(row.key.clone());
                let detail = format!("task {task_id} input/execution/delivery closure conflicts or lacks its incoming parent; the complete local unit is kept");
                record_conflict(tx, peer_id, &row.table, &row.key, &detail)?;
                outcome.conflicts.push(CognitionConflict { table: row.table.clone(), key: row.key.clone(), detail });
            }
        } else {
            outcome.inserted += part.inserted;
            outcome.updated += part.updated;
            outcome.unchanged += part.unchanged;
        }
    }
    Ok((outcome, owned, rejected, rejected_ids))
}

/// Every string value and every object key, recursively: window call_states
/// hold task ids as MAP KEYS, and sealed S0 refs can appear nested anywhere.
fn json_holds_any_id(value: &serde_json::Value, ids: &HashSet<String>) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, v)| {
            ids.contains(key.as_str()) || json_holds_any_id(v, ids)
        }),
        serde_json::Value::Array(items) => items.iter().any(|v| json_holds_any_id(v, ids)),
        serde_json::Value::String(text) => ids.contains(text),
        _ => false,
    }
}

/// A rejected task delivery must not survive through consumers committed in
/// an earlier table phase. Rather than mutating sealed step/window records
/// (which would falsify their identity), the ENTIRE import transaction is
/// rejected when a dependency cannot be established as safe: the peer gets an
/// honest "publication refused", never a half-applied snapshot or fake ACK.
pub(super) fn verify_no_rejected_task_consumers(
    store: &Store,
    tx: &Transaction<'_>,
    rows: &[&CognitionRow],
    derivatives: &[CognitionDerivative],
    rejected: &HashSet<(&str, &str)>,
    rejected_ids: &HashSet<String>,
) -> Result<()> {
    if rejected.is_empty() {
        return Ok(());
    }
    let refuse = |what: String| -> Result<()> {
        bail!("cognition import refused: task closure rejection cannot publish dependent {what}; the complete import is rolled back, nothing was applied")
    };

    // The seq of every rejected staged raw event, for from/to range tests.
    let rejected_raw_seqs: Vec<(String, i64)> = {
        let mut out = Vec::new();
        for (_table, key) in rejected.iter().filter(|(table, _)| *table == "session_events") {
            let row: Option<(String, i64)> = rows.iter().copied()
                .find(|r| r.table == "session_events" && r.key == *key)
                .and_then(|r| {
                    Some((r.value("session_id")?.to_owned(), r.value("seq")?.parse::<i64>().ok()?))
                });
            if let Some(pair) = row { out.push(pair); }
        }
        out
    };

    // Union of session_ids so a range can only match its own branch.
    let rejected_sessions: HashSet<&str> =
        rejected_raw_seqs.iter().map(|(session, _)| session.as_str()).collect();

    // Window bodies by window_id (manifest + existing chain head reads).
    let window_body = |id: &str| -> Result<Option<serde_json::Value>> {
        if let Some(body) = rows.iter().copied()
            .find(|r| r.table == "context_windows" && r.key == id)
            .and_then(|r| r.value("body"))
        {
            return Ok(serde_json::from_str(body).ok());
        }
        let body: Option<String> = tx.query_row(
            "SELECT body FROM context_windows WHERE window_id=?1", [id], |r| r.get(0),
        ).optional().map_err(db)?;
        Ok(body.and_then(|body| serde_json::from_str(&body).ok()))
    };

    // Transitive dirty-window closure: direct id hits, from/to ranges that
    // cover a rejected raw seq, and base.source_window predecessors.
    let mut dirty_windows: HashSet<String> = HashSet::new();
    let mut work: Vec<String> = rows.iter().copied()
        .filter(|r| r.table == "context_windows")
        .map(|r| r.key.clone())
        .collect();
    // Seed with windows any rejected step's frozen context points at.
    for row in rows.iter().copied().filter(|r| r.table == "subject_steps") {
        if let Some(data) = row.value("data").and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()) {
            if let Some(id) = data.get("input_plan_ref")
                .and_then(|o| o.get("digest")).and_then(serde_json::Value::as_str)
                .and_then(|d| store.cas.read_object(&Digest(d.to_owned())).ok())
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|plan| plan.pointer("/context/source/window_id").and_then(serde_json::Value::as_str).map(str::to_owned))
            {
                if json_holds_any_id(&data, rejected_ids) { dirty_windows.insert(id); }
            }
        }
    }
    while let Some(id) = work.pop() {
        let Some(body) = window_body(&id)? else { continue };
        if dirty_windows.contains(&id) {
            // Propagate to predecessor and to windows naming this as source.
            if let Some(pred) = body.pointer("/base/source_window").and_then(serde_json::Value::as_str) {
                if dirty_windows.insert(pred.to_owned()) { work.push(pred.to_owned()); }
            }
            continue;
        }
        let hit_ids = json_holds_any_id(&body, rejected_ids);
        let hit_range = rejected_sessions.contains(
            body.get("scope_id").or_else(|| body.pointer("/base/scope_id")).and_then(serde_json::Value::as_str).unwrap_or(""),
        ) && {
            let from = body.pointer("/base/from").or_else(|| body.get("from")).and_then(serde_json::Value::as_i64);
            let to = body.pointer("/base/to").or_else(|| body.get("to")).and_then(serde_json::Value::as_i64);
            matches!((from, to), (Some(f), Some(t))
                if rejected_raw_seqs.iter().any(|(s, seq)| *seq >= f && *seq <= t
                    && (body.get("scope_id").or_else(|| body.pointer("/base/scope_id"))
                        .and_then(serde_json::Value::as_str) == Some(s.as_str()))))
        };
        if hit_ids || hit_range {
            dirty_windows.insert(id.clone());
            if let Some(pred) = body.pointer("/base/source_window").and_then(serde_json::Value::as_str) {
                work.push(pred.to_owned());
            }
            // Successor windows in the same manifest that name this window
            // as their source inherit the contamination.
            for candidate in rows.iter().copied().filter(|r| r.table == "context_windows") {
                if !dirty_windows.contains(candidate.key.as_str()) { work.push(candidate.key.clone()); }
            }
        }
    }
    // Steps whose frozen S0 selects rejected deliveries or a dirty window.
    for row in rows.iter().copied().filter(|r| r.table == "subject_steps") {
        let Some(data) = row.value("data").and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok()) else { continue };
        let plan: Option<serde_json::Value> = data.get("input_plan_ref")
            .and_then(|o| o.get("digest")).and_then(serde_json::Value::as_str)
            .and_then(|d| store.cas.read_object(&Digest(d.to_owned())).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let Some(plan) = plan else {
            // Cannot verify this step's dependency closure while its own
            // data mentions rejected facts: refuse rather than guess.
            if json_holds_any_id(&data, rejected_ids) {
                return refuse(format!("subject step {} with unresolved input plan", row.key));
            }
            continue;
        };
        let window = plan.pointer("/context/source/window_id").and_then(serde_json::Value::as_str);
        if json_holds_any_id(&plan, rejected_ids)
            || window.is_some_and(|id| dirty_windows.contains(id) || rejected_ids.contains(id))
        {
            return refuse(format!("subject step {} frozen input", row.key));
        }
    }
    // Dirty imported windows or any derivative of one fail publication.
    if !dirty_windows.is_empty() {
        let mut dirty: Vec<_> = dirty_windows.iter().cloned().collect();
        dirty.sort();
        return refuse(format!("context window generations {dirty:?}"));
    }
    for derivative in derivatives {
        if dirty_windows.contains(derivative.window_id.to_string().as_str()) {
            return refuse(format!("derivative {}:{}", derivative.window_id, derivative.idx));
        }
    }
    // An imported context_window_scopes row naming a rejected window's scope
    // is safe only when no window became dirty — already covered above.
    Ok(())
}
