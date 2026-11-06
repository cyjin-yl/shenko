//! Read-only inspector for the autonomous PMA initiation experiment.
//!
//! It answers one mechanical question from durable product state: did the
//! ordinary runtime admit a `pma.evaluate` native task from the subject's own
//! model output, and did a durable reminder reach exactly one later request?
//!
//! What it does and does not do:
//!
//! * it opens the store read-only in effect (only `Store` read APIs are used);
//! * every sealed object it reports is re-hashed against its `ObjectRef`, so a
//!   reported body is the bytes the runtime sealed, not a re-serialization;
//! * it never grades advice, never labels a decision "correct", and derives no
//!   semantic verdict. `admission` is a provenance fact, not a quality claim;
//! * an object that exists but fails verification is an error (corruption is
//!   not data); an absent optional object is reported as `null`.
//!
//! Interface: `<data-dir> <output.json>`. The data directory is the private
//! root of one bounded CLI observation; the output is a JSON document whose
//! `checks` object reports only mechanical integrity.

use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use shenko_store::{BankSelection, Store};
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};
use std::path::PathBuf;

/// Operation names this inspector asks the ledger for, by exact name. The
/// ledger's operation index is keyed by `(task_id, logical_key)` and can be
/// read per exact name only; unrelated kinds are not guessed.
const OPERATION_NAMES: [&str; 6] = [
    "research.reset.v1",
    "research.bank.v1",
    "research.candidate.v1",
    "pma.evaluate.v1",
    "model.child.generate.v1",
    "model.child.result.v1",
];

const TASK_LIMIT: usize = 500;
const OPERATION_LIMIT: usize = 256;
const SESSION_EVENT_LIMIT: i64 = i64::MAX;

/// Kinds whose delegation control must be present and identity-checked. Other
/// kinds keep their record and a `null` control instead of failing the whole
/// readback for a shape this inspector does not claim to know.
const FOCUS_KINDS: [&str; 4] = ["pma.evaluate", "research.reset", "research.bank", "research.candidate"];

/// Read one sealed object and prove it is the exact sealed bytes.
fn read_object(store: &Store, object: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = store
        .cas
        .read_object(&object.digest)
        .with_context(|| format!("read sealed object {}", serde_json::to_string(object).unwrap_or_default()))?;
    ensure!(
        bytes.len() as u64 == object.bytes,
        "sealed object length differs from its reference"
    );
    ensure!(
        Digest::blake3(&bytes) == object.digest,
        "sealed object digest differs from its reference"
    );
    Ok(bytes)
}

fn read_json(store: &Store, object: &ObjectRef, what: &str) -> Result<Value> {
    let bytes = read_object(store, object)?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("{what} is not the sealed JSON it claims to be"))
}

fn object_json(object: &ObjectRef) -> Result<Value> {
    Ok(serde_json::to_value(object)?)
}

/// The sealed body of a subject step's S0, its model answer, terminal provider
/// evidence and committed decision.
fn step_document(store: &Store, step: &shenko_types::subject::SubjectStep, errors: &mut Vec<String>) -> Result<Value> {
    let mut checks = serde_json::Map::new();
    let mut s0 = Value::Null;
    let mut model_result = Value::Null;
    let mut model_outcome = Value::Null;
    let mut decision = Value::Null;
    let mut controls: Vec<Value> = Vec::new();
    let mut contributions: Vec<Value> = Vec::new();

    if let Some(object) = step.input_plan_ref.as_ref() {
        match read_json(store, object, "step input plan") {
            Ok(body) => {
                let identity = body["step_id"] == json!(step.step_id)
                    && body["step_no"] == json!(step.step_no);
                checks.insert("s0_identity".into(), json!(identity));
                if !identity {
                    errors.push(format!("step {} S0 identity differs", step.step_no));
                }
                let parts = body["contributions"].as_array().cloned().unwrap_or_default();
                for item in parts {
                    let key = item["key"].as_str().unwrap_or_default().to_owned();
                    if !key.starts_with("pma.") {
                        continue;
                    }
                    let body_ref: ObjectRef = serde_json::from_value(item["data"].clone())
                        .context("pma contribution does not carry an object reference")?;
                    let sealed = read_json(store, &body_ref, "pma contribution body")?;
                    contributions.push(json!({
                        "key": key,
                        "data": item["data"],
                        "body": sealed,
                        "s0_request_id": body["request_id"],
                    }));
                }
                s0 = json!({"object": object_json(object)?, "body": body});
            }
            Err(error) => errors.push(format!("step {} S0: {error:#}", step.step_no)),
        }
    }

    if let Some(object) = step.model_result_ref.as_ref() {
        match read_object(store, object) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes.clone());
                checks.insert("model_result_utf8".into(), json!(text.is_ok()));
                model_result = json!({
                    "object": object_json(object)?,
                    "text": text.clone().unwrap_or_default(),
                    "utf8": text.is_ok(),
                });
            }
            Err(error) => errors.push(format!("step {} model result: {error:#}", step.step_no)),
        }
    }

    match store.recorded_model_outcome(step) {
        Ok(outcome) => {
            model_outcome = json!({
                "request_id": outcome.request_id,
                "result_ref": outcome.result_ref,
                "stop_reason": outcome.stop_reason,
                "raw_response": outcome.raw_response,
                "usage": outcome.usage,
                "complete": outcome.is_complete(),
            });
        }
        Err(error) => {
            if step.model_outcome_ref.is_some() {
                errors.push(format!("step {} model outcome: {error}", step.step_no));
            }
        }
    }

    if let Some(object) = step.decision_ref.as_ref() {
        match read_json(store, object, "step decision") {
            Ok(body) => {
                let identity = body["step_id"] == json!(step.step_id)
                    && body["input_plan_ref"] == json!(step.input_plan_ref);
                checks.insert("decision_identity".into(), json!(identity));
                if !identity {
                    errors.push(format!("step {} decision identity differs", step.step_no));
                }
                for control in body["output"]["controls"].as_array().cloned().unwrap_or_default() {
                    let action = &control["action"];
                    controls.push(json!({
                        "action_id": control["action_id"],
                        "spans": control["spans"],
                        "kind": action["kind"],
                        "text": action["text"],
                        "definition": action["definition"],
                    }));
                }
                decision = json!({
                    "object": object_json(object)?,
                    "schema_version": body["schema_version"],
                    "body": body,
                });
            }
            Err(error) => errors.push(format!("step {} decision: {error:#}", step.step_no)),
        }
    }

    Ok(json!({
        "step_no": step.step_no,
        "step_id": step.step_id,
        "state": step.state,
        "created_by_step_id": step.created_by_step_id,
        "active_request_id": step.active_request_id,
        "controller_epoch": step.controller_epoch,
        "commit_ref": step.commit_ref,
        "checks": checks,
        "s0": s0,
        "model_result": model_result,
        "model_outcome": model_outcome,
        "decision": decision,
        "controls": controls,
        "pma_contributions": contributions,
    }))
}

/// The native definition input a task was dispatched with.
///
/// A subject-authored native delegation stores the whole committed decision as
/// the task input; the exact definition travels in the matching control
/// (`action_id == task_id`) and in the matching delegation intent. Both are
/// reported, and a mismatch is an error rather than a silent choice.
fn task_input(store: &Store, record: &shenko_types::task::TaskRecord, errors: &mut Vec<String>) -> Result<Value> {
    let task_id = record.task_id;
    let body = read_json(store, &record.input_ref, "task input")?;
    let mut by_control: Option<Value> = None;
    for control in body["output"]["controls"].as_array().cloned().unwrap_or_default() {
        if control["action_id"] == json!(task_id) {
            by_control = Some(json!({
                "action_id": control["action_id"],
                "kind": control["action"]["kind"],
                "definition": control["action"]["definition"],
                "text": control["action"]["text"],
                "spans": control["spans"],
            }));
        }
    }
    let mut by_intent: Option<Value> = None;
    for intent in body["delegated_task_intents"].as_array().cloned().unwrap_or_default() {
        if intent["intent_id"] == json!(task_id) {
            by_intent = Some(intent);
        }
    }
    if by_control.is_none() && FOCUS_KINDS.contains(&record.kind.as_str()) {
        errors.push(format!("task {task_id} has no control with its own action id"));
    }
    let definition = by_control
        .as_ref()
        .and_then(|control| control["definition"].as_object().cloned());
    if let Some(definition) = &definition {
        let kind = definition.get("kind").and_then(Value::as_str);
        let version = definition.get("definition_version").and_then(Value::as_u64);
        if kind != Some(record.kind.as_str()) || version != Some(record.definition_version as u64) {
            errors.push(format!("task {task_id} definition identity differs from its record"));
        }
    }
    Ok(json!({
        "decision_step_id": body["step_id"],
        "decision_schema_version": body["schema_version"],
        "control": by_control,
        "intent": by_intent,
        "record_kind": record.kind,
        "record_definition_version": record.definition_version,
    }))
}

fn main() -> Result<()> {
    let data = PathBuf::from(std::env::args().nth(1).context("data root")?);
    let output = PathBuf::from(std::env::args().nth(2).context("output")?);
    let store = Store::open(data.join("shenko.db"), data.join("cas"))?;
    let agent = store.get_existing_agent_id()?.context("no initialized subject")?;
    let (head, _) = store.load_subject(agent)?;
    let mut errors: Vec<String> = Vec::new();

    let mut steps = Vec::new();
    for step in store.sealed_subject_steps(&head.branch_id)? {
        steps.push(step_document(&store, &step, &mut errors)?);
    }

    let mut tasks = Vec::new();
    let mut lineages: Vec<EntityId> = Vec::new();
    let mut epochs: Vec<EntityId> = Vec::new();
    let recent = store.list_recent_tasks(TASK_LIMIT)?;
    for row in &recent {
        let id: EntityId = row.task_id.parse().context("task identity")?;
        let record = match store.external_task(id) {
            Ok(record) => record,
            Err(error) => {
                errors.push(format!("task {id} record: {error}"));
                continue;
            }
        };
        let mut operations = Vec::new();
        let mut operation_errors = Vec::new();
        for name in OPERATION_NAMES {
            let found = match store.external_operations_after(name, None, OPERATION_LIMIT) {
                Ok(found) => found,
                Err(error) => {
                    operation_errors.push(format!("{name}: {error}"));
                    continue;
                }
            };
            for operation in found {
                if operation.task_id != id {
                    continue;
                }
                let attempts = store.external_operation_attempts(operation.operation_id)?;
                let evidence = store.external_operation_evidence(operation.operation_id)?;
                operations.push(json!({
                    "operation": serde_json::to_value(&operation)?,
                    "attempts": attempts.iter().map(serde_json::to_value).collect::<std::result::Result<Vec<_>,_>>()?,
                    "evidence_receipt_ids": evidence.iter().map(|e| e.receipt_id).collect::<Vec<_>>(),
                }));
            }
        }
        let input = match task_input(&store, &record, &mut errors) {
            Ok(input) => input,
            Err(error) => {
                errors.push(format!("task {id} input: {error:#}"));
                Value::Null
            }
        };
        if record.kind == "research.reset" {
            if let Some(lineage) = input["control"]["definition"]["input"]["lineage_id"].as_str() {
                lineages.push(lineage.parse().context("research lineage identity")?);
            }
        }
        if record.kind == "pma.evaluate" || record.kind == "research.bank" {
            if let Some(epoch) = input["control"]["definition"]["input"]["epoch_id"].as_str() {
                epochs.push(epoch.parse().context("research epoch identity")?);
            }
            if let Some(lineage) = input["control"]["definition"]["input"]["lineage_id"].as_str() {
                lineages.push(lineage.parse().context("research lineage identity")?);
            }
        }
        tasks.push(json!({
            "recent": row,
            "record": serde_json::to_value(&record)?,
            "input": input,
            "operations": operations,
            "operation_errors": operation_errors,
        }));
    }

    // Epochs named by a bank/evaluation input whose lineage task is not in the
    // bounded recent page are still resolved, from their own identity.
    for epoch_id in epochs {
        if let Ok(Some(epoch)) = store.research_epoch(epoch_id) {
            lineages.push(epoch.lineage_id);
        }
    }
    lineages.sort();
    lineages.dedup();

    let mut research = Vec::new();
    for lineage in lineages {
        let mut lineage_row = json!({
            "lineage_id": lineage,
            "active_epoch": store.active_research_epoch(lineage)?,
            "epochs": Vec::<Value>::new(),
        });
        let mut epoch_rows = Vec::new();
        let epoch_list = store.research_lineage(lineage)?;
        for epoch in &epoch_list {
            let bank = store.bank_entries(epoch.epoch_id, BankSelection::All)?;
            let decision = store.latest_intervention(epoch.epoch_id)?;
            let scope = match &decision {
                Some(decision) => store.intervention_delivery_scope(decision)?,
                None => None,
            };
            epoch_rows.push(json!({
                "epoch": serde_json::to_value(epoch)?,
                "bank": bank.iter().map(serde_json::to_value).collect::<std::result::Result<Vec<_>,_>>()?,
                "latest_intervention": decision.as_ref().map(|d| serde_json::to_value(d)).transpose()?,
                "delivery_scope": scope.as_ref().map(|s| serde_json::to_value(s)).transpose()?,
            }));
        }
        lineage_row["epochs"] = json!(epoch_rows);
        research.push(lineage_row);
    }

    let branch_events: Vec<_> = store
        .session_events_range(EntityId(head.branch_id.0), 1, SESSION_EVENT_LIMIT)?
        .into_iter()
        .map(|event| {
            let decoded = serde_json::from_str::<Value>(&event.data);
            json!({
                "seq": event.seq,
                "event_type": event.event_type,
                "role": event.role,
                "raw": event.data,
                "decoded": decoded.as_ref().ok(),
                "decode_error": decoded.as_ref().err().map(ToString::to_string),
            })
        })
        .collect();
    let delivery_journal: Vec<_> = branch_events
        .iter()
        .filter(|event| event["decoded"]["kind"] == "pma_delivery")
        .cloned()
        .collect();

    // A pma reminder contribution in a sealed S0 is only usable if a durable
    // decision of the same epoch names it; report the join, not a verdict.
    // Any other `pma.*` prepared contribution (for example ambient epoch
    // context) is reported separately: it has no decision to join.
    let mut reminder_joins = Vec::new();
    let mut ambient_contributions = Vec::new();
    for step in &steps {
        for use_row in step["pma_contributions"].as_array().cloned().unwrap_or_default() {
            let key = use_row["key"].as_str().unwrap_or_default().to_owned();
            let Some(reminder_id) = key.strip_prefix("pma.reminder.") else {
                ambient_contributions.push(json!({
                    "step_no": step["step_no"],
                    "step_id": step["step_id"],
                    "key": key,
                    "contribution": use_row,
                }));
                continue;
            };
            let parsed: Option<EntityId> = reminder_id.parse().ok();
            let stored = match parsed {
                Some(id) => store.intervention_reminder(id)?,
                None => None,
            };
            reminder_joins.push(json!({
                "step_no": step["step_no"],
                "step_id": step["step_id"],
                "key": key,
                "contribution": use_row,
                "durable_reminder": stored.as_ref().map(|r| serde_json::to_value(r)).transpose()?,
                "decision_join": stored.is_some(),
            }));
        }
    }

    let checks = json!({
        "no_verification_error": errors.is_empty(),
        "steps_with_s0": steps.iter().filter(|s| s["s0"].is_object()).count(),
        "pma_reminder_joins": reminder_joins.len(),
        "pma_ambient_contributions": ambient_contributions.len(),
        "delivery_journal_rows": delivery_journal.len(),
    });
    let document = json!({
        "scope": "Read-only native readback of one bounded CLI observation root. Every reported sealed body is re-hashed against its ObjectRef. Admission provenance and delivery joins only: no semantic grading, no claim about model quality, chance outcomes or autonomous efficacy.",
        "data_dir": data.to_string_lossy(),
        "agent_id": agent,
        "head": head,
        "steps": steps,
        "recent_tasks": recent,
        "tasks": tasks,
        "research": research,
        "pma_reminder_joins": reminder_joins,
        "pma_ambient_contributions": ambient_contributions,
        "pma_delivery_journal": delivery_journal,
        "session_events": branch_events,
        "errors": errors,
        "checks": checks,
    });
    std::fs::write(&output, serde_json::to_vec_pretty(&document)?)?;
    println!(
        "{}",
        json!({
            "steps": steps_len(&document),
            "tasks": document["tasks"].as_array().map(Vec::len),
            "lineages": document["research"].as_array().map(Vec::len),
            "errors": document["errors"].as_array().map(Vec::len),
        })
    );
    if !document["errors"].as_array().is_some_and(Vec::is_empty) {
        std::process::exit(1);
    }
    Ok(())
}

fn steps_len(document: &Value) -> Option<usize> {
    document["steps"].as_array().map(Vec::len)
}
