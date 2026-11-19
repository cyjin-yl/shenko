//! Durable generic trial plans, terminal receipts and measurement provenance.
//!
//! Measurement follows the original attempt and its actual receipts, while
//! transferred evidence remains provenance rather than local acceptance.

use anyhow::{Context, Result, bail, ensure};
use crate::Node;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use super::codec::{MAX_EDGES, MAX_TEXT_BYTES};
use super::installation::now;
use super::process::{CommandTrialProvenance, TrialEvidence, command_ran, process_gone, runtime_id};

/// Result of a real trial.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrialOutcome {
    Succeeded,
    Failed,
    Signaled,
    TimedOut,
    Unknown,
}

impl TrialOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrialOutcome::Succeeded => "succeeded",
            TrialOutcome::Failed => "failed",
            TrialOutcome::Signaled => "signaled",
            TrialOutcome::TimedOut => "timed_out",
            TrialOutcome::Unknown => "unknown",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "succeeded" => TrialOutcome::Succeeded,
            "failed" => TrialOutcome::Failed,
            "signaled" => TrialOutcome::Signaled,
            "timed_out" => TrialOutcome::TimedOut,
            "unknown" => TrialOutcome::Unknown,
            other => bail!("unknown trial outcome {other}"),
        })
    }
}

/// Transferable evidence of a command or a runner-owned generic trial.
/// Command evidence keeps its original safe projection; generic trials carry
/// the caller's explicit plan and receipt objects, not captured command data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TrialProvenance {
    Command(CommandTrialProvenance),
    Generic(GenericTrialProvenance),
}

/// A generic trial's original plan and terminal receipts. These remain foreign
/// provenance on import and never become local readiness or adoption facts.
/// Referenced digests do not imply that their object bodies travel here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GenericTrialProvenance {
    pub attempt_id: String,
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    pub kind: String,
    pub started_at: String,
    pub finished_at: String,
    pub outcome: TrialOutcome,
    pub plan_digest: Digest,
    pub plan: Value,
    pub receipts: Vec<Value>,
    pub receipt_count: usize,
    pub interrupted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&TrialEvidence> for TrialProvenance {
    fn from(evidence: &TrialEvidence) -> Self {
        Self::Command(CommandTrialProvenance {
            attempt_id: evidence.attempt_id.clone(),
            started_at: evidence.started_at.clone(),
            finished_at: evidence.finished_at.clone(),
            outcome: evidence.outcome,
            environment_digest: evidence.environment_digest.clone(),
            exit_code: evidence.exit_code,
            signal: evidence.signal,
            stdout_truncated: evidence.stdout_truncated,
            stderr_truncated: evidence.stderr_truncated,
            interrupted: evidence.interrupted,
            artifact_bound: evidence.artifact_bound,
            artifact_verified_before: evidence.artifact_verified_before,
            artifact_verified_after: evidence.artifact_verified_after,
            reader_failed: evidence.reader_failed,
            group_signalled: evidence.group_signalled,
            descendants_unknown: evidence.descendants_unknown,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericTrialPlan {
    kind: String,
    plan: Value,
    plan_digest: Digest,
    started_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericTrialCompletion {
    kind: String,
    plan_digest: Digest,
    receipts: Vec<Value>,
    receipt_count: usize,
    finished_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericTrialInterruption {
    kind: String,
    reason: String,
}

struct StoredTrialProvenance {
    attempt_id: String,
    unit_id: String,
    revision: String,
    artifact_digest: Digest,
    environment_digest: Digest,
    started_at: String,
    finished_at: Option<String>,
    outcome: String,
    data: String,
    attempt_data: Option<String>,
    attempt_state: Option<String>,
    attempt_started_at: Option<String>,
    attempt_matches: Option<bool>,
}

impl StoredTrialProvenance {
    fn generic(self) -> Result<GenericTrialProvenance> {
        ensure!(
            self.attempt_matches == Some(true),
            "generic trial does not match its original attempt"
        );
        let plan: GenericTrialPlan = serde_json::from_str(
            self.attempt_data.as_deref().context("generic trial lost its original plan")?,
        )?;
        let mut plan_hasher = blake3::Hasher::new();
        serde_json::to_writer(&mut plan_hasher, &plan.plan)?;
        ensure!(
            plan.plan.is_object()
                && self.attempt_started_at.as_deref() == Some(plan.started_at.as_str())
                && self.environment_digest == plan.plan_digest
                && plan.plan_digest.canonical_blake3_hex().is_some_and(|expected|
                    plan_hasher.finalize().to_hex().as_str() == expected),
            "generic trial plan does not match its original binding"
        );
        let finished_at = self.finished_at.context("generic trial has no terminal time")?;
        let outcome = TrialOutcome::parse(&self.outcome)?;
        let (receipts, receipt_count, interrupted, reason) = match self.attempt_state.as_deref() {
            Some("finished") => {
                let completed: GenericTrialCompletion = serde_json::from_str(&self.data)?;
                ensure!(
                    completed.kind == plan.kind
                        && completed.plan_digest == plan.plan_digest
                        && completed.finished_at == finished_at
                        && self.started_at == plan.started_at
                        && completed.receipt_count == completed.receipts.len()
                        && completed.receipts.iter().all(|receipt| {
                            receipt.is_object()
                                && receipt.get("digest").and_then(Value::as_str).is_some()
                        }),
                    "generic trial completion does not match its original binding"
                );
                (completed.receipts, completed.receipt_count, false, None)
            }
            Some("unknown") => {
                let interrupted: GenericTrialInterruption = serde_json::from_str(&self.data)?;
                ensure!(
                    interrupted.kind == "unknown"
                        && outcome == TrialOutcome::Unknown
                        // Earlier retirement rows used their retirement time
                        // as started_at; the attempt retains the actual start.
                        && (self.started_at == plan.started_at || self.started_at == finished_at),
                    "interrupted generic trial does not match its original binding"
                );
                (Vec::new(), 0, true, Some(interrupted.reason))
            }
            _ => bail!("generic trial attempt is not terminal"),
        };
        Ok(GenericTrialProvenance {
            attempt_id: self.attempt_id,
            unit_id: self.unit_id,
            revision: self.revision,
            artifact_digest: self.artifact_digest,
            kind: plan.kind,
            started_at: plan.started_at,
            finished_at,
            outcome,
            plan_digest: plan.plan_digest,
            plan: plan.plan,
            receipts,
            receipt_count,
            interrupted,
            reason,
        })
    }
}

/// What one trial attempt can carry as evidence of a measurement.
///
/// The node derives this from the attempt's own durable facts — the plan it
/// was opened with, the receipts its run produced, and the execution fact of
/// a command trial — never from an outcome word. A failed, signalled or
/// timed-out run is a measurement; an attempt that was opened and never
/// closed is not.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptMeasurement {
    /// Every sample slot of the attempt's own plan produced a judge-scored
    /// receipt, or a command trial recorded that its command ran.
    Measured,
    /// Some planned slots produced judge-scored receipts; others did not.
    Partial,
    /// The attempt closed without a single authenticated measurement fact.
    Inconclusive,
    /// The attempt was opened and never closed: no terminal trial fact exists.
    Unexecuted,
}

impl AttemptMeasurement {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttemptMeasurement::Measured => "measured",
            AttemptMeasurement::Partial => "partial",
            AttemptMeasurement::Inconclusive => "inconclusive",
            AttemptMeasurement::Unexecuted => "unexecuted",
        }
    }

    /// Whether the attempt carries an authenticated measurement, including a
    /// negative one. Only such evidence can make a judgement empirical.
    pub fn carries_measurement(&self) -> bool {
        matches!(
            self,
            AttemptMeasurement::Measured | AttemptMeasurement::Partial
        )
    }
}

/// The durable facts of one trial attempt.
///
/// An attempt is opened before the trial runs and closed with the receipts
/// the run produced. Its plan digest binds the attempt to the exact unit
/// version, the scenarios and rubrics, the candidate and control
/// configuration and the model binding the trial used.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrialAttemptFacts {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    /// `trial_open` while the trial runs, `finished` when it closed with
    /// receipts, `unknown` when the owning process disappeared.
    pub state: String,
    pub started_at: String,
    pub kind: Option<String>,
    pub plan_digest: Option<String>,
    /// The terminal outcome of the append-only trial row, when one exists.
    /// `None` means the attempt was never closed.
    pub outcome: Option<String>,
    /// The environment the closed run was bound to. A command trial has no
    /// plan; this is the probe the node made around its run.
    pub environment_digest: Option<String>,
    /// The receipts the closed run wrote, counted before they are judged.
    pub receipts: u64,
    /// The receipts that are judge-scored facts of a slot the attempt's own
    /// plan named.
    pub scored: u64,
    /// The sample slots the attempt's own plan named, when it names them.
    pub planned_slots: Option<u64>,
    /// What this attempt's facts can carry.
    pub measurement: AttemptMeasurement,
}

// ------------------------------------------------------------- helpers -----

/// The sample slots a comparison plan named: one candidate and one control
/// run per scenario and repetition.
///
/// A plan that names no scenarios or no repetition count does not describe
/// slots, so nothing in it can be called a complete measurement.
fn plan_slots(plan: Option<&Value>) -> Option<u64> {
    let plan = plan?;
    let scenarios = plan.get("scenarios")?.as_array()?;
    let trials = plan.get("trials_per_condition")?.as_u64()?;
    (scenarios.len() as u64).checked_mul(trials)?.checked_mul(2)
}

/// Whether one receipt names a sample slot the plan named.
fn receipt_in_plan(plan: &Value, receipt: &Value) -> bool {
    let Some(scenarios) = plan.get("scenarios").and_then(Value::as_array) else {
        return false;
    };
    let Some(trials) = plan.get("trials_per_condition").and_then(Value::as_u64) else {
        return false;
    };
    let Some(name) = receipt.get("scenario").and_then(Value::as_str) else {
        return false;
    };
    let Some(condition) = receipt.get("condition").and_then(Value::as_str) else {
        return false;
    };
    let Some(trial) = receipt.get("trial").and_then(Value::as_u64) else {
        return false;
    };
    matches!(condition, "candidate" | "control")
        && trial < trials
        && scenarios
            .iter()
            .any(|scenario| scenario.get("name").and_then(Value::as_str) == Some(name))
}

/// Receipts that are judge-scored facts of a slot the attempt's own plan
/// named.
///
/// A receipt naming a sample the plan never asked for was not produced by
/// this attempt's run, so it never counts as a scored sample.
fn scored_receipts(plan: &Value, trial_data: &Value) -> usize {
    trial_data
        .get("receipts")
        .and_then(Value::as_array)
        .map(|receipts| {
            receipts
                .iter()
                .filter(|receipt| {
                    matches!(
                        receipt.get("verdict").and_then(Value::as_str),
                        Some("effective") | Some("ineffective")
                    ) && receipt_in_plan(plan, receipt)
                })
                .count()
        })
        .unwrap_or(0)
}

impl Node {
    pub(super) fn unit_evidence(&self, unit_id: Option<&str>, after: &str, limit: u32) -> Result<Value> {
        ensure!((1..=128).contains(&limit), "evidence limit 1..128");
        let after = crate::number(after)?;
        let after_seq = i64::try_from(after)?;
        let rows: Vec<(
            i64,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            String,
        )> = match unit_id {
            Some(id) => self
                .conn
                .prepare(
                    "SELECT seq,attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,data \
                     FROM unit_trials WHERE seq>?1 AND unit_id=?2 ORDER BY seq LIMIT ?3",
                )?
                .query_map(params![after_seq, id, limit], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => self
                .conn
                .prepare(
                    "SELECT seq,attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,data \
                     FROM unit_trials WHERE seq>?1 ORDER BY seq LIMIT ?2",
                )?
                .query_map(params![after_seq, limit], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        let mut entries = Vec::new();
        let mut cursor = after;
        for (seq, attempt, unit, revision, digest, environment, started, finished, outcome, data) in
            rows
        {
            cursor = seq as u64;
            // The classification is the node's own reading of the attempt,
            // so a reader never has to re-derive it from the raw evidence.
            let classified = Self::trial_attempt_facts(&self.conn, &attempt)?;
            entries.push(json!({
                "seq": seq.to_string(),
                "attempt_id": attempt,
                "unit_id": unit,
                "revision": revision,
                "artifact_digest": digest,
                "environment_digest": environment,
                "started_at": started,
                "finished_at": finished,
                "outcome": outcome,
                "measurement": classified.as_ref().map(|facts| facts.measurement.as_str()),
                "scored": classified.as_ref().map(|facts| facts.scored),
                "planned_slots": classified.as_ref().and_then(|facts| facts.planned_slots),
                "evidence": serde_json::from_str::<Value>(&data)?,
            }));
        }
        Ok(json!({
            "evidence": entries,
            "through": cursor.to_string(),
            "append_only": true,
        }))
    }

    /// The durable facts of one trial attempt.
    ///
    /// A trial attempt is runner-owned: this node opened it before the run
    /// started and closes it with the receipts the run produced. A caller
    /// cannot write an attempt row directly, so a report cannot promote
    /// itself into a measurement.
    pub(super) fn trial_attempt_facts(conn: &Connection, attempt_id: &str) -> Result<Option<TrialAttemptFacts>> {
        let row: Option<(String, String, String, String, String, String)> = conn
            .prepare(
                "SELECT unit_id,revision,artifact_digest,state,started_at,data FROM unit_attempts WHERE attempt_id=?1",
            )?
            .query_row(params![attempt_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .optional()?;
        let Some((unit_id, revision, artifact_digest, state, started_at, data)) = row else {
            return Ok(None);
        };
        let stored: Value = serde_json::from_str(&data).unwrap_or_else(|_| json!({}));
        let plan = stored.get("plan").filter(|plan| plan.is_object());
        // The append-only trial row is the attempt's terminal fact. Its
        // absence means the attempt was opened and never closed, so nothing
        // about a run is recorded under it.
        let trial: Option<(String, String, String)> = conn
            .prepare(
                "SELECT outcome,environment_digest,data FROM unit_trials WHERE attempt_id=?1 ORDER BY seq DESC LIMIT 1",
            )?
            .query_row(params![attempt_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let (outcome, environment_digest, receipts, scored, planned_slots, measurement) = match trial
        {
            None => (None, None, 0, 0, plan_slots(plan), AttemptMeasurement::Unexecuted),
            Some((outcome, environment_digest, trial_data)) => {
                let trial_data: Value =
                    serde_json::from_str(&trial_data).unwrap_or_else(|_| json!({}));
                let receipts = trial_data
                    .get("receipts")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                match plan {
                    // A comparison attempt is measured by the receipts its
                    // own run wrote for the slots its own plan named. The
                    // outcome word is not what makes it a measurement.
                    Some(plan) => {
                        let planned_slots = plan_slots(Some(plan));
                        let scored = scored_receipts(plan, &trial_data);
                        let measurement = if scored == 0 {
                            AttemptMeasurement::Inconclusive
                        } else if planned_slots.is_none_or(|planned| scored as u64 >= planned) {
                            AttemptMeasurement::Measured
                        } else {
                            AttemptMeasurement::Partial
                        };
                        (
                            Some(outcome),
                            Some(environment_digest),
                            receipts as u64,
                            scored as u64,
                            planned_slots,
                            measurement,
                        )
                    }
                    // A command trial has no plan slots: its measurement is
                    // the execution fact the run recorded.
                    None => {
                        let measurement = if command_ran(&trial_data) {
                            AttemptMeasurement::Measured
                        } else {
                            AttemptMeasurement::Inconclusive
                        };
                        (
                            Some(outcome),
                            Some(environment_digest),
                            0,
                            0,
                            None,
                            measurement,
                        )
                    }
                }
            }
        };
        Ok(Some(TrialAttemptFacts {
            unit_id,
            revision,
            artifact_digest: Digest(artifact_digest),
            state,
            started_at,
            kind: stored.get("kind").and_then(Value::as_str).map(str::to_string),
            plan_digest: stored
                .get("plan_digest")
                .and_then(Value::as_str)
                .map(str::to_string),
            outcome,
            environment_digest,
            receipts,
            scored,
            planned_slots,
            measurement,
        }))
    }

    /// The plan one attempt was opened with, when it carries one.
    fn attempt_plan(conn: &Connection, attempt_id: &str) -> Result<Option<Value>> {
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM unit_attempts WHERE attempt_id=?1",
                params![attempt_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        let stored: Value = serde_json::from_str(&data).unwrap_or_else(|_| json!({}));
        Ok(stored.get("plan").filter(|plan| plan.is_object()).cloned())
    }

    /// Open a durable trial attempt for one unit version.
    ///
    /// The attempt exists before anything runs, so a crash leaves an
    /// unresolved attempt instead of a missing fact. The plan records what
    /// the trial is bound to: the exact artifact, the scenarios and rubrics,
    /// the candidate and control configuration and the model binding.
    pub(super) fn unit_begin_trial(
        &mut self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        kind: &str,
        plan: &Value,
    ) -> Result<Value> {
        let kind = kind.trim();
        ensure!(
            !kind.is_empty() && kind.len() <= MAX_TEXT_BYTES,
            "the trial kind is empty or too long"
        );
        ensure!(plan.is_object(), "the trial plan must be an object");
        let plan_bytes = serde_json::to_vec(plan)?;
        ensure!(
            plan_bytes.len() <= MAX_TEXT_BYTES * 8,
            "the trial plan exceeds the size bound"
        );
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        ensure!(
            unit.revision == revision && unit.artifact_digest == *artifact_digest,
            "the trial must bind the current version of the unit"
        );
        self.retire_open_trials()?;
        let plan_digest = Digest::blake3(&plan_bytes);
        let attempt_id = EntityId::new().to_string();
        let started_at = now();
        let owner_pid = i64::from(std::process::id());
        let owner_runtime = runtime_id(&self.conn)?;
        let data = json!({
            "kind": kind,
            "plan": plan,
            "plan_digest": plan_digest.0,
            "started_at": started_at,
        });
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        tx.execute(
            "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
             VALUES(?1,?2,?3,?4,?5,'trial_open',?6,NULL,?7,?8,?9)",
            params![
                attempt_id,
                unit_id,
                revision,
                artifact_digest.0,
                plan_digest.0,
                started_at,
                owner_pid,
                owner_runtime,
                serde_json::to_string(&data)?
            ],
        )?;
        tx.commit()?;
        Ok(json!({
            "attempt_id": attempt_id,
            "kind": kind,
            "plan_digest": plan_digest.0,
            "state": "trial_open",
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest.0,
        }))
    }

    /// Close a trial attempt with the receipts the run produced.
    ///
    /// Every receipt is an independent terminal fact of one condition. The
    /// node stores them with the attempt and never derives a measurement from
    /// a summary the caller wrote.
    pub(super) fn unit_finish_trial(
        &mut self,
        attempt_id: &str,
        outcome: TrialOutcome,
        receipts: &[Value],
    ) -> Result<Value> {
        ensure!(
            receipts.len() <= MAX_EDGES * 4,
            "too many trial receipts"
        );
        for receipt in receipts {
            ensure!(receipt.is_object(), "a trial receipt must be an object");
            ensure!(
                receipt.get("digest").and_then(Value::as_str).is_some(),
                "a trial receipt must name the digest of the fact it carries"
            );
        }
        // A comparison attempt's receipts are facts about the sample slots
        // its own plan named. When the plan names slots, a receipt outside
        // them was never produced by this attempt's run, so accepting it
        // would let a caller hand the node a measurement it did not make.
        if let Some(plan) = Self::attempt_plan(&self.conn, attempt_id)? {
            if plan_slots(Some(&plan)).is_some() {
                for receipt in receipts {
                    ensure!(
                        receipt_in_plan(&plan, receipt),
                        "a trial receipt does not belong to a sample slot this attempt's plan named"
                    );
                }
            }
        }
        let facts = Self::trial_attempt_facts(&self.conn, attempt_id)?
            .context("this node never opened that trial attempt")?;
        ensure!(
            facts.state == "trial_open",
            "the trial attempt is not open; it was already closed"
        );
        let finished_at = now();
        let data = json!({
            "kind": facts.kind,
            "plan_digest": facts.plan_digest,
            "receipts": receipts,
            "receipt_count": receipts.len(),
            "finished_at": finished_at,
        });
        let owner_runtime = runtime_id(&self.conn)?;
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        tx.execute(
            "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0,0,?9,?10,?11)",
            params![
                attempt_id,
                facts.unit_id,
                facts.revision,
                facts.artifact_digest.0,
                facts.plan_digest.clone().unwrap_or_default(),
                facts.started_at,
                finished_at,
                outcome.as_str(),
                i64::from(std::process::id()),
                owner_runtime,
                serde_json::to_string(&data)?
            ],
        )?;
        tx.execute(
            "UPDATE unit_attempts SET state='finished',finished_at=?2 WHERE attempt_id=?1",
            params![attempt_id, finished_at],
        )?;
        tx.commit()?;
        Ok(json!({
            "attempt_id": attempt_id,
            "state": "finished",
            "outcome": outcome.as_str(),
            "receipt_count": receipts.len(),
            "unit_id": facts.unit_id,
            "revision": facts.revision,
            "artifact_digest": facts.artifact_digest.0,
        }))
    }

    /// Close trial attempts whose owning process is gone.
    ///
    /// A trial that never finished is unknown, never a measurement. The
    /// attempt keeps its plan so a reader can see what was bound.
    fn retire_open_trials(&mut self) -> Result<u32> {
        let my_runtime = runtime_id(&self.conn)?;
        let my_pid = i64::from(std::process::id());
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        let pending: Vec<(String, String, String, String, String, i64, String)> = tx
            .prepare(
                "SELECT attempt_id,unit_id,revision,artifact_digest,environment_digest,owner_pid,owner_runtime FROM unit_attempts WHERE state='trial_open' ORDER BY started_at",
            )?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if pending.is_empty() {
            tx.commit()?;
            return Ok(0);
        }
        let finished = now();
        let mut retired = 0u32;
        for (
            attempt_id,
            unit_id,
            revision,
            digest,
            environment,
            owner_pid,
            owner_runtime,
        ) in pending
        {
            let mine = owner_runtime == my_runtime && owner_pid == my_pid;
            if !mine && !process_gone(owner_pid, &owner_runtime) {
                continue;
            }
            tx.execute(
                "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'unknown',0,0,?8,?9,?10)",
                params![
                    attempt_id,
                    unit_id,
                    revision,
                    digest,
                    environment,
                    finished,
                    finished,
                    owner_pid,
                    owner_runtime,
                    json!({"kind": "unknown", "reason": "the trial attempt was never closed"}).to_string()
                ],
            )?;
            tx.execute(
                "UPDATE unit_attempts SET state='unknown',finished_at=?2 WHERE attempt_id=?1",
                params![attempt_id, finished],
            )?;
            retired += 1;
        }
        tx.commit()?;
        Ok(retired)
    }

    pub(super) fn local_trial_count(&self, unit_id: &str, digest: &Digest) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM unit_trials WHERE unit_id=?1 AND artifact_digest=?2",
            params![unit_id, digest.0],
            |r| r.get(0),
        )?;
        Ok(count as u32)
    }

    pub(super) fn local_provenance(&self, unit_id: &str, digest: &Digest) -> Result<Vec<TrialProvenance>> {
        let mut statement = self.conn.prepare(
            "SELECT t.attempt_id,t.unit_id,t.revision,t.artifact_digest,t.environment_digest,t.started_at,t.finished_at,t.outcome,t.data,\
             a.data,a.state,a.started_at,\
             a.unit_id=t.unit_id AND a.revision=t.revision AND a.artifact_digest=t.artifact_digest \
             AND a.environment_digest=t.environment_digest AND a.finished_at=t.finished_at \
             FROM unit_trials t LEFT JOIN unit_attempts a ON a.attempt_id=t.attempt_id \
             WHERE t.unit_id=?1 AND t.artifact_digest=?2 ORDER BY t.seq",
        )?;
        let rows = statement.query_map(params![unit_id, digest.0], |r| {
            Ok(StoredTrialProvenance {
                attempt_id: r.get(0)?,
                unit_id: r.get(1)?,
                revision: r.get(2)?,
                artifact_digest: Digest(r.get(3)?),
                environment_digest: Digest(r.get(4)?),
                started_at: r.get(5)?,
                finished_at: r.get(6)?,
                outcome: r.get(7)?,
                data: r.get(8)?,
                attempt_data: r.get(9)?,
                attempt_state: r.get(10)?,
                attempt_started_at: r.get(11)?,
                attempt_matches: r.get(12)?,
            })
        })?;
        rows.map(|row| {
            let row = row?;
            match serde_json::from_str::<TrialEvidence>(&row.data) {
                Ok(command) => {
                    ensure!(
                        command.attempt_id == row.attempt_id
                            && command.environment_digest == row.environment_digest
                            && command.outcome.as_str() == row.outcome,
                        "command trial does not match its stored identity"
                    );
                    Ok(TrialProvenance::from(&command))
                }
                Err(command_error) => row.generic()
                    .map(TrialProvenance::Generic)
                    .with_context(|| format!("invalid local trial evidence; command decoding: {command_error}")),
            }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::Node;
    use crate::units::UnitRequest;
    use crate::units::test_support::{import, package, trial};
    use rusqlite::params;
    use serde_json::{Value, json};
    use shenko_types::ids::EntityId;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use super::TrialOutcome;

    #[test]
    fn generic_trials_export_with_original_plans_without_inheriting_readiness() {
        let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/unit-provenance"));
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&base).unwrap();
        let dir = base.join(format!("generic-provenance-{}", EntityId::new()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let source_root = dir.join("source-packages");
        let receiver_root = dir.join("receiver-packages");
        std::fs::create_dir(&source_root).unwrap();
        std::fs::create_dir(&receiver_root).unwrap();
        let mut source = Node::open(&dir.join("source")).unwrap();
        let package = package("trial.generic", "1.0.0", b"owned trial artifact\n", vec![]);
        let identity = package.unit.clone();
        import(&mut source, &source_root, vec![package]);

        let plan = json!({
            "candidate": {"unit_id": identity.unit_id, "artifact_digest": identity.artifact_digest},
            "conditions": [{"world": "one", "ordinal": 1}, {"world": "two", "ordinal": 2}],
            "numeric_fact": 51.248178375505404_f64,
            "scope": "native provenance mechanics, no model or efficacy claim"
        });
        let begun = source.handle_unit_request(UnitRequest::BeginTrial {
            unit_id: identity.unit_id.clone(),
            revision: identity.revision.clone(),
            artifact_digest: identity.artifact_digest.clone(),
            kind: "sandbox_comparison".into(),
            plan: plan.clone(),
        }).unwrap();
        let attempt_id = begun["attempt_id"].as_str().unwrap().to_string();
        let receipts = vec![
            json!({"digest": identity.artifact_digest, "fact": {"kind": "artifact", "ordinal": 1,
                "numeric_fact": 51.248178375505404_f64}}),
            json!({"digest": begun["plan_digest"], "fact": {"kind": "plan", "ordinal": 2}}),
        ];
        source.handle_unit_request(UnitRequest::FinishTrial {
            attempt_id: attempt_id.clone(),
            outcome: TrialOutcome::Succeeded,
            receipts: receipts.clone(),
        }).unwrap();
        assert_eq!(source.unit_status(&identity.unit_id).unwrap()["readiness"]["state"], json!("unverified"));

        let command = source.handle_unit_request(trial(
            &identity.unit_id,
            vec!["/bin/sh", "-c", "cat \"$1\"; printf '\\nprivate-native-trial-output\\n'", "trial-reader", "{artifact}"],
        )).unwrap();
        assert_eq!(command["outcome"], json!("succeeded"));
        assert!(command["stdout"].as_str().unwrap().contains("private-native-trial-output"));
        assert_eq!(source.unit_status(&identity.unit_id).unwrap()["readiness"]["state"], json!("ready"));

        let interrupted_plan = json!({"scope": "opened but never completed", "artifact": identity.artifact_digest});
        let interrupted = source.handle_unit_request(UnitRequest::BeginTrial {
            unit_id: identity.unit_id.clone(),
            revision: identity.revision.clone(),
            artifact_digest: identity.artifact_digest.clone(),
            kind: "observation_capture".into(),
            plan: interrupted_plan.clone(),
        }).unwrap();
        assert_eq!(source.retire_open_trials().unwrap(), 1);

        let exported = source.handle_unit_request(UnitRequest::Export {
            unit_id: identity.unit_id.clone(),
        }).unwrap();
        let wire = &exported["export"];
        let evidence = wire["packages"][0]["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 3);
        let completed = evidence.iter().find(|row| row["attempt_id"] == begun["attempt_id"]).unwrap();
        assert_eq!(completed["kind"], json!("sandbox_comparison"));
        assert_eq!(completed["plan_digest"], begun["plan_digest"]);
        assert_eq!(completed["plan"], plan);
        assert_eq!(completed["receipts"], json!(receipts));
        assert_eq!(completed["outcome"], json!("succeeded"));
        assert_eq!(completed["interrupted"], json!(false));
        let unresolved = evidence.iter().find(|row| row["attempt_id"] == interrupted["attempt_id"]).unwrap();
        assert_eq!(unresolved["kind"], json!("observation_capture"));
        assert_eq!(unresolved["plan"], interrupted_plan);
        assert_eq!(unresolved["outcome"], json!("unknown"));
        assert_eq!(unresolved["interrupted"], json!(true));
        assert_eq!(unresolved["receipts"], json!([]));
        let command_provenance = evidence.iter().find(|row| row["exit_code"] == json!(0)).unwrap();
        assert_eq!(command_provenance["artifact_bound"], json!(true));
        assert!(command_provenance.get("kind").is_none());
        assert!(command_provenance.get("stdout").is_none());
        assert!(!serde_json::to_string(wire).unwrap().contains("private-native-trial-output"));

        let mut receiver = Node::open(&dir.join("receiver")).unwrap();
        receiver.handle_unit_request(UnitRequest::Import {
            export: serde_json::from_value(wire.clone()).unwrap(),
            package_root: receiver_root.to_string_lossy().into_owned(),
            enable: Some(false),
        }).unwrap();
        let received = receiver.unit_status(&identity.unit_id).unwrap();
        assert_eq!(received["readiness"]["state"], json!("unverified"));
        assert_eq!(received["local_trials"], json!(0));
        assert_eq!(received["remote_evidence"], json!(3));
        let stored: String = receiver.conn.query_row(
            "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![identity.unit_id, identity.revision, identity.artifact_digest.0],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap()["remote_evidence"], json!(evidence));

        let original: String = source.conn.query_row(
            "SELECT data FROM unit_attempts WHERE attempt_id=?1", [&attempt_id], |row| row.get(0),
        ).unwrap();
        let mut changed: Value = serde_json::from_str(&original).unwrap();
        changed["plan"]["conditions"][0]["world"] = json!("different-plan");
        source.conn.execute(
            "UPDATE unit_attempts SET data=?2 WHERE attempt_id=?1",
            params![attempt_id, serde_json::to_string(&changed).unwrap()],
        ).unwrap();
        assert!(source.handle_unit_request(UnitRequest::Export { unit_id: identity.unit_id.clone() }).is_err());
        source.conn.execute(
            "UPDATE unit_attempts SET data=?2,revision='different-version' WHERE attempt_id=?1",
            params![attempt_id, original],
        ).unwrap();
        assert!(source.handle_unit_request(UnitRequest::Export { unit_id: identity.unit_id.clone() }).is_err());
        source.conn.execute(
            "UPDATE unit_attempts SET revision=?2 WHERE attempt_id=?1",
            params![attempt_id, identity.revision],
        ).unwrap();
    }
}
