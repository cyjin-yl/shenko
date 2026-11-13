//! Durable-fact capture and the consumer's classification of one run.
//!
//! Every fact here is read from the installation itself, after the default CLI
//! has exited: the Node's unit authority (versions, learning, adoption,
//! observations and trial evidence), the subject store's own task, operation,
//! attempt and result records, the sealed frozen inputs, and the exact artifact
//! bytes of every version. Nothing is re-derived from the model's words and
//! nothing is written back.
//!
//! The classification is a description of what the installation holds, not an
//! approval gate: an unmeasured candidate, a judgement that cites another
//! artifact, an inconclusive comparison and a refusal all stay visible with the
//! product's own error code or verdict next to them.

use anyhow::{Context as _, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use shenko_awareness_core::{CognitionCatalog, parse_cognition_entry};
use shenko_node::{Node, UnitRequest};
use shenko_store::Store;
use shenko_types::{ids::EntityId, object::Digest};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

/// The subject store and its slot, resolved the way the launcher resolves it.
pub struct InstallationPaths {
    pub data_dir: PathBuf,
    pub live_slot: PathBuf,
    pub db_path: PathBuf,
    pub cas_dir: PathBuf,
    pub node_dir: PathBuf,
    pub pointer: Option<PathBuf>,
}

impl InstallationPaths {
    pub fn resolve(data_dir: &Path) -> Result<Self> {
        let live_slot = shenko_update::slots::resolve_live_slot(data_dir)?
            .unwrap_or_else(|| data_dir.to_path_buf());
        let db_path = live_slot.join(shenko_update::SLOT_DB_FILENAME);
        let pointer = data_dir.join("current-slot");
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            live_slot,
            db_path,
            cas_dir: data_dir.join("cas"),
            node_dir: data_dir.join("node"),
            pointer: pointer.is_file().then_some(pointer),
        })
    }

    pub fn as_json(&self) -> Value {
        json!({
            "data_dir": self.data_dir.display().to_string(),
            "live_slot": self.live_slot.display().to_string(),
            "db_path": self.db_path.display().to_string(),
            "db_present": self.db_path.is_file(),
            "cas_dir": self.cas_dir.display().to_string(),
            "node_dir": self.node_dir.display().to_string(),
            "slot_pointer": self.pointer.as_ref().map(|path| path.display().to_string()),
        })
    }
}

/// One exact artifact version: the bytes this installation holds.
#[derive(Debug, Clone, Serialize)]
pub struct EntryFacts {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: String,
    pub artifact_bytes: usize,
    /// blake3 of the bytes actually read back equals the recorded digest.
    pub digest_verified: bool,
    pub enabled: bool,
    pub current: bool,
    /// The authored body, parsed from the exact entry bytes.
    pub body: Option<String>,
    pub body_error: Option<String>,
    /// The whole authored entry, exactly as installed (bounded by the caller).
    pub entry_text: String,
}

/// One durable native operation of a cognition child.
#[derive(Debug, Clone, Serialize)]
pub struct OperationFacts {
    pub operation_id: String,
    pub operation_name: String,
    pub state: Value,
    pub outcome: Option<Value>,
    pub effect_status: Value,
    /// The frozen input the operation is bound to, read back from CAS.
    pub final_input: Option<Value>,
    pub final_input_error: Option<String>,
    pub final_input_digest: String,
    pub attempts: Vec<Value>,
    pub attempt_error: Option<String>,
}

/// One durable native task of a cognition child.
#[derive(Debug, Clone, Serialize)]
pub struct TaskFacts {
    pub task_id: String,
    pub kind: String,
    pub definition_version: u32,
    pub created_at: String,
    pub state: Value,
    /// The invocation input the dispatcher sealed for this task.
    pub invocation_input: Option<Value>,
    pub invocation_input_error: Option<String>,
    pub result: Option<Value>,
    pub result_error: Option<String>,
    pub operations: Vec<OperationFacts>,
    /// Failure to enumerate operations; an empty list is incomplete when set.
    pub operations_error: Option<String>,
}

/// Everything one installation start left behind, read after it stopped.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub phase: String,
    pub paths: Value,
    pub ledger_id: Option<String>,
    pub subject_agent_id: Option<String>,
    pub units: Vec<Value>,
    pub versions: BTreeMap<String, Vec<Value>>,
    pub entries: BTreeMap<String, EntryFacts>,
    pub evidence: BTreeMap<String, Value>,
    pub evidence_errors: BTreeMap<String, String>,
    pub tasks: BTreeMap<String, TaskFacts>,
    pub task_errors: Vec<String>,
}

fn read_json_object(store: &Store, digest: &Digest) -> Result<Value> {
    let bytes = store.cas.read_object(digest)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn capture_task(store: &Store, task_id: &str, kind: &str, created_at: &str) -> Result<TaskFacts> {
    let id: EntityId = task_id.parse().context("task id is not an entity id")?;
    let task = store.external_task(id)?;
    let (invocation_input, invocation_input_error) = match read_json_object(store, &task.input_ref.digest)
    {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let (result, result_error) = match store.native_task_result(id) {
        Ok(Some(result)) => (
            Some(json!({
                "task_id": result.task_id.to_string(),
                "result_id": result.result_id.to_string(),
                "outcome": result.outcome,
                "result": result.result,
            })),
            None,
        ),
        Ok(None) => (None, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let operation_name = format!("{kind}.v{}", task.definition_version);
    let (operations, operations_error) = match store.external_task_operations(id, &operation_name) {
        Ok(records) => {
            let mut captured = Vec::new();
            for record in records {
                let (final_input, final_input_error) = match read_json_object(store, &record.final_input_ref.digest) {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                };
                let (attempts, error) = match store.external_operation_attempt(record.operation_id) {
                    Ok(attempt) => (vec![serde_json::to_value(&attempt)?], None),
                    Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                };
                captured.push(OperationFacts {
                    operation_id: record.operation_id.to_string(),
                    operation_name: record.operation_name.clone(),
                    state: serde_json::to_value(&record.state)?,
                    outcome: record.outcome.as_ref().map(serde_json::to_value).transpose()?,
                    effect_status: serde_json::to_value(&record.effect_status)?,
                    final_input,
                    final_input_error,
                    final_input_digest: record.final_input_ref.digest.0.clone(),
                    attempts,
                    attempt_error: error,
                });
            }
            (captured, None)
        }
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    Ok(TaskFacts {
        task_id: task_id.to_owned(),
        kind: kind.to_owned(),
        definition_version: task.definition_version,
        created_at: created_at.to_owned(),
        state: serde_json::to_value(&task.state)?,
        invocation_input,
        invocation_input_error,
        result,
        result_error,
        operations,
        operations_error,
    })
}

/// Read one stopped installation's own facts.
pub fn snapshot(data_dir: &Path, phase: &str) -> Result<Snapshot> {
    let paths = InstallationPaths::resolve(data_dir)?;
    ensure!(
        paths.db_path.is_file(),
        "the installation store {} is missing; the default CLI never created it",
        paths.db_path.display()
    );
    let store = Store::open(paths.db_path.clone(), paths.cas_dir.clone())
        .with_context(|| format!("open the installation store {}", paths.db_path.display()))?;
    let binding = store.installation_binding()?;
    let ledger_id = Some(binding.ledger_id.to_string());
    // The installation's own bound subject is the only scope in which its
    // durable cognition tasks and operations may be read. A missing subject is
    // an incomplete capture, never a clean empty installation.
    let bound_subject = binding.subject.as_ref().map(|subject| subject.agent_id);
    let subject_agent_id = bound_subject.map(|agent_id| agent_id.to_string());

    let mut tasks = BTreeMap::new();
    let mut task_errors = Vec::new();
    match bound_subject {
        Some(agent_id) => {
            for recent in store.list_task_summaries(agent_id, 500)? {
                if !recent.kind.starts_with("cognition.unit.") {
                    continue;
                }
                match capture_task(&store, &recent.task_id, &recent.kind, &recent.created_at) {
                    Ok(facts) => {
                        tasks.insert(recent.task_id.clone(), facts);
                    }
                    Err(error) => task_errors.push(format!("{} ({}): {error:#}", recent.task_id, recent.kind)),
                }
            }
        }
        None => task_errors.push(
            "the stopped installation has no bound subject; durable task/operation capture is incomplete"
                .to_owned(),
        ),
    }

    let node = Arc::new(parking_lot::Mutex::new(
        Node::open_inspect(&paths.node_dir)?.with_context(|| {
            format!(
                "the installation has no node database under {}",
                paths.node_dir.display()
            )
        })?,
    ));
    let catalog = CognitionCatalog::new(Arc::clone(&node));
    let units: Vec<Value> = catalog
        .units()?
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()?;

    let mut versions = BTreeMap::new();
    let mut entries = BTreeMap::new();
    let mut evidence = BTreeMap::new();
    let mut evidence_errors = BTreeMap::new();
    let mut unit_ids: BTreeSet<String> = units
        .iter()
        .filter_map(|unit| unit.get("unit_id").and_then(Value::as_str).map(ToOwned::to_owned))
        .collect();
    unit_ids.extend(tasks.values().filter_map(|task| {
        task.invocation_input
            .as_ref()
            .and_then(|input| input.get("unit_id"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }));
    for unit_id in unit_ids {
        let retained = catalog.versions(&unit_id)?;
        let mut retained_values = Vec::new();
        for unit in &retained {
            let digest = Digest(unit.artifact_digest.clone());
            let content = node
                .lock()
                .unit_content(&unit_id, &unit.revision, Some(&digest))
                .with_context(|| format!("read {unit_id}@{} exactly", unit.revision))?;
            let verified = Digest::blake3(&content.bytes) == digest;
            let text = String::from_utf8_lossy(&content.bytes).to_string();
            let (body, body_error) = match parse_cognition_entry(&text, Path::new("<installed>")) {
                Ok(package) => (Some(package.body), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
            entries.insert(
                format!("{unit_id}@{}@{}", unit.revision, unit.artifact_digest),
                EntryFacts {
                    unit_id: unit_id.clone(),
                    revision: unit.revision.clone(),
                    artifact_digest: unit.artifact_digest.clone(),
                    artifact_bytes: content.bytes.len(),
                    digest_verified: verified,
                    enabled: content.enabled,
                    current: content.current,
                    body,
                    body_error,
                    entry_text: text,
                },
            );
            retained_values.push(serde_json::to_value(unit)?);
        }
        versions.insert(unit_id.clone(), retained_values);
        match node.lock().handle_unit_request(UnitRequest::Evidence {
            unit_id: Some(unit_id.clone()),
            after: "0".to_owned(),
            limit: 100,
        }) {
            Ok(value) => {
                evidence.insert(unit_id.clone(), value);
            }
            Err(error) => {
                evidence_errors.insert(unit_id, format!("{error:#}"));
            }
        }
    }

    Ok(Snapshot {
        phase: phase.to_owned(),
        paths: paths.as_json(),
        ledger_id,
        subject_agent_id,
        units,
        versions,
        entries,
        evidence,
        evidence_errors,
        tasks,
        task_errors,
    })
}

/// The consumer's own reading of one authored version.
#[derive(Debug, Clone, Serialize)]
pub struct VersionReport {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub current: Option<bool>,
    pub enabled: Option<bool>,
    pub state: Option<String>,
    pub body_bytes: Option<usize>,
    pub body_sha256_blake3: Option<String>,
    pub digest_verified: bool,
    pub learning: Option<Value>,
    /// The adoption fact exactly as the installation recorded it.
    pub adoption: Option<Value>,
    /// The judge's own result body, when this version was judged.
    pub judge_result: Option<Value>,
    pub judge_error_code: Option<String>,
    pub judge_measurement: Option<Value>,
    /// The judgement cites at least one trial attempt whose own report names
    /// this exact artifact digest.
    pub cites_measured_attempt: bool,
    /// docs/specs/cognition-units.md#spec-51-1: immutable content, the ledger's attitude towards one exact
    /// revision, and where that attitude's reason stops.
    pub metacontract: Metacontract,
    /// Every trial task the subject ran against this exact artifact.
    pub trials: Vec<TrialFact>,
    /// What this version does *not* have, in the consumer's own words. A
    /// negative here is a recorded observation, never a reason to rewrite the
    /// installation or to claim the opposite.
    pub negatives: Vec<String>,
    /// Why the version is in the state the consumer reports.
    pub reason: String,
}

/// The cognitive metacontract of one authored revision, read from the
/// installation (docs/specs/cognition-units.md#spec-51-1, published `shenko_types::cognition` shape).
///
/// The consumer owns none of these types and opens no second store. It reads the
/// canonical attitude JSON when the installation publishes one
/// (`content{digest,bytes}`, `attitude{attitude_id,digest,role,reasons[],
/// confidence,knowledge_kind_hint,supersedes,created_commit,created_at_ms}`) and
/// otherwise reports the legacy adoption record it can actually see. An absent
/// confidence stays absent — `{"state":"unknown"}` is an explicit "not judged
/// yet", and neither is ever turned into `0.0`. `ReasonReading` is derived here
/// from the reason terminations, never read as a stored field.
#[derive(Debug, Clone, Serialize)]
pub struct Metacontract {
    /// `{digest, bytes}` of the content this revision installs.
    pub content: Value,
    pub content_bytes_verified: bool,
    /// The canonical attitude verbatim, or null when this installation holds
    /// only the legacy adoption record.
    pub attitude: Option<Value>,
    /// `canonical_content_attitude`, `legacy_adoption_record` or `none`.
    pub attitude_source: String,
    /// Where a canonical attitude was found: `unit_facts`, `unit_evidence` or
    /// `none`.
    pub attitude_origin: String,
    /// The canonical confidence verbatim (`{"state":"unknown"}` or
    /// `{"state":"stated","value":x}`), or `{"state":"absent"}` when the record
    /// carries none. Never a default number.
    pub confidence: Value,
    /// The canonical reason pointers verbatim.
    pub reasons: Vec<Value>,
    /// A reason that passed through someone else's perception is not the
    /// subject's own observation (docs/specs/cognition-units.md#spec-51-2, decision 0002 perspective field).
    pub via_perception_present: bool,
    /// Whether this installation verified the cited evidence:
    /// `verified_local` / `refuted_local` / `undetermined`, read from the
    /// canonical tri-state when published. A legacy adoption record is labelled
    /// by its own `evidence_verified` flag and never upgraded by the mere
    /// presence of reasons.
    pub evidence_state: String,
    /// `canonical_tri_state`, `legacy_evidence_verified_flag` or `absent`.
    pub evidence_state_source: String,
    /// Where the reasons stop, as read: `observation`, `source`, `receipt`,
    /// `reconciliation`, `content`, `perspective`, `none`, or the legacy
    /// evidence kinds.
    pub reason_terminations: Vec<String>,
    /// docs/specs/cognition-units.md#spec-51-1 reading, derived here and never stored by the product:
    /// observation/source/receipt/reconciliation read like a fact; content and
    /// perspective read like a conclusion; no reason reads as unstated.
    pub reads_as: String,
    pub reason_reading_derived_here: bool,
    pub evidence_kinds: Vec<String>,
    /// A free, learnable hint when the record carries one. Never a class
    /// boundary between fact and conclusion.
    pub knowledge_kind_hint: Option<Value>,
    pub empirical_is_not_approval: bool,
    /// The canonical revision disclosure, read verbatim from the installation
    /// (`family`, `selected`, `content_digest`, `written_at_ms`,
    /// `older_versions`, `latest_modified_at_ms`, `full_text`). The consumer
    /// reads this projection; it does not implement taste policy.
    pub disclosure_projection: Option<Value>,
    /// `present` when the installation published a projection, otherwise
    /// `omitted_or_unresolved` — the edge is not resolved here and is never
    /// turned into an invented number.
    pub disclosure_state: String,
    /// The consumer's own bounded observation of the retained versions, kept
    /// separate from the canonical projection so the two are never confused.
    pub consumer_observation: Value,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrialFact {
    pub task_id: String,
    pub attempt_id: Option<String>,
    pub attempt_bound_artifact: Option<String>,
    pub attempt_bound_unit: Option<String>,
    pub attempt_bound_revision: Option<String>,
    pub report_verdict: Option<String>,
    pub report_reason: Option<String>,
    pub report_artifact_digest: Option<String>,
    pub report_candidate: Option<Value>,
    pub report_control: Option<Value>,
    pub samples: usize,
    pub failed_samples: usize,
    pub report_error: Option<String>,
    /// The node's own classification of this attempt, when it carries one.
    pub node_measurement: Option<Value>,
    pub node_scored: Option<Value>,
    pub node_planned_slots: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Classification {
    pub authored_unit_ids: Vec<String>,
    pub versions: Vec<VersionReport>,
    /// A single source-truth sentence: what this run actually established.
    pub outcome: String,
    /// True only when the installation itself recorded an empirical acceptance
    /// for a version whose cited attempt measured that same artifact.
    pub empirical_acceptance: bool,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

fn trial_facts_for(snapshot: &Snapshot, before: &Snapshot, unit_id: &str) -> Vec<TrialFact> {
    let node_trials = snapshot
        .evidence
        .get(unit_id)
        .and_then(|value| value.get("trials"))
        .cloned()
        .unwrap_or(Value::Null);
    let rows: Vec<Value> = match node_trials {
        Value::Array(rows) => rows,
        Value::Object(map) => map
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut facts = Vec::new();
    for task in snapshot
        .tasks
        .iter()
        .filter(|(task_id, task)| {
            !before.tasks.contains_key(*task_id)
                && task.kind == "cognition.unit.trial"
                && task.definition_version == 1
        })
        .map(|(_, task)| task)
    {
        let input = task.invocation_input.clone().unwrap_or(Value::Null);
        let result_body = task
            .result
            .as_ref()
            .map(|result| result.get("result").cloned().unwrap_or(Value::Null))
            .unwrap_or(Value::Null);
        let report = result_body.get("report").cloned().unwrap_or(Value::Null);
        let report_artifact = report
            .get("artifact_digest")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let plan = task
            .operations
            .iter()
            .find_map(|operation| operation.final_input.clone())
            .unwrap_or(Value::Null);
        let bound_unit = plan
            .get("unit_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| input.get("unit_id").and_then(Value::as_str).map(ToOwned::to_owned));
        let bound_revision = plan
            .get("revision")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| input.get("revision").and_then(Value::as_str).map(ToOwned::to_owned));
        if bound_unit.as_deref() != Some(unit_id) {
            continue;
        }
        let attempt_id = result_body
            .get("attempt_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let samples = report
            .get("samples")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let failed_samples = samples
            .iter()
            .filter(|sample| sample.get("verdict").and_then(Value::as_str) != Some("effective")
                && sample.get("verdict").and_then(Value::as_str) != Some("ineffective"))
            .count();
        let node_row = attempt_id.as_deref().and_then(|attempt| {
            rows.iter().find(|row| {
                row.get("attempt_id").and_then(Value::as_str) == Some(attempt)
            })
        });
        let first_scenario = report
            .get("scenarios")
            .and_then(Value::as_array)
            .and_then(|rows| rows.first())
            .cloned()
            .unwrap_or(Value::Null);
        let report_artifact_digest = report_artifact.clone();
        facts.push(TrialFact {
            task_id: task.task_id.clone(),
            attempt_id,
            attempt_bound_artifact: report_artifact_digest.clone(),
            attempt_bound_unit: bound_unit,
            attempt_bound_revision: bound_revision,
            report_verdict: report.get("verdict").and_then(Value::as_str).map(ToOwned::to_owned),
            report_reason: report.get("reason").and_then(Value::as_str).map(ToOwned::to_owned),
            report_artifact_digest,
            report_candidate: first_scenario.get("candidate").cloned(),
            report_control: first_scenario.get("control").cloned(),
            samples: samples.len(),
            failed_samples,
            report_error: task
                .result
                .as_ref()
                .and_then(|result| result.get("result"))
                .and_then(|body| body.get("error"))
                .map(|error| error.to_string()),
            node_measurement: node_row
                .and_then(|row| row.get("measurement"))
                .cloned()
                .filter(|value| !value.is_null()),
            node_scored: node_row.and_then(|row| row.get("scored")).cloned().filter(|value| !value.is_null()),
            node_planned_slots: node_row
                .and_then(|row| row.get("planned_slots"))
                .cloned()
                .filter(|value| !value.is_null()),
        });
    }
    facts
}

fn judge_task_for<'a>(
    snapshot: &'a Snapshot,
    before: &Snapshot,
    unit_id: &str,
) -> Option<&'a TaskFacts> {
    snapshot
        .tasks
        .iter()
        .filter(|(task_id, task)| {
            !before.tasks.contains_key(*task_id)
                && task.kind == "cognition.unit.judge"
                && task.definition_version == 1
        })
        .map(|(_, task)| task)
        .find(|task| {
            task.invocation_input
                .as_ref()
                .and_then(|input| input.get("unit_id"))
                .and_then(Value::as_str)
                == Some(unit_id)
        })
}

/// Find a canonical revision disclosure on the unit facts, if this installation
/// publishes one. The shape is read, never rebuilt: `family`, `selected`,
/// `content_digest`, `written_at_ms`, `older_versions`,
/// `latest_modified_at_ms`, `full_text`.
fn disclosure_projection(version: &Value) -> Option<Value> {
    fn looks_projected(value: &Value) -> bool {
        value.get("older_versions").is_some()
            && value.get("content_digest").is_some()
            && value.get("latest_modified_at_ms").is_some()
    }
    let candidates = ["disclosure", "revision_disclosure", "projection", "taste_projection"];
    if let Some(found) = candidates
        .iter()
        .filter_map(|key| version.get(*key))
        .find(|value| looks_projected(value))
    {
        return Some(found.clone());
    }
    for container in ["taste", "revision"] {
        if let Some(inner) = version.get(container)
            && looks_projected(inner)
        {
            return Some(inner.clone());
        }
    }
    None
}

/// The canonical `ContentAttitude` this installation publishes for the
/// revision, if any. The legacy adoption record is *not* one, so it stays
/// labelled as legacy instead of being dressed up as a reason pointer.
fn canonical_attitude(version: &Value) -> Option<Value> {
    ["attitude", "content_attitude", "adoption_attitude"]
        .iter()
        .filter_map(|key| version.get(*key))
        .find(|value| {
            value.is_object()
                && (value.get("attitude_id").is_some() || value.get("role").is_some())
        })
        .cloned()
}

/// The variant a `ReasonPointer` stops at, as its own name.
fn termination_kind(reason: &Value) -> Option<String> {
    let termination = reason.get("termination").unwrap_or(reason);
    if let Some(text) = termination.as_str() {
        return Some(text.to_ascii_lowercase());
    }
    let object = termination.as_object()?;
    let mut keys = object.keys();
    match (keys.next(), keys.next()) {
        (Some(first), None) => Some(first.to_ascii_lowercase()),
        _ => None,
    }
}

/// A published attitude for this exact content digest, if the unit evidence
/// response carries one. The node stays the owner of these facts; this only
/// widens where the consumer may find them instead of guessing.
fn evidence_attitude(evidence: Option<&Value>, digest: &str) -> Option<Value> {
    let evidence = evidence?;
    for key in ["attitudes", "content_attitudes"] {
        let Some(items) = evidence.get(key).and_then(Value::as_array) else {
            continue;
        };
        if let Some(found) = items.iter().find(|item| {
            item.get("digest").and_then(Value::as_str) == Some(digest)
                && (item.get("attitude_id").is_some() || item.get("role").is_some())
        }) {
            return Some(found.clone());
        }
    }
    evidence
        .get("attitude")
        .filter(|item| {
            item.get("digest").and_then(Value::as_str) == Some(digest)
                && (item.get("attitude_id").is_some() || item.get("role").is_some())
        })
        .cloned()
}

/// Read the metacontract facts of one revision out of what the installation
/// actually holds (docs/specs/cognition-units.md#spec-51-1).
fn metacontract(
    version: &Value,
    entry: Option<&EntryFacts>,
    unit_versions: &[Value],
    adoption: &Option<Value>,
    published_attitude: Option<&Value>,
) -> Metacontract {
    let mut evidence_kinds: Vec<String> = adoption
        .as_ref()
        .and_then(|record| record.get("evidence"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("kind").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    evidence_kinds.sort();
    evidence_kinds.dedup();
    let from_unit_facts = canonical_attitude(version);
    let attitude_origin = if from_unit_facts.is_some() {
        "unit_facts"
    } else if published_attitude.is_some() {
        "unit_evidence"
    } else {
        "none"
    };
    let attitude = from_unit_facts.or_else(|| published_attitude.cloned());
    let reasons: Vec<Value> = attitude
        .as_ref()
        .and_then(|attitude| attitude.get("reasons"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut reason_terminations: Vec<String> = reasons
        .iter()
        .filter_map(termination_kind)
        .collect();
    let via_perception_present = reasons.iter().any(|reason| {
        reason
            .get("via_perception")
            .is_some_and(|value| !value.is_null())
    });
    if reason_terminations.is_empty() {
        // Legacy: the adoption record's own evidence kinds are where its reason
        // stops. A runner-owned trial attempt is a receipt of a measurement; a
        // stored observation is an observation.
        for kind in &evidence_kinds {
            let mapped = match kind.as_str() {
                "trial_attempt" => "receipt",
                "stored_observation" => "observation",
                other => other,
            };
            reason_terminations.push(mapped.to_owned());
        }
    }
    reason_terminations.sort();
    reason_terminations.dedup();
    let conclusion_like = ["content", "perspective"];
    let fact_like = ["observation", "source", "receipt", "reconciliation"];
    let reads_as = if reason_terminations
        .iter()
        .any(|kind| conclusion_like.contains(&kind.as_str()))
    {
        "conclusion"
    } else if via_perception_present
        && reason_terminations
            .iter()
            .any(|kind| fact_like.contains(&kind.as_str()))
    {
        "fact_via_other_perception"
    } else if reason_terminations
        .iter()
        .any(|kind| fact_like.contains(&kind.as_str()))
    {
        "fact"
    } else {
        "unstated"
    };
    let confidence = attitude
        .as_ref()
        .and_then(|attitude| attitude.get("confidence"))
        .cloned()
        .filter(|value| !value.is_null())
        .or_else(|| {
            adoption
                .as_ref()
                .and_then(|record| record.get("confidence"))
                .cloned()
                .filter(|value| !value.is_null())
        })
        .unwrap_or_else(|| json!({"state": "absent"}));
    let attitude_source = if attitude.is_some() {
        "canonical_content_attitude"
    } else if adoption.is_some() {
        "legacy_adoption_record"
    } else {
        "none"
    };
    let artifact_digest = version
        .get("artifact_digest")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let newest_modified_at = unit_versions
        .iter()
        .filter_map(|row| row.get("imported_at").and_then(Value::as_str))
        .max()
        .map(ToOwned::to_owned);
    let projection = disclosure_projection(version);
    let disclosure_state = if projection.is_some() {
        "present"
    } else {
        "omitted_or_unresolved"
    };
    let knowledge_kind_hint = attitude
        .as_ref()
        .and_then(|attitude| attitude.get("knowledge_kind_hint").cloned())
        .or_else(|| version.get("knowledge_kind").cloned());
    // Evidence verification is a tri-state owned by the real verifier. Never
    // infer "verified" from a non-empty reason list: an absent canonical state
    // stays undetermined, and a legacy flag is reported as exactly that.
    let published_state = attitude
        .as_ref()
        .and_then(|attitude| {
            attitude
                .get("evidence_state")
                .or_else(|| attitude.get("evidence"))
        })
        .and_then(|value| {
            if let Some(text) = value.as_str() {
                return Some(text.to_ascii_lowercase());
            }
            let object = value.as_object()?;
            if let Some(state) = object.get("state").and_then(Value::as_str) {
                return Some(state.to_ascii_lowercase());
            }
            let mut keys = object.keys();
            match (keys.next(), keys.next()) {
                (Some(first), None) => Some(first.to_ascii_lowercase()),
                _ => None,
            }
        });
    let (evidence_state, evidence_state_source) = match published_state.as_deref() {
        Some("verifiedlocal") | Some("verified_local") => {
            ("verified_local", "canonical_tri_state")
        }
        Some("refutedlocal") | Some("refuted_local") => {
            ("refuted_local", "canonical_tri_state")
        }
        Some("undetermined") => ("undetermined", "canonical_tri_state"),
        _ => match adoption
            .as_ref()
            .and_then(|record| record.get("evidence_verified"))
            .and_then(Value::as_bool)
        {
            Some(true) => ("verified_local", "legacy_evidence_verified_flag"),
            Some(false) => ("undetermined", "legacy_evidence_verified_flag"),
            None => ("undetermined", "absent"),
        },
    };
    Metacontract {
        content: json!({
            "digest": artifact_digest,
            "bytes": entry.map(|entry| entry.artifact_bytes),
        }),
        content_bytes_verified: entry.is_some_and(|entry| entry.digest_verified),
        attitude,
        attitude_source: attitude_source.to_owned(),
        attitude_origin: attitude_origin.to_owned(),
        confidence,
        reasons,
        via_perception_present,
        evidence_state: evidence_state.to_owned(),
        evidence_state_source: evidence_state_source.to_owned(),
        reason_terminations,
        reads_as: reads_as.to_owned(),
        reason_reading_derived_here: true,
        evidence_kinds,
        knowledge_kind_hint,
        empirical_is_not_approval: true,
        disclosure_projection: projection,
        disclosure_state: disclosure_state.to_owned(),
        consumer_observation: json!({
            "role": "consumer observation, not the canonical taste projection",
            "retained_versions_seen": unit_versions.len(),
            "newest_modified_at": newest_modified_at,
            "source": "node retained unit facts",
        }),
        note: "docs/specs/cognition-units.md#spec-51-6: this runtime still installs cognition units whose judgement is recorded as an adoption record; when this installation publishes no canonical ContentAttitude the block says so (attitude_source=legacy_adoption_record) instead of inventing one. ReasonReading is derived by this consumer and never read as a stored field. An absent confidence stays absent, an absent disclosure stays omitted/unresolved, and neither becomes a default number. Evidence verification is read as the verifier's tri-state; a non-empty reason list never upgrades it, and a legacy flag is labelled as a legacy flag.".to_owned(),
    }
}

/// Describe what the installation holds after the run.
pub fn classify(before: &Snapshot, after: &Snapshot) -> Classification {
    let before_units: BTreeSet<&str> = before
        .units
        .iter()
        .filter_map(|unit| unit.get("unit_id").and_then(Value::as_str))
        .collect();
    let before_versions: BTreeSet<&str> = before.versions.keys().map(String::as_str).collect();
    let mut authored: Vec<String> = after
        .versions
        .keys()
        .filter(|unit_id| {
            !before_versions.contains(unit_id.as_str()) || !before_units.contains(unit_id.as_str())
        })
        .cloned()
        .collect();
    // A unit that existed before can also gain a new version in this run.
    for (unit_id, versions) in &after.versions {
        let known: BTreeSet<String> = before
            .versions
            .get(unit_id)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        let revision = row.get("revision")?.as_str()?;
                        let digest = row.get("artifact_digest")?.as_str()?;
                        Some(format!("{revision}@{digest}"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let gained = versions.iter().any(|row| {
            let revision = row.get("revision").and_then(Value::as_str).unwrap_or_default();
            let digest = row.get("artifact_digest").and_then(Value::as_str).unwrap_or_default();
            !known.contains(&format!("{revision}@{digest}"))
        });
        if gained && !authored.contains(unit_id) {
            authored.push(unit_id.clone());
        }
    }
    authored.sort();

    let mut reports = Vec::new();
    let mut checks = Vec::new();
    let mut empirical_acceptance = false;
    let mut wrote = false;
    let mut measured_any = false;
    let mut judged_any = false;
    let mut wrong_artifact = false;

    let write_tasks: Vec<&TaskFacts> = after
        .tasks
        .iter()
        .filter(|(task_id, task)| {
            !before.tasks.contains_key(*task_id)
                && task.kind == "cognition.unit.write"
                && task.definition_version == 1
        })
        .map(|(_, task)| task)
        .collect();
    if !write_tasks.is_empty() {
        wrote = true;
    }
    checks.push(Check {
        name: "a_cognition_write_task_was_attempted".into(),
        passed: !write_tasks.is_empty(),
        detail: format!(
            "{} cognition.unit.write task(s) in this phase",
            write_tasks.len()
        ),
    });

    for unit_id in &authored {
        let versions = after.versions.get(unit_id).cloned().unwrap_or_default();
        for version in &versions {
            let revision = version
                .get("revision")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let artifact = version
                .get("artifact_digest")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let entry = after
                .entries
                .get(&format!("{unit_id}@{revision}@{artifact}"));
            let published_attitude = evidence_attitude(after.evidence.get(unit_id), &artifact);
            let trials = trial_facts_for(after, before, unit_id);
            measured_any |= !trials.is_empty();
            let judge_task = judge_task_for(after, before, unit_id);
            let judge_body = judge_task
                .and_then(|task| task.result.as_ref())
                .and_then(|result| result.get("result"))
                .cloned();
            let judge_input = judge_task.and_then(|task| {
                task.operations
                    .iter()
                    .find_map(|operation| operation.final_input.clone())
                    .or_else(|| task.invocation_input.clone())
            });
            let cited_attempts: Vec<String> = judge_input
                .as_ref()
                .and_then(|input| input.get("evidence"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| item.get("kind").and_then(Value::as_str) == Some("trial_attempt"))
                        .filter_map(|item| item.get("attempt_id").and_then(Value::as_str))
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let cited_this_artifact: Vec<&TrialFact> = trials
                .iter()
                .filter(|trial| {
                    trial
                        .attempt_id
                        .as_ref()
                        .is_some_and(|attempt| cited_attempts.contains(attempt))
                })
                .collect();
            let cites_same_artifact = !cited_this_artifact.is_empty()
                && cited_this_artifact.iter().all(|trial| {
                    trial.report_artifact_digest.as_deref() == Some(artifact.as_str())
                });
            let adoption = version.get("adoption").cloned().filter(|value| !value.is_null());
            let judge_measurement = judge_body
                .as_ref()
                .and_then(|body| body.get("measurement"))
                .cloned()
                .filter(|value| !value.is_null());
            let judge_error_code = judge_body
                .as_ref()
                .and_then(|body| body.get("error"))
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let empirical = adoption
                .as_ref()
                .and_then(|record| record.get("empirical"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let evidence_verified = adoption
                .as_ref()
                .and_then(|record| record.get("evidence_verified"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let adopted_this_artifact = adoption.is_some()
                && version.get("current").and_then(Value::as_bool) == Some(true);
            let meta = metacontract(
                version,
                entry,
                &versions,
                &adoption,
                published_attitude.as_ref(),
            );
            if adoption.is_some() {
                judged_any = true;
            }
            if adoption.is_some() && !cites_same_artifact && !cited_attempts.is_empty() {
                wrong_artifact = true;
            }
            if empirical
                && evidence_verified
                && meta.evidence_state == "verified_local"
                && cites_same_artifact
                && adopted_this_artifact
            {
                empirical_acceptance = true;
            }
            let reason = if adoption.is_none() {
                if trials.is_empty() {
                    "written without a trial attempt of this installation: the version stays tentative"
                        .to_owned()
                } else {
                    "measured but not judged: the trial result was never cited by a recorded judgement"
                        .to_owned()
                }
            } else if judge_error_code.is_some() {
                format!(
                    "the judge attempt ended with the product's own error code {}: {}",
                    judge_error_code.as_deref().unwrap_or("<none>"),
                    judge_body
                        .as_ref()
                        .and_then(|body| body.get("error"))
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("<no message>")
                )
            } else if !cites_same_artifact {
                "the judgement does not cite a trial attempt that measured this exact artifact"
                    .to_owned()
            } else {
                format!(
                    "recorded judgement: decision {:?}, method {:?}, evidence_verified {evidence_verified}, empirical {empirical}",
                    adoption.as_ref().and_then(|record| record.get("decision")).cloned(),
                    adoption.as_ref().and_then(|record| record.get("method")).cloned()
                )
            };
            let mut negatives = Vec::new();
            if trials.is_empty() {
                negatives.push("unmeasured".to_owned());
            }
            for trial in &trials {
                match trial.report_verdict.as_deref() {
                    Some("inconclusive") => negatives.push("inconclusive_comparison".to_owned()),
                    Some("not_supported") => negatives.push("comparison_not_supported".to_owned()),
                    _ => {}
                }
                if trial.failed_samples > 0 {
                    negatives.push(format!("{}_failed_sample(s)", trial.failed_samples));
                }
                if let (Some(measurement), Some(attempt)) =
                    (trial.node_measurement.as_ref(), trial.attempt_id.as_ref())
                {
                    let text = measurement.as_str().unwrap_or_default();
                    if text != "measured" {
                        negatives.push(format!("attempt_{attempt}_measurement_{text}"));
                    }
                }
            }
            if adoption.is_some() && cited_attempts.is_empty() {
                negatives.push("judgement_cites_no_trial_attempt".to_owned());
            }
            if adoption.is_some() && !cites_same_artifact {
                negatives.push("judgement_not_bound_to_this_artifact".to_owned());
            }
            if let Some(code) = judge_error_code.as_deref() {
                negatives.push(format!("judge_refused:{code}"));
            }
            if adoption.is_some() && !empirical {
                negatives.push("not_empirical".to_owned());
            }
            if !entry.is_some_and(|entry| entry.digest_verified) {
                negatives.push("artifact_bytes_not_verified".to_owned());
            }
            reports.push(VersionReport {
                unit_id: unit_id.clone(),
                revision,
                artifact_digest: artifact.clone(),
                kind: version.get("kind").and_then(Value::as_str).map(ToOwned::to_owned),
                title: version.get("title").and_then(Value::as_str).map(ToOwned::to_owned),
                current: version.get("current").and_then(Value::as_bool),
                enabled: version.get("enabled").and_then(Value::as_bool),
                state: version.get("state").and_then(Value::as_str).map(ToOwned::to_owned),
                body_bytes: entry.and_then(|entry| entry.body.as_ref().map(|body| body.len())),
                body_sha256_blake3: entry
                    .and_then(|entry| entry.body.as_ref())
                    .map(|body| blake3::hash(body.as_bytes()).to_hex().to_string()),
                digest_verified: entry.is_some_and(|entry| entry.digest_verified),
                learning: version.get("learning").cloned().filter(|value| !value.is_null()),
                adoption,
                judge_result: judge_body.clone(),
                judge_error_code,
                judge_measurement,
                cites_measured_attempt: cites_same_artifact,
                metacontract: meta,
                trials,
                negatives,
                reason,
            });
        }
    }

    checks.push(Check {
        name: "the_judgement_cites_an_attempt_that_measured_the_judged_artifact".into(),
        passed: !authored.is_empty()
            && !wrong_artifact
            && reports
                .iter()
                .any(|report| report.adoption.is_some() && report.cites_measured_attempt),
        detail: if authored.is_empty() {
            "the run wrote no new cognition version".into()
        } else if wrong_artifact {
            "a judgement cited an attempt measured against another artifact".into()
        } else {
            "checked every authored version against the trial reports and the judge's frozen input"
                .into()
        },
    });
    checks.push(Check {
        name: "empirical_acceptance_is_the_installation_s_own_fact".into(),
        passed: empirical_acceptance,
        detail: if empirical_acceptance {
            "the adoption record is empirical, its evidence verified, and the cited attempt measured this exact artifact"
                .into()
        } else {
            "no authored version carries a verified empirical acceptance in this installation".into()
        },
    });
    checks.push(Check {
        name: "every_authored_artifact_reads_back_at_its_recorded_digest".into(),
        passed: !authored.is_empty()
            && reports.iter().all(|report| report.digest_verified)
            && authored
                .iter()
                .all(|unit_id| after.versions.contains_key(unit_id)),
        detail: "each version's bytes were re-read from the node and re-hashed".into(),
    });

    let outcome = if authored.is_empty() {
        if wrote {
            "unit_write_attempted_but_no_version_installed".to_owned()
        } else if after.tasks.values().any(|task| task.kind.starts_with("cognition.unit.")) {
            "cognition_children_ran_but_no_unit_was_written".to_owned()
        } else {
            "no_cognition_child_ran".to_owned()
        }
    } else if empirical_acceptance {
        "judged_empirical_on_the_measured_artifact".to_owned()
    } else if judged_any && wrong_artifact {
        "judgement_cites_a_different_artifact".to_owned()
    } else if judged_any {
        "judgement_recorded_without_verified_empirical_evidence".to_owned()
    } else if measured_any {
        "measured_but_not_judged".to_owned()
    } else {
        "written_without_measurement".to_owned()
    };

    Classification {
        authored_unit_ids: authored,
        versions: reports,
        outcome,
        empirical_acceptance,
        checks,
    }
}

/// Restart parity: the same installation started again must not rewrite,
/// supersede or re-judge the authored versions.
pub fn restart_parity(before_restart: &Snapshot, after_restart: &Snapshot, authored: &[String]) -> Value {
    let mut rows = Vec::new();
    let mut all_equal = true;
    let mut duplicates = Vec::new();
    for unit_id in authored {
        let versions_before = before_restart.versions.get(unit_id).cloned().unwrap_or_default();
        let versions_after = after_restart.versions.get(unit_id).cloned().unwrap_or_default();
        let unit_before = before_restart.units.iter().find(|unit| {
            unit.get("unit_id").and_then(Value::as_str) == Some(unit_id.as_str())
        });
        let unit_after = after_restart.units.iter().find(|unit| {
            unit.get("unit_id").and_then(Value::as_str) == Some(unit_id.as_str())
        });
        let versions_equal = versions_before == versions_after;
        let current_equal = unit_before == unit_after;
        let tasks_before: BTreeSet<String> = before_restart
            .tasks
            .iter()
            .filter(|(_, task)| task.kind.starts_with("cognition.unit."))
            .map(|(id, _)| id.clone())
            .collect();
        let new_tasks: Vec<String> = after_restart
            .tasks
            .keys()
            .filter(|id| !tasks_before.contains(*id))
            .cloned()
            .collect();
        // docs/specs/cognition-units.md#spec-51-1 dedup: unchanged bytes never open a second revision.
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for row in &versions_after {
            let digest = row
                .get("artifact_digest")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let revision = row
                .get("revision")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(previous) = seen.insert(digest.clone(), revision.clone())
                && previous != revision
            {
                duplicates.push(json!({
                    "unit_id": unit_id,
                    "artifact_digest": digest,
                    "revisions": [previous, revision],
                }));
            }
        }
        let equal = versions_equal && current_equal && new_tasks.is_empty();
        all_equal &= equal;
        rows.push(json!({
            "unit_id": unit_id,
            "retained_versions_equal": versions_equal,
            "current_version_facts_equal": current_equal,
            "new_cognition_tasks_in_restart": new_tasks,
            "equal": equal,
        }));
    }
    // docs/specs/cognition-units.md#spec-51-4, docs/specs/cognition-units.md#spec-51-10: the subject keeps its pre-switch identity across a restart.
    let agent_id_stable = before_restart.subject_agent_id.is_some()
        && before_restart.subject_agent_id == after_restart.subject_agent_id;
    json!({
        "units": rows,
        "no_rewrite_across_restart": all_equal && !authored.is_empty(),
        "agent_id_stable": agent_id_stable,
        "agent_id_before": before_restart.subject_agent_id,
        "agent_id_after": after_restart.subject_agent_id,
        "duplicate_revision_bytes": duplicates,
        "no_duplicate_revision_bytes": duplicates.is_empty(),
        "scope": "reading the installation after a restart, not a durability or crash proof",
    })
}
